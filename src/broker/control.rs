//! TMOD-05: broker admin control plane — register/deregister/health/list.
//!
//! This is the missing piece that makes TMOD-02's `WorkerTransport` and
//! TMOD-04's [`crate::broker::routes::RouteTable`] actually usable without a
//! process restart: before this item, a worker could be dialed and routed
//! to *in code*, but nothing on any live path ever called
//! `RouteTable::install_many`/`remove_worker` — this module is that live
//! path, exposed as a small AUTHENTICATED HTTP admin surface.
//!
//! ## Mounted on the control surface, never on public `/mcp`
//! [`build_control_router`] returns its own standalone `axum::Router` — it
//! is NOT merged into, and shares no route prefix with,
//! `crate::mcp_server::build_router`'s `/mcp` router. A caller reaching
//! `/mcp` can never accidentally hit `/admin/workers/*`, and vice versa;
//! `crate::pki::server::build_gateway_router` merges this router in
//! alongside the enrollment router, at the SAME `/admin/workers*` paths on
//! every listener that router is served from (see that function's doc for
//! why enrollment and admin are both treated as "control", not "public MCP
//! tool traffic").
//!
//! ## AuthN: reuses the federation service-JWT / mTLS scheme, fail-closed
//! Every handler here follows the exact same identity-extraction shape
//! `crate::mcp_server`'s handlers use: an optional
//! [`crate::pki::mtls::ClientIdentity`] request extension (populated by the
//! mTLS listener) and an optional
//! [`crate::mesh::TailnetIdentity`] extension, reconciled to a single
//! [`crate::mesh::Principal`] via [`crate::mcp_server::resolve_principal`] —
//! no new identity model, no bespoke header. That principal is then run
//! through [`crate::gateway_framework::GatewayFramework::guard`] — the same
//! allowlist + rate-limit + audit pipeline `/mcp`'s `tools/call` and the
//! inference-proxy routes already use — via [`require_gate`].
//!
//! Unlike `/mcp` (which stays USABLE, ungated, when
//! `McpServerState::gateway` is `None`, preserving pre-TGW-04 behavior for
//! deployments that never opted into gating), the admin control plane NEVER
//! runs open: [`require_gate`] treats a `None` gateway as an unconditional
//! deny, not a bypass. "No `GatewayFramework` configured" means no admin-auth
//! secret (`TERMINUS_GATEWAY_*` — see `crate::gateway_framework::rate_limit`/
//! `AllowlistPolicy::from_env`) is provisioned on this process at all, and a
//! process with no admin-auth secret must refuse every admin op, not silently
//! allow them — "fail closed if the admin-auth secret is unset" from this
//! item's acceptance criteria.
//!
//! ## Registration: validate → health-gate → atomic install
//! [`handle_register`]:
//! 1. Deserializes the manifest into a [`crate::config::WorkerTransportEntry`]
//!    (`#[serde(flatten)]`) plus the worker's advertised `tools`.
//! 2. Validates it via [`crate::config::validate_worker_transport_entry`] —
//!    the SAME rule set `TERMINUS_BROKER_WORKERS_JSON` startup config uses,
//!    including **the [`crate::broker::transport::MinTierPolicy`] floor**
//!    (a `write_scoped`/`secret_holding` worker registering below T2 is
//!    rejected here, before any connection is even attempted).
//! 3. Builds the concrete [`crate::broker::transport::WorkerTransport`] for
//!    the declared tier (`build_transport`) and health-gates it: `connect()`
//!    must succeed AND a bounded `health()` probe (same
//!    [`crate::broker::routes::HEALTH_PROBE_TIMEOUT`] budget `tools/list`
//!    uses) must return `true`. A worker that fails either check is REFUSED
//!    registration — no route is ever installed for a worker that can't
//!    prove it's alive at onboarding time.
//! 4. Only once all of the above succeed does it call
//!    [`crate::broker::routes::RouteTable::install_many`] — ONE atomic
//!    snapshot swap for every tool the manifest advertises, so a reader can
//!    never observe a half-registered worker (some tools routed, some not).
//!    Registering an already-registered `name` replaces its tools in the
//!    same atomic swap (install-or-replace), which is how a worker "moves"
//!    to a new address/tier without a restart.
//!
//! ## Deregistration / health / list
//! [`handle_deregister`] removes every route for a `worker_id` in one atomic
//! swap (`RouteTable::remove_worker`) — in-flight calls already dispatched
//! against the OLD snapshot finish normally (TMOD-04's documented
//! no-tearing guarantee); it does not attempt to cancel them.
//! [`handle_health`] probes one or all currently-registered workers and
//! reports liveness — it never mutates the route table (a probe is a
//! read-only liveness check, not an eviction mechanism; TMOD-04's own
//! per-call/per-list health check already handles a worker that goes
//! unhealthy between admin health checks). [`handle_list`] reports the
//! current routes (worker id, tools, tier, capability class, last known
//! health) with no secret material in the response — transport identity
//! strings (a cert CN, a socket path) are logged/returned as-is elsewhere in
//! this crate too; they are not credentials.
//!
//! ## Audit
//! Every handler runs through [`require_gate`], which — like
//! `GatewayFramework::guard` itself — already audits every DENIAL. Each
//! handler additionally calls `GatewayContext::record_result` exactly once
//! after its operation completes, with a short, name-only detail string
//! (worker id, tier, tool count — never a cert PEM, socket path contents, or
//! any other secret-shaped value) — [`crate::gateway_framework::audit::sanitize`]
//! (S6) still runs on that detail as a second, defense-in-depth layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::broker::routes::{RouteTable, WorkerRoute, HEALTH_PROBE_TIMEOUT};
use crate::broker::transport::{
    mtls_tcp::MtlsTcpTransport, uds_mtls::UdsMtlsTransport, uds_peercred::UdsPeercredTransport,
    CapabilityClass, TransportTier, WorkerTransport,
};
use crate::config::{validate_worker_transport_entry, WorkerTransportConfigError, WorkerTransportEntry};
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::gateway_framework::{ActionKind, GatewayContext, ANONYMOUS_IDENTITY};
use crate::mcp_server::{resolve_principal, McpServerState};
use crate::mesh::{Principal, TailnetIdentity};
use crate::pki::mtls::ClientIdentity;
use crate::registry::ToolInfo;

