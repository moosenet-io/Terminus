//! Atlas knowledge-graph model (KGRAPH-01).
//!
//! The core data types for a project's code knowledge graph: typed nodes, and
//! edges stamped with a relation kind and a *confidence tier* (adopted from
//! Graphify: EXTRACTED / INFERRED / AMBIGUOUS — so a reader and a model both
//! know which links a parser proved versus which a model guessed).
//!
//! Node identity is a **stable, fully-qualified name** (SCIP/Kythe-style), e.g.
//! `crate::scribe::graph::model::KnowledgeGraph::insert_node` — never a
//! file+line, so a reference from any file resolves to exactly one node and an
//! incremental re-index is diffable. Extraction (KGRAPH-02) and the store
//! (KGRAPH-03) build on these types; nothing here does I/O, parsing, or
//! networking.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::ToolError;

/// What a node represents. `DocSection` covers Markdown/prose nodes so the
/// graph can link docs to the code they describe (`EdgeKind::Documents`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Function,
    Struct,
    Enum,
    Trait,
    Class,
    Module,
    DocSection,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Function => "function",
            NodeKind::Struct => "struct",
            NodeKind::Enum => "enum",
            NodeKind::Trait => "trait",
            NodeKind::Class => "class",
            NodeKind::Module => "module",
            NodeKind::DocSection => "doc_section",
        }
    }
}

/// The relationship an edge encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `from` invokes `to`.
    Calls,
    /// `from` imports / `use`s `to`.
    Imports,
    /// `from` names `to` in a non-call position.
    References,
    /// `from` structurally contains `to` (module → item, impl → method).
    Contains,
    /// A `DocSection` documents a code node.
    Documents,
    /// A model-proposed semantic relationship (KGRAPH-04).
    RelatedTo,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::References => "references",
            EdgeKind::Contains => "contains",
            EdgeKind::Documents => "documents",
            EdgeKind::RelatedTo => "related_to",
        }
    }
}

/// How much to trust an edge. EXTRACTED came from a deterministic parse;
/// INFERRED was proposed by a model; AMBIGUOUS is a model proposal the model
/// itself was unsure of. Visual outputs render these as solid / dashed /
/// dotted (KGRAPH-08).
///
/// Variant order matters: `Extracted` sorts lowest, so `<=` comparisons treat
/// it as the "highest confidence, keep it" case in [`KnowledgeGraph::insert_edge`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Extracted,
    Inferred,
    Ambiguous,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Extracted => "extracted",
            Confidence::Inferred => "inferred",
            Confidence::Ambiguous => "ambiguous",
        }
    }
}

/// A graph node. `id` is the stable FQN and the sole identity key; `span` is a
/// best-effort `(start_line, end_line)` for tooling, deliberately NOT part of
/// identity so line churn does not create a "new" node on re-index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KgNode {
    /// Stable fully-qualified name — the identity key.
    pub id: String,
    pub kind: NodeKind,
    /// Short display name (the last FQN segment).
    pub name: String,
    /// Repo-relative source path the node was defined in.
    pub path: String,
    /// Best-effort `(start_line, end_line)`, 1-based. Not part of identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<(u32, u32)>,
    /// Leiden community id (KGRAPH-05); `None` until clustered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<u32>,
    /// PageRank importance (KGRAPH-13); `0.0` until ranked.
    #[serde(default)]
    pub rank: f32,
    /// Degree, filled by the graph on rebuild of the adjacency view.
    #[serde(default)]
    pub degree: u32,
}

impl KgNode {
    pub fn new(id: impl Into<String>, kind: NodeKind, name: impl Into<String>, path: impl Into<String>) -> Self {
        KgNode {
            id: id.into(),
            kind,
            name: name.into(),
            path: path.into(),
            span: None,
            cluster: None,
            rank: 0.0,
            degree: 0,
        }
    }

    pub fn with_span(mut self, start: u32, end: u32) -> Self {
        self.span = Some((start, end));
        self
    }
}

/// A directed, confidence-tagged edge between two node ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KgEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub confidence: Confidence,
}

impl KgEdge {
    pub fn new(from: impl Into<String>, to: impl Into<String>, kind: EdgeKind, confidence: Confidence) -> Self {
        KgEdge {
            from: from.into(),
            to: to.into(),
            kind,
            confidence,
        }
    }

    /// De-dup key: same endpoints + relation. Confidence is intentionally NOT
    /// in the key so a later EXTRACTED edge can upgrade an INFERRED duplicate
    /// (see [`KnowledgeGraph::insert_edge`]).
    fn key(&self) -> (String, String, EdgeKind) {
        (self.from.clone(), self.to.clone(), self.kind)
    }
}

