-- S112 MINT2-01: record the tunable measurement factors on each coder-sweep case.
--
-- Adds first-class, queryable columns to `code_profile_runs` so pass-rate can be
-- analyzed AGAINST THE KNOB THAT WAS SET (quant, reasoning, launched context
-- window, sampling, task category) rather than only against the model name.
--
-- Applied OUT-OF-BAND by an operator (item MINT2-00), NOT by the harness code:
-- `src/intake/storage.rs` is authoritative that the harness only INSERTs and
-- SELECTs `code_profile_runs`, never issuing DDL against it. The harness code is
-- written to tolerate a DB where these columns do not yet exist (the read path
-- falls back to a NULL-typed query, so a missing column reads as NULL and never
-- panics) — so this migration can be applied any time after the MINT2-01 PR
-- merges; until it runs, new `'v3'` rows simply persist without these factors.
--
-- Additive and non-destructive: every column is nullable, there is NO backfill.
-- Legacy `'v1'`/`'v2'` rows keep NULL for all six columns — they belong to a
-- prior harness epoch, and reporting (MINT2-03/05) must not treat a NULL quant
-- as a distinct real quant bucket. `IF NOT EXISTS` makes each statement
-- idempotent so re-applying the migration is a safe no-op.

ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS quant TEXT;
ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS reasoning_enabled BOOLEAN;
ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS context_window_launched INTEGER;
ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS temperature DOUBLE PRECISION;
ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS top_p DOUBLE PRECISION;
-- task_category: the corpus-manifest tier promoted to a stored, recorded factor
-- (blitz | multi_file | deep). Recorded from the manifest, never re-derived from
-- file_count on the write path.
ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS task_category TEXT;
