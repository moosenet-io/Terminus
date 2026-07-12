//! CONST-02: the constellation aggregation layer's AUTH SEAM.
//!
//! **This module is intentionally NOT real authentication.** CONST-02's
//! scope is the aggregation API layer (proxy + mask + audit); the real
//! enroll/JWT-backed session model (`docs/architecture/auth.md`) is CONST-03's
//! scope. What lives here is the minimum SHAPE `constellation-web`'s
//! `aggregationClient.ts` httpAdapter needs to build/run against —
//! `GET /api/auth/me`, `POST /api/auth/login`, `POST /api/auth/logout` — plus
//! a request-extension seam (`SessionSeam`) so CONST-03 can drop in real
//! verification (signature check, expiry, revocation) without every
//! downstream handler changing its shape.
//!
//! ## What this stub actually does
//! - `login` accepts ANY non-empty `{username, password}` pair — there is
//!   deliberately no hardcoded credential check (that would itself violate
//!   the "never hardcode a credential" rule) and no real password
//!   verification (there is nothing yet to verify against — CONST-03 owns
//!   provisioning real operator accounts). It sets an **unsigned, plain**
//!   session cookie carrying the username. This is NOT a security boundary
//!   yet; treat every `/api/*` request as effectively unauthenticated until
//!   CONST-03 lands.
//! - `me` reports `authenticated: true` iff the cookie is present, echoing
//!   the username it carries.
//! - `logout` clears the cookie.
//!
//! CONST-03: replace `principal_from_cookie` + the cookie itself with a
//! verified JWT (`crate::pki`'s enrollment/JWT machinery already exists
//! elsewhere in this crate — see `docs/architecture/auth.md`), and make the
//! guard below actually DENY an unauthenticated mutating request instead of
//! just labeling it for audit. Nothing downstream of `SessionSeam` should
//! need to change shape when that lands. See the `// CONST-03:` markers at
//! each concrete plug-in point below.

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

const SESSION_COOKIE: &str = "constellation_session";

/// The resolved (stub) session identity for one request. CONST-03's real
/// verification replaces how this is constructed, not how it's consumed.
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

/// Extract a [`SessionSeam`] from the request's `Cookie` header, when
/// present. Best-effort, non-cryptographic parsing — this is the exact
/// stub this module's doc describes, not a verified session.
pub fn principal_from_cookie(headers: &HeaderMap) -> Option<String> {
    session_from_cookie(headers).map(|s| s.username)
}

// CONST-03: this whole function is the seam to replace -- swap the
// unsigned-cookie parse below for verifying a real JWT (signature, expiry,
// revocation) and resolving it to a `SessionSeam`. Every caller of this
// function (`principal_from_cookie`, `auth_me`, `auth_login`, `auth_logout`)
// keeps working unmodified once that swap lands.
fn session_from_cookie(headers: &HeaderMap) -> Option<SessionSeam> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            if !value.is_empty() {
                return Some(SessionSeam { username: value.to_string() });
            }
        }
    }
    None
}

fn set_cookie_header(username: &str) -> String {
    // HttpOnly + SameSite=Lax (no `Secure` flag hardcoded — a LAN-served
    // dev/operator UI may run over plain HTTP; CONST-03 should add `Secure`
    // when it's served over TLS). Session-lifetime cookie (no `Max-Age`) —
    // matches this being an unsigned stub, not a durable credential.
    format!("{SESSION_COOKIE}={username}; Path=/; HttpOnly; SameSite=Lax")
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
    // anything is written to the audit log.
    crate::constellation::audit::record_mutating_request(
        "auth",
        "POST",
        request_path,
        None,
        &crate::constellation::audit::body_text(&body),
    );
    let _ = &headers; // reserved for CONST-03 (e.g. rate-limit by caller)

    let parsed: LoginRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => LoginRequest { username: String::new(), password: String::new() },
    };

    // CONST-03: this only checks for NON-EMPTY credentials, never verifies
    // the password against a real operator account (none is provisioned
    // yet). Replace this branch with real credential verification.
    if parsed.username.trim().is_empty() || parsed.password.is_empty() {
        let masked = crate::constellation::mask::mask_response(auth_me_body(None));
        return (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], masked.to_string())
            .into_response();
    }

    let seam = SessionSeam { username: parsed.username.trim().to_string() };
    let masked = crate::constellation::mask::mask_response(auth_me_body(Some(&seam)));
    let mut resp = (StatusCode::OK, [("content-type", "application/json")], masked.to_string())
        .into_response();
    if let Ok(hv) = axum::http::HeaderValue::from_str(&set_cookie_header(&seam.username)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_cookie(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn no_cookie_means_no_session() {
        assert!(session_from_cookie(&HeaderMap::new()).is_none());
    }

    #[test]
    fn parses_session_cookie_among_others() {
        let headers = headers_with_cookie("foo=bar; constellation_session=alice; other=1");
        let seam = session_from_cookie(&headers).unwrap();
        assert_eq!(seam.username, "alice");
    }

    #[test]
    fn empty_cookie_value_is_no_session() {
        let headers = headers_with_cookie("constellation_session=");
        assert!(session_from_cookie(&headers).is_none());
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
    async fn auth_me_reports_authenticated_with_session_cookie() {
        let headers = headers_with_cookie("constellation_session=bob");
        let resp = auth_me(headers).await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["authenticated"], true);
        assert_eq!(parsed["username"], "bob");
    }

    #[tokio::test]
    async fn login_rejects_empty_credentials() {
        let resp = auth_login(HeaderMap::new(), Bytes::from_static(b"{}")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_accepts_nonempty_credentials_and_sets_cookie() {
        let body = Bytes::from(serde_json::to_vec(&json!({"username": "carol", "password": "x"})).unwrap());
        let resp = auth_login(HeaderMap::new(), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp.headers().get(axum::http::header::SET_COOKIE).unwrap();
        assert!(set_cookie.to_str().unwrap().contains("carol"));
    }

    #[tokio::test]
    async fn logout_clears_cookie() {
        let resp = auth_logout(HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let set_cookie = resp.headers().get(axum::http::header::SET_COOKIE).unwrap();
        assert!(set_cookie.to_str().unwrap().contains("Max-Age=0"));
    }
}
