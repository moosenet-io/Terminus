//! Library driver for the S86 coder fleet sweep (extracted from
//! `bin/intake_coder_sweep.rs` during the MINT Phase 1 build so `mint sweep
//! coder` and the standalone `intake_coder_sweep` binary share ONE code path).
//!
//! Everything that was previously inline in that binary's `main()` lives here
//! now: fleet loading, the resume checkpoint, the per-(model, backend) runner,
//! the skip-with-reason decision, and the end-of-run report. The binary (and
//! the `mint` CLI) are both thin wrappers that read/override configuration and
//! call [`run`].
//!
//! ## Runtime configuration (all env-sourced by default; `run`'s params, when
//! `Some`, override the corresponding env var — see each param's caller)
//! - `INTAKE_DATABASE_URL` (or `DATABASE_URL`) — the intake Postgres.
//! - `INTAKE_STAGING_DIR`  — the reliable NAS staging dir (fleet file + checkpoint).
//! - `MODEL_REGISTRY_PATH` — chord model→backend registry.
//! - `OLLAMA_URL` (or `_BASE_URL` / `_CPU_URL`) — the unified inference base.
//! - `INTAKE_CORPUS_V2_DIR` — the v2 code corpus.
//! - `INTAKE_CODE_LANGS` — optional comma list to narrow languages (overridden
//!   by `run`'s `langs` param when non-empty).
//! - `INTAKE_CODE_CASE_LIMIT` — optional per-model case cap (overridden by
//!   `run`'s `case_limit` param when `Some`).
//! - `SWEEP_MEM_CONFIG` — memory-model tag (overridden by `run`'s `mem_config`
//!   param when `Some`).
//! - `INTAKE_VRAM_CEILING_GB` — over-ceiling models skip the GPU pass cleanly.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::ToolError;
use crate::intake::assistant::acquire::{Nomination, Nominations};
use crate::intake::assistant::schema;
use crate::intake::assistant::BackendTag;
use crate::intake::gpu_authority::{self, GpuMode};
use crate::intake::{self, infer};

// ===========================================================================
// Env-sourced config (pub — shared by the binary AND the `mint` CLI, which
// override these with clap flags when present)
// ===========================================================================

/// Resolve the suite languages from `INTAKE_CODE_LANGS` (comma-separated). An
/// unset or empty value means "all languages in the corpus" (empty vec — the
/// `run_code_suite_v2` convention). Pure over its input so it is unit-testable.
pub fn parse_langs(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
    }
}

/// Read the language narrowing from the environment.
pub fn langs_from_env() -> Vec<String> {
    parse_langs(std::env::var("INTAKE_CODE_LANGS").ok().as_deref())
}

/// Normalize a raw case-limit value using the long-standing "0 means
/// unset/no limit" convention: `Some(0)` collapses to `None` (no cap),
/// same as the value never having been provided at all. Shared by the
/// `INTAKE_CODE_CASE_LIMIT` env parser AND the `mint sweep coder
/// --case-limit` CLI flag so the two input paths can't drift apart —
/// `--case-limit 0` must behave identically to leaving the env var unset,
/// not literally cap every model's run at zero cases.
pub fn normalize_case_limit(raw: Option<usize>) -> Option<usize> {
    raw.filter(|n| *n > 0)
}

/// Optional per-model case cap (smoke/debug), from `INTAKE_CODE_CASE_LIMIT`.
pub fn case_limit_from_env() -> Option<usize> {
    normalize_case_limit(
        std::env::var("INTAKE_CODE_CASE_LIMIT")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok()),
    )
}

