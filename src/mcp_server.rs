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
use tracing::{debug, info, warn};

use crate::federation::PersonalFederationClient;
use crate::gateway_framework::audit::{AuditDecision, AuditEntry, AuditResult};
use crate::gateway_framework::{ActionKind, GatewayFramework, ANONYMOUS_IDENTITY};
use crate::inference_proxy::{
    InferenceProxyClient, AGENT_EXECUTE_PATH, CHAT_COMPLETIONS_PATH, CODING_SELECT_PATH,
    INFER_PATH,
};
use crate::broker::routes::RouteTable;
use crate::mesh::{
    CallRoute, CallerBinding, MergedCatalog, Principal, PrincipalResolver, TailnetIdentity,
    UpstreamPool,
};
use crate::pki::mtls::ClientIdentity;
use crate::registry::ToolRegistry;

/// Shared server state.
/// Which human, if any, this MCP request is for (TERM #595).
///
/// Extracted from `handle_mcp` so the refusal arms are reachable from a test.
///
/// The PLAINTEXT `X-Terminus-On-Behalf-Of` header is never authoritative here.
/// It is meaningful at exactly one hop -- the inference-proxy ingress, where the
/// caller is mutually authenticated and its right to speak for someone else is a
/// grant-map decision -- and is translated there into a signed assertion.
/// Arriving on this endpoint it is a claim this handler cannot honour, so it is
/// REFUSED rather than ignored.
///
/// Ignoring it would be a silent WIDENING: a caller that believes it is acting
/// as one person would transparently read and write the SHARED, service-scoped
/// record instead of that person's -- exactly the data-mixing LOCREG-01 and this
/// item exist to rule out. An attempted identity that cannot be honoured must
/// never be indistinguishable from no identity.
fn asserted_person_for_mcp(
    gateway: Option<&crate::gateway_framework::GatewayFramework>,
    principal: Option<&crate::mesh::Principal>,
    headers: &HeaderMap,
) -> crate::mesh::AssertedPerson {
    if crate::mesh::person::on_behalf_of_header(headers).is_some() {
        return crate::mesh::AssertedPerson::Rejected;
    }
    match (gateway, crate::mesh::person::assertion_header(headers)) {
        (Some(gateway), token) => gateway.assert_person(principal, token),
        (None, None) => crate::mesh::AssertedPerson::None,
        // A gateway that is not configured cannot check a grant, so a token
        // presented to it is refused rather than honoured: absence of a policy
        // is never a reason to trust a claim.
        (None, Some(_)) => crate::mesh::AssertedPerson::Rejected,
    }
}

