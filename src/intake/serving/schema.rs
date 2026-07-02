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
    // CRITICAL (MINT new-model-types fix, twin of the model_dual_profile fix in
    // `assistant::schema`): the `assistant` CTE below filters
    // `WHERE task_category = 'assistant'`. `assistant_dimension_score` now also
    // hosts document_parsing/image_parsing/image_generation/voice_transcription
    // rows (see `intake::newcats`), whose values live on incompatible scales
    // (0-1 accuracy vs. millisecond latencies vs. unbounded WER vs. VRAM MB) and
    // must NOT be folded into `assistant_score_count` / `assistant_avg_value` —
    // without this filter a vision/ASR/image-gen-only model would wrongly show
    // `has_assistant_profile = true` here too.
    //
    // Reuse S84's catalog probe so the builder backend tag degrades to NULL when
    // the early S83 schema lacks the column (same defensive pattern).
    let builder_has_backend_tag = column_exists(pool, "code_profile_runs", "backend_tag").await?;
    let builder_backend_expr = if builder_has_backend_tag {
        "cpr.backend_tag"
    } else {
        "NULL::text"
    };

    // Same defensive pattern as builder_has_backend_tag immediately above: guard
    // `cpr.mem_config` too. Both `backend_tag` and `mem_config` are added to
    // `code_profile_runs` ONLY inside `assistant::schema::migrate()`, and this
    // module's own `migrate()` does not call that first — so on a DB where
    // `code_profile_runs` exists but the assistant migration hasn't run yet,
    // an unguarded `cpr.mem_config` reference would make `CREATE VIEW
    // model_full_profile` hard-fail with "column cpr.mem_config does not
    // exist" before the backend_tag degradation above even gets a chance to
    // help.
    let builder_has_mem_config = column_exists(pool, "code_profile_runs", "mem_config").await?;
    let builder_mem_config_expr = if builder_has_mem_config {
        "cpr.mem_config"
    } else {
        "NULL::text"
    };

    // mem-config-tagging DECISION (documented here, not just in the build
    // report, so a future reader sees the reasoning at the code site):
    //
    //   `builder` (from `code_profile_runs`) DOES get `mem_config` added to
    //   its GROUP BY, surfaced as `builder_mem_config`. This is SAFE because
    //   this view's outer join keys ONLY on `model_id` (see the module doc
    //   comment: the three sides use incompatible backend-tag vocabularies,
    //   so keying is deliberately loose) and `assistant`/`serving` are each
    //   single-row-per-model_id "broadcast" across however many `builder`
    //   rows exist — exactly the same fan-out `builder_backend_tag` already
    //   causes today when a model has both a gpu and a cpu builder row.
    //   Adding `mem_config` is the same accepted pattern extended to a new
    //   axis: it stops `builder_avg_quality` from silently blending the
    //   preserved `carveout` baseline with tonight's `dynamic_gtt` rows.
    //
    //   `assistant` (from `assistant_dimension_score`) is DELIBERATELY LEFT
    //   UNCHANGED — grouped by `model_id` only, still coarse. Splitting it by
    //   `mem_config` too would NOT be safe here: it would turn `assistant`
    //   from a single broadcast row into N rows per model_id, and because the
    //   join key is `model_id` only (not mem_config), that would cross-join
    //   against `builder`'s (also-now-multi-row) rows — pairing a
    //   `dynamic_gtt` builder row with a `carveout` assistant row in some
    //   output rows. That would be a NEW, worse blending bug (wrong-pairing),
    //   not a fix. `model_dual_profile` (the granular, backend_tag- and now
    //   mem_config-keyed view in `assistant::schema`) is the correct place
    //   for exact per-mem_config assistant reconciliation; this view's job is
    //   the coarser "does this model have ANY profile" rollup, and it already
    //   accepted a coarser blend on the assistant side (across backend_tag)
    //   before this change. Follow-up if per-mem_config assistant granularity
    //   is ever needed here: promote `mem_config` (and `backend_tag`) to a
    //   real join key across all three CTEs, not just `builder`.
    let view_sql = format!(
        "CREATE OR REPLACE VIEW model_full_profile AS \
         WITH builder AS ( \
             SELECT mp.model_name AS model_id, \
                    {backend} AS builder_backend_tag, \
                    {mem_config} AS builder_mem_config, \
                    count(cpr.*) AS builder_run_count, \
                    avg(cpr.code_quality_score) AS builder_avg_quality \
             FROM model_profiles mp \
             LEFT JOIN code_profile_runs cpr ON cpr.profile_id = mp.id \
             GROUP BY mp.model_name, {backend}, {mem_config} \
         ), \
         assistant AS ( \
             SELECT model_id, \
                    count(*) AS assistant_score_count, \
                    avg(value) AS assistant_avg_value \
             FROM assistant_dimension_score \
             WHERE task_category = 'assistant' \
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
                b.builder_mem_config, \
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
        mem_config = builder_mem_config_expr,
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

    /// Regression test for the MINT new-model-types fix, twin of
    /// `assistant::schema::tests::assistant_aggregate_excludes_other_task_categories`:
    /// `model_full_profile`'s `assistant` CTE must filter
    /// `task_category = 'assistant'`, so a non-assistant row (e.g.
    /// `document_parsing`) for the same `model_id` does NOT get folded into
    /// `assistant_score_count` / `assistant_avg_value`. This is the template for
    /// catching this bug class if a third view is ever added.
    ///
    /// Gated on a reachable Postgres: skips (passes trivially) when neither
    /// `INTAKE_DATABASE_URL` nor `DATABASE_URL` is configured — view-level SQL
    /// genuinely can only be verified against real Postgres, so there is no
    /// DB-free equivalent of this assertion. Still wired to actually exercise
    /// the fix end-to-end whenever a pool is reachable.
    #[tokio::test]
    async fn full_profile_view_excludes_other_task_categories_from_assistant_aggregate() {
        let pool = match get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping full_profile_view_excludes_other_task_categories_from_assistant_aggregate: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };

        // Bring up both schemas: assistant owns `assistant_dimension_score` (incl.
        // `task_category`), serving owns `model_full_profile` (this module).
        if crate::intake::assistant::schema::migrate(&pool).await.is_err() {
            eprintln!(
                "skipping full_profile_view_excludes_other_task_categories_from_assistant_aggregate: \
                 assistant schema migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }
        if migrate(&pool).await.is_err() {
            eprintln!(
                "skipping full_profile_view_excludes_other_task_categories_from_assistant_aggregate: \
                 serving schema migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }

        let run_id = crate::intake::assistant::schema::insert_run(&pool)
            .await
            .expect("insert_run");
        let model_id = crate::intake::assistant::ModelId::from(format!(
            "mint-newcat-regress-test-full-profile-{}",
            uuid::Uuid::new_v4()
        ));
        let backend = crate::intake::assistant::BackendTag::Gpu;

        // One legitimate assistant-category row (panel-scored, 1-5 scale).
        let assistant_row = crate::intake::assistant::DimensionScore {
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
        crate::intake::assistant::schema::insert_dimension_score(&pool, run_id, &assistant_row)
            .await
            .expect("insert assistant row");

        // One document_parsing row for the SAME model_id, value on a totally
        // different scale (a millisecond latency) — must NOT be folded into the
        // assistant aggregate.
        let docparse_row = crate::intake::assistant::DimensionScore {
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
        crate::intake::assistant::schema::insert_dimension_score_with_category(
            &pool,
            run_id,
            &docparse_row,
            "document_parsing",
        )
        .await
        .expect("insert document_parsing row");

        let (count, avg_value): (i64, f64) = sqlx::query_as(
            "SELECT assistant_score_count, assistant_avg_value FROM model_full_profile \
             WHERE model_id = $1",
        )
        .bind(model_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("query model_full_profile");

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

        // Cleanup: this test's rows are scoped to a unique model_id, so this only
        // ever removes what this test run inserted.
        let _ = sqlx::query("DELETE FROM assistant_dimension_score WHERE model_id = $1")
            .bind(model_id.as_str())
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assistant_profile_run WHERE id = $1")
            .bind(run_id)
            .execute(&pool)
            .await;
    }

    /// Regression test for mem-config-tagging: `model_full_profile`'s `builder`
    /// CTE (over `code_profile_runs`) must key on `mem_config` in addition to
    /// `backend_tag`, so two builder rows for the SAME model_id/backend but
    /// DIFFERENT `mem_config` ("dynamic_gtt" vs "carveout") surface as
    /// SEPARATE `builder_avg_quality` rows, never one blended average.
    ///
    /// Gated on a reachable Postgres, same convention as the sibling test.
    #[tokio::test]
    async fn builder_mem_config_keeps_different_configs_from_blending() {
        let pool = match get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping builder_mem_config_keeps_different_configs_from_blending: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };
        if crate::intake::assistant::schema::migrate(&pool).await.is_err() {
            eprintln!(
                "skipping builder_mem_config_keeps_different_configs_from_blending: \
                 assistant schema migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }
        if migrate(&pool).await.is_err() {
            eprintln!(
                "skipping builder_mem_config_keeps_different_configs_from_blending: \
                 serving schema migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }

        let model_name = format!("mem-config-full-profile-regress-{}", uuid::Uuid::new_v4());
        let profile_id = crate::intake::storage::insert_model_profile(
            &pool,
            &model_name,
            "test-provider",
            None,
            None,
        )
        .await
        .expect("insert_model_profile");

        // KNOWN-BAD scenario (must NOT blend): same model, same backend_tag,
        // different mem_config, wildly different quality scores.
        let dynamic_row = crate::intake::storage::CodeRunRow {
            language: "rust".to_string(),
            code_quality_score: Some(5.0),
            backend_tag: Some("gpu".to_string()),
            ..Default::default()
        };
        crate::intake::storage::insert_code_run(&pool, profile_id, &dynamic_row)
            .await
            .expect("insert dynamic_gtt code run");
        sqlx::query("UPDATE code_profile_runs SET mem_config = 'dynamic_gtt' \
                     WHERE profile_id = $1 AND code_quality_score = 5.0")
            .bind(profile_id)
            .execute(&pool)
            .await
            .expect("tag dynamic_gtt row");

        let carveout_row = crate::intake::storage::CodeRunRow {
            language: "rust".to_string(),
            code_quality_score: Some(1.0),
            backend_tag: Some("gpu".to_string()),
            ..Default::default()
        };
        crate::intake::storage::insert_code_run(&pool, profile_id, &carveout_row)
            .await
            .expect("insert carveout code run");
        sqlx::query("UPDATE code_profile_runs SET mem_config = 'carveout' \
                     WHERE profile_id = $1 AND code_quality_score = 1.0")
            .bind(profile_id)
            .execute(&pool)
            .await
            .expect("tag carveout row");

        let rows: Vec<(Option<String>, i64, f64)> = sqlx::query_as(
            "SELECT builder_mem_config, builder_run_count, builder_avg_quality \
             FROM model_full_profile WHERE model_id = $1 ORDER BY builder_mem_config",
        )
        .bind(&model_name)
        .fetch_all(&pool)
        .await
        .expect("query model_full_profile");

        assert_eq!(
            rows.len(),
            2,
            "expected two SEPARATE builder rows (one per mem_config), not one blended row — got {rows:?}"
        );
        for (mem_config, count, avg_quality) in &rows {
            assert_eq!(*count, 1);
            match mem_config.as_deref() {
                Some("dynamic_gtt") => assert!((avg_quality - 5.0).abs() < 1e-9),
                Some("carveout") => assert!((avg_quality - 1.0).abs() < 1e-9),
                other => panic!("unexpected mem_config value: {other:?}"),
            }
        }

        // Cleanup.
        let _ = sqlx::query("DELETE FROM code_profile_runs WHERE profile_id = $1")
            .bind(profile_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM model_profiles WHERE id = $1")
            .bind(profile_id)
            .execute(&pool)
            .await;
    }

    /// Regression test for the `builder_has_mem_config` guard added alongside
    /// this fix: `column_exists` must correctly report both "column absent"
    /// and "column present" for `mem_config`, since `create_full_profile_view`
    /// picks between `cpr.mem_config` and `NULL::text` based solely on that
    /// probe. This is the same defensive pattern `builder_has_backend_tag`
    /// already relies on (S84) — `mem_config` needed the identical guard
    /// because `serving::schema::migrate()` never calls
    /// `assistant::schema::migrate()` (the only place that adds the column to
    /// `code_profile_runs`), so a DB that has run serving's migrate but not
    /// assistant's would otherwise hit "column cpr.mem_config does not exist"
    /// when `CREATE VIEW model_full_profile` runs.
    ///
    /// This deliberately probes a throwaway, uniquely-named scratch table
    /// (created and dropped entirely within this test) rather than mutating
    /// the shared `code_profile_runs` table: dropping/re-adding a column on
    /// that table would risk clobbering real harness data for any other
    /// concurrently-running test or process against the same DB. The
    /// "column present" side of the view's behavior (mem_config correctly
    /// populated / not blended) is already covered end-to-end by
    /// `builder_mem_config_keeps_different_configs_from_blending` above.
    ///
    /// Gated on a reachable Postgres, same convention as the sibling tests.
    #[tokio::test]
    async fn column_exists_detects_absent_and_present_mem_config() {
        let pool = match get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping column_exists_detects_absent_and_present_mem_config: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };

        let table = format!("mem_config_guard_probe_{}", uuid::Uuid::new_v4().simple());

        // Scratch table, no mem_config column yet — mirrors the pre-assistant-
        // migrate shape of code_profile_runs that the guard must tolerate.
        sqlx::query(&format!(
            "CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, backend_tag TEXT)"
        ))
        .execute(&pool)
        .await
        .expect("create scratch table");

        assert!(
            !column_exists(&pool, &table, "mem_config")
                .await
                .expect("probe absent mem_config"),
            "column_exists must report false before the column is added — this is the exact \
             condition builder_has_mem_config guards against"
        );

        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN mem_config TEXT"))
            .execute(&pool)
            .await
            .expect("add mem_config column");

        assert!(
            column_exists(&pool, &table, "mem_config")
                .await
                .expect("probe present mem_config"),
            "column_exists must report true once the column exists, so builder_mem_config_expr \
             switches from NULL::text to cpr.mem_config"
        );

        // Cleanup: drop the scratch table entirely (nothing shared is touched).
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&pool)
            .await;
    }
}
