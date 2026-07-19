# cortex

`src/cortex` — 424 KG symbols.

Cortex is the code-quality engine: blast-radius analysis, structural-elegance
metrics, and change-risk scoring for review gating. Its first incarnation was a
thin SSH relay to a script on a since-retired host; the rebuild (CXEG series)
replaced that entirely with the in-process Atlas graph — the seven old pure
graph-relay tools are now structured deprecation aliases pointing at their
`kg_*` successors, and the surviving tools do real work against the stored
graph. A calibration harness measures the false-positive rate of the scoring
against PRs that actually merged, so the gate is tuned on evidence before it is
allowed to influence a live review.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `cortex::scope` | module | `src/cortex/scope.rs` | `cortex_scope`: Atlas-backed blast radius — walks the project's stored graph via the same `GraphStore`/`KnowledgeGraph` API `kg_neighbors` uses. |
| `cortex::review` | module | `src/cortex/review.rs` | `cortex_review`: combines structural-elegance signals with finding recurrence into `risk_score`/`band`/`recommendation`. |
| `cortex::metrics` | module | `src/cortex/metrics.rs` | The structural-elegance signal set consumed by `cortex_review`. |
| `cortex::audit::ScratchClone` | struct | `src/cortex/audit.rs` | Isolated, always-cleaned-up scratch clone for `cortex_audit` (`create`, `repo_path`); the tool's `validate_repo_url` front gate is SSRF-hardened. |
| `cortex::house_style::HouseStyleCache` | struct | `src/cortex/house_style.rs` | Cached house-style state backing the deterministic Tier-A lint set (shared with the `house_style_check` binary and the Stage-4 test gate). |
| `cortex::calibrate::CalibrationKnobs` | struct | `src/cortex/calibrate.rs` | Tunable thresholds with `defaults()`; the knobs `cortex_calibrate` recommends adjustments for. |
| `cortex::calibrate::compute_fp_rate` | fn | `src/cortex/calibrate.rs` | Pure FP-rate math over replayed merged-PR scores — unit-tested independently of the network-touching driver binary. |
| `cortex::crystallize::select_candidates` | fn | `src/cortex/crystallize.rs` | Selects recurring findings eligible for crystallization into standing review rules. |
| `cortex::validate_project_id` | fn | `src/cortex/mod.rs` | Input validation for the `project_id` keying every Atlas-backed tool. |
| `cortex::deprecated` | module | `src/cortex/deprecated.rs` | Structured deprecation aliases for the retired SSH-relay tools (`cortex_stats`, `cortex_build`, `cortex_deps`, ... → `kg_*`). |

## Tools

Live: `cortex_scope`, `cortex_review`, `cortex_audit`, plus the deprecation
aliases (which return pointers, never make network calls). The
`cortex_calibrate` binary drives calibration; `house_style_check` runs the lint
set standalone.

## How it connects

Registered on both the core and personal registries. Everything Atlas-backed
reads `scribe::graph` in-process. The calibration binary reaches PR lists and
diffs exclusively through `forge`'s provider-agnostic dispatch (no direct HTTP
client — asserted structurally in its own tests) and runs the consistency lens
in dry mode only (structurally incapable of writing findings). `review` consumes
cortex's risk output in gating; `scribe::graph::rules_store` receives
crystallized rules.

## Configuration

Atlas store location comes from `scribe`'s `SCRIBE_KG_STORE_DIR` (cortex has no
separate store). Calibration output defaults to `docs/cortex-calibration.md`.

## Notes and gaps

The full scoring rubric and per-signal breakdown are documented in
[docs/cortex-elegance-gate.md](../cortex-elegance-gate.md) and
[docs/tools/code-git/cortex.md](../tools/code-git/cortex.md); calibration results
in [docs/cortex-calibration.md](../cortex-calibration.md). This page does not
cover the consistency-lens internals (they live in `review::consistency`).
