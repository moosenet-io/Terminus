# KG review-findings capture (capture-only, Phase 2)
plane_project: TERM
module: Terminus
prefix: KGFIND
spec_id: S113-kg-findings-capture

## Metadata
- **Author:** Moose
- **Session:** S113
- **Date:** 2026-07-12
- **Module version:** Terminus main
- **Estimated total:** ~9h autonomous agent work
- **Context:** Phase 2 of KG-as-behavioral-correction (Calx-on-KG). The review system already
  spots issues; this spec makes those issues DURABLE and LOCATIONAL by capturing structured
  findings from `review_run`, anchoring each to the KG scope it concerns (node / file / community
  / global), semantically de-duplicating near-identical findings via the Phase-1 embeddings, and
  counting recurrence. This is CAPTURE ONLY — pure observation. No rules are minted, nothing is
  promoted, nothing is enforced or blocked. Phase 3 (a separate spec) adds rule crystallization +
  adversarial promotion + Cortex. The point of shipping capture first: earn a corpus of real
  recurrence signal before any rule ever gates work. Everything here is best-effort and
  non-blocking — a findings failure never affects a review's verdict or a build.

## Pre-flight
- Repo `moosenet/Terminus` on `main`, clean, tests green. Reviewers live (codex+agy).
- Phase-1 landed: `AtlasVecStore`/`EmbedClient`/`node_card` (`src/scribe/graph/vec_store.rs`,
  `vec_embed.rs`), `ATLAS_DATABASE_URL` + `EMBEDDINGS_*`. Findings reuse that DB + embeddings.
- Review post-aggregate hook seam: `src/review/mod.rs` after `maybe_scribe_docs`; `ProviderResult`
  in `src/review/aggregate.rs`; prompt in `src/review/prompt.rs`; tolerant-JSON parse pattern in
  `src/scribe/graph/semantic.rs`.

### KGFIND-01: Findings store (kg_findings on the atlas DB, semantic dedup)
- **Priority:** High
- **Labels:** terminus, knowledge-graph, findings
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A store for review findings on the same Postgres/pgvector DB as the embeddings,
  with SEMANTIC de-duplication: a new finding whose description embeds within a cosine threshold of
  an existing finding in the SAME (project, scope, category) is treated as a recurrence (bump
  occurrences + last_seen) rather than a new row. Anchored to a KG scope.

  ## FILES
  - `src/scribe/graph/findings_store.rs` — NEW: `FindingsStore` (reuses the atlas DSN/pool) + model.
  - `src/scribe/graph/mod.rs` — `pub mod findings_store;`.
  - `README.md` — document the findings store + `kg_findings` table.

  ## APPROACH
  1. Reuse `crate::config::atlas_database_url()` and the bounded-pool + advisory-locked idempotent
     migration idiom from `vec_store.rs`. `FindingsStore::from_env()` → `NotConfigured` when unset.
  2. Migration: `CREATE TABLE IF NOT EXISTS kg_findings (id uuid PRIMARY KEY, project_id text,
     category text, severity text, scope_kind text CHECK (scope_kind IN ('node','path','community',
     'global')), scope_ref text, description text, embedding vector(768), provenance jsonb,
     first_seen timestamptz DEFAULT now(), last_seen timestamptz DEFAULT now(), occurrences int
     DEFAULT 1)` + an index on `(project_id, scope_kind, scope_ref, category)` and an HNSW index on
     `embedding` (best-effort).
  3. `record(finding: NewFinding, embedding: Option<Vec<f32>>) -> RecordOutcome`: within the
     matching `(project_id, scope, category)` bucket, if an embedding is provided, find the nearest
     existing finding by cosine; if similarity ≥ `KGFIND_DEDUP_THRESHOLD` (default 0.92, env-tunable)
     → UPDATE occurrences+1, last_seen=now(), merge provenance → outcome `Recurred{id, occurrences}`.
     Else INSERT a new row → `Created{id}`. If no embedding, dedup on exact (scope,category,
     description) match instead. All parameterized.
  4. `list(project_id, filter: {scope?, category?, min_occurrences?}) -> Vec<FindingRow>` ordered by
     occurrences desc, last_seen desc.
  5. No secret literals; DSN is plain env like the store.

  ## TEST PLAN
  - `#[serial]` NotConfigured test (unset DSN → `NotConfigured`, early-return if a real DSN present).
  - Pure tests: the dedup DECISION (given a candidate embedding + a set of existing (embedding,
    id) in-bucket + threshold → Created vs Recurred(id)) factored into a pure function, tested
    without a DB. Threshold boundary (just-below vs just-above). Exact-match dedup when no embedding.
  - SQL consts contain `vector(768)`, `kg_findings`, `occurrences`.
  - `cargo test --workspace` green.

  ## EDGE CASES
  - No embedding available (embeddings endpoint down) → exact-text dedup, still records.
  - Two findings, same text, different scope → two rows (scope is part of the bucket).
  - provenance merge keeps a bounded list (cap stored provenance entries, e.g. last 20).
  - Threshold exactly at boundary → deterministic (define ≥ as recurrence).