/// A project's knowledge graph. Nodes are keyed by their stable FQN id; edges
/// are de-duplicated by `(from, to, kind)`. Using `BTreeMap` storage keeps
/// iteration deterministic (stable diffs, reproducible tests/docs).
///
/// On the wire it serializes via [`GraphWire`] as `{project_id, generated_at,
/// nodes: [...], edges: [...]}` — arrays, not maps — both because a JSON object
/// cannot have a tuple key and because arrays are the natural, tool-friendly
/// shape for downstream renderers/importers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(into = "GraphWire", from = "GraphWire")]
pub struct KnowledgeGraph {
    pub project_id: String,
    /// Build stamp (RFC3339 or a commit SHA). Set by the store/build; the model
    /// treats it as opaque.
    pub generated_at: String,
    nodes: BTreeMap<String, KgNode>,
    edges: BTreeMap<(String, String, EdgeKind), KgEdge>,
}

/// Serialization shadow for [`KnowledgeGraph`]: flat arrays, deterministically
/// ordered (nodes by id, edges by their sort key) so output is byte-stable.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct GraphWire {
    project_id: String,
    #[serde(default)]
    generated_at: String,
    #[serde(default)]
    nodes: Vec<KgNode>,
    #[serde(default)]
    edges: Vec<KgEdge>,
}

impl From<KnowledgeGraph> for GraphWire {
    fn from(g: KnowledgeGraph) -> Self {
        GraphWire {
            project_id: g.project_id,
            generated_at: g.generated_at,
            nodes: g.nodes.into_values().collect(),
            edges: g.edges.into_values().collect(),
        }
    }
}

impl From<GraphWire> for KnowledgeGraph {
    fn from(w: GraphWire) -> Self {
        let mut g = KnowledgeGraph::new(w.project_id);
        g.generated_at = w.generated_at;
        for n in w.nodes {
            g.nodes.insert(n.id.clone(), n);
        }
        for e in w.edges {
            // Trust a persisted graph's own edges (endpoints were validated on
            // the way in); rebuild the keyed map directly.
            g.edges.insert(e.key(), e);
        }
        g
    }
}

impl KnowledgeGraph {
    pub fn new(project_id: impl Into<String>) -> Self {
        KnowledgeGraph {
            project_id: project_id.into(),
            generated_at: String::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    /// Insert or merge a node. If the id already exists, any already-computed
    /// analysis (`cluster`/`rank`) is preserved unless the incoming node
    /// carries its own. This makes re-extraction idempotent without clobbering
    /// a prior clustering/ranking pass.
    pub fn insert_node(&mut self, mut node: KgNode) {
        if let Some(existing) = self.nodes.get(&node.id) {
            if node.cluster.is_none() {
                node.cluster = existing.cluster;
            }
            if node.rank == 0.0 {
                node.rank = existing.rank;
            }
        }
        self.nodes.insert(node.id.clone(), node);
    }

    /// Insert an edge, rejecting an edge whose endpoints are not both known
    /// nodes (a dangling edge is a bug in the extractor, not silent data).
    /// On a duplicate `(from, to, kind)`, the *higher-confidence* edge wins
    /// (EXTRACTED > INFERRED > AMBIGUOUS), so a deterministic parse always
    /// overrides an earlier guess.
    pub fn insert_edge(&mut self, edge: KgEdge) -> Result<(), ToolError> {
        if !self.nodes.contains_key(&edge.from) {
            return Err(ToolError::InvalidArgument(format!(
                "edge references unknown source node '{}'",
                edge.from
            )));
        }
        if !self.nodes.contains_key(&edge.to) {
            return Err(ToolError::InvalidArgument(format!(
                "edge references unknown target node '{}'",
                edge.to
            )));
        }
        let key = edge.key();
        match self.edges.get(&key) {
            // `Extracted` sorts lowest, so a smaller-or-equal confidence is the
            // equal-or-stronger one: keep what's there.
            Some(existing) if existing.confidence <= edge.confidence => {}
            _ => {
                self.edges.insert(key, edge);
            }
        }
        Ok(())
    }

    pub fn nodes(&self) -> impl Iterator<Item = &KgNode> {
        self.nodes.values()
    }

    pub fn edges(&self) -> impl Iterator<Item = &KgEdge> {
        self.edges.values()
    }

    /// Directly set a node's PageRank score (KGRAPH-13). Unlike re-inserting a
    /// clone through `insert_node` — which keeps the existing rank when the
    /// incoming rank is exactly `0.0` — this always stores the given value, so
    /// an analysis pass can write any score (incl. `0.0`) without the merge
    /// heuristic clobbering it. Returns `false` if the id is unknown.
    pub fn set_rank(&mut self, id: &str, rank: f32) -> bool {
        match self.nodes.get_mut(id) {
            Some(n) => {
                n.rank = rank;
                true
            }
            None => false,
        }
    }

    pub fn get_node(&self, id: &str) -> Option<&KgNode> {
        self.nodes.get(id)
    }

    /// Recompute each node's `degree` (in + out) from the current edge set.
    pub fn recompute_degrees(&mut self) {
        for n in self.nodes.values_mut() {
            n.degree = 0;
        }
        for e in self.edges.values() {
            if let Some(n) = self.nodes.get_mut(&e.from) {
                n.degree += 1;
            }
            if let Some(n) = self.nodes.get_mut(&e.to) {
                n.degree += 1;
            }
        }
    }

    /// Remove every node defined in `path` and any edge touching a removed
    /// node. Used by the incremental refresh (KGRAPH-03) before re-extracting
    /// the changed file. Returns the number of nodes removed.
    pub fn remove_path(&mut self, path: &str) -> usize {
        let removed: Vec<String> = self
            .nodes
            .values()
            .filter(|n| n.path == path)
            .map(|n| n.id.clone())
            .collect();
        let set: std::collections::BTreeSet<&String> = removed.iter().collect();
        for id in &removed {
            self.nodes.remove(id);
        }
        self.edges
            .retain(|_, e| !set.contains(&e.from) && !set.contains(&e.to));
        removed.len()
    }

    pub fn to_json_pretty(&self) -> Result<String, ToolError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| ToolError::Execution(format!("serialize knowledge graph: {e}")))
    }

