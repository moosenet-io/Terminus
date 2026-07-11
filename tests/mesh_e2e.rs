//! MESH-14: end-to-end behavior verification for the mesh feature
//! (MESH-01..12), driven against TWO fully in-process mock upstreams.
//!
//! Scope: this test spins zero real network dependencies -- both "upstream
//! Terminus" servers are `httpmock::MockServer` instances bound to
//! `127.0.0.1` on an ephemeral port, matching the pattern `src/mesh/client.rs`
//! and `src/mesh/merge.rs`'s own `#[cfg(test)]` modules already use. Nothing
//! here contacts a live server. Per the moosenet-spec S1 rule, every URL/host
//! is either RFC-2606 example-style (`*.example.test`) or the loopback
//! `httpmock` binds to -- never a real infra IP/hostname.
//!
//! Per this item's brief, the test drives the mesh library surfaces directly
//! (`UpstreamPool` / `MergedCatalog` / `AllowlistPolicy` / `resolve_call_route`
//! / `approval::{is_guarded, gate, mesh_gate_args}`) rather than standing up
//! the full axum `/mcp` router -- deterministic, hermetic, and it exercises
//! exactly the same decision points `src/mcp_server.rs`'s federated dispatch
//! path (see that file's `tools/call` handler, MESH-03/09/10) calls into.
//!
//! Covers, in one flow:
//!   1. Two upstreams, each exporting the SAME bare tool name -> merged
//!      catalog has local (unprefixed) + both upstreams (namespaced), no
//!      collision.
//!   2. `resolve_call_route` sends a namespaced `tools/call` to the correct
//!      upstream client, and an actual `call_tool` against each mock returns
//!      that mock's distinct response (proving routing, not just naming).
//!   3. An unmapped `Principal`/identity gets an empty filtered catalog
//!      (`AllowlistPolicy::filter_tools`) and a denied call
//!      (`AllowlistPolicy::is_allowed` == false) -- RBAC deny contract.
//!   4. A third, unreachable upstream (`meshdown`, dialing a closed loopback
//!      port -- never a real infra host, following `src/mesh/client.rs`'s own
//!      `http://127.0.0.1:1` down-upstream fixture) drops out of a rebuilt
//!      merged catalog once a health probe runs, and its namespace resolves
//!      to `CallRoute::Unavailable`, while upstreams A and B are completely
//!      unaffected and still callable.
//!   5. Approval-gate propagation for a federated guarded tool: classified
//!      guarded via `approval::is_guarded`, content-bound to its target
//!      upstream via `approval::mesh_gate_args` (a code for upstream A's call
//!      never matches upstream B's), and -- following MESH-09's own test
//!      precedent (`src/approval.rs`'s `gate_denies_cleanly_when_database_url_unset`
//!      -style tests) -- `approval::gate` denies cleanly with no live
//!      Postgres available, rather than a live-DB redemption. No DB is spun
//!      up or contacted by this test.

use std::collections::HashMap;

use httpmock::MockServer;
use serde_json::{json, Value};
use serial_test::serial;

use terminus_rs::approval;
use terminus_rs::gateway_framework::{AllowlistPolicy, Grant};
use terminus_rs::mesh::{
    namespaced, resolve_call_route, CallRoute, MergedCatalog, UpstreamPool, UpstreamRegistry,
};

// ── fixtures ─────────────────────────────────────────────────────────────

const TOKEN_A: &str = "MESH_E2E_TOKEN_A";
const TOKEN_B: &str = "MESH_E2E_TOKEN_B";
const TOKEN_DOWN: &str = "MESH_E2E_TOKEN_DOWN";

fn init_response() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "result": {
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "mock-mesh-upstream", "version": "0.0.0"}
    }})
}