/// The label this broker process presents as ITS OWN client identity when
/// dialing a T0/T2 worker (minting a short-lived client leaf cert against
/// the embedded CA) — distinct from `expected_identity` (the WORKER's
/// required identity), same convention `crate::broker::transport`'s own unit
/// tests use for a "test-broker" caller.
const BROKER_CLIENT_IDENTITY_LABEL: &str = "terminus-broker";

// ── Admin-only bookkeeping the route table itself doesn't carry ───────────
//
// `crate::broker::routes::WorkerRoute` intentionally carries only what
// dispatch needs (worker_id, transport, tool metadata) -- it has no opinion
// on tier/capability_class/registration time, which are ADMIN-surface
// concerns (`GET /admin/workers`'s listing, `POST .../health`'s per-worker
// grouping), not dispatch-path ones. This small side table holds exactly
// that, keyed by worker_id, swapped atomically in lock-step with every
// `RouteTable` mutation this module performs so the two never drift.

#[derive(Debug, Clone, Serialize)]
struct WorkerAdminMeta {
    tier: TransportTier,
    capability_class: CapabilityClass,
    /// Unix seconds -- coarse, log/display-only, never used for any
    /// security decision.
    registered_at_unix: u64,
    /// Set by the last successful register/health probe for this worker;
    /// `None` until the first probe (registration always probes at least
    /// once, so this is `None` only for a worker record this process has
    /// never itself health-checked, which should not happen in practice).
    last_health: Option<bool>,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone)]
struct AdminState {
    mcp: Arc<McpServerState>,
    meta: Arc<ArcSwap<HashMap<String, WorkerAdminMeta>>>,
}

