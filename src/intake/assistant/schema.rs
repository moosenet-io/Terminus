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

    // 3. The dual-profile join view. Built so a model present in only ONE side
    //    still appears (key set = UNION of both sides' model_ids), and so a
    //    missing builder `backend_tag` column degrades to NULL rather than
    //    erroring at view-create time.
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
                    count(cpr.*) AS builder_run_count, \
                    avg(cpr.code_quality_score) AS builder_avg_quality \
             FROM model_profiles mp \
             LEFT JOIN code_profile_runs cpr ON cpr.profile_id = mp.id \
             GROUP BY mp.model_name, {backend} \
         ), \
         assistant AS ( \
             SELECT model_id, backend_tag, \
                    count(*) AS assistant_score_count, \
                    avg(value) AS assistant_avg_value \
             FROM assistant_dimension_score \
             GROUP BY model_id, backend_tag \
         ), \
         keys AS ( \
             SELECT model_id, backend_tag FROM builder \
             UNION \
             SELECT model_id, backend_tag FROM assistant \
         ) \
         SELECT k.model_id, \
                k.backend_tag, \
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
         LEFT JOIN assistant a \
             ON a.model_id = k.model_id \
            AND a.backend_tag IS NOT DISTINCT FROM k.backend_tag",
        backend = builder_backend_expr,
    );

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
        "SELECT 1 FROM information_schema.columns \
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

/// Insert one aggregated [`DimensionScore`] against a run. The canonical write
/// path for ASMT-02..07.
pub async fn insert_dimension_score(
    pool: &PgPool,
    run_id: uuid::Uuid,
    score: &DimensionScore,
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
          low_confidence, raw_json) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
