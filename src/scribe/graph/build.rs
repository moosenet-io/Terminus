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

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::GraphStore;
use super::{build_rust_graph, cluster, layout, render};
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
fn walk_rs(root: &Path) -> Result<(Vec<(String, String)>, bool), ToolError> {
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
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if fs::metadata(&path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
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

        let mut graph = if incremental && !changed.is_empty() {
            // Read the changed files' current content, patch the stored graph.
            let mut pairs = Vec::new();
            for rel in &changed {
                if !rel.ends_with(".rs") {
                    continue;
                }
                let p = root.join(rel);
                if let Ok(content) = fs::read_to_string(&p) {
                    pairs.push((rel.clone(), content));
                } else {
                    // a deleted file: patch it out by extracting an empty set
                    // for its path (refresh_files removes the old subgraph).
                    pairs.push((rel.clone(), String::new()));
                }
            }
            store.refresh_files(&project_id, &pairs)?
        } else {
            let (files, was_capped) = walk_rs(root)?;
            capped = was_capped;
            build_rust_graph(&project_id, &files)?
        };

        // Enrich + render on the full merged graph.
        cluster(&mut graph);
        graph.generated_at = now_rfc3339_secs();
        let lay = layout(&graph);
        let svg = render::to_svg(&graph, &lay);
        let graphml = render::to_graphml(&graph);
        let html = render::to_html(&graph, &lay);

        store.save(&project_id, &graph)?;
        let slug = slugify(&project_id);
        let artifacts = write_artifacts(&cfg.kg_store_dir, &slug, &svg, &graphml, &html)?;

        let clusters = graph.nodes().filter_map(|n| n.cluster).collect::<std::collections::HashSet<_>>().len();
        Ok(structured(json!({
            "project_id": project_id,
            "ok": true,
            "mode": if incremental && !changed.is_empty() { "incremental" } else { "full" },
            "nodes": graph.node_count(),
            "edges": graph.edge_count(),
            "clusters": clusters,
            "artifacts": artifacts,
            "file_cap_hit": capped,
        })))
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
}
