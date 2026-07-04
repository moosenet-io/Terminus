//! Library driver for the v2 code-suite gap audit (HFIX-06): find which case
//! ids a model is MISSING valid data for, under a given `mem_config` — the
//! companion to `coder_case` (which reruns an explicit case-id list). Prints a
//! ready-to-paste case-id list so a gap can be closed without re-running a
//! model's entire suite.
//!
//! Extracted from `bin/intake_coder_gaps.rs` during the MINT Phase 1 build so
//! `mint gaps` and the standalone binary share one code path.
//!
//! Depends on the `code_profile_runs.case_id` column (added by the mem-config
//! sprint's schema migration): rows written BEFORE that column existed have
//! `case_id = NULL` and are indistinguishable from "never run" here — this
//! tool can only see gaps in runs made after the column was added. That is
//! reported explicitly (never silently treated as "no gap"), so a stale-data
//! false negative is visible rather than hidden.
//!
//! ## Runtime configuration (env-sourced by default; `run`'s params, when
//! `Some`, override the corresponding env var)
//! - `INTAKE_CASE_MODEL`  — REQUIRED absent an override. The model id to audit.
//! - `SWEEP_MEM_CONFIG`   — optional. The mem_config to scope the audit to.
//! - `INTAKE_CODE_LANGS`  — optional narrowing.
//! - `INTAKE_DATABASE_URL` / `INTAKE_CORPUS_V2_DIR` — shared with the other
//!   intake binaries.

use std::collections::HashSet;

use crate::intake::assistant::schema;
use crate::intake::{corpus_v2_dir, filter_by_language, read_manifest_v2};

pub fn langs_from_env() -> Vec<String> {
    std::env::var("INTAKE_CODE_LANGS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn mem_config_from_env() -> Option<String> {
    std::env::var("SWEEP_MEM_CONFIG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run the gap audit end to end for `model_id` (a caller-resolved value — env
/// read + any CLI-flag override already applied). `mem_config`/`langs` follow
/// the same convention as `coder_sweep::run`.
pub async fn run(
    model_id: Option<&str>,
    mem_config: Option<&str>,
    langs: &[String],
) -> std::process::ExitCode {
    let model_id = match model_id {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => {
            eprintln!("gap audit did not start: model id is required (INTAKE_CASE_MODEL or --model)");
            return std::process::ExitCode::FAILURE;
        }
    };

    let dir = corpus_v2_dir();
    let all = match read_manifest_v2(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gap audit did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let scoped = filter_by_language(&all, langs);
    if scoped.is_empty() {
        eprintln!("gap audit did not start: no corpus cases match the requested languages");
        return std::process::ExitCode::FAILURE;
    }
    let all_ids: HashSet<String> = scoped.iter().map(|c| c.id.clone()).collect();

    let pool = match schema::get_pool().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gap audit did not start: DB pool connect failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Distinct case ids that have at least one VALID row (error IS NULL) —
    // a hard-failed-then-fixed case still counts as a gap until a clean row
    // exists for it. `case_id IS NOT NULL` rows only (see module doc).
    let rows: Vec<(Option<String>,)> = match mem_config {
        Some(mc) => sqlx::query_as(
            "SELECT DISTINCT r.case_id FROM code_profile_runs r \
             JOIN model_profiles p ON p.id = r.profile_id \
             WHERE p.model_name = $1 AND r.mem_config = $2 AND r.error IS NULL \
             AND r.case_id IS NOT NULL",
        )
        .bind(&model_id)
        .bind(mc)
        .fetch_all(&pool)
        .await
        .unwrap_or_default(),
        None => sqlx::query_as(
            "SELECT DISTINCT r.case_id FROM code_profile_runs r \
             JOIN model_profiles p ON p.id = r.profile_id \
             WHERE p.model_name = $1 AND r.mem_config IS NULL AND r.error IS NULL \
             AND r.case_id IS NOT NULL",
        )
        .bind(&model_id)
        .fetch_all(&pool)
        .await
        .unwrap_or_default(),
    };
    let have_valid: HashSet<String> = rows.into_iter().filter_map(|(c,)| c).collect();

    // Rows for this model/mem_config that predate the case_id column — these
    // are exactly the ones this tool CANNOT see into, so a "no gap" result
    // could be a false negative for them. Report the count, never hide it.
    let legacy_count: i64 = match mem_config {
        Some(mc) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_profile_runs r \
             JOIN model_profiles p ON p.id = r.profile_id \
             WHERE p.model_name = $1 AND r.mem_config = $2 AND r.case_id IS NULL",
        )
        .bind(&model_id)
        .bind(mc)
        .fetch_one(&pool)
        .await
        .unwrap_or(0),
        None => sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_profile_runs r \
             JOIN model_profiles p ON p.id = r.profile_id \
             WHERE p.model_name = $1 AND r.mem_config IS NULL AND r.case_id IS NULL",
        )
        .bind(&model_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0),
    };

    let mut missing: Vec<&String> = all_ids.iter().filter(|id| !have_valid.contains(*id)).collect();
    missing.sort();

    println!(
        "model={model_id} mem_config={} corpus_cases={} valid={} missing={}",
        mem_config.unwrap_or("(NULL/carveout)"),
        all_ids.len(),
        have_valid.len(),
        missing.len(),
    );
    if legacy_count > 0 {
        println!(
            "NOTE: {legacy_count} pre-existing row(s) for this model/mem_config have no case_id \
             (written before the case_id column existed) — this audit cannot see into them, so \
             a case reported here as 'missing' might already have valid data under one of those \
             legacy rows. Treat this report as a lower bound, not exact, until those rows age out."
        );
    }
    if missing.is_empty() {
        println!("no gap — every corpus case in scope has at least one valid row.");
    } else {
        println!("INTAKE_CASE_IDS={}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","));
    }
    std::process::ExitCode::SUCCESS
}

/// Env-only convenience used by the legacy binary: resolve `INTAKE_CASE_MODEL`
/// and hand it to [`run`], preserving the exact required-var error text the
/// binary always printed.
pub async fn run_from_env() -> std::process::ExitCode {
    let model_id = std::env::var("INTAKE_CASE_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if model_id.is_none() {
        eprintln!("gap audit did not start: INTAKE_CASE_MODEL is required");
        return std::process::ExitCode::FAILURE;
    }
    let mem_config = mem_config_from_env();
    let langs = langs_from_env();
    run(model_id.as_deref(), mem_config.as_deref(), &langs).await
}
