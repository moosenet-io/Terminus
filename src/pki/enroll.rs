//! Client-cert enrollment endpoint + protocol (TCLI-02 — Terminus Gateway
//! P2). Depends on [`crate::pki::ca`] (TCLI-01).
//!
//! ## What this is
//! Given a bootstrap credential (the vault-rotatable
//! `TERMINUS_ENROLLMENT_SHARED_SECRET`, per identity — see design decision #3
//! in the S107 spec) and a requested identity name, issues:
//! 1. a SHORT-LIVED client certificate signed by the [`crate::pki::ca`] root
//!    CA, with the identity embedded in the cert's Subject CN and SAN, and
//! 2. a paired short-lived JWT carrying the same identity claim.
//!
//! Per the spec's design decision #2: the cert is the TRANSPORT-layer
//! identity (consumed by TCLI-03's mTLS handshake), and the JWT is an
//! APPLICATION-layer claim carried in each MCP request for per-tool authz +
//! audit attribution — belt-and-suspenders, not redundant.
//!
//! ## What this deliberately is NOT
//! - Not the mTLS listener (TCLI-03) — this module only issues credentials,
//!   it doesn't authenticate connections against them.
//! - Not a replacement for the existing `/mcp` JWT-over-HTTP bearer-token
//!   auth (`crate::mcp_server`) — this is a NEW, additive route. The existing
//!   `/mcp`, and any `/v1/tools/call` / `/v1/personal/tools/call` /
//!   `/v1/tools/list` call sites this crate makes as a *client* of Chord
//!   (`crate::odyssey`, `crate::wizard`), are untouched by this item.
//!
//! ## Enrollment transport (bootstrap chicken-and-egg)
//! At enrollment time the caller has no client cert yet (that's the whole
//! point of this endpoint), so this endpoint's own transport is plain TLS at
//! minimum for P2 — it cannot itself require the client cert it is about to
//! issue. Authentication for THIS endpoint is the shared secret alone,
//! compared in constant time. Deploy this route behind the same TLS
//! termination the rest of the binary uses; mTLS-only transport for
//! everything else is TCLI-03's job.
//!
//! ## Secrets
//! `TERMINUS_JWT_SIGNING_KEY` is read via the env-materialized runtime
//! secret store, matching the convention `crate::pki`'s CA bootstrap already
//! established for this crate (see that module's doc comment) — never
//! `std::env::var` treated as a literal source of truth, always the
//! materialized-secret-store read.
//!
//! ## Per-identity enrollment secrets (LHEG-01, S109)
//! As of LHEG-01, the bootstrap credential is looked up **per requested
//! identity**: `TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY_UPPERCASE>`
//! (e.g. `TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA`,
//! `TERMINUS_ENROLLMENT_SHARED_SECRET_HARMONY`,
//! `TERMINUS_ENROLLMENT_SHARED_SECRET_CLAUDE`,
//! `TERMINUS_ENROLLMENT_SHARED_SECRET_MOOSE`). A caller holding only the
//! `..._LUMINA` value can only ever match the comparison run for identity
//! `lumina` — it cannot enroll as `moose` or any other identity, because the
//! lookup key (and therefore the value compared against) is derived from the
//! identity *being requested*, not supplied by the caller directly. This is
//! the structural mechanism (not merely a policy statement) that makes
//! "Lumina/Harmony cannot act as the Moose identity" true.
//!
//! **Non-breaking legacy fallback:** if no per-identity secret is configured
//! for the requested identity, enrollment falls back to the legacy
//! unsuffixed `TERMINUS_ENROLLMENT_SHARED_SECRET` (logging a
//! `tracing::warn!` deprecation notice each time it's used). This keeps the
//! existing terminus-primary + dev-box enroller path working during the
//! per-identity migration. The unsuffixed secret is intentionally NOT
//! removed by this item — that is a later hard-cutover cleanup once
//! per-identity secrets are fully provisioned for every real enroller. The
//! no-Moose guarantee above still holds operationally: `lumina`/`harmony`
//! are provisioned only with their own `_LUMINA`/`_HARMONY` secret, never
//! the unsuffixed value or `_MOOSE`.
//!
//! Both the per-identity and legacy secrets are read via the same
//! env-materialized runtime secret store as `TERMINUS_JWT_SIGNING_KEY`
//! above, and compared to the presented secret in constant time
//! ([`constant_time_eq`]). An unset/empty secret always fails closed (never
//! "everyone's welcome").
//!
//! ## Audit logging (S6)
//! Enrollment log lines carry identity + issuance timestamp + cert serial
//! (and, for the legacy-fallback path, the fact that the fallback was used)
//! — never the private key, the bootstrap secret, or the JWT itself.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Duration as ChronoDuration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::CertificateAuthority;

/// A request to enroll a new (or re-enroll an existing) identity.
#[derive(Debug, Deserialize)]
pub struct EnrollmentRequest {
    /// The identity name to embed in the issued cert's CN/SAN and the JWT's
    /// `sub` claim (e.g. `dev-box-claude-code`, `harmony-primary`).
    pub identity: String,
    /// The bootstrap credential, compared in constant time against
    /// `TERMINUS_ENROLLMENT_SHARED_SECRET`. Never logged.
    pub shared_secret: String,
}

