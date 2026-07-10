//! The daemon's local, loopback-only MCP endpoint (TCLI-05).
//!
//! Presents the SAME wire protocol `terminus_rs::mcp_server` serves on the
//! primary's plain `/mcp` listener (JSON-RPC 2.0 over `POST /mcp`, SSE-framed
//! `event: message\ndata: {...}\n\n` response bodies -- the exact framing a
//! real MCP client, and Chord's own `McpSession`, already know how to parse)
//! -- a local MCP client (Claude Code, initially, per TCLI-06) talks to this
//! daemon exactly as it would talk to a terminus primary directly, except
//! every `tools/list`/`tools/call` this handler answers is actually served
//! from (or forwarded to, via [`crate::forward`]) a REMOTE terminus primary
//! over mTLS -- this endpoint itself is plaintext, which is only safe
//! because it is loopback-only (see [`build_router`]'s caller in the
//! `terminus-client-daemon` binary, which must never bind anything but
//! `127.0.0.1`).
//!
//! ## Tool catalog caching
//! The primary's tool catalog (`tools/list` result) is cached in
//! [`DaemonState`] and refreshed lazily: a request only re-fetches from the
//! primary if the cache is empty or older than `catalog_ttl` ("refresh on
//! miss, else periodic refresh on next access past the TTL" -- the TCLI-05
//! EDGE CASE's "a simple periodic refresh is sufficient for P2"). A refresh
//! that fails while a (now-stale) cached catalog still exists serves the
//! stale catalog rather than failing the request outright -- a transient
//! primary blip should not make an already-known tool catalog disappear
//! from the local client's view; only a *never-yet-successful* fetch is a
//! hard error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::warn;

use crate::error::ClientError;
use crate::forward::{forward, sanitize_for_log, PrimaryConfig};

/// Default tool-catalog cache TTL -- see the module doc's "Tool catalog
/// caching" section. Overridable by the daemon binary via
/// `TERMINUS_CLIENT_CATALOG_TTL_SECS`.
pub const DEFAULT_CATALOG_TTL: Duration = Duration::from_secs(60);

struct CatalogCache {
    tools: Vec<Value>,
    fetched_at: Option<Instant>,
}

/// Shared daemon state: how to reach the primary, and the cached catalog.
pub struct DaemonState {
    pub primary: PrimaryConfig,
    pub server_name: String,
    pub server_version: String,
    catalog: RwLock<CatalogCache>,
    catalog_ttl: Duration,
}

impl DaemonState {
    pub fn new(primary: PrimaryConfig, server_name: impl Into<String>, server_version: impl Into<String>) -> Self {
        Self::with_catalog_ttl(primary, server_name, server_version, DEFAULT_CATALOG_TTL)
    }

    pub fn with_catalog_ttl(
        primary: PrimaryConfig,
        server_name: impl Into<String>,
        server_version: impl Into<String>,
        catalog_ttl: Duration,
    ) -> Self {
        Self {
            primary,
            server_name: server_name.into(),
            server_version: server_version.into(),
            catalog: RwLock::new(CatalogCache { tools: Vec::new(), fetched_at: None }),
            catalog_ttl,
        }
    }

    /// Return the cached tool catalog, refreshing from the primary first if
    /// the cache is empty or stale. See the module doc's caching section for
    /// the stale-serve-on-refresh-failure behavior.
    async fn cached_tools(&self) -> Result<Vec<Value>, ClientError> {
        let is_stale = {
            let cache = self.catalog.read().await;
            match cache.fetched_at {
                Some(t) => t.elapsed() > self.catalog_ttl,
                None => true,
            }
        };
        if !is_stale {
            return Ok(self.catalog.read().await.tools.clone());
        }

        let request = json!({"jsonrpc": "2.0", "id": "terminus-client-daemon-catalog-refresh", "method": "tools/list"});
        match forward(&self.primary, request).await {
            Ok(envelope) => {
                let tools = envelope
                    .get("result")
                    .and_then(|r| r.get("tools"))
                    .and_then(|t| t.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut cache = self.catalog.write().await;
                cache.tools = tools.clone();
                cache.fetched_at = Some(Instant::now());
                Ok(tools)
            }
            Err(e) => {
                let cache = self.catalog.read().await;
                if !cache.tools.is_empty() {
                    warn!(
                        "terminus_client::mcp_server: catalog refresh failed ({e}), serving stale cached catalog"
                    );
                    Ok(cache.tools.clone())
                } else {
                    Err(e)
                }
            }
        }
    }
}

pub fn build_router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(handle_healthz))
        .with_state(state)
}

