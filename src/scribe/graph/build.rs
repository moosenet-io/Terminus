//! Atlas build orchestration (KGRAPH-10): the tools that actually produce a
//! project's knowledge graph end-to-end.
//!
//! `scribe_kg_build` reads a repo (confined to Scribe's allowlisted roots),
//! runs the full pipeline — extract → cluster → layout → render — stores the
//! graph JSON, and writes the three visual artifacts (`<slug>.svg` /
//! `.graphml` / `.html`) next to it. `incremental=true` with `changed_files`
//! patches only those files (KGRAPH-03 refresh). `scribe_kg_status` reports a
//! project's graph size and freshness. Both register on the core registry.
//!
//! This is what the build pipeline's docs stage (and the companion HARM
//! Stage-7c hook) calls. File reads use typed `std::fs` — never a subprocess.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::model::{EdgeKind, KnowledgeGraph};
use super::store::GraphStore;
use super::vec_embed::{node_card, EmbedClient};
use super::vec_store::{card_hash, AtlasVecStore};
use super::{build_rust_graph, cluster, layout, pagerank, render, semantic};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::vault::slugify;
use crate::scribe::ScribeConfig;
use crate::tool::{RustTool, ToolOutput};

/// Skip these directory names when walking a repo for `.rs` files.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".worktrees", "worktrees"];
/// Bound the walk so a pathological tree can't blow up memory (visible in the
/// result, never silent).
const MAX_FILES: usize = 8000;
const MAX_FILE_BYTES: u64 = 1_000_000;

/// Reject a `changed_files` entry that could escape the repo root: it must be
/// repo-relative with no `..` / root / prefix components. Without this the
/// incremental read path would bypass the allowlist that only guards
/// `repo_path` (a `../../x.rs` would be joined and read outside the root).
fn ensure_safe_rel(rel: &str) -> Result<(), ToolError> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(ToolError::InvalidArgument(format!(
            "changed_files entry must be repo-relative, got absolute path '{rel}'"
        )));
    }
    for c in p.components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err(ToolError::InvalidArgument(format!(
                "changed_files entry must not escape the repo (no '..'): '{rel}'"
            )));
        }
    }
    Ok(())
}

fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required and must be a non-empty string")))
}

/// Recursively collect `(repo_relative_path, source)` for every `.rs` file
/// under `root`, skipping build/vcs dirs and oversized files. Returns whether
/// the file cap was hit.
/// `pub(crate)` (not just module-private) so `cortex::audit`'s CXEG-11
/// external-repo audit can reuse the exact same allowlisted-language file
/// walk (skip dirs, symlink-escape guard, file-count/size caps) instead of a
/// second implementation — the walk itself has no opinion on WHERE `root`
/// came from; `scribe_kg_build`'s caller confines `root` via
/// `is_repo_path_allowed`, `cortex_audit`'s confines it by construction (an
/// isolated scratch dir it just cloned into, never operator input).
pub(crate) fn walk_rs(root: &Path) -> Result<(Vec<(String, String)>, bool), ToolError> {
    let mut out = Vec::new();
    let mut capped = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        // deterministic order
        let mut items: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        items.sort();
        for path in items {
            if out.len() >= MAX_FILES {
                capped = true;
                break;
            }
            // Do NOT follow symlinks — a symlink inside the repo could point at a
            // dir/file outside the allowlist (symlink escape). symlink_metadata
            // reports the link itself, not its target.
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                stack.push(path);
            } else if meta.is_file()
                && path
                    .to_str()
                    .and_then(super::extract::Lang::from_path)
                    .is_some()
            {
                // Any supported language (KGRAPH-17), not just Rust.
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&path) else { continue };
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                out.push((rel, content));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((out, capped))
}

/// Write the three visual artifacts next to the graph JSON. Best-effort per
/// file; returns the list actually written.
fn write_artifacts(dir: &str, slug: &str, svg: &str, graphml: &str, html: &str) -> Result<Vec<String>, ToolError> {
    fs::create_dir_all(dir)
        .map_err(|e| ToolError::Execution(format!("create kg store dir {dir}: {e}")))?;
    let mut written = Vec::new();
    for (ext, body) in [("svg", svg), ("graphml", graphml), ("html", html)] {
        let p = Path::new(dir).join(format!("{slug}.{ext}"));
        fs::write(&p, body).map_err(|e| ToolError::Execution(format!("write {}: {e}", p.display())))?;
        written.push(format!("{slug}.{ext}"));
    }
    Ok(written)
}

