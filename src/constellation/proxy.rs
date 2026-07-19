//! CONST-02: namespaced backend proxy handlers for the constellation
//! aggregation layer.
//!
//! `constellation-web`'s `aggregationClient.ts` httpAdapter talks to exactly
//! four namespaced passthrough routes — `/api/harmony/*path`,
//! `/api/chord/*path`, `/api/lumina/*path`, `/api/muse/*path` (CONST-19) —
//! plus its own typed `/api/auth/*`, `/api/health`, `/api/terminus/config`
//! endpoints (handled in `crate::constellation` directly, not here). Each
//! namespaced route forwards method + sub-path + body to that backend's
//! configured base URL
//! (`crate::config::constellation_{harmony,chord,lumina,muse}_url`) via
//! `reqwest`.
//!
//! ## Muse `/art/...` binary passthrough (CONST-19)
//! Muse's `/art/:kind/:id` routes serve poster/art images, not JSON — the
//! generic `proxy()` implementation below always attempts a JSON parse of
//! the upstream body (falling back to a `{"raw": ...}` text wrapper), which
//! would corrupt binary bytes and lose the real image content-type.
//! [`proxy_muse`] special-cases any sub-path starting with `art/` and routes
//! it to [`proxy_muse_art`] instead: the upstream body is forwarded
//! byte-for-byte with the upstream's own `content-type`, skipping both the
//! JSON parse and [`mask::mask_response`] (masking only ever walks JSON
//! values — running it over an arbitrary binary blob is a no-op at best and
//! a correctness risk at worst, so binary bodies deliberately never enter
//! that path). Degraded/unconfigured/unreachable cases still return the
//! standard JSON `{"system","available":false,"detail"}` shape (there's no
//! image to serve to corrupt in those cases anyway).
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
use axum::extract::{Path, RawQuery, State};
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

/// Build the upstream target URL from the backend base, the wildcard sub-path,
/// and the (optional, raw) query string. The query string MUST be preserved
/// when forwarding — dropping it was an agy CONST-02 review finding
/// (e.g. `/api/harmony/tree/LUM?depth=2` lost its `?depth=2`).
fn build_target(base: &str, sub_path: &str, query: Option<&str>) -> String {
    let path_target = format!("{}/{}", base.trim_end_matches('/'), sub_path.trim_start_matches('/'));
    match query {
        Some(q) if !q.is_empty() => format!("{path_target}?{q}"),
        _ => path_target,
    }
}

