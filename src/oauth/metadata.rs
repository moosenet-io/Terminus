//! RMCP-02 — the two discovery documents and the `401` challenge that starts
//! the flow.
//!
//! ## Why a whole module for four JSON keys
//! An MCP client does not guess where a resource server's authorization server
//! lives; it is TOLD, and it is told in exactly one way: it makes an
//! unauthenticated request, receives `401` with a `WWW-Authenticate: Bearer
//! resource_metadata="…"` header, fetches that document, and reads
//! `authorization_servers[0]`. Every step of that chain is load-bearing, and
//! every failure in it surfaces to the operator as the same opaque message —
//! "couldn't reach the MCP server" — with no indication of which link broke.
//! That asymmetry (mechanically trivial, diagnostically hostile) is why this
//! module is written defensively and validated at STARTUP rather than at first
//! request.
//!
//! Three specific facts about the hosted client drive the shape here, and each
//! is a thing a reasonable implementation would get wrong:
//!
//! 1. **The challenge only counts on a `401`.** A `WWW-Authenticate` header
//!    attached to a `200` is ignored outright. So the unauthenticated `/mcp`
//!    path must actually FAIL, not succeed-with-a-hint. See
//!    [`Discovery::unauthorized_challenge`] and its call site in
//!    `crate::mcp_server`.
//! 2. **The protected-resource document is probed at the PATH-SUFFIXED
//!    well-known first** (`/.well-known/oauth-protected-resource/mcp` for a
//!    connector at `https://host/mcp`), falling back to the bare form. Serving
//!    only one of the two works for some clients and not others, which is the
//!    worst outcome, so [`crate::oauth::router`] mounts both.
//! 3. **`resource` must byte-equal the URL the operator typed into the
//!    connector form**, because that same string is echoed as the RFC 8707
//!    `resource` parameter and becomes the issued token's audience. A server
//!    that helpfully normalizes it (strips a trailing slash, lowercases a host)
//!    produces a document that disagrees with the token audience, and the
//!    resulting rejection names neither cause. Hence [`CanonicalUri`] REFUSES
//!    the shapes it could otherwise normalize, at startup, with a message
//!    naming the fix — a loud failure the operator can act on, in place of a
//!    silent one they cannot.
//!
//! ## No database, ever, on this path
//! Discovery has a ~10 second budget on the client side and is the first thing
//! attempted, i.e. exactly when the rest of the process is least likely to be
//! warm. Both documents are rendered to a `String` ONCE, in
//! [`Discovery::new`], and afterwards served as bytes. There is deliberately no
//! store handle in this module: discovery must answer while the database is
//! down, because "the connector cannot be reached" and "the connector's
//! database is down" are wildly different operator problems and must not
//! present identically.
//!
//! ## The advertised endpoints are not mounted yet (TERM #631)
//! `authorization_endpoint`, `token_endpoint` and — when DCR is enabled —
//! `registration_endpoint` name paths that no binary currently serves. That is
//! not a defect in this module: it is the cross-item gap tracked as
//! **TERM #631** ("merged OAuth routers are unreachable in a running binary"),
//! which covers RMCP-03's `authorize` router, RMCP-04's token endpoint and
//! RMCP-11's revoke handler identically. Nothing is mounted here to close it,
//! because a private fix would be a second mechanism to unpick when #631 lands.
//!
//! Review round 4 asked specifically whether `registration_endpoint` should be
//! withheld until it is mounted, given this module's own argument that an
//! advertised-but-refusing endpoint is worse than an absent one. Deliberate
//! decision: **keep advertising it, gated only on DCR being enabled.** Three
//! reasons, in order of weight:
//!
//! 1. Gating only `registration_endpoint` would be arbitrary. The
//!    authorization and token endpoints are advertised unconditionally and are
//!    equally unmounted; if "not yet mounted" is disqualifying, it disqualifies
//!    the entire document, and a resource server that publishes no endpoints is
//!    not a resource server.
//! 2. DCR is OFF by default, so no deployment advertises an unmounted
//!    registration endpoint unless an operator explicitly turns it on. The
//!    default-off flag already IS the gate, and it is the one an operator
//!    controls.
//! 3. Detecting "is it mounted?" from here would mean plumbing router state
//!    into a document builder that deliberately has no dependencies — the exact
//!    coupling #631 exists to resolve once, globally.
//!
//! The document describes the contract this server commits to; #631 is the work
//! that makes it true. If #631 is deferred rather than done, the right response
//! is to stop advertising the door at all (leave `RMCP_OAUTH_RESOURCE` unset),
//! not to publish a document with selective holes in it.
//!
//! ## Secret access (S7/S8)
//! Nothing here is a secret — a canonical connector URL and an issuer are
//! public by construction (they are handed to a third-party client). The env
//! reads follow the same materialized-vault convention as
//! [`crate::oauth::OauthConfig::from_env`]: read in one place, at startup, and
//! reported by NAME on failure.

use std::sync::Arc;

use serde_json::json;

use crate::error::ToolError;

/// The connector URL, exactly as typed into the client's custom-connector
/// form. Required; its absence means the OAuth door is simply not configured.
///
/// **This is the SAME variable [`crate::oauth::authorize::RESOURCE_ENV`]
/// reads**, and deliberately so. An earlier revision of this item, written
/// before RMCP-03/04 merged, introduced a parallel `RMCP_CANONICAL_RESOURCE`
/// for the same value — which would have been the exact defect this module
/// exists to prevent, one level up. The `resource` published here and the
/// `resource` the authorization endpoint requires on every request must be
/// byte-equal (RMCP-04's own docs say so), and the only way to guarantee two
/// values are equal is for there to be one value. Two variables that "must
/// match" are two variables that will eventually not match, and the resulting
/// audience mismatch is invisible from both sides.
pub const CANONICAL_RESOURCE_ENV: &str = "RMCP_OAUTH_RESOURCE";

/// The OAuth issuer identifier. Optional — defaults to the canonical
/// resource's ORIGIN, which is what a single-host deployment always wants.
///
/// Same variable as [`crate::oauth::jwt::ISSUER_ENV`] and
/// [`crate::oauth::authorize::ISSUER_ENV`], for the reason given on
/// [`CANONICAL_RESOURCE_ENV`]: the `issuer` in this document, the RFC 9207
/// `iss` on the authorization response, and the `iss` claim in a minted token
/// are the same identifier, so they read the same variable rather than three
/// that are documented to agree.
pub const ISSUER_ENV: &str = "RMCP_OAUTH_ISSUER";

/// Space-separated scopes this resource advertises. Optional.
pub const SCOPES_SUPPORTED_ENV: &str = "RMCP_OAUTH_SCOPES_SUPPORTED";

/// The scope an access token must carry to reach `/mcp`. Optional.
pub const REQUIRED_SCOPE_ENV: &str = "RMCP_OAUTH_REQUIRED_SCOPE";

/// Whether RFC 7591 dynamic client registration is enabled. Optional,
/// default OFF — see [`Discovery::new`] for why the metadata key is omitted
/// rather than advertised-but-refusing.
pub const DCR_ENABLED_ENV: &str = "RMCP_OAUTH_DCR_ENABLED";

/// Operator acknowledgement that a cross-origin [`ISSUER_ENV`] is served
/// elsewhere. Optional, default off — see [`Discovery::new`] for why a
/// cross-origin issuer is refused without it.
pub const ISSUER_EXTERNALLY_SERVED_ENV: &str = "RMCP_OAUTH_ISSUER_EXTERNALLY_SERVED";

/// Default advertised scopes. `offline_access` is present because a hosted
/// connector that cannot refresh silently degrades into "reauthorize every
/// hour", which reads to the user as an unreliable server rather than as a
/// missing scope.
const DEFAULT_SCOPES: &[&str] = &["mcp", "offline_access"];

/// Default scope required to reach `/mcp`.
const DEFAULT_REQUIRED_SCOPE: &str = "mcp";

/// Well-known path for RFC 9728 protected-resource metadata.
pub const PROTECTED_RESOURCE_WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";

/// Well-known path for RFC 8414 authorization-server metadata.
pub const AUTHORIZATION_SERVER_WELL_KNOWN: &str = "/.well-known/oauth-authorization-server";

/// Path of the authorization endpoint, relative to the issuer. Owned here
/// rather than in RMCP-03 so the advertised URL and the mounted route are
/// derived from ONE constant — a metadata document that points at a path the
/// server does not serve is a failure mode with no client-side diagnosis.
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";

/// Path of the token endpoint, relative to the issuer (see [`AUTHORIZE_PATH`]).
pub const TOKEN_PATH: &str = "/oauth/token";

/// Path of the dynamic-registration endpoint, relative to the issuer.
pub const REGISTER_PATH: &str = "/oauth/register";

