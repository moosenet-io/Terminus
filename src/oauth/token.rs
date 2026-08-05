//! `POST /oauth/token` — the RMCP token endpoint (RMCP-04).
//!
//! Two grants land here: `authorization_code`, which turns the single-use code
//! RMCP-03 handed the browser into an audience-bound access token, and
//! `refresh_token`, which rotates a refresh token into its successor.
//!
//! ## Why this endpoint parses its own body
//! Claude sends BOTH the initial exchange and every refresh as
//! `application/x-www-form-urlencoded`, per OAuth 2.1. A JSON-only body
//! extractor answers such a request with a bare framework `415` — no `error`
//! field, no `error_description` — and the client has no way to distinguish
//! that from the server being broken. Every connection would fail identically
//! and silently. So the content type is checked here and a wrong one is
//! answered with a real OAuth error object, which is also why the handler takes
//! raw [`axum::body::Bytes`] rather than [`axum::extract::Form`]: a rejection
//! from an extractor is a framework response, not an OAuth one.
//!
//! Duplicate parameters are rejected rather than last-write-wins. RFC 6749 §3.1
//! forbids repeating a parameter, and a server that silently picks one of two
//! `resource` or `scope` values is a request-smuggling surface: a proxy and an
//! origin that pick differently disagree about what was authorized.
//!
//! ## What actually stops the attacks
//! - **The audience binding.** The access token's `aud` is the `resource` bound
//!   to the CODE (or to the refresh token's family), never a value taken from
//!   this request. A `resource` parameter is only ever allowed to AGREE with
//!   the binding; it can never establish one. That is what stops a token minted
//!   for a federated peer being replayed at this server.
//! - **Single use, decided in SQL.** The code is consumed by
//!   [`crate::oauth::store::OauthStore::consume_auth_code`], one conditional
//!   `UPDATE`, so two concurrent redemptions cannot both win. Nothing in this
//!   file re-implements that check, because a read-then-write here would
//!   reintroduce exactly the race the store exists to close.
//! - **Consume first, then validate.** The code is burned BEFORE the PKCE,
//!   redirect and resource checks run, so a caller holding a stolen code cannot
//!   retry it with different parameters until one combination is accepted. A
//!   failed exchange leaves the code dead, which is the correct outcome: RFC
//!   6749 §4.1.2 already requires a code presented twice to be treated as
//!   compromised.
//! - **Client authentication happens before the code is touched**, so an
//!   unauthenticated caller cannot burn codes it does not own.
//! - **Rotation reuse is theft.** Presenting an already-rotated refresh token
//!   revokes the whole family. The legitimate holder and the thief cannot be
//!   told apart, so both are cut off and the human re-authorizes.
//!
//! ## Error codes are load-bearing, not cosmetic
//! Claude keys its re-authorization on RFC 6749's `invalid_grant`: an expired or
//! revoked refresh token MUST answer exactly that, and a well-meaning custom
//! code (`refresh_expired`, `token_revoked`) strands the user in a connector
//! that never recovers. Every error out of this endpoint is therefore one of the
//! registered codes, and the human-readable detail lives in
//! `error_description`.
//!
//! ## Latency
//! Anthropic allows 10 seconds for a token request and 30 for a refresh. No
//! bookkeeping is deferred INTO the request path — no expired-code purge, no
//! audit fan-out, no cache warm — so the response time is the handful of
//! database round trips the exchange genuinely requires plus one argon2
//! verification for a confidential client.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::audit::{AuditDetail, DenialReason, OauthAuditRecord, OauthEvent};
use crate::oauth::jwt::JwtSigner;
use crate::oauth::limits::OauthEndpoint;
use crate::oauth::model::Client;
use crate::oauth::store::OauthStore;
use crate::oauth::SecretHash;

/// The path this endpoint registers. It must match the `token_endpoint` field
/// of the RMCP-02 authorization-server metadata document, which is the only
/// place a client learns it.
pub const TOKEN_PATH: &str = "/oauth/token";

/// Env var overriding [`DEFAULT_REFRESH_TTL_SECONDS`].
pub const REFRESH_TTL_ENV: &str = "RMCP_OAUTH_REFRESH_TOKEN_TTL_SECONDS";

/// Thirty days. Long enough that a connector left idle over a holiday still
/// works, short enough that an abandoned grant does not live forever. Rotation
/// means a token in active use is replaced far more often than this.
pub const DEFAULT_REFRESH_TTL_SECONDS: i64 = 30 * 24 * 3600;

/// Ninety days is the longest a single unattended grant may be extended to.
const MAX_REFRESH_TTL_SECONDS: i64 = 90 * 24 * 3600;

/// The only content type this endpoint accepts.
const FORM_MEDIA_TYPE: &str = "application/x-www-form-urlencoded";

/// Entropy per refresh token — 256 bits, per the item's requirement, and the
/// reason [`crate::oauth::secret_hash`] can be an unsalted digest.
const REFRESH_TOKEN_BYTES: usize = 32;

/// The scope value that gates issuing a refresh token at all.
const OFFLINE_ACCESS: &str = "offline_access";

const GRANT_AUTHORIZATION_CODE: &str = "authorization_code";
const GRANT_REFRESH_TOKEN: &str = "refresh_token";

/// RFC 7636 §4.1 bounds on `code_verifier`.
const MIN_VERIFIER_LEN: usize = 43;
const MAX_VERIFIER_LEN: usize = 128;

/// Bound on the request body. The largest legitimate token request is a few
/// hundred bytes; this cheaply removes a trivial DoS vector on a pre-auth,
/// internet-facing route.
const MAX_BODY_BYTES: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The RFC 6749 §5.2 error codes this endpoint can return.
///
/// Deliberately a closed enum with no `Other(String)` variant: the whole point
/// is that no call site can invent a code, because a code Claude does not
/// recognise is indistinguishable from a permanent failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OauthErrorCode {
    InvalidRequest,
    InvalidClient,
    InvalidGrant,
    UnauthorizedClient,
    UnsupportedGrantType,
    InvalidScope,
    ServerError,
}

impl OauthErrorCode {
    /// The wire value. Exactly the registered spelling, always.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidClient => "invalid_client",
            Self::InvalidGrant => "invalid_grant",
            Self::UnauthorizedClient => "unauthorized_client",
            Self::UnsupportedGrantType => "unsupported_grant_type",
            Self::InvalidScope => "invalid_scope",
            Self::ServerError => "server_error",
        }
    }

    /// The HTTP status this code is returned with (RFC 6749 §5.2: `400` for
    /// everything except a client-authentication failure, which is `401`).
    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidClient => StatusCode::UNAUTHORIZED,
            Self::ServerError => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// An error response.
///
/// `description` is `&'static str` on purpose: it can never echo attacker-
/// controlled input back into a response or a log line, and it forces the
/// messages to be about the CLASS of failure rather than about the specific
/// value that failed — which would be an oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OauthError {
    pub code: OauthErrorCode,
    pub description: &'static str,
    /// Whether to answer with a `WWW-Authenticate` challenge, which RFC 6749
    /// requires when the client attempted HTTP Basic authentication.
    challenge: bool,
}

