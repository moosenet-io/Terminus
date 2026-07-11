# MINT — Fleet coverage matrix & results

The results companion to the [Model Fleet Catalog](#how-to-read-it--query-live). This page
describes how MINT reports **what has and has not been tested per model** (the coverage
matrix) and **how reliably each tested cell scored** (the leaderboard), after the S112
MINT-v2 measurement corrections. It documents the *format, columns, statuses, and how the
numbers are generated* — the live numbers themselves come from the catalog and its tool, not
from this file. Any table shown here is labelled **illustrative** and is not live data.

Data source throughout: the shared `lumina_intake` Postgres, reached only through
`config::intake_database_url()` (resolves `INTAKE_DATABASE_URL`, falling back to
`DATABASE_URL`) — never a literal DSN. Builder: `src/intake/catalog.rs`. Reliability
aggregates: `src/intake/aggregate.rs` → `code_run_aggregates`. Epoch definition:
`src/intake/mod.rs` (`CURRENT_EPOCH`).

## Current epoch

The coder build-scenario lineage is at epoch **`v3`** — the measurement-corrected harness.
`harness_version` is the partition key: `v3` is the current epoch, and the prior `v1`
(one-shot) and `v2` (build-scenario, pre-correction) rows are legacy. `CURRENT_EPOCH` is
declared in exactly one place (`src/intake/mod.rs`); a future `v4` is a one-line bump. The
`intake_epoch_marker` table (columns `epoch`, `became_current_at`, `note`) records **when**
each epoch became current — `became_current_at` is stamped once, on first cutover, and is the
audit timestamp for "since when are the current numbers current."

The **assistant sweep is a separate lineage** with its own version string,
`assistant::schema::HARNESS_VERSION` = **`s84-asmt-01`**. It is *not* governed by `v3`; its
report scopes to its own lineage (`src/intake/assistant/reporting.rs`). Do not compare or
merge coder-`v3` and assistant-`s84-asmt-01` version strings — they partition independent
result families.

## Coverage matrix

The matrix is **models × test-types**, one **coverage cell** per
`(model × test_type × task_category)`. The whole point is that gaps are shown explicitly:
an untested combination is a first-class `not_run` cell, never an omitted row. Every model in
the fleet nomination list is enumerated even if it has never been swept, so "which models
have no `multi_file` result" is answerable directly off the matrix.

Test types and their categories (`src/intake/catalog.rs`):

| `test_type` | `task_category` values | Source |
|---|---|---|
| `coder` | `blitz`, `multi_file`, `deep` | `code_run_aggregates` (per quant) |
| `assistant` | `conversation_depth`, `tool_chaining`, `memory_integration`, `personality_latent`, `personality_prompted`, `embeddings`, `yarn_context_depth` | assistant dimension scores |
| `serving` | `context_profile` | operational profile snapshot |
| `agent` | `tool_use` | agent tool-use rollup |

Each cell carries a **coverage status**:

| Status | Meaning |
|---|---|
| `run` | A current-epoch result is present. Carries `pass_rate` + `n_samples` + variance + `last_run_at` + `harness_version` (coder cells at `v3`). |
| `stale` | Only legacy-epoch (`v1`/`v2`) coder results exist; nothing current. History is real, the current gap is real. |
| `not_run` | No result of any kind, ever — an explicit coverage gap. Enumerated from the fleet nomination list. |
| `non_viable` | A recorded `non_viable_vram` skip (model exceeded the VRAM ceiling in pre-flight). Read on its own axis from the `code_profile_runs.failure_class` rows, not inferred from aggregates. |

Coder-cell status precedence (decided in `build_catalog`, the one pure place the logic
lives): a current-epoch aggregate ⇒ `run`; else a recorded `non_viable_vram` skip ⇒
`non_viable`; else a legacy-epoch aggregate ⇒ `stale`; else `not_run`. A current-epoch pass
wins over a `non_viable` skip (the model did run on some backend). Assistant / serving / agent
cells are `run` when a measured row exists and `not_run` otherwise — those lineages are not
epoch-partitioned in the catalog, so they carry no `stale`.

Illustrative matrix (**illustrative, not live data** — coder cells shown; `·` = the cell
exists as `not_run`):

| model | blitz | multi_file | deep |
|---|---|---|---|
| qwen3-coder:30b (Q4_K_M) | run | run | run |
| qwen3:8b (Q4_K_M) | run | run | stale |
| some-base-model:7b (Q4_K_M) | run | not_run · | not_run · |
| oversized:120b (Q8_0) | non_viable | non_viable | non_viable |
| newly-nominated:14b | not_run · | not_run · | not_run · |

The gap cells (`not_run`, `non_viable`) are present rows, not blanks — the reader sees the
untested surface, not just the tested one.

## Leaderboard / reliability

The leaderboard reports, per `(model, task_category)` at the current epoch, the **reliability**
of a cell — not a best pick. The metrics come from `code_run_aggregates` (`src/intake/aggregate.rs`):

- **`pass_rate`** — the fraction of samples that **passed**, where "pass" is effective score
  **≥ 4** (`PASS_THRESHOLD` = 4: compiles + tests + change-correct) on the 0–5 scale. This is
  a *rate across samples*, **not best-of-N**: a model that solves a case 2 of 7 times is not
  the same as one that solves it 7 of 7, and both are distinguished here rather than both
  being called "solved."
- **`n_samples`** — how many samples the rate is over. A cell with `n_samples ≤ 1` is flagged
  `low_confidence` (`LOW_CONFIDENCE_MAX_SAMPLES` = 1): a single sample cannot express variance,
  so its 0.0/1.0 rate must not be read as reliable.
- **`score_stddev`** — the population standard deviation of the effective score across
  samples: the reliability-variance signal Böckeler found most important, which the old
  best-of-N dedup deleted.

`code_run_aggregates` is **keyed by the config factors**, not just the model name:

    (model, task_category, harness_version, quant, reasoning_enabled,
     context_window_launched, temperature, top_p)

so two quants — or two reasoning settings — of one model are *never* blended into one
misleading rate. This is the catalog's reliability source, read cheaply per cell.

A **separate** object, the pre-existing `model_language_stats` materialized view
(`src/intake/assistant/schema.rs`), is a distinct leaderboard: it rolls `sample_index` repeats
up per `(profile, language)` and is keyed by **neither** `task_category`, `harness_version`,
**nor** the config factors. It is a best-of-N-style point rollup, not the variance-aware
reliability source. Read `code_run_aggregates` for reliability; treat `model_language_stats`
as the older language-leaderboard view, not as current-epoch reliability.

Illustrative leaderboard (**illustrative, not live data**):

| model | quant | category | pass_rate | n_samples | score_stddev | note |
|---|---|---|---|---|---|---|
| qwen3-coder:30b | Q4_K_M | multi_file | 0.71 | 7 | 0.49 | |
| qwen3-coder:30b | Q4_K_M | blitz | 1.00 | 7 | 0.00 | |
| qwen3:8b | Q4_K_M | multi_file | 0.29 | 7 | 0.76 | high variance |
| some-base-model:7b | Q4_K_M | deep | 0.00 | 3 | 0.00 | zero passes ≠ not-run |
| tiny:1b | Q4_K_M | blitz | 1.00 | 1 | 0.00 | low_confidence (n=1) |

Note the distinctions the format preserves: `pass_rate = 0.0` with `n_samples = 3` (ran, never
passed) is not the same as a `not_run` cell (no row at all); and `n=1` cells are flagged
`low_confidence` rather than presented as reliable.

## Legacy note

Nothing is deleted. The old best-of-N numbers were computed under the `v1`/`v2` harness epochs
and measured differently (best-batch dedup over `sample_index`, which discarded the variance
signal, and no recorded quant/reasoning/context/sampling factors). Those rows are **preserved**
in `code_profile_runs` and are **partitioned out** of current-epoch numbers by
`harness_version`: every current-epoch read defaults to `EpochSelector::Current` (only `v3`),
so legacy rows never pollute current pass-rates, aggregates, or the catalog. They remain
queryable for provenance via an explicit selector (`EpochSelector::Only("v2")` /
`EpochSelector::All`) but are surfaced as `stale` in the matrix, never as current results. The
old and new numbers are **not comparable**: they are different measurements (best-of-N vs
pass-rate/variance) taken under a different harness, so a `v2` "best" and a `v3` `pass_rate`
must not be placed in the same column. Re-running an evolved cell under `v3` is how a `stale`
cell becomes `run`.

## How to read it / query live

The live, at-a-glance source is the `model_fleet_catalog` core Terminus tool (MINT2-08,
Chord-served on the core registry alongside `plane`/`gitea`), read-only. It reads the persisted
catalog — no SQL needed. Input filters are all optional:

    { "model": "string?",              // one model's card
      "status": "run|stale|not_run|non_viable?",  // e.g. list gaps
      "test_type": "coder|assistant|serving|agent?",
      "format": "json|markdown?" }     // json (default) or a compact matrix

Output (JSON) is the current-epoch fleet card: `epoch`, `refreshed_at`, and per model a
`cells[]` array (each with `test_type`, `task_category`, `status`, and — for `run`/`stale` —
`pass_rate`, `n_samples`, `score_stddev`, `last_run_at`, `harness_version`) plus a
`not_run_count` / `stale_count` gap summary, so "what's missing" is one field away.

Example calls:

- **Full fleet card:** call with no arguments → every fleet model, all cells, current epoch.
- **List the gaps:** `{ "status": "not_run" }` → only the cells never tested (the coverage
  holes); `{ "status": "stale" }` → cells with only legacy results, due a re-run.
- **One model:** `{ "model": "qwen3-coder:30b" }` → that model's card only.
- **Human-readable matrix:** `{ "format": "markdown" }` → the coverage matrix rendered as a
  table for direct display.

Every fleet model appears exactly once; a model with no results has all cells `not_run`; an
empty catalog (fresh `v3` cutover before any sweep) returns all fleet models as `not_run`, not
an error. To regenerate the persisted catalog, `refresh_fleet_catalog` (`src/intake/catalog.rs`)
runs at the end of each unified harness run and on demand.