/// Which per-caller location record belongs to this turn (LOCREG-01 x TERM #595).
///
/// Extracted from the dispatch call so the three arms can be tested directly:
/// the arm that matters most is unreachable from a unit test while it is spelled
/// inline inside the request handler, and an untested fail-closed branch is a
/// fail-closed branch only by assertion.
///
/// - `None` — no assertion header: the legacy, service-scoped key. Unchanged
///   behaviour for every pre-#595 caller.
/// - `Verified` — the person is part of the key, so two people behind one
///   service principal file under different storage keys and neither can read
///   the other's saved home. The principal comes from the ASSERTION, not from
///   the request: `verify` has already bound the two together, and using the
///   bound copy means a future caller cannot pass a mismatched pair.
/// - `Rejected` — NO key at all. Falling back to the service key here would hand
///   the shared, pre-#577 record to exactly the caller whose identity we just
///   refused to believe: the same inversion [`CallerKey::for_person`] documents
///   for a blank person, reached from the other direction. No key means
///   `Lookup::Denied`, which is the only safe answer to "who is this?" when the
///   answer failed verification.
pub(crate) fn caller_key_for(
    principal: Option<&crate::mesh::Principal>,
    asserted: &crate::mesh::AssertedPerson,
) -> Option<crate::locations::CallerKey> {
    match asserted {
        crate::mesh::AssertedPerson::None => {
            principal.and_then(crate::locations::CallerKey::for_principal)
        }
        crate::mesh::AssertedPerson::Verified(vp) => {
            crate::locations::CallerKey::for_person(vp.principal(), vp.person())
        }
        crate::mesh::AssertedPerson::Rejected => None,
    }
}

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

    /// TRTR-08: shared TTL cache for high-traffic tool results (news, weather).
    /// Lives on the server state so it survives across requests — a per-request cache
    /// would never hit, which is the whole point.
    pub tool_cache: crate::tool_cache::ToolCache,
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
    /// RMCP-02: when `Some`, this process is an OAuth-protected remote-MCP
    /// connector door. Two things change, and nothing else:
    ///
    /// 1. The unauthenticated `.well-known` discovery routes are mounted (see
    ///    [`crate::oauth::router::oauth_router`]).
    /// 2. A `401` from `/mcp` carries the `WWW-Authenticate` challenge that
    ///    tells a client where those documents are — the ONLY way a client
    ///    learns which authorization server to use.
    ///
    /// `None` (the default, and every deployment that predates this item)
    /// preserves the previous behavior byte-for-byte: no new routes, and a bare
    /// `401` with the same JSON-RPC body. Same additive `Option`-gated
    /// convention as `personal_federation`/`gateway`/`mesh_pool` above.
    ///
    /// Note what this field does NOT do: it does not make `/mcp` accept an
    /// OAuth token. Token validation is RMCP-05's scope. Until then this is a
    /// discovery surface only, which is deliberate — a door that advertises
    /// where to get a key before it can check one is inert, whereas the reverse
    /// order would be a live unauthenticated path.
    pub rmcp_discovery: Option<Arc<crate::oauth::metadata::Discovery>>,

    /// RMCP-11 (TERM #631): the mounted OAuth endpoints, when this process
    /// serves them. See `crate::oauth::mount` for why mounting needed an owner
    /// and why it ended up here.
    pub rmcp_endpoints: Option<Arc<crate::oauth::mount::OauthEndpoints>>,
    /// RMCP-02: which OAuth surfaces this process serves — the authoritative
    /// input to [`McpServerState::oauth_door_enabled`], and through it the only
    /// thing that narrows `is_authorized`'s open arm.
    ///
    /// Deliberately NOT derived from the other fields on this struct. See
    /// [`crate::oauth::metadata::OauthDoors`] for why two attempts at deriving
    /// it both missed a door that is switched on somewhere else.
    /// `OauthDoors::none()` is the pre-RMCP-02 posture and changes nothing.
    pub oauth_doors: crate::oauth::metadata::OauthDoors,
    /// RMCP-05: when set, a request that presents NO mTLS cert and NO tailnet
    /// identity may instead authenticate with an OAuth 2.1 bearer token, which
    /// resolves through the SAME principal map to the SAME kind of
    /// [`Principal`] every other door produces (see
    /// `crate::oauth::resource`). `None` — the default everywhere, including
    /// `terminus_personal` — means the fourth door does not exist at all: no
    /// bearer token is inspected, no challenge is emitted, and every request
    /// path below behaves byte-for-byte as it did before this item.
    ///
    /// Deliberately an `Option` on the state rather than a router-level layer,
    /// matching `gateway`/`mesh_pool` above: the same handler must be able to
    /// serve a cert caller and an OAuth caller, and precedence between them is
    /// a per-request decision, not a per-route one.
    pub oauth_resource: Option<Arc<crate::oauth::resource::OauthResourceServer>>,

    /// RMCP-07: resolves an OAuth `client_id` to its connector scope.
    ///
    /// `Option` for the same reason as `oauth_resource`: a deployment with no
    /// OAuth door has no use for it. But the two are INDEPENDENT settings, and
    /// a request that carries a connector while this is `None` is refused
    /// rather than let through — see `handle_mcp`. A missing resolver is a
    /// configuration fault, and the fail-closed reading of a fault is denial.
    pub scope_resolver: Option<Arc<dyn crate::oauth::scope::ClientScopeSource>>,
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

    /// Whether this process serves ANY internet-facing OAuth surface.
    ///
    /// # This is the one place a door registers
    ///
    /// [`is_authorized`]'s open arm — "no shared token is configured, so admit
    /// everyone" — is correct for a private loopback/mTLS host and catastrophic
    /// for a public one. Which of those a process is depends on whether an
    /// OAuth door is mounted, and that question is asked HERE and nowhere else,
    /// so there is exactly one function deciding when an uncredentialed request
    /// is admitted. Two pieces of code making that decision independently is a
    /// dual-writer hazard: they drift, and the drift is silent because the
    /// failure mode is a `200` that should have been a `401`.
    ///
    /// # Why a named question instead of a field test
    ///
    /// Review round 5 caught the reason this matters. The condition used to
    /// read `state.rmcp_discovery.is_none()`, keyed on RMCP-02's own discovery
    /// config. But discovery and the resource-server door are INDEPENDENT
    /// switches — an operator can enable OAuth without configuring discovery —
    /// so an anonymous, tokenless request reached dispatch through the second
    /// switch. The identical `401`-vs-`200` defect this item had already fixed
    /// once, arriving by a different route, because the test named a field
    /// rather than asking the question.
    ///
    /// # Adding a door
    ///
    /// Nothing. That is the point of round 6's rewrite.
    ///
    /// The first two attempts at this both enumerated: `rmcp_discovery.is_some()`,
    /// dressed up the second time as a predicate. Both missed RMCP-05's
    /// resource-server door, which is switched on independently by
    /// `RMCP_OAUTH_ENABLED` and is not a field here at all — so the open arm
    /// stayed open and an anonymous, token-less request still reached dispatch
    /// with a `200`. The source-scan guard could not catch it either, because a
    /// door configured elsewhere is not an OAuth-shaped field on this struct;
    /// it read as coverage while providing none, so it has been removed.
    ///
    /// [`crate::oauth::metadata::OauthDoors`] now answers the question
    /// authoritatively, by detecting whether OAuth is configured AT ALL rather
    /// than by listing doors — see its docs. A door nobody can configure is not
    /// a door, and the moment one is configurable it is configurable through
    /// the shared `RMCP_OAUTH_*` prefix, which is what this detects. So a
    /// future door cannot silently reopen the arm, and no one has to remember
    /// to register it.
    pub(crate) fn oauth_door_enabled(&self) -> bool {
        self.oauth_doors.any()
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
/// `pub(crate)`: TMOD-05's `crate::broker::control` admin handlers reuse this
/// exact resolution rule (rather than re-deriving a `Principal` a second,
/// possibly-divergent way) so an admin-control-plane caller's identity is
/// resolved with the SAME configured-map-vs-legacy-passthrough precedence
/// every `/mcp` and inference-proxy handler already uses.
pub(crate) fn resolve_principal(
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
    // RMCP-02: built from its OWN `Arc<Discovery>` state rather than from
    // `McpServerState`, which is the point of the whole surface — those routes
    // must keep answering when everything reachable through `McpServerState`
    // (registry, mesh pool, database) is degraded. Captured here, before
    // `state` is moved into the constellation router below.
    let discovery_routes = state
        .rmcp_discovery
        .clone()
        .map(crate::oauth::router::oauth_router);

    // Likewise captured before `state` is moved. Built from the endpoints' own
    // `Arc`s rather than from `McpServerState`, so the auth surface does not
    // acquire a dependency on the registry or the mesh pool.
    let oauth_endpoint_routes = state.rmcp_endpoints.clone().map(|e| e.router());

    let router = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(handle_healthz))
        // PROMEX-01: Prometheus application-metrics scrape endpoint. Same
        // unauthenticated, always-on posture as `/healthz` above -- see
        // `crate::metrics`'s module doc for why no env-gate is needed.
        .route("/metrics", get(handle_metrics))
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
        .with_state(state.clone())
        // CONST-02: the constellation aggregation API layer -- `/api/*`,
        // `/ws`, and (when configured) the built `constellation-web` static
        // asset host. A compiled-in module merged into this router exactly
        // like every route above, deliberately NOT a broker worker -- see
        // `crate::constellation`'s module doc and
        // `docs/architecture/broker.md` for why.
        .merge(crate::constellation::constellation_router(state));

    // A `match` that returns the router untouched rather than merging an empty
    // one: an unconfigured deployment must get the pre-RMCP-02 router exactly,
    // not one that merely happens to behave the same. These are explicit
    // routes, so they take precedence over the constellation router's SPA
    // fallback (which would otherwise answer `/.well-known/…` with the app
    // shell) irrespective of merge order.
    let router = match discovery_routes {
        Some(discovery) => router.merge(discovery),
        None => router,
    };

    // RMCP-11 (TERM #631): the OAuth ENDPOINTS. Same `match`-rather-than-merge-
    // an-empty-router shape as discovery above, and for the same reason: an
    // unconfigured deployment must get the router it had before this item, not
    // one that merely behaves the same.
    //
    // Merged AFTER the constellation router for the same reason discovery is —
    // these are explicit routes, so they take precedence over that router's SPA
    // fallback, which would otherwise answer `/oauth/authorize` with the app
    // shell and turn a login page into a blank screen.
    let router = match oauth_endpoint_routes {
        Some(endpoints) => router.merge(endpoints),
        None => router,
    };

    router
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
impl McpServerState {
    /// TRTR-02: the MERGED tool catalog the router selects from.
    ///
    /// Mirrors what `tools/list` advertises — compiled-in core, then broker worker
    /// routes, then personal federation, then mesh upstreams — because a router that
    /// only saw the CORE registry would be blind to exactly the tools the operator
    /// asks about most: `pve__*` (Proxmox) is MESH-FEDERATED, not compiled in. Round-2
    /// review caught this; without it the router would have answered "that tool does
    /// not exist here" for the operator's Proxmox questions.
    pub async fn router_catalog(&self) -> Vec<crate::registry::ToolInfo> {
        // Built with the SAME merge helpers `tools/list` uses (they operate on
        // `Vec<Value>`), then converted to `ToolInfo` for selection — so the router
        // selects from exactly what the server advertises, never a narrower view.
        let reg = self.registry.load();
        let mut tools: Vec<Value> = reg
            .list()
            .into_iter()
            .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.parameters}))
            .collect();

        let broker_routes = self.broker_routes.load();
        tools = crate::broker::routes::merge_catalog(tools, &broker_routes).await;

        if self.personal_federation.is_some() {
            let existing: std::collections::HashSet<String> = tools
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect();
            tools.extend(
                crate::registry::personal_only_tool_metadata()
                    .into_iter()
                    .filter(|t| !existing.contains(&t.name))
                    .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.parameters})),
            );
        }

        if let Some(pool) = &self.mesh_pool {
            tools = MergedCatalog::build(tools, pool).await.tools;
        }

        tools
            .into_iter()
            .filter_map(|t| {
                Some(crate::registry::ToolInfo {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parameters: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect()
    }

    /// TRTR-02: dispatch ONE tool for the router, using the SAME precedence and
    /// routing `tools/call` uses — mesh upstream, then core, then broker, then
    /// personal federation. Returns the tool's text payload.
    ///
    /// Authorization, the model-block, and availability are enforced by the CALLER
    /// (`agent_router::dispatch_tool`) before this runs; this function is the routing
    /// half only, so the two concerns stay separable and testable.
    pub async fn router_dispatch(
        &self,
        name: &str,
        args: Value,
        principal: Option<&Principal>,
        // TERM-599: and WHICH HUMAN, so this path resolves the same per-person
        // context and record as `tools/call` does. Passed explicitly rather than
        // defaulted, so a new caller cannot silently inherit service scope.
        asserted: &crate::mesh::AssertedPerson,
    ) -> Result<String, String> {
        // 1. Mesh upstream (a namespaced name is never coincidentally a local tool).
        if let Some(pool) = &self.mesh_pool {
            match crate::mesh::resolve_call_route(name, pool) {
                CallRoute::Upstream { client, bare_name } => {
                    return match client.call_tool(&bare_name, args).await {
                        Ok(r) if r.is_error => Err(r.text),
                        Ok(r) => Ok(r.text),
                        Err(e) => Err(e.to_string()),
                    };
                }
                CallRoute::Unavailable { namespace } => {
                    return Err(format!(
                        "the `{namespace}` upstream is currently unavailable"
                    ));
                }
                CallRoute::Local => {}
            }
        }
        // 2. Core registry.
        //
        // TRTR-05: carry what the gateway knows about this principal into
        // dispatch, so a tool that can otherwise fold in OPERATOR context
        // (`weather`'s calendar/routine location inference) learns who is
        // actually asking. `caller_context` is read-only — the authorization
        // decision and its audit entry are made by this function's CALLER, which
        // is the one place that knows whether this is a foreground turn or a
        // background cache refresh. With no gateway configured there is no
        // verified principal at all, and `CallerContext::untrusted()` is the
        // correct, fail-closed answer.
        let caller = match self.gateway.as_ref() {
            Some(gw) => gw.caller_context_for_person(principal, asserted),
            // No gateway: no grant map, so nothing could have been verified. An
            // unevaluated assertion must land BELOW the service default, not on it.
            None if matches!(asserted, crate::mesh::AssertedPerson::None) => {
                crate::tool::CallerContext::default()
            }
            None => crate::tool::CallerContext::unidentified(),
        };
        // LOCREG-01: alongside WHAT this caller may see, carry WHICH per-caller
        // record is theirs — derived from the same server-verified principal,
        // never from an argument or a header. `None` when there is no principal,
        // which a caller-keyed tool must treat as "decline", not "use a default
        // record".
        let key = caller_key_for(principal, asserted);
        let reg = self.registry.load();
        if let Some(r) = reg.call_with_caller_key(name, args.clone(), caller, key).await {
            return r.map(|o| o.text).map_err(|e| e.to_string());
        }
        // 3. Broker worker routes.
        let broker_routes = self.broker_routes.load();
        if let Some(r) = crate::broker::routes::dispatch_call(&broker_routes, name, args.clone()).await {
            return r.map(|o| o.text).map_err(|e| e.to_string());
        }
        // 4. Personal federation.
        if let Some(pf) = &self.personal_federation {
            return match pf.call_tool(name, args, principal).await {
                Ok(r) if r.is_error => Err(r.text),
                Ok(r) => Ok(r.text),
                Err(e) => Err(e.to_string()),
            };
        }
        Err(format!("`{name}` is not a tool that exists here"))
    }

    /// The process-wide tool result cache (TRTR-08).
    pub fn tool_cache(&self) -> &crate::tool_cache::ToolCache {
        &self.tool_cache
    }
}

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

    // TERM #595: this front door is the ONE place a human identity can enter the
    // agent path with any authority behind it -- it is the only hop where the
    // asserting caller is mutually authenticated (mTLS/tailnet) AND its right to
    // speak for someone else is a grant-map decision. So the plaintext
    // `X-Terminus-On-Behalf-Of` request is translated HERE into a signed,
    // principal-bound assertion; downstream of this point the plaintext form has
    // no meaning at all (it is stripped, see
    // `crate::inference_proxy::is_unforwardable_request_header`).
    //
    // If the caller ASKED to act for a person and we cannot mint that assertion
    // -- no gateway, no grant, no signing key, or a person who is not on the
    // roster -- the request is REFUSED. It is deliberately not downgraded to an
    // anonymous service-scoped turn: the caller asked for something NARROWER
    // than the service identity, and quietly running it as the service would be
    // a silent WIDENING, which is the one direction this whole item exists to
    // rule out.
    //
    // A request that carries only the SIGNED header and no plaintext request is
    // the same situation reached from the other side: an inbound signed
    // assertion is never authoritative at an ingress (it is stripped as
    // unforwardable, precisely because a client could have minted or replayed
    // it), so honouring it is not an option -- but neither is dropping it, which
    // would silently run an attempted-person turn as the shared service. Treat
    // it as an unhonourable claim and refuse, exactly as for an on-behalf-of we
    // cannot mint.
    if crate::mesh::person::on_behalf_of_header(&headers).is_none()
        && crate::mesh::person::assertion_header(&headers).is_some()
    {
        tracing::warn!(
            principal = principal.as_ref().map(Principal::name).unwrap_or("<none>"),
            "TERM #595: refusing an inbound signed person-assertion at the proxy ingress"
        );
        if let Some(ctx) = gate_ctx {
            ctx.record_result(false, Some("inbound person-assertion refused"));
        }
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            json!({
                "error": "on-behalf-of assertion refused",
                "detail": "a signed person-assertion is server-set on each hop and is never accepted                            from a client; ask to act for someone with the on-behalf-of header instead"
            })
            .to_string(),
        )
            .into_response();
    }

    let person_assertion = match crate::mesh::person::on_behalf_of_header(&headers) {
        None => None,
        Some(requested) => {
            let minted = state
                .gateway
                .as_ref()
                .map(|gw| gw.mint_person_assertion(principal.as_ref(), requested));
            match minted {
                Some(Ok(token)) => Some(token),
                other => {
                    let reason = match other {
                        Some(Err(e)) => e.to_string(),
                        _ => "this process has no gateway to authorize an on-behalf-of assertion"
                            .to_string(),
                    };
                    tracing::warn!(
                        principal = principal.as_ref().map(Principal::name).unwrap_or("<none>"),
                        reason = %reason,
                        "TERM #595: refusing an on-behalf-of request rather than running it as the service"
                    );
                    if let Some(ctx) = gate_ctx {
                        ctx.record_result(false, Some("on-behalf-of assertion refused"));
                    }
                    return (
                        StatusCode::FORBIDDEN,
                        [("content-type", "application/json")],
                        json!({"error": "on-behalf-of assertion refused", "detail": reason})
                            .to_string(),
                    )
                        .into_response();
                }
            }
        }
    };

    let response = match &state.inference_proxy {
        Some(client) => {
            // MESH-07: the identity forwarded to Chord is now the resolved
            // canonical `Principal::name` (mapped, when a map is
            // configured), not the raw mTLS cert CN -- same source of truth
            // the gate above just used.
            let caller_identity = principal.as_ref().map(|p| p.name());
            client.forward(path, headers, body, caller_identity, person_assertion.as_deref()).await
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

/// `/v1/agent/execute` — the agentic tool turn.
///
/// TRTR-02: this used to BLIND-FORWARD to Chord, which ran the tool loop against its
/// own catalog. That catalog had no caller identity (so tool exposure could not be
/// scoped per user) and pointed at a stale backend (so news/weather/Proxmox were
/// invisible to the assistant). The loop now runs HERE, where the principal is already
/// resolved and authorization already lives; Chord is called only for the
/// tool-selecting sub-agent's inference.
///
/// Gated by `TERMINUS_ROUTER_LOCAL` (default ON once deployed). Setting it to `0`
/// restores the exact previous blind-forward behaviour — a documented rollback that
/// needs no redeploy.
async fn handle_agent_execute(
    State(state): State<Arc<McpServerState>>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !crate::config::router_local_enabled() {
        return handle_inference_proxy(state, AGENT_EXECUTE_PATH, identity, tailnet, headers, body)
            .await;
    }

    let principal = resolve_principal(
        &state.principal_resolver,
        identity.as_ref().map(|Extension(i)| i),
        tailnet.as_ref().map(|Extension(t)| t),
    );

    // TERM-599: WHICH HUMAN this turn is for, on the path Lumina actually uses.
    //
    // TERM #595 threaded this into `handle_mcp` and refused unhonourable claims at
    // the inference-proxy ingress — but BOTH live in handlers this one only reaches
    // when `TERMINUS_ROUTER_LOCAL=0`. With the local router on (the default), the
    // headers were read once for auth and then never again, so a client sending
    // `x-terminus-on-behalf-of: alice` had it DROPPED WITHOUT ERROR and the turn ran
    // against the shared, service-scoped record.
    //
    // That is the exact failure `asserted_person_for_mcp`'s doc describes — an
    // attempted identity that cannot be honoured must never be indistinguishable
    // from no identity — reproduced on the one door that carries real traffic.
    let asserted = asserted_person_for_mcp(state.gateway.as_ref(), principal.as_ref(), &headers);

    // And REFUSE it here, rather than merely running the turn with less privilege.
    //
    // Threading a `Rejected` assertion downstream does produce an unidentified
    // context and no caller key, which is the safe direction — but it is not the
    // same as refusing, and the sibling `handle_inference_proxy` path answers 403
    // for exactly this input. Two doors giving different answers to the same
    // refused claim is how one of them quietly becomes the way in.
    if matches!(asserted, crate::mesh::AssertedPerson::Rejected) {
        tracing::warn!(
            principal = principal.as_ref().map(Principal::name).unwrap_or("<none>"),
            "TERM-599: refusing an unhonourable person claim on the local router path"
        );
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            json!({
                "error": "on-behalf-of assertion refused",
                "detail": "this identity could not be honoured; it was not silently downgraded"
            })
            .to_string(),
        )
            .into_response();
    }

    // Same gate the forwarding path applies, on the same resolved principal — the
    // router is a caller of the sanctioned path, never a way around it.
    let gate_ctx = match &state.gateway {
        Some(gateway) => {
            match gateway
                .guard(principal.as_ref(), AGENT_EXECUTE_PATH, ActionKind::Inference)
                .await
            {
                Ok(ctx) => Some(ctx),
                Err(denial) => return denial,
            }
        }
        None => None,
    };

    let Some(chord) = state.inference_proxy.as_ref() else {
        // No inference configured on this binary — the router cannot run.
        let r = (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            json!({"error": "no inference backend configured"}).to_string(),
        )
            .into_response();
        if let Some(ctx) = gate_ctx {
            ctx.record_result(false, Some("no inference backend"));
        }
        return r;
    };

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let r = (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                json!({"error": format!("invalid request body: {e}")}).to_string(),
            )
                .into_response();
            if let Some(ctx) = gate_ctx {
                ctx.record_result(false, Some("invalid body"));
            }
            return r;
        }
    };

    let system_prompt = req.get("system_prompt").and_then(|v| v.as_str()).unwrap_or("");

    // CARRY THE WHOLE TRANSCRIPT. Round-4 review caught a real regression here: the
    // first cut extracted only the newest user message and rebuilt a fresh transcript,
    // which would have dropped conversation history on EVERY tool turn — the assistant
    // would forget what was just being discussed. The caller's `messages` are the
    // conversation; pass them through untouched.
    let history: Vec<Value> = req
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    // The newest user turn still drives tool SELECTION — that is what the request is
    // about — but it no longer replaces the transcript.
    let user_message = history
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let deps = crate::agent_router::RouterDeps {
        state: &state,
        cache: state.tool_cache(),
        chord,
        gateway: state.gateway.as_ref(),
        principal: principal.as_ref(),
        asserted: &asserted,
    };

    let outcome = crate::agent_router::execute(
        deps,
        system_prompt,
        &user_message,
        history,
        crate::agent_router::RouterConfig::default(),
    )
    .await;

    tracing::info!(
        "agent_router: complete status={} turns={} tools={} identity={}",
        outcome.status,
        outcome.turns,
        outcome.steps.len(),
        principal.as_ref().map(|p| p.name()).unwrap_or("<none>")
    );

    if let Some(ctx) = gate_ctx {
        ctx.record_result(true, None);
    }

    // TRTR-02/04: honour the caller's streaming preference with CHORD'S EXISTING
    // frame vocabulary. lumina-core sets `stream: true` and parses
    // `tool_call_started`/`tool_call_complete`/`complete` — emitting the same frames
    // means the client needs NO change to talk to the relocated router, which is what
    // makes TRTR-04 a verification step instead of a risky contract change on a live
    // assistant. Its parser FAILS a turn that ends without `complete`, so
    // `render_sse` always emits one, including on timeout.
    let wants_stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    if wants_stream {
        return (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            crate::agent_router::render_sse(&outcome),
        )
            .into_response();
    }

    // Non-streaming callers get the same JSON shape Chord returned.
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        json!({
            "response": outcome.response,
            "status": outcome.status,
            "turns": outcome.turns,
            "tools_called": outcome.steps.len(),
            "execution_log": outcome.steps.iter().map(|s| json!({
                "tool": s.tool,
                "status": s.status,
                "cached": s.cached,
                "duration_ms": s.duration_ms,
            })).collect::<Vec<_>>(),
        })
        .to_string(),
    )
        .into_response()
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

