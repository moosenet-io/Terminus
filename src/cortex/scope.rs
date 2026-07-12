//! CXEG-02: `cortex_scope`'s Atlas-backed blast-radius derivation.
//!
//! The tool struct/registration lives in `super` (`src/cortex/mod.rs`); this
//! module holds the pure-ish derivation logic so it's unit-testable against a
//! small fixture graph without going through the `RustTool` trait.
//!
//! ## Reuse (S9 single-source)
//! - Changed-file parsing reuses
//!   `crate::review::kg_context::derive_changed_files_counted` — the SAME
//!   parser `review_run`'s KGREV-01 grounding uses (via its thin
//!   `derive_changed_files` wrapper) — so `cortex_scope` and `review_run`
//!   agree on which files a `diff` touches. [`changed_files_from_args`] only
//!   adapts `cortex_scope`'s own argument shapes (a comma-separated
//!   `changed_files` string, for backward compatibility with the CXEG-01
//!   stub's schema, or an array, or a `diff`) into the
//!   `{"changed_files"|"diff": ...}` shape it already understands, and passes
//!   through its `input_truncated` signal; it does not re-implement any
//!   parsing itself.
//! - The 1-hop caller/callee walk reuses
//!   `crate::scribe::graph::query::one_hop_neighbors` — the SAME single-source
//!   helper `kg_neighbors` (`scribe::graph::tools`) calls — so a node's
//!   incident edges are iterated in exactly one place. Graph loading + touched
//!   -node resolution use the same `scribe::graph::store::GraphStore` +
//!   `KnowledgeGraph` API `crate::review::kg_context::build_kg_block` uses.
//! - Node resolution (touched nodes via `current_nodes()`, neighbors via an
//!   explicit `valid_to.is_none()` filter) uses the CURRENT bi-temporal view
//!   only (KGRAPH-15 is invalidate-don't-delete), so a since-removed symbol
//!   never appears in a live blast radius — matching the other live-view
//!   `kg_*` tools.
//!
//! ## Degrade contract
//! A missing/unloadable graph (store not configured, or no graph saved yet
//! for `project_id`) is NEVER an error here — [`compute_scope`] returns a
//! `"configured": false` response with the literal `changed_files` echoed
//! back as unresolved `blast_radius` entries, so a dispatch caller always
//! gets a usable (if degraded) answer. A missing/invalid `project_id` is
//! validated by the caller (`CortexScope::execute` in `mod.rs`) BEFORE this
//! module is reached, since that is a caller error, not a graph-availability
//! problem.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::review::kg_context::derive_changed_files_counted;
use crate::scribe::graph::model::{KgNode, KnowledgeGraph};
use crate::scribe::graph::query::{one_hop_neighbors, EdgeDirection, NeighborFilter};
use crate::scribe::graph::store::GraphStore;
use crate::scribe::graph::vec_embed::node_card;
use crate::scribe::ScribeConfig;

/// Default cap on the number of nodes enumerated into `blast_radius` before
/// `truncated: true` is set (`CORTEX_MAX_BLAST_NODES`, see `CortexConfig`).
/// Keeps a hundreds-of-files diff from walking (and serializing) an
/// unbounded neighbor set.
pub const DEFAULT_MAX_BLAST_NODES: usize = 200;

