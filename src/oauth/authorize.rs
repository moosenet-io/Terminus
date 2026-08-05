//! The authorization endpoint: login, consent, and code issuance (RMCP-03).
//!
//! This is the interactive half of the OAuth flow and the most
//! security-sensitive surface in the crate. Three requests make it up:
//!
//! ```text
//! GET  /oauth/authorize   validate the request; render login (or an error page)
//! POST /oauth/login       authenticate the human; render consent (or issue, if
//!                         an unrevoked consent already covers this scope)
//! POST /oauth/consent     record the approval and issue a single-use code
//! ```
//!
//! ## Rule one: the first two checks NEVER redirect
//!
//! An OAuth authorization server signals most errors by redirecting back to the
//! client with an `error` parameter. That is correct — and it is a catastrophic
//! open redirect if it is done before the destination has been established as
//! legitimate. An unknown `client_id` or an unregistered `redirect_uri` means
//! precisely that the destination is NOT established, so those two failures
//! render a terminal error page in the browser and emit no `Location` header at
//! all. Everything checked AFTER them — response type, PKCE, resource, scope —
//! is redirected, because by then the destination is one the operator
//! registered.
//!
//! The ordering is therefore not a style choice. It is the whole reason
//! [`validate`] is a single function with an explicit sequence rather than a
//! set of guards a future edit could reorder.
//!
//! ## Rule two: redirect matching is exact, with ONE named exception
//!
//! [`redirect_uri_matches`] is a byte-for-byte string comparison. The single
//! exception is an RFC 8252 loopback URI, which matches with the port ignored,
//! because a native client (Claude Code among them) binds an ephemeral port it
//! cannot know at registration time. That exception is an explicit branch with
//! its own parser and its own tests — not a relaxation of the general rule.
//!
//! A "fuzzy" or prefix-based matcher is how open redirects are born: `https://
//! trusted.example` prefix-matching `https://trusted.example.attacker.test`,
//! host-only matching ignoring a path, a scheme downgrade slipping through. The
//! loopback branch requires the scheme to be `http`, the host to be one of a
//! fixed allowlist AND identical between the two URIs, and the path, query and
//! fragment to be identical. Only the port may differ.
//!
//! ## Rule three: nothing arriving from the browser is authority
//!
//! The login and consent forms carry the authorization request in hidden
//! fields. Those fields are a convenience for the browser round trip and are
//! re-validated from scratch on every POST — same [`validate`], same store
//! lookups, same ordering. A hidden field that said `redirect_uri` is not a
//! reason to trust it, and the consent screen the human read was rendered from
//! values that were validated, so re-validating cannot change what they
//! approved.
//!
//! ## No configuration values are compiled in
//!
//! The issuer identifier and the canonical resource are read from the
//! environment ([`AuthorizeConfig`]); there is no host, URL, port or address
//! anywhere in this file or its tests.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use uuid::Uuid;

use crate::error::ToolError;
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::oauth::audit::{AuditDetail, DenialReason, OauthAuditRecord, OauthEvent};
use crate::oauth::edge::ResolvedClientIp;
use crate::oauth::limits::{throttled_response, OauthEndpoint, OauthRateLimiter};
use crate::gateway_framework::ActionKind;
use crate::oauth::model::Client;
use crate::oauth::password;
use crate::oauth::session::{self, LoginSession, SessionKey};
use crate::oauth::store::OauthStore;
use crate::oauth::templates::{self, ConsentContext, GroupSummary, LoginContext};
use crate::oauth::SecretHash;

/// Env name of the issuer identifier echoed as RFC 9207 `iss`.
pub const ISSUER_ENV: &str = "RMCP_OAUTH_ISSUER";

/// Env name of the canonical RFC 8707 resource this server issues tokens for.
pub const RESOURCE_ENV: &str = "RMCP_OAUTH_RESOURCE";

/// Lifetime of an authorization code, in seconds.
///
/// The OAuth 2.1 draft says a code SHOULD be short-lived and single-use, and
/// suggests a maximum of ten minutes. Sixty seconds is chosen instead: the code
/// travels from this server to the browser to the client and straight back to
/// the token endpoint, a round trip measured in hundreds of milliseconds even
/// on a bad connection. Every second beyond that is only ever useful to someone
/// who intercepted it. Single use ([`OauthStore::consume_auth_code`]) is the
/// primary defence; a tight TTL bounds the damage when the code is captured
/// before its legitimate redemption.
pub const CODE_TTL_SECONDS: i64 = 60;

/// The scopes this authorization server understands.
///
/// A request for anything else is NARROWED to this set rather than rejected
/// outright — an unknown scope is an unknown capability, and the safe reading
/// of "grant me `mcp` and `admin`" is to grant `mcp`. The narrowed set is what
/// the consent screen displays and what the code is bound to, so the human
/// never approves a string that differs from what is granted.
pub const SUPPORTED_SCOPES: [&str; 2] = ["mcp", "offline_access"];

/// The scope granted when a request names none.
const DEFAULT_SCOPE: &str = "mcp";

/// The hosts the RFC 8252 loopback exception applies to.
///
/// A fixed allowlist, checked by exact string equality. Anything resolving to a
/// loopback address at DNS time is emphatically NOT included: a name an
/// attacker controls can be pointed at `127.0.0.1` today and at their own host
/// tomorrow, so resolution-based loopback detection is a redirect the attacker
/// gets to move afterwards.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// Minimum and maximum length of a PKCE `code_challenge`.
///
/// RFC 7636 fixes these: the challenge is the base64url-unpadded SHA-256 of a
/// verifier of 43..=128 characters, so a value outside the range is not a
/// challenge any conforming client produced.
const CHALLENGE_MIN_LEN: usize = 43;
const CHALLENGE_MAX_LEN: usize = 128;

// The login budget used to live here as a private `LOGIN_BURST` /
// `LOGIN_REFILL_PER_SEC` pair with its own `InProcessRateLimiter`. TERM #633
// converged it onto `crate::oauth::limits::OauthRateLimiter`, which owns the
// budget for every OAuth endpoint.
//
// The move is not tidying. Two budget definitions for one door drift, and this
// one drifted in the direction that matters: a lone `InProcessRateLimiter`
// has no notion of the subject-budget-larger-than-address-budget invariant, so
// the per-account and per-source buckets here were sized IDENTICALLY — which
// means one source address exhausting its own budget also exhausted the named
// account's, and could hold any account whose name it could guess locked out
// for free. The shared limiter refuses that configuration at construction.

/// Rate-limit / audit action label for a login attempt.
///
/// [`ActionKind::Admin`] entries carry an `admin:`-prefixed action string by
/// crate convention, so an admin audit entry can never be confused with a
/// `Tool`-kind one that happens to share a name.
const LOGIN_ACTION: &str = "admin:rmcp_oauth_login";

/// The address attributed to a request whose source cannot be determined.
///
/// Every such request shares ONE bucket, which is stricter than per-address,
/// not weaker — the failure direction here is more throttling, never less. It
/// is the unspecified address rather than a loopback literal so it cannot be
/// confused with a real caller in the audit trail.
const UNATTRIBUTED_SOURCE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

/// The source address to gate and audit on, in priority order.
///
/// 1. [`ResolvedClientIp`] — the address RMCP-09's edge resolved and admitted.
///    This is the only value that agrees with the source policy, so when it is
///    present nothing else is consulted.
/// 2. The socket peer, for a request that reached a private listener directly
///    with no edge in front of it.
/// 3. [`UNATTRIBUTED_SOURCE`], which still consumes budget.
///
/// Note what is NOT here: `X-Forwarded-For`. Reading it in a handler would let
/// a caller choose its own rate-limit key, and would disagree with the edge —
/// which has already decided, from the trusted-proxy set, which hop may be
/// attributed.
pub fn resolved_source_for(extensions: &axum::http::Extensions) -> IpAddr {
    if let Some(ResolvedClientIp(ip)) = extensions.get::<ResolvedClientIp>() {
        return *ip;
    }
    if let Some(ConnectInfo(addr)) = extensions.get::<ConnectInfo<SocketAddr>>() {
        return addr.ip();
    }
    UNATTRIBUTED_SOURCE
}


/// The one message shown for every authentication failure.
///
/// Unknown account, wrong password, disabled account — all produce this exact
/// string, with the same status code, from the same code path. Anything that
/// varies between them is an account-existence oracle.
const GENERIC_LOGIN_FAILURE: &str = "Sign-in failed. Check the account name and password.";

/// Characters left un-encoded when building a redirect query string: the RFC
/// 3986 unreserved set. Everything else is percent-encoded, which is what makes
/// a `state` containing `&`, `=` or `#` round-trip to the client unchanged
/// instead of forging extra parameters.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Non-secret configuration for the authorization endpoint.
///
/// Both values are required and neither has a default. A default issuer would
/// be a hardcoded infrastructure value; a default resource would mean the
/// audience check ([`validate`]) compares against something nobody chose, which
/// is worse than not checking at all because it looks like it is checking.
#[derive(Clone, Debug)]
pub struct AuthorizeConfig {
    issuer: String,
    resource: String,
}

impl AuthorizeConfig {
    /// Build from explicit values. Blank is treated as absent.
    pub fn new(issuer: &str, resource: &str) -> Result<Self, ToolError> {
        let issuer = issuer.trim();
        let resource = resource.trim();
        if issuer.is_empty() {
            return Err(ToolError::NotConfigured(format!("{ISSUER_ENV} not set")));
        }
        if resource.is_empty() {
            return Err(ToolError::NotConfigured(format!("{RESOURCE_ENV} not set")));
        }
        Ok(Self { issuer: issuer.to_string(), resource: resource.to_string() })
    }

    /// Read from the runtime-materialized environment.
    pub fn from_env() -> Result<Self, ToolError> {
        let issuer = std::env::var(ISSUER_ENV).unwrap_or_default();
        let resource = std::env::var(RESOURCE_ENV).unwrap_or_default();
        Self::new(&issuer, &resource)
    }

    /// The RFC 9207 issuer identifier.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The canonical resource every authorization request must name.
    pub fn resource(&self) -> &str {
        &self.resource
    }
}

// ---------------------------------------------------------------------------
// Form / query parsing
// ---------------------------------------------------------------------------

/// A parsed `application/x-www-form-urlencoded` payload, used for BOTH the
/// authorize query string and the login/consent request bodies.
///
/// One parser for both is deliberate: the POST handlers re-validate the
/// authorization request from hidden fields, and if the two sides decoded
/// differently, a value could pass validation in one shape and be used in
/// another.
///
/// ## Duplicate keys are an error, not a last-one-wins
/// `client_id=trusted&client_id=hostile` is HTTP parameter pollution, and the
/// damage comes from two components disagreeing about which value counts —
/// notoriously, a proxy or log taking the first and the application taking the
/// last. Refusing the request outright removes the disagreement entirely, and
/// no legitimate client sends a duplicate.
#[derive(Debug, Default, Clone)]
pub struct FormFields(BTreeMap<String, String>);

