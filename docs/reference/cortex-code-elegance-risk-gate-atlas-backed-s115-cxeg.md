## Cortex — code-elegance / risk gate (Atlas-backed, S115/CXEG)

Cortex is the pipeline's code-elegance, consistency, and risk gate. It was
originally a thin SSH-exec relay to a script on an external fleet host; that
host is retired and the relay with it. As of **CXEG-01** the module is
re-scaffolded in-process, keyed by `project_id` (`TERM`/`LUM`/`HARM`/`CHRD`/
`RAIL`), and built on the live Atlas knowledge graph rather than a subprocess.
Its risk/elegance surface is rebuilt over the following S115 items:

- `cortex_scope` — pre-change blast radius for a planned change, live as of
  **CXEG-02**: given `project_id` + `changed_files` (comma-separated string
  or array) or a unified `diff`, it resolves the touched symbols against the
  project's Atlas graph and walks their 1-hop callers/callees via the shared
  `scribe::graph::query::one_hop_neighbors` helper (the same single-source walk
  `kg_neighbors` uses), filtered to the current bi-temporal view so a
  since-removed symbol never appears. Returns a JSON object with fields
  `configured` (bool), `project_id`, `changed_files`, `blast_radius[]` (each
  entry `{id, path, kind, resolved, role}` where `role` is
  `touched`/`caller`/`callee`), `affected_communities` (sorted cluster ids),
  `blast_count`, `token_reduction_pct` (how much smaller the blast radius is
  than the whole project), and `truncated` (present only when a cap fired).
  Degrades to `configured:false` (the literal `changed_files` echoed back as
  unresolved entries) instead of erroring when the project has no stored Atlas
  graph yet — dispatch never breaks on a missing graph. Sets `truncated:true`
  (with a distinct logged warning on the live AND degrade paths, never a
  silent drop) for either the input-file cap (`MAX_CHANGED_FILES`) or the
  blast-node cap (`CORTEX_MAX_BLAST_NODES`, default 200). An oversized-*by-file
  -count* list/diff truncates (with `truncated:true`) rather than erroring;
  `InvalidArgument` is reserved for genuinely abusive/malformed input (a single
  path over `MAX_TEXT_LEN`, a DoS-scale `diff`/string over `MAX_DIFF_LEN`, or an
  array over `MAX_CHANGED_FILES_ARG` — ceilings set far above the file-count
  cap so real diffs truncate, not reject).
