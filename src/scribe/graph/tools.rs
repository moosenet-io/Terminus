//! Atlas `kg_*` query tools (KGRAPH-06).
//!
//! Exposes a project's knowledge graph to local models as MCP tools on the
//! Terminus core registry — the same path `plane`/`gitea`/`scribe` register on,
//! so Chord surfaces them to any local model. A model can ask the graph instead
//! of grepping raw source (Graphify reports ~70x fewer tokens to answer a
//! question against the graph than by reading files).
//!
//! Five read-only tools, all keyed by `project_id` and reading the KGRAPH-03
//! [`GraphStore`]:
//!   - `kg_search`    — find entities by name/id substring
//!   - `kg_neighbors` — direct callers/callees/imports of a node
//!   - `kg_subgraph`  — the local neighborhood (BFS to a depth)
//!   - `kg_path`      — how two entities connect (shortest undirected path)
//!   - `kg_stats`     — clusters, hotspots (top degree), orphans — the shape
//!   - `kg_file_symbols` — the symbols a given file defines
//!
//! Per-project *exposure gating* (`expose_query`) is the companion HARM
//! pipeline config; these tools are always registered but simply report
//! "no graph for project" when a project has no stored graph. Read-only, no
//! secrets, no subprocess.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::model::KnowledgeGraph;
use super::project_key::{self, ProjectKey};
use super::store::GraphStore;
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::ScribeConfig;
use crate::tool::{RustTool, ToolOutput};

/// A graph that was actually found, together with the CANONICAL KEY that found
/// it and WHEN it was built.
///
/// Every `kg_*` success answer is constructed from one of these (see
/// [`LoadedGraph::ok`]), which is why no `kg_*` tool can return a result
/// without saying which graph answered and how old it is. Before TERM #653 a
/// stale answer and a fresh answer were byte-identical in shape, so a caller
/// had no way to notice it had been served a seven-week-old graph.
struct LoadedGraph {
    /// The raw identifier the caller passed, echoed back unchanged.
    requested: String,
    /// The canonical key that actually addressed the store.
    key: ProjectKey,
    graph: KnowledgeGraph,
    built_at: Option<std::time::SystemTime>,
}

impl std::ops::Deref for LoadedGraph {
    type Target = KnowledgeGraph;
    fn deref(&self) -> &KnowledgeGraph {
        &self.graph
    }
}

impl LoadedGraph {
    /// Merge the per-tool payload into the standard provenance envelope
    /// (`project_id` / `graph_key` / `graph_built_at` / `graph_age_seconds`).
    ///
    /// `extra` MUST be a JSON object; anything else is a programming error and
    /// is surfaced as one rather than silently dropping the payload.
    fn envelope(&self, found: bool, extra: Value) -> Result<ToolOutput, ToolError> {
        let Some(obj) = extra.as_object() else {
            return Err(ToolError::Execution(
                "kg_* payload must be a JSON object".to_string(),
            ));
        };
        // Lay the payload down FIRST, then stamp provenance over it, so
        // provenance always wins. Review finding (opus + codex): merging the
        // payload last meant a future field named `graph_key`/`found`/… would
        // silently overwrite the provenance this envelope exists to guarantee —
        // an answer that looks authoritative while misreporting which graph
        // produced it. Writing provenance last makes that unrepresentable
        // rather than merely unlikely; the debug_assert surfaces the collision
        // to whoever introduces it.
        let mut out = Value::Object(obj.clone());
        for key in PROVENANCE_FIELDS {
            debug_assert!(
                !obj.contains_key(*key),
                "kg_* payload must not define the provenance field {key:?}"
            );
        }
        out["project_id"] = json!(self.requested);
        out["graph_key"] = json!(self.key.to_string());
        out["found"] = json!(found);
        out["graph_built_at"] = json!(self.built_at.and_then(rfc3339));
        out["graph_age_seconds"] = json!(self.built_at.and_then(age_seconds));
        structured(out)
    }

    /// A successful answer from this graph.
    fn ok(&self, extra: Value) -> Result<ToolOutput, ToolError> {
        self.envelope(true, extra)
    }

    /// A truthful "the graph answered, and the thing you asked for is not in
    /// it" — distinct from "the key did not resolve" ([`key_miss`]).
    fn miss(&self, message: impl Into<String>) -> Result<ToolOutput, ToolError> {
        self.envelope(false, json!({"message": message.into()}))
    }
}

/// The provenance keys [`LoadedGraph::envelope`] owns. A tool payload may never
/// define one of these.
const PROVENANCE_FIELDS: &[&str] = &[
    "project_id",
    "graph_key",
    "found",
    "graph_built_at",
    "graph_age_seconds",
];

fn rfc3339(t: std::time::SystemTime) -> Option<String> {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    Some(dt.to_rfc3339())
}

fn age_seconds(t: std::time::SystemTime) -> Option<u64> {
    std::time::SystemTime::now().duration_since(t).ok().map(|d| d.as_secs())
}

/// Load a project's graph from the configured store, resolving the caller's
/// identifier to its canonical key first. `Ok(None)` means the resolved key
/// named no stored graph.
fn load_graph(project_id: &str) -> Result<Option<LoadedGraph>, ToolError> {
    let store = GraphStore::from_config(&ScribeConfig::from_env());
    let key = GraphStore::key_for(project_id);
    Ok(store.load(project_id)?.map(|graph| LoadedGraph {
        requested: project_id.to_string(),
        key,
        graph,
        built_at: store.built_at(project_id),
    }))
}

/// Pull a required non-empty string argument.
fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required and must be a non-empty string")))
}

/// Standard KEY-MISS result (TERM #652).
///
/// This replaces a message that read *"no knowledge graph for this project (run
/// scribe_kg_build first)"*. That sentence was a statement about the SYSTEM,
/// and when the caller had merely used the other key space (a project UUID
/// rather than the slug the store is keyed by) the statement was FALSE — the
/// graph existed, was fresh, and answered fine under its slug. It cost a false
/// bug report and nearly cost an agent dispatched to fix a non-bug.
///
/// So this result asserts only what is actually known: which key was resolved,
/// which keys DO have graphs, and the nearest plausible match. It never claims
/// the project has no graph, because a key miss is not evidence of that.
fn key_miss(project_id: &str) -> Value {
    let store = GraphStore::from_config(&ScribeConfig::from_env());
    let key = GraphStore::key_for(project_id);
    let known = store.stored_keys();
    let suggestion = project_key::nearest(project_id, &known);
    json!({
        "project_id": project_id,
        "graph_key": key.to_string(),
        "found": false,
        "error": format!(
            "unknown project key '{project_id}' (resolved to '{key}') — no graph is stored under that key"
        ),
        "known_keys": known,
        "did_you_mean": suggestion,
        "orphaned_alias_graphs": store.orphaned_alias_keys(),
        "message": "the KEY did not resolve to a stored graph; this does NOT mean the project has no knowledge graph. Retry with one of known_keys, or set SCRIBE_KG_PROJECT_ALIASES to map this identifier to its canonical key.",
    })
}

