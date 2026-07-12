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
//! ## TMOD-01: hot-swappable tool registry
//! [`McpServerState::registry`] is an [`arc_swap::ArcSwap<ToolRegistry>`],
//! not a bare `ToolRegistry` — this lets the active tool set be replaced
//! WITHOUT restarting the process. Every handler that dispatches a request
//! takes exactly ONE snapshot (`state.registry.load()`) at the top and uses
//! that same `Arc<ToolRegistry>` for the entire request, so a swap that
//! lands mid-request never tears a single call: in-flight calls finish
//! against the snapshot they started with, and only calls that begin after
//! a swap observe the new registry. [`McpServerState::swap_registry`]
//! performs the atomic replacement; as of this item nothing on any live
//! path calls it yet (this is foundation only, behavior-preserving).
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

use arc_swap::ArcSwap;
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
use crate::gateway_framework::audit::{AuditDecision, AuditEntry, AuditResult};
use crate::gateway_framework::{ActionKind, GatewayFramework, ANONYMOUS_IDENTITY};
use crate::inference_proxy::{
    InferenceProxyClient, AGENT_EXECUTE_PATH, CHAT_COMPLETIONS_PATH, CODING_SELECT_PATH,
    INFER_PATH,
};
use crate::broker::routes::RouteTable;
use crate::mesh::{CallRoute, MergedCatalog, Principal, PrincipalResolver, TailnetIdentity, UpstreamPool};
use crate::pki::mtls::ClientIdentity;
use crate::registry::ToolRegistry;

/// Shared server state.
pub struct McpServerState {
    /// TMOD-01: the active tool-registry SNAPSHOT, swappable at runtime
    /// without a process restart. Every request handler takes exactly one
    /// `load()` at the top and dispatches the whole request against that
    /// snapshot — see this module's doc comment for the full invariant.
    /// Construct with `ArcSwap::from_pointee(registry)`; replace atomically
    /// via [`McpServerState::swap_registry`].
    pub registry: ArcSwap<ToolRegistry>,
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
    /// MESH-03: when set, `tools/list` merges in every currently-healthy
    /// mesh upstream's tools (namespaced `<namespace>__<tool>`, see
    /// `crate::mesh::merge`), and `tools/call` on a namespaced name is
    /// routed to that upstream instead of local/personal-federated
    /// dispatch. `None` (the default) is byte-for-byte the pre-MESH-03
    /// behavior — purely additive, matching `personal_federation`'s own
    /// `Option`-gated convention above.
    pub mesh_pool: Option<Arc<UpstreamPool>>,
    /// MESH-07: resolves the caller's transport identity/identities
    /// (`ClientIdentity`/`TailnetIdentity` request extensions) to a single
    /// canonical [`Principal`] for every gated request, replacing the
    /// interim `Principal::from(&ClientIdentity)` direct conversion at each
    /// `guard()` call site. See [`resolve_principal`]'s doc for the
    /// precedence rule: a configured `TERMINUS_MESH_PRINCIPAL_MAP_JSON`
    /// (`principal_resolver.is_configured()`) means strict
    /// resolve-or-fail-closed; an unconfigured resolver (the default —
    /// `PrincipalResolver::default()`, e.g. every deployment that predates
    /// this item, and `terminus_personal`, which never sets the map var)
    /// means the legacy cert-CN-as-name passthrough is used instead, so
    /// existing single-identity deployments and every pre-MESH-07 test in
    /// this module keep working unmodified.
    pub principal_resolver: PrincipalResolver,
    /// TMOD-04: the broker-owned, atomically-swappable tool-name → worker
    /// route table (see `crate::broker::routes` for the full design). A
    /// `tools/call` for a name NOT present in `registry`'s snapshot resolves
    /// against THIS table's snapshot before falling through to
    /// `personal_federation`/"Unknown tool"; `tools/list` merges in every
    /// currently-healthy routed worker's tools. Starts empty (`RouteTable::new()`)
    /// for every process until something calls its install methods (nothing
    /// on a live path does yet, as of this item — mutation is TMOD-05's
    /// worker-onboarding scope) — an empty table is behavior-preserving,
    /// identical to pre-TMOD-04 dispatch.
    pub broker_routes: RouteTable,
}

impl McpServerState {
    /// TMOD-01: atomically replace the active tool-registry snapshot with
    /// `new`. Any request that already captured the OLD snapshot (via
    /// `state.registry.load()` at the top of its handler) keeps running
    /// against it to completion — this call never blocks or invalidates an
    /// in-flight call, it only changes what the NEXT `load()` returns.
    ///
    /// As of this item, nothing on any live path calls this yet — it exists
    /// purely as the foundation for a future hot-reload/admin-tool item.
    pub fn swap_registry(&self, new: ToolRegistry) {
        self.registry.store(Arc::new(new));
    }
}

/// MESH-07: resolve one request's [`Principal`] from its transport identity
/// extensions (`cert`, the mTLS-derived [`ClientIdentity`]; `tailnet`, the
/// MESH-05 [`TailnetIdentity`]) via `resolver`, per the precedence this item
/// establishes:
/// - `resolver.is_configured()` (an operator has authored at least one entry
///   in `TERMINUS_MESH_PRINCIPAL_MAP_JSON`) — strict resolution:
///   `resolver.resolve(cert, tailnet)`. An unmapped or absent transport
///   identity yields `None` here (never a fallback to the raw cert CN), which
///   every `guard()` call site below treats as fail-closed, exactly as
///   `crate::mesh::principal`'s module doc requires.
/// - resolver NOT configured (the default — no map authored at all) — legacy
///   passthrough: `cert.map(Principal::from)`, byte-for-byte the interim
///   behavior every call site in this module used before MESH-07 (a present
///   cert's CN IS the principal name; a tailnet-only caller with no cert
///   gets no principal, same as before this item, since the pre-MESH-07 code
///   never looked at `TailnetIdentity` at all). This is what keeps every
///   existing single-identity deployment (and every pre-MESH-07 test in this
///   module) working unmodified when no map is configured.
///
/// Deliberately does NOT consult any HTTP header — a `Principal` is built
/// only from server-verified transport identities attached to the request's
/// `axum::http::Extensions` by the listener itself (mTLS handshake /
/// tailnet WhoIs), never from anything the client can set on the wire. This
/// is what makes a client-supplied `X-Terminus-Client-Identity` (or any
/// other) header unable to elevate identity — this function never reads
/// `HeaderMap` at all.
fn resolve_principal(
    resolver: &PrincipalResolver,
    cert: Option<&ClientIdentity>,
    tailnet: Option<&TailnetIdentity>,
) -> Option<Principal> {
    if resolver.is_configured() {
        resolver.resolve(cert, tailnet).ok()
    } else {
        cert.map(Principal::from)
    }
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
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // MESH-07: resolve the ONE canonical principal for this request once,
    // from server-verified transport identities only (never from a header)
    // -- see `resolve_principal`'s doc for the configured-map-vs-legacy
    // precedence. Used for both the gateway guard below AND (further down)
    // the caller-identity string forwarded to Chord's inference backend, so
    // both are derived from the exact same resolved identity.
    let principal = resolve_principal(
        &state.principal_resolver,
        identity.as_ref().map(|Extension(i)| i),
        tailnet.as_ref().map(|Extension(t)| t),
    );

    // TGW-04: gate every inference-proxy request through the same
    // identity → allowlist → rate-limit pipeline the tool-call path uses
    // (see `handle_mcp`'s `tools/call` branch) — `guard()` returns a ready
    // 403/429 response (already audited) on denial, or a context this
    // handler must `record_result` on once dispatch completes. `None`
    // (`state.gateway` unset, e.g. `terminus_personal`) preserves the exact
    // pre-TGW-04 ungated behavior.
    let gate_ctx = match &state.gateway {
        Some(gateway) => {
            match gateway.guard(principal.as_ref(), path, ActionKind::Inference).await {
                Ok(ctx) => Some(ctx),
                Err(denial) => return denial,
            }
        }
        None => None,
    };

    let response = match &state.inference_proxy {
        Some(client) => {
            // MESH-07: the identity forwarded to Chord is now the resolved
            // canonical `Principal::name` (mapped, when a map is
            // configured), not the raw mTLS cert CN -- same source of truth
            // the gate above just used.
            let caller_identity = principal.as_ref().map(|p| p.name());
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
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, CHAT_COMPLETIONS_PATH, identity, tailnet, headers, body).await
}

async fn handle_infer(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, INFER_PATH, identity, tailnet, headers, body).await
}