/// An absolute `https` URI that is safe to publish in a discovery document and
/// to compare byte-for-byte against a client-supplied `resource` parameter.
///
/// This type exists to make ONE class of bug impossible: a value that is
/// *nearly* right. Every shape rejected below is one that a client would
/// normalize differently from this server, producing a `resource` mismatch at
/// token-issuance time whose error message ("invalid target") names neither the
/// server nor the character responsible. Rejecting at startup converts a
/// production mystery into a boot-time message naming the variable and the fix.
///
/// The rejections, and why each is a rejection rather than a normalization:
///
/// - **Not `https`.** A bearer token on a cleartext connection is the one
///   mistake OAuth 2.1 removed the option to make. (Loopback development is
///   served by an `http` client talking to itself, not by this door, which
///   exists specifically to be internet-facing.)
/// - **Uppercase scheme.** Comparison of a URI scheme is case-insensitive in
///   RFC 3986, but the `resource` value here is compared as a STRING against
///   what the client sends, and clients send a lowercased scheme. Silently
///   lowercasing would make [`Self::raw`] disagree with what the operator
///   pasted, which is the one thing this type promises it never does.
/// - **A fragment.** Forbidden outright for an RFC 8707 resource indicator.
/// - **A query.** Not forbidden by the RFC, but an MCP connector URL never has
///   one, and permitting it would make the path-suffixed well-known URL
///   ambiguous to construct. Refusing an unused shape is cheaper than being
///   subtly wrong about it.
/// - **A trailing slash.** `https://h/mcp` and `https://h/mcp/` are different
///   strings and therefore different audiences. The spec calls this out
///   explicitly as a support case: the server does NOT normalize, because
///   normalizing is what makes the mismatch invisible.
/// - **Userinfo, whitespace, control characters, non-ASCII.** These would also
///   have to be escaped before going into a `WWW-Authenticate` header value;
///   refusing them up front means [`Discovery`] can build that header by
///   concatenation and know the result is well-formed. That property is
///   asserted by a test rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalUri {
    raw: String,
    origin: String,
    path: String,
}

impl CanonicalUri {
    /// Validate `value` as a canonical, publishable `https` URI.
    ///
    /// `var` names the environment variable being validated and appears in the
    /// error so an operator reading a startup failure knows which line of their
    /// configuration to fix. The offending VALUE is echoed too — deliberately,
    /// and unlike everywhere else in this module tree: a connector URL is not a
    /// secret (it is pasted into a third-party web form by design), and an
    /// error that says "this is malformed" without saying what "this" was is
    /// the sort of message that costs an hour.
    pub fn parse(var: &str, value: &str) -> Result<Self, ToolError> {
        // Deliberately NOT `value.trim()`. Review round 1 caught that trimming
        // here was the module's own contract violating itself: this type
        // promises the configured value is used byte-for-byte as the operator
        // typed it, and surrounding whitespace is the input where quietly
        // "fixing" it does the most damage. A stray trailing space is invisible
        // in a shell, a `.env` file and a browser address bar alike, so the
        // operator who has one will paste it into some of the three places the
        // value is compared and not others — and every one of those comparisons
        // is byte-for-byte (the metadata document, the RFC 8707 `resource`
        // parameter, the token audience). Accepting it here therefore does not
        // avoid the mismatch, it just moves it to whichever comparison the
        // operator got wrong, where the symptom is a client-side "couldn't
        // reach the MCP server" and nothing else. Refusing it names the problem
        // at startup, at the one place that can still be acted on.
        let raw = value;
        let refuse = |why: &str| {
            ToolError::InvalidArgument(format!(
                "{var} is not a usable canonical connector URI ({why}); got {raw:?}. This value \
                 is published verbatim as the `resource` field of the protected-resource \
                 metadata document and becomes the audience of every issued token, so it must \
                 byte-equal the URL typed into the client's connector form — the server \
                 deliberately does not normalize it, because a normalized mismatch fails later \
                 with an error that names neither side"
            ))
        };

        if raw.is_empty() {
            return Err(refuse("empty"));
        }
        // Checked BEFORE the general printable-ASCII rule below (which would
        // also catch it) purely so the message names the actual problem. An
        // operator staring at a value that looks identical to the one in their
        // connector form needs to be told it is the whitespace, not that the
        // URI "contains a control character".
        if raw.trim() != raw {
            return Err(refuse(
                "has leading or trailing whitespace — remove it rather than relying on the \
                 server to trim, since the value must byte-equal what was typed into the \
                 connector form",
            ));
        }
        // ASCII-printable only: everything downstream (a header value, a path
        // concatenation, a byte comparison) assumes it.
        if !raw.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(refuse(
                "contains whitespace, a control character, or a non-ASCII byte",
            ));
        }
        // `"` and `\` are the two characters that would need quoting inside a
        // `WWW-Authenticate` quoted-string. Refusing them is what lets
        // `Discovery` build that header by concatenation.
        if raw.contains('"') || raw.contains('\\') {
            return Err(refuse("contains a quote or backslash"));
        }
        if raw.contains('#') {
            return Err(refuse("contains a fragment"));
        }
        if raw.contains('?') {
            return Err(refuse("contains a query string"));
        }

        let rest = match raw.strip_prefix("https://") {
            Some(rest) => rest,
            None if raw.len() >= 8 && raw[..8].eq_ignore_ascii_case("https://") => {
                return Err(refuse("scheme must be lowercase `https`"))
            }
            None => return Err(refuse("must be an absolute https:// URI")),
        };

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(refuse("must not carry userinfo"));
        }
        // Review round 2: an authority is more than "non-empty". `https://:8443`
        // has a port and no host, and passed the old emptiness check — it would
        // have been published as a `resource` no client could ever resolve.
        // Completing a check already started rather than adding a new concern:
        // userinfo, fragments and queries were already refused here.
        if let Err(why) = validate_authority(authority) {
            return Err(refuse(why));
        }
        if raw.ends_with('/') {
            return Err(refuse("must not end with a trailing slash"));
        }
        // An empty path is fine (the connector URL may be a bare origin); a
        // path of `/` was already rejected by the trailing-slash rule above.
        Ok(Self {
            raw: raw.to_string(),
            origin: format!("https://{authority}"),
            path: path.to_string(),
        })
    }

    /// The URI exactly as configured. This is what is published and compared.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Scheme + authority, with no trailing slash.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The path component, `""` for a bare origin, otherwise leading-slashed.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// The two discovery documents plus the challenges that point at them, all
/// rendered once at startup.
///
/// Cloning is cheap by design (`Arc<str>` bodies) because the router holds one
/// of these as axum state and every handler clones it per request.
#[derive(Clone, Debug)]
pub struct Discovery {
    resource: CanonicalUri,
    issuer: CanonicalUri,
    dcr_enabled: bool,
    required_scope: String,
    /// The path-suffixed protected-resource metadata URL — the one the client
    /// probes first, and therefore the one advertised in every challenge.
    protected_resource_url: String,
    protected_resource_body: Arc<str>,
    authorization_server_body: Arc<str>,
    unauthorized_challenge: String,
}

impl Discovery {
    /// Read and validate the whole discovery configuration from the
    /// environment.
    ///
    /// Returns `Ok(None)` when [`CANONICAL_RESOURCE_ENV`] is unset or blank:
    /// the OAuth door is an opt-in surface, and a deployment that has not
    /// configured it must behave byte-for-byte as it did before this item —
    /// no new routes, no changed `401`. A blank value reads as absent, matching
    /// `crate::oauth::OauthConfig::from_env`'s rule for an empty materialized
    /// secret.
    ///
    /// Returns `Err` when the door IS configured but configured wrongly. That
    /// distinction is the whole point: callers are expected to treat `Err` as
    /// fatal at startup (see `src/bin/terminus_primary.rs`), because a
    /// half-configured discovery surface fails at the client with a message
    /// that names nothing.
    pub fn from_env() -> Result<Option<Self>, ToolError> {
        let Some(raw_resource) = read_env(CANONICAL_RESOURCE_ENV)? else {
            // The door is not configured. Before accepting that, check that the
            // operator did not configure the REST of it and miss this one — a
            // half-configured door reads as "feature off", and "feature off" on
            // a gateway with no legacy `auth_token` is an open `/mcp`. The same
            // fail-open the whitespace rule above closes, reached by a
            // different mistake, so it gets the same answer.
            //
            // Only the discovery-OWNED settings are consulted. `ISSUER_ENV` is
            // deliberately excluded: it is shared with RMCP-03/04, which set it
            // for the authorization server's own purposes, so its presence is
            // not evidence that anyone intended to enable discovery.
            let mut orphans: Vec<&str> = Vec::new();
            for var in [
                SCOPES_SUPPORTED_ENV,
                REQUIRED_SCOPE_ENV,
                DCR_ENABLED_ENV,
                ISSUER_EXTERNALLY_SERVED_ENV,
            ] {
                // `?`, not `.ok()`: a whitespace-only value on one of THESE
                // must abort too. Swallowing the error here would reinstate the
                // fail-open through a side door — the variable would read as
                // absent, the orphan check would not fire, and the door would
                // go quietly off.
                if read_env(var)?.is_some() {
                    orphans.push(var);
                }
            }
            if !orphans.is_empty() {
                return Err(ToolError::InvalidArgument(format!(
                    "{} configured, but {CANONICAL_RESOURCE_ENV} is not set — the OAuth door \
                     would be silently DISABLED despite being partly configured. Set \
                     {CANONICAL_RESOURCE_ENV} to the connector URL, or clear the other \
                     settings if the door is meant to be off",
                    orphans.join(", ")
                )));
            }
            return Ok(None);
        };
        let resource = CanonicalUri::parse(CANONICAL_RESOURCE_ENV, &raw_resource)?;

        let issuer = match read_env(ISSUER_ENV)? {
            Some(raw) => CanonicalUri::parse(ISSUER_ENV, &raw)?,
            // Default to the resource's ORIGIN rather than to the resource
            // itself: an issuer with a path forces the path-suffixed RFC 8414
            // well-known, which fewer clients probe. A single-host deployment
            // has no reason to want that.
            None => CanonicalUri::parse(ISSUER_ENV, resource.origin())?,
        };

        let scopes = match read_env(SCOPES_SUPPORTED_ENV)? {
            Some(raw) => parse_scope_list(SCOPES_SUPPORTED_ENV, &raw)?,
            None => DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
        };

        let required_scope = match read_env(REQUIRED_SCOPE_ENV)? {
            // `join(" ")` after a STRICT parse is now an identity round-trip,
            // not a normalization: the parser accepts only single-space
            // separators, so the joined string is byte-equal to the configured
            // one. Asserted by a test, because that is the whole claim.
            Some(raw) => parse_scope_list(REQUIRED_SCOPE_ENV, &raw)?.join(" "),
            None => DEFAULT_REQUIRED_SCOPE.to_string(),
        };

        let dcr_enabled = read_flag(DCR_ENABLED_ENV)?;
        let issuer_externally_served = read_flag(ISSUER_EXTERNALLY_SERVED_ENV)?;

        Self::new(
            resource,
            issuer,
            issuer_externally_served,
            scopes,
            required_scope,
            dcr_enabled,
        )
        .map(Some)
    }

