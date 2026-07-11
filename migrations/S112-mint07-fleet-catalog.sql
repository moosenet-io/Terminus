-- S112 MINT2-07: the Model Fleet Catalog — a derived, refreshable, per-model
-- coverage registry an agent reads to know the fleet WITHOUT running SQL.
--
-- WHY THIS EXISTS: results live scattered across `code_run_aggregates` (MINT2-03,
-- the coder reliability source), `assistant_dimension_score` (the seven-dimension
-- assistant lineage), `model_operational_profiles` (serving/context facts) and
-- `agent_profile_runs` (tool-use). No single object answers "for THIS model, what
-- has and has NOT been tested, and how did it score?". This catalog is that object:
-- a LONG-format cell table (`model_fleet_catalog_cell`) — one row per
-- (model × test_type × task_category) with an explicit COVERAGE STATUS — plus a
-- thin per-model summary (`model_fleet_catalog`) carrying serving facts and the
-- gap counts. The whole point is representing GAPS: an un-tested cell is a
-- FIRST-CLASS `not_run` row, never an omission, so "which models have no
-- multi_file result" is a one-line query, not an inference from absent rows.
--
-- COVERAGE STATUS (per cell), computed by `src/intake/catalog.rs`:
--   run        — a CURRENT-epoch result is present (coder: a `code_run_aggregates`
--                cell for the current `harness_version`; assistant/serving/agent:
--                a measured row exists). Carries pass_rate + n_samples + variance
--                + last_run_at + harness_version.
--   stale      — ONLY legacy-epoch ('v1'/'v2') coder results exist; nothing in the
--                current epoch. The gap is real even though history exists.
--   non_viable — a `code_profile_runs.failure_class = 'non_viable_vram'` skip row
--                was recorded (the model was skipped pre-flight as over-VRAM).
--                Read on its OWN axis from the failure rows — NOT inferred from
--                aggregate cells, which EXCLUDE skips (MINT2-03) and so have none.
--   not_run    — no result of any kind, ever. The core deliverable: coverage gaps
--                are enumerated from the fleet nomination list, so a fleet model
--                that was never swept appears with every cell `not_run`.
--
-- Applied OUT-OF-BAND by an operator (item MINT2-00), NOT by the harness code:
-- `src/intake/storage.rs` is authoritative that the harness only INSERTs and
-- SELECTs, never issuing DDL. The read/build path tolerates EVERY upstream source
-- table being ABSENT on an un-migrated host (a missing relation reads as "no
-- rows", so those cells become `not_run` rather than crashing), and the catalog's
-- OWN read tolerates THESE tables being absent too — mirroring the MINT2-03/05
-- null/absence-tolerant pattern. The persist path is only invoked once the tables
-- exist.
--
-- Additive, idempotent, non-destructive: `CREATE TABLE IF NOT EXISTS` +
-- `CREATE INDEX IF NOT EXISTS`, so re-applying is a safe no-op. No backfill — the
-- catalog is (re)derived cheaply from the upstream tables and re-persisted
-- wholesale by the refresh at the end of each unified harness run (and on demand).

-- Per-model summary card: one row per (model_name, quant). `quant` is the model's
-- representative quant (from its coder aggregates, else NULL when never swept); the
-- authoritative per-config detail lives in the cell table below. Serving facts are
-- stashed as JSONB so the summary stays a single "fleet card" object without a
-- migration every time a new serving fact is surfaced. `not_run_count`/`stale_count`
-- put "what's missing" one field away.
CREATE TABLE IF NOT EXISTS model_fleet_catalog (
    model_name        TEXT        NOT NULL,
    -- Representative quant for the card (cells carry their own per-config quant).
    -- NULL for a fleet model that was never swept (no measured quant to report).
    quant             TEXT,
    -- FALSE ⇒ the model has results but is no longer in the current nomination
    -- fleet (surfaced as historical rather than dropped — see catalog.rs).
    in_current_fleet  BOOLEAN     NOT NULL DEFAULT TRUE,
    -- Serving/operational facts (max_context_safe, quality_degradation_point,
    -- throughput, agent_tool_accuracy, vram_gb, …) as a flexible JSON object.
    serving_json      JSONB,
    -- Gap summary across this model's cells (denormalized for a cheap read).
    not_run_count     INTEGER     NOT NULL DEFAULT 0,
    stale_count       INTEGER     NOT NULL DEFAULT 0,
    -- When this card was last (re)derived.
    refreshed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (model_name, quant)
);

-- LONG-format coverage cells: the queryable core. One row per
-- (model_name, quant, test_type, task_category). `status` is the coverage status
-- above. Metric columns are NULLABLE — only a `run`/`stale` cell carries a
-- pass_rate/variance; a `not_run`/`non_viable` cell leaves them NULL (distinct
-- from a measured 0.0). This shape lets an agent ask, e.g.,
--   SELECT model_name FROM model_fleet_catalog_cell
--    WHERE test_type='coder' AND task_category='multi_file' AND status<>'run';
-- to enumerate exactly the models missing a current multi_file result.
CREATE TABLE IF NOT EXISTS model_fleet_catalog_cell (
    model_name      TEXT        NOT NULL,
    -- Per-config quant this cell was measured at (from the coder aggregate), or
    -- NULL for a not_run/assistant/serving cell with no single measured quant.
    quant           TEXT,
    -- The test family axis: 'coder' | 'assistant' | 'serving' | 'agent'.
    test_type       TEXT        NOT NULL,
    -- The leaf category within the family, e.g. coder 'blitz'/'multi_file'/'deep',
    -- an assistant dimension ('conversation_depth', …), 'context_profile',
    -- 'tool_use'.
    task_category   TEXT        NOT NULL,
    -- 'run' | 'stale' | 'not_run' | 'non_viable'.
    status          TEXT        NOT NULL,
    -- run/stale metrics (NULL otherwise). pass_rate is the coder reliability rate
    -- (fraction of samples with effective score >= 4); NULL for assistant cells
    -- whose value scale is not a pass/fail rate.
    pass_rate       DOUBLE PRECISION,
    n_samples       INTEGER,
    score_stddev    DOUBLE PRECISION,
    low_confidence  BOOLEAN,
    -- When the underlying result was last measured (max run timestamp), and the
    -- epoch/harness version it was measured under.
    last_run_at     TIMESTAMPTZ,
    harness_version TEXT,
    refreshed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (model_name, quant, test_type, task_category)
);

-- Fast lookup by the natural query axes: by model (the fleet card) and by a
-- coverage gap query (status + test_type + category).
CREATE INDEX IF NOT EXISTS idx_fleet_catalog_cell_model
    ON model_fleet_catalog_cell (model_name);
CREATE INDEX IF NOT EXISTS idx_fleet_catalog_cell_gap
    ON model_fleet_catalog_cell (test_type, task_category, status);