/// Mount `initialize` + `tools/list` (advertising one bare tool named
/// `tool_name`) + a callable `tools/call` on `server`, whose reply text
/// includes `reply_marker` so a test can prove which mock actually answered
/// a given call.
fn mount_upstream(server: &MockServer, tool_name: &str, reply_marker: &str) {
    // A live `/healthz` so `UpstreamPool::health_check_all`'s GET probe keeps
    // this upstream marked healthy (the probe is distinct from the `/mcp`
    // handshake -- see `UpstreamClient::health_probe`). Without this, a probe
    // cycle would 404 and wrongly drop a perfectly-reachable upstream.
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/healthz");
        then.status(200).body("ok");
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
        then.status(200).header("Mcp-Session-Id", format!("session-{reply_marker}")).json_body(init_response());
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "tools/list"}"#);
        then.status(200).json_body(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"tools": [
                {"name": tool_name, "description": "a federated widget tool", "inputSchema": {"type": "object"}}
            ]}
        }));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "tools/call"}"#);
        then.status(200).json_body(json!({
            "jsonrpc": "2.0", "id": 3,
            "result": {"content": [{"type": "text", "text": format!("reply-from-{reply_marker}")}], "isError": false}
        }));
    });
}

fn bearer_upstream_json(name: &str, base_url: &str, namespace: &str, secret_key: &str) -> String {
    format!(
        r#"{{"name":"{name}","url":"{base_url}","transport":"bearer","namespace":"{namespace}","secret_key":"{secret_key}"}}"#
    )
}

fn local_tool(name: &str) -> Value {
    json!({"name": name, "description": "a local core tool", "inputSchema": {"type": "object"}})
}

fn clear_env() {
    std::env::remove_var(TOKEN_A);
    std::env::remove_var(TOKEN_B);
    std::env::remove_var(TOKEN_DOWN);
    std::env::remove_var("DATABASE_URL");
}

