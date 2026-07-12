//! CXEG-03: Tier-B structural-elegance metrics engine.
//!
//! A PURE (no LLM), independently-testable scoring library that turns a
//! `cortex_scope` (CXEG-02) blast radius into named structural-elegance
//! signals from the Atlas graph — "does this change quietly make the
//! codebase worse shaped," not "is this correct." Five signals:
//!
//! - [`SignalKind::CentralitySpike`] — a touched node whose PageRank AND
//!   degree both exceed a percentile cut-point of the project's own
//!   distribution (god-object drift).
//! - [`SignalKind::CommunityBoundaryCrossing`] — a touched node's 1-hop edge
//!   crosses into a Leiden community its OWN community has no other edge
//!   into (novel coupling), so pre-existing coupling between two communities
//!   is never re-flagged.
//! - [`SignalKind::SemanticDuplication`] — a touched node's card has an
//!   existing (different) node whose embedding cosine similarity exceeds
//!   `config.dup_cosine`.
//! - [`SignalKind::ComplexitySpike`] — a touched node's line-span size (see
//!   [`complexity_proxy`] — `KgNode` has no dedicated complexity field yet)
//!   exceeds a percentile cut-point.
//! - [`SignalKind::FanOutExplosion`] — a touched node's out-degree exceeds a
//!   percentile cut-point.
//!
//! ## Reuse (S9 single-source)
//! Degree/neighbor walks reuse [`crate::scribe::graph::query::one_hop_neighbors`]
//! — the SAME helper `kg_neighbors` and `cortex_scope` (CXEG-02) use, no second
//! edge-iteration implementation. Semantic duplication reuses
//! [`crate::scribe::graph::vec_embed::EmbedClient`] +
//! [`crate::scribe::graph::vec_embed::node_card`] +
//! [`crate::scribe::graph::vec_store::AtlasVecStore`] — the exact embed+search
//! path `kg_semantic_search` uses, not a re-implementation.
//!
//! ## Percentile self-calibration
//! Every threshold is computed from the PROJECT'S OWN current-node
//! distribution ([`percentile_cutoff`], nearest-rank method) at
//! `config.tier_b_percentile` (default 90th), never a hardcoded absolute —
//! the same absolute PageRank must NOT fire in a repo where it happens to be
//! the median, and must fire in a repo where it's an outlier. See
//! `percentile_self_calibrates` below.
//!
//! ## Bi-temporal filtering
//! Every distribution and every anchor/neighbor lookup filters to CURRENT
//! nodes only (`valid_to.is_none()`, via `graph.current_nodes()` / a
//! `get_node(..).filter(..)` guard on `one_hop_neighbors` results) — an
//! invalidated symbol must never appear in a signal or skew a cut-point
//! (front-loaded from a CXEG-02 review finding).
//!
//! ## Degrade contract
//! [`compute_signals`] is the sole entry point. Its non-semantic detectors
//! ([`centrality_signals`], [`complexity_signals`], [`fan_out_signals`],
//! [`community_boundary_signals`]) are pure sync functions over `&KnowledgeGraph`
//! — no I/O, unit-testable directly with a fixture graph. The ONLY I/O in this
//! module is the embed+vector-search round trip for semantic duplication
//! ([`semantic_duplication_signals`]): when the vector store or embeddings
//! endpoint is unconfigured/unreachable, that ONE signal is silently absent
//! from the result (logged once via `tracing::warn!`) and every other signal
//! is still computed and returned — never a partial error, never a silent cap
//! on the other detectors.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Value};

use crate::cortex::CortexConfig;
use crate::scribe::graph::model::{KgNode, KnowledgeGraph};
use crate::scribe::graph::query::{one_hop_neighbors, NeighborFilter};
use crate::scribe::graph::vec_embed::{node_card, EmbedClient};
use crate::scribe::graph::vec_store::AtlasVecStore;

/// How many existing nodes' cards to compare a touched node's card against
/// for [`SignalKind::SemanticDuplication`] (top-K nearest, excluding self).
const SEMANTIC_TOPK: i64 = 5;

/// A single named structural-elegance finding.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EleganceSignal {
    pub kind: SignalKind,
    /// `0.0..=1.0`-ish magnitude, rounded to 4 decimals for determinism: how
    /// far past the trigger the anchor sits. Not a probability — a relative
    /// "how bad" ranking within one signal kind.
    pub severity: f64,
    /// The node id the signal is anchored to (always a touched node from the
    /// blast radius, never a bystander neighbor).
    pub anchor_node: String,
    /// The anchor's source file — always resolvable (copied straight off the
    /// `KgNode`), so a caller never has to re-look-up the node to locate it.
    pub anchor_file: String,
    /// A deterministic, templated (no-LLM) human-readable explanation. Never
    /// empty — every constructor below fills it from the concrete numbers
    /// that triggered the signal.
    pub why: String,
    /// Signal-specific supporting numbers (percentiles, cosine scores,
    /// community ids, ...), for a caller that wants more than `why`'s prose.
    pub evidence: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    CentralitySpike,
    CommunityBoundaryCrossing,
    SemanticDuplication,
    ComplexitySpike,
    FanOutExplosion,
}

