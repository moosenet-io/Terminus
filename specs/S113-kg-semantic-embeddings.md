# KG semantic embeddings (pgvector on <host>)
plane_project: TERM
module: Terminus
prefix: KGEMB
spec_id: S113-kg-semantic-embeddings

## Metadata
- **Author:** Moose
- **Session:** S113
- **Date:** 2026-07-11
- **Module version:** Terminus main
- **Estimated total:** ~10h autonomous agent work
- **Context:** Atlas KG retrieval today is purely lexical (`kg_search` = substring match on
  name/id). This spec adds semantic retrieval: embed each KG node's card with a
  provider-agnostic embeddings client (default the local Ollama `nomic-embed-text`, swappable to
  OpenRouter), store the vectors in pgvector on the shared Postgres, and add a
  `kg_semantic_search` tool that finds nodes by meaning. This is Phase 1 of the KG-as-behavioral-
  correction program; embeddings are also the dedup substrate later phases build on. Everything is
  best-effort and non-blocking: if the vector store or embeddings provider is unconfigured or
  unreachable, KG builds and reviews are unaffected and search degrades to the existing lexical path.

## Pre-flight
- Repo `moosenet/Terminus` on `main`, clean, tests green. Reviewers live (`codex`+`agy`).
- Infra provisioned (ops, done): pgvector DB reachable via `ATLAS_DATABASE_URL`; an embeddings
  endpoint at `EMBEDDINGS_URL` serving `EMBEDDINGS_MODEL` (768-dim). Neither is required for unit
  tests (they assert the NotConfigured/degrade paths); live tests early-return when unset.
- Reuse templates (confirmed): bounded PgPool `src/vector/mod.rs`; advisory-locked idempotent
  migration `src/intake/assistant/schema.rs`; Ollama `/api/embeddings` call `src/intake/infer.rs`;
  build insertion after pagerank `src/scribe/graph/build.rs`; tool registration `src/registry.rs`.

### KGEMB-01: Atlas pgvector store
- **Priority:** High
- **Labels:** terminus, knowledge-graph, pgvector
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A new module that owns the pgvector table for KG node embeddings and all its
  queries. Bounded PgPool from `ATLAS_DATABASE_URL` (dedicated var, falling back to `DATABASE_URL`),
  idempotent advisory-locked migration creating the `vector` extension + table + ANN index, and
  typed upsert/delete/top-K/hash-diff methods. Returns `NotConfigured` cleanly when the DSN is unset
  so every caller can degrade.

  ## FILES
  - `Cargo.toml` — add `pgvector` crate with its `sqlx` feature (pin a version compatible with
    sqlx 0.8).
  - `src/scribe/graph/vec_store.rs` — NEW: `AtlasVecStore` (pool + methods) + migration.
  - `src/scribe/graph/mod.rs` — `pub mod vec_store;` and re-export.
  - `src/config.rs` — `pub fn atlas_database_url() -> Option<String>` mirroring
    `intake_database_url()` (`ATLAS_DATABASE_URL` then `DATABASE_URL`, blank = unset).
  - `README.md` — document the atlas vector store + `ATLAS_DATABASE_URL`.

  ## APPROACH
  1. `atlas_database_url()` in config.rs via the existing `env_nonempty` helper (dedicated →
     shared fallback), exactly like `intake_database_url()`.
  2. `AtlasVecStore::from_env() -> Result<Self, ToolError>`: resolve the DSN (→ `NotConfigured` if
     absent), build `PgPoolOptions::new().max_connections(4).connect(&url)` (mirror
     `src/vector/mod.rs:106`), then run `migrate()`.
  3. `migrate(&pool)`: acquire one connection, `pg_advisory_lock($1)` with a fixed key, run
     `CREATE EXTENSION IF NOT EXISTS vector`, `CREATE TABLE IF NOT EXISTS kg_embeddings(project_id
     text, node_id text, model text NOT NULL, dim int NOT NULL, embedding vector(768) NOT NULL,
     card_hash text NOT NULL, updated_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY
     (project_id, node_id))`, and a `CREATE INDEX IF NOT EXISTS ... USING hnsw (embedding
     vector_cosine_ops)`, then `pg_advisory_unlock`. All `IF NOT EXISTS`, idempotent (mirror
     `src/intake/assistant/schema.rs:95`). Dimension 768 is a `const KG_EMBED_DIM`.
  4. Methods (all parameterized, `pgvector::Vector` for the vector column):
     - `upsert(project_id, rows: &[(node_id, card_hash, model, Vec<f32>)])` → `INSERT ... ON
       CONFLICT (project_id, node_id) DO UPDATE` set embedding/card_hash/model/updated_at. Batch.
     - `delete(project_id, node_ids: &[String])` → `DELETE ... WHERE project_id=$1 AND node_id =
       ANY($2)`.
     - `existing_hashes(project_id) -> HashMap<String,String>` (node_id→card_hash) for
       incremental skip.
     - `query_topk(project_id, q: &[f32], k) -> Vec<(String, f32)>` → `ORDER BY embedding <=> $q
       LIMIT k`, returning `node_id` and cosine similarity `1 - (embedding <=> $q)`.
  5. No secret literals; the DSN is a plain env var (matching intake). No `unwrap` on pool/lock —
     map to `ToolError::Database`.

  ## TEST PLAN
  - `cargo test -p terminus-rs --lib scribe::graph::vec_store` — passes.
  - `#[serial]` test: `ATLAS_DATABASE_URL` unset → `from_env()` returns `NotConfigured` (no
    connect attempt). Follow `src/vector/mod.rs:807` shape (early-return if a real DSN is present).
  - Pure tests: the migration SQL and upsert/query SQL are `const`s asserted non-empty and
    containing `vector(768)` / `<=>`; `card_hash` helper is deterministic for the same input.
  - `cargo test --workspace` — existing tests unaffected (note the known pre-existing
    `no_pii_in_own_source_tree` failure, TERM-247, is unrelated).

  ## EDGE CASES
  - DSN unset → `NotConfigured`, never a panic, never a connect to a bogus host.
  - Dimension mismatch (a row with wrong-length vector) → rejected by the `vector(768)` column;
    surface the DB error, do not crash.
  - Empty `rows`/`node_ids` slices → no-op, `Ok`.
  - Advisory lock contention (two builds) → serialized by `pg_advisory_lock`, both succeed.

