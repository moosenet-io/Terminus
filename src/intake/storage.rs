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
    // ---- MINT2-02: structured failure classification ----------------------
    /// The queryable, structured reason this (model × case × config) cell did
    /// not yield a full-quality result — or `Some("none")` when it DID (a clean
    /// run). The stable snake_case `key()` of an intake-local `FailureClass`
    /// (variant names mirror Harmony's `FailureCategory` + `non_viable_vram`;
    /// see `code_v2.rs`). The whole point of MINT2-02: a timed-out / OOM'd /
    /// over-VRAM-skipped cell now writes a ROW with this set (score 0) instead
    /// of NO row — so absence-of-data and genuine-failure are distinguishable.
    ///
    /// `None` here writes SQL NULL, which is RESERVED to mean "a legacy /
    /// pre-migration row": a `'v3'` write always sets this (a clean pass →
    /// `Some("none")`, never `None`), so NULL never means "this run succeeded".
    /// The read path (`read_code_run_failure_class`) tolerates the column being
    /// ABSENT on an un-migrated DB — a missing column reads as `None`, never a
    /// panic.
    pub failure_class: Option<String>,
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
      quant, reasoning_enabled, context_window_launched, temperature, top_p, task_category, \
      failure_class, finalized) \
     VALUES ($1, 'v3', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, \
     $24, $25, $26, $27, $28, $29, $30, false) \
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
        .bind(row.failure_class.as_deref())
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

// ===========================================================================
// MINT2-03: variance-aware run aggregates (read input rows; persist/read the
// `code_run_aggregates` table). The COMPUTATION is pure and lives in
// `crate::intake::aggregate`; these are the thin DB wrappers around it.
// ===========================================================================

use crate::intake::aggregate::{AggregateInputRow, RunAggregate};
use crate::intake::EpochSelector;

/// True when a Postgres error text indicates a MISSING RELATION (the table does
/// not exist — the un-migrated `code_run_aggregates` case), so the read path can
/// degrade to an empty aggregate set rather than propagating. Postgres reports
/// `error: relation "code_run_aggregates" does not exist` (SQLSTATE 42P01).
/// Pure over its input; mirrors [`is_missing_column_error`].
fn is_missing_relation_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("relation") && m.contains("does not exist")
}

/// Row shape the aggregate-input SELECT decodes into: model, task_category,
/// harness_version, the five MINT2-01 config factors, and the pre-resolved
/// effective score. Kept as a type alias so the primary and un-migrated fallback
/// queries decode identically.
type AggInputTuple = (
    String,         // model_name
    Option<String>, // task_category
    String,         // harness_version
    Option<String>, // quant
    Option<bool>,   // reasoning_enabled
    Option<i32>,    // context_window_launched
    Option<f64>,    // temperature
    Option<f64>,    // top_p
    i32,            // effective score (COALESCE(GREATEST(retry, first_pass), 0))
);

fn map_agg_input(t: AggInputTuple) -> AggregateInputRow {
    AggregateInputRow {
        model: t.0,
        task_category: t.1,
        harness_version: t.2,
        quant: t.3,
        reasoning_enabled: t.4,
        context_window_launched: t.5,
        temperature: t.6,
        top_p: t.7,
        effective_score: t.8,
    }
}

/// Primary aggregate-input SELECT — references the MINT2-01 factor columns
/// directly. The effective score is resolved in SQL exactly as `code_v2.rs` does
/// in Rust: `max(first_pass, retry)` with a NULL retry ignored, defaulting to 0.
/// Joins `model_profiles` for the model NAME (the sweep keys rows by `profile_id`
/// UUID). Scoped to the caller-supplied epoch (`harness_version = $1`).
const SELECT_AGG_INPUT_SQL: &str = "SELECT p.model_name, r.task_category, r.harness_version, \
     r.quant, r.reasoning_enabled, r.context_window_launched, r.temperature, r.top_p, \
     COALESCE(GREATEST(r.retry_score, r.first_pass_score), 0) \
     FROM code_profile_runs r JOIN model_profiles p ON p.id = r.profile_id \
     WHERE r.harness_version = $1 AND COALESCE(r.task_type, '') <> 'non_viable_skip'";

/// Fallback aggregate-input SELECT for a DB NOT yet migrated for MINT2-01 (the
/// five factor columns + `task_category` are absent): selects correctly-typed
/// SQL NULLs for those so the row still decodes, mirroring
/// [`SELECT_CODE_RUN_FACTORS_FALLBACK_SQL`]. Un-factored rows aggregate under an
/// all-`None` config bucket — honest for a pre-MINT2-01 host.
const SELECT_AGG_INPUT_FALLBACK_SQL: &str = "SELECT p.model_name, NULL::text, r.harness_version, \
     NULL::text, NULL::boolean, NULL::integer, NULL::double precision, NULL::double precision, \
     COALESCE(GREATEST(r.retry_score, r.first_pass_score), 0) \
     FROM code_profile_runs r JOIN model_profiles p ON p.id = r.profile_id \
     WHERE r.harness_version = $1 AND COALESCE(r.task_type, '') <> 'non_viable_skip'";

/// Read the per-sample rows an epoch's aggregates are computed from, tolerating a
/// DB not yet migrated for MINT2-01 (the factor columns fall back to NULL). Any
/// OTHER DB error is propagated. The pure [`crate::intake::aggregate::compute_aggregates`]
/// turns these into the variance-aware aggregates.
pub async fn read_aggregate_input_rows(
    pool: &PgPool,
    epoch: &str,
) -> Result<Vec<AggregateInputRow>, ToolError> {
    match sqlx::query_as::<_, AggInputTuple>(SELECT_AGG_INPUT_SQL)
        .bind(epoch)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => Ok(rows.into_iter().map(map_agg_input).collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_column_error(&msg) {
                let rows = sqlx::query_as::<_, AggInputTuple>(SELECT_AGG_INPUT_FALLBACK_SQL)
                    .bind(epoch)
                    .fetch_all(pool)
                    .await
                    .map_err(|e| {
                        ToolError::Database(format!(
                            "Failed to read aggregate input rows (fallback): {e}"
                        ))
                    })?;
                Ok(rows.into_iter().map(map_agg_input).collect())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read aggregate input rows: {msg}"
                )))
            }
        }
    }
}

/// Persist a freshly-computed aggregate set for ONE epoch into
/// `code_run_aggregates`, replacing that epoch's prior rows wholesale (aggregates
/// are cheap to recompute from `code_profile_runs`, so a DELETE-then-INSERT of the
/// current epoch's rows is the simplest correct idempotent write — and it avoids
/// the NULL-in-a-unique-index upsert hazard the nullable config-factor key would
/// otherwise pose). Runs in a single transaction so a partial failure never
/// leaves the table half-updated. Only the passed epoch's rows are touched; other
/// epochs are left intact.
pub async fn persist_code_run_aggregates(
    pool: &PgPool,
    epoch: &str,
    aggregates: &[RunAggregate],
) -> Result<(), ToolError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ToolError::Database(format!("begin aggregate persist tx: {e}")))?;

    sqlx::query("DELETE FROM code_run_aggregates WHERE harness_version = $1")
        .bind(epoch)
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("clear code_run_aggregates for epoch: {e}")))?;

    for a in aggregates {
        sqlx::query(
            "INSERT INTO code_run_aggregates \
             (model, task_category, harness_version, quant, reasoning_enabled, \
              context_window_launched, temperature, top_p, \
              pass_rate, n_samples, passes, score_stddev, low_confidence) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(&a.key.model)
        .bind(a.key.task_category.as_deref())
        .bind(&a.key.harness_version)
        .bind(a.key.quant.as_deref())
        .bind(a.key.reasoning_enabled)
        .bind(a.key.context_window_launched)
        .bind(a.key.temperature)
        .bind(a.key.top_p)
        .bind(a.pass_rate)
        .bind(a.n_samples as i32)
        .bind(a.passes as i32)
        .bind(a.score_stddev)
        .bind(a.low_confidence)
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("insert code_run_aggregates row: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| ToolError::Database(format!("commit aggregate persist tx: {e}")))
}

/// A persisted `code_run_aggregates` row (the read-back shape for the catalog).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRunAggregate {
    pub model: String,
    pub task_category: Option<String>,
    pub harness_version: String,
    pub quant: Option<String>,
    pub reasoning_enabled: Option<bool>,
    pub context_window_launched: Option<i32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub pass_rate: f64,
    pub n_samples: i32,
    pub passes: i32,
    pub score_stddev: f64,
    pub low_confidence: bool,
}

type StoredAggTuple = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<bool>,
    Option<i32>,
    Option<f64>,
    Option<f64>,
    f64,
    i32,
    i32,
    f64,
    bool,
);

/// Pure mapping from the decoded `code_run_aggregates` tuple to a
/// [`StoredRunAggregate`]. Extracted so the epoch-specific and selector-aware
/// reads decode identically (and so it is unit-testable without a live DB).
fn map_stored_agg(t: StoredAggTuple) -> StoredRunAggregate {
    StoredRunAggregate {
        model: t.0,
        task_category: t.1,
        harness_version: t.2,
        quant: t.3,
        reasoning_enabled: t.4,
        context_window_launched: t.5,
        temperature: t.6,
        top_p: t.7,
        pass_rate: t.8,
        n_samples: t.9,
        passes: t.10,
        score_stddev: t.11,
        low_confidence: t.12,
    }
}

/// Read the persisted aggregates for ONE explicit epoch, TOLERATING the
/// `code_run_aggregates` table being ABSENT on an un-migrated DB: a missing
/// relation ([`is_missing_relation_error`]) reads as an empty set, never a
/// panic — mirroring the MINT2-01/02 null-tolerant column reads. Any other DB
/// error is propagated. Thin wrapper over [`read_code_run_aggregates_selected`]
/// with an [`EpochSelector::Only`], preserved for callers that already hold a
/// concrete epoch string (e.g. the current-epoch persist/read round-trip).
pub async fn read_code_run_aggregates(
    pool: &PgPool,
    epoch: &str,
) -> Result<Vec<StoredRunAggregate>, ToolError> {
    read_code_run_aggregates_selected(pool, &EpochSelector::Only(epoch.to_string())).await
}

/// Read the persisted run aggregates, honoring an [`EpochSelector`] so the coder
/// catalog/reports DEFAULT to the current epoch — legacy rows never pollute the
/// current numbers — while still exposing legacy/all provenance:
///   - [`EpochSelector::Current`] (the default) → only `current_epoch()`,
///   - [`EpochSelector::Only`]    → one explicit legacy/other epoch,
///   - [`EpochSelector::All`]     → every epoch (no `harness_version` filter).
///
/// Legacy rows are partitioned by FILTER only — this read never deletes or
/// mutates them. The epoch `WHERE`-fragment comes from the ONE central
/// [`crate::intake::epoch_where_fragment`] (`All` → a bind-free `TRUE`). Tolerates
/// the `code_run_aggregates` table being ABSENT on an un-migrated DB (empty set),
/// mirroring the MINT2-03 null/absence-tolerant read pattern.
pub async fn read_code_run_aggregates_selected(
    pool: &PgPool,
    selector: &EpochSelector,
) -> Result<Vec<StoredRunAggregate>, ToolError> {
    let where_frag = crate::intake::epoch_where_fragment(selector, 1);
    let sql = format!(
        "SELECT model, task_category, harness_version, quant, reasoning_enabled, \
         context_window_launched, temperature, top_p, pass_rate, n_samples, passes, \
         score_stddev, low_confidence FROM code_run_aggregates WHERE {where_frag} \
         ORDER BY model, task_category NULLS FIRST, quant NULLS FIRST"
    );
    let mut query = sqlx::query_as::<_, StoredAggTuple>(&sql);
    // Bind the epoch only when the selector resolves to one (All binds nothing).
    if let Some(epoch) = selector.epoch() {
        query = query.bind(epoch.to_string());
    }
    match query.fetch_all(pool).await {
        Ok(rows) => Ok(rows.into_iter().map(map_stored_agg).collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                // Un-migrated DB: the table isn't there yet — no aggregates.
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read code_run_aggregates (selected): {msg}"
                )))
            }
        }
    }
}

