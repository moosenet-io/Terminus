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
//! `get_pool()` connects a fresh `PgPool`. The intake run is infrequent
//! (manual / onboarding), so a per-call pool is acceptable.

use sqlx::PgPool;
use uuid::Uuid;

use crate::config;
use crate::error::ToolError;

/// Connect a Postgres pool. Prefers `INTAKE_DATABASE_URL`, falls back to
/// `DATABASE_URL` — Phase 2 item 6: this now matches
/// `assistant::schema::get_pool()`'s (the production-confirmed) precedence
/// via the SAME `config::intake_database_url()` resolver, instead of reading
/// `DATABASE_URL` only. Before this fix, a host with ONLY
/// `INTAKE_DATABASE_URL` set (no `DATABASE_URL`) would have this module's
/// callers fail to connect while `assistant::schema`'s callers, on the exact
/// same shared DB, connected fine.
pub async fn get_pool() -> Result<PgPool, ToolError> {
    let url = config::intake_database_url().ok_or_else(|| {
        ToolError::NotConfigured(
            "neither INTAKE_DATABASE_URL nor DATABASE_URL set — model intake requires a Postgres connection".into(),
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

/// Insert one `context_profile_runs` row, returning its generated id.
///
/// Returns the new row's `id` (multi-point-score-tracking) so a caller can
/// attach per-tier [`ScorePoint`]s to it via [`insert_score_points`]. Same
/// additive-return-of-id pattern as [`insert_code_run_v2`]. The insert itself
/// is unchanged — only the return type gained the row id.
pub async fn insert_context_run(
    pool: &PgPool,
    profile_id: Uuid,
    row: &ContextRunRow,
) -> Result<Uuid, ToolError> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO context_profile_runs \
         (profile_id, context_tokens, throughput_tok_per_sec, ttft_ms, total_time_ms, \
          recall_score, coherence_score, memory_usage_mb, oom, error) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         RETURNING id",
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
    .fetch_one(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert context_profile_runs: {e}")))?;
    Ok(id)
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

/// One measured code case from the REALISTIC build-scenario harness. Stored in
/// `code_profile_runs` with `harness_version='v3'` (MINT2-01 bumped the
/// build-scenario epoch from `'v2'`, the measurement-corrected harness that
/// records the tunable factors below) so it never mixes with the v1 one-shot
/// rows or the pre-correction v2 rows. Mirrors the columns on the table.
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
    /// Whether the model produced at least one extractable/mapped output file
    /// (multi-point-score-tracking). Set to `Some(produced)` by `code_v2.rs`
    /// BEFORE the graduated score is computed, so a 0 score from "nothing
    /// extracted" (`Some(false)`) is distinguishable from "extracted but wrong"
    /// (`Some(true)`). `None` for rows written before this column existed or by
    /// callers that don't track it.
    pub well_formed: Option<bool>,
    /// Which repeat of a multi-sample case this row is (multi-sample-consistency).
    /// When `INTAKE_SAMPLES_PER_CASE > 1`, the sweep runs each case N times and
    /// writes N rows sharing the same `case_id` but incrementing `sample_index`
    /// (0..N-1); the pass@k / pass^k estimators aggregate over the repeats of a
    /// case. `0` for every single-sample row (the DB column defaults to `0`), so
    /// legacy/single-run data reads as "sample 0 of 1" without a backfill.
    pub sample_index: i16,
    /// Heuristic security signal (security-scan-signal): count of
    /// vulnerability-pattern findings in the case's generated output, from the
    /// dependency-free heuristic scanner in `intake::vuln_scan`. `Some(0)` =
    /// scanned, none of the known-bad patterns present; `Some(N)` = N findings;
    /// `None` = NOT scanned (language unsupported by the heuristic, or no code
    /// was produced). SEPARATE signal — it never affects the correctness score.
    /// See `vuln_scan` for the honest caveats: coarse heuristic, not real SAST.
    pub vuln_finding_count: Option<i32>,
    // ---- MINT2-01: tunable measurement factors ----------------------------
    // The knobs a harness would actually turn, recorded as first-class columns
    // so pass-rate can be analyzed against the config that was set, not just the
    // model name. Written on new `'v3'` rows; legacy `'v1'`/`'v2'` rows have
    // NULL for all six (they belong to a prior epoch). `None` here writes SQL
    // NULL, and the read path (`read_code_run_factors`) tolerates the columns
    // being ABSENT on an un-migrated DB — a missing column reads as `None`,
    // never a panic.
    /// Quantization the weights ran at, e.g. `"Q4_K_M"`/`"Q6_K"`/`"fp16"`. On a
    /// `'v3'` write this is `Some("unknown")` when the model nomination / launch
    /// flags don't declare a quant — NEVER guessed — so a genuine "we didn't
    /// know" is distinguishable from a legacy row's NULL. `None` only for rows
    /// written by a caller that doesn't set it (the preserved default).
    pub quant: Option<String>,
    /// Three-state reasoning/thinking flag the runtime was launched with:
    /// `Some(true)` = on, `Some(false)` = off, `None` = unset (the runtime
    /// doesn't expose it / not configured). Never coerce "unset" to `false`.
    pub reasoning_enabled: Option<bool>,
    /// The context window the runtime was LAUNCHED with (the `-c` / `num_ctx`),
    /// distinct from the observed `context_tokens` a given prompt actually used.
    /// `None` when the launch window wasn't configured for this run.
    pub context_window_launched: Option<i32>,
    /// Sampling temperature the run was configured with. `None` = the runtime
    /// default was used (not recorded as a specific value).
    pub temperature: Option<f64>,
    /// Sampling top-p the run was configured with. `None` = runtime default.
    pub top_p: Option<f64>,
    /// The case's declared task category (`"blitz"`/`"multi_file"`/`"deep"`),
    /// promoted from a chart-time bucket over `file_count` to a stored factor.
    /// Recorded FROM THE CORPUS MANIFEST tier on the write path, never
    /// re-derived from `file_count`. `None` for rows written before this column
    /// existed / by callers that don't set it.
    pub task_category: Option<String>,
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
      well_formed, sample_index, vuln_finding_count, \
      quant, reasoning_enabled, context_window_launched, temperature, top_p, task_category, finalized) \
     VALUES ($1, 'v3', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, \
     $24, $25, $26, $27, $28, $29, false) \
     RETURNING id";

/// Insert one build-scenario `code_profile_runs` row (harness_version='v3',
/// MINT2-01). Additive — the columns it does not populate are left NULL.
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
        .bind(row.well_formed)
        .bind(row.sample_index)
        .bind(row.vuln_finding_count)
        .bind(row.quant.as_deref())
        .bind(row.reasoning_enabled)
        .bind(row.context_window_launched)
        .bind(row.temperature)
        .bind(row.top_p)
        .bind(row.task_category.as_deref())
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
/// though the suite is done with it). Updates by primary key, so a row that
/// no longer exists (should never happen in the normal single
/// in-flight-attempt-at-a-time flow, but could follow some out-of-band
/// deletion/race) is NOT treated as success: this returns `Err` unless
/// exactly one row was affected, so a missing row can never silently
/// "checkpoint as complete" with nothing actually persisted. The caller
/// (`code_v2.rs`'s judge loop) treats that `Err` as a per-case skip, matching
/// this codebase's skip-with-reason convention rather than aborting the
/// whole model's sweep.
pub async fn update_code_run_v2_judge(
    pool: &PgPool,
    id: Uuid,
    code_quality_score: Option<f64>,
    first_pass_score: Option<i32>,
    retry_score: Option<i32>,
) -> Result<(), ToolError> {
    let result = sqlx::query(UPDATE_CODE_RUN_V2_JUDGE_SQL)
        .bind(id)
        .bind(code_quality_score)
        .bind(first_pass_score)
        .bind(retry_score)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to update code_profile_runs judge (v2): {e}")))?;
    check_judge_update_affected_one_row(id, result.rows_affected())
}

/// Pure decision extracted from [`update_code_run_v2_judge`] so it can be
/// unit-tested without a live Postgres connection (this crate has no
/// integration-test DB harness — every other test on this function's
/// neighbors asserts against SQL string constants for the same reason):
/// exactly one row affected is success, anything else (0 = row
/// missing/deleted, >1 would mean the `WHERE id = $1` predicate somehow
/// matched more than the primary key) is an error, so the caller can never
/// mistake a no-op for a completed checkpoint.
fn check_judge_update_affected_one_row(id: Uuid, rows_affected: u64) -> Result<(), ToolError> {
    if rows_affected != 1 {
        return Err(ToolError::Database(format!(
            "update_code_run_v2_judge: expected to update exactly 1 row for id={id}, but {rows_affected} rows were affected (row missing/deleted?)"
        )));
    }
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
/// 'v3'` as a belt-and-suspenders guard: MINT2-01 bumped the build-scenario
/// write epoch to `'v3'`, so a fresh attempt's orphaned Phase-1 rows are now
/// `'v3'`, and this reconcile must target that same epoch (v1 rows are never
/// written with `finalized = false` in the first place, since only
/// `insert_code_run_v2` sets that column explicitly, and pre-correction v2
/// rows belong to a closed epoch that is never re-attempted).
const DELETE_UNFINALIZED_CODE_RUNS_V2_SQL: &str = "DELETE FROM code_profile_runs r \
     USING model_profiles p \
     WHERE r.profile_id = p.id \
       AND p.model_name = $1 \
       AND r.harness_version = 'v3' \
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

// ===========================================================================
// MINT2-01: read the tunable measurement factors back (null-tolerant)
// ===========================================================================

/// The MINT2-01 measurement factors read back from one `code_profile_runs`
/// row. Every field is optional and independently nullable: a legacy
/// (`'v1'`/`'v2'`) row has NULL for all six (they predate the columns), and an
/// UN-MIGRATED DB (the columns don't exist yet) reads as all-`None` via the
/// fallback query below — a missing column is never a panic. Round-trips a
/// [`CodeRunRowV2`]'s factor fields for a `'v3'` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeRunFactors {
    pub quant: Option<String>,
    pub reasoning_enabled: Option<bool>,
    pub context_window_launched: Option<i32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub task_category: Option<String>,
}

/// The row-shape the factor SELECTs decode into (kept as a type alias so the
/// primary and fallback queries decode identically and the pure mapper is
/// trivially unit-testable without a live DB).
type FactorTuple = (
    Option<String>,
    Option<bool>,
    Option<i32>,
    Option<f64>,
    Option<f64>,
    Option<String>,
);

/// Pure mapping from the decoded row tuple to [`CodeRunFactors`]. Extracted so a
/// unit test can prove the round-trip (all-set → same values) and the
/// null-tolerant path (all-NULL → all-`None`) without a Postgres connection —
/// this crate has no live-DB test harness (see the SQL-constant tests on this
/// module's other writers for the same constraint).
fn map_factor_row(t: FactorTuple) -> CodeRunFactors {
    CodeRunFactors {
        quant: t.0,
        reasoning_enabled: t.1,
        context_window_launched: t.2,
        temperature: t.3,
        top_p: t.4,
        task_category: t.5,
    }
}

/// Primary factor SELECT — references the MINT2-01 columns directly. Errors on
/// an un-migrated DB where those columns don't exist yet; [`read_code_run_factors`]
/// catches that and retries [`SELECT_CODE_RUN_FACTORS_FALLBACK_SQL`].
const SELECT_CODE_RUN_FACTORS_SQL: &str = "SELECT quant, reasoning_enabled, \
     context_window_launched, temperature, top_p, task_category \
     FROM code_profile_runs WHERE id = $1";

/// Fallback factor SELECT for an UN-MIGRATED DB (the six columns are absent):
/// selects correctly-typed SQL NULLs so the row still decodes into an
/// all-`None` [`CodeRunFactors`] instead of erroring on the missing columns.
/// Still gated on `WHERE id = $1` so a non-existent row yields no row (→
/// default), matching the migrated path exactly.
const SELECT_CODE_RUN_FACTORS_FALLBACK_SQL: &str = "SELECT NULL::text, NULL::boolean, \
     NULL::integer, NULL::double precision, NULL::double precision, NULL::text \
     FROM code_profile_runs WHERE id = $1";

/// True when a Postgres error text indicates a MISSING COLUMN (the un-migrated
/// schema case), so the read path can fall back to the NULL-only query rather
/// than propagating the error. Postgres reports `error: column "quant" does not
/// exist` (SQLSTATE 42703); matching the phrase keeps this dependency-free and
/// unit-testable. Pure over its input.
fn is_missing_column_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("column") && m.contains("does not exist")
}

/// Read the MINT2-01 measurement factors for one `code_profile_runs` row,
/// tolerating an un-migrated DB. Tries [`SELECT_CODE_RUN_FACTORS_SQL`]; if that
/// fails specifically because the columns don't exist yet
/// ([`is_missing_column_error`]), retries the NULL-typed
/// [`SELECT_CODE_RUN_FACTORS_FALLBACK_SQL`] so the caller gets all-`None`
/// instead of an error — the harness keeps running against a DB the migration
/// hasn't reached. A genuinely missing row (id not present) yields
/// [`CodeRunFactors::default`], NOT an error. Any OTHER DB error (connection
/// loss, etc.) is propagated, never masked as "no factors".
pub async fn read_code_run_factors(
    pool: &PgPool,
    id: Uuid,
) -> Result<CodeRunFactors, ToolError> {
    match sqlx::query_as::<_, FactorTuple>(SELECT_CODE_RUN_FACTORS_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
    {
        Ok(row) => Ok(row.map(map_factor_row).unwrap_or_default()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_column_error(&msg) {
                let row = sqlx::query_as::<_, FactorTuple>(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL)
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| {
                        ToolError::Database(format!(
                            "Failed to read code_profile_runs factors (fallback): {e}"
                        ))
                    })?;
                Ok(row.map(map_factor_row).unwrap_or_default())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read code_profile_runs factors: {msg}"
                )))
            }
        }
    }
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

/// Insert one `agent_profile_runs` row, returning its generated id.
///
/// Returns the new row's `id` (multi-point-score-tracking) so a caller can
/// attach per-band [`ScorePoint`]s to it via [`insert_score_points`]. Same
/// additive-return-of-id pattern as [`insert_code_run_v2`]. The insert itself
/// is unchanged — only the return type gained the row id.
pub async fn insert_agent_run(
    pool: &PgPool,
    profile_id: Uuid,
    row: &AgentRunRow,
) -> Result<Uuid, ToolError> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agent_profile_runs \
         (profile_id, test_name, tool_count_available, correct_tool_selected, tool_params_valid, \
          multi_step_completed, instruction_followed, hallucination_detected, \
          response_quality_score, total_time_ms, error) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         RETURNING id",
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
    .fetch_one(pool)
    .await
    .map_err(|e| ToolError::Database(format!("Failed to insert agent_profile_runs: {e}")))?;
    Ok(id)
}

