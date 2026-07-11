-- S112 MINT2-03: variance-aware run aggregates — stop best-of-N dedup deleting
-- the reliability signal.
--
-- The pre-existing `model_language_stats` matview (assistant/schema.rs) collapses
-- the multi-sample (`sample_index`) repeats of a case into per-(profile, language)
-- point aggregates and is keyed by NEITHER `task_category`, `harness_version`, nor
-- the MINT2-01 config factors (quant/reasoning/context/temperature/top_p). That
-- blends two quants (or two reasoning settings) of ONE model into a single
-- misleading number and reports best-of-N-style rollups rather than the
-- reliability VARIANCE across samples that matters most for tuning.
--
-- This table persists, per (model, task_category, harness_version, AND the five
-- MINT2-01 config factors), the pass_rate (fraction of samples whose effective
-- score >= 4), n_samples, a variance measure (population stddev of the effective
-- score), the raw passes count, and a low_confidence flag (n_samples <= 1). The
-- catalog (MINT2-07) reads this cheaply instead of recomputing.
--
-- Applied OUT-OF-BAND by an operator (item MINT2-00), NOT by the harness code:
-- `src/intake/storage.rs` is authoritative that the harness only INSERTs and
-- SELECTs, never issuing DDL. The read path (`read_code_run_aggregates`) tolerates
-- this TABLE being ABSENT on an un-migrated DB (a missing-relation error reads as
-- an empty aggregate set, never a panic), mirroring the MINT2-01/02 null-tolerant
-- read pattern; the persist path is only invoked once the table exists.
--
-- Additive, idempotent, non-destructive: `CREATE TABLE IF NOT EXISTS` +
-- `CREATE INDEX IF NOT EXISTS`, so re-applying is a safe no-op. No backfill —
-- aggregates are (re)computed cheaply from `code_profile_runs` and re-persisted
-- wholesale per epoch by the harness.
--
-- EPOCH SCOPING: `harness_version` is the epoch partition key. The harness only
-- ever computes/persists rows for the CURRENT build-scenario epoch ('v3'); legacy
-- 'v1'/'v2' rows are excluded from the aggregate at compute time and never blended
-- in (MINT2-05 formalizes the epoch concept).

CREATE TABLE IF NOT EXISTS code_run_aggregates (
    model                   TEXT             NOT NULL,
    task_category           TEXT,
    harness_version         TEXT             NOT NULL,
    -- MINT2-01 config factors — part of the grouping key so two quants /
    -- reasoning settings of one model are NOT blended into one rate. Nullable:
    -- an unset factor is its own bucket (a NULL bucket is distinct from any real
    -- value, matched in the persist upsert via a NULL-safe comparison).
    quant                   TEXT,
    reasoning_enabled       BOOLEAN,
    context_window_launched INTEGER,
    temperature             DOUBLE PRECISION,
    top_p                   DOUBLE PRECISION,
    -- variance-aware measures
    pass_rate               DOUBLE PRECISION NOT NULL,
    n_samples               INTEGER          NOT NULL,
    passes                  INTEGER          NOT NULL,
    score_stddev            DOUBLE PRECISION NOT NULL,
    low_confidence          BOOLEAN          NOT NULL,
    updated_at              TIMESTAMPTZ      NOT NULL DEFAULT now()
);

-- Fast lookup by the natural reporting axes (catalog reads by model / epoch).
CREATE INDEX IF NOT EXISTS idx_code_run_aggregates_model_epoch
    ON code_run_aggregates (model, harness_version, task_category);