impl SignalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalKind::CentralitySpike => "centrality_spike",
            SignalKind::CommunityBoundaryCrossing => "community_boundary_crossing",
            SignalKind::SemanticDuplication => "semantic_duplication",
            SignalKind::ComplexitySpike => "complexity_spike",
            SignalKind::FanOutExplosion => "fan_out_explosion",
        }
    }
}

/// Round to 4 decimal places — the shared rounding rule every severity/score
/// in this module uses, so run-to-run output is byte-identical (documented
/// determinism contract, see module doc).
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// Relative "how far past the cut-point" severity: `(value - cutoff) /
/// cutoff`, clamped to non-negative. When `cutoff` is `0.0` (a degenerate
/// all-zero distribution) relative growth is undefined, so this falls back
/// to the raw value capped at `1.0` rather than dividing by zero.
fn relative_severity(value: f64, cutoff: f64) -> f64 {
    if cutoff > 0.0 {
        round4(((value - cutoff) / cutoff).max(0.0))
    } else {
        round4(value.min(1.0).max(0.0))
    }
}

/// Nearest-rank percentile cut-point over a value distribution: the smallest
/// value `v` such that at least `pct` percent of `values` are `<= v`.
/// Deterministic (sorts a COPY; ties break identically every run). `pct` is
/// clamped to `[0, 100]`. Returns `None` for an empty distribution — callers
/// treat that as "no data, nothing can fire."
///
/// This is the self-calibration primitive: every detector below calls this
/// over the PROJECT'S OWN current-node distribution for the metric in
/// question, rather than comparing against a hardcoded absolute.
fn percentile_cutoff(values: &[f64], pct: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let pct = pct.clamp(0.0, 100.0);
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    // Nearest-rank: ceil(pct/100 * n), 1-based, clamped into [1, n].
    let n = sorted.len();
    let rank = ((pct / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    Some(sorted[idx])
}

/// Resolve `ids` to their CURRENT (`valid_to.is_none()`) [`KgNode`]s, in the
/// given order, dropping any id that doesn't resolve to a live node
/// (unindexed file, or since-invalidated symbol).
fn resolve_current<'g>(graph: &'g KnowledgeGraph, ids: &[String]) -> Vec<&'g KgNode> {
    ids.iter()
        .filter_map(|id| graph.get_node(id).filter(|n| n.valid_to.is_none()))
        .collect()
}

/// Line-span-based complexity proxy: `KgNode` has no dedicated complexity
/// field (yet), so this uses `(end - start + 1)` from its best-effort
/// `span`, matching the "extractor complexity proxy" the item brief calls
/// for. `None` when the node has no span (nothing to measure).
fn complexity_proxy(n: &KgNode) -> Option<u32> {
    n.span.map(|(start, end)| end.saturating_sub(start) + 1)
}

/// Out-degree via the shared [`one_hop_neighbors`] walk (reuse, not a second
/// edge-iteration) — outgoing-only, so a node's *callee* fan-out specifically
/// (not its total in+out `degree`).
fn out_degree(graph: &KnowledgeGraph, id: &str) -> usize {
    one_hop_neighbors(graph, id, NeighborFilter::Out).len()
}

// ── centrality_spike ────────────────────────────────────────────────────────

