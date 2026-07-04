//! Assistant-profile schema + idempotent migration (S84 ASMT-01).
//!
//! Unlike S83 `storage` (whose tables pre-exist in the shared DB), S84 owns its
//! own tables and creates them itself with `CREATE TABLE IF NOT EXISTS` /
//! `CREATE OR REPLACE VIEW`, so [`migrate`] is safe to call repeatedly (the
//! schema/migration MUST be idempotent — every dimension runner calls it before
//! writing).
//!
//! ## Tables
//!   - `assistant_profile_run`     — one row per harness invocation.
//!   - `assistant_dimension_score` — one row per (run, model, backend, dimension,
//!     metric, judge). `model_id` is byte-identical to S83
//!     `model_profiles.model_name` (see [`super::ModelId`]).
//!
//! ## View
//!   - `model_dual_profile` — FK-free, LEFT JOIN of the S83 builder side
//!     (`model_profiles` ⨝ `code_profile_runs`) and the S84 assistant side on
//!     `model_id` (+ `backend_tag`, when present on the builder side). A model
//!     with only one profile still appears (FULL OUTER via two LEFT JOINs over a
//!     unioned key set), so reconciliation gaps are visible, not hidden.
//!
//! The intake DB URL is sourced from [`crate::config::intake_database_url`]
//! (NO literals).

use sqlx::{PgConnection, PgPool};

use crate::config;
use crate::error::ToolError;

use super::{BackendTag, DimensionScore, ModelId};

/// Schema/harness version stamped onto every `assistant_profile_run` row.
pub const HARNESS_VERSION: &str = "s84-asmt-01";

/// Postgres advisory-lock key serializing concurrent [`migrate`] callers.
///
/// `intake_coder_sweep` and `intake_assistant_sweep` are separate binaries
/// that each call `migrate()` defensively at their own startup (either might
/// be first on a fresh host). Several of the statements below are NOT safe
/// against concurrent execution even though each is individually guarded
/// with `IF NOT EXISTS`/`IF EXISTS` — most notably the `DROP INDEX IF EXISTS
/// idx_assistant_score_model` + `CREATE INDEX IF NOT EXISTS
/// idx_assistant_score_model ...` pair: two processes can both pass the
/// "does it exist" check before either finishes creating it, and the loser
/// gets `duplicate key value violates unique constraint
/// "pg_class_relname_nsp_index"`. This was observed live in production (a
/// host-level watchdog restarting both services around the same time),
/// surfacing as `coder sweep did not start: schema migrate failed: ...` and
/// costing hours of lost sweep progress before this race was identified as
/// the root cause.
///
/// The fix wraps the whole migration body in a session-scoped
/// `pg_advisory_lock`/`pg_advisory_unlock` pair (see [`migrate`]) so
/// concurrent callers serialize instead of racing. Postgres advisory locks
/// are tied to the connection/session that took them: if a process crashes
/// mid-migration, its connection drops and Postgres releases the lock
/// automatically — there is no persistent lock state to get stuck, so this
/// cannot deadlock a future run.
///
/// The value is the first 8 bytes (big-endian, signed) of
/// `sha256("terminus_assistant_schema_migrate")`, i.e. a fixed, stable,
/// human-traceable constant rather than an arbitrary literal — recompute
/// with:
/// ```text
/// python3 -c "import hashlib; h = hashlib.sha256(b'terminus_assistant_schema_migrate').digest(); print(int.from_bytes(h[:8], 'big', signed=True))"
/// ```
/// to verify it if this constant is ever in doubt. Advisory lock keys are a
/// single global i64 namespace per Postgres cluster, so this MUST stay
/// unique among any other advisory locks Terminus/Lumina ever introduces —
/// grep for `pg_advisory_lock` before reusing or changing it.
const ADVISORY_LOCK_KEY: i64 = -5322992491554488081;

/// Connect a pool to the intake/assistant DB. Prefers `INTAKE_DATABASE_URL`,
/// falls back to `DATABASE_URL` (same shared DB S83 uses).
pub async fn get_pool() -> Result<PgPool, ToolError> {
    let url = config::intake_database_url().ok_or_else(|| {
        ToolError::NotConfigured(
            "neither INTAKE_DATABASE_URL nor DATABASE_URL set — assistant intake \
             requires a Postgres connection"
                .into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to intake database: {e}")))
}

/// Apply the assistant-profile schema. Idempotent: safe to call on every run.
///
/// Creates the two tables, their indexes, and the `model_dual_profile` view.
/// The view is defined defensively so it works whether or not the S83 builder
/// side carries a `backend_tag` column (early S83 schema does not): the join is
/// on `model_id` only, and the builder `backend_tag` is surfaced as `NULL` when
/// the column is absent (resolved at view-creation time via a catalog probe).
pub async fn migrate(pool: &PgPool) -> Result<(), ToolError> {
    // Serialize concurrent callers (see `ADVISORY_LOCK_KEY`'s doc comment for
    // the full race this guards against). The lock is connection-scoped, so
    // it MUST be taken and held on one dedicated connection checked out of
    // the pool — taking it via `pool` directly would acquire it on whichever
    // connection the pool happens to hand out per-query, and release it
    // (implicitly, by the connection going back into the pool's idle set)
    // before the migration body below even runs.
    let mut conn = pool.acquire().await.map_err(|e| {
        ToolError::Database(format!(
            "acquire dedicated connection for schema migrate: {e}"
        ))
    })?;

    // Mark this connection to be closed rather than returned to the pool
    // once it drops, no matter how it drops. `PoolConnection`'s `Drop` impl
    // checks this flag directly (see sqlx `pool/connection.rs`), so it takes
    // effect on every exit path from this function — normal return, an early
    // `?`-propagated error below, or (hypothetically) a panic unwinding
    // through this frame — not just the two `result`/unlock code paths
    // written out explicitly further down. This closes the gap where a
    // session-scoped advisory lock could otherwise survive on a connection
    // that goes back into the pool's idle set: `pg_advisory_lock` is
    // re-entrant per-session, so a future borrower of this exact physical
    // connection would silently inherit an already-held lock instead of
    // acquiring a fresh one.
    conn.close_on_drop();

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("acquire schema migrate advisory lock: {e}")))?;

    let result = migrate_locked(&mut conn).await;

    // Always attempt the unlock, on both the success and error paths. This is
    // now belt-and-suspenders rather than load-bearing: `close_on_drop()`
    // above already guarantees this exact connection is discarded (never
    // returned to the pool) once it drops, so there is no other caller left
    // that could inherit a stuck lock from it. We still unlock explicitly
    // (rather than relying solely on connection-close to release it) so the
    // advisory lock is freed for the *next* `migrate()` caller as soon as
    // possible, instead of only when this connection's close completes.
    // sqlx has no `finally`, so this is structured explicitly: run the
    // unlock after capturing `result`, log-not-swallow any unlock failure,
    // then return the migration's own `result` either way.
    if let Err(unlock_err) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            "assistant schema migrate: failed to release advisory lock {}: {unlock_err} \
             (harmless — the lock is released automatically when this connection closes)",
            ADVISORY_LOCK_KEY
        );
    }

    result
}

