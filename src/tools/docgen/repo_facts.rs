//! DGRICH-01: the deterministic `RepoFacts` grounding layer (Pass 0 of the
//! rich, KG-grounded doc generator design — `fable-docgen-redesign.md` §2).
//!
//! Everything in this module is a **pure function of (an injected graph
//! handle, a checkout path, a project id)** — zero LLM calls, zero network
//! I/O beyond reading local files. `RepoFacts` is the grounding every later
//! pass (identity, per-subsystem pages, guides, the derived architecture
//! diagram) consumes instead of the single thin feat-diff prompt the old
//! engine handed a model (see the design doc's §0 diagnosis).
//!
//! ## Sources, in order (design §2 Pass 0)
//! 1. Scale + hotspots — node/edge counts, `by_kind`, top-20 PageRank.
//! 2. Subsystem rollup — group graph nodes by top-level path prefix, keep
//!    the ones that clear the §1.2 selection rule, fold the rest into
//!    `misc`.
//! 3. Subsystem edge matrix — `calls` edges aggregated into cross-subsystem
//!    weighted directed counts (this is [`SubsystemGraph`], DGRICH-04's
//!    diagram input).
//! 4. Entry points — `[[bin]]` targets + workspace members + `main`/`serve`/
//!    `register_all`-shaped symbols.
//! 5. Config surface — env-var NAMES only (never values) from
//!    `config.rs`-shaped modules.
//! 6. Prose anchors — Cargo.toml `description` + crate-root `//!` docs +
//!    per-subsystem module `//!` docs.
//! 7. Old README — parsed into sections via the existing
//!    [`super::preserve::split_old_sections`], labeled legacy claims to
//!    verify, not truth.
//!
//! ## Graph access is native, not a second sanctioned path
//! [`GraphSource`] wraps `crate::scribe::graph::{store, rank}` directly —
//! the same functions the MCP `kg_*` tools in `crate::scribe::graph::tools`
//! wrap — never an HTTP/MCP hop. The trait exists solely so tests can inject
//! a fixture graph instead of a real [`crate::scribe::graph::GraphStore`];
//! it adds no alternate way to reach the graph in production (there is
//! exactly one non-test impl, [`AtlasGraphSource`]).
//!
//! ## Fallback
//! When the graph store has no entry for a project (`found: false` in
//! `kg_stats` terms), [`build_repo_facts`] degrades to sources 4–7 only,
//! sets `kg_grounded: false`, and never fabricates a KG-derived number —
//! callers (the landing/fact-row assembly, DGRICH-05) must omit those
//! numbers rather than invent them.
//!
//! ## PII
//! [`RepoFacts::identity_slice`] and [`RepoFacts::subsystem_slice`] are the
//! only way any of this content reaches a prompt. Both serialize their
//! subset to JSON and then run it through [`super::pii_gate::sweep_input`]
//! before returning — the same unconditional pre-inference gate every other
//! docgen input passes through (S1: this content can reach a mirror).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use crate::error::ToolError;
use crate::scribe::graph::{pagerank, EdgeKind, KgNode, KnowledgeGraph, NodeKind};
use crate::scribe::inspect::{self, InspectionWorktree};

use super::pii_gate::sweep_input;
use super::preserve::split_old_sections;

// ---------------------------------------------------------------------------
// Graph handle — injectable for tests, native (no MCP/HTTP hop) in production
// ---------------------------------------------------------------------------

/// The seam between `RepoFacts` and the Atlas knowledge graph. Exactly one
/// production implementation ([`AtlasGraphSource`]); tests inject
/// [`FixtureGraphSource`] instead. This is not a second sanctioned door to
/// the graph — both impls ultimately read the same on-disk store
/// (`SCRIBE_KG_STORE_DIR`) that `crate::scribe::graph::tools`'s `kg_*` MCP
/// tools read; this trait only exists so the deterministic builder below
/// never has to spawn a real filesystem-backed store to be unit-tested.
pub trait GraphSource: Send + Sync {
    /// Load `project_id`'s current graph, or `None` if the project has no
    /// entry in the store (the `kg_grounded: false` case). A store-level I/O
    /// failure is a real `Err`; a project simply never having been indexed
    /// is `Ok(None)`, never an error.
    fn load_graph(&self, project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError>;
}

/// The real, native [`GraphSource`]: reads straight from
/// `crate::scribe::graph::store::GraphStore` (the same `SCRIBE_KG_STORE_DIR`-
/// rooted store `kg_*` MCP tools use), no HTTP/MCP hop.
pub struct AtlasGraphSource {
    store: crate::scribe::graph::GraphStore,
}

impl AtlasGraphSource {
    /// Build from `ScribeConfig::from_env()` — the same config source
    /// `crate::scribe::graph::tools` uses for every `kg_*` call.
    pub fn from_env() -> Self {
        Self {
            store: crate::scribe::graph::GraphStore::from_config(&crate::scribe::ScribeConfig::from_env()),
        }
    }

    /// Build from an explicit store (e.g. one rooted at a non-default
    /// directory in an integration test).
    pub fn new(store: crate::scribe::graph::GraphStore) -> Self {
        Self { store }
    }
}

impl GraphSource for AtlasGraphSource {
    fn load_graph(&self, project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
        self.store.load(project_id)
    }
}

/// A canned graph for tests — never used outside `#[cfg(test)]` call sites.
/// Test-only: not compiled into (nor re-exported for) production, so there is
/// exactly ONE non-test graph source ([`AtlasGraphSource`]) and no alternate
/// production path can inject an arbitrary graph or force `kg_grounded:false`.
#[cfg(test)]
pub(crate) struct FixtureGraphSource(pub KnowledgeGraph);

#[cfg(test)]
impl GraphSource for FixtureGraphSource {
    fn load_graph(&self, _project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
        Ok(Some(self.0.clone()))
    }
}

/// A [`GraphSource`] that always reports "no graph for this project" — the
/// `kg_grounded: false` degradation path, exercised without needing a real
/// missing-store lookup. Test-only (see [`FixtureGraphSource`]).
#[cfg(test)]
pub(crate) struct NoGraphSource;

#[cfg(test)]
impl GraphSource for NoGraphSource {
    fn load_graph(&self, _project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Selection-rule constants (design §1.2)
// ---------------------------------------------------------------------------

/// A subsystem prefix must have at least this many nodes to be considered
/// for its own reference page, UNLESS the repo is small enough that 1% of
/// its nodes is larger (see [`selection_threshold`]).
const MIN_SUBSYSTEM_NODES: usize = 30;

/// At most this many derived subsystems get their own page; the rest fold
/// into the synthetic `misc` inventory.
const MAX_SUBSYSTEMS: usize = 16;

/// How many top-PageRank nodes make the repo-scale hotspot list.
const HOTSPOT_LIMIT: usize = 20;

/// How many top-PageRank symbols represent a subsystem in its rollup entry.
const TOP_SYMBOLS_PER_SUBSYSTEM: usize = 8;

/// Cap on how many leading lines of a module's `//!` docs are kept as a
/// prose anchor (design §2 source 6: "~40 lines each").
const PROSE_ANCHOR_LINE_CAP: usize = 40;

const MISC_SUBSYSTEM_NAME: &str = "misc";

/// `max(MIN_SUBSYSTEM_NODES, 1% of total_nodes)` — the §1.2 selection
/// threshold a subsystem prefix's node count must clear to be a *candidate*
/// (still subject to the top-[`MAX_SUBSYSTEMS`]-by-score cap on top of that).
fn selection_threshold(total_nodes: usize) -> usize {
    let one_percent = (total_nodes as f64 * 0.01).ceil() as usize;
    MIN_SUBSYSTEM_NODES.max(one_percent)
}

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

/// A symbol reference used both for repo-scale hotspots and a subsystem's
/// top symbols: enough to name and locate it without re-touching the graph.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolRef {
    pub id: String,
    pub kind: &'static str,
    pub path: String,
    pub rank: f32,
}

impl SymbolRef {
    fn from_node(n: &KgNode) -> Self {
        SymbolRef { id: n.id.clone(), kind: n.kind.as_str(), path: n.path.clone(), rank: n.rank }
    }
}

/// Repo-scale facts: node/edge counts, the kind breakdown, and the top
/// PageRank hotspots — the `kg_stats`-equivalent source (design §2 source 1).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RepoScale {
    pub node_count: usize,
    pub edge_count: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub hotspots: Vec<SymbolRef>,
}

/// One derived subsystem: a top-level path-prefix rollup of graph nodes
/// (design §1.2). `is_misc` marks the single synthetic catch-all subsystem
/// folded remainder prefixes land in; every other subsystem is a real,
/// named prefix that cleared the selection rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Subsystem {
    pub name: String,
    pub source_dir: String,
    pub node_count: usize,
    pub kind_breakdown: BTreeMap<String, usize>,
    pub top_symbols: Vec<SymbolRef>,
    /// Sum of member nodes' PageRank — the score half of the `node_count *
    /// aggregate_rank` ranking rule used to pick the kept-16.
    pub aggregate_rank: f32,
    pub is_misc: bool,
}