// ── the end-to-end flow ─────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn mesh_end_to_end_merge_routing_rbac_health_and_approval() {
    clear_env();
    std::env::set_var(TOKEN_A, "fixture-token-a"); // pii-test-fixture
    std::env::set_var(TOKEN_B, "fixture-token-b"); // pii-test-fixture
    std::env::set_var(TOKEN_DOWN, "fixture-token-down"); // pii-test-fixture

    // ── setup: two in-process mock upstreams, same bare tool name ──────────
    let server_a = MockServer::start();
    let server_b = MockServer::start();
    // Both upstreams export the SAME bare name, "widget_status" -- this is
    // exactly the collision MESH-03 namespacing exists to prevent.
    mount_upstream(&server_a, "widget_status", "upstream-a");
    mount_upstream(&server_b, "widget_status", "upstream-b");

    // A third, unreachable "upstream" for the health-drop assertions below
    // (step 4) -- a closed loopback port, same fixture `src/mesh/client.rs`'s
    // own down-upstream tests use, never a real infra host.
    let registry_json = format!(
        "[{},{},{}]",
        bearer_upstream_json("upstream-a", &server_a.base_url(), "meshaa", TOKEN_A),
        bearer_upstream_json("upstream-b", &server_b.base_url(), "meshbb", TOKEN_B),
        bearer_upstream_json("upstream-down", "http://127.0.0.1:1", "meshdown", TOKEN_DOWN),
    );
    let registry = UpstreamRegistry::from_json(&registry_json).expect("registry JSON is well-formed");
    assert_eq!(registry.len(), 3);
    let pool = UpstreamPool::from_registry(&registry);
    assert_eq!(pool.len(), 3, "all three upstreams should build (valid tokens/config; reachability is a separate, later concern)");

    // ── 1. merged catalog: local unprefixed + both upstreams namespaced ────
    let local_tools = vec![local_tool("health"), local_tool("widget_status")];
    let catalog = MergedCatalog::build(local_tools, &pool).await;

    let names: Vec<&str> =
        catalog.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
    assert_eq!(names.len(), 4, "2 local + 2 namespaced upstream entries, no collision");
    assert!(names.contains(&"health"));
    assert!(names.contains(&"widget_status"), "local bare name stays unprefixed");
    let ns_a = namespaced("meshaa", "widget_status");
    let ns_b = namespaced("meshbb", "widget_status");
    assert_eq!(ns_a, "meshaa__widget_status");
    assert_eq!(ns_b, "meshbb__widget_status");
    assert!(names.contains(&ns_a.as_str()), "upstream A's tool is namespaced under meshaa");
    assert!(names.contains(&ns_b.as_str()), "upstream B's tool is namespaced under meshbb");
    // No collision: the local bare "widget_status" and each upstream's
    // namespaced form are three DISTINCT catalog entries, not one merged one.
    assert_eq!(names.iter().filter(|n| n.contains("widget_status")).count(), 3);

    // ── 2. routing: a namespaced tools/call reaches the RIGHT upstream ─────
    let route_a = resolve_call_route(&ns_a, &pool);
    let (client_a, bare_a) = match route_a {
        CallRoute::Upstream { client, bare_name } => (client, bare_name),
        other => panic!("expected Upstream route for {ns_a}, got {other:?}"),
    };
    assert_eq!(client_a.namespace(), "meshaa");
    assert_eq!(bare_a, "widget_status");
    let result_a = client_a.call_tool(&bare_a, json!({})).await.expect("call to upstream A should succeed");
    assert_eq!(result_a.text, "reply-from-upstream-a", "routed call hit upstream A's mock, not B's");
    assert!(!result_a.is_error);

    let route_b = resolve_call_route(&ns_b, &pool);
    let (client_b, bare_b) = match route_b {
        CallRoute::Upstream { client, bare_name } => (client, bare_name),
        other => panic!("expected Upstream route for {ns_b}, got {other:?}"),
    };
    assert_eq!(client_b.namespace(), "meshbb");
    let result_b = client_b.call_tool(&bare_b, json!({})).await.expect("call to upstream B should succeed");
    assert_eq!(result_b.text, "reply-from-upstream-b", "routed call hit upstream B's mock, not A's");

    // A plain (non-namespaced) name still routes Local, unaffected by the mesh.
    assert!(matches!(resolve_call_route("widget_status", &pool), CallRoute::Local));

    // ── 3. RBAC deny: an unmapped principal gets nothing ────────────────────
    let mut grants = HashMap::new();
    // Only "viewer-a" is granted anything at all, and only upstream A's namespace.
    grants.insert("viewer-a".to_string(), Grant::List(vec![format!("{ns_a}"), "health".to_string()]));
    let policy = AllowlistPolicy::new(grants);

    // A principal identity with NO entry in the policy at all.
    let unmapped_identity = "unmapped-principal";
    assert!(!policy.has_any_entry(unmapped_identity));
    let filtered_for_unmapped = policy.filter_tools(unmapped_identity, catalog.tools.clone());
    assert!(filtered_for_unmapped.is_empty(), "an unmapped principal must see an empty filtered catalog");
    assert!(
        !policy.is_allowed(unmapped_identity, &ns_a),
        "an unmapped principal's tools/call must be denied (default-deny)"
    );
    assert!(!policy.is_allowed(unmapped_identity, &ns_b));

    // Sanity: the mapped principal DOES see/call exactly what it was granted,
    // proving the deny above is a real RBAC decision, not just an empty
    // policy map producing false for everyone.
    let filtered_for_viewer_a = policy.filter_tools("viewer-a", catalog.tools.clone());
    let viewer_a_names: Vec<&str> =
        filtered_for_viewer_a.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
    assert_eq!(viewer_a_names.len(), 2);
    assert!(viewer_a_names.contains(&ns_a.as_str()));
    assert!(viewer_a_names.contains(&"health"));
    assert!(!viewer_a_names.contains(&ns_b.as_str()), "viewer-a was never granted upstream B");
    assert!(policy.is_allowed("viewer-a", &ns_a));
    assert!(!policy.is_allowed("viewer-a", &ns_b), "grant for one namespace must not leak to another");

    // ── 4. health-drop: the unreachable third upstream drops out ───────────
    let ns_down = namespaced("meshdown", "whatever");
    // A freshly-built pool starts optimistically healthy (see
    // `UpstreamPool::from_registry`'s doc) -- force a probe cycle so
    // "meshdown"'s unreachability is actually recorded before asserting on it.
    pool.health_check_all().await;

    let catalog_after_probe = MergedCatalog::build(vec![local_tool("health")], &pool).await;
    let names_after_probe: Vec<&str> =
        catalog_after_probe.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
    assert!(names_after_probe.contains(&"health"));
    assert!(
        !names_after_probe.iter().any(|n| n.starts_with("meshdown__")),
        "the unreachable upstream must drop out of the merged catalog entirely"
    );

    match resolve_call_route(&ns_down, &pool) {
        CallRoute::Unavailable { namespace } => assert_eq!(namespace, "meshdown"),
        other => panic!("expected Unavailable route for the downed upstream, got {other:?}"),
    }

    // Upstreams A and B are completely untouched by the down upstream's
    // outage: both still healthy, both still callable, both still present in
    // a freshly-built catalog alongside "health".
    let catalog_full_after_probe = MergedCatalog::build(vec![local_tool("health")], &pool).await;
    let names_full_after_probe: Vec<&str> = catalog_full_after_probe
        .tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names_full_after_probe.contains(&ns_a.as_str()), "upstream A survives an unrelated upstream's outage");
    assert!(names_full_after_probe.contains(&ns_b.as_str()), "upstream B survives an unrelated upstream's outage");

    let route_a_after = resolve_call_route(&ns_a, &pool);
    let (client_a_after, bare_a_after) = match route_a_after {
        CallRoute::Upstream { client, bare_name } => (client, bare_name),
        other => panic!("upstream A must remain routable after meshdown's outage, got {other:?}"),
    };
    let result_a_after = client_a_after
        .call_tool(&bare_a_after, json!({}))
        .await
        .expect("upstream A must remain callable after meshdown's outage");
    assert_eq!(result_a_after.text, "reply-from-upstream-a");

    // ── 5. approval-gate propagation for a federated guarded tool ──────────
    // "infisical_status" is one of `approval::GUARDED_BARE_NAMES` (see
    // `src/approval.rs`) -- classification must agree for a federated call
    // exactly as it does locally (MESH-09).
    assert!(approval::is_guarded("infisical_status"));
    assert!(!approval::is_guarded(&bare_a), "widget_status is not a guarded tool");

    // Content-binding: the same real args gated against two different mesh
    // upstreams produce DIFFERENT bound content, so a code approved for one
    // upstream's call can never be replayed against the other's (or a local
    // same-named call).
    let real_args = json!({"path": "/some/secret"});
    let gated_for_a = approval::mesh_gate_args(&real_args, "meshaa");
    let gated_for_b = approval::mesh_gate_args(&real_args, "meshbb");
    assert_ne!(gated_for_a, gated_for_b, "approval content must be bound to the target upstream namespace");

    // No live Postgres is available in this hermetic test (DATABASE_URL is
    // deliberately unset) -- following `src/approval.rs`'s own MESH-09 test
    // precedent (`gate_denies_cleanly_when_database_url_unset` and its mesh
    // -args analogue), the gate must deny cleanly rather than panic or hang.
    // This exercises the propagation/classification contract at the
    // content-binding level; a live-DB Granted/Pending/consumed-once
    // redemption is out of scope for this hermetic E2E (no Postgres is
    // spun up here), exactly as MESH-09's own tests scope it.
    std::env::remove_var("DATABASE_URL");
    match approval::gate("infisical_status", &gated_for_a, "federated call \"infisical_status\" on mesh upstream \"meshaa\"").await {
        approval::Gate::Denied(msg) => {
            assert!(
                msg.to_lowercase().contains("unavailable") || msg.contains("DATABASE_URL"),
                "denial message should explain why: {msg}"
            );
        }
        approval::Gate::Granted => {
            panic!("expected a clean Denied with no DATABASE_URL configured, got Granted")
        }
        approval::Gate::Pending(msg) => {
            panic!("expected a clean Denied with no DATABASE_URL configured, got Pending: {msg}")
        }
    }

    clear_env();
}
