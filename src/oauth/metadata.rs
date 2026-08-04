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
pub const CANONICAL_RESOURCE_ENV: &str = "RMCP_CANONICAL_RESOURCE";

/// The OAuth issuer identifier. Optional — defaults to the canonical
/// resource's ORIGIN, which is what a single-host deployment always wants.
pub const ISSUER_ENV: &str = "RMCP_ISSUER";

/// Space-separated scopes this resource advertises. Optional.
pub const SCOPES_SUPPORTED_ENV: &str = "RMCP_SCOPES_SUPPORTED";

/// The scope an access token must carry to reach `/mcp`. Optional.
pub const REQUIRED_SCOPE_ENV: &str = "RMCP_REQUIRED_SCOPE";

/// Whether RFC 7591 dynamic client registration is enabled. Optional,
/// default OFF — see [`Discovery::new`] for why the metadata key is omitted
/// rather than advertised-but-refusing.
pub const DCR_ENABLED_ENV: &str = "RMCP_DCR_ENABLED";

/// Operator acknowledgement that a cross-origin [`ISSUER_ENV`] is served
/// elsewhere. Optional, default off — see [`Discovery::new`] for why a
/// cross-origin issuer is refused without it.
pub const ISSUER_EXTERNALLY_SERVED_ENV: &str = "RMCP_ISSUER_EXTERNALLY_SERVED";

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
        let Some(raw_resource) = read_env(CANONICAL_RESOURCE_ENV) else {
            return Ok(None);
        };
        let resource = CanonicalUri::parse(CANONICAL_RESOURCE_ENV, &raw_resource)?;

        let issuer = match read_env(ISSUER_ENV) {
            Some(raw) => CanonicalUri::parse(ISSUER_ENV, &raw)?,
            // Default to the resource's ORIGIN rather than to the resource
            // itself: an issuer with a path forces the path-suffixed RFC 8414
            // well-known, which fewer clients probe. A single-host deployment
            // has no reason to want that.
            None => CanonicalUri::parse(ISSUER_ENV, resource.origin())?,
        };

        let scopes = match read_env(SCOPES_SUPPORTED_ENV) {
            Some(raw) => parse_scope_list(SCOPES_SUPPORTED_ENV, &raw)?,
            None => DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
        };

        let required_scope = match read_env(REQUIRED_SCOPE_ENV) {
            Some(raw) => {
                let parsed = parse_scope_list(REQUIRED_SCOPE_ENV, &raw)?;
                parsed.join(" ")
            }
            None => DEFAULT_REQUIRED_SCOPE.to_string(),
        };

        // Trimmed explicitly at the call site, now that `read_env` returns the
        // value verbatim. A boolean flag is the opposite case from the
        // canonical URI: nothing compares it byte-for-byte against anything, so
        // ` true` meaning `true` costs nothing, whereas refusing it would fail
        // a deployment over an invisible character for no gain. The
        // normalization rule this module enforces is about values that are
        // PUBLISHED and COMPARED, not about every string it reads.
        let dcr_enabled = read_env(DCR_ENABLED_ENV)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        let issuer_externally_served = read_env(ISSUER_EXTERNALLY_SERVED_ENV)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

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

        if scopes_supported.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "{SCOPES_SUPPORTED_ENV} resolved to an empty scope list — a protected resource \
                 that advertises no scopes gives the client nothing to request"
            )));
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
fn read_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
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
    let mut out = Vec::new();
    for token in raw.split_whitespace() {
        if !token.bytes().all(is_scope_char) {
            return Err(ToolError::InvalidArgument(format!(
                "{var} contains {token:?}, which is not a valid OAuth scope token (RFC 6749 \
                 permits %x21 / %x23-5B / %x5D-7E — notably not a quote or a backslash, which \
                 would break the `WWW-Authenticate` header this value is interpolated into)"
            )));
        }
        if !out.iter().any(|s: &String| s == token) {
            out.push(token.to_string());
        }
    }
    if out.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{var} is present but contains no scope tokens"
        )));
    }
    Ok(out)
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

        // A whitespace-ONLY value is ABSENT (door disabled), not malformed —
        // "there is nothing here" is a different statement from "there is
        // something here and I have quietly altered it".
        std::env::set_var(CANONICAL_RESOURCE_ENV, "   ");
        let blank = Discovery::from_env();
        std::env::remove_var(CANONICAL_RESOURCE_ENV);
        assert!(
            matches!(blank, Ok(None)),
            "a blank value disables the door rather than failing startup"
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

    /// A scope list is configuration, so a typo in it fails loudly rather than
    /// silently narrowing what the resource advertises.
    #[test]
    fn scope_list_parsing_is_fail_closed() {
        assert_eq!(
            parse_scope_list("V", "  mcp   offline_access  mcp ").expect("valid"),
            vec!["mcp".to_string(), "offline_access".to_string()],
            "duplicates collapse, order is preserved"
        );
        assert!(parse_scope_list("V", "mcp \"admin\"").is_err());
        assert!(parse_scope_list("V", "back\\slash").is_err());
        assert!(parse_scope_list("V", "   ").is_err());
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