    pub fn from_json(s: &str) -> Result<Self, ToolError> {
        serde_json::from_str(s)
            .map_err(|e| ToolError::InvalidArgument(format!("parse knowledge graph: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs").with_span(1, 10));
        g.insert_node(KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs"));
        g
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let mut orig = g();
        orig.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        orig.recompute_degrees();
        let json = orig.to_json_pretty().unwrap();
        let back = KnowledgeGraph::from_json(&json).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn edge_with_unknown_endpoint_is_rejected() {
        let mut g = g();
        let err = g
            .insert_edge(KgEdge::new("crate::a::foo", "crate::z::missing", EdgeKind::Calls, Confidence::Extracted))
            .unwrap_err();
        match err {
            ToolError::InvalidArgument(m) => assert!(m.contains("unknown target"), "{m}"),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn node_id_is_stable_across_rebuild_ignoring_line_churn() {
        // Same FQN, different span (code moved down a few lines) => same node,
        // not a duplicate.
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs").with_span(1, 10));
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs").with_span(20, 30));
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.get_node("crate::a::foo").unwrap().span, Some((20, 30)));
    }

    #[test]
    fn extracted_edge_upgrades_inferred_duplicate() {
        let mut g = g();
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::Calls, Confidence::Inferred))
            .unwrap();
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::Calls, Confidence::Extracted))
            .unwrap();
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.edges().next().unwrap().confidence, Confidence::Extracted);
        // And a later weaker duplicate does not downgrade it.
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::Calls, Confidence::Ambiguous))
            .unwrap();
        assert_eq!(g.edges().next().unwrap().confidence, Confidence::Extracted);
    }

    #[test]
    fn remove_path_drops_nodes_and_incident_edges() {
        let mut g = g();
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::References, Confidence::Extracted))
            .unwrap();
        let removed = g.remove_path("src/a.rs");
        assert_eq!(removed, 1);
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0, "edge touching removed node must be gone");
    }

    #[test]
    fn empty_graph_serializes_with_empty_collections() {
        let g = KnowledgeGraph::new("TERM");
        let json = g.to_json_pretty().unwrap();
        let back = KnowledgeGraph::from_json(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn set_rank_stores_value_verbatim_including_zero() {
        let mut g = g();
        assert!(g.set_rank("crate::a::foo", 0.0), "known id");
        assert_eq!(g.get_node("crate::a::foo").unwrap().rank, 0.0);
        assert!(g.set_rank("crate::a::foo", 0.42));
        assert_eq!(g.get_node("crate::a::foo").unwrap().rank, 0.42);
        assert!(!g.set_rank("crate::z::missing", 1.0), "unknown id -> false");
    }

    #[test]
    fn degrees_count_in_and_out() {
        let mut g = g();
        g.insert_node(KgNode::new("crate::c::baz", NodeKind::Function, "baz", "src/c.rs"));
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::Calls, Confidence::Extracted)).unwrap();
        g.insert_edge(KgEdge::new("crate::c::baz", "crate::b::Bar", EdgeKind::Calls, Confidence::Extracted)).unwrap();
        g.recompute_degrees();
        assert_eq!(g.get_node("crate::b::Bar").unwrap().degree, 2);
        assert_eq!(g.get_node("crate::a::foo").unwrap().degree, 1);
    }
}