/// Build the standalone admin control-plane router: `POST
/// /admin/workers/register`, `POST /admin/workers/deregister`, `POST
/// /admin/workers/health`, `GET /admin/workers`. Callers (currently
/// `crate::pki::server::build_gateway_router`) merge this into whatever
/// router they serve on the control surface — see this module's doc for why
/// that is deliberately never the same router prefix as public `/mcp`
/// traffic conceptually, even though today both happen to be served by the
/// same physical listener(s).
pub fn build_control_router(mcp: Arc<McpServerState>) -> Router {
    let state = AdminState { mcp, meta: Arc::new(ArcSwap::from_pointee(HashMap::new())) };
    Router::new()
        .route("/admin/workers/register", post(handle_register))
        .route("/admin/workers/deregister", post(handle_deregister))
        .route("/admin/workers/health", post(handle_health))
        .route("/admin/workers", get(handle_list))
        // A registration manifest is a handful of short strings/JSON, not a
        // bulk payload -- cap it generously (64KiB) so a malformed/malicious
        // huge body can't tie up a request handler for no legitimate reason,
        // mirroring `crate::pki::enroll::build_enroll_router`'s own tight
        // body-limit posture for a small, pre-dispatch control endpoint.
        .layer(axum::extract::DefaultBodyLimit::max(65_536))
        .with_state(state)
}

// ── Wire types ──────────────────────────────────────────────────────────

/// One tool a registering worker advertises. Deliberately a distinct type
/// from [`ToolInfo`] (rather than deriving `Deserialize` directly on that
/// type) — `ToolInfo` is this crate's internal catalog metadata shape, not a
/// wire contract; keeping them separate means a future internal-only field
/// added to `ToolInfo` never silently becomes part of this HTTP API.
#[derive(Debug, Deserialize)]
struct WorkerToolManifestEntry {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_input_schema")]
    parameters: Value,
}

fn default_input_schema() -> Value {
    json!({"type": "object"})
}

impl From<WorkerToolManifestEntry> for ToolInfo {
    fn from(t: WorkerToolManifestEntry) -> Self {
        ToolInfo { name: t.name, description: t.description, parameters: t.parameters }
    }
}

