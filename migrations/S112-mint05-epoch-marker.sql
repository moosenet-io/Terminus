-- S112 MINT2-05: harness-version EPOCHS — persist when each epoch became current.
--
-- `harness_version` is the epoch partition key for the build-scenario coder rows
-- (`code_profile_runs`) and their derived aggregates (`code_run_aggregates`).
-- When a test evolves (Phase 1 changes what/how we measure) the epoch is bumped
-- (`'v1'` → `'v2'` → `'v3'` …): old rows are never DELETED (provenance) but never
-- blend into the current epoch's tuning numbers — current-epoch reads scope to
-- the one central `CURRENT_EPOCH` (`src/intake/mod.rs`) by default, and legacy
-- epochs stay queryable only via an explicit selector.
--
-- This table records the AUDIT TIMELINE of that partitioning: one row per epoch
-- string, with the timestamp it became current and an optional note. It does NOT
-- gate reads (the partition is the `harness_version` value on each data row); it
-- exists so "when did `'v3'` become the current epoch, and why" is answerable
-- from the data rather than only from git history.
--
-- Applied OUT-OF-BAND by an operator (item MINT2-00), NOT by the harness code:
-- `src/intake/storage.rs` is authoritative that the harness only INSERTs and
-- SELECTs, never issuing DDL. The read path (`read_epoch_marker`) tolerates this
-- TABLE being ABSENT on an un-migrated DB (a missing-relation error reads as
-- "no marker", never a panic), mirroring the MINT2-03 `read_code_run_aggregates`
-- null/absence-tolerant pattern. The upsert path is only invoked once the table
-- exists.
--
-- Additive, idempotent, non-destructive: `CREATE TABLE IF NOT EXISTS`, and the
-- marker write is an idempotent upsert keyed on the epoch PRIMARY KEY (see
-- `upsert_epoch_marker`), so re-applying the migration OR re-recording an epoch
-- is a safe no-op. No backfill — a marker for a given epoch appears the first
-- time that epoch is recorded as current.

CREATE TABLE IF NOT EXISTS intake_epoch_marker (
    -- The epoch string, e.g. 'v3' — matches the `harness_version` value the
    -- build-scenario rows carry. PRIMARY KEY so the marker upsert is idempotent.
    epoch             TEXT        PRIMARY KEY,
    -- When this epoch became the current partition (audit timeline).
    became_current_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Optional human note (why the epoch was bumped / what evolved).
    note              TEXT
);