impl FormFields {
    /// Parse a urlencoded payload.
    ///
    /// `Err` carries a short, caller-safe reason that never echoes the input.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let mut fields = BTreeMap::new();
        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some(kv) => kv,
                // A bare key with no `=` is not something any conforming client
                // sends; treating it as an empty value would silently accept a
                // malformed request.
                None => return Err("malformed parameter"),
            };
            let key = decode_component(key).ok_or("undecodable parameter name")?;
            let value = decode_component(value).ok_or("undecodable parameter value")?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate parameter");
            }
        }
        Ok(Self(fields))
    }

    /// A field's value, with blank treated as absent — the crate-wide rule that
    /// an empty value is a missing one, applied here so `state=` and an omitted
    /// `state` behave identically.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }
}

/// Percent-decode one urlencoded component, mapping `+` to a space.
fn decode_component(raw: &str) -> Option<String> {
    let plus_decoded = raw.replace('+', " ");
    percent_decode_str(&plus_decoded).decode_utf8().ok().map(|c| c.into_owned())
}

/// The authorization request parameters, as presented.
///
/// Every field is optional at this stage — absence is a validation failure with
/// a specific OAuth error code, not a parse failure, and the two are reported
/// very differently (an error page versus a redirect).
#[derive(Debug, Default, Clone)]
pub struct AuthorizeParams {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub response_type: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub resource: Option<String>,
}

impl AuthorizeParams {
    /// Extract the authorization parameters from a parsed payload.
    pub fn from_fields(fields: &FormFields) -> Self {
        let take = |name: &str| fields.get(name).map(str::to_string);
        Self {
            client_id: take("client_id"),
            redirect_uri: take("redirect_uri"),
            response_type: take("response_type"),
            scope: take("scope"),
            state: take("state"),
            code_challenge: take("code_challenge"),
            code_challenge_method: take("code_challenge_method"),
            resource: take("resource"),
        }
    }
}

// ---------------------------------------------------------------------------
// Redirect URI matching
// ---------------------------------------------------------------------------

/// A loopback URI, split into the parts the exception treats differently.
#[derive(Debug, PartialEq, Eq)]
struct LoopbackUri<'a> {
    /// The host, which must match EXACTLY between the two URIs — `localhost`
    /// and `127.0.0.1` are different registrations, not synonyms.
    host: &'a str,
    /// Everything after the authority: path, query and fragment. Compared
    /// byte-for-byte.
    rest: &'a str,
}

/// Parse a URI as an RFC 8252 loopback redirect, or `None` if it is not one.
///
/// Strict by construction:
/// - the scheme must be exactly `http` (RFC 8252's loopback redirect is plain
///   HTTP to a local port; an `https` loopback registration gets no port
///   flexibility, because nothing needs it);
/// - the authority must carry no userinfo — `http://127.0.0.1@attackerhost/`
///   is a URI whose HOST is `attacker.test`, and a naive prefix check reads it
///   as loopback. Refusing any `@` in the authority closes that outright;
/// - the host must be one of [`LOOPBACK_HOSTS`] by exact string equality;
/// - the port, if present, must be all digits and non-empty.
fn parse_loopback(uri: &str) -> Option<LoopbackUri<'_>> {
    let after_scheme = uri.strip_prefix("http://")?;
    let authority_end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let rest = &after_scheme[authority_end..];

    if authority.contains('@') {
        return None;
    }

    let (host, port) = if let Some(bracket) = authority.find(']') {
        // IPv6 literal: the host is `[...]` and anything after it is `:port`.
        let host = &authority[..=bracket];
        let remainder = &authority[bracket + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            Some(remainder.strip_prefix(':')?)
        };
        (host, port)
    } else if let Some((host, port)) = authority.split_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };

    if !LOOPBACK_HOSTS.contains(&host) {
        return None;
    }
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }

    Some(LoopbackUri { host, rest })
}

/// Whether a presented redirect URI matches a registered one.
///
/// EXACT string comparison, with the single RFC 8252 loopback exception
/// described in the module docs. See [`parse_loopback`] for what "loopback"
/// admits and what it refuses.
pub fn redirect_uri_matches(registered: &str, presented: &str) -> bool {
    if registered == presented {
        return true;
    }
    match (parse_loopback(registered), parse_loopback(presented)) {
        (Some(reg), Some(pres)) => reg.host == pres.host && reg.rest == pres.rest,
        // If either side is not a loopback URI, the exception does not apply
        // and the exact comparison above has already decided.
        _ => false,
    }
}

/// The authority (`host` or `host:port`) of a URI, for display.
///
/// Best-effort and display-only: it is rendered escaped into the consent page
/// so the human can see where their authorization is going. It is never used
/// for a matching decision.
fn uri_authority(uri: &str) -> String {
    let after_scheme = match uri.split_once("://") {
        Some((_, rest)) => rest,
        None => uri,
    };
    let end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    if authority.is_empty() {
        uri.to_string()
    } else {
        authority.to_string()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A fully validated authorization request. Constructed only by [`validate`].
#[derive(Debug, Clone)]
pub struct ValidatedRequest {
    /// The client's internal row id, for the store.
    pub client_row_id: Uuid,
    /// The public `client_id`, echoed into the forms.
    pub client_id: String,
    /// The client's display name, shown to the human.
    pub client_name: String,
    /// The PRESENTED redirect URI — not the registered one it matched. This is
    /// what the code is bound to and what the token endpoint will compare
    /// against, per RFC 6749 §4.1.3, so binding the registered value instead
    /// would break redemption for every loopback client.
    pub redirect_uri: String,
    /// The redirect authority, for prominent display.
    pub redirect_authority: String,
    /// True when EVERY registered redirect for this client is a loopback URI.
    pub loopback_only: bool,
    pub state: Option<String>,
    pub code_challenge: String,
    /// The NARROWED scope, canonicalized (deduplicated, sorted, space
    /// separated). Consent rows key on this string, so canonicalizing means
    /// `"mcp offline_access"` and `"offline_access mcp"` are one consent rather
    /// than two.
    pub scope: String,
    pub resource: String,
}

impl ValidatedRequest {
    /// The scopes, as a list, for display.
    pub fn scope_list(&self) -> Vec<String> {
        self.scope.split(' ').filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    /// The hidden fields that carry this request across a form submission.
    ///
    /// Built from the VALIDATED values, so what the human approved is exactly
    /// what is re-submitted — and it is re-validated again on arrival anyway.
    fn hidden_fields(&self) -> Vec<(String, String)> {
        let mut fields = vec![
            ("client_id".to_string(), self.client_id.clone()),
            ("redirect_uri".to_string(), self.redirect_uri.clone()),
            ("response_type".to_string(), "code".to_string()),
            ("scope".to_string(), self.scope.clone()),
            ("code_challenge".to_string(), self.code_challenge.clone()),
            ("code_challenge_method".to_string(), "S256".to_string()),
            ("resource".to_string(), self.resource.clone()),
        ];
        if let Some(state) = &self.state {
            fields.push(("state".to_string(), state.clone()));
        }
        fields
    }
}

/// What to do with an authorization request.
#[derive(Debug)]
pub enum AuthorizeOutcome {
    /// Render a terminal error page. **No `Location` header, ever.** Reached
    /// only for an unknown client or an unregistered redirect URI — the two
    /// cases where redirecting would be an open redirect.
    ErrorPage { title: String, detail: String },
    /// Redirect to the (already validated) redirect URI with an OAuth error,
    /// `state` echoed if present, and RFC 9207 `iss`.
    ErrorRedirect { location: String },
    /// The request is well-formed and attributable. Proceed to login/consent.
    Proceed(Box<ValidatedRequest>),
}

/// Validate an authorization request.
///
/// `client` is the result of the store lookup — `None` for unknown OR disabled,
/// which [`OauthStore::find_active_client`] deliberately collapses so a
/// disabled client behaves exactly like one that never existed.
///
/// The check ORDER is the security property; see the module docs. It is written
/// as one straight-line function precisely so the order is visible and a future
/// edit that moves a check has to move it past a comment saying why not.
pub fn validate(
    params: &AuthorizeParams,
    client: Option<&Client>,
    config: &AuthorizeConfig,
) -> AuthorizeOutcome {
    // ---- 1. The client. No redirect is possible before this succeeds. ----
    let Some(client_id) = params.client_id.as_deref() else {
        return AuthorizeOutcome::ErrorPage {
            title: "Missing client".to_string(),
            detail: "The request did not name a client, so it cannot be attributed to any \
                     registered application."
                .to_string(),
        };
    };
    let Some(client) = client else {
        // The message does not repeat the submitted `client_id`. Reflecting an
        // attacker-chosen value into a page is a needless injection surface
        // (the templates escape it, but not reflecting it is better), and it
        // tells a prober nothing they did not already send.
        return AuthorizeOutcome::ErrorPage {
            title: "Unknown client".to_string(),
            detail: "That application is not registered with this server, or has been \
                     disabled. Nothing was sent back to it."
                .to_string(),
        };
    };
    // Belt and braces: `find_active_client` filters disabled clients, but this
    // function is also callable with a client the caller obtained another way.
    if client.disabled {
        return AuthorizeOutcome::ErrorPage {
            title: "Unknown client".to_string(),
            detail: "That application is not registered with this server, or has been \
                     disabled. Nothing was sent back to it."
                .to_string(),
        };
    }

    // ---- 2. The redirect URI. Still no redirect until this succeeds. ----
    //
    // `redirect_uri` is REQUIRED even when the client registered exactly one.
    // OAuth 2.1 permits omitting it in that case, but the permission buys
    // nothing here (every real client sends it) and costs a branch in which
    // this server picks a destination the request never named.
    let Some(presented) = params.redirect_uri.as_deref() else {
        return AuthorizeOutcome::ErrorPage {
            title: "Missing redirect address".to_string(),
            detail: "The request did not say where to return the authorization, so there is \
                     nowhere safe to send it."
                .to_string(),
        };
    };
    if !client.redirect_uris.iter().any(|reg| redirect_uri_matches(reg, presented)) {
        return AuthorizeOutcome::ErrorPage {
            title: "Unregistered redirect address".to_string(),
            detail: "The address the application asked to be returned to is not one \
                     registered for it. This is exactly the shape of a stolen-authorization \
                     attempt, so nothing was sent there."
                .to_string(),
        };
    }

    // ---- 2b. The matched destination must not pre-empt a response parameter.
    //
    // Still an error PAGE, not a redirect, and deliberately so: this is the
    // last part of establishing the destination, not the first check performed
    // against an established one. A URI carrying its own `code=` or `state=`
    // would make the response ambiguous — the client's query parser, not this
    // server, would decide which value wins — so redirecting there is exactly
    // what must not happen. [`build_redirect`] independently drops such a
    // parameter, so the response is unambiguous even if this check is somehow
    // bypassed; this one exists to make the misconfiguration VISIBLE rather
    // than silently dropping something an operator deliberately registered.
    if redirect_uri_has_reserved_parameter(presented) {
        return AuthorizeOutcome::ErrorPage {
            title: "Unusable redirect address".to_string(),
            detail: "The registered redirect address already carries a query parameter \
                     reserved for the authorization response (one of code, state, iss, \
                     error or error_description). That would make the response ambiguous, \
                     so nothing was sent there. Re-register the application without it."
                .to_string(),
        };
    }

    // From here on the destination is established, so a redirect is safe.
    let state = params.state.clone();
    let issuer = config.issuer();
    let fail = |code: &str, description: &str| AuthorizeOutcome::ErrorRedirect {
        location: error_redirect(presented, state.as_deref(), issuer, code, description),
    };

    // ---- 3. Response type. OAuth 2.1 has only one. ----
    match params.response_type.as_deref() {
        Some("code") => {}
        _ => {
            return fail(
                "unsupported_response_type",
                "only the authorization code response type is supported",
            )
        }
    }

    // ---- 4. PKCE. S256 required; `plain` and absent are both refused. ----
    //
    // `plain` puts the verifier on the wire in the authorization request, so it
    // protects against nothing an attacker who can see that request cares
    // about. RFC 7636 also makes `plain` the DEFAULT when the method is
    // omitted, which is why an absent method is refused rather than assumed:
    // silently defaulting would mean accepting `plain`.
    let Some(challenge) = params.code_challenge.as_deref() else {
        return fail("invalid_request", "a PKCE code_challenge is required");
    };
    if params.code_challenge_method.as_deref() != Some("S256") {
        return fail(
            "invalid_request",
            "code_challenge_method must be S256; plain and omitted are not accepted",
        );
    }
    if challenge.len() < CHALLENGE_MIN_LEN
        || challenge.len() > CHALLENGE_MAX_LEN
        || !challenge.bytes().all(is_base64url_char)
    {
        return fail(
            "invalid_request",
            "code_challenge must be a base64url-encoded SHA-256 digest",
        );
    }

    // ---- 5. RFC 8707 resource. Required, and must be OUR resource. ----
    //
    // This is what makes the issued token audience-bound: a token minted here
    // must not be replayable at a federated peer, and a request naming a peer's
    // resource must not be honoured here. `invalid_target` is RFC 8707's own
    // error code for both.
    let Some(resource) = params.resource.as_deref() else {
        return fail("invalid_target", "the resource parameter is required");
    };
    if resource != config.resource() {
        return fail(
            "invalid_target",
            "the requested resource is not the one this server issues tokens for",
        );
    }

    // ---- 6. Scope. Narrowed, never widened. ----
    let scope = narrow_scope(params.scope.as_deref());
    if scope.is_empty() {
        return fail("invalid_scope", "none of the requested scopes are supported here");
    }

    let loopback_only = !client.redirect_uris.is_empty()
        && client.redirect_uris.iter().all(|uri| parse_loopback(uri).is_some());

    AuthorizeOutcome::Proceed(Box::new(ValidatedRequest {
        client_row_id: client.id,
        client_id: client_id.to_string(),
        client_name: client.name.clone(),
        redirect_uri: presented.to_string(),
        redirect_authority: uri_authority(presented),
        loopback_only,
        state: params.state.clone(),
        code_challenge: challenge.to_string(),
        scope,
        resource: resource.to_string(),
    }))
}

/// Whether a byte is in the base64url alphabet a PKCE challenge uses.
fn is_base64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Narrow a requested scope string to the supported set.
///
/// Unknown scopes are DROPPED, not rejected and not granted. Returns a
/// canonical form: deduplicated, sorted, space separated. An absent or blank
/// request yields [`DEFAULT_SCOPE`]; a request naming only unsupported scopes
/// yields an empty string, which [`validate`] turns into `invalid_scope`.
pub fn narrow_scope(requested: Option<&str>) -> String {
    let Some(requested) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return DEFAULT_SCOPE.to_string();
    };
    let mut kept: Vec<&str> = requested
        .split_whitespace()
        .filter(|s| SUPPORTED_SCOPES.contains(s))
        .collect();
    kept.sort_unstable();
    kept.dedup();
    kept.join(" ")
}

// ---------------------------------------------------------------------------
// Redirect construction
// ---------------------------------------------------------------------------

/// The query-parameter names an authorization response carries.
///
/// A registered redirect URI whose own query already contains one of these is
/// refused (see [`redirect_uri_has_reserved_parameter`]), and any such
/// parameter surviving in a base URI is dropped by [`build_redirect`].
pub const RESERVED_RESPONSE_PARAMS: [&str; 5] =
    ["code", "state", "iss", "error", "error_description"];

/// Whether a redirect URI's own query string already carries a reserved
/// response parameter name.
///
/// ## Why this is refused rather than merged around
/// Review round 1 (gpt56) found the real problem: a registered redirect of the
/// form `https://client.test/cb?code=x` produced a response URL containing
/// `code` twice. Which one the client reads is then decided by its query
/// parser's tie-breaking — first wins, last wins, or an array — and NOT by this
/// server. That is a parameter-injection primitive handed to whoever registered
/// the URI, and it is worth nothing to a legitimate client.
///
/// [`build_redirect`] now merges properly and drops these names, so the
/// response is unambiguous either way. This check exists as the second layer,
/// and it is the one that makes the situation VISIBLE: a silent drop would let
/// an operator register a URI whose query quietly stops arriving. Refusing says
/// so, at the point where the redirect target is being established and before
/// anything is sent anywhere.
///
/// Names are compared case-sensitively, which is correct: query parameter names
/// are case-sensitive, and `Code` is a different parameter from `code` — one an
/// OAuth client will not read as a response parameter at all.
pub fn redirect_uri_has_reserved_parameter(uri: &str) -> bool {
    // Only the query is examined. A reserved word in the PATH or the fragment
    // is not a query parameter and cannot collide with one.
    let without_fragment = uri.split('#').next().unwrap_or(uri);
    let Some((_, query)) = without_fragment.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        let name = pair.split('=').next().unwrap_or(pair);
        // Decoded before comparison so a `%63ode=x` encoding of `code` cannot
        // walk past a literal match and reappear as `code` at the client.
        let decoded = decode_component(name).unwrap_or_else(|| name.to_string());
        RESERVED_RESPONSE_PARAMS.contains(&decoded.as_str())
    })
}

