//! Atlas dual-level query routing (KGRAPH-14, LightRAG-style).
//!
//! A single `kg_query(question)` entry point routes between two retrieval
//! levels based on the question's shape:
//!   - **Entity level** — a specific-symbol question ("where is retry handled",
//!     "what calls send") → the ranked nodes matching the question terms.
//!   - **Community level** — an architectural/subsystem question ("what does the
//!     auth subsystem do", "overall structure") → the community summaries
//!     (KGRAPH-12).
//!
//! The classifier is a pure keyword heuristic (fully testable); answer synthesis
//! (two-tier: a strong model over the gathered context) is done by the caller
//! and is best-effort — with no model available the gathered CONTEXT is returned
//! so the tool is still useful. Retrieval reads only graph metadata.

use serde::Serialize;

use super::community::{hierarchical_communities, Community};
use super::model::KnowledgeGraph;

// ── shared 1-hop neighbor walk (single-source) ───────────────────────────────

/// Direction of a 1-hop edge relative to the anchor node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    /// The anchor is the edge's `from` — an outgoing edge (anchor → neighbor);
    /// for a `Calls` edge the neighbor is a CALLEE of the anchor.
    Outgoing,
    /// The anchor is the edge's `to` — an incoming edge (neighbor → anchor);
    /// for a `Calls` edge the neighbor is a CALLER of the anchor.
    Incoming,
}

/// Direction filter for [`one_hop_neighbors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeighborFilter {
    Out,
    In,
    Both,
}

/// A single 1-hop neighbor of an anchor node: the neighbor's id, the edge's
/// relation kind and confidence tier, and which direction the edge runs
/// relative to the anchor.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub id: String,
    pub kind: &'static str,
    pub confidence: &'static str,
    pub direction: EdgeDirection,
}

/// The single-source 1-hop neighbor walk shared by `kg_neighbors`
/// (`scribe::graph::tools`) and `cortex_scope` (`crate::cortex::scope`) — the
/// one place that iterates a node's incident edges, so the two tools can never
/// drift.
///
/// Iterates the graph's edges ONCE, in the graph's own (insertion) edge order,
/// and yields one [`Neighbor`] per matching edge endpoint: an outgoing entry
/// (`id = edge.to`) when the anchor is the edge's `from`, and an incoming entry
/// (`id = edge.from`) when the anchor is the edge's `to`. A self-loop (an edge
/// from the anchor to itself) therefore yields BOTH an outgoing and an incoming
/// entry — exactly as the original hand-rolled `kg_neighbors` walk did, so its
/// output stays byte-identical after the refactor.
///
/// `filter` restricts to outgoing-only / incoming-only / both. This function
/// does NOT filter bi-temporally invalidated nodes (an edge to a since-removed
/// symbol still appears): `kg_neighbors` intentionally surfaces the raw edge
/// set, so a caller that wants a current-only view (e.g. `cortex_scope`)
/// filters resolved neighbors by `valid_to.is_none()` itself.
pub fn one_hop_neighbors(g: &KnowledgeGraph, node_id: &str, filter: NeighborFilter) -> Vec<Neighbor> {
    let want_out = matches!(filter, NeighborFilter::Out | NeighborFilter::Both);
    let want_in = matches!(filter, NeighborFilter::In | NeighborFilter::Both);
    let mut neighbors = Vec::new();
    for e in g.edges() {
        if want_out && e.from == node_id {
            neighbors.push(Neighbor {
                id: e.to.clone(),
                kind: e.kind.as_str(),
                confidence: e.confidence.as_str(),
                direction: EdgeDirection::Outgoing,
            });
        }
        if want_in && e.to == node_id {
            neighbors.push(Neighbor {
                id: e.from.clone(),
                kind: e.kind.as_str(),
                confidence: e.confidence.as_str(),
                direction: EdgeDirection::Incoming,
            });
        }
    }
    neighbors
}

/// Which retrieval level a question routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryLevel {
    Entity,
    Community,
}

/// A matched entity for entity-level retrieval.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityHit {
    pub id: String,
    pub kind: &'static str,
    pub path: String,
    pub rank: f32,
}

const ARCH_CUES: &[&str] = &[
    "architecture",
    "architectural",
    "subsystem",
    "high level",
    "high-level",
    "overall",
    "overview",
    "how does the",
    "what does the",
    "structure of",
    "organized",
    "organised",
    "components",
    "modules",
    "big picture",
    "design of",
];

/// Route a question to entity- or community-level retrieval from its shape.
pub fn classify(question: &str) -> QueryLevel {
    let q = question.to_lowercase();
    if ARCH_CUES.iter().any(|cue| q.contains(cue)) {
        QueryLevel::Community
    } else {
        QueryLevel::Entity
    }
}

/// Split a question into lowercase word tokens ≥3 chars, minus a few stopwords,
/// for entity matching.
fn tokens(question: &str) -> Vec<String> {
    const STOP: &[&str] = &["the", "what", "where", "which", "how", "does", "and", "for", "that", "with", "are", "is"];
    question
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3 && !STOP.contains(&w.as_str()))
        .collect()
}

