//! Postgres read/write for model intake profiling (S83 MINT-01).
//!
//! Tables already exist in the shared DB (`DATABASE_URL` — same DB as nexus /
//! reminders). DO NOT create them here. We only INSERT and SELECT.
//!
//! Tables used:
//!   - `model_profiles`               — one row per intake run
//!   - `context_profile_runs`         — one row per context tier
//!   - `model_operational_profiles`   — one derived row computed after all tiers
//!
//! `get_pool()` mirrors the reminder module: reads `DATABASE_URL`, connects a
//! fresh `PgPool`. The intake run is infrequent (manual / onboarding), so a
//! per-call pool is acceptable.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ToolError;

/// Connect a Postgres pool from `DATABASE_URL` (same pattern as reminder::get_pool).
pub async fn get_pool() -> Result<PgPool, ToolError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured(
            "DATABASE_URL not set — model intake requires a Postgres connection".into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to database: {e}")))
}

/// One measured context tier. Mirrors `context_profile_runs` columns.
#[derive(Debug, Clone)]
pub struct ContextRunRow {
    pub context_tokens: i32,
    pub throughput_tok_per_sec: Option<f64>,
    pub ttft_ms: Option<i32>,
    pub total_time_ms: Option<i32>,
    pub recall_score: Option<i32>,
    pub coherence_score: Option<f64>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
}

/// Derived operational profile. Mirrors the subset of
/// `model_operational_profiles` columns that the context suite populates.
#[derive(Debug, Clone, Default)]
pub struct OperationalProfileRow {
    pub max_context_safe: Option<i32>,
    pub max_context_absolute: Option<i32>,
    pub quality_degradation_point: Option<i32>,
    pub throughput_at_2k: Option<f64>,
    pub throughput_at_8k: Option<f64>,
    pub throughput_at_16k: Option<f64>,
    pub throughput_at_32k: Option<f64>,
    pub throughput_at_64k: Option<f64>,
    pub recommended_timeout_chat_sec: Option<i32>,
    pub recommended_timeout_build_sec: Option<i32>,
    pub recommended_timeout_deep_sec: Option<i32>,
    pub overall_tier: Option<String>,
}

/// Insert a `model_profiles` row, returning its generated id.
pub async fn insert_model_profile(
    pool: &PgPool,
    model_name: &str,
    provider: &str,
    reported_context_window: Option<i32>,
    vram_gb: Option<f64>,
) -> Result<Uuid, ToolError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO model_profiles \
         (id, model_name, provider, vram_gb, reported_context_window) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(model_name)
    .bind(provider)
    .bind(vram_gb)
    .bind(reported_context_window)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert model_profiles: {e}")))?;
    Ok(id)
}

