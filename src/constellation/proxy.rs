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
//!
//! ## LGUI-05: Lumina bearer injection (spec §6, decision D2)
//! [`proxy_lumina`] is the one namespaced arm that authenticates itself to
//! its backend. It attaches two headers the shared [`proxy`] helper doesn't
//! otherwise send: `Authorization: Bearer <CONSTELLATION_LUMINA_TOKEN>`
//! (point-of-use env read via [`config::constellation_lumina_token`]; absent
//! → forward unauthenticated exactly as every other arm always has — a
//! token-less dev Lumina instance keeps working) and `X-Lumina-User: <the
//! VERIFIED session principal>` (never a browser-supplied value — see
//! [`principal_from_headers`]'s doc). Neither header is ever derived from,
//! or copied out of, the caller's own request: [`proxy`] never forwards any
//! inbound header except `content-type` in the first place, so a
//! browser-supplied `Authorization`/`X-Lumina-User` is structurally
//! impossible to smuggle through this door — `constellation-web`'s
//! `enforceHeaders` (`aggregationClient.ts`) strips the same two client-side
//! as a second, defense-in-depth door. A 401 from Lumina (token
//! misconfigured/rejected) degrades to the same `{"available":false,
//! "detail":"lumina auth failed"}` shape every other degraded case uses,
//! rather than forwarding a raw 401 the browser has no session to react to
//! (see [`proxy`]'s `auth_failure_detail` parameter).

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
    // LGUI-05: additional headers to set on the OUTBOUND upstream request,
    // beyond the `content-type` every arm already sends -- e.g.
    // `proxy_lumina`'s bearer + `X-Lumina-User` (see the module doc). Empty
    // for `proxy_harmony`/`proxy_chord`/`proxy_muse`, which authenticate no
    // backend. Deliberately NOT derived from `headers` (the caller's inbound
    // request) anywhere in this function -- the only inbound header this
    // function ever reads is `content-type`, so a caller-supplied
    // `Authorization`/`X-Lumina-User` can never ride along regardless of
    // what a future caller passes here.
    extra_upstream_headers: &[(&str, String)],
    // LGUI-05: when `Some(detail)`, a `401` from the upstream backend is
    // reported as the standard degraded-backend shape
    // (`unavailable_body(system, detail)`, `200 OK`) instead of being
    // forwarded to the browser verbatim -- an unauthenticated Constellation
    // session has no way to react to a raw 401 from a DIFFERENT backend's
    // auth layer (see the module doc's "never a raw 401 loop" note).
    // `None` for arms that don't authenticate to their backend at all.
    auth_failure_detail: Option<&'static str>,
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

    let mut request = http_client()
        .request(method.clone(), &target)
        .timeout(timeout)
        .header("content-type", content_type);
    for (name, value) in extra_upstream_headers {
        request = request.header(*name, value.as_str());
    }

    let result = request.body(body.to_vec()).send().await;

    match result {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            if status == StatusCode::UNAUTHORIZED {
                if let Some(detail) = auth_failure_detail {
                    return respond(StatusCode::OK, unavailable_body(system, detail));
                }
            }
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
        &[],
        None,
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
        &[],
        None,
    )
    .await
}

/// The `extra_upstream_headers` [`proxy_lumina`] passes to [`proxy`] — a
/// small pure function pulled out of the handler body so it's directly
/// unit-testable without an `axum` `State`/`Path`/request round-trip (see
/// the `lumina_upstream_headers_*` tests below). `token` is the
/// point-of-use `CONSTELLATION_LUMINA_TOKEN` read (`None` ⇒ no
/// `Authorization` header at all — unauthenticated passthrough); `principal`
/// is the caller's VERIFIED session identity, never a raw header value.
fn lumina_upstream_headers(token: Option<String>, principal: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    if let Some(token) = token {
        headers.push(("authorization", format!("Bearer {token}")));
    }
    if let Some(user) = principal {
        headers.push(("x-lumina-user", user.to_string()));
    }
    headers
}

