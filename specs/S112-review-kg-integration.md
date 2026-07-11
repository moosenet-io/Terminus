# review_run ↔ Atlas KG + SCRIBE integration
plane_project: TERM
module: Terminus
prefix: KGREV
spec_id: S112-review-kg-integration

## Metadata
- **Author:** Moose
- **Session:** S112
- **Date:** 2026-07-11
- **Module version:** Terminus main
- **Estimated total:** ~8h autonomous agent work
- **Context:** The Atlas knowledge graph is now a central build-pipeline feature: graphs
  are self-contained on the terminus host and queryable fleet-wide via `kg_*`. This spec
  wires the KG (and the SCRIBE doc engine) into the `review_run` tool so that (1) every
  review is grounded in the graph's blast-radius context for the changed symbols, (2) a
  successful review pass rebuilds that project's graph incrementally and holds a lock that
  blocks another incremental review of the same project until the rebuild is ready to be
  referenced by the next review, and (3) the review tool drives documentation through the
  SCRIBE / docgen door. This closes the loop: reviews consume the graph and keep it current.

## Pre-flight
- Repository: `moosenet/Terminus` on Gitea, on `main`, clean, tests green.
- KG engine present: `src/scribe/graph/` (KGRAPH-01..18), `review_run` present: `src/review/`.
- Vault secrets: none new. Env used at runtime: `SCRIBE_KG_STORE_DIR`, `SCRIBE_ALLOWED_REPO_ROOTS`.
- Baseline: `cargo test --workspace` passing; review/KG in-file tests green.