async fn handle_healthz(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    (StatusCode::OK, format!("{} {} ok\n", state.server_name, state.server_version))
}

async fn handle_mcp(State(state): State<Arc<DaemonState>>, body: Bytes) -> Response {
    let parsed: Result<Value, _> = serde_json::from_slice(&body);
    let req = match parsed {
        Ok(v) => v,
        Err(e) => {
            warn!("terminus_client::mcp_server: invalid JSON-RPC body: {e}");
            return (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": "Parse error"}})
                    .to_string(),
            )
                .into_response();
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    if req.get("id").is_none() {
        // JSON-RPC notification (e.g. `notifications/initialized`) -- no
        // response body, per JSON-RPC notification semantics (matches
        // `terminus_rs::mcp_server`'s handling of the same case).
        return StatusCode::ACCEPTED.into_response();
    }

    match method {
        "initialize" => {
            let session_id = uuid::Uuid::new_v4().to_string();
            let result = json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": state.server_name, "version": state.server_version}
            });
            sse_response(id, Ok(result), &session_id)
        }
        "tools/list" => match state.cached_tools().await {
            Ok(tools) => sse_response(id, Ok(json!({"tools": tools})), ""),
            Err(e) => {
                warn!("terminus_client::mcp_server: tools/list failed: {e}");
                sse_response(id, Err((-32000, format!("primary unreachable: {e}"))), "")
            }
        },
        "tools/call" => handle_tools_call(&state, id, params).await,
        other => {
            warn!("terminus_client::mcp_server: unhandled method {other}");
            sse_response(id, Err((-32601, format!("Method not found: {other}"))), "")
        }
    }
}

async fn handle_tools_call(state: &DaemonState, id: Value, params: Value) -> Response {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    tracing::debug!(
        "terminus_client::mcp_server: tools/call name={name} args={}",
        sanitize_for_log(&arguments)
    );

    // TCLI-05 EDGE CASE: a call for a tool not present in the cached catalog
    // is rejected locally as "unknown tool" rather than forwarded -- the
    // primary would also reject it, but there's no reason to pay a round
    // trip (and a re-dial) to find that out.
    let known = match state.cached_tools().await {
        Ok(tools) => tools.iter().any(|t| t.get("name").and_then(|n| n.as_str()) == Some(name.as_str())),
        // If the catalog itself can't be established at all (never
        // succeeded, primary down), fall through and let the forward
        // attempt below surface the real "primary unreachable" error --
        // more informative than a blanket "unknown tool" in that case.
        Err(_) => true,
    };
    if !known {
        return sse_response(
            id,
            Ok(json!({"content": [{"type": "text", "text": format!("Unknown tool: {name}")}], "isError": true})),
            "",
        );
    }

    let upstream_request = json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });

    match forward(&state.primary, upstream_request).await {
        Ok(envelope) => relay_upstream_envelope(id, envelope),
        Err(e) => {
            warn!("terminus_client::mcp_server: tools/call forward failed for '{name}': {e}");
            sse_response(
                id,
                Ok(json!({
                    "content": [{"type": "text", "text": format!("tool call failed, primary unreachable: {e}")}],
                    "isError": true
                })),
                "",
            )
        }
    }
}

/// Relay an already-decoded upstream JSON-RPC envelope (from
/// [`forward`]) back to the local caller UNCHANGED, per the TCLI-05
/// APPROACH step 4 -- only the outer `id` is remapped to the local request's
/// own id (the upstream request already used it, so this is normally a
/// no-op, but kept explicit rather than assumed).
fn relay_upstream_envelope(id: Value, envelope: Value) -> Response {
    if let Some(result) = envelope.get("result") {
        return sse_response(id, Ok(result.clone()), "");
    }
    if let Some(error) = envelope.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
        let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("primary returned an error").to_string();
        return sse_response(id, Err((code, message)), "");
    }
    sse_response(id, Err((-32000, "primary returned a malformed MCP response".to_string())), "")
}