// ===========================================================================
// MINT2-05: persisted epoch marker (audit timeline of when an epoch became
// current). Table-absence tolerant, mirroring the aggregate read above.
// ===========================================================================

/// A persisted `intake_epoch_marker` row: which epoch, when it became the
/// current partition, and an optional note (why it was bumped / what evolved).
#[derive(Debug, Clone, PartialEq)]
pub struct EpochMarker {
    pub epoch: String,
    pub became_current_at: chrono::DateTime<chrono::Utc>,
    pub note: Option<String>,
}

/// SQL for [`upsert_epoch_marker`]. Pulled out to a const so a unit test can
/// assert its idempotent-upsert shape without a live DB: an INSERT keyed on the
/// `epoch` PRIMARY KEY with `ON CONFLICT (epoch) DO UPDATE` so recording the
/// same epoch twice is a safe no-op update (the marker is a single audit row per
/// epoch, never duplicated). `became_current_at` is only set on first insert —
/// `ON CONFLICT` deliberately does NOT overwrite it (the timeline records when
/// the epoch FIRST became current), while `note` is refreshed if re-supplied.
const UPSERT_EPOCH_MARKER_SQL: &str = "INSERT INTO intake_epoch_marker (epoch, note) \
     VALUES ($1, $2) \
     ON CONFLICT (epoch) DO UPDATE SET note = COALESCE(EXCLUDED.note, intake_epoch_marker.note)";

/// Record (idempotently) that `epoch` is/became the current partition. Safe to
/// call every startup/cutover: the first call inserts the marker with
/// `became_current_at = now()`; subsequent calls for the same epoch are a no-op
/// update that preserves the original timestamp (the audit point) and only
/// refreshes the note when a new one is supplied. Never deletes or rewrites any
/// other epoch's marker. Returns whether a NEW marker row was created (`true`)
/// vs. an existing one was seen (`false`) — for logging only.
pub async fn upsert_epoch_marker(
    pool: &PgPool,
    epoch: &str,
    note: Option<&str>,
) -> Result<bool, ToolError> {
    let result = sqlx::query(UPSERT_EPOCH_MARKER_SQL)
        .bind(epoch)
        .bind(note)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to upsert intake_epoch_marker: {e}")))?;
    // A fresh INSERT affects 1 row; an ON CONFLICT no-op update that changes
    // nothing may report 0 — either way it is not an error. `rows_affected == 1`
    // on the very first insert; treat >=1-with-a-real-change as "created/updated".
    Ok(result.rows_affected() >= 1)
}

/// Read the marker for one epoch, TOLERATING the `intake_epoch_marker` table
/// being ABSENT on an un-migrated DB: a missing relation
/// ([`is_missing_relation_error`]) reads as `None` ("no marker recorded"), never
/// a panic — mirroring [`read_code_run_aggregates_selected`]'s absence tolerance.
/// A genuinely absent row (table exists, epoch never recorded) also yields
/// `None`. Any OTHER DB error is propagated.
pub async fn read_epoch_marker(
    pool: &PgPool,
    epoch: &str,
) -> Result<Option<EpochMarker>, ToolError> {
    let sql = "SELECT epoch, became_current_at, note \
         FROM intake_epoch_marker WHERE epoch = $1";
    type MarkerTuple = (String, chrono::DateTime<chrono::Utc>, Option<String>);
    match sqlx::query_as::<_, MarkerTuple>(sql)
        .bind(epoch)
        .fetch_optional(pool)
        .await
    {
        Ok(row) => Ok(row.map(|t| EpochMarker {
            epoch: t.0,
            became_current_at: t.1,
            note: t.2,
        })),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                // Un-migrated DB: the marker table isn't there yet.
                Ok(None)
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read intake_epoch_marker: {msg}"
                )))
            }
        }
    }
}

// ===========================================================================
// MINT2-02: read the structured failure_class back (null-tolerant)
// ===========================================================================

/// Primary `failure_class` SELECT — references the MINT2-02 column directly.
/// Errors on an un-migrated DB where the column doesn't exist yet;
/// [`read_code_run_failure_class`] catches that and retries the NULL-typed
/// [`SELECT_CODE_RUN_FAILURE_CLASS_FALLBACK_SQL`].
///
/// Deliberately SEPARATE from the MINT2-01 factor SELECT (not folded into it):
/// a DB migrated for MINT2-01 but NOT yet MINT2-02 must still read its real
/// quant/reasoning/etc values — folding `failure_class` into that query would
/// make the whole query fail on the missing column and fall back to all-NULL,
/// wrongly nulling the factors that DO exist. Each migration's columns fall
/// back independently.
const SELECT_CODE_RUN_FAILURE_CLASS_SQL: &str =
    "SELECT failure_class FROM code_profile_runs WHERE id = $1";

/// Fallback `failure_class` SELECT for an UN-MIGRATED DB (the column is absent):
/// selects a correctly-typed SQL NULL so the row still decodes into `None`
/// instead of erroring on the missing column. Still gated on `WHERE id = $1`.
const SELECT_CODE_RUN_FAILURE_CLASS_FALLBACK_SQL: &str =
    "SELECT NULL::text FROM code_profile_runs WHERE id = $1";

/// Read the MINT2-02 `failure_class` for one `code_profile_runs` row,
/// tolerating an un-migrated DB. Tries [`SELECT_CODE_RUN_FAILURE_CLASS_SQL`]; if
/// that fails specifically because the column doesn't exist yet
/// ([`is_missing_column_error`]), retries the NULL-typed fallback so the caller
/// gets `None` instead of an error — the harness keeps running against a DB the
/// migration hasn't reached. A genuinely missing row (id not present) yields
/// `None`, NOT an error. Any OTHER DB error is propagated, never masked.
///
/// Semantics of the returned value: `Some("none")` = a clean `'v3'` run;
/// `Some(other)` = a classified failure; `None` = either a legacy/pre-migration
/// row OR an un-migrated DB (the column is absent). The two `None` cases are
/// intentionally indistinguishable HERE — both mean "no structured class was
/// ever recorded" — which is exactly what reporting needs.
pub async fn read_code_run_failure_class(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<String>, ToolError> {
    match sqlx::query_scalar::<_, Option<String>>(SELECT_CODE_RUN_FAILURE_CLASS_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
    {
        Ok(row) => Ok(row.flatten()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_column_error(&msg) {
                let row = sqlx::query_scalar::<_, Option<String>>(
                    SELECT_CODE_RUN_FAILURE_CLASS_FALLBACK_SQL,
                )
                .bind(id)
                .fetch_optional(pool)
                .await
                .map_err(|e| {
                    ToolError::Database(format!(
                        "Failed to read code_profile_runs failure_class (fallback): {e}"
                    ))
                })?;
                Ok(row.flatten())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read code_profile_runs failure_class: {msg}"
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

// ===========================================================================
// MINT2-07: Model Fleet Catalog — tolerant readers for every upstream source +
// the wholesale persist. Each read tolerates its source table/column being
// ABSENT on an un-migrated host (a missing relation/column reads as "no rows",
// so the catalog builder degrades those cells to `not_run` rather than
// crashing) — mirroring the MINT2-03/05 absence-tolerant pattern. The catalog's
// COMPUTATION is the pure `crate::intake::catalog::build_catalog`; these are the
// thin DB wrappers around it.
// ===========================================================================

use crate::intake::catalog::{
    AgentRollup, AssistantCell, CoverageStatus, ModelCatalog, NonViableRow, ServingRow,
};

/// Max run timestamp per `(model_name, task_category)` for one coder epoch —
/// the `last_run_at` a catalog `run` cell reports. Tolerates the `task_category`
/// column being absent (un-migrated MINT2-01) or the table being absent: either
/// yields an empty map, never a panic. Skips `non_viable_skip` rows (they have
/// no measured category) exactly as the aggregate-input read does.
pub async fn read_coder_last_run(
    pool: &PgPool,
    epoch: &str,
) -> Result<std::collections::BTreeMap<(String, String), chrono::DateTime<chrono::Utc>>, ToolError> {
    let sql = "SELECT p.model_name, r.task_category, max(r.created_at) \
         FROM code_profile_runs r JOIN model_profiles p ON p.id = r.profile_id \
         WHERE r.harness_version = $1 AND COALESCE(r.task_type, '') <> 'non_viable_skip' \
           AND r.task_category IS NOT NULL \
         GROUP BY p.model_name, r.task_category";
    type Row = (String, String, chrono::DateTime<chrono::Utc>);
    match sqlx::query_as::<_, Row>(sql).bind(epoch).fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(m, c, t)| ((m, c), t))
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(std::collections::BTreeMap::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read coder last-run timestamps: {msg}"
                )))
            }
        }
    }
}

/// The `non_viable_vram` skip rows for one epoch, read on their OWN axis from
/// `code_profile_runs.failure_class` (NOT inferred from aggregates, which
/// EXCLUDE skips). Tolerates the `failure_class`/`quant` column or the table
/// being absent → empty vec.
pub async fn read_non_viable_rows(
    pool: &PgPool,
    epoch: &str,
) -> Result<Vec<NonViableRow>, ToolError> {
    let sql = "SELECT p.model_name, r.quant \
         FROM code_profile_runs r JOIN model_profiles p ON p.id = r.profile_id \
         WHERE r.harness_version = $1 AND r.failure_class = 'non_viable_vram'";
    type Row = (String, Option<String>);
    match sqlx::query_as::<_, Row>(sql).bind(epoch).fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model_name, quant)| NonViableRow { model_name, quant })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read non_viable_vram rows: {msg}"
                )))
            }
        }
    }
}

/// MINT2-06: current-(assistant-)epoch sample counts per (model_id, dimension),
/// for the assistant stale-cell planner. Scoped to the assistant's OWN epoch
/// lineage — `assistant_profile_run.harness_version = $1` (the caller passes
/// `assistant::schema::HARNESS_VERSION`, NOT the coder `'v3'` epoch) — by joining
/// each `assistant_dimension_score` row to its run, so legacy-epoch scores never
/// count toward current coverage. Tolerates the `assistant_dimension_score` /
/// `assistant_profile_run` tables being ABSENT on an un-migrated DB → empty map
/// (⇒ the planner sees zero current-epoch samples and marks everything stale,
/// which is correct). Any other DB error is propagated.
pub async fn read_assistant_dimension_counts(
    pool: &PgPool,
    harness_version: &str,
) -> Result<std::collections::BTreeMap<(String, String), i64>, ToolError> {
    let sql = "SELECT s.model_id, s.dimension, count(*)::bigint \
         FROM assistant_dimension_score s \
         JOIN assistant_profile_run r ON r.id = s.run_id \
         WHERE r.harness_version = $1 \
         GROUP BY s.model_id, s.dimension";
    type Row = (String, String, i64);
    match sqlx::query_as::<_, Row>(sql)
        .bind(harness_version)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model, dim, n)| ((model, dim), n))
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(std::collections::BTreeMap::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read assistant dimension counts: {msg}"
                )))
            }
        }
    }
}