/// Insert one `context_profile_runs` row.
pub async fn insert_context_run(
    pool: &PgPool,
    profile_id: Uuid,
    row: &ContextRunRow,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO context_profile_runs \
         (profile_id, context_tokens, throughput_tok_per_sec, ttft_ms, total_time_ms, \
          recall_score, coherence_score, memory_usage_mb, oom, error) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(profile_id)
    .bind(row.context_tokens)
    .bind(row.throughput_tok_per_sec)
    .bind(row.ttft_ms)
    .bind(row.total_time_ms)
    .bind(row.recall_score)
    .bind(row.coherence_score)
    .bind(row.memory_usage_mb)
    .bind(row.oom)
    .bind(row.error.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert context_profile_runs: {e}")))?;
    Ok(())
}

/// Insert the derived `model_operational_profiles` row.
pub async fn insert_operational_profile(
    pool: &PgPool,
    profile_id: Uuid,
    p: &OperationalProfileRow,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO model_operational_profiles \
         (profile_id, max_context_safe, max_context_absolute, quality_degradation_point, \
          throughput_at_2k, throughput_at_8k, throughput_at_16k, throughput_at_32k, throughput_at_64k, \
          recommended_timeout_chat_sec, recommended_timeout_build_sec, recommended_timeout_deep_sec, \
          overall_tier) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(profile_id)
    .bind(p.max_context_safe)
    .bind(p.max_context_absolute)
    .bind(p.quality_degradation_point)
    .bind(p.throughput_at_2k)
    .bind(p.throughput_at_8k)
    .bind(p.throughput_at_16k)
    .bind(p.throughput_at_32k)
    .bind(p.throughput_at_64k)
    .bind(p.recommended_timeout_chat_sec)
    .bind(p.recommended_timeout_build_sec)
    .bind(p.recommended_timeout_deep_sec)
    .bind(p.overall_tier.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert model_operational_profiles: {e}")))?;
    Ok(())
}

/// One measured code case. Mirrors `code_profile_runs` columns.
#[derive(Debug, Clone, Default)]
pub struct CodeRunRow {
    pub language: String,
    pub context_tokens: Option<i32>,
    pub file_count: Option<i32>,
    pub total_lines: Option<i32>,
    pub task_type: Option<String>,
    pub compiles: Option<bool>,
    pub tests_pass: Option<bool>,
    pub planted_bug_found: Option<bool>,
    pub code_quality_score: Option<f64>,
    pub throughput_tok_per_sec: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
    /// Which system configuration ran this case: 'gpu' or 'cpu' (the coder-side
    /// twin of `assistant_dimension_score.backend_tag`). `None` for rows written
    /// before this column existed, or by callers that don't yet track it.
    pub backend_tag: Option<String>,
    /// Which memory-model configuration ran this case, e.g. `"dynamic_gtt"`
    /// or `"carveout"` (mem-config-tagging sprint). `None` for rows written
    /// before this column existed (the preserved baseline) or by callers that
    /// don't yet track it — NEVER assume `None` means a specific config.
    pub mem_config: Option<String>,
}

/// Insert one `code_profile_runs` row.
pub async fn insert_code_run(
    pool: &PgPool,
    profile_id: Uuid,
    row: &CodeRunRow,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO code_profile_runs \
         (profile_id, language, context_tokens, file_count, total_lines, task_type, \
          compiles, tests_pass, planted_bug_found, code_quality_score, \
          throughput_tok_per_sec, total_time_ms, memory_usage_mb, oom, error, backend_tag, \
          mem_config) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
    )
    .bind(profile_id)
    .bind(&row.language)
    .bind(row.context_tokens)
    .bind(row.file_count)
    .bind(row.total_lines)
    .bind(row.task_type.as_deref())
    .bind(row.compiles)
    .bind(row.tests_pass)
    .bind(row.planted_bug_found)
    .bind(row.code_quality_score)
    .bind(row.throughput_tok_per_sec)
    .bind(row.total_time_ms)
    .bind(row.memory_usage_mb)
    .bind(row.oom)
    .bind(row.error.as_deref())
    .bind(row.backend_tag.as_deref())
    .bind(row.mem_config.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert code_profile_runs: {e}")))?;
    Ok(())
}

/// One measured code case from the REALISTIC build-scenario harness (v2).
/// Stored in `code_profile_runs` with `harness_version='v2'` so it never mixes
/// with the v1 one-shot rows. Mirrors the v2 columns added to the table.
#[derive(Debug, Clone, Default)]
pub struct CodeRunRowV2 {
    pub language: String,
    pub task_type: Option<String>,
    /// Graduated 0-5 quality of the FIRST attempt.
    pub first_pass_score: Option<i32>,
    /// 0-5 score of the retry (only when first_pass was 1-2; else NULL).
    pub retry_score: Option<i32>,
    pub compiles: Option<bool>,
    pub tests_pass: Option<bool>,
    /// Independent change-present/behavior check passed.
    pub change_correct: Option<bool>,
    /// LLM-judge idiom/style rating 1-5 (NULL if judge unavailable).
    pub code_quality_score: Option<f64>,
    pub context_tokens: Option<i32>,
    pub response_tokens: Option<i32>,
    pub file_count: Option<i32>,
    pub total_lines: Option<i32>,
    pub throughput_tok_per_sec: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
    /// Which system configuration ran this case: 'gpu' or 'cpu' (the coder-side
    /// twin of `assistant_dimension_score.backend_tag`). `None` for rows written
    /// before this column existed, or by callers that don't yet track it.
    pub backend_tag: Option<String>,
    /// Which memory-model configuration ran this case, e.g. `"dynamic_gtt"`
    /// or `"carveout"` (mem-config-tagging sprint). `None` for rows written
    /// before this column existed (the preserved baseline) or by callers that
    /// don't yet track it — NEVER assume `None` means a specific config.
    pub mem_config: Option<String>,
}

/// Insert one v2 `code_profile_runs` row (harness_version='v2'). Additive — the
/// v1 columns it does not populate are left NULL.
pub async fn insert_code_run_v2(
    pool: &PgPool,
    profile_id: Uuid,
    row: &CodeRunRowV2,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO code_profile_runs \
         (profile_id, harness_version, language, task_type, \
          first_pass_score, retry_score, compiles, tests_pass, change_correct, \
          code_quality_score, context_tokens, response_tokens, file_count, total_lines, \
          throughput_tok_per_sec, total_time_ms, oom, error, backend_tag, mem_config) \
         VALUES ($1, 'v2', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
    )
    .bind(profile_id)
    .bind(&row.language)
    .bind(row.task_type.as_deref())
    .bind(row.first_pass_score)
    .bind(row.retry_score)
    .bind(row.compiles)
    .bind(row.tests_pass)
    .bind(row.change_correct)
    .bind(row.code_quality_score)
    .bind(row.context_tokens)
    .bind(row.response_tokens)
    .bind(row.file_count)
    .bind(row.total_lines)
    .bind(row.throughput_tok_per_sec)
    .bind(row.total_time_ms)
    .bind(row.oom)
    .bind(row.error.as_deref())
    .bind(row.backend_tag.as_deref())
    .bind(row.mem_config.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert code_profile_runs (v2): {e}")))?;
    Ok(())
}

/// One measured agent scenario. Mirrors `agent_profile_runs` columns.
#[derive(Debug, Clone, Default)]
pub struct AgentRunRow {
    pub test_name: String,
    pub tool_count_available: Option<i32>,
    pub correct_tool_selected: Option<bool>,
    pub tool_params_valid: Option<bool>,
    pub multi_step_completed: Option<bool>,
    pub instruction_followed: Option<bool>,
    pub hallucination_detected: Option<bool>,
    pub response_quality_score: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub error: Option<String>,
}

/// Insert one `agent_profile_runs` row.
pub async fn insert_agent_run(
    pool: &PgPool,
    profile_id: Uuid,
    row: &AgentRunRow,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO agent_profile_runs \
         (profile_id, test_name, tool_count_available, correct_tool_selected, tool_params_valid, \
          multi_step_completed, instruction_followed, hallucination_detected, \
          response_quality_score, total_time_ms, error) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(profile_id)
    .bind(&row.test_name)
    .bind(row.tool_count_available)
    .bind(row.correct_tool_selected)
    .bind(row.tool_params_valid)
    .bind(row.multi_step_completed)
    .bind(row.instruction_followed)
    .bind(row.hallucination_detected)
    .bind(row.response_quality_score)
    .bind(row.total_time_ms)
    .bind(row.error.as_deref())
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert agent_profile_runs: {e}")))?;
    Ok(())
}

/// Ensure an operational-profile row exists for `profile_id`. The context suite
/// normally inserts it; code/agent-only runs need an empty row to patch.
pub async fn ensure_operational_profile(pool: &PgPool, profile_id: Uuid) -> Result<(), ToolError> {
    let exists: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM model_operational_profiles WHERE profile_id = $1 LIMIT 1",
    )
    .bind(profile_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to check operational profile: {e}")))?;
    if exists.is_none() {
        sqlx::query("INSERT INTO model_operational_profiles (profile_id) VALUES ($1)")
            .bind(profile_id)
            .execute(pool)
            .await
            .map_err(|e| ToolError::Database(format!("Failed to seed operational profile: {e}")))?;
    }
    Ok(())
}

/// Patch the latest operational-profile row for `profile_id` with the code-suite
/// approval set. `approved_languages` is a list of "lang:complexity" tags.
pub async fn update_op_code(
    pool: &PgPool,
    profile_id: Uuid,
    approved_languages: &[String],
    max_files_good: Option<i32>,
    max_files_marginal: Option<i32>,
) -> Result<(), ToolError> {
    ensure_operational_profile(pool, profile_id).await?;
    sqlx::query(
        "UPDATE model_operational_profiles \
         SET approved_languages = $2, max_files_good = $3, max_files_marginal = $4 \
         WHERE profile_id = $1",
    )
    .bind(profile_id)
    .bind(approved_languages)
    .bind(max_files_good)
    .bind(max_files_marginal)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to update op (code): {e}")))?;
    Ok(())
}

/// Patch the latest operational-profile row with agent-suite aggregates.
pub async fn update_op_agent(
    pool: &PgPool,
    profile_id: Uuid,
    tool_accuracy: Option<f64>,
    multistep_accuracy: Option<f64>,
) -> Result<(), ToolError> {
    ensure_operational_profile(pool, profile_id).await?;
    sqlx::query(
        "UPDATE model_operational_profiles \
         SET agent_tool_accuracy = $2, agent_multistep_accuracy = $3 \
         WHERE profile_id = $1",
    )
    .bind(profile_id)
    .bind(tool_accuracy)
    .bind(multistep_accuracy)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to update op (agent): {e}")))?;
    Ok(())
}

/// A single context tier row read back for status/compare.
#[derive(Debug, Clone)]
pub struct StoredContextRun {
    pub context_tokens: i32,
    pub throughput_tok_per_sec: Option<f64>,
    pub ttft_ms: Option<i32>,
    pub recall_score: Option<i32>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
}

/// The most-recent stored profile for a model: the operational summary plus its
/// context tier rows. `None` if the model has never been profiled.
#[derive(Debug, Clone)]
pub struct StoredProfile {
    pub model_name: String,
    pub op: OperationalProfileRow,
    pub tiers: Vec<StoredContextRun>,
}

/// Read the most-recent stored profile for a model, or `None` if not profiled.
pub async fn read_latest_profile(
    pool: &PgPool,
    model_name: &str,
) -> Result<Option<StoredProfile>, ToolError> {
    // Most-recent model_profiles row for this model.
    let prof: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM model_profiles \
         WHERE model_name = $1 ORDER BY profile_date DESC LIMIT 1",
    )
    .bind(model_name)
    .fetch_optional(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to query model_profiles: {e}")))?;

    let Some((profile_id,)) = prof else {
        return Ok(None);
    };

    let op_row: Option<(
        Option<i32>, Option<i32>, Option<i32>,
        Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>,
        Option<i32>, Option<i32>, Option<i32>, Option<String>,
    )> = sqlx::query_as(
        "SELECT max_context_safe, max_context_absolute, quality_degradation_point, \
                throughput_at_2k, throughput_at_8k, throughput_at_16k, throughput_at_32k, throughput_at_64k, \
                recommended_timeout_chat_sec, recommended_timeout_build_sec, recommended_timeout_deep_sec, \
                overall_tier \
         FROM model_operational_profiles WHERE profile_id = $1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(profile_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to query operational profile: {e}")))?;

    let op = match op_row {
        Some(r) => OperationalProfileRow {
            max_context_safe: r.0,
            max_context_absolute: r.1,
            quality_degradation_point: r.2,
            throughput_at_2k: r.3,
            throughput_at_8k: r.4,
            throughput_at_16k: r.5,
            throughput_at_32k: r.6,
            throughput_at_64k: r.7,
            recommended_timeout_chat_sec: r.8,
            recommended_timeout_build_sec: r.9,
            recommended_timeout_deep_sec: r.10,
            overall_tier: r.11,
        },
        None => OperationalProfileRow::default(),
    };

    let tier_rows: Vec<(i32, Option<f64>, Option<i32>, Option<i32>, Option<i32>, bool)> =
        sqlx::query_as(
            "SELECT context_tokens, throughput_tok_per_sec, ttft_ms, recall_score, memory_usage_mb, oom \
             FROM context_profile_runs WHERE profile_id = $1 ORDER BY context_tokens ASC",
        )
        .bind(profile_id)
        .fetch_all(pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to query context runs: {e}")))?;

    let tiers = tier_rows
        .into_iter()
        .map(|(context_tokens, throughput_tok_per_sec, ttft_ms, recall_score, memory_usage_mb, oom)| {
            StoredContextRun {
                context_tokens,
                throughput_tok_per_sec,
                ttft_ms,
                recall_score,
                memory_usage_mb,
                oom,
            }
        })
        .collect();

    Ok(Some(StoredProfile {
        model_name: model_name.to_string(),
        op,
        tiers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_pool_missing_db_url_errors() {
        std::env::remove_var("DATABASE_URL");
        let r = get_pool().await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    /// mem-config-tagging: `mem_config` defaults to `None` (never silently
    /// mislabels a row written by a caller that doesn't set it) and is
    /// settable like any other field on both row structs.
    #[test]
    fn code_run_row_mem_config_defaults_none_and_is_settable() {
        let default_row = CodeRunRow::default();
        assert_eq!(default_row.mem_config, None);

        let tagged_row = CodeRunRow {
            mem_config: Some("dynamic_gtt".to_string()),
            ..Default::default()
        };
        assert_eq!(tagged_row.mem_config.as_deref(), Some("dynamic_gtt"));
    }

    #[test]
    fn code_run_row_v2_mem_config_defaults_none_and_is_settable() {
        let default_row = CodeRunRowV2::default();
        assert_eq!(default_row.mem_config, None);

        let tagged_row = CodeRunRowV2 {
            mem_config: Some("carveout".to_string()),
            ..Default::default()
        };
        assert_eq!(tagged_row.mem_config.as_deref(), Some("carveout"));
    }
}
