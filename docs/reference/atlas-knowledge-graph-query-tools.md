## Atlas — knowledge-graph query tools

Atlas builds a per-project knowledge graph from **any of ~14 languages** (Rust, Python, JavaScript/TypeScript, Go, Java, C, C++, Ruby, Lua, C#, PHP, Bash) via tree-sitter, not just Rust (KGRAPH-17). Atlas (the knowledge-graph subsystem of the Scribe documentation engine, spec
`S112-knowledge-graph-docs`) builds a per-project graph of a codebase — nodes
are code entities (functions/structs/…), edges are calls/imports/references
tagged with a confidence tier — and exposes it to local models as `kg_*` tools
on the core registry, so a model can query the graph instead of grepping source:

| Tool | What it answers |
| --- | --- |
| `kg_search` | Find entities by name or id substring. |
| `kg_neighbors` | What a node calls/imports/references, and what references it. |
| `kg_subgraph` | The local neighborhood (blast radius) around a symbol, to a depth. |
| `kg_path` | The shortest path connecting two entities. |
| `kg_stats` | Node/edge counts, clusters, top-degree hotspots, orphans. |
| `kg_communities` | The community structure (level-0 clusters + a coarser level-1), each with members and — when a model is available — a short summary, for answering subsystem/architecture questions at the right zoom. |
| `kg_query` | Answer a natural-language question — routes automatically to entity-level retrieval (specific symbols) or community-level retrieval (architecture/subsystems), returns the context plus a synthesized answer when a model is available. |
| `kg_file_symbols` | The symbols a given repo-relative file defines, sorted by PageRank importance. |
| `kg_semantic_search` | Meaning-based (embedding) search — finds nodes related to `query` even without a shared substring. Degrades to `configured:false` when embeddings aren't set up; see [KGEMB-04](#kg-semantic-search-tool-kgemb-04) below. |
| `kg_findings` | Lists captured analysis findings (lint-like observations, review notes, anomalies) for a project, ordered by recurrence, with optional `scope`/`category`/`min_occurrences` filters. Degrades to `configured:false` when the findings store isn't set up; see [KGFIND-04](#kg-findings-tool-kgfind-04) below. |

All take a `project_id` and read the per-project graph store
(`SCRIBE_KG_STORE_DIR`); a project with no graph yet returns `found: false`
rather than an error. Graphs are produced/refreshed by the build pipeline's
docs stage (`scribe_kg_build`).

A graph is produced end-to-end by **`scribe_kg_build`** (`project_id`,
`repo_path` under `SCRIBE_ALLOWED_REPO_ROOTS`; `incremental` + `changed_files`
to patch only those files) — it walks the repo, extracts → clusters → lays out
→ renders, stores the graph JSON, and writes the visual artifacts.
**`scribe_kg_status`** reports a project's counts, freshness, and which
artifacts exist. When `scribe_generate_readme` is given a `project_id` whose
graph has been built, it appends the rendered map (`map.svg` + confidence
legend) to the generated README as an **"## Architecture map"** section — so the
graph informs the doc's visual output; projects without a graph are unchanged.

A graph also renders to three visual artifacts (all from one shared
force-directed layout, so they agree): a static **`map.svg`** — nodes colored by
cluster, sized by degree, edges styled by confidence (solid EXTRACTED / dashed
INFERRED / dotted AMBIGUOUS) with a legend — which Scribe embeds directly in the
README/wiki/vault; a **`graph.graphml`** interchange file for Gephi/yEd/
Cytoscape; and a self-contained interactive **`graph.html`** (inline SVG with
vanilla-JS pan/zoom/search, no external hosts).

### `review_run` is KG-grounded (KGREV-01)

`review_run` best-effort grounds every dispatched review in the project's
Atlas graph: before building each provider's prompt, it looks for two optional
keys on `context`:

| Context key | Type | Purpose |
| --- | --- | --- |
| `project_id` | string | Which project's stored Atlas graph (`SCRIBE_KG_STORE_DIR`) to consult. Omit this and nothing below happens — the review is byte-for-byte identical to a build with no Atlas awareness at all. |
| `changed_files` | array of repo-relative path strings | The files under review. If omitted, they're parsed from `context.diff`'s unified-diff `+++ b/<path>` headers instead. |

