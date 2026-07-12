//! KGREV-01: ground `review_run` in the Atlas knowledge graph.
//!
//! Best-effort, backward-compatible enrichment: if the review `context` names
//! a `project_id` (and that project has a stored Atlas graph — see
//! `crate::scribe::graph::store::GraphStore`), derive the changed files
//! (either an explicit `context.changed_files` array or parsed from
//! `context.diff`'s unified-diff `+++ b/<path>` headers), find the graph
//! nodes defined in those files, and build a small, BOUNDED "blast radius"
//! block: each touched symbol plus up to a few 1-hop callers/callees and its
//! cluster/community id. That block is injected into `context` under the
//! `"knowledge_graph"` key so `crate::review::prompt::build_prompt`'s existing
//! `serde_json::to_string_pretty(context)` serialization surfaces it in every
//! provider's prompt automatically.
//!
//! No `project_id` -> [`inject`] is a total no-op (context untouched), which
//! is what keeps the common/pre-existing review path byte-for-byte unchanged.
//! Any other failure to ground (no store, no graph, no matching node) is also
//! a silent no-op -- never an error, never a partial/empty block.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::scribe::graph::model::KnowledgeGraph;
use crate::scribe::graph::rules_store::{RuleRow, RulesStore};
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;

/// Hard caps so the injected block can never blow up a provider prompt.
const MAX_CHANGED_FILES: usize = 200;
const MAX_SYMBOLS: usize = 40;
const MAX_NEIGHBORS_PER_SYMBOL: usize = 5;
/// Approximate serialized-size cap (bytes); enforced by trimming symbols and
/// setting `"truncated": true` rather than emitting an oversized block.
const MAX_BLOCK_BYTES: usize = 2048;

/// KGRULE-04: hard caps for the `active_rules` block, mirroring
/// `MAX_SYMBOLS`/`MAX_BLOCK_BYTES` above but sized for rules (no
/// per-file/per-symbol grouping, so a flat cap suffices).
const MAX_RULES: usize = 20;
const MAX_RULES_BYTES: usize = 2048;

/// Derive the repo-relative changed-file list from a review `context`.
///
/// Prefers an explicit `context.changed_files` array of strings. Falls back
/// to parsing unified-diff `+++ b/<path>` headers out of `context.diff`
/// (stripping the `b/` prefix; `/dev/null` — a deleted file — is ignored
/// since there's nothing left in the tree to look up in the graph). Capped at
/// [`MAX_CHANGED_FILES`]. Returns an empty vec (never an error) on anything
/// malformed or absent.
pub fn derive_changed_files(context: &Value) -> Vec<String> {
    if let Some(arr) = context.get("changed_files").and_then(|v| v.as_array()) {
        let mut files: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        files.truncate(MAX_CHANGED_FILES);
        return files;
    }

    let Some(diff) = context.get("diff").and_then(|v| v.as_str()) else {
        return Vec::new();
    };

    let mut files = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("+++ ") else { continue };
        let rest = rest.trim();
        // Unified diff may carry a trailing tab + timestamp; only the path
        // (up to the first whitespace after the `b/...` token) matters.
        let path_token = rest.split('\t').next().unwrap_or(rest).trim();
        if path_token == "/dev/null" {
            continue; // deleted file -- nothing to look up
        }
        let path = path_token.strip_prefix("b/").unwrap_or(path_token);
        if path.is_empty() {
            continue;
        }
        if !files.iter().any(|f: &String| f == path) {
            files.push(path.to_string());
        }
        if files.len() >= MAX_CHANGED_FILES {
            break;
        }
    }
    files
}

/// A minimal neighbor summary: id, edge kind, direction-implied by which list
/// it's placed in (callers = incoming `Calls`-or-any edge, callees = outgoing).
fn neighbor_ids(g: &KnowledgeGraph, node_id: &str, outgoing: bool, limit: usize) -> Vec<String> {
    let mut ids: Vec<String> = g
        .edges()
        .filter(|e| if outgoing { e.from == node_id } else { e.to == node_id })
        .map(|e| if outgoing { e.to.clone() } else { e.from.clone() })
        .collect();
    ids.sort();
    ids.dedup();
    ids.truncate(limit);
    ids
}

