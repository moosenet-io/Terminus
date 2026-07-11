//! DOCGEN-12: ground-truth Rust module/crate dependency graph (S95/S111,
//! Plane TERM-163).
//!
//! ## Zero-hallucination, no LLM
//! Unlike `diagram.rs` (DOCGEN-11, which asks a model to describe an
//! architecture), this module NEVER calls a model. Every node and edge in
//! [`CrateGraphModel`] is extracted directly from real source: each
//! workspace crate's `Cargo.toml` (crate-level dependency edges) plus a
//! deterministic scan of `mod`/`use` declarations in the target crate's
//! `.rs` files (module-level containment + dependency edges), reusing
//! `crate::scribe::inspect`'s existing read-only worktree checkout and file
//! walk (`inspect_module` -> [`crate::scribe::inspect::FileExcerpt`]'s new
//! `mod_decls`/`use_decls` fields, added by this item) rather than a second
//! file-walking implementation. Because the graph is derived from the code
//! itself, it doubles as a drift signal per the spec: if this graph ever
//! contradicts prose documentation (a README claiming module A doesn't
//! depend on module B when the extracted graph shows an edge), that's
//! exactly the kind of code-vs-contract mismatch DOCGEN-10 escalates.
//!
//! ## Tool availability (checked before writing this file)
//! `which cargo-modules cargo-depgraph dot sfdp` in this sandbox found
//! NONE of the four on `PATH`. Per the item's own guidance ("if the graph
//! tool is absent... follow the DOCGEN-14/06 pattern"), this module does
//! NOT shell out to `cargo-modules`/`cargo-depgraph` at all -- it builds
//! the ground-truth graph MODEL itself, in pure Rust, from the same
//! line-scan approach `scribe::inspect::extract_excerpt` already uses for
//! doc comments/public signatures (deterministic, dependency-free, fully
//! unit-tested). This is a permanent design choice, not a temporary
//! workaround pending those binaries: it keeps DOCGEN-12 buildable and
//! testable in any sandbox, exactly like `render/pdf.rs`'s "no PDF crate in
//! the tree" skip pattern is permanent rather than provisional.
//!
//! ## Render-if-available, else produce-and-test the model (mirrors DOCGEN-14)
//! The DOT source is ALWAYS produced (the diffable, versioned ground-truth
//! artifact -- see [`to_dot`]). Rasterizing it to SVG additionally shells
//! out to `sfdp` (preferred) or `neato` (fallback) if either is on `PATH`,
//! reusing `render::wiki_graph`'s `engine_available`/`run_engine` (made
//! `pub(crate)` by this item rather than duplicated) -- see
//! [`render_crate_graph_svg`]. Neither present -> the DOT model is returned
//! with `svg: None` and a clear note, exactly mirroring
//! [`super::render::pdf::render`] and
//! [`super::render::wiki_graph::render_graph_view`]'s skip shape.
//!
//! ## Cycle detection (`--acyclic`)
//! [`find_cycles`] runs a DFS over the module-level dependency edges (the
//! `use`-derived edges; the `mod`-declaration containment edges are a tree
//! by construction and can never cycle) and reports every cycle found, as
//! an ordered list of module ids. This is the ground-truth equivalent of
//! `cargo-modules --acyclic`: it surfaces real circular `use` dependencies
//! rather than merely asserting none exist.
//!
//! ## Scope limits (documented, not hidden)
//! - Crate-level edges only cover WORKSPACE-internal path/name dependencies
//!   (a crate depending on another crate declared in this workspace).
//!   External crates.io dependencies are deliberately excluded from the
//!   emitted graph -- this workspace's root crate alone declares dozens of
//!   them, and a ground-truth graph of "what this codebase's own crates
//!   depend on each other" is the useful signal, not a re-derivation of
//!   `Cargo.lock`.
//! - Module ids are derived from file PATHS relative to `src/`, not full
//!   Rust name resolution -- `src/tools/docgen/crate_graph.rs` is module id
//!   `"tools/docgen/crate_graph"`, and a `mod.rs`/`lib.rs` file's id is its
//!   containing directory (or `""` for the crate root). This sidesteps
//!   `#[path]` attributes and macro-generated modules (out of scope for a
//!   lightweight, dependency-free scan) while staying exact for the
//!   overwhelming common case, and -- because it's just the file path --
//!   is trivially auditable against the real tree.
//! - `use` edges are resolved against the ACTUAL discovered module id set
//!   (see [`resolve_use_target`]) rather than a blind "always drop the last
//!   segment" heuristic, so `use crate::error::ToolError;` correctly
//!   resolves to the `"error"` module (dropping the type) while
//!   `use crate::tools::docgen;` correctly resolves to `"tools/docgen"`
//!   (keeping the whole path, since it names a module, not an item within
//!   one). A `use` path with no matching prefix in the module id set (e.g.
//!   an external crate, or a re-exported item this scan can't resolve) is
//!   simply not turned into an edge -- never a guessed/wrong one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::error::ToolError;
use crate::scribe::inspect::{self, FileExcerpt, InspectionWorktree};

use super::render::wiki_graph::{engine_available, run_engine, GraphTheme};
use super::versioning::{ArtifactKey, ArtifactVersion, VersionStore};

// ─── Graph model (the diffable, ground-truth artifact) ──────────────────────

