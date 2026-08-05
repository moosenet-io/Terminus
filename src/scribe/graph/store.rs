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
use super::project_key::ProjectKey;
use crate::error::ToolError;

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

    /// Path of a project's graph file: `<root>/<canonical_key>.json`.
    ///
    /// Takes a [`ProjectKey`], never a raw string — and it is the ONLY function
    /// in the crate that turns a project identifier into a store path. Since
    /// `ProjectKey` has exactly one constructor ([`ProjectKey::resolve`]) and a
    /// private field, there is no way to address a graph by an
    /// un-canonicalized key. That is what makes TERM #653's duplicate-key state
    /// (`chord.json` stale beside `chrd.json` fresh) unrepresentable rather
    /// than merely corrected: the stale spelling no longer names anything.
    fn path_for(&self, key: &ProjectKey) -> PathBuf {
        self.root.join(format!("{}.json", key))
    }

    /// The canonical key `project_id` resolves to. Callers that need to REPORT
    /// which key answered (so a stale-alias query is visible at the point of
    /// use) should use this rather than re-deriving it.
    pub fn key_for(project_id: &str) -> ProjectKey {
        ProjectKey::resolve(project_id)
    }

    /// When this project's graph file was last written, if it exists.
    ///
    /// Surfaced on every `kg_*` response so a consumer can SEE the graph's age
    /// instead of having to trust it. The whole cost of TERM #653 was that a
    /// stale answer and a fresh answer were indistinguishable to the caller.
    pub fn built_at(&self, project_id: &str) -> Option<std::time::SystemTime> {
        fs::metadata(self.path_for(&ProjectKey::resolve(project_id)))
            .ok()
            .and_then(|m| m.modified().ok())
    }

    /// Every canonical key that currently has a stored graph, sorted.
    ///
    /// Used to answer a key miss with the truth ("these keys exist") instead of
    /// the falsehood TERM #652 was filed for ("no knowledge graph for this
    /// project"). An unreadable store root yields an EMPTY list, never an
    /// error — a diagnostic must not itself fail.
    pub fn stored_keys(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_string)
            })
            .filter(|k| !k.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Load a project's graph, or `None` if it has never been saved. A missing
    /// file is not an error.
    pub fn load(&self, project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
        let path = self.path_for(&ProjectKey::resolve(project_id));
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
        let key = ProjectKey::resolve(project_id);
        let path = self.path_for(&key);
        let json = graph.to_json_pretty()?;
        let tmp = self.root.join(format!(
            "{}.{}.{}.tmp",
            key,
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
        Path::new(&self.path_for(&ProjectKey::resolve(project_id))).exists()
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

    /// The part of a source file BEFORE its `#[cfg(test)]` module.
    ///
    /// Both ratchets below scan production code only. Test code legitimately
    /// mentions the very things they ban — the slugify-equivalence test calls
    /// `slugify` on purpose, and the key-miss test asserts the banned sentence
    /// is ABSENT by naming it. Scanning tests too would make the ratchets fire
    /// on the tests that prove they work.
    fn non_test_source(body: &str) -> &str {
        match body.find("#[cfg(test)]") {
            Some(i) => &body[..i],
            None => body,
        }
    }

    // ── TERM #652 / #653: one canonical key per project ────────────────────

    /// The normalization that names graph files MUST stay byte-identical to the
    /// `slugify` that named the files already on disk. Any divergence silently
    /// orphans the entire live store — every graph would read as "not found"
    /// while sitting right there. This is the one property that cannot be
    /// checked from the standalone harness, so it is pinned here.
    #[test]
    fn normalize_matches_slugify_for_every_key_shape_in_play() {
        use crate::scribe::graph::project_key::normalize;
        use crate::scribe::vault::slugify;
        for input in [
            "TERM", "term", "CHRD", "chrd", "chord", "Chord", "harmony", "harm",
            "lumina", "lum", "terminus", "muse", "rail", "aptr",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", // pii-test-fixture (fabricated uuid shape)
            "My Proj", "  Foo   Bar  ", "S91-scribe-knowledge-infrastructure",
            "../../etc/passwd", "a/b", "..", ".", "Hello, World!",
            "Модуль", "🎉🎊", "",
        ] {
            assert_eq!(
                normalize(input),
                slugify(input),
                "normalize/slugify diverged on {input:?} — this orphans the live store"
            );
        }
    }

    /// SOURCE-LEVEL RATCHET. The duplicate-key bug is only fixed for as long as
    /// `ProjectKey` is the sole way to name a graph. A future edit that goes
    /// back to `slugify(project_id)` inside the KG module would silently
    /// re-open it, so the build fails instead.
    #[test]
    fn kg_module_never_builds_a_store_path_from_a_raw_slug() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scribe/graph");
        let mut offenders = Vec::new();
        for entry in fs::read_dir(&dir).expect("kg module dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().map(|e| e != "rs").unwrap_or(true) {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read kg source");
            for (i, line) in non_test_source(&source).lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("slugify(") {
                    offenders.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "KG store paths must go through ProjectKey::resolve, not slugify — found: {offenders:?}"
        );
    }

    /// SOURCE-LEVEL RATCHET for TERM #652. The sentence "no knowledge graph for
    /// this project" asserts something about the system that is FALSE whenever
    /// the caller merely used the other key space. It must never be emitted
    /// again — only discussed in comments explaining why it was removed.
    #[test]
    fn kg_module_never_claims_a_project_has_no_graph() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scribe/graph");
        let mut offenders = Vec::new();
        for entry in fs::read_dir(&dir).expect("kg module dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().map(|e| e != "rs").unwrap_or(true) {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read kg source");
            for (i, line) in non_test_source(&source).lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("no knowledge graph for this project") {
                    offenders.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a key miss must not be reported as the absence of a graph — found: {offenders:?}"
        );
    }

    /// TERM #653, the whole defect in one test: two spellings of one project
    /// must be ONE stored graph, and the fresh write must be what the stale
    /// spelling reads back. Before this change these were two files with
    /// seven weeks between them.
    #[test]
    fn duplicate_spellings_share_one_stored_graph() {
        let root = tmp_root("dupe");
        let _ = fs::remove_dir_all(&root);
        let store = GraphStore::new(&root);

        // Write under the LONG (historically stale) spelling...
        store.save("chord", &sample("chord")).unwrap();
        // ...and it lands on the canonical short key, not a second file.
        assert!(root.join("chrd.json").exists(), "canonical file written");
        assert!(!root.join("chord.json").exists(), "no second, divergent file");

        // Both spellings — and a mixed-case one — read the SAME graph.
        for spelling in ["chord", "chrd", "Chord", "CHORD"] {
            let g = store.load(spelling).unwrap().expect("graph via {spelling}");
            assert_eq!(g.node_count(), 2, "{spelling} must see the one graph");
            assert!(store.exists(spelling), "{spelling} must report existing");
        }

        // A later write under the SHORT spelling is visible through the LONG
        // one — i.e. the stale side can no longer serve an old answer.
        let mut g2 = sample("chrd");
        g2.insert_node(KgNode::new("crate::c::fresh", NodeKind::Function, "fresh", "src/c.rs"));
        store.save("chrd", &g2).unwrap();
        let via_stale = store.load("chord").unwrap().unwrap();
        assert!(
            via_stale.get_node("crate::c::fresh").is_some(),
            "the alias must return the FRESH graph, never a stale one"
        );

        // Exactly one graph file for this project.
        assert_eq!(store.stored_keys(), vec!["chrd".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    /// A project with no alias is untouched by collapsing — `muse`/`rail`/`aptr`
    /// have real graphs and must not be swept into someone else's key.
    #[test]
    fn a_project_without_an_alias_keeps_its_own_file() {
        let root = tmp_root("noalias");
        let _ = fs::remove_dir_all(&root);
        let store = GraphStore::new(&root);
        store.save("muse", &sample("muse")).unwrap();
        store.save("rail", &sample("rail")).unwrap();
        assert!(root.join("muse.json").exists());
        assert!(root.join("rail.json").exists());
        assert_eq!(store.stored_keys(), vec!["muse".to_string(), "rail".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    /// A UUID resolves to the project's graph once the deployment alias table
    /// maps it — this is TERM #652's headline case.
    #[test]
    #[serial_test::serial]
    fn a_configured_uuid_reads_the_projects_graph() {
        use crate::scribe::graph::project_key::ALIASES_ENV;
        let root = tmp_root("uuid");
        let _ = fs::remove_dir_all(&root);
        let store = GraphStore::new(&root);
        store.save("chrd", &sample("chrd")).unwrap();

        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"; // pii-test-fixture (fabricated uuid shape)
        assert!(store.load(uuid).unwrap().is_none(), "unmapped UUID: honest miss");

        std::env::set_var(ALIASES_ENV, format!("{uuid}=chrd"));
        let g = store.load(uuid).unwrap();
        std::env::remove_var(ALIASES_ENV);
        assert!(g.is_some(), "a mapped UUID must reach the graph");
        assert_eq!(g.unwrap().node_count(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    /// `built_at` is what makes staleness visible at the point of use. It must
    /// be present for a stored graph and absent (not zero, not "now") for one
    /// that does not exist — a fabricated timestamp would be the same class of
    /// lie as the message this change removed.
    #[test]
    fn built_at_is_present_for_a_stored_graph_and_absent_otherwise() {
        let root = tmp_root("builtat");
        let _ = fs::remove_dir_all(&root);
        let store = GraphStore::new(&root);
        assert!(store.built_at("chrd").is_none(), "nothing stored yet");
        store.save("chrd", &sample("chrd")).unwrap();
        assert!(store.built_at("chrd").is_some(), "stored graph has a build time");
        assert!(store.built_at("chord").is_some(), "readable via the alias too");
        assert!(store.built_at("no-such-project").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// `stored_keys` backs the honest key-miss message, so it must degrade to
    /// an empty list rather than erroring when the store root is missing.
    #[test]
    fn stored_keys_on_a_missing_root_is_empty_not_an_error() {
        let root = tmp_root("gone");
        let _ = fs::remove_dir_all(&root);
        assert!(GraphStore::new(&root).stored_keys().is_empty());
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