/// PROMEX-01: `GET /metrics` — encodes the process-global
/// `crate::metrics` registry (tool-call counts + latency histogram) in the
/// standard Prometheus text exposition format. Takes no `State` — the
/// registry is process-global, not per-server-instance — so this route
/// works unmodified on every binary that mounts `build_router`.
async fn handle_metrics() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        crate::metrics::gather_text(),
    )
}

/// The `401` for a request that presented no usable credential.
///
/// RMCP-02: when this process is configured as an OAuth connector door, this
/// response carries `WWW-Authenticate: Bearer resource_metadata="…"`. That
/// header is the entire discovery bootstrap — a hosted MCP client learns which
/// authorization server to use from it and from nowhere else — and it is
/// honoured ONLY on a `401`. The same header on a `200` is discarded, which is
/// why the unauthenticated path must genuinely fail rather than succeed with a
/// hint attached.
///
/// The JSON-RPC body is unchanged from the pre-RMCP-02 response, and no header
/// is added when the door is unconfigured, so every existing deployment and
/// every existing test sees exactly what it saw before.
///
/// Note the argument is DISCOVERY specifically, not
/// [`McpServerState::oauth_door_enabled`], and that is correct rather than an
/// oversight: the challenge's whole payload is a `resource_metadata` URL, so a
/// process with a door but no discovery surface has nothing to point a client
/// at and correctly emits a bare `401`. The status code — the part that decides
/// whether the request was admitted — is gated on the predicate; only the
/// advisory header is gated on discovery.
fn unauthorized(discovery: Option<&crate::oauth::metadata::Discovery>) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        [("content-type", "application/json")],
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {"code": -32001, "message": "Unauthorized"}
        })
        .to_string(),
    )
        .into_response();

    if let Some(discovery) = discovery {
        // Infallible in practice — `CanonicalUri` refuses every byte that could
        // not appear in a header value, which is precisely why it refuses them
        // at startup. Handled rather than unwrapped anyway: a panic here would
        // turn a config edge case into a downed listener, and a `401` without
        // the challenge is still a correct (if undiscoverable) answer.
        match axum::http::HeaderValue::from_str(discovery.unauthorized_challenge()) {
            Ok(value) => {
                response
                    .headers_mut()
                    .insert(axum::http::header::WWW_AUTHENTICATE, value);
            }
            Err(_) => warn!(
                "rmcp: the configured discovery challenge is not a valid header value — \
                 clients will be unable to discover the authorization server"
            ),
        }
    }
    response
}

/// The `403` for a VALID token that lacks a scope this resource requires.
///
/// Kept next to [`unauthorized`] because the pair is easy to collapse and
/// costly to collapse. `401` means "your credential is not good here" and a
/// client responds by discarding it and authorizing afresh; `403` +
/// `insufficient_scope` means "your credential is fine and too narrow" and a
/// client responds by re-authorizing for the NAMED scopes. Answering `401` to a
/// scope problem sends the user around a consent loop that re-grants exactly
/// the scope that was already insufficient.
///
/// Unused on any live path as of RMCP-02 — `/mcp` cannot yet accept an OAuth
/// token at all, so nothing can currently present a valid-but-narrow one. It
/// lands here, with the challenge builder it belongs to, so RMCP-05's token
/// validation has one shape to call rather than inventing a second.
pub(crate) fn insufficient_scope(
    discovery: &crate::oauth::metadata::Discovery,
    required_scope: &str,
) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        [("content-type", "application/json")],
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {"code": -32003, "message": "Insufficient scope"}
        })
        .to_string(),
    )
        .into_response();

    if let Ok(value) =
        axum::http::HeaderValue::from_str(&discovery.insufficient_scope_challenge(required_scope))
    {
        response
            .headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, value);
    }
    response
}

/// Whether this request may proceed past the door.
///
/// `has_transport_identity` is true when the listener attached a
/// server-verified identity to the request — an mTLS client certificate or a
/// resolved tailnet WhoIs. It is NOT derived from any header: both extensions
/// are inserted by the listener itself post-handshake, so a client cannot set
/// one (the same property `resolve_principal` relies on).
///
/// RMCP-02, review round 2 — the defect this parameter exists to close. The
/// `None` arm below is the legacy "no token configured, so everyone is
/// authorized" posture, and it is correct for a loopback/mTLS-only host. But
/// combined with an ENABLED OAuth door it was a hole of exactly the shape this
/// whole item exists to prevent: a first-time operator sets
/// `RMCP_OAUTH_RESOURCE`, does not set a legacy `TERMINUS_AUTH_TOKEN`
/// (there is no reason they would — they are configuring OAuth), and an
/// uncredentialed `/mcp` answers `200`. Discovery is entirely `401`-driven and
/// a `WWW-Authenticate` on a `200` is discarded, so the connector fails with a
/// generic "couldn't reach the MCP server" while the metadata documents sit
/// there being served perfectly. The header would have been right, the status
/// code wrong, and the status code is the part that carries the meaning.
///
/// So when the door is enabled, the open arm narrows: a caller the transport
/// layer vouched for still passes (this must not break the mTLS and tailnet
/// callers an existing gateway serves), and anything else — which is precisely
/// the shape of a request arriving from the public internet — is refused so it
/// receives the challenge.
///
/// A bearer token presented in that state is deliberately NOT accepted either.
/// Nothing in this item can validate an OAuth access token (that is RMCP-05),
/// and honouring an unvalidated one would be a live unauthenticated path, which
/// is a far worse outcome than a client being told to authorize again.
fn is_authorized(state: &McpServerState, headers: &HeaderMap, has_transport_identity: bool) -> bool {
    let Some(expected) = &state.auth_token else {
        // No shared token configured. Open, UNLESS ANY OAuth door is on — see
        // this function's doc, and [`McpServerState::oauth_door_enabled`] for
        // why this asks a named question rather than naming a field.
        return !state.oauth_door_enabled() || has_transport_identity;
    };
    let Some(got) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    got.strip_prefix("Bearer ") == Some(expected.as_str())
}

/// RMCP-05: the OAuth rejection rendered as a response — the same JSON-RPC
/// error body [`unauthorized`] produces, plus the `WWW-Authenticate` challenge
/// that makes the failure ACTIONABLE.
///
/// The challenge is what turns an expired token from a dead end into a
/// refresh: a hosted client re-authorizes reactively off this header, so
/// omitting it would strand the user until they noticed and reconnected by
/// hand. It is deliberately absent for exactly one rejection — see
/// `ResourceRejection::Unavailable`.
fn oauth_unauthorized(
    server: &crate::oauth::resource::OauthResourceServer,
    rejection: crate::oauth::resource::ResourceRejection,
) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": {"code": -32001, "message": "Unauthorized"}
    })
    .to_string();
    let mut response = (rejection.status(), body).into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", axum::http::HeaderValue::from_static("application/json"));
    if let Some(challenge) = server.config().challenge(rejection) {
        // The challenge is built entirely from fixed strings and a
        // startup-validated URI (see `ResourceServerConfig::challenge`), so
        // this cannot fail; if it somehow did, dropping the header is the
        // right degradation — the request is still refused.
        if let Ok(value) = axum::http::HeaderValue::from_str(&challenge) {
            headers.insert(axum::http::header::WWW_AUTHENTICATE, value);
        }
    }
    response
}

/// RMCP-05: whether this request is a candidate for OAuth bearer validation.
///
/// Four conditions, each ruling out a different way the new door could have
/// changed something that already worked:
/// - the resource server is configured at all (otherwise the door does not
///   exist and nothing here runs);
/// - NO transport identity was presented. A cert or tailnet caller resolves
///   through the established path, whether or not it also happens to carry a
///   bearer token — the precedence rule in `crate::mesh::principal`, enforced
///   here a second time so a bearer token is not even LOOKED AT when a
///   stronger identity is present;
/// - an `Authorization` header is present at all — with nothing to validate,
///   the legacy posture decides the request exactly as it did before;
/// - the credential is not the legacy shared `auth_token`. That token is this
///   server's pre-existing static credential, not an OAuth access token, and
///   running it through JWT verification would turn every legacy caller into a
///   401.
///
/// A header that IS present but is not a well-formed single Bearer credential
/// is still this door's business (`None` from
/// [`crate::oauth::resource::bearer_credential`] ⇒ candidate). Letting it fall
/// through to the legacy check would mean a malformed credential is decided by
/// whatever posture that path happens to have — which, on a host with no
/// static token configured, is "admit everyone".
///
/// Takes the two pieces of state it needs rather than the whole
/// `McpServerState`, so the precedence and case-matching rules are unit-testable
/// without standing up a server. Review round 1 found a case-sensitivity seam
/// here that a test would have caught; making it testable is half the fix, and
/// routing BOTH paths through
/// [`crate::oauth::resource::bearer_credential`] — one definition of what a
/// bearer header is, not two — is the other half.
fn oauth_is_candidate(
    oauth_configured: bool,
    legacy_token: Option<&str>,
    headers: &HeaderMap,
    cert: Option<&ClientIdentity>,
    tailnet: Option<&TailnetIdentity>,
) -> bool {
    if !oauth_configured || cert.is_some() || tailnet.is_some() {
        return false;
    }
    if !crate::oauth::resource::authorization_header_present(headers) {
        return false;
    }
    match crate::oauth::resource::bearer_credential(headers) {
        Some(presented) => legacy_token != Some(presented),
        None => true,
    }
}

