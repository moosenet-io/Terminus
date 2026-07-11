//! DOCGEN-14: rich wiki information architecture -- auto-generated nav/
//! sidebar, a `[[wikilink]]` backlink index, and a force-directed graph-view
//! (S95, Plane TERM-165).
//!
//! ## Scope and shape (why this isn't just another `render/*.rs` target)
//! Every other renderer in this module (`markdown`, `wiki`, `pdf`,
//! `obsidian`, `notion`, `blog`) renders ONE piece of already-generated
//! content, described by a single [`super::RenderContext`], into one format.
//! Nav/sidebar, backlinks, and the graph view are inherently *whole-vault*
//! concerns -- a sidebar groups every note by module, a backlink index needs
//! every note's `[[wikilink]]`s to resolve cross-references, and a graph
//! needs every node and edge at once. So this module operates over a
//! *slice* of notes ([`VaultNoteSummary`]), not a single [`super::RenderContext`],
//! and does not plug into [`super::render_all`] (which is a one-target-at-a-
//! time driver) or [`super::config::DocTargetType`] (whose variants are all
//! single-content render targets). A future item can wire a whole-vault pass
//! through this module's public functions once a caller assembles the note
//! slice; this item ships the engine + tested logic.
//!
//! ## REUSE, not reimplementation (RECONCILIATION CONSTRAINTS)
//! Built entirely on `crate::scribe::vault`'s existing pure primitives --
//! [`build_wikilink`], [`slugify`], [`note_path`], `NoteFrontmatter`/
//! `render_note` -- the same wikilink convention [`super::wiki`] and
//! [`super::obsidian`] already use. No new markup/frontmatter dialect is
//! invented here.
//!
//! ## WRITE-MODEL INVERSION (unchanged from the rest of `render/`)
//! Every function here is pure (no filesystem, network, or subprocess I/O)
//! EXCEPT [`render_graph_view`], which -- exactly like [`super::pdf`]'s
//! "renderer unavailable" pattern -- shells out ONLY to a *local* graph
//! layout binary (`sfdp`/`neato`) to rasterize the already-computed DOT
//! source to SVG, and only ever returns the result; it never writes a file,
//! commits, or pushes anything. The calling harness decides where any of
//! this lands.
//!
//! ## Graph SVG: render-if-available, else produce-and-test the model
//! Per the item's guidance (mirroring [`super::pdf`]'s skip-if-unavailable
//! pattern): the DOT source -- the adjacency list serialized as GraphViz DOT,
//! the diffable artifact -- is ALWAYS produced and always tested, regardless
//! of environment. Rasterizing it to SVG additionally shells out to `sfdp`
//! (preferred, force-directed) or `neato` (fallback) if either binary is on
//! `PATH`; if neither is present, [`render_graph_view`] returns the DOT
//! source with `svg: None` and a clear note, exactly mirroring
//! [`super::pdf::render`]'s skip shape. `d2` support is DEFERRED (checked
//! for presence, reported in the skip note, but no D2-language emission is
//! implemented -- D2's own diagram language is a distinct source format
//! from DOT, and adding a second emitter is out of scope for this item; the
//! DOT/GraphViz path is the shipped one). This mirrors how a Starlight site
//! target is also deferred below -- both are noted, neither is half-built.
//!
//! ## Starlight site target (deferred)
//! Emitting a full Starlight-project structure (an Astro+Starlight scaffold
//! with the nav tree as `astro.config` sidebar entries) is NOT implemented
//! here -- it is a materially larger, framework-specific output shape than
//! "cheap," and the item's own guidance says defer rather than half-build
//! it. The pieces this item DOES ship ([`NavTree`], [`BacklinkIndex`],
//! [`GraphModel`]) are exactly the inputs a future Starlight renderer would
//! consume, so nothing here blocks that follow-up.
//!
//! ## Theme-aware
//! [`to_dot`] takes a [`GraphTheme`] and renders distinct background/node/
//! edge colors for `Light` vs `Dark`, so a rendered SVG (when a layout
//! engine is available) matches the caller's theme -- see
//! `to_dot_differs_by_theme` in this module's tests.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::scribe::vault::{build_wikilink, note_path, slugify, NoteType};