/// Build the bounded knowledge-graph "blast radius" block for `changed_files`
/// in `project_id`'s stored Atlas graph, or `None` if there is no store/graph
/// for the project, or the graph has no node defined in any of the changed
/// files (an empty block is never injected).
pub fn build_kg_block(project_id: &str, changed_files: &[String]) -> Option<Value> {
    if project_id.trim().is_empty() || changed_files.is_empty() {
        return None;
    }

    let store = GraphStore::from_config(&ScribeConfig::from_env());
    let graph = match store.load(project_id) {
        Ok(Some(g)) => g,
        Ok(None) | Err(_) => return None,
    };

    let changed: std::collections::HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();

    // Group touched (current) nodes by file, deterministic order (by id).
    let mut touched: Vec<&crate::scribe::graph::model::KgNode> = graph
        .current_nodes()
        .filter(|n| changed.contains(n.path.as_str()))
        .collect();
    touched.sort_by(|a, b| a.id.cmp(&b.id));

    if touched.is_empty() {
        return None;
    }

    let mut truncated = touched.len() > MAX_SYMBOLS;
    touched.truncate(MAX_SYMBOLS);

    let mut by_file: HashMap<&str, Vec<Value>> = HashMap::new();
    for n in &touched {
        let callers = neighbor_ids(&graph, &n.id, false, MAX_NEIGHBORS_PER_SYMBOL);
        let callees = neighbor_ids(&graph, &n.id, true, MAX_NEIGHBORS_PER_SYMBOL);
        by_file.entry(n.path.as_str()).or_default().push(json!({
            "id": n.id,
            "name": n.name,
            "kind": n.kind.as_str(),
            "cluster": n.cluster,
            "callers": callers,
            "callees": callees,
        }));
    }

    let mut files: Vec<&str> = by_file.keys().copied().collect();
    files.sort();
    let files_json: Vec<Value> = files
        .into_iter()
        .map(|f| json!({"path": f, "symbols": by_file.remove(f).unwrap_or_default()}))
        .collect();

    let mut block = json!({
        "project_id": project_id,
        "files": files_json,
    });

    // Enforce the serialized-size cap by progressively dropping the
    // lowest-priority (last, by our deterministic id order) symbols rather
    // than emitting an oversized prompt payload.
    while serde_json::to_string(&block).map(|s| s.len()).unwrap_or(0) > MAX_BLOCK_BYTES {
        let Some(files_arr) = block.get_mut("files").and_then(|v| v.as_array_mut()) else { break };
        let Some(last_file) = files_arr.last_mut() else { break };
        let Some(symbols) = last_file.get_mut("symbols").and_then(|v| v.as_array_mut()) else { break };
        if symbols.pop().is_none() {
            files_arr.pop();
            if files_arr.is_empty() {
                break;
            }
        }
        truncated = true;
        if files_arr.is_empty() {
            break;
        }
    }

    // If everything got trimmed away by the size cap, don't inject an empty
    // block.
    let has_any_symbol = block["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .any(|f| f["symbols"].as_array().map(|s| !s.is_empty()).unwrap_or(false))
        })
        .unwrap_or(false);
    if !has_any_symbol {
        return None;
    }

    if truncated {
        block["truncated"] = json!(true);
    }

    Some(block)
}

/// Inject a `"knowledge_graph"` block into `context` in place, if and only if
/// `context.project_id` is present AND a matching, non-empty blast-radius
/// block can be built. No `project_id` -> total no-op. Any other miss (no
/// store, no graph, no matching node) -> also a no-op, never an error.
pub fn inject(context: &mut Value) {
    let Some(project_id) = context.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return;
    };
    let changed_files = derive_changed_files(context);
    if let Some(block) = build_kg_block(&project_id, &changed_files) {
        if let Value::Object(map) = context {
            map.insert("knowledge_graph".to_string(), block);
        }
    }
}