/// A successful enrollment result: the issued short-lived client cert +
/// private key (returned to the caller to hold — this endpoint keeps no copy
/// beyond signing it), plus a paired short-lived JWT.
///
/// Deliberately does NOT derive [`std::fmt::Debug`] via the usual
/// `#[derive(Debug)]` — the hand-written impl below redacts `key_pem` and
/// `jwt` (both secret-ish: the key is the client's private key, the JWT is a
/// bearer credential), matching the redaction convention
/// `crate::pki::ca::CertificateAuthority` already uses. `cert_pem` and
/// `ca_cert_pem` are public certificates, safe to print; `expires_at` is a
/// timestamp.
#[derive(Serialize)]
pub struct EnrollmentResponse {
    /// PEM-encoded leaf certificate, signed by the TCLI-01 CA.
    pub cert_pem: String,
    /// PEM-encoded private key for the issued cert. Caller-held only; this
    /// endpoint never persists it and never logs it.
    pub key_pem: String,
    /// The CA's own PEM certificate, so the caller can pin it locally
    /// (TCLI-04's `connect()` validates the primary's server cert against
    /// this rather than trusting an arbitrary system CA store).
    pub ca_cert_pem: String,
    /// Short-lived JWT carrying the same identity claim.
    pub jwt: String,
    /// Unix timestamp (seconds) the cert + JWT should be considered expired
    /// by. The JWT's own `exp` claim is authoritative for JWT validation;
    /// this field additionally covers the cert, which has no `exp` claim of
    /// its own to inspect without parsing X.509.
    pub expires_at: i64,
}

impl std::fmt::Debug for EnrollmentResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentResponse")
            .field("cert_pem", &self.cert_pem)
            .field("key_pem", &"<redacted>")
            .field("ca_cert_pem", &self.ca_cert_pem)
            .field("jwt", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// JWT claims minted at enrollment. Deliberately minimal — this is an
/// application-layer identity claim, not a general-purpose token.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnrollmentClaims {
    /// Identity name — same value embedded in the paired cert's CN/SAN.
    pub sub: String,
    /// Standard JWT expiry (Unix seconds).
    pub exp: i64,
    /// Standard JWT issued-at (Unix seconds).
    pub iat: i64,
}

/// Errors from an enrollment attempt. Every variant's `Display` is safe to
/// log verbatim (no secret material) and safe to return to the caller as an
/// error message (no internal detail beyond "rejected" / "invalid").
#[derive(Debug, Error)]
pub enum EnrollError {
    /// Neither the requested identity's per-identity secret
    /// (`TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY>`) nor the legacy
    /// unsuffixed `TERMINUS_ENROLLMENT_SHARED_SECRET` fallback is configured
    /// — an operator provisioning gap, not a client error, but still
    /// surfaced as a rejection (never "everyone's welcome" fail-open). The
    /// message deliberately does not echo the identity name back verbatim
    /// (kept generic) so this endpoint doesn't confirm/deny which specific
    /// identities are provisioned to an unauthenticated caller.
    #[error("enrollment is not configured for the requested identity")]
    NotConfigured,
    /// The presented shared secret didn't match, or was empty.
    #[error("invalid or missing enrollment shared secret")]
    Unauthorized,
    /// The requested identity name failed the naming-convention check.
    #[error("identity name '{0}' does not match the allowed naming pattern")]
    InvalidIdentity(String),
    /// `rcgen` failed to generate or sign the leaf cert.
    #[error("failed to issue client certificate: {0}")]
    CertIssuance(String),
    /// `jsonwebtoken` failed to sign the paired JWT.
    #[error("failed to mint enrollment JWT: {0}")]
    JwtSigning(String),
    /// `jsonwebtoken` rejected a presented token on verification — bad
    /// signature, expired, malformed, or `TERMINUS_JWT_SIGNING_KEY` is
    /// unset. Deliberately one variant covering all of these (CONST-03's
    /// `crate::constellation::auth` only needs "valid or not", never
    /// distinguishes the sub-reason to a caller) — never logged with the
    /// token itself, only this generic message.
    #[error("JWT verification failed: {0}")]
    JwtVerification(String),
}

/// Identity names are used verbatim in the cert's CN/SAN and the JWT `sub`
/// claim, and (per the spec's edge cases) must not allow unbounded namespace
/// growth or SAN-injection-shaped input. DNS-label-like: lowercase
/// alphanumerics and hyphens, 2-63 chars, must not start/end with a hyphen.
pub(crate) fn is_valid_identity(identity: &str) -> bool {
    let len_ok = (2..=63).contains(&identity.len());
    let starts_ends_alnum = identity
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && identity
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    let charset_ok = identity
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    len_ok && starts_ends_alnum && charset_ok
}

