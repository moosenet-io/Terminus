//! Atlas semantic edges (KGRAPH-04): model-proposed INFERRED / AMBIGUOUS edges
//! that a deterministic parser cannot see (e.g. "this module implements the
//! retry policy this other module configures").
//!
//! Two pure halves so the model call stays at the edge of the system and this
//! module is fully unit-testable without a live model:
//!   - [`build_prompt`] turns the graph's node metadata + existing EXTRACTED
//!     edges into a prompt. It sends ONLY names/kinds/paths and the existing
//!     edge list — never raw source (the graph doesn't store source anyway, and
//!     the prompt is built solely from `KgNode`/`KgEdge` metadata).
//!   - [`insert_semantic_edges`] parses the model's JSON reply into
//!     `RelatedTo` edges tagged `Inferred`/`Ambiguous`, dropping anything that
//!     targets an unknown node, is a self-edge, or duplicates a relationship a
//!     deterministic EXTRACTED edge already covers (EXTRACTED wins).
//!
//! The caller (`scribe_kg_build`, opt-in) does the async dispatch between the
//! two; a model failure is best-effort — the EXTRACTED graph is kept as-is.

use serde::Deserialize;

use super::model::{Confidence, EdgeKind, KgEdge, KnowledgeGraph};

/// Cap on candidate edges accepted from one model reply (bounds a runaway reply).
const MAX_CANDIDATES: usize = 300;

/// One candidate relationship as the model is asked to emit it.
#[derive(Debug, Deserialize)]
struct Candidate {
    from: String,
    to: String,
    #[serde(default)]
    confidence: String,
}

/// Build the semantic-enrichment prompt from graph metadata only.
pub fn build_prompt(graph: &KnowledgeGraph) -> String {
    let mut nodes = String::new();
    for n in graph.nodes() {
        nodes.push_str(&format!("- {} ({}) in {}\n", n.id, n.kind.as_str(), n.path));
    }
    let mut edges = String::new();
    for e in graph.edges() {
        edges.push_str(&format!("- {} {} {}\n", e.from, e.kind.as_str(), e.to));
    }
    format!(
        "You are analyzing a code knowledge graph. Below are its nodes (code entities) and the \
edges already extracted deterministically. Propose ADDITIONAL *semantic* relationships that a \
parser cannot see — e.g. one entity implements/configures/validates/complements another — ONLY \
between node ids that appear in the list. Do not restate existing edges. For each, give a \
confidence of \"inferred\" (you are fairly sure) or \"ambiguous\" (a plausible guess).\n\n\
Respond with ONLY a JSON array: [{{\"from\":\"<id>\",\"to\":\"<id>\",\"confidence\":\"inferred|ambiguous\"}}]\n\n\
NODES:\n{nodes}\nEXISTING EDGES:\n{edges}"
    )
}

/// True if a deterministic EXTRACTED edge already connects `from`→`to` (any
/// kind) — in which case a semantic edge between them would be redundant.
fn has_extracted_between(graph: &KnowledgeGraph, from: &str, to: &str) -> bool {
    graph
        .edges()
        .any(|e| e.confidence == Confidence::Extracted && e.from == from && e.to == to)
}

fn confidence_of(s: &str) -> Confidence {
    match s.trim().to_lowercase().as_str() {
        "ambiguous" | "low" | "uncertain" | "maybe" => Confidence::Ambiguous,
        _ => Confidence::Inferred,
    }
}

/// Extract the first top-level JSON array from a possibly-chatty model reply
/// (models sometimes wrap JSON in prose or ``` fences).
fn extract_json_array(reply: &str) -> Option<&str> {
    let start = reply.find('[')?;
    let end = reply.rfind(']')?;
    if end > start {
        Some(&reply[start..=end])
    } else {
        None
    }
}