/// Shared implementation for all four namespaced proxy routes (the three
/// JSON-oriented arms plus `proxy_muse`'s non-`art/` sub-paths). `system` is
/// the fixed literal namespace (`"harmony"`/`"chord"`/`"lumina"`/`"muse"`);
/// `base_url` is that system's configured backend base URL (`None` ⇒
/// unconfigured); `sub_path` is everything after `/api/{system}/` (no
/// leading slash, per axum's `*path` wildcard extraction).
async fn proxy(
    system: &'static str,
    base_url: Option<String>,
    sub_path: &str,
    query: Option<&str>,
    method: Method,
    headers: &HeaderMap,
    body: Bytes,
    principal: Option<&str>,
) -> Response {
    // Audit path deliberately excludes the query string: query params can
    // themselves carry secret-shaped values, and unlike the body they are not
    // run through the sanitizer, so keeping them out of the audit record avoids
    // an unsanitized-secret-in-query leak. The query IS forwarded upstream.
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

    let target = build_target(&base, sub_path, query);
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

/// `principal` extraction: reads the caller's VERIFIED session identity
/// (CONST-03 — `crate::constellation::auth::principal_from_cookie` verifies
/// the cookie's JWT signature + expiry, not just the cookie's raw value) for
/// audit attribution. Access control enforcement itself is
/// `crate::constellation::auth::require_session`'s job (layered as `axum`
/// middleware over these routes in `crate::constellation::mod`'s
/// `protected_router`) — by the time a proxy handler runs at all, the guard
/// has already confirmed a valid session exists, so this call always
/// resolves to `Some` in practice for these routes; it stays a plain
/// best-effort lookup rather than re-deriving that guarantee, since a
/// second, possibly-divergent verification here would add nothing.
fn principal_from_headers(headers: &HeaderMap) -> Option<String> {
    crate::constellation::auth::principal_from_cookie(headers)
}

pub async fn proxy_harmony(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "harmony",
        config::constellation_harmony_url(),
        &path,
        query.as_deref(),
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
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "chord",
        config::constellation_chord_url(),
        &path,
        query.as_deref(),
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
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    proxy(
        "lumina",
        config::constellation_lumina_url(),
        &path,
        query.as_deref(),
        method,
        &headers,
        body,
        principal.as_deref(),
    )
    .await
}

/// `/api/muse/*path` (CONST-19) — the fourth namespaced proxy arm, otherwise
/// identical single-door/masking/audit/degradation semantics to
/// [`proxy_harmony`]/[`proxy_chord`]/[`proxy_lumina`] above. The one
/// deliberate difference: a sub-path under `art/` (Muse's poster/art image
/// routes) is routed to [`proxy_muse_art`] for raw binary passthrough
/// instead of the JSON-oriented [`proxy`] — see this module's doc.
pub async fn proxy_muse(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    if path.starts_with("art/") {
        return proxy_muse_art(&path, query.as_deref(), method, &headers, body, principal.as_deref()).await;
    }
    proxy(
        "muse",
        config::constellation_muse_url(),
        &path,
        query.as_deref(),
        method,
        &headers,
        body,
        principal.as_deref(),
    )
    .await
}

/// Raw binary passthrough for Muse's `/art/:kind/:id` poster/art routes (see
/// this module's doc). Same audit + degradation semantics as [`proxy`], but
/// on a successful upstream response the body is forwarded byte-for-byte
/// with the UPSTREAM's own content-type — no JSON parse attempt, no
/// [`mask::mask_response`] pass (see the module doc for why skipping masking
/// here is correct, not a gap).
async fn proxy_muse_art(
    sub_path: &str,
    query: Option<&str>,
    method: Method,
    headers: &HeaderMap,
    body: Bytes,
    principal: Option<&str>,
) -> Response {
    const SYSTEM: &str = "muse";
    let request_path = format!("/api/{SYSTEM}/{sub_path}");

    if audit::is_mutating_method(method.as_str()) {
        audit::record_mutating_request(
            SYSTEM,
            method.as_str(),
            &request_path,
            principal,
            &audit::body_text(&body),
        );
    }

    let Some(base) = config::constellation_muse_url() else {
        return respond(
            StatusCode::OK,
            unavailable_body(SYSTEM, format!("{SYSTEM} backend not configured")),
        );
    };

    let target = build_target(&base, sub_path, query);
    let timeout = Duration::from_millis(config::constellation_backend_timeout_ms());

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
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
            let status = StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::OK);
            // Forward the UPSTREAM content-type verbatim (defaulting only if
            // the upstream itself omitted one) -- this is the whole point of
            // this arm: an art response's real image content-type must
            // survive, not be overwritten with "application/json" like the
            // generic `respond()` helper does.
            let resp_content_type = upstream_resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            match upstream_resp.bytes().await {
                Ok(bytes) => (status, [("content-type", resp_content_type)], bytes.to_vec()).into_response(),
                Err(e) => respond(
                    StatusCode::OK,
                    unavailable_body(SYSTEM, format!("error reading {SYSTEM} art response: {e}")),
                ),
            }
        }
        Err(e) if e.is_timeout() => respond(
            StatusCode::OK,
            unavailable_body(SYSTEM, format!("{SYSTEM} backend timed out")),
        ),
        Err(e) => respond(
            StatusCode::OK,
            unavailable_body(SYSTEM, format!("{SYSTEM} backend unreachable: {e}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn unconfigured_backend_returns_structured_unavailable_not_5xx() {
        let resp = proxy(
            "harmony",
            None,
            "status",
            None,
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
            None,
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
    fn build_target_preserves_query_string() {
        // Regression (agy CONST-02 review): the query string must survive
        // forwarding, base trailing-slash and sub-path leading-slash normalize,
        // and an empty/absent query adds no bare `?`.
        assert_eq!(
            build_target("http://h/", "tree/LUM", Some("depth=2&x=1")),
            "http://h/tree/LUM?depth=2&x=1"
        );
        assert_eq!(build_target("http://h", "status", None), "http://h/status");
        assert_eq!(build_target("http://h", "status", Some("")), "http://h/status");
    }

    #[test]
    fn unavailable_body_shape_matches_health_status() {
        let v = unavailable_body("lumina", "test reason");
        assert_eq!(v["system"], "lumina");
        assert_eq!(v["available"], false);
        assert_eq!(v["detail"], "test reason");
    }

    // ── CONST-19: Muse proxy arm ─────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn muse_unconfigured_backend_returns_structured_unavailable_not_5xx() {
        // Mirrors `unconfigured_backend_returns_structured_unavailable_not_5xx`
        // above, exercised through the shared `proxy()` fn with "muse" as the
        // system literal (same as `proxy_muse` does for non-`art/` sub-paths).
        let resp = proxy(
            "muse",
            None,
            "on_deck",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "muse");
        assert_eq!(parsed["available"], false);
        assert!(parsed["detail"].as_str().unwrap().contains("not configured"));
    }

    #[tokio::test]
    #[serial]
    async fn muse_unreachable_backend_degrades_cleanly() {
        let resp = proxy(
            "muse",
            Some("http://127.0.0.1:1".to_string()), // pii-test-fixture
            "on_deck",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "muse");
        assert_eq!(parsed["available"], false);
    }

    #[tokio::test]
    #[serial]
    async fn muse_art_unconfigured_backend_returns_structured_unavailable() {
        // `proxy_muse_art` (the raw-passthrough arm) has its own unconfigured
        // path -- must degrade the same way as the JSON arm, not panic or
        // 5xx just because there's no image to serve.
        let resp = proxy_muse_art(
            "art/poster/123",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
        assert!(content_type.contains("json"));
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "muse");
        assert_eq!(parsed["available"], false);
    }

    #[tokio::test]
    #[serial]
    async fn muse_art_unreachable_backend_degrades_cleanly_not_a_panic() {
        // Regression for the CONST-19 edge case: a non-JSON (or, here,
        // unreachable-so-no-body-at-all) response through the art passthrough
        // arm must never panic -- it degrades exactly like the JSON arm when
        // there's no upstream to read bytes from at all.
        let resp = proxy_muse_art(
            "art/poster/123",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            Some("test-operator"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "muse");
        assert_eq!(parsed["available"], false);
    }

    #[test]
    fn proxy_muse_routes_art_subpaths_to_the_binary_passthrough_arm() {
        // A structural check that the dispatch rule in `proxy_muse` (this
        // module's doc) is what it claims: any sub-path prefixed `art/` is
        // treated as a binary-art request, everything else is not.
        assert!("art/poster/abc123".starts_with("art/"));
        assert!(!"api/graph/taste-clusters".starts_with("art/"));
        assert!(!"on_deck".starts_with("art/"));
    }

    /// POSITIVE binary passthrough (review follow-up on the CONST-19 PR):
    /// the earlier art tests only covered unconfigured/unreachable
    /// degradation, never a successful byte-for-byte forward. This spins up
    /// a real ephemeral HTTP server (same pattern as
    /// `pki::server::build_gateway_router_merges_mcp_and_enroll_routes`)
    /// that answers with fixed non-JSON bytes (a PNG magic-number prefix
    /// plus a JSON-secret-shaped byte sequence embedded in the "body", to
    /// prove masking never touches it) and an `image/png` content-type, then
    /// asserts `proxy_muse_art` forwards the bytes EXACTLY and preserves the
    /// upstream's own content-type rather than the JSON arm's hardcoded
    /// `application/json`.
    #[tokio::test]
    #[serial]
    async fn muse_art_success_forwards_bytes_and_content_type_without_masking() {
        // A byte sequence that is (a) not valid UTF-8/JSON at all (the PNG
        // magic number) and (b) contains a secret-shaped substring
        // (`"api_key":"should-never-be-redacted"`) that mask_response WOULD
        // alter if this path ever ran the body through JSON parse + masking.
        let mut art_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        art_bytes.extend_from_slice(br#"{"api_key":"should-never-be-redacted"}"#);
        let expected_bytes = art_bytes.clone();

        async fn serve_art(bytes: Vec<u8>) -> Response {
            (StatusCode::OK, [("content-type", "image/png")], bytes).into_response()
        }
        let art_bytes_for_handler = art_bytes.clone();
        let app = axum::Router::new().route(
            "/art/poster/123",
            axum::routing::get(move || serve_art(art_bytes_for_handler.clone())),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        std::env::set_var("CONSTELLATION_MUSE_URL", format!("http://{addr}")); // pii-test-fixture

        let resp = proxy_muse_art(
            "art/poster/123",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
        assert_eq!(content_type, "image/png", "upstream content-type must be preserved verbatim");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body.as_ref(),
            expected_bytes.as_slice(),
            "art bytes must be forwarded byte-for-byte, unmodified by masking"
        );

        std::env::remove_var("CONSTELLATION_MUSE_URL");
    }

    /// Muse-specific mutating-POST audit test (review follow-up), mirroring
    /// `audit::record_mutating_request_appends_a_jsonl_line`'s pattern but
    /// exercised through this module's own `proxy()` call for the `muse`
    /// namespace — proving `proxy_muse`'s audit call site (not just the
    /// generic `audit` module in isolation) actually fires for a mutating
    /// request under `/api/muse/*`.
    #[tokio::test]
    #[serial]
    async fn muse_mutating_post_is_audited() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);

        // No backend configured -- the request still degrades cleanly (200
        // available:false), but the audit call happens BEFORE the backend
        // dispatch (see `proxy()`'s doc), so it must be recorded regardless.
        let resp = proxy(
            "muse",
            None,
            "api/channels",
            None,
            Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"name":"new channel"}"#),
            Some("test-operator"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let contents = std::fs::read_to_string(&path).unwrap();
        let line = contents.lines().next().expect("expected an audit line to be written");
        let parsed: Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["system"], "muse");
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["path"], "/api/muse/api/channels");
        assert_eq!(parsed["principal"], "test-operator");

        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
    }
}