/// Entity-level context: nodes whose name or id contains a question token,
/// ranked by PageRank (KGRAPH-13) then degree. Deterministic.
pub fn gather_entity(graph: &KnowledgeGraph, question: &str, limit: usize) -> Vec<EntityHit> {
    let toks = tokens(question);
    if toks.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<&super::model::KgNode> = graph
        .nodes()
        .filter(|n| {
            let name = n.name.to_lowercase();
            let id = n.id.to_lowercase();
            toks.iter().any(|t| name.contains(t) || id.contains(t))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then(b.degree.cmp(&a.degree))
            .then(a.id.cmp(&b.id))
    });
    hits.into_iter()
        .take(limit)
        .map(|n| EntityHit {
            id: n.id.clone(),
            kind: n.kind.as_str(),
            path: n.path.clone(),
            rank: n.rank,
        })
        .collect()
}

/// Community-level context: the community structure (+ any summaries).
pub fn gather_community(graph: &KnowledgeGraph) -> Vec<Community> {
    hierarchical_communities(graph)
}

/// Build the answer-synthesis prompt from the question + a JSON context blob.
pub fn build_answer_prompt(question: &str, level: QueryLevel, context: &str) -> String {
    let lens = match level {
        QueryLevel::Entity => "specific code entities",
        QueryLevel::Community => "subsystem/community summaries",
    };
    format!(
        "Answer the question about a codebase using ONLY the {lens} below (from its knowledge \
graph). Be concise and cite entity/community ids you rely on. If the context is insufficient, \
say so.\n\nQUESTION: {question}\n\nCONTEXT:\n{context}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::cluster::cluster as run_cluster;
    use super::super::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn g() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::retry::backoff", NodeKind::Function, "backoff", "src/retry.rs"));
        g.insert_node(KgNode::new("crate::http::send", NodeKind::Function, "send", "src/http.rs"));
        g.insert_node(KgNode::new("crate::http::Client", NodeKind::Struct, "Client", "src/http.rs"));
        g.insert_edge(KgEdge::new("crate::http::send", "crate::retry::backoff", EdgeKind::Calls, Confidence::Extracted)).unwrap();
        g.recompute_degrees();
        run_cluster(&mut g);
        g
    }

    #[test]
    fn architectural_questions_route_to_community() {
        assert_eq!(classify("what does the auth subsystem do"), QueryLevel::Community);
        assert_eq!(classify("give me an overview of the architecture"), QueryLevel::Community);
        assert_eq!(classify("how does the payment subsystem work"), QueryLevel::Community);
    }

    #[test]
    fn specific_questions_route_to_entity() {
        assert_eq!(classify("where is backoff computed"), QueryLevel::Entity);
        assert_eq!(classify("what calls send"), QueryLevel::Entity);
    }

    #[test]
    fn entity_gather_finds_matching_nodes_ranked() {
        let g = g();
        let hits = gather_entity(&g, "where is retry backoff handled", 10);
        assert!(hits.iter().any(|h| h.id == "crate::retry::backoff"), "matched by token 'backoff'/'retry'");
        // empty/stopword-only question yields nothing
        assert!(gather_entity(&g, "how does the", 10).is_empty());
    }

    #[test]
    fn community_gather_returns_communities() {
        let g = g();
        let comms = gather_community(&g);
        assert!(!comms.is_empty(), "clustered graph has communities");
    }

    #[test]
    fn prompt_names_the_level_and_carries_context() {
        let p = build_answer_prompt("what is X", QueryLevel::Community, "[{\"id\":\"c0\"}]");
        assert!(p.contains("subsystem/community summaries"));
        assert!(p.contains("QUESTION: what is X"));
        assert!(p.contains("CONTEXT:"));
    }

    // ── one_hop_neighbors (shared walk) ─────────────────────────────────────

    #[test]
    fn one_hop_neighbors_splits_incoming_and_outgoing() {
        // send -> backoff (Calls). For `send`, backoff is a CALLEE (outgoing);
        // for `backoff`, send is a CALLER (incoming).
        let g = g();
        let out = one_hop_neighbors(&g, "crate::http::send", NeighborFilter::Both);
        assert!(out
            .iter()
            .any(|n| n.id == "crate::retry::backoff" && n.direction == EdgeDirection::Outgoing && n.kind == "calls"));

        let inc = one_hop_neighbors(&g, "crate::retry::backoff", NeighborFilter::Both);
        assert!(inc
            .iter()
            .any(|n| n.id == "crate::http::send" && n.direction == EdgeDirection::Incoming && n.kind == "calls"));
    }

    #[test]
    fn one_hop_neighbors_filter_restricts_direction() {
        let g = g();
        let out_only = one_hop_neighbors(&g, "crate::http::send", NeighborFilter::Out);
        assert!(out_only.iter().all(|n| n.direction == EdgeDirection::Outgoing));
        assert!(out_only.iter().any(|n| n.id == "crate::retry::backoff"));

        let in_only = one_hop_neighbors(&g, "crate::http::send", NeighborFilter::In);
        assert!(in_only.iter().all(|n| n.direction == EdgeDirection::Incoming));
        // `send` has no incoming edges in this fixture.
        assert!(in_only.is_empty());
    }
}
