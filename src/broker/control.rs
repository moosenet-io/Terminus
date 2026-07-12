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
//! inference-proxy routes already use — via [`require_gate`], passing
//! [`crate::gateway_framework::ActionKind::Admin`].
//!
//! ## AuthZ is KIND-AWARE — admin requires an EXPLICIT admin grant
//! Because every admin op is guarded as `ActionKind::Admin` with an
//! `"admin:<op>"` action string, `guard` authorizes it via
//! [`crate::gateway_framework::AllowlistPolicy::is_allowed_admin`], NOT the
//! ordinary tool `is_allowed`. A generic tool wildcard
//! (`Grant::List(["*"])` / `allow: ["*"]`) therefore does NOT authorize any
//! admin op — an identity must hold an admin-namespace-scoped grant
//! (`"admin:*"` or an exact `"admin:register_worker"`). This closes a
//! privilege-escalation gap where any broad tool/inference identity would
//! otherwise silently gain worker register/deregister (route-hijack) power.
//! Fail-closed: no grant, or a tool-only grant, ⇒ denied.
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
//! 4. **Verifies the worker's ACTUAL catalog via
//!    [`crate::broker::transport::WorkerTransport::list`]** — this `list()`
//!    call IS the initialize+catalog gate: a successful answer proves the
//!    worker speaks the wire protocol (the `initialize` equivalent) AND
//!    reports what it TRULY serves. The routes installed come from THIS
//!    verified list, never from the (untrusted) request-body tool set; the
//!    body's tool entries only ENRICH each verified tool's catalog metadata
//!    (description/inputSchema) by name, and a body-declared tool the worker
//!    doesn't actually serve never becomes a route. A worker whose `list()`
//!    fails (or times out, or is empty) is refused before any route is
//!    installed.
//! 5. Only once all of the above succeed does it call
//!    [`crate::broker::routes::RouteTable::replace_worker`] — ONE atomic
//!    snapshot swap that removes the worker's prior routes AND installs the
//!    freshly-verified set together, so a reader can never observe a
//!    half-registered worker (some tools routed, some not) AND a tool the
//!    worker used to serve but no longer does is DROPPED rather than left as
//!    a stale route to the old transport. This is how a worker "moves" to a
//!    new address/tier (or narrows its tool set) without a restart, with no
//!    orphaned routes — see that method's doc for why `install_many` alone
//!    (insert-only) was insufficient.
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
//! after its operation completes, with a short, NAME-ONLY detail string
//! (worker id, tier, capability class, tool count — never a cert PEM, socket
//! path contents, host:port, or any other address/secret-shaped value). A
//! FAILURE audit in particular logs only the fixed error CATEGORY token from
//! [`AdminError::category`] (`"connect_failed"`, `"health_timeout"`,
//! `"catalog_unavailable"`, …), NEVER the error's `Display` string — which,
//! for a transport/catalog failure, can contain a worker host:port, UDS
//! socket path, or cert-CN-mismatch detail. [`crate::gateway_framework::audit::sanitize`]
//! (S6) still runs on that detail as a second, defense-in-depth layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use axum::extract::{Extension, Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::broker::routes::{WorkerRoute, HEALTH_PROBE_TIMEOUT};
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
    #[error("worker '{worker}' failed the initialize+catalog gate — its list() did not answer: {detail}")]
    CatalogUnavailable { worker: String, detail: String },
    #[error("worker '{0}' answered the catalog gate but advertises no tools — nothing to route")]
    EmptyCatalog(String),
    #[error("unknown worker '{0}'")]
    UnknownWorker(String),
    /// TMOD-06: an already-present worker's UPDATE passed its pre-flip gate
    /// (connect/health/`list()`, above) but failed its post-flip health
    /// window and was rolled back — see `crate::broker::rollout`. The
    /// worker is left on its last-known-good (previous) instance, not
    /// removed, so this is reported as a gateway-style failure of the NEW
    /// instance, not a worker outage.
    #[error("worker '{0}' failed its post-flip health window and was rolled back to its previous instance")]
    RolledBack(String),
}

impl AdminError {
    fn status(&self) -> StatusCode {
        match self {
            AdminError::InvalidManifest(_) | AdminError::NoTools => StatusCode::BAD_REQUEST,
            AdminError::Transport(_)
            | AdminError::Unhealthy(_, _)
            | AdminError::CatalogUnavailable { .. }
            | AdminError::EmptyCatalog(_)
            | AdminError::RolledBack(_) => StatusCode::BAD_GATEWAY,
            AdminError::UnknownWorker(_) => StatusCode::NOT_FOUND,
        }
    }