/// Merge the authorization response parameters into a redirect URI.
///
/// Values are percent-encoded to the RFC 3986 unreserved set. That is what
/// makes `state` survive verbatim: a state containing `&`, `=`, `#` or a space
/// is encoded here and decoded by the client to exactly the bytes that were
/// sent, whereas splicing it in raw would let it forge additional parameters in
/// the redirect the client parses.
///
/// A parameter whose value is `None` is OMITTED entirely rather than emitted
/// empty — an absent `state` must not become `state=`, which a client is
/// entitled to read as "the server echoed an empty state" and, in a strict
/// implementation, to reject.
///
/// ## Why this parses instead of concatenating
/// The first revision appended with a `?` or an `&` depending on whether the
/// base already contained a `?`. Review round 1 found two faults in that, and
/// both are fixed by decomposing the URI properly:
///
/// 1. **Duplicate response parameters.** A registered redirect already carrying
///    `?code=…` yielded a URL with two `code` parameters and no defined winner.
///    An existing parameter whose name is reserved is now DROPPED, so the value
///    this server computed is the only one present. (It should never get this
///    far — [`validate`] refuses such a URI outright — but a function that
///    builds a security-relevant URL should not depend on its caller having
///    checked.)
/// 2. **A fragment ending up before the query.** Appending to
///    `https://client.test/cb#section` produced `…/cb#section?code=…`, where
///    the whole query is part of the fragment and no query parameter reaches
///    the server-side parser at all. The fragment is now split off first and
///    re-attached last, which is the only ordering RFC 3986 allows.
///
/// Existing NON-reserved pairs are carried through byte-for-byte rather than
/// decoded and re-encoded: they were registered in that form, and a re-encoding
/// round trip is a chance to change a value that was already correct.
fn build_redirect(base: &str, params: &[(&str, Option<&str>)]) -> String {
    let (without_fragment, fragment) = match base.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (base, None),
    };
    let (target, existing_query) = match without_fragment.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (without_fragment, None),
    };

    let mut query = String::new();
    let push_pair = |query: &mut String, pair: &str| {
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(pair);
    };

    if let Some(existing) = existing_query {
        for pair in existing.split('&').filter(|pair| !pair.is_empty()) {
            let name = pair.split('=').next().unwrap_or(pair);
            let decoded = decode_component(name).unwrap_or_else(|| name.to_string());
            if RESERVED_RESPONSE_PARAMS.contains(&decoded.as_str()) {
                continue;
            }
            push_pair(&mut query, pair);
        }
    }

    for (name, value) in params.iter() {
        let value: &str = match value {
            Some(value) => value,
            None => continue,
        };
        let encoded = utf8_percent_encode(value, UNRESERVED).to_string();
        push_pair(&mut query, &format!("{name}={encoded}"));
    }

    let mut out = String::from(target);
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query);
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Build an OAuth error redirect.
///
/// `iss` is present here as well as on the success redirect. RFC 9207 exists
/// because a client that talks to several authorization servers can otherwise
/// be tricked into taking a response from one as a response from another; an
/// error response is just as capable of carrying that confusion as a successful
/// one, so omitting `iss` from errors would leave the mix-up half open.
fn error_redirect(
    redirect_uri: &str,
    state: Option<&str>,
    issuer: &str,
    code: &str,
    description: &str,
) -> String {
    build_redirect(
        redirect_uri,
        &[
            ("error", Some(code)),
            ("error_description", Some(description)),
            ("state", state),
            ("iss", Some(issuer)),
        ],
    )
}

/// Build the success redirect carrying the authorization code.
fn success_redirect(
    redirect_uri: &str,
    code: &str,
    state: Option<&str>,
    issuer: &str,
) -> String {
    build_redirect(
        redirect_uri,
        &[("code", Some(code)), ("state", state), ("iss", Some(issuer))],
    )
}

// ---------------------------------------------------------------------------
// Secret generation
// ---------------------------------------------------------------------------