impl OauthError {
    pub fn new(code: OauthErrorCode, description: &'static str) -> Self {
        Self { code, description, challenge: false }
    }

    fn invalid_request(description: &'static str) -> Self {
        Self::new(OauthErrorCode::InvalidRequest, description)
    }

    fn invalid_grant(description: &'static str) -> Self {
        Self::new(OauthErrorCode::InvalidGrant, description)
    }

    fn invalid_client(description: &'static str) -> Self {
        Self::new(OauthErrorCode::InvalidClient, description)
    }

    /// The OAuth-audit reason this error is recorded under.
    ///
    /// A mapping into the audit's closed vocabulary rather than logging the
    /// error's own `description`: that description is a `&'static str` today,
    /// but it is also the thing most likely to grow a caller-derived detail
    /// later, and the audit record is where that would become a leak. Mapping
    /// to a variant makes the trail's vocabulary independent of the wire text.
    fn audit_reason(&self) -> DenialReason {
        match self.code {
            OauthErrorCode::InvalidClient => DenialReason::UnknownOrDisabledClient,
            OauthErrorCode::InvalidGrant => DenialReason::RefreshNotUsable,
            OauthErrorCode::UnsupportedGrantType | OauthErrorCode::UnauthorizedClient => {
                DenialReason::UnsupportedGrant
            }
            OauthErrorCode::InvalidScope | OauthErrorCode::InvalidRequest => {
                DenialReason::MalformedRequest
            }
            OauthErrorCode::ServerError => DenialReason::MalformedRequest,
        }
    }

    /// Mark this as answerable with a `WWW-Authenticate: Basic` challenge.
    fn with_challenge(mut self, challenge: bool) -> Self {
        self.challenge = challenge;
        self
    }

    /// Collapse an internal failure into `server_error`.
    ///
    /// The underlying [`ToolError`] is logged, never returned: a store error
    /// can carry schema detail, and this endpoint answers unauthenticated
    /// callers.
    fn internal(context: &str, e: ToolError) -> Self {
        tracing::error!("oauth::token: {context}: {e}");
        Self::new(OauthErrorCode::ServerError, "the request could not be completed")
    }

    fn body(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.code.as_str(),
            "error_description": self.description,
        })
    }
}

impl IntoResponse for OauthError {
    fn into_response(self) -> Response {
        let mut response =
            (self.code.status(), axum::Json(self.body())).into_response();
        set_no_store(response.headers_mut());
        if self.challenge {
            if let Ok(value) = header::HeaderValue::from_str("Basic realm=\"rmcp\"") {
                response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
            }
        }
        response
    }
}

/// A token response carries bearer credentials, so it must not be cached by
/// anything on the path (RFC 6749 §5.1). Applied to error responses too: they
/// are equally uninteresting to cache and the rule is easier to keep when it
/// has no exceptions.
fn set_no_store(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
}

// ---------------------------------------------------------------------------
// Success response
// ---------------------------------------------------------------------------

/// The RFC 6749 §5.1 success body.
///
/// No `Debug`: two of its fields are presentable bearer credentials.
#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub scope: String,
    /// Present only when the grant carries `offline_access`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl IntoResponse for TokenResponse {
    fn into_response(self) -> Response {
        let mut response = axum::Json(self).into_response();
        set_no_store(response.headers_mut());
        response
    }
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// The parsed form body: at most one value per parameter.
#[derive(Debug, Default)]
pub struct TokenParams(BTreeMap<String, String>);

impl TokenParams {
    /// A present, non-empty value. An empty parameter reads as ABSENT rather
    /// than as an empty credential — the same rule the rest of this module
    /// applies to a blank materialized secret.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn require(&self, key: &str, missing: &'static str) -> Result<&str, OauthError> {
        self.get(key).ok_or_else(|| OauthError::invalid_request(missing))
    }
}

/// Reject anything that is not a form body, with an OAuth error.
pub fn require_form_content_type(headers: &HeaderMap) -> Result<(), OauthError> {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // Parameters such as `; charset=utf-8` are legal and must not cause a
    // rejection; only the media type is compared, case-insensitively.
    let media_type = raw.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case(FORM_MEDIA_TYPE) {
        Ok(())
    } else {
        Err(OauthError::invalid_request(
            "the token endpoint accepts only application/x-www-form-urlencoded",
        ))
    }
}

/// Parse a form body, refusing a repeated parameter.
pub fn parse_form(body: &[u8]) -> Result<TokenParams, OauthError> {
    let mut params = BTreeMap::new();
    for (key, value) in form_urlencoded::parse(body) {
        if params.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(OauthError::invalid_request(
                "a parameter was supplied more than once",
            ));
        }
    }
    Ok(TokenParams(params))
}

// ---------------------------------------------------------------------------
// Client authentication
// ---------------------------------------------------------------------------

/// What a request claimed about which client it is, before any of it is
/// believed.
#[derive(Clone, PartialEq, Eq)]
pub struct PresentedClientAuth {
    pub client_id: String,
    secret: Option<String>,
    /// Whether HTTP Basic was used, which decides whether a failure carries a
    /// `WWW-Authenticate` challenge.
    pub via_basic: bool,
}

impl std::fmt::Debug for PresentedClientAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentedClientAuth")
            .field("client_id", &self.client_id)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("via_basic", &self.via_basic)
            .finish()
    }
}