fn now_rfc3339_secs() -> String {
    // A coarse build stamp (unix seconds) — the model treats generated_at as
    // opaque; this avoids a chrono dependency.
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("unixsecs:{secs}")
}

// ── KGEMB-03: gated, best-effort node embedding ─────────────────────────────

/// `calls`-edge adjacency: node id -> (caller names, callee names), built once
/// per build so `embed_graph_nodes` doesn't rescan every edge per candidate
/// node. Mirrors `tools.rs::adjacency` but scoped to `Calls` edges only and
/// keyed by neighbor *name* (what `node_card` embeds), not id.
fn calls_adjacency(graph: &KnowledgeGraph) -> HashMap<String, (Vec<String>, Vec<String>)> {
    let mut adj: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for e in graph.edges() {
        if e.kind != EdgeKind::Calls {
            continue;
        }
        if let (Some(from_node), Some(to_node)) = (graph.get_node(&e.from), graph.get_node(&e.to)) {
            adj.entry(from_node.id.clone()).or_default().1.push(to_node.name.clone());
            adj.entry(to_node.id.clone()).or_default().0.push(from_node.name.clone());
        }
    }
    adj
}

/// Which current node ids are candidates for a (re-)embed this build.
/// `None` (full build) -> every current node; `Some(changed)` (incremental)
/// -> only current nodes whose id is in the changed-node scope. Pure --
/// unit-testable without a graph/DB/HTTP.
fn embed_candidate_ids(current_node_ids: &BTreeSet<String>, changed_node_ids: Option<&HashSet<String>>) -> Vec<String> {
    match changed_node_ids {
        None => current_node_ids.iter().cloned().collect(),
        Some(changed) => current_node_ids.iter().filter(|id| changed.contains(id.as_str())).cloned().collect(),
    }
}

/// Which previously-stored vector rows should be deleted this build: ids that
/// are no longer a current node, scoped to what this build actually looked
/// at. Full build -> any stored id absent from the current graph. Incremental
/// build -> ids in the changed-node scope (which the caller seeds from BOTH
/// the pre-refresh and post-refresh node ids for the changed files, so a
/// removed symbol/file is captured even though it no longer appears in the
/// current graph) that are absent from the current graph. Pure.
fn embed_delete_ids(
    current_node_ids: &BTreeSet<String>,
    changed_node_ids: Option<&HashSet<String>>,
    existing_hashes: &HashMap<String, String>,
) -> Vec<String> {
    match changed_node_ids {
        None => existing_hashes
            .keys()
            .filter(|id| !current_node_ids.contains(id.as_str()))
            .cloned()
            .collect(),
        Some(changed) => changed
            .iter()
            .filter(|id| !current_node_ids.contains(id.as_str()))
            .cloned()
            .collect(),
    }
}

/// Split `candidates` into (needs a fresh embed, unchanged card -> skip) by
/// comparing each candidate's freshly-computed card hash against the store's
/// last-known hash for that id. A candidate with no prior hash (new node) is
/// always embedded. Pure.
fn skip_unchanged_cards(
    candidates: &[String],
    new_hashes: &HashMap<String, String>,
    existing_hashes: &HashMap<String, String>,
) -> (Vec<String>, Vec<String>) {
    let mut to_embed = Vec::new();
    let mut to_skip = Vec::new();
    for id in candidates {
        match (new_hashes.get(id), existing_hashes.get(id)) {
            (Some(new_hash), Some(old_hash)) if new_hash == old_hash => to_skip.push(id.clone()),
            _ => to_embed.push(id.clone()),
        }
    }
    (to_embed, to_skip)
}