async fn handle_agent_execute(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, AGENT_EXECUTE_PATH, identity, tailnet, headers, body).await
}

async fn handle_coding_select(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_inference_proxy(state, CODING_SELECT_PATH, identity, tailnet, headers, body).await
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
    // MESH-05: present only on a request that arrived over a tailnet
    // listener connection whose WhoIs lookup resolved -- see
    // `TailnetIdentityLayer`'s doc.
    tailnet: Option<Extension<TailnetIdentity>>,
    body: Bytes,
) -> Response {
    if !is_authorized(&state, &headers) {
        return unauthorized();
    }

    // TMOD-01: capture ONE tool-registry snapshot for the ENTIRE request —
    // every dispatch branch below (`tools/list`, `tools/call`) reads from
    // this same `Arc<ToolRegistry>`, so a `swap_registry` that lands
    // mid-request can never tear this call: it either fully sees the old
    // registry or (for a request that starts after the swap) fully sees the
    // new one, never a mix of both.
    let reg = state.registry.load();
    // TMOD-04: same one-snapshot-per-request contract as `reg` above, for
    // the broker's worker route table — see `crate::broker::routes`'s
    // module doc and `McpServerState::broker_routes`'s doc.
    let broker_routes = state.broker_routes.load();

    // MESH-07: resolve the ONE canonical `Principal` for this request up
    // front, from server-verified transport identity extensions only (never
    // from any inbound header -- notably NOT
    // `crate::federation::CLIENT_IDENTITY_HEADER`, which this handler never
    // reads at all) -- see `resolve_principal`'s doc for the
    // configured-map-vs-legacy-passthrough precedence. Every `guard()` call
    // site and the personal-federation dispatch below all use this SAME
    // resolved principal, so a client cannot elevate identity by presenting
    // a header the server doesn't consult in the first place.
    let principal = resolve_principal(
        &state.principal_resolver,
        identity.as_ref().map(|Extension(i)| i),
        tailnet.as_ref().map(|Extension(t)| t),
    );

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
            let mut tools: Vec<Value> = reg
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
            // TMOD-04: merge in every currently-healthy broker-routed
            // worker's tools (bare, unprefixed names) -- see
            // `crate::broker::routes::merge_catalog`'s doc. A route whose
            // name collides with a tool already in `tools` (compiled-in, or
            // the personal-federation set above) is skipped: compiled-in/
            // personal-federated wins, per this table's documented
            // precedence. `broker_routes` empty (every deployment before a
            // worker is ever installed) makes this byte-for-byte a no-op.
            tools = crate::broker::routes::merge_catalog(tools, &broker_routes).await;
            // MESH-03: merge in every currently-healthy mesh upstream's
            // tools, namespaced `<namespace>__<tool>` -- see
            // `crate::mesh::merge::MergedCatalog`. `state.mesh_pool` is
            // `None` unless this process is explicitly configured to
            // federate a mesh, so this is a no-op for every deployment that
            // predates MESH-03 (byte-for-byte the tools built above).
            if let Some(pool) = &state.mesh_pool {
                let merged = MergedCatalog::build(tools, pool).await;
                tools = merged.tools;
            }
            // MESH-08: filter the merged catalog down to exactly what the
            // resolved caller `Principal` may CALL, per
            // `crate::gateway_framework::AllowlistPolicy` -- visibility ==
            // enforcement parity with the `tools/call` gate below, which
            // runs the same `is_allowed` decision on the same (possibly
            // namespaced) tool name. `state.gateway` unset (e.g.
            // `terminus_personal`, every pre-TGW-04 deployment) preserves
            // the exact pre-MESH-08 behavior: no filtering at all.
            if let Some(gateway) = &state.gateway {
                tools = gateway.filter_catalog_for_principal(principal.as_ref(), tools);
            }
            sse_response(id, Ok(json!({"tools": tools})), "")
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            // MESH-10: the canonical principal + the namespace (if any) the
            // advertised name parses to -- computed once up front so both
            // the pre-dispatch deny path (mesh routing hasn't run yet, so it
            // can't supply this from a `CallRoute`) and the post-dispatch
            // audit below can attribute a federated call to its upstream.
            let audit_principal =
                principal.as_ref().map(|p| p.name().to_string()).unwrap_or_else(|| ANONYMOUS_IDENTITY.to_string());
            let audit_upstream_ns = crate::mesh::split_namespaced(name).map(|(ns, _)| ns.to_string());

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
                match gateway.guard(principal.as_ref(), name, ActionKind::Tool).await {
                    Ok(ctx) => Some(ctx),
                    Err(denial) => {
                        let denial_text = response_body_text(denial).await;
                        // MESH-10: `guard()` already logged the precise
                        // generic denial (no-identity / not-allowlisted /
                        // rate-limited) -- for a FEDERATED (namespaced) name
                        // specifically, also log a federated-audit entry
                        // carrying the upstream/bare-tool-name context
                        // `guard()` itself can't know about, so a reviewer
                        // never has to correlate two log lines to see that a
                        // mesh call was denied. Never silent either way.
                        if let Some(namespace) = &audit_upstream_ns {
                            let bare = crate::mesh::split_namespaced(name).map(|(_, b)| b).unwrap_or(name);
                            AuditEntry::new_federated(
                                &audit_principal,
                                Some(namespace.clone()),
                                name,
                                bare,
                                ActionKind::Tool,
                                AuditResult::DeniedNotAllowlisted,
                                AuditDecision::Deny,
                                Some(&denial_text),
                            )
                            .log();
                        }
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

            // MESH-03: a namespaced name (`<namespace>__<tool>`) routes to
            // its owning mesh upstream (or a clean "unavailable" tool-error
            // if that upstream is down) BEFORE core/personal-federated
            // dispatch is even attempted -- a namespaced name is never
            // coincidentally a local or personal-federated tool. `None`
            // (`state.mesh_pool` unset, e.g. every pre-MESH-03 deployment)
            // and `Some(CallRoute::Local)` (a plain name, or a `__`-shaped
            // name whose prefix isn't a known mesh namespace) both fall
            // straight through to the existing core/personal-federated
            // dispatch below, byte-for-byte unchanged.
            let mesh_route = state.mesh_pool.as_ref().map(|pool| crate::mesh::resolve_call_route(name, pool));

            // MESH-10: once routing is resolved, attach the upstream/bare
            // tool name to the gate context (a no-op when `state.gateway` is
            // unset) so the terminal audit entry below carries the same
            // federated context the deny path above already logs.
            let gate_ctx = match &mesh_route {
                Some(CallRoute::Upstream { client, bare_name }) => {
                    gate_ctx.map(|ctx| ctx.with_upstream(client.namespace().to_string(), bare_name.clone()))
                }
                Some(CallRoute::Unavailable { namespace }) => {
                    gate_ctx.map(|ctx| ctx.with_upstream(namespace.clone(), name.to_string()))
                }
                _ => gate_ctx,
            };
            // MESH-10: set when dispatch couldn't even reach an upstream at
            // the transport level (unhealthy/unregistered mesh upstream, or
            // a network-level failure calling one that IS registered) --
            // audited below as `AuditDecision::TransportFailure`, never
            // silently dropped, and kept distinct from an ordinary
            // application-level tool error (`success: false` with the
            // default `Allow` decision).
            let mut is_transport_failure = false;

            let (response, success, detail) = match mesh_route {
                Some(CallRoute::Upstream { client, bare_name }) => {
                    // MESH-09: a guarded tool (<secret-manager>/ansible/openhands/  // pii-test-fixture
                    // routines, per `approval::is_guarded`) must be
                    // enforced at THIS gateway even when it lives on a
                    // remote upstream -- federation must never be a way to
                    // bypass human approval. Run the same `approval::gate`
                    // local guarded tools call, keyed on the bare tool name
                    // so guardedness classification matches local dispatch
                    // exactly, but with the target namespace folded into
                    // the gated content (`approval::mesh_gate_args`) so a
                    // code approved for one upstream's tool can never be
                    // replayed against another upstream's (or the local)
                    // same-named tool. This gate is authoritative and runs
                    // regardless of whatever approval gate the upstream
                    // itself may also enforce -- double-gating is fine,
                    // never skipped.
                    if crate::approval::is_guarded(&bare_name) {
                        let approval_code = arguments
                            .get(crate::approval::APPROVAL_ARG)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let gate_args = crate::approval::mesh_gate_args(&arguments, client.namespace());
                        let summary = format!(
                            "federated call \"{bare_name}\" on mesh upstream \"{}\"",
                            client.namespace()
                        );
                        match crate::approval::gate(&bare_name, &gate_args, &summary).await {
                            crate::approval::Gate::Granted => {}
                            crate::approval::Gate::Pending(msg)
                            | crate::approval::Gate::Denied(msg) => {
                                // MESH-16 (F1): the RBAC deny path above
                                // always logs a federated `AuditEntry` before
                                // returning early -- this approval-gate deny
                                // path must do the same, or a
                                // pending/denied federated call would be
                                // completely silent in the audit log.
                                AuditEntry::new_federated(
                                    &audit_principal,
                                    Some(client.namespace().to_string()),
                                    name,
                                    &bare_name,
                                    ActionKind::Tool,
                                    AuditResult::DeniedNotAllowlisted,
                                    AuditDecision::ApprovalRequired,
                                    Some(&msg),
                                )
                                .log();
                                return sse_response(
                                    id,
                                    Ok(json!({
                                        "content": [{"type": "text", "text": msg}],
                                        "isError": true
                                    })),
                                    "",
                                );
                            }
                        }
                        // Approved -- forward the caller's real args, with
                        // the gateway-only `_approval_code` stripped (the
                        // upstream's own tool schema knows nothing about
                        // it, and it must not leak to a remote server).
                        let mut forward_args = arguments.clone();
                        if let Some(obj) = forward_args.as_object_mut() {
                            obj.remove(crate::approval::APPROVAL_ARG);
                        }
                        match client.call_tool(&bare_name, forward_args).await {
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
                            Err(mesh_err) => {
                                // Transport/dispatch failure AFTER approval
                                // was granted -- the operator approved
                                // "run this call", not "spend the one-time
                                // code on a failed attempt at an unhealthy
                                // upstream". Roll the grant back so the same
                                // code can be retried once the upstream
                                // recovers (best-effort; a rollback failure
                                // just means a fresh approval is needed).
                                if let Some(code) = &approval_code {
                                    let _ = crate::approval::unconsume(&bare_name, code).await;
                                }
                                // MESH-16 (F2): a post-approval upstream
                                // failure is a transport/dispatch failure,
                                // not an ordinary application-level tool
                                // error -- route it to
                                // `record_transport_failure` below exactly
                                // like the non-guarded upstream error branch
                                // already does, instead of the default
                                // `record_result(false, ..)`.
                                is_transport_failure = true;
                                warn!(
                                    "mesh: error calling guarded \"{bare_name}\" on upstream \"{}\": {mesh_err}",
                                    client.namespace()
                                );
                                let msg = format!(
                                    "mesh upstream \"{}\" call failed: {mesh_err}",
                                    client.namespace()
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
                    } else {
                    // MESH-16 (F3): a gateway-only `_approval_code` must
                    // never reach an upstream, guarded or not -- the guarded
                    // branch above already strips it before forwarding; mirror
                    // that here so a caller who happens to pass one on a
                    // non-guarded federated tool doesn't leak it upstream.
                    let mut forward_args = arguments.clone();
                    if let Some(obj) = forward_args.as_object_mut() {
                        obj.remove(crate::approval::APPROVAL_ARG);
                    }
                    match client.call_tool(&bare_name, forward_args).await {
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
                        Err(mesh_err) => {
                            is_transport_failure = true;
                            warn!(
                                "mesh: error calling \"{bare_name}\" on upstream \"{}\": {mesh_err}",
                                client.namespace()
                            );
                            let msg = format!(
                                "mesh upstream \"{}\" call failed: {mesh_err}",
                                client.namespace()
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
                }
                Some(CallRoute::Unavailable { namespace }) => {
                    is_transport_failure = true;
                    let msg = crate::mesh::upstream_unavailable_text(&namespace);
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
                Some(CallRoute::Local) | None => match reg
                .call_structured(name, arguments.clone())
                .await
            {
                Some(Ok(output)) => {
                    // EGJS-01: additive `structuredContent` alongside the
                    // existing `content` text field -- only present when the
                    // dispatched tool overrode `RustTool::execute_structured`
                    // (see `crate::tool::ToolOutput`). Text-only tools (the
                    // vast majority, unmodified) produce byte-identical
                    // results to the pre-EGJS-01 `registry.call` path.
                    let mut result = json!({
                        "content": [{"type": "text", "text": output.text}],
                        "isError": false
                    });
                    if let Some(structured) = output.structured {
                        result["structuredContent"] = structured;
                    }
                    (sse_response(id, Ok(result), ""), true, None)
                }
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
                // Not a core tool -- TMOD-04: before falling through to
                // personal-federation, try the broker's worker route table
                // (see `crate::broker::routes::dispatch_call`'s doc). `None`
                // here means no route at all (an empty table, or this name
                // just isn't routed) -- falls through to
                // personal_federation/"Unknown tool" exactly as before this
                // item. `Some(..)` means a route exists: either the worker
                // answered (success or an application-level tool error) or
                // it's currently unhealthy (a clean transport failure) --
                // either way this is authoritative and does NOT also try
                // personal_federation for the same name.
                None => match crate::broker::routes::dispatch_call(&broker_routes, name, arguments.clone()).await {
                    Some(Ok(output)) => {
                        let mut result = json!({
                            "content": [{"type": "text", "text": output.text}],
                            "isError": false
                        });
                        if let Some(structured) = output.structured {
                            result["structuredContent"] = structured;
                        }
                        (sse_response(id, Ok(result), ""), true, None)
                    }
                    Some(Err(e)) => {
                        is_transport_failure = true;
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
                    None => match &state.personal_federation {
                    Some(client) => {
                        // MESH-07: propagate the resolved canonical
                        // `Principal` (not the raw `ClientIdentity`) so the
                        // JWT signed for this hop carries the mapped
                        // identity, and the legacy
                        // `X-Terminus-Client-Identity` header (kept for
                        // backward compatibility with the existing
                        // personal/Chord relay) is populated from the same
                        // source -- see `crate::federation`'s module doc.
                        match client.call_tool(name, arguments, principal.as_ref()).await {
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
                },
                },
            };

            if let Some(ctx) = gate_ctx {
                if is_transport_failure {
                    ctx.record_transport_failure(detail.as_deref());
                } else {
                    ctx.record_result(success, detail.as_deref());
                }
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

/// MESH-05 — tower layer that inserts an already-resolved
/// [`crate::mesh::TailnetIdentity`] into every request on ONE tailnet
/// connection's extensions, parallel to how
/// `crate::pki::mtls::serve_connection` inserts [`ClientIdentity`] for the
/// mTLS listener (see that function's doc comment). Gated under the `tsnet`
/// Cargo feature (off by default; see `crate::mesh::tailnet`'s module doc)
/// because it depends on `crate::mesh::tailnet::TailnetServer` — NOTE
/// [`crate::mesh::TailnetIdentity`] itself is deliberately NOT gated (see
/// that type's own module doc), only this insertion code is.
///
/// A fresh [`TailnetIdentityLayer`] is built PER ACCEPTED CONNECTION (mirror
/// of `crate::mesh::tailnet::serve_tailnet_connection`'s existing
/// per-connection `router.clone()`) with that connection's own resolved
/// identity — `identity: None` (a WhoIs miss or [`TailnetServer::whois`]
/// failure) is a completely normal, non-fatal outcome: the extension is
/// simply absent on every request over that connection, exactly like a
/// plain-HTTP request never carries a [`ClientIdentity`]. This layer never
/// fails a request over a WhoIs miss — precedence between a present
/// [`crate::mesh::TailnetIdentity`] and a present [`ClientIdentity`] (when a
/// future item lets both transports converge) is explicitly MESH-06's
/// decision, not this layer's.
#[cfg(feature = "tsnet")]
#[derive(Clone)]
pub struct TailnetIdentityLayer {
    identity: Option<crate::mesh::TailnetIdentity>,
}

#[cfg(feature = "tsnet")]
impl TailnetIdentityLayer {
    /// `identity` is the already-resolved (or absent) result of
    /// `TailnetServer::whois_identity` for the one connection this layer
    /// will be applied to — resolution itself does not happen here, only
    /// insertion, keeping this layer trivially cheap to construct per
    /// connection.
    pub fn new(identity: Option<crate::mesh::TailnetIdentity>) -> Self {
        Self { identity }
    }
}

#[cfg(feature = "tsnet")]
impl<S> tower::Layer<S> for TailnetIdentityLayer {
    type Service = TailnetIdentityService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TailnetIdentityService {
            inner,
            identity: self.identity.clone(),
        }
    }
}

/// The [`tower::Service`] [`TailnetIdentityLayer`] produces. Inserts the
/// carried identity (if any) into each request's extensions before calling
/// through to `inner` — never short-circuits or rejects a request, since
/// absence of a tailnet identity is an allowed, expected state (see
/// [`TailnetIdentityLayer`]'s doc).
#[cfg(feature = "tsnet")]
#[derive(Clone)]
pub struct TailnetIdentityService<S> {
    inner: S,
    identity: Option<crate::mesh::TailnetIdentity>,
}

#[cfg(feature = "tsnet")]
impl<S> tower::Service<axum::extract::Request> for TailnetIdentityService<S>
where
    S: tower::Service<axum::extract::Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        if let Some(identity) = self.identity.clone() {
            req.extensions_mut().insert(identity);
        }
        // Standard "clone the ready service, move the clone into the
        // future" pattern (the original `self.inner` may not be `Ready`
        // again until this call completes) -- same pattern
        // `tower::util::BoxCloneService`/most hand-rolled `tower::Service`
        // wrappers use.
        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
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
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
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

    // ── TMOD-04: broker worker route fallthrough through the MCP surface ────

    /// A stub in-box [`crate::broker::transport::WorkerTransport`] for the
    /// integration tests below -- no real I/O, programmable health + a fixed
    /// reply.
    struct StubWorker {
        healthy: bool,
        reply: String,
    }

    #[async_trait]
    impl crate::broker::transport::WorkerTransport for StubWorker {
        async fn connect(&self) -> Result<(), crate::broker::transport::TransportError> {
            Ok(())
        }
        async fn call(
            &self,
            _name: &str,
            _args: Value,
        ) -> Result<crate::tool::ToolOutput, ToolError> {
            Ok(crate::tool::ToolOutput { text: self.reply.clone(), structured: None })
        }
        async fn list(&self) -> Result<Vec<String>, crate::broker::transport::TransportError> {
            Ok(vec![])
        }
        async fn health(&self) -> bool {
            self.healthy
        }
    }

    /// Build a `test_state()` whose broker route table has `route` installed.
    fn state_with_broker_route(
        worker_id: &str,
        tool_name: &str,
        transport: Arc<dyn crate::broker::transport::WorkerTransport>,
    ) -> Arc<McpServerState> {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let broker_routes = crate::broker::routes::RouteTable::new();
        broker_routes.install(crate::broker::routes::WorkerRoute {
            worker_id: worker_id.to_string(),
            transport,
            tool: crate::registry::ToolInfo {
                name: tool_name.to_string(),
                description: format!("{tool_name} served by a worker"),
                parameters: json!({"type": "object"}),
            },
        });
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes,
        })
    }

    /// (a) An unknown name (not compiled-in, no route) still surfaces as the
    /// unchanged "Unknown tool" tool-call failure even with a broker route
    /// table present -- fallthrough is registry-miss → route-miss → Unknown.
    #[tokio::test]
    async fn tmod04_unknown_name_with_route_table_present_is_unknown_tool() {
        let state = state_with_broker_route(
            "w1",
            "worker_tool",
            Arc::new(StubWorker { healthy: true, reply: "hi".to_string() }),
        );
        let router = build_router(state);
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 40, "method": "tools/call",
                "params": {"name": "no_such_tool_anywhere", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true);
        assert!(body["result"]["content"][0]["text"].as_str().unwrap().contains("no_such_tool_anywhere"));
    }

    /// A healthy worker route dispatches over its transport on a compiled-in
    /// registry miss.
    #[tokio::test]
    async fn tmod04_healthy_worker_route_dispatches_through_mcp_surface() {
        let state = state_with_broker_route(
            "w1",
            "worker_tool",
            Arc::new(StubWorker { healthy: true, reply: "worker answered".to_string() }),
        );
        let router = build_router(state);
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 41, "method": "tools/call",
                "params": {"name": "worker_tool", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false);
        assert_eq!(body["result"]["content"][0]["text"], "worker answered");
    }

    /// (b) A route whose worker is UNHEALTHY answers a clean "unavailable"
    /// MCP result, while compiled-in tools on the same server still work.
    #[tokio::test]
    async fn tmod04_unhealthy_worker_route_is_unavailable_others_still_work() {
        let state = state_with_broker_route(
            "dead-worker",
            "dead_tool",
            Arc::new(StubWorker { healthy: false, reply: "unused".to_string() }),
        );
        let router = build_router(state.clone());
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 42, "method": "tools/call",
                "params": {"name": "dead_tool", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("dead-worker"));
        assert!(text.to_lowercase().contains("unavailable"));

        // A compiled-in tool on the SAME server is entirely unaffected.
        let router2 = build_router(state);
        let (status2, body2, _) = post_mcp(
            router2,
            json!({
                "jsonrpc": "2.0", "id": 43, "method": "tools/call",
                "params": {"name": "health", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(body2["result"]["isError"], false);
        assert_eq!(body2["result"]["content"][0]["text"], "ok");
    }

    /// (c) `tools/list` merges a healthy worker's catalog with the
    /// compiled-in tools; a name present in BOTH is listed once as the
    /// compiled-in tool (compiled-in wins on clash).
    #[tokio::test]
    async fn tmod04_tools_list_merges_worker_catalog_compiled_in_wins() {
        // Worker advertises a NEW tool plus one that CLASHES with the
        // compiled-in "health".
        let state = {
            let mut registry = ToolRegistry::new();
            registry.register(Box::new(EchoHealthTool)).unwrap();
            let broker_routes = crate::broker::routes::RouteTable::new();
            let transport: Arc<dyn crate::broker::transport::WorkerTransport> =
                Arc::new(StubWorker { healthy: true, reply: "x".to_string() });
            broker_routes.install_many(vec![
                crate::broker::routes::WorkerRoute {
                    worker_id: "w1".to_string(),
                    transport: transport.clone(),
                    tool: crate::registry::ToolInfo {
                        name: "worker_only_tool".to_string(),
                        description: "only on the worker".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                },
                crate::broker::routes::WorkerRoute {
                    worker_id: "w1".to_string(),
                    transport,
                    tool: crate::registry::ToolInfo {
                        name: "health".to_string(), // clashes with compiled-in
                        description: "worker's rival health".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                },
            ]);
            Arc::new(McpServerState {
                registry: ArcSwap::from_pointee(registry),
                server_name: "terminus-personal-test".to_string(),
                server_version: "0.0.0-test".to_string(),
                auth_token: None,
                personal_federation: None,
                inference_proxy: None,
                gateway: None,
                mesh_pool: None,
                principal_resolver: PrincipalResolver::default(),
                broker_routes,
            })
        };
        let router = build_router(state);
        let (status, body, _) = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 44, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"worker_only_tool"), "worker's unique tool must be merged in");
        // "health" appears exactly once -- the compiled-in one wins.
        assert_eq!(names.iter().filter(|n| **n == "health").count(), 1);
        let health = tools.iter().find(|t| t["name"] == "health").unwrap();
        assert_eq!(health["description"], "Health check", "compiled-in health wins on the name clash");
    }

    // ── EGJS-01: structuredContent ──────────────────────────────────────────

    struct StructuredEchoTool;

    #[async_trait]
    impl RustTool for StructuredEchoTool {
        fn name(&self) -> &str {
            "structured_echo"
        }
        fn description(&self) -> &str {
            "Echoes structured JSON alongside a text summary"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("id: 7, name: widget".to_string())
        }
        async fn execute_structured(
            &self,
            _args: Value,
        ) -> Result<crate::tool::ToolOutput, ToolError> {
            Ok(crate::tool::ToolOutput::with_structured(
                "id: 7, name: widget",
                json!({"id": 7, "name": "widget"}),
            ))
        }
    }

    #[tokio::test]
    async fn test_tools_call_includes_structured_content_when_tool_provides_it() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StructuredEchoTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
        });
        let router = build_router(state);
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": {"name": "structured_echo", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false);
        assert_eq!(body["result"]["content"][0]["text"], "id: 7, name: widget");
        assert_eq!(body["result"]["structuredContent"]["id"], 7);
        assert_eq!(body["result"]["structuredContent"]["name"], "widget");
    }

    #[tokio::test]
    async fn test_tools_call_omits_structured_content_for_text_only_tool() {
        // EchoHealthTool doesn't override execute_structured -- the default
        // impl returns structured: None, so the wire result must have NO
        // structuredContent key at all (proves existing text-only tools are
        // byte-for-byte unaffected by EGJS-01).
        let router = build_router(test_state());
        let (status, body, _) = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": "health", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["content"][0]["text"], "ok");
        assert!(body["result"].get("structuredContent").is_none());
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
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
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
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: Some("secret-abc".to_string()),
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
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
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: Some("secret-abc".to_string()),
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
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

    // ── TMOD-01: hot-swappable ArcSwap tool registry ────────────────────────

    struct ExtraTool;

    #[async_trait]
    impl RustTool for ExtraTool {
        fn name(&self) -> &str {
            "extra_tool"
        }
        fn description(&self) -> &str {
            "Only present after a swap"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("extra ok".to_string())
        }
    }

    /// After `swap_registry` installs a registry containing BOTH the
    /// original tool and a newly added one, a fresh request (a fresh
    /// `state.registry.load()`) can call either — the swap is additive from
    /// the caller's point of view, not a full replacement of what's
    /// reachable, as long as the new registry the caller builds includes
    /// both.
    #[tokio::test]
    async fn swap_registry_makes_new_tool_callable_while_keeping_the_old_one() {
        let state = test_state();

        // Pre-swap: only "health" exists.
        let (status, body, _) = post_mcp(build_router(state.clone()), health_call(1)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false);

        let mut new_registry = ToolRegistry::new();
        new_registry.register(Box::new(EchoHealthTool)).unwrap();
        new_registry.register(Box::new(ExtraTool)).unwrap();
        state.swap_registry(new_registry);

        // Post-swap: both "health" (still) and "extra_tool" (new) resolve.
        let (status, body, _) = post_mcp(build_router(state.clone()), health_call(2)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "original tool must still work after swap: {body}");

        let (status, body, _) = post_mcp(
            build_router(state.clone()),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "extra_tool", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "newly swapped-in tool must be callable: {body}");
        assert_eq!(body["result"]["content"][0]["text"], "extra ok");
    }

    /// A snapshot captured BEFORE a swap (`state.registry.load()`) keeps
    /// resolving against the registry it was taken from — a swap changes
    /// what the NEXT `load()` returns, never a snapshot already in hand.
    /// This is the in-flight-call-finishes-on-its-old-snapshot invariant,
    /// exercised directly against the snapshot API (a real concurrent HTTP
    /// request racing a swap is inherently timing-dependent; this pins the
    /// same guarantee deterministically).
    #[tokio::test]
    async fn snapshot_captured_before_swap_is_unaffected_by_a_later_swap() {
        let state = test_state();

        // Simulates `handle_mcp`'s `let reg = state.registry.load();` at the
        // top of an in-flight request.
        let in_flight_snapshot = state.registry.load();
        assert!(in_flight_snapshot.contains("health"));
        assert!(!in_flight_snapshot.contains("extra_tool"));

        let mut new_registry = ToolRegistry::new();
        new_registry.register(Box::new(ExtraTool)).unwrap(); // deliberately drops "health"
        state.swap_registry(new_registry);

        // The already-captured snapshot is untouched by the swap: it still
        // resolves "health" and still has never heard of "extra_tool" — no
        // panic, no missing-tool error mid-call, no tear.
        let result = in_flight_snapshot.call("health", json!({})).await;
        assert!(result.is_some(), "in-flight snapshot must still resolve its own tool after a swap");
        assert_eq!(result.unwrap().unwrap(), "ok");
        assert!(in_flight_snapshot.call("extra_tool", json!({})).await.is_none());

        // A FRESH load (a new request arriving after the swap) sees only the
        // new registry.
        let post_swap_snapshot = state.registry.load();
        assert!(!post_swap_snapshot.contains("health"));
        assert!(post_swap_snapshot.contains("extra_tool"));
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

    // ── MESH-05: TailnetIdentity's no-op (absent) path on DEFAULT features ──
    //
    // `TailnetIdentityLayer`/`TailnetIdentityService` themselves are gated
    // under `#[cfg(feature = "tsnet")]` (see their doc comments above --
    // they depend on `crate::mesh::tailnet::TailnetServer`, which doesn't
    // exist on default features at all). But `crate::mesh::TailnetIdentity`
    // is deliberately UNGATED (see its own module doc), so the "no tailnet
    // identity was ever inserted" path -- the normal state for every
    // request on this crate's existing plain and mTLS listeners, and for a
    // tailnet-listener connection whose WhoIs lookup misses -- is real,
    // testable behavior on a plain default `cargo test`, with no panic and
    // no `tsnet` feature required.
    #[tokio::test]
    async fn tailnet_identity_extension_absent_by_default_causes_no_panic() {
        let router = build_router(test_state());
        let req = Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        // No `crate::mesh::TailnetIdentity` extension was ever inserted on
        // this request (no tailnet listener involved at all here) --
        // dispatch still succeeds normally, exactly as it does today for a
        // plain HTTP request with no `ClientIdentity` either.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn tailnet_identity_extension_get_returns_none_when_never_inserted() {
        let extensions = axum::http::Extensions::new();
        assert!(extensions.get::<crate::mesh::TailnetIdentity>().is_none());
    }

    // ── MESH-07: resolved `Principal` wired through the gateway ───────────

    use crate::gateway_framework::rate_limit::InProcessRateLimiter;
    use crate::gateway_framework::{AllowlistPolicy, Grant};
    use crate::mesh::PrincipalMap;
    use std::collections::HashMap;

    /// A `GatewayFramework` whose allowlist maps EXACTLY `identity ->
    /// actions` (a generous rate-limit budget, high enough that none of
    /// these tests trip it).
    fn gateway_allowing(identity: &str, actions: &[&str]) -> GatewayFramework {
        let mut map = HashMap::new();
        map.insert(identity.to_string(), Grant::List(actions.iter().map(|a| a.to_string()).collect()));
        GatewayFramework::new(AllowlistPolicy::new(map), Arc::new(InProcessRateLimiter::new(1000, 1000.0)))
    }

    fn state_with(gateway: GatewayFramework, principal_resolver: PrincipalResolver) -> Arc<McpServerState> {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-mesh07-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: Some(gateway),
            mesh_pool: None,
            principal_resolver,
            broker_routes: crate::broker::routes::RouteTable::new(),
        })
    }

    /// Build a `POST /mcp` request carrying an optional `ClientIdentity`
    /// request extension (simulating what `crate::pki::mtls::run_listener`
    /// inserts post-handshake) and optional extra headers (simulating what
    /// a client might send on the wire, including an attempted
    /// `X-Terminus-Client-Identity` spoof).
    async fn post_mcp_with_identity(
        router: Router,
        body: Value,
        identity: Option<ClientIdentity>,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let mut req = builder.body(Body::from(body.to_string())).unwrap();
        if let Some(id) = identity {
            req.extensions_mut().insert(id);
        }
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
        (status, value)
    }

    fn health_call(id: i64) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "health", "arguments": {}}
        })
    }

    /// The resolved `Principal` -- not the raw cert CN -- is what `guard()`
    /// checks: a configured map sends `"harmony-primary.example.test"` to
    /// the allowlist as `"harmony"`, which IS granted `health`, even though
    /// the raw CN itself has no allowlist entry at all (default-deny would
    /// reject it if resolution were a no-op / a constant).
    #[tokio::test]
    async fn resolved_principal_not_raw_cn_is_used_at_the_guard_call_site() {
        let resolver = PrincipalResolver::new(
            serde_json::from_value::<PrincipalMap>(json!({
                "cert_cn": {"harmony-primary.example.test": "harmony"}
            }))
            .unwrap(),
        );
        let gateway = gateway_allowing("harmony", &["health"]);
        let state = state_with(gateway, resolver);
        let router = build_router(state);

        let identity = ClientIdentity("harmony-primary.example.test".to_string());
        let (status, body) =
            post_mcp_with_identity(router, health_call(1), Some(identity), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "mapped principal should be granted: {body}");
    }

    /// Same configured map as above, but the allowlist has NO entry for the
    /// raw CN string at all -- proving resolution is really consulted
    /// (denying an unmapped cert), not bypassed in favor of the raw CN.
    #[tokio::test]
    async fn unmapped_cert_with_a_configured_map_is_denied_fail_closed() {
        let resolver = PrincipalResolver::new(
            serde_json::from_value::<PrincipalMap>(json!({
                "cert_cn": {"harmony-primary.example.test": "harmony"}
            }))
            .unwrap(),
        );
        let gateway = gateway_allowing("harmony", &["health"]);
        let state = state_with(gateway, resolver);
        let router = build_router(state);

        // This CN has no entry in the configured map at all.
        let identity = ClientIdentity("stranger.example.test".to_string());
        let (status, body) =
            post_mcp_with_identity(router, health_call(2), Some(identity), &[]).await;
        assert_eq!(status, StatusCode::OK); // JSON-RPC always 200s; the denial is in the result.
        assert_eq!(body["result"]["isError"], true, "unmapped cert must fail closed: {body}");
    }

    /// No `TERMINUS_MESH_PRINCIPAL_MAP_JSON`-shaped map configured at all
    /// (`PrincipalResolver::default()`) -- the legacy pre-MESH-07 behavior
    /// (raw cert CN used verbatim as the principal name) must still work
    /// unmodified, so existing single-identity deployments are never
    /// mass-denied by this item.
    #[tokio::test]
    async fn unconfigured_resolver_keeps_legacy_cn_as_name_passthrough() {
        let gateway = gateway_allowing("legacy-cn.example.test", &["health"]);
        let state = state_with(gateway, PrincipalResolver::default());
        let router = build_router(state);

        let identity = ClientIdentity("legacy-cn.example.test".to_string());
        let (status, body) =
            post_mcp_with_identity(router, health_call(3), Some(identity), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "legacy passthrough should still work: {body}");
    }

    /// A client-supplied `X-Terminus-Client-Identity` header can NEVER
    /// elevate identity: with no `ClientIdentity` extension on the request
    /// (i.e. no server-verified mTLS identity presented), sending the
    /// header that names an identity the gateway WOULD allow must still be
    /// denied -- `resolve_principal` never reads `HeaderMap` at all.
    #[tokio::test]
    async fn client_supplied_identity_header_cannot_elevate_identity() {
        let resolver = PrincipalResolver::new(
            serde_json::from_value::<PrincipalMap>(json!({
                "cert_cn": {"harmony-primary.example.test": "harmony"}
            }))
            .unwrap(),
        );
        let gateway = gateway_allowing("harmony", &["health"]);
        let state = state_with(gateway, resolver);
        let router = build_router(state);

        // No `ClientIdentity` extension at all -- only a spoofed header.
        let (status, body) = post_mcp_with_identity(
            router,
            health_call(4),
            None,
            &[("x-terminus-client-identity", "harmony")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["isError"], true,
            "a bare client-set identity header must never grant access: {body}"
        );
    }

    // ── MESH-10: federated audit trail ─────────────────────────────────────
    //
    // The AuditEntry/AuditDecision shape itself (redaction, principal,
    // upstream, decision values) is covered exhaustively by
    // `gateway_framework::audit`'s own unit tests. These tests instead
    // exercise the `tools/call` dispatch path end to end -- proving a
    // federated call actually reaches `GatewayContext::with_upstream` /
    // `record_result` / `record_transport_failure` (i.e. an audit entry is
    // really emitted, not silently skipped) without panicking, for both the
    // allow and the deny cases.

    fn state_with_mesh(gateway: GatewayFramework, mesh_pool: UpstreamPool) -> Arc<McpServerState> {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-mesh10-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: Some(gateway),
            mesh_pool: Some(Arc::new(mesh_pool)),
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
        })
    }

    fn mesh10_init_response() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mesh10-mock-upstream", "version": "0.0.0"}
        }})
    }

    fn mesh10_registry_json(base_url: &str) -> String {
        // Bearer transport with no `secret_key` configured resolves to "no
        // auth" (see `UpstreamServer::resolve_secret`) -- simplest transport
        // for a plain local mock server, no embedded-CA/mTLS bootstrap
        // needed.
        format!(r#"[{{"name":"mesh10-upstream","url":"{base_url}","transport":"bearer","namespace":"mesh10ns"}}]"#)
    }

    /// A federated (namespaced) call that IS allowlisted and routes to a
    /// healthy upstream: dispatch succeeds, and
    /// `GatewayContext::with_upstream(..).record_result(true, ..)` runs
    /// (proven by a clean `200`/`isError: false` round trip -- a panic in
    /// that path would fail this test).
    #[tokio::test]
    #[serial_test::serial]
    async fn federated_call_allowed_and_routed_is_audited_as_allow() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "mesh10-session").json_body(mesh10_init_response());
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "tools/call"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 3,
                "result": {"content": [{"type": "text", "text": "echo: hi"}], "isError": false}
            }));
        });

        let registry = crate::mesh::registry::UpstreamRegistry::from_json(&mesh10_registry_json(&server.base_url()))
            .expect("valid registry json");
        let pool = UpstreamPool::from_registry(&registry);

        let gateway = gateway_allowing("dev-box", &["mesh10ns__echo"]);
        let state = state_with_mesh(gateway, pool);
        let router = build_router(state);

        let identity = ClientIdentity("dev-box".to_string());
        let (status, body) = post_mcp_with_identity(
            router,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "mesh10ns__echo", "arguments": {"msg": "hi"}}
            }),
            Some(identity),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "federated call should succeed: {body}");
        assert_eq!(body["result"]["content"][0]["text"], "echo: hi");
    }

    /// A federated (namespaced) call NOT allowlisted for this identity: the
    /// deny happens before mesh routing is even resolved -- proving the
    /// `AuditEntry::new_federated(.., AuditDecision::Deny, ..)` branch in the
    /// `Err(denial)` arm runs (a panic there would fail this test), and the
    /// call is never dispatched to the upstream at all (no mock configured
    /// for `tools/call`, so a dispatch attempt would itself fail the mock
    /// server's strict routing).
    #[tokio::test]
    #[serial_test::serial]
    async fn federated_call_denied_before_dispatch_is_audited_as_deny() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "mesh10-session").json_body(mesh10_init_response());
        });

        let registry = crate::mesh::registry::UpstreamRegistry::from_json(&mesh10_registry_json(&server.base_url()))
            .expect("valid registry json");
        let pool = UpstreamPool::from_registry(&registry);

        // Allowlisted for a DIFFERENT tool only -- "mesh10ns__echo" is denied.
        let gateway = gateway_allowing("dev-box", &["some_other_tool"]);
        let state = state_with_mesh(gateway, pool);
        let router = build_router(state);

        let identity = ClientIdentity("dev-box".to_string());
        let (status, body) = post_mcp_with_identity(
            router,
            json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": {"name": "mesh10ns__echo", "arguments": {}}
            }),
            Some(identity),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true, "denied federated call must be a tool-error result: {body}");
    }

    // ── MESH-16: Epic Review fixes to the federated `tools/call` path ──────

    /// F1: a GUARDED federated call (RBAC-allowlisted, so it reaches the
    /// `approval::gate` check) that is refused because it has no valid
    /// approval must still emit a federated `AuditEntry` before the early
    /// `return` -- exactly like the RBAC-deny path above already does.
    /// Pre-fix, that `Gate::Pending | Gate::Denied` arm returned with zero
    /// audit call at all (a silent denial). `DATABASE_URL` unset makes
    /// `approval::gate` deterministically return `Gate::Denied(..)` without
    /// needing a real Postgres -- no mock is registered for `tools/call`
    /// either, so if the fix's new `AuditEntry::new_federated(..).log()`
    /// call were to panic (wrong field/type), or if dispatch incorrectly
    /// proceeded to the upstream, this test would fail.
    #[tokio::test]
    #[serial_test::serial]
    async fn federated_guarded_call_denied_approval_is_audited_not_silent() {
        std::env::remove_var("DATABASE_URL");

        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "mesh10-session").json_body(mesh10_init_response());
        });

        let registry = crate::mesh::registry::UpstreamRegistry::from_json(&mesh10_registry_json(&server.base_url()))
            .expect("valid registry json");
        let pool = UpstreamPool::from_registry(&registry);

        // Allowlisted at RBAC -- it's the approval gate, not RBAC, that must
        // block (and audit) this call.
        let gateway = gateway_allowing("dev-box", &["mesh10ns__infisical_status"]);
        let state = state_with_mesh(gateway, pool);
        let router = build_router(state);

        let identity = ClientIdentity("dev-box".to_string());
        let (status, body) = post_mcp_with_identity(
            router,
            json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": "mesh10ns__infisical_status", "arguments": {}}
            }),
            Some(identity),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["isError"], true,
            "a guarded federated call with no valid approval must be refused, never dispatched: {body}"
        );
    }

    /// F2: a GUARDED federated call that IS approved but then fails at the
    /// transport level must be audited as `TransportFailure`, not an
    /// ordinary `record_result(false, ..)`. Reaching `Gate::Granted` for
    /// real requires a live Postgres grant row -- unavailable in this unit
    /// test environment, the same limitation `approval`'s own
    /// `gate_without_db_url_denies_gracefully` test documents (it too can
    /// only exercise the DB-unavailable `Denied` arm, never `Granted`).
    ///
    /// So this is a targeted source-level regression guard instead of a
    /// live dispatch: it pins that `is_transport_failure = true` is set
    /// inside the GUARDED upstream's `Err(mesh_err)` arm (right after the
    /// `unconsume` rollback call and before its `warn!("mesh: error calling
    /// guarded ...")`), which is exactly the statement the F2 fix adds.
    /// Deleting or moving that line -- the actual regression this guards
    /// against -- fails this test.
    #[test]
    fn guarded_upstream_transport_error_sets_is_transport_failure_before_warn() {
        let src = include_str!("mcp_server.rs");
        let unconsume_pos = src
            .find("let _ = crate::approval::unconsume(&bare_name, code).await;")
            .expect("guarded approval-rollback call must still be present");
        let guarded_warn_pos = src
            .find("\"mesh: error calling guarded \\\"{bare_name}\\\" on upstream \\\"{}\\\": {mesh_err}\"")
            .expect("guarded transport-error warn! must still be present");
        let flag_pos = src[unconsume_pos..guarded_warn_pos]
            .find("is_transport_failure = true;")
            .expect(
                "F2 regression: the guarded `Err(mesh_err)` arm must set \
                 `is_transport_failure = true` between the approval-rollback \
                 and its warn!, so the terminal audit records \
                 `TransportFailure` (not a plain `record_result(false, ..)`) \
                 for a post-approval upstream failure",
            );
        assert!(flag_pos > 0, "flag must be set strictly after the rollback call, matching the fix's placement");
    }

    /// F3: `_approval_code` must never leak to a NON-guarded federated
    /// upstream. This mock only matches a `tools/call` request whose body
    /// does NOT contain `_approval_code` -- if the fix regresses (the arg is
    /// forwarded verbatim again), the mock won't match, the mesh client gets
    /// a 404, and the call surfaces as an error instead of the expected
    /// clean success.
    fn body_excludes_approval_code(req: &httpmock::prelude::HttpMockRequest) -> bool {
        let body = req.body.as_deref().unwrap_or(&[]);
        let text = String::from_utf8_lossy(body);
        text.contains("\"method\":\"tools/call\"") && !text.contains("_approval_code")
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn federated_non_guarded_call_strips_approval_code_before_forwarding() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "mesh10-session").json_body(mesh10_init_response());
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .matches(body_excludes_approval_code);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 9,
                "result": {"content": [{"type": "text", "text": "echo: hi"}], "isError": false}
            }));
        });

        let registry = crate::mesh::registry::UpstreamRegistry::from_json(&mesh10_registry_json(&server.base_url()))
            .expect("valid registry json");
        let pool = UpstreamPool::from_registry(&registry);

        // "echo" is not in `approval::GUARDED_BARE_NAMES`, so this exercises
        // the non-guarded forward branch specifically.
        let gateway = gateway_allowing("dev-box", &["mesh10ns__echo"]);
        let state = state_with_mesh(gateway, pool);
        let router = build_router(state);

        let identity = ClientIdentity("dev-box".to_string());
        let (status, body) = post_mcp_with_identity(
            router,
            json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {
                    "name": "mesh10ns__echo",
                    "arguments": {"msg": "hi", "_approval_code": "SHOULD-NOT-LEAK"}
                }
            }),
            Some(identity),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["isError"], false,
            "call must succeed, proving the upstream only received the request \
             once `_approval_code` was stripped: {body}"
        );
        assert_eq!(body["result"]["content"][0]["text"], "echo: hi");
    }
}
