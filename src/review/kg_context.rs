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
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;

/// Hard caps so the injected block can never blow up a provider prompt.
const MAX_CHANGED_FILES: usize = 200;
const MAX_SYMBOLS: usize = 40;
const MAX_NEIGHBORS_PER_SYMBOL: usize = 5;
/// Approximate serialized-size cap (bytes); enforced by trimming symbols and
/// setting `"truncated": true` rather than emitting an oversized block.
const MAX_BLOCK_BYTES: usize = 2048;

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
}
