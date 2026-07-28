//! CXEG-06: house-style exemplar extraction from Atlas.
//!
//! Derives, per project and per Leiden **community** (KGRAPH-05 — a graph
//! `cluster`, not a whole project), the community's MODAL patterns plus a few
//! representative EXEMPLARS, so a Tier-C reviewer (CXEG-07) can cite "how
//! THIS codebase does X" instead of generic opinion. House-style is
//! deliberately community-scoped, not global — a `pg/` subsystem and a
//! `cortex/` subsystem can legitimately favor different idioms.
//!
//! ## Reuse (S9 single-source)
//! - Card building + embedding reuse
//!   [`crate::scribe::graph::vec_embed::node_card`] /
//!   [`crate::scribe::graph::vec_embed::EmbedClient`] — the SAME text builder
//!   and client `metrics::semantic_duplication_signals` (CXEG-03) and
//!   `scribe_kg_build`'s embedding pipeline use. No second card/embed
//!   implementation.
//! - The 1-hop caller/callee walk for card-building reuses
//!   [`crate::scribe::graph::query::one_hop_neighbors`] — the same
//!   single-source helper `kg_neighbors`/`cortex_scope`/`metrics` all call.
//!
//! ## No source-text inspection
//! Unlike a hypothetical implementation that greps file contents, every
//! signal here (exemplar selection AND the deterministic [`ModalFacts`]) is
//! derived purely from [`KgNode`] METADATA already in the graph (`kind`,
//! `name`, `path`, `rank`, `degree`, `cluster`) plus the embedding of each
//! node's [`node_card`] — matching the rest of this crate's Atlas-consuming
//! modules (`cortex::scope`, `cortex::metrics`, `review::kg_context`), none
//! of which re-reads source off disk. `config_read_idiom` and
//! `rust_tool_shape_present` specifically look for THIS repo's own
//! `from_env()`/`RustTool`-4-method naming conventions in node names, which
//! is a graph-metadata-only proxy, not a text/AST inspection of the idiom.
//!
//! ## Selection method
//! For each `(community, kind)` bucket: build each member's [`node_card`],
//! embed the batch, average the vectors into a centroid, then rank members by
//! cosine similarity to that centroid (ties broken by `rank` desc, then `id`
//! asc) — nearest-to-centroid-and-central, i.e. the MODAL shape of that
//! bucket, not an arbitrary/extreme example. Top-K become the bucket's
//! exemplars.
//!
//! ## Degrade contract (no silent misrepresentation)
//! - A community with fewer than [`MIN_COMMUNITY_SIZE`] CURRENT members is
//!   too small a sample to trust at all: `profile: "unstable"`, no exemplars,
//!   `facts` is the all-`None`/default [`ModalFacts`].
//! - A `(community, kind)` bucket with fewer than [`MIN_BUCKET_SIZE`] members
//!   still returns whatever exemplars exist (up to the bucket size), but the
//!   profile is flagged `sparse: true`.
//! - When the embeddings endpoint/vector batch call fails (or returns nothing
//!   usable), exemplar selection for that bucket falls back to
//!   centrality-only ranking (`rank` desc, then `degree` desc, then `id` asc)
//!   and the profile is flagged `degraded: true` — every OTHER bucket in the
//!   same profile is unaffected.
//! - Every distribution is filtered to CURRENT nodes only
//!   (`graph.current_nodes()`, KGRAPH-15 bi-temporal view) — an invalidated
//!   symbol never appears as an exemplar or skews a modal fact.
//!
//! ## Caching (by graph generation)
//! [`HouseStyleCache`] memoizes a `(project_id, community)` profile keyed by
//! the graph's `build_seq` (KGRAPH-15's monotonic per-project refresh
//! counter — the closest thing the model exposes to a build "generation").
//! A cache hit is returned only when the STORED generation matches the
//! CURRENT graph's `build_seq`; any other generation recomputes and
//! overwrites the entry, so a `scribe_kg_build` rebuild invalidates every
//! profile computed against the prior graph without an explicit eviction
//! pass.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use serde::Serialize;