/// Adapt `cortex_scope`'s own argument shapes into the `{"changed_files"|
/// "diff": ...}` shape [`derive_changed_files_counted`] understands, then
/// delegate to it -- no duplicate diff-parsing here.
///
/// Accepts, in priority order:
/// 1. `changed_files` as a JSON array of strings (same shape `review_run`'s
///    context uses).
/// 2. `changed_files` as a comma-separated string (the CXEG-01 stub's
///    original schema; kept for backward compatibility with existing
///    callers).
/// 3. `diff`, a unified diff (parsed via `derive_changed_files_counted`'s own
///    `+++ b/<path>` header scan).
///
/// Returns `(files, input_truncated)`. `input_truncated` is `true` when the
/// raw input exceeded `MAX_CHANGED_FILES` and entries were dropped by
/// `derive_changed_files_counted` -- so `cortex_scope` can surface that
/// input-level cap instead of silently swallowing it (distinct from its own
/// blast-radius `max_blast_nodes` cap). Returns an empty vec (never an error)
/// if none of the above are present or everything parses to nothing -- the
/// caller (`CortexScope::execute`) decides whether an empty result is an
/// `InvalidArgument`.
pub fn changed_files_from_args(args: &Value) -> (Vec<String>, bool) {
    if let Some(arr) = args.get("changed_files").and_then(|v| v.as_array()) {
        let ctx = json!({"changed_files": arr.clone()});
        return derive_changed_files_counted(&ctx);
    }
    if let Some(s) = args.get("changed_files").and_then(|v| v.as_str()) {
        let arr: Vec<Value> = s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| Value::String(p.to_string()))
            .collect();
        if !arr.is_empty() {
            let ctx = json!({"changed_files": arr});
            return derive_changed_files_counted(&ctx);
        }
    }
    if let Some(diff) = args.get("diff").and_then(|v| v.as_str()) {
        let ctx = json!({"diff": diff});
        return derive_changed_files_counted(&ctx);
    }
    (Vec::new(), false)
}

/// Compute the pre-dispatch blast-radius response for `project_id` +
/// `changed_files`: touched symbols, their 1-hop callers/callees, affected
/// communities, `blast_count`, and a `token_reduction_pct` estimate.
///
/// `input_truncated` is the signal from [`changed_files_from_args`] that the
/// raw INPUT file list was itself capped at `MAX_CHANGED_FILES` before it
/// reached here — when set, the response carries `truncated: true` (and the
/// caller logs the input-cap drop distinctly), so an oversized input is never
/// silently swallowed, independent of the blast-radius `max_blast_nodes` cap.
///
/// Loads the project's Atlas graph via the same `GraphStore` API `kg_*` uses.
/// A store/graph-load failure or an unbuilt graph degrades to a
/// `"configured": false` response (see module doc) rather than propagating
/// an error.
pub fn compute_scope(project_id: &str, changed_files: &[String], max_blast_nodes: usize, input_truncated: bool) -> Value {
    // Input-file cap warn lives HERE (not in `build_scope_response`) so it
    // fires on EVERY path that drops input — the live-graph path AND the
    // `configured:false` degrade/unavailable path — never a silent drop.
    if input_truncated {
        tracing::warn!(
            "cortex_scope: project '{project_id}' input file list exceeded MAX_CHANGED_FILES; \
             some changed files were dropped before scoping (using {} file(s))",
            changed_files.len(),
        );
    }
    let store = GraphStore::from_config(&ScribeConfig::from_env());
    match store.load(project_id) {
        Ok(Some(graph)) => build_scope_response(project_id, changed_files, &graph, max_blast_nodes, input_truncated),
        Ok(None) | Err(_) => unavailable_response(project_id, changed_files, input_truncated),
    }
}

/// The `"configured": false` degrade response: each changed file echoed back
/// as a literal, unresolved `blast_radius` entry.
fn unavailable_response(project_id: &str, changed_files: &[String], input_truncated: bool) -> Value {
    let blast_radius: Vec<Value> = changed_files
        .iter()
        .map(|f| json!({"id": f, "path": f, "kind": "file", "resolved": false, "role": "touched"}))
        .collect();
    let blast_count = blast_radius.len();
    let mut response = json!({
        "configured": false,
        "project_id": project_id,
        "changed_files": changed_files,
        "blast_radius": blast_radius,
        "affected_communities": [],
        "blast_count": blast_count,
        "token_reduction_pct": 0.0,
    });
    if input_truncated {
        response["truncated"] = json!(true);
    }
    response
}

