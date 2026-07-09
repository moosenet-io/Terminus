//! A minimal standalone streamable-HTTP MCP server, built directly against a
//! `ToolRegistry`.
//!
//! This exists so a `[[bin]]` (currently `terminus_personal`) can expose a
//! `ToolRegistry` subset over the same wire protocol the legacy Python
//! `ai-mcp` fleet host speaks on its `/mcp` endpoint (confirmed live via a
//! real `initialize` handshake this session: `protocolVersion: "2024-11-05"`,
//! JSON body, `Mcp-Session-Id` response header, SSE-style
//! `event: message\ndata: {...}\n\n` framing) — so existing MCP clients
//! (including Chord's `McpSession`, see `chord-proxy/src/session.rs`) can talk
//! to it with zero client-side changes.
//!
//! ## Protocol surface (deliberately minimal — no resources/prompts)
//! - `POST /mcp` — JSON-RPC 2.0 body. Methods handled:
//!   - `initialize` — returns `protocolVersion`, `capabilities.tools`,
//!     `serverInfo`. Issues a fresh `Mcp-Session-Id` response header (a
//!     session-per-initialize model; sessions are not currently persisted or
//!     validated against subsequent requests — this server is stateless tool
//!     dispatch, matching the legacy Python host's practical behavior even
//!     though it also emits a session id).
//!   - Any request with no `"id"` (a JSON-RPC notification, e.g.
//!     `notifications/initialized`) — accepted, no response body (empty 202),
//!     per JSON-RPC notification semantics.
//!   - `tools/list` — returns the full registry catalog as MCP `Tool` objects
//!     (`name`, `description`, `inputSchema` sourced from `parameters()`).
//!   - `tools/call` — `{name, arguments}` → registry dispatch → MCP
//!     `CallToolResult` (`content: [{type: "text", text: ...}]`). An unknown
//!     tool name or a tool execution error both surface as `isError: true`
//!     in the result (a tool-call failure, not a JSON-RPC protocol error —
//!     `tools/call` itself is a valid method).
//!   - anything else (an unrecognized method, with an `"id"` present) →
//!     JSON-RPC `-32601 Method not found`.
//! - `GET /healthz` — plain-text liveness probe for systemd/monitoring (not
//!   part of the MCP wire protocol; a separate convenience route).
//!
//! ## Auth
//! Unauthenticated by default, matching the confirmed posture of the existing
//! legacy Python `/mcp` host (no bearer token, no session validation) — this
//! is a LAN-only, personal-network-tool endpoint, not an internet-facing one,
//! and adding auth machinery the legacy host never had
//! would be a scope-creep inconsistency, not a hardening win. If
//! `TERMINUS_PERSONAL_TOKEN` is set in the environment, a lightweight bearer
//! check is enforced instead (`Authorization: Bearer <token>`) — this gives
//! the operator an opt-in upgrade path without forcing one.

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::federation::PersonalFederationClient;
use crate::gateway_framework::{ActionKind, GatewayFramework};
use crate::inference_proxy::{
    InferenceProxyClient, AGENT_EXECUTE_PATH, CHAT_COMPLETIONS_PATH, CODING_SELECT_PATH,
    INFER_PATH,
};
use crate::pki::mtls::ClientIdentity;
use crate::registry::ToolRegistry;

/// Shared server state.
pub struct McpServerState {
    pub registry: ToolRegistry,
    pub server_name: String,
    pub server_version: String,
    /// If set, `Authorization: Bearer <token>` is required on `/mcp`.
    pub auth_token: Option<String>,
    /// TGW-02: when set, a tool name not found in `registry` (i.e. not a
    /// core tool) is proxied to Chord's `/v1/personal/tools/call` relay
    /// instead of being reported as an unknown tool, and `tools/list`
    /// includes the personal-registry tool set
    /// (`crate::registry::personal_only_tool_metadata`) alongside the local
    /// core catalog. `None` (the default for `terminus_personal`, which has
    /// no need to federate to itself) preserves the exact pre-TGW-02
    /// behavior: unknown tool names are just unknown.
    pub personal_federation: Option<PersonalFederationClient>,
    /// TGW-03: when set, `/v1/chat/completions`, `/v1/infer`,
    /// `/v1/agent/execute`, and `/v1/coding/select` are forwarded to Chord's
    /// co-located inference backend — see `crate::inference_proxy`'s module
    /// doc for the full contract. `None` (the default for
    /// `terminus_personal`, which has no inference-proxy role) means those
    /// routes are not mounted at all.
    pub inference_proxy: Option<InferenceProxyClient>,
    /// TGW-04: when set, EVERY request through this server (tool calls —
    /// core and federated-personal — AND the four inference-proxy routes
    /// below) is gated by the shared identity → allowlist → rate-limit →
    /// dispatch → audit pipeline (`crate::gateway_framework`) before
    /// dispatch runs. `None` (the default for `terminus_personal`, which
    /// predates this item and is not this spec's deployment target)
    /// preserves the exact pre-TGW-04 behavior: no gating at all, every
    /// request that reaches the router dispatches unconditionally.
    /// `terminus_primary` (TGW-04) sets `Some(GatewayFramework::from_env())`.
    pub gateway: Option<GatewayFramework>,
}