/// The actual store/client work, isolated from `embed_graph_nodes`'s
/// gating/error-catching so the "any failure here becomes a logged, reported,
/// non-fatal result" contract lives in exactly one place (the caller).
async fn embed_graph_nodes_inner(
    store: &AtlasVecStore,
    client: &EmbedClient,
    project_id: &str,
    graph: &KnowledgeGraph,
    changed_node_ids: Option<&HashSet<String>>,
) -> Result<(usize, usize, usize), ToolError> {
    let current_node_ids: BTreeSet<String> = graph.nodes().map(|n| n.id.clone()).collect();
    let existing_hashes = store.existing_hashes(project_id).await?;

    let candidates = embed_candidate_ids(&current_node_ids, changed_node_ids);
    let deletes = embed_delete_ids(&current_node_ids, changed_node_ids, &existing_hashes);

    let adjacency = calls_adjacency(graph);
    let mut cards: HashMap<String, String> = HashMap::with_capacity(candidates.len());
    let mut new_hashes: HashMap<String, String> = HashMap::with_capacity(candidates.len());
    for id in &candidates {
        // Every candidate id came from current_node_ids, which was built
        // directly from graph.nodes() above, so this lookup cannot miss.
        let Some(node) = graph.get_node(id) else { continue };
        let (callers, callees) = adjacency.get(id).cloned().unwrap_or_default();
        let caller_refs: Vec<&str> = callers.iter().map(String::as_str).collect();
        let callee_refs: Vec<&str> = callees.iter().map(String::as_str).collect();
        let card = node_card(node, &caller_refs, &callee_refs);
        new_hashes.insert(id.clone(), card_hash(&card));
        cards.insert(id.clone(), card);
    }

    let (to_embed, to_skip) = skip_unchanged_cards(&candidates, &new_hashes, &existing_hashes);

    if !to_embed.is_empty() {
        let texts: Vec<String> = to_embed.iter().map(|id| cards[id].clone()).collect();
        let vectors = client.embed_batch(&texts).await?;
        if vectors.len() != to_embed.len() {
            return Err(ToolError::Execution(format!(
                "embeddings: expected {} vectors for {} inputs, got {}",
                to_embed.len(),
                to_embed.len(),
                vectors.len()
            )));
        }
        let model = crate::config::embeddings_model();
        let rows: Vec<(String, String, String, Vec<f32>)> = to_embed
            .iter()
            .zip(vectors)
            .map(|(id, vector)| (id.clone(), new_hashes[id].clone(), model.clone(), vector))
            .collect();
        store.upsert(project_id, &rows).await?;
    }

    if !deletes.is_empty() {
        store.delete(project_id, &deletes).await?;
    }

    Ok((to_embed.len(), to_skip.len(), deletes.len()))
}

/// KGEMB-03: gated, best-effort node-embedding step. Runs after `pagerank`
/// and does NOT mutate `graph` -- it only reads it. Strictly non-blocking:
/// disabled, unconfigured, or failed, this always returns a stats `Value` and
/// never an `Err` the caller has to propagate (mirrors `review::maybe_rebuild`'s
/// contract for the exact same reason -- an embedding/store/HTTP hiccup must
/// never fail a `scribe_kg_build` call or change the graph it saves).
async fn embed_graph_nodes(
    cfg: &ScribeConfig,
    project_id: &str,
    graph: &KnowledgeGraph,
    changed_node_ids: Option<&HashSet<String>>,
) -> Value {
    if !cfg.embed_enabled {
        return json!({"ran": false, "reason": "SCRIBE_KG_EMBED not set"});
    }

    let store = match AtlasVecStore::from_env().await {
        Ok(s) => s,
        Err(e) => {
            // Log AND report: store construction/config failure should be
            // visible in the daemon log, not only in the returned JSON.
            tracing::warn!("KGEMB-03: vector store not configured for project '{project_id}': {e}");
            return json!({"ran": false, "reason": format!("vector store not configured: {e}")});
        }
    };
    let client = EmbedClient::from_env();

    // Pre-flight the embeddings endpoint. `EmbedClient::from_env()` is infallible
    // (it always resolves to a default URL), so the only meaningful "client
    // configured" gate is a reachability probe: if the endpoint can't embed a
    // trivial string, embeddings are effectively NOT configured for this build —
    // report `ran:false` and skip the whole card/candidate pass rather than
    // doing all that work only to fail at `embed_batch`.
    if let Err(e) = client.embed("kgemb-03 embeddings preflight probe").await {
        tracing::warn!("KGEMB-03: embeddings endpoint not usable for project '{project_id}': {e}");
        return json!({"ran": false, "reason": format!("embeddings endpoint not usable: {e}")});
    }

    match embed_graph_nodes_inner(&store, &client, project_id, graph, changed_node_ids).await {
        Ok((embedded, skipped, deleted)) => json!({
            "ran": true, "ok": true,
            "embedded": embedded, "skipped": skipped, "deleted": deleted,
        }),
        Err(e) => {
            tracing::warn!("KGEMB-03: embed step failed for project '{project_id}': {e}");
            json!({"ran": true, "ok": false, "error": e.to_string()})
        }
    }
}

