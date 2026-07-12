# KG rule promotion + Cortex bridge (Phase 3, final)
plane_project: TERM
module: Terminus
prefix: KGRULE
spec_id: S113-kg-rule-promotion

## Metadata
- **Author:** Moose
- **Session:** S113
- **Date:** 2026-07-12
- **Module version:** Terminus main
- **Estimated total:** ~12h autonomous agent work
- **Context:** Phase 3 (final) of KG-as-behavioral-correction (Calx-on-KG). Phases 1-2 built
  semantic embeddings and captured recurring review findings on the KG. This phase closes the
  loop: recurring findings CRYSTALLIZE into candidate rules, a rule is only PROMOTED after an
  ADVERSARIAL review panel (via the `review_run` tool) argues it is real/earned, Cortex risk
  scores prioritize which findings become rules, and active rules are CONSUMED early — surfaced
  to agents at scope-time and injected into `review_run`'s own reviews so the same rules that
  are generated are enforced. The governance discipline: advisory rules can auto-form from
  signal, but any promotion to a BLOCKING (work-gating) rule is operator-gated; nothing here
  ever blocks a build or a review on its own. Rules are earned, measured, and decay if they
  stop recurring.

## Pre-flight
- Repo `moosenet/Terminus` on `main`, clean, tests green. Reviewers live (codex+agy).
- Phases 1-2 landed: `AtlasVecStore`/`EmbedClient` (embeddings), `FindingsStore`/`kg_findings`
  (`src/scribe/graph/findings_store.rs`), `review_run` findings capture. Rules reuse the atlas DB.
- `review_run` is the single sanctioned review door (S9/v3.17) — the adversarial promotion gate
  MUST go through it, never a hand-rolled reviewer.
- Cortex is remote SSH-passthrough (`src/cortex/mod.rs`; `cortex_scope`/`cortex_review`), config-
  gated by `CORTEX_SSH_HOST`/`_KEY_PATH`; the bridge MUST degrade cleanly when Cortex is unset.

### KGRULE-01: Rule store (kg_rules on the atlas DB)
- **Priority:** High
- **Labels:** terminus, knowledge-graph, rules
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A store for crystallized rules on the atlas DB. A rule has a scope (the KG
  location it governs), a category, human guidance, an enforcement level, a lifecycle status,
  bi-temporal validity, provenance (the findings + promotion review that earned it), and the
  recurrence + risk it was born from.

  ## FILES
  - `src/scribe/graph/rules_store.rs` — NEW: `RulesStore` (reuses atlas DSN/pool) + model.
  - `src/scribe/graph/mod.rs` — `pub mod rules_store;`.
  - `src/scribe/graph/README.md` — document `kg_rules`.

  ## APPROACH
  1. Reuse the `findings_store.rs` shape (atlas DSN, advisory-locked idempotent migration,
     parameterized queries, `NotConfigured` handling, distinct advisory-lock key).
  2. Migration: `CREATE TABLE IF NOT EXISTS kg_rules (id uuid PRIMARY KEY, project_id text,
     scope_kind text CHECK (scope_kind IN ('node','path','community','global')), scope_ref text,
     category text, guidance text, enforcement text NOT NULL DEFAULT 'advisory' CHECK (enforcement
     IN ('advisory','lint-candidate','blocking')), status text NOT NULL DEFAULT 'candidate' CHECK
     (status IN ('candidate','active','retired')), provenance jsonb NOT NULL DEFAULT '{}'::jsonb,
     recurrence_at_creation int, cortex_risk real, created_at timestamptz DEFAULT now(),
     valid_from timestamptz DEFAULT now(), valid_to timestamptz)` + index on
     `(project_id, scope_kind, scope_ref, category, status)`.
  3. Methods: `create_candidate(NewRule) -> Uuid` (idempotent per (project,scope,category): if an
     active/candidate rule already exists for that key, return it rather than duplicating);
     `promote(id, enforcement, promotion_provenance)` (candidate→active, sets enforcement);
     `retire(id, reason)` (status→retired, sets `valid_to=now()`); `list_active(project_id,
     scope_kind?, scope_ref?, category?) -> Vec<RuleRow>` (status='active' AND valid_to IS NULL).
  4. All parameterized; provenance is jsonb.

  ## TEST PLAN
  - `#[serial]` NotConfigured (unset DSN → NotConfigured, early-return if a real DSN present).
  - Pure tests: SQL consts contain `kg_rules`, the enforcement/status CHECK values; a pure
    `is_active(status, valid_to)` predicate.
  - `cargo test --workspace` green.

  ## EDGE CASES
  - create_candidate twice for the same (project,scope,category) → one row (idempotent).
  - promote a non-existent id → clear error, no partial state.
  - retire sets valid_to; list_active excludes retired + valid_to IS NOT NULL.

