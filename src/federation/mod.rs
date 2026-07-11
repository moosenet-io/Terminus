//! Personal-tool federation (TGW-02 — Terminus Primary Gateway sprint,
//! S108).
//!
//! Per the S108 spec's RESOLVED design decision (2): `terminus-primary`
//! (TGW-01) registers ONLY `registry::register_all` (core tools) locally —
//! see `crate::registry::core_personal_name_collisions` for why a locally
//! combined core+personal registry isn't viable. Personal-registry tools
//! (`ledger_*`, `vitals_*`, `git_private`, etc. — the `terminus_personal`
//! subset) are instead reached by **reusing Chord's existing, already-live
//! federation**: Chord (`moosenet/Chord`) already proxies
//! `POST /v1/personal/tools/list` and `POST /v1/personal/tools/call` to
//! the personal-registry host's `terminus_personal` deployment (see Chord's
//! `src/routes.rs::{personal_tools_list, personal_tools_call}` and its
//! `AppState::personal_proxy`). This module is the CLIENT side of that same
//! relay, called from `terminus-primary`'s own `/mcp` handler
//! (`crate::mcp_server`) instead of a new direct primary→personal-registry path — this
//! avoids a second, redundant the personal-registry host-reachability dependency and reuses the
//! known-working hop.
//!
//! ## Auth: a short-lived service JWT, not terminus-primary's own mTLS
//! Chord's `/v1/personal/tools/*` routes gate on the SAME JWT scheme Chord's
//! `/v1/tools/*` routes already use (`crate::routes::auth_check` in the
//! Chord repo, HS256, secret = `CHORD_JWT_SECRET`, and — a real, load-bearing
//! detail confirmed by reading Chord's `src/auth.rs` — `validate_jwt`
//! HARD-REQUIRES `sub == "lumina"`; any other subject is rejected as
//! `AuthError::InvalidSubject`). So the JWT this module mints is a small,
//! short-lived (`crate::config::chord_personal_federation_timeout_ms`-scale)
//! service credential shaped exactly like Chord expects
//! (`{"sub": "lumina", "exp": <unix-seconds>}`), signed with the SAME shared
//! secret Chord validates against — provisioned into `terminus-primary`'s
//! own environment as `TERMINUS_PRIMARY_CHORD_JWT_SECRET` (never a literal;
//! this crate's established "materialized into process env, plain env read
//! after that IS the SecretManager read" convention — see `crate::pki`'s
//! module doc for why there's no separate `SecretManager::get()`/
//! `vault::manager()` API here). The OPERATOR provisions
//! `TERMINUS_PRIMARY_CHORD_JWT_SECRET` with the same value as Chord's own
//! `CHORD_JWT_SECRET` at deploy time (TGW-05) — this module has no write
//! path back to any secret store, consistent with the standing "no
//! self-serve secrets" rule.
//!
//! This JWT authenticates *terminus-primary as a service* to Chord — it is
//! deliberately NOT the calling human/agent's own identity (Chord's
//! `Claims::sub` is pinned to `"lumina"` and carries no room for a second
//! identity). The CALLER's identity (extracted from the mTLS client cert
//! that reached `terminus-primary`'s front door, see
//! `crate::pki::mtls::ClientIdentity`) is instead forwarded as a plain
//! header (`X-Terminus-Client-Identity`) alongside the JWT, so the tool
//! itself (and Chord's audit log) can still see who actually asked, exactly
//! as the design's "preserve identity + audit" requirement calls for. This
//! is additive metadata, not a second auth mechanism — Chord's own JWT
//! check is what actually gates the request.
//!
//! ## Error classification (transport vs. tool-level)
//! Chord's `proxy_error_response` (Chord `src/routes.rs`) maps a personal
//! tool-call failure to one of: `404` (`ProxyError::ToolNotFound`), `504`
//! (`ProxyError::Timeout` — Chord's own hop to the personal-registry host timed out), `502`
//! (everything else, including `ProxyError::ToolExecution` — the tool
//! itself ran and failed on the personal-registry host), or a dedicated `503` when
//! `PERSONAL_BACKEND_URL` isn't configured on Chord at all. This module
//! mirrors that into two buckets:
//! - **[`FederationError`]** — could not get a *tool-shaped* answer out of
//!   the relay at all: terminus-primary couldn't reach Chord (connection
//!   refused/DNS failure), the call to Chord itself timed out, Chord
//!   rejected the service JWT (401) or rate-limited it (429), Chord has no
//!   the personal-registry host backend configured (503), or the personal-registry host itself was too slow (504).
//!   None of these mean "the tool ran and failed" — they mean the
//!   federation hop itself broke, so the caller should see a federation
//!   error, not a tool result.
//! - **[`FederationCallResult`]** (the `Ok` case) — a tool-shaped answer DID
//!   come back: either genuine success (Chord's `200`, `is_error: false`)
//!   or a tool-level failure (Chord's `404`/`502` — the relay reached
//!   the personal-registry host, and either the tool name wasn't found there or it executed and
//!   failed — surfaced as `is_error: true` with the personal-registry host/Chord's own message,
//!   distinct from a [`FederationError`]).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::mesh::Principal;