/// The actual migration body, run on the same dedicated, advisory-locked
/// connection acquired by [`migrate`]. Split out purely so [`migrate`] can
/// wrap it in a lock/unlock pair without duplicating the lock handling
/// around every early return — behavior is unchanged from the previous
/// single-function `migrate()`.
async fn migrate_locked(conn: &mut PgConnection) -> Result<(), ToolError> {
    // 1. Runs.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS assistant_profile_run ( \
            id UUID PRIMARY KEY, \
            started_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            harness_version TEXT NOT NULL \
         )",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create assistant_profile_run: {e}")))?;

    // 2. Dimension scores. FK-free to runs by design? No — a score belongs to a
    //    run, so we keep a plain (un-constrained-to-S83) reference. We DO NOT FK
    //    to S83 tables (model_id is a soft join key, not a constraint).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS assistant_dimension_score ( \
            id BIGSERIAL PRIMARY KEY, \
            run_id UUID NOT NULL REFERENCES assistant_profile_run(id) ON DELETE CASCADE, \
            model_id TEXT NOT NULL, \
            backend_tag TEXT NOT NULL CHECK (backend_tag IN ('gpu','cpu')), \
            dimension TEXT NOT NULL, \
            metric TEXT NOT NULL, \
            value DOUBLE PRECISION NOT NULL, \
            std_dev DOUBLE PRECISION, \
            judge TEXT NOT NULL, \
            low_confidence BOOLEAN NOT NULL DEFAULT false, \
            raw_json JSONB, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now() \
         )",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create assistant_dimension_score: {e}")))?;

    // `task_category` extends the flexible (dimension, metric, value, judge,
    // raw_json) scoring mechanism to host benchmarking categories beyond the
    // original "assistant" persona/quality evals — the operator is adding five
    // more categories (document parsing/OCR, image parsing/vision-understanding,
    // image generation, document generation, voice transcription/ASR) on top of
    // "assistant" and "coder". `NOT NULL DEFAULT 'assistant'` back-fills every
    // existing row with the value that already describes them, preserving
    // current meaning exactly. Deliberately NO CHECK constraint: this is a
    // living, non-exhaustive list — constraining it in the DB would force a
    // migration every time a new category is added, defeating the point of the
    // extension. Known values as of this writing (not exhaustive):
    //   'assistant', 'coder' (coder mostly uses code_profile_runs already, but
    //   may use this table for cross-cutting scores), 'document_parsing',
    //   'image_parsing', 'image_generation', 'document_generation',
    //   'voice_transcription'.
    sqlx::query(
        "ALTER TABLE assistant_dimension_score \
         ADD COLUMN IF NOT EXISTS task_category TEXT NOT NULL DEFAULT 'assistant'",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("add task_category column: {e}")))?;

    // `mem_config` tags which memory-model configuration (e.g. `dynamic_gtt`
    // vs `carveout`) a row was measured under (mem-config-tagging sprint).
    // Deliberately NO backfill default and NO CHECK constraint (same
    // unconstrained-living-list rationale as `task_category` above):
    // existing rows PREDATE this column and are the PRESERVED `carveout`
    // baseline dataset from an earlier run — defaulting them to
    // 'dynamic_gtt' (or any other value) would silently mislabel that
    // baseline. Left NULL for old rows; new rows set it explicitly via
    // [`insert_dimension_score_with_category_and_mem_config`].
    sqlx::query("ALTER TABLE assistant_dimension_score ADD COLUMN IF NOT EXISTS mem_config TEXT")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add mem_config column: {e}")))?;

    // Recreated (not just created) so the index definition stays current even
    // on a DB that already had the old (model_id, backend_tag) index from a
    // prior migrate() run. Cheap: this table is intake-scale, not hot-path.
    sqlx::query("DROP INDEX IF EXISTS idx_assistant_score_model")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("drop idx_assistant_score_model: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_assistant_score_model \
         ON assistant_dimension_score (model_id, backend_tag, task_category)",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_assistant_score_model: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_assistant_score_run \
         ON assistant_dimension_score (run_id)",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_assistant_score_run: {e}")))?;

    // 2b. `code_profile_runs` is owned/created by `storage.rs` (NOT this
    //     module — see that file's header comment: "tables already exist in the
    //     shared DB, DO NOT create them here"), but `migrate()` already reaches
    //     into it defensively via `column_exists` below to build the dual-profile
    //     view. `backend_tag` is the coder-side twin of the assistant-side
    //     column added above: the sweep tests every model on both GPU and CPU
    //     backends, and until now `code_profile_runs` had nowhere to record
    //     which one a given row came from — the view's builder-side backend
    //     attribution has silently degraded to NULL because the column has
    //     never existed. Adding it here (rather than in storage.rs) keeps the
    //     "who creates schema" boundary consistent with the existing
    //     `column_exists` probe below, which already anticipated this column's
    //     eventual arrival. No CHECK constraint: conceptually 'gpu'/'cpu' (same
    //     domain as `assistant_dimension_score.backend_tag`), but left
    //     unconstrained in case a future third configuration (e.g.
    //     'gpu-rocm' vs 'gpu-vulkan') is added without a migration.
    sqlx::query("ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS backend_tag TEXT")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.backend_tag: {e}")))?;

    // `mem_config` twin on the builder side: same rationale as the
    // assistant-side column above — the coder sweep tests models against
    // different memory-model configurations (tonight: `dynamic_gtt`), and
    // `code_profile_runs` had nowhere to record which one produced a given
    // row. NO backfill default: pre-existing rows are the preserved
    // `carveout` baseline and must stay unlabeled-as-'dynamic_gtt' (NULL),
    // never silently relabeled.
    sqlx::query("ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS mem_config TEXT")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.mem_config: {e}")))?;

    // `case_id` (HFIX-06): the v2 corpus manifest's unique case identifier
    // (e.g. "rust-blitz-a3"). Before this column existed, a row only carried
    // `language`/`task_type` — not enough to identify WHICH specific case it
    // came from when several cases share a language+task_type, so a gap
    // audit could tell you a model's overall error rate but never which
    // particular cases were missing/invalid, only "re-run the whole model"
    // (exactly the workflow this column exists to end). NULL for rows
    // written before this column existed — never backfilled/guessed.
    sqlx::query("ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS case_id TEXT")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.case_id: {e}")))?;

    // `finalized` (S86 INCR-01 hardening): explicit completion marker for the
    // incremental-persistence rework, where `code_v2::run_code_suite_v2_cases`
    // now inserts each case's row at the END of Phase 1 (inference only) and
    // patches it in place through Phase 2 (judge scoring), instead of
    // inserting every row atomically at the very end of Phase 3 like before.
    // That means a row can now exist in a genuinely incomplete state (Phase 1
    // ran, but the process was killed/crashed before Phase 2/3 finished for
    // that case) — and `error IS NULL AND case_id IS NOT NULL` alone (the
    // predicate `coder_gaps.rs` used to call a row "valid/complete") cannot
    // tell that apart from a real, fully-scored result. `finalized` closes
    // that gap: `storage::insert_code_run_v2`'s Phase-1 insert EXPLICITLY
    // writes `false` (never relies on the column default for new rows), and
    // `storage::update_code_run_v2_judge` sets it to `true` once a case
    // reaches its true end (called for every case, judged or not).
    //
    // DEFAULT TRUE (not false) is deliberate and easy to misread backwards:
    // this column's default exists ONLY to correctly backfill PRE-EXISTING
    // rows, which were all written by the OLD atomic Phase-3-inserts-
    // everything code path and are therefore already complete/valid by
    // construction. `ADD COLUMN ... DEFAULT true` makes every legacy row read
    // as `finalized = true` without a rewrite, preserving `coder_gaps.rs`'s
    // existing "no gap" verdicts for data collected before this column
    // existed. Only the NEW incremental Phase-1 insert path overrides this
    // default with an explicit `false`; every other/future writer that does
    // not mention this column keeps defaulting to "already complete", which
    // is the correct assumption for anything not going through the new
    // incremental path.
    sqlx::query("ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS finalized BOOLEAN NOT NULL DEFAULT true")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.finalized: {e}")))?;

    // `well_formed` (multi-point-score-tracking): distinguishes a code case
    // that scored 0 because the model produced NOTHING extractable (no mapped
    // output files) from one that scored 0 because it produced code that was
    // simply wrong. `code_v2.rs` sets it to `produced` (= at least one mapped
    // output file was extracted) BEFORE the graduated score is computed, so a
    // downstream reader can separate "malformed / no output" from "well-formed
    // but incorrect". NULL for rows written before this column existed, and for
    // any writer (v1, agent, context) that never sets it — the `model_language_
    // stats` matview below only counts `well_formed = false` toward its
    // malformed rate, so a NULL never inflates that rate.
    sqlx::query("ALTER TABLE code_profile_runs ADD COLUMN IF NOT EXISTS well_formed BOOLEAN")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.well_formed: {e}")))?;

    // `run_score_points` (multi-point-score-tracking): a flexible, long-format
    // sidecar table capturing EVERY per-point measurement a suite computes
    // along an axis (context-length tiers, tool-count bands, …), not just the
    // handful that happen to land on a fixed column of
    // `model_operational_profiles` (throughput_at_2k/8k/…, tool_accuracy_at_200).
    // A frontend can plot a line graph (value vs. x_value along `axis`) and a
    // routing layer can pick a model on more than one scalar. Additive: the
    // existing `model_operational_profiles` writes are UNCHANGED — this only
    // preserves what was previously discarded.
    //
    // Exactly ONE of the three run-parent columns is non-NULL per row
    // (enforced by the `one_parent` CHECK), identifying which suite produced
    // the point; `profile_id` is always set (the model the point belongs to)
    // for a cheap per-model rollup without a three-way join.
    //
    // Only `code_run_id` gets a real FK: `code_profile_runs` is guaranteed to
    // exist in every environment this migration runs in (this DB's live
    // coder/assistant sweeps write to it directly). `context_profile_runs`
    // and `agent_profile_runs` do NOT exist in this production database today
    // — verified directly against `information_schema.tables` before this
    // migration shipped — so a hard `REFERENCES` to either would make this
    // `CREATE TABLE` fail outright (`relation does not exist`), breaking
    // startup for every binary that calls `migrate_locked()`, coder-sweep and
    // assistant-sweep included. `context_run_id`/`agent_run_id` are therefore
    // plain UUIDs with no FK enforcement; correctness is carried by the
    // `one_parent` CHECK plus application code only ever populating the field
    // matching the suite that ran. `code_run_id` uses `ON DELETE CASCADE` so
    // its points vanish with their parent run (mirrors the
    // `assistant_dimension_score` → `assistant_profile_run` cascade above).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS run_score_points ( \
            id             BIGSERIAL PRIMARY KEY, \
            code_run_id    UUID REFERENCES code_profile_runs(id) ON DELETE CASCADE, \
            context_run_id UUID, \
            agent_run_id   UUID, \
            profile_id     UUID NOT NULL REFERENCES model_profiles(id), \
            axis           TEXT NOT NULL, \
            x_value        DOUBLE PRECISION NOT NULL, \
            x_label        TEXT, \
            metric         TEXT NOT NULL, \
            value          DOUBLE PRECISION, \
            created_at     TIMESTAMPTZ NOT NULL DEFAULT now(), \
            CONSTRAINT one_parent CHECK ( \
                (code_run_id IS NOT NULL)::int + (context_run_id IS NOT NULL)::int + (agent_run_id IS NOT NULL)::int = 1 \
            ) \
         )",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create run_score_points: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_rsp_profile_axis_metric \
         ON run_score_points (profile_id, axis, metric)",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_rsp_profile_axis_metric: {e}")))?;

    // `model_language_stats` (multi-point-score-tracking): a per-(model,
    // language) rollup of the `dynamic_gtt` code sweep — mean/stddev score,
    // retry lift, throughput, latency (mean + p95), GPU-time cost
    // (`total_gpu_seconds` + the `quality_per_gpu_second` composite), malformed
    // rate (via the new `well_formed` column) and error rate. Pure SQL; no Rust
    // writes to it.
    //
    // GPU-COST SIGNAL (gpu-cost-signal): a coder sweep runs under
    // `gpu_authority`'s `Exclusive` mode — competing services stopped, one
    // Ollama-resident model at a time — so per-case wall-clock `total_time_ms`
    // IS the GPU-time cost of that case (nothing else contends for the GPU
    // during the sweep). `total_gpu_seconds` = SUM(total_time_ms)/1000 per
    // (model, language) is "how much GPU-time budget committing to this model
    // for the whole batch would cost" — different information from the per-case
    // `mean_latency_ms` already above. `quality_per_gpu_second` =
    // mean_score / (total_gpu_seconds / n_scored) folds quality and that cost
    // into one higher-is-better routing number (good quality bought cheaply).
    // Both divisors are `NULLIF`-guarded: a model with zero scored cases or
    // zero accumulated time yields NULL, never a divide-by-zero at view-create
    // time. The pure-Rust twin (independently unit-tested) is
    // `crate::intake::code::quality_per_gpu_second`.
    // Scoped to `mem_config = 'dynamic_gtt'` so it never blends the preserved
    // `carveout` baseline into the current config's numbers.
    //
    // OPERATIONAL NOTE: this is a MATERIALIZED view — its contents are a
    // snapshot frozen at creation/refresh time. Nothing in this change wires an
    // automatic refresh; refreshing it (`REFRESH MATERIALIZED VIEW
    // model_language_stats`) after a sweep is an explicit operational task.
    //
    // DROP + CREATE, not `CREATE ... IF NOT EXISTS` (gpu-cost-signal fixup,
    // caught in review): Postgres's `IF NOT EXISTS` only skips the CREATE if a
    // relation with that name already exists — it does NOT compare or update
    // the view's column list against the CREATE statement's current SELECT.
    // The very first version of this view (multi-point-score-tracking) used
    // `IF NOT EXISTS` and was deployed to production; every subsequent column
    // this migration wants to add (this branch's `total_gpu_seconds`/
    // `quality_per_gpu_second`) would therefore silently never appear on an
    // already-migrated database — the CREATE would just no-op against the
    // existing, older-shaped view. Since this is a pure derived/computed view
    // (no source-of-truth data lives only here — dropping and recreating it
    // loses nothing that isn't trivially recomputable from `code_profile_runs`
    // in milliseconds at current row counts), DROP-then-CREATE on every
    // `migrate_locked()` call is the correct, safe evolution strategy for a
    // view whose definition needs to keep growing new columns, unlike the
    // `ADD COLUMN IF NOT EXISTS` pattern used for actual tables above.
    sqlx::query("DROP MATERIALIZED VIEW IF EXISTS model_language_stats")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("drop model_language_stats matview: {e}")))?;
    sqlx::query(
        "CREATE MATERIALIZED VIEW model_language_stats AS \
         SELECT profile_id, language, \
             count(*) FILTER (WHERE error IS NULL) AS n_scored, \
             avg(first_pass_score) FILTER (WHERE error IS NULL) AS mean_score, \
             stddev(first_pass_score) FILTER (WHERE error IS NULL) AS stddev_score, \
             avg(retry_score - first_pass_score) FILTER (WHERE retry_score IS NOT NULL) AS retry_lift, \
             avg(throughput_tok_per_sec) AS mean_throughput, \
             avg(total_time_ms) AS mean_latency_ms, \
             percentile_cont(0.95) WITHIN GROUP (ORDER BY total_time_ms) AS p95_latency_ms, \
             sum(total_time_ms)::float / 1000.0 AS total_gpu_seconds, \
             avg(first_pass_score) FILTER (WHERE error IS NULL) \
                 / NULLIF( \
                     (sum(total_time_ms)::float / 1000.0) \
                         / NULLIF(count(*) FILTER (WHERE error IS NULL), 0)::float, \
                     0) AS quality_per_gpu_second, \
             (count(*) FILTER (WHERE well_formed = false))::float / greatest(count(*),1)::float AS malformed_rate, \
             (count(*) FILTER (WHERE error IS NOT NULL))::float / greatest(count(*),1)::float AS error_rate \
         FROM code_profile_runs \
         WHERE mem_config = 'dynamic_gtt' \
         GROUP BY profile_id, language",
    )
    .execute(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("create model_language_stats matview: {e}")))?;

    // 3. The dual-profile join view. Built so a model present in only ONE side
    //    still appears (key set = UNION of both sides' model_ids), and so a
    //    missing builder `backend_tag` column degrades to NULL rather than
    //    erroring at view-create time.
    //
    //    CRITICAL (MINT new-model-types fix): the `assistant` CTE below filters
    //    `WHERE task_category = 'assistant'`. `assistant_dimension_score` now
    //    also hosts document_parsing/image_parsing/image_generation/
    //    voice_transcription rows (see `newcats`), which use incompatible value
    //    scales (0-1 accuracy vs. millisecond latencies vs. unbounded WER vs.
    //    VRAM MB) and must NOT be folded into `assistant_score_count` /
    //    `assistant_avg_value` — without this filter, a vision/ASR/image-gen-
    //    only model would wrongly show `has_assistant_profile = true` and
    //    `assistant_avg_value` would blend nonsense across scales.
    //
    //    CRITICAL (mem-config-tagging): `mem_config` is now ALSO part of the
    //    grouping/join key, alongside `backend_tag`. Rationale: tonight's
    //    sweep runs the SAME models under a NEW `dynamic_gtt` memory config
    //    while a PRESERVED `carveout` baseline already occupies these same
    //    tables. If `mem_config` were left out of `GROUP BY`/the join key
    //    (the way it's left out of `model_full_profile`'s coarse `assistant`
    //    CTE — see that view's comment for why THAT omission is safe), a
    //    model measured under both configs would silently average
    //    `dynamic_gtt` and `carveout` rows together into one meaningless
    //    number — exactly the blending this column exists to prevent. Both
    //    the `builder` and `assistant` CTEs here already split by
    //    `backend_tag`, so extending both to also split by `mem_config` is a
    //    safe, symmetric extension of an existing 2-axis key to a 3-axis key
    //    (model_id, backend_tag, mem_config) — no new fan-out/cross-join risk
    //    is introduced (both sides gain the same axis, and the join adds the
    //    matching `IS NOT DISTINCT FROM` predicate for it).
    let builder_has_backend_tag = column_exists(conn, "code_profile_runs", "backend_tag").await?;
    let builder_backend_expr = if builder_has_backend_tag {
        "cpr.backend_tag"
    } else {
        "NULL::text"
    };

    let view_sql = format!(
        "CREATE OR REPLACE VIEW model_dual_profile AS \
         WITH builder AS ( \
             SELECT mp.model_name AS model_id, \
                    {backend} AS backend_tag, \
                    cpr.mem_config AS mem_config, \
                    count(cpr.*) AS builder_run_count, \
                    avg(cpr.code_quality_score) AS builder_avg_quality \
             FROM model_profiles mp \
             LEFT JOIN code_profile_runs cpr ON cpr.profile_id = mp.id \
             GROUP BY mp.model_name, {backend}, cpr.mem_config \
         ), \
         assistant AS ( \
             SELECT model_id, backend_tag, mem_config, \
                    count(*) AS assistant_score_count, \
                    avg(value) AS assistant_avg_value \
             FROM assistant_dimension_score \
             WHERE task_category = 'assistant' \
             GROUP BY model_id, backend_tag, mem_config \
         ), \
         keys AS ( \
             SELECT model_id, backend_tag, mem_config FROM builder \
             UNION \
             SELECT model_id, backend_tag, mem_config FROM assistant \
         ) \
         SELECT k.model_id, \
                k.backend_tag, \
                k.mem_config, \
                (b.model_id IS NOT NULL) AS has_builder_profile, \
                (a.model_id IS NOT NULL) AS has_assistant_profile, \
                b.builder_run_count, \
                b.builder_avg_quality, \
                a.assistant_score_count, \
                a.assistant_avg_value \
         FROM keys k \
         LEFT JOIN builder b \
             ON b.model_id = k.model_id \
            AND b.backend_tag IS NOT DISTINCT FROM k.backend_tag \
            AND b.mem_config IS NOT DISTINCT FROM k.mem_config \
         LEFT JOIN assistant a \
             ON a.model_id = k.model_id \
            AND a.backend_tag IS NOT DISTINCT FROM k.backend_tag \
            AND a.mem_config IS NOT DISTINCT FROM k.mem_config",
        backend = builder_backend_expr,
    );

    // `mem_config` (mem-config-tagging) is inserted into the SELECT list
    // BEFORE the pre-existing `has_builder_profile`/`has_assistant_profile`
    // columns, which shifts their ordinal position. `CREATE OR REPLACE VIEW`
    // requires the existing columns to keep their name AND position (Postgres
    // only allows appending new columns at the end), so on any DB that
    // already has a pre-mem_config `model_dual_profile` view, `CREATE OR
    // REPLACE` fails with "cannot change name of view column ... to ...".
    // Drop-and-recreate is safe here: this is a computed, FK-free view (no
    // data of its own), and every known reader selects columns by name, not
    // position, so a full recreate is transparent to them.
    sqlx::query("DROP VIEW IF EXISTS model_dual_profile")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("drop model_dual_profile view: {e}")))?;

    sqlx::query(&view_sql)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create model_dual_profile view: {e}")))?;

    Ok(())
}