/// The live-graph path: touched nodes (current nodes whose `path` is in
/// `changed_files`) + their 1-hop callers/callees, capped at
/// `max_blast_nodes`.
fn build_scope_response(project_id: &str, changed_files: &[String], graph: &KnowledgeGraph, max_blast_nodes: usize, input_truncated: bool) -> Value {
    let changed: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();

    let mut touched: Vec<&KgNode> = graph.current_nodes().filter(|n| changed.contains(n.path.as_str())).collect();
    touched.sort_by(|a, b| a.id.cmp(&b.id));

    let matched_paths: HashSet<&str> = touched.iter().map(|n| n.path.as_str()).collect();

    let mut blast_radius: Vec<Value> = Vec::new();
    let mut affected: HashSet<String> = HashSet::new();
    let mut communities: HashSet<u32> = HashSet::new();
    // The node-cap (`max_blast_nodes`) truncation is tracked separately from
    // the input-file cap so each can be warned distinctly; both fold into the
    // single response `truncated` flag.
    let mut node_truncated = false;

    // 1. Touched (resolved) symbols, deterministic id order.
    'touched: for n in &touched {
        if affected.len() >= max_blast_nodes {
            node_truncated = true;
            break 'touched;
        }
        if affected.insert(n.id.clone()) {
            if let Some(c) = n.cluster {
                communities.insert(c);
            }
            blast_radius.push(json!({
                "id": n.id, "path": n.path, "kind": n.kind.as_str(),
                "resolved": true, "role": "touched",
            }));
        }
    }

    // 2. Changed files with no matching graph node -- echoed back as literal,
    // unresolved entries so `blast_radius` always accounts for every input
    // file (e.g. a brand-new file not yet indexed by `scribe_kg_build`).
    if !node_truncated {
        'unresolved: for f in changed_files {
            if matched_paths.contains(f.as_str()) {
                continue;
            }
            if affected.len() >= max_blast_nodes {
                node_truncated = true;
                break 'unresolved;
            }
            if affected.insert(f.clone()) {
                blast_radius.push(json!({"id": f, "path": f, "kind": "file", "resolved": false, "role": "touched"}));
            }
        }
    }

    // 3. 1-hop callers/callees of every touched (resolved) node, via the shared
    // `scribe::graph::query::one_hop_neighbors` walk (single-source — the SAME
    // helper `kg_neighbors` uses, no second edge iteration). Collected across
    // all touched nodes, then deduped in a deterministic (sorted) order. The
    // `bool` is `is_outgoing` (outgoing → callee, incoming → caller), matching
    // the sort/dedup order the previous hand-rolled walk produced: a node
    // reachable as BOTH sorts `false` (caller) before `true` (callee), so it is
    // labeled `caller`.
    if !node_truncated {
        let mut neighbor_ids: Vec<(String, bool)> = Vec::new();
        for n in &touched {
            for nb in one_hop_neighbors(graph, &n.id, NeighborFilter::Both) {
                neighbor_ids.push((nb.id, nb.direction == EdgeDirection::Outgoing));
            }
        }
        neighbor_ids.sort();
        neighbor_ids.dedup();

        'neighbors: for (nid, outgoing) in neighbor_ids {
            if affected.contains(&nid) {
                continue;
            }
            // Resolve to a CURRENT node only (KGRAPH-15 is invalidate-don't-
            // delete: `get_node` also returns bi-temporally invalidated nodes).
            // A neighbor that points at a since-removed symbol must NOT appear
            // in a live blast radius, and must not consume the node budget.
            let Some(node) = graph.get_node(&nid).filter(|n| n.valid_to.is_none()) else {
                continue;
            };
            if affected.len() >= max_blast_nodes {
                node_truncated = true;
                break 'neighbors;
            }
            affected.insert(nid.clone());
            if let Some(c) = node.cluster {
                communities.insert(c);
            }
            blast_radius.push(json!({
                "id": node.id, "path": node.path, "kind": node.kind.as_str(),
                "resolved": true,
                "role": if outgoing { "callee" } else { "caller" },
            }));
        }
    }

    if node_truncated {
        tracing::warn!(
            "cortex_scope: project '{project_id}' blast radius exceeded max_blast_nodes \
             ({max_blast_nodes}) with {} changed file(s); dropping remaining nodes \
             (enumerated {})",
            changed_files.len(),
            affected.len(),
        );
    }
    // NOTE: the input-file-cap warn is emitted in `compute_scope` (fires on the
    // degrade path too); it is intentionally NOT duplicated here.

    let mut affected_communities: Vec<u32> = communities.into_iter().collect();
    affected_communities.sort_unstable();

    let token_reduction_pct = token_reduction_pct(graph, &affected);

    let mut response = json!({
        "configured": true,
        "project_id": project_id,
        "changed_files": changed_files,
        "blast_radius": blast_radius,
        "affected_communities": affected_communities,
        "blast_count": affected.len(),
        "token_reduction_pct": token_reduction_pct,
    });
    if node_truncated || input_truncated {
        response["truncated"] = json!(true);
    }
    response
}