- **Acceptance criteria:**
  - [ ] `AtlasVecStore::from_env` returns `NotConfigured` when `ATLAS_DATABASE_URL` is unset and
        otherwise builds a bounded pool and runs the idempotent migration.
  - [ ] `kg_embeddings` table has a `vector(768)` column + an HNSW cosine index; migration is
        idempotent (safe to run repeatedly, advisory-locked).
  - [ ] upsert/delete/existing_hashes/query_topk are parameterized (no string interpolation) and
        typed; query_topk returns cosine similarity ordered desc.
  - [ ] Empty-input methods are no-ops; unset DSN never connects.
  - [ ] README documents the store + `ATLAS_DATABASE_URL`.
  - [ ] No hardcoded infrastructure values in new/modified code.
  - [ ] All existing tests still pass.

### KGEMB-02: Provider-agnostic embeddings client + card builder
- **Priority:** High
- **Labels:** terminus, knowledge-graph, embeddings
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A small client that turns text into a vector against a configurable endpoint,
  supporting both the Ollama (`/api/embeddings`, `{model,prompt}→{embedding}`) and OpenAI-style
  (`/v1/embeddings`, `{model,input}→{data[0].embedding}`) shapes, auto-detected from the URL, with
  optional bearer auth for hosted providers. Plus a deterministic "card" builder that turns a
  `KgNode` (+ its 1-hop neighbor names from the graph) into the short text we embed.

  ## FILES
  - `src/scribe/graph/vec_embed.rs` — NEW: `EmbedClient` + `node_card`.
  - `src/config.rs` — `embeddings_url()` (`EMBEDDINGS_URL`, default `OLLAMA_CPU_URL` +
    `/api/embeddings`), `embeddings_model()` (`EMBEDDINGS_MODEL`, default `nomic-embed-text`),
    `embeddings_timeout_ms()`.
  - `README.md` — document the embeddings client + env vars.

  ## APPROACH
  1. Config resolvers via `env_nonempty` (non-secret) for URL/model/timeout. The optional API key
     is a SECRET: read via `vault::manager().get("EMBEDDINGS_API_KEY").ok()` — NEVER `std::env::var`
     for the key.
  2. `EmbedClient::from_env()` builds a `reqwest::Client` with timeout; stores url + model +
     optional key. Shape is derived from the URL path: contains `/v1/embeddings` → OpenAI shape;
     else Ollama shape.
  3. `async fn embed(&self, text: &str) -> Result<Vec<f32>, ToolError>`: POST the right body,
     bearer if a key is set, parse the right response field, return the vector. `embed_batch` maps
     over inputs with bounded concurrency (or the provider's native batch for OpenAI `input: [..]`).
  4. `node_card(node: &KgNode, callers: &[&str], callees: &[&str]) -> String`: e.g.
     `"{kind} {name} in {path}"` + (if any) `" — calls: {callees…}; called by: {callers…}"`,
     each list capped (≤6 names) and the whole card capped (≤512 chars). Deterministic.
  5. Best-effort contract: any HTTP/parse error is a `ToolError` the CALLER logs and skips — this
     client never panics and never blocks a build (the wiring in KGEMB-03 owns the non-blocking).

  ## TEST PLAN
  - `cargo test -p terminus-rs --lib scribe::graph::vec_embed` — passes.
  - `httpmock` tests (mirror `src/review/dispatch.rs:165`): an Ollama-shaped mock returns
    `{"embedding":[…]}` → parsed; an OpenAI-shaped mock at `/v1/embeddings` returns
    `{"data":[{"embedding":[…]}]}` → parsed; a 500 → `ToolError`, not a panic.
  - Pure tests: `node_card` output is deterministic, respects the name/length caps, and includes
    kind/name/path.
  - Shape auto-detection test: URL with `/v1/embeddings` selects OpenAI body; else Ollama body.

  ## EDGE CASES
  - Empty text → still returns a vector (provider decides) or a clear error; never a panic.
  - API key absent → no `Authorization` header (Ollama needs none).
  - Provider returns wrong-dim vector → returned as-is; KGEMB-03/01 handle dim mismatch at store
    time (the store column enforces 768).
  - Very long card → truncated by the cap before sending.

- **Acceptance criteria:**
  - [ ] `EmbedClient` embeds text against Ollama-shaped and OpenAI-shaped endpoints, auto-detected
        from the URL, with optional bearer auth from `vault::manager().get("EMBEDDINGS_API_KEY")`.
  - [ ] The API key (if any) is read via SecretManager/vault, never `std::env::var`.
  - [ ] `node_card` is deterministic and bounded (name/list caps + overall length cap).
  - [ ] HTTP/parse failures return `ToolError`, never panic.
  - [ ] README documents the client + `EMBEDDINGS_URL`/`EMBEDDINGS_MODEL`.
  - [ ] No hardcoded infrastructure values; secrets via vault, not env.
  - [ ] All existing tests still pass.

### KGEMB-03: Embed KG nodes during scribe_kg_build (gated, non-blocking)
- **Priority:** High
- **Labels:** terminus, knowledge-graph, embeddings
- **Agent:** claude
- **Estimate:** 2h
- **Description:** Wire embedding generation into the graph build so the vector store stays current
  with the graph. Gated behind `SCRIBE_KG_EMBED` (default off) AND a configured store + client;
  full build embeds all nodes, incremental re-embeds only changed-file nodes and deletes removed
  ones; nodes whose card hash is unchanged are skipped. Strictly best-effort: any failure logs and
  the build still succeeds.

  ## FILES
  - `src/scribe/graph/build.rs` — after `pagerank` (~line 242), before `store.save` (~line 249),
    add a gated `embed_nodes(...)` step; thread the incremental changed-node set.
  - `src/scribe/mod.rs` — `ScribeConfig` gains an `embed_enabled` flag from `SCRIBE_KG_EMBED`
    (mirror the `SCRIBE_KG_SEMANTIC` env read).

  ## APPROACH
  1. Gate: only run when `cfg.embed_enabled` AND `AtlasVecStore::from_env()` and
     `EmbedClient::from_env()` both succeed. Any `NotConfigured`/error → log once, skip (build
     continues, result notes `embed: {ran:false, reason}`).
  2. Determine the node set: full build → all current nodes; incremental → nodes in the changed
     files (from the `refresh_files` result / changed_files) plus delete embeddings for node_ids no
     longer present. Use `existing_hashes` to skip nodes whose `node_card` hash is unchanged.
  3. For each node build its card (`node_card` with neighbor names computed from the graph edges),
     `embed_batch`, then `store.upsert`. Wrap the whole step so an error becomes a logged
     `embed:{ran:true, ok:false, error}` in the result — NEVER propagated (mirror KGREV-02's
     `maybe_rebuild` non-blocking contract).
  4. Report `embed` stats (`ran`, `embedded`, `skipped`, `deleted`, `ok`) in `scribe_kg_build`'s
     structured result.

  ## TEST PLAN
  - `cargo test -p terminus-rs --lib scribe::graph::build` — existing build tests pass; new test:
    with `SCRIBE_KG_EMBED` unset the build behaves exactly as today (no embed attempt, result
    `embed.ran=false`).
  - A `#[serial]` test with `SCRIBE_KG_EMBED=1` but `ATLAS_DATABASE_URL` unset → build still
    succeeds, `embed.ran=false` reason "store not configured".
  - Pure test: changed-node selection + hash-skip logic (given a graph + changed files + existing
    hashes, the right add/skip/delete sets are produced) tested without a DB.

  ## EDGE CASES
  - Store configured but embeddings endpoint down → `embed.ok=false`, build still succeeds.
  - Incremental build that deletes a file → its nodes' embeddings are deleted from the store.
  - A node whose card is unchanged across builds → skipped (no re-embed, no upsert).
  - `SCRIBE_KG_EMBED` on but nothing changed → `embedded:0, skipped:N`.

- **Acceptance criteria:**
  - [ ] Embedding runs only when `SCRIBE_KG_EMBED` is set AND store+client are configured; otherwise
        the build is byte-for-byte unchanged from today.
  - [ ] Full build embeds all nodes; incremental embeds only changed-file nodes and deletes removed
        ones; unchanged card-hash nodes are skipped.
  - [ ] Any embedding/store failure is logged and reported in the result but NEVER fails the build.
  - [ ] `scribe_kg_build` result includes `embed` stats.
  - [ ] No hardcoded infrastructure values; all existing tests pass.

### KGEMB-04: kg_semantic_search tool + integration/edge tests + README
- **Priority:** High
- **Labels:** terminus, knowledge-graph, embeddings
- **Agent:** claude
- **Estimate:** 2h
- **Description:** A new `kg_semantic_search(project_id, query, limit)` tool: embed the query, get
  the top-K nearest node_ids from pgvector, join them against the loaded graph, and return
  `{id,name,kind,path,score,cluster}` ordered by similarity. Degrades cleanly (clear
  `found:false`/`configured:false`) when the store or client is unconfigured, so callers can fall
  back to lexical `kg_search`. Ships the Phase-1 edge-case test slice.

  ## FILES
  - `src/scribe/graph/tools.rs` — NEW `KgSemanticSearch` tool + register line.
  - `README.md` — document `kg_semantic_search` alongside the other `kg_*` tools.

  ## APPROACH
  1. `KgSemanticSearch` mirrors `KgSearch`'s `RustTool` shape. Params: `project_id`, `query`,
     optional `limit` (default 10, cap 50).
  2. `EmbedClient::from_env()` + `AtlasVecStore::from_env()`; if either is `NotConfigured`, return
     `{configured:false, found:false, results:[]}` (NOT an error) so the pipeline can fall back.
  3. Embed the query, `store.query_topk(project_id, &q, limit)`, load the graph (`GraphStore`),
     map each `node_id` → node fields, drop node_ids absent from the current graph (stale rows),
     return results with `score`.
  4. Tests + README.

  ## TEST PLAN
  - `cargo test -p terminus-rs --lib scribe::graph::tools` — passes.
  - `#[serial]` test: store/client unset → tool returns `{configured:false, found:false}`, no error.
  - Pure test: node_id→result mapping drops ids not in the graph (stale-row tolerance).
  - Edge tests (the Phase-1 slice): empty query handling; `limit` clamped to 50; a node_id in the
    vector store but missing from the graph is silently dropped; duplicate scores keep a stable
    order.

  ## EDGE CASES
  - Store has rows for a project whose graph was deleted → results filtered to current graph nodes.
  - Query embeds but store empty → `found:true, results:[]`? Prefer `found:true, count:0`.
  - `limit` 0 or >50 → clamped to [1,50].
  - Embeddings endpoint down at query time → `{configured:true, found:false, error}` (degrade, no
    panic).

- **Acceptance criteria:**
  - [ ] `kg_semantic_search` returns graph-joined results ordered by cosine similarity when
        configured; returns `configured:false` (not an error) when store/client unset.
  - [ ] Stale vector rows (node absent from the current graph) are dropped from results.
  - [ ] `limit` is clamped to [1,50]; a down endpoint degrades without panic.
  - [ ] README documents the tool.
  - [ ] No hardcoded infrastructure values; all existing tests pass.