/// Per-(model, dimension) rollup of the assistant sweep's dimension scores:
/// sample count, mean dispersion, last-run. Tolerates the
/// `assistant_dimension_score` table being absent → empty vec.
pub async fn read_assistant_cells(pool: &PgPool) -> Result<Vec<AssistantCell>, ToolError> {
    let sql = "SELECT model_id, dimension, count(*)::bigint, avg(std_dev), max(created_at) \
         FROM assistant_dimension_score GROUP BY model_id, dimension";
    type Row = (
        String,
        String,
        i64,
        Option<f64>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    match sqlx::query_as::<_, Row>(sql).fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model_name, dimension, n, sd, last)| AssistantCell {
                model_name,
                dimension,
                n_samples: n,
                score_stddev: sd,
                last_run_at: last,
            })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read assistant dimension cells: {msg}"
                )))
            }
        }
    }
}

/// SUITE-DOC (S125): per-model rollup of the dimension scores for ONE
/// `task_category` (e.g. `"document_parsing"`): sample count, mean dispersion,
/// last-run — the same shape as [`read_assistant_cells`] but scoped to a single
/// task_category and grouped by model only (a newcats suite has one dimension
/// but several metrics/cases per model, so the coverage signal is per-model, not
/// per-dimension). `dimension` carries the task_category label for the caller's
/// convenience. Tolerates the table being absent → empty vec.
pub async fn read_task_category_cells(
    pool: &PgPool,
    task_category: &str,
) -> Result<Vec<AssistantCell>, ToolError> {
    let sql = "SELECT model_id, count(*)::bigint, avg(std_dev), max(created_at) \
         FROM assistant_dimension_score WHERE task_category = $1 GROUP BY model_id";
    type Row = (
        String,
        i64,
        Option<f64>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    match sqlx::query_as::<_, Row>(sql)
        .bind(task_category)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model_name, n, sd, last)| AssistantCell {
                model_name,
                dimension: task_category.to_string(),
                n_samples: n,
                score_stddev: sd,
                last_run_at: last,
            })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read {task_category} task-category cells: {msg}"
                )))
            }
        }
    }
}

/// Latest serving/operational profile per model (the fleet card's serving
/// facts). Tolerates the operational-profile table being absent → empty vec.
pub async fn read_serving_rows(pool: &PgPool) -> Result<Vec<ServingRow>, ToolError> {
    let sql = "SELECT DISTINCT ON (mp.model_name) mp.model_name, op.max_context_safe, \
                op.quality_degradation_point, op.throughput_at_8k, op.created_at \
         FROM model_profiles mp JOIN model_operational_profiles op ON op.profile_id = mp.id \
         ORDER BY mp.model_name, op.created_at DESC";
    type Row = (
        String,
        Option<i32>,
        Option<i32>,
        Option<f64>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    match sqlx::query_as::<_, Row>(sql).fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(
                |(model_name, max_context_safe, quality_degradation_point, throughput, last)| {
                    ServingRow {
                        model_name,
                        max_context_safe,
                        quality_degradation_point,
                        throughput,
                        last_run_at: last,
                    }
                },
            )
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read serving profiles: {msg}"
                )))
            }
        }
    }
}

/// The agent tool-use rollup query. The tool-accuracy column is
/// `avg(CASE WHEN ap.correct_tool_selected THEN 1.0 ELSE 0.0 END)` — `AVG` over
/// the numeric literals `1.0`/`0.0` returns Postgres `NUMERIC`, which does NOT
/// decode into a Rust `Option<f64>` (`FLOAT8`) and crashed
/// `refresh_fleet_catalog` at runtime with "mismatched types; Rust type
/// Option<f64> (as SQL type FLOAT8) is not compatible with SQL type NUMERIC".
/// The trailing `::double precision` cast pins the column to `FLOAT8` so it
/// decodes cleanly (S125 FIX1). `count(*)::bigint` is likewise pinned to `INT8`
/// for the `i64` decode.
const SELECT_AGENT_ROLLUPS_SQL: &str = "SELECT mp.model_name, count(*)::bigint, \
                avg(CASE WHEN ap.correct_tool_selected THEN 1.0 ELSE 0.0 END)::double precision \
         FROM model_profiles mp JOIN agent_profile_runs ap ON ap.profile_id = mp.id \
         GROUP BY mp.model_name";

/// Per-model agent tool-use rollup: sample count and correct-tool-selection
/// accuracy. Tolerates the `agent_profile_runs` table being absent → empty vec.
pub async fn read_agent_rollups(pool: &PgPool) -> Result<Vec<AgentRollup>, ToolError> {
    let sql = SELECT_AGENT_ROLLUPS_SQL;
    type Row = (String, i64, Option<f64>);
    match sqlx::query_as::<_, Row>(sql).fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model_name, n, acc)| AgentRollup {
                model_name,
                n_samples: n,
                tool_accuracy: acc,
            })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read agent tool-use rollups: {msg}"
                )))
            }
        }
    }
}

/// Persist the freshly-built fleet catalog, replacing BOTH catalog tables
/// wholesale in one transaction (the catalog is fully re-derivable from the
/// upstream tables, so a delete-then-insert is the simplest correct idempotent
/// write — and a partial failure never leaves a half-updated card). The cell
/// table is cleared FIRST, then the summary; inserts stamp `refreshed_at` via
/// the column defaults. If the catalog tables are ABSENT (un-migrated host) this
/// errors on the missing relation — callers wire the refresh best-effort so a
/// not-yet-migrated host degrades to "catalog not refreshed" rather than failing
/// the sweep (see the coder-sweep call site).
pub async fn persist_fleet_catalog(
    pool: &PgPool,
    catalog: &[ModelCatalog],
) -> Result<(), ToolError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ToolError::Database(format!("begin fleet-catalog persist tx: {e}")))?;

    sqlx::query("DELETE FROM model_fleet_catalog_cell")
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("clear model_fleet_catalog_cell: {e}")))?;
    sqlx::query("DELETE FROM model_fleet_catalog")
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("clear model_fleet_catalog: {e}")))?;

    for m in catalog {
        let serving_json = m.serving.to_json();
        sqlx::query(
            "INSERT INTO model_fleet_catalog \
             (model_name, quant, in_current_fleet, serving_json, not_run_count, stale_count) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&m.model_name)
        .bind(m.quant.as_deref())
        .bind(m.in_current_fleet)
        .bind(serving_json)
        .bind(m.not_run_count as i32)
        .bind(m.stale_count as i32)
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("insert model_fleet_catalog row: {e}")))?;

        for c in &m.cells {
            sqlx::query(
                "INSERT INTO model_fleet_catalog_cell \
                 (model_name, quant, test_type, task_category, status, pass_rate, \
                  n_samples, score_stddev, low_confidence, last_run_at, harness_version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(&c.model_name)
            .bind(c.quant.as_deref())
            .bind(&c.test_type)
            .bind(&c.task_category)
            .bind(CoverageStatus::as_str(&c.status))
            .bind(c.pass_rate)
            .bind(c.n_samples.map(|n| n as i32))
            .bind(c.score_stddev)
            .bind(c.low_confidence)
            .bind(c.last_run_at)
            .bind(c.harness_version.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| ToolError::Database(format!("insert model_fleet_catalog_cell row: {e}")))?;
        }
    }

    tx.commit()
        .await
        .map_err(|e| ToolError::Database(format!("commit fleet-catalog persist tx: {e}")))
}

/// The SELECT that reads the per-model summary cards, newest-refresh first.
const READ_FLEET_CATALOG_CARDS_SQL: &str = "SELECT model_name, quant, in_current_fleet, \
     serving_json, not_run_count, stale_count, refreshed_at \
     FROM model_fleet_catalog ORDER BY model_name";

/// The SELECT that reads every coverage cell.
const READ_FLEET_CATALOG_CELLS_SQL: &str = "SELECT model_name, quant, test_type, task_category, \
     status, pass_rate, n_samples, score_stddev, low_confidence, last_run_at, harness_version \
     FROM model_fleet_catalog_cell";

type FleetCardTuple = (
    String,                                  // model_name
    Option<String>,                          // quant
    bool,                                    // in_current_fleet
    Option<serde_json::Value>,               // serving_json
    i32,                                     // not_run_count
    i32,                                     // stale_count
    chrono::DateTime<chrono::Utc>,           // refreshed_at
);

type FleetCellTuple = (
    String,                                  // model_name
    Option<String>,                          // quant
    String,                                  // test_type
    String,                                  // task_category
    String,                                  // status
    Option<f64>,                             // pass_rate
    Option<i32>,                             // n_samples
    Option<f64>,                             // score_stddev
    Option<bool>,                            // low_confidence
    Option<chrono::DateTime<chrono::Utc>>,   // last_run_at
    Option<String>,                          // harness_version
);

/// Read the PERSISTED Model Fleet Catalog (both tables) into per-model cards —
/// the read side MINT2-08's `model_fleet_catalog` tool serves. This NEVER
/// recomputes; it reads exactly what the MINT2-07 refresh last persisted.
///
/// An un-migrated host (the catalog tables do not exist yet) is a clean
/// [`ToolError::NotConfigured`] — NOT a crash and NOT a masked empty result: the
/// tool surfaces "catalog not configured" so the caller knows the difference
/// between "no models" and "no catalog". Any OTHER DB error propagates. Cells are
/// grouped onto their card by `model_name`; a cell whose model has no summary row
/// (should not happen — both tables are rewritten together) is ignored.
pub async fn read_fleet_catalog(
    pool: &PgPool,
) -> Result<Vec<crate::intake::catalog::StoredCatalogCard>, ToolError> {
    use crate::intake::catalog::{StoredCatalogCard, StoredCatalogCell};
    use std::collections::BTreeMap;

    let card_rows = match sqlx::query_as::<_, FleetCardTuple>(READ_FLEET_CATALOG_CARDS_SQL)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                return Err(ToolError::NotConfigured(
                    "the Model Fleet Catalog is not configured on this host \
                     (model_fleet_catalog table absent — run the MINT2-07 migration \
                     and a sweep to populate it)"
                        .into(),
                ));
            }
            return Err(ToolError::Database(format!(
                "Failed to read model_fleet_catalog: {msg}"
            )));
        }
    };

    let cell_rows = match sqlx::query_as::<_, FleetCellTuple>(READ_FLEET_CATALOG_CELLS_SQL)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                return Err(ToolError::NotConfigured(
                    "the Model Fleet Catalog is not configured on this host \
                     (model_fleet_catalog_cell table absent)"
                        .into(),
                ));
            }
            return Err(ToolError::Database(format!(
                "Failed to read model_fleet_catalog_cell: {msg}"
            )));
        }
    };

    // Group cells by model.
    let mut cells_by_model: BTreeMap<String, Vec<StoredCatalogCell>> = BTreeMap::new();
    for (model_name, quant, test_type, task_category, status, pass_rate, n_samples, score_stddev, low_confidence, last_run_at, harness_version) in cell_rows {
        cells_by_model
            .entry(model_name.clone())
            .or_default()
            .push(StoredCatalogCell {
                model_name,
                quant,
                test_type,
                task_category,
                status,
                pass_rate,
                n_samples: n_samples.map(|n| n as i64),
                score_stddev,
                low_confidence,
                last_run_at,
                harness_version,
            });
    }

    let cards = card_rows
        .into_iter()
        .map(
            |(model_name, quant, in_current_fleet, serving_json, not_run_count, stale_count, refreshed_at)| {
                let cells = cells_by_model.remove(&model_name).unwrap_or_default();
                StoredCatalogCard {
                    model_name,
                    quant,
                    in_current_fleet,
                    serving_json,
                    not_run_count: not_run_count as i64,
                    stale_count: stale_count as i64,
                    refreshed_at,
                    cells,
                }
            },
        )
        .collect();
    Ok(cards)
}