/// Which memory-model configuration THIS sweep run is executing against
/// (e.g. `"dynamic_gtt"` or `"carveout"`), from `SWEEP_MEM_CONFIG`
/// (mem-config-tagging sprint). `None` when unset — every row written by
/// this run is then persisted with `mem_config = NULL`, same as any row
/// written before this column existed. Trimmed and treated as unset when
/// empty, matching `langs_from_env`'s tolerance for a blank env var.
pub fn mem_config_from_env() -> Option<String> {
    std::env::var("SWEEP_MEM_CONFIG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ===========================================================================
// Fleet (nominations) loading — reuses the assistant Nominations shape
// ===========================================================================

/// Resolve the fleet file path inside `INTAKE_STAGING_DIR`, preferring a
/// code-specific `coder-nominations.json` and falling back to the shared
/// `nominations.json` (so a host already staged for the assistant sweep works
/// unchanged). `None` ⇒ `INTAKE_STAGING_DIR` is unset.
fn nominations_path() -> Option<String> {
    let dir = config::intake_staging_dir()?;
    let dir = dir.trim_end_matches('/');
    let coder = format!("{dir}/coder-nominations.json");
    if std::path::Path::new(&coder).exists() {
        Some(coder)
    } else {
        Some(format!("{dir}/nominations.json"))
    }
}

/// Load the fleet from the resolved nominations path. Reuses the assistant
/// `Nominations` parser (identical JSON shape: `{"nominations":[{id, size_b,
/// gfx1151_class, acquisition, backends?, …}]}`).
fn load_fleet() -> Result<Nominations, ToolError> {
    let path = nominations_path().ok_or_else(|| {
        ToolError::NotConfigured(
            "INTAKE_STAGING_DIR not set — cannot locate the coder fleet nominations".into(),
        )
    })?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ToolError::NotConfigured(format!("cannot read nominations at {path}: {e}")))?;
    Nominations::from_json(&raw).map_err(ToolError::NotConfigured)
}

// ===========================================================================
// Resume checkpoint — keyed on (model, backend); atomic per code-suite run
// ===========================================================================

/// One completed unit of fleet work: a `(model, backend)` whose
/// `code_profile_runs` rows are durably persisted. Mirrors the assistant
/// runner's `CheckpointKey`, minus the dimension (the code suite is atomic per
/// backend pass).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct CodeCheckpointKey {
    model_id: String,
    backend_tag: String,
}

impl CodeCheckpointKey {
    fn new(model_id: &str, backend: BackendTag) -> Self {
        CodeCheckpointKey {
            model_id: model_id.to_string(),
            backend_tag: backend.as_str().to_string(),
        }
    }
}

/// File-backed resume ledger on the reliable NAS staging dir. Append-on-mark,
/// JSON-lines, survives a reboot — the same durability pattern as the assistant
/// sweep's `FileCheckpoint`, with a distinct filename so the two sweeps never
/// clobber each other's checkpoints.
struct CodeCheckpoint {
    path: String,
}

impl CodeCheckpoint {
    /// Resolve the checkpoint path from `INTAKE_STAGING_DIR`. `Err` (not a
    /// guess) when unset — the resume ledger MUST live on the reliable dir.
    fn open() -> Result<Self, ToolError> {
        let dir = config::intake_staging_dir().ok_or_else(|| {
            ToolError::NotConfigured(
                "INTAKE_STAGING_DIR not set — the resume checkpoint needs the reliable NAS staging dir"
                    .into(),
            )
        })?;
        let path = format!("{}/coder-sweep-checkpoint.json", dir.trim_end_matches('/'));
        Ok(CodeCheckpoint { path })
    }

