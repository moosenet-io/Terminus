//! Tool catalog merge + source namespacing + routing (MESH-03), built on
//! MESH-01's [`crate::mesh::registry::UpstreamRegistry`] and MESH-02's
//! [`crate::mesh::client::UpstreamPool`]/[`crate::mesh::client::UpstreamClient`].
//!
//! Before this item, `src/mcp_server.rs`'s `tools/list` advertised local core
//! tools (unprefixed) plus, when a single hard-coded personal-registry
//! federation is configured, that federation's tool metadata (also
//! unprefixed — TGW-02's `personal_only_tool_metadata`). There was no notion
//! of *many* upstreams sharing one merged catalog, and consequently no need
//! to disambiguate "whose tool is this" or to route a `tools/call` to the
//! right one.
//!
//! This module adds exactly that for the MESH-01/02 many-upstream registry:
//! every federated (mesh) tool is advertised as `<namespace>__<tool>` (see
//! [`MESH_NS_SEP`]) so two upstreams can each export a tool with the same
//! bare name (e.g. both export `echo`) without colliding on the merged
//! catalog, and a caller's provenance is explicit from the name alone. Local
//! core tools (and the pre-existing single personal-registry federation, see
//! `crate::registry::personal_only_tool_metadata`) are deliberately left
//! UNPREFIXED — this module only namespaces tools sourced from the MESH-01/02
//! registry, and is purely additive: when the mesh registry/pool is empty
//! (the `TERMINUS_MESH_ENABLED` default), nothing here changes what
//! `tools/list`/`tools/call` advertise or how they dispatch.
//!
//! ## Two routing paths, deliberately different costs
//! - [`MergedCatalog::build`] is the expensive, *complete* path: it calls
//!   `list_tools()` on every currently-healthy upstream (one network round
//!   trip each) to produce the full advertised catalog for `tools/list`,
//!   alongside a [`RoutingTable`] recording each advertised name's
//!   provenance. An upstream whose `list_tools()` call fails mid-build is
//!   excluded from the merged catalog for this build (logged, not fatal) —
//!   mirroring [`crate::mesh::client::UpstreamPool`]'s own "one bad upstream
//!   never takes down the others" convention.
//! - [`resolve_call_route`] is the cheap, *per-call* path `tools/call` uses:
//!   routing a single namespaced name to its owning upstream only requires
//!   knowing which namespace belongs to which (healthy) client — it never
//!   needs each upstream's full tool inventory, so it does zero network
//!   calls. This is why `tools/call` does not simply consult a cached
//!   [`RoutingTable`] from the last `tools/list`: that table can go stale the
//!   moment an upstream's health flips, whereas [`resolve_call_route`]
//!   reflects [`crate::mesh::client::UpstreamPool`]'s health state at the
//!   instant of the call.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::client::{UpstreamClient, UpstreamPool};

/// Separator between an upstream's namespace and a tool's bare name in an
/// advertised, federated tool name (`<namespace>__<tool>`). A well-known
/// constant, not configuration — every mesh peer and every caller must agree
/// on it, so it is not something an operator should be able to override per
/// deployment.
pub const MESH_NS_SEP: &str = "__";

/// Build the advertised, namespaced tool name for a tool sourced from
/// upstream `namespace`.
pub fn namespaced(namespace: &str, tool: &str) -> String {
    format!("{namespace}{MESH_NS_SEP}{tool}")
}

/// Split an advertised tool name on the FIRST occurrence of [`MESH_NS_SEP`]
/// into `(namespace, bare_name)`. Purely syntactic — this does NOT check
/// whether `namespace` is actually a known/registered upstream namespace;
/// that is [`RoutingTable`]'s and [`resolve_call_route`]'s job. Returns
/// `None` when the separator is absent, or when either half would be empty
/// (e.g. a name that starts or ends with `__`), since neither is a
/// syntactically valid `<namespace>__<tool>` pair.
///
/// A tool whose OWN bare name contains `__` is still handled correctly:
/// only the first `__` is treated as the namespace boundary, so
/// `"ns__foo__bar"` splits to `("ns", "foo__bar")`, matching
/// [`namespaced`]'s own construction (`namespaced("ns", "foo__bar") ==
/// "ns__foo__bar"`) — i.e. the two functions round-trip.
pub fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    let idx = name.find(MESH_NS_SEP)?;
    let namespace = &name[..idx];
    let bare = &name[idx + MESH_NS_SEP.len()..];
    if namespace.is_empty() || bare.is_empty() {
        return None;
    }
    Some((namespace, bare))
}

