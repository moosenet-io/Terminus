# scribe

`src/scribe` — 739 KG symbols.

Scribe is the knowledge-infrastructure subsystem. Its centerpiece is **Atlas**
(`scribe::graph`): a per-project, persisted, queryable knowledge graph of a
codebase. Nodes are code entities (functions, structs, traits, modules, doc
sections) keyed by stable fully-qualified names; edges are calls/imports/
references stamped with confidence tiers. Extraction is tree-sitter-based across
~14 languages, with PageRank ranking, Louvain-style community clustering, and
optional embeddings. Around the graph, Scribe is also a standing documentation
agent that generates READMEs, wiki pages, and build-diary entries as a byproduct
of the build pipeline — dispatching LLM work through the review daemon, never
spawning subprocesses itself.

Atlas is the data source for this documentation: the subsystem inventory, symbol
counts, and hotspots cited across these pages come from its stored graph.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `scribe::graph::model::KnowledgeGraph` | struct | `src/scribe/graph/model.rs` | The in-memory graph: `insert_node` / `get_node` / edge storage. |
| `scribe::graph::model::KgNode` | struct | `src/scribe/graph/model.rs` | One code entity with fully-qualified id, kind, path, and rank. |
| `scribe::graph::store::GraphStore` | struct | `src/scribe/graph/store.rs` | Per-project persisted store (`new` / `exists`) under the configured store dir. |
| `scribe::graph::extract::build_graph` | fn | `src/scribe/graph/extract.rs` | Multi-language tree-sitter extraction of nodes/edges from a source tree. |
| `scribe::graph::rank::pagerank` | fn | `src/scribe/graph/rank.rs` | PageRank over the call graph (plus `personalized` variant) — powers hotspot ranking. |
| `scribe::graph::cluster::cluster` | fn | `src/scribe/graph/cluster.rs` | Community clustering over the graph. |
| `scribe::graph::findings_store` | module | `src/scribe/graph/findings_store.rs` | Persistent review-findings store (`kg_findings`) keyed to graph entities. |
| `scribe::graph::rules_store` | module | `src/scribe/graph/rules_store.rs` | Crystallized review rules (`kg_rules`, `kg_rule_crystallize`, `kg_rule_promote`). |
| `scribe::ScribeConfig` | struct | `src/scribe/mod.rs` | Env-sourced Scribe configuration (`from_env`). |
| `scribe::inspect` | module | `src/scribe/inspect.rs` | Read-only inspection worktrees for documentation runs. |

## Tools

Graph queries: `kg_search`, `kg_semantic_search`, `kg_neighbors`, `kg_subgraph`,
`kg_path`, `kg_stats`, `kg_communities`, `kg_file_symbols`, `kg_query`,
`kg_findings`, `kg_rules`, `kg_rule_crystallize`, `kg_rule_promote`,
`kg_rebuild`, `kg_embeddings`. Documentation agent: `scribe_generate_readme`,
`scribe_update_wiki_page`, `scribe_build_diary_entry`,
`scribe_report_discrepancy`, `scribe_status`.

## How it connects

Registered on the core registry (`register_all`). Three subsystems consume the
graph in-process: `review::kg_context` injects KG blocks into review prompts and
records findings; `cortex` walks it for blast-radius (`cortex_scope`), risk
scoring (`cortex_review`), and audits; `tools::docgen` reads it for repository
facts and diagrams. Scribe's own prose generation goes out through the review
daemon (`REVIEW_DAEMON_URL`/`REVIEW_DAEMON_TOKEN`, read via
`review::ReviewConfig::from_env`, never duplicated).

## Configuration

`SCRIBE_KG_STORE_DIR` (per-project graph store root), `SCRIBE_REPO_PATH` and
`SCRIBE_ALLOWED_REPO_ROOTS` (which checkouts extraction may read),
`SCRIBE_KG_EMBED` / `SCRIBE_KG_SEMANTIC` (optional embedding/semantic layers),
`SCRIBE_WORKTREE_ROOT` (inspection worktrees), `SCRIBE_VAULT_REMOTE` /
`SCRIBE_VAULT_LOCAL_DIR` (wiki/diary vault), `SCRIBE_PENDING_QUEUE_PATH`,
`SCRIBE_ALLOW_SUBPROCESS_INSPECTION` / `SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE`
(explicit opt-ins for the two guarded subprocess seams).

## Notes and gaps

Community summaries and `kg_query` natural-language answers are only as good as
the summary-writeback state of the serving store — an empty-summary store yields
thin `kg_query` results. This page does not cover the render/layout SVG output
(`graph::render`, `graph::layout`) or the deferred enhancements list — see
[docs/atlas/deferred-enhancements.md](../atlas/deferred-enhancements.md) and the
[Atlas query-tools page](atlas-knowledge-graph-query-tools.md).