/// Read an env var, trimmed; `None` when unset or empty. Same convention as
/// `crate::config`'s private helper of the same name — duplicated here
/// (rather than made `pub(crate)` there) to keep this module's one secret
/// read self-contained and easy to audit.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Claims shape Chord's `validate_jwt` (Chord `src/auth.rs`) requires:
/// `sub` MUST be the literal string `"lumina"`, `exp` is unix seconds, `role`
/// is optional and unused by this module (Chord defaults an absent role to
/// its own "user" role server-side).
///
/// `principal` (MESH-07) is an ADDITIVE, optional claim: the resolved
/// caller [`Principal::name`] for this call, carried INSIDE the signed JWT
/// rather than as a raw, client-settable header — forging it would require
/// the shared `TERMINUS_PRIMARY_CHORD_JWT_SECRET` signing key itself, so an
/// upstream that opts into reading it gets a tamper-evident propagation of
/// the gateway's RBAC decision, not just an unauthenticated hint. It is
/// `#[serde(skip_serializing_if = "Option::is_none")]` (omitted entirely,
/// never serialized as `null`) so a call with no resolved principal (e.g.
/// the plain HTTP+JWT listener, no mTLS/tailnet identity presented) produces
/// the exact byte-for-byte pre-MESH-07 claims shape, and so an upstream
/// (Chord's own `validate_jwt` today) that has no opinion on this claim at
/// all keeps validating unchanged — it only checks `sub`/`exp`. The
/// transport-auth `sub` stays pinned to `"lumina"` regardless (see this
/// module's "Auth" doc section for why) — `principal` is the RBAC identity
/// of record, `sub` is only who's allowed to speak to Chord's relay at all.
#[derive(Debug, Serialize, Deserialize)]
struct ChordServiceClaims {
    sub: String,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal: Option<String>,
}

/// The one subject value Chord's JWT validation accepts (`Claims::sub !=
/// "lumina"` is a hard `AuthError::InvalidSubject` rejection in Chord's
/// `src/auth.rs`) — not a `terminus-primary`-chosen identity, a fixed
/// contract of the existing relay this module reuses.
const CHORD_SERVICE_SUBJECT: &str = "lumina";

/// Header the caller's mTLS-derived identity (if any) is forwarded under,
/// alongside the service JWT — additive audit/identity metadata, not a
/// second auth mechanism (Chord's own JWT check is what actually gates the
/// request). `pub(crate)`: `crate::inference_proxy` (TGW-03) reuses this same
/// header name/convention for its own hop to Chord's inference routes, so
/// both federation hops present caller identity to Chord identically.
pub(crate) const CLIENT_IDENTITY_HEADER: &str = "x-terminus-client-identity";