/// Where an advertised tool name dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Dispatch locally — the local core registry (and, unchanged by this
    /// item, the pre-existing single personal-registry federation).
    /// Also the fallback for any name whose `__`-prefix does not match a
    /// currently-known mesh namespace, per this module's doc.
    Local,
    /// Dispatch to the named mesh upstream, calling it with `bare_name`
    /// (the namespace prefix already stripped).
    Upstream { namespace: String, bare_name: String },
}

/// Advertised-name → [`Route`] map, the output of [`MergedCatalog::build`].
/// A name absent from the table resolves to [`Route::Local`] (see
/// [`RoutingTable::get`]) — consistent with [`Route::Local`] being the
/// correct behavior for both genuinely-local names and unrecognized-looking
/// namespaced names.
#[derive(Debug, Clone, Default)]
pub struct RoutingTable {
    routes: HashMap<String, Route>,
}

impl RoutingTable {
    pub fn new() -> Self {
        Self { routes: HashMap::new() }
    }

    pub fn insert(&mut self, advertised_name: impl Into<String>, route: Route) {
        self.routes.insert(advertised_name.into(), route);
    }

    /// Resolve an advertised name to its route. Defaults to [`Route::Local`]
    /// for any name not present in the table.
    pub fn get(&self, advertised_name: &str) -> Route {
        self.routes.get(advertised_name).cloned().unwrap_or(Route::Local)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// The merged `tools/list` catalog: local core tools (unprefixed, passed
/// through as-is) plus every currently-healthy mesh upstream's tools
/// (namespaced), alongside the [`RoutingTable`] that maps each advertised
/// name back to its source.
pub struct MergedCatalog {
    /// MCP `Tool` JSON objects (`name`/`description`/`inputSchema`), ready to
    /// drop into a `tools/list` result's `"tools"` array.
    pub tools: Vec<Value>,
    pub routing: RoutingTable,
}

impl MergedCatalog {
    /// `local_tools` are already-shaped MCP `Tool` JSON objects (each with a
    /// `"name"` field) — e.g. what `src/mcp_server.rs`'s `tools/list` handler
    /// already builds from the local `ToolRegistry` (and, unchanged, the
    /// single personal-registry federation). They are passed through
    /// unmodified and their names route [`Route::Local`].
    ///
    /// Every currently-healthy upstream in `pool` is asked for its tools
    /// (`UpstreamClient::list_tools`); each one's tools are namespaced (see
    /// [`namespaced`]) and appended. An upstream whose `list_tools()` call
    /// fails is excluded from the merged catalog for this build (a
    /// `tracing::warn!`, not a build failure) — the same "one bad upstream
    /// doesn't take the others down" posture `UpstreamPool` itself follows.
    pub async fn build(local_tools: Vec<Value>, pool: &UpstreamPool) -> Self {
        let mut routing = RoutingTable::new();
        let mut tools = Vec::with_capacity(local_tools.len());

        for tool in local_tools {
            if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                routing.insert(name.to_string(), Route::Local);
            }
            tools.push(tool);
        }

        for client in pool.healthy_clients() {
            match client.list_tools().await {
                Ok(upstream_tools) => {
                    for ut in upstream_tools {
                        let advertised = namespaced(client.namespace(), &ut.name);
                        routing.insert(
                            advertised.clone(),
                            Route::Upstream {
                                namespace: client.namespace().to_string(),
                                bare_name: ut.name.clone(),
                            },
                        );
                        tools.push(json!({
                            "name": advertised,
                            "description": ut.description,
                            "inputSchema": ut.input_schema,
                        }));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "mesh: excluding upstream \"{}\" (namespace \"{}\") from the merged catalog: {e}",
                        client.name(),
                        client.namespace(),
                    );
                }
            }
        }

        Self { tools, routing }
    }
}

/// The outcome of routing a single `tools/call` name against `pool`'s
/// CURRENT health state — see the module doc for why this is a separate,
/// cheap, no-network-call path distinct from [`MergedCatalog::build`].
#[derive(Debug)]
pub enum CallRoute<'a> {
    /// Dispatch locally (genuinely local name, or a `__`-containing name
    /// whose prefix is not a known mesh namespace at all).
    Local,
    /// Dispatch to this healthy upstream client with `bare_name`.
    Upstream { client: &'a UpstreamClient, bare_name: String },
    /// `namespace` IS a known, registered mesh upstream, but it is not
    /// currently healthy (or was excluded from the pool entirely, e.g. a
    /// missing credential at startup) — callers should surface a clean
    /// tool-error ("upstream unavailable"), never attempt a call and never
    /// fall back to local dispatch (a namespaced name is never coincidentally
    /// a local tool).
    Unavailable { namespace: String },
}

/// Route a single advertised `tools/call` name against `pool`'s current
/// health state. Does not perform any network I/O — it only inspects
/// `pool`'s already-tracked health flags (see
/// `crate::mesh::client::UpstreamPool::healthy_clients`/`all_clients`).
pub fn resolve_call_route<'a>(name: &str, pool: &'a UpstreamPool) -> CallRoute<'a> {
    let Some((namespace, bare_name)) = split_namespaced(name) else {
        return CallRoute::Local;
    };

    if let Some(client) = pool.healthy_clients().find(|c| c.namespace() == namespace) {
        return CallRoute::Upstream { client, bare_name: bare_name.to_string() };
    }

    if pool.all_clients().any(|c| c.namespace() == namespace) {
        return CallRoute::Unavailable { namespace: namespace.to_string() };
    }

    // `__`-shaped name, but the prefix isn't a namespace this pool knows
    // about at all — treated as an ordinary (probably-unknown) local tool
    // name, per this module's doc.
    CallRoute::Local
}

