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
//!
//! ## CONST-27: viewer role
//!
//! Extends the above with a **`role` claim** in the same session JWT (§3.4)
//! rather than a new auth system:
//! - [`auth_login`] checks the submitted password against
//!   `CONSTELLATION_OPERATOR_SECRET` first (role [`Role::Operator`] on
//!   match), then `CONSTELLATION_VIEWER_SECRET` (role [`Role::Viewer`] on
//!   match) — both constant-time, both fail-closed when their respective
//!   secret is unset (an unconfigured viewer secret simply disables that
//!   tier; it never falls back to a default-allow).
//! - [`Role::from_claim`] treats a claim-ABSENT token as [`Role::Operator`]
//!   — backward compatible with sessions minted before this item shipped, so
//!   a live session survives the deploy instead of being silently
//!   downgraded.
//! - [`enforce_viewer_role_gate`] is the ONE server-side enforcement point
//!   (structural, not cosmetic): a viewer session gets `403
//!   {"error":"forbidden","required_role":"operator"}` on every mutating
//!   method (`POST`/`PUT`/`PATCH`/`DELETE`) reaching a route wrapped by it —
//!   see `crate::constellation::mod::protected_router`'s layering. The UI's
//!   `RoleGate` (`constellation-web/src/components/RoleGate.tsx`) is a
//!   courtesy layer only; this is what actually enforces it.

use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

const SESSION_COOKIE: &str = "constellation_session";

/// A session's access tier (CONST-27, §3.4). No third role, no per-module
/// ACLs (YAGNI — single-operator fleet, per the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full read/write access — every protected route.
    Operator,
    /// Read-only: mutating methods are rejected server-side by
    /// [`enforce_viewer_role_gate`] regardless of what the UI shows.
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    /// Resolve a decoded JWT's `role` claim into a [`Role`]. A claim-absent
    /// token (`None` — every session minted before CONST-27, and every
    /// enrollment JWT that happens to share this signing key) resolves to
    /// [`Role::Operator`]: that is the ONLY tier that existed before this
    /// item, so treating "no claim" as anything else would silently log out
    /// (downgrade) every live operator session on deploy. Any unrecognized
    /// string value (defensive — should never happen for a token this
    /// crate minted) also resolves to [`Role::Operator`] for the same
    /// backward-compatibility reason; only an explicit `"viewer"` claim ever
    /// narrows access.
    pub fn from_claim(claim: Option<&str>) -> Role {
        match claim {
            Some("viewer") => Role::Viewer,
            _ => Role::Operator,
        }
    }
}

/// The resolved, VERIFIED session identity for one request — populated only
/// from a signature+expiry-checked JWT (see [`session_from_cookie`]), never
/// from an unsigned cookie value.
#[derive(Debug, Clone)]
pub struct SessionSeam {
    pub username: String,
    /// CONST-27: the session's access tier, from the JWT's `role` claim
    /// (absent ⇒ [`Role::Operator`], see [`Role::from_claim`]).
    pub role: Role,
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
        // CONST-27: `role` lets the shell gate at render time (§3.4) — this is the
        // cosmetic/UI signal only, never the enforcement (see `enforce_viewer_role_gate`).
        Some(s) => json!({"authenticated": true, "username": s.username, "role": s.role.as_str()}),
        None => json!({"authenticated": false, "username": null, "role": null}),
    }
}

