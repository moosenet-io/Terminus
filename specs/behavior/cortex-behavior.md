# Cortex elegance/consistency gate — behavior spec

Spec: `S115-cortex-elegance-gate` (Plane project `TERM`, prefix `CXEG`),
covering CXEG-01 through CXEG-12. See `docs/cortex-elegance-gate.md` for the
prose operator/contributor reference this spec formalizes; this file states
the load-bearing SAFETY properties as checked states + verify blocks.

Source: `src/cortex/{mod,scope,metrics,review,house_style,waiver,crystallize,
debt,audit}.rs`, `src/house_style/mod.rs`, `src/review/{mod,consistency}.rs`,
`src/scribe/graph/findings_store.rs`.

Verify: `cargo test -p terminus-rs` drives every state/contract below
hermetically — no live Postgres/network/mTLS dependency is required for the
load-bearing safety properties (fail-open, never-flips-verdict, never-auto-
rejects), which are tested against pure functions and synthetic/fixture
inputs. Tests whose behavior additionally depends on a configured Atlas KG
Postgres (`ATLAS_DATABASE_URL`) self-skip when it is unset, exercising the
degrade path instead (see each state's `verify` for which is which).

## Why this spec exists

The three tiers (mechanical / structural / taste — see
`docs/cortex-elegance-gate.md`'s tier table) intentionally carry different
authority: Tier A can hard-block a merge, Tiers B and C structurally cannot.
That asymmetry is the single most important property of this whole gate —
an accidental regression that let a risk score or a consistency finding flip
a correctness verdict would silently convert an advisory signal into a
blocking one, exactly the taste-gate failure mode the design (and CXEG-10's
calibration discipline) exists to prevent. This spec exists to make that
asymmetry a CHECKED contract, not just a design intention documented in
prose.

## States

### State: Tier-A house-style gate — Clean

- entry: `house_style::check_tree(repo_root)` walks every `src/**/*.rs` file
  and returns zero `Rule::*` violations (no `RawSecretEnvVar`, no
  `EmptyDescription`, no `PanicInExecute`, no `FileError`).
- exit: a new violation is introduced into the tree → **Violated** (the
  Stage-4 test gate fails at the next `cargo test` run).
- verify:
  - `command_exit_code("cargo test -p terminus-rs --test house_style", 0)`
  - `command_exit_code("cargo run --bin house_style_check", 0)`

### State: Tier-A house-style gate — Violated (detection, not a live-tree state)

The main tree is expected to always be in the **Clean** state above (Tier A
is a hard Stage-4 gate — a `Violated` main tree would already be a build
failure, not a state this spec asks a live checkout to demonstrate). This
state instead verifies the DETECTOR correctly recognizes each violation
class against fixtures, so "Clean" above is a real signal and not a checker
that never fires:

- entry: a fixture/synthetic file trips Rule 1 (a bare
  `std::env::var("...TOKEN")`/`env::var(...)`/aliased `var(...)` inside a
  `RustTool::execute`/`execute_structured` body, outside the sanctioned
  `crate::config`/`*::from_env()`/`crate::secrets_bootstrap` exemption),
  Rule 2 (an `impl RustTool`'s `description()` returns an empty string
  literal), or Rule 4 (a `panic!` inside `execute`/`execute_structured`).
- exit: n/a — a fixture-only, unit-tested classification path.
- verify:
  - `command_output_contains("cargo test -p terminus-rs house_style::tests:: -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs house_style::tests::test_only_code_is_skipped", "test result: ok")` (Rules 1/4 never fire inside `#[test]`/`#[cfg(test)]` code — a documented exemption, checked, not assumed).

### State: Tier-B risk score — Not Escalated

- entry: `cortex_review`'s computed `band` is `"low"` or `"elevated"`
  (`risk_score < CORTEX_RISK_SCORE_THRESHOLD`), OR `CORTEX_ESCALATION_ENABLED`
  is `false`, OR the `review_run` call carries no `context.project_id` /
  derivable `changed_files`.
- exit: a subsequent review of the same scope computes a `"high"` band with
  escalation enabled and no active waiver → **Escalated**.
- verify:
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_disabled_is_a_noop -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_no_project_id_is_a_noop -- --nocapture", "test result: ok")`
  - `json_field("<review_run response>", "escalation.escalated", "false")` for a call meeting this state's entry condition (schema check; the tool itself is invoked via MCP, not a fixed file path — see "API Contracts" below).

### State: Tier-B risk score — Escalated

- entry: `cortex_review`'s `band` is `"high"`, `CORTEX_ESCALATION_ENABLED` is
  `true`, no active non-expired waiver covers the change's scope for rule
  `cortex_review_high_band`, the `review_run` `structure` is NOT
  `adversarial_pair`, and the panel has room under `MAX_PROVIDERS` (5).
- exit: the finding driving the `"high"` band is fixed (a later review of
  the same scope scores lower) → **Not Escalated**; or a waiver is recorded
  covering the scope → **Waived**.
- verify:
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_high_band_widens_panel_and_sets_escalated_true -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_does_not_duplicate_an_already_present_add_provider -- --nocapture", "test result: ok")`
  - `json_field("<review_run response>", "escalation.escalated", "true")` AND the response's `providers` array length is exactly one more than the caller-supplied panel (never more, never a duplicate entry).

### State: Tier-B risk score — Waived

- entry: an active (non-expired, rule- and scope-matching) `category:
  "waiver"` finding exists for `cortex_review_high_band` covering the
  change's scope.
- exit: the waiver's `expiry` passes → **Escalated** (assuming the band is
  still `"high"` at the next review); or a new, non-covering waiver replaces
  it with a narrower scope that no longer covers the change.
- verify:
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_active_waiver_suppresses_escalation -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_expired_waiver_does_not_suppress_escalation -- --nocapture", "test result: ok")`
  - `json_field("<review_run response>", "escalation.waived", "true")` and `escalation.escalated` is `"false"` for a call meeting this state's entry condition.

### State: Tier-C consistency lens — Disabled

- entry: `CORTEX_ENABLE_TIER_C` is `false` (the default), OR the `review_run`
  call carries no `context.project_id`.
- exit: `CORTEX_ENABLE_TIER_C=true` AND a subsequent call carries
  `project_id` → **Running** or **Degraded** (below), decided by whether the
  project has a usable Atlas graph and a reachable lens provider.
- verify:
  - `json_field("<review_run response>", "consistency.status", "disabled")` (or `"no_project_id"`) for a call meeting this state's entry condition.
  - `json_field("<review_run response>", "consistency.advisory_only", "true")` — present in EVERY state of this lens, not just when it actually ran (see "API Contracts" below for why this field's constant presence is itself part of the safety contract).

### State: Tier-C consistency lens — Running

- entry: `CORTEX_ENABLE_TIER_C=true`, `context.project_id` is present, at
  least one touched community resolves to a usable (≥ `MIN_COMMUNITY_SIZE`)
  house-style profile, and `CONSISTENCY_REVIEW_PROVIDER` is reachable.
- exit: the lens completes for this call (always terminal per-call — there
  is no persistent "running" state across calls, this state exists only to
  distinguish a completed lens pass from disabled/degraded).
- verify:
  - `json_field("<review_run response>", "consistency.status", "ok")`
  - `json_field("<review_run response>", "consistency.advisory_only", "true")`
  - the SAME response's `aggregate_verdict` and `complete` fields are
    byte-identical to what they would be with `CORTEX_ENABLE_TIER_C=false`
    for the identical panel input — this is the property "Transition:
    Consistency lens findings never alter the correctness verdict" below
    checks directly.

### State: Tier-C consistency lens — Degraded

- entry: `CORTEX_ENABLE_TIER_C=true` and `project_id` present, but EITHER no
  stored Atlas graph / no touched community clears `MIN_COMMUNITY_SIZE`
  (`"no_graph_or_exemplars"`) OR the pinned lens provider is unreachable
  (`"lens_unavailable"`).
- exit: the missing dependency becomes available on a later call →
  **Running**.
- verify:
  - `json_field("<review_run response>", "consistency.status", "no_graph_or_exemplars")` or `"lens_unavailable"`, mutually exclusive with `"ok"`.
  - `json_field("<review_run response>", "consistency.findings_count", "0")` — a degraded lens never fabricates findings.
  - the response's `aggregate_verdict`/`complete` are unaffected — same check as the Running state.

## Transitions (the load-bearing safety properties)

### Transition: The advisory layer NEVER flips `aggregate_verdict` (CXEG-07/08)

- trigger: any `review_run` call, regardless of `cortex_review` band or
  consistency-lens outcome.
- guard: none — this must hold unconditionally, for every combination of
  Tier-B band and Tier-C status.
- action: `aggregate(structure, &results)` (`src/review/mod.rs`) is computed
  from the correctness panel's `ProviderResult`s ALONE, before either
  `maybe_escalate`'s panel-widening changes what gets dispatched next call,
  or `consistency::maybe_run` is even invoked. Neither function's return
  value is ever passed into `aggregate`, nor does either function mutate
  `results` in place.
- verify:
  - Source-level ordering check (regression-proof against a future
    reordering): `command_output_contains("grep -n 'let (aggregate_verdict, complete) = aggregate' -A 20 src/review/mod.rs | grep -c consistency::maybe_run", "1")` — confirms `consistency::maybe_run` is called AFTER, never before, the `aggregate()` call in `execute()`'s source order.
  - `command_output_contains("cargo test -p terminus-rs review::tests:: -- --nocapture", "test result: ok")` — the full `review::mod` test suite, which includes escalation-decision tests whose fixtures assert `providers`/panel composition changes without ever asserting a changed `aggregate_verdict` for the same underlying panel results.
  - Property check: for two `review_run` calls with IDENTICAL panel
    `ProviderResult`s, one with `CORTEX_ENABLE_TIER_C=true` and a fired
    consistency finding, one with it `false` — `aggregate_verdict` and
    `complete` are byte-identical across both. (Exercised by the Running/
    Degraded state verify blocks above, which check this per-state; stated
    here as the general property those checks instantiate.)

### Transition: Risk escalation never auto-rejects (CXEG-08)

- trigger: `cortex_review`'s `band` is `"high"` on a `review_run` call.
- guard: none.
- action: `maybe_escalate` may append a provider name to the `providers`
  list about to be dispatched. It never sets `aggregate_verdict` to
  `REQUEST_CHANGES`/`CHANGES_REQUESTED`, never removes a provider, and never
  causes `review_run` to return early without dispatching. The ONLY way a
  `"high"` band changes the outcome is by adding one more independent
  reviewer's OWN correctness opinion to the normal panel — identical in
  kind to the caller having asked for a bigger panel up front.
- verify:
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_high_band_widens_panel_and_sets_escalated_true -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs cortex::review::tests:: -- --nocapture", "test result: ok")` — `recommendation_for` never contains the substring `"reject"` for any band (unit-tested directly in `src/cortex/review.rs`'s test module).
  - Source check: `command_output_contains("grep -c 'aggregate_verdict' src/cortex/waiver.rs src/cortex/review.rs", ":0")` on both files — neither the risk-scoring module nor the waiver module references `aggregate_verdict` at all; the field doesn't exist in their vocabulary, so it structurally cannot be assigned there.

### Transition: The risk/advisory layer fails OPEN, never blocks (CXEG-04/07/08)

- trigger: ANY dependency of Tier B or Tier C is unavailable — no stored
  Atlas graph, an unreachable/unconfigured KGFIND findings store, an
  unreachable consistency-lens provider, an unreachable/erroring waiver
  lookup, an invalid `escalation_add_provider`, or the panel already at
  `MAX_PROVIDERS`.
- guard: none — every one of these is independently checked.
- action: the correctness gate proceeds using ONLY the panel's own verdict,
  exactly as if the unavailable dependency (or CXEG-07/08 themselves) did
  not exist. `cortex_review` returns a labeled degrade response
  (`configured:false`/`band:"unknown"`, or `findings:"unavailable"`) rather
  than an error; `maybe_escalate` treats a non-`"high"` (including
  `"unknown"`) band, an unreachable waiver lookup, or a full/invalid-target
  panel as "no escalation" rather than blocking dispatch; the consistency
  lens returns a `"status"` describing why it didn't run, never an error
  that would propagate out of `review_run`.
- verify:
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_fail_open_when_cortex_review_unavailable -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_invalid_add_provider_degrades_without_widening -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_panel_already_at_max_degrades_without_widening -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs review::tests::maybe_escalate_adversarial_pair_panel_is_never_widened -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs cortex::review::tests::compute_review_degrades_when_project_has_no_graph -- --nocapture", "test result: ok")`
  - `command_output_contains("cargo test -p terminus-rs cortex::scope::tests:: -- --nocapture", "test result: ok")` — `cortex_scope`'s own no-graph degrade (the same posture `cortex_review`/`cortex_consistency_debt` mirror).
  - `command_output_contains("cargo test -p terminus-rs cortex::debt::tests::compute_debt_degrades_without_a_configured_findings_store -- --nocapture", "test result: ok")` — CXEG-12's read-only trend degrades the same way; never errors when `ATLAS_DATABASE_URL` is unset.

## API Contracts

### API: `review_run`'s `escalation` block (CXEG-08)

- input: any `review_run` call with `context.project_id` set.
- output: an additive, always-present `"escalation"` object —
  `{escalated: bool, band?: string, risk_score?: number, waived?: bool,
  escalation_degraded: bool, reason: string, advisory_only: true,
  added_provider?: string, waiver?: object}`. `advisory_only` is `true` in
  EVERY case — this is a deliberate, checked constant, not a value that
  varies with outcome, because it is the field a caller checks to remember
  that nothing in this block ever touched `aggregate_verdict`/`complete`.
- verify:
  - `json_field("<review_run response>", "escalation.advisory_only", "true")` — for every band, every waiver state, every degrade path.
  - `json_valid("<review_run response>")` for every combination exercised by the State verify blocks above.
- error_cases:
  - `context.project_id` absent → `escalation.escalated == false`, `reason` names the missing precondition; never a `review_run`-level error.
  - `cortex_review` itself errors internally → never surfaces as a `review_run` error; `maybe_escalate` treats it as "not risky" (see the fail-open Transition above).

### API: `review_run`'s `consistency` block (CXEG-07)

- input: any `review_run` call.
- output: an additive, always-present `"consistency"` object —
  `{status: string, provider: string|null, degraded: bool,
  advisory_only: true, findings_count: number, subjective_count: number}`.
  `status` is one of `disabled` / `no_project_id` / `no_changed_files` /
  `no_graph_or_exemplars` / `lens_unavailable` / `ok`.
- verify:
  - `json_field("<review_run response>", "consistency.advisory_only", "true")` — unconditionally, same rationale as the escalation block above.
  - `response.consistency.status` is always one of the six documented values (never an undocumented string, never absent).
- error_cases:
  - Lens provider dispatch fails → `status: "lens_unavailable"`, `findings_count: 0`; never a `review_run`-level error and never a partial/malformed `consistency` object.

### API: `cortex_consistency_debt` (CXEG-12)

- input: `{project_id: string}` (one of `TERM`/`LUM`/`HARM`/`CHRD`/`RAIL`).
- output: `{configured: bool, project_id: string, graph_available?: bool,
  generation?: number, rollups?: array, totals?: object, message?: string}`.
  Read-only: this call never mutates `kg_findings` (no `crystallize_state`
  write, no `FindingsStore::record` call anywhere in `src/cortex/debt.rs`).
- verify:
  - `command_output_contains("grep -c 'FindingsStore::record\\|\\.record(' src/cortex/debt.rs", "0")` — a source-level guarantee that this module contains no write call, mirroring the source-check style used for the verdict-isolation transition above.
  - `json_field("<cortex_consistency_debt response>", "configured", "false")` for a call with `ATLAS_DATABASE_URL` unset (`command_output_contains("cargo test -p terminus-rs cortex::debt::tests::compute_debt_degrades_without_a_configured_findings_store -- --nocapture", "test result: ok")`).
- error_cases:
  - Unknown `project_id` → `InvalidArgument`, before any store I/O (`command_output_contains("cargo test -p terminus-rs cortex::debt::tests::tool_rejects_unknown_project_id -- --nocapture", "test result: ok")`).
  - Findings store configured but unreachable at call time → `configured: false` with a message, never a tool-level error (same degrade posture as `cortex_review`'s `findings:"unavailable"`).