/// Estimate `token_reduction_pct`: `1 - (blast-radius node-card bytes /
/// total-project node-card bytes) * 100`, clamped to `[0, 100]`. Uses the
/// same `node_card` text `scribe_kg_build`'s embedding pipeline embeds
/// (`crate::scribe::graph::vec_embed::node_card`) as the per-node "how many
/// bytes would a model need to read" proxy -- consistent with what
/// `kg_semantic_search`/`kg_stats` already treat as a node's footprint.
///
/// `0.0` whenever there is no resolved (graph-node) blast radius to compare
/// against -- an empty graph, OR `affected_ids` matching zero current nodes
/// (e.g. every changed file was unresolved). A wholly-unresolved blast
/// radius must NOT read as "100% reduction": there is nothing here to know
/// is safe to skip, so no reduction claim is defensible.
fn token_reduction_pct(graph: &KnowledgeGraph, affected_ids: &HashSet<String>) -> f64 {
    let mut total_bytes: usize = 0;
    let mut blast_bytes: usize = 0;
    for n in graph.current_nodes() {
        let card_len = node_card(n, &[], &[]).len();
        total_bytes += card_len;
        if affected_ids.contains(&n.id) {
            blast_bytes += card_len;
        }
    }
    if total_bytes == 0 || blast_bytes == 0 {
        return 0.0;
    }
    let raw_pct = (1.0 - (blast_bytes as f64 / total_bytes as f64)) * 100.0;
    (raw_pct.clamp(0.0, 100.0) * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{Confidence, EdgeKind, KgEdge, NodeKind};
    use std::path::PathBuf;

    fn tmp_store(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("atlas-cortexscope-test-{}-{}", tag, std::process::id()))
    }

    /// `a::foo` calls `a::helper` (same file); `b::Bar` references `a::foo`
    /// (different file, different cluster) -- mirrors `kg_context`'s own
    /// fixture shape so a documented caller/callee shows up across files.
    fn seed_graph(store: &GraphStore, project_id: &str) {
        let mut g = KnowledgeGraph::new(project_id);
        let mut foo = KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs");
        foo.cluster = Some(1);
        let mut helper = KgNode::new("crate::a::helper", NodeKind::Function, "helper", "src/a.rs");
        helper.cluster = Some(1);
        let mut bar = KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs");
        bar.cluster = Some(2);
        g.insert_node(foo);
        g.insert_node(helper);
        g.insert_node(bar);
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::a::helper", EdgeKind::Calls, Confidence::Extracted))
            .unwrap();
        g.insert_edge(KgEdge::new("crate::b::Bar", "crate::a::foo", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        g.recompute_degrees();
        store.save(project_id, &g).unwrap();
    }

    // ── changed_files_from_args ─────────────────────────────────────────

    #[test]
    fn changed_files_from_args_prefers_array() {
        let args = json!({"changed_files": ["src/a.rs", "src/b.rs"], "diff": "ignored"});
        let (files, truncated) = changed_files_from_args(&args);
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"]);
        assert!(!truncated);
    }

    #[test]
    fn changed_files_from_args_splits_csv_string() {
        let args = json!({"changed_files": "src/a.rs, src/b.rs ,,src/c.rs"});
        let (files, truncated) = changed_files_from_args(&args);
        assert_eq!(files, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(!truncated);
    }

    #[test]
    fn changed_files_from_args_falls_back_to_diff() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let args = json!({"diff": diff});
        let (files, truncated) = changed_files_from_args(&args);
        assert_eq!(files, vec!["src/a.rs"]);
        assert!(!truncated);
    }

    #[test]
    fn changed_files_from_args_matches_csv_and_diff_forms() {
        // The explicit-CSV and diff-only forms must agree on the same file
        // set, since both funnel through `derive_changed_files_counted`.
        let csv_args = json!({"changed_files": "src/a.rs"});
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let diff_args = json!({"diff": diff});
        assert_eq!(changed_files_from_args(&csv_args), changed_files_from_args(&diff_args));
    }

    #[test]
    fn changed_files_from_args_empty_when_nothing_present() {
        assert_eq!(changed_files_from_args(&json!({})), (Vec::<String>::new(), false));
        assert_eq!(changed_files_from_args(&json!({"changed_files": ""})), (Vec::<String>::new(), false));
    }

    #[test]
    fn changed_files_from_args_flags_input_truncation_for_oversized_array() {
        // An input array well over MAX_CHANGED_FILES must report the drop so
        // cortex_scope can surface `truncated:true` for the INPUT cap.
        let big: Vec<String> = (0..300).map(|i| format!("src/f{i}.rs")).collect();
        let args = json!({"changed_files": big});
        let (files, truncated) = changed_files_from_args(&args);
        assert!(truncated, "oversized input array must flag input truncation");
        assert!(files.len() < 300, "input must actually be capped");
    }

    // ── compute_scope: graph-unavailable degrade ─────────────────────────

    #[test]
    #[serial_test::serial]
    fn compute_scope_degrades_when_project_has_no_graph() {
        let store_dir = tmp_store("nograph");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let changed = vec!["src/a.rs".to_string(), "src/missing.rs".to_string()];
        let out = compute_scope("NOPE", &changed, DEFAULT_MAX_BLAST_NODES, false);

        assert_eq!(out["configured"], false);
        assert_eq!(out["blast_count"], 2);
        let radius = out["blast_radius"].as_array().unwrap();
        assert_eq!(radius.len(), 2);
        assert!(radius.iter().all(|e| e["resolved"] == false));
        assert_eq!(out["token_reduction_pct"], 0.0);

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // ── compute_scope: live graph ─────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn compute_scope_resolves_touched_symbol_and_documented_neighbors() {
        let store_dir = tmp_store("live");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let out = compute_scope("TERM", &["src/a.rs".to_string()], DEFAULT_MAX_BLAST_NODES, false);
        assert_eq!(out["configured"], true);

        let ids: Vec<&str> = out["blast_radius"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"crate::a::foo"), "{ids:?}");
        assert!(ids.contains(&"crate::a::helper"), "callee neighbor present: {ids:?}");
        assert!(ids.contains(&"crate::b::Bar"), "caller neighbor present: {ids:?}");
        assert_eq!(out["blast_count"], ids.len() as i64);

        let comms = out["affected_communities"].as_array().unwrap();
        assert!(comms.iter().any(|c| c.as_u64() == Some(1)), "{comms:?}");
        assert!(comms.iter().any(|c| c.as_u64() == Some(2)), "{comms:?}");

        let pct = out["token_reduction_pct"].as_f64().unwrap();
        assert!(pct >= 0.0 && pct <= 100.0, "{pct}");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_echoes_unresolved_files_alongside_resolved_symbols() {
        let store_dir = tmp_store("partial");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let changed = vec!["src/a.rs".to_string(), "src/unindexed.rs".to_string()];
        let out = compute_scope("TERM", &changed, DEFAULT_MAX_BLAST_NODES, false);
        assert_eq!(out["configured"], true);

        let radius = out["blast_radius"].as_array().unwrap();
        assert!(radius.iter().any(|e| e["id"] == "src/unindexed.rs" && e["resolved"] == false));
        assert!(radius.iter().any(|e| e["id"] == "crate::a::foo" && e["resolved"] == true));

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_truncates_when_over_max_blast_nodes() {
        let store_dir = tmp_store("trunc");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        // Only 1 touched symbol allowed through before the cap bites, even
        // though the fixture graph has 2 touched nodes in src/a.rs plus a
        // cross-file caller.
        let out = compute_scope("TERM", &["src/a.rs".to_string()], 1, false);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["blast_radius"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_sets_truncated_on_input_cap_distinct_from_node_cap() {
        // Input-file cap path: a healthy node budget (nothing dropped by the
        // blast-radius cap), but `input_truncated=true` propagated from the
        // parse step must still surface `truncated:true`.
        let store_dir = tmp_store("inputcap");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let out = compute_scope("TERM", &["src/a.rs".to_string()], DEFAULT_MAX_BLAST_NODES, true);
        assert_eq!(out["truncated"], true, "input-cap must set truncated even when node cap didn't fire");
        // The node cap did NOT fire: the full fixture blast radius is present.
        assert!(out["blast_radius"].as_array().unwrap().len() >= 3);

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_input_cap_sets_truncated_even_when_graph_unavailable() {
        // The degrade (configured:false) path must also surface an input cap.
        let store_dir = tmp_store("inputcap-nograph");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let out = compute_scope("NOPE", &["src/a.rs".to_string()], DEFAULT_MAX_BLAST_NODES, true);
        assert_eq!(out["configured"], false);
        assert_eq!(out["truncated"], true);

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_no_touched_nodes_all_unresolved_not_truncated() {
        let store_dir = tmp_store("nomatch");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let out = compute_scope("TERM", &["src/unrelated.rs".to_string()], DEFAULT_MAX_BLAST_NODES, false);
        assert_eq!(out["configured"], true);
        assert_eq!(out.get("truncated"), None);
        let radius = out["blast_radius"].as_array().unwrap();
        assert_eq!(radius.len(), 1);
        assert_eq!(radius[0]["resolved"], false);
        assert_eq!(out["token_reduction_pct"], 0.0, "nothing resolved -> no reduction");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn compute_scope_excludes_bitemporally_invalidated_neighbors() {
        // KGRAPH-15 is invalidate-don't-delete: a neighbor pointing at a
        // since-removed symbol (valid_to set) must NOT appear in a live blast
        // radius.
        let store_dir = tmp_store("invalidated");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());

        let mut g = KnowledgeGraph::new("TERM");
        let mut foo = KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs");
        foo.cluster = Some(1);
        let mut bar = KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs");
        bar.cluster = Some(2);
        g.insert_node(foo);
        g.insert_node(bar);
        // Bar references foo, so Bar is a 1-hop CALLER of foo...
        g.insert_edge(KgEdge::new("crate::b::Bar", "crate::a::foo", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        // ...but Bar has since been removed (its file invalidated).
        let seq = g.next_build_seq();
        assert_eq!(g.invalidate_path("src/b.rs", seq), 1);
        g.recompute_degrees();
        store.save("TERM", &g).unwrap();

        let out = compute_scope("TERM", &["src/a.rs".to_string()], DEFAULT_MAX_BLAST_NODES, false);
        let ids: Vec<&str> = out["blast_radius"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"crate::a::foo"), "touched node present: {ids:?}");
        assert!(!ids.contains(&"crate::b::Bar"), "invalidated caller must be excluded: {ids:?}");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // ── token_reduction_pct ────────────────────────────────────────────────

    #[test]
    fn token_reduction_pct_is_zero_for_empty_graph() {
        let g = KnowledgeGraph::new("EMPTY");
        let pct = token_reduction_pct(&g, &HashSet::new());
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn token_reduction_pct_is_high_when_blast_radius_is_small_fraction() {
        let mut g = KnowledgeGraph::new("BIG");
        for i in 0..20 {
            g.insert_node(KgNode::new(format!("crate::m::f{i}"), NodeKind::Function, format!("f{i}"), "src/m.rs"));
        }
        let mut affected = HashSet::new();
        affected.insert("crate::m::f0".to_string());
        let pct = token_reduction_pct(&g, &affected);
        assert!(pct > 80.0, "1-of-20 touched should read as a high reduction: {pct}");
    }
}
