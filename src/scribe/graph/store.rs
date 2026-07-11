//! Atlas per-project graph store (KGRAPH-03).
//!
//! Persists and loads a project's [`KnowledgeGraph`] as one JSON file per
//! `project_id` under a configurable root (`ScribeConfig::kg_store_dir`, from
//! `SCRIBE_KG_STORE_DIR` — a filesystem path, never a secret). Writes are
//! atomic (temp file + rename) so a concurrent reader never sees a partial
//! file. Also supports an incremental refresh that replaces only the subgraph
//! belonging to changed files (Graphify/LightRAG-style patch), leaving the rest
//! of the graph intact.
//!
//! Extraction itself lives in [`super::extract`]; the store orchestrates
//! load → remove changed paths → re-extract those files → merge → save. Precise
//! cross-file edge repair after a partial refresh is deferred to a full rebuild
//! (KGRAPH-10) and the stack-graphs resolver (KGRAPH-11); a partial refresh is a
//! fast approximation, not a full reindex.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::extract::build_rust_graph;
use super::model::KnowledgeGraph;
use crate::error::ToolError;
use crate::scribe::vault::slugify;

/// Process-global sequence so every `save` gets a distinct temp file name even
/// for concurrent writes of the SAME project from multiple threads (pid alone
/// is not enough — see the atomicity note on [`GraphStore::save`]).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A filesystem-backed store of per-project knowledge graphs.
#[derive(Clone, Debug)]
pub struct GraphStore {
    root: PathBuf,
}