    /// Build the documents. Separate from [`Self::from_env`] so every property
    /// below can be tested without touching process-global environment state,
    /// which would race the rest of the suite.
    pub fn new(
        resource: CanonicalUri,
        issuer: CanonicalUri,
        issuer_externally_served: bool,
        scopes_supported: Vec<String>,
        required_scope: String,
        dcr_enabled: bool,
    ) -> Result<Self, ToolError> {
        // Review round 2. This process serves the RFC 8414 metadata document on
        // ITS OWN origin and nowhere else — an axum router cannot answer for a
        // host that does not route to it. So a configured issuer on a different
        // origin produces a protected-resource document naming an authorization
        // server whose `.well-known` nothing here serves. The client follows
        // `authorization_servers[0]`, gets whatever that origin returns (very
        // likely a 404), and reports the same undifferentiated "couldn't reach
        // the MCP server".
        //
        // Refused rather than documented, because the operator has no way to
        // satisfy it from this configuration: there is no setting here that
        // makes another host serve a document. The escape hatch exists for the
        // deployment where the issuer genuinely IS a separate authorization
        // server that publishes its own metadata — that is a legitimate
        // architecture and this check must not make it unbuildable — but it
        // requires the operator to say so explicitly, because the failure it
        // guards is invisible from this side and silent on the other.
        if !issuer_externally_served && issuer.origin() != resource.origin() {
            return Err(ToolError::InvalidArgument(format!(
                "{ISSUER_ENV} is on a different origin ({}) from {CANONICAL_RESOURCE_ENV} ({}), \
                 but this process only serves {AUTHORIZATION_SERVER_WELL_KNOWN} on its own \
                 origin — so nothing would answer the client's discovery request for that \
                 issuer. Either drop {ISSUER_ENV} (it defaults to the resource's origin, which \
                 is what a single-host deployment wants), or set \
                 {ISSUER_EXTERNALLY_SERVED_ENV}=1 to confirm that origin runs its own \
                 authorization server publishing its own RFC 8414 metadata",
                issuer.origin(),
                resource.origin()
            )));
        }

        // Review round 3. Both scope inputs are validated HERE, at construction,
        // rather than only where they are used.
        //
        // The constructor was weaker than the module's own contract: it claimed
        // published values are validated, but took `scopes_supported` and
        // `required_scope` on trust. `required_scope` is interpolated into a
        // `WWW-Authenticate` header and both are published in the metadata
        // documents, so a malformed token could reach the wire even though
        // `insufficient_scope_challenge` sanitizes its own argument — that
        // sanitizer defends the RUNTIME path (a scope arriving from a stored
        // client scoping record, RMCP-07), and it cannot defend a document that
        // was rendered at startup from a bad configuration. Same shape of gap
        // as the whitespace trimming fixed in round 1: a guarantee stated in one
        // place and enforced in another, narrower one.
        //
        // Startup refusal, consistent with `CanonicalUri`: a scope list is
        // configuration, and a typo in it should stop the process rather than
        // quietly narrow what the resource advertises.
        if scopes_supported.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "{SCOPES_SUPPORTED_ENV} resolved to an empty scope list — a protected resource \
                 that advertises no scopes gives the client nothing to request"
            )));
        }
        for scope in &scopes_supported {
            validate_scope_token(SCOPES_SUPPORTED_ENV, scope)?;
        }
        if required_scope.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "{REQUIRED_SCOPE_ENV} is empty — a resource that requires no scope cannot say \
                 what a client is missing, and its `insufficient_scope` challenge would carry \
                 an empty `scope` parameter that tells the client nothing"
            )));
        }
        // Split on a single space, not `split_whitespace`: a scope list is
        // space-delimited by RFC 6749, so a tab or a double space is a
        // malformed list rather than an alternative spelling of the same one.
        // `split_whitespace` would silently accept both and publish a value the
        // operator did not write.
        for scope in required_scope.split(' ') {
            validate_scope_token(REQUIRED_SCOPE_ENV, scope)?;
        }
        // A required scope the resource does not advertise is unobtainable: the
        // client asks for what `scopes_supported` offers, the token comes back
        // without the required scope, and every call 403s forever. Caught here
        // rather than discovered in production.
        for scope in required_scope.split(' ').filter(|s| !s.is_empty()) {
            if !scopes_supported.iter().any(|s| s == scope) {
                return Err(ToolError::InvalidArgument(format!(
                    "{REQUIRED_SCOPE_ENV} requires the scope {scope:?}, which \
                     {SCOPES_SUPPORTED_ENV} does not advertise — no client could ever obtain a \
                     token that satisfies it"
                )));
            }
        }

        // The path-suffixed form, per RFC 9728: the resource's path is appended
        // to the well-known path. For a bare-origin resource this collapses to
        // the bare well-known, which is correct.
        let protected_resource_url = format!(
            "{}{PROTECTED_RESOURCE_WELL_KNOWN}{}",
            resource.origin(),
            resource.path()
        );

        let protected_resource_body = json!({
            // Byte-equal to the configured value. Not derived, not rebuilt from
            // parts — the parts exist for URL construction only.
            "resource": resource.as_str(),
            // Only entry [0] is consulted by the hosted client; a second entry
            // would be decoration that implies a choice nobody makes.
            "authorization_servers": [issuer.as_str()],
            "scopes_supported": scopes_supported.clone(),
            // The token goes in the `Authorization` header and nowhere else.
            // Advertising the form-encoded or query-parameter methods (both
            // deprecated, the latter actively harmful — it lands tokens in
            // access logs) would invite a client to use them.
            "bearer_methods_supported": ["header"],
        })
        .to_string();

        let mut as_doc = json!({
            "issuer": issuer.as_str(),
            "authorization_endpoint": format!("{}{AUTHORIZE_PATH}", issuer.as_str()),
            "token_endpoint": format!("{}{TOKEN_PATH}", issuer.as_str()),
            "scopes_supported": scopes_supported.clone(),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            // No implicit grant and no password grant: OAuth 2.1 removed both,
            // and this list is the only thing telling a client what to try.
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_methods_supported": [
                "none", "client_secret_post", "client_secret_basic"
            ],
            // Exactly `["S256"]`. `plain` is not merely weaker, it is a
            // downgrade target: a client that sees it offered may use it, and
            // PKCE with `plain` protects against nothing an attacker who can
            // read the redirect cannot already do.
            "code_challenge_methods_supported": ["S256"],
            // RFC 9207. Lets the client verify WHICH server answered its
            // authorization request, closing the mix-up attack that arises once
            // a client can talk to more than one authorization server — which
            // is precisely the situation a hosted connector platform is in.
            "authorization_response_iss_parameter_supported": true,
        });

        if dcr_enabled {
            // Advertised ONLY when it works. An advertised endpoint that
            // refuses every request is worse than an absent one: the client
            // takes the presence of the key as a supported path, attempts it,
            // and reports the refusal as a server fault rather than falling
            // back to the pre-registered `client_id` the operator already
            // pasted in.
            as_doc["registration_endpoint"] =
                json!(format!("{}{REGISTER_PATH}", issuer.as_str()));
        }

        let unauthorized_challenge = format!(
            "Bearer realm=\"{}\", resource_metadata=\"{protected_resource_url}\", \
             scope=\"{required_scope}\"",
            issuer.as_str()
        );

        Ok(Self {
            resource,
            issuer,
            dcr_enabled,
            required_scope,
            protected_resource_url,
            protected_resource_body: protected_resource_body.into(),
            authorization_server_body: as_doc.to_string().into(),
            unauthorized_challenge,
        })
    }

    /// The canonical connector URI this resource server answers for.
    pub fn resource(&self) -> &CanonicalUri {
        &self.resource
    }

    /// The authorization server identifier advertised to clients.
    pub fn issuer(&self) -> &CanonicalUri {
        &self.issuer
    }

    /// Whether dynamic client registration is enabled (and therefore
    /// advertised).
    pub fn dcr_enabled(&self) -> bool {
        self.dcr_enabled
    }

    /// The scope an access token must carry to reach `/mcp`.
    pub fn required_scope(&self) -> &str {
        &self.required_scope
    }

    /// The URL a client should fetch to find this resource's metadata — the
    /// path-suffixed form, which is what clients probe first.
    pub fn protected_resource_url(&self) -> &str {
        &self.protected_resource_url
    }

    /// The rendered RFC 9728 protected-resource metadata document.
    pub fn protected_resource_json(&self) -> &str {
        &self.protected_resource_body
    }

    /// The rendered RFC 8414 authorization-server metadata document.
    pub fn authorization_server_json(&self) -> &str {
        &self.authorization_server_body
    }

    /// The `WWW-Authenticate` value for an unauthenticated or badly
    /// authenticated request. Must accompany a `401` and only a `401`.
    pub fn unauthorized_challenge(&self) -> &str {
        &self.unauthorized_challenge
    }

    /// The `WWW-Authenticate` value for a VALID token that lacks the scope this
    /// resource requires — RFC 6750's `insufficient_scope`, which pairs with a
    /// `403`, not a `401`.
    ///
    /// The distinction is not pedantry. `401` tells the client "your credential
    /// is not good here", and a well-behaved client responds by discarding it
    /// and starting a fresh authorization. `403` + `insufficient_scope` tells it
    /// "your credential is fine, it is too narrow", and it responds by
    /// re-authorizing for the named scopes. Collapsing the two into `401` makes
    /// a scope problem look like a broken token and sends the user around a
    /// consent loop that re-grants exactly the same insufficient scope.
    ///
    /// `required` is filtered through [`sanitize_scope`] rather than trusted:
    /// this value can originate in a client scoping record, and a `"` in it
    /// would let a stored string forge additional header parameters.
    pub fn insufficient_scope_challenge(&self, required: &str) -> String {
        let required = sanitize_scope(required);
        let required = if required.is_empty() {
            self.required_scope.clone()
        } else {
            required
        };
        format!(
            "Bearer error=\"insufficient_scope\", error_description=\"the access token does not \
             carry a scope this resource requires\", scope=\"{required}\", \
             resource_metadata=\"{}\"",
            self.protected_resource_url
        )
    }
}