/// What kind of thing a [`GraphNode`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    /// A workspace crate (a `Cargo.toml` package), e.g. `"terminus-rs"`.
    Crate,
    /// A module within the target crate, identified by its source file path
    /// relative to `src/` (see the module doc comment's scope-limits note).
    Module,
}

/// One node in the graph: its stable id and what kind of thing it is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphNode {
    pub id: String,
    pub kind: NodeKind,
}

/// The full ground-truth graph: crate-level nodes/edges (from `Cargo.toml`)
/// and module-level nodes/edges (from `mod`/`use` scanning), combined into
/// one model. Deterministic and sorted, so regenerating against an
/// unchanged tree produces byte-identical output (same convention as
/// `render::wiki_graph::GraphModel`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateGraphModel {
    pub nodes: Vec<GraphNode>,
    /// Directed `(depender, dependency)` edges -- "depender depends on
    /// dependency". For module nodes this includes BOTH the `mod`
    /// declaration containment edges (parent contains child) and the
    /// `use`-derived reference edges; both kinds are tagged in
    /// [`EdgeKind`] rather than merged into one untyped list, so a
    /// consumer (DOT rendering, cycle detection) can tell them apart.
    pub edges: Vec<GraphEdge>,
}

/// Why an edge exists -- lets [`find_cycles`] restrict itself to the edges
/// that can actually cycle (containment is a tree; it can't), and lets
/// [`to_dot`] style the two kinds differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Crate-level: `from`'s `Cargo.toml` declares a workspace-internal
    /// dependency on `to`.
    CrateDependency,
    /// Module-level: `from`'s file declares `mod to_leaf;` (containment).
    ModuleContainment,
    /// Module-level: `from`'s file has a `use` statement resolving into
    /// `to`'s module (a real reference/dependency edge).
    ModuleUse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

impl CrateGraphModel {
    fn add_node(&mut self, id: impl Into<String>, kind: NodeKind) {
        let id = id.into();
        if !self.nodes.iter().any(|n| n.id == id) {
            self.nodes.push(GraphNode { id, kind });
        }
    }

    fn add_edge(&mut self, from: impl Into<String>, to: impl Into<String>, kind: EdgeKind) {
        let edge = GraphEdge { from: from.into(), to: to.into(), kind };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    /// Sort nodes and edges into a deterministic order. Called once after
    /// construction so callers never depend on filesystem iteration order.
    fn sort(&mut self) {
        self.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        self.edges.sort_by(|a, b| (&a.from, &a.to, a.kind as u8).cmp(&(&b.from, &b.to, b.kind as u8)));
    }
}

// ─── Crate-level extraction (Cargo.toml) ────────────────────────────────────

/// One crate's minimal manifest facts: its package name and the set of
/// OTHER workspace crate names it depends on (path or workspace deps only
/// -- see the module doc comment's scope-limits note).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CrateManifest {
    name: String,
    /// Every dependency table key this manifest declares, regardless of
    /// whether it's a workspace member -- filtered down to internal edges
    /// by the caller, which knows the full workspace member set.
    dependency_names: BTreeSet<String>,
}

/// Parse one `Cargo.toml`'s `[package].name` and `[dependencies]` (plus
/// `[dev-dependencies]`/`[build-dependencies]`, since any of the three can
/// declare a workspace-internal edge) key names. Returns `None` for a
/// manifest with no `[package]` table (a virtual workspace root) rather
/// than erroring -- that's a valid, if unusual, Cargo.toml shape.
fn parse_crate_manifest(toml_content: &str) -> Result<Option<CrateManifest>, ToolError> {
    let parsed: toml::Value = toml_content
        .parse()
        .map_err(|e| ToolError::Execution(format!("failed to parse Cargo.toml: {e}")))?;

    let Some(name) = parsed.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) else {
        return Ok(None);
    };

    let mut dependency_names = BTreeSet::new();
    for table_name in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = parsed.get(table_name).and_then(|t| t.as_table()) {
            dependency_names.extend(table.keys().cloned());
        }
    }

    Ok(Some(CrateManifest { name: name.to_string(), dependency_names }))
}

/// Build the crate-level portion of the graph from `repo_root`'s workspace:
/// the root `Cargo.toml` plus every `[workspace].members` entry's own
/// `Cargo.toml`. Every discovered crate becomes a [`NodeKind::Crate`] node;
/// an edge is added ONLY when a crate's declared dependency name matches
/// another crate discovered in this same workspace (external crates.io
/// dependencies never produce an edge -- see the module doc comment).
fn build_crate_level_graph(repo_root: &Path, model: &mut CrateGraphModel) -> Result<(), ToolError> {
    let root_toml_path = repo_root.join("Cargo.toml");
    let root_toml = std::fs::read_to_string(&root_toml_path).map_err(|e| {
        ToolError::NotFound(format!("no Cargo.toml at {}: {e}", root_toml_path.display()))
    })?;
    let root_parsed: toml::Value = root_toml
        .parse()
        .map_err(|e| ToolError::Execution(format!("failed to parse root Cargo.toml: {e}")))?;

    let members: Vec<String> = root_parsed
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let mut manifests = Vec::new();
    if let Some(root_manifest) = parse_crate_manifest(&root_toml)? {
        manifests.push(root_manifest);
    }
    for member in &members {
        let member_toml_path = repo_root.join(member).join("Cargo.toml");
        let Ok(member_toml) = std::fs::read_to_string(&member_toml_path) else {
            continue; // a declared member with no manifest is skipped, not fatal
        };
        if let Some(manifest) = parse_crate_manifest(&member_toml)? {
            manifests.push(manifest);
        }
    }

    let known_crate_names: BTreeSet<String> = manifests.iter().map(|m| m.name.clone()).collect();

    for manifest in &manifests {
        model.add_node(manifest.name.clone(), NodeKind::Crate);
    }
    for manifest in &manifests {
        for dep in &manifest.dependency_names {
            if known_crate_names.contains(dep) && dep != &manifest.name {
                model.add_edge(manifest.name.clone(), dep.clone(), EdgeKind::CrateDependency);
            }
        }
    }

    Ok(())
}