// ─── Vault note summary (the whole-vault input shape) ───────────────────────

/// The minimal per-note facts nav/backlinks/graph need: enough to place the
/// note in the sidebar (`module`, `note_type`), enough to identify it as a
/// wikilink target (`title`), and its raw body so `[[wikilink]]`s can be
/// parsed out of it. Deliberately NOT [`super::RenderContext`] -- that type
/// carries a single already-*rendered* artifact's content plus source-commit/
/// generated-at provenance; a vault-wide pass only needs these four fields
/// per note.
#[derive(Debug, Clone)]
pub struct VaultNoteSummary {
    pub title: String,
    pub module: String,
    pub note_type: NoteType,
    /// Raw note body (Markdown, may contain `[[wikilink]]`s).
    pub content: String,
}

// ─── Wikilink parsing (pure) ─────────────────────────────────────────────────

/// Extract every `[[Target]]` (and `[[Target|Alias]]`, taking `Target`) from
/// `content`, in the order they appear. An unterminated `[[` (no matching
/// `]]` before end of input) is ignored rather than causing a panic or
/// swallowing the rest of the content -- the scan simply stops looking for a
/// close once none exists, whatever came before is preserved unaffected.
pub fn parse_wikilinks(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(rel_end) = content[i + 2..].find("]]") {
                let inner = &content[i + 2..i + 2 + rel_end];
                let target = inner.split('|').next().unwrap_or(inner).trim();
                if !target.is_empty() {
                    out.push(target.to_string());
                }
                i += 2 + rel_end + 2;
                continue;
            } else {
                // Unterminated -- nothing further in `content` can close it.
                break;
            }
        }
        i += 1;
    }
    out
}

// ─── Nav / sidebar ────────────────────────────────────────────────────────────

/// One note's sidebar entry: its title (rendered as a wikilink) and the
/// vault-relative path it would live at, computed via
/// `crate::scribe::vault::note_path` (pure -- no filesystem access, matches
/// [`super::obsidian`]'s "intended path is informational only" convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavEntry {
    pub title: String,
    pub note_type: NoteType,
    pub vault_path: PathBuf,
}

/// A module's group of notes in the sidebar, sorted by title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavGroup {
    pub module: String,
    pub entries: Vec<NavEntry>,
}

/// The full auto-generated sidebar/nav: one [`NavGroup`] per module, modules
/// sorted alphabetically, notes within a module sorted alphabetically by
/// title -- deterministic regardless of input order, so regenerating nav
/// from an unchanged vault always produces byte-identical output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavTree {
    pub groups: Vec<NavGroup>,
}

/// Build the nav tree by grouping `notes` by `module`. `vault_root` is only
/// used to compute each entry's informational `vault_path` (pure, per
/// `note_path`'s own contract) -- pass `Path::new(".")` when the caller has
/// no real vault root handy, matching [`super::obsidian::render`]'s
/// convention.
pub fn build_nav_tree(vault_root: &Path, notes: &[VaultNoteSummary]) -> NavTree {
    let mut by_module: BTreeMap<String, Vec<NavEntry>> = BTreeMap::new();
    for note in notes {
        let vault_path = note_path(vault_root, note.note_type, &note.module, &note.title);
        by_module.entry(note.module.clone()).or_default().push(NavEntry {
            title: note.title.clone(),
            note_type: note.note_type,
            vault_path,
        });
    }

    let mut groups: Vec<NavGroup> = by_module
        .into_iter()
        .map(|(module, mut entries)| {
            entries.sort_by(|a, b| a.title.cmp(&b.title));
            NavGroup { module, entries }
        })
        .collect();
    groups.sort_by(|a, b| a.module.cmp(&b.module));

    NavTree { groups }
}