use crate::cortex::metrics::round4;
use crate::scribe::graph::model::{KgNode, KnowledgeGraph, NodeKind};
use crate::scribe::graph::query::{one_hop_neighbors, EdgeDirection, NeighborFilter};
use crate::scribe::graph::vec_embed::{node_card, EmbedClient};

/// A community with fewer than this many CURRENT members is "brand-new/
/// unstable" — too small a sample to trust its modal facts at all.
pub const MIN_COMMUNITY_SIZE: usize = 2;

/// A `(community, kind)` bucket with fewer than this many members still
/// returns what exists, but flags the profile `sparse: true`.
pub const MIN_BUCKET_SIZE: usize = 3;

/// Default `K` — exemplars per kind per community — when
/// `CORTEX_HOUSE_STYLE_K` is unset/unparseable/zero.
pub const DEFAULT_EXEMPLARS_K: usize = 3;

/// Function names this repo's own config-read idiom uses (see
/// `CortexConfig::from_env`, `EmbedClient::from_env`,
/// `AtlasVecStore::from_env`) — a graph-metadata-only proxy for "does this
/// community read its config the same way the rest of the crate does."
const FROM_ENV_FN_NAME: &str = "from_env";

/// The 4 method names a `RustTool` implementation always defines
/// (`src/tool.rs`) — used purely as a name-set signature within one file's
/// member functions, never a source/AST inspection.
const RUST_TOOL_METHODS: [&str; 4] = ["name", "description", "parameters", "execute"];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One selected exemplar member of a `(community, kind)` bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExemplarRef {
    pub node_id: String,
    pub kind: &'static str,
    /// Repo-relative source path — combined with `span` this is the
    /// "file:line" reference CXEG-07 cites.
    pub path: String,
    pub span: Option<(u32, u32)>,
    pub rank: f32,
    /// Selection score: cosine similarity (`0.0..=1.0`-ish) to the bucket's
    /// centroid embedding when embeddings were available for this bucket, or
    /// the raw `rank` value in the `degraded: true` centrality-only
    /// fallback. The two modes are NOT comparable to each other — always
    /// read `score` alongside the profile's own `degraded` flag.
    pub score: f64,
}

/// Deterministic, no-LLM modal facts for one community.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModalFacts {
    /// The most common `NodeKind` among the community's current members
    /// (ties broken toward the lexicographically/declaration-order-earlier
    /// kind, for determinism). `None` for an empty community.
    pub dominant_kind: Option<&'static str>,
    /// A struct/enum member whose name contains `"Error"` — the community's
    /// own error-type idiom, if any (deterministic: lowest node id wins a
    /// tie). `None` when no such member exists.
    pub dominant_error_type: Option<String>,
    /// Whether the community contains a function member named exactly
    /// `"from_env"` — this crate's own config-read constructor idiom.
    pub config_read_idiom: &'static str, // "from_env" | "none_detected"
    /// Whether some single file among the community's members defines all 4
    /// `RustTool` method names (`name`/`description`/`parameters`/
    /// `execute`) — i.e. the community contains at least one live tool impl.
    pub rust_tool_shape_present: bool,
}

impl Default for ModalFacts {
    /// The "nothing detected" fact set — deliberately NOT `#[derive(Default)]`:
    /// a derived default would leave `config_read_idiom` as `""`, not the
    /// documented `"none_detected"` sentinel every OTHER code path returns
    /// for "no `from_env()` idiom found."
    fn default() -> Self {
        ModalFacts {
            dominant_kind: None,
            dominant_error_type: None,
            config_read_idiom: "none_detected",
            rust_tool_shape_present: false,
        }
    }
}