/// RMCP-05: combine the transport-resolved principal and the OAuth-resolved
/// caller into the ONE [`CallerBinding`] every authorization decision for this
/// request is made from.
///
/// The precedence is the third statement of the same rule (the resolver
/// enforces it, `oauth_is_candidate` refuses to even look at a token when a
/// transport identity is present, and this refuses to let one win): a
/// transport-resolved principal is kept AS IS, and the OAuth caller is used
/// only when there is none. So a bearer token cannot displace a stronger
/// identity even if some future call site forgets one of the other two checks.
///
/// Extracted from the handler so the rule is unit-testable. The property that
/// matters most — the connector is carried but is NOT the identity — is
/// invisible while this is spelled inline, and an untested separation is a
/// separation by assertion only.
pub(crate) fn caller_binding(
    principal: Option<Principal>,
    oauth: Option<crate::oauth::resource::OauthCaller>,
) -> CallerBinding {
    match oauth {
        Some(caller) if principal.is_none() => {
            let client_id = caller.client_id().to_string();
            CallerBinding::oauth(caller.into_principal(), client_id)
        }
        // Includes the both-present case: the transport identity wins and the
        // OAuth caller is dropped entirely rather than contributing a
        // connector ceiling to somebody else's identity.
        _ => CallerBinding::transport(principal),
    }
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
    // RMCP-05: needed ONLY to detect — and refuse — an access token in the
    // query string. Nothing in this handler ever reads a credential from it.
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    // RMCP-02: the two transport identities are read here, BEFORE dispatch,
    // purely as a presence check — `resolve_principal` below still does the
    // real resolution. Both are listener-inserted extensions, never headers, so
    // a public-internet caller cannot manufacture either one.
    let has_transport_identity = identity.is_some() || tailnet.is_some();

    // RMCP-05: FIRST, before any identity source is even selected — a
    // credential in the URL is refused and audited for EVERY request, whatever
    // door it came through and whether or not it would otherwise have been
    // authenticated. The harm is that the token has already been written to
    // access logs, `Referer` headers, proxy caches and shell history by the
    // time it arrives; a client certificate on the same request does not
    // un-leak it. Gating this on the OAuth branch (as the first revision of
    // this item did) silently exempted exactly the callers that took a
    // different door.
    if let Err(rejection) = crate::oauth::resource::refuse_query_string_token(uri.query()) {
        return match state.oauth_resource.as_deref() {
            Some(server) => oauth_unauthorized(server, rejection),
            // No OAuth door configured, so there is no challenge to issue and
            // nothing to discover — but the refusal and its audit record still
            // stand. A leaked credential is not less leaked on a host that
            // does not speak OAuth.
            None => unauthorized(state.rmcp_discovery.as_deref()),
        };
    }

    let cert = identity.as_ref().map(|Extension(i)| i);
    let tailnet_identity = tailnet.as_ref().map(|Extension(t)| t);

    // RMCP-05: the fourth door. Attempted only for a request that presented no
    // transport identity at all (see `oauth_is_candidate`), so an mTLS or
    // tailnet caller reaches the unchanged path below without a bearer token
    // ever being inspected — a token can therefore never displace, or be
    // needed alongside, a stronger identity.
    let mut oauth_caller = None;
    if oauth_is_candidate(
        state.oauth_resource.is_some(),
        state.auth_token.as_deref(),
        &headers,
        cert,
        tailnet_identity,
    ) {
        let server = state.oauth_resource.clone().expect("candidate implies configured");
        match server
            .authenticate(&state.principal_resolver, &headers, uri.query())
            .await
        {
            Ok(caller) => oauth_caller = Some(caller),
            // A presented-but-refused OAuth credential is refused HERE rather
            // than falling through to the legacy check. Falling through would
            // mean a token that failed the audience check could still be
            // admitted by whatever posture the legacy path happens to have —
            // which, on a host with no static token configured, is "admit
            // everyone". A failed authentication must never resolve to a
            // weaker authentication.
            Err(rejection) => return oauth_unauthorized(&server, rejection),
        }
    }

    // RMCP-02 owns the admission decision, including the narrowing that closes
    // the tokenless open arm once ANY OAuth door is configured. This item does
    // not restate it — there is one `is_authorized`, called once, here.
    //
    // Two things about the placement. It runs AFTER the OAuth attempt because
    // an OAuth caller is precisely the case RMCP-02's narrowing refuses: with a
    // door enabled and no transport identity, `is_authorized` returns false, so
    // checking first would 401 the very callers this item exists to admit. And
    // `oauth_caller.is_none()` is a SKIP for callers already authenticated by
    // this item, never a widening — a request that failed OAuth authentication
    // returned above and never reaches this line, so the only way to get past
    // it without a validated token is to satisfy RMCP-02's own rule.
    if oauth_caller.is_none() && !is_authorized(&state, &headers, has_transport_identity) {
        return unauthorized(state.rmcp_discovery.as_deref());
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
    let principal = resolve_principal(&state.principal_resolver, cert, tailnet_identity);

    // RMCP-05: fold the transport identity and the OAuth caller into the ONE
    // `CallerBinding` that every authorization decision below is made from —
    // the `tools/list` catalog filter and the `tools/call` gate both take it,
    // so RMCP-07's intersection has a single value to read and cannot end up
    // gating on one axis while filtering on the other.
    let binding = caller_binding(principal, oauth_caller);
    if let Some(client_id) = binding.oauth_client_id() {
        // Recorded as soon as it is known: which CONNECTOR a call arrived
        // through is exactly what an operator reviewing an internet-facing
        // door needs, and it is the input RMCP-07 will intersect against.
        debug!(
            target: "gateway_audit",
            principal = binding.principal().map(Principal::name).unwrap_or("<none>"),
            oauth_client_id = client_id,
            "oauth caller authenticated"
        );
    }
    // Borrowed FROM the binding, not cloned out of it: the binding owns the
    // request's identity for the rest of this handler, so there is no second
    // copy that could drift from it and no path that reaches a gate with the
    // connector quietly stripped off.
    let principal = binding.principal();

    // RMCP-07: the connector scope for this request, read ONCE like the
    // principal above and used by BOTH the `tools/list` filter and the
    // `tools/call` gate below — the same value through the same
    // `crate::oauth::scope::decide`, so visibility and callability cannot
    // disagree within a request.
    // RMCP-07: the connector scope for this request, resolved ONCE from the
    // `client_id` RMCP-05 bound to the token, and used by BOTH the
    // `tools/list` filter and the `tools/call` gate below — the same value
    // through the same `crate::oauth::scope::decide`, so visibility and
    // callability cannot disagree within a request.
    //
    // `None` means "this request has no OAuth connector" (every mTLS, tailnet
    // and loopback caller) and leaves the account grant to decide alone. It
    // never means "an unscoped connector": resolution never yields absence —
    // an unknown, disabled or unscoped client resolves to an EMPTY
    // `ClientScope`, which permits nothing. Conflating the two in either
    // direction is a bug; read as "empty" it would deny the entire fleet, and
    // the reverse is the widening this item exists to prevent.
    let client_scope: Option<Arc<crate::oauth::scope::ClientScope>> =
        match binding.oauth_client_id() {
            None => None,
            Some(client_id) => Some(match &state.scope_resolver {
                Some(resolver) => resolver.scope_for(client_id).await,
                // A connector arrived but nothing can resolve what it may
                // reach. That is a misconfiguration, and the only safe reading
                // of it is the empty scope — never "unscoped, therefore
                // unrestricted".
                None => {
                    tracing::warn!(
                        oauth_client_id = %client_id,
                        "rmcp: no scope resolver is configured, so this connector's scope \
                         cannot be read; resolving to the empty scope"
                    );
                    Arc::new(crate::oauth::scope::ClientScope::empty(client_id))
                }
            }),
        };

    // TERM #595: and WHICH HUMAN this turn is being run for, if a trusted
    // principal said so. Resolved ONCE per request from the same server-verified
    // principal above plus a SIGNED, principal-bound assertion -- never from a
    // bare header value, which is why an intermediary that merely relays the
    // token (Chord) cannot invent or alter one.
    //
    // The tri-state matters: `None` (no header) is the unchanged, service-scoped
    // pre-#595 path; `Rejected` is strictly LESS privilege than that. A gateway
    // that is not configured at all cannot check a grant, so a token presented
    // to it is refused rather than honoured -- absence of a policy is never a
    // reason to trust a claim.
    //
    // The PLAINTEXT `X-Terminus-On-Behalf-Of` header is never authoritative
    // HERE. It is meaningful at exactly one hop -- the inference-proxy ingress,
    // where the caller is mutually authenticated and its right to speak for
    // someone else is a grant-map decision -- and is translated there into a
    // signed assertion. Arriving on this endpoint it is an identity claim this
    // handler cannot honour, so it is REFUSED rather than ignored.
    //
    // Ignoring it would be a silent WIDENING: a caller that believes it is
    // acting as one person would transparently read and write the SHARED,
    // service-scoped record instead of that person's -- exactly the data-mixing
    // LOCREG-01 and this item exist to rule out. An attempted identity that
    // cannot be honoured must never be indistinguishable from no identity.
    let asserted =
        asserted_person_for_mcp(state.gateway.as_ref(), principal, &headers);

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
            // TMOD-04 precedence (IDENTICAL in `tools/list` here and in the
            // `tools/call` dispatch order below): compiled-in > worker-route
            // > personal-federation. A given bare tool name is advertised by
            // -- and dispatched to -- the FIRST of those three sources that
            // owns it, so `tools/list` and `tools/call` never disagree about
            // which implementation a name resolves to.
            //
            // Worker routes are therefore merged BEFORE the personal set:
            // `merge_catalog` skips any route whose name collides with a
            // tool already in `tools` (so far, only compiled-in), so
            // compiled-in wins over a worker route; then the personal set
            // below is filtered to skip any name already present
            // (compiled-in OR worker-route), so a worker route wins over a
            // personal-federated tool of the same name -- matching the
            // `tools/call` order (registry miss -> worker route -> personal
            // federation). `broker_routes` empty (every deployment before a
            // worker is ever installed) makes this a no-op.
            tools = crate::broker::routes::merge_catalog(tools, &broker_routes).await;
            // TGW-02: aggregate in the personal-registry tool set (metadata
            // only, no network call -- see
            // `crate::registry::personal_only_tool_metadata`'s doc) when
            // this process is configured to federate personal-tool calls.
            // Per the precedence above, a personal tool whose name is already
            // served by a compiled-in tool or a worker route is dropped here
            // (that higher-precedence source is what `tools/call` dispatches
            // to), so list and call agree.
            if state.personal_federation.is_some() {
                let existing: std::collections::HashSet<String> = tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect();
                tools.extend(
                    crate::registry::personal_only_tool_metadata()
                        .into_iter()
                        .filter(|t| !existing.contains(&t.name))
                        .map(|t| {
                            json!({
                                "name": t.name,
                                "description": t.description,
                                "inputSchema": t.parameters,
                            })
                        }),
                );
            }
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
                // RMCP-05 → RMCP-07 SEAM (visibility half), now closed.
                // `client_scope` is `Some` exactly when `binding` carries a
                // connector, so the catalog a caller SEES is computed from the
                // same inputs as the calls it may MAKE.
                //
                // An OAuth request takes ONE pass, not two.
                // `filter_catalog_for_client` re-checks the account grant
                // itself (it is `decide`'s first check), so the result is
                // identical to running the principal filter first — but
                // running that filter first would strip grant-denied tools
                // BEFORE the connector filter ever saw them, and the aggregate
                // audit record would then report `denied_by_grant=0` no matter
                // what the grant did. The single pass is what makes the audit
                // counts true. A transport caller has no connector and takes
                // the account-grant-only path unchanged.
                match client_scope.as_deref() {
                    Some(scope) => {
                        tools = gateway.filter_catalog_for_client(
                            binding.principal(),
                            Some(scope),
                            tools,
                        );
                    }
                    None => {
                        tools = gateway.filter_catalog_for_principal(binding.principal(), tools);
                    }
                }
            } else if let Some(scope) = client_scope.as_deref() {
                // An OAuth request reaching a process with no gateway: there is
                // no account grant to intersect WITH, so there is no safe
                // answer other than the empty one. The alternative — falling
                // back to the connector scope alone — would be the widening
                // this item exists to prevent, because a connector scope is
                // only ever meaningful as the narrow side of an intersection.
                //
                // Audited, not merely logged. Round 3 found this branch
                // substituting an empty catalog with no audit record — the same
                // gap, in the same shape, as the gateway-less `tools/call`
                // branch that round 1 found. A connector that suddenly sees
                // nothing is the case an operator has to diagnose, so it emits
                // the same aggregate record as every other filtered list.
                let mut tally = crate::oauth::scope::ListFilterTally::default();
                for _ in tools.iter() {
                    tally.record_no_account_grant();
                }
                crate::gateway_framework::audit_client_scope_list(
                    binding.principal(),
                    scope,
                    &tally,
                );
                tools = Vec::new();
            }
            // TAVAIL-01: availability filter — COMPOSES WITH the authorization filter
            // above, never replaces it. Authorization answers "may THIS principal use
            // it"; availability answers "does this tool work at all, for anyone". A
            // tool is advertised only if BOTH allow it, and availability can only ever
            // REMOVE (it never re-grants something the gateway just filtered out).
            //
            // Applied unconditionally — including when `state.gateway` is None — because
            // a dead backend is dead regardless of whether this process gates identity.
            // With no `TERMINUS_TOOL_AVAILABILITY_JSON` configured the policy is empty
            // and this is a byte-for-byte no-op.
            let avail = crate::availability::policy();
            if !avail.is_empty() {
                let before = tools.len();
                tools.retain(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| avail.agent_usable(n))
                        .unwrap_or(true)
                });
                let hidden = before - tools.len();
                if hidden > 0 {
                    tracing::debug!("availability: hid {hidden} unavailable tool(s) from tools/list");
                }
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
                principal.map(|p| p.name().to_string()).unwrap_or_else(|| ANONYMOUS_IDENTITY.to_string());
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
            // RMCP-07: the connector-scope gate, run BEFORE `guard()` and
            // through the SAME `crate::oauth::scope::decide` that filtered the
            // catalog above — so a tool `tools/list` hid is never callable and
            // a tool it advertised is never refused here. It denies in the same
            // JSON-RPC `isError` shape as the gate below and audits with a
            // machine-readable reason (`no_group` / `no_namespace` /
            // `denied_by_grant`), so exactly one audit entry is still written
            // per request. A no-op for every non-OAuth caller.
            //
            // Ordering: before `guard()` for the same reason `guard()` runs
            // authorization before rate limiting — a call the caller was never
            // permitted to make should not consume its budget. The mirror of
            // the `tools/list` branch above, an OAuth request on a
            // gateway-less process, is denied here for the same reason.
            if let Some(scope) = client_scope.as_deref() {
                let denial_text = match &state.gateway {
                    Some(gateway) => gateway
                        .guard_client_scope(binding.principal(), Some(scope), name)
                        .err(),
                    // No gateway means no account grant to intersect with, so
                    // there is no safe answer but the empty one. Round 1
                    // finding 3: this used to deny WITHOUT an audit record,
                    // which made a real denial undiagnosable. It now emits the
                    // same record as every other connector-scope denial, with
                    // its own reason code.
                    None => Some(crate::gateway_framework::audit_client_scope_denial(
                        binding.principal(),
                        scope,
                        name,
                        crate::oauth::scope::DENY_NO_ACCOUNT_GRANT,
                    )),
                };
                if let Some(denial_text) = denial_text {
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

            let gate_ctx = if let Some(gateway) = &state.gateway {
                // RMCP-05 → RMCP-07 SEAM (enforcement half), the twin of the
                // `tools/list` filter above and fed from the SAME `binding`.
                // The connector is deliberately NOT part of the identity passed
                // here: `binding.principal()` is the same value whether this
                // caller arrived through one client or another, so a connector
                // can only ever narrow (RMCP-07) and never widen what the
                // account itself was granted.
                match gateway.guard(binding.principal(), name, ActionKind::Tool).await {
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

            // TAVAIL-01: availability gate — AFTER authorization, BEFORE dispatch.
            //
            // Ordering is deliberate and was corrected in review: running this BEFORE
            // `guard()` leaked existence, letting an UNAUTHORIZED caller learn that a
            // tool exists and read the operator's reason for parking it. Authorization
            // decides what you may know about; availability then decides whether the
            // thing you are allowed to use actually works.
            //
            // Still before dispatch, because the `tools/list` filter alone is not
            // enough: an agent holding a catalog cached from before a tool was parked
            // would otherwise still invoke it. The list filter stops the model being
            // tempted; THIS is the gate that protects.
            //
            // The refusal names the state + reason rather than "not found" — a
            // "not found" is what sends a model hunting for the tool, which is exactly
            // how the `deep_research` phantom burned loop turns.
            {
                let avail = crate::availability::policy();
                if !name.is_empty() && !avail.agent_usable(name) {
                    let detail = avail.denial_message(name);
                    tracing::warn!("availability: refused tools/call for unavailable tool {name}");
                    return sse_response(
                        id,
                        Ok(json!({
                            "content": [{"type": "text", "text": detail}],
                            "isError": true
                        })),
                        "",
                    );
                }
            }

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

            // PROMEX-01: capture the CONFIGURED upstream namespace (if any)
            // from the RESOLVED route — `Some` only for a real Upstream/
            // Unavailable route (a configured, bounded namespace), so an
            // unknown `foo__bar` name (which resolves to `Local`) yields
            // `None` and can never smuggle an arbitrary or secret-shaped
            // prefix into the `tool` metric label. Borrow only — the owning
            // `mesh_route` is still consumed by the dispatch match below.
            let metric_ns: Option<String> = match &mesh_route {
                Some(CallRoute::Upstream { client, .. }) => Some(client.namespace().to_string()),
                Some(CallRoute::Unavailable { namespace }) => Some(namespace.clone()),
                _ => None,
            };

            // MESH-10: set when dispatch couldn't even reach an upstream at
            // the transport level (unhealthy/unregistered mesh upstream, or
            // a network-level failure calling one that IS registered) --
            // audited below as `AuditDecision::TransportFailure`, never
            // silently dropped, and kept distinct from an ordinary
            // application-level tool error (`success: false` with the
            // default `Allow` decision).
            let mut is_transport_failure = false;

            // PROMEX-01: time the ENTIRE dispatch below (mesh upstream, the
            // local core registry, a broker worker route, personal
            // federation, or "unknown tool") in one central place, rather
            // than instrumenting each branch separately -- this is the
            // single point every `tools/call` outcome (`response`,
            // `success`, `detail`) already funnels through for the audit
            // log just below, so it is the natural place to also record
            // `terminus_tool_calls_total`/`terminus_tool_duration_seconds`.
            let dispatch_started = std::time::Instant::now();

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
                .call_with_caller_key(
                    name,
                    arguments.clone(),
                    // TRTR-05: the same server-verified principal `guard()` just
                    // authorized above decides what OPERATOR context a tool may
                    // use on this caller's behalf. No gateway = no verified
                    // identity = the fail-closed default.
                    //
                    // TERM #595: and, layered on top of that decision, WHICH
                    // HUMAN the turn is for. A verified person NARROWS the
                    // context (entitlements are intersected with that person's
                    // own grants, and the media account resolves from the person
                    // with no fallback to the principal); a refused assertion
                    // strips it below the service identity. See
                    // `GatewayFramework::caller_context_for_person`.
                    match &state.gateway {
                        Some(gw) => gw.caller_context_for_person(principal, &asserted),
                        // No gateway: no grant map, so nothing could have been
                        // verified. An unevaluated assertion must land BELOW the
                        // service default, not on it.
                        None if matches!(asserted, crate::mesh::AssertedPerson::None) => {
                            crate::tool::CallerContext::default()
                        }
                        None => crate::tool::CallerContext::unidentified(),
                    },
                    // LOCREG-01 x TERM #595: and WHICH per-caller record is
                    // theirs. The person, when one was verified, is part of the
                    // key -- so two people behind the same service principal
                    // file under different storage keys and neither can read the
                    // other's saved home.
                    //
                    // A REJECTED assertion gets NO key at all. Falling back to
                    // the service key here would hand the shared, pre-#577
                    // record to exactly the caller whose identity we just
                    // refused to believe -- the same inversion `for_person`
                    // documents for a blank person, arrived at from the other
                    // direction. No key means `Lookup::Denied`, which is the
                    // only safe answer to "who is this?" when the answer failed
                    // verification.
                    caller_key_for(principal, &asserted),
                )
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
                        // LIMITATION: every `WorkerTransport::call` failure is
                        // audited as a transport failure -- an unhealthy
                        // worker AND an application-level tool error the
                        // worker deliberately returned both collapse to
                        // `ToolError::Execution` at the TMOD-02 transport
                        // boundary, so they're indistinguishable here.
                        // Splitting them apart requires a TMOD-02
                        // transport-contract change (a distinct app-error
                        // wire shape); that is a documented follow-up, not
                        // part of TMOD-04.
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
                        match client.call_tool(name, arguments, principal).await {
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

            // PROMEX-01: record the tool call under a BOUNDED label. The raw
            // `name` is caller-supplied, so the label is derived only from
            // VALIDATED state: `reg.contains(name)` (a known local tool) and
            // `metric_ns` (a configured upstream namespace from the resolved
            // route). `bounded_tool_label` caps the label to {known local tool
            // names} ∪ {`<mesh:ns>` for configured ns} ∪ {`<unknown>`}.
            let metric_tool_name =
                crate::metrics::bounded_tool_label(name, reg.contains(name), metric_ns.as_deref());
            crate::metrics::record_tool_call(&metric_tool_name, success, dispatch_started.elapsed());

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

    // ---- LOCREG-01 x TERM #595: which record belongs to this turn ----
    //
    // These cover `caller_key_for`, the join between the two items. The
    // `Rejected` arm is the reason the function was extracted at all: it is the
    // fail-closed branch, and a fail-closed branch nobody exercises is only
    // fail-closed by assertion.
    // ---- TERM #595: an attempted identity that cannot be honoured ----
    //
    // The failure these cover is a SILENT WIDENING, not an escalation: a caller
    // that thinks it is acting as one person, whose claim is quietly dropped,
    // reads and writes the SHARED service-scoped record. That is the exact
    // data-mixing LOCREG-01 exists to prevent, so "ignored" is not an acceptable
    // way to handle a claim we cannot honour.
    mod unhonourable_claims {
        use crate::mcp_server::asserted_person_for_mcp;
        use crate::mesh::person::{ON_BEHALF_OF_HEADER, PERSON_ASSERTION_HEADER};
        use crate::mesh::{AssertedPerson, Principal, PrincipalSource};
        use axum::http::{HeaderMap, HeaderValue};

        fn principal() -> Principal {
            Principal::new("lumina", PrincipalSource::MtlsCert)
        }

        /// POSITIVE CONTROL: with no identity headers at all, an ungatewayed
        /// process still takes the unchanged pre-#595 service-scoped path. A
        /// build that refused everything would fail here.
        #[test]
        fn no_headers_is_the_unchanged_service_path() {
            let p = principal();
            assert!(matches!(
                asserted_person_for_mcp(None, Some(&p), &HeaderMap::new()),
                AssertedPerson::None
            ));
        }

        /// The plaintext on-behalf-of header is only honourable at the proxy
        /// ingress. Presented to the MCP endpoint it must be REFUSED, not
        /// ignored -- ignoring it silently runs the turn as the shared service.
        #[test]
        fn a_plaintext_on_behalf_of_is_refused_here_not_ignored() {
            let p = principal();
            let mut h = HeaderMap::new();
            // pii-test-fixture: invented household name
            h.insert(ON_BEHALF_OF_HEADER, HeaderValue::from_static("alice"));
            assert!(
                matches!(asserted_person_for_mcp(None, Some(&p), &h), AssertedPerson::Rejected),
                "an identity claim this endpoint cannot honour must not read as no claim"
            );
        }

        /// And it outranks the signed header: a request carrying both must not
        /// be able to launder an unhonourable plaintext claim past the check by
        /// also presenting a token.
        #[test]
        fn the_plaintext_claim_is_refused_even_alongside_a_token() {
            let p = principal();
            let mut h = HeaderMap::new();
            h.insert(ON_BEHALF_OF_HEADER, HeaderValue::from_static("alice")); // pii-test-fixture
            h.insert(PERSON_ASSERTION_HEADER, HeaderValue::from_static("any.token.here"));
            assert!(matches!(
                asserted_person_for_mcp(None, Some(&p), &h),
                AssertedPerson::Rejected
            ));
        }

        /// A blank on-behalf-of is still an ATTEMPT (`Some("")` per
        /// `on_behalf_of_header`'s tri-state contract), so it is refused rather
        /// than treated as absent.
        #[test]
        fn a_blank_on_behalf_of_is_an_attempt_not_an_absence() {
            let p = principal();
            let mut h = HeaderMap::new();
            h.insert(ON_BEHALF_OF_HEADER, HeaderValue::from_static("   "));
            assert!(matches!(
                asserted_person_for_mcp(None, Some(&p), &h),
                AssertedPerson::Rejected
            ));
        }

        /// A token presented to a process with no gateway cannot have its grant
        /// checked, so it is refused. Absence of a policy is never a reason to
        /// trust a claim.
        #[test]
        fn a_token_without_a_gateway_to_check_it_is_refused() {
            let p = principal();
            let mut h = HeaderMap::new();
            h.insert(PERSON_ASSERTION_HEADER, HeaderValue::from_static("any.token.here"));
            assert!(matches!(
                asserted_person_for_mcp(None, Some(&p), &h),
                AssertedPerson::Rejected
            ));
        }
    }

    mod caller_key {
        use crate::mcp_server::caller_key_for;
        use crate::mesh::person::{mint, verify};
        use crate::mesh::{AssertedPerson, Principal, PrincipalSource};
        use serial_test::serial;

        const KEY: &str = "term595-callerkey-test-key"; // pii-test-fixture: invented test key

        fn configure() {
            std::env::set_var(crate::mesh::person::SIGNING_KEY_ENV, KEY);
            // pii-test-fixture: invented household names, not a real roster
            std::env::set_var(crate::mesh::person::ROSTER_ENV, "alice,bob");
        }

        fn unconfigure() {
            std::env::remove_var(crate::mesh::person::SIGNING_KEY_ENV);
            std::env::remove_var(crate::mesh::person::ROSTER_ENV);
        }

        fn principal(name: &str) -> Principal {
            Principal::new(name, PrincipalSource::MtlsCert)
        }

        fn verified(person: &str) -> AssertedPerson {
            let token = mint("lumina", person).expect("minting must succeed");
            AssertedPerson::Verified(verify(&token, Some("lumina")).expect("must verify"))
        }

        /// POSITIVE CONTROL. Without an assertion the legacy, service-scoped key
        /// is still produced — a build that returned `None` everywhere would
        /// pass the fail-closed tests below while breaking every existing
        /// caller, and this is what catches that.
        #[test]
        fn no_assertion_still_yields_the_legacy_service_key() {
            let p = principal("lumina");
            let key = caller_key_for(Some(&p), &AssertedPerson::None)
                .expect("a service-scoped caller must still get its key");
            assert!(!key.is_person_scoped(), "no assertion means no person in the key");
        }

        /// The point of the item: two people behind ONE service principal file
        /// under DIFFERENT storage keys, so neither can read the other's saved
        /// home.
        #[test]
        #[serial]
        fn two_verified_people_get_different_storage_keys() {
            configure();
            let p = principal("lumina");
            let a = caller_key_for(Some(&p), &verified("alice")).expect("alice must key");
            let b = caller_key_for(Some(&p), &verified("bob")).expect("bob must key");
            assert!(a.is_person_scoped() && b.is_person_scoped());
            assert_ne!(
                a.storage_key(),
                b.storage_key(),
                "two people behind one principal must not share a record"
            );
            unconfigure();
        }

        /// A verified person's key is also distinct from the SERVICE key for the
        /// same principal — otherwise "per-person" would collapse back onto the
        /// shared pre-#577 record for whoever asserted first.
        #[test]
        #[serial]
        fn a_person_key_is_not_the_service_key() {
            configure();
            let p = principal("lumina");
            let person = caller_key_for(Some(&p), &verified("alice")).unwrap();
            let service = caller_key_for(Some(&p), &AssertedPerson::None).unwrap();
            assert_ne!(person.storage_key(), service.storage_key());
            unconfigure();
        }

        /// THE FAIL-CLOSED CASE. A refused assertion gets NO key — it must not
        /// fall back to the service key, which would hand the shared record to
        /// exactly the caller whose identity verification just refused to
        /// believe.
        #[test]
        fn a_rejected_assertion_gets_no_key_at_all() {
            let p = principal("lumina");
            assert!(
                caller_key_for(Some(&p), &AssertedPerson::Rejected).is_none(),
                "a refused identity must not be handed the shared service record"
            );
        }

        /// And the refusal does not depend on there being no principal: even
        /// with a perfectly good principal in hand, `Rejected` yields nothing.
        /// (Guards against a future edit that reads the principal first and only
        /// consults the assertion as a modifier.)
        #[test]
        fn rejection_outranks_a_usable_principal() {
            let p = principal("lumina");
            let would_have_been = caller_key_for(Some(&p), &AssertedPerson::None);
            assert!(would_have_been.is_some(), "control: this principal does key");
            assert!(caller_key_for(Some(&p), &AssertedPerson::Rejected).is_none());
        }

        /// No principal and no assertion: nothing to file under.
        #[test]
        fn no_principal_yields_no_key() {
            assert!(caller_key_for(None, &AssertedPerson::None).is_none());
            assert!(caller_key_for(None, &AssertedPerson::Rejected).is_none());
        }
    }
    use super::*;
    use crate::error::ToolError;
    use crate::tool::RustTool;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use serial_test::serial; // RMCP-02: env-mutating door-detection tests
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes,
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
                tool_cache: Default::default(),
                gateway: None,
                mesh_pool: None,
                principal_resolver: PrincipalResolver::default(),
                broker_routes,
                rmcp_discovery: None,
                rmcp_endpoints: None,
                oauth_doors: crate::oauth::metadata::OauthDoors::none(),
                oauth_resource: None,
                scope_resolver: None,
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

    /// Round-2 review: a name present as BOTH a worker route AND a
    /// personal-federated tool must be LISTED and CALLED as the SAME
    /// implementation (the worker route), per the documented precedence
    /// compiled-in > worker-route > personal-federation applied identically
    /// in `tools/list` ordering and `tools/call` dispatch order.
    #[tokio::test]
    async fn tmod04_worker_route_wins_over_personal_federation_in_list_and_call() {
        // A real personal-only tool name to collide with.
        let personal_name = crate::registry::personal_only_tool_metadata()
            .into_iter()
            .next()
            .expect("there is at least one personal-only tool")
            .name;

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let broker_routes = crate::broker::routes::RouteTable::new();
        broker_routes.install(crate::broker::routes::WorkerRoute {
            worker_id: "w1".to_string(),
            transport: Arc::new(StubWorker { healthy: true, reply: "served by worker route".to_string() }),
            tool: crate::registry::ToolInfo {
                name: personal_name.clone(),
                description: "worker-route implementation".to_string(),
                parameters: json!({"type": "object"}),
            },
        });
        let state = Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            // Points at a dead address: if the worker route did NOT win, the
            // call below would fall THROUGH to here and surface a
            // "federation error" -- which the assertions would catch.
            personal_federation: Some(
                crate::federation::PersonalFederationClient::with_base_url("http://127.0.0.1:1")
                    .with_timeout(std::time::Duration::from_millis(200)),
            ),
            inference_proxy: None,
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes,
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
        });

        // tools/list: the colliding name is advertised exactly ONCE, as the
        // worker route (not the personal-federation metadata).
        let router = build_router(state.clone());
        let (_, list_body, _) =
            post_mcp(router, json!({"jsonrpc": "2.0", "id": 50, "method": "tools/list"})).await;
        let tools = list_body["result"]["tools"].as_array().unwrap();
        let matching: Vec<&Value> = tools.iter().filter(|t| t["name"] == personal_name.as_str()).collect();
        assert_eq!(matching.len(), 1, "the colliding name must be listed exactly once");
        assert_eq!(
            matching[0]["description"], "worker-route implementation",
            "worker route wins over personal federation in tools/list"
        );

        // tools/call: dispatches to the worker route, NOT personal federation.
        let router2 = build_router(state);
        let (_, call_body, _) = post_mcp(
            router2,
            json!({
                "jsonrpc": "2.0", "id": 51, "method": "tools/call",
                "params": {"name": personal_name.clone(), "arguments": {}}
            }),
        )
        .await;
        assert_eq!(call_body["result"]["isError"], false, "worker route must serve the call cleanly");
        assert_eq!(
            call_body["result"]["content"][0]["text"], "served by worker route",
            "worker route wins over personal federation in tools/call"
        );
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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

    // ── RMCP-02: OAuth discovery and the 401 challenge ──────────────────────

    /// `auth_token` here is a test DOUBLE for a bearer credential, not a
    /// secret, and is passed as a literal on purpose.
    ///
    /// Review round 1 flagged the literal as a hardcoded credential. Held, with
    /// reasoning: it authenticates nothing, exists only inside the test binary,
    /// and grants access to a `ToolRegistry` containing one echo tool built two
    /// lines above. The S7/S8 secrets rule governs how the RUNTIME obtains real
    /// credentials — routing this through a vault accessor would test the vault
    /// rather than the router, and would leave the thing under test (does an
    /// unauthenticated `/mcp` emit a discovery challenge?) dependent on secret
    /// provisioning that has nothing to do with it. The pre-existing
    /// `test_unauthorized_when_token_configured_and_missing` above uses the
    /// same literal for the same reason; this follows that precedent rather
    /// than inventing a second convention beside it.
    fn rmcp_state(auth_token: Option<&str>) -> Arc<McpServerState> {
        use crate::oauth::metadata::{CanonicalUri, Discovery, OauthDoors};

        // A discovery surface built in code rather than read from the
        // environment, so it registers itself explicitly.
        let mut doors = OauthDoors::none();
        doors.register("rmcp-discovery (test fixture)");

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let discovery = Discovery::new(
            CanonicalUri::parse("TEST", "https://connector.test/mcp").unwrap(),
            CanonicalUri::parse("TEST", "https://connector.test").unwrap(),
            false,
            vec!["mcp".to_string(), "offline_access".to_string()],
            "mcp".to_string(),
            false,
        )
        .unwrap();
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: auth_token.map(str::to_string),
            personal_federation: None,
            inference_proxy: None,
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: Some(Arc::new(discovery)),
            rmcp_endpoints: None,
            oauth_doors: doors,
            oauth_resource: None,
            scope_resolver: None,
        })
    }

    /// THE acceptance criterion for this item. An unauthenticated `/mcp` must
    /// return `401` — never `200` — carrying a `WWW-Authenticate` whose
    /// `resource_metadata` URL is where the document actually is. That header
    /// is the ONLY way a hosted client learns which authorization server to
    /// use, and it is honoured only on a `401`: attached to a `200` it is
    /// discarded and the user sees a generic "couldn't reach the MCP server".
    #[tokio::test]
    async fn rmcp02_unauthenticated_mcp_returns_a_401_challenge() {
        let state = rmcp_state(Some("secret-abc"));
        let expected = state
            .rmcp_discovery
            .as_ref()
            .unwrap()
            .protected_resource_url()
            .to_string();
        let (status, _body, headers) = post_mcp(
            build_router(state),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED, "must be 401, never 200");
        let challenge = headers
            .get("www-authenticate")
            .expect("a 401 without a challenge is undiscoverable")
            .to_str()
            .expect("header must be printable ASCII");
        assert!(challenge.starts_with("Bearer "));
        assert!(
            challenge.contains(&format!("resource_metadata=\"{expected}\"")),
            "challenge must point at the served document: {challenge}"
        );
        assert!(challenge.contains("scope=\"mcp\""), "{challenge}");
    }

    /// The `resource_metadata` URL in the challenge and the path this router
    /// mounts are derived from the same configuration, but nothing structural
    /// forces them to agree — so the loop is closed here: take the URL out of
    /// the live challenge header and fetch it from the live router.
    #[tokio::test]
    async fn rmcp02_the_challenges_metadata_url_is_actually_mounted() {
        let state = rmcp_state(Some("secret-abc"));
        let origin = state
            .rmcp_discovery
            .as_ref()
            .unwrap()
            .resource()
            .origin()
            .to_string();
        let router = build_router(state);

        let (_status, _body, headers) = post_mcp(
            router.clone(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        let challenge = headers.get("www-authenticate").unwrap().to_str().unwrap();
        let advertised = challenge
            .split("resource_metadata=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("challenge carries a metadata URL")
            .to_string();
        let path = advertised
            .strip_prefix(&origin)
            .expect("the advertised URL is on this server's own origin");

        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "advertised path {path}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let doc: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["resource"], json!("https://connector.test/mcp"));
        assert_eq!(doc["authorization_servers"], json!(["https://connector.test"]));
    }

    /// Discovery is unauthenticated even when `/mcp` is not — requiring a
    /// credential to discover how to obtain a credential is a loop with no
    /// entry point.
    #[tokio::test]
    async fn rmcp02_discovery_is_reachable_without_credentials() {
        let router = build_router(rmcp_state(Some("secret-abc")));
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "unauthenticated GET {path}");
        }
    }

    /// A deployment that has not configured the door must be byte-for-byte
    /// what it was before this item: no discovery routes, and a bare `401`
    /// with no challenge. A stray `WWW-Authenticate` on an unconfigured server
    /// would advertise an authorization server that does not exist.
    #[tokio::test]
    async fn rmcp02_an_unconfigured_door_adds_nothing() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-personal-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: Some("secret-abc".to_string()),
            personal_federation: None,
            inference_proxy: None,
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
        });
        let router = build_router(state);

        let (status, _body, headers) = post_mcp(
            router.clone(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            !headers.contains_key("www-authenticate"),
            "an unconfigured server must not advertise an authorization server"
        );

        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Deliberately NOT an assertion on the status code: the constellation
        // router installs an SPA fallback, so an unmounted path may answer
        // `200 index.html` rather than `404` depending on whether a web bundle
        // is embedded in this build. What matters — and what is asserted — is
        // that nothing on this path is a protected-resource metadata document.
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("authorization_servers"),
            "the discovery routes must not be mounted at all: {body}"
        );
    }

    /// A successfully authenticated call must NOT carry the challenge: a
    /// `WWW-Authenticate` on a `200` is ignored by clients anyway, and emitting
    /// one would make a working connection look like a failing one to anything
    /// that logs on the header's presence.
    #[tokio::test]
    async fn rmcp02_an_authorized_call_carries_no_challenge() {
        let router = build_router(rmcp_state(Some("secret-abc")));
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
        assert!(!resp.headers().contains_key("www-authenticate"));
    }

    /// REVIEW ROUND 2, and the deployment shape a first-time operator will
    /// actually have: the OAuth door enabled and NO legacy `auth_token` set —
    /// because someone configuring OAuth has no reason to also set a shared
    /// bearer token.
    ///
    /// Before the fix this answered `200`. That is the single worst outcome
    /// available to this item: the whole discovery flow is `401`-driven, a
    /// `WWW-Authenticate` on a `200` is discarded by the client, and the
    /// operator sees a generic "couldn't reach the MCP server" while the
    /// metadata documents are being served perfectly. The header was right and
    /// the status code was wrong, and the status code is what carries the
    /// meaning.
    #[tokio::test]
    async fn rmcp02_door_enabled_without_a_legacy_token_still_challenges() {
        let (status, _body, headers) = post_mcp(
            build_router(rmcp_state(None)),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an uncredentialed call to an OAuth-enabled door must never be 200"
        );
        assert!(
            headers
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|c| c.contains("resource_metadata=")),
            "and it must carry the challenge that bootstraps discovery"
        );
    }

    /// A bearer token presented in that same state must not be honoured either.
    /// Nothing in this item can validate an OAuth access token (RMCP-05 owns
    /// that), and accepting an unvalidated one would be a live unauthenticated
    /// path — strictly worse than telling the client to authorize again.
    #[tokio::test]
    async fn rmcp02_an_unvalidatable_bearer_is_not_honoured() {
        let router = build_router(rmcp_state(None));
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("authorization", "Bearer not-a-validatable-access-token")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key("www-authenticate"));
    }

    /// The narrowing must not break the callers a gateway already serves. An
    /// mTLS or tailnet caller is vouched for by the LISTENER — an extension
    /// inserted post-handshake, which a public-internet client cannot
    /// manufacture — so it keeps the pre-RMCP-02 open posture.
    #[tokio::test]
    async fn rmcp02_a_transport_verified_caller_is_unaffected() {
        for tag in ["mtls", "tailnet"] {
            let router = build_router(rmcp_state(None));
            let mut req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
                ))
                .unwrap();
            match tag {
                "mtls" => {
                    req.extensions_mut()
                        .insert(ClientIdentity("a-enrolled-service".to_string()));
                }
                _ => {
                    req.extensions_mut().insert(TailnetIdentity {
                        login: "a-tailnet-account".to_string(),
                        node: "a-tailnet-peer".to_string(),
                        tags: vec![],
                    });
                }
            }
            let resp = router.oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "a {tag}-verified caller must keep working"
            );
        }
    }

    /// And a deployment with NO door and no token keeps the legacy open
    /// posture exactly — the narrowing above is gated on the door, not on the
    /// absence of a token.
    #[tokio::test]
    async fn rmcp02_no_door_and_no_token_stays_open() {
        let (status, _body, _headers) = post_mcp(
            build_router(test_state()),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The open arm follows `oauth_door_enabled`, in BOTH directions.
    #[test]
    fn the_open_arm_follows_the_door_predicate_not_a_single_field() {
        let state = rmcp_state(None);
        assert!(
            state.oauth_door_enabled(),
            "a configured discovery surface IS a door"
        );
        // Door on, no legacy token, no transport identity => refused, so the
        // caller receives the challenge.
        assert!(!is_authorized(&state, &HeaderMap::new(), false));
        // Door on, but the listener vouched for the caller => unchanged.
        assert!(is_authorized(&state, &HeaderMap::new(), true));

        let no_door = test_state();
        assert!(!no_door.oauth_door_enabled());
        // No door and no token => the legacy open posture, untouched.
        assert!(is_authorized(&no_door, &HeaderMap::new(), false));
    }

    /// REVIEW ROUND 6 — the case the previous two attempts got wrong, and the
    /// one the removed source-scan guard could never have caught.
    ///
    /// A door that is NOT this item's discovery surface — RMCP-05's
    /// resource server, switched on by its own `RMCP_OAUTH_ENABLED` — must
    /// close the open arm just the same. Here `rmcp_discovery` is `None` and
    /// the ONLY evidence of a door is the environment, exactly the deployment
    /// shape that was still answering `200`.
    ///
    /// `#[serial]`: mutates process-global environment, per the crate's
    /// convention.
    #[tokio::test]
    #[serial]
    async fn rmcp02_a_door_other_than_discovery_still_closes_the_open_arm() {
        use crate::oauth::metadata::OauthDoors;

        std::env::set_var("RMCP_OAUTH_ENABLED", "1");
        let doors = OauthDoors::detect_from_env();
        std::env::remove_var("RMCP_OAUTH_ENABLED");

        assert!(
            doors.any(),
            "a resource-server door configured purely by env must be detected: {}",
            doors.describe()
        );

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        let state = Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            // No legacy shared token — the posture a first-time OAuth operator
            // has, and the one that used to fall through to "admit everyone".
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            tool_cache: Default::default(),
            gateway: None,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            // Discovery is DELIBERATELY unset. This is the whole point.
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: doors,
            oauth_resource: None,
            scope_resolver: None,
        });

        assert!(
            state.oauth_door_enabled(),
            "a door is a door even when this item's own config is absent"
        );

        let (status, _body, headers) = post_mcp(
            build_router(state),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an uncredentialed call to an OAuth-enabled process must never be 200, \
             whichever switch enabled it"
        );
        // No discovery surface means no `resource_metadata` to point at, so a
        // BARE 401 is correct here — the status code is what decides admission,
        // and RMCP-05 owns the challenge for its own door.
        assert!(!headers.contains_key("www-authenticate"));
    }

    /// Detection must not fire on an unrelated environment, or every deployment
    /// on the fleet would narrow. Pins both directions of the prefix rule.
    #[test]
    #[serial]
    fn oauth_door_detection_keys_on_the_shared_prefix() {
        use crate::oauth::metadata::OauthDoors;

        std::env::remove_var("RMCP_OAUTH_ENABLED");
        std::env::set_var("TERMINUS_SOMETHING_ELSE", "1");
        let unrelated = OauthDoors::detect_from_env();
        std::env::remove_var("TERMINUS_SOMETHING_ELSE");
        assert!(
            !unrelated.any(),
            "an unrelated variable must not be read as a door: {}",
            unrelated.describe()
        );

        // An EMPTY value is "not configured" — the same rule `read_env` applies,
        // and the reason a materialized `.env` template does not switch every
        // gateway into the narrowed posture.
        std::env::set_var("RMCP_OAUTH_ENABLED", "");
        let blank = OauthDoors::detect_from_env();
        std::env::remove_var("RMCP_OAUTH_ENABLED");
        assert!(!blank.any(), "an empty setting is not evidence of a door");

        // And `describe` must never leak a value — one of these settings is a
        // signing key.
        std::env::set_var("RMCP_OAUTH_SIGNING_KEY", "super-secret-value");
        let doors = OauthDoors::detect_from_env();
        std::env::remove_var("RMCP_OAUTH_SIGNING_KEY");
        assert!(doors.any());
        assert!(
            !doors.describe().contains("super-secret-value"),
            "describe must name settings, never their values: {}",
            doors.describe()
        );
    }

    /// Insufficient scope is a `403` with its own error code, not a `401`.
    /// Collapsing the two sends a user round a consent loop that re-grants
    /// exactly the scope that was already too narrow.
    #[tokio::test]
    async fn rmcp02_insufficient_scope_is_a_403_naming_what_is_needed() {
        let state = rmcp_state(None);
        let discovery = state.rmcp_discovery.as_ref().unwrap();
        let response = insufficient_scope(discovery, "mcp admin");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let challenge = response
            .headers()
            .get("www-authenticate")
            .expect("a scope failure must say which scope")
            .to_str()
            .unwrap();
        assert!(challenge.contains("error=\"insufficient_scope\""), "{challenge}");
        assert!(challenge.contains("scope=\"mcp admin\""), "{challenge}");
        assert!(
            challenge.contains(&format!(
                "resource_metadata=\"{}\"",
                discovery.protected_resource_url()
            )),
            "{challenge}"
        );
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
            tool_cache: Default::default(),
            gateway: Some(gateway),
            mesh_pool: None,
            principal_resolver,
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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
            tool_cache: Default::default(),
            gateway: Some(gateway),
            mesh_pool: Some(Arc::new(mesh_pool)),
            principal_resolver: PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: None,
            rmcp_endpoints: None,
            oauth_doors: crate::oauth::metadata::OauthDoors::none(),
            oauth_resource: None,
            scope_resolver: None,
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

    // ── RMCP-05: the OAuth door, end to end ──────────────────────────────

    use crate::oauth::resource::{OauthResourceServer, ResourceServerConfig, TokenState};

    // pii-test-fixture: invented connector URIs, an invented HMAC key and an
    // invented client id — none of them name a real host or credential.
    const OA_RESOURCE: &str = "https://connector.example.test/mcp"; // pii-test-fixture
    const OA_ISSUER: &str = "https://connector.example.test"; // pii-test-fixture
    const OA_KEY: &str = "mcp-server-test-signing-key-32-bytes"; // pii-test-fixture
    const OA_CLIENT: &str = "client-abc";
    /// The account UUID RMCP-04 puts in `sub`; `AlwaysLive` resolves it to the
    /// name `operator`, which the principal map below sends to `lumina`.
    const OA_ACCOUNT_ID: &str = "11111111-2222-3333-4444-555555555555";

    /// Live OAuth state that always says yes. The point of these tests is the
    /// wiring in `handle_mcp`, not the store — the store's own fail-closed
    /// branches are asserted in `crate::oauth::resource`.
    struct AlwaysLive;

    #[async_trait::async_trait]
    impl TokenState for AlwaysLive {
        async fn active_client_row(&self, _: &str) -> Result<Option<uuid::Uuid>, crate::error::ToolError> {
            Ok(Some(uuid::Uuid::nil()))
        }
        async fn active_account_name(&self, _: uuid::Uuid) -> Result<Option<String>, crate::error::ToolError> {
            Ok(Some("operator".to_string()))
        }
        async fn consent_is_live(&self, _: uuid::Uuid, _: uuid::Uuid) -> Result<bool, crate::error::ToolError> {
            Ok(true)
        }
        async fn any_session_is_live(&self, _: uuid::Uuid, _: uuid::Uuid) -> Result<bool, crate::error::ToolError> {
            Ok(true)
        }
    }

    fn oauth_server() -> Arc<OauthResourceServer> {
        let signer = crate::oauth::jwt::JwtSigner::new(
            OA_KEY.to_string(),
            None,
            OA_ISSUER.to_string(),
            900,
            30,
        )
        .expect("valid signer");
        let config = ResourceServerConfig::new(OA_RESOURCE, signer)
            .expect("valid resource-server config");
        Arc::new(OauthResourceServer::with_state(config, Arc::new(AlwaysLive)))
    }

    /// A token this server would itself have issued: account `operator`,
    /// connector `client-abc`, audienced at this exact resource.
    fn oauth_token() -> String {
        let now = chrono::Utc::now().timestamp();
        let claims = json!({
            "iss": OA_ISSUER,
            "sub": OA_ACCOUNT_ID,
            "aud": OA_RESOURCE,
            "client_id": OA_CLIENT,
            "scope": "mcp",
            "jti": "jti-mcp-1",
            "iat": now,
            "nbf": now - 1,
            "exp": now + 900,
        });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(OA_KEY.as_bytes()),
        )
        .expect("mint")
    }

    /// A scope source that grants every connector the whole catalog.
    ///
    /// RMCP-05's tests are about IDENTITY — that a token maps to its account's
    /// principal, and that the connector is a ceiling rather than an identity.
    /// With RMCP-07's connector ceiling now wired in, those tests would fail
    /// for a reason they are not about (an unscoped connector reaches nothing),
    /// and weakening their assertions to accommodate that would gut the guards
    /// they exist to be. Giving them a fully-scoped connector keeps each test
    /// testing its own property. RMCP-07's own behaviour — including the
    /// fail-closed default when no source is configured — has its own tests.
    struct ScopeSourceAllowingEverything;

    #[async_trait::async_trait]
    impl crate::oauth::scope::ClientScopeSource for ScopeSourceAllowingEverything {
        async fn scope_for(&self, client_id: &str) -> Arc<crate::oauth::scope::ClientScope> {
            Arc::new(crate::oauth::scope::ClientScope::unrestricted_for_test(client_id))
        }
    }

    fn state_with_oauth(gateway: GatewayFramework) -> Arc<McpServerState> {
        state_with_oauth_and_discovery(gateway, None)
    }

    /// The same OAuth state with NO scope source configured — the fail-closed
    /// deployment case RMCP-07 must refuse rather than let through.
    fn state_with_oauth_unscoped(gateway: GatewayFramework) -> Arc<McpServerState> {
        state_with_oauth_scoped(gateway, None, None)
    }

    /// As above, plus RMCP-02's discovery surface — which is what gives a `401`
    /// something to point at.
    fn state_with_oauth_and_discovery(
        gateway: GatewayFramework,
        discovery: Option<crate::oauth::metadata::Discovery>,
    ) -> Arc<McpServerState> {
        state_with_oauth_scoped(gateway, discovery, Some(Arc::new(ScopeSourceAllowingEverything)))
    }

    fn state_with_oauth_scoped(
        gateway: GatewayFramework,
        discovery: Option<crate::oauth::metadata::Discovery>,
        scope_resolver: Option<Arc<dyn crate::oauth::scope::ClientScopeSource>>,
    ) -> Arc<McpServerState> {
        let resolver = PrincipalResolver::new(
            serde_json::from_value::<PrincipalMap>(json!({
                "oauth_account": {"operator": "lumina"}
            }))
            .unwrap(),
        );
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoHealthTool)).unwrap();
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "terminus-rmcp05-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            tool_cache: Default::default(),
            gateway: Some(gateway),
            mesh_pool: None,
            principal_resolver: resolver,
            broker_routes: crate::broker::routes::RouteTable::new(),
            rmcp_discovery: discovery.map(Arc::new),
            // This fixture exercises the RESOURCE server (bearer token in,
            // principal out), which is independent of whether the
            // authorization-server endpoints are mounted — so `None` here is
            // the honest value, not a placeholder.
            rmcp_endpoints: None,
            // The door is on, so `OauthDoors` must say so — in production that
            // is automatic, because `RMCP_OAUTH_ENABLED` is an `RMCP_OAUTH_*`
            // name and `detect_from_env` keys on the prefix. Registering it
            // here keeps the fixture honest rather than accidentally testing a
            // combination (a resource server with no door detected) that no
            // configuration can produce.
            oauth_doors: {
                let mut doors = crate::oauth::metadata::OauthDoors::none();
                doors.register(crate::oauth::resource::ENABLED_ENV);
                doors
            },
            oauth_resource: Some(oauth_server()),
            scope_resolver,
        })
    }

    /// `post_mcp_with_identity`'s sibling with a settable request URI, so the
    /// query-string refusal can be driven through the real handler.
    async fn post_mcp_to(
        router: Router,
        uri: &str,
        body: Value,
        identity: Option<ClientIdentity>,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
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
        let headers = resp.headers().clone();
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
        (status, value, headers)
    }

    fn bearer_header(token: &str) -> String {
        format!("Bearer {token}")
    }

    /// POSITIVE CONTROL for everything below: a real token, through the real
    /// handler, reaches dispatch and is authorized under the principal its
    /// ACCOUNT maps to.
    #[tokio::test]
    async fn rmcp05_a_valid_token_reaches_dispatch_as_its_mapped_principal() {
        let router = build_router(state_with_oauth(gateway_allowing("lumina", &["health"])));
        let auth = bearer_header(&oauth_token());
        let (status, body, _) = post_mcp_to(
            router,
            "/mcp",
            health_call(70),
            None,
            &[("authorization", auth.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "mapped OAuth principal must be granted: {body}");
    }

    /// RMCP-07: an OAuth caller reaching a process with NO scope source
    /// configured is REFUSED, not let through on its account grant alone.
    ///
    /// This is a deliberate behaviour change to what RMCP-05 shipped, and the
    /// reasoning is the item's premise: an internet-facing door onto the whole
    /// fleet catalog is only safe if the door is narrower than the room behind
    /// it. A process that cannot read a connector's ceiling does not know how
    /// narrow the door is, and "unknown" resolves to denied everywhere else in
    /// this module. Falling back to the account grant would make a missing
    /// config field silently produce the unscoped door this item exists to
    /// prevent.
    ///
    /// The identical request succeeds in the test above, whose state HAS a
    /// scope source — so this pins the fail-closed default rather than some
    /// unrelated denial.
    #[tokio::test]
    async fn rmcp07_an_oauth_caller_is_refused_when_no_scope_source_is_configured() {
        let router = build_router(state_with_oauth_unscoped(gateway_allowing("lumina", &["health"])));
        let auth = bearer_header(&oauth_token());
        let (status, body, _) = post_mcp_to(
            router,
            "/mcp",
            health_call(72),
            None,
            &[("authorization", auth.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["isError"], true,
            "a connector whose ceiling cannot be read must reach nothing: {body}"
        );
    }

    /// FINDING 3, the half that proves the connector is NOT the identity.
    ///
    /// Identical request to the test above; the ONLY difference is that the
    /// grant is keyed on the `client_id` rather than on the account's mapped
    /// principal. It must be DENIED — if the connector had been folded into
    /// the principal (or used as a fallback identity) this would succeed, and
    /// a connector would then be able to GRANT what the account never had.
    #[tokio::test]
    async fn rmcp05_the_connector_is_carried_to_dispatch_but_is_never_the_identity() {
        let router = build_router(state_with_oauth(gateway_allowing(OA_CLIENT, &["health"])));
        let auth = bearer_header(&oauth_token());
        let (status, body, _) = post_mcp_to(
            router,
            "/mcp",
            health_call(71),
            None,
            &[("authorization", auth.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["isError"], true,
            "a grant keyed on the client_id must NOT authorize the caller — the connector is a \
             ceiling, not an identity: {body}"
        );
    }

    /// FINDING 3, the half that proves it is carried rather than dropped: the
    /// binding that reaches the dispatch boundary holds BOTH axes, and they
    /// are different values.
    #[test]
    fn rmcp05_the_binding_handed_to_dispatch_holds_both_axes() {
        let principal = Principal::new("lumina", crate::mesh::PrincipalSource::OAuth);
        let oauth = crate::oauth::resource::OauthCaller::for_test(principal, OA_CLIENT, "operator");
        let binding = caller_binding(None, Some(oauth));

        assert_eq!(binding.principal().map(Principal::name), Some("lumina"));
        assert_eq!(binding.oauth_client_id(), Some(OA_CLIENT));
        assert_ne!(binding.principal().map(Principal::name), binding.oauth_client_id());
    }

    /// PRECEDENCE at the binding: a cert-resolved principal wins and the OAuth
    /// caller is dropped WHOLE — it does not get to contribute a connector
    /// ceiling to an identity it did not establish.
    #[test]
    fn rmcp05_a_transport_principal_wins_and_drops_the_oauth_caller_entirely() {
        let cert_principal = Principal::new("harmony", crate::mesh::PrincipalSource::MtlsCert);
        let oauth = crate::oauth::resource::OauthCaller::for_test(
            Principal::new("lumina", crate::mesh::PrincipalSource::OAuth),
            OA_CLIENT,
            "operator",
        );
        let binding = caller_binding(Some(cert_principal), Some(oauth));

        assert_eq!(binding.principal().map(Principal::name), Some("harmony"));
        assert_eq!(binding.oauth_client_id(), None);
    }

    /// FINDING 1: a token in the URL is refused even when a CLIENT CERTIFICATE
    /// authenticated the request, and even though the call would otherwise
    /// have succeeded. The credential has already been written to logs and
    /// proxy caches by the time it arrives; which door the request took does
    /// not un-leak it.
    #[tokio::test]
    async fn rmcp05_a_query_string_token_is_refused_even_with_a_cert_identity() {
        let resolver = PrincipalResolver::new(
            serde_json::from_value::<PrincipalMap>(json!({
                "cert_cn": {"harmony-primary.example.test": "harmony"}
            }))
            .unwrap(),
        );
        let cert_identity = || ClientIdentity("harmony-primary.example.test".to_string());

        // POSITIVE CONTROL: the same cert, same call, no query string — allowed.
        let router = build_router(state_with(gateway_allowing("harmony", &["health"]), resolver.clone()));
        let (status, body, _) =
            post_mcp_to(router, "/mcp", health_call(72), Some(cert_identity()), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "control must pass: {body}");

        // Same request with a token in the URL — refused outright, before any
        // identity source is selected.
        let router = build_router(state_with(gateway_allowing("harmony", &["health"]), resolver));
        let (status, _, _) = post_mcp_to(
            router,
            "/mcp?access_token=leaked.jwt.value", // pii-test-fixture: invented token value
            health_call(73),
            Some(cert_identity()),
            &[],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a URL-borne credential must be refused whatever door authenticated the request"
        );
    }

    /// ...and on an OAuth-configured host it is refused WITH the challenge,
    /// even when a perfectly valid header credential is also present. The
    /// header would have authenticated; the URL is what fails the request.
    #[tokio::test]
    async fn rmcp05_a_query_string_token_is_refused_even_beside_a_valid_header() {
        let router = build_router(state_with_oauth(gateway_allowing("lumina", &["health"])));
        let auth = bearer_header(&oauth_token());
        let (status, _, headers) = post_mcp_to(
            router,
            "/mcp?access_token=leaked.jwt.value", // pii-test-fixture: invented token value
            health_call(74),
            None,
            &[("authorization", auth.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let challenge = headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.contains("invalid_request"), "challenge was {challenge:?}");
    }

    /// An expired token is refused WITH the challenge, which is how a hosted
    /// client knows to refresh rather than to give up.
    #[tokio::test]
    async fn rmcp05_an_expired_token_is_401_with_a_challenge() {
        let now = chrono::Utc::now().timestamp();
        let claims = json!({
            "iss": OA_ISSUER, "sub": OA_ACCOUNT_ID, "aud": OA_RESOURCE,
            "client_id": OA_CLIENT, "scope": "mcp", "jti": "jti-mcp-2",
            "iat": now - 7200, "nbf": now - 7200, "exp": now - 3600,
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(OA_KEY.as_bytes()),
        )
        .unwrap();

        let router = build_router(state_with_oauth(gateway_allowing("lumina", &["health"])));
        let auth = bearer_header(&token);
        let (status, _, headers) = post_mcp_to(
            router,
            "/mcp",
            health_call(75),
            None,
            &[("authorization", auth.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let challenge = headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.contains("invalid_token"), "challenge was {challenge:?}");
        assert!(challenge.contains("resource_metadata="), "challenge was {challenge:?}");
    }

    /// The acceptance criterion this item could not meet on its own: with the
    /// resource-server door ENABLED and NO credential of any kind — no bearer
    /// token, no client certificate, no tailnet identity — `/mcp` must answer
    /// `401` carrying the discovery challenge, not `200`.
    ///
    /// ## Why this test lives here and not in RMCP-02
    ///
    /// The two items own different halves and only a test from THIS side pins
    /// that they compose. RMCP-02 owns the narrowing — its `is_authorized` is
    /// the single place that decides an uncredentialed request is refused once
    /// any OAuth door is configured — and its own test deliberately asserts a
    /// BARE `401`, because with no discovery surface configured there is no
    /// `resource_metadata` to point a client at.
    ///
    /// This item owns the door that makes the narrowing apply, and a deployment
    /// running it has a discovery surface. So the end state a Claude client
    /// actually meets is the one asserted here: the `401` AND the spec-shaped
    /// challenge that tells it where to authorize. Discovery is entirely
    /// `401`-driven and a `WWW-Authenticate` on a `200` is discarded, so status
    /// and header have to be right together — asserting either alone would pass
    /// while the connector still failed with a generic "couldn't reach the MCP
    /// server".
    ///
    /// Note this item writes NO admission logic of its own. It is a consumer
    /// here, and the test is the proof that consuming was enough.
    #[tokio::test]
    async fn rmcp05_an_uncredentialed_request_to_an_enabled_door_is_401_with_the_challenge() {
        let discovery = crate::oauth::metadata::Discovery::new(
            crate::oauth::metadata::CanonicalUri::parse(
                crate::oauth::authorize::RESOURCE_ENV,
                OA_RESOURCE,
            )
            .expect("resource fixture"),
            crate::oauth::metadata::CanonicalUri::parse(
                crate::oauth::authorize::ISSUER_ENV,
                OA_ISSUER,
            )
            .expect("issuer fixture"),
            false,
            vec!["mcp".to_string()],
            "mcp".to_string(),
            false,
        )
        .expect("discovery fixture must build");

        let state = state_with_oauth_and_discovery(
            gateway_allowing("lumina", &["health"]),
            Some(discovery),
        );
        // The precondition the whole assertion rests on: this host has no
        // legacy shared token, so before RMCP-02's narrowing the open arm would
        // have admitted this request outright.
        assert!(state.auth_token.is_none());

        let router = build_router(state);
        // No Authorization header, no ClientIdentity extension, no tailnet
        // identity — exactly what arrives from the public internet.
        let (status, _, headers) =
            post_mcp_to(router, "/mcp", health_call(76), None, &[]).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an enabled door must refuse an uncredentialed request rather than dispatch it"
        );
        let challenge = headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            challenge.starts_with("Bearer "),
            "the challenge must name the Bearer scheme: {challenge:?}"
        );
        assert!(
            challenge.contains("resource_metadata="),
            "a 401 a client cannot act on is why this item exists: {challenge:?}"
        );
    }

    /// The control for the test above, and the reason it proves anything: with
    /// the door CLOSED, the identical uncredentialed request is still served.
    /// Every pre-RMCP-05 loopback and mTLS deployment depends on that, so the
    /// narrowing must be a consequence of opening the door and of nothing else.
    #[tokio::test]
    async fn rmcp05_an_uncredentialed_request_to_a_closed_door_is_unchanged() {
        let state = state_with(gateway_allowing("anonymous", &["health"]), PrincipalResolver::default());
        assert!(state.oauth_resource.is_none() && !state.oauth_door_enabled());

        let router = build_router(state);
        let (status, _, headers) =
            post_mcp_to(router, "/mcp", health_call(77), None, &[]).await;

        assert_eq!(status, StatusCode::OK, "a closed door must change nothing");
        assert!(headers.get("www-authenticate").is_none());
    }

    /// FINDING 2: the candidate check and the credential parser must agree
    /// about what a Bearer header IS. They now share one matcher, so every
    /// case form is recognised — a `bearer …` header used to fall through to
    /// the legacy path, which on a host with no static token means "admit".
    #[test]
    fn rmcp05_the_candidate_check_matches_the_bearer_scheme_case_insensitively() {
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("{scheme} a.b.c").parse().unwrap());
            assert!(
                oauth_is_candidate(true, None, &headers, None, None),
                "{scheme:?} must be recognised as a bearer credential"
            );
        }
    }

    /// The rest of the candidate rule, each clause asserted on its own.
    #[test]
    fn rmcp05_candidate_selection_never_overrides_a_transport_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer a.b.c".parse().unwrap());

        let cert = ClientIdentity("harmony-primary.example.test".to_string());
        // Synthetic: the node keeps the `<host>.<tailnet>.ts.net` shape the
        // parser needs, with a tailnet segment that is deliberately a non-name.
        let tailnet = crate::mesh::TailnetIdentity {
            login: "operator-login".to_string(), // pii-test-fixture
            node: "test-node.not-a-real-tailnet.ts.net".to_string(),  // pii-test-fixture
            tags: vec![],
        };

        assert!(
            !oauth_is_candidate(true, None, &headers, Some(&cert), None),
            "a cert caller's token must not even be inspected"
        );
        assert!(
            !oauth_is_candidate(true, None, &headers, None, Some(&tailnet)),
            "a tailnet caller's token must not even be inspected"
        );
        assert!(
            !oauth_is_candidate(false, None, &headers, None, None),
            "with no resource server configured the door does not exist"
        );
        assert!(
            !oauth_is_candidate(true, None, &HeaderMap::new(), None, None),
            "no Authorization header means nothing for this door to validate"
        );

        // The legacy shared token is not an OAuth credential and must not be
        // run through JWT verification.
        let mut legacy = HeaderMap::new();
        legacy.insert("authorization", "Bearer legacy-static".parse().unwrap());
        assert!(!oauth_is_candidate(true, Some("legacy-static"), &legacy, None, None));

        // ...but a DIFFERENT credential on the same host still is one.
        assert!(oauth_is_candidate(true, Some("legacy-static"), &headers, None, None));

        // A present-but-malformed Authorization header is this door's business
        // too: falling through would let the legacy posture (on a host with no
        // static token, "admit everyone") decide a broken credential.
        let mut malformed = HeaderMap::new();
        malformed.insert("authorization", "Basic abc".parse().unwrap());
        assert!(oauth_is_candidate(true, None, &malformed, None, None));
    }
}