/// Errors from the federation hop itself (terminus-primary ⇄ Chord, or
/// Chord ⇄ the personal-registry host as reported by Chord) — distinct from a tool-level failure
/// that DID make it through the relay (see the module doc's "Error
/// classification" section, and [`FederationCallResult`]).
#[derive(Debug, Error)]
pub enum FederationError {
    /// Failed to mint the outbound service JWT (e.g.
    /// `TERMINUS_PRIMARY_CHORD_JWT_SECRET` is unset).
    #[error("failed to mint federation service JWT: {0}")]
    JwtSigning(String),
    /// Could not open a connection to Chord's relay at all (connection
    /// refused, DNS failure, TLS failure, etc.) — Chord itself is
    /// unreachable, not just slow.
    #[error("chord personal-tool relay unreachable: {0}")]
    Unreachable(String),
    /// The request to Chord (or Chord's own hop to the personal-registry host, reported back as
    /// its `504`) did not complete within the configured timeout.
    #[error("chord personal-tool relay timed out: {0}")]
    Timeout(String),
    /// Chord's own JWT check rejected the service credential (`401`) — a
    /// misconfigured/rotated `TERMINUS_PRIMARY_CHORD_JWT_SECRET`, not a
    /// tool-level problem.
    #[error("chord rejected the federation service credential: {0}")]
    AuthRejected(String),
    /// Chord rate-limited the federation caller (`429`).
    #[error("chord personal-tool relay rate-limited the request: {0}")]
    RateLimited(String),
    /// Chord has no the personal-registry host backend configured at all
    /// (`PERSONAL_BACKEND_URL` unset on the Chord side, its `503`) — a
    /// deploy/config gap on Chord's side, not this call's fault.
    #[error("chord has no personal-tool backend configured: {0}")]
    BackendUnconfigured(String),
    /// Chord returned a response this client could not parse into the
    /// expected shape (`{"result": ...}` on success, `{"error": ...}` on
    /// failure).
    #[error("unexpected response from chord personal-tool relay: {0}")]
    BadResponse(String),
}

/// A tool-shaped answer that DID come back from the relay — either genuine
/// success or a tool-level failure the personal-registry host/Chord reported (see the module
/// doc's "Error classification" section). Distinct from [`FederationError`],
/// which means the relay hop itself never produced a tool-shaped answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationCallResult {
    /// Human-readable text: the tool's own result on success, or a
    /// tool-level error message on failure.
    pub text: String,
    /// `true` when this is a tool-level failure (the personal-registry host tool not found, or
    /// executed and returned an error) rather than genuine success.
    pub is_error: bool,
}

/// Client for terminus-primary's federation hop to Chord's
/// `/v1/personal/tools/*` relay. Cheap to construct/clone (wraps a shared
/// `reqwest::Client` + a base URL String) — one instance lives on
/// `terminus-primary`'s [`crate::mcp_server::McpServerState`] for the
/// process lifetime.
#[derive(Debug, Clone)]
pub struct PersonalFederationClient {
    base_url: String,
    timeout: Duration,
    http: reqwest::Client,
}

impl PersonalFederationClient {
    /// Build a client from env config (`crate::config::chord_personal_federation_url`
    /// / `chord_personal_federation_timeout_ms`) — what `terminus_primary`'s
    /// `main()` calls.
    pub fn from_env() -> Self {
        Self::with_base_url(crate::config::chord_personal_federation_url())
    }