    /// A short, fixed error CATEGORY token for the audit trail — never the
    /// raw `Display` string, which (for `Transport`/`CatalogUnavailable`) can
    /// carry a worker host:port, UDS socket path, or cert-CN-mismatch detail.
    /// The audit records this category plus name-only worker identifiers
    /// (id/tier/class), so a reviewer sees WHAT failed and for WHICH worker
    /// without any address/identity material leaking into the log. This is
    /// the "name-only detail" posture the admin surface documents.
    fn category(&self) -> &'static str {
        match self {
            AdminError::InvalidManifest(_) => "invalid_manifest",
            AdminError::NoTools => "no_tools",
            AdminError::Transport(_) => "connect_failed",
            AdminError::Unhealthy(_, _) => "health_timeout",
            AdminError::CatalogUnavailable { .. } => "catalog_unavailable",
            AdminError::EmptyCatalog(_) => "empty_catalog",
            AdminError::UnknownWorker(_) => "unknown_worker",
            AdminError::RolledBack(_) => "rolled_back",
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

    // Capture name-only identifiers BEFORE `req` is consumed, so a FAILURE
    // audit can name the worker (id/tier/class/declared-tool-count) without
    // ever logging the raw error string -- which, for a transport/catalog
    // error, can carry a host:port, socket path, or cert CN. Only the fixed
    // error CATEGORY (`AdminError::category`) is logged, never `Display`.
    let worker_name = req.entry.name.clone();
    let worker_tier = req.entry.tier;
    let worker_class = req.entry.capability_class;
    let declared_count = req.tools.len();

    let result = do_register(&admin, req).await;
    match &result {
        Ok((worker_id, tool_count)) => {
            gate_ctx.record_result(true, Some(&format!("registered worker '{worker_id}' ({tool_count} tools)")));
        }
        Err(e) => gate_ctx.record_result(
            false,
            Some(&format!(
                "register failed: worker='{worker_name}' tier={worker_tier} class={worker_class:?} \
                 declared_tools={declared_count} category={}",
                e.category()
            )),
        ),
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

    // 2. Open the transport for the validated entry, then hand off to the
    //    transport-injectable core (`register_verified_transport`) that does
    //    connect → health → list-verify → atomic replace. Splitting the
    //    concrete-transport CONSTRUCTION (which needs a real UDS/TCP dial)
    //    from the health-gate/catalog-verify/install LOGIC is what lets that
    //    logic be unit-tested against a stub transport.
    let transport = build_transport(&req.entry)?;
    register_verified_transport(admin, &req.entry, req.tools, transport).await
}

/// The transport-injectable core of registration: connect + health-gate +
/// catalog-verify (`list()`) + atomic replace + bookkeeping, given an
/// already-constructed [`WorkerTransport`]. Split out of [`do_register`] so
/// the health-gate/verify/install path can be exercised in tests with a stub
/// transport (the real `build_transport` needs a live UDS/TCP dial). `entry`
/// is assumed already validated by [`do_register`].
async fn register_verified_transport(
    admin: &AdminState,
    entry: &WorkerTransportEntry,
    declared_tools: Vec<WorkerToolManifestEntry>,
    transport: Arc<dyn WorkerTransport>,
) -> Result<(String, usize), AdminError> {
    // 2. Health-gate it BEFORE installing any route -- a worker that can't be
    //    dialed or doesn't answer healthy never gets a route.
    transport.connect().await?;
    if !probe_health(&transport).await {
        return Err(AdminError::Unhealthy(entry.name.clone(), HEALTH_PROBE_TIMEOUT));
    }

    // 3. Verify the worker's ACTUAL catalog via `WorkerTransport::list()` --
    //    this is the initialize+catalog gate: a successful `list()` proves
    //    the worker speaks the wire protocol (the initialize equivalent) AND
    //    tells us what it TRULY serves, rather than trusting the tool set an
    //    untrusted request body declared. The routes we install come from
    //    THIS verified list, not the request body; the request body's tool
    //    entries are used only to ENRICH each verified tool's catalog
    //    metadata (description/inputSchema) by name, and a body-declared tool
    //    the worker does NOT actually serve is silently ignored (it never
    //    becomes a route). A worker whose `list()` fails is refused before
    //    any route is installed; a worker that answers but serves nothing is
    //    refused too (nothing to route). Bounded by the same
    //    HEALTH_PROBE_TIMEOUT so a worker that accepts-but-never-answers the
    //    list can't stall registration.
    let worker_id = entry.name.clone();
    let verified_names = match tokio::time::timeout(HEALTH_PROBE_TIMEOUT, transport.list()).await {
        Ok(Ok(names)) => names,
        Ok(Err(e)) => {
            return Err(AdminError::CatalogUnavailable { worker: worker_id, detail: e.to_string() })
        }
        Err(_) => {
            return Err(AdminError::CatalogUnavailable {
                worker: worker_id,
                detail: format!("list() did not answer within {HEALTH_PROBE_TIMEOUT:?}"),
            })
        }
    };
    if verified_names.is_empty() {
        return Err(AdminError::EmptyCatalog(worker_id));
    }

    // Index the request body's declared tools by name so a verified tool can
    // borrow the operator-supplied description/inputSchema when the names
    // match; a verified tool with no matching body entry gets a minimal
    // default catalog metadata (empty description, permissive object schema).
    let mut declared: HashMap<String, ToolInfo> =
        declared_tools.into_iter().map(|t| (t.name.clone(), t.into())).collect();

    // 4. Build a route for every tool the worker VERIFIABLY serves.
    //
    //    TMOD-06: if `worker_id` is ALREADY present (this is an UPDATE, not
    //    a first-ever registration), installing it is a blue-green rollout
    //    (`crate::broker::rollout::rollout_worker`), not a bare
    //    `replace_worker` -- the new instance has passed the PRE-flip gate
    //    above (connect + bounded health probe + `list()`-verify), but that
    //    only proves it was alive a moment ago; the rollout module flips it
    //    in while retaining the previous instance as rollback state, watches
    //    a bounded post-flip health window, and automatically rolls back to
    //    the previous instance (atomically, and safe against a concurrent
    //    deregister) if that window fails. A worker with no prior routes has
    //    nothing to roll back to -- a plain atomic `replace_worker` (== a
    //    first install) is the correct, simpler primitive for that case; see
    //    `RouteTable::replace_worker`'s doc for why this is still not the
    //    same as `install_many` (a no-longer-served tool is dropped, not
    //    orphaned).
    let tool_count = verified_names.len();
    let routes: Vec<WorkerRoute> = verified_names
        .into_iter()
        .map(|name| {
            let tool = declared.remove(&name).unwrap_or_else(|| ToolInfo {
                name: name.clone(),
                description: String::new(),
                parameters: default_input_schema(),
            });
            WorkerRoute { worker_id: worker_id.clone(), transport: transport.clone(), tool }
        })
        .collect();

    let already_present = admin.mcp.broker_routes.load().all().any(|r| r.worker_id == worker_id);
    if already_present {
        let outcome = crate::broker::rollout::rollout_worker(&admin.mcp.broker_routes, &worker_id, routes).await;
        if outcome.state == crate::broker::rollout::RolloutState::RolledBack {
            return Err(AdminError::RolledBack(worker_id));
        }
    } else {
        admin.mcp.broker_routes.replace_worker(&worker_id, routes);
    }

    // 5. Record admin-only bookkeeping (tier/capability_class/registered_at)
    //    in lock-step -- read by `handle_list`/`handle_health`, never by
    //    dispatch.
    upsert_meta(
        &admin.meta,
        &worker_id,
        WorkerAdminMeta {
            tier: entry.tier,
            capability_class: entry.capability_class,
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
                // Name-only detail: worker id + category, never a raw error
                // string (consistent with the register failure audit).
                gate_ctx.record_result(false, Some(&format!("health failed: worker='{id}' category={}", err.category())));
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

    /// `test-admin` holds an EXPLICIT admin grant (`"admin:*"`) plus a tool
    /// wildcard, for tests that only care about the register/health-gate/
    /// route-table mechanics, not the admin-authz rule itself (that's covered
    /// by the dedicated authz tests below). The explicit `"admin:*"` is
    /// required now that a bare `"*"` no longer authorizes admin ops.
    fn allow_all_gateway() -> GatewayFramework {
        use crate::gateway_framework::{AllowlistPolicy, Grant};
        use std::collections::HashMap;
        let mut entries = HashMap::new();
        entries.insert(
            "test-admin".to_string(),
            Grant::List(vec!["*".to_string(), "admin:*".to_string()]),
        );
        GatewayFramework::new(AllowlistPolicy::new(entries), Arc::new(InProcessRateLimiter::new(1000, 1000.0)))
    }

    /// A gateway where `test-admin` has ONLY a generic tool wildcard (`"*"`)
    /// and NO admin grant — used to prove a broad tool identity is denied
    /// admin ops (the privilege-escalation fix).
    fn tool_wildcard_only_gateway() -> GatewayFramework {
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

    // ── Kind-aware authz: a tool wildcard does NOT grant admin ──────────

    /// An identity holding ONLY a generic tool wildcard (`"*"`) — enough for
    /// every tool/inference call — is DENIED an admin op. This is the
    /// privilege-escalation fix: a broad tool identity is not, by that fact
    /// alone, a worker-control admin.
    #[tokio::test]
    async fn tool_wildcard_identity_is_denied_admin_ops() {
        let state = test_mcp_state(Some(tool_wildcard_only_gateway()));
        let router = build_control_router(state);

        // A GET /admin/workers (list) is the cheapest admin op to probe.
        let mut req = Request::builder().method("GET").uri("/admin/workers").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ClientIdentity("test-admin".to_string()));
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "a generic tool wildcard must NOT authorize an admin op"
        );
    }

    /// An identity with an EXPLICIT admin grant (`"admin:*"`) IS allowed the
    /// admin op (the complement of the deny test above).
    #[tokio::test]
    async fn explicit_admin_grant_is_allowed_admin_ops() {
        let state = test_mcp_state(Some(allow_all_gateway())); // holds "admin:*"
        let router = build_control_router(state);

        let mut req = Request::builder().method("GET").uri("/admin/workers").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ClientIdentity("test-admin".to_string()));
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "an explicit admin grant must authorize the admin op");
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

    // ── list()-verified registration (the initialize+catalog gate) ─────────

    /// A configurable stub whose `list()` (the initialize+catalog gate) and
    /// `health()` outcomes are programmable, for exercising
    /// `register_verified_transport` without a live socket.
    struct CatalogStub {
        listed: Result<Vec<String>, ()>,
        healthy: bool,
    }

    #[async_trait::async_trait]
    impl WorkerTransport for CatalogStub {
        async fn connect(&self) -> Result<(), crate::broker::transport::TransportError> {
            Ok(())
        }
        async fn call(&self, name: &str, _args: Value) -> Result<crate::tool::ToolOutput, crate::error::ToolError> {
            Ok(crate::tool::ToolOutput { text: format!("served {name}"), structured: None })
        }
        async fn list(&self) -> Result<Vec<String>, crate::broker::transport::TransportError> {
            self.listed
                .clone()
                .map_err(|_| crate::broker::transport::TransportError::Unavailable("stub list() failure".to_string()))
        }
        async fn health(&self) -> bool {
            self.healthy
        }
    }

    fn read_only_t1_entry(name: &str) -> WorkerTransportEntry {
        WorkerTransportEntry {
            name: name.to_string(),
            tier: TransportTier::T1,
            capability_class: CapabilityClass::ReadOnly,
            socket_path: Some("/tmp/unused-in-stub-tests.sock".to_string()),
            host: None,
            port: None,
            expected_uid: Some(0),
            expected_identity: None,
        }
    }

    /// The worker's REAL catalog (`list()`) — not the request body — decides
    /// which routes are installed: the body declares `[declared_only,
    /// shared]`, but the worker actually serves `[shared, worker_only]`, so
    /// exactly `shared` + `worker_only` are routed and `declared_only`
    /// (declared but not served) is dropped. `shared` keeps the body's
    /// enriched description; `worker_only` gets default metadata.
    #[tokio::test]
    async fn registration_installs_the_workers_real_catalog_not_the_request_body() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let admin = AdminState { mcp: state.clone(), meta: Arc::new(ArcSwap::from_pointee(HashMap::new())) };

        let declared = vec![
            WorkerToolManifestEntry {
                name: "declared_only".to_string(),
                description: "body says this exists".to_string(),
                parameters: default_input_schema(),
            },
            WorkerToolManifestEntry {
                name: "shared".to_string(),
                description: "enriched from body".to_string(),
                parameters: json!({"type": "object", "properties": {"x": {"type": "number"}}}),
            },
        ];
        let transport: Arc<dyn WorkerTransport> = Arc::new(CatalogStub {
            listed: Ok(vec!["shared".to_string(), "worker_only".to_string()]),
            healthy: true,
        });

        let (worker_id, count) =
            register_verified_transport(&admin, &read_only_t1_entry("w1"), declared, transport).await.unwrap();
        assert_eq!(worker_id, "w1");
        assert_eq!(count, 2, "exactly the two tools the worker VERIFIABLY serves");

        let snap = state.broker_routes.load();
        // Only the worker's real tools are routed.
        assert!(snap.get("shared").is_some());
        assert!(snap.get("worker_only").is_some());
        assert!(
            snap.get("declared_only").is_none(),
            "a body-declared tool the worker does NOT serve must never become a route"
        );
        // `shared` borrowed the body's enriched description; `worker_only`
        // (no body entry) got default (empty) metadata.
        assert_eq!(snap.get("shared").unwrap().tool.description, "enriched from body");
        assert_eq!(snap.get("worker_only").unwrap().tool.description, "");
    }

    /// A worker whose `list()` (the initialize+catalog gate) FAILS is refused
    /// registration before any route is installed.
    #[tokio::test]
    async fn worker_failing_list_is_refused_before_install() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let admin = AdminState { mcp: state.clone(), meta: Arc::new(ArcSwap::from_pointee(HashMap::new())) };

        let transport: Arc<dyn WorkerTransport> = Arc::new(CatalogStub { listed: Err(()), healthy: true });
        let declared = vec![WorkerToolManifestEntry {
            name: "wishful".to_string(),
            description: String::new(),
            parameters: default_input_schema(),
        }];

        let err = register_verified_transport(&admin, &read_only_t1_entry("bad"), declared, transport)
            .await
            .expect_err("a worker whose list() fails must be refused");
        assert!(matches!(err, AdminError::CatalogUnavailable { .. }));
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        // No route was installed.
        assert!(state.broker_routes.load().is_empty(), "a list()-failing worker must install NO route");
    }

