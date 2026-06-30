//! Serving-profile schema + idempotent migration (S85 SRV-01).
//!
//! Mirrors the S84 [`super::super::assistant::schema`] pattern: this module owns
//! its own table and creates it with `CREATE TABLE IF NOT EXISTS` /
//! `CREATE OR REPLACE VIEW`, so [`migrate`] is safe to call repeatedly (the SRV-02
//! runner calls it before every write). The intake DB URL is sourced from
//! [`crate::config::intake_database_url`] (NO literals).
//!
//! ## Table
//!   - `serving_profile` — one row per (model × serving backend). `model_id` is
//!     byte-identical to S83 `model_profiles.model_name` (see [`super::ModelId`]).
//!     Re-running the harness UPSERTs the row for the same `(model_id,
//!     backend_tag)`, never duplicates.
//!
//! ## View
//!   - `model_full_profile` — extends S84's `model_dual_profile` (builder +
//!     assistant) with the serving side. A LEFT JOIN over the UNIONed key set of
//!     all three profiles, joined on `model_id` (+ `backend_tag`), so a model
//!     present in ANY subset still appears. NOTE the backend tags differ across
//!     dimensions (builder/assistant use `gpu`/`cpu`; serving uses
//!     `llama-gpu`/`ollama-gpu`/`cpu`), so the view keys the join on `model_id`
//!     and surfaces each side's backend tag in its own column rather than forcing
//!     a single shared tag — see the view SQL.

use sqlx::PgPool;

use crate::config;
use crate::error::ToolError;

use super::ServingProfile;

/// Schema/harness version stamped onto serving rows / runs.
pub const HARNESS_VERSION: &str = "s85-srv-01";

/// Connect a pool to the intake DB. Prefers `INTAKE_DATABASE_URL`, falls back to
/// `DATABASE_URL` (the same shared DB S83/S84 use), all via [`config`].
pub async fn get_pool() -> Result<PgPool, ToolError> {
    let url = config::intake_database_url().ok_or_else(|| {
        ToolError::NotConfigured(
            "neither INTAKE_DATABASE_URL nor DATABASE_URL set — serving intake \
             requires a Postgres connection"
                .into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to intake database: {e}")))
}

/// Apply the serving-profile schema. Idempotent: safe to call on every run.
///
/// Creates the `serving_profile` table, its UPSERT-supporting unique key, its
/// indexes, and the `model_full_profile` view. The CHECK constraints mirror the
/// in-Rust [`ServingProfile::validate`] gate so a contradictory enum combo is
/// rejected at BOTH the application boundary and the DB boundary.
pub async fn migrate(pool: &PgPool) -> Result<(), ToolError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS serving_profile ( \
            id BIGSERIAL PRIMARY KEY, \
            run_id UUID NOT NULL, \
            model_id TEXT NOT NULL, \
            backend_tag TEXT NOT NULL \
                CHECK (backend_tag IN ('llama-gpu','ollama-gpu','cpu')), \
            best_runtime TEXT NOT NULL \
                CHECK (best_runtime IN ('llama-cpp','ollama','cpu')), \
            env_json JSONB NOT NULL DEFAULT '{}'::jsonb, \
            tok_s DOUBLE PRECISION, \
            vram_or_ram_peak_gb DOUBLE PRECISION, \
            cold_load_s DOUBLE PRECISION, \
            keep_warm BOOLEAN NOT NULL DEFAULT false, \
            fallback_runtime TEXT \
                CHECK (fallback_runtime IS NULL OR fallback_runtime IN ('llama-cpp','ollama','cpu')), \
            exclusion_reason TEXT NOT NULL DEFAULT 'none' \
                CHECK (exclusion_reason IN \
                    ('none','permanent-unknown-arch','build-conditional', \
                     'quant-unsupported','oom-host-ram','oom-vram')), \
            recheck_trigger TEXT NOT NULL DEFAULT 'none' \
                CHECK (recheck_trigger IN ('none','llama-cpp-version-bump')), \
            provenance TEXT, \
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            \
            -- Schema-level rejection of the contradictory enum combo (mirrors \
            -- ServingProfile::validate): the version-bump recheck is ONLY valid \
            -- with a build-conditional exclusion, and build-conditional MUST \
            -- carry it. \
            CONSTRAINT serving_profile_recheck_coherent CHECK ( \
                (recheck_trigger = 'llama-cpp-version-bump' \
                     AND exclusion_reason = 'build-conditional') \
                OR (recheck_trigger = 'none' \
                     AND exclusion_reason <> 'build-conditional') \
            ) \
         )",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create serving_profile: {e}")))?;

    // UNIQUE (model_id, backend_tag) is the UPSERT conflict target: re-running the
    // serving harness overwrites the row for the same backend, never duplicates.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_serving_profile_model_backend \
         ON serving_profile (model_id, backend_tag)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create uq_serving_profile_model_backend: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_serving_profile_keepwarm \
         ON serving_profile (keep_warm)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_serving_profile_keepwarm: {e}")))?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_serving_profile_recheck \
         ON serving_profile (recheck_trigger)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_serving_profile_recheck: {e}")))?;

    create_full_profile_view(pool).await?;

    Ok(())
}

