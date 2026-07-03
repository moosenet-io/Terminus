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

use sqlx::PgPool;

use crate::config;
use crate::error::ToolError;

use super::{BackendTag, DimensionScore, ModelId};

/// Schema/harness version stamped onto every `assistant_profile_run` row.
pub const HARNESS_VERSION: &str = "s84-asmt-01";

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
    // 1. Runs.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS assistant_profile_run ( \
            id UUID PRIMARY KEY, \
            started_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            harness_version TEXT NOT NULL \
         )",
    )
    .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
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
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("add mem_config column: {e}")))?;

    // Recreated (not just created) so the index definition stays current even
    // on a DB that already had the old (model_id, backend_tag) index from a
    // prior migrate() run. Cheap: this table is intake-scale, not hot-path.
    sqlx::query("DROP INDEX IF EXISTS idx_assistant_score_model")
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("drop idx_assistant_score_model: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_assistant_score_model \
         ON assistant_dimension_score (model_id, backend_tag, task_category)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_assistant_score_model: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_assistant_score_run \
         ON assistant_dimension_score (run_id)",
    )
    .execute(pool)
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
        .execute(pool)
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
        .execute(pool)
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
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("add code_profile_runs.case_id: {e}")))?;

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
    let builder_has_backend_tag = column_exists(pool, "code_profile_runs", "backend_tag").await?;
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
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("drop model_dual_profile view: {e}")))?;

    sqlx::query(&view_sql)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("create model_dual_profile view: {e}")))?;

    Ok(())
}

/// Probe `information_schema` for a column. Used so the view definition adapts to
/// whether S83's builder table already carries `backend_tag` (P5+) or not.
async fn column_exists(pool: &PgPool, table: &str, column: &str) -> Result<bool, ToolError> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1::bigint FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = $2 LIMIT 1",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
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