// ── scribe_kg_build ────────────────────────────────────────────────────────
pub struct ScribeKgBuild;

#[async_trait]
impl RustTool for ScribeKgBuild {
    fn name(&self) -> &str {
        "scribe_kg_build"
    }
    fn description(&self) -> &str {
        "Build (or incrementally refresh) a project's Atlas knowledge graph from a repo: extract → \
cluster → layout → render, storing the graph JSON plus map.svg / graph.graphml / graph.html. \
repo_path must be under SCRIBE_ALLOWED_REPO_ROOTS. Set incremental=true with changed_files to patch \
only those files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "repo_path": {"type": "string", "description": "absolute path to the repo (defaults to SCRIBE_REPO_PATH)"},
                "incremental": {"type": "boolean", "description": "patch only changed_files instead of a full rebuild"},
                "changed_files": {"type": "array", "items": {"type": "string"}, "description": "repo-relative paths (incremental mode)"}
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let cfg = ScribeConfig::from_env();
        let project_id = req_str(&args, "project_id")?;
        let repo_path = args
            .get("repo_path")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| cfg.repo_path.clone())
            .ok_or_else(|| ToolError::InvalidArgument("repo_path is required (or set SCRIBE_REPO_PATH)".into()))?;

        // Default-deny confinement — reuse Scribe's own guard.
        if !crate::scribe::is_repo_path_allowed(Path::new(&repo_path), &cfg.allowed_repo_roots) {
            return Err(ToolError::InvalidArgument(format!(
                "repo_path '{repo_path}' is not under any root in SCRIBE_ALLOWED_REPO_ROOTS (default-deny)"
            )));
        }

        let incremental = args.get("incremental").and_then(|v| v.as_bool()).unwrap_or(false);
        let changed: Vec<String> = args
            .get("changed_files")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        let store = GraphStore::from_config(&cfg);
        let root = Path::new(&repo_path);
        let mut capped = false;

        // KGEMB-03: snapshot the OLD graph's node ids for the changed paths
        // BEFORE refresh_files mutates/overwrites the stored graph below --
        // refresh_files both drops the old subgraph for each changed path and
        // re-merges the freshly extracted one in a single call, so a node id
        // that vanished (a symbol or whole file removed) leaves no other
        // trace once it returns. Capturing it here is what lets the embed
        // step below delete that stale vector-store row instead of leaving
        // an orphaned embedding forever. Only bothered with when the embed
        // step can actually run (cfg.embed_enabled) -- an extra graph load on
        // every incremental build otherwise would be pure overhead.
        let mut old_node_ids_for_changed: HashSet<String> = HashSet::new();
        if cfg.embed_enabled && incremental && !changed.is_empty() {
            if let Ok(Some(old_graph)) = store.load(&project_id) {
                old_node_ids_for_changed = old_graph
                    .nodes()
                    .filter(|n| changed.iter().any(|p| p == &n.path))
                    .map(|n| n.id.clone())
                    .collect();
            }
        }