/// Generate a high-entropy, URL-safe token: an authorization code, a session
/// identifier, or a CSRF token.
///
/// ## Where the entropy comes from
/// Three v4 UUIDs concatenated: 48 bytes, of which 366 bits are CSPRNG output
/// (a v4 UUID fixes 6 of its 128 bits as version and variant markers). That is
/// comfortably past the 256-bit floor this item requires, with room to spare
/// even if one counted only two of the three.
///
/// The obvious implementation reaches for `rand::rng().fill_bytes(..)`. It is
/// not used because `rand` in this tree is a version whose API differs from
/// every example, and fighting that at the point of generating a security
/// token is how a weakened generator gets committed. `uuid`'s v4 constructor is
/// already a dependency, already backed by the operating system's CSPRNG
/// through `getrandom`, and has one obvious call. Entropy is INCREASED to make
/// the substitution safe rather than traded away for convenience — the failure
/// mode this avoids is exactly "used a weaker source because the strong one's
/// API was awkward".
pub fn new_high_entropy_token() -> String {
    let mut bytes = Vec::with_capacity(48);
    for _ in 0..3 {
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// The Content-Security-Policy every page here is served with.
///
/// `default-src 'none'` means the page may load nothing at all; `style-src
/// 'unsafe-inline'` re-permits the one inline `<style>` block; `form-action
/// 'self'` stops any injected form from posting credentials elsewhere;
/// `frame-ancestors 'none'` stops the consent screen from being framed and
/// clickjacked, which is the specific attack that turns "Allow" into a button
/// the user did not know they were pressing.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
                   frame-ancestors 'none'; base-uri 'none'";

fn html_response(status: StatusCode, body: String, cookie: Option<String>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        // A login or consent page in a shared cache is a login or consent page
        // served to the next person.
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::PRAGMA, "no-cache")
        .header(header::CONTENT_SECURITY_POLICY, CSP)
        .header(header::REFERRER_POLICY, "no-referrer")
        .header("X-Frame-Options", "DENY")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(cookie) = cookie {
        builder = builder.header(header::SET_COOKIE, cookie);
    }
    builder
        .body(Body::from(body))
        // The builder only fails on an invalid header value, all of which are
        // constants here except the cookie (which this module constructs). A
        // bare 500 with no body is the right degradation: it discloses nothing.
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("an empty 500 response is always constructible")
        })
}

/// A 302 to `location`, always clearing the login cookie.
///
/// Every redirect out of this module is terminal for the session — success,
/// refusal or protocol error — so the cookie is cleared on all of them rather
/// than only on success. A session cookie that outlives its flow is a session
/// cookie waiting to be replayed.
fn redirect_response(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::SET_COOKIE, session::clear_cookie())
        .header(header::REFERRER_POLICY, "no-referrer")
        .body(Body::empty())
        .unwrap_or_else(|_| {
            // An unconstructible `Location` means the URI held a control
            // character. It came from a registered redirect URI plus values
            // this module percent-encoded, so this is unreachable in practice —
            // but the fallback must not redirect anywhere.
            html_response(
                StatusCode::BAD_REQUEST,
                templates::error_page(
                    "Invalid redirect address",
                    "The registered redirect address for this application cannot be used.",
                ),
                Some(session::clear_cookie()),
            )
        })
}