// ─── Module-level extraction (mod/use scanning, via scribe::inspect reuse) ──

/// Derive a module id from a source file path relative to `src_root`. The
/// crate root (`lib.rs`/`main.rs`) is id `""`; a `mod.rs`/directory-owning
/// file's id is its containing directory; any other file's id is its path
/// minus the `.rs` extension, with path separators normalized to `/` (see
/// the module doc comment's scope-limits note on why this is file-path-
/// based rather than full Rust name resolution).
fn module_id_for_file(src_root: &Path, file_path: &Path) -> String {
    let rel = file_path.strip_prefix(src_root).unwrap_or(file_path);
    let rel = rel.with_extension("");
    let mut parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if let Some(last) = parts.last() {
        if last == "mod" || last == "lib" || last == "main" {
            parts.pop();
        }
    }
    parts.join("/")
}

/// Resolve a raw `use` path (e.g. `"crate::error::ToolError"`,
/// `"super::vault::slugify"`) into a target module id, using ONLY the
/// actually-discovered module id set (`known_modules`) -- never a blind
/// "drop the last segment" guess. Tries the longest possible prefix first
/// (the full path minus 0 segments, then minus 1, minus 2, ...) so
/// `use crate::tools::docgen;` (naming a module itself) resolves to
/// `"tools/docgen"` rather than incorrectly dropping `docgen`, while
/// `use crate::error::ToolError;` (naming an item) correctly drops
/// `ToolError` to land on `"error"`. Returns `None` if no prefix matches
/// any known module (an external crate, std, or an unresolvable
/// re-export) -- never a guessed edge.
///
/// `current_module` resolves a leading `super::`/`self::` relative to the
/// file the `use` statement was found in; a `crate::`-prefixed path is
/// resolved as absolute from the crate root (`""`).
fn resolve_use_target(raw: &str, current_module: &str, known_modules: &BTreeSet<String>) -> Option<String> {
    let segments: Vec<&str> = raw.split("::").collect();

    // Build the fully-qualified (crate-root-relative) segment list for this
    // `use` path, based on its leading qualifier. `super::` resolves one
    // level relative to `current_module`'s parent (multi-level
    // `super::super::...` is a documented, out-of-scope case for this
    // lightweight scan). Any other leading segment (an external crate name,
    // or bare `std`/`serde_json`/etc.) is out of scope -- returns `None`.
    let absolute: Vec<String> = match segments.first().copied() {
        Some("crate") => segments[1..].iter().map(|s| s.to_string()).collect(),
        Some("super") => {
            let mut v: Vec<String> =
                parent_module_id(current_module).split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
            v.extend(segments[1..].iter().map(|s| s.to_string()));
            v
        }
        Some("self") => {
            let mut v: Vec<String> =
                current_module.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
            v.extend(segments[1..].iter().map(|s| s.to_string()));
            v
        }
        _ => return None, // an external crate path (e.g. "serde_json::Value") -- out of scope
    };

    resolve_from_absolute_segments(&absolute, known_modules)
}

fn resolve_from_absolute_segments(segments: &[String], known_modules: &BTreeSet<String>) -> Option<String> {
    // Try the longest prefix first (whole path = a module reference),
    // shrinking by one segment each time (dropping trailing item names)
    // until a known module id matches, or nothing is left.
    for take in (1..=segments.len()).rev() {
        let candidate = segments[..take].join("/");
        if known_modules.contains(&candidate) {
            return Some(candidate);
        }
    }
    // The crate root itself (`use crate::SomeTopLevelItem;`) has module id
    // `""`, which the loop above can't produce since `take >= 1`. Only
    // return it if there truly was exactly one segment (an item directly
    // in the crate root) AND the crate root module is known.
    if segments.len() == 1 && known_modules.contains("") {
        return Some(String::new());
    }
    None
}

