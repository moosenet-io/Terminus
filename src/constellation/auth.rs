//! CONST-03: real signed-session auth + deny-unauthenticated guard for the
//! constellation control plane.
//!
//! This module replaces CONST-02's auth SEAM (an unsigned, plain cookie
//! that accepted any non-empty username/password) with the real thing:
//!
//! - **Login** (`auth_login`) verifies the submitted password against an
//!   operator shared secret (`CONSTELLATION_OPERATOR_SECRET`,
//!   `crate::config::constellation_operator_secret`), compared in constant
//!   time (`crate::pki::enroll::constant_time_eq` — the same comparator
//!   TCLI-02's enrollment shared-secret check uses). On success it mints a
//!   signed session JWT (`crate::pki::enroll::mint_jwt_with_ttl`, reusing
//!   the SAME `TERMINUS_JWT_SIGNING_KEY` HS256 signing primitive TCLI-02's
//!   enrollment JWT uses — no new JWT crate, no hand-rolled HS256) and sets
//!   it as an HttpOnly, SameSite=Lax session cookie. If
//!   `CONSTELLATION_OPERATOR_SECRET` is unset, login fails closed (every
//!   attempt is rejected, never a default-allow).
//! - **Session verification** (`session_from_cookie`) verifies the
//!   cookie's JWT signature and expiry
//!   (`crate::pki::enroll::verify_jwt`) instead of trusting the cookie's
//!   plaintext value. An invalid, expired, tampered, or absent token
//!   resolves to no [`SessionSeam`].
//! - **The guard** ([`require_session`]) is real `axum` middleware
//!   (`axum::middleware::from_fn`), layered in `crate::constellation::mod`
//!   over the proxied `/api/{harmony,chord,lumina}/*` and
//!   `/api/terminus/config` routes only — an unauthenticated request to any
//!   of those is rejected `401` before any backend dispatch. `/api/auth/*`
//!   and `/api/health` stay reachable pre-auth (see `mod.rs`'s router
//!   wiring for exactly which routes are public vs. protected).
//!
//! [`SessionSeam`]/`principal_from_cookie`/`auth_me`'s shapes are UNCHANGED
//! from CONST-02 — `crate::constellation::proxy`'s audit-attribution call
//! site and `constellation-web`'s `aggregationClient.ts` contract needed no
//! changes for this item.

use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

const SESSION_COOKIE: &str = "constellation_session";

/// The resolved, VERIFIED session identity for one request — populated only
/// from a signature+expiry-checked JWT (see [`session_from_cookie`]), never
/// from an unsigned cookie value.
#[derive(Debug, Clone)]
pub struct SessionSeam {
    pub username: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

fn auth_me_body(seam: Option<&SessionSeam>) -> serde_json::Value {
    match seam {
        Some(s) => json!({"authenticated": true, "username": s.username}),
        None => json!({"authenticated": false, "username": null}),
    }
}

fn unauthorized_response() -> Response {
    let masked = crate::constellation::mask::mask_response(json!({"error": "unauthorized"}));
    (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// Extract a [`SessionSeam`] from the request's `Cookie` header, when
/// present and its token verifies. Used both by `auth_me`/audit attribution
/// (`crate::constellation::proxy::principal_from_headers`) and by
/// [`require_session`].
pub fn principal_from_cookie(headers: &HeaderMap) -> Option<String> {
    session_from_cookie(headers).map(|s| s.username)
}

/// Resolve a [`SessionSeam`] from the request's `Cookie` header: find the
/// `constellation_session` cookie, then verify it as a JWT via
/// `crate::pki::enroll::verify_jwt` (signature + expiry, HS256,
/// `TERMINUS_JWT_SIGNING_KEY`). Anything that doesn't verify (absent
/// cookie, malformed value, bad signature, expired) resolves to `None` —
/// never a partial/best-effort session.
fn session_from_cookie(headers: &HeaderMap) -> Option<SessionSeam> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            if value.is_empty() {
                continue;
            }
            return verify_session_token(value);
        }
    }
    None
}