fn centrality_signals(touched: &[&KgNode], graph: &KnowledgeGraph, config: &CortexConfig) -> Vec<EleganceSignal> {
    let ranks: Vec<f64> = graph.current_nodes().map(|n| n.rank as f64).collect();
    let degrees: Vec<f64> = graph.current_nodes().map(|n| n.degree as f64).collect();
    let (Some(rank_cut), Some(degree_cut)) = (
        percentile_cutoff(&ranks, config.tier_b_percentile),
        percentile_cutoff(&degrees, config.tier_b_percentile),
    ) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for n in touched {
        let rank = n.rank as f64;
        let degree = n.degree as f64;
        // Strictly greater-than (not >=): a distribution where the value
        // EQUALS its own percentile cut-point (e.g. every node uniform at
        // the same value) must never fire — there is no outlier there.
        if rank > rank_cut && degree > degree_cut {
            let over_rank = relative_severity(rank, rank_cut);
            let over_degree = relative_severity(degree, degree_cut);
            out.push(EleganceSignal {
                kind: SignalKind::CentralitySpike,
                severity: round4(((over_rank + over_degree) / 2.0).max(0.0)),
                anchor_node: n.id.clone(),
                anchor_file: n.path.clone(),
                why: format!(
                    "{} has PageRank {:.4} (above the project's {:.0}th-percentile cut-point {:.4}) \
                     and degree {} (above the {:.0}th-percentile cut-point {:.0}) — a touched \
                     god-object-shaped hub, not a typical leaf/utility node.",
                    n.id, rank, config.tier_b_percentile, rank_cut, n.degree, config.tier_b_percentile, degree_cut
                ),
                evidence: json!({
                    "rank": rank, "rank_cutoff": rank_cut,
                    "degree": n.degree, "degree_cutoff": degree_cut,
                    "percentile": config.tier_b_percentile,
                }),
            });
        }
    }
    out
}

// ── complexity_spike ────────────────────────────────────────────────────────

fn complexity_signals(touched: &[&KgNode], graph: &KnowledgeGraph, config: &CortexConfig) -> Vec<EleganceSignal> {
    let values: Vec<f64> = graph.current_nodes().filter_map(complexity_proxy).map(|v| v as f64).collect();
    let Some(cut) = percentile_cutoff(&values, config.tier_b_percentile) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for n in touched {
        let Some(size) = complexity_proxy(n) else { continue };
        let size_f = size as f64;
        if size_f > cut {
            out.push(EleganceSignal {
                kind: SignalKind::ComplexitySpike,
                severity: relative_severity(size_f, cut),
                anchor_node: n.id.clone(),
                anchor_file: n.path.clone(),
                why: format!(
                    "{} spans {} line(s), above the project's {:.0}th-percentile \
                     line-span cut-point ({:.0}) — a complexity-proxy outlier (KgNode has no \
                     dedicated complexity metric yet, so span size stands in for it).",
                    n.id, size, config.tier_b_percentile, cut
                ),
                evidence: json!({"span_lines": size, "cutoff": cut, "percentile": config.tier_b_percentile}),
            });
        }
    }
    out
}

// ── fan_out_explosion ───────────────────────────────────────────────────────

fn fan_out_signals(touched: &[&KgNode], graph: &KnowledgeGraph, config: &CortexConfig) -> Vec<EleganceSignal> {
    let values: Vec<f64> = graph.current_nodes().map(|n| out_degree(graph, &n.id) as f64).collect();
    let Some(cut) = percentile_cutoff(&values, config.tier_b_percentile) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for n in touched {
        let fan_out = out_degree(graph, &n.id) as f64;
        if fan_out > cut {
            out.push(EleganceSignal {
                kind: SignalKind::FanOutExplosion,
                severity: relative_severity(fan_out, cut),
                anchor_node: n.id.clone(),
                anchor_file: n.path.clone(),
                why: format!(
                    "{} calls/references {} distinct neighbor(s) outward, above the \
                     project's {:.0}th-percentile out-degree cut-point ({:.0}) — this change \
                     touches a node with an unusually wide fan-out.",
                    n.id, fan_out as u32, config.tier_b_percentile, cut
                ),
                evidence: json!({"out_degree": fan_out, "cutoff": cut, "percentile": config.tier_b_percentile}),
            });
        }
    }
    out
}

// ── community_boundary_crossing ─────────────────────────────────────────────

/// Baseline-vs-novel is decided per community pair via
/// [`count_cross_community_edges`]: a pair backed by more than one distinct
/// crossing edge somewhere in the current graph reads as pre-existing/
/// established coupling (never re-flagged); a pair backed by exactly one
/// edge (this one) reads as novel.
fn community_boundary_signals(touched: &[&KgNode], graph: &KnowledgeGraph, _config: &CortexConfig) -> Vec<EleganceSignal> {
    // Track pairs already flagged (per touched anchor + pair) so a node with
    // several neighbors in the same "other" community doesn't fire twice.
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    let mut out = Vec::new();

    for n in touched {
        let Some(my_cluster) = n.cluster else { continue };
        for nb in one_hop_neighbors(graph, &n.id, NeighborFilter::Both) {
            let Some(nb_node) = graph.get_node(&nb.id).filter(|x| x.valid_to.is_none()) else { continue };
            let Some(nb_cluster) = nb_node.cluster else { continue };
            if nb_cluster == my_cluster {
                continue;
            }
            let pair = if my_cluster < nb_cluster { (my_cluster, nb_cluster) } else { (nb_cluster, my_cluster) };
            if !seen.insert((n.id.clone(), pair.0, pair.1)) {
                continue;
            }
            // Count how many DISTINCT edges realize this community pair —
            // used only to decide "pre-existing" vs "novel" wording/evidence.
            let crossing_edge_count = count_cross_community_edges(graph, pair);
            if crossing_edge_count > 1 {
                // Established coupling elsewhere in the graph -- not novel.
                continue;
            }
            out.push(EleganceSignal {
                kind: SignalKind::CommunityBoundaryCrossing,
                severity: round4(1.0),
                anchor_node: n.id.clone(),
                anchor_file: n.path.clone(),
                why: format!(
                    "{} (community {}) is now the only link coupling community {} and community \
                     {} — this change introduces (or is currently the sole carrier of) cross-\
                     community coupling that wasn't otherwise established in the project's \
                     Atlas graph.",
                    n.id, my_cluster, pair.0, pair.1
                ),
                evidence: json!({
                    "from_community": my_cluster, "to_community": nb_cluster,
                    "neighbor": nb_node.id, "crossing_edge_count": crossing_edge_count,
                }),
            });
        }
    }
    out
}

