//! CONST-02: namespaced backend proxy handlers for the constellation
//! aggregation layer.
//!
//! `constellation-web`'s `aggregationClient.ts` httpAdapter talks to exactly
//! three namespaced passthrough routes — `/api/harmony/*path`,
//! `/api/chord/*path`, `/api/lumina/*path` — plus its own typed
//! `/api/auth/*`, `/api/health`, `/api/terminus/config` endpoints (handled
//! in `crate::constellation` directly, not here). Each namespaced route
//! forwards method + sub-path + body to that backend's configured base URL
//! (`crate::config::constellation_{harmony,chord,lumina}_url`) via
//! `reqwest`.
//!
//! ## The single-door property
//! This module is deliberately the ONLY place in this crate (and, per the
//! CONST-02 spec, the only path in the whole constellation) that forwards a
//! caller's request to Harmony/Chord/Lumina's own HTTP APIs — no other
//! module should grow a second ad-hoc client for one of these three
//! backends. Mirrors S9's single-access-path principle, applied to these
//! backend surfaces instead of GitHub/Gitea/Plane.
//!
//! ## Graceful backend degradation
//! A backend that is unconfigured, unreachable, or too slow (bounded by
//! `crate::config::constellation_backend_timeout_ms`) does NOT cascade into
//! a `500` for the caller. It returns a `200 OK` with a structured
//! `{"system": <s>, "available": false, "detail": <reason>}` body — the
//! SAME shape `crate::constellation::health` reports per-system — so a
//! sibling system's panel in the UI keeps working even while one backend is
//! down. This is deliberately `200`, not `502`/`503`: from the aggregation
//! layer's own perspective nothing failed — it correctly determined and
//! reported that the requested backend isn't reachable, which is a
//! successful outcome of the proxy's own contract, not an error in it.
//!
//! ## Masking + audit
//! Every response (proxied success, or this module's own degraded-backend
//! JSON) is passed through [`crate::constellation::mask::mask_response`]
//! before being returned — see that module's doc for why this is the
//! load-bearing security property of the whole aggregation layer. Every
//! mutating request (`POST`/`PUT`/`PATCH`/`DELETE`) is recorded via
//! [`crate::constellation::audit::record_mutating_request`] before the
//! backend call is attempted.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::config;
use crate::constellation::{audit, mask};
use crate::mcp_server::McpServerState;

/// The shared `reqwest::Client` for every outbound aggregation-layer HTTP
/// call — the proxy handlers below AND `crate::constellation::probe_system`
/// (the `/api/health` reachability probe) both reuse this ONE client rather
/// than constructing a fresh one per request, so connection pooling works
/// across both call sites.
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Build the degraded-backend JSON body this module returns whenever the
/// target backend is unconfigured, unreachable, or timed out — the exact
/// shape `crate::constellation::health`'s per-entry `HealthStatus` also
/// uses, so a caller can treat "a proxied call degraded" and "a health
/// check reported this system down" identically.
fn unavailable_body(system: &str, detail: impl Into<String>) -> Value {
    json!({"system": system, "available": false, "detail": detail.into()})
}

fn respond(status: StatusCode, body: Value) -> Response {
    let masked = mask::mask_response(body);
    (status, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// Shared implementation for all three namespaced proxy routes. `system` is
/// the fixed literal namespace (`"harmony"`/`"chord"`/`"lumina"`);
/// `base_url` is that system's configured backend base URL (`None` ⇒
/// unconfigured); `sub_path` is everything after `/api/{system}/` (no
/// leading slash, per axum's `*path` wildcard extraction).
async fn proxy(
    system: &'static str,
    base_url: Option<String>,
    sub_path: &str,
    method: Method,
    headers: &HeaderMap,
    body: Bytes,
    principal: Option<&str>,
) -> Response {
    let request_path = format!("/api/{system}/{sub_path}");

    if audit::is_mutating_method(method.as_str()) {
        audit::record_mutating_request(
            system,
            method.as_str(),
            &request_path,
            principal,
            &audit::body_text(&body),
        );
    }

    let Some(base) = base_url else {
        return respond(
            StatusCode::OK,
            unavailable_body(system, format!("{system} backend not configured")),
        );
    };

    let target = format!("{}/{}", base.trim_end_matches('/'), sub_path.trim_start_matches('/'));
    let timeout = Duration::from_millis(config::constellation_backend_timeout_ms());

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let result = http_client()
        .request(method.clone(), &target)
        .timeout(timeout)
        .header("content-type", content_type)
        .body(body.to_vec())
        .send()
        .await;

    match result {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            let bytes = match upstream_resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return respond(
                        StatusCode::OK,
                        unavailable_body(system, format!("error reading {system} response: {e}")),
                    )
                }
            };
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(parsed) => respond(
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
                    parsed,
                ),
                // Non-JSON (or empty) upstream body — wrap it as text rather
                // than fail; still passed through masking (a non-JSON body
                // can't be walked field-by-field, but `mask_response` is a
                // no-op on a bare string leaf that isn't itself
                // secret-shaped, and this wraps it as one JSON string leaf).
                Err(_) => respond(
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
                    json!({ "raw": String::from_utf8_lossy(&bytes) }),
                ),
            }
        }
        Err(e) if e.is_timeout() => respond(
            StatusCode::OK,
            unavailable_body(system, format!("{system} backend timed out")),
        ),
        Err(e) => respond(
            StatusCode::OK,
            unavailable_body(system, format!("{system} backend unreachable: {e}")),
        ),
    }
}

/// `principal` extraction seam: the CONST-02 auth guard (see
/// `crate::constellation::auth`) inserts a resolved session identity into
/// request extensions when present; proxy handlers read it (if any) purely
/// for audit attribution — access control enforcement itself is CONST-03's
/// scope, not this module's.
fn principal_from_headers(headers: &HeaderMap) -> Option<String> {
    // CONST-03: replace this cookie-name sniff with the real verified
    // session/JWT principal once the auth seam is implemented. For now this
    // is a best-effort audit label only — see `crate::constellation::auth`.
    crate::constellation::auth::principal_from_cookie(headers)
}

pub async fn proxy_harmony(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "harmony",
        config::constellation_harmony_url(),
        &path,
        method,
        &headers,
        body,
        principal.as_deref(),
    )
    .await
}

pub async fn proxy_chord(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "chord",
        config::constellation_chord_url(),
        &path,
        method,
        &headers,
        body,
        principal.as_deref(),
    )
    .await
}

pub async fn proxy_lumina(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "lumina",
        config::constellation_lumina_url(),
        &path,
        method,
        &headers,
        body,
        principal.as_deref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use serial_test::serial;

    #[tokio::test]
    async fn unconfigured_backend_returns_structured_unavailable_not_5xx() {
        let resp = proxy(
            "harmony",
            None,
            "status",
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "harmony");
        assert_eq!(parsed["available"], false);
        assert!(parsed["detail"].as_str().unwrap().contains("not configured"));
    }

    #[tokio::test]
    #[serial]
    async fn unreachable_backend_degrades_cleanly() {
        // A base URL pointed at a closed local port -- connection refused,
        // never a 500 cascade to the caller.
        let resp = proxy(
            "chord",
            Some("http://127.0.0.1:1".to_string()), // pii-test-fixture
            "health",
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "chord");
        assert_eq!(parsed["available"], false);
    }

    #[test]
    fn unavailable_body_shape_matches_health_status() {
        let v = unavailable_body("lumina", "test reason");
        assert_eq!(v["system"], "lumina");
        assert_eq!(v["available"], false);
        assert_eq!(v["detail"], "test reason");
    }
}