// ===========================================================================
// Multi-point score tracking (`run_score_points`)
//
// A long-format sidecar: every per-point measurement a suite computes along an
// axis (context-length tiers, tool-count bands, …) lands here, not just the
// handful that fit a fixed `model_operational_profiles` column. Additive — the
// operational-profile writes are unchanged.
// ===========================================================================

/// Which run a batch of [`ScorePoint`]s belongs to. Exactly one variant is
/// chosen per `insert_score_points` call; it decides which of the mutually-
/// exclusive `code_run_id` / `context_run_id` / `agent_run_id` FK columns is
/// set (the `run_score_points.one_parent` CHECK enforces exactly-one at the DB
/// layer). All three PKs are `UUID` (matching `code_profile_runs.id` and the
/// UUID convention across `model_operational_profiles`/`model_profiles`).
#[derive(Debug, Clone, Copy)]
pub enum ScorePointParent {
    Code(Uuid),
    Context(Uuid),
    Agent(Uuid),
}

/// One measured point along an axis (e.g. throughput at a context tier, tool
/// accuracy at a tool-count band). `value = None` writes SQL NULL — callers
/// skip a metric entirely rather than passing a `Some(0.0)` placeholder for a
/// value that was never measured.
#[derive(Debug, Clone)]
pub struct ScorePoint {
    pub axis: String,
    pub x_value: f64,
    pub x_label: Option<String>,
    pub metric: String,
    pub value: Option<f64>,
}