pub fn build_router(state: Arc<McpServerState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(handle_healthz))
        // TGW-03: inference-proxy routes forwarded to Chord — mounted
        // unconditionally; `handle_inference_proxy` itself returns a clean
        // 503 when `state.inference_proxy` is `None` (e.g. on
        // `terminus_personal`, which never sets it), rather than 404 (a
        // clearer signal than "route doesn't exist" for a route the binary
        // knows about but isn't configured to serve).
        .route(CHAT_COMPLETIONS_PATH, post(handle_chat_completions))
        .route(INFER_PATH, post(handle_infer))
        .route(AGENT_EXECUTE_PATH, post(handle_agent_execute))
        .route(CODING_SELECT_PATH, post(handle_coding_select))
        .with_state(state)
        // Request-level tracing (method/path/status/latency) via RUST_LOG —
        // useful for an admin-tools endpoint where knowing who called what,
        // when, matters operationally.
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

/// Shared dispatch for all four TGW-03 inference-proxy routes: if this
/// process is configured to proxy inference (`state.inference_proxy ==
/// Some`), forward to Chord at `path` via
/// `crate::inference_proxy::InferenceProxyClient::forward`, carrying the
/// mTLS-derived caller identity (if present) exactly as
/// `handle_mcp`'s personal-tool federation branch already does. Otherwise
/// (this binary has no inference-proxy role configured), return a clean
/// `503` rather than silently 404ing or hanging.
async fn handle_inference_proxy(
    state: Arc<McpServerState>,
    path: &'static str,
    identity: Option<Extension<ClientIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // TGW-04: gate every inference-proxy request through the same
    // identity → allowlist → rate-limit pipeline the tool-call path uses
    // (see `handle_mcp`'s `tools/call` branch) — `guard()` returns a ready
    // 403/429 response (already audited) on denial, or a context this
    // handler must `record_result` on once dispatch completes. `None`
    // (`state.gateway` unset, e.g. `terminus_personal`) preserves the exact
    // pre-TGW-04 ungated behavior.
    let gate_ctx = match &state.gateway {
        Some(gateway) => {
            let client_identity = identity.as_ref().map(|Extension(i)| i.clone());
            match gateway.guard(client_identity.as_ref(), path, ActionKind::Inference).await {
                Ok(ctx) => Some(ctx),
                Err(denial) => return denial,
            }
        }
        None => None,
    };

    let response = match &state.inference_proxy {
        Some(client) => {
            let caller_identity = identity.as_ref().map(|Extension(i)| i.as_str());
            client.forward(path, headers, body, caller_identity).await
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            json!({"error": "inference proxy not configured on this terminus process"})
                .to_string(),
        )
            .into_response(),
    };

    if let Some(ctx) = gate_ctx {
        let success = response.status().is_success();
        let detail = if success {
            None
        } else {
            Some(format!("upstream status {}", response.status()))
        };
        ctx.record_result(success, detail.as_deref());
    }

    response
}

async fn handle_chat_completions(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, CHAT_COMPLETIONS_PATH, identity, headers, body).await
}

async fn handle_infer(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, INFER_PATH, identity, headers, body).await
}

async fn handle_agent_execute(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, AGENT_EXECUTE_PATH, identity, headers, body).await
}

async fn handle_coding_select(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, CODING_SELECT_PATH, identity, headers, body).await
}

/// Extract a human-readable denial message from a `GatewayFramework::guard`
/// denial response (a JSON `{"error": "..."}` body per
/// `gateway_framework::denied_response`) — used to surface the SAME denial
/// text the inference-proxy path returns as an HTTP status/body into the
/// `tools/call` JSON-RPC result's `isError: true` text, since JSON-RPC has
/// no distinct status-code channel to carry it in.
async fn response_body_text(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => v
            .get("error")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn handle_healthz(State(state): State<Arc<McpServerState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        format!("{} {} ok\n", state.server_name, state.server_version),
    )
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("content-type", "application/json")],
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {"code": -32001, "message": "Unauthorized"}
        })
        .to_string(),
    )
        .into_response()
}

fn is_authorized(state: &McpServerState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.auth_token else {
        return true; // no token configured -> unauthenticated posture (matches legacy host)
    };
    let Some(got) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    got.strip_prefix("Bearer ") == Some(expected.as_str())
}

