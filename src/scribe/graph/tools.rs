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
//!
//! Per-project *exposure gating* (`expose_query`) is the companion HARM
//! pipeline config; these tools are always registered but simply report
//! "no graph for project" when a project has no stored graph. Read-only, no
//! secrets, no subprocess.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::model::KnowledgeGraph;
use super::store::GraphStore;
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::ScribeConfig;
use crate::tool::{RustTool, ToolOutput};

/// Load a project's graph from the configured store, or a typed "not found"
/// signal (`Ok(None)`).
fn load_graph(project_id: &str) -> Result<Option<KnowledgeGraph>, ToolError> {
    GraphStore::from_config(&ScribeConfig::from_env()).load(project_id)
}

/// Pull a required non-empty string argument.
fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required and must be a non-empty string")))
}

/// Standard "no graph yet" structured result.
fn no_graph(project_id: &str) -> Value {
    json!({"project_id": project_id, "found": false, "message": "no knowledge graph for this project (run scribe_kg_build first)"})
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
            return structured(no_graph(&project_id));
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
        structured(json!({"project_id": project_id, "found": true, "query": query, "count": results.len(), "results": results}))
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
            return structured(no_graph(&project_id));
        };
        if g.get_node(&node_id).is_none() {
            return structured(json!({"project_id": project_id, "found": false, "message": format!("no node '{node_id}'")}));
        }
        let mut out = Vec::new();
        let mut incoming = Vec::new();
        for e in g.edges() {
            if e.from == node_id {
                out.push(json!({"id": e.to, "kind": e.kind.as_str(), "confidence": e.confidence.as_str()}));
            }
            if e.to == node_id {
                incoming.push(json!({"id": e.from, "kind": e.kind.as_str(), "confidence": e.confidence.as_str()}));
            }
        }
        let mut res = json!({"project_id": project_id, "found": true, "node_id": node_id});
        if direction == "out" || direction == "both" {
            res["outgoing"] = json!(out);
        }
        if direction == "in" || direction == "both" {
            res["incoming"] = json!(incoming);
        }
        structured(res)
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
            return structured(no_graph(&project_id));
        };
        if g.get_node(&node_id).is_none() {
            return structured(json!({"project_id": project_id, "found": false, "message": format!("no node '{node_id}'")}));
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
        structured(json!({"project_id": project_id, "found": true, "root": node_id, "depth": depth, "nodes": nodes, "edges": edges}))
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
            return structured(no_graph(&project_id));
        };
        if g.get_node(&from).is_none() || g.get_node(&to).is_none() {
            return structured(json!({"project_id": project_id, "found": false, "message": "from/to node not in graph"}));
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
        structured(json!({"project_id": project_id, "found": true, "from": from, "to": to, "connected": !path.is_empty(), "path": path}))
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
            return structured(no_graph(&project_id));
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
        structured(json!({
            "project_id": project_id, "found": true,
            "nodes": g.node_count(), "edges": g.edge_count(),
            "clusters": clusters.len(), "orphans": orphans,
            "by_kind": by_kind, "hotspots": hotspots,
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
            return structured(no_graph(&project_id));
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
        structured(json!({"project_id": project_id, "found": true, "count": comms.len(), "communities": comms_json}))
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
    async fn missing_required_arg_is_invalid_argument() {
        let err = KgSearch.execute_structured(json!({"project_id": "X"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "missing query must be InvalidArgument, got {err:?}");
    }
}