/// The clean, user-facing tool-error text for a [`CallRoute::Unavailable`]
/// namespace — a single place both `src/mcp_server.rs` and this module's own
/// tests use, so the wording stays consistent.
pub fn upstream_unavailable_text(namespace: &str) -> String {
    format!("mesh upstream \"{namespace}\" is currently unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::registry::UpstreamRegistry;
    use httpmock::MockServer;
    use serial_test::serial;

    fn tool_json(name: &str) -> Value {
        json!({"name": name, "description": "a local tool", "inputSchema": {"type": "object"}})
    }

    fn init_mock_response() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-upstream", "version": "0.0.0"}
        }})
    }

    fn mount_list_tools(server: &MockServer, tool_name: &str) {
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "s").json_body(init_mock_response());
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "tools/list"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 2,
                "result": {"tools": [
                    {"name": tool_name, "description": "echoes input", "inputSchema": {"type": "object"}}
                ]}
            }));
        });
    }

    fn bearer_upstream_json(name: &str, base_url: &str, namespace: &str, secret_key: &str) -> String {
        format!(
            r#"{{"name":"{name}","url":"{base_url}","transport":"bearer","namespace":"{namespace}","secret_key":"{secret_key}"}}"#
        )
    }

    // ── namespaced / split_namespaced ───────────────────────────────────────

    #[test]
    fn namespaced_round_trips_with_split_namespaced() {
        let advertised = namespaced("mockns", "echo");
        assert_eq!(advertised, "mockns__echo");
        assert_eq!(split_namespaced(&advertised), Some(("mockns", "echo")));
    }

    #[test]
    fn split_namespaced_only_splits_on_the_first_separator() {
        // The upstream's own bare tool name contains "__" -- only the first
        // occurrence is the namespace boundary.
        let advertised = namespaced("mockns", "foo__bar");
        assert_eq!(advertised, "mockns__foo__bar");
        assert_eq!(split_namespaced(&advertised), Some(("mockns", "foo__bar")));
    }

    #[test]
    fn split_namespaced_rejects_a_name_with_no_separator() {
        assert_eq!(split_namespaced("plain_local_tool"), None);
    }

    #[test]
    fn split_namespaced_rejects_leading_or_trailing_separator() {
        assert_eq!(split_namespaced("__bare"), None);
        assert_eq!(split_namespaced("ns__"), None);
    }

    // ── resolve_call_route: unknown-namespace name is treated as Local ─────

    #[test]
    fn resolve_call_route_treats_unknown_namespace_as_local() {
        let pool = UpstreamPool::from_registry(&UpstreamRegistry::empty());
        let route = resolve_call_route("totallyunknownns__thing", &pool);
        assert!(matches!(route, CallRoute::Local));
    }

    #[test]
    fn resolve_call_route_local_for_plain_name() {
        let pool = UpstreamPool::from_registry(&UpstreamRegistry::empty());
        assert!(matches!(resolve_call_route("health", &pool), CallRoute::Local));
    }

    // ── Two upstreams, same bare tool name: no collision, correct routing ──

    #[tokio::test]
    #[serial]
    async fn two_upstreams_with_same_bare_tool_name_are_namespaced_and_routed_distinctly() {
        std::env::set_var("MESH_MERGE_TEST_TOKEN_A", "fixture-token-a"); // pii-test-fixture
        std::env::set_var("MESH_MERGE_TEST_TOKEN_B", "fixture-token-b"); // pii-test-fixture

        let server_a = MockServer::start();
        let server_b = MockServer::start();
        mount_list_tools(&server_a, "echo");
        mount_list_tools(&server_b, "echo");

        let json = format!(
            "[{},{}]",
            bearer_upstream_json("upstream-a", &server_a.base_url(), "nsa", "MESH_MERGE_TEST_TOKEN_A"),
            bearer_upstream_json("upstream-b", &server_b.base_url(), "nsb", "MESH_MERGE_TEST_TOKEN_B"),
        );
        let registry = UpstreamRegistry::from_json(&json).expect("valid json");
        let pool = UpstreamPool::from_registry(&registry);

        let local_tools = vec![tool_json("echo")];
        let catalog = MergedCatalog::build(local_tools, &pool).await;

        let names: Vec<&str> =
            catalog.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        // Local "echo" stays unprefixed; each upstream's "echo" is namespaced
        // distinctly -- three total entries, no collision.
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"nsa__echo"));
        assert!(names.contains(&"nsb__echo"));

        assert_eq!(catalog.routing.get("echo"), Route::Local);
        assert_eq!(
            catalog.routing.get("nsa__echo"),
            Route::Upstream { namespace: "nsa".to_string(), bare_name: "echo".to_string() }
        );
        assert_eq!(
            catalog.routing.get("nsb__echo"),
            Route::Upstream { namespace: "nsb".to_string(), bare_name: "echo".to_string() }
        );

        // Per-call routing agrees with the catalog's routing table.
        match resolve_call_route("nsa__echo", &pool) {
            CallRoute::Upstream { client, bare_name } => {
                assert_eq!(client.namespace(), "nsa");
                assert_eq!(bare_name, "echo");
            }
            other => panic!("expected Upstream route, got {other:?}"),
        }
        match resolve_call_route("nsb__echo", &pool) {
            CallRoute::Upstream { client, bare_name } => {
                assert_eq!(client.namespace(), "nsb");
                assert_eq!(bare_name, "echo");
            }
            other => panic!("expected Upstream route, got {other:?}"),
        }
        assert!(matches!(resolve_call_route("echo", &pool), CallRoute::Local));

        std::env::remove_var("MESH_MERGE_TEST_TOKEN_A");
        std::env::remove_var("MESH_MERGE_TEST_TOKEN_B");
    }

    // ── Down upstream: namespaced call routes to a clean Unavailable, no panic ─

    #[tokio::test]
    #[serial]
    async fn namespaced_call_to_down_upstream_routes_to_clean_unavailable() {
        std::env::set_var("MESH_MERGE_TEST_TOKEN_DOWN", "fixture-token"); // pii-test-fixture
        let json = bearer_upstream_json(
            "down-upstream",
            "http://127.0.0.1:1",
            "downns",
            "MESH_MERGE_TEST_TOKEN_DOWN",
        );
        let registry = UpstreamRegistry::from_json(&format!("[{json}]")).expect("valid json");
        let pool = UpstreamPool::from_registry(&registry);

        // Force a health probe against the unreachable upstream so the pool
        // marks it unhealthy (a freshly-built pool starts optimistically
        // healthy -- see `UpstreamPool::from_registry`'s doc).
        pool.health_check_all().await;

        match resolve_call_route("downns__whatever", &pool) {
            CallRoute::Unavailable { namespace } => assert_eq!(namespace, "downns"),
            other => panic!("expected Unavailable route for a down upstream, got {other:?}"),
        }
        let text = upstream_unavailable_text("downns");
        assert!(text.contains("downns"));
        assert!(text.to_lowercase().contains("unavailable"));

        // The merged catalog build must also exclude the down upstream's
        // tools cleanly, not panic or error the whole build.
        let catalog = MergedCatalog::build(vec![tool_json("health")], &pool).await;
        let names: Vec<&str> =
            catalog.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        assert_eq!(names, vec!["health"]);

        std::env::remove_var("MESH_MERGE_TEST_TOKEN_DOWN");
    }

    // ── Mesh-disabled / empty pool: additive, behaves like today ────────────

    #[tokio::test]
    async fn empty_pool_merged_catalog_is_just_the_local_tools_unchanged() {
        let pool = UpstreamPool::from_registry(&UpstreamRegistry::empty());
        let local_tools = vec![tool_json("health"), tool_json("ledger_accounts")];
        let catalog = MergedCatalog::build(local_tools.clone(), &pool).await;
        assert_eq!(catalog.tools, local_tools);
        assert_eq!(catalog.routing.len(), 2);
        assert_eq!(catalog.routing.get("health"), Route::Local);
    }
}