fn parent_module_id(module_id: &str) -> String {
    match module_id.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Build the module-level portion of the graph for the crate rooted at
/// `src_root` (e.g. `<worktree>/src`), by walking `excerpts` -- the
/// [`FileExcerpt`]s `scribe::inspect::inspect_module` already extracted
/// (this function performs NO file I/O of its own; it is pure over already-
/// walked data, matching `render::wiki_graph::build_graph_model`'s shape).
fn build_module_level_graph(src_root: &Path, excerpts: &[FileExcerpt], model: &mut CrateGraphModel) {
    // Pass 1: every file becomes a module node.
    let mut file_to_module: BTreeMap<&str, String> = BTreeMap::new();
    for excerpt in excerpts {
        let module_id = module_id_for_file(src_root, Path::new(&excerpt.path));
        file_to_module.insert(&excerpt.path, module_id.clone());
        model.add_node(module_id, NodeKind::Module);
    }
    let known_modules: BTreeSet<String> = model
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Module)
        .map(|n| n.id.clone())
        .collect();

    // Pass 2: mod-declaration containment edges + use-derived edges.
    for excerpt in excerpts {
        let this_module = file_to_module.get(excerpt.path.as_str()).cloned().unwrap_or_default();

        for child_name in &excerpt.mod_decls {
            let child_id = if this_module.is_empty() {
                child_name.clone()
            } else {
                format!("{this_module}/{child_name}")
            };
            if known_modules.contains(&child_id) {
                model.add_edge(this_module.clone(), child_id, EdgeKind::ModuleContainment);
            }
        }

        for use_path in &excerpt.use_decls {
            if let Some(target) = resolve_use_target(use_path, &this_module, &known_modules) {
                if target != this_module {
                    model.add_edge(this_module.clone(), target, EdgeKind::ModuleUse);
                }
            }
        }
    }
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Build the full ground-truth crate graph for the crate rooted at
/// `repo_root`, scanning `module_path` (typically `"src"`) for its module
/// tree. Pure over an already-checked-out directory -- callers that want
/// the read-only-worktree behavior described in this item's spec go through
/// [`build_crate_graph_from_worktree`], which resolves `repo_root` from a
/// [`InspectionWorktree`] first; this function itself takes any directory
/// so it's independently unit-testable against a fixture tree with no git
/// checkout involved (same testability shape as `render::wiki_graph`'s pure
/// `build_graph_model`).
pub fn build_crate_graph(repo_root: &Path, module_path: &str) -> Result<CrateGraphModel, ToolError> {
    let mut model = CrateGraphModel::default();
    build_crate_level_graph(repo_root, &mut model)?;

    let src_root = repo_root.join(module_path);
    if src_root.exists() {
        let excerpts = walk_for_module_graph(&src_root)?;
        build_module_level_graph(&src_root, &excerpts, &mut model);
    }

    model.sort();
    Ok(model)
}

/// Walk `src_root` via `scribe::inspect`'s own file-walk (through
/// `inspect_module`, reusing its recursive `.rs` walk and `FileExcerpt`
/// extraction rather than a second implementation). Constructs a throwaway
/// [`InspectionWorktree`] pointed directly at `src_root`'s parent -- this is
/// safe and correct because `inspect_module` only ever READS files under
/// `wt.path.join(module_path)`; it performs no git operation itself (that
/// happens earlier, in [`inspect::checkout`], which
/// [`build_crate_graph_from_worktree`] already called).
fn walk_for_module_graph(src_root: &Path) -> Result<Vec<FileExcerpt>, ToolError> {
    let repo_root = src_root.parent().ok_or_else(|| {
        ToolError::InvalidArgument(format!("src root {} has no parent directory", src_root.display()))
    })?;
    let module_name = src_root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ToolError::InvalidArgument(format!("src root {} has no file name", src_root.display())))?;
    let throwaway_wt = InspectionWorktree {
        path: repo_root.to_path_buf(),
        repo_path: repo_root.to_path_buf(),
        git_ref: "docgen-crate-graph-scan".to_string(),
    };
    let bundle = inspect::inspect_module(&throwaway_wt, module_name)?;
    Ok(bundle.files)
}

/// Build the crate graph starting from an already-checked-out
/// [`InspectionWorktree`] (`scribe::inspect::checkout`'s result) -- the
/// entry point matching this item's spec ("extend `crate::scribe::inspect`
/// to run graph extraction in the read-only worktree"). `module_path` is
/// typically `"src"`.
pub fn build_crate_graph_from_worktree(
    wt: &InspectionWorktree,
    module_path: &str,
) -> Result<CrateGraphModel, ToolError> {
    build_crate_graph(&wt.path, module_path)
}

// ─── Cycle detection (`--acyclic` ground truth) ─────────────────────────────

/// Find every cycle in the graph's `use`-derived edges ([`EdgeKind::ModuleUse`]
/// only -- containment and crate-dependency edges form a tree/DAG by
/// construction in this model and are excluded, matching `cargo-modules
/// --acyclic`'s own focus on circular USE dependencies, not the module
/// tree). Returns each distinct cycle as an ordered list of module ids
/// (the cycle's own traversal order, first id repeated is implied, not
/// included). Empty if the graph is acyclic. Deterministic: iterates nodes
/// in their already-sorted order, so the same graph always reports cycles
/// in the same order.
pub fn find_cycles(model: &CrateGraphModel) -> Vec<Vec<String>> {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &model.edges {
        if edge.kind == EdgeKind::ModuleUse {
            adjacency.entry(edge.from.as_str()).or_default().push(edge.to.as_str());
        }
    }

    let mut cycles = Vec::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();

    for node in &model.nodes {
        if node.kind != NodeKind::Module || visited.contains(node.id.as_str()) {
            continue;
        }
        let mut stack: Vec<&str> = Vec::new();
        let mut on_stack: BTreeSet<&str> = BTreeSet::new();
        dfs_find_cycles(node.id.as_str(), &adjacency, &mut visited, &mut stack, &mut on_stack, &mut cycles);
    }

    cycles
}