// ===========================================================================
// CONST-21: additive read functions backing the Constellation web GUI's
// `/api/terminus/models*` + `/api/terminus/mint/*` endpoints
// (`src/constellation/models_api.rs`). These are READ-ONLY over the SAME
// tables the rest of this module writes — no new pool, no new schema, no
// change to any existing write path. Every function follows this file's
// established absence-tolerance convention ([`is_missing_relation_error`] /
// [`is_missing_column_error`] → empty result, not a panic or 500) so the web
// API degrades to empty arrays on an un-migrated or freshly-provisioned host
// exactly like the MCP tools already do.
// ===========================================================================

/// One raw `serving_profile` row, read back for the Model Library detail
/// panel's "Deployment" section (spec §6.1.3 / §8
/// `GET /api/terminus/models/{name}` `serving[]`). Deliberately a plain,
/// stringly-typed row (not `crate::intake::serving::ServingProfile`, whose
/// enums are validated at WRITE time) — a read path has no business rejecting
/// a persisted row for failing a write-side enum parse; the web API surfaces
/// whatever is stored, verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct ServingProfileRow {
    pub backend_tag: String,
    pub best_runtime: String,
    pub tok_s: Option<f64>,
    pub vram_or_ram_peak_gb: Option<f64>,
    pub cold_load_s: Option<f64>,
    pub keep_warm: bool,
    pub fallback_runtime: Option<String>,
    pub exclusion_reason: String,
    pub recheck_trigger: String,
    pub provenance: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Every `serving_profile` row for one model (one per `backend_tag`).
/// Tolerates the `serving_profile` table being absent (un-migrated host) →
/// empty vec, never an error — the Deployment section just degrades to "no
/// serving data yet".
pub async fn read_serving_profiles_for_model(
    pool: &PgPool,
    model_name: &str,
) -> Result<Vec<ServingProfileRow>, ToolError> {
    let sql = "SELECT backend_tag, best_runtime, tok_s, vram_or_ram_peak_gb, cold_load_s, \
               keep_warm, fallback_runtime, exclusion_reason, recheck_trigger, provenance, updated_at \
               FROM serving_profile WHERE model_id = $1 ORDER BY backend_tag";
    type Row = (
        String,
        String,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        bool,
        Option<String>,
        String,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    );
    match sqlx::query_as::<_, Row>(sql)
        .bind(model_name)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(
                |(
                    backend_tag,
                    best_runtime,
                    tok_s,
                    vram_or_ram_peak_gb,
                    cold_load_s,
                    keep_warm,
                    fallback_runtime,
                    exclusion_reason,
                    recheck_trigger,
                    provenance,
                    updated_at,
                )| ServingProfileRow {
                    backend_tag,
                    best_runtime,
                    tok_s,
                    vram_or_ram_peak_gb,
                    cold_load_s,
                    keep_warm,
                    fallback_runtime,
                    exclusion_reason,
                    recheck_trigger,
                    provenance,
                    updated_at,
                },
            )
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read serving_profile rows for {model_name}: {msg}"
                )))
            }
        }
    }
}

/// The set of model ids currently `keep_warm = true` on ANY backend — the
/// Model Library's "serving now" signal (§6.1 header stat row + `serving_now`
/// field, §8 models-list `coverage`). Tolerates an absent `serving_profile`
/// table → empty set (nothing reports as serving).
pub async fn read_keep_warm_model_ids(
    pool: &PgPool,
) -> Result<std::collections::BTreeSet<String>, ToolError> {
    let sql = "SELECT DISTINCT model_id FROM serving_profile WHERE keep_warm = true";
    match sqlx::query_as::<_, (String,)>(sql).fetch_all(pool).await {
        Ok(rows) => Ok(rows.into_iter().map(|(m,)| m).collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(std::collections::BTreeSet::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read keep_warm serving_profile rows: {msg}"
                )))
            }
        }
    }
}

/// The latest `model_operational_profiles` row for one model, ordered by
/// `COALESCE(mp.profile_date, mp.created_at) DESC` per the CONST-GUI audit's
/// contracts-to-confirm #1: the live intake DB carries a `model_profiles.
/// profile_date` column this checkout's own `CREATE TABLE` doesn't (an
/// out-of-band live migration) — `COALESCE` prefers it when present and
/// falls back to `created_at` otherwise, so a row profiled and explicitly
/// (re-)dated via `profile_date` sorts correctly ahead of `created_at`-only
/// rows. Review note (build session): the sanctioned read-only `pg_*` tool
/// connected to a DB reporting zero tables, so this could not be directly
/// re-confirmed against a live schema this session — implemented per the
/// audit's pinned finding. SAFE either way: if some environment's
/// `model_profiles` truly lacks `profile_date` (matching this checkout's own
/// `CREATE TABLE`), Postgres reports a missing-column error, which this
/// function's `Err` arm already degrades to `Ok(None)` via
/// [`is_missing_column_error`] — never a hard failure.
pub async fn read_latest_operational_profile_for_model(
    pool: &PgPool,
    model_name: &str,
) -> Result<Option<OperationalProfileRow>, ToolError> {
    // Two-attempt ordering (review-cycle-2 fix): the primary attempt honors the
    // contracts-to-confirm #1 `COALESCE(profile_date, created_at)` ordering, but
    // `model_profiles.profile_date` exists on the LIVE schema and NOT in this
    // checkout's own `CREATE TABLE` — on a schema without the column the primary
    // attempt fails with a missing-column error, and the old behavior silently
    // returned `Ok(None)`, DROPPING a real profile. Now a missing-column error
    // triggers a retry ordered by `mp.created_at` alone; only a missing RELATION
    // degrades to `None`.
    let sql_for = |order_expr: &str| {
        format!(
            "SELECT op.max_context_safe, op.max_context_absolute, op.quality_degradation_point, \
             op.throughput_at_2k, op.throughput_at_8k, op.throughput_at_16k, op.throughput_at_32k, \
             op.throughput_at_64k, op.recommended_timeout_chat_sec, op.recommended_timeout_build_sec, \
             op.recommended_timeout_deep_sec, op.overall_tier \
             FROM model_operational_profiles op \
             JOIN model_profiles mp ON mp.id = op.profile_id \
             WHERE mp.model_name = $1 \
             ORDER BY {order_expr} DESC LIMIT 1"
        )
    };
    let sql = sql_for("COALESCE(mp.profile_date, mp.created_at)");
    type Row = (
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<String>,
    );
    let primary = sqlx::query_as::<_, Row>(&sql)
        .bind(model_name)
        .fetch_optional(pool)
        .await;
    let attempt = match primary {
        Err(e) if is_missing_column_error(&e.to_string()) => {
            let fallback_sql = sql_for("mp.created_at");
            sqlx::query_as::<_, Row>(&fallback_sql)
                .bind(model_name)
                .fetch_optional(pool)
                .await
        }
        other => other,
    };
    match attempt {
        Ok(Some((
            max_context_safe,
            max_context_absolute,
            quality_degradation_point,
            throughput_at_2k,
            throughput_at_8k,
            throughput_at_16k,
            throughput_at_32k,
            throughput_at_64k,
            recommended_timeout_chat_sec,
            recommended_timeout_build_sec,
            recommended_timeout_deep_sec,
            overall_tier,
        ))) => Ok(Some(OperationalProfileRow {
            max_context_safe,
            max_context_absolute,
            quality_degradation_point,
            throughput_at_2k,
            throughput_at_8k,
            throughput_at_16k,
            throughput_at_64k,
            throughput_at_32k,
            recommended_timeout_chat_sec,
            recommended_timeout_build_sec,
            recommended_timeout_deep_sec,
            overall_tier,
        })),
        Ok(None) => Ok(None),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(None)
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read operational profile for {model_name}: {msg}"
                )))
            }
        }
    }
}

/// One raw run row for the MINT "runs" table view (`GET
/// /api/terminus/mint/runs?suite=code`), field list per the CONST-GUI audit
/// §5 `code_profile_runs` list verbatim (the columns a table-view drill-down
/// needs — not every column on the table, but every column the spec's C3/C5
/// drill-downs and the raw table view bind to).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CodeRunListRow {
    pub run_id: uuid::Uuid,
    pub model: String,
    pub language: Option<String>,
    pub task_category: Option<String>,
    pub backend_tag: Option<String>,
    pub case_id: Option<String>,
    pub first_pass_score: Option<i32>,
    pub code_quality_score: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub throughput_tok_per_sec: Option<f64>,
    pub memory_usage_mb: Option<i32>,
    pub oom: Option<bool>,
    pub failure_class: Option<String>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Filters shared by [`read_code_runs_page`] and [`read_code_run_values_for_box`]
/// — the `code` suite's slice of `GET /api/terminus/mint/runs`/`.../box`'s query
/// params (model/task_category/language/failure_class/epoch).
#[derive(Debug, Clone, Default)]
pub struct CodeRunFilter {
    pub model: Option<String>,
    pub task_category: Option<String>,
    pub language: Option<String>,
    pub failure_class: Option<String>,
    pub epoch: EpochSelector,
}

impl CodeRunFilter {
    /// Build the `WHERE` fragment + positional binds (starting at `$1`) common
    /// to every `code_profile_runs` read this item adds. `epoch` binds LAST via
    /// [`crate::intake::epoch_where_fragment`] (`All` consumes no bind), so the
    /// caller knows exactly which `$n` is the epoch bind (or none) without
    /// re-deriving the count. Returns `(where_sql, next_free_idx)`.
    fn where_sql(&self) -> (String, usize) {
        let mut clauses: Vec<String> = vec!["r.finalized = true".to_string()];
        let mut idx = 1usize;
        if self.model.is_some() {
            clauses.push(format!("mp.model_name = ${idx}"));
            idx += 1;
        }
        if self.task_category.is_some() {
            clauses.push(format!("r.task_category = ${idx}"));
            idx += 1;
        }
        if self.language.is_some() {
            clauses.push(format!("r.language = ${idx}"));
            idx += 1;
        }
        if self.failure_class.is_some() {
            clauses.push(format!("r.failure_class = ${idx}"));
            idx += 1;
        }
        clauses.push(crate::intake::epoch_where_fragment(&self.epoch, idx).replace(
            "harness_version",
            "r.harness_version",
        ));
        if self.epoch.epoch().is_some() {
            idx += 1;
        }
        (format!("WHERE {}", clauses.join(" AND ")), idx)
    }
}