/// One directed, weighted cross-subsystem call edge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubsystemEdge {
    pub from: String,
    pub to: String,
    pub weight: u32,
}

/// The subsystem-level call graph: `calls` edges aggregated one level up
/// from raw nodes into cross-subsystem weighted directed counts. This is
/// the exact type DGRICH-04's `subsystem_architecture_mermaid_source`
/// consumes, so its shape is kept intentionally small and stable.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SubsystemGraph {
    pub edges: Vec<SubsystemEdge>,
}

impl SubsystemGraph {
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// The aggregated weight of the `from -> to` edge, or `0` if there is no
    /// cross-call between those two subsystems.
    pub fn weight_between(&self, from: &str, to: &str) -> u32 {
        self.edges
            .iter()
            .find(|e| e.from == from && e.to == to)
            .map(|e| e.weight)
            .unwrap_or(0)
    }

    /// All distinct subsystem names appearing as either endpoint of an edge.
    pub fn participants(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for e in &self.edges {
            set.insert(e.from.clone());
            set.insert(e.to.clone());
        }
        set.into_iter().collect()
    }
}

/// A `[[bin]]` target read from Cargo.toml.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BinTarget {
    pub name: String,
    pub path: String,
}

/// Entry points: real binaries, workspace members, and `main`/`serve`/
/// `register_all`-shaped symbols — the grounding for guides/getting-started
/// (design §2 source 4) and the honest-command lint (DGRICH-02) downstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EntryPoints {
    pub bin_targets: Vec<BinTarget>,
    pub workspace_members: Vec<String>,
    /// Names/ids of `main`/`serve`/`register_all`-shaped symbols found
    /// either in the graph (when `kg_grounded`) or via a checkout scan
    /// (fallback).
    pub entrypoint_symbols: Vec<String>,
    /// Best-effort count of `registry.register(...)`-shaped call sites in
    /// `src/registry.rs` — the "~53 MCP tools" style fact. `None` when the
    /// file isn't present in the checkout.
    pub registered_tool_count: Option<usize>,
}

/// The real configuration inventory: env-var **NAMES only, never values**
/// (design §2 source 5 / DGRICH-01 acceptance criterion).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConfigSurface {
    pub env_var_names: Vec<String>,
}

/// Cargo.toml `description` + crate-root `//!` docs + per-subsystem module
/// `//!` docs (design §2 source 6) — the highest-quality identity text in
/// the repo, and (per the design's diagnosis) the thing the old engine
/// never showed a model at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProseAnchors {
    pub crate_description: Option<String>,
    pub crate_root_docs: Vec<String>,
    pub subsystem_docs: BTreeMap<String, Vec<String>>,
}

/// One top-level (`## `) section recovered from the OLD README, labeled a
/// legacy claim to verify against the code — never trusted as ground truth
/// on its own (design §2 source 7).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LegacySection {
    pub heading: String,
    pub body: String,
}

/// DGDG-02: the existing, already-checked-out `docs/` tree at `checkout_path`
/// (when a repo already has one, e.g. a one-time hand/Fable-written rewrite)
/// -- the "deepen, not regenerate" baseline for a repo-level generation.
/// Every field is `None`/empty exactly when nothing was found on disk, which
/// is also the correct state for a project's first-ever repo-level run: the
/// baseline is purely additive grounding, never required for
/// `build_repo_facts` or any downstream pass to succeed.
///
/// This is read the SAME way every other checkout-scan source in this module
/// is (`read_optional`, degrade-on-absent, no error) -- it adds no second
/// filesystem-reading path. Content here is untrusted, model-facing input
/// exactly like every other `RepoFacts` field: it only ever leaves this
/// module through `RepoFacts::identity_slice` / `RepoFacts::subsystem_slice`
/// (both of which run the WHOLE slice, existing-docs content included,
/// through `sweep_slice`) or, for guides, through the repo-level generator's
/// own whole-prompt sweep before every send (`generate.rs::run_guides_pass`)
/// -- never sent to a generator raw.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExistingDocs {
    /// The current `README.md` content in full -- the Pass 1 identity
    /// baseline (current tagline/what_is to refine). `None` when no README
    /// exists yet at `checkout_path` (a genuine first-ever run).
    pub landing: Option<String>,
    /// `docs/reference/<subsystem>.md` content, keyed by subsystem name --
    /// only for subsystems this `RepoFacts` actually kept (never `misc`,
    /// which has no single reference page to deepen). A subsystem with no
    /// entry here has no existing page and Pass 2 must write it fresh.
    pub subsystem_pages: BTreeMap<String, String>,
    /// `docs/getting-started.md` content, if present.
    pub getting_started: Option<String>,
    /// `docs/guides/<slug>.md` content, keyed by file stem (e.g.
    /// `"run-the-fixture"` for `docs/guides/run-the-fixture.md`).
    pub guides: BTreeMap<String, String>,
}

impl ExistingDocs {
    /// `true` when nothing at all was found on disk -- the first-run case,
    /// where every later pass must behave exactly as it did before this item
    /// (no baseline to deepen from).
    pub fn is_empty(&self) -> bool {
        self.landing.is_none()
            && self.subsystem_pages.is_empty()
            && self.getting_started.is_none()
            && self.guides.is_empty()
    }
}

/// The full deterministic grounding for one repo at one ref. Built by
/// [`build_repo_facts`]; never constructed with fabricated/guessed content
/// by any other path in this crate.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RepoFacts {
    pub project_id: String,
    pub git_ref: String,
    /// `false` when the project has no entry in the KG store — every
    /// KG-derived field above is then empty/zeroed, and downstream passes
    /// must omit those numbers rather than invent them.
    pub kg_grounded: bool,
    pub scale: RepoScale,
    pub subsystems: Vec<Subsystem>,
    pub edge_matrix: SubsystemGraph,
    pub entry_points: EntryPoints,
    pub config_surface: ConfigSurface,
    pub prose_anchors: ProseAnchors,
    pub old_readme_sections: Vec<LegacySection>,
    /// DGDG-02: the existing `docs/` tree at this checkout, if any -- the
    /// "deepen, not regenerate" baseline. Empty/absent on a first-ever run;
    /// additive only, never required for any pass to succeed.
    pub existing_docs: ExistingDocs,
}