When `project_id` resolves to a graph with at least one node defined in a
changed file, `review_run` injects a bounded `knowledge_graph` block into
`context` — the touched symbols (id/name/kind/cluster) plus up to a few 1-hop
callers and callees each (≤ 40 symbols total, ≤ ~2 KB serialized; a
`"truncated": true` marker appears if the cap was hit) — and every provider's
prompt gets a one-line pointer to it ("... weigh cross-module impact").
Grounding is entirely best-effort: no `project_id`, no stored graph, or no
node matching any changed file all silently skip injection — never an error,
never a partial/empty block.

### `review_run` rebuilds the graph on pass + holds a per-project lock (KGREV-02)

When a dispatched review's aggregate verdict is `APPROVE` and `complete`, and
`context` carries both `project_id` and `repo_path` (an absolute path under
`SCRIBE_ALLOWED_REPO_ROOTS`), `review_run` incrementally rebuilds that
project's Atlas graph via `scribe_kg_build` (`incremental: true`,
`changed_files` reusing the same derivation KGREV-01 uses) — so the graph the
*next* review consults reflects the change that was just approved.

While that rebuild is in flight, `review_run` holds a per-project lock keyed
by `project_id`. Another call with the SAME `project_id` short-circuits
immediately at the top of `execute()`:

```json
{ "structure": "...", "providers": [], "aggregate_verdict": "UNKNOWN",
  "complete": false, "locked": true,
  "reason": "KG rebuild in progress for <project>; retry when ready" }
```

No providers are dispatched on a locked call. Reviews of *different*
`project_id`s never block each other. The lock is released via an RAII guard
on every path — rebuild success, rebuild error, or a panic-unwind — so it can
never deadlock a project.

The rebuild is entirely non-blocking to the review result: a rebuild failure
(bad `repo_path`, disallowed root, etc.) is logged and reported in a
`kg_rebuild` field, and never turns an `APPROVE` into a tool error or changes
the aggregate verdict. Every `review_run` result now includes `kg_rebuild`:

| Shape | Meaning |
| --- | --- |
| `{"ran": false, "reason": "..."}` | Not an approved+complete pass, or `project_id`/`repo_path` missing — no lock taken, backward compatible. |
| `{"ran": true, "ok": true, "nodes": …, "edges": …, "clusters": …, "mode": "incremental"}` | Rebuild succeeded. |
| `{"ran": true, "ok": false, "error": "..."}` | Rebuild failed; review verdict is unaffected. |

### `review_run` refreshes docs through the SCRIBE door on pass (KGREV-03)

When a dispatched review's aggregate verdict is `APPROVE` and `complete`, and
`context` also carries both `project` and `spec_id`, `review_run` drives a doc
refresh through the ONE sanctioned doc-generation door — the existing
`docgen_run` tool (`crate::tools::docgen::trigger::DocgenRun`), called
in-process. This runs **after** the KGREV-02 rebuild above, so the doc engine
sees the just-refreshed Atlas graph.