fn count_cross_community_edges(graph: &KnowledgeGraph, pair: (u32, u32)) -> usize {
    graph
        .edges()
        .filter(|e| {
            let (Some(from), Some(to)) = (
                graph.get_node(&e.from).filter(|n| n.valid_to.is_none()),
                graph.get_node(&e.to).filter(|n| n.valid_to.is_none()),
            ) else {
                return false;
            };
            match (from.cluster, to.cluster) {
                (Some(a), Some(b)) if a != b => {
                    let p = if a < b { (a, b) } else { (b, a) };
                    p == pair
                }
                _ => false,
            }
        })
        .count()
}

// ── semantic_duplication ────────────────────────────────────────────────────

/// The pure, unit-testable half of duplication detection: given a touched
/// node's already-fetched top-K nearest-neighbor hits (node_id, cosine
/// score), decide whether the BEST hit (excluding the anchor's own id) fires
/// [`SignalKind::SemanticDuplication`]. Kept separate from the async
/// embed+query round trip ([`semantic_duplication_signals`]) so the
/// decision logic is testable with a fixture hit list and no live vector
/// store.
fn duplication_signal_from_hits(anchor: &KgNode, hits: &[(String, f32)], dup_cosine: f64) -> Option<EleganceSignal> {
    let best = hits
        .iter()
        .filter(|(id, _)| id != &anchor.id)
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let (dup_id, score) = best;
    let score = *score as f64;
    if score < dup_cosine {
        return None;
    }
    Some(EleganceSignal {
        kind: SignalKind::SemanticDuplication,
        severity: round4((score - dup_cosine).max(0.0)),
        anchor_node: anchor.id.clone(),
        anchor_file: anchor.path.clone(),
        why: format!(
            "{}'s card is {:.4} cosine-similar to existing node {} (>= the {:.2} \
             dup_cosine threshold) — this change may be re-implementing something that \
             already exists rather than reusing it.",
            anchor.id, score, dup_id, dup_cosine
        ),
        evidence: json!({"nearest": dup_id, "cosine": score, "dup_cosine": dup_cosine}),
    })
}

/// The async half: for each touched node, build its card the same way
/// `scribe_kg_build`'s embedding pipeline does ([`node_card`] over its
/// current callers/callees), embed it, and query the vector store's
/// top-K nearest existing cards ([`AtlasVecStore::query_topk`]) — the exact
/// path `kg_semantic_search` uses, not a re-implementation.
///
/// Degrades cleanly and SILENTLY (one `tracing::warn!`, not per-node) to "no
/// semantic_duplication signals" when the vector store or embeddings
/// endpoint is unconfigured or unreachable — every other Tier-B detector
/// still runs; this is the only I/O in the whole module.
async fn semantic_duplication_signals(touched: &[&KgNode], graph: &KnowledgeGraph, project_id: &str, config: &CortexConfig) -> Vec<EleganceSignal> {
    let store = match AtlasVecStore::from_env().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "cortex metrics: semantic_duplication skipped for project '{project_id}' — \
                 vector store unavailable: {e}"
            );
            return Vec::new();
        }
    };
    let client = EmbedClient::from_env();

    let mut out = Vec::new();
    for n in touched {
        let neighbors = one_hop_neighbors(graph, &n.id, NeighborFilter::Both);
        let mut callers: Vec<String> = Vec::new();
        let mut callees: Vec<String> = Vec::new();
        for nb in &neighbors {
            let Some(nb_node) = graph.get_node(&nb.id).filter(|x| x.valid_to.is_none()) else { continue };
            match nb.direction {
                crate::scribe::graph::query::EdgeDirection::Outgoing => callees.push(nb_node.name.clone()),
                crate::scribe::graph::query::EdgeDirection::Incoming => callers.push(nb_node.name.clone()),
            }
        }
        let caller_refs: Vec<&str> = callers.iter().map(String::as_str).collect();
        let callee_refs: Vec<&str> = callees.iter().map(String::as_str).collect();
        let card = node_card(n, &caller_refs, &callee_refs);

        let qvec = match client.embed(&card).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "cortex metrics: semantic_duplication embed failed for '{}' in project \
                     '{project_id}' (skipping this node, continuing others): {e}",
                    n.id
                );
                continue;
            }
        };
        let hits = match store.query_topk(project_id, &qvec, SEMANTIC_TOPK).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    "cortex metrics: semantic_duplication query failed for project \
                     '{project_id}' (skipping remaining nodes): {e}"
                );
                break;
            }
        };
        if let Some(sig) = duplication_signal_from_hits(n, &hits, config.dup_cosine) {
            out.push(sig);
        }
    }
    out
}