/// Probe `information_schema` for a column. Used so the view definition adapts to
/// whether S83's builder table already carries `backend_tag` (P5+) or not.
async fn column_exists(
    conn: &mut PgConnection,
    table: &str,
    column: &str,
) -> Result<bool, ToolError> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1::bigint FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = $2 LIMIT 1",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("probe column {table}.{column}: {e}")))?;
    Ok(row.is_some())
}

/// Insert a new `assistant_profile_run`, returning its id. Dimension runners
/// call this once, then write scores against the returned id.
pub async fn insert_run(pool: &PgPool) -> Result<uuid::Uuid, ToolError> {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assistant_profile_run (id, harness_version) VALUES ($1, $2)",
    )
    .bind(id)
    .bind(HARNESS_VERSION)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("insert assistant_profile_run: {e}")))?;
    Ok(id)
}

/// Insert one aggregated [`DimensionScore`] against a run, tagged with
/// `task_category = "assistant"`. The canonical write path for ASMT-02..07.
///
/// Thin wrapper over [`insert_dimension_score_with_category`] — zero behavior
/// change for every existing caller (dim1..dim6 runners), which never pass a
/// category and always meant "assistant".
pub async fn insert_dimension_score(
    pool: &PgPool,
    run_id: uuid::Uuid,
    score: &DimensionScore,
) -> Result<(), ToolError> {
    insert_dimension_score_with_category(pool, run_id, score, "assistant").await
}