/// Parse a model reply and insert the semantic edges it proposes. Returns the
/// number of edges actually added. Never errors — a malformed reply simply adds
/// nothing (the semantic pass is best-effort on top of the EXTRACTED graph).
pub fn insert_semantic_edges(graph: &mut KnowledgeGraph, reply: &str) -> usize {
    let Some(json) = extract_json_array(reply) else {
        return 0;
    };
    let Ok(cands) = serde_json::from_str::<Vec<Candidate>>(json) else {
        return 0;
    };
    let mut added = 0;
    for c in cands.into_iter().take(MAX_CANDIDATES) {
        let (from, to) = (c.from.trim(), c.to.trim());
        if from.is_empty() || to.is_empty() || from == to {
            continue;
        }
        // both endpoints must be real nodes
        if graph.get_node(from).is_none() || graph.get_node(to).is_none() {
            continue;
        }
        // don't restate what a deterministic edge already covers
        if has_extracted_between(graph, from, to) {
            continue;
        }
        let edge = KgEdge::new(from, to, EdgeKind::RelatedTo, confidence_of(&c.confidence));
        // insert_edge validates endpoints (both known here); a duplicate
        // RelatedTo is de-duped by the model layer (confidence-max wins).
        if graph.insert_edge(edge).is_ok() {
            added += 1;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::{KgNode, NodeKind};

    fn base() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::retry::Policy", NodeKind::Struct, "Policy", "src/retry.rs"));
        g.insert_node(KgNode::new("crate::http::Client", NodeKind::Struct, "Client", "src/http.rs"));
        g.insert_node(KgNode::new("crate::http::send", NodeKind::Function, "send", "src/http.rs"));
        g.insert_edge(KgEdge::new("crate::http::Client", "crate::http::send", EdgeKind::Contains, Confidence::Extracted)).unwrap();
        g
    }

    #[test]
    fn prompt_contains_nodes_and_no_source() {
        let g = base();
        let p = build_prompt(&g);
        assert!(p.contains("crate::retry::Policy"), "node ids present");
        assert!(p.contains("EXISTING EDGES"), "edges section present");
        // the graph stores no source, so the prompt cannot contain any
        assert!(!p.contains("fn "), "no raw source in prompt");
    }

    #[test]
    fn parses_inferred_and_ambiguous_edges() {
        let mut g = base();
        let reply = r#"Here you go:
[
  {"from":"crate::http::Client","to":"crate::retry::Policy","confidence":"inferred"},
  {"from":"crate::http::send","to":"crate::retry::Policy","confidence":"ambiguous"}
]"#;
        let added = insert_semantic_edges(&mut g, reply);
        assert_eq!(added, 2);
        let e1 = g.edges().find(|e| e.from == "crate::http::Client" && e.to == "crate::retry::Policy").unwrap();
        assert_eq!(e1.kind, EdgeKind::RelatedTo);
        assert_eq!(e1.confidence, Confidence::Inferred);
        let e2 = g.edges().find(|e| e.from == "crate::http::send" && e.to == "crate::retry::Policy").unwrap();
        assert_eq!(e2.confidence, Confidence::Ambiguous);
    }

    #[test]
    fn drops_edge_to_unknown_node() {
        let mut g = base();
        let reply = r#"[{"from":"crate::http::Client","to":"crate::nope::Ghost","confidence":"inferred"}]"#;
        assert_eq!(insert_semantic_edges(&mut g, reply), 0, "unknown target dropped");
    }

    #[test]
    fn drops_self_edge_and_duplicate_of_extracted() {
        let mut g = base();
        let reply = r#"[
          {"from":"crate::http::Client","to":"crate::http::Client","confidence":"inferred"},
          {"from":"crate::http::Client","to":"crate::http::send","confidence":"inferred"}
        ]"#;
        // self-edge dropped; the second duplicates the EXTRACTED Contains edge -> dropped
        assert_eq!(insert_semantic_edges(&mut g, reply), 0);
    }

    #[test]
    fn malformed_reply_adds_nothing() {
        let mut g = base();
        assert_eq!(insert_semantic_edges(&mut g, "the model is confused, no json here"), 0);
        assert_eq!(insert_semantic_edges(&mut g, "[not valid json}"), 0);
    }
}