impl AuthorizeOutcome {
    /// Render a non-`Proceed` outcome. Panics are impossible: `Proceed` is
    /// handled by the caller, and rendering it here would be a bug, so it is
    /// rendered as a generic error rather than unwrapped.
    fn into_response(self) -> Response {
        match self {
            AuthorizeOutcome::ErrorPage { title, detail } => html_response(
                StatusCode::BAD_REQUEST,
                templates::error_page(&title, &detail),
                Some(session::clear_cookie()),
            ),
            AuthorizeOutcome::ErrorRedirect { location } => redirect_response(&location),
            AuthorizeOutcome::Proceed(_) => html_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                templates::error_page(
                    "Internal error",
                    "The request could not be completed. Try again from the application.",
                ),
                Some(session::clear_cookie()),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Endpoint state and routing
// ---------------------------------------------------------------------------

/// Shared state for the three handlers.
pub struct AuthorizeState {
    store: OauthStore,
    config: AuthorizeConfig,
    session_key: SessionKey,
    /// The door-wide limiter (TERM #633). Shared with every other OAuth
    /// endpoint, so the login budget is defined in exactly one place and
    /// inherits the subject-over-address invariant.
    limiter: Arc<OauthRateLimiter>,
}

/// How long a spent login-session claim is retained — comfortably longer than
/// the session's own TTL, so a claim is never forgotten while the token that
/// produced it could still verify.
const CLAIM_RETENTION_SECONDS: i64 = 900;

impl AuthorizeState {
    /// Build the endpoint state with the door's default budgets.
    pub fn new(store: OauthStore, config: AuthorizeConfig, session_key: SessionKey) -> Self {
        Self::with_limiter(store, config, session_key, Arc::new(OauthRateLimiter::with_defaults()))
    }

    /// Build with an injected limiter, for tests and for a future shared
    /// (Redis-backed) limiter — the seam `crate::gateway_framework::rate_limit`
    /// exists to provide.
    pub fn with_limiter(
        store: OauthStore,
        config: AuthorizeConfig,
        session_key: SessionKey,
        limiter: Arc<OauthRateLimiter>,
    ) -> Self {
        Self { store, config, session_key, limiter }
    }

    /// Claim a login session as spent. `Ok(true)` only for the caller that
    /// claimed it; `Ok(false)` for every replay; `Err` when the claim could not
    /// be decided at all.
    ///
    /// ## Why this is a database row and not a field on this struct
    /// The first revision kept the spent identifiers in a `Mutex<HashMap>` here.
    /// Review round 1 (gpt56) rejected that, and was right: Terminus runs with
    /// more than one replica, so the same signed session cookie presented to two
    /// instances is unspent at both and each issues its own authorization code.
    /// The property — one authentication, at most one code — was correct; the
    /// place it was enforced was the weaker one. Everything else in this item is
    /// careful about single use, and this was the exception.
    ///
    /// [`OauthStore::claim_login_session`] makes the claim an `INSERT … ON
    /// CONFLICT DO NOTHING` arbitrated by a primary key, so exactly one caller
    /// anywhere in the cluster wins — the same one-statement check-and-claim
    /// shape as `consume_auth_code`.
    ///
    /// The `jti` is hashed rather than stored: it is carried inside a live
    /// session cookie, and no table in this schema holds anything presentable.
    ///
    /// An `Err` is propagated rather than collapsed into `false`, so the caller
    /// can distinguish "this session was already used" (send the human back to
    /// the login form) from "the guard is unavailable" (a server error). Both
    /// refuse to issue a code — a guard that opens when its store is unreachable
    /// is not a guard — but they are not the same event and must not be
    /// reported to the operator as if they were.
    async fn claim_session(&self, jti: &str) -> Result<bool, ToolError> {
        self.store
            .claim_login_session(&SecretHash::of(jti), CLAIM_RETENTION_SECONDS)
            .await
    }
}

/// The router for the interactive endpoints.
///
/// Mounted under `/oauth` by the caller, which is also the cookie's path scope.
pub fn router(state: Arc<AuthorizeState>) -> Router {
    Router::new()
        .route("/authorize", get(get_authorize))
        .route("/login", post(post_login))
        .route("/consent", post(post_consent))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Look the client up and validate, or produce the outcome to render.
async fn validated(
    state: &AuthorizeState,
    params: &AuthorizeParams,
) -> AuthorizeOutcome {
    let client = match params.client_id.as_deref() {
        Some(id) => match state.store.find_active_client(id).await {
            Ok(client) => client,
            Err(_) => {
                // A store failure must not be reported as "unknown client" —
                // that would tell a prober a real client is unknown whenever
                // the database hiccups — and must not redirect, because the
                // client was never established.
                return AuthorizeOutcome::ErrorPage {
                    title: "Temporarily unavailable".to_string(),
                    detail: "This server could not check the application's registration. \
                             Try again shortly."
                        .to_string(),
                };
            }
        },
        None => None,
    };
    validate(params, client.as_ref(), &state.config)
}

async fn get_authorize(
    State(state): State<Arc<AuthorizeState>>,
    RawQuery(query): RawQuery,
) -> Response {
    // No rate-limit call here: `crate::oauth::mount`'s `charge_address_budget`
    // layer has already charged this request's per-address budget, before this
    // handler ran and before anything was parsed. This endpoint names no
    // subject (the `client_id` is in the query, which is not yet read), so the
    // address dimension is the whole of its limiting — see the mounted-route
    // contract in that module.
    let fields = match FormFields::parse(query.as_deref().unwrap_or("")) {
        Ok(fields) => fields,
        Err(reason) => {
            // A refusal before anything is attributed. Audited for the same
            // reason the token and revoke endpoints audit theirs: an
            // unaudited pre-auth denial is one an operator cannot see. The
            // record carries no part of the query — it is caller-controlled
            // text, and the endpoint plus the reason is the whole operational
            // signal.
            OauthAuditRecord::new(OauthEvent::AuthorizationDenied)
                .endpoint(OauthEndpoint::Authorize)
                .reason(DenialReason::MalformedRequest)
                .detail(AuditDetail::RefusedBeforeParsing)
                .emit();
            // A malformed query cannot be attributed to a client, so it gets an
            // error page and no redirect.
            return html_response(
                StatusCode::BAD_REQUEST,
                templates::error_page("Malformed request", reason),
                Some(session::clear_cookie()),
            );
        }
    };
    let params = AuthorizeParams::from_fields(&fields);
    let request = match validated(&state, &params).await {
        AuthorizeOutcome::Proceed(request) => request,
        other => return other.into_response(),
    };

    // A fresh authorization request always starts at the login form, even if a
    // session cookie is present. The cookie proves an earlier authentication,
    // not an intent to authorize THIS request, and re-using it here would let a
    // second `/authorize` ride an already-authenticated browser straight to
    // consent.
    html_response(
        StatusCode::OK,
        templates::login_page(&LoginContext {
            client_name: &request.client_name,
            redirect_host: &request.redirect_authority,
            loopback_only: request.loopback_only,
            notice: None,
            hidden: &request.hidden_fields(),
        }),
        Some(session::clear_cookie()),
    )
}

/// Read and parse a urlencoded request body.
fn parse_body(headers: &HeaderMap, body: &str) -> Result<FormFields, &'static str> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Checked explicitly rather than relying on an extractor: a POST that is
    // not a form submission is not something this endpoint should try to read.
    if !content_type.starts_with("application/x-www-form-urlencoded") {
        return Err("expected a form submission");
    }
    FormFields::parse(body)
}

async fn post_login(
    State(state): State<Arc<AuthorizeState>>,
    cleared: crate::oauth::limits::AddressCleared,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    body: String,
) -> Response {
    let fields = match parse_body(&headers, &body) {
        Ok(fields) => fields,
        Err(reason) => {
            OauthAuditRecord::new(OauthEvent::LoginDenied)
                .endpoint(OauthEndpoint::Login)
                .reason(DenialReason::MalformedRequest)
                .detail(AuditDetail::RefusedBeforeParsing)
                .emit();
            return html_response(
                StatusCode::BAD_REQUEST,
                templates::error_page("Malformed request", reason),
                Some(session::clear_cookie()),
            )
        }
    };
    let params = AuthorizeParams::from_fields(&fields);

    // Re-validated from scratch: the hidden fields are a browser convenience,
    // never authority.
    let request = match validated(&state, &params).await {
        AuthorizeOutcome::Proceed(request) => request,
        other => return other.into_response(),
    };

    let submitted_account = fields.get("account").unwrap_or("").to_string();
    let submitted_pw = fields.get("pw").unwrap_or("").to_string();

    // Source address for the per-address budget. Resolved by RMCP-09's edge
    // and handed down as `ResolvedClientIp`, falling back to the socket peer
    // when this process is reached on a private listener with no edge in front.
    // Never from `X-Forwarded-For` read here: a header the caller controls is a
    // rate-limit key the caller can rotate at will. The edge is the ONE place
    // that decides which hop of a forwarded chain may be attributed, and
    // re-deciding it here would be a second, divergent answer behind the same
    // door.
    let source = resolved_source_for(&extensions);

    // The SUBJECT dimension only. The address dimension was charged by
    // `mount::charge_address_budget` before this handler ran — before the body
    // was parsed, which is the property round 9 established: a malformed login
    // post must cost budget too, and this handler cannot charge earlier than its
    // own first line.
    //
    // Charged here, before any credential work, so the limiter cannot itself
    // become an oracle: an unknown account consumes subject budget exactly like
    // a known one. Reached only when the address check ALLOWED, which preserves
    // the short-circuit — a flood from one address cannot drain the named
    // account's budget beyond that address's own, the property the old
    // same-sized pair of buckets did not have.
    let outcome = state.limiter.check_subject(&cleared, &submitted_account).await;
    if outcome.is_limited() {
        // The OAuth-vocabulary record. `limits` emits its own RateLimited entry
        // for the throttle itself; this one ties it to the login decision.
        OauthAuditRecord::new(OauthEvent::LoginDenied)
            .endpoint(OauthEndpoint::Login)
            .from_address(source)
            .reason(DenialReason::RateLimited)
            .emit();
        AuditEntry::new(
            submitted_account.as_str(),
            LOGIN_ACTION,
            ActionKind::Admin,
            AuditResult::DeniedRateLimited,
            Some("login throttled"),
        )
        .log();
        return throttled_response(&outcome);
    }

    // The account lookup. `find_active_account_by_name` returns `None` for
    // unknown AND for disabled, which is what keeps those two indistinguishable
    // all the way down this path.
    let account = match state.store.find_active_account_by_name(&submitted_account).await {
        Ok(account) => account,
        Err(_) => None,
    };

    // The constant-time shape: verify against a real hash either way, and only
    // then AND in whether an account was found. Written in this order (not
    // `account.is_some() && verify(..)`) because `&&` would short-circuit and
    // skip the ~40ms of argon2 work for an unknown account — a timing oracle a
    // remote attacker can measure in a handful of requests.
    let stored = account
        .as_ref()
        .map(|a| a.password_hash.clone())
        .unwrap_or_else(|| password::dummy_hash().to_string());
    let password_ok = password::verify_password(&submitted_pw, &stored);
    let authenticated = password_ok && account.is_some();

    if !authenticated {
        // The OAuth-vocabulary record. `BadCredentials` collapses "no such
        // account" and "wrong password" into one reason for the same reason the
        // response does — a trail that separates them is an existence oracle
        // for anyone who can read it, and one refactor away from becoming one
        // in the response too.
        OauthAuditRecord::new(OauthEvent::LoginDenied)
            .endpoint(OauthEndpoint::Login)
            .from_address(source)
            .reason(DenialReason::BadCredentials)
            .emit();
        AuditEntry::new(
            submitted_account.as_str(),
            LOGIN_ACTION,
            ActionKind::Admin,
            AuditResult::DeniedNotAllowlisted,
            // Deliberately does NOT record whether the account existed. An
            // audit log an attacker can read is an oracle; one they cannot is
            // still a place where "user unknown" invites a support reply that
            // leaks the same thing.
            Some(&format!("sign-in failed from {source}")),
        )
        .log();
        return html_response(
            StatusCode::UNAUTHORIZED,
            templates::login_page(&LoginContext {
                client_name: &request.client_name,
                redirect_host: &request.redirect_authority,
                loopback_only: request.loopback_only,
                notice: Some(GENERIC_LOGIN_FAILURE),
                hidden: &request.hidden_fields(),
            }),
            Some(session::clear_cookie()),
        );
    }

    let account = account.expect("authentication implies an account was found");

    // Second factor: refuse rather than silently downgrade to one factor. See
    // `crate::oauth::password`'s module docs for why this cannot be verified
    // yet and why refusing is the correct direction to fail.
    if password::requires_unavailable_second_factor(account.totp_secret_enc.as_deref()) {
        AuditEntry::new(
            submitted_account.as_str(),
            LOGIN_ACTION,
            ActionKind::Admin,
            AuditResult::DeniedNotAllowlisted,
            Some(
                "second factor required but not verifiable by this door \
                 (totp_secret_enc subkey derivation lands in RMCP-08)",
            ),
        )
        .log();
        return html_response(
            StatusCode::FORBIDDEN,
            templates::error_page(
                "Second factor required, and not yet verifiable",
                "This account has a TOTP second factor. The sign-in was refused rather than \
                 allowed on the password alone \u{2014} this is a deliberate gate, not a fault. \
                 What is missing: the stored TOTP seed (rmcp_account.totp_secret_enc) is \
                 encrypted with a subkey derived from the OAuth signing key, and nothing \
                 derives that subkey yet, so the code cannot be checked against the seed. \
                 RMCP-08 provisions it. Until then, either use an account without a second \
                 factor for this connector, or wait for RMCP-08 \u{2014} do not clear \
                 totp_secret_enc to get past this, which would silently downgrade the \
                 account to one factor.",
            ),
            Some(session::clear_cookie()),
        );
    }

    // Identified by ACCOUNT ID, not by name: the id is what correlates with
    // every other record in this trail, and the name is the human's login
    // identifier.
    OauthAuditRecord::new(OauthEvent::LoginSucceeded)
        .endpoint(OauthEndpoint::Login)
        .account(account.id)
        .from_address(source)
        .detail(AuditDetail::LoginAccepted)
        .emit();
    AuditEntry::new(
        submitted_account.as_str(),
        LOGIN_ACTION,
        ActionKind::Admin,
        AuditResult::Success,
        Some(&format!("sign-in succeeded from {source}")),
    )
    .log();

    let jti = new_high_entropy_token();
    let csrf = new_high_entropy_token();
    let token = match session::mint(&state.session_key, account.id, &account.name, &jti, &csrf) {
        Ok(token) => token,
        Err(_) => return internal_error(),
    };
    let cookie = session::set_cookie(&token);

    // An unrevoked consent for this exact (account, client, scope) skips the
    // screen. Any NEW scope produces a different canonical string, finds no
    // consent, and re-prompts — which is the property that stops a client from
    // quietly enlarging what it holds.
    let already_consented = state
        .store
        .find_live_consent(account.id, request.client_row_id, &request.scope)
        .await
        .ok()
        .flatten()
        .is_some();

    if already_consented {
        let session = match session::verify(&state.session_key, &token) {
            Some(session) => session,
            None => return internal_error(),
        };
        return issue_code(&state, &request, &session).await;
    }

    render_consent(&state, &request, &account.name, &csrf, cookie).await
}

async fn post_consent(
    State(state): State<Arc<AuthorizeState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Charged by the `charge_address_budget` layer before this handler ran —
    // under the `Authorize` budget rather than one of its own, because consent
    // and the page that leads to it are one human action, and separate budgets
    // would let a flood of consent posts proceed while that page was throttled.
    // This handler names no subject, so the address dimension is the whole of
    // its limiting.
    let fields = match parse_body(&headers, &body) {
        Ok(fields) => fields,
        Err(reason) => {
            OauthAuditRecord::new(OauthEvent::AuthorizationDenied)
                .endpoint(OauthEndpoint::Authorize)
                .reason(DenialReason::MalformedRequest)
                .detail(AuditDetail::RefusedBeforeParsing)
                .emit();
            return html_response(
                StatusCode::BAD_REQUEST,
                templates::error_page("Malformed request", reason),
                Some(session::clear_cookie()),
            )
        }
    };
    let params = AuthorizeParams::from_fields(&fields);
    let request = match validated(&state, &params).await {
        AuthorizeOutcome::Proceed(request) => request,
        other => return other.into_response(),
    };

    // Session first: without one there is nobody to attribute a consent to.
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session = session::token_from_cookie_header(cookie_header)
        .and_then(|token| session::verify(&state.session_key, token));
    let Some(session) = session else {
        return restart_login(&request, "Your sign-in expired. Sign in again.");
    };

    // CSRF: the form must echo the value carried inside the signed cookie. A
    // cross-site attacker can make the browser SEND the cookie but cannot read
    // it, so they cannot populate the field.
    if !session.csrf_matches(fields.get("csrf").unwrap_or("")) {
        return restart_login(&request, "That form expired. Sign in again.");
    }

    if fields.get("approve") != Some("yes") {
        // A refusal is a legitimate, specified OAuth outcome and the client is
        // entitled to hear about it at its (already validated) redirect URI.
        return redirect_response(&error_redirect(
            &request.redirect_uri,
            request.state.as_deref(),
            state.config.issuer(),
            "access_denied",
            "the account holder refused this authorization",
        ));
    }

    if state
        .store
        .record_consent(session.account_id(), request.client_row_id, &request.scope)
        .await
        .is_err()
    {
        return internal_error();
    }

    issue_code(&state, &request, &session).await
}

/// Render the consent screen with the client's RESOLVED capability list.
async fn render_consent(
    state: &AuthorizeState,
    request: &ValidatedRequest,
    account_name: &str,
    csrf: &str,
    cookie: String,
) -> Response {
    // A store failure here must not fall back to rendering an empty (or worse,
    // an assumed-full) capability list: the human would be approving something
    // other than what they were shown.
    // `client_authorized_groups`, not `client_tool_groups`: the consent screen
    // must show what this client can ACTUALLY reach, and a group's patterns are
    // filtered by its owner's CURRENT authority at resolution (RMCP-12). Showing
    // a pattern that resolves to nothing would overstate the grant the human is
    // approving — and overstating it is the direction that makes them refuse a
    // safe connector, or approve while believing a wilder one.
    let groups = match state.store.client_authorized_groups(request.client_row_id).await {
        Ok(groups) => groups,
        Err(_) => return internal_error(),
    };
    let namespaces = match state.store.client_namespaces(request.client_row_id).await {
        Ok(namespaces) => namespaces,
        Err(_) => return internal_error(),
    };

    let groups: Vec<GroupSummary> = groups
        .into_iter()
        .map(|authorized| {
            let owner = authorized.owner;
            let g = authorized.group;
            GroupSummary {
                name: g.name,
                description: g.description,
                // Same rule as the resolver applies, so the screen and the
                // dispatch path cannot disagree about this client's reach.
                patterns: g
                    .patterns
                    .into_iter()
                    .filter(|raw| {
                        crate::oauth::groups::Pattern::parse_stored(raw).is_some_and(|parsed| {
                            crate::oauth::delegation::owner_may_hold(owner, parsed.shape())
                        })
                    })
                    .collect(),
            }
        })
        .collect();

    html_response(
        StatusCode::OK,
        templates::consent_page(&ConsentContext {
            client_name: &request.client_name,
            account_name,
            redirect_host: &request.redirect_authority,
            redirect_uri: &request.redirect_uri,
            loopback_only: request.loopback_only,
            scopes: &request.scope_list(),
            groups: &groups,
            namespaces: &namespaces,
            csrf,
            hidden: &request.hidden_fields(),
        }),
        Some(cookie),
    )
}

/// Mint, store and deliver an authorization code.
///
/// Three things happen here in a deliberate order:
/// 1. The session is CLAIMED, durably and cluster-wide. A replayed consent post
///    — at this replica or any other — finds it already claimed and gets no
///    second code for one human approval.
/// 2. The account is re-checked for the `disabled` flag against the live store.
///    An operator who disables an account mid-flow expects that to take effect
///    now, not at the next login — so the check is at ISSUANCE, not only at
///    authentication.
/// 3. The code is generated, hashed, and stored bound to all six fields.
///
/// The claim comes FIRST, before the code is minted. The reverse order would
/// let two concurrent replays each insert a code before either claim landed,
/// which is the precise failure this guard exists to prevent. The cost of this
/// ordering is that a later failure (a disabled account, a store error on the
/// code insert) leaves the session spent and the human has to sign in again —
/// a denial, never a widening, and the correct direction to fail.
async fn issue_code(
    state: &AuthorizeState,
    request: &ValidatedRequest,
    session: &LoginSession,
) -> Response {
    match state.claim_session(session.jti()).await {
        Ok(true) => {}
        Ok(false) => {
            // Not an OAuth error: the client's request was fine, the human's
            // browser re-posted. Sending them back to the login form is honest
            // and mints nothing.
            return restart_login(request, "That approval was already used. Sign in again.");
        }
        Err(_) => {
            // The guard could not be consulted. Refuse rather than proceed: an
            // unavailable single-use check is not a passed one.
            return internal_error();
        }
    }

    let account = match state.store.find_active_account_by_name(session.account_name()).await {
        Ok(Some(account)) if account.id == session.account_id() => account,
        Ok(_) => {
            // Disabled, deleted, or renamed onto a different row since login.
            AuditEntry::new(
                session.account_name(),
                LOGIN_ACTION,
                ActionKind::Admin,
                AuditResult::DeniedNotAllowlisted,
                Some("account no longer active at code issuance"),
            )
            .log();
            return redirect_response(&error_redirect(
                &request.redirect_uri,
                request.state.as_deref(),
                state.config.issuer(),
                "access_denied",
                "the account is no longer active",
            ));
        }
        Err(_) => return internal_error(),
    };

    let code = new_high_entropy_token();
    if state
        .store
        .insert_auth_code(
            &SecretHash::of(&code),
            request.client_row_id,
            account.id,
            &request.redirect_uri,
            &request.resource,
            &request.code_challenge,
            &request.scope,
            CODE_TTL_SECONDS,
        )
        .await
        .is_err()
    {
        return internal_error();
    }

    // The authorization DECISION, in OAuth vocabulary. The code itself never
    // appears — the record names the account, the client row, and the fact that
    // a code was issued, which is what an operator reconstructing "who granted
    // this connector access, and when" actually needs.
    OauthAuditRecord::new(OauthEvent::AuthorizationGranted)
        .endpoint(OauthEndpoint::Authorize)
        .account(account.id)
        .client_uuid(request.client_row_id)
        .detail(AuditDetail::AuthorizationCodeIssued)
        .emit();

    redirect_response(&success_redirect(
        &request.redirect_uri,
        &code,
        request.state.as_deref(),
        state.config.issuer(),
    ))
}

/// Re-render the login form with a notice. Used for every recoverable session
/// problem, so none of them mints a code or reveals anything about the account.
fn restart_login(request: &ValidatedRequest, notice: &str) -> Response {
    html_response(
        StatusCode::OK,
        templates::login_page(&LoginContext {
            client_name: &request.client_name,
            redirect_host: &request.redirect_authority,
            loopback_only: request.loopback_only,
            notice: Some(notice),
            hidden: &request.hidden_fields(),
        }),
        Some(session::clear_cookie()),
    )
}

/// A generic internal failure. Never redirects, never explains — a store error
/// is not information the requester is entitled to.
fn internal_error() -> Response {
    html_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        templates::error_page(
            "Something went wrong",
            "This server could not complete the authorization. Try again from the application.",
        ),
        Some(session::clear_cookie()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn config() -> AuthorizeConfig {
        AuthorizeConfig::new("https://issuer.test", "https://issuer.test/mcp")
            .expect("test config must be valid")
    }

    fn client_with_redirects(redirects: &[&str]) -> Client {
        Client {
            id: Uuid::nil(),
            client_id: "a-client".into(),
            client_secret_hash: None,
            name: "A Client".into(),
            redirect_uris: redirects.iter().map(|s| s.to_string()).collect(),
            grant_types: vec!["authorization_code".into()],
            token_endpoint_auth_method: "none".into(),
            owner_account_id: Uuid::nil(),
            registration_source: "operator".into(),
            disabled: false,
            created_at: Utc::now(),
        }
    }

    /// A well-formed request, which each test then breaks in exactly one way.
    fn good_params() -> AuthorizeParams {
        AuthorizeParams {
            client_id: Some("a-client".into()),
            redirect_uri: Some("https://client.test/cb".into()),
            response_type: Some("code".into()),
            scope: Some("mcp".into()),
            state: Some("opaque-state".into()),
            code_challenge: Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into()),
            code_challenge_method: Some("S256".into()),
            resource: Some("https://issuer.test/mcp".into()),
        }
    }

    fn proceed(outcome: AuthorizeOutcome) -> ValidatedRequest {
        match outcome {
            AuthorizeOutcome::Proceed(request) => *request,
            other => panic!("expected the request to be accepted, got {other:?}"),
        }
    }

    fn redirect_location(outcome: AuthorizeOutcome) -> String {
        match outcome {
            AuthorizeOutcome::ErrorRedirect { location } => location,
            other => panic!("expected an error redirect, got {other:?}"),
        }
    }

    // -- Rule one: the two no-redirect cases ------------------------------

    /// The headline security property: an unknown client is an ERROR PAGE and
    /// the response carries no `Location` header at all. Redirecting here would
    /// be an open redirect to an address nobody registered.
    #[test]
    fn an_unknown_client_renders_a_page_and_never_redirects() {
        let outcome = validate(&good_params(), None, &config());
        assert!(matches!(outcome, AuthorizeOutcome::ErrorPage { .. }), "{outcome:?}");
        let response = outcome.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            response.headers().get(header::LOCATION).is_none(),
            "an unknown client must not produce a Location header"
        );
    }

    /// Same rule for an unregistered redirect URI — and this is the case that
    /// matters most, because the attacker chose the URI.
    #[test]
    fn an_unregistered_redirect_renders_a_page_and_never_redirects() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let mut params = good_params();
        params.redirect_uri = Some("https://attacker.test/steal".into());
        let outcome = validate(&params, Some(&client), &config());
        assert!(matches!(outcome, AuthorizeOutcome::ErrorPage { .. }), "{outcome:?}");
        let response = outcome.into_response();
        assert!(response.headers().get(header::LOCATION).is_none());
    }

    /// A missing client_id or redirect_uri is the same class: nothing to
    /// attribute the request to, so nothing to redirect to.
    #[test]
    fn a_missing_client_or_redirect_renders_a_page() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let mut params = good_params();
        params.client_id = None;
        assert!(matches!(
            validate(&params, None, &config()),
            AuthorizeOutcome::ErrorPage { .. }
        ));

        let mut params = good_params();
        params.redirect_uri = None;
        let outcome = validate(&params, Some(&client), &config());
        assert!(matches!(outcome, AuthorizeOutcome::ErrorPage { .. }), "{outcome:?}");
        assert!(outcome.into_response().headers().get(header::LOCATION).is_none());
    }