fn dfs_find_cycles<'a>(
    node: &'a str,
    adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
    visited: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
    on_stack: &mut BTreeSet<&'a str>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node);
    stack.push(node);
    on_stack.insert(node);

    if let Some(neighbors) = adjacency.get(node) {
        for &next in neighbors {
            if on_stack.contains(next) {
                // Found a cycle: the slice of `stack` from `next`'s first
                // occurrence to the top is the cycle.
                if let Some(start) = stack.iter().position(|&n| n == next) {
                    let cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                    if !cycles.contains(&cycle) {
                        cycles.push(cycle);
                    }
                }
            } else if !visited.contains(next) {
                dfs_find_cycles(next, adjacency, visited, stack, on_stack, cycles);
            }
        }
    }

    stack.pop();
    on_stack.remove(node);
}

// ─── DOT serialization + theming (mirrors render::wiki_graph::to_dot) ───────

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

struct ThemeColors {
    bgcolor: &'static str,
    crate_fill: &'static str,
    module_fill: &'static str,
    font: &'static str,
    use_edge: &'static str,
    contain_edge: &'static str,
    cycle_edge: &'static str,
}

fn theme_colors(theme: GraphTheme) -> ThemeColors {
    match theme {
        GraphTheme::Light => ThemeColors {
            bgcolor: "#ffffff",
            crate_fill: "#ffe8cc",
            module_fill: "#e8ecff",
            font: "#1a1a2e",
            use_edge: "#4a4a8a",
            contain_edge: "#bbbbbb",
            cycle_edge: "#cc3333",
        },
        GraphTheme::Dark => ThemeColors {
            bgcolor: "#1a1a2e",
            crate_fill: "#5a3d1f",
            module_fill: "#2d2d55",
            font: "#eef2ff",
            use_edge: "#8a8aff",
            contain_edge: "#555577",
            cycle_edge: "#ff6666",
        },
    }
}

/// Serialize `model` as GraphViz DOT source, theming per `theme` and
/// highlighting any edge that participates in a detected cycle (`cycles`,
/// from [`find_cycles`]) in the theme's alert color -- the surfaced
/// `--acyclic`-equivalent signal. This is the diffable, always-produced
/// artifact regardless of whether a layout engine is available to
/// rasterize it (mirrors `render::wiki_graph::to_dot`).
pub fn to_dot(model: &CrateGraphModel, cycles: &[Vec<String>], theme: GraphTheme) -> String {
    let c = theme_colors(theme);
    let cycle_edges: BTreeSet<(String, String)> = cycles
        .iter()
        .flat_map(|cycle| {
            let n = cycle.len();
            (0..n).map(move |i| (cycle[i].clone(), cycle[(i + 1) % n].clone()))
        })
        .collect();

    let mut out = String::new();
    out.push_str("digraph crate_graph {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str(&format!("  bgcolor=\"{}\";\n", c.bgcolor));
    out.push_str(&format!("  node [style=filled, fontcolor=\"{}\"];\n\n", c.font));

    for node in &model.nodes {
        let (shape, fill) = match node.kind {
            NodeKind::Crate => ("box", c.crate_fill),
            NodeKind::Module => ("ellipse", c.module_fill),
        };
        let label = if node.id.is_empty() { "(crate root)" } else { node.id.as_str() };
        out.push_str(&format!(
            "  \"{}\" [label=\"{}\", shape={}, fillcolor=\"{}\"];\n",
            dot_escape(&node.id),
            dot_escape(label),
            shape,
            fill
        ));
    }
    out.push('\n');

    for edge in &model.edges {
        let is_cycle_edge = cycle_edges.contains(&(edge.from.clone(), edge.to.clone()));
        let (color, style) = if is_cycle_edge {
            (c.cycle_edge, "bold")
        } else {
            match edge.kind {
                EdgeKind::CrateDependency => (c.use_edge, "solid"),
                EdgeKind::ModuleContainment => (c.contain_edge, "dashed"),
                EdgeKind::ModuleUse => (c.use_edge, "solid"),
            }
        };
        out.push_str(&format!(
            "  \"{}\" -> \"{}\" [color=\"{}\", style={}];\n",
            dot_escape(&edge.from),
            dot_escape(&edge.to),
            color,
            style
        ));
    }
    out.push_str("}\n");
    out
}

// ─── Graph-view rendering: SVG-if-available, else model+note (PDF pattern) ──

/// The result of attempting to rasterize the crate graph: `dot_source` is
/// ALWAYS present, `svg` is `Some` only when a local layout engine
/// (`sfdp`/`neato`) actually rasterized it, `cycles` reports every detected
/// circular `use` dependency (empty if acyclic) -- mirrors
/// `render::wiki_graph::GraphRenderResult`'s shape exactly, with the added
/// `cycles` field this item's spec calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateGraphRenderResult {
    pub dot_source: String,
    pub svg: Option<String>,
    pub engine_used: Option<String>,
    pub cycles: Vec<Vec<String>>,
    pub note: String,
}

/// Build the crate graph for `repo_root`/`module_path`, detect cycles, and
/// attempt SVG rasterization via `sfdp` then `neato` (reusing
/// `render::wiki_graph`'s engine spawn helpers). If neither binary is on
/// `PATH`, returns the DOT model with `svg: None` and a clear note -- the
/// model itself (and cycle detection) is unaffected either way.
pub fn render_crate_graph_svg(
    repo_root: &Path,
    module_path: &str,
    theme: GraphTheme,
) -> Result<CrateGraphRenderResult, ToolError> {
    let model = build_crate_graph(repo_root, module_path)?;
    let cycles = find_cycles(&model);
    let dot_source = to_dot(&model, &cycles, theme);

    for engine in ["sfdp", "neato"] {
        if engine_available(engine) {
            if let Some(svg) = run_engine(engine, &dot_source) {
                return Ok(CrateGraphRenderResult {
                    dot_source,
                    svg: Some(svg),
                    engine_used: Some(engine.to_string()),
                    cycles,
                    note: format!("rendered via {engine} -Tsvg"),
                });
            }
        }
    }

    Ok(CrateGraphRenderResult {
        dot_source,
        svg: None,
        engine_used: None,
        cycles,
        note: "graph SVG rasterization unavailable in this environment (no `sfdp`/`neato`/\
`cargo-modules`/`cargo-depgraph` on PATH) -- the ground-truth DOT graph model was still \
produced and is fully diffable/testable; node/edge extraction is direct from Cargo.toml + \
mod/use scanning, no LLM involved."
            .to_string(),
    })
}