        let mut graph = if incremental && !changed.is_empty() {
            // Read the changed files' current content, patch the stored graph.
            let mut pairs = Vec::new();
            for rel in &changed {
                if super::extract::Lang::from_path(rel).is_none() {
                    continue; // unsupported language
                }
                ensure_safe_rel(rel)?; // refuse traversal — the allowlist only guards repo_path
                let p = root.join(rel);
                match fs::read_to_string(&p) {
                    Ok(content) => pairs.push((rel.clone(), content)),
                    // Genuinely gone → patch it out (drop its subgraph).
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        pairs.push((rel.clone(), String::new()))
                    }
                    // Exists but unreadable (permissions / transient I/O): leave
                    // its existing subgraph intact rather than silently dropping.
                    Err(_) => continue,
                }
            }
            store.refresh_files(&project_id, &pairs)?
        } else {
            let (files, was_capped) = walk_rs(root)?;
            capped = was_capped;
            build_rust_graph(&project_id, &files)?
        };

        // KGRAPH-04: optional semantic-edge pass (opt-in; best-effort). Runs
        // BEFORE clustering so INFERRED edges can influence communities. Gated
        // on SCRIBE_KG_SEMANTIC + a configured review daemon; a model failure
        // keeps the EXTRACTED graph unchanged.
        let semantic_on = std::env::var("SCRIBE_KG_SEMANTIC")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if semantic_on {
            let review_cfg = crate::review::ReviewConfig::from_env();
            if review_cfg.daemon_token.is_some() {
                let prompt = semantic::build_prompt(&graph);
                if let Ok(reply) = crate::scribe::dispatch_docs_generation(&review_cfg, &prompt).await {
                    if semantic::insert_semantic_edges(&mut graph, &reply) > 0 {
                        graph.recompute_degrees();
                    }
                }
            }
        }

        // Enrich + render on the full merged graph.
        cluster(&mut graph);
        pagerank(&mut graph); // KGRAPH-13: node importance for ranking/hotspots
        graph.generated_at = now_rfc3339_secs();

        // KGEMB-03: gated, best-effort node-embedding step. After pagerank,
        // before store.save, and reads-only against `graph` -- see
        // `embed_graph_nodes`'s doc comment for the non-blocking contract.
        let is_incremental = incremental && !changed.is_empty();
        let changed_node_scope: Option<HashSet<String>> = if is_incremental {
            let mut scope = old_node_ids_for_changed.clone();
            scope.extend(graph.nodes().filter(|n| changed.iter().any(|p| p == &n.path)).map(|n| n.id.clone()));
            Some(scope)
        } else {
            None
        };
        // Only run + surface the embed step when embeddings are enabled, so a
        // deployment that hasn't opted in (SCRIBE_KG_EMBED unset) gets a
        // scribe_kg_build result byte-for-byte identical to pre-KGEMB-03.
        let embed_stats = if cfg.embed_enabled {
            Some(embed_graph_nodes(&cfg, &project_id, &graph, changed_node_scope.as_ref()).await)
        } else {
            None
        };

        let lay = layout(&graph);
        let svg = render::to_svg(&graph, &lay);
        let graphml = render::to_graphml(&graph);
        let html = render::to_html(&graph, &lay);

        store.save(&project_id, &graph)?;
        let slug = slugify(&project_id);
        let artifacts = write_artifacts(&cfg.kg_store_dir, &slug, &svg, &graphml, &html)?;

        let clusters = graph.nodes().filter_map(|n| n.cluster).collect::<std::collections::HashSet<_>>().len();
        let mut result = json!({
            "project_id": project_id,
            "ok": true,
            "mode": if incremental && !changed.is_empty() { "incremental" } else { "full" },
            "nodes": graph.node_count(),
            "edges": graph.edge_count(),
            "clusters": clusters,
            "artifacts": artifacts,
            "file_cap_hit": capped,
        });
        if let Some(embed) = embed_stats {
            result["embed"] = embed;
        }
        Ok(structured(result))
    }
}

// ── scribe_kg_status ───────────────────────────────────────────────────────
pub struct ScribeKgStatus;

#[async_trait]
impl RustTool for ScribeKgStatus {
    fn name(&self) -> &str {
        "scribe_kg_status"
    }
    fn description(&self) -> &str {
        "Report a project's Atlas knowledge-graph status: node/edge/cluster counts, when it was last \
built, and which visual artifacts exist. Returns found:false if no graph has been built yet."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"project_id": {"type": "string"}},
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let cfg = ScribeConfig::from_env();
        let project_id = req_str(&args, "project_id")?;
        let store = GraphStore::from_config(&cfg);
        let Some(g) = store.load(&project_id)? else {
            return Ok(structured(json!({"project_id": project_id, "found": false})));
        };
        let slug = slugify(&project_id);
        let has = |ext: &str| Path::new(&cfg.kg_store_dir).join(format!("{slug}.{ext}")).exists();
        let clusters = g.nodes().filter_map(|n| n.cluster).collect::<std::collections::HashSet<_>>().len();
        Ok(structured(json!({
            "project_id": project_id, "found": true,
            "nodes": g.node_count(), "edges": g.edge_count(), "clusters": clusters,
            "generated_at": g.generated_at,
            "artifacts": {"svg": has("svg"), "graphml": has("graphml"), "html": has("html")},
        })))
    }
}