    /// A disabled client must behave exactly like one that never existed —
    /// including producing no redirect.
    #[test]
    fn a_disabled_client_is_refused_like_an_unknown_one() {
        let mut client = client_with_redirects(&["https://client.test/cb"]);
        client.disabled = true;
        let outcome = validate(&good_params(), Some(&client), &config());
        assert!(matches!(outcome, AuthorizeOutcome::ErrorPage { .. }), "{outcome:?}");
    }

    // -- Rule two: redirect matching --------------------------------------

    /// Non-loopback matching is byte-for-byte. Every one of these near misses
    /// is a real open-redirect technique.
    #[test]
    fn non_loopback_matching_is_exact() {
        let registered = "https://client.test/cb";
        assert!(redirect_uri_matches(registered, "https://client.test/cb"));

        for near_miss in [
            // A different port on a NON-loopback host gets no flexibility.
            "https://client.test:8443/cb",
            // Prefix extension of the host — the classic.
            "https://client.test.attacker.test/cb",
            // Path extension and path change.
            "https://client.test/cb/extra",
            "https://client.test/other",
            // Trailing slash is a different path.
            "https://client.test/cb/",
            // Scheme downgrade.
            "http://client.test/cb",
            // Added query and fragment.
            "https://client.test/cb?next=1",
            "https://client.test/cb#x",
            // Userinfo trick: the real host here is `attacker.test`.
            "https://client.test@attackerhost/cb",
            "",
        ] {
            assert!(
                !redirect_uri_matches(registered, near_miss),
                "must not match {near_miss:?}"
            );
        }
    }

    /// The one deliberate exception: a native client binds an ephemeral port it
    /// could not know at registration time, so the PORT — and only the port —
    /// is ignored for a loopback URI.
    #[test]
    fn loopback_matching_ignores_the_port() {
        assert!(redirect_uri_matches("http://127.0.0.1/callback", "http://127.0.0.1:3118/callback"));
        assert!(redirect_uri_matches("http://127.0.0.1:1/callback", "http://127.0.0.1:65535/callback"));
        assert!(redirect_uri_matches("http://localhost/callback", "http://localhost:49152/callback"));
        assert!(redirect_uri_matches("http://[::1]/callback", "http://[::1]:3118/callback"));
        // Registered WITH a port, presented without one.
        assert!(redirect_uri_matches("http://127.0.0.1:3118/callback", "http://127.0.0.1/callback"));
        // A registered loopback with a query keeps it in the comparison.
        assert!(redirect_uri_matches("http://127.0.0.1/cb?a=1", "http://127.0.0.1:5/cb?a=1"));
    }