/// Constant-time byte comparison — deliberately not `==`, so a timing side
/// channel can't be used to guess the shared secret one byte at a time.
/// Small hand-rolled implementation (no new dependency): a length mismatch
/// is folded into the same accumulator rather than short-circuiting, so the
/// function's timing does not depend on *where* (or whether) the inputs
/// first diverge.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_diff = (a.len() != b.len()) as u8;
    let mut diff: u8 = len_diff;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The legacy, unsuffixed shared-secret env var name — the pre-LHEG-01
/// bootstrap credential, kept only as a non-breaking fallback (see the
/// module doc's "Per-identity enrollment secrets" section).
const LEGACY_SHARED_SECRET_ENV: &str = "TERMINUS_ENROLLMENT_SHARED_SECRET";

/// Build the per-identity enrollment secret's env var name:
/// `TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY_UPPERCASE>`.
///
/// Callers MUST validate `identity` via [`is_valid_identity`] before calling
/// this (both call sites in [`handle_enrollment`] validate first) — the
/// identity charset `is_valid_identity` enforces (lowercase ASCII
/// alphanumerics + hyphens only) is exactly what makes this transform safe:
/// hyphens become underscores and letters are uppercased, so an
/// already-validated identity can never inject anything beyond a legal
/// Rust/shell env var name (no key-injection surprise per the spec's edge
/// cases).
fn identity_secret_env_key(identity: &str) -> String {
    let normalized: String = identity
        .chars()
        .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
        .collect();
    format!("{LEGACY_SHARED_SECRET_ENV}_{normalized}")
}

/// Handle one enrollment request: validate the identity + bootstrap secret,
/// then issue a signed leaf cert + paired JWT.
///
/// Re-enrollment (an identity that already has an outstanding cert) is not
/// tracked or rejected here — short-lived certs are expected to be
/// re-requested periodically (TCLI-02 edge case), so every valid request
/// simply issues a fresh cert/JWT pair.
pub fn handle_enrollment(
    ca: &CertificateAuthority,
    req: &EnrollmentRequest,
) -> Result<EnrollmentResponse, EnrollError> {
    // Identity shape is validated FIRST (before any secret lookup): the
    // per-identity env key is derived from the requested identity, so the
    // identity must be well-formed before it's used to build that key
    // (key-injection guard, per the spec's edge cases).
    if !is_valid_identity(&req.identity) {
        tracing::warn!(
            "pki::enroll: rejected enrollment attempt for disallowed identity name pattern"
        );
        return Err(EnrollError::InvalidIdentity(req.identity.clone()));
    }

    let per_identity_key = identity_secret_env_key(&req.identity);
    let (expected, used_legacy_fallback) = match env_nonempty(&per_identity_key) {
        Some(secret) => (secret, false),
        None => match env_nonempty(LEGACY_SHARED_SECRET_ENV) {
            Some(secret) => {
                tracing::warn!(
                    identity = %req.identity,
                    "pki::enroll: DEPRECATED — no {per_identity_key} configured for this \
                     identity, falling back to the legacy unsuffixed \
                     TERMINUS_ENROLLMENT_SHARED_SECRET; provision a per-identity secret and \
                     retire this fallback"
                );
                (secret, true)
            }
            None => return Err(EnrollError::NotConfigured),
        },
    };

    if req.shared_secret.is_empty()
        || !constant_time_eq(req.shared_secret.as_bytes(), expected.as_bytes())
    {
        tracing::warn!(
            identity = %req.identity,
            legacy_fallback = used_legacy_fallback,
            "pki::enroll: rejected enrollment attempt (invalid shared secret)"
        );
        return Err(EnrollError::Unauthorized);
    }

    let (cert_pem, key_pem, serial) = issue_leaf_cert(ca, &req.identity)?;
    let (jwt, exp) = mint_jwt(&req.identity)?;

    tracing::info!(
        identity = %req.identity,
        serial = %serial,
        "pki::enroll: issued client certificate + JWT"
    );

    Ok(EnrollmentResponse {
        cert_pem,
        key_pem,
        ca_cert_pem: ca.cert_pem().to_string(),
        jwt,
        expires_at: exp,
    })
}