/// Create/replace the `model_full_profile` view: builder + assistant + serving.
///
/// Extends S84's `model_dual_profile`. The builder/assistant sides use the
/// `gpu`/`cpu` hardware tag; the serving side uses the three-tier
/// `llama-gpu`/`ollama-gpu`/`cpu` tag. They are NOT the same axis, so the view
/// joins all three sides on `model_id` (the byte-identical key) and surfaces each
/// side's own tag in a dedicated column, rather than forcing a single shared tag.
/// The key set is the UNION of every side's model_ids, so a model present in ANY
/// subset (any one, any two, or all three) appears exactly once.
async fn create_full_profile_view(pool: &PgPool) -> Result<(), ToolError> {
    // Reuse S84's catalog probe so the builder backend tag degrades to NULL when
    // the early S83 schema lacks the column (same defensive pattern).
    let builder_has_backend_tag = column_exists(pool, "code_profile_runs", "backend_tag").await?;
    let builder_backend_expr = if builder_has_backend_tag {
        "cpr.backend_tag"
    } else {
        "NULL::text"
    };

    let view_sql = format!(
        "CREATE OR REPLACE VIEW model_full_profile AS \
         WITH builder AS ( \
             SELECT mp.model_name AS model_id, \
                    {backend} AS builder_backend_tag, \
                    count(cpr.*) AS builder_run_count, \
                    avg(cpr.code_quality_score) AS builder_avg_quality \
             FROM model_profiles mp \
             LEFT JOIN code_profile_runs cpr ON cpr.profile_id = mp.id \
             GROUP BY mp.model_name, {backend} \
         ), \
         assistant AS ( \
             SELECT model_id, \
                    count(*) AS assistant_score_count, \
                    avg(value) AS assistant_avg_value \
             FROM assistant_dimension_score \
             GROUP BY model_id \
         ), \
         serving AS ( \
             SELECT model_id, \
                    count(*) AS serving_row_count, \
                    bool_or(keep_warm) AS serving_any_keep_warm, \
                    array_agg(backend_tag ORDER BY backend_tag) AS serving_backends \
             FROM serving_profile \
             GROUP BY model_id \
         ), \
         keys AS ( \
             SELECT model_id FROM builder \
             UNION \
             SELECT model_id FROM assistant \
             UNION \
             SELECT model_id FROM serving \
         ) \
         SELECT k.model_id, \
                (b.model_id IS NOT NULL) AS has_builder_profile, \
                (a.model_id IS NOT NULL) AS has_assistant_profile, \
                (s.model_id IS NOT NULL) AS has_serving_profile, \
                b.builder_backend_tag, \
                b.builder_run_count, \
                b.builder_avg_quality, \
                a.assistant_score_count, \
                a.assistant_avg_value, \
                s.serving_row_count, \
                s.serving_any_keep_warm, \
                s.serving_backends \
         FROM keys k \
         LEFT JOIN builder b ON b.model_id = k.model_id \
         LEFT JOIN assistant a ON a.model_id = k.model_id \
         LEFT JOIN serving s ON s.model_id = k.model_id",
        backend = builder_backend_expr,
    );

    sqlx::query(&view_sql)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("create model_full_profile view: {e}")))?;

    Ok(())
}