impl GraphStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        GraphStore { root: root.into() }
    }

    /// Build a store rooted at the Scribe config's `kg_store_dir`.
    pub fn from_config(cfg: &crate::scribe::ScribeConfig) -> Self {
        GraphStore::new(&cfg.kg_store_dir)
    }

    /// Path of a project's graph file: `<root>/<project_slug>.json`.
    fn path_for(&self, project_id: &str) -> PathBuf {
        self.root.join(format!("{}.json", slugify(project_id)))
    }

    /// Load a project's graph, or `None` if it has never been saved. A missing
    /// file is not an error.
    pub fn load(&self, project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
        let path = self.path_for(project_id);
        match fs::read_to_string(&path) {
            Ok(s) => KnowledgeGraph::from_json(&s).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ToolError::Execution(format!(
                "read graph {}: {e}",
                path.display()
            ))),
        }
    }

    /// Save a project's graph atomically (temp file in the same dir + rename, so
    /// a reader never observes a half-written file). Creates the root dir if
    /// needed. The temp name is unique per write — `pid` + a process-global
    /// sequence — so two concurrent saves of the same project never write to the
    /// same temp file (which would corrupt it); the rename is atomic within a
    /// dir, so whichever save renames last wins cleanly.
    pub fn save(&self, project_id: &str, graph: &KnowledgeGraph) -> Result<(), ToolError> {
        fs::create_dir_all(&self.root).map_err(|e| {
            ToolError::Execution(format!("create kg store dir {}: {e}", self.root.display()))
        })?;
        let path = self.path_for(project_id);
        let json = graph.to_json_pretty()?;
        let tmp = self.root.join(format!(
            "{}.{}.{}.tmp",
            slugify(project_id),
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, json.as_bytes())
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", tmp.display())))?;
        fs::rename(&tmp, &path).map_err(|e| {
            // best-effort cleanup of the temp file on failure
            let _ = fs::remove_file(&tmp);
            ToolError::Execution(format!("rename into {}: {e}", path.display()))
        })?;
        Ok(())
    }

    /// Incrementally refresh only the subgraph for `changed` files
    /// (`(repo_relative_path, source)`): load the current graph (or start
    /// empty), drop every node/edge belonging to a changed path, re-extract just
    /// those files, merge the result, recompute degrees, and save. Returns the
    /// merged graph.
    ///
    /// An empty `changed` list is a no-op that still returns the current graph.
    pub fn refresh_files(
        &self,
        project_id: &str,
        changed: &[(String, String)],
    ) -> Result<KnowledgeGraph, ToolError> {
        let mut graph = self
            .load(project_id)?
            .unwrap_or_else(|| KnowledgeGraph::new(project_id));

        if changed.is_empty() {
            return Ok(graph);
        }

        // Drop the old subgraph for each changed path.
        for (path, _) in changed {
            graph.remove_path(path);
        }

        // Re-extract just the changed files and merge them back in.
        let sub = build_rust_graph(project_id, changed)?;
        for n in sub.nodes() {
            graph.insert_node(n.clone());
        }
        for e in sub.edges() {
            // sub's endpoints were all just inserted above, so this validates.
            let _ = graph.insert_edge(e.clone());
        }
        graph.recompute_degrees();

        self.save(project_id, &graph)?;
        Ok(graph)
    }

    /// Bi-temporal incremental refresh (KGRAPH-15): like [`Self::refresh_files`]
    /// but **invalidate-don't-delete** — a changed file's old nodes are marked
    /// invalidated (kept for history) rather than removed, new elements are
    /// stamped with the build sequence, and the merged graph is saved. The live
    /// working set is unchanged (`current_nodes` / the default views still see
    /// exactly the current graph); a past state is reconstructable via
    /// `KnowledgeGraph::as_of`. Returns the merged graph.
    pub fn refresh_files_temporal(
        &self,
        project_id: &str,
        changed: &[(String, String)],
    ) -> Result<KnowledgeGraph, ToolError> {
        let mut graph = self
            .load(project_id)?
            .unwrap_or_else(|| KnowledgeGraph::new(project_id));
        if changed.is_empty() {
            return Ok(graph);
        }
        let seq = graph.next_build_seq();
        let known_before = graph.node_ids();
        for (path, _) in changed {
            graph.invalidate_path(path, seq);
        }
        let sub = build_rust_graph(project_id, changed)?;
        // Re-insert: a surviving node revives (insert_node keeps its original
        // valid_from and clears the invalidation); a genuinely-new node (id not
        // in known_before) is stamped valid_from = seq below.
        for n in sub.nodes() {
            graph.insert_node(n.clone());
        }
        for e in sub.edges() {
            let _ = graph.insert_edge(e.clone());
        }
        graph.stamp_new_nodes(&known_before, seq);
        graph.recompute_degrees();
        self.save(project_id, &graph)?;
        Ok(graph)
    }

    /// Whether a graph file exists for `project_id` (without loading it).
    pub fn exists(&self, project_id: &str) -> bool {
        Path::new(&self.path_for(project_id)).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn tmp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("atlas-kgstore-test-{}-{}", tag, std::process::id()))
    }

    fn sample(project: &str) -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new(project);
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs"));
        g.insert_node(KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs"));
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        g.recompute_degrees();
        g
    }

    #[test]
    fn save_then_load_round_trips() {
        let root = tmp_root("roundtrip");
        let store = GraphStore::new(&root);
        let g = sample("TERM");
        store.save("TERM", &g).unwrap();
        let loaded = store.load("TERM").unwrap().expect("graph present");
        assert_eq!(loaded, g);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_unknown_project_is_none_not_error() {
        let root = tmp_root("unknown");
        let store = GraphStore::new(&root);
        assert!(store.load("NOPE").unwrap().is_none());
        assert!(!store.exists("NOPE"));
    }

    #[test]
    fn saved_file_is_named_by_project_slug() {
        let root = tmp_root("slug");
        let store = GraphStore::new(&root);
        store.save("My Proj", &KnowledgeGraph::new("My Proj")).unwrap();
        assert!(root.join("my-proj.json").exists(), "slugified filename");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refresh_patches_only_changed_files() {
        let root = tmp_root("refresh");
        let store = GraphStore::new(&root);

        // Initial graph from two files.
        let a1 = ("src/a.rs".to_string(), "pub fn old_a() {}".to_string());
        let b = ("src/b.rs".to_string(), "pub fn keep_b() {}".to_string());
        let g0 = build_rust_graph("TERM", &[a1, b]).unwrap();
        store.save("TERM", &g0).unwrap();
        assert!(store.load("TERM").unwrap().unwrap().get_node("crate::a::old_a").is_some());

        // Change only a.rs — old_a gone, new_a present; b untouched.
        let a2 = ("src/a.rs".to_string(), "pub fn new_a() {}".to_string());
        let merged = store.refresh_files("TERM", &[a2]).unwrap();
        assert!(merged.get_node("crate::a::old_a").is_none(), "old symbol dropped");
        assert!(merged.get_node("crate::a::new_a").is_some(), "new symbol added");
        assert!(merged.get_node("crate::b::keep_b").is_some(), "unchanged file preserved");

        // Persisted, too.
        let reloaded = store.load("TERM").unwrap().unwrap();
        assert!(reloaded.get_node("crate::a::new_a").is_some());
        assert!(reloaded.get_node("crate::b::keep_b").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporal_refresh_keeps_removed_symbol_history() {
        let root = tmp_root("temporal");
        let store = GraphStore::new(&root);
        let v1 = ("src/w.rs".to_string(), "pub fn old_fn() {}\npub fn keep_fn() {}".to_string());
        store.save("TERM", &build_rust_graph("TERM", &[v1]).unwrap()).unwrap();

        // Change: drop old_fn, keep keep_fn, add new_fn.
        let v2 = ("src/w.rs".to_string(), "pub fn keep_fn() {}\npub fn new_fn() {}".to_string());
        let merged = store.refresh_files_temporal("TERM", &[v2]).unwrap();

        let cur: Vec<&str> = merged.current_nodes().map(|n| n.id.as_str()).collect();
        assert!(cur.contains(&"crate::w::keep_fn"), "survivor current");
        assert!(cur.contains(&"crate::w::new_fn"), "new current");
        assert!(!cur.contains(&"crate::w::old_fn"), "removed not current");
        // History kept, not deleted; reconstructable.
        assert!(merged.get_node("crate::w::old_fn").is_some(), "removed symbol retained");
        let (n0, _) = merged.as_of(0);
        assert!(n0.iter().any(|n| n.id == "crate::w::old_fn"), "old_fn present at seq 0");
        assert!(!n0.iter().any(|n| n.id == "crate::w::new_fn"), "new_fn absent at seq 0");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refresh_empty_changeset_is_noop() {
        let root = tmp_root("noop");
        let store = GraphStore::new(&root);
        store.save("TERM", &sample("TERM")).unwrap();
        let g = store.refresh_files("TERM", &[]).unwrap();
        assert_eq!(g.node_count(), 2, "unchanged");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_saves_same_project_do_not_corrupt_and_last_wins() {
        // Regression (review P2): each save uses a unique temp name, so repeated
        // saves of the same project succeed and the final graph is intact.
        let root = tmp_root("repeat");
        let store = GraphStore::new(&root);
        for _ in 0..5 {
            store.save("TERM", &sample("TERM")).unwrap();
        }
        let mut g2 = sample("TERM");
        g2.insert_node(KgNode::new("crate::c::baz", NodeKind::Function, "baz", "src/c.rs"));
        store.save("TERM", &g2).unwrap();
        let loaded = store.load("TERM").unwrap().unwrap();
        assert_eq!(loaded, g2, "last save wins, file not corrupted");
        // no leftover temp files
        let stray = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray, "no temp files left behind");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refresh_on_missing_project_starts_empty() {
        let root = tmp_root("fresh");
        let store = GraphStore::new(&root);
        let g = store
            .refresh_files("NEW", &[("src/x.rs".to_string(), "pub fn x() {}".to_string())])
            .unwrap();
        assert!(g.get_node("crate::x::x").is_some());
        let _ = fs::remove_dir_all(&root);
    }
}