    /// Build a client pointed at an explicit base URL (e.g. a mocked Chord
    /// endpoint in tests, or an operator override already resolved by the
    /// caller). Timeout still comes from
    /// `crate::config::chord_personal_federation_timeout_ms` unless
    /// overridden via [`PersonalFederationClient::with_timeout`].
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout: Duration::from_millis(crate::config::chord_personal_federation_timeout_ms()),
            http: reqwest::Client::new(),
        }
    }

    /// Override the per-call timeout (mainly for tests that want a fast
    /// failure against a deliberately unreachable address rather than
    /// waiting out the 30s production default).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Dispatch `name(arguments)` through Chord's `/v1/personal/tools/call`,
    /// presenting a freshly-minted service JWT and forwarding `principal`
    /// (MESH-07: the resolved canonical caller [`Principal`] — mapped, when
    /// `crate::mesh::PrincipalResolver` has a configured map — for whoever
    /// called terminus-primary, if any) for audit purposes AND as a signed
    /// JWT claim. See the module doc's "Error classification" section for
    /// the `Ok`/`Err` split, and [`ChordServiceClaims`]'s doc for why the
    /// signed `principal` claim is the tamper-evident propagation path
    /// rather than the plain [`CLIENT_IDENTITY_HEADER`] alone (kept
    /// alongside it for backward compatibility with the existing
    /// personal/Chord relay contract, which already reads that header).
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        principal: Option<&Principal>,
    ) -> Result<FederationCallResult, FederationError> {
        let jwt = mint_service_jwt_for_principal(principal)?;

        let mut req = self
            .http
            .post(format!("{}/v1/personal/tools/call", self.base_url))
            .timeout(self.timeout)
            .bearer_auth(jwt)
            .json(&json!({"name": name, "arguments": arguments}));
        if let Some(p) = principal {
            req = req.header(CLIENT_IDENTITY_HEADER, p.name());
        }

        let resp = req.send().await.map_err(|e| classify_transport_error(&e))?;
        let status = resp.status();

        if status.is_success() {
            let body: Value = resp
                .json()
                .await
                .map_err(|e| FederationError::BadResponse(e.to_string()))?;
            let text = body
                .get("result")
                .and_then(|r| r.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    FederationError::BadResponse(
                        "success response missing string \"result\" field".to_string(),
                    )
                })?;
            return Ok(FederationCallResult { text, is_error: false });
        }

        let error_text = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|b| b.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("chord relay returned HTTP {status}"));

        match status.as_u16() {
            // Chord's proxy_error_response: ToolNotFound -> 404, the
            // catch-all (incl. ToolExecution, the genuine "tool ran and
            // failed" case) -> 502. Both mean the relay DID reach the personal-registry host and
            // got a tool-shaped answer -- a tool-level result, not a
            // federation-transport failure.
            404 | 502 => Ok(FederationCallResult { text: error_text, is_error: true }),
            // Chord's own hop to the personal-registry host timed out (ProxyError::Timeout -> 504)
            // -- the relay never got a tool-shaped answer.
            504 => Err(FederationError::Timeout(error_text)),
            // PERSONAL_BACKEND_URL unset on the Chord side.
            503 => Err(FederationError::BackendUnconfigured(error_text)),
            401 => Err(FederationError::AuthRejected(error_text)),
            429 => Err(FederationError::RateLimited(error_text)),
            _ => Err(FederationError::BadResponse(format!(
                "unexpected status {status}: {error_text}"
            ))),
        }
    }
}

/// Classify a `reqwest::Error` from the `send().await` step itself (i.e. no
/// HTTP response was ever received) into [`FederationError::Timeout`] or
/// [`FederationError::Unreachable`].
fn classify_transport_error(e: &reqwest::Error) -> FederationError {
    if e.is_timeout() {
        FederationError::Timeout(e.to_string())
    } else {
        FederationError::Unreachable(e.to_string())
    }
}

/// Mint a short-lived service JWT shaped exactly as Chord's `validate_jwt`
/// requires (`sub: "lumina"`, `exp` a near-future unix timestamp), signed
/// with `TERMINUS_PRIMARY_CHORD_JWT_SECRET` (HS256) — the SAME secret value
/// Chord validates incoming `/v1/personal/tools/*` requests against
/// (`CHORD_JWT_SECRET` on Chord's side), provisioned into terminus-primary's
/// own environment at deploy time. See the module doc's "Auth" section for
/// why the subject is pinned and why this isn't the caller's own identity.
///
/// `pub(crate)`: TGW-03's `crate::inference_proxy` reuses this exact minting
/// logic for its own hop to Chord's inference routes (`/v1/chat/completions`
/// et al.), which gate on the SAME `auth_check`/`CHORD_JWT_SECRET` scheme as
/// `/v1/personal/tools/*` (confirmed by reading Chord's `src/routes.rs` —
/// every Chord route this crate proxies to shares one `auth_check` call) —
/// factored here rather than duplicated, per the TGW-03 spec item's "reuse
/// that machinery" instruction. Mints with no `principal` claim
/// (`crate::inference_proxy` predates MESH-07's principal propagation and is
/// out of this item's scope — see [`mint_service_jwt_for_principal`] for the
/// principal-carrying variant [`PersonalFederationClient::call_tool`] uses).
pub(crate) fn mint_service_jwt() -> Result<String, FederationError> {
    mint_service_jwt_for_principal(None)
}