// ── entry point ──────────────────────────────────────────────────────────

/// Sort signals into a deterministic, stable order: by `kind`, then by
/// `anchor_node` id. Two runs over the identical graph + blast radius always
/// produce byte-identical output.
fn sort_signals(mut signals: Vec<EleganceSignal>) -> Vec<EleganceSignal> {
    signals.sort_by(|a, b| a.kind.as_str().cmp(b.kind.as_str()).then_with(|| a.anchor_node.cmp(&b.anchor_node)));
    signals
}

/// Compute every Tier-B structural-elegance signal for a `cortex_scope`
/// blast radius. `touched_node_ids` are the TOUCHED (not neighbor) node ids
/// from the blast radius — see `cortex::scope::compute_scope`'s
/// `blast_radius[].role == "touched"` entries; ids that don't resolve to a
/// CURRENT graph node (unindexed file, or since-invalidated symbol) are
/// silently skipped (nothing to score).
///
/// Pure/sync for four of the five signals (no I/O beyond reading `graph`);
/// only [`SignalKind::SemanticDuplication`] does I/O (embed + vector-store
/// query), and degrades to "signal absent" rather than failing the whole
/// call when that I/O is unavailable — see the module doc's degrade
/// contract.
pub async fn compute_signals(touched_node_ids: &[String], graph: &KnowledgeGraph, project_id: &str, config: &CortexConfig) -> Vec<EleganceSignal> {
    let touched = resolve_current(graph, touched_node_ids);

    let mut signals = Vec::new();
    signals.extend(centrality_signals(&touched, graph, config));
    signals.extend(complexity_signals(&touched, graph, config));
    signals.extend(fan_out_signals(&touched, graph, config));
    signals.extend(community_boundary_signals(&touched, graph, config));
    signals.extend(semantic_duplication_signals(&touched, graph, project_id, config).await);

    sort_signals(signals)
}

