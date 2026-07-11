//! Atlas node ranking (KGRAPH-13): PageRank importance + query-personalized
//! ranking (aider's repo-map idea).
//!
//! `pagerank` writes a global importance score onto every node's `rank` — hubs
//! (heavily called/referenced) outrank leaves — feeding `kg_stats` hotspots and
//! the default `kg_search` ordering. `personalized` biases the teleport vector
//! toward a set of seed nodes (the query terms / current-file symbols) so a
//! search returns results ranked by relevance-to-context, not raw adjacency.
//!
//! Deterministic: nodes are processed in sorted-id order over a fixed iteration
//! count; scores are guarded so a disconnected or empty graph never produces
//! NaN. Pure computation — no I/O, networking, or secrets.

use std::collections::HashMap;

use super::model::KnowledgeGraph;

const DAMPING: f32 = 0.85;
const ITERS: usize = 50;

/// Shared power-iteration. `out` is the adjacency (index → out-neighbor indices);
/// `teleport` is the (normalized) restart distribution. Returns a normalized
/// score per node index.
fn run(n: usize, out: &[Vec<usize>], teleport: &[f32]) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let outdeg: Vec<usize> = out.iter().map(|v| v.len()).collect();
    let mut score = vec![1.0f32 / n as f32; n];
    for _ in 0..ITERS {
        let mut next = vec![0.0f32; n];
        // Dangling mass (nodes with no out-edges) is redistributed by teleport.
        let mut dangling = 0.0f32;
        for i in 0..n {
            if outdeg[i] == 0 {
                dangling += score[i];
            }
        }
        for i in 0..n {
            // restart + redistributed dangling mass, both via teleport
            next[i] = (1.0 - DAMPING) * teleport[i] + DAMPING * dangling * teleport[i];
        }
        for i in 0..n {
            if outdeg[i] == 0 {
                continue;
            }
            let share = DAMPING * score[i] / outdeg[i] as f32;
            for &j in &out[i] {
                next[j] += share;
            }
        }
        score = next;
    }
    // Normalize to sum 1 (guard divide-by-zero).
    let total: f32 = score.iter().sum();
    if total > 0.0 {
        for s in score.iter_mut() {
            *s /= total;
        }
    }
    score
}

/// Build the (sorted ids, index map, out-adjacency) view of the graph's directed
/// edges.
fn adjacency(graph: &KnowledgeGraph) -> (Vec<String>, HashMap<String, usize>, Vec<Vec<usize>>) {
    let mut ids: Vec<String> = graph.nodes().map(|n| n.id.clone()).collect();
    ids.sort_unstable();
    let index: HashMap<String, usize> = ids.iter().cloned().enumerate().map(|(i, s)| (s, i)).collect();
    let mut out = vec![Vec::new(); ids.len()];
    for e in graph.edges() {
        if let (Some(&a), Some(&b)) = (index.get(&e.from), index.get(&e.to)) {
            if a != b {
                out[a].push(b);
            }
        }
    }
    (ids, index, out)
}

/// Compute global PageRank and write each node's `rank`.
pub fn pagerank(graph: &mut KnowledgeGraph) {
    let (ids, _index, out) = adjacency(graph);
    let n = ids.len();
    if n == 0 {
        return;
    }
    let teleport = vec![1.0f32 / n as f32; n];
    let scores = run(n, &out, &teleport);
    for (i, id) in ids.iter().enumerate() {
        if let Some(node) = graph.get_node(id) {
            let mut updated = node.clone();
            updated.rank = scores[i];
            graph.insert_node(updated);
        }
    }
}

/// Query-personalized PageRank: teleport biased toward `seeds` (node ids present
/// in the graph). Returns id → score. An empty/entirely-unknown seed set falls
/// back to uniform teleport (i.e. global PageRank).
pub fn personalized(graph: &KnowledgeGraph, seeds: &[&str]) -> HashMap<String, f32> {
    let (ids, index, out) = adjacency(graph);
    let n = ids.len();
    if n == 0 {
        return HashMap::new();
    }
    let seed_idx: Vec<usize> = seeds.iter().filter_map(|s| index.get(*s).copied()).collect();
    let teleport = if seed_idx.is_empty() {
        vec![1.0f32 / n as f32; n]
    } else {
        let mut t = vec![0.0f32; n];
        let w = 1.0f32 / seed_idx.len() as f32;
        for &i in &seed_idx {
            t[i] = w;
        }
        t
    };
    let scores = run(n, &out, &teleport);
    ids.into_iter().zip(scores).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn node(id: &str) -> KgNode {
        KgNode::new(id, NodeKind::Function, id, "src/x.rs")
    }
    fn edge(g: &mut KnowledgeGraph, a: &str, b: &str) {
        g.insert_edge(KgEdge::new(a, b, EdgeKind::Calls, Confidence::Extracted)).unwrap();
    }

    /// a hub everyone calls should outrank the leaves.
    fn hub_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        for id in ["hub", "l1", "l2", "l3"] {
            g.insert_node(node(id));
        }
        edge(&mut g, "l1", "hub");
        edge(&mut g, "l2", "hub");
        edge(&mut g, "l3", "hub");
        g
    }

    #[test]
    fn pagerank_ranks_hub_above_leaves() {
        let mut g = hub_graph();
        pagerank(&mut g);
        let r = |id: &str| g.get_node(id).unwrap().rank;
        assert!(r("hub") > r("l1"), "hub {} > leaf {}", r("hub"), r("l1"));
    }

    #[test]
    fn pagerank_is_deterministic() {
        let mut a = hub_graph();
        let mut b = hub_graph();
        pagerank(&mut a);
        pagerank(&mut b);
        for id in ["hub", "l1", "l2", "l3"] {
            assert_eq!(a.get_node(id).unwrap().rank, b.get_node(id).unwrap().rank);
        }
    }

    #[test]
    fn personalized_shifts_mass_toward_seed() {
        // two disjoint pairs; seeding one pair should score it higher than the other.
        let mut g = KnowledgeGraph::new("TERM");
        for id in ["a1", "a2", "b1", "b2"] {
            g.insert_node(node(id));
        }
        edge(&mut g, "a1", "a2");
        edge(&mut g, "b1", "b2");
        let scores = personalized(&g, &["a1"]);
        assert!(scores["a1"] + scores["a2"] > scores["b1"] + scores["b2"], "seeded component scores higher");
    }

    #[test]
    fn personalized_empty_seed_falls_back_to_uniform() {
        let g = hub_graph();
        let ppr = personalized(&g, &[]);
        // equals global pagerank (uniform teleport)
        let mut g2 = hub_graph();
        pagerank(&mut g2);
        assert!((ppr["hub"] - g2.get_node("hub").unwrap().rank).abs() < 1e-5);
    }

    #[test]
    fn empty_and_disconnected_no_nan() {
        let mut empty = KnowledgeGraph::new("TERM");
        pagerank(&mut empty); // no panic

        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(node("x"));
        g.insert_node(node("y")); // disconnected, no edges (all dangling)
        pagerank(&mut g);
        for id in ["x", "y"] {
            let r = g.get_node(id).unwrap().rank;
            assert!(r.is_finite(), "{id} rank finite: {r}");
        }
    }
}