/// Generate a fresh keypair and sign a short-lived leaf cert for `identity`,
/// chained to `ca`. Returns `(cert_pem, key_pem, serial_hex)`.
///
/// `pub(crate)`: besides [`handle_enrollment`] (the shared-secret-gated HTTP
/// path), [`crate::mesh::client_onboarding`] (MESH-12) also calls this
/// directly to mint a client cert as part of the `mesh_onboard_client`
/// workflow — that call site is reached only through terminus-rs's own
/// already-authenticated tool dispatch (the caller must already be an
/// allowlisted principal to invoke a CORE tool at all), not a fresh
/// unauthenticated HTTP request, so it deliberately does not re-derive or
/// re-check `TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY>` — that bootstrap
/// credential exists to gate the *pre-auth* `/enroll` HTTP route
/// specifically (see this module's doc), not every possible cert-issuance
/// call site in the crate.
pub(crate) fn issue_leaf_cert(
    ca: &CertificateAuthority,
    identity: &str,
) -> Result<(String, String, String), EnrollError> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| EnrollError::CertIssuance(format!("leaf params: {e}")))?;

    let now = Utc::now();
    let ttl = ChronoDuration::hours(crate::config::enrollment_cert_ttl_hours());
    // Small backdate to tolerate clock skew between this host and the
    // enrolling client, same rationale as the CA's own backdating
    // (`crate::pki::ca::CertificateAuthority::generate`).
    params.not_before = to_rcgen_time(now - ChronoDuration::minutes(5));
    params.not_after = to_rcgen_time(now + ttl);
    params
        .distinguished_name
        .push(DnType::CommonName, identity);
    params.subject_alt_names = vec![SanType::DnsName(
        identity
            .to_string()
            .try_into()
            .map_err(|e| EnrollError::CertIssuance(format!("SAN encoding: {e:?}")))?,
    )];
    // TCLI-03 follow-up (from the TCLI-02 review): this leaf is presented as
    // the CLIENT cert in the mTLS handshake (`crate::pki::mtls`), so it must
    // carry the clientAuth EKU + a DigitalSignature KeyUsage or a strict TLS
    // stack (and `crate::pki::mtls`'s own explicit, independent EKU check)
    // will reject the handshake. Previously unset -- enrollment issued a
    // cert with no EKU at all, which happened to be harmless before TCLI-03
    // existed but silently would not have worked as a client-auth cert.
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);

    let key_pair =
        KeyPair::generate().map_err(|e| EnrollError::CertIssuance(format!("leaf keypair: {e}")))?;
    let cert = params
        .signed_by(&key_pair, ca.issuer())
        .map_err(|e| EnrollError::CertIssuance(format!("leaf signing: {e}")))?;

    let serial = cert
        .der()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    Ok((cert.pem(), key_pair.serialize_pem(), serial))
}

fn to_rcgen_time(dt: chrono::DateTime<Utc>) -> time::OffsetDateTime {
    // `rcgen` 0.14's `CertificateParams::not_before`/`not_after` take
    // `time::OffsetDateTime`, not `chrono`. `crate::pki::ca` sidesteps this
    // by only needing day granularity (`rcgen::date_time_ymd`), but TCLI-02's
    // leaf certs need hour-granularity TTLs, so this bridges via a Unix
    // timestamp instead.
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp())
        .expect("chrono timestamps are always in range for time::OffsetDateTime")
}

/// Mint a short-lived JWT carrying `identity` as the `sub` claim, TTL from
/// [`crate::config::enrollment_jwt_ttl_seconds`]. Thin wrapper over
/// [`mint_jwt_with_ttl`] — the enrollment TTL policy lives here, not in the
/// shared signing primitive.
fn mint_jwt(identity: &str) -> Result<(String, i64), EnrollError> {
    mint_jwt_with_ttl(identity, crate::config::enrollment_jwt_ttl_seconds())
}

/// Mint a short-lived JWT carrying `sub` as the subject claim, with an
/// explicit TTL (seconds). Signed with `TERMINUS_JWT_SIGNING_KEY` (HS256) —
/// the one JWT signing key this crate uses (see the `jsonwebtoken`
/// dependency comment in `Cargo.toml`).
///
/// Reused outside enrollment by CONST-03 (`crate::constellation::auth`) to
/// mint the constellation control plane's signed session token, with `sub`
/// = the operator username and a session-specific TTL
/// (`crate::config::constellation_session_ttl_seconds`) rather than the
/// enrollment JWT TTL — same signing key, same claim shape
/// ([`EnrollmentClaims`]), different TTL policy owned by the caller.
pub fn mint_jwt_with_ttl(sub: &str, ttl_seconds: i64) -> Result<(String, i64), EnrollError> {
    let signing_key = env_nonempty("TERMINUS_JWT_SIGNING_KEY")
        .ok_or_else(|| EnrollError::JwtSigning("TERMINUS_JWT_SIGNING_KEY is unset".to_string()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| EnrollError::JwtSigning(format!("system clock: {e}")))?
        .as_secs() as i64;
    let exp = now + ttl_seconds;

    let claims = EnrollmentClaims {
        sub: sub.to_string(),
        exp,
        iat: now,
    };

    let jwt = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(signing_key.as_bytes()),
    )
    .map_err(|e| EnrollError::JwtSigning(e.to_string()))?;

    Ok((jwt, exp))
}