| Context key | Type | Purpose |
| --- | --- | --- |
| `project` | string | Passed through to `docgen_run` as `project`. Required (with `spec_id`) to trigger a doc refresh at all. |
| `spec_id` | string | Passed through to `docgen_run` as `spec_id`. Required (with `project`). |
| `git_ref` | string, optional | Passed through to `docgen_run` as `git_ref`. Defaults to `"unknown"` if omitted. |
| `module_path` | string, optional | Passed through to `docgen_run` as `module_path`. Defaults to `"."` if omitted. |
| `project_config` | object, optional | Passed through to `docgen_run` as `project_config` (the project's doc-target config). Omitting it means `docgen_run`'s own opt-in gate skips cleanly — no doc-target config declared. |
| `diff` | string, optional | Passed through to `docgen_run` as the unswept `feat_context` (`docgen_run` runs its own PII sweep before anything else touches it). |
| `repo_path` | string, optional | (DLAND-04) When present, also passed through to `docgen_run` as `place: true` + `target_root: repo_path` — a passing epic capstone doesn't just generate docs, it lands them (`README.md` + `docs/**`) directly into that working tree. Absent → generation-only, exactly the pre-DLAND-04 behavior. |

If `project`/`spec_id` are absent, this is a no-op — most reviews won't supply
doc params; the wire only fires for real merge-time reviews that do. The doc
refresh is entirely non-blocking to the review result: `docgen_run` is
already structurally non-blocking (an internal doc-gen failure surfaces as
`outcome: "failed"`, never a tool error), and any unexpected error calling it
is caught, logged, and reported rather than propagated — it never turns an
`APPROVE` into a tool error or changes the aggregate verdict. This includes
the DLAND-04 placement step itself: a placement failure (bad `target_root`, a
DLAND-03 landing-lint gate failure, an I/O error) shows up nested inside the
returned `docgen`/`outcome` JSON exactly like any other docgen-internal
failure — it is never fatal to the review. Every `review_run` result now
includes `scribe_docs`:

| Shape | Meaning |
| --- | --- |
| `{"ran": false, "reason": "not an approved pass"}` | Not an approved+complete pass. |
| `{"ran": false, "reason": "no doc params"}` | `project`/`spec_id` missing — no `docgen_run` call. |
| `{"ran": true, "outcome": "skipped"\|"completed"\|"failed", "docgen": {...}}` | `docgen_run` was called; `docgen` carries its full structured result. |
| `{"ran": true, "ok": false, "error": "..."}` | Calling `docgen_run` itself errored unexpectedly; review verdict is unaffected. |

No direct doc-generation HTTP/Chord call is made from `review_run` — the only
doc path is the existing `docgen_run` tool (S9 single door).

### Doc engine placement — writing the artifacts to disk (DLAND-01)

Every doc-engine renderer (`readme_layers::render_layered_readme`,
`render::docs_tree::build_docs_tree`, `render::render_all`) is pure: it
*returns* the rendered concise README and `docs/` tree and never touches a
filesystem, git, or network.
`crate::tools::docgen::place::place_docs(target_root, landing, docs_tree)` is
the one placement step that actually writes those artifacts into a real
working tree — `README.md` plus every `docs/**` file, at their exact
repo-relative paths. Writes are atomic (tempfile + `rename`) and idempotent
(a file whose on-disk content already matches is left untouched, so a
no-op run produces an empty diff); any path that is absolute or escapes
`target_root` via `..` is refused and reported rather than written. It
touches no git and no network — placement only, same as every other step in
this engine keeps those concerns separate.

### Wiring placement into the pipeline (DLAND-04) — capstone-APPROVE places the docs

`trigger::run_docgen_trigger` and the `docgen_run` tool are the one place
DLAND-01's placement primitive is actually wired in. Both now accept two
additional, OPTIONAL parameters:

| Param | Type | Default | Purpose |
| --- | --- | --- | --- |
| `place` | bool | `false` | Opt-in switch. When `false` (the default), behavior is byte-for-byte unchanged from before DLAND-04 — no filesystem is ever touched. |
| `target_root` | string, optional | absent | A working-tree root (e.g. a repo checkout or worktree path) to place `README.md`/`docs/**` into. Only has an effect when `place: true` is also given. |

When `place: true` and `target_root` are both given AND generation actually
produced content, `run_docgen_trigger` takes the concise landing README that
`render_all` already rendered (the `readme` target) and the `docs/` tree
`render_all` already built from that SAME generated content
(`RenderOutcome::docs_tree`) — reusing both, never re-deriving them — and
calls `place_docs(target_root, landing, docs_tree)`. The result is folded
into a new `placement: Option<PlacementReport>` field on
`TriggerOutcome::Completed`. This is still a **local working-tree write
only** — no git add/commit/push, no forge call, no new Plane/Gitea/GitHub
door — the pipeline's own existing git stages are what carry the resulting
working-tree change forward. Non-blocking, matching every other step in this
engine: a placement failure (bad `target_root`, a DLAND-03 landing-lint gate
failure, an I/O error) is recorded in `placement` (`gate_failures`/`skipped`),
never turned into an `Err` or a panic, and never reverts a merge or fails the
build.

**No-loss guard on the placement path (DLAND-CAP-01).** Because this automatic
door overwrites a real `README.md`, it enforces the same DLAND-02 no-loss
guarantee as `docgen_backfill`: before placing, `run_docgen_trigger` reads the
target's current `README.md` (using it both to *deepen* generation and to
check preservation), runs `check_preservation`, and **withholds the whole
cutover** — writing nothing and recording the dropped sections in
`placement.gate_failures` — if any old section's substance would be lost. An
absent README is a safe first-doc (placed normally); a present-but-unreadable
README is never overwritten. So a passing capstone can never silently drop
content from a not-yet-migrated bloated README.