/// A community's full house-style profile: modal facts plus per-kind
/// exemplars, cached by graph generation (see module doc).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HouseStyleProfile {
    pub project_id: String,
    pub community: u32,
    /// The graph's `build_seq` this profile was computed against — the cache
    /// key's generation component.
    pub generation: u64,
    pub member_count: usize,
    /// `"ready"` (usable facts + exemplars) or `"unstable"` (too few
    /// members — see [`MIN_COMMUNITY_SIZE`]).
    pub profile: &'static str,
    /// `true` when at least one `(community, kind)` bucket had fewer than
    /// [`MIN_BUCKET_SIZE`] members.
    pub sparse: bool,
    /// `true` when at least one bucket's embedding-based selection was
    /// unavailable and fell back to centrality-only ranking.
    pub degraded: bool,
    pub facts: ModalFacts,
    /// `NodeKind::as_str()` -> that kind's exemplars, only for kinds present
    /// in the community. Empty when `profile == "unstable"`.
    pub exemplars_by_kind: BTreeMap<String, Vec<ExemplarRef>>,
}

// ---------------------------------------------------------------------------
// Modal facts (pure, sync, graph-metadata-only)
// ---------------------------------------------------------------------------

/// Compute [`ModalFacts`] for a set of CURRENT community members. Pure and
/// deterministic — no I/O, no source-text read.
fn modal_facts(members: &[&KgNode]) -> ModalFacts {
    if members.is_empty() {
        return ModalFacts::default();
    }

    // Dominant kind: BTreeMap<NodeKind, _> iterates in NodeKind's declared
    // Ord, and `fold` keeps the FIRST-seen max (strictly-greater update
    // only), so a tie deterministically resolves to the lexicographically
    // earliest kind.
    let mut counts: BTreeMap<NodeKind, usize> = BTreeMap::new();
    for n in members {
        *counts.entry(n.kind).or_default() += 1;
    }
    let dominant_kind = counts
        .into_iter()
        .fold(None::<(NodeKind, usize)>, |acc, (k, c)| match acc {
            Some((_, bc)) if bc >= c => acc,
            _ => Some((k, c)),
        })
        .map(|(k, _)| k.as_str());

    // Dominant error type: lowest-id struct/enum member whose name contains
    // "Error", for a deterministic pick among ties.
    let dominant_error_type = members
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Struct | NodeKind::Enum) && n.name.contains("Error"))
        .min_by(|a, b| a.id.cmp(&b.id))
        .map(|n| n.name.clone());

    let config_read_idiom = if members
        .iter()
        .any(|n| n.kind == NodeKind::Function && n.name == FROM_ENV_FN_NAME)
    {
        "from_env"
    } else {
        "none_detected"
    };

    // RustTool shape: group function member NAMES by their file path, then
    // check whether any one file's function-name set is a superset of the 4
    // RustTool method names.
    let mut fn_names_by_path: HashMap<&str, HashSet<&str>> = HashMap::new();
    for n in members {
        if n.kind == NodeKind::Function {
            fn_names_by_path.entry(n.path.as_str()).or_default().insert(n.name.as_str());
        }
    }
    let rust_tool_shape_present = fn_names_by_path
        .values()
        .any(|names| RUST_TOOL_METHODS.iter().all(|m| names.contains(m)));

    ModalFacts {
        dominant_kind,
        dominant_error_type,
        config_read_idiom,
        rust_tool_shape_present,
    }
}

// ---------------------------------------------------------------------------
// Exemplar selection
// ---------------------------------------------------------------------------

/// L2-normalize-free cosine similarity between two same-ish-length vectors
/// (walks the shorter length; mismatched dims are not expected in practice
/// since every card in one batch comes from the same embedding model/call).
/// Returns `0.0` for a degenerate (zero-norm) input rather than dividing by
/// zero.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for i in 0..n {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Component-wise mean of a non-empty batch of equal-length vectors.
/// Vectors shorter than the first are treated as zero-padded for the
/// remaining components (defensive only — a real embedding batch is always
/// uniform-dimension).
fn centroid(vectors: &[Vec<f32>]) -> Vec<f32> {
    let dim = vectors.iter().map(Vec::len).max().unwrap_or(0);
    let mut sum = vec![0.0f32; dim];
    for v in vectors {
        for (i, x) in v.iter().enumerate() {
            sum[i] += *x;
        }
    }
    let n = vectors.len() as f32;
    sum.into_iter().map(|x| x / n).collect()
}