impl NavTree {
    /// Render the sidebar as a Markdown nested list, each note rendered as a
    /// real `[[wikilink]]` (via [`build_wikilink`]) -- an Obsidian-openable
    /// vault treats this list as clickable navigation with zero extra
    /// tooling, matching the wiki's existing `[[wikilink]]` convention.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Navigation\n\n");
        for group in &self.groups {
            out.push_str(&format!("## {}\n\n", group.module));
            for entry in &group.entries {
                out.push_str(&format!(
                    "- {} ({})\n",
                    build_wikilink(&entry.title),
                    entry.note_type.as_str()
                ));
            }
            out.push('\n');
        }
        out
    }
}

// ─── Backlink index ───────────────────────────────────────────────────────────

/// `target title -> set of source titles that link to it`, built by parsing
/// every note's `[[wikilink]]`s. A note linking to ITSELF never counts as
/// its own backlink (a self-link isn't a cross-reference), and a link to a
/// title with no matching note (a "dangling" link) is still recorded -- the
/// target simply has no corresponding [`VaultNoteSummary`], which
/// [`build_graph_model`] surfaces as an orphan node rather than a crash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BacklinkIndex {
    pub backlinks: BTreeMap<String, BTreeSet<String>>,
}

impl BacklinkIndex {
    pub fn count(&self, title: &str) -> usize {
        self.backlinks.get(title).map(|s| s.len()).unwrap_or(0)
    }

    /// Render a per-note "linked from" section, e.g. for appending to a
    /// wiki/obsidian page -- omits notes with zero backlinks rather than
    /// printing an empty section for every note in the vault.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Backlinks\n\n");
        for (target, sources) in &self.backlinks {
            if sources.is_empty() {
                continue;
            }
            out.push_str(&format!("## {}\n\n", target));
            for source in sources {
                out.push_str(&format!("- {}\n", build_wikilink(source)));
            }
            out.push('\n');
        }
        out
    }
}

pub fn build_backlink_index(notes: &[VaultNoteSummary]) -> BacklinkIndex {
    let mut backlinks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for note in notes {
        for target in parse_wikilinks(&note.content) {
            if target == note.title {
                continue; // self-link excluded
            }
            backlinks.entry(target).or_default().insert(note.title.clone());
        }
    }
    BacklinkIndex { backlinks }
}

// ─── Graph model (adjacency list -- the diffable artifact) ──────────────────

/// One graph node: its title and its "size" -- the backlink count, per the
/// item's instruction that node size reflects backlink count (a note many
/// others reference renders larger in the eventual force-directed layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub title: String,
    pub size: usize,
}

/// The full graph: every known title (real notes AND dangling link targets)
/// as a node, plus every distinct, non-self-referential `(source, target)`
/// wikilink edge -- sorted, so the model is deterministic and the DOT
/// serialization ([`to_dot`]) diffs cleanly across regenerations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(String, String)>,
}

pub fn build_graph_model(notes: &[VaultNoteSummary]) -> GraphModel {
    let backlinks = build_backlink_index(notes);

    let mut titles: BTreeSet<String> = notes.iter().map(|n| n.title.clone()).collect();
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for note in notes {
        for target in parse_wikilinks(&note.content) {
            if target == note.title {
                continue; // self-loop excluded
            }
            titles.insert(target.clone());
            edges.insert((note.title.clone(), target));
        }
    }

    let nodes = titles
        .into_iter()
        .map(|title| {
            let size = backlinks.count(&title);
            GraphNode { title, size }
        })
        .collect();

    GraphModel { nodes, edges: edges.into_iter().collect() }
}

// ─── DOT serialization + theming ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTheme {
    Light,
    Dark,
}

struct ThemeColors {
    bgcolor: &'static str,
    node_fill: &'static str,
    node_font: &'static str,
    edge_color: &'static str,
}