`review_run`'s post-capstone doc hook (KGREV-03, above) is the first caller
of this opt-in path: when a passing epic capstone's context carries
`repo_path`, it passes `place: true` + `target_root: repo_path` through to
`docgen_run`, so an `APPROVE` epic capstone lands its generated docs directly
into the working tree, riding the same once-per-build capstone path as
generation (never per-merge).

### One-shot backfill — migrating an already-bloated README (DLAND-05, mechanically relocated per DLAND-RELOC)

`docgen_backfill` (`crate::tools::docgen::backfill::DocgenBackfill`, function
`backfill_readme`) is the tool for a repo's *first* cutover — migrating a
hand-grown mega-README (Terminus, Chord, Muse, lumina-constellation, ...)
into a concise landing + `docs/` hierarchy in one guarded pass.

**Why mechanical relocation, not LLM regeneration.** A live run proved the
original DLAND-05 flow (ask the LLM to regenerate the whole README) is lossy
BY CONSTRUCTION: an LLM asked for a concise landing naturally *summarizes* (a
2285-line README became a 61-line landing) and produces no `docs/` tree of
its own, so the DLAND-02 no-loss guard correctly withheld every cutover.
`backfill_readme` now splits the two jobs the old flow conflated:

1. Reads `target_root`'s current `README.md` (if any) and passes it as
   `existing_docs` into `run_docgen_trigger`'s own sweep → generate → render
   flow (called with `place: false`), but uses the result **ONLY** for its
   concise top-page landing text (hero + quick start) — the LLM's own
   `docs_tree` is **ignored** (it never saw the old README's sections
   rendered as its own docs, so it's necessarily empty for this flow).
2. Builds the `docs/` tree **mechanically**, directly from the OLD README:
   a purpose-built **byte-offset slicer** (`old_readme_parts`) splits it into
   a preamble plus one entry per top-level `## ` section, and each section's
   **EXACT ORIGINAL SOURCE BYTES** (from its `## ` line through just before
   the next `## `/EOF — sub-headings, code fences, and spacing preserved
   byte-for-byte) are copied **VERBATIM** into its own
   `docs/reference/<slug>.md` page — no paraphrasing, no summarizing, not even
   an appended newline. This slicer is deliberately distinct from the no-loss
   guard's line-based `preserve::split_old_sections` (which normalises
   whitespace to COMPARE tokens); both agree on where `## ` sections begin.
   `docs/index.md` is a hub page: a short title, the verbatim preamble (if
   any), and a link list to every relocated page. Slugs are de-duplicated with
   a numeric suffix on collision; a README with no `##` headings at all still
   relocates as one `docs/reference/overview.md` page (the whole document
   verbatim), so section-less READMEs are never silently dropped either.
3. Assembles the final landing: the LLM's hero/quick-start text with any
   LLM-authored `## Documentation` section and any `docs/…` links stripped
   (they would dangle against the mechanical tree — a leftover of the LLM
   inventing paths that were never actually rendered), followed by a
   mechanically-built `## Documentation` section linking to the real
   `docs/index.md` hub (and up to 3 of its reference pages) — every `docs/`
   link this step emits comes directly from the mechanical `docs_tree`, so
   `check_landing_links` always resolves against it. An oversized LLM
   hero/quick-start is **trimmed** to fit the `LANDING_MAX_LINES` budget
   (trailing prose cut, the Documentation link never dropped) rather than
   refused.
4. Runs the DLAND-02 no-loss guard (`check_preservation`) against the OLD
   README vs. the assembled landing + mechanical `docs/` tree as a
   **backstop** — verbatim relocation makes `missing` empty / coverage
   `1.0` true *by construction*, but the check stays wired in rather than
   trusted blindly. If it ever did flag a drop, nothing is placed at all —
   not `README.md`, not a single `docs/**` file — and the flagged sections
   are returned in `BackfillReport::missing` for an operator to confirm
   before re-running.
5. Runs the DLAND-03 landing gates (length + link-resolution) and surfaces
   the outcome even though `place_docs` also enforces them fail-closed.
6. Only when both checks clear does it call `place_docs(target_root,
   landing, docs_tree)` — the same DLAND-01 writer every other placement
   path uses.