/// Centrality-only fallback ranking: `rank` desc, then `degree` desc, then
/// `id` asc for a fully deterministic order. Used both when embeddings are
/// unavailable and (implicitly, as the natural tie-break) it never needs to
/// call out to any I/O.
fn rank_by_centrality<'a>(nodes: &[&'a KgNode]) -> Vec<&'a KgNode> {
    let mut sorted: Vec<&KgNode> = nodes.to_vec();
    sorted.sort_by(|a, b| b.rank.total_cmp(&a.rank).then(b.degree.cmp(&a.degree)).then(a.id.cmp(&b.id)));
    sorted
}

/// Build the same `node_card` text `scribe_kg_build`'s embedding pipeline and
/// `metrics::semantic_duplication_signals` use for a node: its current
/// 1-hop callers/callees resolved via the shared [`one_hop_neighbors`] walk.
fn card_for(graph: &KnowledgeGraph, n: &KgNode) -> String {
    let neighbors = one_hop_neighbors(graph, &n.id, NeighborFilter::Both);
    let mut callers: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();
    for nb in &neighbors {
        let Some(nb_node) = graph.get_node(&nb.id).filter(|x| x.valid_to.is_none()) else { continue };
        match nb.direction {
            EdgeDirection::Incoming => callers.push(nb_node.name.clone()),
            EdgeDirection::Outgoing => callees.push(nb_node.name.clone()),
        }
    }
    let caller_refs: Vec<&str> = callers.iter().map(String::as_str).collect();
    let callee_refs: Vec<&str> = callees.iter().map(String::as_str).collect();
    node_card(n, &caller_refs, &callee_refs)
}

/// Select up to `k` exemplars from one `(community, kind)` bucket. Returns
/// `(exemplars, degraded)` — `degraded` is `true` iff the embedding path was
/// unavailable/unusable and the centrality-only fallback was used instead.
async fn select_exemplars(graph: &KnowledgeGraph, nodes: &[&KgNode], k: usize) -> (Vec<ExemplarRef>, bool) {
    if nodes.is_empty() || k == 0 {
        return (Vec::new(), false);
    }

    // Deterministic embedding order (id asc) so a tied-similarity output is
    // stable across runs regardless of the caller's input ordering.
    let mut ordered: Vec<&KgNode> = nodes.to_vec();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let cards: Vec<String> = ordered.iter().map(|n| card_for(graph, n)).collect();
    let client = EmbedClient::from_env();

    match client.embed_batch(&cards).await {
        Ok(vectors) if vectors.len() == ordered.len() && vectors.iter().any(|v| !v.is_empty()) => {
            let centroid_vec = centroid(&vectors);
            let mut scored: Vec<(f64, &KgNode)> = ordered
                .iter()
                .zip(vectors.iter())
                .map(|(n, v)| (cosine(v, &centroid_vec), *n))
                .collect();
            scored.sort_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then_with(|| b.1.rank.total_cmp(&a.1.rank))
                    .then_with(|| a.1.id.cmp(&b.1.id))
            });
            let exemplars = scored
                .into_iter()
                .take(k)
                .map(|(score, n)| ExemplarRef {
                    node_id: n.id.clone(),
                    kind: n.kind.as_str(),
                    path: n.path.clone(),
                    span: n.span,
                    rank: n.rank,
                    score: round4(score),
                })
                .collect();
            (exemplars, false)
        }
        Ok(_) => (fallback_exemplars(&ordered, k), true),
        Err(e) => {
            tracing::warn!(
                "cortex house_style: embedding batch unavailable for {} node(s) \
                 (falling back to centrality-only exemplar selection): {e}",
                ordered.len()
            );
            (fallback_exemplars(&ordered, k), true)
        }
    }
}