/// Frame a JSON-RPC response body the same way `terminus_rs::mcp_server`
/// does -- `event: message\ndata: {...}\n\n` -- so a local MCP client speaks
/// the identical wire protocol whether it's talking to this daemon or a
/// terminus primary directly.
fn sse_response(id: Value, result: Result<Value, (i64, String)>, session_id: &str) -> Response {
    let body = match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    };
    let sse = format!("event: message\ndata: {body}\n\n");

    let mut resp = (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response();
    if !session_id.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(session_id) {
            resp.headers_mut().insert("mcp-session-id", hv);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enroll::EnrollConfig;
    use crate::transport::ConnectConfig;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn unreachable_primary_config() -> PrimaryConfig {
        let mut cfg = PrimaryConfig::new(
            EnrollConfig::new("http://127.0.0.1:1", "test-daemon", "irrelevant"),
            ConnectConfig { host: "127.0.0.1".to_string(), port: 1, server_name: "terminus-primary".to_string() },
        );
        cfg.timeout = Duration::from_millis(300);
        cfg
    }

    fn test_state() -> Arc<DaemonState> {
        Arc::new(DaemonState::new(unreachable_primary_config(), "terminus-client-daemon-test", "0.0.0-test"))
    }

    async fn post_mcp(router: Router, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        let json_str = raw.lines().find(|l| l.starts_with("data:")).map(|l| l.trim_start_matches("data:").trim()).unwrap_or(&raw);
        let value = if json_str.is_empty() { Value::Null } else { serde_json::from_str(json_str).unwrap() };
        (status, value)
    }

    #[tokio::test]
    async fn initialize_returns_local_server_info_without_touching_primary() {
        let router = build_router(test_state());
        let (status, body) = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["serverInfo"]["name"], "terminus-client-daemon-test");
    }

    #[tokio::test]
    async fn tools_list_surfaces_a_clear_error_when_primary_unreachable() {
        let router = build_router(test_state());
        let (status, body) = post_mcp(router, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["error"]["message"].as_str().unwrap().contains("primary unreachable"));
    }

    #[tokio::test]
    async fn tools_call_for_unknown_tool_is_rejected_locally() {
        // Pre-seed an empty-but-fetched-once catalog so the "unknown tool"
        // path (not the "primary unreachable" fallback path) is exercised.
        let state = test_state();
        {
            let mut cache = state.catalog.write().await;
            cache.tools = vec![json!({"name": "known_tool"})];
            cache.fetched_at = Some(Instant::now());
        }
        let router = build_router(state);
        let (status, body) = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "does_not_exist", "arguments": {}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true);
        assert!(body["result"]["content"][0]["text"].as_str().unwrap().contains("Unknown tool"));
    }

    #[tokio::test]
    async fn notifications_get_no_response_body() {
        let router = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn healthz_ok() {
        let router = build_router(test_state());
        let req = Request::builder().method("GET").uri("/healthz").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── End-to-end: real mock mTLS primary behind the local daemon router ──
    // (reuses `crate::forward`'s test-support so the exact same handshake +
    // framing the daemon drives in production is exercised here too).

    use crate::forward::test_support::*;

    #[tokio::test]
    async fn local_tools_list_aggregates_the_real_primarys_catalog() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary(&ca, |req| {
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "weather", "description": "Weather lookup"}]}})
        })
        .await;

        let primary = primary_config(&credential, host, port);
        let state = Arc::new(DaemonState::new(primary, "terminus-client-daemon-test", "0.0.0-test"));
        let router = build_router(state);

        let (status, body) = post_mcp(router, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"][0]["name"], "weather");
    }

    #[tokio::test]
    async fn local_tools_call_is_forwarded_and_relayed_back_unchanged() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary(&ca, |req| {
            if req["method"] == "tools/list" {
                json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "weather"}]}})
            } else {
                json!({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": "72F sunny"}], "isError": false}})
            }
        })
        .await;

        let primary = primary_config(&credential, host, port);
        let state = Arc::new(DaemonState::new(primary, "terminus-client-daemon-test", "0.0.0-test"));
        let router = build_router(state);

        let (status, body) = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "weather", "arguments": {"city": "x"}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["content"][0]["text"], "72F sunny");
        assert_eq!(body["result"]["isError"], false);
    }
}