/// Page through `code_profile_runs` (joined to `model_profiles` for the
/// display name) under `filter`, newest first, with the SAME `total` +
/// `limit`/`offset` contract as `GET /api/terminus/models` (§8: "All list
/// endpoints: `limit` … + `offset`, … a `total` field"). Tolerates the tables
/// being absent → `(vec![], 0)`.
pub async fn read_code_runs_page(
    pool: &PgPool,
    filter: &CodeRunFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CodeRunListRow>, i64), ToolError> {
    let (where_sql, next_idx) = filter.where_sql();
    let count_sql = format!(
        "SELECT count(*)::bigint FROM code_profile_runs r \
         JOIN model_profiles mp ON mp.id = r.profile_id {where_sql}"
    );
    let page_sql = format!(
        "SELECT r.id, mp.model_name, r.language, r.task_category, r.backend_tag, r.case_id, \
         r.first_pass_score, r.code_quality_score, r.total_time_ms, r.throughput_tok_per_sec, \
         r.memory_usage_mb, r.oom, r.failure_class, r.error, r.created_at \
         FROM code_profile_runs r JOIN model_profiles mp ON mp.id = r.profile_id {where_sql} \
         ORDER BY r.created_at DESC LIMIT ${next_idx} OFFSET ${}",
        next_idx + 1
    );

    let mut count_query = sqlx::query(&count_sql);
    if let Some(m) = &filter.model {
        count_query = count_query.bind(m.clone());
    }
    if let Some(t) = &filter.task_category {
        count_query = count_query.bind(t.clone());
    }
    if let Some(l) = &filter.language {
        count_query = count_query.bind(l.clone());
    }
    if let Some(f) = &filter.failure_class {
        count_query = count_query.bind(f.clone());
    }
    if let Some(e) = filter.epoch.epoch() {
        count_query = count_query.bind(e.to_string());
    }
    let total: i64 = match count_query.fetch_one(pool).await {
        Ok(row) => {
            use sqlx::Row as _;
            row.try_get::<i64, _>(0).unwrap_or(0)
        }
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                return Ok((Vec::new(), 0));
            }
            return Err(ToolError::Database(format!("Failed to count code runs: {msg}")));
        }
    };

    type Row = (
        uuid::Uuid,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<f64>,
        Option<i32>,
        Option<f64>,
        Option<i32>,
        Option<bool>,
        Option<String>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    );
    let mut page_query = sqlx::query_as::<_, Row>(&page_sql);
    if let Some(m) = &filter.model {
        page_query = page_query.bind(m.clone());
    }
    if let Some(t) = &filter.task_category {
        page_query = page_query.bind(t.clone());
    }
    if let Some(l) = &filter.language {
        page_query = page_query.bind(l.clone());
    }
    if let Some(f) = &filter.failure_class {
        page_query = page_query.bind(f.clone());
    }
    if let Some(e) = filter.epoch.epoch() {
        page_query = page_query.bind(e.to_string());
    }
    page_query = page_query.bind(limit).bind(offset);

    match page_query.fetch_all(pool).await {
        Ok(rows) => Ok((
            rows.into_iter()
                .map(
                    |(
                        run_id,
                        model,
                        language,
                        task_category,
                        backend_tag,
                        case_id,
                        first_pass_score,
                        code_quality_score,
                        total_time_ms,
                        throughput_tok_per_sec,
                        memory_usage_mb,
                        oom,
                        failure_class,
                        error,
                        created_at,
                    )| CodeRunListRow {
                        run_id,
                        model,
                        language,
                        task_category,
                        backend_tag,
                        case_id,
                        first_pass_score,
                        code_quality_score,
                        total_time_ms,
                        throughput_tok_per_sec,
                        memory_usage_mb,
                        oom,
                        failure_class,
                        error,
                        created_at,
                    },
                )
                .collect(),
            total,
        )),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok((Vec::new(), 0))
            } else {
                Err(ToolError::Database(format!("Failed to read code runs: {msg}")))
            }
        }
    }
}

/// One (model, value, case_id, failure_class) tuple for [`crate::constellation::models_api`]'s
/// server-side quartile computation (`GET /api/terminus/mint/box`) — the raw
/// values per model for whichever `metric` was requested
/// (`total_time_ms`/`code_quality_score`), never shipped to the browser
/// unaggregated (the handler reduces these to quartiles before responding).
#[derive(Debug, Clone)]
pub struct BoxMetricRow {
    pub model: String,
    pub value: f64,
    pub run_id: uuid::Uuid,
    pub case_id: Option<String>,
    pub failure_class: Option<String>,
}

/// Read the raw per-run values of `metric` (`total_time_ms` or
/// `code_quality_score` — validated by the caller before this is reached, so
/// this function trusts its column-name argument) under `filter`, for
/// server-side quartile computation. Tolerates absent tables → empty vec.
pub async fn read_code_run_values_for_box(
    pool: &PgPool,
    metric_column: &str,
    filter: &CodeRunFilter,
) -> Result<Vec<BoxMetricRow>, ToolError> {
    let (where_sql, _next_idx) = filter.where_sql();
    let sql = format!(
        "SELECT mp.model_name, r.{metric_column}::double precision, r.id, r.case_id, r.failure_class \
         FROM code_profile_runs r JOIN model_profiles mp ON mp.id = r.profile_id {where_sql} \
         AND r.{metric_column} IS NOT NULL"
    );
    let mut query = sqlx::query_as::<_, (String, f64, uuid::Uuid, Option<String>, Option<String>)>(&sql);
    if let Some(m) = &filter.model {
        query = query.bind(m.clone());
    }
    if let Some(t) = &filter.task_category {
        query = query.bind(t.clone());
    }
    if let Some(l) = &filter.language {
        query = query.bind(l.clone());
    }
    if let Some(f) = &filter.failure_class {
        query = query.bind(f.clone());
    }
    if let Some(e) = filter.epoch.epoch() {
        query = query.bind(e.to_string());
    }
    match query.fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(model, value, run_id, case_id, failure_class)| BoxMetricRow {
                model,
                value,
                run_id,
                case_id,
                failure_class,
            })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read {metric_column} values for box plot: {msg}"
                )))
            }
        }
    }
}

/// One per-(model, language) rollup row for `GET
/// /api/terminus/mint/language-stats` — same shape/formulas as the
/// `model_language_stats` matview (`assistant/schema.rs`), joined to
/// `model_profiles.vram_gb` for the Pareto scatter's point-size dimension.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanguageStatsRow {
    pub model: String,
    pub language: String,
    pub n_scored: Option<i64>,
    pub mean_score: Option<f64>,
    pub stddev_score: Option<f64>,
    pub retry_lift: Option<f64>,
    pub mean_throughput: Option<f64>,
    pub mean_latency_ms: Option<f64>,
    pub p95_latency_ms: Option<f64>,
    pub total_gpu_seconds: Option<f64>,
    pub quality_per_gpu_second: Option<f64>,
    pub pass_hat_3: Option<f64>,
    pub vram_gb: Option<f64>,
}

/// Read per-(model, language) coder-sweep rollups (optionally scoped to one
/// `language`), properly [`EpochSelector`]-scoped by `harness_version`
/// (review fix: the earlier version read the pre-aggregated
/// `model_language_stats` MATERIALIZED VIEW directly, which has no
/// `harness_version` column at all — it is scoped only to `mem_config =
/// 'dynamic_gtt'` at CREATE time — so an `epoch=v2` or `epoch=<current>`
/// request silently returned the SAME all-epochs-blended numbers regardless
/// of the requested epoch). This version recomputes the matview's own
/// formulas (verbatim: same `case_counts`/`pass_k` CTEs for `pass_hat_3`,
/// same `mean_score`/`stddev_score`/`retry_lift`/`mean_throughput`/
/// `mean_latency_ms`/`p95_latency_ms`/`total_gpu_seconds`/
/// `quality_per_gpu_second` expressions) directly over `code_profile_runs`
/// with an added `harness_version` filter, so it is ALWAYS epoch-correct and
/// never depends on the matview having been refreshed. Still scoped to
/// `mem_config = 'dynamic_gtt'`, matching the matview's own scoping decision
/// (see that migration's comment for why). Tolerates absent tables → empty
/// vec.
pub async fn read_language_stats(
    pool: &PgPool,
    language: Option<&str>,
    epoch: &EpochSelector,
) -> Result<Vec<LanguageStatsRow>, ToolError> {
    // Bind order: epoch first (if scoped), then language (if given) — both
    // fragments below reference the SAME placeholder index for a given bind,
    // just qualified differently per-CTE (`case_counts`'s bare
    // `code_profile_runs` vs. the outer query's `r` alias).
    let mut idx = 1usize;
    let epoch_bare = crate::intake::epoch_where_fragment(epoch, idx);
    let epoch_aliased = epoch_bare.replace("harness_version", "r.harness_version");
    if epoch.epoch().is_some() {
        idx += 1;
    }
    let (lang_bare, lang_aliased) = if language.is_some() {
        (format!("AND language = ${idx}"), format!("AND r.language = ${idx}"))
    } else {
        (String::new(), String::new())
    };

    let sql = format!(
        "WITH case_counts AS ( \
             SELECT profile_id, language, case_id, \
                 count(*) AS n_samples, \
                 count(*) FILTER (WHERE error IS NULL AND first_pass_score >= 3) AS c_success \
             FROM code_profile_runs \
             WHERE mem_config = 'dynamic_gtt' AND {epoch_bare} {lang_bare} \
             GROUP BY profile_id, language, case_id \
         ), \
         pass_k AS ( \
             SELECT profile_id, language, \
                 avg(CASE WHEN n_samples < 3 THEN NULL ELSE \
                     power(c_success::float / greatest(n_samples, 1)::float, 3) END) AS pass_hat_3 \
             FROM case_counts GROUP BY profile_id, language \
         ) \
         SELECT mp.model_name, r.language, \
             count(*) FILTER (WHERE r.error IS NULL) AS n_scored, \
             avg(r.first_pass_score::double precision) FILTER (WHERE r.error IS NULL) AS mean_score, \
             stddev(r.first_pass_score::double precision) FILTER (WHERE r.error IS NULL) AS stddev_score, \
             avg((r.retry_score - r.first_pass_score)::double precision) FILTER (WHERE r.retry_score IS NOT NULL) AS retry_lift, \
             avg(r.throughput_tok_per_sec) AS mean_throughput, \
             avg(r.total_time_ms::double precision) AS mean_latency_ms, \
             percentile_cont(0.95) WITHIN GROUP (ORDER BY r.total_time_ms) AS p95_latency_ms, \
             sum(r.total_time_ms)::float / 1000.0 AS total_gpu_seconds, \
             avg(r.first_pass_score::double precision) FILTER (WHERE r.error IS NULL) \
                 / NULLIF( \
                     (sum(r.total_time_ms)::float / 1000.0) \
                         / NULLIF(count(*) FILTER (WHERE r.error IS NULL), 0)::float, \
                     0) AS quality_per_gpu_second, \
             pk.pass_hat_3, \
             mp.vram_gb \
         FROM code_profile_runs r \
         JOIN model_profiles mp ON mp.id = r.profile_id \
         LEFT JOIN pass_k pk ON pk.profile_id = r.profile_id AND pk.language = r.language \
         WHERE r.mem_config = 'dynamic_gtt' AND {epoch_aliased} {lang_aliased} \
         GROUP BY mp.model_name, r.language, pk.pass_hat_3, mp.vram_gb \
         ORDER BY mp.model_name, r.language"
    );
    type Row = (
        String,
        String,
        Option<i64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    );
    let mut query = sqlx::query_as::<_, Row>(&sql);
    if let Some(e) = epoch.epoch() {
        query = query.bind(e.to_string());
    }
    if let Some(l) = language {
        query = query.bind(l);
    }
    match query.fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(
                |(
                    model,
                    language,
                    n_scored,
                    mean_score,
                    stddev_score,
                    retry_lift,
                    mean_throughput,
                    mean_latency_ms,
                    p95_latency_ms,
                    total_gpu_seconds,
                    quality_per_gpu_second,
                    pass_hat_3,
                    vram_gb,
                )| LanguageStatsRow {
                    model,
                    language,
                    n_scored,
                    mean_score,
                    stddev_score,
                    retry_lift,
                    mean_throughput,
                    mean_latency_ms,
                    p95_latency_ms,
                    total_gpu_seconds,
                    quality_per_gpu_second,
                    pass_hat_3,
                    vram_gb,
                },
            )
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read per-language coder rollups: {msg}"
                )))
            }
        }
    }
}