fn fallback_exemplars(nodes: &[&KgNode], k: usize) -> Vec<ExemplarRef> {
    rank_by_centrality(nodes)
        .into_iter()
        .take(k)
        .map(|n| ExemplarRef {
            node_id: n.id.clone(),
            kind: n.kind.as_str(),
            path: n.path.clone(),
            span: n.span,
            rank: n.rank,
            // Round for parity with the embedding path's `round4(cosine)`, so
            // both selection modes emit scores at the same precision.
            score: round4(n.rank as f64),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Profile computation (the async entry point)
// ---------------------------------------------------------------------------

/// Compute a fresh [`HouseStyleProfile`] for `community` in `project_id`
/// against the given (already-loaded) `graph`. Does not consult or populate
/// [`HouseStyleCache`] — callers that want caching go through
/// [`HouseStyleCache::get_or_compute`].
pub async fn compute_profile(graph: &KnowledgeGraph, project_id: &str, community: u32, k: usize) -> HouseStyleProfile {
    let members: Vec<&KgNode> = graph.current_nodes().filter(|n| n.cluster == Some(community)).collect();
    let member_count = members.len();
    let generation = graph.build_seq;

    if member_count < MIN_COMMUNITY_SIZE {
        return HouseStyleProfile {
            project_id: project_id.to_string(),
            community,
            generation,
            member_count,
            profile: "unstable",
            sparse: member_count > 0 && member_count < MIN_BUCKET_SIZE,
            degraded: false,
            facts: ModalFacts::default(),
            exemplars_by_kind: BTreeMap::new(),
        };
    }

    let facts = modal_facts(&members);

    let mut by_kind: BTreeMap<NodeKind, Vec<&KgNode>> = BTreeMap::new();
    for &n in &members {
        by_kind.entry(n.kind).or_default().push(n);
    }

    let mut sparse = false;
    let mut degraded = false;
    let mut exemplars_by_kind: BTreeMap<String, Vec<ExemplarRef>> = BTreeMap::new();
    for (kind, nodes) in &by_kind {
        if nodes.len() < MIN_BUCKET_SIZE {
            sparse = true;
        }
        let (exemplars, kind_degraded) = select_exemplars(graph, nodes, k).await;
        degraded |= kind_degraded;
        exemplars_by_kind.insert(kind.as_str().to_string(), exemplars);
    }

    HouseStyleProfile {
        project_id: project_id.to_string(),
        community,
        generation,
        member_count,
        profile: "ready",
        sparse,
        degraded,
        facts,
        exemplars_by_kind,
    }
}

/// All distinct current community/cluster ids present in `graph`, ascending.
pub fn current_communities(graph: &KnowledgeGraph) -> BTreeSet<u32> {
    graph.current_nodes().filter_map(|n| n.cluster).collect()
}

// ---------------------------------------------------------------------------
// Cache (keyed by graph generation)
// ---------------------------------------------------------------------------

/// Process-local memoization of `(project_id, community) -> HouseStyleProfile`,
/// keyed additionally by the graph's `build_seq` so a `scribe_kg_build`
/// rebuild (which bumps `build_seq`) transparently invalidates every stale
/// entry on next access — no explicit eviction pass needed. See module doc's
/// "Caching" section.
#[derive(Default)]
pub struct HouseStyleCache {
    entries: Mutex<HashMap<(String, u32), (u64, HouseStyleProfile)>>,
}

impl HouseStyleCache {
    pub fn new() -> Self {
        HouseStyleCache { entries: Mutex::new(HashMap::new()) }
    }

    /// Return the cached profile for `(project_id, community)` if its stored
    /// generation matches `graph.build_seq`; otherwise recompute, cache, and
    /// return the fresh profile.
    pub async fn get_or_compute(&self, graph: &KnowledgeGraph, project_id: &str, community: u32, k: usize) -> HouseStyleProfile {
        let key = (project_id.to_string(), community);
        let generation = graph.build_seq;

        if let Some(hit) = self.cached_if_current(&key, generation) {
            return hit;
        }

        let profile = compute_profile(graph, project_id, community, k).await;
        if let Ok(mut guard) = self.entries.lock() {
            guard.insert(key, (generation, profile.clone()));
        }
        profile
    }

    fn cached_if_current(&self, key: &(String, u32), generation: u64) -> Option<HouseStyleProfile> {
        let guard = self.entries.lock().ok()?;
        let (gen, profile) = guard.get(key)?;
        if *gen == generation {
            Some(profile.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force `EmbedClient::from_env()` to fail fast and deterministically,
    /// regardless of the test host: `EMBEDDINGS_URL` otherwise defaults to
    /// `crate::config::chord_personal_federation_url()` (`:8099`), which is a
    /// REAL, reachable Chord embeddings proxy on the <host> build host this  // pii-test-fixture: doc comment inside #[cfg(test)] fixture helper, no real service reached
    /// suite runs on (per project infra docs) — a unit test must never
    /// silently round-trip to a live production service. Every test below
    /// that reaches [`select_exemplars`]/[`compute_profile`]/the cache calls
    /// this first and is marked `#[serial_test::serial]` (env var mutation is
    /// process-global), matching the port-1-refused fixture convention
    /// `scribe::graph::vec_embed`'s own tests already use.
    fn force_embeddings_unreachable() {
        std::env::set_var("EMBEDDINGS_URL", "http://127.0.0.1:1/v1/embeddings"); // pii-test-fixture
    }

    fn clear_embeddings_override() {
        std::env::remove_var("EMBEDDINGS_URL");
    }

    fn node(id: &str, kind: NodeKind, path: &str, cluster: u32) -> KgNode {
        let mut n = KgNode::new(id, kind, id.rsplit("::").next().unwrap_or(id), path);
        n.cluster = Some(cluster);
        n
    }

    /// A fixture with two clearly-different-idiom communities:
    /// - community 1 (`src/pg/*.rs`): a `PgError` enum, a `from_env`
    ///   constructor, and a full `RustTool` shape in `tool.rs`.
    /// - community 2 (`src/other/*.rs`): plain functions, no error type, no
    ///   `from_env`, no `RustTool` shape.
    fn fixture() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        // community 1
        g.insert_node(node("crate::pg::PgError", NodeKind::Enum, "src/pg/error.rs", 1));
        let mut from_env = node("crate::pg::Config::from_env", NodeKind::Function, "src/pg/config.rs", 1);
        from_env.name = "from_env".to_string();
        g.insert_node(from_env);
        for (fname, path) in [
            ("name", "src/pg/tool.rs"),
            ("description", "src/pg/tool.rs"),
            ("parameters", "src/pg/tool.rs"),
            ("execute", "src/pg/tool.rs"),
        ] {
            let mut n = node(&format!("crate::pg::PgTool::{fname}"), NodeKind::Function, path, 1);
            n.name = fname.to_string();
            g.insert_node(n);
        }
        // community 2 — plain, different idiom
        for i in 0..5 {
            g.insert_node(node(&format!("crate::other::f{i}"), NodeKind::Function, "src/other/x.rs", 2));
        }
        g.recompute_degrees();
        g
    }

    // ── modal_facts ──────────────────────────────────────────────────────

    #[test]
    fn modal_facts_empty_is_default() {
        let facts = modal_facts(&[]);
        assert_eq!(facts, ModalFacts::default());
        assert_eq!(facts.config_read_idiom, "none_detected");
        assert!(!facts.rust_tool_shape_present);
    }

    #[test]
    fn modal_facts_detects_error_type_from_env_and_rust_tool_shape() {
        let g = fixture();
        let members: Vec<&KgNode> = g.current_nodes().filter(|n| n.cluster == Some(1)).collect();
        let facts = modal_facts(&members);
        assert_eq!(facts.dominant_error_type.as_deref(), Some("PgError"));
        assert_eq!(facts.config_read_idiom, "from_env");
        assert!(facts.rust_tool_shape_present);
    }

    #[test]
    fn modal_facts_differ_by_community_house_style_is_scoped() {
        let g = fixture();
        let c1: Vec<&KgNode> = g.current_nodes().filter(|n| n.cluster == Some(1)).collect();
        let c2: Vec<&KgNode> = g.current_nodes().filter(|n| n.cluster == Some(2)).collect();
        let f1 = modal_facts(&c1);
        let f2 = modal_facts(&c2);
        assert_ne!(f1, f2, "two communities with different idioms must yield different profiles");
        assert_eq!(f2.dominant_error_type, None);
        assert_eq!(f2.config_read_idiom, "none_detected");
        assert!(!f2.rust_tool_shape_present);
    }

    #[test]
    fn modal_facts_dominant_kind_breaks_ties_deterministically() {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(node("crate::a::Foo", NodeKind::Struct, "src/a.rs", 1));
        g.insert_node(node("crate::a::bar", NodeKind::Function, "src/a.rs", 1));
        let members: Vec<&KgNode> = g.current_nodes().collect();
        let a = modal_facts(&members).dominant_kind;
        let b = modal_facts(&members).dominant_kind;
        assert_eq!(a, b, "deterministic across repeated calls");
        // Function < Struct in NodeKind's declared Ord, and both have count
        // 1 here -- fold's strictly-greater update keeps the FIRST seen.
        assert_eq!(a, Some(NodeKind::Function.as_str()));
    }

    // ── exemplar selection (fallback path, no embeddings endpoint) ───────

    #[tokio::test]
    async fn select_exemplars_empty_input_yields_empty_not_degraded() {
        let (out, degraded) = select_exemplars(&KnowledgeGraph::new("TERM"), &[], 3).await;
        assert!(out.is_empty());
        assert!(!degraded);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn select_exemplars_falls_back_to_centrality_when_embeddings_unavailable() {
        force_embeddings_unreachable();
        let mut g = KnowledgeGraph::new("TERM");
        let mut hi = node("crate::a::hi", NodeKind::Function, "src/a.rs", 1);
        hi.rank = 0.9;
        let mut lo = node("crate::a::lo", NodeKind::Function, "src/a.rs", 1);
        lo.rank = 0.1;
        g.insert_node(hi);
        g.insert_node(lo);
        let nodes: Vec<&KgNode> = g.current_nodes().collect();

        // The embeddings endpoint is forced unreachable above -> the client
        // errors out -> fallback path, `degraded: true`.
        let (out, degraded) = select_exemplars(&g, &nodes, 2).await;
        assert!(degraded, "unreachable embeddings endpoint must degrade, not panic/error");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].node_id, "crate::a::hi", "centrality fallback ranks by rank desc");
        assert_eq!(out[0].score, round4(0.9_f32 as f64), "fallback score is the rank, round4'd for parity with the embedding path");

        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn select_exemplars_respects_k_cap() {
        force_embeddings_unreachable();
        let mut g = KnowledgeGraph::new("TERM");
        for i in 0..10 {
            g.insert_node(node(&format!("crate::a::f{i}"), NodeKind::Function, "src/a.rs", 1));
        }
        let nodes: Vec<&KgNode> = g.current_nodes().collect();
        let (out, _) = select_exemplars(&g, &nodes, 3).await;
        assert_eq!(out.len(), 3);
        clear_embeddings_override();
    }

    // ── cosine / centroid ─────────────────────────────────────────────────

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector_is_zero_not_nan() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 1.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    #[test]
    fn centroid_of_uniform_vectors_is_itself() {
        let vs = vec![vec![1.0_f32, 2.0], vec![1.0, 2.0], vec![1.0, 2.0]];
        assert_eq!(centroid(&vs), vec![1.0, 2.0]);
    }

    // ── compute_profile: degrade contract ─────────────────────────────────

    #[tokio::test]
    async fn compute_profile_unstable_below_min_community_size() {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(node("crate::a::solo", NodeKind::Function, "src/a.rs", 9));
        let p = compute_profile(&g, "TERM", 9, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(p.profile, "unstable");
        assert!(p.exemplars_by_kind.is_empty());
        assert_eq!(p.facts, ModalFacts::default());
    }

    #[tokio::test]
    async fn compute_profile_empty_community_is_unstable_not_panicking() {
        let g = KnowledgeGraph::new("TERM");
        let p = compute_profile(&g, "TERM", 42, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(p.profile, "unstable");
        assert_eq!(p.member_count, 0);
        assert!(!p.sparse, "an EMPTY community is unstable, not sparse -- sparse implies partial data");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_profile_flags_sparse_bucket() {
        force_embeddings_unreachable();
        // community 1 has 6 members total but the Enum bucket has exactly 1
        // (PgError) -- below MIN_BUCKET_SIZE (3) -> sparse:true overall.
        let g = fixture();
        let p = compute_profile(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(p.profile, "ready");
        assert!(p.sparse, "the Enum bucket (1 member) is below MIN_BUCKET_SIZE");
        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_profile_filters_invalidated_nodes() {
        force_embeddings_unreachable();
        let mut g = fixture();
        let seq = g.next_build_seq();
        g.invalidate_path("src/pg/error.rs", seq);
        let p = compute_profile(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(p.facts.dominant_error_type, None, "invalidated PgError must not surface as a modal fact");
        assert!(
            !p.exemplars_by_kind.contains_key(NodeKind::Enum.as_str())
                || p.exemplars_by_kind[NodeKind::Enum.as_str()].is_empty(),
            "invalidated enum must not appear as an exemplar"
        );
        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_profile_is_deterministic() {
        force_embeddings_unreachable();
        let g = fixture();
        let a = compute_profile(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        let b = compute_profile(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(a, b);
        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_profile_generation_matches_build_seq() {
        force_embeddings_unreachable();
        let mut g = fixture();
        g.next_build_seq();
        g.next_build_seq();
        let p = compute_profile(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(p.generation, g.build_seq);
        assert_eq!(p.generation, 2);
        clear_embeddings_override();
    }

    // ── current_communities ────────────────────────────────────────────────

    #[test]
    fn current_communities_lists_distinct_clusters_ascending() {
        let g = fixture();
        assert_eq!(current_communities(&g), BTreeSet::from([1, 2]));
    }

    #[test]
    fn current_communities_excludes_invalidated_nodes_cluster() {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(node("crate::a::solo", NodeKind::Function, "src/a.rs", 7));
        let seq = g.next_build_seq();
        g.invalidate_path("src/a.rs", seq);
        assert!(current_communities(&g).is_empty());
    }

    // ── HouseStyleCache ──────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn cache_returns_stale_entry_when_generation_unchanged() {
        force_embeddings_unreachable();
        // Prove caching (not just correctness) by mutating the underlying
        // graph's CONTENT between calls without bumping `build_seq`: if the
        // cache is working, the second call must return the FIRST profile
        // (byte-identical), even though a fresh compute would differ.
        let cache = HouseStyleCache::new();
        let g1 = fixture();
        let first = cache.get_or_compute(&g1, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(first.member_count, 6);

        let mut g2 = fixture();
        // Same build_seq (0) as g1, but with 3 extra community-1 members --
        // a fresh compute would see member_count 9, not 6.
        for i in 0..3 {
            g2.insert_node(node(&format!("crate::pg::extra{i}"), NodeKind::Function, "src/pg/extra.rs", 1));
        }
        assert_eq!(g2.build_seq, g1.build_seq);

        let second = cache.get_or_compute(&g2, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(second, first, "same generation -> cache hit, stale content returned verbatim");
        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cache_recomputes_when_generation_changes() {
        force_embeddings_unreachable();
        let cache = HouseStyleCache::new();
        let mut g = fixture();
        let before = cache.get_or_compute(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(before.generation, 0);

        g.next_build_seq(); // bump generation, no content change needed to prove recompute happened
        let after = cache.get_or_compute(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        assert_eq!(after.generation, 1);
        assert_ne!(after.generation, before.generation);
        clear_embeddings_override();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cache_keys_are_independent_per_community_and_project() {
        force_embeddings_unreachable();
        let cache = HouseStyleCache::new();
        let g = fixture();
        let c1 = cache.get_or_compute(&g, "TERM", 1, DEFAULT_EXEMPLARS_K).await;
        let c2 = cache.get_or_compute(&g, "TERM", 2, DEFAULT_EXEMPLARS_K).await;
        assert_ne!(c1.community, c2.community);
        assert_ne!(c1.facts, c2.facts);
        clear_embeddings_override();
    }
}