/// Sort priority mirroring `rules_store::Enforcement`'s own ordering
/// (`Advisory < LintCandidate < Blocking`), duplicated here as a plain
/// string match so `select_rules_for_context` stays free of any DB/async
/// dependency and is trivially unit-testable.
fn enforcement_priority(enforcement: &str) -> i32 {
    match enforcement {
        "blocking" => 2,
        "lint-candidate" => 1,
        _ => 0,
    }
}

/// KGRULE-04: pure selection + bounding of active rules for a review context.
///
/// `rules` is the full set of active rules already loaded for the project
/// (unordered — this function does its own ordering, so callers don't need
/// to rely on the store's own `ORDER BY`). A rule applies if it is
/// `scope_kind == "global"`, or its `scope_ref` is present in
/// `changed_files` (the caller may pass both changed file paths AND touched
/// symbol ids in this slice, so both `path`- and `node`-scoped rules can
/// match). Applicable rules are ordered by enforcement (`blocking` >
/// `lint-candidate` > `advisory`) then most-recently-created first, then
/// truncated to `cap`. No I/O — fully unit-testable without a DB.
pub fn select_rules_for_context(rules: Vec<RuleRow>, changed_files: &[String], cap: usize) -> Vec<Value> {
    let changed: std::collections::HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();

    let mut applicable: Vec<RuleRow> = rules
        .into_iter()
        .filter(|r| r.scope_kind == "global" || changed.contains(r.scope_ref.as_str()))
        .collect();

    applicable.sort_by(|a, b| {
        enforcement_priority(&b.enforcement)
            .cmp(&enforcement_priority(&a.enforcement))
            .then(b.created_at.cmp(&a.created_at))
    });

    applicable
        .into_iter()
        .take(cap)
        .map(|r| {
            json!({
                "scope_kind": r.scope_kind,
                "scope_ref": r.scope_ref,
                "category": r.category,
                "guidance": r.guidance,
                "enforcement": r.enforcement,
            })
        })
        .collect()
}