/// MESH-07: mint the same short-lived Chord-shaped service JWT
/// [`mint_service_jwt`] does, additionally carrying the resolved caller
/// [`Principal::name`] (if any) as a signed `principal` claim — see
/// [`ChordServiceClaims`]'s doc for why this is the tamper-evident
/// propagation path.
pub(crate) fn mint_service_jwt_for_principal(
    principal: Option<&Principal>,
) -> Result<String, FederationError> {
    let signing_key = env_nonempty("TERMINUS_PRIMARY_CHORD_JWT_SECRET").ok_or_else(|| {
        FederationError::JwtSigning("TERMINUS_PRIMARY_CHORD_JWT_SECRET is unset".to_string())
    })?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| FederationError::JwtSigning(format!("system clock: {e}")))?
        .as_secs();
    // Short-lived: this JWT is minted fresh per call, not cached/reused, so
    // it only needs to outlive one federation round trip.
    let exp = now + 120;

    let claims = ChordServiceClaims {
        sub: CHORD_SERVICE_SUBJECT.to_string(),
        exp,
        principal: principal.map(|p| p.name().to_string()),
    };

    encode(&Header::default(), &claims, &EncodingKey::from_secret(signing_key.as_bytes()))
        .map_err(|e| FederationError::JwtSigning(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::PrincipalSource;
    use httpmock::MockServer;
    use serial_test::serial;

    fn set_jwt_secret() {
        std::env::set_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET", "test-chord-shared-secret");
    }
    fn clear_jwt_secret() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET");
    }

    #[test]
    #[serial]
    fn mint_service_jwt_fails_loudly_when_secret_unset() {
        clear_jwt_secret();
        let err = mint_service_jwt().expect_err("must fail with no signing key configured");
        assert!(matches!(err, FederationError::JwtSigning(_)));
    }

    #[test]
    #[serial]
    fn mint_service_jwt_produces_chord_shaped_claims() {
        set_jwt_secret();
        let jwt = mint_service_jwt().expect("signing should succeed");

        // Decode with the same secret/algorithm Chord's validate_jwt uses,
        // proving the shape (`sub` == "lumina", `exp` in the future) matches
        // what Chord actually requires -- not just "some JWT came out".
        use jsonwebtoken::{decode, DecodingKey, Validation};
        let decoded = decode::<ChordServiceClaims>(
            &jwt,
            &DecodingKey::from_secret(b"test-chord-shared-secret"),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .expect("jwt should decode with the shared secret");
        assert_eq!(decoded.claims.sub, "lumina");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!(decoded.claims.exp > now, "exp should be in the future");
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_success_returns_ok_with_is_error_false() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/personal/tools/call")
                .json_body_partial(r#"{"name": "ledger_accounts"}"#);
            then.status(200)
                .json_body(json!({"result": "3 accounts", "source": "terminus_personal"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let principal = Principal::new("dev-box", PrincipalSource::MtlsCert);
        let outcome = client
            .call_tool("ledger_accounts", json!({}), Some(&principal))
            .await
            .expect("call should succeed");
        assert_eq!(outcome.text, "3 accounts");
        assert!(!outcome.is_error);
        mock.assert();
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_forwards_caller_identity_header() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/personal/tools/call")
                .header(CLIENT_IDENTITY_HEADER, "harmony-primary");
            then.status(200).json_body(json!({"result": "ok", "source": "terminus_personal"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let principal = Principal::new("harmony-primary", PrincipalSource::MtlsCert);
        client
            .call_tool("ledger_accounts", json!({}), Some(&principal))
            .await
            .expect("call should succeed");
        mock.assert();
        clear_jwt_secret();
    }

    // ── MESH-07: signed `principal` claim (tamper-evident propagation) ────

    #[tokio::test]
    #[serial]
    async fn call_tool_signs_principal_into_the_jwt_not_just_the_header() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/personal/tools/call")
                .matches(|req| {
                    let auth = req
                        .headers
                        .as_ref()
                        .and_then(|hs| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    let Some(token) = auth.strip_prefix("Bearer ") else { return false };
                    use jsonwebtoken::{decode, DecodingKey, Validation};
                    decode::<ChordServiceClaims>(
                        token,
                        &DecodingKey::from_secret(b"test-chord-shared-secret"),
                        &Validation::new(jsonwebtoken::Algorithm::HS256),
                    )
                    .map(|d| d.claims.principal.as_deref() == Some("harmony"))
                    .unwrap_or(false)
                });
            then.status(200).json_body(json!({"result": "ok", "source": "terminus_personal"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let principal = Principal::new("harmony", PrincipalSource::MtlsCert);
        client
            .call_tool("ledger_accounts", json!({}), Some(&principal))
            .await
            .expect("call should succeed");
        mock.assert();
        clear_jwt_secret();
    }

    #[test]
    #[serial]
    fn mint_service_jwt_omits_principal_claim_when_none() {
        set_jwt_secret();
        let jwt = mint_service_jwt_for_principal(None).expect("signing should succeed");
        use jsonwebtoken::{decode, DecodingKey, Validation};
        let decoded = decode::<ChordServiceClaims>(
            &jwt,
            &DecodingKey::from_secret(b"test-chord-shared-secret"),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .expect("jwt should decode");
        assert_eq!(decoded.claims.principal, None);
        // The pre-MESH-07 `mint_service_jwt()` entrypoint (still used by
        // `crate::inference_proxy`) must produce byte-identical claims to
        // the explicit `None` call above.
        let legacy_jwt = mint_service_jwt().expect("signing should succeed");
        let legacy_decoded = decode::<ChordServiceClaims>(
            &legacy_jwt,
            &DecodingKey::from_secret(b"test-chord-shared-secret"),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .expect("jwt should decode");
        assert_eq!(legacy_decoded.claims.principal, None);
        assert_eq!(legacy_decoded.claims.sub, "lumina");
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_presents_bearer_jwt_signed_with_shared_secret() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/personal/tools/call")
                .matches(|req| {
                    let auth = req
                        .headers
                        .as_ref()
                        .and_then(|hs| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    let Some(token) = auth.strip_prefix("Bearer ") else { return false };
                    use jsonwebtoken::{decode, DecodingKey, Validation};
                    decode::<ChordServiceClaims>(
                        token,
                        &DecodingKey::from_secret(b"test-chord-shared-secret"),
                        &Validation::new(jsonwebtoken::Algorithm::HS256),
                    )
                    .map(|d| d.claims.sub == "lumina")
                    .unwrap_or(false)
                });
            then.status(200).json_body(json!({"result": "ok", "source": "terminus_personal"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect("call should succeed");
        mock.assert();
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_tool_not_found_is_tool_level_not_federation_error() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(404).json_body(json!({"error": "tool not found: bogus_tool"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let outcome = client
            .call_tool("bogus_tool", json!({}), None)
            .await
            .expect("a 404 is a tool-level result, not an Err");
        assert!(outcome.is_error);
        assert!(outcome.text.contains("bogus_tool"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_execution_failure_502_is_tool_level_not_federation_error() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(502).json_body(json!({"error": "tool execution failed: bad gitea PAT"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let outcome = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect("a 502 (tool execution error) is a tool-level result, not an Err");
        assert!(outcome.is_error);
        assert!(outcome.text.contains("bad gitea PAT"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_backend_unconfigured_503_is_federation_error() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(503).json_body(json!({"error": "personal backend not configured"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let err = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect_err("a 503 must surface as a FederationError, never a tool result");
        assert!(matches!(err, FederationError::BackendUnconfigured(_)));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_chord_gateway_timeout_504_is_federation_error() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(504).json_body(json!({"error": "backend timeout"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let err = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect_err("a 504 must surface as a FederationError::Timeout");
        assert!(matches!(err, FederationError::Timeout(_)));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_auth_rejected_401_is_federation_error() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(401).json_body(json!({"error": "invalid token"}));
        });

        let client = PersonalFederationClient::with_base_url(server.base_url());
        let err = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect_err("a 401 must surface as a FederationError, not a tool result");
        assert!(matches!(err, FederationError::AuthRejected(_)));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_unreachable_chord_is_federation_error_no_hang() {
        set_jwt_secret();
        // No mock server started at all -- nothing listening on this port.
        let client = PersonalFederationClient::with_base_url("http://127.0.0.1:1")
            .with_timeout(Duration::from_millis(500));
        let err = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect_err("an unreachable chord must surface as a FederationError, no hang");
        assert!(matches!(
            err,
            FederationError::Unreachable(_) | FederationError::Timeout(_)
        ));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn call_tool_fails_fast_with_no_jwt_secret_configured() {
        clear_jwt_secret();
        let server = MockServer::start();
        let client = PersonalFederationClient::with_base_url(server.base_url());
        let err = client
            .call_tool("ledger_accounts", json!({}), None)
            .await
            .expect_err("no signing key configured must fail before any network call");
        assert!(matches!(err, FederationError::JwtSigning(_)));
    }
}