fn node_json(n: &super::model::KgNode) -> Value {
    json!({
        "id": n.id, "kind": n.kind.as_str(), "name": n.name,
        "path": n.path, "cluster": n.cluster, "degree": n.degree,
    })
}

/// Undirected adjacency: node id -> list of (neighbor id, edge kind, confidence, outgoing?).
fn adjacency(g: &KnowledgeGraph) -> HashMap<&str, Vec<(&str, &'static str, &'static str, bool)>> {
    let mut adj: HashMap<&str, Vec<(&str, &'static str, &'static str, bool)>> = HashMap::new();
    for e in g.edges() {
        adj.entry(&e.from)
            .or_default()
            .push((&e.to, e.kind.as_str(), e.confidence.as_str(), true));
        adj.entry(&e.to)
            .or_default()
            .push((&e.from, e.kind.as_str(), e.confidence.as_str(), false));
    }
    adj
}

// ── kg_search ───────────────────────────────────────────────────────────────
pub struct KgSearch;

#[async_trait]
impl RustTool for KgSearch {
    fn name(&self) -> &str {
        "kg_search"
    }
    fn description(&self) -> &str {
        "Search a project's Atlas knowledge graph for code entities by name or fully-qualified id \
(case-insensitive substring). Returns matching nodes (functions/structs/etc.) with their kind, path, \
cluster, and degree. Ask the graph instead of grepping source."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "query": {"type": "string", "description": "name or id substring to match"},
                "limit": {"type": "integer", "description": "max results (default 25)"}
            },
            "required": ["project_id", "query"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let query = req_str(&args, "query")?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        let q = query.to_lowercase();
        let mut hits: Vec<&super::model::KgNode> = g
            .nodes()
            .filter(|n| n.name.to_lowercase().contains(&q) || n.id.to_lowercase().contains(&q))
            .collect();
        // Rank: exact name first, then PageRank importance (KGRAPH-13), then
        // degree, then id for stability.
        hits.sort_by(|a, b| {
            let ae = (a.name.to_lowercase() == q) as u8;
            let be = (b.name.to_lowercase() == q) as u8;
            be.cmp(&ae)
                .then(b.rank.total_cmp(&a.rank))
                .then(b.degree.cmp(&a.degree))
                .then(a.id.cmp(&b.id))
        });
        let results: Vec<Value> = hits.iter().take(limit).map(|n| node_json(n)).collect();
        g.ok(json!({"query": query, "count": results.len(), "results": results}))
    }
}

// ── kg_neighbors ──────────────────────────────────────────────────────────────
pub struct KgNeighbors;

#[async_trait]
impl RustTool for KgNeighbors {
    fn name(&self) -> &str {
        "kg_neighbors"
    }
    fn description(&self) -> &str {
        "List the direct neighbors of a node in a project's Atlas knowledge graph: what it calls/imports/\
references (outgoing) and what calls/references it (incoming). direction = out|in|both (default both)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "node_id": {"type": "string", "description": "fully-qualified node id"},
                "direction": {"type": "string", "enum": ["out", "in", "both"], "description": "default both"}
            },
            "required": ["project_id", "node_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let node_id = req_str(&args, "node_id")?;
        let direction = args.get("direction").and_then(|v| v.as_str()).unwrap_or("both");
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        if g.get_node(&node_id).is_none() {
            return g.miss(format!("no node '{node_id}'"));
        }
        // Single-source 1-hop walk (KGRAPH/CXEG-02): `super::query::one_hop_neighbors`
        // is the one place edges are iterated for a node's neighbors, shared with
        // `cortex_scope`. Splitting its (edge-order) result by direction here
        // reproduces the previous hand-rolled `outgoing`/`incoming` arrays
        // byte-for-byte.
        use super::query::{one_hop_neighbors, EdgeDirection, NeighborFilter};
        let neighbors = one_hop_neighbors(&g, &node_id, NeighborFilter::Both);
        let mut res = json!({"node_id": node_id});
        if direction == "out" || direction == "both" {
            let out: Vec<Value> = neighbors
                .iter()
                .filter(|nb| nb.direction == EdgeDirection::Outgoing)
                .map(|nb| json!({"id": nb.id, "kind": nb.kind, "confidence": nb.confidence}))
                .collect();
            res["outgoing"] = json!(out);
        }
        if direction == "in" || direction == "both" {
            let incoming: Vec<Value> = neighbors
                .iter()
                .filter(|nb| nb.direction == EdgeDirection::Incoming)
                .map(|nb| json!({"id": nb.id, "kind": nb.kind, "confidence": nb.confidence}))
                .collect();
            res["incoming"] = json!(incoming);
        }
        g.ok(res)
    }
}

// ── kg_subgraph ───────────────────────────────────────────────────────────────
pub struct KgSubgraph;

