//! Atlas community detection (KGRAPH-05).
//!
//! Assigns each node a `cluster` id via modularity-maximizing community
//! detection over the graph topology alone — no embeddings, no vector store
//! (Graphify-style: clustering is a property of how the code connects, not of
//! any semantic vectors). Populates [`KgNode::cluster`]; degree is filled by the
//! model's `recompute_degrees`.
//!
//! We run the local-moving phase of **Louvain** (one level). A Leiden crate is
//! not vendored in the local registry, so Louvain is the spec-sanctioned
//! fallback; hierarchical/multi-level communities are KGRAPH-12. The pass is
//! deterministic — nodes are visited in the graph's stable (BTreeMap-sorted) id
//! order, ties break toward the lowest community id, and final community ids are
//! renumbered by first appearance in that order — so identical input always
//! yields identical cluster ids (reproducible docs/tests).
//!
//! Pure computation: no I/O, networking, or secrets.

use std::collections::HashMap;

use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::visit::EdgeRef;

use super::model::{EdgeKind, KnowledgeGraph};

/// Edges that count as topological "coupling" for clustering. `Contains` is
/// excluded on purpose: every item is Contained by its module, which would
/// collapse each file into one trivial community and drown out the call/import
/// structure we actually want to cluster on.
fn couples(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls | EdgeKind::Imports | EdgeKind::References | EdgeKind::RelatedTo | EdgeKind::Documents
    )
}

/// Assign a community id to every node (writing `KgNode.cluster`). Isolated
/// nodes (no coupling edges) each become their own singleton community.
pub fn cluster(graph: &mut KnowledgeGraph) {
    // Stable id order.
    let ids: Vec<String> = graph.nodes().map(|n| n.id.clone()).collect();
    if ids.is_empty() {
        return;
    }
    let index_of: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let n = ids.len();

    // Build an undirected weighted graph (accumulate parallel/opposite edges).
    let mut g: UnGraph<(), f64> = UnGraph::with_capacity(n, graph.edge_count());
    let nodes: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
    // pair-weight accumulation keyed by ordered (min,max) index
    let mut pair_w: HashMap<(usize, usize), f64> = HashMap::new();
    for e in graph.edges() {
        if !couples(e.kind) {
            continue;
        }
        let (Some(&a), Some(&b)) = (index_of.get(e.from.as_str()), index_of.get(e.to.as_str())) else {
            continue;
        };
        if a == b {
            continue; // no self-loops
        }
        let key = if a < b { (a, b) } else { (b, a) };
        *pair_w.entry(key).or_insert(0.0) += 1.0;
    }
    for ((a, b), w) in &pair_w {
        g.add_edge(nodes[*a], nodes[*b], *w);
    }

    let comm = louvain_local_moving(&g);

    // Renumber communities by first appearance in id order (determinism).
    let mut remap: HashMap<usize, u32> = HashMap::new();
    let mut next: u32 = 0;
    for i in 0..n {
        let c = comm[i];
        let id = *remap.entry(c).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        // write back
        // (ids[i] is the node id; look it up mutably)
        set_cluster(graph, &ids[i], id);
    }
}

/// Set a node's cluster id in place (helper so the public model stays minimal).
fn set_cluster(graph: &mut KnowledgeGraph, id: &str, cluster: u32) {
    // KnowledgeGraph exposes nodes() immutably; we reinsert an updated clone to
    // set the cluster (insert_node merges, preserving other fields).
    if let Some(node) = graph.get_node(id) {
        let mut updated = node.clone();
        updated.cluster = Some(cluster);
        graph.insert_node(updated);
    }
}