/// Decode one `application/x-www-form-urlencoded` component.
///
/// RFC 6749 §2.3.1 requires the two halves of a Basic credential to be form-
/// encoded before base64, so a secret containing `:` or a non-ASCII character
/// survives the round trip. Decoding is done here rather than with
/// `form_urlencoded::parse`, which would additionally split on `&` and `=` —
/// characters that are legal INSIDE an encoded component.
fn form_decode_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // A stray `%` that is not an escape is kept verbatim rather
                    // than dropped: silently deleting bytes from a credential
                    // would turn a typo into a different, possibly valid, value.
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Work out which client a request claims to be, and with what credential.
///
/// Presenting BOTH a Basic header and a `client_secret` body parameter is
/// [`OauthErrorCode::InvalidRequest`], not "pick one" — RFC 6749 §2.3 says a
/// client MUST NOT use more than one authentication method per request, and a
/// server that picks silently lets a caller probe which credential the server
/// actually checked.
pub fn extract_client_auth(
    headers: &HeaderMap,
    params: &TokenParams,
) -> Result<PresentedClientAuth, OauthError> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let basic = match authorization {
        None => None,
        Some(value) => {
            let credential = value
                .split_once(' ')
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Basic"))
                .map(|(_, rest)| rest.trim())
                .ok_or_else(|| {
                    // An unrecognised scheme is a failed authentication attempt,
                    // not a request to fall through to the body: falling through
                    // would let a caller send a bogus header and still be treated
                    // as an unauthenticated public client.
                    OauthError::invalid_client(
                        "unsupported client authentication scheme; use Basic or client_secret_post",
                    )
                    .with_challenge(true)
                })?;
            let decoded = {
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine as _;
                STANDARD.decode(credential).ok().and_then(|b| String::from_utf8(b).ok())
            }
            .ok_or_else(|| {
                OauthError::invalid_client("malformed Basic credential").with_challenge(true)
            })?;
            let (user, pass) = decoded.split_once(':').ok_or_else(|| {
                OauthError::invalid_client("malformed Basic credential").with_challenge(true)
            })?;
            Some((form_decode_component(user), form_decode_component(pass)))
        }
    };

    let body_client_id = params.get("client_id");
    let body_secret = params.get("client_secret");

    match basic {
        Some((basic_id, basic_secret)) => {
            if body_secret.is_some() {
                return Err(OauthError::invalid_request(
                    "present exactly one client credential: Basic or client_secret_post, never both",
                ));
            }
            // A `client_id` in the body ALONGSIDE Basic is permitted for
            // identification, but only if it agrees. A disagreement is a
            // confused or malicious request either way.
            if let Some(body_id) = body_client_id {
                if body_id != basic_id {
                    return Err(OauthError::invalid_request(
                        "client_id in the body disagrees with the Basic credential",
                    ));
                }
            }
            if basic_id.is_empty() {
                return Err(
                    OauthError::invalid_client("Basic credential names no client")
                        .with_challenge(true),
                );
            }
            Ok(PresentedClientAuth {
                client_id: basic_id,
                secret: Some(basic_secret).filter(|s| !s.is_empty()),
                via_basic: true,
            })
        }
        None => {
            let client_id = body_client_id.ok_or_else(|| {
                OauthError::invalid_request("client_id is required")
            })?;
            Ok(PresentedClientAuth {
                client_id: client_id.to_string(),
                secret: body_secret.map(str::to_string),
                via_basic: false,
            })
        }
    }
}