/// Per-(model, failure_class) run counts for `GET /api/terminus/mint/failures`,
/// excluding `failure_class = 'none'` (a "failures" view is about failures) and
/// scoped by [`EpochSelector`] + an optional `task_category`. Tolerates the
/// table being absent → empty vec.
pub async fn read_failure_class_counts(
    pool: &PgPool,
    epoch: &EpochSelector,
    task_category: Option<&str>,
) -> Result<Vec<(String, String, i64)>, ToolError> {
    let mut idx = 1usize;
    let mut clauses = vec![
        "r.failure_class IS NOT NULL".to_string(),
        "r.failure_class <> 'none'".to_string(),
    ];
    if task_category.is_some() {
        clauses.push(format!("r.task_category = ${idx}"));
        idx += 1;
    }
    clauses.push(crate::intake::epoch_where_fragment(epoch, idx).replace(
        "harness_version",
        "r.harness_version",
    ));
    let sql = format!(
        "SELECT mp.model_name, r.failure_class, count(*)::bigint \
         FROM code_profile_runs r JOIN model_profiles mp ON mp.id = r.profile_id \
         WHERE {} GROUP BY mp.model_name, r.failure_class",
        clauses.join(" AND ")
    );
    let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
    if let Some(t) = task_category {
        query = query.bind(t);
    }
    if let Some(e) = epoch.epoch() {
        query = query.bind(e.to_string());
    }
    match query.fetch_all(pool).await {
        Ok(rows) => Ok(rows),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read failure-class counts: {msg}"
                )))
            }
        }
    }
}

/// One `context_profile_runs` tier row for `GET
/// /api/terminus/mint/context-profiles`, joined to its model's name and
/// (best-effort) `max_context_safe`/`max_context_absolute` from the latest
/// operational profile.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextProfileTierRow {
    pub model: String,
    pub context_tokens: i32,
    pub throughput_tok_per_sec: Option<f64>,
    pub ttft_ms: Option<i32>,
    pub recall_score: Option<i32>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
    pub max_context_safe: Option<i32>,
}

/// Read every `context_profile_runs` tier for the requested `models` (empty
/// slice ⇒ every model), joined to `model_profiles` + the latest
/// `model_operational_profiles.max_context_safe`. Tolerates absent tables →
/// empty vec.
pub async fn read_context_profiles(
    pool: &PgPool,
    models: &[String],
) -> Result<Vec<ContextProfileTierRow>, ToolError> {
    let where_sql = if models.is_empty() {
        ""
    } else {
        "WHERE mp.model_name = ANY($1)"
    };
    // The `max_context_safe` subselect resolves the model's LATEST operational
    // profile BY MODEL NAME (review-cycle-2 fix — the old subselect scoped to the
    // run's own `mp.id`, so a run joined to an older `model_profiles` row carried
    // that older profile's value instead of the model's latest). Two-attempt
    // ordering for the same missing-`profile_date` reason as
    // [`read_latest_operational_profile_for_model`].
    let sql_for = |order_expr: &str| {
        format!(
            "SELECT mp.model_name, cr.context_tokens, cr.throughput_tok_per_sec, cr.ttft_ms, \
             cr.recall_score, cr.memory_usage_mb, cr.oom, \
             (SELECT op.max_context_safe FROM model_operational_profiles op \
              JOIN model_profiles mp2 ON mp2.id = op.profile_id \
              WHERE mp2.model_name = mp.model_name \
              ORDER BY {order_expr} DESC, op.created_at DESC LIMIT 1) AS max_context_safe \
             FROM context_profile_runs cr JOIN model_profiles mp ON mp.id = cr.profile_id {where_sql} \
             ORDER BY mp.model_name, cr.context_tokens"
        )
    };
    // Third fallback (cycle-3 review fix): `model_operational_profiles` is
    // ANCILLARY data here — if that table alone is absent, the subselect must
    // not erase the primary `context_profile_runs` rows; retry without the
    // subselect (max_context_safe := NULL) instead.
    let sql_no_subselect = format!(
        "SELECT mp.model_name, cr.context_tokens, cr.throughput_tok_per_sec, cr.ttft_ms, \
         cr.recall_score, cr.memory_usage_mb, cr.oom, \
         NULL::integer AS max_context_safe \
         FROM context_profile_runs cr JOIN model_profiles mp ON mp.id = cr.profile_id {where_sql} \
         ORDER BY mp.model_name, cr.context_tokens"
    );
    let sql = sql_for("COALESCE(mp2.profile_date, mp2.created_at)");
    type Row = (
        String,
        i32,
        Option<f64>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        bool,
        Option<i32>,
    );
    let run = |sql: String, models: Vec<String>| async move {
        let mut query = sqlx::query_as::<_, Row>(&sql);
        if !models.is_empty() {
            query = query.bind(models);
        }
        query.fetch_all(pool).await
    };
    let primary = run(sql, models.to_vec()).await;
    let attempt = match primary {
        Err(e) if is_missing_column_error(&e.to_string()) => {
            run(sql_for("mp2.created_at"), models.to_vec()).await
        }
        // The operational-profiles table alone missing must not drop primary
        // context rows (cycle-3 fix) — the error names the missing relation,
        // so only retry subselect-free when it is the ANCILLARY table.
        Err(e)
            if is_missing_relation_error(&e.to_string())
                && e.to_string().contains("model_operational_profiles") =>
        {
            run(sql_no_subselect, models.to_vec()).await
        }
        other => other,
    };
    match attempt {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(
                |(model, context_tokens, throughput_tok_per_sec, ttft_ms, recall_score, memory_usage_mb, oom, max_context_safe)| {
                    ContextProfileTierRow {
                        model,
                        context_tokens,
                        throughput_tok_per_sec,
                        ttft_ms,
                        recall_score,
                        memory_usage_mb,
                        oom,
                        max_context_safe,
                    }
                },
            )
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read context profiles: {msg}"
                )))
            }
        }
    }
}

/// One (dimension, model_id, mean value, mean std_dev, n, any-low-confidence)
/// rollup row for `GET /api/terminus/mint/dimensions` (the C1 capability
/// radar). Scoped by an [`EpochSelector`] against `assistant_profile_run.
/// harness_version` (mirrors [`read_assistant_dimension_counts`]'s join, but
/// rolled up to mean/std_dev/n instead of a bare count, and over EVERY
/// dimension rather than counts-only).
#[derive(Debug, Clone)]
pub struct DimensionRollupRow {
    pub model_id: String,
    pub dimension: String,
    pub mean_value: f64,
    pub mean_std_dev: Option<f64>,
    pub n: i64,
    pub any_low_confidence: bool,
}

/// Read the per-(model, dimension) rollup (mean `value`, mean `std_dev`,
/// sample count, whether any contributing row was `low_confidence`) scoped by
/// `epoch`, preferring `judge = 'panel'` rows when any exist for a given
/// (model, dimension) — falling back to every judge's rows otherwise (a
/// `panel`-only filter would silently blank a model that was only ever judged
/// by a single-judge pipeline). Tolerates absent tables → empty vec.
pub async fn read_assistant_dimension_rollup(
    pool: &PgPool,
    epoch: &EpochSelector,
) -> Result<Vec<DimensionRollupRow>, ToolError> {
    let where_frag = crate::intake::epoch_where_fragment(epoch, 1).replace(
        "harness_version",
        "run.harness_version",
    );
    let sql = format!(
        "WITH scoped AS ( \
             SELECT s.* FROM assistant_dimension_score s \
             JOIN assistant_profile_run run ON run.id = s.run_id \
             WHERE {where_frag} \
         ), \
         preferred AS ( \
             SELECT * FROM scoped WHERE judge = 'panel' \
         ) \
         SELECT model_id, dimension, avg(value), avg(std_dev), count(*)::bigint, bool_or(low_confidence) \
         FROM (SELECT * FROM preferred \
               UNION ALL \
               SELECT s.* FROM scoped s \
               WHERE NOT EXISTS ( \
                   SELECT 1 FROM preferred p \
                   WHERE p.model_id = s.model_id AND p.dimension = s.dimension \
               )) merged \
         GROUP BY model_id, dimension"
    );
    let mut query = sqlx::query_as::<
        _,
        (String, String, Option<f64>, Option<f64>, i64, Option<bool>),
    >(&sql);
    if let Some(e) = epoch.epoch() {
        query = query.bind(e.to_string());
    }
    match query.fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .filter_map(|(model_id, dimension, mean_value, mean_std_dev, n, low_conf)| {
                mean_value.map(|v| DimensionRollupRow {
                    model_id,
                    dimension,
                    mean_value: v,
                    mean_std_dev,
                    n,
                    any_low_confidence: low_conf.unwrap_or(false),
                })
            })
            .collect()),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read assistant dimension rollup: {msg}"
                )))
            }
        }
    }
}

/// Activity-histogram day bucket: run counts by suite for `GET
/// /api/terminus/mint/activity`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ActivityDayCounts {
    pub date: chrono::NaiveDate,
    pub code: i64,
    pub context: i64,
    pub agent: i64,
}

/// Read per-day run counts across all three suites (`code_profile_runs`,
/// `context_profile_runs`, `agent_profile_runs`) for the last `range_days`
/// days, merged into one `{date, code, context, agent}` series. Each suite's
/// count is read independently and tolerated missing (an absent suite table
/// contributes zero counts, never fails the whole endpoint).
pub async fn read_activity_histogram(
    pool: &PgPool,
    range_days: i64,
) -> Result<Vec<ActivityDayCounts>, ToolError> {
    use std::collections::BTreeMap;

    async fn day_counts(
        pool: &PgPool,
        table: &str,
        range_days: i64,
    ) -> Result<Vec<(chrono::NaiveDate, i64)>, ToolError> {
        let sql = format!(
            "SELECT date(created_at), count(*)::bigint FROM {table} \
             WHERE created_at >= now() - make_interval(days => $1) \
             GROUP BY date(created_at)"
        );
        match sqlx::query_as::<_, (chrono::NaiveDate, i64)>(&sql)
            .bind(range_days as i32)
            .fetch_all(pool)
            .await
        {
            Ok(rows) => Ok(rows),
            Err(e) => {
                let msg = e.to_string();
                if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                    Ok(Vec::new())
                } else {
                    Err(ToolError::Database(format!("Failed to read {table} activity: {msg}")))
                }
            }
        }
    }

    let code = day_counts(pool, "code_profile_runs", range_days).await?;
    let context = day_counts(pool, "context_profile_runs", range_days).await?;
    let agent = day_counts(pool, "agent_profile_runs", range_days).await?;

    let mut by_day: BTreeMap<chrono::NaiveDate, ActivityDayCounts> = BTreeMap::new();
    for (date, n) in code {
        by_day.entry(date).or_insert_with(|| ActivityDayCounts { date, ..Default::default() }).code = n;
    }
    for (date, n) in context {
        by_day.entry(date).or_insert_with(|| ActivityDayCounts { date, ..Default::default() }).context = n;
    }
    for (date, n) in agent {
        by_day.entry(date).or_insert_with(|| ActivityDayCounts { date, ..Default::default() }).agent = n;
    }
    Ok(by_day.into_values().collect())
}