/// Structural check on a URI authority: a non-empty host, and — when a port is
/// present — a numeric port in range.
///
/// Not a full RFC 3986 host parser, and deliberately not one. The job here is
/// to rule out an authority that CANNOT name a reachable server, because such a
/// value would be published as a `resource` no client can resolve and would
/// then fail as an audience mismatch rather than as a connection error. Deciding
/// whether a syntactically fine host actually exists is DNS's job, not this
/// function's.
///
/// The bracketed IPv6 form is handled explicitly: without it, `[::1]:8443` reads
/// as a host containing colons, and either the brackets get rejected or the
/// port check misfires on the address's own colons.
fn validate_authority(authority: &str) -> Result<(), &'static str> {
    // The two host FORMS are validated in their own branches and only the PORT
    // check is shared. An earlier revision validated the IPv6 literal and then
    // fell through into the hostname rules below, which reject a colon — so a
    // perfectly good `[::1]:8443` was refused by the very check that had just
    // accepted it. Caught by this function's own test, which is the argument
    // for asserting the accept cases and not only the reject cases.
    let port = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal. Everything up to `]` is the address; anything after it
        // must be a port or nothing at all.
        let Some((inner, after)) = rest.split_once(']') else {
            return Err("has an unterminated IPv6 literal in its authority");
        };
        let port = match after {
            "" => None,
            with_port => match with_port.strip_prefix(':') {
                Some(port) => Some(port),
                None => return Err("has trailing junk after its IPv6 literal"),
            },
        };
        if inner.is_empty() {
            return Err("has an empty IPv6 literal");
        }
        if !inner
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return Err("has a malformed IPv6 literal");
        }
        port
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };

        if host.is_empty() {
            return Err("has no host");
        }
        // A stray colon left in a non-bracketed host means either a second port
        // separator or a bare IPv6 address that should have been bracketed.
        // Both are malformed, and both would otherwise sail through.
        if host.contains(':') {
            return Err("has a malformed host (an IPv6 address must be bracketed)");
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        {
            return Err("has a host containing characters that are not valid in a hostname");
        }
        if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
            return Err("has a host with an empty label");
        }
        port
    };

    if let Some(port) = port {
        if port.is_empty() {
            return Err("has a colon in its authority but no port");
        }
        if !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err("has a non-numeric port");
        }
        // `u16::from_str` rejects anything above 65535; port 0 is syntactically
        // fine but never listenable, and a URL carrying it is a typo.
        match port.parse::<u16>() {
            Ok(0) | Err(_) => return Err("has a port outside the range 1-65535"),
            Ok(_) => {}
        }
    }

    Ok(())
}

/// Read an env var VERBATIM, treating a blank or whitespace-only value as
/// absent.
///
/// The runtime secret store materializes into the process environment at
/// startup (see this module's doc and `crate::pki`'s), so this IS the
/// configuration read for this module; there is no second path.
///
/// The value is deliberately NOT trimmed. Review round 1 caught that trimming
/// here defeated [`CanonicalUri::parse`]'s whitespace rule from the other side:
/// the parser can only refuse surrounding whitespace it is actually shown, and
/// a reader that helpfully strips it first makes the refusal unreachable in the
/// only code path that matters — the real one. The two functions have to agree,
/// so the verbatim value goes all the way through and each caller decides.
///
/// A whitespace-ONLY value still reads as absent, matching
/// `crate::oauth::OauthConfig::from_env`'s rule that an empty materialized
/// secret is a missing one. That is not the same normalization: "there is
/// nothing here" is a different statement from "there is something here and I
/// have quietly altered it".
fn read_env(var: &str) -> Result<Option<String>, ToolError> {
    // ONE rule, applied to every setting this module reads — URIs, scope lists
    // and booleans alike:
    //
    //   ABSENT   -> the feature is not configured. Unset, or set to the empty
    //               string, are both absent.
    //   PRESENT  -> the value must be USABLE. Anything present-but-unusable is
    //               malformed configuration and ABORTS startup.
    //
    // The arms below are that rule, not a list of special cases, and neither is
    // the boolean check in `read_flag` or the URI check in `CanonicalUri` —
    // "usable" just means something different per type: parseable as a URI,
    // well-formed as a scope list, recognisable as true or false. What makes it
    // one rule is the consequence. On this door, "not configured" disables an
    // internet-facing surface, and disabling it on a gateway with no legacy
    // `auth_token` restores an open `/mcp`. So every value that is not clearly
    // an operator saying "off" must stop the process rather than be guessed at:
    // a value nobody can read is not consent to any particular posture.
    let value = match std::env::var(var) {
        Ok(value) => value,
        // Not set at all. Absent.
        Err(std::env::VarError::NotPresent) => return Ok(None),
        // Set, but not UTF-8. Review round 6 caught this being folded into
        // "absent" along with `NotPresent`, which is the same fail-open as the
        // whitespace case and for the same reason: the operator DID configure
        // something, and this process cannot read it. Invalid encoding is
        // malformed configuration, never absence. The value is not echoed —
        // there is no safe way to render bytes that are not text, and the
        // variable name is what the operator needs.
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(ToolError::InvalidArgument(format!(
                "{var} is set to a value that is not valid UTF-8. That is a malformed setting, \
                 not an absent one — reading it as \"not configured\" would silently disable \
                 the OAuth door. Re-set the variable with a valid value, or clear it"
            )))
        }
    };
    if value.is_empty() {
        // Set to the EMPTY string. Still "nothing here".
        //
        // This is not a normalization, it is what an empty value MEANS in this
        // fleet. Nothing in this process parses a `.env` file (there is no
        // `dotenv` dependency); variables arrive through systemd's
        // `EnvironmentFile=`, where a bare `KEY=` line sets the variable to the
        // empty string. That is precisely how `.env.example` ships every
        // optional key, so "present and empty" is the ordinary, deliberate way
        // an operator says "I am not configuring this". Aborting on it would
        // fail to boot every deployment that materializes the full template —
        // and it is the same rule `crate::oauth::OauthConfig::from_env` already
        // documents for an empty materialized secret.
        return Ok(None);
    }
    if value.trim().is_empty() {
        // Set to WHITESPACE. This is the case round 4 of review caught, and it
        // is categorically different from the two above.
        //
        // A `KEY=` line cannot produce spaces; only a typo, a botched template
        // substitution, or a quoted value gone wrong can. So there is no
        // deployment that means "unconfigured" by writing spaces — and the
        // consequence of guessing that it does is severe and silent. Treated as
        // absent, a whitespace-only canonical resource switches the entire
        // OAuth door OFF, and on a gateway with no legacy `auth_token` that
        // restores exactly the open `/mcp` posture round 2 closed. A fail-open
        // reachable by an invisible character.
        //
        // "There is nothing here" and "there is something here and it is
        // unusable" must not lead to the same outcome. The first disables a
        // feature; the second is a broken configuration and stops the process.
        return Err(ToolError::InvalidArgument(format!(
            "{var} is set to whitespace. That is not the same as leaving it unset or empty \
             (either of which means \"not configured\" and is fine) — a whitespace value can \
             only come from a typo or a botched substitution, and silently reading it as \
             \"unset\" would disable the OAuth door without saying so. Set a real value or \
             clear the variable"
        )));
    }
    Ok(Some(value))
}

/// The prefix every RMCP OAuth setting shares, across every item in the sprint
/// (RMCP-01 through RMCP-13). [`OauthDoors::detect_from_env`] keys on it.
pub const OAUTH_ENV_PREFIX: &str = "RMCP_OAUTH_";