    /// The exception must extend to the port and NOTHING else. Each of these
    /// would be an open redirect if the loopback branch were a fuzzy match.
    #[test]
    fn loopback_matching_rejects_a_different_host_path_scheme_or_query() {
        let registered = "http://127.0.0.1/callback";
        for near_miss in [
            // Different host — including the other loopback spellings, which
            // are distinct registrations, not synonyms.
            "http://localhost:3118/callback",
            "http://[::1]:3118/callback",
            "http://127.0.0.2:3118/callback",
            // A host that merely LOOKS loopback-prefixed.
            "http://127.0.0.1.attacker.test:3118/callback",
            // Userinfo whose real host is elsewhere.
            "http://127.0.0.1@attackerhost:3118/callback",
            // Different path.
            "http://127.0.0.1:3118/other",
            "http://127.0.0.1:3118/callback/extra",
            // Different query or fragment.
            "http://127.0.0.1:3118/callback?a=1",
            "http://127.0.0.1:3118/callback#x",
            // Scheme change: the exception is http-only.
            "https://127.0.0.1:3118/callback",
            // A non-numeric port is not a port.
            "http://127.0.0.1:notaport/callback",
        ] {
            assert!(
                !redirect_uri_matches(registered, near_miss),
                "must not match {near_miss:?}"
            );
        }
        // And the exception must not fire when only ONE side is loopback.
        assert!(!redirect_uri_matches("https://client.test/cb", "http://127.0.0.1:3118/cb"));
        assert!(!redirect_uri_matches("http://127.0.0.1/cb", "https://client.test/cb"));
    }

    // -- PKCE --------------------------------------------------------------

    /// `plain` protects against nothing an attacker who can read the
    /// authorization request cares about, and an OMITTED method defaults to
    /// `plain` under RFC 7636 — so both must be refused.
    #[test]
    fn pkce_s256_is_required_and_plain_or_absent_is_refused() {
        let client = client_with_redirects(&["https://client.test/cb"]);

        let mut params = good_params();
        params.code_challenge_method = Some("plain".into());
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_request"), "{location}");

        let mut params = good_params();
        params.code_challenge_method = None;
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_request"), "{location}");

        let mut params = good_params();
        params.code_challenge = None;
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_request"), "{location}");

        // A challenge outside RFC 7636's length range, or outside base64url.
        let over_long = "a".repeat(129);
        for bad in ["short", over_long.as_str(), "not/base64+at=all!!"] {
            let mut params = good_params();
            params.code_challenge = Some(bad.to_string());
            let location = redirect_location(validate(&params, Some(&client), &config()));
            assert!(location.contains("error=invalid_request"), "{bad}: {location}");
        }
    }

    // -- Resource (RFC 8707) ----------------------------------------------

    /// The audience binding: a request naming a peer's resource, or none at
    /// all, must be refused here.
    #[test]
    fn the_resource_parameter_is_required_and_must_be_ours() {
        let client = client_with_redirects(&["https://client.test/cb"]);

        let mut params = good_params();
        params.resource = None;
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_target"), "{location}");

        let mut params = good_params();
        params.resource = Some("https://peer.test/mcp".into());
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_target"), "{location}");

        // Not a prefix or suffix match either.
        for near_miss in ["https://issuer.test/mcp/", "https://issuer.test", "https://issuer.test/mcpx"] {
            let mut params = good_params();
            params.resource = Some(near_miss.into());
            let location = redirect_location(validate(&params, Some(&client), &config()));
            assert!(location.contains("error=invalid_target"), "{near_miss}: {location}");
        }
    }

    // -- Response type -----------------------------------------------------