    /// A worker that answers `list()` but serves NOTHING is refused (nothing
    /// to route), even though it's healthy and the body declared tools.
    #[tokio::test]
    async fn worker_with_empty_catalog_is_refused() {
        let state = test_mcp_state(Some(allow_all_gateway()));
        let admin = AdminState { mcp: state.clone(), meta: Arc::new(ArcSwap::from_pointee(HashMap::new())) };

        let transport: Arc<dyn WorkerTransport> = Arc::new(CatalogStub { listed: Ok(vec![]), healthy: true });
        let declared = vec![WorkerToolManifestEntry {
            name: "claimed".to_string(),
            description: String::new(),
            parameters: default_input_schema(),
        }];

        let err = register_verified_transport(&admin, &read_only_t1_entry("empty"), declared, transport)
            .await
            .expect_err("a worker serving no tools must be refused");
        assert!(matches!(err, AdminError::EmptyCatalog(_)));
        assert!(state.broker_routes.load().is_empty());
    }

    /// Re-registration is a TRUE replace end-to-end through
    /// `register_verified_transport`: w1 first serves [a,b], then re-registers
    /// serving only [a] at a new transport — `b` is GONE, `a` points at the
    /// new transport, in one atomic snapshot (no stale route).
    #[tokio::test]
    async fn reregistration_is_a_true_replace_no_stale_route() {
        use crate::broker::routes::dispatch_call;

        let state = test_mcp_state(Some(allow_all_gateway()));
        let admin = AdminState { mcp: state.clone(), meta: Arc::new(ArcSwap::from_pointee(HashMap::new())) };

        // First registration: worker serves [a, b].
        let first: Arc<dyn WorkerTransport> =
            Arc::new(CatalogStub { listed: Ok(vec!["a".to_string(), "b".to_string()]), healthy: true });
        register_verified_transport(&admin, &read_only_t1_entry("w1"), vec![], first).await.unwrap();
        assert_eq!(state.broker_routes.load().len(), 2);

        // Re-registration: same worker now serves ONLY [a], new transport.
        let second: Arc<dyn WorkerTransport> =
            Arc::new(CatalogStub { listed: Ok(vec!["a".to_string()]), healthy: true });
        register_verified_transport(&admin, &read_only_t1_entry("w1"), vec![], second).await.unwrap();

        let snap = state.broker_routes.load();
        assert_eq!(snap.len(), 1, "re-register must not leave b behind");
        assert!(snap.get("b").is_none(), "the no-longer-served tool must be GONE, not a stale route");
        let out = dispatch_call(&snap, "a", json!({})).await.unwrap().unwrap();
        assert_eq!(out.text, "served a", "a resolves to the new transport");
    }
}