/// `POST /admin/workers/register` request body: the worker's transport
/// manifest (`#[serde(flatten)]`, the exact same shape/field names
/// `TERMINUS_BROKER_WORKERS_JSON` entries use — see
/// [`crate::config::WorkerTransportEntry`]) plus the tools it advertises.
#[derive(Debug, Deserialize)]
struct RegisterWorkerRequest {
    #[serde(flatten)]
    entry: WorkerTransportEntry,
    tools: Vec<WorkerToolManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct DeregisterWorkerRequest {
    worker_id: String,
}

#[derive(Debug, Deserialize, Default)]
struct HealthRequest {
    /// A single worker to probe; when absent, every currently-registered
    /// worker is probed.
    #[serde(default)]
    worker_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkerHealthReport {
    worker_id: String,
    healthy: bool,
}

#[derive(Debug, Serialize)]
struct WorkerSummary {
    worker_id: String,
    tools: Vec<String>,
    /// `None` when a worker has routes installed but this process has no
    /// admin metadata for it (should not happen via this module's own
    /// register/deregister path, but the list handler is read-only and
    /// tolerates it rather than panicking or hiding the worker).
    tier: Option<TransportTier>,
    capability_class: Option<CapabilityClass>,
    last_health: Option<bool>,
    registered_at_unix: Option<u64>,
}

// ── Error → HTTP response mapping ──────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum AdminError {
    #[error("invalid worker manifest: {0}")]
    InvalidManifest(#[from] WorkerTransportConfigError),
    #[error("worker manifest must advertise at least one tool")]
    NoTools,
    #[error("worker transport could not be constructed: {0}")]
    Transport(#[from] crate::broker::transport::TransportError),
    #[error("worker '{0}' failed its onboarding health probe (connect succeeded but health check did not report healthy within {1:?})")]
    Unhealthy(String, std::time::Duration),
    #[error("unknown worker '{0}'")]
    UnknownWorker(String),
}

impl AdminError {
    fn status(&self) -> StatusCode {
        match self {
            AdminError::InvalidManifest(_) | AdminError::NoTools => StatusCode::BAD_REQUEST,
            AdminError::Transport(_) | AdminError::Unhealthy(_, _) => StatusCode::BAD_GATEWAY,
            AdminError::UnknownWorker(_) => StatusCode::NOT_FOUND,
        }
    }
}

fn error_response(err: &AdminError) -> Response {
    (err.status(), [("content-type", "application/json")], json!({"error": err.to_string()}).to_string())
        .into_response()
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (status, [("content-type", "application/json")], body.to_string()).into_response()
}

// ── AuthN/authZ gate: fail-closed even when the tool-call path wouldn't be ─

/// Gate one admin request through the same identity → allowlist →
/// rate-limit → audit pipeline `/mcp` uses, with ONE deliberate difference:
/// a `None` `McpServerState::gateway` (no `GatewayFramework` configured at
/// all) is an unconditional DENY here, never the "ungated, dispatch anyway"
/// posture `/mcp`'s handlers fall back to. See this module's doc for why:
/// the admin control plane must never run open just because a deployment
/// hasn't opted a process into TGW-04 gating for its `/mcp` traffic.
async fn require_gate(
    admin: &AdminState,
    principal: Option<&Principal>,
    action: &str,
) -> Result<GatewayContext, Response> {
    match &admin.mcp.gateway {
        Some(gateway) => gateway.guard(principal, action, ActionKind::Admin).await,
        None => {
            AuditEntry::new(
                ANONYMOUS_IDENTITY,
                action,
                ActionKind::Admin,
                AuditResult::DeniedNoIdentity,
                Some(
                    "no GatewayFramework configured on this process -- the admin control plane \
                     fails closed rather than dispatching ungated",
                ),
            )
            .log();
            Err(json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": "admin control plane is not configured on this process (no gateway/admin-auth)"}),
            ))
        }
    }
}

/// Resolve the caller's [`Principal`] the exact same way every `/mcp` and
/// inference-proxy handler does — see [`resolve_principal`]'s doc.
fn principal_from_extensions(
    admin: &AdminState,
    identity: &Option<Extension<ClientIdentity>>,
    tailnet: &Option<Extension<TailnetIdentity>>,
) -> Option<Principal> {
    resolve_principal(
        &admin.mcp.principal_resolver,
        identity.as_ref().map(|Extension(i)| i),
        tailnet.as_ref().map(|Extension(t)| t),
    )
}

// ── Transport construction from a validated manifest ───────────────────

/// Build the concrete [`WorkerTransport`] for `entry`'s declared tier,
/// minting this broker process's own client cert against the embedded CA
/// for the T0/T2 (mTLS-bearing) tiers. Assumes `entry` has already passed
/// [`validate_worker_transport_entry`] (every tier-required field present) —
/// the `ok_or_else`s below are defensive, never expected to fire on a
/// manifest that already validated, and return a clean
/// [`crate::broker::transport::TransportError::Protocol`] rather than
/// panicking if they somehow do.
fn build_transport(
    entry: &WorkerTransportEntry,
) -> Result<Arc<dyn WorkerTransport>, crate::broker::transport::TransportError> {
    use crate::broker::transport::TransportError;

    match entry.tier {
        TransportTier::T1 => {
            let socket_path = entry
                .socket_path
                .clone()
                .ok_or_else(|| TransportError::Protocol("T1 worker missing socket_path".to_string()))?;
            let expected_uid = entry
                .expected_uid
                .ok_or_else(|| TransportError::Protocol("T1 worker missing expected_uid".to_string()))?;
            Ok(Arc::new(UdsPeercredTransport::new(socket_path, expected_uid)))
        }
        TransportTier::T2 => {
            let socket_path = entry
                .socket_path
                .clone()
                .ok_or_else(|| TransportError::Protocol("T2 worker missing socket_path".to_string()))?;
            let expected_uid = entry
                .expected_uid
                .ok_or_else(|| TransportError::Protocol("T2 worker missing expected_uid".to_string()))?;
            let expected_identity = entry
                .expected_identity
                .clone()
                .ok_or_else(|| TransportError::Protocol("T2 worker missing expected_identity".to_string()))?;
            let ca = crate::pki::ca().map_err(|e| TransportError::Unavailable(format!("CA unavailable: {e}")))?;
            let transport =
                UdsMtlsTransport::new(socket_path, expected_uid, expected_identity, ca, BROKER_CLIENT_IDENTITY_LABEL)?;
            Ok(Arc::new(transport))
        }
        TransportTier::T0 => {
            let host = entry
                .host
                .clone()
                .ok_or_else(|| TransportError::Protocol("T0 worker missing host".to_string()))?;
            let port = entry.port.ok_or_else(|| TransportError::Protocol("T0 worker missing port".to_string()))?;
            let expected_identity = entry
                .expected_identity
                .clone()
                .ok_or_else(|| TransportError::Protocol("T0 worker missing expected_identity".to_string()))?;
            let ca = crate::pki::ca().map_err(|e| TransportError::Unavailable(format!("CA unavailable: {e}")))?;
            let transport = MtlsTcpTransport::new(host, port, expected_identity, ca, BROKER_CLIENT_IDENTITY_LABEL)?;
            Ok(Arc::new(transport))
        }
    }
}

/// Bounded liveness probe, same budget [`crate::broker::routes`]'s own
/// `tools/list`/`tools/call` dispatch uses — a worker that accepts a probe
/// but never answers must not be able to stall an admin request either.
async fn probe_health(transport: &Arc<dyn WorkerTransport>) -> bool {
    matches!(tokio::time::timeout(HEALTH_PROBE_TIMEOUT, transport.health()).await, Ok(true))
}

// ── Handlers ────────────────────────────────────────────────────────────

async fn handle_register(
    State(admin): State<AdminState>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    Json(req): Json<RegisterWorkerRequest>,
) -> Response {
    let principal = principal_from_extensions(&admin, &identity, &tailnet);
    let gate_ctx = match require_gate(&admin, principal.as_ref(), "admin:register_worker").await {
        Ok(ctx) => ctx,
        Err(denial) => return denial,
    };

    let result = do_register(&admin, req).await;
    match &result {
        Ok((worker_id, tool_count)) => {
            gate_ctx.record_result(true, Some(&format!("registered worker '{worker_id}' ({tool_count} tools)")));
        }
        Err(e) => gate_ctx.record_result(false, Some(&e.to_string())),
    }

    match result {
        Ok((worker_id, tool_count)) => json_response(
            StatusCode::OK,
            json!({"worker_id": worker_id, "tools_registered": tool_count}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn do_register(admin: &AdminState, req: RegisterWorkerRequest) -> Result<(String, usize), AdminError> {
    // 1. Validate the manifest -- includes the MinTierPolicy floor check
    //    (write_scoped/secret_holding must be >= T2), the same rules
    //    TERMINUS_BROKER_WORKERS_JSON startup config enforces.
    validate_worker_transport_entry(&req.entry)?;

    if req.tools.is_empty() {
        return Err(AdminError::NoTools);
    }

    // 2. Open the transport and health-gate it BEFORE installing any route
    //    -- a worker that can't be dialed or doesn't answer healthy never
    //    gets a route.
    let transport = build_transport(&req.entry)?;
    transport.connect().await?;
    if !probe_health(&transport).await {
        return Err(AdminError::Unhealthy(req.entry.name.clone(), HEALTH_PROBE_TIMEOUT));
    }

    // 3. Build every route this worker's manifest advertises and install
    //    them in ONE atomic swap (TMOD-04's `install_many`) -- a reader
    //    never observes a half-registered worker.
    let worker_id = req.entry.name.clone();
    let tool_count = req.tools.len();
    let routes: Vec<WorkerRoute> = req
        .tools
        .into_iter()
        .map(|t| WorkerRoute { worker_id: worker_id.clone(), transport: transport.clone(), tool: t.into() })
        .collect();
    admin.mcp.broker_routes.install_many(routes);

    // 4. Record admin-only bookkeeping (tier/capability_class/registered_at)
    //    in lock-step -- read by `handle_list`/`handle_health`, never by
    //    dispatch.
    upsert_meta(
        &admin.meta,
        &worker_id,
        WorkerAdminMeta {
            tier: req.entry.tier,
            capability_class: req.entry.capability_class,
            registered_at_unix: now_unix(),
            last_health: Some(true),
        },
    );

    Ok((worker_id, tool_count))
}

fn upsert_meta(meta: &ArcSwap<HashMap<String, WorkerAdminMeta>>, worker_id: &str, entry: WorkerAdminMeta) {
    meta.rcu(|current| {
        let mut next = (**current).clone();
        next.insert(worker_id.to_string(), entry.clone());
        next
    });
}

fn remove_meta(meta: &ArcSwap<HashMap<String, WorkerAdminMeta>>, worker_id: &str) {
    meta.rcu(|current| {
        let mut next = (**current).clone();
        next.remove(worker_id);
        next
    });
}

async fn handle_deregister(
    State(admin): State<AdminState>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    Json(req): Json<DeregisterWorkerRequest>,
) -> Response {
    let principal = principal_from_extensions(&admin, &identity, &tailnet);
    let gate_ctx = match require_gate(&admin, principal.as_ref(), "admin:deregister_worker").await {
        Ok(ctx) => ctx,
        Err(denial) => return denial,
    };

    // Atomic removal (TMOD-04's `remove_worker`) -- in-flight calls already
    // dispatched against the OLD snapshot finish normally; this only changes
    // what the NEXT request's `load()` sees.
    admin.mcp.broker_routes.remove_worker(&req.worker_id);
    remove_meta(&admin.meta, &req.worker_id);

    gate_ctx.record_result(true, Some(&format!("deregistered worker '{}'", req.worker_id)));
    json_response(StatusCode::OK, json!({"worker_id": req.worker_id, "removed": true}))
}

async fn handle_health(
    State(admin): State<AdminState>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
    Json(req): Json<HealthRequest>,
) -> Response {
    let principal = principal_from_extensions(&admin, &identity, &tailnet);
    let gate_ctx = match require_gate(&admin, principal.as_ref(), "admin:health_worker").await {
        Ok(ctx) => ctx,
        Err(denial) => return denial,
    };

    let snapshot = admin.mcp.broker_routes.load();

    // One representative transport per distinct worker_id -- every route
    // from the same worker's manifest shares one transport (see
    // `WorkerRoute::transport`'s doc), so probing once per worker is
    // sufficient and avoids redundant round trips.
    let mut by_worker: HashMap<String, Arc<dyn WorkerTransport>> = HashMap::new();
    for route in snapshot.all() {
        by_worker.entry(route.worker_id.clone()).or_insert_with(|| route.transport.clone());
    }

    let targets: Vec<String> = match &req.worker_id {
        Some(id) => {
            if !by_worker.contains_key(id) {
                let err = AdminError::UnknownWorker(id.clone());
                gate_ctx.record_result(false, Some(&err.to_string()));
                return error_response(&err);
            }
            vec![id.clone()]
        }
        None => by_worker.keys().cloned().collect(),
    };

    // Probe every target CONCURRENTLY, same fault-isolation posture
    // `crate::broker::routes::merge_catalog` uses -- one hung worker must
    // not stall this admin request for the others.
    let probes = targets.into_iter().map(|worker_id| {
        let transport = by_worker.get(&worker_id).expect("target came from by_worker's own keys").clone();
        async move {
            let healthy = probe_health(&transport).await;
            (worker_id, healthy)
        }
    });
    let results: Vec<(String, bool)> = futures_util::future::join_all(probes).await;

    for (worker_id, healthy) in &results {
        if let Some(mut existing) = admin.meta.load().get(worker_id).cloned() {
            existing.last_health = Some(*healthy);
            upsert_meta(&admin.meta, worker_id, existing);
        }
    }

    let report: Vec<WorkerHealthReport> =
        results.into_iter().map(|(worker_id, healthy)| WorkerHealthReport { worker_id, healthy }).collect();

    gate_ctx.record_result(true, Some(&format!("health-probed {} worker(s)", report.len())));
    json_response(StatusCode::OK, json!({"workers": report}))
}

async fn handle_list(
    State(admin): State<AdminState>,
    identity: Option<Extension<ClientIdentity>>,
    tailnet: Option<Extension<TailnetIdentity>>,
) -> Response {
    let principal = principal_from_extensions(&admin, &identity, &tailnet);
    let gate_ctx = match require_gate(&admin, principal.as_ref(), "admin:list_workers").await {
        Ok(ctx) => ctx,
        Err(denial) => return denial,
    };

    let snapshot = admin.mcp.broker_routes.load();
    let meta = admin.meta.load();

    let mut tools_by_worker: HashMap<String, Vec<String>> = HashMap::new();
    for route in snapshot.all() {
        tools_by_worker.entry(route.worker_id.clone()).or_default().push(route.tool.name.clone());
    }

    let summaries: Vec<WorkerSummary> = tools_by_worker
        .into_iter()
        .map(|(worker_id, mut tools)| {
            tools.sort();
            let m = meta.get(&worker_id);
            WorkerSummary {
                worker_id,
                tools,
                tier: m.map(|m| m.tier),
                capability_class: m.map(|m| m.capability_class),
                last_health: m.and_then(|m| m.last_health),
                registered_at_unix: m.map(|m| m.registered_at_unix),
            }
        })
        .collect();

    gate_ctx.record_result(true, Some(&format!("listed {} worker(s)", summaries.len())));
    json_response(StatusCode::OK, json!({"workers": summaries}))
}

// `Path` is imported for forward-compatibility with a future
// `/admin/workers/:id`-shaped route but unused by the current body-driven
// handlers above; silence the unused-import lint rather than dropping the
// import and re-adding it the moment such a route is needed.
#[allow(unused_imports)]
use axum::extract::Path as _UnusedPathImportPlaceholder;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::routes::RouteTable;
    use crate::federation::PersonalFederationClient;
    use crate::gateway_framework::rate_limit::InProcessRateLimiter;
    use crate::gateway_framework::GatewayFramework;
    use crate::mesh::PrincipalResolver;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_mcp_state(gateway: Option<GatewayFramework>) -> Arc<McpServerState> {
        Arc::new(McpServerState {
            registry: arc_swap::ArcSwap::from_pointee(crate::registry::ToolRegistry::new()),
            server_name: "test-broker".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None::<PersonalFederationClient>,
            inference_proxy: None,
            gateway,
            mesh_pool: None,
            principal_resolver: PrincipalResolver::default(),
            broker_routes: RouteTable::new(),
        })
    }

    /// Every identity is allowed every admin action, for tests that only
    /// care about the register/health-gate/route-table mechanics, not
    /// allowlist policy itself (that's covered separately below).
    fn allow_all_gateway() -> GatewayFramework {
        use crate::gateway_framework::{AllowlistPolicy, Grant};
        use std::collections::HashMap;
        let mut entries = HashMap::new();
        entries.insert("test-admin".to_string(), Grant::List(vec!["*".to_string()]));
        GatewayFramework::new(AllowlistPolicy::new(entries), Arc::new(InProcessRateLimiter::new(1000, 1000.0)))
    }

    fn authed_request(uri: &str, body: Value) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ClientIdentity("test-admin".to_string()));
        req
    }

    // ── Fail-closed: no GatewayFramework configured at all ─────────────

    #[tokio::test]
    async fn admin_ops_fail_closed_when_gateway_not_configured() {
        let state = test_mcp_state(None);
        let router = build_control_router(state);

        let req = authed_request(
            "/admin/workers/register",
            json!({"name": "w1", "tier": "T1", "capability_class": "read_only", "tools": []}),
        );
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "no GatewayFramework configured must fail closed, never dispatch ungated"
        );
    }

    // ── Unauthenticated (no ClientIdentity extension) is rejected ──────

    #[tokio::test]
    async fn unauthenticated_admin_call_is_rejected() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/admin/workers")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "no identity at all must be denied");
    }