async fn handle_mcp(
    State(state): State<Arc<McpServerState>>,
    headers: HeaderMap,
    // Present only on requests that arrived over the mTLS listener
    // (`crate::pki::mtls::run_listener` inserts it into the connection's
    // request extensions post-handshake) -- absent on the plain HTTP+JWT
    // listener, in which case federated calls forward no caller identity.
    identity: Option<Extension<ClientIdentity>>,
    body: Bytes,
) -> Response {
    if !is_authorized(&state, &headers) {
        return unauthorized();
    }

    let parsed: Result<Value, _> = serde_json::from_slice(&body);
    let req = match parsed {
        Ok(v) => v,
        Err(e) => {
            warn!("terminus_personal: invalid JSON-RPC body: {e}");
            return (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": "Parse error"}
                })
                .to_string(),
            )
                .into_response();
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no "id") get no JSON-RPC response body at all — true for
    // `notifications/initialized` and, per spec, for any other id-less
    // request a client might send.
    let is_notification = req.get("id").is_none();
    if is_notification {
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
            info!("terminus_personal: initialize -> session {session_id}");
            sse_response(id, Ok(result), &session_id)
        }
        "tools/list" => {
            let mut tools: Vec<Value> = state
                .registry
                .list()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.parameters,
                    })
                })
                .collect();
            // TGW-02: aggregate in the personal-registry tool set (metadata
            // only, no network call -- see
            // `crate::registry::personal_only_tool_metadata`'s doc) when
            // this process is configured to federate personal-tool calls.
            if state.personal_federation.is_some() {
                tools.extend(crate::registry::personal_only_tool_metadata().into_iter().map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.parameters,
                    })
                }));
            }
            sse_response(id, Ok(json!({"tools": tools})), "")
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            // TGW-04: gate every tool call -- core (local) AND
            // personal-federated -- through the same identity → allowlist →
            // rate-limit pipeline the inference-proxy routes use (see
            // `handle_inference_proxy`), keyed by tool NAME regardless of
            // which branch below ultimately dispatches it. A denial here
            // returns a JSON-RPC `tools/call` *result* with `isError: true`
            // (there is no distinct "403" concept in JSON-RPC-over-HTTP —
            // this server always answers `200 OK` with the outcome encoded
            // in the result body, exactly like the pre-existing "Unknown
            // tool" case below), but the underlying gate decision and its
            // sanitized audit entry are identical to the inference-proxy
            // path's real `403`/`429` HTTP responses.
            let gate_ctx = if let Some(gateway) = &state.gateway {
                let client_identity = identity.as_ref().map(|Extension(i)| i.clone());
                match gateway.guard(client_identity.as_ref(), name, ActionKind::Tool).await {
                    Ok(ctx) => Some(ctx),
                    Err(denial) => {
                        let denial_text = response_body_text(denial).await;
                        return sse_response(
                            id,
                            Ok(json!({
                                "content": [{"type": "text", "text": denial_text}],
                                "isError": true
                            })),
                            "",
                        );
                    }
                }
            } else {
                None
            };

            let (response, success, detail) = match state.registry.call(name, arguments.clone()).await
            {
                Some(Ok(text)) => (
                    sse_response(
                        id,
                        Ok(json!({
                            "content": [{"type": "text", "text": text}],
                            "isError": false
                        })),
                        "",
                    ),
                    true,
                    None,
                ),
                Some(Err(e)) => {
                    let msg = e.to_string();
                    (
                        sse_response(
                            id,
                            Ok(json!({
                                "content": [{"type": "text", "text": msg.clone()}],
                                "isError": true
                            })),
                            "",
                        ),
                        false,
                        Some(msg),
                    )
                }
                // Not a core tool -- TGW-02: if this process federates
                // personal-tool calls, proxy to Chord's
                // `/v1/personal/tools/call` relay before giving up. Core
                // dispatch above is completely unchanged by this branch.
                None => match &state.personal_federation {
                    Some(client) => {
                        let caller_identity = identity.as_ref().map(|Extension(i)| i.as_str());
                        match client.call_tool(name, arguments, caller_identity).await {
                            Ok(outcome) => (
                                sse_response(
                                    id,
                                    Ok(json!({
                                        "content": [{"type": "text", "text": outcome.text}],
                                        "isError": outcome.is_error
                                    })),
                                    "",
                                ),
                                !outcome.is_error,
                                None,
                            ),
                            Err(fed_err) => {
                                warn!(
                                    "terminus_primary: federation error calling {name}: {fed_err}"
                                );
                                let msg = format!(
                                    "federation error: could not reach personal-tool backend via \
                                     chord relay ({fed_err})"
                                );
                                (
                                    sse_response(
                                        id,
                                        Ok(json!({
                                            "content": [{"type": "text", "text": msg.clone()}],
                                            "isError": true
                                        })),
                                        "",
                                    ),
                                    false,
                                    Some(msg),
                                )
                            }
                        }
                    }
                    // Per MCP convention, an unknown tool is a *tool-call*
                    // failure (`isError: true` in the result), not a
                    // JSON-RPC protocol error — `tools/call` itself is a
                    // valid method, so `-32601 Method not found` would be a
                    // misleading code here.
                    None => {
                        let msg = format!("Unknown tool: {name}");
                        (
                            sse_response(
                                id,
                                Ok(json!({
                                    "content": [{"type": "text", "text": msg.clone()}],
                                    "isError": true
                                })),
                                "",
                            ),
                            false,
                            Some(msg),
                        )
                    }
                },
            };

            if let Some(ctx) = gate_ctx {
                ctx.record_result(success, detail.as_deref());
            }
            response
        }
        other => {
            warn!("terminus_personal: unhandled method {other}");
            sse_response(id, Err((-32601, format!("Method not found: {other}"))), "")
        }
    }
}