/// Probe `information_schema` for a column (same defensive helper as S84).
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

/// UPSERT a [`ServingProfile`] for one `(model_id, backend_tag)`. The canonical
/// write path for SRV-02/03. Re-running overwrites the existing row in place
/// (ON CONFLICT on the unique key), never duplicates.
///
/// Validates the enum combination FIRST ([`ServingProfile::validate`]) so a
/// contradictory row is rejected at the application boundary before the DB CHECK
/// would also reject it — the in-Rust gate gives a clearer error and keeps the
/// negative test fast (no DB needed).
pub async fn upsert_serving_profile(
    pool: &PgPool,
    run_id: uuid::Uuid,
    profile: &ServingProfile,
) -> Result<(), ToolError> {
    profile
        .validate()
        .map_err(|e| ToolError::InvalidArgument(e.to_string()))?;

    let env: serde_json::Value =
        serde_json::from_str(&profile.env_json).unwrap_or_else(|_| serde_json::json!({}));

    sqlx::query(
        "INSERT INTO serving_profile \
         (run_id, model_id, backend_tag, best_runtime, env_json, tok_s, \
          vram_or_ram_peak_gb, cold_load_s, keep_warm, fallback_runtime, \
          exclusion_reason, recheck_trigger, provenance) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
         ON CONFLICT (model_id, backend_tag) DO UPDATE SET \
            run_id = EXCLUDED.run_id, \
            best_runtime = EXCLUDED.best_runtime, \
            env_json = EXCLUDED.env_json, \
            tok_s = EXCLUDED.tok_s, \
            vram_or_ram_peak_gb = EXCLUDED.vram_or_ram_peak_gb, \
            cold_load_s = EXCLUDED.cold_load_s, \
            keep_warm = EXCLUDED.keep_warm, \
            fallback_runtime = EXCLUDED.fallback_runtime, \
            exclusion_reason = EXCLUDED.exclusion_reason, \
            recheck_trigger = EXCLUDED.recheck_trigger, \
            provenance = EXCLUDED.provenance, \
            updated_at = now()",
    )
    .bind(run_id)
    .bind(profile.model_id.as_str())
    .bind(profile.backend_tag.as_str())
    .bind(profile.best_runtime.as_str())
    .bind(env)
    .bind(profile.tok_s)
    .bind(profile.vram_or_ram_peak_gb)
    .bind(profile.cold_load_s)
    .bind(profile.keep_warm)
    .bind(profile.fallback_runtime.map(|r| r.as_str()))
    .bind(profile.exclusion_reason.as_str())
    .bind(profile.recheck_trigger.as_str())
    .bind(profile.provenance.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("upsert serving_profile: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::serving::{ExclusionReason, RecheckTrigger, Runtime, ServingBackend};
    use crate::intake::serving::ModelId;

    // The SQL is exercised against a live Postgres in integration tests (gated on
    // DATABASE_URL); here we assert the static contracts that keep writers honest.

    #[test]
    fn harness_version_is_stamped() {
        assert_eq!(HARNESS_VERSION, "s85-srv-01");
    }

    #[test]
    fn upsert_path_rejects_contradiction_before_db() {
        // The write path validates first, so a contradictory row never reaches a
        // bind. We assert via validate() (the same gate upsert calls) so the
        // negative test needs no DB.
        let bad = ServingProfile {
            model_id: ModelId::from("gpt-oss:20b"),
            backend_tag: ServingBackend::OllamaGpu,
            best_runtime: Runtime::Ollama,
            env_json: "{}".into(),
            tok_s: None,
            vram_or_ram_peak_gb: None,
            cold_load_s: None,
            keep_warm: false,
            fallback_runtime: None,
            // permanent-unknown-arch + version-bump is the contradiction.
            exclusion_reason: ExclusionReason::PermanentUnknownArch,
            recheck_trigger: RecheckTrigger::LlamaCppVersionBump,
            provenance: None,
        };
        assert!(bad.validate().is_err());
    }
}