/// SQL for one `run_score_points` insert. A const so a unit test can assert its
/// shape without a live DB (this crate's testing convention — see the sibling
/// `INSERT_CODE_RUN_V2_SQL` tests). Batch inserts loop this inside one
/// transaction (below).
const INSERT_SCORE_POINT_SQL: &str = "INSERT INTO run_score_points \
     (code_run_id, context_run_id, agent_run_id, profile_id, axis, x_value, x_label, metric, value) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

/// Pure mapping from a [`ScorePointParent`] to the `(code_run_id,
/// context_run_id, agent_run_id)` column triple: exactly one is `Some`, the
/// other two `None`. Extracted so a unit test can prove the exactly-one
/// invariant (which the DB's `one_parent` CHECK also enforces) without a live
/// Postgres connection.
fn score_point_parent_columns(
    parent: ScorePointParent,
) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
    match parent {
        ScorePointParent::Code(id) => (Some(id), None, None),
        ScorePointParent::Context(id) => (None, Some(id), None),
        ScorePointParent::Agent(id) => (None, None, Some(id)),
    }
}

/// Batch-insert `points` against one parent run, all tagged with `profile_id`.
/// Runs inside a single transaction so a batch either fully lands or not at all
/// (never a half-written set of points for one tier/band). Empty `points` is a
/// no-op (no transaction opened). Returns `Err` on any DB error, matching the
/// idiom used by every other writer in this module.
pub async fn insert_score_points(
    pool: &PgPool,
    parent: ScorePointParent,
    profile_id: Uuid,
    points: &[ScorePoint],
) -> Result<(), ToolError> {
    if points.is_empty() {
        return Ok(());
    }
    let (code_run_id, context_run_id, agent_run_id) = score_point_parent_columns(parent);
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ToolError::Database(format!("Failed to begin run_score_points tx: {e}")))?;
    for p in points {
        sqlx::query(INSERT_SCORE_POINT_SQL)
            .bind(code_run_id)
            .bind(context_run_id)
            .bind(agent_run_id)
            .bind(profile_id)
            .bind(&p.axis)
            .bind(p.x_value)
            .bind(p.x_label.as_deref())
            .bind(&p.metric)
            .bind(p.value)
            .execute(&mut *tx)
            .await
            .map_err(|e| ToolError::Database(format!("Failed to insert run_score_points: {e}")))?;
    }
    tx.commit()
        .await
        .map_err(|e| ToolError::Database(format!("Failed to commit run_score_points tx: {e}")))?;
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

    // Serializes tests that mutate the shared DATABASE_URL/INTAKE_DATABASE_URL
    // process env vars — `cargo test` runs tests in the same process on
    // multiple threads by default.
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn get_pool_missing_db_url_errors() {
        std::env::remove_var("DATABASE_URL");
        std::env::remove_var("INTAKE_DATABASE_URL");
        let r = get_pool().await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    // The env-var PRECEDENCE order itself (INTAKE_DATABASE_URL wins, falls
    // back to DATABASE_URL) is exercised as a pure, network-free test against
    // `config::intake_database_url()` in `config.rs` — the exact resolver
    // `get_pool()` now delegates to (Phase 2 item 6) — rather than here via a
    // real `PgPool::connect()` attempt against a fake host, which would incur
    // a real (possibly slow/hanging) network/DNS round trip in this test.

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
        // The last bind param ($29 = `task_category`, MINT2-01, the last of the
        // six new measurement-factor binds $24..$29) is the final VALUES entry
        // before the literal `false` for `finalized` — confirming the insert
        // never falls through to the column's `DEFAULT true`.
        assert!(
            INSERT_CODE_RUN_V2_SQL.contains("$28, $29, false) RETURNING id"),
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

    /// Hardening (Codex review): a row that no longer exists — deleted out
    /// from under the judge loop, or an id that was never inserted — must
    /// surface as an `Err`, not silently succeed with zero effect. Exercised
    /// via `check_judge_update_affected_one_row`, the pure decision
    /// `update_code_run_v2_judge` delegates to after `.execute()`, since this
    /// crate has no live-Postgres test harness (see the sibling SQL-constant
    /// tests above for the same constraint).
    #[test]
    fn update_code_run_v2_judge_errors_when_zero_rows_affected() {
        let id = Uuid::new_v4();
        let r = check_judge_update_affected_one_row(id, 0);
        assert!(matches!(r, Err(ToolError::Database(_))));
        let msg = match r {
            Err(ToolError::Database(m)) => m,
            _ => unreachable!(),
        };
        assert!(msg.contains(&id.to_string()));
    }

    /// Sanity companion: the normal case (exactly one row touched) must stay
    /// `Ok`, so this guard never false-positives on a healthy update.
    #[test]
    fn update_code_run_v2_judge_ok_when_exactly_one_row_affected() {
        assert!(check_judge_update_affected_one_row(Uuid::new_v4(), 1).is_ok());
    }

    // ---- multi-point-score-tracking: `run_score_points` ----------------

    /// `well_formed` defaults to `None` (never mislabels a row from a writer
    /// that doesn't set it) and is settable like any other field.
    #[test]
    fn code_run_row_v2_well_formed_defaults_none_and_is_settable() {
        assert_eq!(CodeRunRowV2::default().well_formed, None);
        let row = CodeRunRowV2 { well_formed: Some(true), ..Default::default() };
        assert_eq!(row.well_formed, Some(true));
    }

    /// The insert names `sample_index` then `vuln_finding_count`, then the six
    /// MINT2-01 measurement-factor columns, then `finalized` last — so it never
    /// falls through to a column default.
    #[test]
    fn insert_code_run_v2_sql_includes_sample_index_and_vuln_count_last() {
        assert!(INSERT_CODE_RUN_V2_SQL.contains(
            "well_formed, sample_index, vuln_finding_count, \
      quant, reasoning_enabled, context_window_launched, temperature, top_p, task_category, finalized)"
        ));
    }

    /// `sample_index` defaults to `0` (single-sample / legacy rows read as
    /// "sample 0") and is settable like any other field.
    #[test]
    fn code_run_row_v2_sample_index_defaults_zero_and_is_settable() {
        assert_eq!(CodeRunRowV2::default().sample_index, 0);
        let row = CodeRunRowV2 { sample_index: 2, ..Default::default() };
        assert_eq!(row.sample_index, 2);
    }

    /// `vuln_finding_count` defaults to `None` (never mislabels a row from a
    /// writer that doesn't set it) and is settable like any other field.
    #[test]
    fn code_run_row_v2_vuln_finding_count_defaults_none_and_is_settable() {
        assert_eq!(CodeRunRowV2::default().vuln_finding_count, None);
        let row = CodeRunRowV2 { vuln_finding_count: Some(2), ..Default::default() };
        assert_eq!(row.vuln_finding_count, Some(2));
    }

    /// Column-count-vs-value-count guard (this file has a documented history of
    /// that class of bug). The explicit column list between the first `(` and
    /// `)` must have exactly as many entries as the `VALUES (...)` list has
    /// slots, where a slot is either a `$N` placeholder or a hardcoded literal
    /// (`'v2'` for harness_version, `false` for finalized). Counting `$N`
    /// placeholders plus literals and comparing to the comma-separated column
    /// names keeps INSERT_CODE_RUN_V2_SQL internally consistent without a DB.
    #[test]
    fn insert_code_run_v2_sql_column_count_matches_value_count() {
        let sql = INSERT_CODE_RUN_V2_SQL;
        // Column list: text inside the FIRST parenthesized group.
        let col_open = sql.find('(').expect("no column paren");
        let col_close = sql[col_open..].find(')').expect("no column close") + col_open;
        let cols = &sql[col_open + 1..col_close];
        let n_cols = cols.split(',').filter(|s| !s.trim().is_empty()).count();

        // Values list: text inside the `VALUES ( ... )` group.
        let v_start = sql.find("VALUES").expect("no VALUES");
        let v_open = sql[v_start..].find('(').expect("no values paren") + v_start;
        let v_close = sql[v_open..].find(')').expect("no values close") + v_open;
        let vals = &sql[v_open + 1..v_close];
        let n_vals = vals.split(',').filter(|s| !s.trim().is_empty()).count();

        // Sanity: placeholders run $1..$29 contiguously.
        let max_placeholder = 29;
        for n in 1..=max_placeholder {
            assert!(sql.contains(&format!("${n}")), "missing placeholder ${n}");
        }
        assert_eq!(
            n_cols, n_vals,
            "column count ({n_cols}) != value count ({n_vals}); cols=[{cols}] vals=[{vals}]"
        );
    }

    /// The exactly-one-parent invariant (also enforced by the DB `one_parent`
    /// CHECK): each `ScorePointParent` variant sets exactly one of the three FK
    /// columns and leaves the other two NULL.
    #[test]
    fn score_point_parent_columns_sets_exactly_one() {
        let id = Uuid::new_v4();
        for (parent, which) in [
            (ScorePointParent::Code(id), 0u8),
            (ScorePointParent::Context(id), 1),
            (ScorePointParent::Agent(id), 2),
        ] {
            let cols = score_point_parent_columns(parent);
            let set = [cols.0, cols.1, cols.2];
            assert_eq!(set.iter().filter(|c| c.is_some()).count(), 1);
            assert_eq!(set[which as usize], Some(id));
        }
    }

    /// The insert names all four write columns beyond the FK triple, in the
    /// order the binds are supplied.
    #[test]
    fn insert_score_point_sql_shape() {
        assert!(INSERT_SCORE_POINT_SQL.contains(
            "(code_run_id, context_run_id, agent_run_id, profile_id, axis, x_value, x_label, metric, value)"
        ));
        assert!(INSERT_SCORE_POINT_SQL.contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
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
        // MINT2-01 bumped the build-scenario write epoch to 'v3'; the orphan
        // reconcile must target that same epoch (fresh attempts now write 'v3').
        assert!(sql.contains("harness_version = 'v3'"));
        // Must join through model_profiles so the DELETE cannot match rows
        // belonging to a different model_id sharing the same backend/mem_config.
        assert!(sql.contains("USING model_profiles p"));
        assert!(sql.contains("r.profile_id = p.id"));
    }

    // ---- MINT2-01: tunable measurement factors -------------------------

    /// The build-scenario insert now stamps the epoch as `'v3'` (bumped from
    /// `'v2'`) and names all six new factor columns.
    #[test]
    fn insert_code_run_v2_sql_writes_v3_epoch_and_factor_columns() {
        assert!(
            INSERT_CODE_RUN_V2_SQL.contains("VALUES ($1, 'v3',"),
            "build-scenario path must write harness_version = 'v3'"
        );
        for col in [
            "quant",
            "reasoning_enabled",
            "context_window_launched",
            "temperature",
            "top_p",
            "task_category",
        ] {
            assert!(
                INSERT_CODE_RUN_V2_SQL.contains(col),
                "INSERT must name the {col} factor column"
            );
        }
    }

    /// Every new factor field defaults to `None` (a writer that doesn't set it
    /// never mislabels a row) and is settable like any other field.
    #[test]
    fn code_run_row_v2_factor_fields_default_none_and_are_settable() {
        let d = CodeRunRowV2::default();
        assert_eq!(d.quant, None);
        assert_eq!(d.reasoning_enabled, None);
        assert_eq!(d.context_window_launched, None);
        assert_eq!(d.temperature, None);
        assert_eq!(d.top_p, None);
        assert_eq!(d.task_category, None);

        let row = CodeRunRowV2 {
            quant: Some("Q4_K_M".to_string()),
            reasoning_enabled: Some(false),
            context_window_launched: Some(16384),
            temperature: Some(0.2),
            top_p: Some(0.9),
            task_category: Some("multi_file".to_string()),
            ..Default::default()
        };
        assert_eq!(row.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(row.reasoning_enabled, Some(false));
        assert_eq!(row.context_window_launched, Some(16384));
        assert_eq!(row.temperature, Some(0.2));
        assert_eq!(row.top_p, Some(0.9));
        assert_eq!(row.task_category.as_deref(), Some("multi_file"));
    }

    /// Round-trip through the pure row mapper: a fully-populated `'v3'` row's
    /// factors decode back to the same values (the migrated-DB read path).
    #[test]
    fn map_factor_row_round_trips_all_set() {
        let t: FactorTuple = (
            Some("Q6_K".to_string()),
            Some(true),
            Some(32768),
            Some(0.7),
            Some(0.95),
            Some("deep".to_string()),
        );
        let f = map_factor_row(t);
        assert_eq!(
            f,
            CodeRunFactors {
                quant: Some("Q6_K".to_string()),
                reasoning_enabled: Some(true),
                context_window_launched: Some(32768),
                temperature: Some(0.7),
                top_p: Some(0.95),
                task_category: Some("deep".to_string()),
            }
        );
    }

    /// The null-tolerant path: an un-migrated DB (all columns absent → the
    /// fallback query returns all-NULL) decodes to an all-`None` factors, never
    /// a panic and never a fabricated value.
    #[test]
    fn map_factor_row_all_null_is_all_none() {
        let t: FactorTuple = (None, None, None, None, None, None);
        assert_eq!(map_factor_row(t), CodeRunFactors::default());
    }

    /// The read path detects a MISSING-COLUMN error (un-migrated schema) so it
    /// can fall back to the NULL-only query, and does NOT misclassify unrelated
    /// DB errors as "columns absent".
    #[test]
    fn is_missing_column_error_matches_only_missing_column() {
        assert!(is_missing_column_error(
            "error returned from database: column \"quant\" does not exist"
        ));
        assert!(is_missing_column_error(
            "column \"task_category\" does not exist"
        ));
        // Unrelated errors must propagate, not silently become all-NULL.
        assert!(!is_missing_column_error("connection refused"));
        assert!(!is_missing_column_error("relation \"foo\" does not exist"));
        assert!(!is_missing_column_error("syntax error at or near \"SELECT\""));
    }

    /// The fallback SELECT casts each NULL to the matching column type so the
    /// tuple decodes identically to the primary query, and stays scoped by id.
    #[test]
    fn factor_select_sql_shapes() {
        assert!(SELECT_CODE_RUN_FACTORS_SQL.contains(
            "quant, reasoning_enabled, context_window_launched, temperature, top_p, task_category"
        ));
        assert!(SELECT_CODE_RUN_FACTORS_SQL.contains("WHERE id = $1"));
        assert!(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL.contains("NULL::text"));
        assert!(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL.contains("NULL::boolean"));
        assert!(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL.contains("NULL::integer"));
        assert!(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL.contains("NULL::double precision"));
        assert!(SELECT_CODE_RUN_FACTORS_FALLBACK_SQL.contains("WHERE id = $1"));
    }
}