/// Verify a presented secret against a stored argon2id PHC string.
///
/// Any failure — an unparseable stored hash, a wrong secret — is one `false`.
/// argon2's own verifier is used rather than a comparison of re-derived
/// digests, because it reads the cost parameters and salt back out of the PHC
/// string and compares in constant time.
fn verify_client_secret(presented: &str, stored_phc: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    match PasswordHash::new(stored_phc) {
        Ok(parsed) => argon2::Argon2::default()
            .verify_password(presented.as_bytes(), &parsed)
            .is_ok(),
        Err(e) => {
            // A stored hash that will not parse is an operational fault, not a
            // caller error — worth a log, but it must still deny.
            tracing::error!("oauth::token: stored client secret hash is unparseable: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// The RFC 7636 S256 transformation: unpadded base64url of the SHA-256 of the
/// verifier's ASCII bytes.
pub fn s256_challenge(code_verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Whether `code_verifier` is the preimage of `stored_challenge`.
///
/// The comparison is constant-time ([`crate::pki::enroll::constant_time_eq`],
/// reused rather than re-rolled): a byte-at-a-time `==` on the challenge leaks,
/// through timing, how much of a guessed verifier's digest was correct, which
/// turns an offline search into an online one.
///
/// An EMPTY stored challenge denies. PKCE is mandatory for this door, so a code
/// somehow persisted without a challenge must not be exchangeable — the
/// tempting reading, "no challenge recorded, so nothing to check", is exactly
/// the bypass.
pub fn verify_pkce(code_verifier: &str, stored_challenge: &str) -> bool {
    if stored_challenge.is_empty() {
        return false;
    }
    crate::pki::enroll::constant_time_eq(
        s256_challenge(code_verifier).as_bytes(),
        stored_challenge.as_bytes(),
    )
}

/// Whether a `code_verifier` is syntactically legal (RFC 7636 §4.1): 43–128
/// characters from the unreserved set. Checked before it is used, so a
/// malformed value is a request error rather than a silent digest of garbage.
pub fn verifier_is_well_formed(code_verifier: &str) -> bool {
    let len = code_verifier.len();
    (MIN_VERIFIER_LEN..=MAX_VERIFIER_LEN).contains(&len)
        && code_verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Whether a space-delimited scope string contains an exact scope token.
/// Substring matching would make `offline_access_denied` grant offline access.
pub fn scope_contains(scope: &str, wanted: &str) -> bool {
    scope.split(' ').any(|token| token == wanted)
}

/// Whether every token of `requested` appears in `granted`.
///
/// RFC 6749 §6 lets a refresh narrow the scope but never widen it. An empty
/// request is a subset of anything, which is why the caller treats an absent
/// `scope` parameter as "keep what was granted" rather than routing it here.
pub fn scope_is_subset(requested: &str, granted: &str) -> bool {
    requested
        .split(' ')
        .filter(|token| !token.is_empty())
        .all(|token| scope_contains(granted, token))
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// Everything the token endpoint needs: the store, the signer, and the refresh
/// lifetime.
#[derive(Clone)]
pub struct TokenEndpoint {
    store: OauthStore,
    signer: JwtSigner,
    refresh_ttl_seconds: i64,
    /// The one VERIFIED revocation path (RMCP-11). Built over the same store, so
    /// a family revocation triggered here goes through the same
    /// snapshot-write-reread as an operator's `rmcp_session_revoke` — see
    /// [`Self::revoke_family`].
    revocation: crate::oauth::revoke::RevocationService,
}

impl TokenEndpoint {
    /// Build an endpoint over an already-opened store and signer.
    pub fn new(
        store: OauthStore,
        signer: JwtSigner,
        refresh_ttl_seconds: i64,
    ) -> Result<Self, ToolError> {
        if refresh_ttl_seconds <= 0 || refresh_ttl_seconds > MAX_REFRESH_TTL_SECONDS {
            return Err(ToolError::NotConfigured(format!(
                "{REFRESH_TTL_ENV} must be between 1 and {MAX_REFRESH_TTL_SECONDS} seconds"
            )));
        }
        // The verified revocation path, over the SAME store this endpoint
        // reads through, so a reuse-triggered revocation and an operator's
        // `rmcp_session_revoke` are the same code against the same rows.
        let revocation = crate::oauth::revoke::RevocationService::new(
            std::sync::Arc::new(store.clone()),
        );
        Ok(Self { store, signer, refresh_ttl_seconds, revocation })
    }

    /// Build from the environment (see [`crate::oauth`] on why an env read is
    /// the vault read here).
    pub fn from_env(store: OauthStore) -> Result<Self, ToolError> {
        let ttl = std::env::var(REFRESH_TTL_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_REFRESH_TTL_SECONDS);
        Self::new(store, JwtSigner::from_env()?, ttl)
    }

    /// Handle one token request, from raw headers and body.
    ///
    /// Split out from the axum handler so the ordering below — content type,
    /// then client authentication, then the grant — is one readable sequence
    /// rather than something distributed across extractors.
    pub async fn handle(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<TokenResponse, OauthError> {
        // TWO stages, each with exactly one place that sees every outcome.
        //
        // It used to be one function whose first five steps were `?`, so a
        // wrong content type, an undecodable body, an UNKNOWN CLIENT or a
        // FAILED CLIENT SECRET returned before reaching the audit at the grant
        // boundary — unaudited pre-auth refusals on an internet-facing
        // endpoint, and the last two are exactly what an operator wants to see.
        // Round 14 (`gpt56`) named the same class on `/oauth/revoke`; this is
        // where it was widest.
        //
        // Splitting at "is there an authenticated client yet" is what makes a
        // single emission point possible on each side: before it there is no
        // client to name, after it every record can carry one.
        let (params, client) = match self.authenticate_request(headers, body).await {
            Ok(authenticated) => authenticated,
            Err(e) => {
                OauthAuditRecord::new(OauthEvent::TokenDenied)
                    .endpoint(OauthEndpoint::Token)
                    .reason(e.audit_reason())
                    .emit();
                return Err(e);
            }
        };

        // Read back for the emission below rather than borrowed across the
        // grant call, so `run_grant` can take `&params` without a borrow that
        // outlives it.
        let grant_type = params.get("grant_type").unwrap_or_default().to_string();
        let outcome = self.run_grant(&client, &params).await;

        match (&outcome, grant_type.as_str()) {
            (Ok(_), GRANT_AUTHORIZATION_CODE) => {
                OauthAuditRecord::new(OauthEvent::TokenIssued)
                    .endpoint(OauthEndpoint::Token)
                    .client_uuid(client.id)
                    .detail(AuditDetail::TokensIssuedForCode)
                    .emit();
            }
            (Ok(_), _) => {
                OauthAuditRecord::new(OauthEvent::TokenRefreshed)
                    .endpoint(OauthEndpoint::Token)
                    .client_uuid(client.id)
                    .detail(AuditDetail::TokensRefreshed {
                        scope_narrowed: params.get("scope").is_some(),
                    })
                    .emit();
            }
            (Err(e), _) => {
                OauthAuditRecord::new(OauthEvent::TokenDenied)
                    .endpoint(OauthEndpoint::Token)
                    .client_uuid(client.id)
                    .reason(e.audit_reason())
                    .emit();
            }
        }
        outcome
    }

    /// Everything before an authenticated client exists: content type, form
    /// decoding, and client authentication.
    ///
    /// Every failure in here is a refusal with no client to attribute it to,
    /// which is why it is one function with one caller — the caller audits its
    /// `Err` once, and no step inside can `?` past that.
    async fn authenticate_request(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<(TokenParams, Client), OauthError> {
        require_form_content_type(headers)?;
        let params = parse_form(body)?;
        let presented = extract_client_auth(headers, &params)?;
        let client = self.authenticate_client(&presented).await?;
        Ok((params, client))
    }

    /// Everything after the client is known: which grant, whether this client
    /// may use it, and running it.
    ///
    /// The two refusals before dispatch used to `return Err` past the audit
    /// too; folding them in here means the caller's single `match` sees them.
    async fn run_grant(
        &self,
        client: &Client,
        params: &TokenParams,
    ) -> Result<TokenResponse, OauthError> {
        let grant_type = params.require("grant_type", "grant_type is required")?;
        // Whether the CLIENT may use this grant is checked before the grant
        // runs. An empty `grant_types` column denies everything, per the
        // module's absence-is-denial rule.
        if !client.grant_types.iter().any(|g| g == grant_type) {
            return Err(OauthError::new(
                OauthErrorCode::UnauthorizedClient,
                "this client is not registered for the requested grant type",
            ));
        }
        match grant_type {
            GRANT_AUTHORIZATION_CODE => self.authorization_code_grant(client, params).await,
            GRANT_REFRESH_TOKEN => self.refresh_token_grant(client, params).await,
            _ => Err(OauthError::new(
                OauthErrorCode::UnsupportedGrantType,
                "supported grant types are authorization_code and refresh_token",
            )),
        }
    }

    /// Resolve and authenticate the client.
    ///
    /// Confidentiality follows the STORED secret ([`Client::is_confidential`]),
    /// not the advertised `token_endpoint_auth_method`: a row claiming a
    /// confidential method while holding no secret must not be "authenticated"
    /// by verifying against nothing.
    async fn authenticate_client(
        &self,
        presented: &PresentedClientAuth,
    ) -> Result<Client, OauthError> {
        let challenge = presented.via_basic;
        let client = self
            .store
            .find_active_client(&presented.client_id)
            .await
            .map_err(|e| OauthError::internal("client lookup failed", e))?
            .ok_or_else(|| {
                // Unknown and disabled are one answer, matching the store's own
                // rule: a disabled client must behave exactly like one that
                // never existed.
                OauthError::invalid_client("unknown or disabled client").with_challenge(challenge)
            })?;

        // Matched on the stored hash itself rather than on
        // [`Client::is_confidential`] so the "confidential" arm cannot reach a
        // hash that is not there — the two are the same predicate, but only one
        // of them makes that a compiler-checked fact.
        match (client.client_secret_hash.as_deref(), presented.secret.as_deref()) {
            (Some(stored), Some(secret)) => {
                if !verify_client_secret(secret, stored) {
                    return Err(OauthError::invalid_client("client authentication failed")
                        .with_challenge(challenge));
                }
            }
            (Some(_), None) => {
                return Err(OauthError::invalid_client(
                    "this client must authenticate at the token endpoint",
                )
                .with_challenge(challenge));
            }
            (None, Some(_)) => {
                // A secret for a client that has none cannot be verified
                // against anything, so accepting the request would mean
                // ignoring a credential the caller believed was checked.
                return Err(OauthError::invalid_client(
                    "this client is registered as public and must not present a secret",
                )
                .with_challenge(challenge));
            }
            (None, None) => {}
        }
        Ok(client)
    }

    /// `grant_type=authorization_code`.
    async fn authorization_code_grant(
        &self,
        client: &Client,
        params: &TokenParams,
    ) -> Result<TokenResponse, OauthError> {
        let code = params.require("code", "code is required")?;
        let redirect_uri = params.require("redirect_uri", "redirect_uri is required")?;
        let code_verifier = params.require("code_verifier", "code_verifier is required")?;
        if !verifier_is_well_formed(code_verifier) {
            return Err(OauthError::invalid_request(
                "code_verifier must be 43-128 unreserved characters",
            ));
        }

        // Burn the code FIRST. See the module docs: this is what makes a failed
        // exchange non-retryable, and the single-use guarantee lives in the
        // store's conditional UPDATE, not here.
        let record = self
            .store
            .consume_auth_code(&SecretHash::of(code))
            .await
            .map_err(|e| OauthError::internal("authorization code lookup failed", e))?
            .ok_or_else(|| {
                OauthError::invalid_grant(
                    "the authorization code is invalid, expired, or already used",
                )
            })?;

        // Everything below is one error message on purpose. Telling a caller
        // WHICH binding failed says whether it holds a real code for a
        // different client, a real code with the wrong verifier, and so on.
        let mismatch =
            || OauthError::invalid_grant("the authorization code does not match this request");

        if record.client_id != client.id {
            return Err(mismatch());
        }
        if record.redirect_uri != redirect_uri {
            return Err(mismatch());
        }
        if record.resource.trim().is_empty() {
            // An unbound code cannot produce a bound token, and minting an
            // unbound one is the thing the audience design exists to prevent.
            tracing::error!("oauth::token: authorization code carries no bound resource");
            return Err(mismatch());
        }
        if let Some(requested) = params.get("resource") {
            // A `resource` parameter may only AGREE with the binding the code
            // already carries; it can never establish or change one.
            if requested != record.resource {
                return Err(mismatch());
            }
        }
        if !verify_pkce(code_verifier, &record.code_challenge) {
            return Err(mismatch());
        }
        if !self.account_is_active(record.account_id).await? {
            return Err(OauthError::invalid_grant(
                "the authorizing account is no longer active",
            ));
        }

        let access = self
            .signer
            .mint(record.account_id, &client.client_id, &record.resource, &record.scope)
            .map_err(|e| OauthError::internal("access token minting failed", e))?;

        // `offline_access` is what gates a refresh token at all. Without it the
        // grant ends when the access token expires.
        let refresh_token = if scope_contains(&record.scope, OFFLINE_ACCESS) {
            let token = crate::oauth::random_token(REFRESH_TOKEN_BYTES)
                .map_err(|e| OauthError::internal("refresh token generation failed", e))?;
            self.store
                .insert_refresh_token(
                    &SecretHash::of(&token),
                    // A fresh family per authorization: rotation reuse inside
                    // one grant must not revoke a different grant's tokens.
                    Uuid::new_v4(),
                    client.id,
                    record.account_id,
                    &record.resource,
                    &record.scope,
                    self.refresh_ttl_seconds,
                )
                .await
                .map_err(|e| OauthError::internal("refresh token storage failed", e))?;
            Some(token)
        } else {
            None
        };

        Ok(TokenResponse {
            access_token: access.token,
            token_type: "Bearer",
            expires_in: access.expires_in,
            scope: record.scope,
            refresh_token,
        })
    }

    /// `grant_type=refresh_token`.
    async fn refresh_token_grant(
        &self,
        client: &Client,
        params: &TokenParams,
    ) -> Result<TokenResponse, OauthError> {
        let presented = params.require("refresh_token", "refresh_token is required")?;
        let presented_hash = SecretHash::of(presented);

        // Deliberately the UNFILTERED lookup: an already-rotated or revoked row
        // has to be visible, because its visibility IS the theft signal.
        let record = self
            .store
            .find_refresh_token(&presented_hash)
            .await
            .map_err(|e| OauthError::internal("refresh token lookup failed", e))?
            .ok_or_else(|| OauthError::invalid_grant("the refresh token is not valid"))?;

        // Expired, revoked, rotated, or wrong client all answer with exactly
        // `invalid_grant` — the code Claude keys its re-authorization on.
        let dead = || OauthError::invalid_grant("the refresh token is not valid");

        if record.client_id != client.id {
            // Possession of another client's refresh token is possession of a
            // leaked credential; the holder cannot be distinguished from a
            // thief, so the family goes. The alternative — a bare rejection —
            // leaves a token known to be in the wrong hands still live.
            self.revoke_family(
                record.family_id,
                FamilyRevocationCause::SuspectedTheft,
                "refresh token presented by a different client",
            )
                .await?;
            return Err(dead());
        }
        if record.is_rotated() {
            // THE reuse case. The legitimate holder and the thief cannot be
            // told apart, so both are cut off and the human re-authorizes.
            self.revoke_family(
                record.family_id,
                FamilyRevocationCause::SuspectedTheft,
                "rotated refresh token was presented again",
            )
                .await?;
            return Err(dead());
        }
        if record.revoked_at.is_some() {
            return Err(dead());
        }
        if !self.account_is_active(record.account_id).await? {
            self.revoke_family(
                record.family_id,
                FamilyRevocationCause::AccountDisabled,
                "account is disabled",
            )
            .await?;
            return Err(dead());
        }
        if record.resource.trim().is_empty() {
            // The same guard the authorization-code path applies, for the same
            // reason: an unbound row cannot produce a bound token. `mint` would
            // refuse this anyway, but refusing here keeps the answer
            // `invalid_grant` (a dead grant) rather than `server_error`, and
            // means neither path relies on the other to catch it.
            tracing::error!("oauth::token: refresh token row carries no bound resource");
            return Err(dead());
        }
        if let Some(requested) = params.get("resource") {
            if requested != record.resource {
                return Err(dead());
            }
        }

        // A refresh may NARROW the scope but never widen it (RFC 6749 §6). The
        // narrowing applies to the access token only: the successor refresh
        // token inherits the family's original binding from the store, so a
        // narrowed refresh cannot be used to permanently shrink — or later
        // re-widen — the grant.
        let effective_scope = match params.get("scope") {
            Some(requested) => {
                if !scope_is_subset(requested, &record.scope) {
                    return Err(OauthError::new(
                        OauthErrorCode::InvalidScope,
                        "the requested scope exceeds the scope originally granted",
                    ));
                }
                requested.to_string()
            }
            None => record.scope.clone(),
        };

        let successor = crate::oauth::random_token(REFRESH_TOKEN_BYTES)
            .map_err(|e| OauthError::internal("refresh token generation failed", e))?;
        let rotated = self
            .store
            .rotate_refresh_token(
                &presented_hash,
                &SecretHash::of(&successor),
                self.refresh_ttl_seconds,
            )
            .await
            .map_err(|e| OauthError::internal("refresh token rotation failed", e))?;
        if !rotated {
            // The row was live a moment ago and is not now: either it expired
            // against the database clock, or a concurrent request rotated it
            // first. The second case is indistinguishable from a thief racing
            // the legitimate client, so it is treated as one.
            self.revoke_family(
                record.family_id,
                FamilyRevocationCause::SuspectedTheft,
                "refresh token was not live at rotation",
            )
                .await?;
            return Err(dead());
        }

        // Rotation is its own event, distinct from the refresh that caused it:
        // a family's rotation count is what tells an operator a session is
        // genuinely in use, and a rotation with no matching refresh would be the
        // signature of something replaying against it.
        OauthAuditRecord::new(OauthEvent::TokenRotated)
            .endpoint(OauthEndpoint::Token)
            .account(record.account_id)
            .client_uuid(record.client_id)
            .family(record.family_id)
            .detail(AuditDetail::RefreshRotated)
            .emit();

        let access = self
            .signer
            .mint(record.account_id, &client.client_id, &record.resource, &effective_scope)
            .map_err(|e| OauthError::internal("access token minting failed", e))?;

        // The successor is returned in the SAME response that invalidated its
        // predecessor. Anything else leaves the client without a usable token.
        Ok(TokenResponse {
            access_token: access.token,
            token_type: "Bearer",
            expires_in: access.expires_in,
            scope: effective_scope,
            refresh_token: Some(successor),
        })
    }

    /// Revoke a family after detecting theft, FAILING CLOSED if the revocation
    /// itself fails.
    ///
    /// An earlier revision logged the failure and let the caller return
    /// `invalid_grant` anyway, reasoning that the client-facing answer was
    /// correct regardless. Review round 1 rejected that, correctly: revoking
    /// the family IS the entire response to detected theft, so swallowing its
    /// failure returns a clean-looking rejection to an attacker while every
    /// token in that family stays live — under-enforcement that surfaces as a
    /// log line rather than as an incident. A `server_error` on a database
    /// that genuinely cannot write is the honest outcome; the request must not
    /// report a rejection when the enforcement that was supposed to accompany
    /// it did not happen.
    ///
    /// The rejection is still what a caller sees on the SUCCESS path — this
    /// returns `Ok(())` and the caller then answers `invalid_grant`.
    async fn revoke_family(
        &self,
        family_id: Uuid,
        cause: FamilyRevocationCause,
        why: &str,
    ) -> Result<(), OauthError> {
        // Emitted BEFORE the write is attempted, so the detection is on the
        // record even when the revocation then fails — the failure case is
        // precisely the one an operator most needs to see, and an audit line
        // that only appears on success would omit it.
        if cause == FamilyRevocationCause::SuspectedTheft {
            OauthAuditRecord::new(OauthEvent::RefreshReuseDetected)
                .endpoint(OauthEndpoint::Token)
                .family(family_id)
                .reason(DenialReason::RefreshReused)
                .detail(AuditDetail::RefreshReuse)
                .emit();
        }

        // Through the SERVICE, never the store. `RevocationService` re-reads the
        // affected families after the write and errors if any is still live;
        // the direct `store.revoke_refresh_family` call this replaced reported
        // success on the strength of an UPDATE returning. Two ways to revoke a
        // family, only one of which checked its work — and the unchecked one was
        // on the theft path.
        //
        // `revoke_on_reuse` also emits the reuse record, so the theft arm above
        // deliberately does NOT use it: this endpoint emits its own with the
        // `Token` endpoint attached, and a second identical record would just be
        // noise. Both entry points land on the same verified primitive.
        match self.revocation.revoke_family_verified(family_id).await {
            Ok(report) => {
                tracing::warn!(
                    "oauth::token: revoked refresh family {family_id} \
                     ({} token(s), verified): {why}",
                    report.tokens_revoked
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    "oauth::token: FAILED to revoke refresh family {family_id} after detecting \
                     {why} — refusing to report a clean rejection while the family may still be \
                     live"
                );
                Err(OauthError::internal("refresh family revocation failed", e))
            }
        }
    }

    /// Whether the authorizing account is still usable.
    ///
    /// A disabled account must not be able to keep refreshing: the code or the
    /// refresh token was issued before the account was turned off, and the
    /// grant does not outlive the human.
    async fn account_is_active(&self, account_id: Uuid) -> Result<bool, OauthError> {
        self.store
            .account_is_active(account_id)
            .await
            .map_err(|e| OauthError::internal("account lookup failed", e))
    }
}

/// Why the token endpoint is revoking a refresh family.
///
/// Exists so the audit vocabulary stays accurate. All four of this endpoint's
/// family revocations used to emit [`OauthEvent::RefreshReuseDetected`], which
/// is right for three of them and wrong for the fourth: a family revoked because
/// its ACCOUNT was disabled is not a theft signal, and labelling it as one would
/// have an operator hunting a stolen credential that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FamilyRevocationCause {
    /// A refresh token was replayed, presented by the wrong client, or lost a
    /// rotation race — all indistinguishable from theft, so all treated as it.
    SuspectedTheft,
    /// The authorizing account is disabled. A revocation, not a theft signal.
    AccountDisabled,
}

/// Build the standalone token router, for a binary to merge into whatever it
/// already serves — the same shape as
/// [`crate::pki::enroll::build_enroll_router`], and separate for the same
/// reason: this endpoint's request/response and error shapes are its own, and a
/// separate router means it cannot change `/mcp`'s behaviour by accident.
pub fn build_token_router(endpoint: Arc<TokenEndpoint>) -> axum::Router {
    axum::Router::new()
        .route(TOKEN_PATH, axum::routing::post(handle_token))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(endpoint)
}

/// The axum glue. Deliberately thin: it takes the body as raw bytes so that a
/// wrong content type reaches [`require_form_content_type`] and comes back as
/// an OAuth error, rather than being turned into a framework `415` by an
/// extractor before this code runs at all.
async fn handle_token(
    axum::extract::State(endpoint): axum::extract::State<Arc<TokenEndpoint>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match endpoint.handle(&headers, &body).await {
        Ok(response) => response.into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                header::HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    fn form_headers() -> HeaderMap {
        headers_with(&[("content-type", FORM_MEDIA_TYPE)])
    }

    /// The failure mode that silently breaks every Claude connection: a JSON
    /// body must produce an OAuth error object, never a bare framework 415.
    #[test]
    fn a_json_body_is_an_oauth_error_not_a_bare_415() {
        let headers = headers_with(&[("content-type", "application/json")]);
        let err = require_form_content_type(&headers).expect_err("must reject");
        assert_eq!(err.code, OauthErrorCode::InvalidRequest);
        assert_eq!(err.code.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.body()["error"], "invalid_request");

        // A missing content type is equally a rejection, not a default.
        assert!(require_form_content_type(&HeaderMap::new()).is_err());
    }

    /// Claude sends the form type with and without a charset parameter, and
    /// casing is not guaranteed. All of these must be accepted.
    #[test]
    fn form_content_type_variants_are_accepted() {
        for value in [
            FORM_MEDIA_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
            "APPLICATION/X-WWW-FORM-URLENCODED",
            "  application/x-www-form-urlencoded  ",
        ] {
            let headers = headers_with(&[("content-type", value)]);
            assert!(require_form_content_type(&headers).is_ok(), "must accept {value}");
        }
    }

    #[test]
    fn form_parsing_decodes_and_rejects_duplicates() {
        let params =
            parse_form(b"grant_type=authorization_code&redirect_uri=https%3A%2F%2Fapp.test%2Fcb")
                .expect("parses");
        assert_eq!(params.get("grant_type"), Some("authorization_code"));
        assert_eq!(params.get("redirect_uri"), Some("https://app.test/cb"));
        // An empty parameter reads as absent, not as an empty credential.
        assert_eq!(parse_form(b"client_secret="<REDACTED-SECRET>"client_secret"), None);

        let err = parse_form(b"resource=a&resource=b").expect_err("duplicate must be refused");
        assert_eq!(err.code, OauthErrorCode::InvalidRequest);
    }

    /// RFC 6749 §2.3: one authentication method per request. Picking one
    /// silently would let a caller probe which credential was actually checked.
    #[test]
    fn presenting_both_basic_and_post_credentials_is_invalid_request() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let headers = headers_with(&[(
            "authorization",
            &format!("Basic {}", STANDARD.encode("a-client:a-secret")),
        )]);
        let params = parse_form(b"client_secret=a-secret").expect("parses");
        let err = extract_client_auth(&headers, &params).expect_err("must refuse both");
        assert_eq!(err.code, OauthErrorCode::InvalidRequest);
    }

    #[test]
    fn basic_and_post_credentials_are_each_accepted_alone() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;

        let headers = headers_with(&[(
            "authorization",
            &format!("basic {}", STANDARD.encode("a-client:a-secret")),
        )]);
        let auth = extract_client_auth(&headers, &parse_form(b"").unwrap()).expect("basic");
        assert_eq!(auth.client_id, "a-client");
        assert_eq!(auth.secret.as_deref(), Some("a-secret"));
        assert!(auth.via_basic, "a Basic failure must carry a challenge");

        let params = parse_form(b"client_id=a-client&client_secret=a-secret").unwrap();
        let auth = extract_client_auth(&HeaderMap::new(), &params).expect("post");
        assert_eq!(auth.client_id, "a-client");
        assert_eq!(auth.secret.as_deref(), Some("a-secret"));
        assert!(!auth.via_basic);

        // A public client presents no credential at all.
        let params = parse_form(b"client_id=a-client").unwrap();
        let auth = extract_client_auth(&HeaderMap::new(), &params).expect("public");
        assert_eq!(auth.secret, None);
    }

    /// The two halves of a Basic credential are form-encoded before base64, so
    /// a secret containing `:` must survive the round trip intact.
    #[test]
    fn basic_credentials_are_form_decoded() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let headers = headers_with(&[(
            "authorization",
            &format!("Basic {}", STANDARD.encode("a%2Dclient:with%3Acolon+and+space")),
        )]);
        let auth = extract_client_auth(&headers, &parse_form(b"").unwrap()).expect("basic");
        assert_eq!(auth.client_id, "a-client");
        assert_eq!(auth.secret.as_deref(), Some("with:colon and space"));
    }

    #[test]
    fn a_disagreeing_body_client_id_is_refused_but_an_agreeing_one_is_not() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let headers = headers_with(&[(
            "authorization",
            &format!("Basic {}", STANDARD.encode("a-client:a-secret")),
        )]);
        let disagreeing = parse_form(b"client_id=another-client").unwrap();
        assert_eq!(
            extract_client_auth(&headers, &disagreeing).expect_err("must refuse").code,
            OauthErrorCode::InvalidRequest
        );
        let agreeing = parse_form(b"client_id=a-client").unwrap();
        assert!(extract_client_auth(&headers, &agreeing).is_ok());
    }

    /// An unrecognised scheme must not fall through to "unauthenticated public
    /// client" — that would let a bogus header buy a weaker check.
    #[test]
    fn an_unsupported_authorization_scheme_is_refused() {
        let headers = headers_with(&[("authorization", "Bearer some-token")]);
        let params = parse_form(b"client_id=a-client").unwrap();
        let err = extract_client_auth(&headers, &params).expect_err("must refuse");
        assert_eq!(err.code, OauthErrorCode::InvalidClient);
        assert!(err.challenge);
    }

    #[test]
    fn a_request_naming_no_client_is_refused() {
        let err = extract_client_auth(&HeaderMap::new(), &parse_form(b"").unwrap())
            .expect_err("must refuse");
        assert_eq!(err.code, OauthErrorCode::InvalidRequest);
    }

    /// The PKCE transformation must match RFC 7636's own worked example, or
    /// every real client fails.
    #[test]
    fn s256_matches_the_rfc_7636_test_vector() {
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_accepts_the_preimage_and_nothing_else() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = s256_challenge(verifier);
        assert!(verify_pkce(verifier, &challenge));
        assert!(!verify_pkce("a-different-verifier-of-the-same-general-shape", &challenge));
        // A code somehow stored without a challenge must NOT be exchangeable:
        // "nothing recorded, so nothing to check" is the bypass.
        assert!(!verify_pkce(verifier, ""));
    }

    #[test]
    fn a_malformed_verifier_is_rejected_before_it_is_hashed() {
        assert!(verifier_is_well_formed(&"a".repeat(MIN_VERIFIER_LEN)));
        assert!(verifier_is_well_formed(&"a".repeat(MAX_VERIFIER_LEN)));
        assert!(!verifier_is_well_formed(&"a".repeat(MIN_VERIFIER_LEN - 1)));
        assert!(!verifier_is_well_formed(&"a".repeat(MAX_VERIFIER_LEN + 1)));
        // Characters outside RFC 7636's unreserved set.
        assert!(!verifier_is_well_formed(&format!("{}/", "a".repeat(MIN_VERIFIER_LEN))));
        assert!(!verifier_is_well_formed(&format!("{} ", "a".repeat(MIN_VERIFIER_LEN))));
    }

    /// Substring matching would make `offline_access_denied` grant a refresh
    /// token, so scope tokens are compared whole.
    #[test]
    fn scope_matching_is_by_whole_token() {
        assert!(scope_contains("mcp offline_access", OFFLINE_ACCESS));
        assert!(scope_contains(OFFLINE_ACCESS, OFFLINE_ACCESS));
        assert!(!scope_contains("mcp offline_access_denied", OFFLINE_ACCESS));
        assert!(!scope_contains("mcp", OFFLINE_ACCESS));
        assert!(!scope_contains("", OFFLINE_ACCESS));
    }

    /// A refresh may narrow but never widen (RFC 6749 §6).
    #[test]
    fn a_refresh_may_narrow_the_scope_but_never_widen_it() {
        assert!(scope_is_subset("mcp", "mcp offline_access"));
        assert!(scope_is_subset("mcp offline_access", "mcp offline_access"));
        assert!(scope_is_subset("", "mcp"));
        assert!(!scope_is_subset("mcp admin", "mcp offline_access"));
        assert!(!scope_is_subset("admin", ""));
    }

    /// The exact wire spellings. A custom code here strands a Claude connector
    /// permanently, because its re-authorization is keyed on `invalid_grant`.
    #[test]
    fn error_codes_are_the_registered_rfc_6749_spellings() {
        assert_eq!(OauthErrorCode::InvalidRequest.as_str(), "invalid_request");
        assert_eq!(OauthErrorCode::InvalidClient.as_str(), "invalid_client");
        assert_eq!(OauthErrorCode::InvalidGrant.as_str(), "invalid_grant");
        assert_eq!(OauthErrorCode::UnauthorizedClient.as_str(), "unauthorized_client");
        assert_eq!(OauthErrorCode::UnsupportedGrantType.as_str(), "unsupported_grant_type");
        assert_eq!(OauthErrorCode::InvalidScope.as_str(), "invalid_scope");
        assert_eq!(OauthErrorCode::ServerError.as_str(), "server_error");
    }

    /// Only a client-authentication failure is a 401; every other client-facing
    /// failure is a 400 (RFC 6749 §5.2).
    #[test]
    fn error_statuses_follow_the_rfc() {
        assert_eq!(OauthErrorCode::InvalidClient.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(OauthErrorCode::InvalidGrant.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OauthErrorCode::InvalidRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OauthErrorCode::InvalidScope.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OauthErrorCode::UnauthorizedClient.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OauthErrorCode::UnsupportedGrantType.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OauthErrorCode::ServerError.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// An internal fault must not describe itself to an unauthenticated caller,
    /// and must not carry a store error's text.
    #[test]
    fn an_internal_failure_is_an_opaque_server_error() {
        let err = OauthError::internal(
            "context",
            ToolError::Database("relation rmcp_auth_code does not exist".into()),
        );
        assert_eq!(err.code, OauthErrorCode::ServerError);
        assert!(!err.description.contains("rmcp_auth_code"));
    }

    /// A token response carries bearer credentials; nothing on the path may
    /// cache it, and the refresh token is omitted entirely without
    /// `offline_access` rather than sent as null.
    #[test]
    fn the_success_body_has_the_rfc_shape() {
        let with_refresh = TokenResponse {
            access_token: "a-jwt".into(),
            token_type: "Bearer",
            expires_in: 900,
            scope: "mcp offline_access".into(),
            refresh_token: Some("a-refresh".into()),
        };
        let json = serde_json::to_value(&with_refresh).expect("serializes");
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["expires_in"], 900);
        assert_eq!(json["refresh_token"], "a-refresh");

        let without = TokenResponse {
            access_token: "a-jwt".into(),
            token_type: "Bearer",
            expires_in: 900,
            scope: "mcp".into(),
            refresh_token: None,
        };
        let json = serde_json::to_value(&without).expect("serializes");
        assert!(json.get("refresh_token").is_none(), "absent, not null");

        let response = without.into_response();
        assert_eq!(response.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(response.headers().get(header::PRAGMA).unwrap(), "no-cache");
    }

    /// A Basic-authenticated failure must carry the challenge RFC 6749 §5.2
    /// requires; a body-authenticated one must not invite a scheme the caller
    /// did not use.
    #[test]
    fn a_basic_authentication_failure_carries_a_challenge() {
        let challenged = OauthError::invalid_client("no").with_challenge(true).into_response();
        assert_eq!(challenged.status(), StatusCode::UNAUTHORIZED);
        assert!(challenged.headers().get(header::WWW_AUTHENTICATE).is_some());

        let plain = OauthError::invalid_client("no").into_response();
        assert!(plain.headers().get(header::WWW_AUTHENTICATE).is_none());
        assert_eq!(plain.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    /// A wrong stored hash, an unparseable one, and a wrong secret must all
    /// deny — a PHC string that will not parse must never verify as a match.
    #[test]
    fn client_secret_verification_denies_on_anything_but_a_match() {
        // argon2id of "a-client-secret", generated by this crate's own hasher.
        let hasher = argon2::Argon2::default();
        let salt = argon2::password_hash::SaltString::from_b64("c29tZXNhbHRzYWx0")
            .expect("valid salt");
        let stored = {
            use argon2::password_hash::PasswordHasher;
            hasher
                .hash_password(b"a-client-secret", &salt)
                .expect("hash")
                .to_string()
        };
        assert!(verify_client_secret("a-client-secret", &stored));
        assert!(!verify_client_secret("the-wrong-secret", &stored));
        assert!(!verify_client_secret("a-client-secret", "not-a-phc-string"));
        assert!(!verify_client_secret("a-client-secret", ""));
    }

    /// The credential must not be reachable through `Debug`.
    #[test]
    fn presented_client_auth_debug_redacts_the_secret() {
        let auth = PresentedClientAuth {
            client_id: "a-client".into(),
            secret: Some("a-secret".into()),
            via_basic: false,
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("a-secret"));
        assert!(rendered.contains("a-client"), "the client_id is not secret and aids debugging");
    }

    /// The refresh lifetime is a real security parameter: an unbounded one is
    /// an unattended grant that never ends.
    // `#[tokio::test]` rather than `#[test]`: building a `PgPool` — even a lazy
    // one that opens no connection — starts the pool's maintenance task, which
    // needs a reactor. Nothing here awaits or issues a query.
    #[tokio::test]
    async fn a_nonsensical_refresh_lifetime_is_refused() {
        // Constructed without a store: `new` validates the TTL before anything
        // else, so this exercises the guard without a database.
        //
        // The key is a TEST DOUBLE, not a credential: it exists only in the
        // test binary, authenticates nothing, and is padded to clear the
        // 32-byte minimum. No token is minted or verified here at all — the
        // signer is required only to construct a `TokenEndpoint`. Taking it
        // from a runtime secret accessor would test the vault rather than the
        // lifetime guard under test.
        let signer = JwtSigner::new(
            // pii-test-fixture: invented test key
            "test-double-not-a-secret-aaaaaaaaaaaaaaaa".into(),
            None,
            "https://connector.test".into(),
            900,
            30,
        )
        .expect("valid signer");
        assert!(TokenEndpoint::new(store_stub(), signer.clone(), 0).is_err());
        assert!(TokenEndpoint::new(store_stub(), signer.clone(), -1).is_err());
        assert!(
            TokenEndpoint::new(store_stub(), signer.clone(), MAX_REFRESH_TTL_SECONDS + 1).is_err()
        );
        assert!(TokenEndpoint::new(store_stub(), signer, DEFAULT_REFRESH_TTL_SECONDS).is_ok());
    }

    /// A store over a lazily-connected pool. `connect_lazy_with` performs no
    /// I/O and opens no file, so this is a valid `OauthStore` value for tests
    /// that never issue a query — which is every test in this module. The
    /// DB-backed behaviours (atomic single-use, rotation, family revocation)
    /// are guarantees of the store's SQL and are covered where they live, in
    /// RMCP-01 — which, since S132/RMCP-SQLITE, covers them against a REAL
    /// database file rather than by scanning the SQL's text.
    ///
    /// The fixture used to be an invented Postgres DSN, and it needed two
    /// paragraphs of justification: every part obviously fake, and the host
    /// deliberately DOTLESS, because the repo's own `no_pii_in_own_source_tree`
    /// scanner reads a user part followed by a dotted host as an email address.
    /// That shape is deliberately described rather than quoted here — writing
    /// it out trips the scanner even inside a comment. An in-memory SQLite
    /// handle names no user, no host and no credential, so none of the
    /// reasoning is needed any more.
    fn store_stub() -> OauthStore {
        OauthStore::from_pool(
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect_lazy_with(sqlx::sqlite::SqliteConnectOptions::new().in_memory(true)),
        )
    }
}