fn structured(v: Value) -> ToolOutput {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    ToolOutput { text, structured: Some(v) }
}

/// Register the build/status tools on the core registry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(ScribeKgBuild));
    let _ = registry.register(Box::new(ScribeKgStatus));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct Env {
        repo: PathBuf,
        store: PathBuf,
    }
    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.repo);
            let _ = fs::remove_dir_all(&self.store);
        }
    }

    fn setup(tag: &str) -> Env {
        let base = std::env::temp_dir().join(format!("atlas-kgbuild-{}-{}", tag, std::process::id()));
        let repo = base.join("repo");
        let store = base.join("store");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n").unwrap();
        fs::write(repo.join("src/w.rs"), "pub struct Widget;\nimpl Widget { pub fn n() -> Widget { Widget } }\n").unwrap();
        // a file under target/ that must be skipped
        fs::create_dir_all(repo.join("target")).unwrap();
        fs::write(repo.join("target/junk.rs"), "pub fn should_not_appear() {}\n").unwrap();
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store);
        std::env::set_var("SCRIBE_ALLOWED_REPO_ROOTS", &repo);
        Env { repo, store }
    }

    #[tokio::test]
    #[serial]
    async fn full_build_produces_graph_and_artifacts() {
        let env = setup("full");
        let out = ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": env.repo.to_str().unwrap()}))
            .await
            .unwrap();
        let v = out.structured.unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["nodes"].as_u64().unwrap() >= 4, "lib + w modules + fns/struct");
        assert_eq!(v["mode"], "full");
        // target/ file skipped
        assert!(v["artifacts"].as_array().unwrap().iter().any(|a| a == "term.svg"));
        assert!(env.store.join("term.svg").exists(), "svg written");
        assert!(env.store.join("term.graphml").exists());
        assert!(env.store.join("term.html").exists());
        assert!(env.store.join("term.json").exists(), "graph stored");

        // status reflects it
        let st = ScribeKgStatus.execute_structured(json!({"project_id": "TERM"})).await.unwrap().structured.unwrap();
        assert_eq!(st["found"], true);
        assert_eq!(st["artifacts"]["svg"], true);
    }

    #[tokio::test]
    #[serial]
    async fn skips_target_dir() {
        let env = setup("skip");
        ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": env.repo.to_str().unwrap()}))
            .await
            .unwrap();
        let g = GraphStore::from_config(&ScribeConfig::from_env()).load("TERM").unwrap().unwrap();
        assert!(g.get_node("crate::target::junk::should_not_appear").is_none(), "target/ skipped");
        assert!(g.get_node("crate::helper").is_some(), "src/lib.rs indexed");
    }

    #[tokio::test]
    #[serial]
    async fn repo_path_outside_allowlist_is_refused() {
        let _env = setup("deny");
        let other = std::env::temp_dir().join(format!("atlas-outside-{}", std::process::id()));
        let _ = fs::create_dir_all(&other);
        let err = ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": other.to_str().unwrap()}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "outside allowlist rejected");
        let _ = fs::remove_dir_all(&other);
    }

    #[tokio::test]
    #[serial]
    async fn incremental_traversal_in_changed_files_is_refused() {
        let env = setup("trav");
        let repo = env.repo.to_str().unwrap().to_string();
        ScribeKgBuild.execute_structured(json!({"project_id": "TERM", "repo_path": repo})).await.unwrap();
        let err = ScribeKgBuild
            .execute_structured(json!({
                "project_id": "TERM", "repo_path": repo,
                "incremental": true, "changed_files": ["../../../../etc/hosts.rs"]
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "traversal rejected, got {err:?}");
    }

    #[tokio::test]
    #[serial]
    async fn incremental_deleted_file_drops_its_subgraph() {
        let env = setup("del");
        let repo = env.repo.to_str().unwrap().to_string();
        ScribeKgBuild.execute_structured(json!({"project_id": "TERM", "repo_path": repo})).await.unwrap();
        fs::remove_file(env.repo.join("src/w.rs")).unwrap();
        ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": repo, "incremental": true, "changed_files": ["src/w.rs"]}))
            .await
            .unwrap();
        let g = GraphStore::from_config(&ScribeConfig::from_env()).load("TERM").unwrap().unwrap();
        assert!(g.get_node("crate::w::Widget").is_none(), "deleted file's nodes dropped");
        assert!(g.get_node("crate::helper").is_some(), "unchanged file preserved");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn walk_does_not_follow_symlinks_out_of_repo() {
        let env = setup("symlink");
        // a secret file OUTSIDE the repo, and a symlink to it INSIDE the repo
        let outside = std::env::temp_dir().join(format!("atlas-secret-{}", std::process::id()));
        let _ = fs::create_dir_all(&outside);
        fs::write(outside.join("secret.rs"), "pub fn top_secret() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.rs"), env.repo.join("src/link.rs")).unwrap();

        ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": env.repo.to_str().unwrap()}))
            .await
            .unwrap();
        let g = GraphStore::from_config(&ScribeConfig::from_env()).load("TERM").unwrap().unwrap();
        assert!(g.get_node("crate::link::top_secret").is_none(), "symlinked file not followed/read");
        assert!(g.get_node("crate::helper").is_some(), "real files still indexed");
        let _ = fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    #[serial]
    async fn status_unknown_project_is_found_false() {
        let _env = setup("status");
        let st = ScribeKgStatus.execute_structured(json!({"project_id": "NOPE"})).await.unwrap().structured.unwrap();
        assert_eq!(st["found"], false);
    }

    #[tokio::test]
    #[serial]
    async fn incremental_patches_changed_file() {
        let env = setup("incr");
        let repo = env.repo.to_str().unwrap().to_string();
        ScribeKgBuild.execute_structured(json!({"project_id": "TERM", "repo_path": repo})).await.unwrap();
        // change w.rs on disk, then incremental build
        fs::write(env.repo.join("src/w.rs"), "pub struct Gadget;\n").unwrap();
        let out = ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": repo, "incremental": true, "changed_files": ["src/w.rs"]}))
            .await
            .unwrap()
            .structured
            .unwrap();
        assert_eq!(out["mode"], "incremental");
        let g = GraphStore::from_config(&ScribeConfig::from_env()).load("TERM").unwrap().unwrap();
        assert!(g.get_node("crate::w::Gadget").is_some(), "new symbol present");
        assert!(g.get_node("crate::w::Widget").is_none(), "old symbol gone");
        assert!(g.get_node("crate::helper").is_some(), "unchanged file preserved");
    }

    // ─── KGEMB-03: embed step wiring ────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn embed_step_is_a_noop_when_scribe_kg_embed_unset() {
        let env = setup("noembed");
        std::env::remove_var("SCRIBE_KG_EMBED");
        let out = ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": env.repo.to_str().unwrap()}))
            .await
            .unwrap();
        let v = out.structured.unwrap();
        // The build behaves exactly as before this item: ok, full mode, real
        // node/edge counts -- the embed step being off changes nothing else.
        assert_eq!(v["ok"], true);
        assert_eq!(v["mode"], "full");
        assert!(v["nodes"].as_u64().unwrap() >= 4);
        // The `embed` key is entirely ABSENT when SCRIBE_KG_EMBED is unset — the
        // result is byte-for-byte identical to pre-KGEMB-03 (no observability
        // field is added for a deployment that hasn't opted in).
        assert!(v.get("embed").is_none(), "embed field must be absent when SCRIBE_KG_EMBED unset");
    }

    #[tokio::test]
    #[serial]
    async fn embed_step_degrades_cleanly_when_store_not_configured() {
        // Mirrors vec_store.rs's own NotConfigured test shape: if a real DSN
        // happens to be configured in this process, skip rather than mutate
        // global env state / risk a live connection from a unit test.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let env = setup("embednostore");
        std::env::remove_var("ATLAS_DATABASE_URL");
        std::env::set_var("SCRIBE_KG_EMBED", "1");
        let result = ScribeKgBuild
            .execute_structured(json!({"project_id": "TERM", "repo_path": env.repo.to_str().unwrap()}))
            .await;
        std::env::remove_var("SCRIBE_KG_EMBED");

        let out = result.expect("build must still succeed when the embed step can't run");
        let v = out.structured.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["embed"]["ran"], false);
        let reason = v["embed"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("not configured") || reason.contains("ATLAS_DATABASE_URL"),
            "unexpected embed.reason: {reason}"
        );
    }

    // ─── KGEMB-03: pure changed-node selection / hash-skip decision logic ──

    #[test]
    fn embed_candidate_ids_full_build_returns_all_current_nodes() {
        let current: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let ids: HashSet<String> = embed_candidate_ids(&current, None).into_iter().collect();
        assert_eq!(ids, current.into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn embed_candidate_ids_incremental_filters_to_changed_scope() {
        let current: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let changed: HashSet<String> = ["b"].iter().map(|s| s.to_string()).collect();
        let ids = embed_candidate_ids(&current, Some(&changed));
        assert_eq!(ids, vec!["b".to_string()]);
    }

    #[test]
    fn embed_delete_ids_full_build_deletes_stale_store_rows_not_in_current_graph() {
        let current: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let mut existing = HashMap::new();
        existing.insert("a".to_string(), "h1".to_string());
        existing.insert("gone".to_string(), "h2".to_string());
        let mut deletes = embed_delete_ids(&current, None, &existing);
        deletes.sort();
        assert_eq!(deletes, vec!["gone".to_string()]);
    }

    #[test]
    fn embed_delete_ids_incremental_deletes_changed_scope_ids_no_longer_current() {
        // "removed" was in the changed-node scope (its file/symbol was one of
        // the changed files) but is absent from the post-refresh graph --
        // must be deleted. "a" is in-scope AND still current -- must not be.
        let current: BTreeSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let changed: HashSet<String> = ["a", "removed"].iter().map(|s| s.to_string()).collect();
        let existing = HashMap::new();
        let deletes = embed_delete_ids(&current, Some(&changed), &existing);
        assert_eq!(deletes, vec!["removed".to_string()]);
    }

    #[test]
    fn embed_delete_ids_incremental_ignores_ids_outside_the_changed_scope() {
        // A node id that simply isn't a current node id, but was never part
        // of THIS build's changed scope, must not be swept up as a deletion
        // -- only the scope this build actually touched is in play.
        let current: BTreeSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let changed: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let existing = HashMap::new();
        let deletes = embed_delete_ids(&current, Some(&changed), &existing);
        assert!(deletes.is_empty());
    }

    #[test]
    fn skip_unchanged_cards_skips_matching_hash_and_embeds_the_rest() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut new_hashes = HashMap::new();
        new_hashes.insert("a".to_string(), "h1".to_string());
        new_hashes.insert("b".to_string(), "h2-changed".to_string());
        new_hashes.insert("c".to_string(), "h3".to_string());
        let mut existing = HashMap::new();
        existing.insert("a".to_string(), "h1".to_string()); // unchanged -> skip
        existing.insert("b".to_string(), "h2".to_string()); // changed -> embed
        // "c" has no prior hash at all -> new node -> embed

        let (mut to_embed, to_skip) = skip_unchanged_cards(&candidates, &new_hashes, &existing);
        to_embed.sort();
        assert_eq!(to_skip, vec!["a".to_string()]);
        assert_eq!(to_embed, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn skip_unchanged_cards_all_unchanged_yields_zero_embeds() {
        let candidates = vec!["a".to_string(), "b".to_string()];
        let hashes: HashMap<String, String> =
            [("a".to_string(), "h1".to_string()), ("b".to_string(), "h2".to_string())].into_iter().collect();
        let (to_embed, mut to_skip) = skip_unchanged_cards(&candidates, &hashes, &hashes);
        to_skip.sort();
        assert!(to_embed.is_empty());
        assert_eq!(to_skip, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn calls_adjacency_maps_caller_and_callee_names() {
        use super::super::model::{Confidence, KgEdge, KgNode, NodeKind};
        let mut g = KnowledgeGraph::new("TEST");
        g.insert_node(KgNode::new("crate::caller", NodeKind::Function, "caller", "src/lib.rs"));
        g.insert_node(KgNode::new("crate::callee", NodeKind::Function, "callee", "src/lib.rs"));
        g.insert_edge(KgEdge::new("crate::caller", "crate::callee", EdgeKind::Calls, Confidence::Extracted)).unwrap();

        let adj = calls_adjacency(&g);
        assert_eq!(adj.get("crate::caller").unwrap().1, vec!["callee".to_string()], "caller's callees");
        assert_eq!(adj.get("crate::callee").unwrap().0, vec!["caller".to_string()], "callee's callers");
    }
}