fn unauthorized_response() -> Response {
    let masked = crate::constellation::mask::mask_response(json!({"error": "unauthorized"}));
    (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// CONST-27: the structural 403 a viewer session gets on a mutating request
/// to a protected route — see [`enforce_viewer_role_gate`].
fn forbidden_response() -> Response {
    let masked = crate::constellation::mask::mask_response(
        json!({"error": "forbidden", "required_role": "operator"}),
    );
    (StatusCode::FORBIDDEN, [("content-type", "application/json")], masked.to_string()).into_response()
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
        Ok(claims) => Some(SessionSeam {
            username: claims.sub,
            // CONST-27: claim-absent (`None`) resolves to `Role::Operator` —
            // see `Role::from_claim`'s doc for why that's the correct
            // backward-compatible default.
            role: Role::from_claim(claims.role.as_deref()),
        }),
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

    // CONST-27 (§3.4): check the operator secret first, then the viewer
    // secret. Both are read fresh per-login (not cached) so an operator can
    // provision/rotate either one live. Neither being configured is the
    // pre-CONST-27 fail-closed posture generalized to two tiers -- an unset
    // tier's secret can never match ANY submitted password, so that tier
    // simply never succeeds a login, never a default-allow.
    let operator_secret = crate::config::constellation_operator_secret();
    let viewer_secret = crate::config::constellation_viewer_secret();

    // Edge case (spec §10 CONST-27): an operator who configures the SAME
    // value for both secrets doesn't get a viewer session ever -- the
    // operator check below runs first and wins on any match. Surfaced as a
    // warning (never blocks the login) so a misconfiguration is visible in
    // logs rather than silently doing something unexpected.
    if let (Some(op), Some(vw)) = (&operator_secret, &viewer_secret) {
        if op == vw {
            tracing::warn!(
                "constellation::auth: CONSTELLATION_OPERATOR_SECRET and CONSTELLATION_VIEWER_SECRET \
                 are configured to the same value -- the operator tier always wins on a match, so \
                 the viewer tier is effectively unreachable; provision a distinct viewer secret"
            );
        }
    }

    // Constant-time comparison (reusing `crate::pki::enroll`'s comparator --
    // the same one TCLI-02's enrollment shared-secret check uses) so a
    // timing side channel can't be used to guess either secret byte by byte.
    let role = if operator_secret
        .as_deref()
        .is_some_and(|s| crate::pki::enroll::constant_time_eq(parsed.password.as_bytes(), s.as_bytes()))
    {
        Some(Role::Operator)
    } else if viewer_secret
        .as_deref()
        .is_some_and(|s| crate::pki::enroll::constant_time_eq(parsed.password.as_bytes(), s.as_bytes()))
    {
        Some(Role::Viewer)
    } else {
        None
    };

    let Some(role) = role else {
        tracing::warn!(
            username = %parsed.username.trim(),
            "constellation::auth: login rejected -- invalid credential (or the matching tier's \
             secret is unset, fail-closed, not default-allow)"
        );
        return unauthorized_response();
    };

    let username = parsed.username.trim().to_string();
    let ttl = crate::config::constellation_session_ttl_seconds();
    let token = match crate::pki::enroll::mint_jwt_with_role(&username, ttl, Some(role.as_str())) {
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

    tracing::info!(username = %username, role = %role.as_str(), "constellation::auth: login succeeded");

    let seam = SessionSeam { username, role };
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

/// CONST-27 (§3.4): the ONE server-side enforcement point for the viewer
/// role. `axum` middleware layered (see
/// `crate::constellation::mod::protected_router`) INSIDE [`require_session`]
/// -- so an unauthenticated request is already rejected `401` by the time
/// this runs, and this only ever has to decide "operator vs. viewer", never
/// "no session at all" (though it degrades safely to that case too: a
/// request with no/invalid session simply isn't gated here at all and falls
/// through to `next`, where it either hits `require_session`'s `401` or, if
/// this is somehow layered without that guard, the real handler -- this
/// function's OWN contract is narrowly "deny a viewer's mutation", not
/// "enforce authentication", so it never invents a stricter response for an
/// absent session than whatever the rest of the stack already provides).
///
/// A viewer session making a mutating request (`POST`/`PUT`/`PATCH`/
/// `DELETE`) to anything this middleware wraps gets `403
/// {"error":"forbidden","required_role":"operator"}` -- the mutating
/// method never reaches the proxy/config handler, exactly like
/// `require_session` denies an unauthenticated request before any backend
/// dispatch. `GET`/`HEAD`/`OPTIONS` (and any other non-mutating method) pass
/// through regardless of role -- the viewer tier is read-only, not
/// no-access.
pub async fn enforce_viewer_role_gate(headers: HeaderMap, request: Request, next: Next) -> Response {
    let is_mutating = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );

    if is_mutating {
        if let Some(seam) = session_from_cookie(&headers) {
            if seam.role == Role::Viewer {
                return forbidden_response();
            }
        }
    }

    next.run(request).await
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
        std::env::remove_var("CONSTELLATION_VIEWER_SECRET");
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

    // ── CONST-27: viewer role ───────────────────────────────────────────────

    fn login_body(username: &str, password: &str) -> Bytes {
        Bytes::from(serde_json::to_vec(&json!({"username": username, "password": password})).unwrap())
    }

    #[tokio::test]
    #[serial]
    async fn login_with_operator_secret_yields_operator_role() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "op-secret"); // pii-test-fixture
        std::env::set_var("CONSTELLATION_VIEWER_SECRET", "view-secret"); // pii-test-fixture
        let resp = auth_login(HeaderMap::new(), login_body("carol", "op-secret")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["role"], "operator");
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_with_viewer_secret_yields_viewer_role() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "op-secret"); // pii-test-fixture
        std::env::set_var("CONSTELLATION_VIEWER_SECRET", "view-secret"); // pii-test-fixture
        let resp = auth_login(HeaderMap::new(), login_body("dave", "view-secret")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["role"], "viewer");
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_with_unset_viewer_secret_rejects_any_viewer_attempt() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "op-secret"); // pii-test-fixture
        // CONSTELLATION_VIEWER_SECRET deliberately left unset -- the viewer
        // tier must be fully disabled, not default-allow for any password.
        let resp = auth_login(HeaderMap::new(), login_body("erin", "anything-at-all")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_rejects_a_password_matching_neither_secret() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "op-secret"); // pii-test-fixture
        std::env::set_var("CONSTELLATION_VIEWER_SECRET", "view-secret"); // pii-test-fixture
        let resp = auth_login(HeaderMap::new(), login_body("frank", "neither-of-these")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn login_with_same_value_for_both_secrets_resolves_to_operator() {
        // Edge case (§10 CONST-27): operator and viewer secrets configured identically --
        // the operator check runs first and wins on any match, so the submitted password
        // that equals both never yields a viewer session.
        clear_env();
        set_jwt_key("test-signing-key-const27");
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "shared-secret"); // pii-test-fixture
        std::env::set_var("CONSTELLATION_VIEWER_SECRET", "shared-secret"); // pii-test-fixture
        let resp = auth_login(HeaderMap::new(), login_body("grace", "shared-secret")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["role"], "operator");
        clear_env();
    }

    #[test]
    #[serial]
    fn a_role_claim_absent_token_resolves_to_operator() {
        // Backward compat: a session token minted before CONST-27 (or any other caller of
        // `mint_jwt_with_ttl`, which always passes `role: None`) has no `role` claim at all --
        // it must decode as `Role::Operator`, not lock a live session out on deploy.
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("legacy-operator", 300).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        let seam = session_from_cookie(&headers).unwrap();
        assert_eq!(seam.role, Role::Operator);
        clear_env();
    }

    #[test]
    #[serial]
    fn an_explicit_viewer_role_claim_round_trips() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let (token, _exp) =
            crate::pki::enroll::mint_jwt_with_role("viewer-user", 300, Some("viewer")).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        let seam = session_from_cookie(&headers).unwrap();
        assert_eq!(seam.role, Role::Viewer);
        assert_eq!(seam.username, "viewer-user");
        clear_env();
    }

    #[test]
    #[serial]
    fn a_role_claim_tampered_by_resigning_with_a_different_key_is_rejected() {
        // The JWT signature covers the WHOLE claim set, including `role` -- resigning a
        // viewer token with a different key (simulating an attempt to forge/elevate a role
        // without the real signing key) must fail verification entirely, not silently
        // decode with the tampered role.
        clear_env();
        set_jwt_key("key-one-const27");
        let (token, _exp) =
            crate::pki::enroll::mint_jwt_with_role("someone", 300, Some("viewer")).unwrap();
        set_jwt_key("key-two-const27");
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        assert!(session_from_cookie(&headers).is_none());
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn auth_me_reports_role_for_an_authenticated_viewer_session() {
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let (token, _exp) =
            crate::pki::enroll::mint_jwt_with_role("viewer-user", 300, Some("viewer")).unwrap();
        let headers = headers_with_cookie(&format!("constellation_session={token}"));
        let resp = auth_me(headers).await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["authenticated"], true);
        assert_eq!(parsed["role"], "viewer");
        clear_env();
    }

    /// The load-bearing CONST-27 property: a viewer session's mutating request is rejected
    /// `403` with the documented shape, structurally (never reaches the wrapped handler).
    #[tokio::test]
    #[serial]
    async fn viewer_role_gate_denies_a_mutating_request() {
        use tower::ServiceExt;
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let router = axum::Router::new()
            .route("/protected", axum::routing::post(|| async { "mutated" }))
            .layer(axum::middleware::from_fn(enforce_viewer_role_gate));
        let (token, _exp) =
            crate::pki::enroll::mint_jwt_with_role("viewer-user", 300, Some("viewer")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/protected")
            .header("cookie", format!("constellation_session={token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["error"], "forbidden");
        assert_eq!(parsed["required_role"], "operator");
        clear_env();
    }

    /// Complements the denial test above: the SAME viewer session's `GET` passes through --
    /// the viewer tier is read-only, not no-access.
    #[tokio::test]
    #[serial]
    async fn viewer_role_gate_allows_a_get_request() {
        use tower::ServiceExt;
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let router = axum::Router::new()
            .route("/protected", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(enforce_viewer_role_gate));
        let (token, _exp) =
            crate::pki::enroll::mint_jwt_with_role("viewer-user", 300, Some("viewer")).unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/protected")
            .header("cookie", format!("constellation_session={token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        clear_env();
    }

    /// And an operator session's mutating request is never blocked by the gate.
    #[tokio::test]
    #[serial]
    async fn viewer_role_gate_allows_an_operator_mutating_request() {
        use tower::ServiceExt;
        clear_env();
        set_jwt_key("test-signing-key-const27");
        let router = axum::Router::new()
            .route("/protected", axum::routing::post(|| async { "mutated" }))
            .layer(axum::middleware::from_fn(enforce_viewer_role_gate));
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("operator-user", 300).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/protected")
            .header("cookie", format!("constellation_session={token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        clear_env();
    }
}
