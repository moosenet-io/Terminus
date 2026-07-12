//! Broker-owned, atomically-swappable tool-name → worker route table
//! (TMOD-04).
//!
//! Before this item, a worker registered over a [`crate::broker::transport::WorkerTransport`]
//! (TMOD-02) had no way to actually receive traffic: nothing in
//! `src/mcp_server.rs`'s `tools/call`/`tools/list` dispatch knew a worker
//! existed. This module closes that gap by generalizing the existing
//! namespaced mesh-upstream route table
//! (`crate::mesh::merge::RoutingTable`/[`crate::mesh::merge::MergedCatalog`])
//! to a broker-local, non-namespaced ("bare tool name") table of routes to
//! IN-BOX workers reached over a [`WorkerTransport`](crate::broker::transport::WorkerTransport).
//!
//! ## Read/write split, same pattern as TMOD-01
//! [`RouteTable`] is a thin wrapper around
//! `arc_swap::ArcSwap<RouteTableSnapshot>`, mirroring
//! `McpServerState::registry`'s `ArcSwap<ToolRegistry>` (TMOD-01):
//! - **Writers** (TMOD-05's worker onboarding/health-eviction logic; this
//!   item ships the mutation API but nothing yet calls it on a live path) go
//!   through [`RouteTable::install`]/[`RouteTable::remove`], which each build
//!   a brand-new [`RouteTableSnapshot`] (copy-on-write over the previous one)
//!   and atomically `store` it. There is no in-place mutation of a snapshot
//!   already in flight to a reader.
//! - **Readers** (`src/mcp_server.rs`'s `handle_mcp`) call [`RouteTable::load`]
//!   ONCE per request, exactly like `state.registry.load()`, and dispatch the
//!   whole request against that one snapshot — a swap that lands mid-request
//!   is never torn: the in-flight call either sees the table as it stood
//!   when the request started, or (for a request that starts after the swap)
//!   the new one, never a mix.
//!
//! ## Dispatch precedence (documented once, here, and enforced in
//! `src/mcp_server.rs`)
//! A `tools/call` whose name exists BOTH in the compiled-in registry
//! snapshot AND as a route in this table always dispatches to the
//! COMPILED-IN tool — the route table is consulted only on a compiled-in
//! MISS. Same precedence for `tools/list`: a name present in both the
//! compiled-in catalog and a healthy worker's advertised catalog is listed
//! once, as the compiled-in tool. This mirrors
//! `crate::mesh::merge`'s "local wins" posture, adapted from mesh's
//! namespaced-vs-unprefixed distinction to this table's flat (unprefixed)
//! name space.
//!
//! ## Health is a per-call/per-list concern, not baked into the snapshot
//! A route's worker being unhealthy does NOT remove it from the table —
//! exactly like `crate::mesh::merge`'s `UpstreamPool` health tracking, health
//! is checked live (via [`crate::broker::transport::WorkerTransport::health`])
//! at the moment of a `tools/list` build or a `tools/call` dispatch, so a
//! worker recovering doesn't require any route-table mutation. An unhealthy
//! worker's routes are simply skipped in `tools/list` and answer a clean
//! "worker unavailable" [`ToolError`] on `tools/call` — one dead worker only
//! ever fails its OWN tools, never any other route or any compiled-in tool.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use crate::error::ToolError;
use crate::registry::ToolInfo;
use crate::tool::ToolOutput;

use super::transport::WorkerTransport;

/// One tool-name's route to the worker that serves it.
#[derive(Clone)]
pub struct WorkerRoute {
    /// Stable identity of the owning worker (e.g. `"gitea-worker"`) — used
    /// only for logging/diagnostics; routing itself is keyed purely on tool
    /// name (see [`RouteTable`]).
    pub worker_id: String,
    /// The transport this route's worker is reached over. An
    /// `Arc<dyn WorkerTransport>` (rather than an owned value) because
    /// several routes from the SAME worker's manifest (it can advertise
    /// multiple tools) share one underlying transport/connection
    /// configuration.
    pub transport: Arc<dyn WorkerTransport>,
    /// The tool's catalog metadata (name/description/inputSchema), sourced
    /// from the worker's advertised manifest — this is what `tools/list`
    /// advertises for a route without needing a live network round trip
    /// (unlike `crate::mesh::merge::MergedCatalog::build`, which re-lists
    /// every upstream on every `tools/list` call; a route's metadata is
    /// already known at install time from the worker's manifest).
    pub tool: ToolInfo,
}

impl std::fmt::Debug for WorkerRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRoute")
            .field("worker_id", &self.worker_id)
            .field("tool", &self.tool.name)
            .finish()
    }
}