#[async_trait]
impl RustTool for KgSubgraph {
    fn name(&self) -> &str {
        "kg_subgraph"
    }
    fn description(&self) -> &str {
        "Return the local neighborhood around a node (BFS to `depth`, default 1) in a project's Atlas \
knowledge graph — the blast radius around a symbol. Returns the nodes and the edges among them."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "node_id": {"type": "string"},
                "depth": {"type": "integer", "description": "BFS hops (default 1, capped at 5)"}
            },
            "required": ["project_id", "node_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let node_id = req_str(&args, "node_id")?;
        let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1).min(5) as usize;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        if g.get_node(&node_id).is_none() {
            return g.miss(format!("no node '{node_id}'"));
        }
        let adj = adjacency(&g);
        // BFS to depth
        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(node_id.as_str());
        let mut frontier: Vec<&str> = vec![node_id.as_str()];
        for _ in 0..depth {
            let mut next = Vec::new();
            for cur in &frontier {
                if let Some(ns) = adj.get(cur) {
                    for (nb, _, _, _) in ns {
                        if seen.insert(nb) {
                            next.push(*nb);
                        }
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        let nodes: Vec<Value> = g.nodes().filter(|n| seen.contains(n.id.as_str())).map(|n| node_json(n)).collect();
        let edges: Vec<Value> = g
            .edges()
            .filter(|e| seen.contains(e.from.as_str()) && seen.contains(e.to.as_str()))
            .map(|e| json!({"from": e.from, "to": e.to, "kind": e.kind.as_str(), "confidence": e.confidence.as_str()}))
            .collect();
        g.ok(json!({"root": node_id, "depth": depth, "nodes": nodes, "edges": edges}))
    }
}

// ── kg_path ───────────────────────────────────────────────────────────────────
pub struct KgPath;

#[async_trait]
impl RustTool for KgPath {
    fn name(&self) -> &str {
        "kg_path"
    }
    fn description(&self) -> &str {
        "Find how two entities connect: the shortest undirected path between `from` and `to` in a \
project's Atlas knowledge graph. Returns the node-id sequence, or an empty path if they are unconnected."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "from": {"type": "string"},
                "to": {"type": "string"}
            },
            "required": ["project_id", "from", "to"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let from = req_str(&args, "from")?;
        let to = req_str(&args, "to")?;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        if g.get_node(&from).is_none() || g.get_node(&to).is_none() {
            return g.miss("from/to node not in graph");
        }
        let adj = adjacency(&g);
        // BFS shortest path (undirected).
        let mut prev: HashMap<&str, &str> = HashMap::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut q: VecDeque<&str> = VecDeque::new();
        q.push_back(from.as_str());
        seen.insert(from.as_str());
        while let Some(cur) = q.pop_front() {
            if cur == to.as_str() {
                break;
            }
            if let Some(ns) = adj.get(cur) {
                // deterministic neighbor order
                let mut nbrs: Vec<&str> = ns.iter().map(|(n, _, _, _)| *n).collect();
                nbrs.sort_unstable();
                nbrs.dedup();
                for nb in nbrs {
                    if seen.insert(nb) {
                        prev.insert(nb, cur);
                        q.push_back(nb);
                    }
                }
            }
        }
        let path = if seen.contains(to.as_str()) {
            let mut p = vec![to.as_str()];
            let mut cur = to.as_str();
            while cur != from.as_str() {
                cur = prev[cur];
                p.push(cur);
            }
            p.reverse();
            p.into_iter().map(|s| s.to_string()).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        g.ok(json!({"from": from, "to": to, "connected": !path.is_empty(), "path": path}))
    }
}

// ── kg_stats ───────────────────────────────────────────────────────────────────
pub struct KgStats;

#[async_trait]
impl RustTool for KgStats {
    fn name(&self) -> &str {
        "kg_stats"
    }
    fn description(&self) -> &str {
        "Summarize the shape of a project's Atlas knowledge graph: node/edge counts, per-kind and \
per-cluster counts, the top-degree hotspots, and orphan (degree-0) count."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"project_id": {"type": "string"}},
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
        let mut clusters: HashSet<u32> = HashSet::new();
        let mut orphans = 0u64;
        for n in g.nodes() {
            *by_kind.entry(n.kind.as_str()).or_default() += 1;
            if let Some(c) = n.cluster {
                clusters.insert(c);
            }
            if n.degree == 0 {
                orphans += 1;
            }
        }
        // Hotspots ranked by PageRank importance (KGRAPH-13), then degree.
        let mut top: Vec<&super::model::KgNode> = g.nodes().collect();
        top.sort_by(|a, b| {
            b.rank.total_cmp(&a.rank)
                .then(b.degree.cmp(&a.degree))
                .then(a.id.cmp(&b.id))
        });
        let hotspots: Vec<Value> = top
            .iter()
            .take(10)
            .filter(|n| n.degree > 0)
            .map(|n| json!({"id": n.id, "rank": n.rank, "degree": n.degree, "cluster": n.cluster}))
            .collect();
        g.ok(json!({
            "nodes": g.node_count(), "edges": g.edge_count(),
            "clusters": clusters.len(), "orphans": orphans,
            "by_kind": by_kind, "hotspots": hotspots,
        }))
    }
}

// ── kg_file_symbols ───────────────────────────────────────────────────────────
pub struct KgFileSymbols;

const KG_FILE_SYMBOLS_MAX: usize = 500;

#[async_trait]
impl RustTool for KgFileSymbols {
    fn name(&self) -> &str {
        "kg_file_symbols"
    }
    fn description(&self) -> &str {
        "List the symbols a file defines in a project's Atlas knowledge graph: every currently-valid \
node whose path equals the given repo-relative file path, sorted by PageRank importance. Ask the \
graph instead of grepping the file for `fn`/`struct`/etc."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "path": {"type": "string", "description": "repo-relative file path, e.g. src/review/mod.rs"}
            },
            "required": ["project_id", "path"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let path = req_str(&args, "path")?;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        let mut hits: Vec<&super::model::KgNode> = g.current_nodes().filter(|n| n.path == path).collect();
        hits.sort_by(|a, b| b.rank.total_cmp(&a.rank).then(a.id.cmp(&b.id)));
        let symbols: Vec<Value> = hits
            .iter()
            .take(KG_FILE_SYMBOLS_MAX)
            .map(|n| json!({"id": n.id, "name": n.name, "kind": n.kind.as_str(), "rank": n.rank, "cluster": n.cluster}))
            .collect();
        g.ok(json!({
            "path": path,
            "count": symbols.len(), "symbols": symbols,
        }))
    }
}

/// Wrap a JSON value as a `ToolOutput` carrying both a pretty text form and the
/// structured payload (so a model gets typed results).
fn structured(v: Value) -> Result<ToolOutput, ToolError> {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    Ok(ToolOutput { text, structured: Some(v) })
}

// ── kg_communities ────────────────────────────────────────────────────────────
pub struct KgCommunities;

#[async_trait]
impl RustTool for KgCommunities {
    fn name(&self) -> &str {
        "kg_communities"
    }
    fn description(&self) -> &str {
        "Return the community structure of a project's Atlas knowledge graph (KGRAPH-12): the \
level-0 clusters and a coarser level-1 grouping, each with its member entities and — when a model \
is available — a short summary. Lets a model answer subsystem/architecture questions at the right \
zoom without walking every node."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "level": {"type": "integer", "description": "filter to a zoom level (0 finest); omit for all"}
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let want_level = args.get("level").and_then(|v| v.as_u64()).map(|l| l as u32);
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        let mut comms = super::community::hierarchical_communities(&g);

        // Best-effort summaries when a review daemon is configured AND opt-in.
        let semantic_on = std::env::var("SCRIBE_KG_SEMANTIC")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if semantic_on {
            let review_cfg = crate::review::ReviewConfig::from_env();
            if review_cfg.daemon_token.is_some() {
                for c in comms.iter_mut() {
                    let prompt = super::community::community_prompt(c, &g);
                    if let Ok(reply) = crate::scribe::dispatch_docs_generation(&review_cfg, &prompt).await {
                        super::community::set_summary(c, &reply);
                    }
                }
            }
        }

        if let Some(l) = want_level {
            comms.retain(|c| c.level == l);
        }
        let comms_json = serde_json::to_value(&comms).unwrap_or_else(|_| json!([]));
        g.ok(json!({"count": comms.len(), "communities": comms_json}))
    }
}

// ── kg_query ──────────────────────────────────────────────────────────────────
pub struct KgQuery;

#[async_trait]
impl RustTool for KgQuery {
    fn name(&self) -> &str {
        "kg_query"
    }
    fn description(&self) -> &str {
        "Answer a natural-language question about a project's codebase against its Atlas knowledge \
graph (KGRAPH-14). Routes automatically: a specific-symbol question retrieves the ranked matching \
entities; an architectural/subsystem question retrieves the community summaries. Returns the \
retrieved context plus, when a model is available, a synthesized answer."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "question": {"type": "string"}
            },
            "required": ["project_id", "question"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let question = req_str(&args, "question")?;
        let Some(g) = load_graph(&project_id)? else {
            return structured(key_miss(&project_id));
        };
        let level = super::query::classify(&question);
        let context: Value = match level {
            super::query::QueryLevel::Entity => {
                serde_json::to_value(super::query::gather_entity(&g, &question, 15)).unwrap_or_else(|_| json!([]))
            }
            super::query::QueryLevel::Community => {
                serde_json::to_value(super::query::gather_community(&g)).unwrap_or_else(|_| json!([]))
            }
        };

        // Best-effort answer synthesis (two-tier: strong model over the context).
        let mut answer = Value::Null;
        let semantic_on = std::env::var("SCRIBE_KG_SEMANTIC")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if semantic_on {
            let review_cfg = crate::review::ReviewConfig::from_env();
            if review_cfg.daemon_token.is_some() {
                let ctx_str = serde_json::to_string(&context).unwrap_or_default();
                let prompt = super::query::build_answer_prompt(&question, level, &ctx_str);
                if let Ok(reply) = crate::scribe::dispatch_docs_generation(&review_cfg, &prompt).await {
                    answer = json!(reply.trim());
                }
            }
        }

        g.ok(json!({
            "level": level, "question": question,
            "answer": answer, "context": context,
        }))
    }
}

// ── kg_semantic_search ────────────────────────────────────────────────────────
pub struct KgSemanticSearch;

/// Clamp a user-supplied `limit` into `[1, KG_SEMANTIC_SEARCH_MAX]` (0 → 1,
/// values above the cap → the cap).
const KG_SEMANTIC_SEARCH_MAX: i64 = 50;

fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, KG_SEMANTIC_SEARCH_MAX)
}

/// Join top-K `(node_id, score)` hits against the loaded graph: drop ids not
/// present in the graph (stale vector rows — e.g. the graph was rebuilt and
/// the node no longer exists), map the rest to their result JSON, and
/// preserve the input (score-descending) order. Pure — no DB/HTTP — so the
/// stale-row-drop and field-mapping behavior is testable without live infra.
fn map_topk_to_results(hits: &[(String, f32)], g: &KnowledgeGraph) -> Vec<Value> {
    hits.iter()
        .filter_map(|(node_id, score)| {
            // Only surface CURRENTLY-VALID nodes. `get_node` also returns
            // bi-temporally invalidated nodes (a removed/renamed symbol kept in
            // the graph with `valid_to` set) — a stale vector row must not
            // resurrect a deleted symbol, so drop anything not current.
            g.get_node(node_id)
                .filter(|n| n.valid_to.is_none())
                .map(|n| {
                    json!({
                        "id": n.id, "name": n.name, "kind": n.kind.as_str(),
                        "path": n.path, "score": score, "cluster": n.cluster,
                    })
                })
        })
        .collect()
}

#[async_trait]
impl RustTool for KgSemanticSearch {
    fn name(&self) -> &str {
        "kg_semantic_search"
    }
    fn description(&self) -> &str {
        "Semantic (embedding) search over a project's Atlas knowledge graph: embeds `query` and \
returns the nearest nodes by meaning, not substring. Degrades cleanly — returns \
`configured:false` (not an error) when the vector store or embeddings endpoint is unconfigured \
or unreachable, so callers should fall back to the lexical `kg_search` in that case."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "query": {"type": "string", "description": "natural-language search text"},
                "limit": {"type": "integer", "description": "max results, clamped to [1,50] (default 10)"}
            },
            "required": ["project_id", "query"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let query = req_str(&args, "query")?;
        let limit = clamp_limit(args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10));