impl GraphTheme {
    fn colors(self) -> ThemeColors {
        match self {
            GraphTheme::Light => ThemeColors {
                bgcolor: "#ffffff",
                node_fill: "#e8ecff",
                node_font: "#1a1a2e",
                edge_color: "#999999",
            },
            GraphTheme::Dark => ThemeColors {
                bgcolor: "#1a1a2e",
                node_fill: "#2d2d55",
                node_font: "#eef2ff",
                edge_color: "#7a7a9a",
            },
        }
    }
}

/// Escape a string for use inside a DOT double-quoted attribute value.
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Serialize `model` as GraphViz DOT source (a `digraph`, since wikilinks
/// are directional), theming background/node/edge colors per `theme`, and
/// scaling each node's `width`/`height` by its backlink count (`size`) --
/// this is the diffable, always-produced artifact regardless of whether a
/// layout engine is available to rasterize it. Node identifiers are
/// [`slugify`]d (DOT bare identifiers can't safely hold arbitrary titles),
/// with the original title kept intact as the node's `label`.
pub fn to_dot(model: &GraphModel, theme: GraphTheme) -> String {
    let c = theme.colors();
    let mut out = String::new();
    out.push_str("digraph wiki_graph {\n");
    out.push_str("  layout=sfdp;\n");
    out.push_str("  overlap=false;\n");
    out.push_str(&format!("  bgcolor=\"{}\";\n", c.bgcolor));
    out.push_str(&format!(
        "  node [style=filled, shape=ellipse, fillcolor=\"{}\", fontcolor=\"{}\", color=\"{}\"];\n",
        c.node_fill, c.node_font, c.edge_color
    ));
    out.push_str(&format!("  edge [color=\"{}\"];\n\n", c.edge_color));

    for node in &model.nodes {
        // Base width 0.6in, growing with backlink count, capped so a very
        // hot note doesn't blow out the layout.
        let width = (0.6 + node.size as f64 * 0.15).min(3.0);
        out.push_str(&format!(
            "  \"{}\" [label=\"{}\", width={:.2}, height={:.2}];\n",
            slugify(&node.title),
            dot_escape(&node.title),
            width,
            width * 0.6,
        ));
    }
    out.push('\n');
    for (from, to) in &model.edges {
        out.push_str(&format!("  \"{}\" -> \"{}\";\n", slugify(from), slugify(to)));
    }
    out.push_str("}\n");
    out
}

// ─── Graph-view rendering: SVG-if-available, else model+note (PDF pattern) ──

/// Which local layout binary, if any, is on `PATH`. Checked by attempting to
/// spawn `<engine> -V` -- spawn success (regardless of exit code; GraphViz's
/// `-V` prints its version to stderr and some builds exit non-zero) is
/// enough to know the binary is invocable. A spawn failure (binary not
/// found) means unavailable.
/// `pub(crate)`: reused by `crate::tools::docgen::crate_graph` (DOCGEN-12)
/// for its own sfdp/neato SVG-if-available rasterization, rather than a
/// second copy of this exact spawn-and-probe logic.
pub(crate) fn engine_available(engine: &str) -> bool {
    Command::new(engine).arg("-V").stdout(Stdio::null()).stderr(Stdio::null()).output().is_ok()
}

/// `pub(crate)`: see [`engine_available`]'s doc comment -- same reuse by
/// `crate::tools::docgen::crate_graph`.
pub(crate) fn run_engine(engine: &str, dot_source: &str) -> Option<String> {
    let mut child = Command::new(engine)
        .arg("-Tsvg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(dot_source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if output.status.success() && !output.stdout.is_empty() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// The result of attempting a graph-view render: the DOT source is ALWAYS
/// present (the diffable model), `svg` is `Some` only when a local layout
/// engine actually rasterized it, and `engine_used`/`note` explain which
/// path was taken -- mirrors [`super::pdf::render`]'s skip shape, but keeps
/// the model content alongside the note (unlike [`super::RenderedArtifact`],
/// which is deliberately content-XOR-note; this whole-vault result always
/// has content, and additionally notes whether rasterization happened).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRenderResult {
    pub dot_source: String,
    pub svg: Option<String>,
    pub engine_used: Option<String>,
    pub note: String,
}

/// Build the graph model from `notes` and attempt to rasterize it to SVG via
/// `sfdp` (preferred -- force-directed, per the item's guidance) then
/// `neato` (fallback). If neither is on `PATH`, returns the DOT model with
/// `svg: None` and a clear note (also mentioning `d2` if that binary is
/// present, since D2-language emission itself is deferred -- see the module
/// doc comment).
pub fn render_graph_view(notes: &[VaultNoteSummary], theme: GraphTheme) -> GraphRenderResult {
    let model = build_graph_model(notes);
    let dot_source = to_dot(&model, theme);

    for engine in ["sfdp", "neato"] {
        if engine_available(engine) {
            if let Some(svg) = run_engine(engine, &dot_source) {
                return GraphRenderResult {
                    dot_source,
                    svg: Some(svg),
                    engine_used: Some(engine.to_string()),
                    note: format!("rendered via {engine} -Tsvg"),
                };
            }
        }
    }

    let d2_hint = if engine_available("d2") {
        " (a `d2` binary was found, but D2-language emission is not implemented in this build -- \
the DOT/GraphViz path is the shipped one; see module docs)"
    } else {
        ""
    };
    GraphRenderResult {
        dot_source,
        svg: None,
        engine_used: None,
        note: format!(
            "graph SVG rasterization unavailable in this environment (no `sfdp`/`neato` on PATH){d2_hint} \
-- the DOT graph model was still produced and is fully diffable/testable; node sizes reflect \
backlink counts, edges reflect [[wikilink]] references."
        ),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, module: &str, content: &str) -> VaultNoteSummary {
        VaultNoteSummary {
            title: title.to_string(),
            module: module.to_string(),
            note_type: NoteType::Wiki,
            content: content.to_string(),
        }
    }

    // ── Wikilink parsing ────────────────────────────────────────────────

    #[test]
    fn parse_wikilinks_extracts_simple_targets() {
        let links = parse_wikilinks("See [[Widget]] and [[Gadget]] for details.");
        assert_eq!(links, vec!["Widget".to_string(), "Gadget".to_string()]);
    }

    #[test]
    fn parse_wikilinks_takes_target_before_pipe_alias() {
        let links = parse_wikilinks("See [[Widget Module|the widget]].");
        assert_eq!(links, vec!["Widget Module".to_string()]);
    }

    #[test]
    fn parse_wikilinks_ignores_unterminated_brackets() {
        let links = parse_wikilinks("Broken [[Widget and more text with no close");
        assert!(links.is_empty());
    }

    #[test]
    fn parse_wikilinks_no_links_returns_empty() {
        assert!(parse_wikilinks("Plain text, no links here.").is_empty());
    }

    #[test]
    fn parse_wikilinks_ignores_empty_brackets() {
        assert!(parse_wikilinks("[[]]").is_empty());
    }

    // ── Nav / sidebar ───────────────────────────────────────────────────

    #[test]
    fn nav_tree_groups_by_module_sorted() {
        let notes = vec![
            note("Zeta", "moduleb", "body"),
            note("Alpha", "modulea", "body"),
            note("Beta", "modulea", "body"),
        ];
        let tree = build_nav_tree(Path::new("."), &notes);
        assert_eq!(tree.groups.len(), 2);
        assert_eq!(tree.groups[0].module, "modulea");
        assert_eq!(tree.groups[1].module, "moduleb");
        // Within a module, notes are sorted by title.
        let titles: Vec<_> = tree.groups[0].entries.iter().map(|e| e.title.clone()).collect();
        assert_eq!(titles, vec!["Alpha".to_string(), "Beta".to_string()]);
    }

    #[test]
    fn nav_tree_to_markdown_contains_wikilinks_and_module_headers() {
        let notes = vec![note("Widget", "core", "body")];
        let tree = build_nav_tree(Path::new("."), &notes);
        let md = tree.to_markdown();
        assert!(md.contains("## core"));
        assert!(md.contains("[[Widget]]"));
    }

    #[test]
    fn nav_entry_vault_path_matches_note_path_convention() {
        let notes = vec![note("Widget", "core", "body")];
        let tree = build_nav_tree(Path::new("/vault"), &notes);
        let entry = &tree.groups[0].entries[0];
        assert_eq!(entry.vault_path, note_path(Path::new("/vault"), NoteType::Wiki, "core", "Widget"));
    }

    // ── Backlink index ──────────────────────────────────────────────────

    #[test]
    fn backlink_index_builds_correct_adjacency() {
        let notes = vec![
            note("A", "m", "links to [[B]] and [[C]]"),
            note("B", "m", "links to [[C]]"),
            note("C", "m", "no links here"),
        ];
        let idx = build_backlink_index(&notes);
        assert_eq!(idx.count("C"), 2); // A and B both link to C
        assert_eq!(idx.count("B"), 1); // A links to B
        assert_eq!(idx.count("A"), 0); // nothing links to A
        assert!(idx.backlinks["C"].contains("A"));
        assert!(idx.backlinks["C"].contains("B"));
    }

    #[test]
    fn backlink_index_excludes_self_links() {
        let notes = vec![note("A", "m", "self-references [[A]] and links [[B]]")];
        let idx = build_backlink_index(&notes);
        assert_eq!(idx.count("A"), 0, "a self-link must never count as a backlink");
        assert_eq!(idx.count("B"), 1);
    }

    #[test]
    fn backlink_index_records_dangling_links_without_crashing() {
        let notes = vec![note("A", "m", "links to [[NoSuchNote]]")];
        let idx = build_backlink_index(&notes);
        assert_eq!(idx.count("NoSuchNote"), 1);
    }

    #[test]
    fn backlink_markdown_omits_notes_with_zero_backlinks() {
        let notes = vec![note("A", "m", "links to [[B]]"), note("B", "m", "no links")];
        let idx = build_backlink_index(&notes);
        let md = idx.to_markdown();
        assert!(md.contains("## B"));
        assert!(!md.contains("## A"), "A has zero incoming backlinks, must not get a section");
    }

    // ── Graph model ──────────────────────────────────────────────────────

    #[test]
    fn graph_model_node_size_equals_backlink_count() {
        let notes = vec![
            note("A", "m", "links to [[C]]"),
            note("B", "m", "links to [[C]]"),
            note("C", "m", "no links"),
        ];
        let model = build_graph_model(&notes);
        let c = model.nodes.iter().find(|n| n.title == "C").unwrap();
        assert_eq!(c.size, 2);
        let a = model.nodes.iter().find(|n| n.title == "A").unwrap();
        assert_eq!(a.size, 0);
    }

    #[test]
    fn graph_model_includes_dangling_targets_as_orphan_nodes() {
        let notes = vec![note("A", "m", "links to [[Ghost]]")];
        let model = build_graph_model(&notes);
        assert!(model.nodes.iter().any(|n| n.title == "Ghost"));
        assert!(model.edges.contains(&("A".to_string(), "Ghost".to_string())));
    }

    #[test]
    fn graph_model_excludes_self_loop_edges() {
        let notes = vec![note("A", "m", "links to [[A]] and [[B]]")];
        let model = build_graph_model(&notes);
        assert!(!model.edges.contains(&("A".to_string(), "A".to_string())));
        assert!(model.edges.contains(&("A".to_string(), "B".to_string())));
    }

    #[test]
    fn graph_model_dedupes_repeated_edges() {
        let notes = vec![note("A", "m", "[[B]] and again [[B]] and once more [[B]]")];
        let model = build_graph_model(&notes);
        let count = model.edges.iter().filter(|(f, t)| f == "A" && t == "B").count();
        assert_eq!(count, 1, "a repeated wikilink to the same target must produce one edge, not N");
    }

    // ── DOT serialization + theming ─────────────────────────────────────

    #[test]
    fn to_dot_produces_valid_looking_digraph_with_nodes_and_edges() {
        let notes = vec![note("A", "m", "links to [[B]]"), note("B", "m", "no links")];
        let model = build_graph_model(&notes);
        let dot = to_dot(&model, GraphTheme::Light);
        assert!(dot.starts_with("digraph wiki_graph {"));
        assert!(dot.trim_end().ends_with('}'));
        assert!(dot.contains("label=\"A\""));
        assert!(dot.contains("label=\"B\""));
        assert!(dot.contains("->"));
    }

    #[test]
    fn to_dot_escapes_embedded_quotes_in_titles() {
        let notes = vec![note("A \"Quoted\" Title", "m", "no links")];
        let model = build_graph_model(&notes);
        let dot = to_dot(&model, GraphTheme::Light);
        assert!(dot.contains("A \\\"Quoted\\\" Title"));
    }

    #[test]
    fn to_dot_differs_by_theme() {
        let notes = vec![note("A", "m", "no links")];
        let model = build_graph_model(&notes);
        let light = to_dot(&model, GraphTheme::Light);
        let dark = to_dot(&model, GraphTheme::Dark);
        assert_ne!(light, dark);
        assert!(light.contains("#ffffff"));
        assert!(dark.contains("#1a1a2e"));
    }

    #[test]
    fn to_dot_scales_node_width_by_backlink_count() {
        let notes = vec![
            note("Hot", "m", "no links"),
            note("A", "m", "links to [[Hot]]"),
            note("B", "m", "links to [[Hot]]"),
            note("C", "m", "links to [[Hot]]"),
            note("Cold", "m", "no links"),
        ];
        let model = build_graph_model(&notes);
        let dot = to_dot(&model, GraphTheme::Light);
        // "Hot" (3 backlinks) must get a strictly larger width than "Cold" (0).
        let hot_line = dot.lines().find(|l| l.contains("label=\"Hot\"")).unwrap();
        let cold_line = dot.lines().find(|l| l.contains("label=\"Cold\"")).unwrap();
        let extract_width = |line: &str| -> f64 {
            let idx = line.find("width=").unwrap() + "width=".len();
            line[idx..].split(',').next().unwrap().parse().unwrap()
        };
        assert!(extract_width(hot_line) > extract_width(cold_line));
    }

    // ── Graph-view rendering (real environment, no mocking) ──────────────

    #[test]
    fn engine_available_returns_false_for_a_nonexistent_binary() {
        assert!(!engine_available("definitely-not-a-real-graphviz-binary-xyz123"));
    }

    #[test]
    fn render_graph_view_always_produces_a_dot_model() {
        let notes = vec![note("A", "m", "links to [[B]]"), note("B", "m", "no links")];
        let result = render_graph_view(&notes, GraphTheme::Light);
        assert!(!result.dot_source.is_empty());
        assert!(result.dot_source.contains("digraph wiki_graph"));
        // Whether or not a layout engine is present, the note always
        // explains what happened, and svg/engine_used are consistent
        // (both Some or both None).
        assert!(!result.note.is_empty());
        assert_eq!(result.svg.is_some(), result.engine_used.is_some());
    }

    #[test]
    fn render_graph_view_empty_vault_still_produces_a_valid_empty_graph() {
        let result = render_graph_view(&[], GraphTheme::Dark);
        assert!(result.dot_source.contains("digraph wiki_graph"));
    }
}