/// An immutable point-in-time table of `tool name -> `[`WorkerRoute`].
/// Built fresh (copy-on-write) by [`RouteTable::install`]/[`RouteTable::remove`]
/// — never mutated in place, so a snapshot a reader already holds via
/// [`RouteTable::load`] never changes underneath it.
#[derive(Clone, Default)]
pub struct RouteTableSnapshot {
    routes: HashMap<String, WorkerRoute>,
}

impl RouteTableSnapshot {
    /// Look up the route for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&WorkerRoute> {
        self.routes.get(name)
    }

    /// All routes currently in this snapshot, in arbitrary order — used by
    /// `tools/list` to build the merged catalog.
    pub fn all(&self) -> impl Iterator<Item = &WorkerRoute> {
        self.routes.values()
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// The broker-owned, atomically-swappable route table. See this module's
/// doc for the read/write split and dispatch-precedence contract.
pub struct RouteTable {
    snapshot: ArcSwap<RouteTableSnapshot>,
}

impl RouteTable {
    /// An empty table — the default for every process until TMOD-05's worker
    /// onboarding installs a route. Behavior-preserving: with an empty
    /// table, every `tools/call` compiled-in miss falls through exactly as
    /// it did before this item (to `personal_federation`, or "Unknown
    /// tool").
    pub fn new() -> Self {
        Self { snapshot: ArcSwap::from_pointee(RouteTableSnapshot::default()) }
    }

    /// Take one immutable snapshot for the duration of a single request —
    /// mirrors `McpServerState::registry`'s `load()` contract (TMOD-01):
    /// callers must take exactly one snapshot at the top of a request and
    /// dispatch the whole request against it.
    pub fn load(&self) -> Arc<RouteTableSnapshot> {
        self.snapshot.load_full()
    }

    /// Install (or replace, if `tool.name` already has a route) one route.
    /// Builds a brand-new snapshot (copy-on-write over the current one) and
    /// atomically swaps it in — never mutates a snapshot a reader may
    /// already hold.
    pub fn install(&self, route: WorkerRoute) {
        let current = self.snapshot.load();
        let mut routes = current.routes.clone();
        routes.insert(route.tool.name.clone(), route);
        self.snapshot.store(Arc::new(RouteTableSnapshot { routes }));
    }

    /// Install every route a worker's manifest advertises in one atomic
    /// swap (rather than one `install` per tool, which would otherwise
    /// expose a partially-installed worker to a reader mid-loop).
    pub fn install_many(&self, new_routes: impl IntoIterator<Item = WorkerRoute>) {
        let current = self.snapshot.load();
        let mut routes = current.routes.clone();
        for route in new_routes {
            routes.insert(route.tool.name.clone(), route);
        }
        self.snapshot.store(Arc::new(RouteTableSnapshot { routes }));
    }

    /// Remove a single named route, if present.
    pub fn remove(&self, name: &str) {
        let current = self.snapshot.load();
        if !current.routes.contains_key(name) {
            return;
        }
        let mut routes = current.routes.clone();
        routes.remove(name);
        self.snapshot.store(Arc::new(RouteTableSnapshot { routes }));
    }

    /// Remove every route belonging to `worker_id` in one atomic swap — used
    /// when a worker is deregistered/evicted entirely.
    pub fn remove_worker(&self, worker_id: &str) {
        let current = self.snapshot.load();
        if !current.routes.values().any(|r| r.worker_id == worker_id) {
            return;
        }
        let routes: HashMap<String, WorkerRoute> =
            current.routes.iter().filter(|(_, r)| r.worker_id != worker_id).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.snapshot.store(Arc::new(RouteTableSnapshot { routes }));
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The clean, user-facing tool-error text for a route whose worker is
/// currently unhealthy — parallel to
/// `crate::mesh::merge::upstream_unavailable_text`.
pub fn worker_unavailable_text(worker_id: &str) -> String {
    format!("broker worker \"{worker_id}\" is currently unavailable")
}

/// Dispatch a `tools/call` for `name` against `snapshot`, iff a route
/// exists. Returns:
/// - `None` — no route for `name` at all (caller should continue falling
///   through, e.g. to `personal_federation` or "Unknown tool").
/// - `Some(Err(ToolError::Execution(_)))` (via [`worker_unavailable_text`]) —
///   a route exists but its worker is currently unhealthy. A live health
///   check is performed here (not cached in the snapshot — see this
///   module's doc) so a route table swap need not race a health flip.
/// - `Some(route.transport.call(..))`'s outcome otherwise.
///
/// Mirrors `crate::mesh::merge::resolve_call_route` + the upstream dispatch
/// it feeds in `src/mcp_server.rs`, adapted to this table's flat (bare) name
/// space and in-box [`WorkerTransport`] instead of a namespaced MCP
/// upstream.
pub async fn dispatch_call(
    snapshot: &RouteTableSnapshot,
    name: &str,
    args: Value,
) -> Option<Result<ToolOutput, ToolError>> {
    let route = snapshot.get(name)?;
    if !route.transport.health().await {
        return Some(Err(ToolError::Execution(worker_unavailable_text(&route.worker_id))));
    }
    Some(route.transport.call(name, args).await)
}

/// Build the merged `tools/list` catalog: `local_tools` (already-shaped MCP
/// `Tool` JSON objects, exactly as `src/mcp_server.rs` builds from the
/// compiled-in registry) plus every route in `snapshot` whose worker is
/// CURRENTLY healthy, skipping any route whose name collides with a local
/// tool (compiled-in wins — see this module's doc). Mirrors
/// `crate::mesh::merge::MergedCatalog::build`'s "one bad
/// upstream/worker never takes the others down" posture: an unhealthy
/// worker's routes are silently excluded, not an error.
pub async fn merge_catalog(local_tools: Vec<Value>, snapshot: &RouteTableSnapshot) -> Vec<Value> {
    use std::collections::HashSet;

    let local_names: HashSet<String> = local_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .map(str::to_string)
        .collect();

    let mut tools = local_tools;

    // De-dupe workers so a worker advertising many tools is health-checked
    // once, not once per tool.
    let mut checked: HashMap<String, bool> = HashMap::new();

    for route in snapshot.all() {
        if local_names.contains(route.tool.name.as_str()) {
            // Compiled-in wins -- documented precedence.
            continue;
        }
        let healthy = match checked.get(&route.worker_id) {
            Some(h) => *h,
            None => {
                let h = route.transport.health().await;
                checked.insert(route.worker_id.clone(), h);
                h
            }
        };
        if !healthy {
            continue;
        }
        tools.push(serde_json::json!({
            "name": route.tool.name,
            "description": route.tool.description,
            "inputSchema": route.tool.parameters,
        }));
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::transport::TransportError;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A stub [`WorkerTransport`] for tests -- no real network I/O, just
    /// programmable health/call/list responses.
    struct StubTransport {
        healthy: AtomicBool,
        call_count: AtomicUsize,
        text: String,
    }

    impl StubTransport {
        fn new(healthy: bool, text: &str) -> Self {
            Self { healthy: AtomicBool::new(healthy), call_count: AtomicUsize::new(0), text: text.to_string() }
        }
    }

    #[async_trait::async_trait]
    impl WorkerTransport for StubTransport {
        async fn connect(&self) -> Result<(), TransportError> {
            Ok(())
        }

        async fn call(&self, _name: &str, _args: Value) -> Result<ToolOutput, ToolError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput { text: self.text.clone(), structured: None })
        }

        async fn list(&self) -> Result<Vec<String>, TransportError> {
            Ok(vec![])
        }

        async fn health(&self) -> bool {
            self.healthy.load(Ordering::SeqCst)
        }
    }

    fn tool_info(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn tool_json(name: &str) -> Value {
        serde_json::json!({"name": name, "description": "local tool", "inputSchema": {"type": "object"}})
    }

    // ── Empty table: behavior-preserving ────────────────────────────────

    #[tokio::test]
    async fn empty_table_has_no_route_and_merge_is_identity() {
        let table = RouteTable::new();
        let snap = table.load();
        assert!(snap.is_empty());
        assert!(dispatch_call(&snap, "anything", serde_json::json!({})).await.is_none());

        let local = vec![tool_json("health")];
        let merged = merge_catalog(local.clone(), &snap).await;
        assert_eq!(merged, local);
    }

    // ── Healthy worker route dispatches and is listed ───────────────────

    #[tokio::test]
    async fn healthy_route_dispatches_call_and_appears_in_list() {
        let table = RouteTable::new();
        let transport: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(true, "worker said hi"));
        table.install(WorkerRoute {
            worker_id: "w1".to_string(),
            transport: transport.clone(),
            tool: tool_info("worker_tool"),
        });

        let snap = table.load();
        let out = dispatch_call(&snap, "worker_tool", serde_json::json!({"x": 1}))
            .await
            .expect("route present")
            .expect("call succeeds");
        assert_eq!(out.text, "worker said hi");

        let merged = merge_catalog(vec![tool_json("health")], &snap).await;
        let names: Vec<&str> = merged.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        assert_eq!(names, vec!["health", "worker_tool"]);
    }

    // ── Unhealthy worker: route call is a clean "unavailable", list skips it ─

    #[tokio::test]
    async fn unhealthy_route_is_unavailable_on_call_and_skipped_in_list() {
        let table = RouteTable::new();
        let transport: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(false, "unused"));
        table.install(WorkerRoute {
            worker_id: "w-dead".to_string(),
            transport,
            tool: tool_info("dead_tool"),
        });

        let snap = table.load();
        let err = dispatch_call(&snap, "dead_tool", serde_json::json!({})).await.expect("route present");
        match err {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("w-dead"));
                assert!(msg.to_lowercase().contains("unavailable"));
            }
            other => panic!("expected Execution error, got {other:?}"),
        }

        let merged = merge_catalog(vec![tool_json("health")], &snap).await;
        let names: Vec<&str> = merged.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        assert_eq!(names, vec!["health"]);
    }

    // ── One dead worker only fails its own tools ────────────────────────

    #[tokio::test]
    async fn one_dead_worker_does_not_affect_a_different_healthy_worker() {
        let table = RouteTable::new();
        let dead: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(false, "unused"));
        let alive: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(true, "alive reply"));
        table.install(WorkerRoute { worker_id: "dead".to_string(), transport: dead, tool: tool_info("dead_tool") });
        table.install(WorkerRoute { worker_id: "alive".to_string(), transport: alive, tool: tool_info("alive_tool") });

        let snap = table.load();
        assert!(dispatch_call(&snap, "dead_tool", serde_json::json!({})).await.unwrap().is_err());
        let ok = dispatch_call(&snap, "alive_tool", serde_json::json!({})).await.unwrap().unwrap();
        assert_eq!(ok.text, "alive reply");

        let merged = merge_catalog(vec![], &snap).await;
        let names: Vec<&str> = merged.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        assert_eq!(names, vec!["alive_tool"]);
    }

    // ── Compiled-in wins on a name clash, in both call and list ─────────

    #[tokio::test]
    async fn merge_catalog_excludes_a_route_whose_name_clashes_with_a_local_tool() {
        let table = RouteTable::new();
        let transport: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(true, "worker version"));
        table.install(WorkerRoute {
            worker_id: "w1".to_string(),
            transport,
            tool: tool_info("health"), // clashes with the compiled-in "health" tool
        });

        let snap = table.load();
        let merged = merge_catalog(vec![tool_json("health")], &snap).await;
        // Only the local, compiled-in "health" survives -- no duplicate.
        let healths: Vec<&Value> = merged.iter().filter(|t| t["name"] == "health").collect();
        assert_eq!(healths.len(), 1);
    }

    // ── Unknown name (no registry, no route) -- caller keeps falling through ─

    #[tokio::test]
    async fn unknown_name_yields_no_route_for_caller_to_fall_through() {
        let table = RouteTable::new();
        table.install(WorkerRoute {
            worker_id: "w1".to_string(),
            transport: Arc::new(StubTransport::new(true, "x")),
            tool: tool_info("known_tool"),
        });
        let snap = table.load();
        assert!(dispatch_call(&snap, "totally_unknown", serde_json::json!({})).await.is_none());
    }

    // ── Route-table swap mid-request uses the captured snapshot (no tear) ──

    #[tokio::test]
    async fn swap_mid_request_does_not_affect_an_already_captured_snapshot() {
        let table = RouteTable::new();
        table.install(WorkerRoute {
            worker_id: "w1".to_string(),
            transport: Arc::new(StubTransport::new(true, "v1")),
            tool: tool_info("versioned_tool"),
        });

        // Simulate `handle_mcp`'s `let snap = table.load();` at the top of a
        // request.
        let in_flight_snapshot = table.load();

        // A swap lands "mid-request" -- installs a v2 route AND removes the
        // v1 route entirely (worst case: the tool disappears).
        table.remove("versioned_tool");
        table.install(WorkerRoute {
            worker_id: "w2".to_string(),
            transport: Arc::new(StubTransport::new(true, "v2")),
            tool: tool_info("versioned_tool"),
        });

        // The snapshot captured before the swap still resolves to the OLD
        // (w1/"v1") route -- no tear.
        let out = dispatch_call(&in_flight_snapshot, "versioned_tool", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "v1");

        // A freshly-loaded snapshot (a new request) sees the new route.
        let post_swap_snapshot = table.load();
        let out2 = dispatch_call(&post_swap_snapshot, "versioned_tool", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out2.text, "v2");
    }

    // ── install_many installs atomically; remove_worker removes all its routes ─

    #[tokio::test]
    async fn install_many_and_remove_worker() {
        let table = RouteTable::new();
        let t1: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(true, "a"));
        let t2: Arc<dyn WorkerTransport> = Arc::new(StubTransport::new(true, "b"));
        table.install_many(vec![
            WorkerRoute { worker_id: "multi".to_string(), transport: t1, tool: tool_info("tool_a") },
            WorkerRoute { worker_id: "multi".to_string(), transport: t2, tool: tool_info("tool_b") },
        ]);
        let snap = table.load();
        assert_eq!(snap.len(), 2);

        table.remove_worker("multi");
        let snap2 = table.load();
        assert!(snap2.is_empty());
    }
}