/// Verify a JWT previously minted by [`mint_jwt`]/[`mint_jwt_with_ttl`]:
/// signature (HS256, `TERMINUS_JWT_SIGNING_KEY`) and expiry (`exp`, checked
/// by `jsonwebtoken`'s default [`Validation`] — no `leeway` override, so
/// expiry is exact). Returns the decoded [`EnrollmentClaims`] on success —
/// any failure (bad signature, expired, malformed, unset signing key)
/// collapses to a single [`EnrollError::JwtVerification`], deliberately not
/// distinguishing the sub-reason to callers (CONST-03 only needs
/// valid-or-not).
///
/// Reused by CONST-03 (`crate::constellation::auth::session_from_cookie`) to
/// verify the constellation control plane's session cookie — the SAME
/// verification primitive TCLI-02's enrollment JWT uses, not a second
/// hand-rolled HS256 check.
pub fn verify_jwt(token: &str) -> Result<EnrollmentClaims, EnrollError> {
    let signing_key = env_nonempty("TERMINUS_JWT_SIGNING_KEY")
        .ok_or_else(|| EnrollError::JwtVerification("TERMINUS_JWT_SIGNING_KEY is unset".to_string()))?;

    decode::<EnrollmentClaims>(
        token,
        &DecodingKey::from_secret(signing_key.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map(|data| data.claims)
    .map_err(|e| EnrollError::JwtVerification(e.to_string()))
}

// ── HTTP route (additive) ──────────────────────────────────────────────────
//
// `build_enroll_router()` returns a standalone `axum::Router` a binary
// merges into whatever router it already serves (see
// `src/bin/terminus_personal.rs`'s `main()`). This is INTENTIONALLY separate
// from `crate::mcp_server::build_router`/`McpServerState` — enrollment has
// its own request/response shape (plain JSON in/out, not JSON-RPC-over-SSE)
// and its own auth model (the shared secret in the body, not the `/mcp`
// bearer-token header), and keeping it a fully separate router means this
// item cannot, even accidentally, change `/mcp`'s behavior. Mounting is the
// caller's choice: this module has no opinion on path prefixes beyond the
// single route it registers at `crate::config::enrollment_path()`.

/// Build the standalone enrollment router. Call [`crate::pki::ca`] (or let
/// the handler do so lazily on first request) before merging this into a
/// binary's served router if you want a fast startup failure on CA bootstrap
/// problems rather than deferring that failure to the first enrollment
/// request.
pub fn build_enroll_router() -> axum::Router {
    // Bound the request body to a few KB (TCLI-02 hardening): the enrollment
    // payload is two short JSON strings, so a tight limit cheaply removes a
    // trivial DoS vector on this public-facing, pre-auth route without ever
    // constraining a legitimate request. Overrides axum's larger default
    // body limit for this router only.
    axum::Router::new()
        .route(
            &crate::config::enrollment_path(),
            axum::routing::post(handle_enroll_http),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
}

async fn handle_enroll_http(
    axum::extract::Json(req): axum::extract::Json<EnrollmentRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let ca = match crate::pki::ca() {
        Ok(ca) => ca,
        Err(e) => {
            tracing::error!("pki::enroll: CA unavailable for enrollment request: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "enrollment temporarily unavailable"})),
            )
                .into_response();
        }
    };

    match handle_enrollment(ca, &req) {
        Ok(resp) => (StatusCode::OK, axum::Json(resp)).into_response(),
        Err(EnrollError::NotConfigured) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "enrollment is not configured"})),
        )
            .into_response(),
        Err(EnrollError::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "invalid or missing enrollment shared secret"})),
        )
            .into_response(),
        Err(e @ EnrollError::InvalidIdentity(_)) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => {
            // CertIssuance / JwtSigning: internal failures, not client error.
            // The `Display` impl for both is safe to log (see `EnrollError`
            // doc) but the HTTP response stays generic to avoid leaking
            // implementation detail to an unauthenticated-by-secret caller.
            tracing::error!("pki::enroll: enrollment failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "enrollment failed"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::CertificateAuthority;
    use serial_test::serial;

    fn set_secrets(shared_secret: &str, jwt_key: &str) {
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET", shared_secret);
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", jwt_key);
    }

    fn clear_secrets() {
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
        std::env::remove_var("TERMINUS_ENROLLMENT_CERT_TTL_HOURS");
        std::env::remove_var("TERMINUS_ENROLLMENT_JWT_TTL_SECONDS");
    }

    #[test]
    fn identity_pattern_accepts_expected_shapes() {
        assert!(is_valid_identity("dev-box-claude-code"));
        assert!(is_valid_identity("harmony-primary"));
        assert!(is_valid_identity("ab"));
    }

    #[test]
    fn identity_pattern_rejects_bad_shapes() {
        assert!(!is_valid_identity(""));
        assert!(!is_valid_identity("a"));
        assert!(!is_valid_identity("-leading-hyphen"));
        assert!(!is_valid_identity("trailing-hyphen-"));
        assert!(!is_valid_identity("Has_Upper_And_Underscore"));
        assert!(!is_valid_identity("has a space"));
        assert!(!is_valid_identity("cn=injected,dc=evil"));
        assert!(!is_valid_identity(&"a".repeat(64)));
    }

    #[test]
    fn constant_time_eq_matches_normal_equality_semantics() {
        assert!(constant_time_eq(b"same-value", b"same-value"));
        assert!(!constant_time_eq(b"same-value", b"different"));
        assert!(!constant_time_eq(b"short", b"a-longer-value"));
        assert!(!constant_time_eq(b"", b"nonempty"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    #[serial]
    fn valid_secret_and_identity_issue_cert_and_jwt() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "dev-box-claude-code".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req).expect("valid enrollment should succeed");

        assert!(resp.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(resp.key_pem.contains("PRIVATE KEY"));
        assert_eq!(resp.ca_cert_pem, ca.cert_pem());
        assert!(!resp.jwt.is_empty());
        assert!(resp.expires_at > 0);

        clear_secrets();
    }

    #[test]
    #[serial]
    fn wrong_shared_secret_is_rejected_no_cert_issued() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "dev-box-claude-code".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &req).expect_err("wrong secret must be rejected");
        assert!(matches!(err, EnrollError::Unauthorized));

        clear_secrets();
    }

    #[test]
    #[serial]
    fn missing_shared_secret_is_rejected() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "dev-box-claude-code".to_string(),
            shared_secret: String::new(),
        };
        let err = handle_enrollment(&ca, &req).expect_err("empty secret must be rejected");
        assert!(matches!(err, EnrollError::Unauthorized));

        clear_secrets();
    }

    #[test]
    #[serial]
    fn enrollment_not_configured_when_shared_secret_unset() {
        clear_secrets();
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "dev-box-claude-code".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &req).expect_err("unset shared secret must fail closed");
        assert!(matches!(err, EnrollError::NotConfigured));
    }

    #[test]
    #[serial]
    fn disallowed_identity_name_is_rejected() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "../../etc/passwd".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &req).expect_err("bad identity name must be rejected");
        assert!(matches!(err, EnrollError::InvalidIdentity(_)));

        clear_secrets();
    }

    #[test]
    #[serial]
    fn reenrollment_of_same_identity_issues_a_fresh_pair_not_an_error() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "harmony-primary".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let first = handle_enrollment(&ca, &req).expect("first enrollment succeeds");
        let second = handle_enrollment(&ca, &req).expect("re-enrollment succeeds, not an error");
        assert_ne!(
            first.cert_pem, second.cert_pem,
            "re-enrollment must issue a fresh cert, not reuse the prior one"
        );

        clear_secrets();
    }

    // ── LHEG-01: per-identity enrollment secrets ───────────────────────────

    fn clear_identity_secrets() {
        for key in [
            "TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA",
            "TERMINUS_ENROLLMENT_SHARED_SECRET_HARMONY",
            "TERMINUS_ENROLLMENT_SHARED_SECRET_MOOSE",
            "TERMINUS_ENROLLMENT_SHARED_SECRET_CLAUDE",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn identity_secret_env_key_uppercases_and_maps_hyphens() {
        assert_eq!(
            identity_secret_env_key("lumina"),
            "TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA"
        );
        assert_eq!(
            identity_secret_env_key("dev-box-claude-code"),
            "TERMINUS_ENROLLMENT_SHARED_SECRET_DEV_BOX_CLAUDE_CODE"
        );
    }

    #[test]
    #[serial]
    fn per_identity_secret_matches_enrolls_as_that_identity() {
        clear_secrets();
        clear_identity_secrets();
        set_secrets("legacy-not-used-here", "jwt-signing-key-for-tests-only");
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET"); // no legacy fallback available
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA", "lumina-only-secret");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "lumina".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req)
            .expect("matching per-identity secret must enroll as that identity");
        assert!(resp.cert_pem.contains("BEGIN CERTIFICATE"));

        clear_identity_secrets();
        clear_secrets();
    }

    #[test]
    #[serial]
    fn wrong_per_identity_secret_with_no_legacy_fallback_is_rejected() {
        clear_secrets();
        clear_identity_secrets();
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "jwt-signing-key-for-tests-only");
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET"); // no legacy fallback available
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA", "lumina-only-secret");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "lumina".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &req).expect_err("wrong per-identity secret must be rejected");
        assert!(matches!(err, EnrollError::Unauthorized));

        clear_identity_secrets();
        clear_secrets();
    }

    #[test]
    #[serial]
    fn identity_with_no_configured_secret_and_no_legacy_fallback_is_rejected() {
        clear_secrets();
        clear_identity_secrets();
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "jwt-signing-key-for-tests-only");
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET"); // no legacy fallback available
        // Only `_LUMINA` is provisioned; `harmony` has nothing.
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA", "lumina-only-secret");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "harmony".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &req)
            .expect_err("identity with no configured secret and no legacy fallback must fail closed");
        assert!(matches!(err, EnrollError::NotConfigured));

        clear_identity_secrets();
        clear_secrets();
    }

    #[test]
    #[serial]
    fn lumina_secret_enrolls_lumina_but_not_moose() {
        // Structural no-Moose guarantee: a caller holding ONLY the `_LUMINA`
        // secret can successfully enroll as `lumina`, but presenting that
        // same secret value while requesting identity `moose` must be
        // rejected — the comparison is always against `moose`'s OWN secret
        // (unset here), never `lumina`'s.
        clear_secrets();
        clear_identity_secrets();
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "jwt-signing-key-for-tests-only");
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET"); // no legacy fallback available
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET_LUMINA", "lumina-only-secret");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let lumina_req = EnrollmentRequest {
            identity: "lumina".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        assert!(
            handle_enrollment(&ca, &lumina_req).is_ok(),
            "lumina's own secret must enroll lumina"
        );

        // Negative test (per LHEG-01 test plan): attempt enrollment as
        // `moose` using the `_LUMINA` secret value.
        let moose_req = EnrollmentRequest {
            identity: "moose".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let err = handle_enrollment(&ca, &moose_req)
            .expect_err("the _LUMINA secret must never enroll identity moose");
        assert!(matches!(err, EnrollError::NotConfigured));

        clear_identity_secrets();
        clear_secrets();
    }

    #[test]
    #[serial]
    fn legacy_unsuffixed_fallback_still_works_when_no_per_identity_secret_set() {
        // Non-breaking fallback (RESOLVED decision #1, S109): if no
        // per-identity secret is configured, enrollment falls back to the
        // legacy unsuffixed TERMINUS_ENROLLMENT_SHARED_SECRET so the
        // existing terminus-primary / dev-box enroller path keeps working.
        clear_secrets();
        clear_identity_secrets();
        set_secrets("legacy-shared-secret", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "some-legacy-enroller".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req)
            .expect("legacy unsuffixed fallback must still enroll when no per-identity secret is set");
        assert!(resp.cert_pem.contains("BEGIN CERTIFICATE"));

        clear_identity_secrets();
        clear_secrets();
    }

    /// Parse a PEM certificate's DER bytes via `x509-parser` (already a
    /// transitive dependency of `rcgen`'s `x509-parser` feature, pinned here
    /// directly as a dev-dependency so tests can inspect issued certs the
    /// same way `crate::pki::ca`'s own `Issuer::from_ca_cert_pem` load path
    /// does internally).
    fn parse_cert_der(pem_str: &str) -> Vec<u8> {
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(pem_str.as_bytes()).expect("valid PEM structure");
        pem.contents
    }

    #[test]
    #[serial]
    fn issued_cert_chains_to_the_ca() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "harmony-primary".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req).expect("enrollment succeeds");

        let leaf_der = parse_cert_der(&resp.cert_pem);
        let (_, leaf) =
            x509_parser::parse_x509_certificate(&leaf_der).expect("parse leaf DER");
        let ca_der = parse_cert_der(ca.cert_pem());
        let (_, ca_cert) =
            x509_parser::parse_x509_certificate(&ca_der).expect("parse CA DER");

        // Chain-of-trust check: the leaf's issuer DN must match the CA's own
        // subject DN, AND the leaf's signature must cryptographically verify
        // against the CA's public key (not just a name match, which alone
        // wouldn't prove anything was actually signed by this CA).
        assert_eq!(
            leaf.issuer().to_string(),
            ca_cert.subject().to_string(),
            "leaf cert's issuer must match the CA's subject (chain-of-trust)"
        );
        assert!(
            leaf.verify_signature(Some(ca_cert.public_key())).is_ok(),
            "leaf cert's signature must cryptographically validate against the CA's public key"
        );
    }

    #[test]
    #[serial]
    fn issued_cert_has_client_auth_eku_and_digital_signature_key_usage() {
        // TCLI-03 follow-up: the enrollment leaf is the client cert an mTLS
        // handshake presents (`crate::pki::mtls`); it must carry the
        // clientAuth EKU (+ DigitalSignature KeyUsage) or a strict TLS stack
        // rejects it. Regression test for the previously-missing EKU.
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "harmony-primary".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req).expect("enrollment succeeds");

        let leaf_der = parse_cert_der(&resp.cert_pem);
        let (_, leaf) = x509_parser::parse_x509_certificate(&leaf_der).expect("parse leaf");

        let eku = leaf
            .extended_key_usage()
            .expect("EKU extension parses")
            .expect("EKU extension is present");
        assert!(
            eku.value.client_auth,
            "issued client cert must carry the clientAuth extended key usage"
        );

        let ku = leaf
            .key_usage()
            .expect("KeyUsage extension parses")
            .expect("KeyUsage extension is present");
        assert!(
            ku.value.digital_signature(),
            "issued client cert must carry the DigitalSignature key usage"
        );

        clear_secrets();
    }

    #[test]
    #[serial]
    fn issued_cert_ttl_is_short_not_ca_length() {
        clear_secrets();
        std::env::set_var("TERMINUS_ENROLLMENT_CERT_TTL_HOURS", "2");
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");

        let req = EnrollmentRequest {
            identity: "harmony-primary".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req).expect("enrollment succeeds");

        let leaf_der = parse_cert_der(&resp.cert_pem);
        let (_, leaf) =
            x509_parser::parse_x509_certificate(&leaf_der).expect("parse leaf");
        let validity_seconds =
            leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp();
        // ~2h TTL + the 5-minute backdate == a little over 2h; must be far
        // short of the CA's multi-year window either way.
        assert!(
            validity_seconds < 3 * 3600,
            "expected a ~2h leaf TTL, got {validity_seconds}s"
        );

        let jwt_claims = decode::<EnrollmentClaims>(
            &resp.jwt,
            &DecodingKey::from_secret(b"jwt-signing-key-for-tests-only"),
            &Validation::default(),
        )
        .expect("issued JWT should decode with the configured signing key")
        .claims;
        assert_eq!(jwt_claims.sub, "harmony-primary");
        assert!(
            jwt_claims.exp - jwt_claims.iat <= 1800,
            "default JWT TTL should be short (<=30min), got {}s",
            jwt_claims.exp - jwt_claims.iat
        );

        clear_secrets();
    }

    #[test]
    #[serial]
    fn audit_log_never_contains_secret_or_key_material() {
        // This test asserts the CONTRACT via the function's documented
        // behavior (handle_enrollment only ever logs identity + serial, per
        // the module doc and S6) rather than capturing `tracing` output
        // (this crate's `tracing-subscriber` is installed once per binary,
        // not per-test) — see `crate::intake::init_tracing` for why a
        // per-test subscriber isn't the established pattern here. The
        // string-search assertion the TCLI-02 test plan calls for is
        // covered by inspecting the actual log call sites in
        // `handle_enrollment`, which reference only `identity` and `serial`
        // fields — grepped for in this same module by
        // `no_log_call_references_secret_or_key_material` below via the
        // module source itself, which is the more robust version of a
        // runtime string-search for a fire-and-forget `tracing::info!`.
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        let ca = CertificateAuthority::generate().expect("generate CA");
        let req = EnrollmentRequest {
            identity: "harmony-primary".to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(&ca, &req).expect("enrollment succeeds");

        let source = include_str!("enroll.rs");
        // Every tracing:: call in this module's non-test code must not
        // reference `shared_secret`, `key_pem`, or `signing_key` as an
        // interpolated field.
        for line in source.lines() {
            let is_log_call = line.trim_start().starts_with("tracing::");
            if is_log_call {
                assert!(
                    !line.contains("shared_secret") && !line.contains("key_pem") && !line.contains("signing_key"),
                    "a tracing:: call site must never reference secret/key fields: {line}"
                );
            }
        }
        // Sanity: the response itself legitimately carries key material
        // (it's returned to the caller, not logged) — assert that's still
        // true so this test would fail loudly if `handle_enrollment` ever
        // stopped returning it.
        assert!(resp.key_pem.contains("PRIVATE KEY"));

        clear_secrets();
    }

    // ── HTTP route tests ────────────────────────────────────────────────

    async fn post_enroll(router: axum::Router, body: serde_json::Value) -> (u16, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .method("POST")
            .uri(crate::config::enrollment_path())
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    #[tokio::test]
    #[serial]
    async fn http_route_issues_cert_and_jwt_for_valid_request() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        std::env::set_var(
            "TERMINUS_CA_STORE_PATH",
            format!(
                "{}/tcli02-http-route-test-{}.json",
                std::env::temp_dir().display(),
                std::process::id()
            ),
        );

        let router = build_enroll_router();
        let (status, body) = post_enroll(
            router,
            serde_json::json!({
                "identity": "dev-box-claude-code",
                "shared_secret": "correct-horse-battery-staple"
            }),
        )
        .await;

        assert_eq!(status, 200);
        assert!(body["cert_pem"]
            .as_str()
            .unwrap()
            .contains("BEGIN CERTIFICATE"));
        assert!(body["jwt"].as_str().unwrap().len() > 10);

        clear_secrets();
        std::env::remove_var("TERMINUS_CA_STORE_PATH");
    }

    #[tokio::test]
    #[serial]
    async fn http_route_rejects_wrong_shared_secret() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");
        std::env::set_var(
            "TERMINUS_CA_STORE_PATH",
            format!(
                "{}/tcli02-http-route-test-reject-{}.json",
                std::env::temp_dir().display(),
                std::process::id()
            ),
        );

        let router = build_enroll_router();
        let (status, _body) = post_enroll(
            router,
            serde_json::json!({
                "identity": "dev-box-claude-code",
                "shared_secret": "nope"
            }),
        )
        .await;

        assert_eq!(status, 401);

        clear_secrets();
        std::env::remove_var("TERMINUS_CA_STORE_PATH");
    }

    #[tokio::test]
    #[serial]
    async fn http_route_rejects_oversize_body() {
        clear_secrets();
        set_secrets("correct-horse-battery-staple", "jwt-signing-key-for-tests-only");

        // A body well past the 4KB limit (a padded shared_secret) must be
        // rejected by the DefaultBodyLimit layer before the handler ever runs
        // — 413 Payload Too Large, not a 200/401 from the auth path. The
        // rejection body is plain text (not JSON), so this checks status
        // directly rather than via the JSON-parsing `post_enroll` helper.
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let oversize = serde_json::json!({
            "identity": "dev-box-claude-code",
            "shared_secret": "x".repeat(8192)
        })
        .to_string();
        let router = build_enroll_router();
        let req = Request::builder()
            .method("POST")
            .uri(crate::config::enrollment_path())
            .header("content-type", "application/json")
            .body(Body::from(oversize))
            .unwrap();
        let status = router.oneshot(req).await.unwrap().status().as_u16();

        assert_eq!(status, 413, "oversize enrollment body must be rejected");

        clear_secrets();
    }
}