// ─── Versioning (DOCGEN-07 reuse, not reimplementation) ─────────────────────

/// [`ArtifactKey`] for the DOT source, per project -- one history, since a
/// crate has exactly one ground-truth graph (unlike `diagram.rs`, which
/// keys per-module because there can be many distinct generated diagrams).
pub fn crate_graph_dot_key(project: &str) -> ArtifactKey {
    ArtifactKey::new(project, "crate-graph-dot")
}

pub fn crate_graph_svg_key(project: &str) -> ArtifactKey {
    ArtifactKey::new(project, "crate-graph-svg")
}

/// Store `result`'s DOT source (always) and SVG (when produced) as new
/// versions in `store`, via [`VersionStore::store_version`] -- the exact
/// same append-only DOCGEN-07 store `diagram.rs`'s `version_diagram` uses.
/// Never overwrites a prior version.
pub fn version_crate_graph(
    store: &VersionStore,
    project: &str,
    result: &CrateGraphRenderResult,
    source_commit: &str,
    timestamp: &str,
) -> (ArtifactVersion, Option<ArtifactVersion>) {
    let dot_version =
        store.store_version(crate_graph_dot_key(project), result.dot_source.clone(), source_commit, timestamp);
    let svg_version = result
        .svg
        .as_ref()
        .map(|svg| store.store_version(crate_graph_svg_key(project), svg.clone(), source_commit, timestamp));
    (dot_version, svg_version)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(label: &str) -> std::path::PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("docgen-crate-graph-test-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    // ── module id derivation ─────────────────────────────────────────────

    #[test]
    fn module_id_for_lib_rs_is_crate_root() {
        let src = Path::new("/repo/src");
        assert_eq!(module_id_for_file(src, Path::new("/repo/src/lib.rs")), "");
    }

    #[test]
    fn module_id_for_mod_rs_is_containing_directory() {
        let src = Path::new("/repo/src");
        assert_eq!(
            module_id_for_file(src, Path::new("/repo/src/tools/docgen/mod.rs")),
            "tools/docgen"
        );
    }

    #[test]
    fn module_id_for_plain_file_strips_extension() {
        let src = Path::new("/repo/src");
        assert_eq!(
            module_id_for_file(src, Path::new("/repo/src/tools/docgen/crate_graph.rs")),
            "tools/docgen/crate_graph"
        );
    }

    // ── use-target resolution against a known module set ────────────────

    #[test]
    fn resolve_use_target_drops_item_name_to_reach_known_module() {
        let known: BTreeSet<String> = ["error".to_string(), "tools/docgen".to_string()].into_iter().collect();
        assert_eq!(
            resolve_use_target("crate::error::ToolError", "tools/docgen/crate_graph", &known),
            Some("error".to_string())
        );
    }

    #[test]
    fn resolve_use_target_keeps_whole_path_when_it_names_a_module() {
        let known: BTreeSet<String> = ["tools/docgen".to_string()].into_iter().collect();
        assert_eq!(
            resolve_use_target("crate::tools::docgen", "some/other", &known),
            Some("tools/docgen".to_string())
        );
    }

    #[test]
    fn resolve_use_target_super_resolves_relative_to_parent() {
        let known: BTreeSet<String> = ["tools".to_string()].into_iter().collect();
        assert_eq!(resolve_use_target("super::helper", "tools/docgen", &known), Some("tools".to_string()));
    }

    #[test]
    fn resolve_use_target_returns_none_for_external_crate() {
        let known: BTreeSet<String> = ["error".to_string()].into_iter().collect();
        assert_eq!(resolve_use_target("serde_json::Value", "tools/docgen", &known), None);
    }

    #[test]
    fn resolve_use_target_returns_none_when_no_prefix_matches() {
        let known: BTreeSet<String> = ["error".to_string()].into_iter().collect();
        assert_eq!(resolve_use_target("crate::totally::unknown::Thing", "x", &known), None);
    }

    // ── crate-level graph (Cargo.toml) ───────────────────────────────────

    #[test]
    fn crate_level_graph_includes_workspace_members_with_no_edge_when_undeclared() {
        let dir = tmp_dir("workspace-no-edge");
        write(
            &dir,
            "Cargo.toml",
            r#"
[workspace]
members = ["member-a"]

[package]
name = "root-crate"
version = "0.1.0"

[dependencies]
serde = "1"
"#,
        );
        write(
            &dir,
            "member-a/Cargo.toml",
            r#"
[package]
name = "member-a"
version = "0.1.0"

[dependencies]
log = "0.4"
"#,
        );

        let model = build_crate_graph(&dir, "src").unwrap();
        let crate_nodes: Vec<&str> =
            model.nodes.iter().filter(|n| n.kind == NodeKind::Crate).map(|n| n.id.as_str()).collect();
        assert!(crate_nodes.contains(&"root-crate"));
        assert!(crate_nodes.contains(&"member-a"));
        // Neither crate declares the other as a dependency -> zero crate edges.
        assert!(model.edges.iter().all(|e| e.kind != EdgeKind::CrateDependency));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crate_level_graph_adds_edge_for_workspace_internal_dependency() {
        let dir = tmp_dir("workspace-with-edge");
        write(
            &dir,
            "Cargo.toml",
            r#"
[workspace]
members = ["member-a"]

[package]
name = "root-crate"
version = "0.1.0"

[dependencies]
member-a = { path = "member-a" }
serde = "1"
"#,
        );
        write(&dir, "member-a/Cargo.toml", "[package]\nname = \"member-a\"\nversion = \"0.1.0\"\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(model.edges.iter().any(|e| {
            e.from == "root-crate" && e.to == "member-a" && e.kind == EdgeKind::CrateDependency
        }));
        // External dep `serde` never becomes a node or edge.
        assert!(!model.nodes.iter().any(|n| n.id == "serde"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_cargo_toml_is_a_clean_not_found_error() {
        let dir = tmp_dir("no-manifest");
        let result = build_crate_graph(&dir, "src");
        assert!(matches!(result, Err(ToolError::NotFound(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── module-level graph (mod/use scanning end to end) ─────────────────

    fn minimal_workspace(dir: &Path) {
        write(dir, "Cargo.toml", "[package]\nname = \"fixture-crate\"\nversion = \"0.1.0\"\n");
    }

    #[test]
    fn module_graph_includes_containment_edges_from_mod_decls() {
        let dir = tmp_dir("mod-containment");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod tools;\n");
        write(&dir, "src/tools/mod.rs", "pub mod docgen;\n");
        write(&dir, "src/tools/docgen.rs", "//! docgen module\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(model.nodes.iter().any(|n| n.id == "tools" && n.kind == NodeKind::Module));
        assert!(model.nodes.iter().any(|n| n.id == "tools/docgen" && n.kind == NodeKind::Module));
        assert!(model.edges.iter().any(|e| {
            e.from == "" && e.to == "tools" && e.kind == EdgeKind::ModuleContainment
        }));
        assert!(model.edges.iter().any(|e| {
            e.from == "tools" && e.to == "tools/docgen" && e.kind == EdgeKind::ModuleContainment
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn module_graph_includes_use_edges_between_real_modules() {
        let dir = tmp_dir("mod-use-edges");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\npub mod beta;\n");
        write(&dir, "src/alpha.rs", "//! alpha\n");
        write(&dir, "src/beta.rs", "use crate::alpha::Something;\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(model.edges.iter().any(|e| {
            e.from == "beta" && e.to == "alpha" && e.kind == EdgeKind::ModuleUse
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOAD-BEARING negative test: a `use` for an external crate must never
    /// produce a bogus edge to a same-named-but-unrelated module.
    #[test]
    fn module_graph_never_fabricates_an_edge_for_an_unresolvable_use() {
        let dir = tmp_dir("mod-use-unresolvable");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\n");
        write(&dir, "src/alpha.rs", "use serde::Deserialize;\nuse std::collections::HashMap;\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(model.edges.iter().filter(|e| e.kind == EdgeKind::ModuleUse).count() == 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── cycle detection ("--acyclic" ground truth) ───────────────────────

    #[test]
    fn find_cycles_reports_a_real_circular_use_dependency() {
        let dir = tmp_dir("cycle");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\npub mod beta;\n");
        write(&dir, "src/alpha.rs", "use crate::beta::Thing;\n");
        write(&dir, "src/beta.rs", "use crate::alpha::OtherThing;\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        let cycles = find_cycles(&model);
        assert!(!cycles.is_empty(), "alpha <-> beta use-cycle must be detected");
        assert!(cycles
            .iter()
            .any(|c| c.contains(&"alpha".to_string()) && c.contains(&"beta".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_cycles_empty_for_acyclic_graph() {
        let dir = tmp_dir("acyclic");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\npub mod beta;\n");
        write(&dir, "src/alpha.rs", "use crate::beta::Thing;\n");
        write(&dir, "src/beta.rs", "//! no back-reference\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(find_cycles(&model).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_cycles_never_flags_the_mod_containment_tree_as_cyclic() {
        // A deep, purely-containment tree (no `use` edges at all) must
        // never be reported as having a cycle -- containment is excluded
        // from cycle detection by construction.
        let dir = tmp_dir("containment-only");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod a;\n");
        write(&dir, "src/a/mod.rs", "pub mod b;\n");
        write(&dir, "src/a/b.rs", "//! leaf\n");

        let model = build_crate_graph(&dir, "src").unwrap();
        assert!(find_cycles(&model).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── DOT serialization + theming ──────────────────────────────────────

    #[test]
    fn to_dot_produces_valid_looking_digraph() {
        let dir = tmp_dir("dot-basic");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\n");
        write(&dir, "src/alpha.rs", "//! alpha\n");
        let model = build_crate_graph(&dir, "src").unwrap();
        let cycles = find_cycles(&model);
        let dot = to_dot(&model, &cycles, GraphTheme::Light);
        assert!(dot.starts_with("digraph crate_graph {"));
        assert!(dot.trim_end().ends_with('}'));
        assert!(dot.contains("fixture-crate"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_dot_differs_by_theme() {
        let dir = tmp_dir("dot-theme");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "//! root\n");
        let model = build_crate_graph(&dir, "src").unwrap();
        let cycles = find_cycles(&model);
        let light = to_dot(&model, &cycles, GraphTheme::Light);
        let dark = to_dot(&model, &cycles, GraphTheme::Dark);
        assert_ne!(light, dark);
        assert!(light.contains("#ffffff"));
        assert!(dark.contains("#1a1a2e"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_dot_highlights_cycle_edges_distinctly() {
        let dir = tmp_dir("dot-cycle-highlight");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\npub mod beta;\n");
        write(&dir, "src/alpha.rs", "use crate::beta::Thing;\n");
        write(&dir, "src/beta.rs", "use crate::alpha::OtherThing;\n");
        let model = build_crate_graph(&dir, "src").unwrap();
        let cycles = find_cycles(&model);
        assert!(!cycles.is_empty());
        let dot = to_dot(&model, &cycles, GraphTheme::Light);
        assert!(dot.contains("#cc3333"), "cycle edges must use the alert color");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── graph-view rendering: real environment, skip-if-unavailable ─────

    #[test]
    fn render_crate_graph_svg_always_produces_a_dot_model_with_cycles_and_note() {
        let dir = tmp_dir("render-svg");
        minimal_workspace(&dir);
        write(&dir, "src/lib.rs", "pub mod alpha;\n");
        write(&dir, "src/alpha.rs", "//! alpha\n");

        let result = render_crate_graph_svg(&dir, "src", GraphTheme::Dark).unwrap();
        assert!(!result.dot_source.is_empty());
        assert!(result.dot_source.contains("digraph crate_graph"));
        assert!(!result.note.is_empty());
        // Consistent with the DOCGEN-14/pdf skip pattern: svg and
        // engine_used are always both Some or both None.
        assert_eq!(result.svg.is_some(), result.engine_used.is_some());
        assert!(result.cycles.is_empty());

        // This sandbox has neither sfdp/neato/cargo-modules/cargo-depgraph
        // on PATH (checked before writing this module) -- assert the skip
        // path fires for real, not merely "handled in theory".
        if !engine_available("sfdp") && !engine_available("neato") {
            assert!(result.svg.is_none());
            assert!(result.note.contains("unavailable"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── versioning (DOCGEN-07 reuse) ─────────────────────────────────────

    #[test]
    fn versions_dot_always_and_svg_only_when_rendered() {
        let store = VersionStore::new();
        let skipped = CrateGraphRenderResult {
            dot_source: "digraph crate_graph {}\n".to_string(),
            svg: None,
            engine_used: None,
            cycles: vec![],
            note: "unavailable".to_string(),
        };
        let (dot_v1, svg_v1) = version_crate_graph(&store, "terminus", &skipped, "c1", "t0");
        assert_eq!(dot_v1.version, 1);
        assert!(svg_v1.is_none());

        let rendered = CrateGraphRenderResult {
            dot_source: "digraph crate_graph {}\n".to_string(),
            svg: Some("<svg/>".to_string()),
            engine_used: Some("sfdp".to_string()),
            cycles: vec![],
            note: "rendered via sfdp -Tsvg".to_string(),
        };
        let (dot_v2, svg_v2) = version_crate_graph(&store, "terminus", &rendered, "c2", "t1");
        assert_eq!(dot_v2.version, 2);
        assert_eq!(svg_v2.unwrap().version, 1);

        let dot_history = store.history(&crate_graph_dot_key("terminus"));
        assert_eq!(dot_history.len(), 2, "prior dot version must never be overwritten");
    }

    // ── real-world smoke test: this crate's own real graph ───────────────

    /// Runs `build_crate_graph` against THIS repo's own real workspace
    /// (root `terminus-rs` + member `terminus-client`) -- the actual
    /// ground-truth extraction this item ships, not a fixture. Skips
    /// gracefully if run from a packaged tarball with no `Cargo.toml`.
    #[test]
    fn real_workspace_graph_finds_both_crates_with_no_edge_between_them() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        if !repo_root.join("Cargo.toml").exists() {
            eprintln!("skipping: {} has no Cargo.toml", repo_root.display());
            return;
        }

        let model = build_crate_graph(repo_root, "src").unwrap();
        let crate_nodes: Vec<&str> =
            model.nodes.iter().filter(|n| n.kind == NodeKind::Crate).map(|n| n.id.as_str()).collect();
        assert!(crate_nodes.contains(&"terminus-rs"));
        assert!(crate_nodes.contains(&"terminus-client"));
        // Per this workspace's own documented design (Cargo.toml's own
        // comment: "terminus-client does NOT depend on terminus-rs"; and
        // the root crate has no path dep on terminus-client either) --
        // this ground-truth extraction should independently confirm zero
        // crate-level edges between them.
        assert!(!model
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::CrateDependency
                && ((e.from == "terminus-rs" && e.to == "terminus-client")
                    || (e.from == "terminus-client" && e.to == "terminus-rs"))));

        // The real module tree is large; just sanity-check a couple of
        // well-known modules and at least one real use-edge exist.
        assert!(model.nodes.iter().any(|n| n.id == "tools/docgen" && n.kind == NodeKind::Module));
        assert!(model.edges.iter().any(|e| e.kind == EdgeKind::ModuleUse));
    }
}