/// One-level Louvain local-moving. Returns a community index per node index.
pub(crate) fn louvain_local_moving(g: &UnGraph<(), f64>) -> Vec<usize> {
    let n = g.node_count();
    let mut community: Vec<usize> = (0..n).collect();
    if n == 0 {
        return community;
    }

    // Weighted degree per node and total edge weight m.
    let mut k: Vec<f64> = vec![0.0; n];
    let mut two_m = 0.0;
    for e in g.edge_indices() {
        let (a, b) = g.edge_endpoints(e).unwrap();
        let w = *g.edge_weight(e).unwrap();
        k[a.index()] += w;
        k[b.index()] += w;
        two_m += 2.0 * w;
    }
    if two_m == 0.0 {
        // No coupling edges: every node is its own community.
        return community;
    }

    // sigma_tot[c] = sum of degrees of nodes currently in community c.
    let mut sigma_tot: Vec<f64> = k.clone(); // each node alone initially

    // Precompute adjacency (neighbor index -> weight) once.
    let adj: Vec<Vec<(usize, f64)>> = (0..n)
        .map(|i| {
            g.edges(NodeIndex::new(i))
                .map(|er| {
                    let other = if er.source().index() == i {
                        er.target().index()
                    } else {
                        er.source().index()
                    };
                    (other, *er.weight())
                })
                .collect()
        })
        .collect();

    let mut improved = true;
    let mut guard = 0;
    while improved && guard < 100 {
        improved = false;
        guard += 1;
        for i in 0..n {
            let ci = community[i];
            // weight from i into each neighboring community
            let mut w_to: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &adj[i] {
                *w_to.entry(community[j]).or_insert(0.0) += w;
            }
            // remove i from its community
            sigma_tot[ci] -= k[i];

            // best target: maximize w_to[c] - k_i * sigma_tot[c] / (2m).
            // Deterministic tie-break: lowest community id wins.
            let mut best_c = ci;
            let mut best_gain = w_to.get(&ci).copied().unwrap_or(0.0) - k[i] * sigma_tot[ci] / two_m;
            let mut candidates: Vec<usize> = w_to.keys().copied().collect();
            candidates.sort_unstable();
            for &c in &candidates {
                let gain = w_to[&c] - k[i] * sigma_tot[c] / two_m;
                if gain > best_gain + f64::EPSILON {
                    best_gain = gain;
                    best_c = c;
                }
            }
            // reinsert i into chosen community
            sigma_tot[best_c] += k[i];
            if best_c != ci {
                community[i] = best_c;
                improved = true;
            }
        }
    }
    community
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::{Confidence, KgEdge, KgNode, NodeKind};

    fn n(id: &str) -> KgNode {
        KgNode::new(id, NodeKind::Function, id.rsplit("::").next().unwrap(), "src/x.rs")
    }
    fn call(g: &mut KnowledgeGraph, a: &str, b: &str) {
        g.insert_edge(KgEdge::new(a, b, EdgeKind::Calls, Confidence::Extracted)).unwrap();
    }

    #[test]
    fn two_dense_groups_form_two_communities() {
        let mut g = KnowledgeGraph::new("TERM");
        for id in ["a1", "a2", "a3", "b1", "b2", "b3"] {
            g.insert_node(n(id));
        }
        // dense within A
        call(&mut g, "a1", "a2");
        call(&mut g, "a2", "a3");
        call(&mut g, "a3", "a1");
        // dense within B
        call(&mut g, "b1", "b2");
        call(&mut g, "b2", "b3");
        call(&mut g, "b3", "b1");
        // a single bridge
        call(&mut g, "a1", "b1");

        cluster(&mut g);
        let c = |id: &str| g.get_node(id).unwrap().cluster.unwrap();
        assert_eq!(c("a1"), c("a2"));
        assert_eq!(c("a2"), c("a3"));
        assert_eq!(c("b1"), c("b2"));
        assert_eq!(c("b2"), c("b3"));
        assert_ne!(c("a1"), c("b1"), "the two dense groups are distinct communities");
    }

    #[test]
    fn deterministic_across_runs() {
        let build = || {
            let mut g = KnowledgeGraph::new("TERM");
            for id in ["a1", "a2", "b1", "b2"] {
                g.insert_node(n(id));
            }
            call(&mut g, "a1", "a2");
            call(&mut g, "b1", "b2");
            cluster(&mut g);
            g.nodes().map(|x| (x.id.clone(), x.cluster)).collect::<Vec<_>>()
        };
        assert_eq!(build(), build(), "clustering is deterministic");
    }

    #[test]
    fn isolated_and_empty_graphs_do_not_panic() {
        let mut empty = KnowledgeGraph::new("TERM");
        cluster(&mut empty); // no panic

        let mut solo = KnowledgeGraph::new("TERM");
        solo.insert_node(n("only"));
        cluster(&mut solo);
        assert_eq!(solo.get_node("only").unwrap().cluster, Some(0), "lone node is its own community 0");
    }

    #[test]
    fn disconnected_components_get_distinct_communities() {
        let mut g = KnowledgeGraph::new("TERM");
        for id in ["a1", "a2", "b1", "b2"] {
            g.insert_node(n(id));
        }
        call(&mut g, "a1", "a2");
        call(&mut g, "b1", "b2");
        cluster(&mut g);
        let c = |id: &str| g.get_node(id).unwrap().cluster.unwrap();
        assert_eq!(c("a1"), c("a2"));
        assert_eq!(c("b1"), c("b2"));
        assert_ne!(c("a1"), c("b1"));
    }
}
