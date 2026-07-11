//! Atlas: the knowledge-graph subsystem of the Scribe documentation engine.
//!
//! Atlas builds a per-project, queryable knowledge graph of a codebase
//! alongside the prose Scribe already generates. Nodes are code entities
//! (functions/structs/…/doc-sections) keyed by a stable fully-qualified name;
//! edges are calls/imports/references/etc., each stamped with a confidence
//! tier. Downstream items add extraction (KGRAPH-02), a per-project store
//! (KGRAPH-03), semantic edges + clustering, `kg_*` query tools, and SVG/HTML
//! renderers.
//!
//! It is the Rust-native, per-project successor to the Cortex prototype (which
//! in this crate is only a degrading dispatcher shim to an external, unshipped
//! script): Atlas holds a real in-process graph, not a subprocess relay.
//!
//! This item (KGRAPH-01) lands only the model; nothing here does I/O, parsing,
//! or networking.

pub mod extract;
pub mod model;
pub mod store;

pub use extract::build_rust_graph;
pub use model::{Confidence, EdgeKind, KgEdge, KgNode, KnowledgeGraph, NodeKind};
pub use store::GraphStore;