        let store = match super::vec_store::AtlasVecStore::from_env().await {
            Ok(store) => store,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                }));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        let client = super::vec_embed::EmbedClient::from_env();
        let qvec = match client.embed(&query).await {
            Ok(v) => v,
            Err(e) => {
                // Store IS configured; only the embedding STEP failed (endpoint
                // transiently down). Per the KGEMB-04 edge-case contract this is
                // `configured:true, found:false, error` — semantic search is set
                // up but momentarily unusable — distinct from an unset store.
                return structured(json!({
                    "configured": true, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        let hits = match store.query_topk(&project_id, &qvec, limit).await {
            Ok(h) => h,
            Err(e) => {
                // A vector-query failure means the STORE itself is unusable — the
                // store gates "configured", so this is configured:false (fall
                // back to lexical), the same signal as an unset/unreachable store.
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        // No graph for this project is NOT a config problem (the store/embeddings
        // are configured) — it's a genuine found:false, following the same
        // no_graph convention as the other kg_* tools. Callers should NOT fall
        // back to lexical here (there is nothing to search either way).
        let Some(g) = load_graph(&project_id)? else {
            // Same key-miss truth as every other kg_* tool, plus the
            // `configured` flag this tool's contract requires (the store and
            // embeddings ARE configured; only the key missed).
            let mut v = key_miss(&project_id);
            v["configured"] = json!(true);
            v["count"] = json!(0);
            v["results"] = json!([]);
            return structured(v);
        };

        // `found` reflects whether there are actual semantic matches: zero hits,
        // or every hit dropped as a stale vector row (node deleted from the
        // graph), is found:false — a caller can distinguish "search ran, nothing
        // matched" from a genuine hit set without inspecting count.
        let results = map_topk_to_results(&hits, &g);
        g.envelope(
            !results.is_empty(),
            json!({"configured": true, "count": results.len(), "results": results}),
        )
    }
}

// ── kg_findings ─────────────────────────────────────────────────────────────

/// Read-only lister over the KGFIND-01 [`super::findings_store::FindingsStore`]:
/// captured findings for a project, ordered by recurrence
/// (`occurrences DESC, last_seen DESC`), optionally filtered by scope kind,
/// category, and a minimum occurrence count.
pub struct KgFindings;

/// Maximum `limit` accepted by `kg_findings` (default 50, clamped to
/// `[1, KG_FINDINGS_MAX]`).
const KG_FINDINGS_MAX: i64 = 200;

fn clamp_findings_limit(limit: i64) -> i64 {
    limit.clamp(1, KG_FINDINGS_MAX)
}

/// Map a stored [`super::findings_store::FindingRow`] to its result JSON.
/// Pure — no I/O — so the field mapping is unit-testable without a live
/// store.
fn finding_row_json(row: &super::findings_store::FindingRow) -> Value {
    json!({
        "id": row.id.to_string(),
        "category": row.category,
        "severity": row.severity,
        "scope_kind": row.scope_kind,
        "scope_ref": row.scope_ref,
        "description": row.description,
        "occurrences": row.occurrences,
        "first_seen": row.first_seen.to_rfc3339(),
        "last_seen": row.last_seen.to_rfc3339(),
    })
}

#[async_trait]
impl RustTool for KgFindings {
    fn name(&self) -> &str {
        "kg_findings"
    }
    fn description(&self) -> &str {
        "Lists captured Atlas knowledge-graph findings (lint-like observations, review notes, \
anomalies) for a project, ordered by recurrence (most-repeated first). Optionally filter by \
`scope` (node/path/community/global), `category`, and `min_occurrences`. Degrades cleanly — \
returns `configured:false` (not an error) when the findings store is unconfigured."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "scope": {"type": "string", "description": "optional scope filter: node/path/community/global"},
                "category": {"type": "string", "description": "optional category filter"},
                "min_occurrences": {"type": "integer", "description": "optional minimum recurrence count"},
                "limit": {"type": "integer", "description": "max results, clamped to [1,200] (default 50)"}
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let scope = args.get("scope").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let category = args.get("category").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        // Saturating clamp, never `as i32`: a huge JSON integer would wrap to a
        // negative i32 and silently broaden the filter. Clamp to [0, i32::MAX] so a
        // very large value simply matches nothing (the expected behavior).
        let min_occurrences = args
            .get("min_occurrences")
            .and_then(|v| v.as_i64())
            .map(|v| v.clamp(0, i32::MAX as i64) as i32);
        let limit = clamp_findings_limit(args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50));

        let store = match super::findings_store::FindingsStore::from_env().await {
            Ok(store) => store,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                }));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        // A list/query failure means the store isn't usable for this call — degrade
        // (configured:false + error), never a hard tool error, matching the
        // contract and the sibling kg_semantic_search store-failure handling.
        let rows = match store
            .list(&project_id, scope.as_deref(), category.as_deref(), min_occurrences)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        let results: Vec<Value> = rows
            .iter()
            .take(limit as usize)
            .map(finding_row_json)
            .collect();

        structured(json!({
            "configured": true, "found": !results.is_empty(), "project_id": project_id,
            "count": results.len(), "results": results,
        }))
    }
}

// ── kg_rules ─────────────────────────────────────────────────────────────

/// Read-only lister over the KGRULE-01 [`super::rules_store::RulesStore`]:
/// active (status='active', non-expired) rules for a project, ordered by
/// enforcement priority (blocking > lint-candidate > advisory) then recency
/// (mirrors `RulesStore::list_active`'s own ORDER BY), optionally filtered by
/// scope kind and category.
pub struct KgRules;

/// Maximum `limit` accepted by `kg_rules` (default 50, clamped to
/// `[1, KG_RULES_MAX]`) — mirrors `kg_findings`'s `KG_FINDINGS_MAX` shape.
const KG_RULES_MAX: i64 = 200;

fn clamp_rules_limit(limit: i64) -> i64 {
    limit.clamp(1, KG_RULES_MAX)
}

/// Map a stored [`super::rules_store::RuleRow`] to its result JSON. Pure — no
/// I/O — so the field mapping is unit-testable without a live store.
fn rule_row_json(row: &super::rules_store::RuleRow) -> Value {
    json!({
        "id": row.id.to_string(),
        "scope_kind": row.scope_kind,
        "scope_ref": row.scope_ref,
        "category": row.category,
        "guidance": row.guidance,
        "enforcement": row.enforcement,
        "cortex_risk": row.cortex_risk,
    })
}

#[async_trait]
impl RustTool for KgRules {
    fn name(&self) -> &str {
        "kg_rules"
    }
    fn description(&self) -> &str {
        "Lists ACTIVE Atlas knowledge-graph rules (durable, earned coding guidance crystallized \
from recurring review findings and promoted via an adversarial review panel) for a project, \
ordered by enforcement (blocking first) then recency. Optionally filter by `scope` \
(node/path/community/global) and `category`. Ground here before scoping work. Degrades cleanly \
— returns `configured:false` (not an error) when the rules store is unconfigured."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "scope": {"type": "string", "description": "optional scope filter: node/path/community/global"},
                "category": {"type": "string", "description": "optional category filter"},
                "limit": {"type": "integer", "description": "max results, clamped to [1,200] (default 50)"}
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = req_str(&args, "project_id")?;
        let scope = args.get("scope").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let category = args.get("category").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let limit = clamp_rules_limit(args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50));

        let store = match super::rules_store::RulesStore::from_env().await {
            Ok(store) => store,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                }));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        // A list failure means the store isn't usable for this call — degrade
        // (configured:false + error), never a hard tool error, matching the
        // sibling kg_findings/kg_semantic_search store-failure handling.
        let rows = match store.list_active(&project_id, scope.as_deref(), None, category.as_deref()).await {
            Ok(rows) => rows,
            Err(e) => {
                return structured(json!({
                    "configured": false, "found": false, "project_id": project_id, "results": [],
                    "error": e.to_string(),
                }));
            }
        };

        let results: Vec<Value> = rows.iter().take(limit as usize).map(rule_row_json).collect();

        structured(json!({
            "configured": true, "found": !results.is_empty(), "project_id": project_id,
            "count": results.len(), "results": results,
        }))
    }
}