### KGREV-01: Ground each review in the Atlas knowledge graph
- **Priority:** High
- **Labels:** terminus, review, knowledge-graph
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Before dispatching a review to its providers, `review_run` consults the
  project's Atlas graph for the blast radius of the changed files/symbols and injects a
  compact, bounded knowledge-graph context block into the review `context`, so every
  provider reviews the change with the graph's structural context (what the changed symbols
  call, what calls them, which subsystem/community they live in). Best-effort and backward
  compatible: with no `project_id` (or no graph / repo not allowed), behavior is byte-for-byte
  unchanged.

  ## FILES
  - `src/review/mod.rs` — in `execute()`, after `parse_input` and before the provider loop,
    call a new `kg_context::inject(&mut context)` helper; thread `project_id`/`changed_files`
    out of `context`.
  - `src/review/kg_context.rs` — NEW module: `derive_changed_files(&Value) -> Vec<String>`
    (from `context.changed_files` or parsed from `context.diff` `+++ b/<path>` lines) and
    `build_kg_block(project_id, &[changed_files]) -> Option<Value>` (loads the graph via
    `GraphStore::from_config`, gathers touched nodes + 1-hop neighbors + communities, bounded).
  - `src/review/prompt.rs` — add an explicit, labelled "Knowledge graph (structural context)"
    section to the reviewer/defend/attack prompts when `context.knowledge_graph` is present.
  - `README.md` — document that `review_run` is KG-grounded and the optional context keys
    (`project_id`, `changed_files`).

  ## APPROACH
  1. Add `src/review/kg_context.rs`. `derive_changed_files`: prefer `context["changed_files"]`
     (array of repo-relative strings); else parse unified-diff `+++ b/<path>` headers from
     `context["diff"]`. Cap at a sane maximum (e.g. 200 files).
  2. `build_kg_block(project_id, changed_files)`: `GraphStore::from_config()` then
     `.load(project_id)`. If the store/graph is absent, return `None` (no-op). For each changed
     file, collect nodes whose `path` matches; for each such node collect name, kind, 1-hop
     in/out neighbors (callers/callees) by id, and community/cluster id. BOUND the total (≤ ~40
     symbols, ≤ ~2 KB serialized) — truncate with a `"truncated": true` marker rather than
     emitting a huge block.
  3. In `execute()`: read `project_id` from `context`; if present, `context["knowledge_graph"]
     = build_kg_block(...)` (only when `Some`). Everything downstream (`build_prompt`) then
     serializes it into each provider prompt. No `project_id` → skip entirely.
  4. `prompt.rs`: if `context` contains a `knowledge_graph` key, the existing context
     serialization already surfaces it; ADDITIONALLY prepend a one-line pointer in the role
     framing ("A `knowledge_graph` section below gives the structural blast radius of the
     changed symbols — use it to judge cross-module impact."). Keep `build_prompt`'s signature
     stable (still `(role, criteria, context)`).
  5. All config via `ScribeConfig`/`GraphStore::from_config` (already env-driven); no new env,
     no hardcoded paths, no secrets.

  ## TEST PLAN
  - `cargo test --workspace` — all existing review tests still pass unchanged.
  - New `#[serial]` test (sets `SCRIBE_KG_STORE_DIR` to a temp dir with a tiny seeded graph):
    a `context` with `project_id` + `changed_files` yields a `knowledge_graph` block naming
    the touched symbol and a neighbor.
  - Negative: `context` with no `project_id` → no `knowledge_graph` key, prompt identical to
    pre-change (guard against accidental behavior change for the common path).
  - Negative: `project_id` set but graph missing → `None`, no key, no error.
  - Verify no hardcoded IPs/org names in new files; no `std::env::var` for secrets.

  ## EDGE CASES
  - `context.diff` present but malformed / no `+++` headers → empty changed-files, no block.
  - Graph loads but no node matches any changed file → `None` (don't inject an empty block).
  - Very large diff (hundreds of files) → cap files scanned and symbols emitted; mark truncated.
  - `project_id` referring to a repo not under `SCRIBE_ALLOWED_REPO_ROOTS` → load is read-only
    from the store dir, so allowed-roots does not gate reads; still return `None` if not found.

- **Acceptance criteria:**
  - [ ] With `context.project_id` + a present graph, `review_run` injects a bounded
        `knowledge_graph` block naming touched symbols + neighbors into the review context.
  - [ ] With no `project_id`, the review prompt and result are unchanged (backward compatible).
  - [ ] A missing/absent graph is a silent no-op, never an error and never fails the review.
  - [ ] The injected block is bounded (≤ ~40 symbols / ~2 KB) and marks truncation.
  - [ ] README documents `review_run`'s KG grounding and the optional context keys.
  - [ ] No hardcoded infrastructure values in new/modified code.
  - [ ] All existing tests still pass.

### KGREV-02: Rebuild-on-pass hook + per-project re-review lock
- **Priority:** High
- **Labels:** terminus, review, knowledge-graph
- **Agent:** claude
- **Estimate:** 3h
- **Description:** On a successful review pass (aggregate `APPROVE` + `complete`),
  `review_run` incrementally rebuilds the project's Atlas graph for the changed files, so the
  graph the next review references reflects the just-approved change. While that rebuild is in
  flight, a per-project lock BLOCKS another incremental review of the same project from
  running — the next review must reference a ready, rebuilt graph, never a stale or
  mid-rebuild one. The rebuild is non-blocking to the review result itself: a rebuild failure
  is reported in the result, never turns an APPROVE into a failure.

  ## FILES
  - `src/review/mod.rs` — module-global `IN_FLIGHT: OnceLock<Mutex<HashSet<String>>>` keyed by
    `project_id`; lock check at the top of `execute()`; post-aggregate rebuild hook; RAII guard
    to always clear the lock. New `kg_rebuild` field in the returned JSON.
  - `README.md` — document the rebuild-on-pass hook and the re-review lock semantics.

  ## APPROACH
  1. Add `static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>>` (pattern from
     `src/sysversion/mod.rs:45`). Helper `in_flight()` returns the initialized `&Mutex`.
  2. At the TOP of `execute()` (after `parse_input`): if `context.project_id` is present AND
     the set contains it, SHORT-CIRCUIT — return
     `{ "structure":…, "providers":[], "aggregate_verdict":"UNKNOWN", "complete":false,
        "locked":true, "reason":"KG rebuild in progress for <project>; retry when ready" }`
     WITHOUT running the review (a re-review must not run against a mid-rebuild graph).
  3. After `aggregate(...)`: if `aggregate_verdict == "APPROVE" && complete` AND
     `context.project_id` + `context.repo_path` are present: insert `project_id` into the set
     (this is what other reviews now see as "locked"), then call
     `ScribeKgBuild.execute_structured(json!({ "project_id":…, "repo_path":…,
     "incremental": true, "changed_files": [...] }))` and await it. Use a drop-guard so the
     `project_id` is removed from the set on EVERY path (success, error, panic-unwind).
  4. Record the rebuild outcome in a `kg_rebuild` field of the result
     (`{"ran":true,"ok":…,"nodes":…}` or `{"ran":false,"reason":…}`); a rebuild error is
     logged (`tracing::warn!`) and surfaced there, NEVER converted into a tool error.
  5. `changed_files` reuse the KGREV-01 derivation (share the helper).

  ## TEST PLAN
  - `cargo test --workspace` — existing tests unchanged.
  - New `#[serial]` test: pre-insert a `project_id` into `IN_FLIGHT`, call `execute()` with
    that `project_id` → result has `locked:true`, `providers` empty, no provider dispatch.
  - New `#[serial]` test: an APPROVE path (providers degrade-approve or a stubbed aggregate)
    with `project_id` + temp `repo_path` → `kg_rebuild.ran == true` and `IN_FLIGHT` is empty
    afterward (lock released).
  - Negative: a rebuild error (bogus repo_path) → review result still returns Ok with
    `kg_rebuild.ok=false`, aggregate verdict unchanged, `IN_FLIGHT` cleared.
  - No `project_id` → no lock, no rebuild, identical to today.

  ## EDGE CASES
  - APPROVE but no `repo_path` in context → rebuild skipped (`kg_rebuild.ran=false`,
    reason "no repo_path"), no lock held.
  - Concurrent reviews of DIFFERENT projects → independent, never block each other.
  - Rebuild panics/unwinds → the drop-guard still clears the lock (no permanent deadlock).
  - Lock is best-effort liveness, not correctness-critical: a poisoned mutex recovers via
    `into_inner`/`lock().unwrap_or_else(|e| e.into_inner())`.

- **Acceptance criteria:**
  - [ ] A successful pass (`APPROVE` + `complete`) with `project_id` + `repo_path` triggers an
        incremental `scribe_kg_build` for the changed files.
  - [ ] While that rebuild is in flight, another incremental review of the SAME `project_id`
        short-circuits with `locked:true` and dispatches no providers.
  - [ ] The lock is released on every path (success, rebuild error, unwind) — no deadlock.
  - [ ] A rebuild failure is reported in `kg_rebuild` and never turns an APPROVE into a failure
        or a tool error.
  - [ ] Reviews of different projects never block each other.
  - [ ] README documents the hook + lock; no hardcoded infra values; all existing tests pass.

### KGREV-03: Drive documentation through the SCRIBE door on pass
- **Priority:** Medium
- **Labels:** terminus, review, docs, scribe
- **Agent:** claude
- **Estimate:** 2h
- **Description:** When a review passes and the context supplies documentation parameters, the
  review tool refreshes documentation through the sanctioned SCRIBE / docgen door
  (`docgen_run`) rather than any ad-hoc path — the single-door principle (S9) applied to doc
  generation, and the doc build is KG-grounded because it runs after KGREV-02's graph refresh.
  Non-blocking: a doc-gen failure never fails the review.

  ## FILES
  - `src/review/mod.rs` — after the KGREV-02 rebuild, if `context` carries doc params
    (`project`, `spec_id`, `git_ref`, `module_path`), call `DocgenRun.execute_structured(...)`;
    record a `scribe_docs` field in the result.
  - `README.md` — document that a passing review drives doc refresh through `docgen_run`.

  ## APPROACH
  1. After the rebuild hook (still inside the `APPROVE && complete` branch), if
     `context.project` and `context.spec_id` are present, build the `docgen_run` args
     (`spec_id`, `feat_context` = the diff, `project`, `module_path`, `git_ref`,
     `project_config` if provided) and call `DocgenRun::default().execute_structured(args)`.
  2. `docgen_run` is already structurally non-blocking (returns `outcome: skipped|completed|
     failed`); surface its outcome in a `scribe_docs` field. Never propagate an error.
  3. If doc params are absent, skip cleanly (`scribe_docs.ran=false`) — most reviews won't
     supply them; this wire fires for real merge-time reviews that do.
  4. Sequence: KG rebuild FIRST (KGREV-02), then docs — so the doc engine sees the refreshed
     graph/state.

  ## TEST PLAN
  - `cargo test --workspace` — existing tests unchanged.
  - New test: APPROVE with `project` + `spec_id` but docgen unconfigured (no Chord) →
    `scribe_docs.ran=true`, outcome `failed`/`skipped`, review result still Ok, verdict intact.
  - Negative: APPROVE without doc params → `scribe_docs.ran=false`, no docgen call.
  - Verify no direct doc-generation path is added that bypasses `docgen_run`/scribe (S9).

  ## EDGE CASES
  - docgen unconfigured / Chord unreachable → `outcome: failed`, review unaffected.
  - Doc params present but review did NOT pass → no doc build (only on APPROVE).
  - Large diff as `feat_context` → passed through to docgen which runs its own PII sweep first.

- **Acceptance criteria:**
  - [ ] A passing review with doc params refreshes docs via `docgen_run` (the SCRIBE/docgen
        door), sequenced AFTER the KG rebuild.
  - [ ] Doc refresh is non-blocking: a docgen failure never fails the review or changes the
        verdict.
  - [ ] No ad-hoc/duplicate doc-generation path is introduced (S9 single door).
  - [ ] README documents the behavior; no hardcoded infra values; all existing tests pass.
