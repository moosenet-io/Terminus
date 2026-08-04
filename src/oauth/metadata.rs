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
        let raw = value.trim();
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
        if authority.is_empty() {
            return Err(refuse("has no host"));
        }
        if authority.contains('@') {
            return Err(refuse("must not carry userinfo"));
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

        let dcr_enabled = read_env(DCR_ENABLED_ENV)
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

        Self::new(resource, issuer, scopes, required_scope, dcr_enabled).map(Some)
    }

    /// Build the documents. Separate from [`Self::from_env`] so every property
    /// below can be tested without touching process-global environment state,
    /// which would race the rest of the suite.
    pub fn new(
        resource: CanonicalUri,
        issuer: CanonicalUri,
        scopes_supported: Vec<String>,
        required_scope: String,
        dcr_enabled: bool,
    ) -> Result<Self, ToolError> {
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

/// Read an env var, treating blank as absent.
///
/// The runtime secret store materializes into the process environment at
/// startup (see this module's doc and `crate::pki`'s), so this IS the
/// configuration read for this module; there is no second path.
fn read_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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

    fn uri(value: &str) -> CanonicalUri {
        CanonicalUri::parse("TEST_VAR", value).expect("fixture must parse")
    }

    fn discovery(dcr_enabled: bool) -> Discovery {
        Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test"),
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
            vec!["mcp".to_string()],
            "admin".to_string(),
            false,
        )
        .expect_err("an unobtainable requirement must be refused");
        assert!(err.to_string().contains("admin"));

        assert!(Discovery::new(
            uri("https://connector.test/mcp"),
            uri("https://connector.test"),
            vec![],
            "mcp".to_string(),
            false,
        )
        .is_err());
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