/// Verify a raw session token string as a signed JWT, returning a
/// [`SessionSeam`] on success. Every failure path is logged (never with the
/// token itself) and resolves to `None` — no partial trust.
fn verify_session_token(token: &str) -> Option<SessionSeam> {
    match crate::pki::enroll::verify_jwt(token) {
        Ok(claims) => Some(SessionSeam { username: claims.sub }),
        Err(e) => {
            tracing::warn!("constellation::auth: rejected session token: {e}");
            None
        }
    }
}

/// Build the `Set-Cookie` header for a freshly minted session `token`.
/// HttpOnly + SameSite=Lax always; `Secure` is added only when
/// `crate::config::constellation_cookie_secure` is true (an operator
/// serving this behind TLS) — a LAN-served dev/operator UI may run over
/// plain HTTP, so `Secure` is never hardcoded on. `Max-Age` matches the
/// token's own TTL so the browser doesn't hold a cookie past the JWT's own
/// `exp` — the JWT's `exp` remains authoritative server-side regardless.
fn set_cookie_header(token: &str, ttl_seconds: i64) -> String {
    let secure = if crate::config::constellation_cookie_secure() { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_seconds}{secure}")
}

fn clear_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub async fn auth_me(headers: HeaderMap) -> Response {
    let seam = session_from_cookie(&headers);
    let masked = crate::constellation::mask::mask_response(auth_me_body(seam.as_ref()));
    (StatusCode::OK, [("content-type", "application/json")], masked.to_string()).into_response()
}

pub async fn auth_login(headers: HeaderMap, body: Bytes) -> Response {
    let request_path = "/api/auth/login";
    // Login itself carries a password in its body -- audit it too (it's a
    // mutating request), sanitized exactly like every other mutating
    // `/api/*` call; `sanitize_body` redacts the password field before
    // anything is written to the audit log. Recorded unconditionally (both
    // success and failure paths reach this point), matching this sink's
    // "every mutating request" contract -- the raw password never reaches
    // the log either way.
    crate::constellation::audit::record_mutating_request(
        "auth",
        "POST",
        request_path,
        None,
        &crate::constellation::audit::body_text(&body),
    );
    let _ = &headers; // reserved (e.g. future rate-limit by caller)

    let parsed: LoginRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => LoginRequest { username: String::new(), password: String::new() },
    };

    if parsed.username.trim().is_empty() || parsed.password.is_empty() {
        tracing::warn!("constellation::auth: login rejected -- missing username or password");
        return unauthorized_response();
    }

    // Fail-closed: no configured operator secret means NO login can ever
    // succeed, never a default-allow. This is deliberately checked before
    // touching the submitted password so an unconfigured deployment always
    // takes the same code path regardless of what's submitted.
    let Some(operator_secret) = crate::config::constellation_operator_secret() else {
        tracing::warn!(
            "constellation::auth: login rejected -- CONSTELLATION_OPERATOR_SECRET is unset \
             (fail-closed, not default-allow)"
        );
        return unauthorized_response();
    };

    // Constant-time comparison (reusing `crate::pki::enroll`'s comparator --
    // the same one TCLI-02's enrollment shared-secret check uses) so a
    // timing side channel can't be used to guess the operator secret byte
    // by byte.
    if !crate::pki::enroll::constant_time_eq(parsed.password.as_bytes(), operator_secret.as_bytes()) {
        tracing::warn!(username = %parsed.username.trim(), "constellation::auth: login rejected -- invalid credential");
        return unauthorized_response();
    }

    let username = parsed.username.trim().to_string();
    let ttl = crate::config::constellation_session_ttl_seconds();
    let token = match crate::pki::enroll::mint_jwt_with_ttl(&username, ttl) {
        Ok((jwt, _exp)) => jwt,
        Err(e) => {
            tracing::warn!("constellation::auth: failed to mint session token: {e}");
            let masked = crate::constellation::mask::mask_response(json!({"error": "internal error"}));
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                masked.to_string(),
            )
                .into_response();
        }
    };

    tracing::info!(username = %username, "constellation::auth: login succeeded");

    let seam = SessionSeam { username };
    let masked = crate::constellation::mask::mask_response(auth_me_body(Some(&seam)));
    let mut resp = (StatusCode::OK, [("content-type", "application/json")], masked.to_string())
        .into_response();
    if let Ok(hv) = axum::http::HeaderValue::from_str(&set_cookie_header(&token, ttl)) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, hv);
    }
    resp
}