/// Register the `kg_*` tools on the core registry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(KgSearch));
    let _ = registry.register(Box::new(KgNeighbors));
    let _ = registry.register(Box::new(KgSubgraph));
    let _ = registry.register(Box::new(KgPath));
    let _ = registry.register(Box::new(KgStats));
    let _ = registry.register(Box::new(KgCommunities));
    let _ = registry.register(Box::new(KgQuery));
    let _ = registry.register(Box::new(KgFileSymbols));
    let _ = registry.register(Box::new(KgSemanticSearch));
    let _ = registry.register(Box::new(KgFindings));
    let _ = registry.register(Box::new(KgRules));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::build_rust_graph;
    use serial_test::serial;

    fn seed_project(project: &str) -> tempdir_like::Guard {
        // Isolate the store under a unique temp dir via SCRIBE_KG_STORE_DIR.
        let dir = std::env::temp_dir().join(format!("atlas-kgtools-{}-{}", project, std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &dir);
        let src = r#"
pub fn helper() -> u8 { 1 }
pub fn caller() -> u8 { helper() }
pub struct Widget;
"#;
        let g = build_rust_graph(project, &[("src/w.rs".to_string(), src.to_string())]).unwrap();
        GraphStore::from_config(&ScribeConfig::from_env()).save(project, &g).unwrap();
        tempdir_like::Guard { dir }
    }

    // tiny RAII temp-dir cleanup without an external crate
    mod tempdir_like {
        pub struct Guard {
            pub dir: std::path::PathBuf,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    fn val(out: ToolOutput) -> Value {
        out.structured.expect("structured payload")
    }

    #[tokio::test]
    #[serial]
    async fn search_finds_by_name() {
        let _g = seed_project("SRCH");
        let out = KgSearch
            .execute_structured(json!({"project_id": "SRCH", "query": "helper"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], true);
        assert!(v["results"].as_array().unwrap().iter().any(|r| r["id"] == "crate::w::helper"));
    }

    #[tokio::test]
    #[serial]
    async fn neighbors_returns_call_edge() {
        let _g = seed_project("NBR");
        let out = KgNeighbors
            .execute_structured(json!({"project_id": "NBR", "node_id": "crate::w::caller"}))
            .await
            .unwrap();
        let v = val(out);
        assert!(v["outgoing"].as_array().unwrap().iter().any(|e| e["id"] == "crate::w::helper" && e["kind"] == "calls"));
    }

    #[tokio::test]
    #[serial]
    async fn path_connects_caller_to_helper() {
        let _g = seed_project("PATH");
        let out = KgPath
            .execute_structured(json!({"project_id": "PATH", "from": "crate::w::caller", "to": "crate::w::helper"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["connected"], true);
        let path = v["path"].as_array().unwrap();
        assert_eq!(path.first().unwrap(), "crate::w::caller");
        assert_eq!(path.last().unwrap(), "crate::w::helper");
    }

    #[tokio::test]
    #[serial]
    async fn stats_counts_nodes_and_hotspots() {
        let _g = seed_project("STAT");
        let out = KgStats.execute_structured(json!({"project_id": "STAT"})).await.unwrap();
        let v = val(out);
        assert_eq!(v["found"], true);
        assert!(v["nodes"].as_u64().unwrap() >= 3);
    }

    #[tokio::test]
    #[serial]
    async fn unknown_project_reports_not_found_not_error() {
        // point at an empty store dir
        let dir = std::env::temp_dir().join(format!("atlas-kgtools-empty-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &dir);
        let out = KgStats.execute_structured(json!({"project_id": "NOPE"})).await.unwrap();
        assert_eq!(val(out)["found"], false);
    }

    #[tokio::test]
    #[serial]
    async fn communities_returns_clusters_without_summaries_when_no_daemon() {
        let dir = std::env::temp_dir().join(format!("atlas-kgcomm-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &dir);
        std::env::remove_var("SCRIBE_KG_SEMANTIC");
        let mut g = build_rust_graph(
            "COMM",
            &[("src/x.rs".to_string(),
               "pub fn a1(){a2();}\npub fn a2(){a1();}\npub fn b1(){b2();}\npub fn b2(){b1();}".to_string())],
        )
        .unwrap();
        crate::scribe::graph::cluster::cluster(&mut g);
        GraphStore::from_config(&ScribeConfig::from_env()).save("COMM", &g).unwrap();

        let out = KgCommunities.execute_structured(json!({"project_id": "COMM"})).await.unwrap();
        let v = val(out);
        assert_eq!(v["found"], true);
        assert!(v["count"].as_u64().unwrap() >= 1, "at least one community");
        let comms = v["communities"].as_array().unwrap();
        assert!(comms.iter().all(|c| c["summary"] == ""), "no summaries without a daemon");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[serial]
    async fn query_routes_and_returns_context_without_daemon() {
        let dir = std::env::temp_dir().join(format!("atlas-kgquery-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &dir);
        std::env::remove_var("SCRIBE_KG_SEMANTIC");
        let mut g = build_rust_graph(
            "QRY",
            &[("src/x.rs".to_string(), "pub fn backoff(){}\npub fn caller(){backoff();}".to_string())],
        )
        .unwrap();
        crate::scribe::graph::cluster::cluster(&mut g);
        GraphStore::from_config(&ScribeConfig::from_env()).save("QRY", &g).unwrap();

        // specific-symbol question → entity level, matching node in context
        let v = val(KgQuery
            .execute_structured(json!({"project_id": "QRY", "question": "where is backoff"}))
            .await
            .unwrap());
        assert_eq!(v["level"], "entity");
        assert!(v["answer"].is_null(), "no synthesis without a daemon");
        assert!(v["context"].as_array().unwrap().iter().any(|h| h["id"] == "crate::x::backoff"));

        // architectural question → community level
        let v2 = val(KgQuery
            .execute_structured(json!({"project_id": "QRY", "question": "give an overview of the architecture"}))
            .await
            .unwrap());
        assert_eq!(v2["level"], "community");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[serial]
    async fn file_symbols_returns_symbols_defined_in_the_file() {
        let _g = seed_project("FSYM");
        let out = KgFileSymbols
            .execute_structured(json!({"project_id": "FSYM", "path": "src/w.rs"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], true);
        let symbols = v["symbols"].as_array().unwrap();
        assert!(symbols.iter().any(|s| s["id"] == "crate::w::helper" && s["kind"] == "function"));
        assert!(symbols.iter().any(|s| s["id"] == "crate::w::caller" && s["kind"] == "function"));
        assert!(symbols.iter().any(|s| s["id"] == "crate::w::Widget" && s["kind"] == "struct"));
        assert_eq!(v["count"].as_u64().unwrap(), symbols.len() as u64);
    }

    #[tokio::test]
    #[serial]
    async fn file_symbols_unknown_path_is_empty_not_error() {
        let _g = seed_project("FSYM2");
        let out = KgFileSymbols
            .execute_structured(json!({"project_id": "FSYM2", "path": "src/does_not_exist.rs"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], true);
        assert_eq!(v["count"].as_u64().unwrap(), 0);
        assert!(v["symbols"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn file_symbols_missing_project_reports_not_found() {
        let dir = std::env::temp_dir().join(format!("atlas-kgfsym-empty-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &dir);
        let out = KgFileSymbols
            .execute_structured(json!({"project_id": "NOPE", "path": "src/w.rs"}))
            .await
            .unwrap();
        assert_eq!(val(out)["found"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── TERM #652 / #653 ───────────────────────────────────────────────────

    /// The headline defect: a key that does not resolve must NOT be reported as
    /// the project having no graph. The old message said
    /// "no knowledge graph for this project (run scribe_kg_build first)" while
    /// the graph existed and was fresh — a false statement about system state
    /// that caused a real false bug report.
    #[tokio::test]
    #[serial]
    async fn a_key_miss_is_reported_as_a_key_miss_not_as_an_absent_graph() {
        let _g = seed_project("FSYMKEY");
        // A UUID: the other key space, mapped to nothing.
        let out = KgFileSymbols
            .execute_structured(json!({
                "project_id": "<uuid>", // pii-test-fixture (fabricated uuid shape)
                "path": "src/w.rs"
            }))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], false);

        let blob = serde_json::to_string(&v).unwrap();
        assert!(
            !blob.contains("no knowledge graph for this project"),
            "must not assert the project has no graph: {blob}"
        );
        assert!(
            v["error"].as_str().unwrap_or_default().contains("unknown project key"),
            "must name the failing KEY: {v}"
        );
        // It must tell the caller what DOES exist, so the miss is actionable.
        let known = v["known_keys"].as_array().expect("known_keys listed");
        assert!(
            known.iter().any(|k| k == "fsymkey"),
            "the store's real keys must be listed: {v}"
        );
        // ...and it must not invent a suggestion for something unrecognisable.
        assert!(v["did_you_mean"].is_null(), "no confident wrong guess: {v}");
    }

    /// A near-miss key gets an actionable suggestion.
    #[tokio::test]
    #[serial]
    async fn a_near_miss_key_suggests_the_real_one() {
        let _g = seed_project("STATNEAR");
        let out = KgStats
            .execute_structured(json!({"project_id": "statnea"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], false);
        assert_eq!(v["did_you_mean"], "statnear", "{v}");
    }

    /// TERM #653's visibility half: EVERY answer carries the key that answered
    /// and when that graph was built, so a consumer can SEE staleness instead
    /// of having to trust the result.
    ///
    /// Asserted across every graph-backed tool, not a representative one. The
    /// first version of this test checked `kg_stats` alone, and `kg_neighbors`
    /// duly shipped a success response with no provenance at all — the same
    /// shape of gap as the defect being fixed.
    #[tokio::test]
    #[serial]
    async fn every_answer_reports_which_graph_answered_and_how_old_it_is() {
        let _g = seed_project("FRESH");
        let cases: Vec<(&str, Value)> = vec![
            ("kg_search", json!({"project_id": "FRESH", "query": "helper"})),
            ("kg_neighbors", json!({"project_id": "FRESH", "node_id": "crate::w::caller"})),
            ("kg_subgraph", json!({"project_id": "FRESH", "node_id": "crate::w::caller"})),
            ("kg_path", json!({"project_id": "FRESH", "from": "crate::w::caller", "to": "crate::w::helper"})),
            ("kg_stats", json!({"project_id": "FRESH"})),
            ("kg_communities", json!({"project_id": "FRESH"})),
            ("kg_file_symbols", json!({"project_id": "FRESH", "path": "src/w.rs"})),
            ("kg_query", json!({"project_id": "FRESH", "question": "where is helper"})),
        ];
        for (name, args) in cases {
            let out = match name {
                "kg_search" => KgSearch.execute_structured(args).await,
                "kg_neighbors" => KgNeighbors.execute_structured(args).await,
                "kg_subgraph" => KgSubgraph.execute_structured(args).await,
                "kg_path" => KgPath.execute_structured(args).await,
                "kg_stats" => KgStats.execute_structured(args).await,
                "kg_communities" => KgCommunities.execute_structured(args).await,
                "kg_file_symbols" => KgFileSymbols.execute_structured(args).await,
                "kg_query" => KgQuery.execute_structured(args).await,
                other => panic!("unhandled tool {other}"),
            }
            .unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
            let v = val(out);
            assert_eq!(v["graph_key"], "fresh", "{name} must report the canonical key: {v}");
            assert_eq!(v["project_id"], "FRESH", "{name} must echo the raw request: {v}");
            assert!(
                v["graph_built_at"].as_str().is_some(),
                "{name} must report the graph build time: {v}"
            );
            assert!(
                v["graph_age_seconds"].as_u64().is_some(),
                "{name} must report the graph age: {v}"
            );
            assert!(v["found"].is_boolean(), "{name} must report found: {v}");
        }
    }

    /// Provenance must win over a colliding payload field, so an answer can
    /// never misreport which graph produced it.
    #[tokio::test]
    #[serial]
    async fn payload_fields_cannot_overwrite_provenance() {
        let _g = seed_project("CLOBBER");
        let out = KgFileSymbols
            .execute_structured(json!({"project_id": "CLOBBER", "path": "src/w.rs"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["graph_key"], "clobber");
        // Directly exercise the choke point with a hostile payload.
        let loaded = load_graph("CLOBBER").unwrap().expect("graph");
        let out = loaded
            .ok(json!({"graph_key": "WRONG", "found": false, "project_id": "WRONG", "count": 1}))
            .unwrap();
        let v = val(out);
        assert_eq!(v["graph_key"], "clobber", "provenance must win: {v}");
        assert_eq!(v["project_id"], "CLOBBER", "provenance must win: {v}");
        assert_eq!(v["found"], true, "provenance must win: {v}");
        assert_eq!(v["count"], 1, "non-colliding payload preserved: {v}");
    }

    /// A found:false that came from the GRAPH (node not present) still reports
    /// which graph was searched — otherwise "not found" is ambiguous between
    /// "wrong key" and "genuinely absent", which is the ambiguity that started
    /// all of this.
    #[tokio::test]
    #[serial]
    async fn a_graph_level_miss_still_names_the_graph_that_answered() {
        let _g = seed_project("NBRMISS");
        let out = KgNeighbors
            .execute_structured(json!({"project_id": "NBRMISS", "node_id": "crate::w::nope"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["found"], false);
        assert_eq!(v["graph_key"], "nbrmiss", "{v}");
        assert!(v["graph_built_at"].as_str().is_some(), "{v}");
        assert!(v["message"].as_str().unwrap().contains("no node"), "{v}");
        // Crucially NOT a key miss.
        assert!(v["error"].is_null(), "a present graph is not a key error: {v}");
    }

    #[tokio::test]
    #[serial]
    async fn missing_required_arg_is_invalid_argument() {
        let err = KgSearch.execute_structured(json!({"project_id": "X"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "missing query must be InvalidArgument, got {err:?}");
    }

    // ── kg_semantic_search ────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn semantic_search_unconfigured_store_degrades_not_errors() {
        // Mirrors vec_store's own test shape: never mutate global env to force
        // NotConfigured if a real DSN is already present in this process.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let _g = seed_project("SEMSRCH");
        let out = KgSemanticSearch
            .execute_structured(json!({"project_id": "SEMSRCH", "query": "helper function"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["configured"], false);
        assert_eq!(v["found"], false);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn clamp_limit_clamps_zero_and_over_cap() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(100), 50);
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(-5), 1);
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(1), 1);
    }

    fn semantic_test_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("SEM");
        g.insert_node(super::super::model::KgNode::new(
            "crate::a::foo",
            super::super::model::NodeKind::Function,
            "foo",
            "src/a.rs",
        ));
        g.insert_node(super::super::model::KgNode::new(
            "crate::b::bar",
            super::super::model::NodeKind::Function,
            "bar",
            "src/b.rs",
        ));
        g
    }

    #[test]
    fn map_topk_drops_stale_node_ids_not_in_graph() {
        let g = semantic_test_graph();
        let hits = vec![
            ("crate::a::foo".to_string(), 0.9_f32),
            ("crate::gone::stale".to_string(), 0.85_f32),
            ("crate::b::bar".to_string(), 0.5_f32),
        ];
        let results = map_topk_to_results(&hits, &g);
        assert_eq!(results.len(), 2, "stale id must be dropped");
        assert_eq!(results[0]["id"], "crate::a::foo");
        assert_eq!(results[0]["score"], 0.9_f32);
        assert_eq!(results[1]["id"], "crate::b::bar");
    }

    #[test]
    fn map_topk_drops_bitemporally_invalidated_nodes() {
        // A vector row can outlive the symbol it points at: the node is still in
        // the graph but bi-temporally invalidated (valid_to set). It must NOT be
        // returned — a deleted symbol resurrected by a stale embedding.
        let mut g = semantic_test_graph();
        g.invalidate_path("src/b.rs", 100); // `bar` removed at build-seq 100
        let hits = vec![
            ("crate::a::foo".to_string(), 0.9_f32),
            ("crate::b::bar".to_string(), 0.8_f32), // stale vector row
        ];
        let results = map_topk_to_results(&hits, &g);
        assert_eq!(results.len(), 1, "invalidated node must be dropped");
        assert_eq!(results[0]["id"], "crate::a::foo");
    }

    #[test]
    fn map_topk_preserves_input_order() {
        let g = semantic_test_graph();
        // Reverse of pagerank/degree order — order must come purely from the
        // input slice (already score-sorted by query_topk), not re-sorted.
        let hits = vec![
            ("crate::b::bar".to_string(), 0.99_f32),
            ("crate::a::foo".to_string(), 0.1_f32),
        ];
        let results = map_topk_to_results(&hits, &g);
        assert_eq!(results[0]["id"], "crate::b::bar");
        assert_eq!(results[1]["id"], "crate::a::foo");
    }

    #[test]
    fn map_topk_empty_hits_is_empty_results() {
        let g = semantic_test_graph();
        let results = map_topk_to_results(&[], &g);
        assert!(results.is_empty());
    }

    #[test]
    fn map_topk_includes_all_expected_fields() {
        let g = semantic_test_graph();
        let hits = vec![("crate::a::foo".to_string(), 0.42_f32)];
        let results = map_topk_to_results(&hits, &g);
        let r = &results[0];
        assert_eq!(r["id"], "crate::a::foo");
        assert_eq!(r["name"], "foo");
        assert_eq!(r["kind"], "function");
        assert_eq!(r["path"], "src/a.rs");
        assert_eq!(r["score"], 0.42_f32);
        assert!(r["cluster"].is_null());
    }

    // ── kg_findings ──────────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn findings_unconfigured_store_degrades_not_errors() {
        // Mirrors kg_semantic_search's own test shape: never mutate global env
        // to force NotConfigured if a real DSN is already present in this
        // process.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let out = KgFindings
            .execute_structured(json!({"project_id": "FIND"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["configured"], false);
        assert_eq!(v["found"], false);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn clamp_findings_limit_clamps_zero_and_over_cap() {
        assert_eq!(clamp_findings_limit(0), 1);
        assert_eq!(clamp_findings_limit(300), 200);
        assert_eq!(clamp_findings_limit(50), 50);
        assert_eq!(clamp_findings_limit(-5), 1);
        assert_eq!(clamp_findings_limit(200), 200);
        assert_eq!(clamp_findings_limit(1), 1);
    }

    fn sample_finding_row() -> super::super::findings_store::FindingRow {
        let now = chrono::Utc::now();
        super::super::findings_store::FindingRow {
            id: uuid::Uuid::new_v4(),
            project_id: "FIND".to_string(),
            category: "lint".to_string(),
            severity: "warning".to_string(),
            scope_kind: "path".to_string(),
            scope_ref: "src/lib.rs".to_string(),
            description: "unused import".to_string(),
            provenance: json!([]),
            first_seen: now,
            last_seen: now,
            occurrences: 3,
            crystallize_state: None,
        }
    }

    #[test]
    fn finding_row_json_preserves_fields() {
        let row = sample_finding_row();
        let v = finding_row_json(&row);
        assert_eq!(v["id"], row.id.to_string());
        assert_eq!(v["category"], "lint");
        assert_eq!(v["severity"], "warning");
        assert_eq!(v["scope_kind"], "path");
        assert_eq!(v["scope_ref"], "src/lib.rs");
        assert_eq!(v["description"], "unused import");
        assert_eq!(v["occurrences"], 3);
        assert_eq!(v["first_seen"], row.first_seen.to_rfc3339());
        assert_eq!(v["last_seen"], row.last_seen.to_rfc3339());
    }

    #[test]
    fn finding_row_json_empty_rows_maps_to_empty_results() {
        let rows: Vec<super::super::findings_store::FindingRow> = Vec::new();
        let results: Vec<Value> = rows.iter().map(finding_row_json).collect();
        assert!(results.is_empty());
        // Mirrors the tool's own found:false-on-empty contract.
        assert_eq!(!results.is_empty(), false);
    }

    // ── kg_rules ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn rules_unconfigured_store_degrades_not_errors() {
        // Mirrors kg_findings's own test shape: never mutate global env to
        // force NotConfigured if a real DSN is already present in this
        // process.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let out = KgRules
            .execute_structured(json!({"project_id": "RULE"}))
            .await
            .unwrap();
        let v = val(out);
        assert_eq!(v["configured"], false);
        assert_eq!(v["found"], false);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn clamp_rules_limit_clamps_zero_and_over_cap() {
        assert_eq!(clamp_rules_limit(0), 1);
        assert_eq!(clamp_rules_limit(300), 200);
        assert_eq!(clamp_rules_limit(50), 50);
        assert_eq!(clamp_rules_limit(-5), 1);
        assert_eq!(clamp_rules_limit(200), 200);
        assert_eq!(clamp_rules_limit(1), 1);
    }

    fn sample_rule_row() -> super::super::rules_store::RuleRow {
        let now = chrono::Utc::now();
        super::super::rules_store::RuleRow {
            id: uuid::Uuid::new_v4(),
            project_id: "RULE".to_string(),
            scope_kind: "path".to_string(),
            scope_ref: "src/lib.rs".to_string(),
            category: "lint".to_string(),
            guidance: "always propagate errors, never unwrap".to_string(),
            enforcement: "advisory".to_string(),
            status: "active".to_string(),
            provenance: json!({}),
            recurrence_at_creation: Some(3),
            cortex_risk: Some(0.42),
            created_at: now,
            valid_from: now,
            valid_to: None,
        }
    }

    #[test]
    fn rule_row_json_preserves_fields() {
        let row = sample_rule_row();
        let v = rule_row_json(&row);
        assert_eq!(v["id"], row.id.to_string());
        assert_eq!(v["scope_kind"], "path");
        assert_eq!(v["scope_ref"], "src/lib.rs");
        assert_eq!(v["category"], "lint");
        assert_eq!(v["guidance"], "always propagate errors, never unwrap");
        assert_eq!(v["enforcement"], "advisory");
        assert_eq!(v["cortex_risk"], 0.42_f32);
    }

    #[test]
    fn rule_row_json_empty_rows_maps_to_empty_results() {
        let rows: Vec<super::super::rules_store::RuleRow> = Vec::new();
        let results: Vec<Value> = rows.iter().map(rule_row_json).collect();
        assert!(results.is_empty());
        assert_eq!(!results.is_empty(), false);
    }
}