/// Which OAuth surfaces this process serves — the authoritative answer to
/// "is this process internet-facing?", and the single input to
/// `crate::mcp_server::McpServerState::oauth_door_enabled`.
///
/// # Why this is not a list of doors
///
/// Review round 5 replaced a test of one field with a predicate, and round 6
/// correctly pointed out that the predicate was still just that field: it
/// returned `rmcp_discovery.is_some()`, so RMCP-05's resource-server door —
/// enabled by its own independent `RMCP_OAUTH_ENABLED` switch — did not close
/// the open arm. Generalizing the CALL SITE while leaving the predicate an
/// enumeration fixed nothing, and the source-scan guard could not catch it,
/// because a door configured elsewhere is not a field on that struct. A test
/// that cannot fail for the case it exists to catch reads as coverage while
/// providing none, which is worse than having no test.
///
/// So this type does not enumerate doors. It detects whether OAuth is
/// CONFIGURED AT ALL, by looking for the `RMCP_OAUTH_*` prefix that every
/// setting in this sprint already shares. A door that does not exist yet is
/// still covered, because a door nobody can configure is not a door — and the
/// moment someone can configure one, they do it through a variable with this
/// prefix. Registration is therefore not something a future author can forget
/// to do; it is a consequence of the door being configurable at all.
///
/// # Direction of failure
///
/// Detection is deliberately broad, and every inaccuracy points the same way.
/// A process with, say, only `RMCP_OAUTH_SIGNING_KEY` set is treated as having
/// a door even if it serves no OAuth surface. The consequence is that an
/// anonymous caller with no transport identity and no legacy token gets a `401`
/// instead of being admitted — which is the safe answer on any host where that
/// question is even close, and no answer at all for the mTLS and tailnet
/// callers a gateway actually serves, since the listener vouches for them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OauthDoors {
    /// The setting names that evidenced a door, for logging. Empty means none.
    evidence: Vec<String>,
}

impl OauthDoors {
    /// No OAuth surface. The posture of every deployment that predates this
    /// sprint, and of `terminus_personal`, which is not internet-facing.
    pub fn none() -> Self {
        Self {
            evidence: Vec::new(),
        }
    }

    /// Detect from the process environment: any non-empty `RMCP_OAUTH_*`
    /// setting is evidence that an OAuth door is configured here.
    ///
    /// Reads the whole environment once, at startup, rather than naming
    /// variables — naming them is what made the previous two attempts miss a
    /// door. A non-UTF-8 key or value is still evidence: it means SOMETHING was
    /// configured, and the point of this function is presence, not parsing.
    /// (`Discovery::from_env` is where an unreadable value becomes a hard
    /// error; treating it as absence here would be the round-6 fail-open.)
    pub fn detect_from_env() -> Self {
        let mut evidence: Vec<String> = std::env::vars_os()
            .filter(|(key, value)| {
                key.to_string_lossy().starts_with(OAUTH_ENV_PREFIX) && !value.is_empty()
            })
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        evidence.sort();
        Self { evidence }
    }

    /// Register a door explicitly, for one built in code rather than read from
    /// the environment (a test, or a caller that constructs a [`Discovery`]
    /// directly).
    pub fn register(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.evidence.contains(&what) {
            self.evidence.push(what);
        }
    }

    /// Whether ANY OAuth door is configured on this process.
    pub fn any(&self) -> bool {
        !self.evidence.is_empty()
    }

    /// A log-safe description: the setting NAMES that evidenced a door, never
    /// their values (one of them is a signing key).
    pub fn describe(&self) -> String {
        if self.evidence.is_empty() {
            "no OAuth door configured".to_string()
        } else {
            format!("OAuth door configured, evidenced by: {}", self.evidence.join(", "))
        }
    }
}

/// The boolean spellings this module accepts, in both directions.
///
/// Listed once, so the parser, the error message and the documentation cannot
/// disagree about what is accepted.
const TRUE_FORMS: &[&str] = &["1", "true", "yes", "on"];
const FALSE_FORMS: &[&str] = &["0", "false", "no", "off"];

/// Read a boolean flag, applying [`read_env`]'s presence rule.
///
/// # The same rule, not a smaller one
///
/// ABSENT (unset or empty) means not configured, and the flag takes its
/// documented default. PRESENT means the value must be USABLE — and for a
/// boolean, usable means it is recognisably true or false. Anything else aborts
/// startup.
///
/// Review round 7 caught this reading an unrecognised value as `false`. That is
/// the same fail-open as the whitespace and non-UTF-8 cases, in a smaller box: a
/// typo is a legible instruction the operator believes is in force, and
/// answering it with a silent `false` changes the configured posture without a
/// word. It matters most on the knob where being wrong is cheapest to miss —
/// [`DCR_ENABLED_ENV`] gates dynamic client registration, so `=ture` would
/// quietly leave a security-relevant feature in a state nobody chose, and the
/// operator's evidence that they set it is the line they are looking at.
///
/// # Why the value is trimmed when the canonical URI's is not
///
/// Nothing compares a flag byte-for-byte against anything, so ` true` meaning
/// `true` costs nothing, whereas refusing it would fail a deployment over an
/// invisible character for no gain. The no-normalization rule this module
/// enforces is about values that are PUBLISHED and COMPARED, not about every
/// string it reads. Trimming is not the same as guessing: a trimmed value still
/// has to be one of the accepted spellings.
fn read_flag(var: &str) -> Result<bool, ToolError> {
    let Some(raw) = read_env(var)? else {
        return Ok(false);
    };
    let value = raw.trim().to_ascii_lowercase();
    if TRUE_FORMS.contains(&value.as_str()) {
        return Ok(true);
    }
    if FALSE_FORMS.contains(&value.as_str()) {
        return Ok(false);
    }
    Err(ToolError::InvalidArgument(format!(
        "{var} is set to {raw:?}, which is not a recognised boolean. Accepted: {} for on, {} for \
         off (any case); unset or empty means off. This is refused rather than read as \"off\" \
         because a typo here is an instruction you believe is in force, and silently ignoring it \
         would change the configured posture without saying so",
        TRUE_FORMS.join("/"),
        FALSE_FORMS.join("/")
    )))
}

/// Split a space-separated scope list, rejecting any token that is not a valid
/// RFC 6749 `scope-token`.
///
/// Fail-closed rather than filter-and-continue: a scope list with a bad
/// character in it is a configuration typo, and quietly dropping the offending
/// entry would leave a resource advertising fewer scopes than the operator
/// believes — which then shows up as an unexplained `403` on a call that ought
/// to work.
fn parse_scope_list(var: &str, raw: &str) -> Result<Vec<String>, ToolError> {
    let mut out: Vec<String> = Vec::new();
    // `split(' ')`, NOT `split_whitespace`. Round 4 of review caught that this
    // reader still used the lenient form while `Discovery::new` had been
    // tightened to the strict one — which made the constructor's rule
    // unreachable for the only values that actually matter, the configured
    // ones. RFC 6749 delimits a scope list with a SINGLE space, so a tab, a
    // newline, a double space or a leading/trailing space is a malformed list,
    // not an alternative spelling of a valid one. `split_whitespace` would
    // absorb all of them and publish a list the operator did not write.
    //
    // Every one of those cases now surfaces as an empty token or a bad
    // character, both of which `validate_scope_token` refuses — so this
    // function and the constructor genuinely cannot drift, which is what the
    // shared validator was introduced for.
    for token in raw.split(' ') {
        validate_scope_token(var, token)?;
        // A REPEATED scope is refused rather than silently collapsed, for the
        // same reason the separators are: de-duplicating would publish a list
        // that differs from the configured one, and a duplicate can only be a
        // typo. Refusing it says so; collapsing it hides it.
        if out.iter().any(|s| s == token) {
            return Err(ToolError::InvalidArgument(format!(
                "{var} lists the scope {token:?} more than once — a duplicate is a typo, and \
                 silently collapsing it would publish a scope list that differs from the \
                 configured one"
            )));
        }
        out.push(token.to_string());
    }
    // Unreachable in practice: `split(' ')` always yields at least one item, and
    // an empty one is refused above. Kept as a total match rather than an
    // `unwrap`-shaped assumption about `split`'s behavior.
    if out.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{var} is present but contains no scope tokens"
        )));
    }
    Ok(out)
}

/// Validate ONE scope token against RFC 6749's `scope-token` production.
///
/// The single place that decides what a scope may contain, so
/// [`parse_scope_list`] (which reads configuration) and [`Discovery::new`]
/// (which publishes it) cannot drift apart on the answer. `var` names the
/// setting so a startup failure points at a line the operator can edit.
///
/// An EMPTY token is refused explicitly rather than skipped. An empty entry can
/// only arise from a delimiter mistake — a double space, a trailing space, a
/// stray comma — and skipping it would publish a scope list subtly different
/// from the one that was written, which is the class of silent normalization
/// this module refuses everywhere else.
fn validate_scope_token(var: &str, scope: &str) -> Result<(), ToolError> {
    if scope.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{var} contains an empty scope token — scopes are separated by a SINGLE space, so \
             this is a delimiter mistake (a double space, or a leading/trailing one) rather \
             than a scope"
        )));
    }
    if !scope.bytes().all(is_scope_char) {
        return Err(ToolError::InvalidArgument(format!(
            "{var} contains {scope:?}, which is not a valid OAuth scope token (RFC 6749 permits \
             %x21 / %x23-5B / %x5D-7E — notably not a space, a tab, a quote or a backslash; the \
             last two would break the `WWW-Authenticate` header this value is interpolated into)"
        )));
    }
    Ok(())
}