impl RepoFacts {
    /// ~6-8KB JSON slice for Pass 1 (identity): scale + subsystem rollup +
    /// entry points + prose anchors + legacy headings (not full legacy
    /// bodies — those are per-subsystem detail, see [`Self::subsystem_slice`]).
    /// Swept through [`sweep_input`] before it is returned; this is the
    /// ONLY way any `RepoFacts` content leaves this module for a prompt.
    pub fn identity_slice(&self) -> Result<String, ToolError> {
        let value = json!({
            "project_id": self.project_id,
            "git_ref": self.git_ref,
            "kg_grounded": self.kg_grounded,
            "scale": {
                "node_count": self.scale.node_count,
                "edge_count": self.scale.edge_count,
                "by_kind": self.scale.by_kind,
                "hotspots": self.scale.hotspots,
            },
            "subsystems": self.subsystems.iter().map(|s| json!({
                "name": s.name,
                "node_count": s.node_count,
                "kind_breakdown": s.kind_breakdown,
                "top_symbols": s.top_symbols,
                "is_misc": s.is_misc,
            })).collect::<Vec<_>>(),
            "entry_points": self.entry_points,
            "config_surface": self.config_surface,
            "prose_anchors": {
                "crate_description": self.prose_anchors.crate_description,
                "crate_root_docs": self.prose_anchors.crate_root_docs,
            },
            "legacy_headings": self.old_readme_sections.iter().map(|s| &s.heading).collect::<Vec<_>>(),
            // DGDG-02: the current README (if this repo already has one) as
            // the Pass 1 "deepen, don't regenerate" baseline. `None` on a
            // first-ever run -- `build_repo_identity_prompt` treats absence
            // as "write fresh," exactly as before this field existed.
            "existing_landing": self.existing_docs.landing,
        });
        sweep_slice(&value)
    }

    /// ~4-6KB JSON slice for Pass 2 (one subsystem's reference page): that
    /// subsystem's top symbols, its cross-subsystem edges (in/out), its
    /// module doc anchor, and any old-README section whose heading names
    /// it (a simple case-insensitive heading match — good enough to hand a
    /// model a labeled "here's what the old docs claimed" hint, not a
    /// substitute for the code-grounded facts above it).
    pub fn subsystem_slice(&self, name: &str) -> Result<String, ToolError> {
        let subsystem = self
            .subsystems
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| ToolError::NotFound(format!("no subsystem named '{name}' in RepoFacts")))?;

        let inbound: Vec<&SubsystemEdge> = self.edge_matrix.edges.iter().filter(|e| e.to == name).collect();
        let outbound: Vec<&SubsystemEdge> = self.edge_matrix.edges.iter().filter(|e| e.from == name).collect();

        let legacy_section = self
            .old_readme_sections
            .iter()
            .find(|s| !s.heading.is_empty() && s.heading.to_lowercase().contains(&name.to_lowercase()));

        let value = json!({
            "project_id": self.project_id,
            "subsystem": {
                "name": subsystem.name,
                "source_dir": subsystem.source_dir,
                "node_count": subsystem.node_count,
                "kind_breakdown": subsystem.kind_breakdown,
                "top_symbols": subsystem.top_symbols,
                "is_misc": subsystem.is_misc,
            },
            "calls_in": inbound.iter().map(|e| json!({"from": e.from, "weight": e.weight})).collect::<Vec<_>>(),
            "calls_out": outbound.iter().map(|e| json!({"to": e.to, "weight": e.weight})).collect::<Vec<_>>(),
            "module_docs": self.prose_anchors.subsystem_docs.get(name).cloned().unwrap_or_default(),
            "config_surface": self.config_surface,
            "legacy_section": legacy_section,
            // DGDG-02: this subsystem's CURRENT `docs/reference/<name>.md`
            // content, if this repo already has one -- the Pass 2 "deepen,
            // don't regenerate" baseline. `None` when no such page exists yet
            // (first-ever page for this subsystem) -- `build_subsystem_page_prompt`
            // treats absence as "write fresh," exactly as before this field
            // existed.
            "existing_page": self.existing_docs.subsystem_pages.get(name),
        });
        sweep_slice(&value)
    }
}

fn sweep_slice(value: &serde_json::Value) -> Result<String, ToolError> {
    let raw = serde_json::to_string(value)
        .map_err(|e| ToolError::Execution(format!("serialize RepoFacts slice: {e}")))?;
    let outcome = sweep_input(&raw)?;
    Ok(outcome.sanitized_content().to_string())
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Build a repo's [`RepoFacts`] — a pure function of `graph_source`,
/// `checkout_path`, and `project_id` (`git_ref` only stamps metadata; it
/// does not change what is derived). Zero LLM calls. Degrades to
/// `kg_grounded: false` when `graph_source` has no entry for `project_id`;
/// never fabricates a KG-derived number in that case.
pub fn build_repo_facts(
    graph_source: &dyn GraphSource,
    checkout_path: &Path,
    project_id: &str,
    git_ref: &str,
) -> Result<RepoFacts, ToolError> {
    let graph = graph_source.load_graph(project_id)?;

    let (kg_grounded, scale, subsystems, edge_matrix, graph_entrypoint_symbols) = match graph {
        Some(mut g) => {
            // Recomputed here (not trusted from the persisted store) so the
            // builder's output is deterministic given the SAME graph
            // content regardless of when/whether an earlier pipeline stage
            // ran `pagerank` over it -- load-bearing for fixture-graph unit
            // tests below, which construct graphs with no rank set at all.
            pagerank(&mut g);

            let scale = build_scale(&g);
            let subsystems = build_subsystems(&g);
            let edge_matrix = build_edge_matrix(&g, &subsystems);
            let entrypoints = kg_entrypoint_symbols(&g);
            (true, scale, subsystems, edge_matrix, entrypoints)
        }
        None => (false, RepoScale::default(), Vec::new(), SubsystemGraph::default(), Vec::new()),
    };

    let cargo_toml = read_optional(&checkout_path.join("Cargo.toml"));
    let bin_targets = cargo_toml.as_deref().map(parse_bin_targets).unwrap_or_default();
    let workspace_members = cargo_toml.as_deref().map(parse_workspace_members).unwrap_or_default();
    let crate_description = cargo_toml.as_deref().and_then(parse_package_description);

    let mut entrypoint_symbols = graph_entrypoint_symbols;
    if entrypoint_symbols.is_empty() {
        entrypoint_symbols = scan_entrypoint_symbols(checkout_path, &bin_targets);
    }

    let registered_tool_count = read_optional(&checkout_path.join("src/registry.rs"))
        .map(|s| s.matches(".register(").count());

    let entry_points = EntryPoints {
        bin_targets,
        workspace_members,
        entrypoint_symbols,
        registered_tool_count,
    };

    let config_surface = ConfigSurface { env_var_names: scan_config_surface(checkout_path) };

    let crate_root_docs = read_optional(&checkout_path.join("src/lib.rs"))
        .map(|s| leading_doc_comments(&s, PROSE_ANCHOR_LINE_CAP))
        .unwrap_or_default();
    let subsystem_docs = build_subsystem_docs(checkout_path, &subsystems);
    let prose_anchors = ProseAnchors { crate_description, crate_root_docs, subsystem_docs };

    // Read the README's raw content ONCE and reuse it both for the legacy
    // per-section split below and as DGDG-02's identity-pass baseline
    // (`ExistingDocs::landing`) -- no second README read.
    let readme_raw = read_optional(&checkout_path.join("README.md"));
    let old_readme_sections = readme_raw
        .as_deref()
        .map(|s| {
            split_old_sections(s)
                .into_iter()
                .map(|raw| LegacySection { heading: raw.heading, body: raw.body })
                .collect()
        })
        .unwrap_or_default();

    let existing_docs = scan_existing_docs(checkout_path, &subsystems, readme_raw.as_deref());

    Ok(RepoFacts {
        project_id: project_id.to_string(),
        git_ref: git_ref.to_string(),
        kg_grounded,
        scale,
        subsystems,
        edge_matrix,
        entry_points,
        config_surface,
        prose_anchors,
        old_readme_sections,
        existing_docs,
    })
}

/// DGDG-02: read whatever `docs/` tree already exists at `checkout_path`
/// (`docs/reference/<subsystem>.md` for every KEPT (non-`misc`) subsystem,
/// `docs/getting-started.md`, and every `docs/guides/*.md`) as the "deepen,
/// not regenerate" baseline for a repo-level generation. `readme` is the
/// README content the caller already read (never re-read here). Every read
/// is best-effort via `read_optional` -- an absent file/tree degrades to
/// `None`/empty, exactly like every other checkout-scan source in this
/// module, and is the correct, additive-only state for a project's
/// first-ever repo-level run.
fn scan_existing_docs(checkout_path: &Path, subsystems: &[Subsystem], readme: Option<&str>) -> ExistingDocs {
    let mut subsystem_pages = BTreeMap::new();
    for s in subsystems {
        if s.is_misc {
            continue;
        }
        let rel = format!("docs/reference/{}.md", s.name);
        // `s.name` is graph-derived (untrusted, same category as
        // `Subsystem::source_dir` above) -- gate it through the same
        // containment guard before ever joining it onto `checkout_path`.
        if !path_within_checkout(checkout_path, &rel) {
            continue;
        }
        if let Some(body) = read_optional(&checkout_path.join(&rel)) {
            subsystem_pages.insert(s.name.clone(), body);
        }
    }

    let getting_started = read_optional(&checkout_path.join("docs/getting-started.md"));

    let mut guides = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(checkout_path.join("docs/guides")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if let Some(body) = read_optional(&path) {
                guides.insert(stem.to_string(), body);
            }
        }
    }

    ExistingDocs { landing: readme.map(str::to_string), subsystem_pages, getting_started, guides }
}