    /// All `(model, backend)` units already completed (empty on a fresh run).
    fn done(&self) -> BTreeSet<CodeCheckpointKey> {
        std::fs::read_to_string(&self.path)
            .map(|s| {
                s.lines()
                    .filter_map(|l| serde_json::from_str::<CodeCheckpointKey>(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record one completed `(model, backend)`. Durable BEFORE the next unit
    /// starts (called only AFTER the suite's rows are persisted), so a crash
    /// can never leave a checkpoint that claims work the DB doesn't have.
    fn mark(&self, key: &CodeCheckpointKey) -> Result<(), ToolError> {
        let line = serde_json::to_string(key)
            .map_err(|e| ToolError::Execution(format!("serialize checkpoint key: {e}")))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| ToolError::Execution(format!("open checkpoint {}: {e}", self.path)))?;
        writeln!(f, "{line}")
            .map_err(|e| ToolError::Execution(format!("append checkpoint: {e}")))?;
        Ok(())
    }
}

// ===========================================================================
// Per-(model, backend) outcome reporting
// ===========================================================================

/// Why a `(model, backend)` pass did not produce a fresh score (so a 0-row run
/// is diagnosable). `None` ⇒ the suite ran and persisted rows this run.
#[derive(Debug, Clone, PartialEq)]
enum BackendOutcome {
    /// Already in the checkpoint — resumed, not re-run.
    Resumed,
    /// Ran this invocation; carries the suite summary.
    Profiled {
        cases_run: usize,
        scored: usize,
        errors: usize,
        avg_first_pass: f64,
        avg_effective: f64,
        approved: Vec<String>,
    },
    /// Skipped cleanly with a reason (over-VRAM, hang, unavailable, …) — the
    /// sweep continued.
    Skipped(String),
}

/// One line of the end-of-run report: a `(model, backend)` and its outcome.
struct BackendReport {
    model_id: String,
    backend_tag: BackendTag,
    outcome: BackendOutcome,
}

// ===========================================================================
// Skip decision (pure) — VRAM ceiling on the GPU pass
// ===========================================================================

/// Decide whether `(nomination, backend)` should be skipped BEFORE inference,
/// returning the skip reason. Pure so it is unit-testable. A GPU pass for a
/// model whose footprint exceeds the host VRAM ceiling is skipped (the big-model
/// wedge guard); the CPU pass is always attempted (it has no VRAM ceiling).
fn pre_skip_reason(nom: &Nomination, backend: BackendTag) -> Option<String> {
    if backend == BackendTag::Gpu && nom.exceeds_vram() {
        return Some(format!(
            "over VRAM ceiling on GPU ({:.0}GB footprint > {:.0}GB ceiling)",
            nom.vram_footprint_gb(),
            crate::intake::assistant::acquire::vram_ceiling_gb()
        ));
    }
    None
}

/// Run one `(model, backend)` code-suite pass under the P5 backend override,
/// honoring the resume checkpoint. NEVER returns `Err` for a per-model failure —
/// a hang/unavailable/OOM becomes a `Skipped` outcome so the fleet continues.
/// `Err` is reserved for a checkpoint-write failure (a durability bug we must
/// surface, not swallow).
#[allow(clippy::too_many_arguments)]
async fn run_one_backend(
    nom: &Nomination,
    backend: BackendTag,
    override_str: &str,
    langs: &[String],
    case_limit: Option<usize>,
    checkpoint: &CodeCheckpoint,
    done: &BTreeSet<CodeCheckpointKey>,
    mem_config: Option<&str>,
) -> Result<BackendReport, ToolError> {
    let model_id = nom.id.clone();
    let key = CodeCheckpointKey::new(&model_id, backend);

    // ── resume: already complete → skip without touching the model ──
    if done.contains(&key) {
        return Ok(BackendReport {
            model_id,
            backend_tag: backend,
            outcome: BackendOutcome::Resumed,
        });
    }

    // ── pre-flight skip (over-VRAM on GPU) ──
    if let Some(reason) = pre_skip_reason(nom, backend) {
        return Ok(BackendReport {
            model_id,
            backend_tag: backend,
            outcome: BackendOutcome::Skipped(reason),
        });
    }

    // ── force the backend for this pass (process-global; intake runs are
    //    sequential), cleared on every exit path via RAII ──
    struct ClearOverride;
    impl Drop for ClearOverride {
        fn drop(&mut self) {
            infer::set_backend_override(None);
        }
    }
    infer::set_backend_override(Some(override_str.to_string()));
    let _clear = ClearOverride;

    // ── HFIX-05 pre-flight: skip cleanly (one reason) instead of persisting
    //    a "model not found" 404 PER CASE (up to 200 wasted rows per model,
    //    the dominant failure mode found auditing the dynamic_gtt run) ──
    if !infer::model_available(&model_id).await {
        let reason = format!(
            "model '{model_id}' not present in the resolved backend's Ollama registry (not pulled)"
        );
        return Ok(BackendReport {
            model_id,
            backend_tag: backend,
            outcome: BackendOutcome::Skipped(reason),
        });
    }

    // ── per-model flow, mirroring ModelIntake: profile row → suite → persist.
    //    A fresh profile row scopes this (model, backend) pass's code rows. ──
    let profile_id = match intake::create_profile_row(&model_id).await {
        Ok(id) => id,
        Err(e) => {
            return Ok(BackendReport {
                model_id,
                backend_tag: backend,
                outcome: BackendOutcome::Skipped(format!("profile row create failed: {e}")),
            });
        }
    };

    // The suite persists one code_profile_runs row per case internally. Any
    // hang/unavailable/OOM surfaces as Err here → recorded as a skip-with-reason
    // (NOT propagated), so one wedged model never stalls the fleet.
    // `backend.as_str()` yields the short 'gpu'/'cpu' tag (matching the
    // assistant-side `backend_tag` convention), NOT `override_str` (which is
    // the longer serving-backend name like "ollama"/"ollama-cpu").
    let outcome = match intake::run_code_suite_v2(
        &model_id,
        langs,
        profile_id,
        case_limit,
        Some(backend.as_str()),
        mem_config,
    )
    .await
    {
        Ok(res) => {
            // Durable checkpoint AFTER rows are persisted — resume-safe ordering.
            checkpoint.mark(&key)?;
            BackendOutcome::Profiled {
                cases_run: res.cases_run,
                scored: res.scored,
                errors: res.errors,
                avg_first_pass: res.avg_first_pass,
                avg_effective: res.avg_effective,
                approved: res.approved,
            }
        }
        Err(e) => BackendOutcome::Skipped(format!("code suite did not complete: {e}")),
    };

    Ok(BackendReport {
        model_id,
        backend_tag: backend,
        outcome,
    })
}

/// Drive the whole fleet: for each nomination, for each backend in its strategy,
/// run (or resume) one code-suite pass. Sequential — the backend override is
/// process-global and inference is single-VRAM, exactly like the assistant sweep.
async fn run_fleet(
    fleet: &Nominations,
    langs: &[String],
    case_limit: Option<usize>,
    checkpoint: &CodeCheckpoint,
    mem_config: Option<&str>,
) -> Result<Vec<BackendReport>, ToolError> {
    let done = checkpoint.done();
    let mut reports = Vec::new();
    for nom in &fleet.nominations {
        for (backend_tag, override_str) in nom.backend_strategy() {
            // S86 / gfx1151: serve GPU passes via ollama-rocm (the always-on
            // `ollama` backend, :11434), NOT llama-server (`llama-gpu`), which
            // wedges on MoE models on this Vulkan stack (S84: MiniMax/Ornith;
            // S86: ornith-1.0). ollama-rocm serves dense AND MoE cleanly (proven
            // on qwen3-coder). The CPU pass uses the genuine-CPU `ollama-cpu`.
            let override_str = match (backend_tag, override_str) {
                (BackendTag::Gpu, "llama-gpu") => "ollama",
                (BackendTag::Cpu, "ollama") => "ollama-cpu",
                (_, other) => other,
            };
            let report = run_one_backend(
                nom, backend_tag, override_str, langs, case_limit, checkpoint, &done, mem_config,
            )
            .await?;
            reports.push(report);
        }
    }
    Ok(reports)
}

// ===========================================================================
// Reporting
// ===========================================================================

/// Print the end-of-run per-(model, backend) detail so a run with no score rows
/// is diagnosable (which model skipped + why). Mirrors the assistant sweep's
/// end-of-run dump.
fn print_report(reports: &[BackendReport]) {
    let profiled = reports
        .iter()
        .filter(|r| matches!(r.outcome, BackendOutcome::Profiled { .. }))
        .count();
    let resumed = reports
        .iter()
        .filter(|r| matches!(r.outcome, BackendOutcome::Resumed))
        .count();
    let skipped = reports
        .iter()
        .filter(|r| matches!(r.outcome, BackendOutcome::Skipped(_)))
        .count();
    eprintln!(
        "coder sweep complete: {profiled} backend-passes profiled, {resumed} resumed, \
         {skipped} skipped (rows persisted to the intake DB)"
    );
    for r in reports {
        match &r.outcome {
            BackendOutcome::Resumed => {
                eprintln!(
                    "MODEL {} backend={} RESUMED (already checkpointed)",
                    r.model_id, r.backend_tag
                );
            }
            BackendOutcome::Skipped(reason) => {
                eprintln!(
                    "MODEL {} backend={} SKIPPED: {reason}",
                    r.model_id, r.backend_tag
                );
            }
            BackendOutcome::Profiled {
                cases_run,
                scored,
                errors,
                avg_first_pass,
                avg_effective,
                approved,
            } => {
                eprintln!(
                    "MODEL {} backend={} PROFILED cases={cases_run} scored={scored} errors={errors} \
                     avg_first_pass={avg_first_pass:.2} avg_effective={avg_effective:.2} approved=[{}]",
                    r.model_id,
                    r.backend_tag,
                    approved.join(", ")
                );
            }
        }
    }
}

// ===========================================================================
// Entry point — shared by `bin/intake_coder_sweep.rs` and `mint sweep coder`
// ===========================================================================

/// Run the whole coder fleet sweep end to end: load the fleet, open the
/// resume checkpoint, migrate the shared schema, claim exclusive GPU use, run
/// every `(model, backend)` pass, and print the end-of-run report.
///
/// `langs`/`case_limit`/`mem_config` are the CALLER's already-resolved config
/// (env read + any CLI-flag override already applied) — this function does no
/// env reading of its own, so both the legacy binary (pure env) and `mint`
/// (env with optional flag overrides) share this one path.
pub async fn run(
    langs: &[String],
    case_limit: Option<usize>,
    mem_config: Option<&str>,
) -> std::process::ExitCode {
    let fleet = match load_fleet() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("coder sweep did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let checkpoint = match CodeCheckpoint::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("coder sweep did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Schema-dependency ordering (NOT accidental -- flagged in review): the
    // `backend_tag` column on the externally-managed `code_profile_runs` table
    // (storage.rs: "tables already exist ... DO NOT create them here") is added
    // ONLY by `assistant::schema::migrate()`. The assistant-side entry points
    // (runner.rs::run(), reporting.rs) already call it before any DB work; this
    // path is a second, independent entry point into the SAME shared DB and
    // must not assume the assistant sweep ran first (a fresh DB, or a host
    // where the coder sweep is the first thing ever run, would otherwise fail
    // every `insert_code_run_v2` with "column backend_tag does not exist" --
    // silently swallowed by `run_one_backend`'s `?` into a skip-with-reason, so
    // the sweep "succeeds" while persisting zero rows). migrate() is idempotent
    // and cheap, so calling it here unconditionally is safe on every run.
    let pool = match schema::get_pool().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("coder sweep did not start: schema pool connect failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = schema::migrate(&pool).await {
        eprintln!("coder sweep did not start: schema migrate failed: {e}");
        return std::process::ExitCode::FAILURE;
    }

    // HFIX-07: proactively claim exclusive GPU use BEFORE running a single
    // case — stops competing production services and brings Ollama's own
    // runner config to a single-resident-model state, idempotently (a
    // resumed run that's already exclusive touches nothing). Refuses to
    // start rather than silently racing another exclusive holder for the
    // GPU (the exact failure mode that produced false "wedge" timeouts
    // earlier in this sweep — see the gpu_authority module doc).
    let _gpu_guard = match gpu_authority::ExclusiveGuard::acquire(GpuMode::Exclusive, "intake_coder_sweep") {
        Ok(g) => g,
        Err(e) => {
            eprintln!("coder sweep did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    eprintln!(
        "coder sweep starting: {} models, langs={}, case_limit={:?}, mem_config={}, checkpoint={}",
        fleet.nominations.len(),
        if langs.is_empty() { "all".into() } else { langs.join(",") },
        case_limit,
        mem_config.unwrap_or("(unset — rows land with mem_config=NULL)"),
        checkpoint.path,
    );

    match run_fleet(&fleet, langs, case_limit, &checkpoint, mem_config).await {
        Ok(reports) => {
            print_report(&reports);
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            // Only a durability (checkpoint-write) failure reaches here; a
            // per-model failure is a recorded skip, not an error.
            eprintln!("coder sweep aborted on a durability error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

// ===========================================================================
// Unit / smoke tests — the module's PURE helpers. The full fleet run is
// integration (needs Postgres + corpus + live inference), not a unit test.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    // ---- language parsing ----

    #[test]
    fn parse_langs_unset_means_all() {
        assert!(parse_langs(None).is_empty());
    }

    #[test]
    fn parse_langs_empty_means_all() {
        assert!(parse_langs(Some("")).is_empty());
        assert!(parse_langs(Some("   ,  , ")).is_empty());
    }

    #[test]
    fn parse_langs_splits_trims_lowercases() {
        assert_eq!(
            parse_langs(Some("Rust, Python ,TS")),
            vec!["rust".to_string(), "python".to_string(), "ts".to_string()]
        );
    }

    // ---- checkpoint skip logic ----

    #[test]
    fn checkpoint_key_distinguishes_backends() {
        let gpu = CodeCheckpointKey::new("qwen3:8b", BackendTag::Gpu);
        let cpu = CodeCheckpointKey::new("qwen3:8b", BackendTag::Cpu);
        assert_ne!(gpu, cpu);
        assert_eq!(gpu.backend_tag, "gpu");
        assert_eq!(cpu.backend_tag, "cpu");
    }

    #[test]
    fn checkpoint_done_set_drives_resume_skip() {
        // The exact skip predicate `run_one_backend` uses: a (model, backend)
        // present in `done` is resumed (skipped), absent is run.
        let mut done = BTreeSet::new();
        done.insert(CodeCheckpointKey::new("m:8b", BackendTag::Gpu));
        assert!(done.contains(&CodeCheckpointKey::new("m:8b", BackendTag::Gpu)));
        assert!(!done.contains(&CodeCheckpointKey::new("m:8b", BackendTag::Cpu)));
        assert!(!done.contains(&CodeCheckpointKey::new("other:8b", BackendTag::Gpu)));
    }

    #[test]
    fn checkpoint_key_roundtrips_through_jsonlines() {
        // The file ledger is JSON-lines; a written key must parse back identically
        // (this is what makes a reboot resume rather than restart).
        let key = CodeCheckpointKey::new("mixtral:8x7b", BackendTag::Cpu);
        let line = serde_json::to_string(&key).unwrap();
        let back: CodeCheckpointKey = serde_json::from_str(&line).unwrap();
        assert_eq!(key, back);
    }

    #[test]
    fn case_limit_parse_rejects_zero_and_garbage() {
        // Mirrors case_limit_from_env's parse-then-normalize (no env access
        // in the test).
        let parse = |s: &str| normalize_case_limit(s.trim().parse::<usize>().ok());
        assert_eq!(parse("5"), Some(5));
        assert_eq!(parse("0"), None);
        assert_eq!(parse("abc"), None);
    }

    #[test]
    fn normalize_case_limit_treats_zero_as_unset() {
        // The shared "0 means no limit" convention both case_limit_from_env
        // (env-var path) and `mint sweep coder --case-limit` (CLI path)
        // delegate to, so the two input paths can't drift apart.
        assert_eq!(normalize_case_limit(Some(0)), None);
        assert_eq!(normalize_case_limit(Some(5)), Some(5));
        assert_eq!(normalize_case_limit(None), None);
    }

    // ---- mem_config env threading (mem-config-tagging) ----

    #[test]
    fn mem_config_from_env_reads_and_trims_set_value() {
        std::env::set_var("SWEEP_MEM_CONFIG", "  dynamic_gtt  ");
        assert_eq!(mem_config_from_env(), Some("dynamic_gtt".to_string()));
        std::env::remove_var("SWEEP_MEM_CONFIG");
    }

    #[test]
    fn mem_config_from_env_none_when_unset_or_blank() {
        std::env::remove_var("SWEEP_MEM_CONFIG");
        assert_eq!(mem_config_from_env(), None);

        std::env::set_var("SWEEP_MEM_CONFIG", "   ");
        assert_eq!(
            mem_config_from_env(),
            None,
            "a blank value must be treated as unset, not as an empty-string mem_config"
        );
        std::env::remove_var("SWEEP_MEM_CONFIG");
    }

    // ---- pre-flight VRAM skip (pure) ----

    fn nom(json: &str) -> Nomination {
        let wrapped = format!(r#"{{"nominations":[{json}]}}"#);
        Nominations::from_json(&wrapped)
            .unwrap()
            .nominations
            .pop()
            .unwrap()
    }

    #[test]
    fn small_model_runs_on_both_backends() {
        let n = nom(r#"{"id":"qwen3:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}"#);
        assert!(pre_skip_reason(&n, BackendTag::Gpu).is_none());
        assert!(pre_skip_reason(&n, BackendTag::Cpu).is_none());
    }

    #[test]
    fn oversized_model_skips_gpu_but_not_cpu() {
        // 218B at ~0.6GB/B = ~131GB footprint — over any realistic ceiling, so
        // the GPU pass is skipped with a reason while CPU is still attempted.
        std::env::set_var("INTAKE_VRAM_CEILING_GB", "96");
        let n = nom(r#"{"id":"command-a-plus:218b","size_b":218,"gfx1151_class":"experimental","acquisition":"hf_fetch","hf_repo":"cohere/command-a-plus"}"#);
        let gpu = pre_skip_reason(&n, BackendTag::Gpu);
        assert!(gpu.is_some(), "oversized model must skip GPU");
        assert!(gpu.unwrap().contains("VRAM"));
        assert!(
            pre_skip_reason(&n, BackendTag::Cpu).is_none(),
            "CPU pass has no VRAM ceiling"
        );
        std::env::remove_var("INTAKE_VRAM_CEILING_GB");
    }

    // ---- fleet shape reuse (assistant Nominations parser) ----

    #[test]
    fn fleet_parses_and_yields_backend_strategy() {
        let fleet = Nominations::from_json(
            r#"{"nominations":[
                {"id":"qwen3-coder:30b","size_b":30,"gfx1151_class":"confirmed","acquisition":"ollama_pull"},
                {"id":"cpu-only:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["cpu"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(fleet.nominations.len(), 2);
        // Default (no explicit backends) → both passes, GPU first.
        let s0 = fleet.nominations[0].backend_strategy();
        assert_eq!(s0.len(), 2);
        assert_eq!(s0[0].0, BackendTag::Gpu);
        // Explicit ["cpu"] → CPU-only.
        let s1 = fleet.nominations[1].backend_strategy();
        assert_eq!(s1, vec![(BackendTag::Cpu, "ollama")]);
    }
}