    #[test]
    fn only_the_code_response_type_is_accepted() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        for bad in [None, Some("token"), Some("code id_token"), Some("CODE")] {
            let mut params = good_params();
            params.response_type = bad.map(str::to_string);
            let location = redirect_location(validate(&params, Some(&client), &config()));
            assert!(location.contains("error=unsupported_response_type"), "{bad:?}: {location}");
        }
    }

    // -- Scope narrowing ---------------------------------------------------

    /// An unknown scope is dropped, never granted — and the narrowed set is
    /// what the consent screen shows and the code is bound to, so the human
    /// never approves a string that differs from what is issued.
    #[test]
    fn an_unknown_scope_is_narrowed_away_rather_than_granted() {
        assert_eq!(narrow_scope(Some("mcp admin")), "mcp");
        assert_eq!(narrow_scope(Some("admin")), "");
        assert_eq!(narrow_scope(Some("mcp")), "mcp");
        // Canonical form: deduplicated and sorted, so one approval is one
        // consent row regardless of the order the client asked in.
        assert_eq!(narrow_scope(Some("offline_access mcp")), "mcp offline_access");
        assert_eq!(narrow_scope(Some("mcp offline_access")), "mcp offline_access");
        assert_eq!(narrow_scope(Some("mcp mcp")), "mcp");
        // Absent or blank falls back to the default rather than to everything.
        assert_eq!(narrow_scope(None), DEFAULT_SCOPE);
        assert_eq!(narrow_scope(Some("   ")), DEFAULT_SCOPE);
    }

    #[test]
    fn a_request_with_only_unsupported_scopes_is_refused() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let mut params = good_params();
        params.scope = Some("admin root".into());
        let location = redirect_location(validate(&params, Some(&client), &config()));
        assert!(location.contains("error=invalid_scope"), "{location}");
    }

    #[test]
    fn the_narrowed_scope_is_what_the_request_carries_forward() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let mut params = good_params();
        params.scope = Some("mcp admin offline_access".into());
        let request = proceed(validate(&params, Some(&client), &config()));
        assert_eq!(request.scope, "mcp offline_access");
        assert_eq!(request.scope_list(), vec!["mcp", "offline_access"]);
    }

    // -- state and iss -----------------------------------------------------

    /// `state` must reach the client as the bytes that were sent, and `iss`
    /// must be on the redirect. Percent-encoding is what preserves the value:
    /// splicing it in raw would let a state containing `&` forge parameters.
    #[test]
    fn state_round_trips_byte_for_byte_and_iss_is_present() {
        let hostile_state = "a b&code=stolen#frag=/?";
        let location = error_redirect(
            "https://client.test/cb",
            Some(hostile_state),
            "https://issuer.test",
            "invalid_request",
            "why",
        );
        assert!(location.contains("iss=https%3A%2F%2Fissuer.test"), "{location}");

        // Recover the state parameter and decode it: it must be exactly what
        // was sent, and the `&` inside it must NOT have created a `code`
        // parameter of its own.
        let fields = FormFields::parse(location.split_once('?').expect("query").1)
            .expect("the redirect's own query must parse");
        assert_eq!(fields.get("state"), Some(hostile_state));
        assert_eq!(fields.get("code"), None, "the state must not forge parameters: {location}");
        assert_eq!(fields.get("error"), Some("invalid_request"));
    }

    /// An absent `state` is OMITTED, not sent empty. `state=` is a value a
    /// strict client is entitled to treat as a mismatch.
    #[test]
    fn an_absent_state_is_omitted_rather_than_sent_empty() {
        let location =
            error_redirect("https://client.test/cb", None, "https://issuer.test", "invalid_scope", "why");
        assert!(!location.contains("state="), "{location}");
        assert!(location.contains("iss="), "{location}");
    }

    /// `iss` is required on SUCCESS too — RFC 9207's mix-up defence is only
    /// complete if both arms carry it.
    #[test]
    fn iss_and_state_are_present_on_the_success_redirect() {
        let location = success_redirect(
            "https://client.test/cb",
            "the-code-value",
            Some("opaque-state"),
            "https://issuer.test",
        );
        let fields = FormFields::parse(location.split_once('?').expect("query").1).expect("parse");
        assert_eq!(fields.get("code"), Some("the-code-value"));
        assert_eq!(fields.get("state"), Some("opaque-state"));
        assert_eq!(fields.get("iss"), Some("https://issuer.test"));

        let without_state =
            success_redirect("https://client.test/cb", "c", None, "https://issuer.test");
        assert!(!without_state.contains("state="), "{without_state}");
    }

    /// A registered redirect URI may carry its own query string; appending must
    /// not produce a second `?`.
    #[test]
    fn a_redirect_uri_with_an_existing_query_is_appended_to_correctly() {
        let location = success_redirect(
            "https://client.test/cb?tenant=one",
            "the-code",
            None,
            "https://issuer.test",
        );
        assert_eq!(location.matches('?').count(), 1, "{location}");
        assert!(location.contains("tenant=one&code=the-code"), "{location}");

        // The client's own parameters must arrive unchanged, and be readable
        // alongside the response parameters rather than displaced by them.
        let fields = FormFields::parse(location.split_once('?').expect("query").1).expect("parse");
        assert_eq!(fields.get("tenant"), Some("one"));
        assert_eq!(fields.get("code"), Some("the-code"));
        assert_eq!(fields.get("iss"), Some("https://issuer.test"));
    }

    /// A registered redirect carrying a RESERVED response parameter is refused
    /// at validation, with an error page and no `Location` header.
    ///
    /// Round 1 (gpt56): appending to `…/cb?code=x` produced a URL with two
    /// `code` parameters, and which one the client reads is decided by its query
    /// parser's tie-breaking rather than by this server — a parameter-injection
    /// primitive handed to whoever registered the URI.
    #[test]
    fn a_redirect_uri_carrying_a_reserved_response_parameter_is_refused() {
        for reserved in [
            "https://client.test/cb?code=attacker-chosen",
            "https://client.test/cb?state=forged",
            "https://client.test/cb?iss=https%3A%2F%2Fattackerhost",
            "https://client.test/cb?error=access_denied",
            "https://client.test/cb?error_description=x",
            // Benign first, reserved second — the whole query is examined.
            "https://client.test/cb?tenant=one&code=x",
            // Percent-encoded name: `%63ode` decodes to `code` at the client,
            // so a literal-only comparison would let it through.
            "https://client.test/cb?%63ode=x",
        ] {
            assert!(
                redirect_uri_has_reserved_parameter(reserved),
                "must be detected: {reserved}"
            );
            let client = client_with_redirects(&[reserved]);
            let mut params = good_params();
            params.redirect_uri = Some(reserved.to_string());
            let outcome = validate(&params, Some(&client), &config());
            assert!(matches!(outcome, AuthorizeOutcome::ErrorPage { .. }), "{outcome:?}");
            assert!(
                outcome.into_response().headers().get(header::LOCATION).is_none(),
                "an ambiguous destination must not be redirected to: {reserved}"
            );
        }
    }

    /// The refusal must not be so broad that it rejects ordinary redirects. A
    /// reserved WORD in the path or the fragment is not a query parameter, and
    /// a name is case-sensitive — `Code` is not the parameter a client reads.
    #[test]
    fn the_reserved_parameter_check_does_not_over_reject() {
        for benign in [
            "https://client.test/cb",
            "https://client.test/cb?tenant=one",
            "https://client.test/cb?tenant=one&flow=two",
            // The value may be anything; only names are reserved.
            "https://client.test/cb?tenant=code",
            // Reserved word in the path, not the query.
            "https://client.test/code/cb",
            // …and in the fragment, which is never sent to the server.
            "https://client.test/cb#code",
            "https://client.test/cb?tenant=one#code=1",
            // Query parameter names are case-sensitive.
            "https://client.test/cb?Code=x",
            "http://127.0.0.1/callback",
        ] {
            assert!(
                !redirect_uri_has_reserved_parameter(benign),
                "must not be refused: {benign}"
            );
        }
        // …and one of them all the way through validation.
        let client = client_with_redirects(&["https://client.test/cb?tenant=one"]);
        let mut params = good_params();
        params.redirect_uri = Some("https://client.test/cb?tenant=one".to_string());
        let request = proceed(validate(&params, Some(&client), &config()));
        assert_eq!(request.redirect_uri, "https://client.test/cb?tenant=one");
    }

    /// `build_redirect` must be correct on its own, not merely because
    /// `validate` screened its input: a security-relevant URL builder should not
    /// depend on its caller having checked.
    #[test]
    fn build_redirect_drops_a_reserved_parameter_it_is_handed_anyway() {
        let location = success_redirect(
            "https://client.test/cb?tenant=one&code=attacker-chosen",
            "the-real-code",
            None,
            "https://issuer.test",
        );
        let fields = FormFields::parse(location.split_once('?').expect("query").1)
            // `FormFields::parse` refuses duplicates outright, so this parsing
            // at all is itself the assertion that only one `code` was emitted.
            .expect("exactly one code parameter must be present");
        assert_eq!(fields.get("code"), Some("the-real-code"));
        assert_eq!(fields.get("tenant"), Some("one"), "benign parameters survive");
        assert!(!location.contains("attacker-chosen"), "{location}");
    }

    /// A fragment must stay at the END. Appending to `…/cb#section` used to
    /// produce `…/cb#section?code=…`, where the entire query is part of the
    /// fragment and no response parameter reaches the client's server side at
    /// all — a silently broken flow, not merely an untidy URL.
    #[test]
    fn a_fragment_stays_after_the_query() {
        let location = success_redirect(
            "https://client.test/cb#section",
            "the-code",
            Some("opaque-state"),
            "https://issuer.test",
        );
        let question = location.find('?').expect("a query must be present");
        let hash = location.find('#').expect("the fragment must survive");
        assert!(question < hash, "the query must precede the fragment: {location}");
        assert!(location.ends_with("#section"), "{location}");

        let query = &location[question + 1..hash];
        let fields = FormFields::parse(query).expect("parse");
        assert_eq!(fields.get("code"), Some("the-code"));
        assert_eq!(fields.get("state"), Some("opaque-state"));
        assert_eq!(fields.get("iss"), Some("https://issuer.test"));
    }

    // -- Payload parsing ---------------------------------------------------

    /// Parameter pollution: two components disagreeing about which value counts
    /// is the whole attack, so the request is refused rather than resolved.
    #[test]
    fn a_duplicated_parameter_is_refused_rather_than_resolved() {
        assert!(FormFields::parse("client_id=trusted&client_id=hostile").is_err());
        assert!(FormFields::parse("a=1&b=2").is_ok());
        assert!(FormFields::parse("bare").is_err(), "a valueless parameter is malformed");
    }

    #[test]
    fn payload_parsing_decodes_percent_and_plus_forms() {
        let fields = FormFields::parse("a=one+two&b=%2Fslash%26amp&c=").expect("parse");
        assert_eq!(fields.get("a"), Some("one two"));
        assert_eq!(fields.get("b"), Some("/slash&amp"));
        // Blank reads as absent, so `state=` behaves like an omitted state.
        assert_eq!(fields.get("c"), None);
        assert_eq!(fields.get("missing"), None);
    }

    // -- Entropy -----------------------------------------------------------

    /// The code must be unguessable. 48 bytes of UUIDv4 material carries 366
    /// bits of CSPRNG output, well past the 256-bit floor.
    #[test]
    fn generated_tokens_are_long_unique_and_url_safe() {
        let first = new_high_entropy_token();
        let second = new_high_entropy_token();
        assert_ne!(first, second);
        // 48 bytes base64url-unpadded is 64 characters.
        assert_eq!(first.len(), 64, "{first}");
        assert!(first.bytes().all(is_base64url_char), "must be URL-safe: {first}");
        let decoded = URL_SAFE_NO_PAD.decode(&first).expect("must decode");
        assert_eq!(decoded.len(), 48, "at least 256 bits of material");

        // No repeats across a batch — a generator seeded once per process, or
        // keyed on time, would fail this.
        let batch: std::collections::HashSet<String> =
            (0..64).map(|_| new_high_entropy_token()).collect();
        assert_eq!(batch.len(), 64);
    }

    /// The stored form must never be the code itself. RMCP-01's `SecretHash`
    /// makes that true by construction; this asserts the property at the point
    /// of use, where a future refactor could reintroduce a plaintext write.
    #[test]
    fn the_stored_code_is_a_hash_not_the_code() {
        let code = new_high_entropy_token();
        let stored = SecretHash::of(&code);
        assert_ne!(stored.as_bytes(), code.as_bytes());
        assert_eq!(stored.as_bytes().len(), 32);
    }

    /// A code that outlives its round trip is only useful to whoever
    /// intercepted it.
    #[test]
    fn the_code_ttl_is_at_most_a_minute() {
        assert!(CODE_TTL_SECONDS <= 60, "an authorization code must be short-lived");
        assert!(CODE_TTL_SECONDS > 0);
    }

    // -- Config ------------------------------------------------------------

    /// Neither value may default. A default resource would make the audience
    /// check compare against something nobody chose — worse than not checking,
    /// because it looks like it is checking.
    #[test]
    fn the_config_refuses_a_missing_or_blank_value() {
        assert!(AuthorizeConfig::new("", "https://issuer.test/mcp").is_err());
        assert!(AuthorizeConfig::new("https://issuer.test", "").is_err());
        assert!(AuthorizeConfig::new("  ", "  ").is_err());
        let config = AuthorizeConfig::new(" https://issuer.test ", " https://issuer.test/mcp ")
            .expect("trimmed values are valid");
        assert_eq!(config.issuer(), "https://issuer.test");
        assert_eq!(config.resource(), "https://issuer.test/mcp");
    }

    // -- Single-use consent ------------------------------------------------
    //
    // NOTE on what is deliberately NOT unit-tested here.
    //
    // An earlier revision guarded replay with a process-local `Mutex<HashMap>`
    // and tested it by spending an id twice. Review round 1 (gpt56) removed the
    // guard, not the test: a per-process map does not hold across replicas, so
    // the same signed session presented to two instances was unspent at both.
    // The guard is now `OauthStore::claim_login_session`, an `INSERT … ON
    // CONFLICT DO NOTHING` arbitrated by a primary key.
    //
    // That invariant lives in a SQL constraint and can only be verified against
    // a real database — the same position `crate::oauth::store`'s own test
    // module takes, and for the same reason: a test that exercises no query
    // proves nothing about a guarantee the query provides. It is covered by
    // RMCP-14's end-to-end test, which runs against an applied schema.
    //
    // The old test's `PgPool` fixture went with it. Its connection string was
    // raised on two separate items as looking credential-shaped at a glance
    // (it was not — fake values, deliberately dotless host, never left the
    // process), and deleting the fixture outright settles that better than
    // renaming its parts would have. `crate::oauth::model`'s
    // `describe_never_contains_the_url` still carries the written-out reason a
    // DSN fixture must use a dotless host: this repo's own PII detector matches
    // any user-at-dotted-domain, including inside a test fixture.

    // -- Validated request -------------------------------------------------

    /// The request carries the PRESENTED redirect URI, not the registered one
    /// it matched. RFC 6749 makes the token endpoint compare against what the
    /// client sent, so binding the registered value would break redemption for
    /// every loopback client.
    #[test]
    fn the_presented_redirect_uri_is_what_is_bound() {
        let client = client_with_redirects(&["http://127.0.0.1/callback"]);
        let mut params = good_params();
        params.redirect_uri = Some("http://127.0.0.1:3118/callback".into());
        let request = proceed(validate(&params, Some(&client), &config()));
        assert_eq!(request.redirect_uri, "http://127.0.0.1:3118/callback");
        assert_eq!(request.redirect_authority, "127.0.0.1:3118");
        assert!(request.loopback_only, "every registered redirect here is loopback");
    }

    /// The loopback warning fires only when EVERY registered redirect is
    /// loopback — a client with one hosted redirect is not a local application.
    #[test]
    fn loopback_only_requires_every_registered_redirect_to_be_loopback() {
        let mixed = client_with_redirects(&["http://127.0.0.1/cb", "https://client.test/cb"]);
        let request = proceed(validate(&good_params(), Some(&mixed), &config()));
        assert!(!request.loopback_only);

        let hosted = client_with_redirects(&["https://client.test/cb"]);
        let request = proceed(validate(&good_params(), Some(&hosted), &config()));
        assert!(!request.loopback_only);
    }

    /// The hidden fields must carry the VALIDATED values, and must omit `state`
    /// entirely when there was none.
    #[test]
    fn hidden_fields_carry_the_validated_request_and_omit_an_absent_state() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let request = proceed(validate(&good_params(), Some(&client), &config()));
        let fields: HashMap<String, String> = request.hidden_fields().into_iter().collect();
        assert_eq!(fields.get("client_id").map(String::as_str), Some("a-client"));
        assert_eq!(fields.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(fields.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(fields.get("state").map(String::as_str), Some("opaque-state"));

        let mut params = good_params();
        params.state = None;
        let request = proceed(validate(&params, Some(&client), &config()));
        let fields: HashMap<String, String> = request.hidden_fields().into_iter().collect();
        assert!(!fields.contains_key("state"), "an absent state must not be materialized");
    }

    /// Every error response must carry the no-store and anti-framing headers —
    /// a consent screen that can be framed is a consent screen that can be
    /// clickjacked, and a cached one is served to the next person.
    #[test]
    fn pages_are_uncacheable_and_unframeable() {
        let response = AuthorizeOutcome::ErrorPage {
            title: "t".into(),
            detail: "d".into(),
        }
        .into_response();
        let headers = response.headers();
        let header_text = |name: &str| -> Option<String> {
            headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
        };
        assert_eq!(header_text("cache-control").as_deref(), Some("no-store"));
        assert_eq!(header_text("x-frame-options").as_deref(), Some("DENY"));
        assert!(header_text("content-security-policy")
            .unwrap_or_default()
            .contains("frame-ancestors 'none'"));
        // Even an error page clears any session cookie the browser holds.
        assert!(header_text("set-cookie").unwrap_or_default().contains("Max-Age=0"));
    }

    /// Every outcome that DOES redirect must carry a `Location` — the mirror of
    /// the error-page assertions, so "never redirects" cannot be satisfied by
    /// never redirecting at all.
    #[test]
    fn a_validated_request_that_fails_later_does_redirect() {
        let client = client_with_redirects(&["https://client.test/cb"]);
        let mut params = good_params();
        params.resource = None;
        let response = validate(&params, Some(&client), &config()).into_response();
        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .expect("an OAuth error must redirect once the destination is established");
        assert!(location.starts_with("https://client.test/cb?"), "{location}");
    }
}