// ---------------------------------------------------------------------------
// KG-derived sources (1-4 of design §2)
// ---------------------------------------------------------------------------

fn build_scale(g: &KnowledgeGraph) -> RepoScale {
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for n in g.nodes() {
        *by_kind.entry(n.kind.as_str().to_string()).or_insert(0) += 1;
    }

    let mut ranked: Vec<&KgNode> = g.nodes().collect();
    ranked.sort_by(|a, b| b.rank.total_cmp(&a.rank).then_with(|| a.id.cmp(&b.id)));
    let hotspots = ranked.into_iter().take(HOTSPOT_LIMIT).map(SymbolRef::from_node).collect();

    RepoScale { node_count: g.node_count(), edge_count: g.edge_count(), by_kind, hotspots }
}

/// Top-level path-prefix grouping (design §1.2): `crate::<mod>` for a
/// Rust-shaped node id, `<pkg>::src` for a TS-tree node id whose second
/// segment is `src` (e.g. `constellation-web::src::App`), else the id's
/// first segment.
fn subsystem_prefix(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("crate::") {
        return rest.split("::").next().unwrap_or(MISC_SUBSYSTEM_NAME).to_string();
    }
    let mut parts = id.splitn(3, "::");
    let first = parts.next().unwrap_or(MISC_SUBSYSTEM_NAME).to_string();
    match parts.next() {
        Some("src") => format!("{first}::src"),
        _ => first,
    }
}

/// The dominant top-two-component source directory among a prefix's member
/// nodes (e.g. `"src/mesh/tailnet.rs"` -> `"src/mesh"`) — advisory metadata
/// for a reference page's "where this lives" line, not load-bearing for any
/// gate.
fn dominant_source_dir(members: &[&KgNode]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in members {
        let comps: Vec<&str> = n.path.split('/').collect();
        let dir = if comps.len() > 1 { comps[..comps.len() - 1].join("/") } else { n.path.clone() };
        let top_two: String = dir.split('/').take(2).collect::<Vec<_>>().join("/");
        *counts.entry(top_two).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(dir, _)| dir)
        .unwrap_or_default()
}

fn build_subsystems(g: &KnowledgeGraph) -> Vec<Subsystem> {
    let total_nodes = g.node_count();
    let threshold = selection_threshold(total_nodes);

    let mut by_prefix: BTreeMap<String, Vec<&KgNode>> = BTreeMap::new();
    for n in g.nodes() {
        by_prefix.entry(subsystem_prefix(&n.id)).or_default().push(n);
    }

    let to_subsystem = |name: &str, members: &[&KgNode], is_misc: bool| -> Subsystem {
        let mut kind_breakdown: BTreeMap<String, usize> = BTreeMap::new();
        for n in members {
            *kind_breakdown.entry(n.kind.as_str().to_string()).or_insert(0) += 1;
        }
        let mut ranked: Vec<&KgNode> = members.to_vec();
        ranked.sort_by(|a, b| b.rank.total_cmp(&a.rank).then_with(|| a.id.cmp(&b.id)));
        let top_symbols = ranked
            .into_iter()
            .take(TOP_SYMBOLS_PER_SUBSYSTEM)
            .map(SymbolRef::from_node)
            .collect();
        let aggregate_rank: f32 = members.iter().map(|n| n.rank).sum();
        Subsystem {
            name: name.to_string(),
            source_dir: if is_misc { String::new() } else { dominant_source_dir(members) },
            node_count: members.len(),
            kind_breakdown,
            top_symbols,
            aggregate_rank,
            is_misc,
        }
    };

    // Candidates: prefixes clearing the node-count threshold.
    let mut candidates: Vec<(String, Vec<&KgNode>)> =
        by_prefix.iter().filter(|(_, m)| m.len() >= threshold).map(|(k, v)| (k.clone(), v.clone())).collect();
    // Rank by node_count * aggregate_rank, descending; tie-break by name for
    // determinism.
    candidates.sort_by(|(_, a), (_, b)| {
        let score_a = a.len() as f32 * a.iter().map(|n| n.rank).sum::<f32>();
        let score_b = b.len() as f32 * b.iter().map(|n| n.rank).sum::<f32>();
        score_b.total_cmp(&score_a)
    });

    let kept_names: std::collections::BTreeSet<String> =
        candidates.iter().take(MAX_SUBSYSTEMS).map(|(name, _)| name.clone()).collect();

    let mut out: Vec<Subsystem> = Vec::new();
    let mut misc_members: Vec<&KgNode> = Vec::new();
    for (name, members) in &by_prefix {
        if kept_names.contains(name) {
            out.push(to_subsystem(name, members, false));
        } else {
            misc_members.extend(members.iter().copied());
        }
    }
    // Keep output order matching the ranked-by-score order (not BTreeMap's
    // alphabetical iteration) so the "top subsystems first" framing holds.
    out.sort_by(|a, b| {
        let score_a = a.node_count as f32 * a.aggregate_rank;
        let score_b = b.node_count as f32 * b.aggregate_rank;
        score_b.total_cmp(&score_a).then_with(|| a.name.cmp(&b.name))
    });

    if !misc_members.is_empty() {
        out.push(to_subsystem(MISC_SUBSYSTEM_NAME, &misc_members, true));
    }

    out
}