`BackfillReport` carries the old/new README line counts, the no-loss
coverage ratio, any missing sections, which `docs/**` files were created,
and whether placement actually happened, plus a human-readable `summary`.
Like every other step in this engine, `docgen_backfill` **never runs git**
(no add/commit/push) and makes **no Plane/Gitea/GitHub call of any kind** —
working-copy write only. The resulting working-tree diff is handed to the
normal build pipeline (review → merge) for an operator to bless, exactly
like any other change to a tracked repo: a first cutover is always
operator-reviewed, never auto-committed. Idempotent — re-running against an
already-migrated repo either reports `GenerationOutcome::NoChange` or a
placement whose `written` list is empty (byte-identical content already on
disk).

### Atlas vector store (KGEMB-01)

Phase 1 of KG-as-behavioral-correction adds semantic (meaning-based) retrieval
alongside the lexical `kg_search` above. `AtlasVecStore`
(`src/scribe/graph/vec_store.rs`) owns a dedicated Postgres table,
`kg_embeddings`, holding one 768-dim [pgvector](https://github.com/pgvector/pgvector)
embedding per `(project_id, node_id)`, plus the `card_hash` of the text that
was embedded (so a rebuild can skip re-embedding unchanged nodes) and an HNSW
cosine-similarity index for fast top-K search.

- **`ATLAS_DATABASE_URL`** — the dedicated Postgres DSN for the embeddings
  store. This is the ONLY source for the store's DSN — there is deliberately no
  fallback to a shared `DATABASE_URL`, so the store stays isolated to its own
  database. When `ATLAS_DATABASE_URL` is unset, `AtlasVecStore::from_env()`
  returns `NotConfigured` cleanly — no connection is attempted, and callers (the
  build-time embed step and the `kg_semantic_search` tool) degrade to the
  existing lexical path rather than failing.
- The migration (`CREATE EXTENSION IF NOT EXISTS vector`, the table, and its
  `hnsw (embedding vector_cosine_ops)` index) is idempotent and
  advisory-lock-serialized, safe to run on every `from_env()` call including
  from concurrent processes. HNSW index creation is best-effort: if a given
  pgvector build rejects it, the table still works (exact top-K scan via
  `<=>`), just without the ANN speedup.
- Typed methods: `upsert` (batched, parameterized, `ON CONFLICT` update),
  `delete` (by `node_id` list), `existing_hashes` (for incremental
  hash-diff skip), and `query_topk` (cosine similarity, descending).
- This module lands only the store. The embeddings client, the gated
  build-time wiring, and the `kg_semantic_search` tool are later items in
  spec `S113-kg-semantic-embeddings` (KGEMB-02/03/04).

### KG embeddings client (KGEMB-02)

`EmbedClient` (`src/scribe/graph/vec_embed.rs`) turns text into a vector
against a configurable endpoint, provider-agnostic between the local Ollama
shape and hosted OpenAI-style APIs, auto-detected from the URL:

- Ollama (`/api/embeddings`, `{"model","prompt"}` → `{"embedding":[...]}`) —
  the default, matching the CPU-tier ollama unit already used elsewhere.
- OpenAI-style (any URL containing `/v1/embeddings`, `{"model","input"}` →
  `{"data":[{"embedding":[...]}]}`) — for hosted providers (e.g. an
  OpenRouter-compatible embeddings endpoint), with bearer auth.

Config (non-secret, via `crate::config`):

- **`EMBEDDINGS_URL`** — the embeddings endpoint. Defaults to the secondary
  (CPU) ollama unit's `OLLAMA_CPU_URL` + `/api/embeddings`; with neither set,
  falls back to a loopback CPU-ollama default (never a real non-loopback host
  baked in).
- **`EMBEDDINGS_MODEL`** — the model name sent on each request. Defaults to
  `nomic-embed-text`.
- **`EMBEDDINGS_TIMEOUT_MS`** — per-request timeout. Defaults to 30000 (30s).

**`EMBEDDINGS_API_KEY`** (optional, for hosted providers) is secret material
and is read directly from the env-materialized runtime secret store inside
`vec_embed` itself, not from `crate::config` — this crate has no separate
`SecretManager`/`vault` API of its own (same convention as `crate::pki`'s CA
material and `review::dispatch`'s `OPENROUTER_API_KEY`: the deployment's
secret store materializes into env at startup, so a plain env read afterward
already IS the SecretManager read). When unset, no `Authorization` header is
sent (Ollama needs none).

`EmbedClient::embed`/`embed_batch` never panic: transport, HTTP-status, and
parse failures all become a `ToolError` for the caller to log and skip — a
best-effort contract, since KGEMB-03's build-time wiring must never block on
an embeddings outage.

`node_card(node, callers, callees)` builds the deterministic short text that
gets embedded for a `KgNode`: `"{kind} {name} in {path}"`, plus (if any
neighbors) `" — calls: ...; called by: ..."`, each neighbor list capped at 6
names and the whole card capped at 512 characters (truncated on a char
boundary).

This item ships only the client + card builder — it is not yet wired into
`scribe_kg_build` (that's KGEMB-03).

### `kg_semantic_search` tool (KGEMB-04)

`kg_semantic_search(project_id, query, limit?)` (`src/scribe/graph/tools.rs`)
is the query-side counterpart to KGEMB-01/02/03: it embeds `query` with
`EmbedClient`, asks `AtlasVecStore::query_topk` for the nearest node ids by
cosine similarity, joins the hits against the project's currently-loaded
Atlas graph, and returns `{id,name,kind,path,score,cluster}` per hit ordered
by similarity (descending — the store's own order is preserved, never
re-sorted). `limit` is optional (default 10) and clamped to `[1, 50]`.

**Degrade-to-lexical contract:** this tool is safe to call unconditionally,
including in a deployment that has never enabled embeddings:

| Condition | Result |
| --- | --- |
| `AtlasVecStore::from_env()` returns `NotConfigured` (`ATLAS_DATABASE_URL` unset) | `{"configured": false, "found": false, "results": []}` — a normal result, not a tool error. Callers should fall back to `kg_search`. |
| The store is configured but some other error occurs (e.g. connect failure) | Also degrades to `{"configured": false, "found": false, "results": [], "error": "..."}` rather than a hard error. |
| The embeddings endpoint is down/unreachable at query time | `{"configured": true, "found": false, "results": [], "error": "..."}` — the store IS configured, but the query embedding itself failed. |
| No knowledge graph exists for `project_id` yet | `{"configured": true, "found": false, "count": 0, "message": "..."}` — a genuine empty result, not a config problem (run `scribe_kg_build` first). |
| Both are up, query ran | `{"configured": true, "found": <has-results>, "project_id", "count", "results": [...]}` — `found` reflects whether there were actual matches (zero hits, or every hit dropped as a stale row, is `found:false`). |

A vector-store row whose `node_id` is no longer present in the currently
loaded graph (e.g. the graph was rebuilt and the symbol was removed/renamed)
is silently dropped from the results rather than surfaced — stale-row
tolerance, so a query never returns a dangling reference.

### `kg_findings` tool (KGFIND-04)

`kg_findings(project_id, scope?, category?, min_occurrences?, limit?)`
(`src/scribe/graph/tools.rs`) is the read-only query counterpart to the
KGFIND-01 `FindingsStore`: it lists a project's captured findings ordered by
recurrence (`occurrences DESC, last_seen DESC`), so the corpus is inspectable
independent of the write path. `scope` filters to one of
`node`/`path`/`community`/`global`; `category` and `min_occurrences` narrow
further; `limit` is optional (default 50) and clamped to `[1, 200]`.

**Degrade contract**, mirroring `kg_semantic_search`:

| Condition | Result |
| --- | --- |
| `FindingsStore::from_env()` returns `NotConfigured` (`ATLAS_DATABASE_URL` unset) | `{"configured": false, "found": false, "results": []}` — a normal result, not a tool error. |
| The store is configured but some other error occurs (e.g. connect failure) | Also degrades to `{"configured": false, "found": false, "results": [], "error": "..."}` rather than a hard error. |
| Store configured, query ran, no matching rows | `{"configured": true, "found": false, "project_id", "count": 0, "results": []}` — a genuine empty result, not a config problem. |
| Store configured, matches found | `{"configured": true, "found": true, "project_id", "count", "results": [{id, category, severity, scope_kind, scope_ref, description, occurrences, first_seen, last_seen}, ...]}` ordered by recurrence. |

