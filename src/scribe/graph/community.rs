//! Atlas community summaries + hierarchical communities (KGRAPH-12, GraphRAG).
//!
//! Turns the flat Leiden/Louvain clusters (KGRAPH-05) into a small hierarchy of
//! communities and lets a model write a short summary per community, so a model
//! can answer subsystem/architecture questions ("what does the auth subsystem
//! do") at the right zoom without walking hundreds of nodes.
//!
//! - [`hierarchical_communities`] derives **level 0** communities from
//!   `node.cluster`, and — when there are enough of them — a coarser **level 1**
//!   by running the same Louvain move over the *community super-graph* (each
//!   level-0 community is a super-node; edges weigh inter-community coupling).
//!   Deterministic.
//! - [`community_prompt`] builds a summary prompt from member node NAMES only
//!   (never raw source), and [`set_summary`] applies the model's reply.
//!
//! Communities are computed on demand from the stored graph (no core-model
//! change); summaries are best-effort (skipped when no model is available).

use std::collections::BTreeMap;

use petgraph::graph::{NodeIndex, UnGraph};
use serde::Serialize;

use super::cluster::louvain_local_moving;
use super::model::KnowledgeGraph;

/// A community of nodes at a zoom `level` (0 = finest), with an optional model
/// summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Community {
    pub id: u32,
    pub level: u32,
    pub members: Vec<String>,
    #[serde(default)]
    pub summary: String,
}

/// Level-0 communities (grouped by `node.cluster`) plus, when there are >2 of
/// them, a level-1 grouping over the community super-graph. Deterministic:
/// clusters iterate in sorted id order; member lists are sorted.
pub fn hierarchical_communities(graph: &KnowledgeGraph) -> Vec<Community> {
    // Level 0: group node ids by cluster.
    let mut by_cluster: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut node_cluster: BTreeMap<&str, u32> = BTreeMap::new();
    for n in graph.nodes() {
        if let Some(c) = n.cluster {
            by_cluster.entry(c).or_default().push(n.id.clone());
            node_cluster.insert(n.id.as_str(), c);
        }
    }
    if by_cluster.is_empty() {
        return Vec::new();
    }
    for m in by_cluster.values_mut() {
        m.sort();
    }
    let level0: Vec<(u32, Vec<String>)> = by_cluster.into_iter().collect();

    let mut out: Vec<Community> = level0
        .iter()
        .map(|(c, members)| Community {
            id: *c,
            level: 0,
            members: members.clone(),
            summary: String::new(),
        })
        .collect();

    // Level 1: cluster the community super-graph.
    let k = level0.len();
    if k > 2 {
        // super-node index per level-0 cluster id (sorted order).
        let super_idx: BTreeMap<u32, usize> = level0.iter().enumerate().map(|(i, (c, _))| (*c, i)).collect();
        let mut g: UnGraph<(), f64> = UnGraph::with_capacity(k, 0);
        let sn: Vec<NodeIndex> = (0..k).map(|_| g.add_node(())).collect();
        let mut pair: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        for e in graph.edges() {
            let (Some(&ca), Some(&cb)) = (node_cluster.get(e.from.as_str()), node_cluster.get(e.to.as_str())) else {
                continue;
            };
            let (Some(&ia), Some(&ib)) = (super_idx.get(&ca), super_idx.get(&cb)) else {
                continue;
            };
            if ia == ib {
                continue;
            }
            let key = if ia < ib { (ia, ib) } else { (ib, ia) };
            *pair.entry(key).or_insert(0.0) += 1.0;
        }
        for ((a, b), w) in &pair {
            g.add_edge(sn[*a], sn[*b], *w);
        }
        let labels = louvain_local_moving(&g);
        // Group level-0 communities by their super-label into level-1 communities.
        let mut by_label: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (i, (_, members)) in level0.iter().enumerate() {
            by_label.entry(labels[i]).or_default().extend(members.iter().cloned());
        }
        // Renumber labels deterministically by first appearance (sorted label order).
        for (new_id, (_, mut members)) in by_label.into_iter().enumerate() {
            members.sort();
            out.push(Community {
                id: new_id as u32,
                level: 1,
                members,
                summary: String::new(),
            });
        }
    }

    out
}

/// Build a summary prompt for one community from member node NAMES only (never
/// raw source — the graph stores none).
pub fn community_prompt(community: &Community, graph: &KnowledgeGraph) -> String {
    let mut members = String::new();
    for id in &community.members {
        if let Some(n) = graph.get_node(id) {
            members.push_str(&format!("- {} ({})\n", n.name, n.kind.as_str()));
        }
    }
    format!(
        "These code entities form one community (cluster) of a codebase's knowledge graph. In ONE \
or TWO sentences, describe what this group of entities collectively does / is responsible for. \
Respond with only the sentence(s), no preamble.\n\nENTITIES:\n{members}"
    )
}

/// Apply a model reply as a community's summary (trimmed, single-lined, bounded).
pub fn set_summary(community: &mut Community, reply: &str) {
    let s = reply.trim().replace('\n', " ");
    community.summary = s.chars().take(400).collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::cluster::cluster as run_cluster;
    use super::super::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn node(id: &str) -> KgNode {
        KgNode::new(id, NodeKind::Function, id.rsplit("::").next().unwrap(), "src/x.rs")
    }
    fn call(g: &mut KnowledgeGraph, a: &str, b: &str) {
        g.insert_edge(KgEdge::new(a, b, EdgeKind::Calls, Confidence::Extracted)).unwrap();
    }

    fn two_group_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        for id in ["a1", "a2", "a3", "b1", "b2", "b3"] {
            g.insert_node(node(id));
        }
        call(&mut g, "a1", "a2");
        call(&mut g, "a2", "a3");
        call(&mut g, "a3", "a1");
        call(&mut g, "b1", "b2");
        call(&mut g, "b2", "b3");
        call(&mut g, "b3", "b1");
        call(&mut g, "a1", "b1");
        run_cluster(&mut g);
        g
    }

    #[test]
    fn level0_communities_group_by_cluster() {
        let g = two_group_graph();
        let comms = hierarchical_communities(&g);
        let l0: Vec<&Community> = comms.iter().filter(|c| c.level == 0).collect();
        assert_eq!(l0.len(), 2, "two level-0 communities");
        // members are sorted and partition the clustered nodes
        let total: usize = l0.iter().map(|c| c.members.len()).sum();
        assert_eq!(total, 6);
        assert!(l0.iter().all(|c| c.members.windows(2).all(|w| w[0] <= w[1])), "members sorted");
    }

    #[test]
    fn deterministic() {
        let g = two_group_graph();
        assert_eq!(hierarchical_communities(&g), hierarchical_communities(&g));
    }

    #[test]
    fn empty_graph_has_no_communities() {
        let g = KnowledgeGraph::new("TERM");
        assert!(hierarchical_communities(&g).is_empty());
    }

    #[test]
    fn prompt_uses_names_not_source_and_summary_is_bounded() {
        let g = two_group_graph();
        let comms = hierarchical_communities(&g);
        let p = community_prompt(&comms[0], &g);
        assert!(p.contains("ENTITIES:"));
        assert!(!p.contains("fn "), "no source in prompt");
        let mut c = comms[0].clone();
        set_summary(&mut c, "  This community handles the A subsystem.\nExtra line.  ");
        assert_eq!(c.summary, "This community handles the A subsystem. Extra line.");
        // bounded
        set_summary(&mut c, &"x".repeat(1000));
        assert_eq!(c.summary.len(), 400);
    }
}