/// Sync-only entry point over the four non-semantic detectors, for callers
/// (and tests) that want the pure subset without an async runtime or any
/// vector-store dependency. [`compute_signals`] is the full (async)
/// pipeline used by `cortex_review` (CXEG-04).
pub fn compute_structural_signals(touched_node_ids: &[String], graph: &KnowledgeGraph, config: &CortexConfig) -> Vec<EleganceSignal> {
    let touched = resolve_current(graph, touched_node_ids);
    let mut signals = Vec::new();
    signals.extend(centrality_signals(&touched, graph, config));
    signals.extend(complexity_signals(&touched, graph, config));
    signals.extend(fan_out_signals(&touched, graph, config));
    signals.extend(community_boundary_signals(&touched, graph, config));
    sort_signals(signals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{Confidence, EdgeKind, KgEdge, NodeKind};

    fn cfg() -> CortexConfig {
        CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: true,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
        }
    }

    fn node(id: &str, kind: NodeKind, path: &str) -> KgNode {
        KgNode::new(id, kind, id.rsplit("::").next().unwrap_or(id), path)
    }

    // ── percentile_cutoff ────────────────────────────────────────────────

    #[test]
    fn percentile_cutoff_empty_is_none() {
        assert_eq!(percentile_cutoff(&[], 90.0), None);
    }

    #[test]
    fn percentile_cutoff_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        // 90th percentile of 10 values, nearest-rank: ceil(0.9*10)=9 -> v[8]=9.0
        assert_eq!(percentile_cutoff(&v, 90.0), Some(9.0));
        assert_eq!(percentile_cutoff(&v, 50.0), Some(5.0));
        assert_eq!(percentile_cutoff(&v, 100.0), Some(10.0));
    }

    #[test]
    fn percentile_cutoff_is_order_independent() {
        let a = vec![5.0, 1.0, 9.0, 3.0];
        let b = vec![9.0, 3.0, 1.0, 5.0];
        assert_eq!(percentile_cutoff(&a, 75.0), percentile_cutoff(&b, 75.0));
    }

    // ── centrality_spike: fires + self-calibrates ───────────────────────

    fn hub_graph(hub_rank: f32, hub_degree_edges: u32) -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        let mut hub = node("crate::hub::Hub", NodeKind::Struct, "src/hub.rs");
        hub.rank = hub_rank;
        hub.cluster = Some(1);
        g.insert_node(hub);
        for i in 0..20 {
            let mut leaf = node(&format!("crate::leaf::f{i}"), NodeKind::Function, "src/leaf.rs");
            leaf.rank = 0.01;
            leaf.cluster = Some(1);
            g.insert_node(leaf);
        }
        for i in 0..hub_degree_edges {
            g.insert_edge(KgEdge::new("crate::hub::Hub", format!("crate::leaf::f{i}"), EdgeKind::Calls, Confidence::Extracted)).unwrap();
        }
        g.recompute_degrees();
        g
    }

    #[test]
    fn centrality_spike_fires_for_outlier_hub() {
        let g = hub_graph(0.9, 15);
        let touched: Vec<&KgNode> = vec![g.get_node("crate::hub::Hub").unwrap()];
        let sigs = centrality_signals(&touched, &g, &cfg());
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::CentralitySpike);
        assert_eq!(sigs[0].anchor_node, "crate::hub::Hub");
        assert!(!sigs[0].why.is_empty());
    }

    #[test]
    fn centrality_spike_does_not_fire_for_median_node() {
        // Same absolute rank (0.5) as a node that WOULD be an outlier in a
        // low-rank distribution, but here every node shares it -- median,
        // not a spike. Proves the threshold is percentile-relative, not a
        // hardcoded absolute.
        let mut g = KnowledgeGraph::new("TERM");
        for i in 0..20 {
            let mut n = node(&format!("crate::m::f{i}"), NodeKind::Function, "src/m.rs");
            n.rank = 0.5;
            n.cluster = Some(1);
            n.degree = 5;
            g.insert_node(n);
        }
        let touched: Vec<&KgNode> = vec![g.get_node("crate::m::f0").unwrap()];
        let sigs = centrality_signals(&touched, &g, &cfg());
        assert!(sigs.is_empty(), "uniform distribution: nothing is an outlier: {sigs:?}");
    }

    #[test]
    fn percentile_self_calibrates_same_absolute_value_different_outcome() {
        // g1: 0.5 is the ONLY high value among near-zero peers -> outlier -> fires.
        let mut g1 = KnowledgeGraph::new("TERM");
        let mut hub = node("crate::hub::Hub", NodeKind::Struct, "src/hub.rs");
        hub.rank = 0.5;
        hub.cluster = Some(1);
        g1.insert_node(hub);
        for i in 0..20 {
            let mut leaf = node(&format!("crate::leaf::f{i}"), NodeKind::Function, "src/leaf.rs");
            leaf.rank = 0.001;
            leaf.cluster = Some(1);
            g1.insert_node(leaf);
        }
        for i in 0..15 {
            g1.insert_edge(KgEdge::new("crate::hub::Hub", format!("crate::leaf::f{i}"), EdgeKind::Calls, Confidence::Extracted)).unwrap();
        }
        g1.recompute_degrees();

        // g2: every node (including the SAME 0.5) shares 0.5 -> median -> no fire.
        let mut g2 = KnowledgeGraph::new("TERM");
        for i in 0..21 {
            let mut n = node(&format!("crate::m::f{i}"), NodeKind::Function, "src/m.rs");
            n.rank = 0.5;
            n.cluster = Some(1);
            n.degree = 15;
            g2.insert_node(n);
        }

        let touched1: Vec<&KgNode> = vec![g1.get_node("crate::hub::Hub").unwrap()];
        let touched2: Vec<&KgNode> = vec![g2.get_node("crate::m::f0").unwrap()];

        let sigs1 = centrality_signals(&touched1, &g1, &cfg());
        let sigs2 = centrality_signals(&touched2, &g2, &cfg());

        assert_eq!(sigs1.len(), 1, "0.5 is an outlier in g1: {sigs1:?}");
        assert!(sigs2.is_empty(), "0.5 is the median in g2, same absolute value must NOT fire: {sigs2:?}");
    }

    // ── complexity_spike ─────────────────────────────────────────────────

    #[test]
    fn complexity_spike_fires_for_long_span_outlier() {
        let mut g = KnowledgeGraph::new("TERM");
        let mut big = node("crate::big::huge_fn", NodeKind::Function, "src/big.rs").with_span(1, 500);
        big.cluster = Some(1);
        g.insert_node(big);
        for i in 0..20 {
            let mut small = node(&format!("crate::small::f{i}"), NodeKind::Function, "src/small.rs").with_span(1, 5);
            small.cluster = Some(1);
            g.insert_node(small);
        }
        let touched: Vec<&KgNode> = vec![g.get_node("crate::big::huge_fn").unwrap()];
        let sigs = complexity_signals(&touched, &g, &cfg());
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::ComplexitySpike);
        assert!(!sigs[0].why.is_empty());
    }

    #[test]
    fn complexity_spike_skips_nodes_without_span() {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(node("crate::a::foo", NodeKind::Function, "src/a.rs"));
        let touched: Vec<&KgNode> = vec![g.get_node("crate::a::foo").unwrap()];
        let sigs = complexity_signals(&touched, &g, &cfg());
        assert!(sigs.is_empty());
    }

    // ── fan_out_explosion ────────────────────────────────────────────────

    #[test]
    fn fan_out_explosion_fires_for_wide_caller() {
        let mut g = KnowledgeGraph::new("TERM");
        let mut hub = node("crate::hub::wide", NodeKind::Function, "src/hub.rs");
        hub.cluster = Some(1);
        g.insert_node(hub);
        for i in 0..20 {
            let mut leaf = node(&format!("crate::leaf::f{i}"), NodeKind::Function, "src/leaf.rs");
            leaf.cluster = Some(1);
            g.insert_node(leaf);
            g.insert_edge(KgEdge::new("crate::hub::wide", format!("crate::leaf::f{i}"), EdgeKind::Calls, Confidence::Extracted)).unwrap();
        }
        g.recompute_degrees();
        let touched: Vec<&KgNode> = vec![g.get_node("crate::hub::wide").unwrap()];
        let sigs = fan_out_signals(&touched, &g, &cfg());
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::FanOutExplosion);
    }

    // ── community_boundary_crossing ──────────────────────────────────────

    #[test]
    fn community_boundary_crossing_fires_for_novel_single_edge_coupling() {
        let mut g = KnowledgeGraph::new("TERM");
        let mut a = node("crate::a::Foo", NodeKind::Struct, "src/a.rs");
        a.cluster = Some(1);
        let mut b = node("crate::b::Bar", NodeKind::Struct, "src/b.rs");
        b.cluster = Some(2);
        g.insert_node(a);
        g.insert_node(b);
        g.insert_edge(KgEdge::new("crate::a::Foo", "crate::b::Bar", EdgeKind::References, Confidence::Extracted)).unwrap();
        g.recompute_degrees();

        let touched: Vec<&KgNode> = vec![g.get_node("crate::a::Foo").unwrap()];
        let sigs = community_boundary_signals(&touched, &g, &cfg());
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::CommunityBoundaryCrossing);
        assert!(!sigs[0].why.is_empty());
    }

    #[test]
    fn community_boundary_crossing_does_not_refire_established_coupling() {
        let mut g = KnowledgeGraph::new("TERM");
        let mut a1 = node("crate::a::Foo1", NodeKind::Struct, "src/a.rs");
        a1.cluster = Some(1);
        let mut a2 = node("crate::a::Foo2", NodeKind::Struct, "src/a.rs");
        a2.cluster = Some(1);
        let mut b1 = node("crate::b::Bar1", NodeKind::Struct, "src/b.rs");
        b1.cluster = Some(2);
        let mut b2 = node("crate::b::Bar2", NodeKind::Struct, "src/b.rs");
        b2.cluster = Some(2);
        g.insert_node(a1);
        g.insert_node(a2);
        g.insert_node(b1);
        g.insert_node(b2);
        // Two INDEPENDENT edges already couple community 1 <-> 2 -- established.
        g.insert_edge(KgEdge::new("crate::a::Foo1", "crate::b::Bar1", EdgeKind::References, Confidence::Extracted)).unwrap();
        g.insert_edge(KgEdge::new("crate::a::Foo2", "crate::b::Bar2", EdgeKind::References, Confidence::Extracted)).unwrap();
        g.recompute_degrees();

        let touched: Vec<&KgNode> = vec![g.get_node("crate::a::Foo1").unwrap()];
        let sigs = community_boundary_signals(&touched, &g, &cfg());
        assert!(sigs.is_empty(), "pre-existing (>1 edge) coupling must not re-fire: {sigs:?}");
    }

    #[test]
    fn community_boundary_crossing_same_community_never_fires() {
        let mut g = KnowledgeGraph::new("TERM");
        let mut a = node("crate::a::Foo", NodeKind::Struct, "src/a.rs");
        a.cluster = Some(1);
        let mut b = node("crate::a::Baz", NodeKind::Struct, "src/a.rs");
        b.cluster = Some(1);
        g.insert_node(a);
        g.insert_node(b);
        g.insert_edge(KgEdge::new("crate::a::Foo", "crate::a::Baz", EdgeKind::Calls, Confidence::Extracted)).unwrap();
        g.recompute_degrees();

        let touched: Vec<&KgNode> = vec![g.get_node("crate::a::Foo").unwrap()];
        let sigs = community_boundary_signals(&touched, &g, &cfg());
        assert!(sigs.is_empty());
    }

    // ── semantic_duplication: pure decision helper ──────────────────────

    #[test]
    fn duplication_signal_fires_above_threshold() {
        let anchor = node("crate::new::thing", NodeKind::Function, "src/new.rs");
        let hits = vec![("crate::new::thing".to_string(), 1.0), ("crate::old::thing".to_string(), 0.92)];
        let sig = duplication_signal_from_hits(&anchor, &hits, 0.85).unwrap();
        assert_eq!(sig.kind, SignalKind::SemanticDuplication);
        assert_eq!(sig.anchor_node, "crate::new::thing");
        assert!(!sig.why.is_empty());
        assert!(sig.why.contains("crate::old::thing"));
    }

    #[test]
    fn duplication_signal_excludes_self_hit() {
        let anchor = node("crate::new::thing", NodeKind::Function, "src/new.rs");
        // Only the anchor's own (stale) row is nearest -- nothing else to compare.
        let hits = vec![("crate::new::thing".to_string(), 1.0)];
        assert!(duplication_signal_from_hits(&anchor, &hits, 0.85).is_none());
    }

    #[test]
    fn duplication_signal_below_threshold_does_not_fire() {
        let anchor = node("crate::new::thing", NodeKind::Function, "src/new.rs");
        let hits = vec![("crate::old::thing".to_string(), 0.5)];
        assert!(duplication_signal_from_hits(&anchor, &hits, 0.85).is_none());
    }

    // ── compute_signals: embeddings-unavailable degrade ─────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_signals_degrades_when_vector_store_unconfigured() {
        // No ATLAS_DATABASE_URL in a bare test env -> AtlasVecStore::from_env()
        // returns NotConfigured -> semantic_duplication is silently absent,
        // but the other detectors still run against a graph with an obvious
        // centrality outlier.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // a real DSN is live in this process; skip (would attempt a real connect)
        }
        let g = hub_graph(0.9, 15);
        let touched_ids = vec!["crate::hub::Hub".to_string()];
        let sigs = compute_signals(&touched_ids, &g, "TERM", &cfg()).await;
        assert!(sigs.iter().any(|s| s.kind == SignalKind::CentralitySpike), "{sigs:?}");
        assert!(
            sigs.iter().all(|s| s.kind != SignalKind::SemanticDuplication),
            "semantic_duplication must be absent, not erroring, when unconfigured: {sigs:?}"
        );
    }

    // ── determinism ──────────────────────────────────────────────────────

    #[test]
    fn compute_structural_signals_is_deterministic() {
        let g = hub_graph(0.9, 15);
        let touched_ids = vec!["crate::hub::Hub".to_string()];
        let a = compute_structural_signals(&touched_ids, &g, &cfg());
        let b = compute_structural_signals(&touched_ids, &g, &cfg());
        assert_eq!(a, b);
    }

    #[test]
    fn compute_structural_signals_ignores_invalidated_touched_nodes() {
        let mut g = hub_graph(0.9, 15);
        let seq = g.next_build_seq();
        g.invalidate_path("src/hub.rs", seq);
        let touched_ids = vec!["crate::hub::Hub".to_string()];
        let sigs = compute_structural_signals(&touched_ids, &g, &cfg());
        assert!(sigs.is_empty(), "an invalidated touched node must yield no signals: {sigs:?}");
    }

    // ── every emitted signal has a non-empty why + resolvable anchor ────

    #[test]
    fn every_signal_has_nonempty_why_and_resolvable_anchor() {
        let g = hub_graph(0.9, 15);
        let touched_ids = vec!["crate::hub::Hub".to_string()];
        let sigs = compute_structural_signals(&touched_ids, &g, &cfg());
        assert!(!sigs.is_empty());
        for s in &sigs {
            assert!(!s.why.is_empty(), "{s:?}");
            assert!(g.get_node(&s.anchor_node).is_some(), "anchor must resolve in graph: {s:?}");
            assert!(!s.anchor_file.is_empty(), "{s:?}");
        }
    }
}