pub async fn auth_logout(headers: HeaderMap) -> Response {
    let seam = session_from_cookie(&headers);
    crate::constellation::audit::record_mutating_request(
        "auth",
        "POST",
        "/api/auth/logout",
        seam.as_ref().map(|s| s.username.as_str()),
        "",
    );
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let Ok(hv) = axum::http::HeaderValue::from_str(&clear_cookie_header()) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, hv);
    }
    resp
}

/// `axum` middleware that DENIES an unauthenticated request. Layered (see
/// `crate::constellation::mod::constellation_router`) over the proxied
/// `/api/{harmony,chord,lumina}/*` and `/api/terminus/config` routes only —
/// `/api/auth/*` and `/api/health` are wired outside this layer so they
/// stay reachable pre-auth (a caller can't log in through a route that
/// itself requires being logged in). An unauthenticated request never
/// reaches the wrapped handler -- no backend dispatch, no proxying -- it is
/// rejected `401` here, before `next.run(..)` is ever called.
pub async fn require_session(headers: HeaderMap, request: Request, next: Next) -> Response {
    match session_from_cookie(&headers) {
        Some(_seam) => next.run(request).await,
        None => unauthorized_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    // login/logout handlers write to the process-global CONSTELLATION_AUDIT_LOG_PATH audit
    // sink, and several tests here set/unset TERMINUS_JWT_SIGNING_KEY /
    // CONSTELLATION_OPERATOR_SECRET / CONSTELLATION_SESSION_TTL_SECONDS -- serialize with
    // every other #[serial] test in the crate (env-var/global-path races), matching CONST-02's
    // existing convention.
    use serial_test::serial;

    fn headers_with_cookie(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_str(cookie).unwrap());
        h
    }

    fn set_jwt_key(key: &str) {
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", key);
    }

    fn clear_env() {
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
        std::env::remove_var("CONSTELLATION_OPERATOR_SECRET");
        std::env::remove_var("CONSTELLATION_SESSION_TTL_SECONDS");
    }

    #[test]
    fn no_cookie_means_no_session() {
        assert!(session_from_cookie(&HeaderMap::new()).is_none());
    }

    #[test]
    #[serial]
    fn empty_cookie_value_is_no_session() {
        clear_env();
        let headers = headers_with_cookie("constellation_session=");
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[test]
    #[serial]
    fn garbage_cookie_value_is_no_session() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let headers = headers_with_cookie("constellation_session=not-a-real-jwt");
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[test]
    #[serial]
    fn signed_session_round_trip_mint_then_verify() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", 300).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        let seam = session_from_cookie(&headers).unwrap();
        assert_eq!(seam.username, "alice");
        clear_env();
    }

    #[test]
    #[serial]
    fn tampered_session_token_is_rejected() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", 300).unwrap();
        // Flip a character in the signature portion (after the last '.') --
        // still well-formed JWT shape, but signature verification must fail.
        let mut tampered = token.clone();
        tampered.push('x');
        let headers = headers_with_cookie(&format!("constellation_session={tampered}"));
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[test]
    #[serial]
    fn expired_session_token_is_rejected() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        // A negative TTL mints a token whose `exp` is already in the past --
        // well beyond `jsonwebtoken`'s default 60s validation leeway, so
        // this is unambiguously expired, not a leeway edge case.
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", -300).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[test]
    #[serial]
    fn session_token_signed_with_a_different_key_is_rejected() {
        clear_env();
        set_jwt_key("key-one");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", 300).unwrap();
        set_jwt_key("key-two");
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[tokio::test]
    async fn auth_me_reports_unauthenticated_with_no_cookie() {
        let resp = auth_me(HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["authenticated"], false);
        assert_eq!(parsed["username"], serde_json::Value::Null);
    }

    #[tokio::test]
    #[serial]
    async fn auth_me_reports_authenticated_with_valid_session_cookie() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("bob", 300).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        let resp = auth_me(headers).await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["authenticated"], true);
        assert_eq!(parsed["username"], "bob");
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_rejects_empty_credentials() {
        clear_env();
        let resp = auth_login(HeaderMap::new(), Bytes::from_static(b"{}")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_fails_closed_when_operator_secret_unset() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        // CONSTELLATION_OPERATOR_SECRET deliberately left unset.
        let body = Bytes::from(
            serde_json::to_vec(&json!({"username": "carol", "password": "anything"})).unwrap(),
        );
        let resp = auth_login(HeaderMap::new(), body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_rejects_wrong_operator_secret() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "correct-secret"); // pii-test-fixture
        let body = Bytes::from(
            serde_json::to_vec(&json!({"username": "carol", "password": "wrong-secret"})).unwrap(),
        );
        let resp = auth_login(HeaderMap::new(), body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_accepts_correct_operator_secret_and_sets_a_verifiable_signed_cookie() {
        clear_env();
        set_jwt_key("test-signing-key-const03");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "correct-secret"); // pii-test-fixture
        let body = Bytes::from(
            serde_json::to_vec(&json!({"username": "carol", "password": "correct-secret"})).unwrap(),
        );
        let resp = auth_login(HeaderMap::new(), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp.headers().get(axum::http::header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(set_cookie.starts_with("constellation_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));

        // The cookie's value must be a JWT that verifies, not the plain
        // username (the CONST-02 stub's behavior) -- extract it and check.
        let token = set_cookie
            .strip_prefix("constellation_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert_ne!(token, "carol", "cookie must carry a signed token, not the raw username");
        let claims = crate::pki::enroll::verify_jwt(token).unwrap();
        assert_eq!(claims.sub, "carol");
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn logout_clears_cookie() {
        clear_env();
        let resp = auth_logout(HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let set_cookie = resp.headers().get(axum::http::header::SET_COOKIE).unwrap();
        assert!(set_cookie.to_str().unwrap().contains("Max-Age=0"));
        clear_env();
    }

    // `axum::middleware::Next` has no public constructor, so `require_session`
    // is exercised through a real (tiny) `Router` wearing the middleware --
    // the same shape it's actually layered in via
    // `crate::constellation::mod::constellation_router`.
    fn guarded_test_router() -> axum::Router {
        axum::Router::new()
            .route("/protected", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(require_session))
    }

    #[tokio::test]
    #[serial]
    async fn require_session_denies_a_request_with_no_cookie() {
        use tower::ServiceExt;
        clear_env();
        let req = Request::builder().uri("/protected").body(axum::body::Body::empty()).unwrap();
        let resp = guarded_test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn require_session_denies_a_request_with_an_invalid_cookie() {
        use tower::ServiceExt;
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let req = Request::builder()
            .uri("/protected")
            .header("cookie", "constellation_session=not-a-real-jwt")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = guarded_test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn require_session_allows_a_request_with_a_valid_cookie() {
        use tower::ServiceExt;
        clear_env();
        set_jwt_key("test-signing-key-const03");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", 300).unwrap();
        let req = Request::builder()
            .uri("/protected")
            .header("cookie", format!("constellation_session={token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = guarded_test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        clear_env();
    }
}