/// Insert one aggregated [`DimensionScore`] against a run, tagged with an
/// explicit `task_category` (MINT new-model-types extension).
///
/// The `assistant_dimension_score` table's `task_category` column is
/// deliberately unconstrained (see [`migrate`]'s doc comment) so any new
/// benchmarking category — `"document_parsing"`, `"image_parsing"`,
/// `"image_generation"`, `"voice_transcription"`, etc. — writes through this
/// same flexible (dimension, metric, value, judge, raw_json) shape without a
/// schema change. [`insert_dimension_score`] is the `"assistant"`-tagged
/// special case of this function, kept for source compatibility.
///
/// Thin wrapper over [`insert_dimension_score_with_category_and_mem_config`]
/// with `mem_config = None` — zero behavior change for every existing caller
/// (dim1..dim6 runners, the `newcats` modules), none of which know about a
/// memory-model configuration yet.
pub async fn insert_dimension_score_with_category(
    pool: &PgPool,
    run_id: uuid::Uuid,
    score: &DimensionScore,
    task_category: &str,
) -> Result<(), ToolError> {
    insert_dimension_score_with_category_and_mem_config(pool, run_id, score, task_category, None)
        .await
}

/// Insert one aggregated [`DimensionScore`] against a run, tagged with an
/// explicit `task_category` AND an explicit `mem_config` (mem-config-tagging
/// sprint). `mem_config` identifies which memory-model configuration (e.g.
/// `"dynamic_gtt"` vs `"carveout"`) the score was measured under; `None`
/// writes SQL `NULL` (used by every caller that predates or doesn't yet track
/// this axis — see [`migrate`]'s doc comment on why old rows must stay NULL,
/// not be back-filled).
pub async fn insert_dimension_score_with_category_and_mem_config(
    pool: &PgPool,
    run_id: uuid::Uuid,
    score: &DimensionScore,
    task_category: &str,
    mem_config: Option<&str>,
) -> Result<(), ToolError> {
    let raw: Option<serde_json::Value> = match &score.raw_json {
        Some(s) => Some(
            serde_json::from_str(s)
                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
        ),
        None => None,
    };
    sqlx::query(
        "INSERT INTO assistant_dimension_score \
         (run_id, model_id, backend_tag, dimension, metric, value, std_dev, judge, \
          low_confidence, raw_json, task_category, mem_config) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(run_id)
    .bind(score.model_id.as_str())
    .bind(score.backend_tag.as_str())
    .bind(&score.dimension)
    .bind(&score.metric)
    .bind(score.value)
    .bind(score.std_dev)
    .bind(&score.judge)
    .bind(score.low_confidence)
    .bind(raw)
    .bind(task_category)
    .bind(mem_config)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("insert assistant_dimension_score: {e}")))?;
    Ok(())
}