- `cortex_review` — post-change risk assessment, live as of **CXEG-04**:
  given `project_id` + `changed_files`/`diff` (same argument shapes as
  `cortex_scope`, sharing its validation via
  `crate::cortex::validate_and_parse_changed_files`), it combines CXEG-03's
  structural-elegance signals (`metrics::compute_signals`, over the diff's
  touched Atlas nodes) with KGFIND-01 recurrence counts for the same touched
  node/path/community scopes (`scribe::graph::findings_store::FindingsStore`
  — the same store `kg_findings` reads, no second access path) into a single
  `risk_score` (0–10, clamped), a `band` (`low`/`elevated`/`high`), the full
  `risk_signals` list, and per-source `contributions` (`{source, weight,
  points}`) whose `points` sum reconstructs the raw pre-clamp score —
  nothing hidden. Recurrence is log-scaled (`log2(1 + occurrences)`) so one
  pathologically-recurring finding bucket can't alone pin the score at the
  ceiling. `recommendation` only ever ESCALATES review rigor for a high
  band — never an auto-reject. Degrades cleanly, never erroring: no stored
  Atlas graph yet → `configured:false` + `band:"unknown"` (mirrors
  `cortex_scope`'s own degrade shape); an unconfigured/unreachable findings
  store → a structural-only score labeled `findings:"unavailable"`; a
  reachable store with no matching recurrence → `findings:"empty"` (distinct
  from `"unavailable"`). See `src/cortex/review.rs` and
  `docs/tools/code-git/cortex.md`'s `cortex_review` section for the full
  rubric, weights, and response shape.
  Its structural-metrics half is a standalone library
  (`src/cortex/metrics.rs`, **CXEG-03**): `metrics::compute_signals` turns a
  `cortex_scope` blast radius into five named, no-LLM structural-elegance
  signals — `centrality_spike`, `community_boundary_crossing`,
  `semantic_duplication`, `complexity_spike`, `fan_out_explosion` — each with
  a percentile-relative (self-calibrating, never hardcoded) threshold, a
  non-empty deterministic `why`, and a resolvable anchor node; see
  `docs/tools/code-git/cortex.md`'s "Tier-B structural-elegance signals"
  section for the full signal catalog.
- `cortex_audit` — audit an external public repo URL, live as of **CXEG-11**:
  `url` first passes the unchanged SSRF-hardened `validate_repo_url`
  front-gate (`src/cortex/audit.rs`) — it rejects non-http(s) schemes,
  embedded credentials, shell metacharacters, and loopback/private/link-local
  /metadata hosts in their common obfuscated encodings (fail-closed) — then
  the tool clones the url into an isolated, always-cleaned-up scratch
  directory (shallow, no submodules, no repo code ever executes), statically
  extracts a transient (never persisted) Atlas graph via the same
  `build_rust_graph`/`walk_rs` path `scribe_kg_build` uses, runs the CXEG-03
  structural-elegance detectors (`metrics::compute_structural_signals`) over
  the whole repo, and returns a report before deleting the clone. Clone size
  and time are bounded (`CORTEX_AUDIT_MAX_CLONE_BYTES` /
  `CORTEX_AUDIT_CLONE_TIMEOUT_SECS`) — an oversized or slow clone is refused,
  not silently truncated.
- `cortex_house_style` — live as of **CXEG-06**: house-style exemplar
  extraction from Atlas (`src/cortex/house_style.rs`), so a future Tier-C
  reviewer can cite "how THIS codebase does X" instead of generic opinion.
  Scoped **per Leiden community** (KGRAPH-05), never a single global style —
  a `pg/` subsystem and a `cortex/` subsystem can legitimately favor
  different idioms. For `project_id` (+ optional `community`, else up to 25
  communities ascending), returns each community's deterministic modal
  `facts` (dominant node kind, an error-type idiom, a `from_env()`
  config-read idiom, whether the `RustTool` 4-method shape is present — all
  derived from graph metadata only: `kind`/`name`/`path`, never a
  source-text read or an LLM call) plus per-kind `exemplars_by_kind` (node
  id, file, span, rank, selection score), chosen by nearest-to-centroid
  embedding similarity over each member's `node_card` (the same
  `vec_embed::node_card`/`EmbedClient` path `metrics`'s semantic-duplication
  detector and `scribe_kg_build`'s pipeline reuse). Degrades honestly rather
  than misrepresenting a thin sample: a community below
  `house_style::MIN_COMMUNITY_SIZE` is `profile:"unstable"` with no
  exemplars; a `(community, kind)` bucket below `MIN_BUCKET_SIZE` flags
  `sparse:true`; an unavailable/unreachable embeddings endpoint falls back to
  centrality-only ranking and flags `degraded:true` (only for the affected
  buckets — every other bucket in the same profile is unaffected). Every
  distribution filters to the current bi-temporal view
  (`graph.current_nodes()`), so an invalidated symbol never appears. Profiles
  are cached in-process per `(project_id, community)`, keyed by the graph's
  `build_seq` "generation" (`house_style::HouseStyleCache`), so a
  `scribe_kg_build` rebuild transparently invalidates every stale entry on
  next access. Degrades to `configured:false` (never an error) when the
  project has no stored Atlas graph yet.
- `cortex_waive` — live as of **CXEG-08**: record a tracked waiver
  (`project_id`, `rule`, `scope`, a MANDATORY non-blank `reason`, `author`,
  optional `expiry`) against `review_run`'s Stage-5b risk-gate escalation
  (below), stored as a `category:"waiver"` finding on the same KGFIND-01
  `FindingsStore` every other finding uses — no new database. See
  "`review_run`'s Stage-5b risk-gate escalation + waivers (CXEG-08)" below
  for the full escalation/waiver policy and response shapes.
- `cortex_crystallize` — live as of **CXEG-09**: the rule crystallization
  loop (`src/cortex/crystallize.rs`). See "Rule crystallization loop
  (CXEG-09)" below for the full lifecycle.
- `cortex_consistency_debt` — live as of **CXEG-12**: a READ-ONLY,
  per-community/per-category rollup of `consistency`/`elegance`/`waiver`
  findings (`src/cortex/debt.rs`) over the SAME KGFIND-01 `FindingsStore`
  every other finding-shaped Cortex tool already reads — no new store, no
  writes. See "Consistency-debt trend (CXEG-12)" below.

For the full three-tier mental model (mechanical Tier-A house-style lints,
structural Tier-B metrics, taste-grounded Tier-C consistency review),
the risk-score rubric, the crystallization lifecycle, and how waivers/
calibration/the debt trend all fit together, see
[`docs/cortex-elegance-gate.md`](docs/cortex-elegance-gate.md).

The seven retired graph-relay tools are kept only as zero-I/O **deprecation
aliases** (`src/cortex/deprecated.rs`) that return a structured
`{"deprecated": true, "use": "kg_..."}` pointer to their live Atlas
equivalents: `cortex_stats`→`kg_stats`, `cortex_build`→`scribe_kg_build`,
`cortex_deps`→`kg_neighbors`, `cortex_recent`→`kg_query`,
`cortex_community`/`cortex_architecture`→`kg_communities`,
`cortex_flows`→`kg_path`.

### Rule crystallization loop (CXEG-09)

`cortex_crystallize(project_id, min_recurrence?, apply?, providers?)`
(`src/cortex/crystallize.rs`) closes the loop between KGFIND recurrence and
durable, ENFORCED house-style guidance. A `category:consistency|elegance`
finding in `kg_findings` graduates from "a reviewer noticed this a few
times" to a standing rule only after two independent bars, never one alone:

1. **Recurrence** — the finding's `occurrences` (queried via
   `FindingsStore::list`, KGFIND's own query path — no parallel SQL) is at
   or above `CortexConfig.crystallize_min_recurrence`
   (`CORTEX_CRYSTALLIZE_MIN_RECURRENCE`, default `3`).
2. **Adversarial promotion** — an in-process `review_run` call with
   `structure="panel_majority"` (default a 3-provider panel: `codex`, `agy`,
   `nemotron`; overridable via `providers`), whose `criteria` text
   explicitly instructs every reviewer to try to REFUTE that the candidate
   should become a durable, enforced rule — spurious, overfit to a handful
   of findings, mere taste, already covered by an existing lint, or not
   generalizable — and to DEFAULT to refuting (`VERDICT: REQUEST_CHANGES`)
   when uncertain. Promotion requires a **complete** panel AND an aggregate
   `APPROVE` (majority failed to refute); `review_run`'s own
   `panel_majority` aggregation already fails safe to `REQUEST_CHANGES` on
   any tie or split, so "uncertain" never accidentally promotes.

A promoted candidate is then classified, deterministically and
conservatively, by whether its description names a concrete, mechanically
AST-checkable construct (`std::env::var`, `panic!`, `.unwrap()`, …):
- **Lint-able** → an inert Markdown scaffold is appended to
  `src/house_style/candidate_lint_stubs.md` (inside the CXEG-05 crate's own
  directory, but never compiled or auto-wired) — a human still has to
  confirm the pattern and hand-write the actual `Rule::` variant + `syn`
  visitor logic before it's ever enforced.
- **Everything else** → a prose house rule is appended to
  `docs/house-style.md` under "Crystallized house rules (CXEG-09)".

**Convergence.** `kg_findings` carries a `crystallize_state` column
(`None` / `"promoted"` / `"refuted"`) that this loop is the sole writer of.
A promoted or refuted finding is excluded from candidate selection on every
later call — so a batch crystallization run terminates rather than
re-arguing the same candidates forever. A candidate whose promotion panel
comes back *incomplete* (a provider didn't answer) is left unmarked and
stays eligible: a transient dispatch failure must never permanently and
silently discard a candidate that was never actually argued.

**Dry-run by default.** `apply` defaults to `false`: the tool lists
candidates (with a `would_classify_as` preview) and writes/marks nothing.
`apply:true` is required to actually dispatch the promotion panel and write
an artifact — and if neither `REVIEW_DAEMON_TOKEN` nor `OPENROUTER_API_KEY`
is configured at all, `apply` REFUSES outright (falls back to a dry
listing) rather than ever crystallizing on recurrence alone.

**Distinct from KGRULE.** `crate::scribe::graph::rules`
(`kg_rule_crystallize`/`kg_rule_promote`, KGRULE-01..04) is a separate,
more general crystallization loop: it mints enforcement-level `kg_rules`
rows (`advisory`/`lint-candidate`/`blocking`) from recurring findings of
ANY category, promoted via an `adversarial_pair` review, and its promoted
rules feed back into `review_run`'s own prompt context (KGRULE-04). CXEG-09
is scoped specifically to `consistency`/`elegance` findings and always
emits a CXEG-05-shaped artifact (a lint stub or a house-style doc entry),
never a `kg_rules` row — the two loops read from the same `kg_findings`
corpus but write to different destinations and are not layered on top of
each other.

### Consistency-debt trend (CXEG-12)

`cortex_consistency_debt(project_id)` (`src/cortex/debt.rs`) is a READ-ONLY
aggregation over the exact same KGFIND-01 `FindingsStore` corpus every other
finding-shaped Cortex tool already reads (`cortex_review`'s recurrence
lookup, `cortex_crystallize`'s candidate selection, `cortex_waive`'s waiver
ledger) — no new store, no writes, no second findings-access path (S9). It
answers the question none of the per-PR tools answer on their own: **across
everything the review gates have already recorded, is house-style debt
growing or shrinking, and which subsystems are accruing it?**

It rolls up every `category: "consistency"` (CXEG-07's Tier-C lens),
`"elegance"` (CXEG-04's structural signals when captured as a finding), and
`"waiver"` (CXEG-08's `cortex_waive` — over-waiving is itself debt worth
surfacing) finding for `project_id`, grouped by:
- **community** — a `node`/`path`-scoped finding is resolved to its Leiden
  community via the project's stored Atlas graph (the SAME `GraphStore`/
  `KnowledgeGraph::get_node`/`current_nodes` lookups `cortex_scope`/
  `cortex_review` already use — no second graph-walk implementation); a
  `community`-scoped finding's `scope_ref` is the community id directly; a
  `global`-scoped finding (most waivers) rolls up under `"project-wide"`; a
  finding that can't be resolved (no stored Atlas graph, or an invalidated
  node/path) rolls up under `"unmapped"` — never fabricated.
- **category** — `consistency` / `elegance` / `waiver`, kept separate so a
  community's debt profile is legible (e.g. "this community has 12 recurring
  consistency findings and 3 waivers" is a very different signal than "15
  waivers and nothing else").

Each `(community, category)` bucket reports `distinct_findings`,
`total_occurrences` (summed across every finding in the bucket — the same
recurrence count `cortex_review`'s log-scaled recurrence term reads),
`first_seen`, and `last_seen` — so a caller can eyeball "is this bucket's
`last_seen` recent" as a growing-vs-stale signal without a second query.
A project-wide `totals` object (one entry per category) gives the
whole-project trend at a glance. Ordering is fully deterministic (community
id ascending, then `"project-wide"`, then `"unmapped"`; category ascending
within a bucket).

**Degrades cleanly, never an error**: no `ATLAS_DATABASE_URL` configured (or
the findings store otherwise unreachable) → `{"configured": false, ...}`,
mirroring `cortex_scope`/`cortex_review`'s own posture. No stored Atlas graph
for the project → the rollup still runs (the findings are real either way),
but every `node`/`path`-scoped finding falls into `"unmapped"` and the
response's `graph_available` is `false`, rather than guessing a community.

```json
{
  "configured": true,
  "project_id": "TERM",
  "graph_available": true,
  "generation": 5,
  "rollups": [
    { "community": 1, "category": "consistency", "distinct_findings": 3, "total_occurrences": 9, "first_seen": "2026-06-01T00:00:00Z", "last_seen": "2026-07-10T00:00:00Z" },
    { "community": "project-wide", "category": "waiver", "distinct_findings": 1, "total_occurrences": 2, "first_seen": "2026-06-15T00:00:00Z", "last_seen": "2026-07-01T00:00:00Z" }
  ],
  "totals": {
    "consistency": { "distinct_findings": 3, "total_occurrences": 9, "first_seen": "2026-06-01T00:00:00Z", "last_seen": "2026-07-10T00:00:00Z" },
    "waiver": { "distinct_findings": 1, "total_occurrences": 2, "first_seen": "2026-06-15T00:00:00Z", "last_seen": "2026-07-01T00:00:00Z" }
  }
}
```

**No new config field.** Unlike most of Cortex, `cortex_consistency_debt`
reuses `FindingsStore::from_env()` and `GraphStore::from_config`/
`ScribeConfig::from_env()` directly — the same env vars (`ATLAS_DATABASE_URL`
and the `SCRIBE_KG_*` family) every other Atlas-backed tool already reads,
with no dedicated `CortexConfig` field of its own to keep in sync.

### `review_run`'s Tier-C consistency/elegance lens (CXEG-07) — ADVISORY ONLY

`review_run` gains an optional additional lens (`src/review/consistency.rs`)
that asks a reviewer to flag deviations from **this repository's own**
established patterns — never generic style opinion, and never a rule the
codebase doesn't already exhibit. It is a **strictly advisory** capture path:
it can never influence `aggregate_verdict`/`complete`, and a total failure of
any of its dependencies degrades cleanly to a no-op.

**Gating.** The lens only runs when BOTH are true: `CortexConfig.enable_tier_c`
(`CORTEX_ENABLE_TIER_C`, default `false`) and `context.project_id` is present
on the `review_run` call. With `enable_tier_c=false` (the default),
`review_run` behaves byte-for-byte as it did before CXEG-07 except for one
additive `"consistency": {"status": "disabled", ...}` field in the result —
no other field, and no dispatched-provider count, changes.

**What it injects.** For the touched community/ies (the Atlas graph's Leiden
clusters covering the changed files, up to 5 communities), the lens's prompt
carries:
- CXEG-06's house-style exemplars + modal facts for each touched community
  (`cortex::house_style`, via the SAME `HouseStyleCache` `cortex_house_style`
  uses — a per-`ReviewRun`-instance cache, so repeated reviews of the same
  project benefit from its generation-keyed memoization);
- CXEG-04's structural `risk_signals` for the change (`cortex::review::compute_review`,
  the same function `cortex_review` calls).

No source-text is re-read for this — every signal is graph-metadata-only
(same posture as `cortex_house_style` itself).

**Pinning.** `CONSISTENCY_REVIEW_PROVIDER` (default a cheap, code-specialized
free-tier OpenRouter model) fixes exactly which provider the lens dispatches
to, routed through the same `is_daemon_provider`/`openrouter_model_for`/`"free"`
table the correctness panel uses (`review::dispatch_provider_raw`, S9
single-source) — a hard guarantee. `CONSISTENCY_REVIEW_TEMPERATURE` (default
`0.0`) is currently **best-effort only**: neither `ReviewConfig::dispatch_daemon`
nor `ReviewConfig::dispatch_openrouter` expose a temperature parameter today,
so it is surfaced to the model as an explicit prompt instruction rather than
an API-level pin — a known, documented gap, not a silent over-claim.

**Findings capture.** The lens's structured output (an optional
`CONSISTENCY_FINDINGS_JSON:` block, distinct from the correctness lens's own
`FINDINGS_JSON:` sentinel so the two are never confused) is tagged
`category: "consistency"` or `"elegance"` and recorded through the SAME
KGFIND-03 `FindingsStore` path every other `review_run` finding goes through
— no second findings-access path (S9). Every entry is anchored to a KG scope
exactly like a correctness finding (`resolve_scope`); a finding with no KG
anchor falls back to `scope: "path"` then `"global"`, never dropped.

**Disagreement, not escalation.** The lens's findings are cross-checked
against any correctness reviewer that independently tagged
`category:consistency|elegance` on its own `FINDINGS_JSON:` block (KGFIND-02):
findings at the same `(category, file, symbol)` anchor from 2+ distinct
sources with DIFFERING description text are marked `subjective: true` on
every entry in that group. A subjective finding is still captured — it is
never escalated or dropped; escalation (if ever built) is explicitly out of
this item's scope (a hypothetical future CXEG-08).

**Degrade contract** — none of the following ever affects the correctness
gate or raises an error from `review_run`:

| Condition | Result |
| --- | --- |
| `enable_tier_c=false`, or no `project_id`/`changed_files` on the call | Clean no-op; `"consistency": {"status": "disabled"\|"no_project_id"\|"no_changed_files", "findings_count": 0, ...}`. |
| No stored Atlas graph, no touched community, or every touched community below `house_style::MIN_COMMUNITY_SIZE` | `"status": "no_graph_or_exemplars"` — never fabricates exemplars for an absent/unstable community; the OTHER touched communities (if any) are unaffected by one unstable one. |
| Lens provider unreachable/unconfigured | `"status": "lens_unavailable"`, `"findings_count": 0`. |
| Embeddings endpoint down during exemplar selection | Exemplars still returned via `cortex_house_style`'s own centrality-only fallback; `"degraded": true` on the result, lens still runs. |
| Lens ran, produced findings | `"status": "ok"`, `"findings_count"`, `"subjective_count"`. |

`review_run`'s result now includes:

```json
"consistency": {
  "status": "disabled" | "no_project_id" | "no_changed_files" | "no_graph_or_exemplars" | "lens_unavailable" | "ok",
  "provider": "qwen_coder" | null,
  "degraded": false,
  "advisory_only": true,
  "findings_count": 0,
  "subjective_count": 0
}
```

`"advisory_only": true` is always present as a reminder at the call site that
this block, however populated, never altered `aggregate_verdict`/`complete`
above it in the same result — those are computed and fixed BEFORE the
consistency lens even runs (see `consistency`'s module doc for why this
ordering is the load-bearing safety property, not just a convention).

### Calibration — `cortex_calibrate` (CXEG-10)

Before the CXEG-04 structural review or the CXEG-07 consistency lens is
allowed to influence a live `review_run` (i.e. before `CORTEX_ENABLE_TIER_C`
ever flips to `true` in a real deployment, and before any threshold weight is
trusted), run the **retroactive calibration harness**: it replays the last N
merged PRs of a project, scores each diff with both engines in a
DRY/capture-only mode, and reports how often that scoring WOULD have flagged
code that in fact shipped and merged — a proxy false-positive rate. This is
the guardrail against a taste-gate that blocks PRs on a reviewer's mood: tune
thresholds against real merged history first, not vibes.

```text
cargo run --bin cortex_calibrate -- \
    --project-id TERM --owner moosenet --repo Terminus --n 50
```

Key flags (see `cortex_calibrate --help` for the full list):

| Flag | Default | Meaning |
| --- | --- | --- |
| `--project-id` | required | Atlas KG project id (`TERM`/`LUM`/`HARM`/`CHRD`/`RAIL`) — the corpus is SCORED against this project's Atlas graph, independent of which Gitea repo the PRs come from. |
| `--owner` / `--repo` | required | Gitea/Forgejo repo the merged-PR corpus is fetched from. |
| `--provider` | pool default | Explicit git-private forge provider id (`gitea`, `forgejo`, …). |
| `--n` | 50 | Target number of merged PRs to replay. |
| `--min-sample` | 20 | Below this many SCORED PRs, the report flags `sample_small: true` and declines to recommend a threshold change. |
| `--target-fp-rate` | 0.10 | The would-have-flagged rate calibration tries to get under. |
| `--include-reverts` | false | By default, PRs that look like a revert/hotfix are excluded from the scored sample (excludable, not a hard drop — they're still counted in the corpus total) so a rushed hotfix's noise doesn't skew the FP rate. |
| `--consistency-lens` | true | Also score with the CXEG-07 lens (LLM calls, one pinned provider per PR). Pass `--consistency-lens false` for a faster structural-only pass. |
| `--out` | `docs/cortex-calibration.md` | Where the generated report is written. |

**S9 — single door.** Every PR-list and diff-compare call goes through
`crate::forge`'s provider-agnostic dispatch (`ForgeRegistry::from_env()` →
`ForgeProvider::dispatch`) — the same mechanism the `git_private` MCP tool
itself uses — never a raw HTTP client. If the git-private forge tool isn't
configured/reachable, the harness fails cleanly (a clear stderr message, no
partial `docs/cortex-calibration.md` write), matching the rest of this
codebase's Terminus-Plane/Gitea "one sanctioned door" discipline.

**Dry mode, structurally.** The harness calls
`review::run_consistency_lens_dry` — a narrow wrapper around the SAME
`consistency::maybe_run` `review_run` itself calls — and never calls
`review::maybe_record_findings` (that function isn't even exported outside
`review::mod`). Nothing in the calibration path can write to the KGFIND
findings store; this is a structural guarantee, not a flag that could drift.

**Interpreting the report.** `docs/cortex-calibration.md` is regenerated on
every run: total/scored PR counts, the excluded-revert and diff-unavailable
counts, the overall would-have-flagged rate against `--target-fp-rate`, a
per-signal firing-rate breakdown (ranked, highest first), and a
plain-language recommendation with a **concrete recommended value** for the
top contributor's controlling knob — e.g. "raise `CORTEX_TIER_B_PERCENTILE`
from 90 to 93" or "raise `CORTEX_DUP_COSINE_THRESHOLD` from 0.85 to 0.95",
derived from the observed overshoot (not just a variable name) — or "no
change needed", or (for a small sample) "collect more merged PRs before
tuning." A consistency-lens top signal has no numeric threshold, so there
the report says so honestly instead of inventing a number. See
`docs/cortex-calibration.md` itself for the full report-format and
methodology writeup. The pure FP-rate math lives in
`crate::cortex::calibrate` and is unit-tested independent of any live corpus
(`cargo test -p terminus-rs calibrate::`).

**Known limitation.** The shared forge vocabulary's only diff-capable
endpoint today is `CommitsCompareDiff` (`GET /repos/{owner}/{repo}/compare/{basehead}`),
whose JSON body may or may not carry a per-file list depending on the
Gitea/Forgejo server version (see `src/bin/cortex_calibrate.rs`'s module doc).
A PR whose compare response has no recognizable file list is flagged
`diff_unavailable: true` and excluded from the scored sample rather than
scored against a fabricated/empty diff. If a live run shows most PRs landing
in `diff_unavailable`, that's the honest signal to add a
`PullRequestsListFiles`-shaped endpoint to the forge vocabulary in a
follow-up item, not something to work around in the harness itself.

### `review_run`'s Stage-5b risk-gate escalation + waivers (CXEG-08) — GOVERNANCE ONLY, never a verdict input

CXEG-04 gave `cortex_review` a `risk_score`/`band`, but explicitly scoped
"what happens when a band is `high`" out (its `recommendation_for` only ever
suggests escalating rigor — never auto-rejects). CXEG-08 is that governance
wiring: it widens the `review_run` panel on a `high` band, and adds a tracked
waiver mechanism so a project owner can accept elevated risk for a specific
rule/scope without the escalation firing every time. **No new scoring** — the
risk score itself is unchanged from CXEG-04.

**Where it runs, and why that ordering matters.** Unlike CXEG-07's
consistency lens (which runs strictly AFTER `aggregate()`), CXEG-08's
escalation runs strictly BEFORE dispatch (`review::mod::maybe_escalate`,
called right after `ReviewConfig`/`CortexConfig` are built and before the
provider `JoinSet` is spawned). Its ONLY effect is appending one provider
name to the `providers` list that is about to be dispatched — it never reads
or sets `aggregate_verdict`/`complete`. This is the load-bearing safety
property: **risk cannot flip the verdict**, because nothing about the
escalation logic is in `aggregate()`'s input at all. What a `high` band buys
is one more independent reviewer's *own* correctness opinion in the normal
panel — same as if the caller had asked for a bigger panel up front.

**Gating.** Controlled by `CortexConfig`:
- `escalation_enabled` (`CORTEX_ESCALATION_ENABLED`, default `true`) — the
  master switch. `false` is byte-for-byte the pre-CXEG-08 dispatch path plus
  one additive `"escalation": {"escalated": false, "reason": "disabled"}`
  field.
- `escalation_add_provider` (`CORTEX_ESCALATION_ADD_PROVIDER`, default
  `"agy"`) — which provider gets appended to the panel on escalation. Must be
  one of `review::ALLOWED_PROVIDERS`; an invalid value degrades the
  escalation attempt rather than erroring the call (see below).

**Decision flow** (`maybe_escalate`), all fail-open:

| Condition | Result |
| --- | --- |
| `escalation_enabled=false`, or no `context.project_id`, or no derivable `changed_files` | No escalation; `providers` untouched. |
| `cortex_review`'s band isn't `"high"` (including an ungraphed project, which degrades internally to `band:"unknown"` — `cortex_review` itself never errors) | No escalation; `providers` untouched. This is the fail-open contract in full: `cortex_review` unavailable ⇒ the correctness gate proceeds on the panel's own verdict alone, exactly as if CXEG-08 didn't exist. |
| An active (non-expired, rule + scope-matching) waiver exists for `HIGH_RISK_BAND_RULE` (`"cortex_review_high_band"`) | No escalation; `"waived": true` + the waiver's details in the result. An EXPIRED waiver does not suppress. |
| `structure == "adversarial_pair"` | `"escalated": true`, `"escalation_degraded": true`, `providers` untouched — a fixed 2-provider defend/attack panel can't be widened without misassigning roles. |
| `escalation_add_provider` isn't a valid provider name, or the panel is already at `MAX_PROVIDERS` (5) and doesn't already include it | `"escalated": true`, `"escalation_degraded": true`, `providers` untouched — escalation degrades gracefully rather than ever blocking dispatch. |
| High band, unwaived, room in the panel | `providers` gains exactly one entry (the configured add-provider; a no-op if it's already present); `"escalated": true`. |

After dispatch, `finalize_escalation` folds in whether the ADDED provider's
own `ProviderResult` came back with an `error` (unreachable daemon/OpenRouter,
same degrade path every other provider already has) — if so,
`"escalation_degraded": true` even though `"escalated"` stayed `true`, so a
caller can tell "we tried to widen the panel but that reviewer didn't answer"
from "we didn't try." Either way, dispatch itself never deadlocks: a degraded
extra reviewer is just one more `"unavailable: ..."` panel entry, handled by
the exact same per-provider degrade path (`run_one_provider`) as any other.

`review_run`'s result now includes:

```json
"escalation": {
  "escalated": true,
  "band": "high",
  "risk_score": 8.2,
  "waived": false,
  "escalation_degraded": false,
  "reason": "high band; panel widened by one provider",
  "advisory_only": true,
  "added_provider": "agy"
}
```

(`"waiver": {...}` is present instead of `"added_provider"` when an active
waiver suppressed escalation.) `"advisory_only": true` is always present for
the same reason CXEG-07's `consistency` block carries it: a reminder that
nothing in this block ever touched `aggregate_verdict`/`complete` above it.

**Waivers — `cortex_waive`.** Records a tracked exception on the SAME
KGFIND-01 `FindingsStore` every other `review_run` finding uses (`category:
"waiver"`, `scope_kind: Global` — no second findings-access path, S9, and no
new database table). `reason` is MANDATORY and non-blank —
`ToolError::InvalidArgument` if empty or whitespace-only. `scope` is `"*"`
(project-wide, the default) or a comma-separated file-path set; a waiver
whose `scope` is broader than the change it later suppresses is still
honored (never rejected for being "too broad"), but the escalation lookup
flags `"broad": true` on the waiver it returns so over-broad waivers are
visible rather than silently accepted. `expiry` is an optional RFC3339
timestamp; an expired waiver is treated as absent (`scope_covers` in
`src/cortex/waiver.rs` is the pure, unit-tested coverage check; matching
happens against the LATEST recorded entry for a given `(rule, reason)` row).
Every waiver is itself a `category:"waiver"` finding, so over-waiving a rule
surfaces in the normal findings/trend tooling (`kg_findings`) exactly like
any other recurring observation — this is deliberate, not an oversight.

```json
// cortex_waive
{
  "project_id": "TERM",
  "rule": "cortex_review_high_band",
  "scope": "*",
  "reason": "accepted risk for the S115 sprint, revisit after CXEG-10 calibration",
  "author": "<operator>",
  "expiry": "2026-08-01T00:00:00Z"
}
```

```json
// response
{
  "recorded": true,
  "created": true,
  "waiver_id": "…",
  "occurrences": 1,
  "project_id": "TERM",
  "rule": "cortex_review_high_band",
  "scope": "*",
  "reason": "accepted risk for the S115 sprint, revisit after CXEG-10 calibration",
  "author": "<operator>",
  "expiry": "2026-08-01T00:00:00Z"
}
```

**What CXEG-08 deliberately does NOT do**: it never sets `aggregate_verdict`
to `REQUEST_CHANGES`/`CHANGES_REQUESTED` from risk alone, never blocks a
merge by itself, and never introduces a new risk-scoring signal — all of
that is CXEG-04's `cortex_review` (unchanged) plus the correctness panel's
own aggregation (unchanged). It is governance around an existing signal, not
a new gate.