/// The fleet's best model by `model_language_stats.pass_hat_3` (the C0
/// overview tile's "fleet-best model" figure) — the single highest
/// `pass_hat_3` across every (model, language) row. Tolerates the matview
/// being absent → `None`.
pub async fn read_best_model_by_pass_hat_3(
    pool: &PgPool,
) -> Result<Option<(String, f64)>, ToolError> {
    let sql = "SELECT mp.model_name, mls.pass_hat_3 FROM model_language_stats mls \
               JOIN model_profiles mp ON mp.id = mls.profile_id \
               WHERE mls.pass_hat_3 IS NOT NULL \
               ORDER BY mls.pass_hat_3 DESC LIMIT 1";
    match sqlx::query_as::<_, (String, f64)>(sql).fetch_optional(pool).await {
        Ok(row) => Ok(row),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(None)
            } else {
                Err(ToolError::Database(format!(
                    "Failed to read fleet-best model: {msg}"
                )))
            }
        }
    }
}

/// GPU-hours (`Σ total_time_ms / 3_600_000`) across `code_profile_runs` for
/// the given [`EpochSelector`] — the C0 overview tile. Tolerates the table
/// being absent → `0.0`.
pub async fn read_gpu_hours(pool: &PgPool, epoch: &EpochSelector) -> Result<f64, ToolError> {
    let where_frag = crate::intake::epoch_where_fragment(epoch, 1);
    let sql = format!(
        "SELECT COALESCE(sum(total_time_ms), 0)::double precision / 3600000.0 \
         FROM code_profile_runs WHERE {where_frag}"
    );
    let mut query = sqlx::query_as::<_, (f64,)>(&sql);
    if let Some(e) = epoch.epoch() {
        query = query.bind(e.to_string());
    }
    match query.fetch_one(pool).await {
        Ok((v,)) => Ok(v),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok(0.0)
            } else {
                Err(ToolError::Database(format!("Failed to read GPU-hours: {msg}")))
            }
        }
    }
}

/// Run counts (code + context + agent) for the C0 overview tile, scoped by
/// [`EpochSelector`] on ALL THREE suites (review fix: an earlier version only
/// scoped `code`, since `context_profile_runs`/`agent_profile_runs` have no
/// `harness_version` column — see the module-level note on
/// [`read_language_stats`] — and silently counted every row regardless of
/// `epoch`, overcounting the summary tile for any non-`All` epoch). `code` is
/// scoped exactly via `harness_version`, its real epoch-partition key.
/// `context`/`agent` are scoped by TIME instead, via
/// [`epoch_time_window`]'s `[became_current_at, next_epoch's
/// became_current_at)` window over `intake_epoch_marker` — a best-effort
/// proxy (these suites don't carry their own epoch key) that is nonetheless
/// exact for the common case (`Current`/no marker gaps) and never worse than
/// the old "count everything" behavior. `epoch=all` (no marker lookup) counts
/// every row for every suite, same as before. Tolerates absent tables → `0`
/// for that suite.
pub async fn read_run_counts(pool: &PgPool, epoch: &EpochSelector) -> Result<(i64, i64, i64), ToolError> {
    let where_frag = crate::intake::epoch_where_fragment(epoch, 1);
    let code_sql = format!("SELECT count(*)::bigint FROM code_profile_runs WHERE {where_frag}");
    let mut code_query = sqlx::query_as::<_, (i64,)>(&code_sql);
    if let Some(e) = epoch.epoch() {
        code_query = code_query.bind(e.to_string());
    }
    let code = match code_query.fetch_one(pool).await {
        Ok((n,)) => n,
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                0
            } else {
                return Err(ToolError::Database(format!("Failed to count code runs: {msg}")));
            }
        }
    };

    let window = match epoch.epoch() {
        None => None, // `All` — unscoped, matches `code`'s own `TRUE` fragment.
        Some(e) => epoch_time_window(pool, e).await?,
    };
    let context = count_rows_in_window(pool, "context_profile_runs", window).await?;
    let agent = count_rows_in_window(pool, "agent_profile_runs", window).await?;
    Ok((code, context, agent))
}

/// The `[start, end)` time window a suite lacking its own epoch column should
/// count rows within, for `epoch`'s marker: `start` = that epoch's
/// `became_current_at`; `end` = the NEXT-recorded epoch's `became_current_at`
/// (`None` when `epoch` is still the newest marker — i.e. an open-ended
/// window). Returns `None` (⇒ caller counts unscoped) when `epoch` has no
/// recorded marker at all — this is the honest degrade: without a marker
/// there is no time boundary to scope by, so counting everything is more
/// truthful than fabricating a boundary.
async fn epoch_time_window(
    pool: &PgPool,
    epoch: &str,
) -> Result<Option<(chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)>, ToolError> {
    let Some(marker) = read_epoch_marker(pool, epoch).await? else {
        return Ok(None);
    };
    let next_sql = "SELECT min(became_current_at) FROM intake_epoch_marker WHERE became_current_at > $1";
    let next: Option<chrono::DateTime<chrono::Utc>> =
        match sqlx::query_as::<_, (Option<chrono::DateTime<chrono::Utc>>,)>(next_sql)
            .bind(marker.became_current_at)
            .fetch_one(pool)
            .await
        {
            Ok((n,)) => n,
            Err(e) => {
                let msg = e.to_string();
                if is_missing_relation_error(&msg) {
                    None
                } else {
                    return Err(ToolError::Database(format!(
                        "Failed to read next epoch marker after {epoch}: {msg}"
                    )));
                }
            }
        };
    Ok(Some((marker.became_current_at, next)))
}

/// `count(*)` over `table`, optionally scoped to a `[start, end)` `created_at`
/// window (see [`epoch_time_window`]) — `None` ⇒ unscoped (every row).
/// Tolerates the relation being absent → `0`.
async fn count_rows_in_window(
    pool: &PgPool,
    table: &str,
    window: Option<(chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)>,
) -> Result<i64, ToolError> {
    let (where_sql, has_end) = match window {
        None => (String::new(), false),
        Some((_, Some(_))) => ("WHERE created_at >= $1 AND created_at < $2".to_string(), true),
        Some((_, None)) => ("WHERE created_at >= $1".to_string(), false),
    };
    let sql = format!("SELECT count(*)::bigint FROM {table} {where_sql}");
    let mut query = sqlx::query_as::<_, (i64,)>(&sql);
    if let Some((start, end)) = window {
        query = query.bind(start);
        if has_end {
            query = query.bind(end.unwrap());
        }
    }
    match query.fetch_one(pool).await {
        Ok((n,)) => Ok(n),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(0)
            } else {
                Err(ToolError::Database(format!("Failed to count {table}: {msg}")))
            }
        }
    }
}

/// Count distinct models profiled at all (`model_profiles`) — the C0 overview
/// tile's "models profiled" figure. Tolerates the table being absent → `0`
/// (should not happen in practice — `model_profiles` is this module's own
/// root table — but kept consistent with every other CONST-21 read here).
pub async fn read_models_profiled_count(pool: &PgPool) -> Result<i64, ToolError> {
    let sql = "SELECT count(DISTINCT model_name)::bigint FROM model_profiles";
    match sqlx::query_as::<_, (i64,)>(sql).fetch_one(pool).await {
        Ok((n,)) => Ok(n),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                Ok(0)
            } else {
                Err(ToolError::Database(format!(
                    "Failed to count profiled models: {msg}"
                )))
            }
        }
    }
}

/// One `context_profile_runs` row for the MINT "runs" table view
/// (`GET /api/terminus/mint/runs?suite=context`) — `context_profile_runs` has
/// no `task_category`/`failure_class`/`language` columns (unlike the coder
/// suite), so this suite's page only accepts a `model` filter; the handler
/// documents that narrower contract rather than silently accepting and
/// ignoring the coder-suite params.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextRunListRow {
    pub run_id: uuid::Uuid,
    pub model: String,
    pub context_tokens: i32,
    pub throughput_tok_per_sec: Option<f64>,
    pub ttft_ms: Option<i32>,
    pub total_time_ms: Option<i32>,
    pub recall_score: Option<i32>,
    pub coherence_score: Option<f64>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Page through `context_profile_runs` (optionally scoped to one `model`),
/// newest first, with the same `(rows, total)` contract as
/// [`read_code_runs_page`]. Tolerates absent tables → `(vec![], 0)`.
///
/// NOTE on pagination bounds: `limit`/`offset` arrive PRE-CLAMPED — the HTTP
/// layer's `paginate()` (`crate::constellation::models_api`) enforces the
/// spec's default-50/max-500 clamp before any storage read; these fns bind
/// whatever they are given (single enforcement point, deliberately not
/// duplicated here).
pub async fn read_context_runs_page(
    pool: &PgPool,
    model: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ContextRunListRow>, i64), ToolError> {
    let where_sql = if model.is_some() { "WHERE mp.model_name = $1" } else { "" };
    let count_sql = format!(
        "SELECT count(*)::bigint FROM context_profile_runs r \
         JOIN model_profiles mp ON mp.id = r.profile_id {where_sql}"
    );
    let mut count_query = sqlx::query(&count_sql);
    if let Some(m) = model {
        count_query = count_query.bind(m);
    }
    let total: i64 = match count_query.fetch_one(pool).await {
        Ok(row) => {
            use sqlx::Row as _;
            row.try_get::<i64, _>(0).unwrap_or(0)
        }
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                return Ok((Vec::new(), 0));
            }
            return Err(ToolError::Database(format!("Failed to count context runs: {msg}")));
        }
    };

    let (limit_idx, offset_idx) = if model.is_some() { (2, 3) } else { (1, 2) };
    let page_sql = format!(
        "SELECT r.id, mp.model_name, r.context_tokens, r.throughput_tok_per_sec, r.ttft_ms, \
         r.total_time_ms, r.recall_score, r.coherence_score, r.memory_usage_mb, r.oom, r.error, \
         r.created_at \
         FROM context_profile_runs r JOIN model_profiles mp ON mp.id = r.profile_id {where_sql} \
         ORDER BY r.created_at DESC LIMIT ${limit_idx} OFFSET ${offset_idx}"
    );
    type Row = (
        uuid::Uuid,
        String,
        i32,
        Option<f64>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<f64>,
        Option<i32>,
        bool,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    );
    let mut page_query = sqlx::query_as::<_, Row>(&page_sql);
    if let Some(m) = model {
        page_query = page_query.bind(m);
    }
    page_query = page_query.bind(limit).bind(offset);
    match page_query.fetch_all(pool).await {
        Ok(rows) => Ok((
            rows.into_iter()
                .map(
                    |(
                        run_id,
                        model,
                        context_tokens,
                        throughput_tok_per_sec,
                        ttft_ms,
                        total_time_ms,
                        recall_score,
                        coherence_score,
                        memory_usage_mb,
                        oom,
                        error,
                        created_at,
                    )| ContextRunListRow {
                        run_id,
                        model,
                        context_tokens,
                        throughput_tok_per_sec,
                        ttft_ms,
                        total_time_ms,
                        recall_score,
                        coherence_score,
                        memory_usage_mb,
                        oom,
                        error,
                        created_at,
                    },
                )
                .collect(),
            total,
        )),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok((Vec::new(), 0))
            } else {
                Err(ToolError::Database(format!("Failed to read context runs: {msg}")))
            }
        }
    }
}

/// One `agent_profile_runs` row for `GET /api/terminus/mint/runs?suite=agent`.
/// Same narrower-contract note as [`read_context_runs_page`]: only a `model`
/// filter (this table has no `task_category`/`language`/`failure_class`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentRunListRow {
    pub run_id: uuid::Uuid,
    pub model: String,
    pub test_name: Option<String>,
    pub tool_count_available: Option<i32>,
    pub correct_tool_selected: Option<bool>,
    pub tool_params_valid: Option<bool>,
    pub multi_step_completed: Option<bool>,
    pub instruction_followed: Option<bool>,
    pub hallucination_detected: Option<bool>,
    pub response_quality_score: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Page through `agent_profile_runs` (optionally scoped to one `model`),