- **Acceptance criteria:**
  - [ ] `RulesStore::from_env` NotConfigured when DSN unset; else idempotent advisory-locked
        migration creating `kg_rules` with the enforcement/status CHECKs + bi-temporal columns.
  - [ ] create_candidate is idempotent per (project,scope,category); promote/retire transition
        status + set valid_to; list_active returns only active + non-expired rules.
  - [ ] All queries parameterized; provenance stored as jsonb. README documents the table.
  - [ ] No hardcoded infra. All existing tests pass.

### KGRULE-02: Crystallize candidate rules from recurring findings (+ Cortex risk)
- **Priority:** High
- **Labels:** terminus, knowledge-graph, rules, cortex
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A `kg_rule_crystallize(project_id)` tool that scans `kg_findings` for
  (scope, category) buckets whose recurrence meets a threshold and mints CANDIDATE rules
  (advisory, status=candidate) — never active, never blocking. It attaches the Cortex risk score
  for the scope when Cortex is configured (best-effort), so higher-risk recurring issues surface
  first. Crystallization is idempotent (KGRULE-01's create_candidate dedups).

  ## FILES
  - `src/scribe/graph/rules.rs` — NEW: crystallization logic + `KgRuleCrystallize` tool.
  - `src/scribe/graph/tools.rs` (or rules.rs) — register the tool.
  - `src/scribe/graph/README.md` — document `kg_rule_crystallize`.

  ## APPROACH
  1. `kg_rule_crystallize(project_id, min_occurrences?)`: `FindingsStore::list(project_id, …,
     min_occurrences = threshold)` → for each finding bucket ≥ threshold (default from
     `KGRULE_CRYSTALLIZE_MIN_OCCURRENCES`, e.g. 3) that has no existing active/candidate rule,
     build a candidate `NewRule` (scope + category from the finding; guidance = a concise
     imperative derived from the finding's description/category; provenance = the finding id(s) +
     occurrences).
  2. Cortex risk (best-effort): if Cortex is configured, call `cortex_review`/`cortex_scope` for
     the scope (a path or symbol) and parse a numeric risk; attach as `cortex_risk`. If Cortex is
     `NotConfigured` or errors → `cortex_risk = null`, continue. NEVER fail crystallization on
     Cortex. (See the Cortex bridge helper in KGRULE-05 — this item may land the helper or call a
     thin inline version guarded behind the same degrade contract.)
  3. `RulesStore::create_candidate` each (idempotent). Return `{created:N, skipped:M, candidates:[…]}`.
  4. Degrade: store unset → `{configured:false}`.

  ## TEST PLAN
  - Pure test: the crystallize DECISION (given finding rows + existing rules + threshold →
    which become new candidates) factored out and tested without a DB.
  - Pure test: guidance derivation from a finding is deterministic + non-empty.
  - `#[serial]`: store unset → `{configured:false}`.
  - Cortex-unconfigured path → `cortex_risk` null, still crystallizes (test the degrade of the
    risk helper).
  - `cargo test --workspace` green.

  ## EDGE CASES
  - Finding below threshold → no candidate.
  - Existing active rule for the bucket → skipped (idempotent).
  - Cortex down/unset → cortex_risk null, crystallization proceeds.

- **Acceptance criteria:**
  - [ ] `kg_rule_crystallize` mints candidate (advisory, status=candidate) rules for finding
        buckets ≥ threshold, idempotently; never mints active/blocking rules.
  - [ ] Cortex risk is attached best-effort and NEVER blocks crystallization when Cortex is
        unconfigured/unreachable.
  - [ ] Degrades to `configured:false` when the store is unset.
  - [ ] Pure crystallize/guidance decisions are unit-tested without a DB.
  - [ ] No hardcoded infra. All existing tests pass.

### KGRULE-03: Adversarial rule promotion via review_run
- **Priority:** High
- **Labels:** terminus, knowledge-graph, rules, review
- **Agent:** claude
- **Estimate:** 3h
- **Description:** A `kg_rule_promote(rule_id)` flow that runs an ADVERSARIAL `review_run` panel
  whose job is to argue whether a candidate rule is real, correct, and earned — and only promotes
  the rule (candidate→active, enforcement=advisory or lint-candidate) when the panel APPROVEs.
  Promotion to `blocking` is NEVER automatic — it requires an explicit operator flag. This is the
  single sanctioned review door (S9/v3.17) applied to rule governance.

  ## FILES
  - `src/scribe/graph/rules.rs` — `KgRulePromote` tool + the promotion flow.
  - `src/scribe/graph/README.md` — document `kg_rule_promote` + the operator-gate for blocking.

  ## APPROACH
  1. `kg_rule_promote(rule_id, target_enforcement? (default 'advisory'), allow_blocking? (default
     false))`. Load the candidate rule from `RulesStore`.
  2. Build a `review_run` call — `structure="adversarial_pair"` or `panel_unanimous` — with a
     criteria/context that frames the panel ADVERSARIALLY: "This is a proposed durable coding rule
     crystallized from N recurring review findings on {scope}. Argue whether it is a REAL,
     correct, non-spurious, generally-applicable rule that should govern future work — or whether
     it is noise / overfit / already covered by a lint. APPROVE only if it is genuinely earned."
     Pass the rule guidance, the provenance findings, the recurrence count, and the Cortex risk.
     Call `review_run` (the tool) — do NOT hand-roll a reviewer.
  3. On aggregate APPROVE + complete: `RulesStore::promote(rule_id, enforcement, provenance=the
     review result)`. `enforcement = 'blocking'` ONLY if `allow_blocking == true` (operator-gated);
     otherwise cap at `lint-candidate`/`advisory`. On CHANGES_REQUESTED / incomplete: leave the
     rule a candidate, record the panel verdict in provenance, return not-promoted.
  4. Return `{promoted:bool, enforcement, aggregate_verdict, ...}`. Never blocks anything.

  ## TEST PLAN
  - Pure test: the promotion DECISION given (aggregate_verdict, complete, allow_blocking,
    target_enforcement) → (promote?, final_enforcement) — e.g. APPROVE+complete+!allow_blocking+
    target=blocking ⇒ promote at lint-candidate (capped); CHANGES_REQUESTED ⇒ no promote.
  - `#[serial]`: store unset → clear degrade.
  - Confirm the flow constructs a `review_run` call (S9) and does not shell out to a raw reviewer.
  - `cargo test --workspace` green.

  ## EDGE CASES
  - Panel incomplete (a provider unavailable) → NOT promoted (fail-closed, like the pipeline gate).
  - allow_blocking=true but panel CHANGES_REQUESTED → not promoted.
  - Rule already active → no-op (idempotent), returns current state.

- **Acceptance criteria:**
  - [ ] `kg_rule_promote` runs an adversarial `review_run` panel and promotes candidate→active
        ONLY on APPROVE+complete; incomplete/CHANGES_REQUESTED leaves it a candidate.
  - [ ] Promotion to `blocking` is operator-gated (`allow_blocking`), never automatic; without it,
        enforcement is capped at lint-candidate/advisory.
  - [ ] The gate goes through the `review_run` tool (S9), never a hand-rolled reviewer.
  - [ ] Promotion decision is a pure, unit-tested function; the flow is non-blocking.
  - [ ] No hardcoded infra. All existing tests pass.

### KGRULE-04: Consume active rules — kg_rules tool + review injection + skill
- **Priority:** High
- **Labels:** terminus, knowledge-graph, rules, review
- **Agent:** claude
- **Estimate:** 2h
- **Description:** Make active rules CONSUMABLE early: a `kg_rules(project_id, scope?, category?)`
  read tool, and — closing the loop — inject the active rules for the changed files into
  `review_run`'s own review prompt (alongside the KGREV-01 knowledge-graph block) so reviewers
  enforce the rules the system has learned. Plus the skill note so agents ground in rules before
  scoping.

  ## FILES
  - `src/scribe/graph/tools.rs` — NEW `KgRules` read tool (degrade shape like `kg_findings`).
  - `src/review/kg_context.rs` — extend the review-injection to add an `active_rules` block for the
    changed files' scopes when `RulesStore` is configured (best-effort, bounded).
  - `src/scribe/graph/README.md` — document `kg_rules`.
  - (skill) update handled separately after merge.

  ## APPROACH
  1. `KgRules` mirrors `kg_findings`'s degrade contract; `RulesStore::list_active(...)`, return
     `{id, scope_kind, scope_ref, category, guidance, enforcement, cortex_risk}` ordered by
     enforcement (blocking > lint-candidate > advisory) then recency.
  2. In `kg_context::inject` (KGREV-01), after the `knowledge_graph` block, if `RulesStore` is
     configured, look up active rules for the changed files (path scope) + touched symbols (node
     scope) + global, and add a bounded `active_rules` array to the context (≤ ~20 rules, ≤ ~2 KB)
     so every provider sees the learned rules for the code under review. Best-effort — a rules
     lookup failure leaves the review unchanged (never errors).
  3. README.

  ## TEST PLAN
  - `#[serial]`: `kg_rules` store unset → `{configured:false}`.
  - Pure test: the rules-injection selection/bounding (given active rules + changed files → the
    bounded set added to context) factored out and tested without a DB.
  - Existing review/kg_context tests still pass (no rules configured ⇒ context unchanged).
  - `cargo test --workspace` green.

  ## EDGE CASES
  - No rules store configured → review context byte-for-byte unchanged (backward compatible).
  - More than the cap of applicable rules → highest-enforcement first, truncated, marked.
  - A rule whose scope node was refactored away → still returned by path/global scope if it applies.

- **Acceptance criteria:**
  - [ ] `kg_rules` lists active rules by scope/category with the standard degrade-to-configured:false.
  - [ ] `review_run` injects a bounded `active_rules` block for the changed code when the rules
        store is configured; unconfigured ⇒ the review context is unchanged (backward compatible).
  - [ ] Injection is best-effort + bounded; a lookup failure never errors the review.
  - [ ] No hardcoded infra. All existing tests pass.

### KGRULE-05: Cortex↔KG bridge helper
- **Priority:** Medium
- **Labels:** terminus, knowledge-graph, cortex
- **Agent:** claude
- **Estimate:** 2h
- **Description:** A small, reusable bridge that turns a KG scope into a Cortex risk score
  (best-effort, degrades cleanly when Cortex is unconfigured), used by KGRULE-02's crystallization
  to prioritize high-risk recurring issues. This is the concrete "connect Cortex and KG": the
  remote Cortex risk analysis feeds the local KG's rule prioritization.

  ## FILES
  - `src/scribe/graph/cortex_bridge.rs` — NEW: `cortex_risk_for_scope(scope_kind, scope_ref) ->
    Option<f32>`.
  - `src/scribe/graph/mod.rs` — `pub mod cortex_bridge;`.
  - `src/scribe/graph/README.md` — document the bridge + its degrade contract.

  ## APPROACH
  1. `async fn cortex_risk_for_scope(scope_kind: &str, scope_ref: &str) -> Option<f32>`: if
     `CORTEX_SSH_HOST` is unset → `None` (no-op). Else call the existing Cortex tool path
     (`crate::cortex` — reuse its config + `cortex_review`/`cortex_scope` for a path/symbol),
     parse a numeric risk from the returned JSON (tolerant — the tool returns `{raw:…}` on
     non-JSON; extract a `risk`/`score` field if present, else `None`). NEVER panics, NEVER errors
     out — returns `None` on any failure.
  2. Keep it a thin, dependency-light helper so KGRULE-02 (and future callers) can call it without
     knowing Cortex internals.

  ## TEST PLAN
  - Pure test: the JSON→risk extraction (given a `{ "risk": 0.7 }` or `{ "score": … }` or a
    `{ "raw": … }` or malformed value → the right `Option<f32>`), tested without SSH.
  - `#[serial]`: `CORTEX_SSH_HOST` unset → `None` (no SSH attempt).
  - `cargo test --workspace` green.

  ## EDGE CASES
  - Cortex unset → None, no SSH.
  - Cortex returns `{raw:…}` (non-JSON) → None.
  - Cortex returns a risk out of [0,1] → clamp or pass through consistently (document which).

- **Acceptance criteria:**
  - [ ] `cortex_risk_for_scope` returns `None` when `CORTEX_SSH_HOST` is unset (no SSH attempt),
        and a parsed `Option<f32>` when Cortex answers; never panics or errors.
  - [ ] The JSON→risk extraction is pure + unit-tested for JSON/`raw`/malformed inputs.
  - [ ] Used by KGRULE-02 crystallization to attach `cortex_risk`, best-effort.
  - [ ] No hardcoded infra. All existing tests pass.