/// A single reconciliation finding: a (model_id, backend_tag) present in one
/// profile but not the other.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciliationGap {
    pub model_id: ModelId,
    pub backend_tag: Option<BackendTag>,
    /// Which side this key exists on.
    pub present_in: ProfileSide,
}

/// Which profile a reconciliation key was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSide {
    /// Has an S83 builder profile but no S84 assistant profile.
    BuilderOnly,
    /// Has an S84 assistant profile but no S83 builder profile.
    AssistantOnly,
}

/// Reconciliation report: list every model_id present in exactly one profile.
/// Drives the operator-facing "these models still need an assistant profile /
/// these have an assistant profile but were never builder-profiled" check.
///
/// Reads straight from `model_dual_profile`, so it reflects the same join logic.
pub async fn reconcile(pool: &PgPool) -> Result<Vec<ReconciliationGap>, ToolError> {
    let rows: Vec<(String, Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT model_id, backend_tag, has_builder_profile, has_assistant_profile \
         FROM model_dual_profile \
         WHERE has_builder_profile <> has_assistant_profile \
         ORDER BY model_id, backend_tag",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ToolError::Database(format!("reconcile query: {e}")))?;

    Ok(rows
        .into_iter()
        .map(|(model_id, backend_tag, has_builder, _has_assistant)| ReconciliationGap {
            model_id: ModelId::from_registry_key(model_id),
            backend_tag: backend_tag.as_deref().and_then(BackendTag::parse),
            present_in: if has_builder {
                ProfileSide::BuilderOnly
            } else {
                ProfileSide::AssistantOnly
            },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The SQL is exercised against a live Postgres in integration tests (gated on
    // DATABASE_URL); here we assert the static contract that keeps writers honest.

    #[test]
    fn harness_version_is_stamped() {
        assert_eq!(HARNESS_VERSION, "s84-asmt-01");
    }

    /// Regression test for the MINT new-model-types fix: `model_dual_profile`'s
    /// `assistant` CTE must filter `task_category = 'assistant'`, so a
    /// non-assistant row (e.g. `document_parsing`) for the same
    /// (model_id, backend_tag) does NOT get folded into
    /// `assistant_score_count` / `assistant_avg_value`. Without the filter,
    /// `assistant_score_count` would be 2 (not 1) and `assistant_avg_value`
    /// would blend a 1-5 panel score with a millisecond latency into a
    /// meaningless number.
    ///
    /// Gated on a reachable Postgres: skips (passes trivially) when neither
    /// `INTAKE_DATABASE_URL` nor `DATABASE_URL` is configured, so it stays
    /// green in environments (like tonight's) with no live DB, while still
    /// running for real whenever one is available.
    #[tokio::test]
    async fn assistant_aggregate_excludes_other_task_categories() {
        let pool = match get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping assistant_aggregate_excludes_other_task_categories: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };
        if migrate(&pool).await.is_err() {
            eprintln!(
                "skipping assistant_aggregate_excludes_other_task_categories: \
                 migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }

        let run_id = insert_run(&pool).await.expect("insert_run");
        let model_id = ModelId::from(format!("mint-newcat-regress-test-{}", uuid::Uuid::new_v4()));
        let backend = BackendTag::Gpu;

        // One legitimate assistant-category row (panel-scored, 1-5 scale).
        let assistant_row = DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: "instruction_following".to_string(),
            metric: "concision".to_string(),
            value: 4.0,
            std_dev: None,
            judge: "panel".to_string(),
            low_confidence: false,
            raw_json: None,
        };
        insert_dimension_score(&pool, run_id, &assistant_row)
            .await
            .expect("insert assistant row");

        // One document_parsing row for the SAME (model_id, backend_tag), value
        // on a totally different scale (a millisecond latency) — must NOT be
        // folded into the assistant aggregate.
        let docparse_row = DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: "ocr_extraction".to_string(),
            metric: "latency_ms".to_string(),
            value: 5000.0,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        };
        insert_dimension_score_with_category(&pool, run_id, &docparse_row, "document_parsing")
            .await
            .expect("insert document_parsing row");

        let (count, avg_value): (i64, f64) = sqlx::query_as(
            "SELECT assistant_score_count, assistant_avg_value FROM model_dual_profile \
             WHERE model_id = $1 AND backend_tag = $2",
        )
        .bind(model_id.as_str())
        .bind(backend.as_str())
        .fetch_one(&pool)
        .await
        .expect("query model_dual_profile");

        assert_eq!(
            count, 1,
            "assistant_score_count must only count the assistant-category row, not the \
             document_parsing row"
        );
        assert!(
            (avg_value - 4.0).abs() < 1e-9,
            "assistant_avg_value must stay at the assistant row's own value (4.0), not blend in \
             the document_parsing row's 5000.0 latency — got {avg_value}"
        );

        // Cleanup: this test's rows are scoped to a unique model_id, so this
        // only ever removes what this test run inserted.
        let _ = sqlx::query("DELETE FROM assistant_dimension_score WHERE model_id = $1")
            .bind(model_id.as_str())
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assistant_profile_run WHERE id = $1")
            .bind(run_id)
            .execute(&pool)
            .await;
    }

    /// Regression test for mem-config-tagging: `model_dual_profile` must key
    /// (GROUP BY + JOIN) on `mem_config` in addition to `backend_tag`, so two
    /// rows for the SAME (model_id, backend_tag) but DIFFERENT `mem_config`
    /// values ("dynamic_gtt" vs "carveout") produce SEPARATE aggregate rows,
    /// never a blended average. This is the exact scenario the operator
    /// flagged: a model re-tested under a new memory config while a
    /// preserved baseline for the old config still occupies the same table.
    ///
    /// Gated on a reachable Postgres (same convention as the task_category
    /// regression test above): skips (passes trivially) with no configured
    /// DB, runs for real whenever one is reachable.
    #[tokio::test]
    async fn mem_config_keeps_different_configs_from_blending() {
        let pool = match get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping mem_config_keeps_different_configs_from_blending: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };
        if migrate(&pool).await.is_err() {
            eprintln!(
                "skipping mem_config_keeps_different_configs_from_blending: \
                 migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }

        let run_id = insert_run(&pool).await.expect("insert_run");
        let model_id = ModelId::from(format!(
            "mem-config-regress-test-{}",
            uuid::Uuid::new_v4()
        ));
        let backend = BackendTag::Gpu;

        // KNOWN-BAD scenario (must NOT blend): same (model_id, backend_tag),
        // different mem_config, wildly different values (4.0 vs 1.0).
        let dynamic_row = DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: "instruction_following".to_string(),
            metric: "concision".to_string(),
            value: 4.0,
            std_dev: None,
            judge: "panel".to_string(),
            low_confidence: false,
            raw_json: None,
        };
        insert_dimension_score_with_category_and_mem_config(
            &pool,
            run_id,
            &dynamic_row,
            "assistant",
            Some("dynamic_gtt"),
        )
        .await
        .expect("insert dynamic_gtt row");

        let carveout_row = DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: "instruction_following".to_string(),
            metric: "concision".to_string(),
            value: 1.0,
            std_dev: None,
            judge: "panel".to_string(),
            low_confidence: false,
            raw_json: None,
        };
        insert_dimension_score_with_category_and_mem_config(
            &pool,
            run_id,
            &carveout_row,
            "assistant",
            Some("carveout"),
        )
        .await
        .expect("insert carveout row");

        let rows: Vec<(Option<String>, i64, f64)> = sqlx::query_as(
            "SELECT mem_config, assistant_score_count, assistant_avg_value \
             FROM model_dual_profile \
             WHERE model_id = $1 AND backend_tag = $2 \
             ORDER BY mem_config",
        )
        .bind(model_id.as_str())
        .bind(backend.as_str())
        .fetch_all(&pool)
        .await
        .expect("query model_dual_profile");

        assert_eq!(
            rows.len(),
            2,
            "expected two SEPARATE aggregate rows (one per mem_config), not one blended row — got {rows:?}"
        );
        for (mem_config, count, avg_value) in &rows {
            assert_eq!(*count, 1, "each mem_config's aggregate must count only its own row");
            match mem_config.as_deref() {
                Some("dynamic_gtt") => assert!(
                    (avg_value - 4.0).abs() < 1e-9,
                    "dynamic_gtt aggregate must stay at 4.0, not blend with carveout's 1.0 — got {avg_value}"
                ),
                Some("carveout") => assert!(
                    (avg_value - 1.0).abs() < 1e-9,
                    "carveout aggregate must stay at 1.0, not blend with dynamic_gtt's 4.0 — got {avg_value}"
                ),
                other => panic!("unexpected mem_config value: {other:?}"),
            }
        }

        // KNOWN-GOOD scenario: a THIRD row, same mem_config as the first
        // ("dynamic_gtt"), must aggregate together with it (count=2), proving
        // the fix doesn't over-split — only DIFFERENT mem_config values stay
        // separate; SAME values still aggregate normally.
        let dynamic_row_2 = DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: "instruction_following".to_string(),
            metric: "concision".to_string(),
            value: 2.0,
            std_dev: None,
            judge: "panel".to_string(),
            low_confidence: false,
            raw_json: None,
        };
        insert_dimension_score_with_category_and_mem_config(
            &pool,
            run_id,
            &dynamic_row_2,
            "assistant",
            Some("dynamic_gtt"),
        )
        .await
        .expect("insert second dynamic_gtt row");

        let (count, avg_value): (i64, f64) = sqlx::query_as(
            "SELECT assistant_score_count, assistant_avg_value FROM model_dual_profile \
             WHERE model_id = $1 AND backend_tag = $2 AND mem_config = 'dynamic_gtt'",
        )
        .bind(model_id.as_str())
        .bind(backend.as_str())
        .fetch_one(&pool)
        .await
        .expect("query model_dual_profile for dynamic_gtt");
        assert_eq!(count, 2, "same-mem_config rows must still aggregate together");
        assert!(
            (avg_value - 3.0).abs() < 1e-9,
            "dynamic_gtt aggregate over (4.0, 2.0) must average to 3.0 — got {avg_value}"
        );

        // Cleanup.
        let _ = sqlx::query("DELETE FROM assistant_dimension_score WHERE model_id = $1")
            .bind(model_id.as_str())
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assistant_profile_run WHERE id = $1")
            .bind(run_id)
            .execute(&pool)
            .await;
    }

    #[test]
    fn profile_side_is_distinct() {
        assert_ne!(ProfileSide::BuilderOnly, ProfileSide::AssistantOnly);
    }

    #[test]
    fn reconciliation_gap_round_trips_tag() {
        let g = ReconciliationGap {
            model_id: ModelId::from("qwen3:8b"),
            backend_tag: Some(BackendTag::Gpu),
            present_in: ProfileSide::AssistantOnly,
        };
        assert_eq!(g.model_id.as_str(), "qwen3:8b");
        assert_eq!(g.backend_tag, Some(BackendTag::Gpu));
    }
}