/// KGRULE-04: inject a bounded `"active_rules"` array into `context`,
/// closing the loop between rule crystallization/promotion (KGRULE-01..03)
/// and enforcement — the same rules the system has learned are now surfaced
/// to every reviewer.
///
/// Best-effort and entirely additive:
/// - No `project_id` in `context` -> no-op.
/// - `RulesStore::from_env()` returns `NotConfigured` (no `ATLAS_DATABASE_URL`)
///   -> no-op. This is what keeps a rules-store-unconfigured review's context
///   byte-for-byte unchanged (backward compatible with pre-KGRULE-04
///   behavior).
/// - Any other store/lookup error -> also a no-op (never surfaces an error,
///   never panics, never blocks/delays the review).
/// - No applicable active rules for the changed files/symbols/global scope
///   -> no-op (an empty `active_rules` array is never injected, mirroring
///   `inject`'s own "no empty block" contract for `knowledge_graph`).
///
/// Must be called from an async context (this awaits `RulesStore`'s sqlx
/// calls) — see `review::execute()`, which calls this right after the sync
/// `inject()` KGREV-01 grounding step.
pub async fn inject_active_rules(context: &mut Value) {
    let Some(project_id) = context.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return;
    };

    let store = match RulesStore::from_env().await {
        Ok(store) => store,
        Err(_) => return, // NotConfigured or any other from_env error -> no-op
    };

    let changed_files = derive_changed_files(context);

    // Include touched symbol ids alongside the changed file paths so
    // node-scoped rules for symbols defined in the changed files are
    // considered too (mirrors `inject`'s own "blast radius" grounding) --
    // best-effort: no graph/no store for the project just means no node-scope
    // ids get added, never an error.
    let mut scope_refs = changed_files.clone();
    if !changed_files.is_empty() {
        let kg_store = GraphStore::from_config(&ScribeConfig::from_env());
        if let Ok(Some(graph)) = kg_store.load(&project_id) {
            let changed: std::collections::HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
            for n in graph.current_nodes().filter(|n| changed.contains(n.path.as_str())) {
                scope_refs.push(n.id.clone());
            }
        }
    }

    let rules = match store.list_active(&project_id, None, None, None).await {
        Ok(rules) => rules,
        Err(_) => return, // best-effort -- a lookup failure leaves context unchanged
    };

    let selected = select_rules_for_context(rules, &scope_refs, MAX_RULES);
    if selected.is_empty() {
        return;
    }

    let mut block = json!(selected);
    // Enforce the serialized-size cap by progressively dropping the
    // lowest-priority (trailing, per our enforcement/recency sort) entries
    // rather than emitting an oversized prompt payload -- mirrors
    // `build_kg_block`'s own size-cap loop.
    while serde_json::to_string(&block).map(|s| s.len()).unwrap_or(0) > MAX_RULES_BYTES {
        let Some(arr) = block.as_array_mut() else { break };
        if arr.pop().is_none() || arr.is_empty() {
            break;
        }
    }

    if block.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return; // everything got trimmed away -- don't inject an empty array
    }

    if let Value::Object(map) = context {
        map.insert("active_rules".to_string(), block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_store(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("atlas-kgcontext-test-{}-{}", tag, std::process::id()))
    }

    fn seed_graph(store: &GraphStore, project_id: &str) {
        let mut g = KnowledgeGraph::new(project_id);
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs"));
        g.insert_node(KgNode::new("crate::a::helper", NodeKind::Function, "helper", "src/a.rs"));
        g.insert_node(KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs"));
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::a::helper", EdgeKind::Calls, Confidence::Extracted))
            .unwrap();
        g.insert_edge(KgEdge::new("crate::b::Bar", "crate::a::foo", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        g.recompute_degrees();
        store.save(project_id, &g).unwrap();
    }

    #[test]
    fn derive_changed_files_prefers_explicit_array() {
        let ctx = json!({"changed_files": ["src/a.rs", "src/b.rs", ""], "diff": "ignored"});
        assert_eq!(derive_changed_files(&ctx), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn derive_changed_files_parses_unified_diff_headers() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n\
diff --git a/deleted.rs b/deleted.rs\n\
--- a/deleted.rs\n\
+++ /dev/null\n";
        let ctx = json!({"diff": diff});
        assert_eq!(derive_changed_files(&ctx), vec!["src/a.rs"]);
    }

    #[test]
    fn derive_changed_files_empty_when_nothing_present() {
        assert_eq!(derive_changed_files(&json!({})), Vec::<String>::new());
        assert_eq!(derive_changed_files(&json!({"diff": "not a diff"})), Vec::<String>::new());
    }

    #[test]
    #[serial_test::serial]
    fn build_kg_block_names_touched_symbol_and_neighbor() {
        let store_dir = tmp_store("block");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let block = build_kg_block("TERM", &["src/a.rs".to_string()]).expect("block present");
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains("crate::a::foo"), "{s}");
        assert!(s.contains("crate::a::helper"), "callee neighbor present: {s}");
        assert!(s.contains("crate::b::Bar"), "caller neighbor present: {s}");

        let _ = fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn inject_adds_knowledge_graph_key_when_project_and_graph_present() {
        let store_dir = tmp_store("inject");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let mut ctx = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        inject(&mut ctx);
        assert!(ctx.get("knowledge_graph").is_some(), "{ctx}");

        let _ = fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    fn inject_is_noop_without_project_id() {
        let mut ctx = json!({"diff": "+ fn x() {}"});
        let before = ctx.clone();
        inject(&mut ctx);
        assert_eq!(ctx, before, "no project_id -> context untouched, byte-for-byte");
    }

    #[test]
    #[serial_test::serial]
    fn inject_is_noop_when_graph_missing() {
        let store_dir = tmp_store("missing");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let mut ctx = json!({"project_id": "NOPE", "changed_files": ["src/a.rs"]});
        let before = ctx.clone();
        inject(&mut ctx);
        assert_eq!(ctx, before, "missing graph -> no-op, no error");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn build_kg_block_none_when_no_node_matches_changed_files() {
        let store_dir = tmp_store("nomatch");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        seed_graph(&store, "TERM");

        let block = build_kg_block("TERM", &["src/unrelated.rs".to_string()]);
        assert!(block.is_none());

        let _ = fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // ── KGRULE-04: select_rules_for_context (pure) ─────────────────────

    fn rule(scope_kind: &str, scope_ref: &str, category: &str, enforcement: &str, created_offset_secs: i64) -> RuleRow {
        let now = chrono::Utc::now();
        RuleRow {
            id: uuid::Uuid::new_v4(),
            project_id: "TERM".to_string(),
            scope_kind: scope_kind.to_string(),
            scope_ref: scope_ref.to_string(),
            category: category.to_string(),
            guidance: format!("guidance for {scope_ref}"),
            enforcement: enforcement.to_string(),
            status: "active".to_string(),
            provenance: json!({}),
            recurrence_at_creation: Some(3),
            cortex_risk: None,
            created_at: now + chrono::Duration::seconds(created_offset_secs),
            valid_from: now,
            valid_to: None,
        }
    }

    #[test]
    fn select_rules_includes_matching_path_and_global_excludes_unrelated() {
        let rules = vec![
            rule("path", "src/a.rs", "style", "advisory", 0),
            rule("path", "src/unrelated.rs", "style", "advisory", 0),
            rule("global", "*", "safety", "advisory", 0),
        ];
        let selected = select_rules_for_context(rules, &["src/a.rs".to_string()], 20);
        let scope_refs: Vec<&str> = selected.iter().map(|v| v["scope_ref"].as_str().unwrap()).collect();
        assert!(scope_refs.contains(&"src/a.rs"));
        assert!(scope_refs.contains(&"*"), "global-scope rule always applies");
        assert!(!scope_refs.contains(&"src/unrelated.rs"));
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_rules_includes_node_scope_when_scope_ref_in_changed_set() {
        // Callers pass touched symbol ids alongside file paths in the same
        // slice; a node-scoped rule matches by scope_ref membership just like
        // a path-scoped one.
        let rules = vec![rule("node", "crate::a::foo", "correctness", "advisory", 0)];
        let selected = select_rules_for_context(rules, &["src/a.rs".to_string(), "crate::a::foo".to_string()], 20);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0]["scope_ref"], "crate::a::foo");
    }

    #[test]
    fn select_rules_orders_by_enforcement_then_recency() {
        let rules = vec![
            rule("global", "*", "a", "advisory", -100),
            rule("global", "*", "b", "blocking", -50),
            rule("global", "*", "c", "lint-candidate", 0),
            rule("global", "*", "d", "advisory", -10),
        ];
        let selected = select_rules_for_context(rules, &[], 20);
        let cats: Vec<&str> = selected.iter().map(|v| v["category"].as_str().unwrap()).collect();
        // blocking first, then lint-candidate, then advisory-by-recency (d
        // newer than a).
        assert_eq!(cats, vec!["b", "c", "d", "a"]);
    }

    #[test]
    fn select_rules_truncates_to_cap() {
        let rules: Vec<RuleRow> = (0..30).map(|i| rule("global", "*", &format!("cat{i}"), "advisory", i)).collect();
        let selected = select_rules_for_context(rules, &[], 5);
        assert_eq!(selected.len(), 5);
    }

    #[test]
    fn select_rules_empty_when_nothing_applies() {
        let rules = vec![rule("path", "src/unrelated.rs", "style", "advisory", 0)];
        let selected = select_rules_for_context(rules, &["src/a.rs".to_string()], 20);
        assert!(selected.is_empty());
    }

    // ── KGRULE-04: inject_active_rules (async, degrade + backward compat) ─

    #[tokio::test]
    #[serial_test::serial]
    async fn inject_active_rules_unconfigured_store_leaves_context_unchanged() {
        // Mirrors the sibling kg_* tool tests' own shape: never mutate global
        // env to force NotConfigured if a real DSN is already present in this
        // process.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let mut ctx = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let before = ctx.clone();
        inject_active_rules(&mut ctx).await;
        assert_eq!(ctx, before, "unconfigured rules store -> context unchanged, byte-for-byte");
        assert!(ctx.get("active_rules").is_none());
    }

    #[tokio::test]
    async fn inject_active_rules_is_noop_without_project_id() {
        let mut ctx = json!({"changed_files": ["src/a.rs"]});
        let before = ctx.clone();
        inject_active_rules(&mut ctx).await;
        assert_eq!(ctx, before);
        assert!(ctx.get("active_rules").is_none());
    }
}
