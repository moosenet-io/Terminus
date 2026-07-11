# Atlas — deferred enhancements (KGRAPH-16)

This note records enhancements to the Atlas knowledge-graph engine
(`src/scribe/graph/`, spec `S112-knowledge-graph-docs`) that were **deliberately
deferred**, so they are tracked rather than silently dropped. Each is a real
follow-on with a rough size and the phase/module it would extend. None is
required for the shipped engine (model → extraction → store → clustering →
`kg_*` query tools → PageRank → layout → SVG/GraphML/HTML renderers →
`scribe_kg_build` → docs embed) to be useful.

## Deferred items

### 1. CFG / data-flow (CPG) edge layer — size L
Add intra-procedural **control-flow** and **data-dependence** edges on top of
the current structural edges (calls / imports / references / contains), à la a
Joern-style Code Property Graph. This is what makes reachability and taint
questions answerable ("does this input reach that sink", "what feeds this
argument"). Extends: edge extraction (`extract.rs` / a new `cpg.rs`). Deferred
because it is a large, per-language effort well beyond the structural graph, and
the query/visual value lands first without it.

### 2. SCIP index ingestion — size L
Ingest existing **SCIP** indexes emitted by compiler-accurate indexers
(`rust-analyzer`, `scip-python`, `scip-typescript`) to get precise cross-file /
cross-dependency symbol resolution "for free," instead of (or alongside) the
tree-sitter extraction. Extends: a new ingest path feeding the KGRAPH-01 model.
Deferred because it requires those indexers in the build environment and a SCIP
protobuf reader; the tree-sitter path (plus KGRAPH-11 stack-graphs, when built)
covers the common case.

### 3. CPGQL-style traversal grammar — size L
A composable **map / filter / repeat / where** traversal query surface (à la
Joern's CPGQL) beyond the current fixed `kg_*` verbs, so a model or operator can
express arbitrary graph traversals in one call. Extends: the query tools
(`tools.rs`). Deferred because the fixed verbs (`search` / `neighbors` /
`subgraph` / `path` / `stats`) cover the overwhelmingly common questions at far
lower complexity and injection surface.

### 4. fs-watch auto-sync — size S–M
A filesystem watcher that debounces file-change events and calls
`scribe_kg_build` incrementally (KGRAPH-03 `refresh_files` / KGRAPH-10) to keep a
project's graph live without a manual rebuild. Extends: a new watcher
binary/daemon around `scribe_kg_build`. Deferred because the pipeline already
refreshes the graph at its docs stage (the companion HARM `KGWIRE` hook), which
covers the primary "keep it current on merge" need; a live watcher is a
developer-ergonomics add-on.

## Related but separately-tracked (not "deferred", just their own items)

- **Semantic (INFERRED) edges** — `KGRAPH-04`.
- **Stack-graphs scope-correct name resolution** — `KGRAPH-11`.
- **Per-community GraphRAG summaries + hierarchical Leiden** — `KGRAPH-12`.
- **Bi-temporal, commit-keyed edges + incremental merge** — `KGRAPH-15`.
- **`kg_query` dual-level routing + two-tier model config** — `KGRAPH-14`.
- **Per-project pipeline wiring** — companion HARM `KGWIRE-01/02`.