/// Frame a JSON-RPC response body the way the legacy FastMCP host does
/// (`event: message\ndata: {...}\n\n`), which is also exactly what Chord's
/// `McpSession::send_request` already knows how to parse (it looks for a
/// `data:` line and falls back to plain JSON otherwise) — so this server
/// works as a drop-in MCP backend for Chord-style clients as well as for any
/// plain-JSON MCP client.
fn sse_response(id: Value, result: Result<Value, (i64, String)>, session_id: &str) -> Response {
    let body = match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    let sse = format!("event: message\ndata: {body}\n\n");

    let mut resp = (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        sse,
    )
        .into_response();

    if !session_id.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(session_id) {
            // HTTP header *names* inserted via a `&'static str` literal must
            // be lowercase (case-insensitive lookup/matching is unaffected;
            // this is purely about the insertion-side literal).
            resp.headers_mut().insert("mcp-session-id", hv);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ToolError;
    use crate::tool::RustTool;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    struct EchoHealthTool;

    #[async_trait]
    impl RustTool for EchoHealthTool {
        fn name(&self) -> &str {
            "health"
        }
        fn description(&self) -> &str {
            "Health check"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("ok".to_string())
        }
    }

    fn test_state() -> Arc<McpServerState> {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        Arc::new(McpServerState {
            registry,
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
        })
    }

    async fn post_mcp(router: Router, body: Value) -> (StatusCode, Value, HeaderMap) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        let json_str = raw
            .lines()
            .find(|l| l.starts_with("data:"))
            .map(|l| l.trim_start_matches("data:").trim())
            .unwrap_or(&raw);
        let value: Value = if json_str.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(json_str).unwrap()
        };
        (status, value, headers)
    }

    #[tokio::test]
    async fn test_initialize_handshake() {
        let router = build_router(test_state());
        let (status, body, headers) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0.1"}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(body["result"]["serverInfo"]["name"], "terminus-personal-test");
        assert!(headers.contains_key("mcp-session-id"));
    }

    #[tokio::test]
    async fn test_tools_list_returns_registered_tools() {
        let router = build_router(test_state());
        let (status, body, _) = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "health");
    }

    #[tokio::test]
    async fn test_tools_call_round_trips() {
        let router = build_router(test_state());
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "health", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["content"][0]["text"], "ok");
        assert_eq!(body["result"]["isError"], false);
    }

    #[tokio::test]
    async fn test_tools_call_unknown_tool_is_error_result() {
        let router = build_router(test_state());
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "does_not_exist", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Unknown tool is a tool-call failure (isError: true in the result),
        // not a JSON-RPC protocol error -- tools/call itself is a real method.
        assert_eq!(body["result"]["isError"], true);
        assert!(body["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("does_not_exist"));
    }

    struct AlwaysFailTool;

    #[async_trait]
    impl RustTool for AlwaysFailTool {
        fn name(&self) -> &str {
            "always_fail"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Err(ToolError::Execution("boom".to_string()))
        }
    }

    #[tokio::test]
    async fn test_tools_call_tool_error_is_error_result() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(AlwaysFailTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry,
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
        });
        let router = build_router(state);
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "always_fail", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true);
        assert!(body["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("boom"));
    }

    #[tokio::test]
    async fn test_notifications_initialized_returns_202_no_body() {
        let router = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
                    .to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn test_unauthorized_when_token_configured_and_missing() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry,
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: Some("secret-abc".to_string()),
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
        });
        let router = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_authorized_when_token_matches() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry,
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: Some("secret-abc".to_string()),
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
        });
        let router = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret-abc")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_healthz() {
        let router = build_router(test_state());
        let req = Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
