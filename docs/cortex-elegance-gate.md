# The Cortex elegance/consistency gate — operator & contributor reference

Cortex (`src/cortex/`) plus its two companion mechanisms — the Tier-A lint
set (`src/house_style/`) and the Tier-C consistency lens wired into
`review_run` (`src/review/consistency.rs`) — together form the build
pipeline's code-elegance and consistency-debt gate. This page is the single
end-to-end reference for how all of it fits together: what each tier does,
why they're three separate mechanisms rather than one, how to read a
`cortex_review` risk score, how a recurring finding becomes a durable rule,
how waivers work, why calibration comes before any of this is trusted to
gate a real review, and what the consistency-debt trend tells you.

If you only want one tool's own detailed contract (input schema, exact
response shape, config env vars), see `docs/tools/code-git/cortex.md` and
`README.md`'s "Cortex" section — this page is the map that ties them
together, not a replacement for either.

## The three tiers, and why they're separate mechanisms

A single "does this code look right" checker conflates three questions that
have fundamentally different failure modes, confidence levels, and correct
responses. Cortex keeps them as three DISTINCT mechanisms on purpose:

| Tier | Question | Confidence | Mechanism | Gate posture |
| --- | --- | --- | --- | --- |
| **A — Mechanical** | Does the code violate a rule that's ALWAYS wrong, with zero judgment required? | Certain (deterministic AST check) | `src/house_style/` (CXEG-05), a `syn`-based checker | **HARD gate** — `tests/house_style.rs`'s `house_style_rules_hold()` fails the Stage-4 test gate on any violation. No advisory softening; a violation blocks the merge exactly like a failing unit test. |
| **B — Structural** | Does this change quietly make the codebase worse-SHAPED (a god-object growing, a coupling introduced, near-duplicate code), independent of correctness? | High but relative (self-calibrating percentile thresholds against the project's OWN distribution — never a hardcoded absolute) | `src/cortex/metrics.rs` (CXEG-03), consumed by `cortex_review` (CXEG-04) | **Escalation only** — a `"high"` risk band widens the `review_run` panel by one provider (CXEG-08). It NEVER auto-rejects; `cortex_review`'s own `recommendation` text only ever says "escalate scrutiny." |
| **C — Taste / house-style** | Does this change deviate from patterns THIS repository has already established (not generic style opinion)? | Lower (an LLM's judgment, even when grounded in real exemplars) | `src/review/consistency.rs` (CXEG-07), grounded by `cortex_house_style`'s Atlas-derived exemplars (CXEG-06) | **Strictly advisory, captured only** — structurally CANNOT influence `aggregate_verdict`/`complete` (see "Fail-open, always" below). Recurring findings can later CRYSTALLIZE into a Tier-A lint or a documented house rule (CXEG-09) — but only after clearing a much higher, adversarially-tested bar. |

**Why not collapse these into one mechanism?** Each tier's failure mode is
different, and conflating them would make the WRONG failure mode blocking:

- Tier A is cheap and certain, so it can safely be a hard, unappealable gate
  — false positives are structurally impossible for the rules it checks (a
  raw `std::env::var("...TOKEN")` inside an `execute` body either is or
  isn't there).
- Tier B is a relative signal computed from the project's own graph — useful
  for triage ("spend more review attention here"), but not certain enough to
  reject a change outright. A god-object-shaped touch might be exactly the
  right refactor.
- Tier C is an LLM's opinion, even a well-grounded one. Blocking a merge on
  an LLM's taste judgment is the taste-gate failure mode this whole system
  is designed to avoid (see "Calibration" below) — so it is architecturally
  incapable of doing so, and its only path to real enforcement is
  CRYSTALLIZATION, which re-tests the claim adversarially before it can ever
  gate anything.

### Fail-open, always (the load-bearing property across B and C)

Both Tier B's escalation (CXEG-08) and Tier C's lens (CXEG-07) are wired so
that **nothing about risk/taste can ever be an input to `aggregate_verdict`**
— not "weighted low," structurally absent from that computation entirely:

- CXEG-08's `maybe_escalate` runs BEFORE the provider `JoinSet` is spawned.
  Its only effect is appending a provider name to the panel about to be
  dispatched. It never reads or sets `aggregate_verdict`/`complete`.
- CXEG-07's consistency lens runs strictly AFTER `aggregate()` has already
  computed the correctness verdict. Its findings are captured into a
  separate `"consistency"` response block, never fed back into the verdict
  computation.
- Both degrade to a clean no-op (never an error, never a block) when their
  dependencies are unavailable: `cortex_review` unconfigured → treated as
  "not risky" (escalation's fail-open contract, `maybe_escalate` step 2);
  the consistency lens's provider unreachable → `"status":
  "lens_unavailable"`, zero findings, review proceeds normally.

See `specs/behavior/cortex-behavior.md` for this as a checked behavior
contract, not just a design description.

## Reading a `cortex_review` risk score

`cortex_review(project_id, changed_files|diff)` (CXEG-04, `src/cortex/review.rs`)
is Tier B's single entry point. It returns a `risk_score` (0.0–10.0, clamped),
a `band`, the fired `risk_signals`, and per-source `contributions` whose
`points` sum reconstructs the raw pre-clamp score exactly — nothing hidden.

**Where the score comes from** — two additive sources:
1. **Structural signals** (CXEG-03, `metrics::compute_signals`): each fired
   `EleganceSignal` (`centrality_spike`, `complexity_spike`,
   `fan_out_explosion`, `community_boundary_crossing`, `semantic_duplication`)
   contributes `weight(kind) * severity` points. The three percentile-based
   signals compare against the PROJECT'S OWN current-node distribution
   (`CORTEX_TIER_B_PERCENTILE`, default the 90th percentile) — self-
   calibrating, never a hardcoded absolute value that would fire differently
   on a small repo than a large one.
2. **KGFIND recurrence**: every `(category, total_occurrences)` bucket for
   the touched node/path/community scopes contributes
   `risk_weight_recurrence * log2(1 + total_occurrences)` points —
   LOG-scaled deliberately, so one pathologically-recurring finding bucket
   can't alone pin the score at the ceiling.

**Bands** (cut-points from `CortexConfig`, both inclusive at their lower
bound so a value exactly at a cut-point always resolves to the HIGHER band):
- `"low"` — `risk_score < CORTEX_RISK_BAND_ELEVATED_CUT` (default `4.0`).
  Ordinary review rigor.
- `"elevated"` — `< CORTEX_RISK_SCORE_THRESHOLD` (default `7.0`). Standard
  rigor, attention to the flagged `risk_signals`.
- `"high"` — at or above `CORTEX_RISK_SCORE_THRESHOLD`. Triggers CXEG-08's
  panel-widening escalation (see below) — **never** an auto-reject.
- `"unknown"` — the degrade path (no stored Atlas graph for the project).
  Not a real "zero risk" assessment; `cortex_review` never errors here, it
  just can't score.

**`findings`** distinguishes three states you must not conflate: `"ok"`
(recurrence was looked up and matched something), `"empty"` (looked up,
nothing matched — a real zero), and `"unavailable"` (the KGFIND store itself
was unconfigured/unreachable, or the whole response is on the graph-
unavailable degrade path — the recurrence term simply wasn't computed).

Full rubric, weights table, and response shape:
`docs/tools/code-git/cortex.md`'s `cortex_review` section.

## Escalation — a `"high"` band widens the panel, never blocks (CXEG-08)

A `"high"` `cortex_review` band causes `review_run` to append exactly one
extra provider (`CORTEX_ESCALATION_ADD_PROVIDER`, default `"agy"`) to the
dispatched panel — one more independent correctness opinion, nothing more.
See the decision table in `README.md`'s "Stage-5b risk-gate escalation +
waivers" section for every branch (disabled, waived, `adversarial_pair`
structure, panel already full). The property to remember: **escalation can
only ask for MORE review, never LESS, and never touches the verdict itself.**

## Waivers — accepting risk on purpose (CXEG-08, `cortex_waive`)

When a project owner has looked at a recurring `"high"`-band escalation and
decided it's an accepted, intentional risk (e.g. a hub module that's
correctly centralized, not accidentally god-object-shaped), record a waiver
so the escalation stops firing for it:

```json
{
  "project_id": "TERM",
  "rule": "cortex_review_high_band",
  "scope": "*",
  "reason": "accepted risk for the S115 sprint, revisit after CXEG-10 calibration",
  "author": "<operator>",
  "expiry": "2026-08-01T00:00:00Z"
}
```

**`reason` is MANDATORY and must be non-blank** — `cortex_waive` rejects an
empty/whitespace-only reason with `InvalidArgument` before any store write.
`scope` is `"*"` (project-wide, the default) or a comma-separated file-path
set; a waiver whose scope is broader than what it later suppresses is still
honored, but the escalation lookup flags it `"broad": true` so an over-broad
waiver stays visible rather than silently accepted as "just right." `expiry`
is optional; an unexpired waiver never suppresses past its own recorded
timestamp.

**Waivers are themselves tracked debt.** Every `cortex_waive` call is
recorded as a `category: "waiver"` finding on the SAME KGFIND store every
other finding uses (no new table, S9) — repeating the identical `(rule,
reason)` bumps `occurrences` rather than hiding the repetition. This is
deliberate: **over-waiving the same thing repeatedly is itself a debt
signal**, and it shows up in `kg_findings` and in the consistency-debt trend
(below) exactly like a recurring code finding would. A waiver is not a way
to make risk invisible — it's a way to make ACCEPTED risk visible and
attributable (who, why, until when) instead of silently re-litigated on
every review.

## Crystallization — from a recurring finding to a durable rule (CXEG-09)

`cortex_crystallize(project_id, min_recurrence?, apply?, providers?)`
(`src/cortex/crystallize.rs`) is how Tier C's advisory findings can EARN
real, standing enforcement — but only by clearing two independent bars, one
after the other, never by recurrence alone:

```
finding (category: consistency|elegance)
   │
   │  captured by review_run on every review outcome (KGFIND-01)
   ▼
recurrence ≥ crystallize_min_recurrence (default 3)
   │
   │  bar 1: "this keeps coming up" — necessary but NOT sufficient
   ▼
adversarial review_run panel_majority
   (every provider tries to REFUTE the candidate; defaults to refuting
    when uncertain; majority must FAIL to refute)
   │
   │  bar 2: "and it survives someone actively arguing against it"
   ▼
promotion → classified deterministically
   ├─ mechanically AST-checkable (std::env::var, panic!, .unwrap(), …)
   │     → inert Markdown scaffold appended to
   │       src/house_style/candidate_lint_stubs.md
   │       (NEVER auto-compiled or auto-wired — a human still writes the
   │        real Rule:: variant + syn visitor before it's enforced)
   │
   └─ everything else
         → prose house rule appended to docs/house-style.md under
           "Crystallized house rules (CXEG-09)"
```

**Why recurrence alone is never enough.** A pattern that merely repeats
could still be coincidence, a reviewer's personal preference restated
several times, or already covered by an existing rule under different
wording. The adversarial panel is the check against exactly that: every
provider is explicitly instructed to try to refute the candidate — spurious,
overfit, mere taste, already covered, not generalizable — and to DEFAULT to
refuting when uncertain. `review_run`'s own `panel_majority` aggregation
already fails safe to `REQUEST_CHANGES` on any tie or split, so genuine
uncertainty never accidentally promotes something.

**Convergence, so this terminates.** `kg_findings.crystallize_state`
(`None` / `"promoted"` / `"refuted"`) is written ONLY by this loop. A
promoted or refuted finding is excluded from candidate selection on every
later run — the loop doesn't re-argue the same candidate forever. A
candidate whose promotion panel comes back INCOMPLETE (a provider didn't
answer) is left unmarked and stays eligible — a transient dispatch failure
must never permanently discard a candidate that was never actually argued.

**Dry-run by default.** `apply` defaults to `false` — a call lists
candidates with a `would_classify_as` preview and writes nothing. `apply:
true` is required to actually dispatch the promotion panel and write an
artifact, and it REFUSES outright (falls back to a dry listing) if neither
`REVIEW_DAEMON_TOKEN` nor `OPENROUTER_API_KEY` is configured — recurrence
alone can never crystallize a rule without a real adversarial panel to run.

**Not the same loop as KGRULE.** `crate::scribe::graph::rules`
(`kg_rule_crystallize`/`kg_rule_promote`) is a SEPARATE, more general
crystallization pipeline that mints enforcement-level `kg_rules` rows
(`advisory`/`lint-candidate`/`blocking`) from recurring findings of ANY
category, promoted via an `adversarial_pair` review, feeding back into every
`review_run`'s own prompt context. CXEG-09 is scoped specifically to
`consistency`/`elegance` findings and always emits a CXEG-05-shaped artifact
(a lint stub or a house-style doc entry) — the two loops read the same
`kg_findings` corpus but write to different destinations and are not layered
on top of each other. Don't confuse "crystallized a CXEG-09 house rule" with
"promoted a `kg_rules` row" — they are different durable outcomes with
different governance paths.

## Calibration — before any of this is trusted to gate a real review (CXEG-10)

Every threshold above (`CORTEX_TIER_B_PERCENTILE`, `CORTEX_DUP_COSINE_THRESHOLD`,
`CORTEX_RISK_BAND_ELEVATED_CUT`, whether `CORTEX_ENABLE_TIER_C` is ever
flipped to `true` in a live deployment) starts as a "sane conservative
default" — not a value derived from evidence. Trusting an untuned automated
taste/risk signal risks the exact failure mode this whole gate exists to
avoid: a taste-gate that blocks PRs on a reviewer's mood instead of on
anything that predicts real trouble.

`cortex_calibrate` (CXEG-10, `src/bin/cortex_calibrate.rs`) answers this
empirically before any threshold is trusted: replay the last N merged PRs of
a project, score each diff with BOTH engines in a dry/capture-only mode
(never writing to KGFIND), and report the proxy false-positive rate — how
often this scoring WOULD have flagged code that, in fact, shipped and merged
fine.

```sh
cargo run --bin cortex_calibrate -- \
    --project-id TERM --owner moosenet --repo Terminus --n 50
```

Below the minimum sample size (`--min-sample`, default 20 scored PRs), the
report declines to recommend a threshold change at all (`sample_small:
true`) — a small sample can't distinguish a real problem from noise. At or
above it, an over-target false-positive rate gets a CONCRETE recommended new
value for the top-firing signal's controlling env var (e.g. "raise
`CORTEX_TIER_B_PERCENTILE` from 90 to 93"), derived from the observed
overshoot — not just a variable name to go guess at. The workflow is
iterative: adjust the named env var, re-run calibration against the same or
a fresher corpus, confirm the rate actually moved, repeat. Only after this
converges near the target rate should `CORTEX_ENABLE_TIER_C` go live in a
real deployment, or a `risk_weight_*`/band-cut default be tightened.

Full methodology, exclusions (revert/hotfix PRs), and the exact adjustment
formulas: `docs/cortex-calibration.md`.

## Consistency-debt trend (CXEG-12)

`cortex_consistency_debt(project_id)` (`src/cortex/debt.rs`) is the
longitudinal view none of the per-PR tools above give you on their own: a
READ-ONLY aggregation over the same KGFIND corpus, rolled up per Leiden
community and per category (`consistency` / `elegance` / `waiver`), so an
operator can see whether house-style debt is growing or shrinking and which
subsystems are accruing it — without standing up a second store or writing
anything back to the graph.

This closes the loop the rest of this page describes: findings are captured
by every `review_run` (Tier C, and Tier B when captured as a finding);
recurrence is what crystallization watches for; waivers are themselves
findings; and the debt trend is where all three become visible as a single
per-community picture instead of scattered per-PR facts. A community whose
`consistency`/`elegance` buckets keep growing (recent `last_seen`, rising
`total_occurrences`) without a matching `cortex_crystallize` promotion is the
concrete signal that "this subsystem has real, un-addressed house-style
debt" — as opposed to a community with a high `waiver` bucket, which instead
says "this subsystem's flagged risk is being deliberately, trackably
accepted rather than fixed." Those are different situations that call for
different operator responses, and the trend is what makes the difference
visible instead of anecdotal.

See `README.md`'s "Consistency-debt trend (CXEG-12)" section for the full
response shape and degrade contract, and `src/cortex/debt.rs`'s module doc
for the implementation.