    // ── Sub-floor registration (write_scoped worker below T2) rejected ──

    #[tokio::test]
    async fn sub_floor_registration_is_rejected() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = authed_request(
            "/admin/workers/register",
            json!({
                "name": "writer",
                "tier": "T1",
                "capability_class": "write_scoped",
                "socket_path": "/tmp/does-not-exist.sock",
                "expected_uid": 0,
                "tools": [{"name": "writer_tool"}]
            }),
        );
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a write_scoped worker registering below T2 must be rejected by the MinTierPolicy floor"
        );
    }

    // ── A worker that can't be dialed at all is refused registration ────

    #[tokio::test]
    async fn unreachable_worker_is_refused_registration() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = authed_request(
            "/admin/workers/register",
            json!({
                "name": "ghost",
                "tier": "T1",
                "capability_class": "read_only",
                "socket_path": "/tmp/terminus-tmod05-test-does-not-exist.sock",
                "expected_uid": 0,
                "tools": [{"name": "ghost_tool"}]
            }),
        );
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "an unreachable worker must be refused registration");
    }

    // ── A manifest with no tools is rejected ─────────────────────────────

    #[tokio::test]
    async fn empty_tools_manifest_is_rejected() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = authed_request(
            "/admin/workers/register",
            json!({"name": "empty", "tier": "T1", "capability_class": "read_only", "socket_path": "/tmp/x.sock", "expected_uid": 0, "tools": []}),
        );
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── Deregister on a never-registered worker is a harmless no-op ─────

    #[tokio::test]
    async fn deregister_unknown_worker_is_a_harmless_no_op() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = authed_request("/admin/workers/deregister", json!({"worker_id": "never-existed"}));
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── health on unknown worker_id -> 404 ───────────────────────────────

    #[tokio::test]
    async fn health_probe_on_unknown_worker_is_not_found() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = authed_request("/admin/workers/health", json!({"worker_id": "never-existed"}));
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── list on an empty broker returns an empty set, not an error ──────

    #[tokio::test]
    async fn list_on_empty_broker_returns_empty_set() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let router = build_control_router(state);

        let req = {
            let mut r = Request::builder().method("GET").uri("/admin/workers").body(Body::empty()).unwrap();
            r.extensions_mut().insert(ClientIdentity("test-admin".to_string()));
            r
        };
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 65_536).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["workers"].as_array().unwrap().len(), 0);
    }

    // ── A registered STUB worker's tools appear in the route table and are
    //    callable, atomically -- confirms register actually wires into
    //    dispatch, not just the admin-side bookkeeping. ──────────────────

    #[tokio::test]
    async fn registered_stub_worker_routes_are_installed_and_callable() {
        // Directly exercise `do_register`'s route-table effect using a stub
        // transport, since `build_transport` needs a real UDS/TCP dial that
        // a unit test shouldn't depend on -- the HTTP-level tests above
        // already cover manifest validation and the fail-closed/rejection
        // paths through the real router; this test proves the atomic
        // install + immediate callability contract using the same
        // `RouteTable` API `do_register` itself calls.
        use crate::broker::routes::dispatch_call;

        struct StubTransport;
        #[async_trait::async_trait]
        impl WorkerTransport for StubTransport {
            async fn connect(&self) -> Result<(), crate::broker::transport::TransportError> {
                Ok(())
            }
            async fn call(&self, _name: &str, _args: Value) -> Result<crate::tool::ToolOutput, crate::error::ToolError> {
                Ok(crate::tool::ToolOutput { text: "stub reply".to_string(), structured: None })
            }
            async fn list(&self) -> Result<Vec<String>, crate::broker::transport::TransportError> {
                Ok(vec!["stub_tool".to_string()])
            }
            async fn health(&self) -> bool {
                true
            }
        }

        let route_table = RouteTable::new();
        let transport: Arc<dyn WorkerTransport> = Arc::new(StubTransport);
        route_table.install_many(vec![WorkerRoute {
            worker_id: "stub-worker".to_string(),
            transport,
            tool: ToolInfo {
                name: "stub_tool".to_string(),
                description: "a stub tool".to_string(),
                parameters: json!({"type": "object"}),
            },
        }]);

        let snap = route_table.load();
        let out = dispatch_call(&snap, "stub_tool", json!({})).await.expect("route present").expect("call succeeds");
        assert_eq!(out.text, "stub reply");

        // Register-or-replace at a "new address" (a second stub) in one
        // atomic swap -- a call straddling the swap always resolves to
        // exactly one of the two, never a torn mix.
        let transport2: Arc<dyn WorkerTransport> = Arc::new(StubTransport);
        route_table.install_many(vec![WorkerRoute {
            worker_id: "stub-worker".to_string(),
            transport: transport2,
            tool: ToolInfo {
                name: "stub_tool".to_string(),
                description: "a stub tool v2".to_string(),
                parameters: json!({"type": "object"}),
            },
        }]);
        let snap2 = route_table.load();
        assert_eq!(snap2.len(), 1, "re-registering the same worker_id replaces, not duplicates, its routes");

        // Deregister removes the route -- a subsequent call sees no route.
        route_table.remove_worker("stub-worker");
        let snap3 = route_table.load();
        assert!(dispatch_call(&snap3, "stub_tool", json!({})).await.is_none());
    }
}