/// newest first. Tolerates absent tables → `(vec![], 0)`.
pub async fn read_agent_runs_page(
    pool: &PgPool,
    model: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<AgentRunListRow>, i64), ToolError> {
    let where_sql = if model.is_some() { "WHERE mp.model_name = $1" } else { "" };
    let count_sql = format!(
        "SELECT count(*)::bigint FROM agent_profile_runs r \
         JOIN model_profiles mp ON mp.id = r.profile_id {where_sql}"
    );
    let mut count_query = sqlx::query(&count_sql);
    if let Some(m) = model {
        count_query = count_query.bind(m);
    }
    let total: i64 = match count_query.fetch_one(pool).await {
        Ok(row) => {
            use sqlx::Row as _;
            row.try_get::<i64, _>(0).unwrap_or(0)
        }
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                return Ok((Vec::new(), 0));
            }
            return Err(ToolError::Database(format!("Failed to count agent runs: {msg}")));
        }
    };

    let (limit_idx, offset_idx) = if model.is_some() { (2, 3) } else { (1, 2) };
    let page_sql = format!(
        "SELECT r.id, mp.model_name, r.test_name, r.tool_count_available, r.correct_tool_selected, \
         r.tool_params_valid, r.multi_step_completed, r.instruction_followed, r.hallucination_detected, \
         r.response_quality_score, r.total_time_ms, r.error, r.created_at \
         FROM agent_profile_runs r JOIN model_profiles mp ON mp.id = r.profile_id {where_sql} \
         ORDER BY r.created_at DESC LIMIT ${limit_idx} OFFSET ${offset_idx}"
    );
    type Row = (
        uuid::Uuid,
        String,
        Option<String>,
        Option<i32>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        Option<f64>,
        Option<i32>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    );
    let mut page_query = sqlx::query_as::<_, Row>(&page_sql);
    if let Some(m) = model {
        page_query = page_query.bind(m);
    }
    page_query = page_query.bind(limit).bind(offset);
    match page_query.fetch_all(pool).await {
        Ok(rows) => Ok((
            rows.into_iter()
                .map(
                    |(
                        run_id,
                        model,
                        test_name,
                        tool_count_available,
                        correct_tool_selected,
                        tool_params_valid,
                        multi_step_completed,
                        instruction_followed,
                        hallucination_detected,
                        response_quality_score,
                        total_time_ms,
                        error,
                        created_at,
                    )| AgentRunListRow {
                        run_id,
                        model,
                        test_name,
                        tool_count_available,
                        correct_tool_selected,
                        tool_params_valid,
                        multi_step_completed,
                        instruction_followed,
                        hallucination_detected,
                        response_quality_score,
                        total_time_ms,
                        error,
                        created_at,
                    },
                )
                .collect(),
            total,
        )),
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) || is_missing_column_error(&msg) {
                Ok((Vec::new(), 0))
            } else {
                Err(ToolError::Database(format!("Failed to read agent runs: {msg}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes tests that mutate the shared DATABASE_URL/INTAKE_DATABASE_URL
    // process env vars — `cargo test` runs tests in the same process on
    // multiple threads by default.
    use serial_test::serial;

    /// S125 FIX1 regression: `read_agent_rollups` decodes its tool-accuracy
    /// column into `Option<f64>` (`FLOAT8`). `AVG(CASE ... THEN 1.0 ELSE 0.0
    /// END)` returns Postgres `NUMERIC` (the arms are numeric literals), which
    /// does NOT decode into `f64` and crashed `refresh_fleet_catalog` at
    /// runtime. Since this crate has no live-Postgres test harness (see the
    /// sibling SQL-constant tests), assert at the string level that the query
    /// pins the aggregate to `double precision` (→ `FLOAT8`) so the `Row`
    /// tuple's column types match. The count column stays pinned to `::bigint`
    /// for the `i64` decode.
    #[test]
    fn agent_rollups_sql_casts_accuracy_to_float8() {
        // The tool-accuracy AVG must be cast to double precision so the numeric
        // result decodes as Option<f64> rather than crashing on NUMERIC≠FLOAT8.
        assert!(
            SELECT_AGENT_ROLLUPS_SQL.contains(
                "avg(CASE WHEN ap.correct_tool_selected THEN 1.0 ELSE 0.0 END)::double precision"
            ),
            "agent-rollups AVG must be ::double precision, got: {SELECT_AGENT_ROLLUPS_SQL}"
        );
        // Count stays ::bigint for the i64 decode.
        assert!(SELECT_AGENT_ROLLUPS_SQL.contains("count(*)::bigint"));
    }

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
        // The last bind param ($30 = `failure_class`, MINT2-02) is the final
        // VALUES entry before the literal `false` for `finalized` — confirming
        // the insert never falls through to the column's `DEFAULT true`. ($29 =
        // `task_category` is the last of the six MINT2-01 factor binds; $30
        // appends the MINT2-02 failure_class immediately before `finalized`.)
        assert!(
            INSERT_CODE_RUN_V2_SQL.contains("$29, $30, false) RETURNING id"),
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
    /// MINT2-01 measurement-factor columns, then the MINT2-02 `failure_class`,
    /// then `finalized` last — so it never falls through to a column default.
    #[test]
    fn insert_code_run_v2_sql_includes_sample_index_and_vuln_count_last() {
        assert!(INSERT_CODE_RUN_V2_SQL.contains(
            "well_formed, sample_index, vuln_finding_count, \
      quant, reasoning_enabled, context_window_launched, temperature, top_p, task_category, \
      failure_class, finalized)"
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

        // Sanity: placeholders run $1..$30 contiguously.
        let max_placeholder = 30;
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

    // ---- MINT2-02: structured failure_class ----------------------------

    /// The build-scenario insert names the `failure_class` column (MINT2-02),
    /// positioned immediately before the literal `finalized`.
    #[test]
    fn insert_code_run_v2_sql_names_failure_class_before_finalized() {
        assert!(INSERT_CODE_RUN_V2_SQL.contains("task_category, failure_class, finalized)"));
    }

    /// `failure_class` defaults to `None` (a writer that doesn't set it never
    /// mislabels a row) and is settable like any other field.
    #[test]
    fn code_run_row_v2_failure_class_defaults_none_and_is_settable() {
        assert_eq!(CodeRunRowV2::default().failure_class, None);
        let row = CodeRunRowV2 {
            failure_class: Some("non_viable_vram".to_string()),
            ..Default::default()
        };
        assert_eq!(row.failure_class.as_deref(), Some("non_viable_vram"));
    }

    /// The failure_class read path is a SEPARATE query from the MINT2-01 factor
    /// read (so a DB migrated for MINT2-01 but not MINT2-02 keeps its real
    /// factors), with its own NULL-typed fallback for the un-migrated schema,
    /// still scoped by id — reusing the same missing-column detector.
    #[test]
    fn failure_class_select_sql_shapes() {
        assert!(SELECT_CODE_RUN_FAILURE_CLASS_SQL.contains("SELECT failure_class"));
        assert!(SELECT_CODE_RUN_FAILURE_CLASS_SQL.contains("WHERE id = $1"));
        assert!(SELECT_CODE_RUN_FAILURE_CLASS_FALLBACK_SQL.contains("NULL::text"));
        assert!(SELECT_CODE_RUN_FAILURE_CLASS_FALLBACK_SQL.contains("WHERE id = $1"));
        // Must NOT reference the MINT2-01 factor columns — independence is the point.
        assert!(!SELECT_CODE_RUN_FAILURE_CLASS_SQL.contains("quant"));
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

    // ---- MINT2-05: harness-version epochs ------------------------------

    /// The missing-RELATION detector (used by the aggregate read AND the new
    /// epoch-marker read) matches only "relation … does not exist", never an
    /// unrelated DB error or a missing-COLUMN error.
    #[test]
    fn is_missing_relation_error_matches_only_missing_relation() {
        assert!(is_missing_relation_error(
            "error returned from database: relation \"code_run_aggregates\" does not exist"
        ));
        assert!(is_missing_relation_error(
            "relation \"intake_epoch_marker\" does not exist"
        ));
        // A missing COLUMN is a different case (handled by is_missing_column_error).
        assert!(!is_missing_relation_error("column \"quant\" does not exist"));
        assert!(!is_missing_relation_error("connection refused"));
    }

    /// The selector-aware aggregate read scopes to ONE epoch for
    /// Current/Only (binding `harness_version = $1`) and to EVERY epoch for
    /// All (a bind-free `TRUE`), reusing the central epoch fragment.
    #[test]
    fn epoch_where_fragment_drives_selected_aggregate_scoping() {
        // Current resolves to the one central epoch value.
        assert_eq!(
            crate::intake::epoch_where_fragment(&EpochSelector::Current, 1),
            "harness_version = $1"
        );
        // Only(legacy) is still a single-epoch filter (legacy stays queryable).
        assert_eq!(
            crate::intake::epoch_where_fragment(&EpochSelector::Only("v1".into()), 1),
            "harness_version = $1"
        );
        // All → no filter, so legacy + current both return (provenance view).
        assert_eq!(
            crate::intake::epoch_where_fragment(&EpochSelector::All, 1),
            "TRUE"
        );
        // ...and the selector reports whether a bind is consumed.
        assert_eq!(EpochSelector::Current.epoch(), Some(crate::intake::current_epoch()));
        assert_eq!(EpochSelector::All.epoch(), None);
    }

    /// The stored-aggregate tuple mapper round-trips every column into the
    /// struct (proving the epoch-specific and selector-aware reads decode
    /// identically) — including the `harness_version` epoch key.
    #[test]
    fn map_stored_agg_round_trips_all_columns() {
        let t: StoredAggTuple = (
            "qwen3-coder:30b".to_string(),
            Some("blitz".to_string()),
            "v3".to_string(),
            Some("Q4_K_M".to_string()),
            Some(true),
            Some(8192),
            Some(0.7),
            Some(0.9),
            0.5,
            4,
            2,
            0.25,
            false,
        );
        let a = map_stored_agg(t);
        assert_eq!(a.model, "qwen3-coder:30b");
        assert_eq!(a.harness_version, "v3");
        assert_eq!(a.n_samples, 4);
        assert_eq!(a.passes, 2);
        assert_eq!(a.pass_rate, 0.5);
    }

    /// The epoch-marker upsert is idempotent: an INSERT keyed on the `epoch`
    /// PRIMARY KEY with `ON CONFLICT (epoch) DO UPDATE`, so recording `'v3'`
    /// twice never duplicates the audit row, and the first-seen
    /// `became_current_at` is preserved (only `note` is refreshed).
    #[test]
    fn upsert_epoch_marker_sql_is_idempotent_upsert() {
        assert!(UPSERT_EPOCH_MARKER_SQL.contains("INSERT INTO intake_epoch_marker (epoch, note)"));
        assert!(UPSERT_EPOCH_MARKER_SQL.contains("ON CONFLICT (epoch) DO UPDATE"));
        // became_current_at is NOT in the conflict SET — the original stamp stays.
        assert!(!UPSERT_EPOCH_MARKER_SQL.contains("became_current_at ="));
        // note is refreshed only when a new one is supplied (COALESCE keeps prior).
        assert!(UPSERT_EPOCH_MARKER_SQL.contains("note = COALESCE(EXCLUDED.note"));
    }
}