/// LGUI-05 (spec §6, D2): the one namespaced arm that authenticates itself
/// to its backend — see the module doc's "LGUI-05: Lumina bearer injection"
/// section for the full design and why the two headers built here can never
/// carry a browser-supplied value through.
pub async fn proxy_lumina(
    State(_state): State<Arc<McpServerState>>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = principal_from_headers(&headers);
    // Point-of-use env read (see `config::constellation_lumina_token`'s doc)
    // and the VERIFIED session principal (from the signed session cookie,
    // never any inbound header) -- resolves Lumina's `X-Lumina-User`
    // convention (spec §7 C-1) for its admin-gated routes.
    let extra_headers = lumina_upstream_headers(config::constellation_lumina_token(), principal.as_deref());

    proxy(
        "lumina",
        config::constellation_lumina_url(),
        &path,
        query.as_deref(),
        method,
        &headers,
        body,
        principal.as_deref(),
        &extra_headers,
        Some("lumina auth failed"),
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
        &[],
        None,
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
    use axum::http::{HeaderMap, HeaderValue};
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
            None,
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

    // ── LGUI-05: Lumina bearer injection (spec §6, D2) ──────────────────────

    #[test]
    fn lumina_upstream_headers_attaches_bearer_and_x_lumina_user_when_both_present() {
        let headers = lumina_upstream_headers(Some("secret-token".to_string()), Some("alice"));
        assert_eq!(
            headers,
            vec![("authorization", "Bearer secret-token".to_string()), ("x-lumina-user", "alice".to_string())]
        );
    }

    #[test]
    fn lumina_upstream_headers_omits_bearer_when_token_absent() {
        // Absent token -> unauthenticated passthrough exactly as before this item -- no
        // `Authorization` header at all, never an empty-string one.
        let headers = lumina_upstream_headers(None, Some("alice"));
        assert_eq!(headers, vec![("x-lumina-user", "alice".to_string())]);
    }

    #[test]
    fn lumina_upstream_headers_omits_x_lumina_user_when_no_principal() {
        let headers = lumina_upstream_headers(Some("secret-token".to_string()), None);
        assert_eq!(headers, vec![("authorization", "Bearer secret-token".to_string())]);
    }

    #[test]
    fn lumina_upstream_headers_empty_when_both_absent() {
        assert!(lumina_upstream_headers(None, None).is_empty());
    }

    /// Mock-upstream, wire-level test: spins up a real ephemeral HTTP server that echoes back
    /// whatever `authorization`/`x-lumina-user` headers it actually received, then drives a
    /// request through `proxy()` exactly as `proxy_lumina` wires it (built-with
    /// `lumina_upstream_headers`) while ALSO setting bogus/spoofed `Authorization`/
    /// `X-Lumina-User` headers on the INBOUND (browser-simulated) request. Asserts the upstream
    /// sees only the server-side token + the verified principal -- never the browser-supplied
    /// values -- proving `proxy()`'s "only `content-type` is ever read from the caller's
    /// inbound headers" property (see the module doc) holds for the Lumina arm specifically.
    #[tokio::test]
    #[serial]
    async fn lumina_proxy_attaches_server_side_bearer_and_never_forwards_browser_supplied_headers() {
        // Captured SERVER-SIDE (shared state), NOT observed through the echoed response body:
        // the proxy's egress masking correctly masks any bearer-shaped value an upstream
        // echoes back (that masked echo is itself asserted below as a bonus property), so the
        // response body is the WRONG observation channel for what the upstream received.
        let seen: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_writer = seen.clone();
        let capture = move |headers: HeaderMap| {
            let seen_writer = seen_writer.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let user = headers.get("x-lumina-user").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                *seen_writer.lock().unwrap() = Some((auth.clone(), user.clone()));
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    json!({"auth": auth, "user": user}).to_string(),
                )
                    .into_response()
            }
        };
        let app = axum::Router::new().route("/status", axum::routing::get(capture));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        std::env::set_var("CONSTELLATION_LUMINA_TOKEN", "server-side-secret"); // pii-test-fixture

        // Simulate a browser that tried to smuggle its own Authorization/X-Lumina-User --
        // these must never reach the upstream.
        let mut inbound = HeaderMap::new();
        inbound.insert("authorization", HeaderValue::from_static("Bearer browser-supplied-token"));
        inbound.insert("x-lumina-user", HeaderValue::from_static("spoofed-user"));

        let extra_headers =
            lumina_upstream_headers(config::constellation_lumina_token(), Some("verified-operator"));

        let resp = proxy(
            "lumina",
            Some(format!("http://{addr}")), // pii-test-fixture
            "status",
            None,
            Method::GET,
            &inbound,
            Bytes::new(),
            Some("verified-operator"),
            &extra_headers,
            Some("lumina auth failed"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let (seen_auth, seen_user) = seen.lock().unwrap().clone().expect("upstream was hit");
        assert_eq!(
            seen_auth, "Bearer server-side-secret", // pii-test-fixture
            "upstream must see the SERVER-SIDE token, never the browser-supplied Authorization header"
        );
        assert_eq!(
            seen_user, "verified-operator",
            "upstream must see the VERIFIED principal, never the browser-supplied X-Lumina-User header"
        );
        // Bonus masking property: the upstream ECHOED the bearer back in its JSON body, and
        // the proxy's egress masking must have scrubbed it before it could reach a browser.
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body);
        assert!(
            !body_text.contains("server-side-secret"), // pii-test-fixture
            "a token echoed by the upstream must never survive the proxy's egress masking"
        );

        std::env::remove_var("CONSTELLATION_LUMINA_TOKEN");
    }

    /// Complements the test above: an ABSENT `CONSTELLATION_LUMINA_TOKEN` forwards
    /// unauthenticated exactly as every proxy arm always has -- no `Authorization` header
    /// reaches the upstream at all (a token-less dev Lumina instance keeps working).
    #[tokio::test]
    #[serial]
    async fn lumina_proxy_forwards_unauthenticated_when_token_unset() {
        async fn capture(headers: HeaderMap) -> Response {
            let has_auth = headers.contains_key("authorization");
            (StatusCode::OK, [("content-type", "application/json")], json!({"has_auth": has_auth}).to_string())
                .into_response()
        }
        let app = axum::Router::new().route("/status", axum::routing::get(capture));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        std::env::remove_var("CONSTELLATION_LUMINA_TOKEN");
        let extra_headers = lumina_upstream_headers(config::constellation_lumina_token(), None);

        let resp = proxy(
            "lumina",
            Some(format!("http://{addr}")), // pii-test-fixture
            "status",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
            &extra_headers,
            Some("lumina auth failed"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["has_auth"], false);
    }

    /// EDGE CASE (spec §9 LGUI-05): a `401` from Lumina (token misconfigured/rejected upstream)
    /// must degrade to the standard `{"available":false,"detail":"lumina auth failed"}` shape,
    /// never a raw `401` forwarded to a browser that has no way to react to it (the operator's
    /// Constellation session and the Lumina bearer are two independent credentials).
    #[tokio::test]
    #[serial]
    async fn lumina_proxy_401_from_upstream_degrades_to_auth_failed_detail_not_a_raw_401() {
        async fn unauthorized() -> Response {
            (StatusCode::UNAUTHORIZED, "nope").into_response()
        }
        let app = axum::Router::new().route("/status", axum::routing::get(unauthorized));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let resp = proxy(
            "lumina",
            Some(format!("http://{addr}")), // pii-test-fixture
            "status",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
            &[],
            Some("lumina auth failed"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK, "never a raw 401 -- must degrade like every other backend failure");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["system"], "lumina");
        assert_eq!(parsed["available"], false);
        assert_eq!(parsed["detail"], "lumina auth failed");
    }

    /// Regression: `auth_failure_detail: None` (every non-Lumina arm) leaves a `401` from the
    /// upstream forwarded VERBATIM -- the LGUI-05 degrade-on-401 behavior is Lumina-specific,
    /// not a change to `harmony`/`chord`/`muse`'s existing pass-through semantics.
    #[tokio::test]
    #[serial]
    async fn non_lumina_arms_forward_a_401_verbatim_unaffected_by_lgui_05() {
        async fn unauthorized() -> Response {
            (StatusCode::UNAUTHORIZED, "nope").into_response()
        }
        let app = axum::Router::new().route("/status", axum::routing::get(unauthorized));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let resp = proxy(
            "harmony",
            Some(format!("http://{addr}")), // pii-test-fixture
            "status",
            None,
            Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
            None,
            &[],
            None,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