- **Acceptance criteria:**
  - [ ] `FindingsStore::from_env` returns `NotConfigured` when `ATLAS_DATABASE_URL` unset; else
        bounded pool + idempotent advisory-locked migration creating `kg_findings` (vector(768)).
  - [ ] `record` semantically dedups within (project, scope, category): ≥ threshold ⇒ recurrence
        (occurrences++/last_seen), else a new row; exact-text dedup when no embedding.
  - [ ] `list` filters by scope/category/min_occurrences, ordered by recurrence.
  - [ ] Provenance is merged + bounded; all queries parameterized.
  - [ ] README documents the store. No hardcoded infra. All existing tests pass.

### KGFIND-02: review_run emits structured findings
- **Priority:** High
- **Labels:** terminus, review, findings
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Extend the review prompt to ask each provider for an OPTIONAL machine-readable
  findings block alongside its verdict, and parse it tolerantly into structured findings on each
  `ProviderResult`. Purely additive — the verdict parsing and every existing behavior are unchanged;
  a provider that emits no findings block yields an empty findings list.

  ## FILES
  - `src/review/prompt.rs` — append a `FINDINGS_JSON:` block request to each role's prompt; add
    `parse_findings(&str) -> Vec<Finding>` (tolerant, mirrors `semantic::extract_json_array`).
  - `src/review/aggregate.rs` — `ProviderResult` gains `findings: Vec<Finding>`; define `Finding`
    (`category`, `severity`, `file`, `symbol`, `description`).
  - `src/review/mod.rs` — `run_one_provider` also calls `parse_findings` and attaches them; the
    result JSON surfaces each provider's findings.

  ## APPROACH
  1. `Finding { category: String, severity: String, file: Option<String>, symbol: Option<String>,
     description: String }` (serde). Keep it small + provider-friendly.
  2. `build_prompt`: after the `VERDICT:` sentinel instruction, add: "Then, on a new line, emit
     `FINDINGS_JSON:` followed by a JSON array of any concrete issues you found, each
     `{category, severity, file, symbol, description}` (empty array if none). This is optional
     structured output; your VERDICT line above is still authoritative." Keep `build_prompt`'s
     signature stable.
  3. `parse_findings`: locate the `FINDINGS_JSON:` marker, extract the JSON array with the same
     tolerant approach as `semantic::extract_json_array` (brace/bracket matching), `serde_json`
     parse; malformed/absent → empty Vec. NEVER errors.
  4. `run_one_provider`: on a successful provider reply, `parse_findings(&text)` and set
     `findings` on the `ProviderResult`. Degraded providers → empty findings.
  5. Surface findings per provider in the tool result JSON (additive field).

  ## TEST PLAN
  - Pure tests for `parse_findings`: a well-formed block → parsed findings; absent block → empty;
    malformed JSON → empty (no panic); a `FINDINGS_JSON: []` → empty; extra prose around it → still
    extracted.
  - `build_prompt` includes the findings instruction (and still includes the VERDICT sentinel).
  - Existing review tests still pass (verdict parsing unchanged; the degrade test still holds).
  - `cargo test --workspace` green.

  ## EDGE CASES
  - Provider emits findings but a bad verdict → verdict handling unchanged; findings still parsed.
  - `FINDINGS_JSON:` present twice → take the first well-formed array.
  - Huge findings array → cap the number parsed (e.g. ≤ 50) to bound memory.
  - Non-string fields / missing optional fields → tolerated by serde defaults.

- **Acceptance criteria:**
  - [ ] Each `ProviderResult` carries a `findings: Vec<Finding>` parsed from an optional
        `FINDINGS_JSON:` block; absent/malformed ⇒ empty, never an error or panic.
  - [ ] Verdict parsing and all existing review behavior are byte-for-byte unchanged.
  - [ ] The prompt requests the findings block without weakening the VERDICT sentinel.
  - [ ] Findings count is capped; findings surfaced in the result JSON.
  - [ ] No hardcoded infra. All existing tests pass.

