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
    /// The v2 corpus manifest's unique case id (HFIX-06) — lets a gap audit
    /// identify WHICH specific case a row came from, not just its
    /// language/task_type. `None` for rows written before this column
    /// existed.
    pub case_id: Option<String>,
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

/// SQL for [`insert_code_run_v2`]. Pulled out to a const so a unit test can
/// assert, without a live DB, that the Phase-1 insert explicitly writes
/// `finalized = false` (S86 INCR-01 hardening) rather than silently relying
/// on the column's `DEFAULT true` — which exists ONLY to backfill
/// pre-existing (already-complete) legacy rows, not to describe a brand-new
/// Phase-1-only row. See the `finalized` column's doc comment in
/// `assistant/schema.rs::migrate_locked` for the full rationale.
const INSERT_CODE_RUN_V2_SQL: &str = "INSERT INTO code_profile_runs \
     (profile_id, harness_version, language, task_type, \
      first_pass_score, retry_score, compiles, tests_pass, change_correct, \
      code_quality_score, context_tokens, response_tokens, file_count, total_lines, \
      throughput_tok_per_sec, total_time_ms, oom, error, backend_tag, mem_config, case_id, \
      finalized) \
     VALUES ($1, 'v2', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
     false) \
     RETURNING id";

/// Insert one v2 `code_profile_runs` row (harness_version='v2'). Additive — the
/// v1 columns it does not populate are left NULL.
///
/// INCR-01: returns the new row's id so a caller doing incremental
/// (per-case, not per-model-batch) persistence can patch the row later
/// (see `update_code_run_v2_judge`) once the deferred idiom judge runs. Also
/// always writes `finalized = false` (S86 hardening) — this row is Phase-1
/// only at insert time; `update_code_run_v2_judge` marks it `true` once the
/// case reaches its true end.
/// Kept as a thin, backwards-compatible wrapper is NOT done here — this
/// function's signature changed (Result<Uuid, _> instead of Result<(), _>);
/// its one caller (`code_v2.rs::run_code_suite_v2_cases`) is updated in the
/// same change.
pub async fn insert_code_run_v2(
    pool: &PgPool,
    profile_id: Uuid,
    row: &CodeRunRowV2,
) -> Result<Uuid, ToolError> {
    let rec = sqlx::query_scalar::<_, Uuid>(INSERT_CODE_RUN_V2_SQL)
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
        .bind(row.case_id.as_deref())
        .fetch_one(pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to insert code_profile_runs (v2): {e}")))?;
    Ok(rec)
}

/// SQL for [`update_code_run_v2_judge`]. Pulled out to a const so a unit test
/// can assert, without a live DB, that this statement ALWAYS sets
/// `finalized = true` — this is the finalization point for every case (see
/// the function doc below for why it must be unconditional).
const UPDATE_CODE_RUN_V2_JUDGE_SQL: &str = "UPDATE code_profile_runs \
     SET code_quality_score = $2, first_pass_score = $3, retry_score = $4, finalized = true \
     WHERE id = $1";

/// INCR-01 + S86 hardening: patch a previously-inserted v2 row with the
/// batched idiom-judge result AND mark it `finalized = true`. Called from
/// Phase 2 of `run_code_suite_v2_cases` for EVERY case (judged or not) —
/// inference (Phase 1) now persists each case's row immediately (so
/// `code_profile_runs` gets a steady trickle of rows instead of one 40-row
/// burst at the very end of a model's suite), and this patches in
/// `code_quality_score` plus the judge-driven 4→5 score bump when a judge
/// pass ran, while ALSO being the one place every case's row reaches its true
/// completion marker regardless of whether judging applied to it (a case
/// with no `first_response`/`retry_response` still needs `finalized = true`,
/// or `coder_gaps.rs`'s gap audit would treat it as forever-incomplete even
/// though the suite is done with it). No-op-safe: updates by primary key, so
/// a row that no longer exists (should never happen in the normal single
/// in-flight-attempt-at-a-time flow) simply matches zero rows rather than
/// erroring.
pub async fn update_code_run_v2_judge(
    pool: &PgPool,
    id: Uuid,
    code_quality_score: Option<f64>,
    first_pass_score: Option<i32>,
    retry_score: Option<i32>,
) -> Result<(), ToolError> {
    sqlx::query(UPDATE_CODE_RUN_V2_JUDGE_SQL)
        .bind(id)
        .bind(code_quality_score)
        .bind(first_pass_score)
        .bind(retry_score)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to update code_profile_runs judge (v2): {e}")))?;
    Ok(())
}

/// SQL for [`delete_unfinalized_code_runs_v2`]. Pulled out to a const so a
/// unit test can assert, without a live DB, the exact shape of this
/// production-data-deleting statement: scoped by `model_profiles.model_name`
/// (joined via `profile_id`), `code_profile_runs.backend_tag`, and
/// `code_profile_runs.mem_config` (the last two compared with
/// `IS NOT DISTINCT FROM` so a `NULL` `mem_config` matches `NULL`, not "no
/// rows"), AND requiring `finalized = false` so an already-complete row —
/// even one for this exact (model, backend, mem_config) from a genuinely
/// finished prior run — is never touched. Also scoped to `harness_version =
/// 'v2'` as a belt-and-suspenders guard (v1 rows are never written with
/// `finalized = false` in the first place, since only `insert_code_run_v2`
/// sets that column explicitly).
const DELETE_UNFINALIZED_CODE_RUNS_V2_SQL: &str = "DELETE FROM code_profile_runs r \
     USING model_profiles p \
     WHERE r.profile_id = p.id \
       AND p.model_name = $1 \
       AND r.harness_version = 'v2' \
       AND r.backend_tag = $2 \
       AND r.mem_config IS NOT DISTINCT FROM $3 \
       AND r.finalized = false";

/// S86 hardening: reconcile orphaned incomplete rows left behind by a prior
/// crashed/killed attempt at this EXACT `(model_id, backend_tag, mem_config)`
/// combination, before `run_one_backend` creates a fresh `profile_id` and
/// starts a brand-new attempt. Without this, a process kill between Phase 1
/// (row insert) and the per-model checkpoint mark (written only after the
/// WHOLE suite returns `Ok`) leaves those Phase-1-only rows sitting in
/// `code_profile_runs` forever — every restart starts a fresh `profile_id`
/// and re-runs every case from scratch, so the orphaned rows just accumulate
/// with each crash/restart instead of ever being cleaned up or reconciled.
///
/// Deliberately narrow: only rows for THIS model/backend/mem_config AND only
/// `finalized = false` rows are deleted. Returns the number of rows deleted
/// (for logging/observability — callers are not required to act on it).
pub async fn delete_unfinalized_code_runs_v2(
    pool: &PgPool,
    model_id: &str,
    backend_tag: &str,
    mem_config: Option<&str>,
) -> Result<u64, ToolError> {
    let result = sqlx::query(DELETE_UNFINALIZED_CODE_RUNS_V2_SQL)
        .bind(model_id)
        .bind(backend_tag)
        .bind(mem_config)
        .execute(pool)
        .await
        .map_err(|e| {
            ToolError::Database(format!("Failed to delete orphaned unfinalized code_profile_runs rows: {e}"))
        })?;
    Ok(result.rows_affected())
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
    // Self-healing schema guard: `model_operational_profiles` was assumed to be
    // an S83 pre-existing table (per this module's doc comment), but on at
    // least one deployed DB it never actually existed — every call here
    // silently errored, which made `update_op_code`/`update_op_agent` fail,
    // which in turn made `run_code_suite_v2` return `Err` AFTER its rows were
    // already durably persisted — so `intake_coder_sweep`'s resume checkpoint
    // was never marked (S86 / ORC-297). Idempotent `CREATE TABLE IF NOT
    // EXISTS`, so this is a no-op once the table exists.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS model_operational_profiles ( \
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
            profile_id UUID NOT NULL, \
            max_context_safe INTEGER, \
            max_context_absolute INTEGER, \
            quality_degradation_point INTEGER, \
            throughput_at_2k DOUBLE PRECISION, \
            throughput_at_8k DOUBLE PRECISION, \
            throughput_at_16k DOUBLE PRECISION, \
            throughput_at_32k DOUBLE PRECISION, \
            throughput_at_64k DOUBLE PRECISION, \
            recommended_timeout_chat_sec INTEGER, \
            recommended_timeout_build_sec INTEGER, \
            recommended_timeout_deep_sec INTEGER, \
            overall_tier TEXT, \
            approved_languages TEXT[], \
            max_files_good INTEGER, \
            max_files_marginal INTEGER, \
            agent_tool_accuracy DOUBLE PRECISION, \
            agent_multistep_accuracy DOUBLE PRECISION, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now() \
         )",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create model_operational_profiles: {e}")))?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_model_op_profiles_profile_id \
         ON model_operational_profiles(profile_id)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_model_op_profiles_profile_id: {e}")))?;

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

    // ---- S86 INCR-01 hardening: `finalized` column wiring -------------

    /// Phase-1 insert must explicitly write `finalized = false` (never rely
    /// on the column's `DEFAULT true`, which exists only to backfill
    /// pre-existing legacy rows as already-complete).
    #[test]
    fn insert_code_run_v2_sql_explicitly_sets_finalized_false() {
        assert!(INSERT_CODE_RUN_V2_SQL.contains("finalized"));
        // The 21st bind param position ($20) is the last VALUES entry before
        // the literal `false` for `finalized` — confirming the insert never
        // falls through to the column's `DEFAULT true`.
        assert!(
            INSERT_CODE_RUN_V2_SQL.contains("$20, false) RETURNING id"),
            "expected the VALUES list to end with an explicit `false` for \
             `finalized`, got: {INSERT_CODE_RUN_V2_SQL}"
        );
    }

    /// The judge-patch statement is the finalization point for every case —
    /// it must set `finalized = true` unconditionally (the caller now invokes
    /// it for every case, judged or not).
    #[test]
    fn update_code_run_v2_judge_sql_sets_finalized_true() {
        assert!(UPDATE_CODE_RUN_V2_JUDGE_SQL.contains("finalized = true"));
    }

    /// The startup-cleanup delete must scope by model_name + backend_tag +
    /// mem_config (NULL-safe) AND only ever touch `finalized = false` rows —
    /// this is the exact predicate that keeps it from ever deleting a
    /// genuinely-completed row, even one for the same model/backend/mem_config.
    #[test]
    fn delete_unfinalized_sql_scopes_by_key_and_only_targets_unfinalized_rows() {
        let sql = DELETE_UNFINALIZED_CODE_RUNS_V2_SQL;
        assert!(sql.contains("p.model_name = $1"));
        assert!(sql.contains("r.backend_tag = $2"));
        assert!(sql.contains("r.mem_config IS NOT DISTINCT FROM $3"));
        assert!(sql.contains("r.finalized = false"));
        assert!(sql.contains("harness_version = 'v2'"));
        // Must join through model_profiles so the DELETE cannot match rows
        // belonging to a different model_id sharing the same backend/mem_config.
        assert!(sql.contains("USING model_profiles p"));
        assert!(sql.contains("r.profile_id = p.id"));
    }
}