fn build_edge_matrix(g: &KnowledgeGraph, subsystems: &[Subsystem]) -> SubsystemGraph {
    // Map every node id to the subsystem name that ended up owning it
    // (kept subsystem or misc) -- needed because `subsystem_prefix` alone
    // doesn't know which raw prefixes got folded into misc.
    let mut node_subsystem: BTreeMap<&str, &str> = BTreeMap::new();
    // Rebuild prefix->subsystem-name membership directly, mirroring
    // build_subsystems' own grouping so the two never drift apart.
    let kept: std::collections::BTreeSet<&str> =
        subsystems.iter().filter(|s| !s.is_misc).map(|s| s.name.as_str()).collect();
    let has_misc = subsystems.iter().any(|s| s.is_misc);

    for n in g.nodes() {
        let prefix = subsystem_prefix(&n.id);
        let owner: &str = if kept.contains(prefix.as_str()) {
            // SAFETY: `kept` was built from `subsystems`' own `name` fields,
            // so a match here always resolves to one of those `String`s;
            // re-borrow it from `subsystems` to get a `&'a str` with the
            // right lifetime.
            subsystems.iter().find(|s| s.name == prefix).map(|s| s.name.as_str()).unwrap()
        } else if has_misc {
            MISC_SUBSYSTEM_NAME
        } else {
            continue;
        };
        node_subsystem.insert(n.id.as_str(), owner);
    }

    let mut weights: BTreeMap<(String, String), u32> = BTreeMap::new();
    for e in g.edges() {
        if e.kind != EdgeKind::Calls {
            continue;
        }
        let (Some(&from), Some(&to)) = (node_subsystem.get(e.from.as_str()), node_subsystem.get(e.to.as_str()))
        else {
            continue;
        };
        if from == to {
            // Only CROSS-prefix calls belong in the architecture edge
            // matrix (design §2 source 3) -- intra-subsystem calls are
            // implementation detail, not an architecture edge.
            continue;
        }
        *weights.entry((from.to_string(), to.to_string())).or_insert(0) += 1;
    }

    let mut edges: Vec<SubsystemEdge> =
        weights.into_iter().map(|((from, to), weight)| SubsystemEdge { from, to, weight }).collect();
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    SubsystemGraph { edges }
}

/// `main`/`serve`/`register_all`-shaped node names, from the graph, sorted.
fn kg_entrypoint_symbols(g: &KnowledgeGraph) -> Vec<String> {
    const SHAPES: &[&str] = &["main", "serve", "register_all"];
    let mut out: Vec<String> = g
        .nodes()
        .filter(|n| matches!(n.kind, NodeKind::Function))
        .filter(|n| SHAPES.contains(&n.name.as_str()))
        .map(|n| n.id.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Checkout scan (sources 4-7): entry points / config surface / prose anchors
// ---------------------------------------------------------------------------

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// True iff `rel` (a graph-derived, untrusted relative module path) resolves to
/// a location INSIDE `checkout_path`. Rejects absolute paths, any `..`/root/prefix
/// component (the syntactic traversal guard, which holds even when the target
/// doesn't exist), and — when both paths canonicalize — confirms containment on
/// the real filesystem (defeats symlink escapes). Used to gate every read of a
/// graph-derived path so a corrupt node path can never read outside the checkout.
fn path_within_checkout(checkout_path: &Path, rel: &str) -> bool {
    use std::path::Component;
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return false;
    }
    if rel_path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return false;
    }
    // Filesystem-level containment check (best-effort; symlink defense). If the
    // joined path can't be canonicalized yet (e.g. doesn't exist), the syntactic
    // guard above already established it can't escape, so allow it through.
    let joined = checkout_path.join(rel_path);
    match (joined.canonicalize(), checkout_path.canonicalize()) {
        (Ok(real), Ok(root)) => real.starts_with(&root),
        _ => true,
    }
}

/// Parse `[[bin]] name = "..." path = "..."` tables out of a Cargo.toml's
/// raw text. A line-oriented scan (this repo's Cargo.toml -- and every
/// Cargo.toml this builder will ever see -- writes `[[bin]]` tables as
/// simple `key = "value"` lines, never inline tables), not a full TOML
/// parser: good enough for entry-point grounding, not a substitute for
/// `cargo metadata`.
fn parse_bin_targets(cargo_toml: &str) -> Vec<BinTarget> {
    let mut out = Vec::new();
    let mut in_bin = false;
    let mut name: Option<String> = None;
    let mut path: Option<String> = None;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_bin {
                if let (Some(n), Some(p)) = (name.take(), path.take()) {
                    out.push(BinTarget { name: n, path: p });
                }
            }
            in_bin = trimmed == "[[bin]]";
            if !in_bin {
                name = None;
                path = None;
            }
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(v) = parse_toml_string_value(trimmed, "name") {
            name = Some(v);
        } else if let Some(v) = parse_toml_string_value(trimmed, "path") {
            path = Some(v);
        }
    }
    if in_bin {
        if let (Some(n), Some(p)) = (name, path) {
            out.push(BinTarget { name: n, path: p });
        }
    }
    out
}

/// Parse `[workspace]\nmembers = ["a", "b"]` (single-line array form) out of
/// a Cargo.toml's raw text.
fn parse_workspace_members(cargo_toml: &str) -> Vec<String> {
    let mut in_workspace = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace = trimmed == "[workspace]";
            continue;
        }
        if in_workspace {
            if let Some(rest) = trimmed.strip_prefix("members") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    return parse_toml_string_array(rest.trim());
                }
            }
        }
    }
    Vec::new()
}

/// Parse `description = "..."` out of the `[package]` table.
fn parse_package_description(cargo_toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(v) = parse_toml_string_value(trimmed, "description") {
                return Some(v);
            }
        }
    }
    None
}