### KGFIND-03: Record findings on the KG (post-aggregate hook, capture-only)
- **Priority:** High
- **Labels:** terminus, review, findings, knowledge-graph
- **Agent:** claude
- **Estimate:** 2h
- **Description:** After aggregation, `review_run` records the providers' structured findings into
  the `FindingsStore`, anchored to the KG scope each finding concerns (resolve `symbol`→node,
  else `file`→path, else community/global), with provenance (the PR/review/commit from context),
  semantically deduped via `EmbedClient`. Fires on ANY verdict (findings matter most on
  CHANGES_REQUESTED). CAPTURE ONLY — non-blocking, no rules, never affects the verdict.

  ## FILES
  - `src/review/mod.rs` — `maybe_record_findings(...)` after `maybe_scribe_docs`; add a
    `findings_recorded` field to the result.

  ## APPROACH
  1. Gate: only when `context.project_id` is present AND `FindingsStore::from_env()` succeeds AND
     there are findings across providers. Else `{recorded:false, reason}`. (Independent of verdict.)
  2. For each finding: resolve scope — if `symbol` matches a current graph node id/name →
     `(node, id)`; else if `file` present → `(path, file)`; else `(global, project_id)`. (Community
     anchoring optional — start with node/path/global.)
  3. Embed the finding `description` via `EmbedClient` (best-effort; on failure, record with no
     embedding → exact-text dedup). `store.record(...)` with provenance built from
     `context` (`pr`, `review`/`spec_id`, `git_ref`).
  4. Aggregate outcomes into `findings_recorded: {recorded:true, created:N, recurred:M}` (or
     `{recorded:false, reason}`); `tracing::warn!` + swallow any error. NEVER propagate — the
     review already produced its verdict.
  5. Dedup findings across providers first (same file+symbol+category+similar text) so 2 reviewers
     reporting the same issue count as one occurrence, not two.

  ## TEST PLAN
  - Pure test: cross-provider dedup + scope-resolution decision (given providers' findings + a graph
    → the resolved (scope, ref) per finding and the collapsed set) factored out and tested w/o DB.
  - `#[serial]`: `project_id` present but store unset → `{recorded:false}`, review verdict unchanged.
  - `#[serial]`: no `project_id` → hook is a no-op, review unchanged.
  - A store/embedding failure → `recorded:false`/error surfaced, verdict intact, no panic.
  - `cargo test --workspace` green.

  ## EDGE CASES
  - No findings at all → `{recorded:false, reason:"no findings"}`.
  - APPROVE with zero findings → nothing recorded (clean pass leaves no noise).
  - Finding referencing a symbol not in the graph → falls back to file/global scope.
  - Embeddings endpoint down → record with exact-text dedup, still captured.

- **Acceptance criteria:**
  - [ ] On any verdict with `project_id` + findings, `review_run` records deduped findings anchored
        to node/path/global with provenance, reported in `findings_recorded`.
  - [ ] Cross-provider duplicate findings collapse to one occurrence.
  - [ ] Fully non-blocking: a findings failure never changes the verdict or errors the call.
  - [ ] No `project_id` ⇒ no-op; clean APPROVE with no findings records nothing.
  - [ ] No hardcoded infra. All existing tests pass.

### KGFIND-04: kg_findings query tool + edge tests + README
- **Priority:** Medium
- **Labels:** terminus, knowledge-graph, findings
- **Agent:** claude
- **Estimate:** 1h
- **Description:** A read-only `kg_findings(project_id, scope?, category?, min_occurrences?)` tool
  that lists captured findings ordered by recurrence — so the corpus is inspectable (and, in Phase
  3, consumable at scope time). Degrades to `configured:false` when the store is unset.

  ## FILES
  - `src/scribe/graph/tools.rs` — NEW `KgFindings` tool + register line.
  - `README.md` — document `kg_findings`.

  ## APPROACH
  1. Mirror the `KgSemanticSearch` degrade shape: `FindingsStore::from_env()` NotConfigured →
     `{configured:false, found:false, results:[]}`.
  2. `store.list(project_id, filter)`, return rows `{id, category, severity, scope_kind, scope_ref,
     description, occurrences, first_seen, last_seen}` ordered by recurrence. Clamp any limit.
  3. Register; README.

  ## TEST PLAN
  - `#[serial]` unset store → `{configured:false}`, no error.
  - Pure test: filter/limit clamping.
  - `cargo test --workspace` green.

  ## EDGE CASES
  - Store configured but empty → `{configured:true, found:true, count:0}`.
  - Unknown project → empty result, not an error.

- **Acceptance criteria:**
  - [ ] `kg_findings` lists captured findings by recurrence with scope/category/min_occurrences
        filters; degrades to `configured:false` (not an error) when the store is unset.
  - [ ] README documents the tool. No hardcoded infra. All existing tests pass.