/// RFC 6749 `scope-token` character set: printable ASCII except `"` and `\`.
fn is_scope_char(b: u8) -> bool {
    b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b)
}

/// Reduce an arbitrary string to something safe to interpolate into a
/// `WWW-Authenticate` quoted-string: scope tokens separated by single spaces,
/// with everything else dropped.
///
/// Dropping rather than erroring, unlike [`parse_scope_list`], because this
/// runs on a request path where the alternative to a slightly-reduced header is
/// no header at all — and a caller that gets no challenge cannot recover, while
/// a caller that gets a truncated scope list at least learns it needs to
/// re-authorize.
fn sanitize_scope(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|t| t.bytes().all(is_scope_char))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Every hostname in this module's tests is under `.test`, and that is
    /// deliberate rather than a placeholder someone forgot to replace.
    ///
    /// Review round 1 flagged `connector.test`/`evil.test` as hardcoded
    /// infrastructure values. Held, with reasoning: `.test` is a reserved TLD
    /// (RFC 2606, §2) that exists precisely so examples and test fixtures can
    /// name a host without any possibility of colliding with a real one.
    /// Substituting a more "realistic-looking" domain would be strictly worse —
    /// it might be somebody's actual registered name, and a test that resolved
    /// it would reach a stranger's server. These are also not fleet hostnames,
    /// which is what the no-hardcoded-infrastructure rule targets; the repo's
    /// own `no_pii_in_own_source_tree` gate (`crate::github::pii`) walks this
    /// file and passes on them.
    fn uri(value: &str) -> CanonicalUri {
        CanonicalUri::parse("TEST_VAR", value).expect("fixture must parse")
    }

    fn discovery(dcr_enabled: bool) -> Discovery {
        Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test"),
            false,
            vec!["mcp".to_string(), "offline_access".to_string()],
            "mcp".to_string(),
            dcr_enabled,
        )
        .expect("fixture must build")
    }

    /// The headline acceptance criterion: the published `resource` is the
    /// configured string, byte for byte. A normalizing server produces a
    /// document that disagrees with the token audience it later enforces.
    #[test]
    fn resource_byte_equals_the_configured_uri() {
        let configured = "https://connector.test/some/path/mcp";
        let d = Discovery::new(
            uri(configured),
            uri("https://connector.test"),
            false,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .expect("must build");
        let doc: serde_json::Value =
            serde_json::from_str(d.protected_resource_json()).expect("valid JSON");
        assert_eq!(doc["resource"].as_str(), Some(configured));
    }

    /// Startup must refuse every shape it would otherwise have to normalize.
    /// Each entry here is a real support case: the value looks right to the
    /// operator and fails opaquely at the client.
    #[test]
    fn canonical_uri_refuses_every_normalizable_shape() {
        for bad in [
            "",
            "   ",
            // Not https.
            "http://connector.test/mcp",
            "connector.test/mcp",
            "/mcp",
            "ftp://connector.test/mcp",
            // Case-differing scheme: legal per RFC 3986, useless here.
            "HTTPS://connector.test/mcp",
            "Https://connector.test/mcp",
            // Fragment and query.
            "https://connector.test/mcp#frag",
            "https://connector.test/mcp?x=1",
            // Trailing slash — the documented mismatch cause.
            "https://connector.test/mcp/",
            "https://connector.test/",
            // No host.
            "https://",
            "https:///mcp",
            // Userinfo, and characters that would break a header value.
            // Userinfo. The host here is deliberately DOTLESS: this repo's own
            // `no_pii_in_own_source_tree` self-check treats any
            // any userinfo-plus-dotted-authority shape as an email address, so
            // a realistic authority in this fixture would fail the PII gate
            // rather than the assertion.
            "https://userinfo@connectorhost/mcp",
            "https://connector.test/m cp",
            "https://connector.test/\"mcp",
            "https://connector.test/\\mcp",
        ] {
            assert!(
                CanonicalUri::parse("TEST_VAR", bad).is_err(),
                "must refuse {bad:?}"
            );
        }
    }

    /// Review round 1. Surrounding whitespace must be REFUSED, not trimmed —
    /// in both positions and through the env read, since a reader that strips
    /// it first makes the parser's rule unreachable on the only path that
    /// matters. A trailing space is invisible in a shell, a `.env` file and an
    /// address bar alike, so trimming it here does not avoid the byte-for-byte
    /// mismatch; it relocates the mismatch to whichever of the three
    /// comparisons (document / RFC 8707 parameter / token audience) the
    /// operator pasted the untrimmed value into, where the only symptom is a
    /// client-side "couldn't reach the MCP server".
    #[test]
    fn surrounding_whitespace_is_refused_not_trimmed() {
        for bad in [
            " https://connector.test/mcp",
            "https://connector.test/mcp ",
            " https://connector.test/mcp ",
            "https://connector.test/mcp\t",
            "\nhttps://connector.test/mcp",
        ] {
            let err = CanonicalUri::parse("TEST_VAR", bad)
                .expect_err("surrounding whitespace must be refused");
            assert!(
                err.to_string().contains("whitespace"),
                "the error must name the whitespace, not something downstream of it: {err}"
            );
        }

    }

    /// The other half of the same finding, and the half that decides whether
    /// the rule above is reachable at all: the env READ must hand the parser
    /// the value verbatim. Exercised end-to-end through
    /// [`Discovery::from_env`], because asserting on a re-implementation of
    /// `read_env`'s filter would pass even if `read_env` itself started
    /// trimming again — which is exactly the regression this guards.
    ///
    /// `#[serial]` because it mutates process-global environment state, per
    /// this crate's existing convention (`crate::config`,
    /// `crate::secrets_bootstrap`).
    #[test]
    #[serial]
    fn from_env_refuses_an_untrimmed_canonical_resource() {
        // SAFETY-BY-CONVENTION: serialized against every other env-mutating
        // test in the crate by `#[serial]`; cleared on every exit path below.
        std::env::set_var(CANONICAL_RESOURCE_ENV, "https://connector.test/mcp ");
        let untrimmed = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        let err = untrimmed.expect_err("a trailing space must not reach a document");
        assert!(err.to_string().contains("whitespace"), "{err}");

        // Control: the same value without the space builds, so the test above
        // is demonstrating the whitespace rule and not some unrelated refusal.
        std::env::set_var(CANONICAL_RESOURCE_ENV, "https://connector.test/mcp");
        let clean = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        let discovery = clean
            .expect("a clean value must build")
            .expect("a set canonical resource enables the door");
        assert_eq!(discovery.resource().as_str(), "https://connector.test/mcp");

        // ROUND 4, and the reason this assertion is the exact opposite of what
        // it used to be. A whitespace-only value previously read as "absent",
        // which switched the OAuth door OFF — and on a gateway with no legacy
        // `auth_token` that restores the open `/mcp` posture round 2 closed. A
        // fail-open reachable by an invisible character. It must ABORT.
        std::env::set_var(CANONICAL_RESOURCE_ENV, "   ");
        let whitespace = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        let err = whitespace.expect_err("whitespace must abort, never disable the door");
        assert!(err.to_string().contains("whitespace"), "{err}");

        // But the EMPTY string is still "not configured", and must stay that
        // way: systemd's `EnvironmentFile=` turns a bare `KEY=` line — how
        // `.env.example` ships every optional key — into an empty value, so
        // aborting on it would fail to boot every deployment that materializes
        // the full template.
        std::env::set_var(CANONICAL_RESOURCE_ENV, "");
        let empty = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        assert!(
            matches!(empty, Ok(None)),
            "an empty value means 'not configured', not 'malformed'"
        );

        // And unset is the same as empty.
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        assert!(matches!(Discovery::from_env(), Ok(None)));
    }

    /// REVIEW ROUND 6, and the third member of the same family. A CONFIGURED
    /// but non-UTF-8 value used to be folded into "absent" alongside
    /// `NotPresent`, which switched the door off and — with no legacy
    /// `auth_token` — restored the open `/mcp` posture. Invalid encoding is
    /// malformed configuration, never absence.
    ///
    /// The one rule, in one test: unset and empty are ABSENT; whitespace and
    /// invalid encoding are MALFORMED and must abort.
    #[test]
    #[serial]
    fn a_non_utf8_value_is_malformed_not_absent() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // A lone continuation byte: valid as an OS string, never valid UTF-8.
        std::env::set_var(
            CANONICAL_RESOURCE_ENV,
            OsString::from_vec(vec![b'h', b't', b't', b'p', 0x80]),
        );
        let result = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);

        let err = result.expect_err("a non-UTF-8 value must abort, never disable the door");
        let message = err.to_string();
        assert!(message.contains(CANONICAL_RESOURCE_ENV), "{message}");
        assert!(message.contains("UTF-8"), "{message}");
    }

    /// REVIEW ROUND 7 — the same rule, applied to booleans.
    ///
    /// An unrecognised value used to read as `false`, silently choosing a
    /// posture the operator did not. That matters most on [`DCR_ENABLED_ENV`],
    /// which gates dynamic client registration: `=ture` would leave a
    /// security-relevant feature in a state nobody picked, while the operator
    /// looks at a line that says they set it.
    #[test]
    #[serial]
    fn an_unrecognised_boolean_aborts_rather_than_defaulting_to_false() {
        let with = |var: &str, value: &str| {
            std::env::set_var(CANONICAL_RESOURCE_ENV, "https://connector.test/mcp");
            std::env::set_var(var, value);
            let result = Discovery::from_env();
            std::env::remove_var(var);
            std::env::remove_var(CANONICAL_RESOURCE_ENV);
            result
        };

        // Both flags, because both are reached through the same reader and a
        // regression in either is the same class of defect.
        for var in [DCR_ENABLED_ENV, ISSUER_EXTERNALLY_SERVED_ENV] {
            for garbage in ["garbage", "ture", "enabled", "2", "yes please", "-1"] {
                assert!(
                    with(var, garbage).is_err(),
                    "{var}={garbage:?} must abort, not quietly read as false"
                );
            }
        }

        // And the refusal must name the variable, so an operator can act on it.
        let err = with(DCR_ENABLED_ENV, "ture").expect_err("a typo must abort");
        let message = err.to_string();
        assert!(message.contains(DCR_ENABLED_ENV), "{message}");
        assert!(message.contains("ture"), "the error must show what was read: {message}");
        assert!(message.contains("true"), "and what is accepted: {message}");

        // Every accepted spelling still works, in both directions and any case
        // — a guard that refuses real configuration is worse than the gap it
        // closes.
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("TRUE", true),
            ("Yes", true),
            ("on", true),
            (" true ", true),
            ("0", false),
            ("false", false),
            ("No", false),
            ("OFF", false),
        ] {
            let discovery = with(DCR_ENABLED_ENV, value)
                .unwrap_or_else(|e| panic!("{value:?} must be accepted: {e}"))
                .expect("the door is enabled in this fixture");
            assert_eq!(discovery.dcr_enabled(), expected, "for {value:?}");
        }

        // And ABSENT still means off, without an error — the other half of the
        // one rule.
        std::env::set_var(CANONICAL_RESOURCE_ENV, "https://connector.test/mcp");
        std::env::remove_var(DCR_ENABLED_ENV);
        let unset = Discovery::from_env();
        std::env::set_var(DCR_ENABLED_ENV, "");
        let empty = Discovery::from_env();
        std::env::remove_var(DCR_ENABLED_ENV);
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        assert!(!unset.expect("unset is fine").expect("enabled").dcr_enabled());
        assert!(!empty.expect("empty is fine").expect("enabled").dcr_enabled());
    }

    /// The other route to the same fail-open: the operator configured the
    /// discovery knobs and missed the one that enables the door. "Feature off"
    /// on a gateway with no legacy `auth_token` is an open `/mcp`, so a
    /// half-configured door is refused rather than silently ignored.
    #[test]
    #[serial]
    fn a_half_configured_door_is_refused_rather_than_disabled() {
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        std::env::set_var(REQUIRED_SCOPE_ENV, "mcp");
        let orphaned = Discovery::from_env();
        std::env::remove_var(REQUIRED_SCOPE_ENV);
        let err = orphaned.expect_err("a partly-configured door must not read as 'off'");
        assert!(err.to_string().contains(CANONICAL_RESOURCE_ENV), "{err}");
        assert!(err.to_string().contains(REQUIRED_SCOPE_ENV), "{err}");

        // The shared issuer is deliberately NOT evidence of intent: RMCP-03/04
        // set it for the authorization server's own purposes, so its presence
        // must not force the discovery door on.
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        std::env::set_var(ISSUER_ENV, "https://connector.test");
        let issuer_only = Discovery::from_env();
        std::env::remove_var(ISSUER_ENV);
        assert!(
            matches!(issuer_only, Ok(None)),
            "a shared variable owned by another item must not enable this one"
        );
    }

    /// A bare origin is a legitimate connector URL, and its path-suffixed
    /// well-known collapses to the bare one. If this ever regressed to
    /// emitting a double slash the document would be served from a URL the
    /// router does not mount.
    #[test]
    fn canonical_uri_accepts_a_bare_origin_and_a_path() {
        let origin = uri("https://connector.test");
        assert_eq!(origin.origin(), "https://connector.test");
        assert_eq!(origin.path(), "");

        let with_path = uri("https://connector.test/a/b");
        assert_eq!(with_path.origin(), "https://connector.test");
        assert_eq!(with_path.path(), "/a/b");
        assert_eq!(with_path.as_str(), "https://connector.test/a/b");
    }

    /// The client probes the path-suffixed well-known first, so that is the URL
    /// every challenge must advertise.
    #[test]
    fn protected_resource_url_is_the_path_suffixed_form() {
        let d = discovery(false);
        assert_eq!(
            d.protected_resource_url(),
            "https://connector.test/.well-known/oauth-protected-resource/mcp"
        );

        let bare = Discovery::new(
            uri("https://connector.test"),
            uri("https://connector.test"),
            false,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .expect("must build");
        assert_eq!(
            bare.protected_resource_url(),
            "https://connector.test/.well-known/oauth-protected-resource"
        );
    }

    /// `plain` PKCE is a downgrade target, so the advertised list is exactly
    /// `["S256"]` — not "contains S256".
    #[test]
    fn authorization_server_advertises_only_s256() {
        let doc: serde_json::Value =
            serde_json::from_str(discovery(false).authorization_server_json())
                .expect("valid JSON");
        assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(doc["authorization_response_iss_parameter_supported"], json!(true));
        assert_eq!(
            doc["grant_types_supported"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(
            doc["token_endpoint_auth_methods_supported"],
            json!(["none", "client_secret_post", "client_secret_basic"])
        );
        assert_eq!(
            doc["authorization_endpoint"],
            json!("https://connector.test/oauth/authorize")
        );
        assert_eq!(doc["token_endpoint"], json!("https://connector.test/oauth/token"));
        // Removed by OAuth 2.1 — advertising either is an invitation.
        let grants = doc["grant_types_supported"].as_array().expect("array");
        assert!(!grants.iter().any(|g| g == "implicit" || g == "password"));
    }

    /// An advertised endpoint that refuses everything is worse than an absent
    /// one: the client tries it and reports the refusal instead of falling back
    /// to the pre-registered client the operator already configured.
    #[test]
    fn registration_endpoint_appears_only_when_dcr_is_enabled() {
        let off: serde_json::Value =
            serde_json::from_str(discovery(false).authorization_server_json())
                .expect("valid JSON");
        assert!(
            off.get("registration_endpoint").is_none(),
            "the key must be ABSENT, not null: {off}"
        );

        let on: serde_json::Value =
            serde_json::from_str(discovery(true).authorization_server_json())
                .expect("valid JSON");
        assert_eq!(
            on["registration_endpoint"],
            json!("https://connector.test/oauth/register")
        );
    }

    /// The protected-resource document's own contract.
    #[test]
    fn protected_resource_document_shape() {
        let doc: serde_json::Value =
            serde_json::from_str(discovery(false).protected_resource_json())
                .expect("valid JSON");
        assert_eq!(
            doc["authorization_servers"],
            json!(["https://connector.test"])
        );
        assert_eq!(doc["bearer_methods_supported"], json!(["header"]));
        assert_eq!(doc["scopes_supported"], json!(["mcp", "offline_access"]));
    }

    /// The challenge is the only thing that tells a client where to look, so
    /// its two load-bearing parameters are asserted explicitly.
    #[test]
    fn unauthorized_challenge_carries_metadata_url_and_scope() {
        let d = discovery(false);
        let challenge = d.unauthorized_challenge();
        assert!(challenge.starts_with("Bearer "));
        assert!(challenge.contains(&format!(
            "resource_metadata=\"{}\"",
            d.protected_resource_url()
        )));
        assert!(challenge.contains("scope=\"mcp\""));
        // No `error=` on the plain challenge: an unauthenticated request is not
        // an invalid token, and naming one sends some clients down a
        // token-refresh path instead of a fresh authorization.
        assert!(!challenge.contains("error="));
    }

    /// Insufficient scope is a DIFFERENT answer from "not authenticated" and
    /// must stay one, or a scope problem sends the user round a consent loop
    /// that re-grants the same insufficient scope.
    #[test]
    fn insufficient_scope_challenge_is_distinct_and_names_what_is_needed() {
        let d = discovery(false);
        let challenge = d.insufficient_scope_challenge("mcp admin");
        assert!(challenge.contains("error=\"insufficient_scope\""));
        assert!(challenge.contains("scope=\"mcp admin\""));
        assert!(challenge.contains(&format!(
            "resource_metadata=\"{}\"",
            d.protected_resource_url()
        )));
        assert_ne!(challenge, d.unauthorized_challenge());
    }

    /// A required-scope string can originate in stored scoping data, so a
    /// header-breaking character in it must not be able to forge an extra
    /// challenge parameter.
    #[test]
    fn insufficient_scope_challenge_cannot_be_used_to_forge_parameters() {
        let d = discovery(false);
        let challenge =
            d.insufficient_scope_challenge("mcp\", resource_metadata=\"https://evil.test/x");
        assert_eq!(
            challenge.matches("resource_metadata=").count(),
            1,
            "a stored scope must not be able to inject a second metadata pointer: {challenge}"
        );
        assert!(!challenge.contains("evil.test"));
        // An entirely unusable input falls back to the configured requirement
        // rather than emitting `scope=""`, which tells a client nothing.
        assert!(d.insufficient_scope_challenge("\"\\").contains("scope=\"mcp\""));
    }

    /// Everything published is header-safe by construction, which is what lets
    /// the challenge be built by concatenation rather than by escaping.
    #[test]
    fn every_published_value_is_header_safe() {
        let d = discovery(true);
        for value in [
            d.unauthorized_challenge().to_string(),
            d.insufficient_scope_challenge("mcp"),
        ] {
            assert!(
                value.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                "header values must be printable ASCII: {value:?}"
            );
            // Balanced quotes: an odd count means a parameter value escaped its
            // own quoted-string.
            assert_eq!(value.matches('"').count() % 2, 0, "unbalanced quotes: {value}");
        }
    }

    /// A requirement the resource never advertises is unobtainable, and every
    /// call would `403` forever. Caught at startup instead.
    #[test]
    fn required_scope_must_be_advertised() {
        let err = Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test"),
            false,
            vec!["mcp".to_string()],
            "admin".to_string(),
            false,
        )
        .expect_err("an unobtainable requirement must be refused");
        assert!(err.to_string().contains("admin"));

        assert!(Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test"),
            false,
            vec![],
            "mcp".to_string(),
            false,
        )
        .is_err());
    }

    /// Review round 2: an authority is more than "not empty". Each of these
    /// would have been published as a `resource` that no client could resolve,
    /// and would then have failed as an audience mismatch rather than as the
    /// connection error it actually is.
    #[test]
    fn malformed_authorities_are_refused() {
        for bad in [
            // A port with no host — the case that passed the old emptiness
            // check outright.
            "https://:8443/mcp",
            "https://:/mcp",
            // Ports that are not ports.
            "https://connector.test:/mcp",
            "https://connector.test:http/mcp",
            "https://connector.test:8443x/mcp",
            "https://connector.test:65536/mcp",
            "https://connector.test:99999/mcp",
            "https://connector.test:0/mcp",
            // A bare IPv6 address must be bracketed, or the colons are
            // indistinguishable from a port separator.
            "https://::1/mcp",
            "https://connector.test:8443:9000/mcp",
            // Malformed brackets.
            "https://[::1/mcp",
            "https://[]/mcp",
            "https://[::1]x/mcp",
            "https://[zz::gg]/mcp",
            // Empty host labels.
            "https://.connector.test/mcp",
            "https://connector..test/mcp",
            "https://connector.test./mcp",
        ] {
            assert!(
                CanonicalUri::parse("TEST_VAR", bad).is_err(),
                "must refuse {bad:?}"
            );
        }

        // The guard must not be so eager that it rejects real authorities.
        for good in [
            "https://connector.test/mcp",
            "https://connector.test:8443/mcp",
            "https://connector.test:1/mcp",
            "https://connector.test:65535/mcp",
            "https://sub.connector.test/mcp",
            "https://connector-1.test/mcp",
            "https://[::1]/mcp",
            "https://[::1]:8443/mcp",
        ] {
            assert!(
                CanonicalUri::parse("TEST_VAR", good).is_ok(),
                "must accept {good:?}"
            );
        }
    }

    /// Review round 2: this process serves the RFC 8414 document on its OWN
    /// origin and nowhere else, so an issuer on a different origin names an
    /// authorization server whose metadata nothing here publishes — discovery
    /// then fails at the client with the same undifferentiated message.
    /// Refused rather than documented, because there is no setting here that
    /// makes another host serve a document; the acknowledgement flag exists so
    /// a genuine separate-authorization-server deployment stays buildable.
    #[test]
    fn a_cross_origin_issuer_is_refused_unless_acknowledged() {
        let err = Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://auth.elsewhere.test"),
            false,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .expect_err("a cross-origin issuer must not silently break discovery");
        let message = err.to_string();
        assert!(message.contains(ISSUER_EXTERNALLY_SERVED_ENV), "{message}");
        assert!(message.contains("https://auth.elsewhere.test"), "{message}");

        // Acknowledged: the operator has stated that origin publishes its own
        // metadata, so the document is built and points there.
        let acknowledged = Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://auth.elsewhere.test"),
            true,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .expect("an acknowledged external issuer must build");
        let doc: serde_json::Value =
            serde_json::from_str(acknowledged.protected_resource_json()).expect("valid JSON");
        assert_eq!(
            doc["authorization_servers"],
            json!(["https://auth.elsewhere.test"])
        );

        // A same-origin issuer with a PATH is not cross-origin and needs no
        // acknowledgement — the suffixed RFC 8414 well-known covers it, and it
        // is served from this very router.
        assert!(Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test/tenant-a"),
            false,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .is_ok());
    }

    /// Review round 3: the CONSTRUCTOR must enforce the scope rules, not just
    /// the env reader and not just the challenge builder.
    ///
    /// `required_scope` is interpolated into a `WWW-Authenticate` header and
    /// both inputs are published in the metadata documents, so a malformed
    /// token could otherwise reach the wire from a caller that did not go
    /// through `from_env`. `insufficient_scope_challenge`'s sanitizer defends
    /// the RUNTIME path (a scope arriving from a stored scoping record); it
    /// cannot defend a document rendered once at startup.
    #[test]
    fn the_constructor_validates_both_scope_inputs() {
        let build = |scopes: Vec<&str>, required: &str| {
            Discovery::new(
                uri("https://connector.test/mcp"),
                uri("https://connector.test"),
                false,
                scopes.into_iter().map(str::to_string).collect(),
                required.to_string(),
                false,
            )
        };

        // A header-breaking character in the ADVERTISED list.
        assert!(build(vec!["mcp", "ad\"min"], "mcp").is_err());
        assert!(build(vec!["mcp", "back\\slash"], "mcp").is_err());
        // Whitespace inside a token is a delimiter mistake, not a scope.
        assert!(build(vec!["mcp", "two words"], "mcp").is_err());
        assert!(build(vec!["mcp", "\ttab"], "mcp").is_err());
        // An empty entry can only come from a delimiter mistake, so it is
        // refused rather than skipped — skipping would publish a list subtly
        // different from the one that was written.
        assert!(build(vec!["mcp", ""], "mcp").is_err());

        // And the same rules on the REQUIRED scope, which is the one that
        // reaches a header.
        assert!(build(vec!["mcp"], "").is_err());
        assert!(build(vec!["mcp"], "mcp  admin").is_err(), "double space");
        assert!(build(vec!["mcp"], "mcp\tadmin").is_err(), "tab is not a delimiter");
        assert!(build(vec!["mcp"], "mcp ").is_err(), "trailing space");
        assert!(build(vec!["mcp", "ad\"min"], "ad\"min").is_err());

        // The rule that already existed still holds: a requirement the resource
        // does not advertise is unobtainable.
        assert!(build(vec!["mcp"], "admin").is_err());

        // Valid input is still accepted — a guard that refuses real
        // configuration is worse than the gap it closes.
        let ok = build(vec!["mcp", "offline_access", "profile:read"], "mcp offline_access")
            .expect("a valid scope configuration must build");
        assert!(ok
            .insufficient_scope_challenge("mcp offline_access")
            .contains("scope=\"mcp offline_access\""));
    }

    /// A scope list is configuration, so a typo in it fails loudly rather than
    /// silently narrowing what the resource advertises.
    #[test]
    fn scope_list_parsing_is_fail_closed() {
        // A well-formed list parses to itself, in order.
        assert_eq!(
            parse_scope_list("V", "mcp offline_access").expect("valid"),
            vec!["mcp".to_string(), "offline_access".to_string()],
        );

        // ROUND 4: every separator that is not a SINGLE space is a malformed
        // list, not an alternative spelling. `split_whitespace` used to absorb
        // all of these and publish a list the operator did not write — which
        // also made the constructor's strict rule unreachable for the only
        // values that matter, the configured ones.
        for bad in [
            "  mcp offline_access",   // leading
            "mcp offline_access ",    // trailing
            "mcp  offline_access",    // repeated
            "mcp\toffline_access",    // tab
            "mcp\noffline_access",    // newline
            "   ",                    // whitespace only
            "",                       // empty
            "mcp mcp",                // duplicate: a typo, not a shorthand
        ] {
            assert!(parse_scope_list("V", bad).is_err(), "must refuse {bad:?}");
        }

        // Charset rules still hold.
        assert!(parse_scope_list("V", "mcp \"admin\"").is_err());
        assert!(parse_scope_list("V", "back\\slash").is_err());
    }

    /// The claim the strict parser exists to make good: parsing a configured
    /// scope list and re-joining it reproduces the configured string BYTE FOR
    /// BYTE. If that ever stops holding, the reader is normalizing again and
    /// `required_scope` is publishing something nobody wrote.
    #[test]
    fn parsing_a_scope_list_is_an_identity_round_trip() {
        for configured in ["mcp", "mcp offline_access", "a b c"] {
            let parsed = parse_scope_list("V", configured).expect("valid");
            assert_eq!(
                parsed.join(" "),
                configured,
                "the strict parser must not alter a valid list"
            );
        }
    }

    /// The request-path sanitizer drops rather than fails, because no header at
    /// all is strictly worse for the client than a reduced one.
    #[test]
    fn sanitize_scope_drops_unusable_tokens() {
        assert_eq!(sanitize_scope("mcp offline_access"), "mcp offline_access");
        assert_eq!(sanitize_scope("mcp \"bad\" ok"), "mcp ok");
        assert_eq!(sanitize_scope(""), "");
    }
}