/// `key = "value"` (or `key="value"`) -> `Some("value")` when `trimmed`'s key
/// matches `key`; `None` otherwise. Only handles a plain quoted string
/// value, which is the only shape this builder's callers need.
fn parse_toml_string_value(trimmed: &str, key: &str) -> Option<String> {
    let rest = trimmed.strip_prefix(key)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// `["a", "b", "c"]` -> `["a", "b", "c"]`. Single-line only.
fn parse_toml_string_array(rest: &str) -> Vec<String> {
    let inner = rest.trim_start_matches('[');
    let inner = inner.split(']').next().unwrap_or("");
    inner
        .split(',')
        .filter_map(|tok| {
            let tok = tok.trim();
            let tok = tok.strip_prefix('"')?;
            let end = tok.find('"')?;
            Some(tok[..end].to_string())
        })
        .collect()
}

/// Fallback entry-point scan (used only when the KG had no `main`/`serve`/
/// `register_all`-shaped symbol, e.g. `kg_grounded: false`): does `src/bin/
/// <bin>.rs` contain `fn main(` and does `src/registry.rs` contain `fn
/// register_all(`.
fn scan_entrypoint_symbols(checkout_path: &Path, bin_targets: &[BinTarget]) -> Vec<String> {
    let mut out = Vec::new();
    for bin in bin_targets {
        if let Some(src) = read_optional(&checkout_path.join(&bin.path)) {
            if src.contains("fn main(") {
                out.push(format!("bin::{}::main", bin.name));
            }
        }
    }
    if let Some(src) = read_optional(&checkout_path.join("src/registry.rs")) {
        if src.contains("fn register_all(") {
            out.push("registry::register_all".to_string());
        }
    }
    out.sort();
    out
}

/// Scan every `config.rs`-shaped file under `checkout_path` for env-var
/// accessor call sites (`std::env::var("NAME")` / `env::var("NAME")` /
/// `env_nonempty("NAME"` / `SecretManager::get("NAME"` /
/// `vault::manager().get("NAME"`) and return the literal NAMES found —
/// never a resolved value (this scan never executes anything, only reads
/// source text).
fn scan_config_surface(checkout_path: &Path) -> Vec<String> {
    const CALL_MARKERS: &[&str] =
        &["env::var(", "env_nonempty(", "SecretManager::get(", "vault::manager().get("];

    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in find_files_named(checkout_path, "config.rs") {
        let Some(src) = read_optional(&path) else { continue };
        for marker in CALL_MARKERS {
            let mut rest = src.as_str();
            while let Some(pos) = rest.find(marker) {
                let after = &rest[pos + marker.len()..];
                if let Some(name) = parse_leading_string_literal(after) {
                    names.insert(name);
                }
                // Advance past exactly one char (never a raw byte index) so
                // a non-ASCII byte right after the marker can never land the
                // next slice off a UTF-8 char boundary.
                let advance = after.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                rest = &after[advance..];
            }
        }
    }
    names.into_iter().collect()
}

/// `"NAME", ...` (any text starting with a quoted string) -> `Some("NAME")`.
fn parse_leading_string_literal(s: &str) -> Option<String> {
    let s = s.trim_start();
    let inner = s.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// Recursively collect every file under `root` whose file name is exactly
/// `filename`. Bounded, best-effort: an unreadable directory is skipped
/// rather than failing the whole scan (this is grounding data, not a build
/// step).
fn find_files_named(root: &Path, filename: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_named(root, filename, &mut out);
    out
}

fn walk_named(dir: &Path, filename: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_named(&path, filename, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some(filename) {
            out.push(path);
        }
    }
}

/// The leading `//!` doc-comment block of `src`, stripped of the `//!`
/// marker, capped at `limit` lines.
fn leading_doc_comments(src: &str, limit: usize) -> Vec<String> {
    src.lines()
        .map(str::trim)
        .take_while(|l| l.starts_with("//!") || l.is_empty())
        .filter(|l| l.starts_with("//!"))
        .map(|l| l.trim_start_matches("//!").trim().to_string())
        .take(limit)
        .collect()
}

/// Per-kept-subsystem module `//!` docs (design §2 source 6), gathered via
/// [`crate::scribe::inspect::inspect_module`] over that subsystem's
/// `source_dir` -- reusing the existing worktree-inspection walk (no git
/// call is involved: `inspect_module` only walks `wt.path.join(module_path)`
/// on disk) rather than a second file-walking implementation.
fn build_subsystem_docs(checkout_path: &Path, subsystems: &[Subsystem]) -> BTreeMap<String, Vec<String>> {
    let wt = InspectionWorktree {
        path: checkout_path.to_path_buf(),
        repo_path: checkout_path.to_path_buf(),
        git_ref: "working-tree".to_string(),
    };

    let mut out = BTreeMap::new();
    for s in subsystems {
        if s.is_misc || s.source_dir.is_empty() {
            continue;
        }
        // `source_dir` is derived from graph node paths (untrusted input). Refuse
        // to read anything that could escape the checkout — a malformed/corrupt
        // graph path such as `../../etc` or an absolute path must never turn into
        // a filesystem read outside `checkout_path`. This runs BEFORE inspect_module
        // touches disk, closing the graph-path -> local-read boundary.
        if !path_within_checkout(checkout_path, &s.source_dir) {
            continue;
        }
        if let Ok(bundle) = inspect::inspect_module(&wt, &s.source_dir) {
            let mut docs: Vec<String> = Vec::new();
            for file in &bundle.files {
                for line in &file.doc_comments {
                    if docs.len() >= PROSE_ANCHOR_LINE_CAP {
                        break;
                    }
                    docs.push(line.trim_start_matches("//!").trim_start_matches("///").trim().to_string());
                }
            }
            if !docs.is_empty() {
                out.insert(s.name.clone(), docs);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::{Confidence, KgEdge};

    fn node(id: &str, kind: NodeKind, path: &str) -> KgNode {
        let name = id.rsplit("::").next().unwrap_or(id).to_string();
        KgNode::new(id, kind, name, path)
    }

    fn call(g: &mut KnowledgeGraph, from: &str, to: &str) {
        g.insert_edge(KgEdge::new(from, to, EdgeKind::Calls, Confidence::Extracted)).unwrap();
    }

    // ── subsystem_prefix ─────────────────────────────────────────────────

    #[test]
    fn subsystem_prefix_handles_crate_and_ts_shapes() {
        assert_eq!(subsystem_prefix("crate::mesh::tailnet::TailnetServer::start"), "mesh");
        assert_eq!(subsystem_prefix("crate::intake::code_v2::CaseV2"), "intake");
        assert_eq!(subsystem_prefix("constellation-web::src::App"), "constellation-web::src");
        assert_eq!(subsystem_prefix("bare_symbol"), "bare_symbol");
    }

    // ── rollup + fold + edge matrix (spec TEST PLAN item 1) ─────────────

    /// Builds a 68-node fixture graph: `alpha` (32 nodes, kept), `beta` (31
    /// nodes, kept), `gamma` (5 nodes, below the max(30, 1%) threshold ->
    /// folded into `misc`). Cross-prefix calls are counted in the edge
    /// matrix; a same-prefix call is not.
    fn three_group_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("FIX");
        for i in 0..32 {
            g.insert_node(node(&format!("crate::alpha::f{i}"), NodeKind::Function, &format!("src/alpha/f{i}.rs")));
        }
        for i in 0..31 {
            g.insert_node(node(&format!("crate::beta::f{i}"), NodeKind::Function, &format!("src/beta/f{i}.rs")));
        }
        for i in 0..5 {
            g.insert_node(node(&format!("crate::gamma::f{i}"), NodeKind::Function, &format!("src/gamma/f{i}.rs")));
        }
        // Cross-prefix: alpha -> beta (counted), alpha -> gamma (counted,
        // gamma folds into misc so this becomes alpha -> misc).
        call(&mut g, "crate::alpha::f0", "crate::beta::f0");
        call(&mut g, "crate::alpha::f1", "crate::beta::f0");
        call(&mut g, "crate::alpha::f0", "crate::gamma::f0");
        // Same-prefix: must NOT appear in the edge matrix.
        call(&mut g, "crate::alpha::f0", "crate::alpha::f1");
        g
    }

    #[test]
    fn rollup_keeps_threshold_subsystems_and_folds_the_rest_to_misc() {
        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent"), "FIX", "abc").unwrap();

        assert!(facts.kg_grounded);
        assert_eq!(facts.scale.node_count, 68);

        let names: Vec<&str> = facts.subsystems.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(!names.contains(&"gamma"), "gamma is below threshold, must not survive as its own subsystem");
        assert!(names.contains(&"misc"), "gamma's 5 nodes must fold into misc");

        let alpha = facts.subsystems.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.node_count, 32);
        let misc = facts.subsystems.iter().find(|s| s.is_misc).unwrap();
        assert_eq!(misc.node_count, 5, "misc holds exactly gamma's folded nodes");
    }

    #[test]
    fn edge_matrix_counts_cross_prefix_calls_and_excludes_same_prefix() {
        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent"), "FIX", "abc").unwrap();

        assert_eq!(facts.edge_matrix.weight_between("alpha", "beta"), 2, "two alpha->beta calls");
        assert_eq!(facts.edge_matrix.weight_between("alpha", "misc"), 1, "alpha->gamma folds to alpha->misc");
        assert_eq!(facts.edge_matrix.weight_between("alpha", "alpha"), 0, "same-prefix calls excluded");
        assert!(!facts.edge_matrix.is_empty());
    }

    #[test]
    fn subsystem_with_zero_calls_edges_still_appears_in_rollup() {
        // Same fixture, but query a subsystem's slice/edges when it has no
        // incident cross-subsystem edges at all -- must not panic, and must
        // simply report empty in/out.
        let mut g = KnowledgeGraph::new("FIX2");
        for i in 0..40 {
            g.insert_node(node(&format!("crate::lonely::f{i}"), NodeKind::Function, &format!("src/lonely/f{i}.rs")));
        }
        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent"), "FIX2", "abc").unwrap();
        let names: Vec<&str> = facts.subsystems.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["lonely"]);
        assert!(facts.edge_matrix.is_empty());
    }

    // ── cap at 16, ranked by node_count * aggregate_rank ────────────────

    #[test]
    fn selection_caps_at_sixteen_ranked_subsystems_folding_the_rest_to_misc() {
        let mut g = KnowledgeGraph::new("BIG");
        // 18 candidate prefixes, each with 30 nodes (clears the floor
        // threshold) and a distinct, increasing hub degree so their
        // PageRank -- and therefore their node_count*aggregate_rank score
        // -- is strictly ordered by index.
        for group in 0..18 {
            let hub = format!("crate::g{group}::hub");
            g.insert_node(node(&hub, NodeKind::Function, &format!("src/g{group}/hub.rs")));
            for i in 0..29 {
                let leaf = format!("crate::g{group}::f{i}");
                g.insert_node(node(&leaf, NodeKind::Function, &format!("src/g{group}/f{i}.rs")));
            }
            // More leaves call the hub in higher-numbered groups -> higher
            // PageRank -> higher score -> kept preferentially.
            for i in 0..(group + 1) {
                let leaf = format!("crate::g{group}::f{}", i % 29);
                call(&mut g, &leaf, &hub);
            }
        }

        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent"), "BIG", "abc").unwrap();

        let kept: Vec<&Subsystem> = facts.subsystems.iter().filter(|s| !s.is_misc).collect();
        assert_eq!(kept.len(), 16, "exactly 16 subsystems kept, cap enforced");

        let kept_names: std::collections::BTreeSet<&str> = kept.iter().map(|s| s.name.as_str()).collect();
        // The two lowest-scored groups (g0, g1) must NOT survive as their
        // own subsystem -- they fold into misc.
        assert!(!kept_names.contains("g0"));
        assert!(!kept_names.contains("g1"));
        assert!(kept_names.contains("g17"), "highest-scored group kept");

        let misc = facts.subsystems.iter().find(|s| s.is_misc).unwrap();
        assert_eq!(misc.node_count, 60, "g0 + g1's 30 nodes each folded into misc");
    }

    // ── kg_grounded: false degradation (spec TEST PLAN item 2) ──────────

    #[test]
    fn no_kg_entry_degrades_without_fabricating_numbers() {
        let facts = build_repo_facts(&NoGraphSource, Path::new("/nonexistent-checkout-xyz"), "GHOST", "abc").unwrap();
        assert!(!facts.kg_grounded);
        assert_eq!(facts.scale.node_count, 0);
        assert_eq!(facts.scale.edge_count, 0);
        assert!(facts.scale.hotspots.is_empty());
        assert!(facts.subsystems.is_empty());
        assert!(facts.edge_matrix.is_empty());
        // Checkout-scan sources still degrade gracefully on a nonexistent
        // path rather than erroring.
        assert!(facts.entry_points.bin_targets.is_empty());
        assert!(facts.config_surface.env_var_names.is_empty());
        assert!(facts.old_readme_sections.is_empty());
    }

    // ── checkout scan: Cargo.toml / config surface / old README ─────────

    fn tmp_checkout(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dgrich01-repo-facts-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    #[test]
    fn checkout_scan_reads_bin_targets_workspace_and_description() {
        let dir = tmp_checkout("cargo");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n\n\
[package]\nname = \"demo\"\ndescription = \"A demo tool hub\"\n\n\
[[bin]]\nname = \"demo_bin\"\npath = \"src/bin/demo_bin.rs\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src/bin")).unwrap();
        std::fs::write(dir.join("src/bin/demo_bin.rs"), "fn main() {}\n").unwrap();

        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        assert_eq!(facts.entry_points.workspace_members, vec!["a", "b"]);
        assert_eq!(facts.entry_points.bin_targets.len(), 1);
        assert_eq!(facts.entry_points.bin_targets[0].name, "demo_bin");
        assert_eq!(facts.prose_anchors.crate_description.as_deref(), Some("A demo tool hub"));
        assert!(facts.entry_points.entrypoint_symbols.contains(&"bin::demo_bin::main".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_surface_extracts_names_only_never_values() {
        let dir = tmp_checkout("config");
        std::fs::write(
            dir.join("src/config.rs"),
            "pub fn bind() -> String { env_nonempty(\"TERMINUS_BIND\", \"0.0.0.0:8080\") }\n\
pub fn redis() -> String { std::env::var(\"REDIS_URL\").unwrap() }\n",
        )
        .unwrap();

        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        assert!(facts.config_surface.env_var_names.contains(&"TERMINUS_BIND".to_string()));
        assert!(facts.config_surface.env_var_names.contains(&"REDIS_URL".to_string()));
        // Never a value -- the literal default "0.0.0.0:8080" must not
        // appear anywhere in the extracted names list.
        assert!(!facts.config_surface.env_var_names.iter().any(|n| n.contains("0.0.0.0")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_readme_missing_yields_no_sections_not_an_error() {
        let dir = tmp_checkout("noreadme");
        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        assert!(facts.old_readme_sections.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_readme_sections_are_parsed_and_labeled_legacy() {
        let dir = tmp_checkout("readme");
        std::fs::write(
            dir.join("README.md"),
            "# Demo\n\n## Install\n\nRun `cargo build`.\n\n## Configuration\n\nSet `WIDGET_PORT`.\n",
        )
        .unwrap();
        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        let headings: Vec<&str> = facts.old_readme_sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, vec!["Install", "Configuration"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── PII sweep on slice serialization (spec TEST PLAN item 3) ────────

    #[test]
    fn identity_slice_placeholders_a_private_ip_in_a_prose_anchor() {
        // NOTE: `<internal-ip>` below is a deliberately fake private-IP // pii-test-fixture
        // literal for this fixture only -- each source line naming it is
        // tagged `// pii-test-fixture` (the repo's own push-gate whitelist
        // convention) so the push-time scan of THIS SOURCE FILE exempts
        // them. That tag is NOT embedded in the actual fixture CONTENT
        // written to `src/lib.rs` below -- doing so would make the
        // *runtime* PII gate (`crate::github::pii::scan_and_redact`, which
        // shares the identical "line contains pii-test-fixture -> skip"
        // rule) skip the very line this test exists to prove gets redacted.
        let dir = tmp_checkout("pii");
        std::fs::write(dir.join("src/lib.rs"), "//! Connects to <internal-ip> for status.\n").unwrap(); // pii-test-fixture

        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        assert!(facts.prose_anchors.crate_root_docs.iter().any(|l| l.contains("<internal-ip>")), // pii-test-fixture
            "sanity: the raw fact carries the literal before the sweep");

        let slice = facts.identity_slice().unwrap();
        assert!(!slice.contains("<internal-ip>"), "the SWEPT slice must not carry the raw private IP"); // pii-test-fixture
        assert!(slice.contains("[REDACTED:"), "a redaction marker must be present instead");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subsystem_slice_reports_in_out_edges_and_is_swept() {
        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent"), "FIX", "abc").unwrap();
        let slice = facts.subsystem_slice("beta").unwrap();
        assert!(slice.contains("\"calls_in\""));
        assert!(slice.contains("alpha"), "beta's inbound edge from alpha must be present");

        let err = facts.subsystem_slice("does-not-exist").unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    // ── DGDG-02: existing-docs baseline ingestion ───────────────────────

    /// Acceptance criterion 1: when a repo already has a
    /// `docs/reference/<subsystem>.md` page, `subsystem_slice` for that
    /// subsystem carries its content under `existing_page` as a refine
    /// baseline; a sibling subsystem with no such page on disk gets no
    /// fabricated one (first-run behavior for that subsystem, unaffected by
    /// the sibling's baseline).
    #[test]
    fn subsystem_slice_carries_existing_reference_page_when_present_not_when_absent() {
        let dir = tmp_checkout("existing-docs");
        std::fs::create_dir_all(dir.join("docs/reference")).unwrap();
        std::fs::write(
            dir.join("docs/reference/alpha.md"),
            "# alpha\n\nAlpha already handles routing reliably; this is the baseline to deepen.\n",
        )
        .unwrap();

        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), &dir, "FIX", "abc").unwrap();

        assert_eq!(
            facts.existing_docs.subsystem_pages.get("alpha").map(String::as_str),
            Some("# alpha\n\nAlpha already handles routing reliably; this is the baseline to deepen.\n")
        );
        assert!(facts.existing_docs.subsystem_pages.get("beta").is_none());

        let alpha_slice = facts.subsystem_slice("alpha").unwrap();
        assert!(alpha_slice.contains("existing_page"));
        assert!(alpha_slice.contains("Alpha already handles routing reliably"));

        // beta has no existing page on disk -- must not fabricate one, and
        // must not leak alpha's baseline into beta's slice.
        let beta_slice = facts.subsystem_slice("beta").unwrap();
        assert!(!beta_slice.contains("Alpha already handles routing reliably"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Acceptance criterion 4: no existing docs tree at all (a genuine
    /// first-ever run) -- `ExistingDocs` is entirely empty and every slice
    /// behaves exactly as it did before this field existed (no fabricated
    /// baseline anywhere).
    #[test]
    fn no_existing_docs_tree_yields_empty_existing_docs_first_run_case() {
        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent-dgdg02-fixture"), "FIX", "abc")
            .unwrap();

        assert!(facts.existing_docs.is_empty());
        assert!(facts.existing_docs.landing.is_none());
        assert!(facts.existing_docs.getting_started.is_none());
        assert!(facts.existing_docs.guides.is_empty());

        let alpha_slice = facts.subsystem_slice("alpha").unwrap();
        assert!(!alpha_slice.contains("Alpha already"));
    }

    /// Acceptance criterion 1 (identity pass): the current README content is
    /// carried as `existing_landing` when present; absent when there is no
    /// README yet.
    #[test]
    fn identity_slice_carries_existing_landing_when_readme_present_not_when_absent() {
        let dir = tmp_checkout("existing-landing");
        std::fs::write(
            dir.join("README.md"),
            "# Demo\n\nDemo is already a solid fleet hub connecting three subsystems.\n",
        )
        .unwrap();
        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        let slice = facts.identity_slice().unwrap();
        assert!(slice.contains("existing_landing"));
        assert!(slice.contains("Demo is already a solid fleet hub"));
        let _ = std::fs::remove_dir_all(&dir);

        let no_readme_dir = tmp_checkout("no-existing-landing");
        let facts2 = build_repo_facts(&NoGraphSource, &no_readme_dir, "DEMO", "abc").unwrap();
        assert!(facts2.existing_docs.landing.is_none());
        let _ = std::fs::remove_dir_all(&no_readme_dir);
    }

    /// Acceptance criterion 3 (PII sweep): existing-doc content is untrusted
    /// input like every other `RepoFacts` field -- a private IP embedded in
    /// an existing `docs/reference/<subsystem>.md` page must be redacted by
    /// the same `subsystem_slice` sweep before it could ever reach a prompt,
    /// exactly like `identity_slice_placeholders_a_private_ip_in_a_prose_anchor`
    /// above proves for prose anchors.
    #[test]
    fn subsystem_slice_sweeps_pii_in_existing_page_content() {
        let dir = tmp_checkout("existing-docs-pii");
        std::fs::create_dir_all(dir.join("docs/reference")).unwrap();
        std::fs::write(
            dir.join("docs/reference/alpha.md"),
            "# alpha\n\nConnects to <internal-ip> for status.\n", // pii-test-fixture
        )
        .unwrap();

        let g = three_group_graph();
        let facts = build_repo_facts(&FixtureGraphSource(g), &dir, "FIX", "abc").unwrap();

        assert!(
            facts
                .existing_docs
                .subsystem_pages
                .get("alpha")
                .unwrap()
                .contains("<internal-ip>"), // pii-test-fixture
            "sanity: the raw fact carries the literal before the sweep"
        );

        let slice = facts.subsystem_slice("alpha").unwrap();
        assert!(!slice.contains("<internal-ip>"), "the SWEPT slice must not carry the raw private IP"); // pii-test-fixture
        assert!(slice.contains("[REDACTED:"), "a redaction marker must be present instead");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Getting-started and guides on disk are read into `ExistingDocs` for
    /// Pass 3's baseline (threaded into the guides prompt by
    /// `generate::run_guides_pass`, not by a `RepoFacts` slice method, since
    /// Pass 3 already sweeps its whole assembled prompt before every send).
    #[test]
    fn scans_existing_getting_started_and_guides_from_disk() {
        let dir = tmp_checkout("existing-guides");
        std::fs::create_dir_all(dir.join("docs/guides")).unwrap();
        std::fs::write(dir.join("docs/getting-started.md"), "Clone with `git clone <repo>`.\n").unwrap();
        std::fs::write(dir.join("docs/guides/run-the-fixture.md"), "1. Build it.\n2. Run it.\n").unwrap();

        let facts = build_repo_facts(&NoGraphSource, &dir, "DEMO", "abc").unwrap();
        assert_eq!(
            facts.existing_docs.getting_started.as_deref(),
            Some("Clone with `git clone <repo>`.\n")
        );
        assert_eq!(
            facts.existing_docs.guides.get("run-the-fixture").map(String::as_str),
            Some("1. Build it.\n2. Run it.\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graph_derived_paths_cannot_escape_the_checkout() {
        let root = Path::new("/some/checkout");
        // Legitimate in-tree module paths are allowed (syntactic guard passes;
        // canonicalize falls through to `true` for a non-existent fixture root).
        assert!(path_within_checkout(root, "src/mesh"));
        assert!(path_within_checkout(root, "src/tools/docgen"));
        // Traversal / absolute / prefix escapes are refused BEFORE any read.
        assert!(!path_within_checkout(root, "../etc/passwd"));
        assert!(!path_within_checkout(root, "src/../../etc"));
        assert!(!path_within_checkout(root, "/etc/passwd"));
        assert!(!path_within_checkout(root, "src/mesh/../../.."));
    }
}
