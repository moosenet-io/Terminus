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
//! - MINT2-01 measurement-factor knobs (all optional; recorded on every `'v3'`
//!   case row so pass-rate can be analyzed against the knob that was set — see
//!   [`intake::code_v2::MeasurementFactors::from_env`]): `SWEEP_QUANT`,
//!   `SWEEP_REASONING_ENABLED`, `SWEEP_CONTEXT_WINDOW`, `SWEEP_TEMPERATURE`,
//!   `SWEEP_TOP_P`. Unset ⇒ the factor records as "unset"/`unknown`, never guessed.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::ToolError;
use crate::intake::assistant::acquire::{Nomination, Nominations};
use crate::intake::assistant::schema;
use crate::intake::assistant::BackendTag;
use crate::intake::checkpoint::FileCheckpoint;
use crate::intake::gpu_authority;
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
/// sweep's checkpoint (both now share [`crate::intake::checkpoint::FileCheckpoint`]),
/// with a distinct filename so the two sweeps never clobber each other's
/// checkpoints.
type CodeCheckpoint = FileCheckpoint<CodeCheckpointKey>;

/// Resolve the checkpoint path from `INTAKE_STAGING_DIR`. `Err` (not a guess)
/// when unset — the resume ledger MUST live on the reliable dir. A free
/// function (not an inherent `open()`) since `CodeCheckpoint` is now a type
/// alias over the shared generic checkpoint, which knows nothing about
/// `config.rs`'s env-resolution conventions.
fn open_code_checkpoint() -> Result<CodeCheckpoint, ToolError> {
    let dir = config::intake_staging_dir().ok_or_else(|| {
        ToolError::NotConfigured(
            "INTAKE_STAGING_DIR not set — the resume checkpoint needs the reliable NAS staging dir"
                .into(),
        )
    })?;
    let path = format!("{}/coder-sweep-checkpoint.json", dir.trim_end_matches('/'));
    Ok(CodeCheckpoint::at(path))
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

// ===========================================================================
// Suite driver — makes the coder fleet loop driver-agnostic/testable
// ===========================================================================
//
// Mirrors `assistant::runner::SuiteDriver` (item 2, MINT Phase 2): the coder
// loop was a monolithic function that called `infer::model_available`,
// `intake::create_profile_row`, and `intake::run_code_suite_v2` directly,
// making it untestable without a network/DB/GPU — the same gap the
// assistant runner already closed with its `SuiteDriver` trait. This brings
// the coder side up to the SAME abstraction level: `run_one_backend`/
// `run_fleet` depend only on the trait object, the live impl wires the real
// calls, and tests inject a scripted fake (see `mod tests`'s `ScriptDriver`,
// which intentionally mirrors `assistant::runner::tests::ScriptDriver`'s
// shape). Unlike the assistant driver, this trait does NOT take a
// `backend_override` parameter on each method — the coder loop sets the P5
// override ONCE per `(model, backend)` pass (via an RAII guard spanning
// `model_available` + `create_profile_row` + `run_suite`), not once per
// individual call, so the override stays exactly where it already was: in
// the orchestrator (`run_one_backend`), not the driver.
#[async_trait::async_trait]
pub trait CoderSuiteDriver: Send + Sync {
    /// HFIX-05 pre-flight: is `model_id` present in the CURRENTLY-overridden
    /// backend's Ollama registry? Assumes the caller has already applied the
    /// backend override for the pass being checked.
    async fn model_available(&self, model_id: &str) -> bool;

    /// Create a fresh profile row scoping one `(model, backend)` pass's rows.
    async fn create_profile_row(&self, model_id: &str) -> Result<uuid::Uuid, ToolError>;

    /// MINT2-02: record a terminal `failure_class = "non_viable_vram"` row for a
    /// `(model, backend)` cell skipped pre-flight as over-VRAM — so the cell
    /// EXISTS in the data (score 0) instead of silently vanishing. Injected
    /// through the trait (rather than called inline) so the fleet loop's skip
    /// path stays unit-testable without a live DB. The live impl writes the row
    /// via `code_v2::record_non_viable_vram_row`; test fakes just record the call.
    async fn record_non_viable(
        &self,
        model_id: &str,
        backend_tag: &str,
        reason: &str,
        mem_config: Option<&str>,
    ) -> Result<(), ToolError>;

    /// Run the v2 code suite for `model_id` under `profile_id`, against the
    /// currently-overridden backend. Persists one `code_profile_runs` row per
    /// case internally (the canonical write path, unchanged by this trait).
    ///
    /// `gpu_lock` (S86 max-lock-hold safety valve): the SAME [`GpuLock`] this
    /// pass is already held under. The live implementation threads it into
    /// the per-case loop (`code_v2::run_code_suite_v2`) so a pass that runs
    /// unusually long (e.g. a model with a high transport-error retry rate)
    /// yields the lock mid-pass instead of holding it for the pass's entire,
    /// potentially hours-long duration. Test fakes ignore it — the fairness
    /// timing itself is tested in `gpu_authority.rs` and via `ScriptGpuLock`
    /// below, not by re-simulating the real per-case loop.
    #[allow(clippy::too_many_arguments)]
    async fn run_suite(
        &self,
        model_id: &str,
        langs: &[String],
        profile_id: uuid::Uuid,
        case_limit: Option<usize>,
        backend_tag: &str,
        mem_config: Option<&str>,
        gpu_lock: &dyn GpuLock,
    ) -> Result<intake::CodeV2Outcome, ToolError>;
}

/// Live driver: the real inference/DB calls, unchanged from what
/// `run_one_backend` called directly before this refactor.
pub struct LiveCoderDriver;

#[async_trait::async_trait]
impl CoderSuiteDriver for LiveCoderDriver {
    async fn model_available(&self, model_id: &str) -> bool {
        infer::model_available(model_id).await
    }

    async fn create_profile_row(&self, model_id: &str) -> Result<uuid::Uuid, ToolError> {
        intake::create_profile_row(model_id).await
    }

    async fn record_non_viable(
        &self,
        model_id: &str,
        backend_tag: &str,
        reason: &str,
        mem_config: Option<&str>,
    ) -> Result<(), ToolError> {
        intake::code_v2::record_non_viable_vram_row(model_id, backend_tag, reason, mem_config).await
    }

    async fn run_suite(
        &self,
        model_id: &str,
        langs: &[String],
        profile_id: uuid::Uuid,
        case_limit: Option<usize>,
        backend_tag: &str,
        mem_config: Option<&str>,
        gpu_lock: &dyn GpuLock,
    ) -> Result<intake::CodeV2Outcome, ToolError> {
        intake::run_code_suite_v2(
            model_id,
            langs,
            profile_id,
            case_limit,
            Some(backend_tag),
            mem_config,
            Some(gpu_lock),
        )
        .await
    }
}

// ===========================================================================
// S86: GPU-lock fairness — acquire/release PER (model, backend) pass
// ===========================================================================
//
// Root cause + full design: see `gpu_authority.rs`'s "Fairness" module
// section. Short version: this sweep used to acquire ONE
// `gpu_authority::ExclusiveGuard` at the top of `run()` and hold it for the
// ENTIRE multi-day fleet run (by design — a 3-day autonomous run genuinely
// needs the GPU for most of that time). That starved `intake_assistant_sweep`
// completely for as long as this sweep ran (confirmed in production: 2+ days,
// zero `assistant_dimension_score` rows). Giving ONLY the assistant sweep a
// bounded backoff (HFIX-09) fixed ITS crash-loop but not the starvation — the
// lock was still held continuously by whichever sweep got there first. This
// sweep now acquires/releases the SAME exclusive lock per (model, backend)
// pass — the natural unit `run_fleet`'s loop already iterates over — using
// the identical bounded-backoff reacquire and the identical
// `gpu_authority::INTER_UNIT_RELEASE_PAUSE` the assistant sweep uses (same
// module, not a duplicate), so real alternation happens in practice, not
// just in theory.

/// Per-unit-of-work GPU lock, injected into [`run_fleet`] so the
/// acquire-per-pass / release-per-pass fairness policy (including the S86
/// max-lock-hold safety valve, `check_max_hold`) is unit-testable without a
/// real lock file or GPU — mirrors [`CoderSuiteDriver`]'s existing
/// trait-injection pattern (see `mod tests`'s `ScriptGpuLock`). The trait
/// itself, and its live implementation, now live in `gpu_authority.rs`
/// (formerly duplicated near-identically here and in
/// `assistant/runner.rs` — consolidated so both sweeps share one
/// implementation of the safety-valve semantics rather than two copies that
/// could drift apart).
pub use gpu_authority::{GpuLock, LiveGpuLock};

/// Env var overriding the max total time a bounded GPU-(re)acquire wait will
/// spend retrying a refused acquire before giving up (whole seconds;
/// non-positive/unparsable falls back to the default). Distinct from the
/// assistant sweep's own env var (`INTAKE_ASSISTANT_ACQUIRE_MAX_WAIT_SECS`)
/// so each binary's operator-facing knob is unambiguous about which sweep it
/// tunes.
pub const CODER_ACQUIRE_MAX_WAIT_ENV: &str = "INTAKE_CODER_ACQUIRE_MAX_WAIT_SECS";

/// Default max wait: 4 hours — same reasoning as the assistant sweep's
/// default (see `assistant::runner::ACQUIRE_MAX_WAIT_DEFAULT_SECS`): a
/// per-pass reacquire should only ever wait for the OTHER sweep's single
/// current model to finish (minutes, not hours); this is the safety net for
/// a genuinely wedged GPU, not the expected wait.
const CODER_ACQUIRE_MAX_WAIT_DEFAULT_SECS: u64 = 4 * 60 * 60;

fn coder_acquire_max_wait() -> std::time::Duration {
    std::env::var(CODER_ACQUIRE_MAX_WAIT_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(CODER_ACQUIRE_MAX_WAIT_DEFAULT_SECS))
}

/// RAII: releases `lock` on drop, exactly once — so a checkpoint-write
/// failure (`?` early return out of `run_one_backend`) or a panic mid-pass
/// still releases the GPU rather than leaking it for the rest of this
/// process's life.
struct ReleaseOnDrop<'a> {
    lock: &'a dyn GpuLock,
}

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.lock.release();
    }
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
    driver: &dyn CoderSuiteDriver,
    gpu_lock: &dyn GpuLock,
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
        // MINT2-02: the cell EXISTS — record a non_viable_vram row instead of
        // silently continuing (defense-in-depth; the live fleet loop already
        // records + `continue`s at its own pre-skip check before ever reaching
        // here, so this fires only for a direct caller of `run_one_backend`).
        // Best-effort: a DB hiccup must not turn a clean skip into an error.
        if let Err(e) = driver
            .record_non_viable(&model_id, backend.as_str(), &reason, mem_config)
            .await
        {
            eprintln!(
                "coder sweep: could not record non_viable_vram row for model={model_id} \
                 backend={} (continuing): {e}",
                backend.as_str()
            );
        }
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
    if !driver.model_available(&model_id).await {
        let reason = format!(
            "model '{model_id}' not present in the resolved backend's Ollama registry (not pulled)"
        );
        return Ok(BackendReport {
            model_id,
            backend_tag: backend,
            outcome: BackendOutcome::Skipped(reason),
        });
    }

    // ── S86 hardening: reconcile orphaned incomplete rows from a prior
    //    crashed/killed attempt at this EXACT (model, backend, mem_config)
    //    BEFORE starting a fresh attempt. Without this, INCR-01's per-case
    //    Phase-1 inserts left behind by a mid-suite kill (before the
    //    per-model checkpoint mark below) just accumulate forever — every
    //    restart claims a brand-new `profile_id` and re-runs every case from
    //    scratch, never touching the old partial rows. Best-effort: a
    //    failure here is logged and does NOT block the fresh attempt (worst
    //    case, one more generation of orphaned rows sits alongside the new
    //    one — no worse than before this hardening, and never blocks a run
    //    over a cleanup hiccup). ──
    match schema::get_pool().await {
        Ok(pool) => {
            match intake::storage::delete_unfinalized_code_runs_v2(
                &pool,
                &model_id,
                backend.as_str(),
                mem_config,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => eprintln!(
                    "coder sweep: reconciled {n} orphaned unfinalized code_profile_runs row(s) \
                     for model={model_id} backend={} mem_config={} (prior crashed attempt)",
                    backend.as_str(),
                    mem_config.unwrap_or("(NULL)"),
                ),
                Err(e) => eprintln!(
                    "coder sweep: orphaned-row cleanup failed for model={model_id} backend={} \
                     (continuing anyway): {e}",
                    backend.as_str(),
                ),
            }
        }
        Err(e) => eprintln!(
            "coder sweep: orphaned-row cleanup skipped (pool connect failed, continuing anyway): {e}"
        ),
    }

    // ── per-model flow, mirroring ModelIntake: profile row → suite → persist.
    //    A fresh profile row scopes this (model, backend) pass's code rows. ──
    let profile_id = match driver.create_profile_row(&model_id).await {
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
    let outcome = match driver
        .run_suite(
            &model_id,
            langs,
            profile_id,
            case_limit,
            backend.as_str(),
            mem_config,
            gpu_lock,
        )
        .await
    {
        Ok(res) => {
            // Durable checkpoint AFTER rows are persisted — resume-safe ordering.
            checkpoint
                .mark(&key)
                .map_err(|e| ToolError::Execution(format!("mark checkpoint: {e}")))?;
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
///
/// S86: `gpu_lock` is acquired fresh for EACH `(model, backend)` pass and
/// released at the end of that same pass — never held for the whole fleet
/// run — so `intake_assistant_sweep` gets real turns instead of starving for
/// this sweep's entire multi-day duration. See `gpu_authority.rs`'s
/// "Fairness" section for the full design.
#[allow(clippy::too_many_arguments)]
async fn run_fleet(
    fleet: &Nominations,
    langs: &[String],
    case_limit: Option<usize>,
    checkpoint: &CodeCheckpoint,
    mem_config: Option<&str>,
    driver: &dyn CoderSuiteDriver,
    gpu_lock: &dyn GpuLock,
) -> Result<Vec<BackendReport>, ToolError> {
    let done = checkpoint.done();
    let mut reports = Vec::new();
    // Whether the PREVIOUS pass actually took (and released) the GPU lock —
    // only then must we pause before the next real acquire attempt (see
    // `gpu_authority::INTER_UNIT_RELEASE_PAUSE`'s doc for why the pause must
    // follow a genuine release, not just any loop iteration).
    let mut did_prior_unit = false;

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

            let model_id = nom.id.clone();
            let key = CodeCheckpointKey::new(&model_id, backend_tag);

            // ── cheap, GPU-free pre-check: a resumed or pre-flight VRAM
            //    skip never touches the model at all (mirrors the checks
            //    `run_one_backend` repeats internally as defense-in-depth) —
            //    so free work costs zero lock churn and zero pause. ──
            if done.contains(&key) {
                reports.push(BackendReport { model_id, backend_tag, outcome: BackendOutcome::Resumed });
                continue;
            }
            if let Some(reason) = pre_skip_reason(nom, backend_tag) {
                // MINT2-02: kill survivorship bias — the over-VRAM cell EXISTS
                // as a row (failure_class="non_viable_vram", score 0) instead of
                // silently vanishing. Best-effort: a DB hiccup here must not
                // abort the fleet (worst case, this one skip isn't recorded — no
                // worse than the pre-MINT2-02 behavior of never recording it).
                if let Err(e) = driver
                    .record_non_viable(&model_id, backend_tag.as_str(), &reason, mem_config)
                    .await
                {
                    eprintln!(
                        "coder sweep: could not record non_viable_vram row for model={model_id} \
                         backend={} (continuing): {e}",
                        backend_tag.as_str()
                    );
                }
                reports.push(BackendReport { model_id, backend_tag, outcome: BackendOutcome::Skipped(reason) });
                continue;
            }

            // ── fairness: pause AFTER a real release, BEFORE the next real
            //    acquire, so the other sweep's poll loop gets a genuine
            //    window to notice the gap. ──
            if did_prior_unit {
                tokio::time::sleep(gpu_lock.release_pause()).await;
            }

            if let Err(e) = gpu_lock.acquire().await {
                eprintln!(
                    "coder sweep: could not reacquire the GPU for model={model_id} backend={} \
                     — skipping this pass this run (resumable next run): {e}",
                    backend_tag.as_str()
                );
                reports.push(BackendReport {
                    model_id,
                    backend_tag,
                    outcome: BackendOutcome::Skipped(format!("GPU reacquire failed: {e}")),
                });
                did_prior_unit = false; // never took the lock — no pause owed before the next try
                continue;
            }
            let _release_guard = ReleaseOnDrop { lock: gpu_lock };

            let report = run_one_backend(
                nom, backend_tag, override_str, langs, case_limit, checkpoint, &done, mem_config,
                driver, gpu_lock,
            )
            .await?;
            reports.push(report);
            did_prior_unit = true;
            // `_release_guard` drops here, at the end of this pass's scope —
            // AFTER `run_one_backend` has persisted rows and marked the
            // checkpoint internally, never mid-write.
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

/// GPU-authority holder label this suite acquires under (see
/// [`gpu_authority::ExclusiveGuard`]). `pub` so `mint`'s dispatcher can
/// pre-acquire under the IDENTICAL label before calling [`run`] (MINT Phase 2
/// item 7) — `ExclusiveGuard::acquire` treats a re-acquire by the SAME holder
/// as an idempotent no-op, but a DIFFERENT holder (even from the same
/// process) is treated as a live competing holder and rejected, so the two
/// acquisition points MUST agree on the exact label, not just both be
/// "some GPU guard for this subcommand".
pub const GPU_HOLDER: &str = "intake_coder_sweep";

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
    let checkpoint = match open_code_checkpoint() {
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

    // multi-point-score-tracking: corpus-coverage reconciliation. Read-only and
    // log-only — surfaces any language that has a toolchain check in
    // `code::required_toolchain` but ZERO corpus cases (so the gate exists but
    // is never exercised). Does NOT change scheduling and NEVER fails startup:
    // a missing/unparseable manifest just skips this diagnostic.
    warn_uncovered_toolchain_languages();

    // S86: exclusive GPU use is now claimed PER (model, backend) PASS inside
    // `run_fleet`'s loop (see `GpuLock`/`LiveGpuLock` above), NOT once here
    // for the whole multi-day run. HFIX-07's original one-shot, whole-run
    // guard was correct in spirit (never silently race another exclusive
    // holder — the exact failure mode that produced false "wedge" timeouts
    // earlier in this sweep) but, held for the ENTIRE run, it starved
    // `intake_assistant_sweep` completely for as long as this sweep ran
    // (confirmed in production: 2+ days straight, zero
    // `assistant_dimension_score` rows). `LiveGpuLock` still refuses to
    // silently race another holder — it waits, bounded, via the shared
    // `gpu_authority::acquire_with_backoff` — it just does so freshly per
    // pass instead of once for days.
    // MINT2-01: surface the run-global sampling/launch factors that will be
    // recorded on every `'v3'` case row (quant is model-specific — parsed per
    // model at case time — so it's omitted from this run-level banner).
    let factor_summary = intake::code_v2::MeasurementFactors::from_env("").sampling_summary();
    eprintln!(
        "coder sweep starting: {} models, langs={}, case_limit={:?}, mem_config={}, factors=[{}], checkpoint={}",
        fleet.nominations.len(),
        if langs.is_empty() { "all".into() } else { langs.join(",") },
        case_limit,
        mem_config.unwrap_or("(unset — rows land with mem_config=NULL)"),
        factor_summary,
        checkpoint.path(),
    );

    let driver = LiveCoderDriver;
    let gpu_lock = LiveGpuLock::new(GPU_HOLDER, coder_acquire_max_wait());
    match run_fleet(&fleet, langs, case_limit, &checkpoint, mem_config, &driver, &gpu_lock).await {
        Ok(reports) => {
            print_report(&reports);
            // MINT2-03: refresh the variance-aware aggregates (pass_rate +
            // n_samples + stddev per model×category×epoch×config-factors) from
            // the rows this run just persisted, so the catalog reads them
            // cheaply. Best-effort: a DB hiccup here — or an un-migrated DB
            // missing the `code_run_aggregates` table — must NOT turn a
            // successful sweep into a failure (the per-case rows are already
            // durably written; aggregates are trivially recomputable next run).
            match intake::aggregate::recompute_and_persist_current_epoch(&pool).await {
                Ok(n) => eprintln!(
                    "coder sweep: refreshed {n} variance-aware run aggregate cell(s) \
                     (epoch {})",
                    intake::aggregate::CURRENT_EPOCH
                ),
                Err(e) => eprintln!(
                    "coder sweep: could not refresh run aggregates (continuing — rows \
                     persisted, aggregates recompute next run): {e}"
                ),
            }
            // MINT2-05: idempotently record that the current epoch is/became the
            // active build-scenario partition, so the audit timeline (when 'v3'
            // became current) is answerable from the data. Best-effort — same
            // as the aggregate refresh: a DB hiccup or an un-migrated DB missing
            // the `intake_epoch_marker` table must NOT fail an otherwise-
            // successful sweep whose per-case rows are already durably written.
            match intake::storage::upsert_epoch_marker(
                &pool,
                intake::current_epoch(),
                Some("current build-scenario coder epoch (recorded by coder sweep)"),
            )
            .await
            {
                Ok(_) => eprintln!(
                    "coder sweep: epoch marker recorded/confirmed for epoch {}",
                    intake::current_epoch()
                ),
                Err(e) => eprintln!(
                    "coder sweep: could not record epoch marker (continuing — \
                     marker is audit-only, sweep rows persisted): {e}"
                ),
            }
            // MINT2-07: refresh the Model Fleet Catalog — the per-model coverage
            // registry an agent reads to know the fleet WITHOUT SQL — from the
            // rows this run (and every prior source) has persisted. Best-effort,
            // exactly like the aggregate refresh / epoch marker above: a DB
            // hiccup or an un-migrated DB missing the `model_fleet_catalog`
            // table(s) must NOT turn a successful sweep into a failure (the
            // catalog is fully re-derivable next run). The read side is what
            // MINT2-08's tool exposes.
            match intake::catalog::refresh_fleet_catalog(&pool).await {
                Ok(n) => eprintln!("coder sweep: refreshed fleet catalog ({n} model card(s))"),
                Err(e) => eprintln!(
                    "coder sweep: could not refresh fleet catalog (continuing — \
                     catalog is derived, recomputes next run): {e}"
                ),
            }
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

/// Read the v2 corpus manifest and `tracing::warn!` once per toolchain-checked
/// language that has zero corpus cases (multi-point-score-tracking corpus-
/// coverage reconciliation). Read-only/log-only: any manifest read/parse error
/// is itself warned and then swallowed — this diagnostic must never abort or
/// change the sweep. The pure set-difference is
/// [`crate::intake::code::toolchain_coverage_gaps`] (unit-tested there); this
/// wrapper only does the I/O (manifest read) and logging.
fn warn_uncovered_toolchain_languages() {
    let dir = match intake::code_v2::corpus_v2_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("corpus-coverage check skipped: {e}");
            return;
        }
    };
    let cases = match intake::code_v2::read_manifest_v2(&dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "corpus-coverage check skipped: could not read v2 manifest at {}: {e}",
                dir.display()
            );
            return;
        }
    };
    let corpus_languages: BTreeSet<String> =
        cases.iter().map(|c| c.language.to_lowercase()).collect();
    let toolchain_languages: BTreeSet<String> = intake::code::toolchain_checked_languages()
        .iter()
        .map(|s| s.to_string())
        .collect();
    for lang in intake::code::toolchain_coverage_gaps(&corpus_languages, &toolchain_languages) {
        tracing::warn!(
            "language {lang} has a toolchain check but 0 corpus cases — skipped, not tested"
        );
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

    // ---- CoderSuiteDriver: hermetic orchestration tests (item 2) ----
    // Mirrors `assistant::runner::tests::ScriptDriver`/`block` in shape, so the
    // coder loop's test-facing abstraction level matches the assistant side's.

    use std::sync::Mutex;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        // `enable_time()`: S86's `run_fleet` uses `tokio::time::sleep` for the
        // inter-unit release pause (real GpuLock impls use 90s;
        // NoopGpuLock/ScriptGpuLock return `Duration::ZERO`, but the sleep
        // call itself still needs a timer-enabled runtime to resolve at all).
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn tmp_checkpoint(name: &str) -> CodeCheckpoint {
        let path = format!(
            "{}/terminus-coder-sweep-test-{name}-{}.jsonl",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_file(&path);
        CodeCheckpoint::at(path)
    }

    /// Scriptable driver mirroring `assistant::runner::tests::ScriptDriver`:
    /// `model_available` returns a fixed answer; `run_suite` either succeeds
    /// with a canned outcome or fails with a canned reason, per model.
    struct ScriptDriver {
        available: BTreeSet<String>,
        suite_fail: BTreeSet<String>,
        profile_calls: Mutex<Vec<String>>,
        suite_calls: Mutex<Vec<(String, String)>>,
        /// MINT2-02: (model_id, backend_tag, reason) per recorded non_viable row.
        non_viable_calls: Mutex<Vec<(String, String, String)>>,
    }

    impl ScriptDriver {
        fn new() -> Self {
            ScriptDriver {
                available: BTreeSet::new(),
                suite_fail: BTreeSet::new(),
                profile_calls: Mutex::new(Vec::new()),
                suite_calls: Mutex::new(Vec::new()),
                non_viable_calls: Mutex::new(Vec::new()),
            }
        }

        fn available(mut self, model: &str) -> Self {
            self.available.insert(model.to_string());
            self
        }
    }

    #[async_trait::async_trait]
    impl CoderSuiteDriver for ScriptDriver {
        async fn model_available(&self, model_id: &str) -> bool {
            self.available.contains(model_id)
        }

        async fn create_profile_row(&self, model_id: &str) -> Result<uuid::Uuid, ToolError> {
            self.profile_calls.lock().unwrap().push(model_id.to_string());
            Ok(uuid::Uuid::nil())
        }

        async fn record_non_viable(
            &self,
            model_id: &str,
            backend_tag: &str,
            reason: &str,
            _mem_config: Option<&str>,
        ) -> Result<(), ToolError> {
            self.non_viable_calls.lock().unwrap().push((
                model_id.to_string(),
                backend_tag.to_string(),
                reason.to_string(),
            ));
            Ok(())
        }

        async fn run_suite(
            &self,
            model_id: &str,
            _langs: &[String],
            _profile_id: uuid::Uuid,
            _case_limit: Option<usize>,
            backend_tag: &str,
            _mem_config: Option<&str>,
            _gpu_lock: &dyn GpuLock,
        ) -> Result<intake::CodeV2Outcome, ToolError> {
            self.suite_calls
                .lock()
                .unwrap()
                .push((model_id.to_string(), backend_tag.to_string()));
            if self.suite_fail.contains(model_id) {
                return Err(ToolError::Execution("model hung mid-suite".to_string()));
            }
            Ok(intake::CodeV2Outcome {
                cases_run: 3,
                avg_first_pass: 4.0,
                avg_effective: 4.5,
                approved: vec!["rust:blitz".to_string()],
                scored: 3,
                errors: 0,
                ..Default::default()
            })
        }
    }

    // ── S86: GpuLock fakes (mock the lock, never a real lock file / GPU) ──

    /// Grants immediately, zero pause — for tests exercising `run_fleet`'s
    /// resume/skip/driver orchestration, where the GPU-lock fairness
    /// mechanism itself is not under test (that has its own dedicated tests
    /// below, plus `gpu_authority.rs`'s alternation simulation, and the
    /// safety-valve-specific tests further down this module).
    struct NoopGpuLock;
    #[async_trait::async_trait]
    impl GpuLock for NoopGpuLock {
        async fn acquire(&self) -> Result<(), String> {
            Ok(())
        }
        fn release(&self) {}
        fn release_pause(&self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
        async fn check_max_hold(&self) -> Result<bool, String> {
            Ok(false)
        }
    }

    /// Scriptable GpuLock: counts acquire/release/pause calls, and can be
    /// told to refuse specific (1-based) acquire attempts — so tests can
    /// assert `run_fleet` acquires exactly ONCE PER (model, backend) PASS
    /// (not once for the whole run), pauses only BETWEEN passes that
    /// actually touched the GPU (never before the first, never for a
    /// resumed/pre-skipped pass), and treats a reacquire failure as a
    /// per-pass skip that does not abort the rest of the fleet.
    #[derive(Default)]
    struct ScriptGpuLock {
        fail_on_call: BTreeSet<u32>,
        call_no: Mutex<u32>,
        acquire_calls: Mutex<u32>,
        release_calls: Mutex<u32>,
        pause_calls: Mutex<u32>,
    }

    impl ScriptGpuLock {
        fn failing_on(call_no: u32) -> Self {
            let mut s = ScriptGpuLock::default();
            s.fail_on_call.insert(call_no);
            s
        }
    }

    #[async_trait::async_trait]
    impl GpuLock for ScriptGpuLock {
        async fn acquire(&self) -> Result<(), String> {
            let n = {
                let mut c = self.call_no.lock().unwrap();
                *c += 1;
                *c
            };
            if self.fail_on_call.contains(&n) {
                return Err(format!(
                    "GPU is held exclusively by 'other-sweep' (scripted refusal on attempt {n})"
                ));
            }
            *self.acquire_calls.lock().unwrap() += 1;
            Ok(())
        }
        fn release(&self) {
            *self.release_calls.lock().unwrap() += 1;
        }
        fn release_pause(&self) -> std::time::Duration {
            *self.pause_calls.lock().unwrap() += 1;
            std::time::Duration::ZERO
        }
        async fn check_max_hold(&self) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[test]
    fn driver_profiles_an_available_model_on_both_backends() {
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"qwen3-coder:30b","size_b":30,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        )
        .unwrap();
        let driver = ScriptDriver::new().available("qwen3-coder:30b");
        let checkpoint = tmp_checkpoint("profiles-both");

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &NoopGpuLock)).unwrap();

        assert_eq!(reports.len(), 2);
        for r in &reports {
            assert!(matches!(r.outcome, BackendOutcome::Profiled { cases_run: 3, .. }));
        }
        // Both (model, backend) suite calls actually reached the driver.
        let calls = driver.suite_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|(_, b)| b == "gpu"));
        assert!(calls.iter().any(|(_, b)| b == "cpu"));
        // Both passes checkpointed (durable-after-persist).
        assert_eq!(checkpoint.done().len(), 2);
    }

    #[test]
    fn driver_unavailable_model_skips_with_reason_and_never_reaches_suite() {
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"ghost:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}]}"#,
        )
        .unwrap();
        let driver = ScriptDriver::new(); // nothing marked available
        let checkpoint = tmp_checkpoint("unavailable");

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &NoopGpuLock)).unwrap();

        assert_eq!(reports.len(), 1);
        match &reports[0].outcome {
            BackendOutcome::Skipped(reason) => assert!(reason.contains("not present")),
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert!(
            driver.suite_calls.lock().unwrap().is_empty(),
            "an unavailable model must never reach run_suite (HFIX-05)"
        );
        assert!(checkpoint.done().is_empty(), "a skip must never be checkpointed");
    }

    #[test]
    fn driver_suite_failure_is_a_recorded_skip_not_a_propagated_error() {
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"hangy:32b","size_b":32,"gfx1151_class":"experimental","acquisition":"ollama_pull","backends":["gpu"]}]}"#,
        )
        .unwrap();
        let mut driver = ScriptDriver::new().available("hangy:32b");
        driver.suite_fail.insert("hangy:32b".to_string());
        let checkpoint = tmp_checkpoint("suite-fail");

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &NoopGpuLock)).unwrap();

        assert_eq!(reports.len(), 1);
        match &reports[0].outcome {
            BackendOutcome::Skipped(reason) => assert!(reason.contains("code suite did not complete")),
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert!(checkpoint.done().is_empty());
    }

    #[test]
    fn over_vram_skip_records_a_non_viable_row_not_just_an_absence() {
        // MINT2-02 core behavior: a model skipped as over-VRAM must produce a
        // recorded non_viable row (the cell EXISTS), not vanish from the data.
        // 400B at ~0.6GB/B is over any realistic default ceiling, so the GPU
        // pass is pre-skipped in `run_fleet` before any lock/driver suite call.
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"huge:400b","size_b":400,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}]}"#,
        )
        .unwrap();
        let driver = ScriptDriver::new();
        let checkpoint = tmp_checkpoint("non-viable-row");

        let reports =
            block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &NoopGpuLock)).unwrap();

        assert_eq!(reports.len(), 1);
        match &reports[0].outcome {
            BackendOutcome::Skipped(reason) => assert!(reason.contains("VRAM")),
            other => panic!("expected Skipped, got {other:?}"),
        }
        // The cell EXISTS: exactly one non_viable row was recorded for this
        // (model, backend), carrying the over-VRAM reason.
        let calls = driver.non_viable_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "huge:400b");
        assert_eq!(calls[0].1, "gpu");
        assert!(calls[0].2.to_lowercase().contains("vram"));
        // A skip is still never checkpointed (resumable), and never reaches the suite.
        assert!(checkpoint.done().is_empty());
        assert!(driver.suite_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn driver_resume_skips_already_checkpointed_backend_without_touching_driver() {
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"m:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        )
        .unwrap();
        let checkpoint = tmp_checkpoint("resume");
        // Pre-seed as if the gpu pass already completed on a prior run.
        checkpoint
            .mark(&CodeCheckpointKey::new("m:8b", BackendTag::Gpu))
            .unwrap();
        let driver = ScriptDriver::new().available("m:8b");

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &NoopGpuLock)).unwrap();

        assert_eq!(reports.len(), 2);
        let gpu = reports.iter().find(|r| r.backend_tag == BackendTag::Gpu).unwrap();
        assert!(matches!(gpu.outcome, BackendOutcome::Resumed));
        let cpu = reports.iter().find(|r| r.backend_tag == BackendTag::Cpu).unwrap();
        assert!(matches!(cpu.outcome, BackendOutcome::Profiled { .. }));

        // The driver was asked about ONLY the cpu pass — resume never touches
        // an already-checkpointed backend's model_available/run_suite calls.
        let suite_calls = driver.suite_calls.lock().unwrap();
        assert_eq!(suite_calls.len(), 1);
        assert_eq!(suite_calls[0], ("m:8b".to_string(), "cpu".to_string()));
    }

    // ── S86: GPU lock acquired/released PER (model, backend) pass ──

    #[test]
    fn gpu_lock_acquired_and_released_once_per_backend_pass_not_once_for_the_whole_run() {
        // One model with BOTH backends must take the GPU lock twice — once
        // per (model, backend) pass — not once for the whole fleet run.
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"qwen3-coder:30b","size_b":30,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        )
        .unwrap();
        let driver = ScriptDriver::new().available("qwen3-coder:30b");
        let checkpoint = tmp_checkpoint("gpu-lock-both-backends");
        let gpu = ScriptGpuLock::default();

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &gpu)).unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 2, "one acquire per backend pass");
        assert_eq!(*gpu.release_calls.lock().unwrap(), 2, "one release per backend pass");
        // Pause happens exactly once — BETWEEN the two passes, never before the first.
        assert_eq!(*gpu.pause_calls.lock().unwrap(), 1);
    }

    #[test]
    fn gpu_lock_never_touched_for_resumed_or_pre_skipped_passes() {
        // gpu backend already checkpointed (resume) AND the model is tagged
        // gpu-only via backend_strategy for the skip case below — use two
        // separate nominations to isolate "resumed" from "pre-skip" cleanly.
        let fleet = Nominations::from_json(
            r#"{"nominations":[
                {"id":"resumed:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]},
                {"id":"huge:400b","size_b":400,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}
            ]}"#,
        )
        .unwrap();
        let checkpoint = tmp_checkpoint("gpu-lock-skip-cases");
        checkpoint
            .mark(&CodeCheckpointKey::new("resumed:8b", BackendTag::Gpu))
            .unwrap();
        let driver = ScriptDriver::new(); // nothing marked available — irrelevant, never reached
        let gpu = ScriptGpuLock::default();

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &gpu)).unwrap();

        assert_eq!(reports.len(), 2);
        assert!(matches!(reports[0].outcome, BackendOutcome::Resumed));
        assert!(matches!(reports[1].outcome, BackendOutcome::Skipped(_))); // over-VRAM

        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 0, "resume/pre-skip must never touch the GPU lock");
        assert_eq!(*gpu.release_calls.lock().unwrap(), 0);
        assert_eq!(*gpu.pause_calls.lock().unwrap(), 0);
    }

    #[test]
    fn gpu_lock_reacquire_failure_is_a_recorded_skip_and_the_fleet_continues() {
        // First (model, backend) pass's acquire is scripted to be refused (as
        // if the assistant sweep's own unit is mid-run). This must NOT abort
        // the multi-day fleet run — it must be recorded as this pass's
        // Skipped outcome and the fleet must continue to the next pass,
        // which succeeds normally.
        let fleet = Nominations::from_json(
            r#"{"nominations":[{"id":"stuck:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu","cpu"]}]}"#,
        )
        .unwrap();
        let driver = ScriptDriver::new().available("stuck:8b");
        let checkpoint = tmp_checkpoint("gpu-lock-reacquire-fail");
        let gpu = ScriptGpuLock::failing_on(1); // refuse the very first acquire (the gpu pass)

        let reports = block(run_fleet(&fleet, &[], None, &checkpoint, None, &driver, &gpu)).unwrap();

        assert_eq!(reports.len(), 2);
        let gpu_report = reports.iter().find(|r| r.backend_tag == BackendTag::Gpu).unwrap();
        match &gpu_report.outcome {
            BackendOutcome::Skipped(reason) => assert!(
                reason.contains("GPU reacquire failed"),
                "got: {reason}"
            ),
            other => panic!("expected Skipped, got {other:?}"),
        }
        // The NEXT pass (cpu) must still run normally — no fatal abort of the sweep.
        let cpu_report = reports.iter().find(|r| r.backend_tag == BackendTag::Cpu).unwrap();
        assert!(matches!(cpu_report.outcome, BackendOutcome::Profiled { .. }));

        // The driver was only ever asked about the surviving (cpu) pass.
        let suite_calls = driver.suite_calls.lock().unwrap();
        assert_eq!(suite_calls.len(), 1);
        assert_eq!(suite_calls[0], ("stuck:8b".to_string(), "cpu".to_string()));

        // A refused pass was never checkpointed (durability: only real progress is marked).
        assert!(!checkpoint.done().contains(&CodeCheckpointKey::new("stuck:8b", BackendTag::Gpu)));
    }

    // ── S86 max-lock-hold safety valve: wiring tests ────────────────────────
    //
    // The safety valve's own trigger-timing/reacquire-correctness logic is
    // fully unit-tested (with a fake clock) in `gpu_authority.rs`. These
    // tests cover the OTHER half: does `run_one_backend`/`CoderSuiteDriver`
    // correctly plumb the SAME `GpuLock` a pass acquired into its `run_suite`
    // call, so the real per-case loop (`code_v2.rs`) can call
    // `check_max_hold()` after each case — and does a mid-pass valve firing
    // (or failing) behave correctly from the pass's point of view: it must
    // NOT restart/abandon the in-progress model (a real per-case loop simply
    // keeps iterating its existing case list — nothing here re-derives or
    // truncates it), and a reacquire that ultimately fails must surface as
    // this pass's ordinary Skipped-with-reason outcome (resumable next run),
    // exactly like any other GPU reacquire failure already does.

    /// Scriptable driver whose `run_suite` simulates a per-case loop by
    /// calling `gpu_lock.check_max_hold()` `calls_per_suite` times in a row —
    /// standing in for `code_v2.rs`'s real Phase-1 loop calling it once per
    /// case — WITHOUT touching a real corpus/DB/network. Proves the exact
    /// property that matters here: every scripted "case" call happens, in
    /// order, and a `check_max_hold` failure aborts the REST of this SAME
    /// `run_suite` call (mirroring `code_v2.rs`'s `?` propagation) rather than
    /// silently continuing without the lock.
    struct MidUnitScriptDriver {
        calls_per_suite: u32,
        hold_check_calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl CoderSuiteDriver for MidUnitScriptDriver {
        async fn model_available(&self, _model_id: &str) -> bool {
            true
        }

        async fn create_profile_row(&self, _model_id: &str) -> Result<uuid::Uuid, ToolError> {
            Ok(uuid::Uuid::nil())
        }

        async fn record_non_viable(
            &self,
            _model_id: &str,
            _backend_tag: &str,
            _reason: &str,
            _mem_config: Option<&str>,
        ) -> Result<(), ToolError> {
            Ok(())
        }

        async fn run_suite(
            &self,
            _model_id: &str,
            _langs: &[String],
            _profile_id: uuid::Uuid,
            _case_limit: Option<usize>,
            _backend_tag: &str,
            _mem_config: Option<&str>,
            gpu_lock: &dyn GpuLock,
        ) -> Result<intake::CodeV2Outcome, ToolError> {
            // Mirrors `code_v2.rs`'s Phase-1 loop: for each "case", do the
            // (here: no-op) work, then check the safety valve, propagating a
            // failure immediately — never skipping ahead, never repeating a
            // "case" already done.
            for _ in 0..self.calls_per_suite {
                *self.hold_check_calls.lock().unwrap() += 1;
                gpu_lock.check_max_hold().await.map_err(|e| {
                    ToolError::Execution(format!("mid-unit GPU safety-valve reacquire failed: {e}"))
                })?;
            }
            Ok(intake::CodeV2Outcome {
                cases_run: self.calls_per_suite as usize,
                avg_first_pass: 4.0,
                avg_effective: 4.0,
                scored: self.calls_per_suite as usize,
                ..Default::default()
            })
        }
    }

    /// A `GpuLock` whose `check_max_hold` fires (returns `Ok(true)`) on a
    /// scripted call number, `Ok(false)` otherwise, or a scripted `Err` — so
    /// tests can simulate "the safety valve triggers partway through this
    /// pass's case loop" without any real lock file, GPU, or sleeping.
    #[derive(Default)]
    struct ScriptMaxHoldLock {
        fire_on_call: Option<u32>,
        fail_on_call: Option<u32>,
        call_no: Mutex<u32>,
        acquire_calls: Mutex<u32>,
        release_calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl GpuLock for ScriptMaxHoldLock {
        async fn acquire(&self) -> Result<(), String> {
            *self.acquire_calls.lock().unwrap() += 1;
            Ok(())
        }
        fn release(&self) {
            *self.release_calls.lock().unwrap() += 1;
        }
        fn release_pause(&self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
        async fn check_max_hold(&self) -> Result<bool, String> {
            let n = {
                let mut c = self.call_no.lock().unwrap();
                *c += 1;
                *c
            };
            if self.fail_on_call == Some(n) {
                return Err("gave up waiting for the GPU after 4h (cap 4h): scripted failure".into());
            }
            if self.fire_on_call == Some(n) {
                *self.release_calls.lock().unwrap() += 1;
                *self.acquire_calls.lock().unwrap() += 1;
                return Ok(true);
            }
            Ok(false)
        }
    }

    #[test]
    fn well_behaved_pass_never_triggers_the_safety_valve() {
        // 5 fast "cases", never crossing the max-hold threshold — the
        // overwhelmingly common case must see zero valve activity.
        let driver = MidUnitScriptDriver { calls_per_suite: 5, hold_check_calls: Mutex::new(0) };
        let lock = ScriptMaxHoldLock::default(); // never fires, never fails

        let outcome = block(driver.run_suite(
            "m:8b", &[], uuid::Uuid::nil(), None, "gpu", None, &lock,
        ))
        .unwrap();

        assert_eq!(outcome.cases_run, 5);
        assert_eq!(*driver.hold_check_calls.lock().unwrap(), 5, "checked after EVERY case");
        assert_eq!(*lock.acquire_calls.lock().unwrap(), 0, "well-behaved: never released/reacquired");
        assert_eq!(*lock.release_calls.lock().unwrap(), 0);
    }

    #[test]
    fn slow_unreliable_pass_triggers_the_valve_once_and_resumes_the_same_suite() {
        // The valve fires on the 3rd of 6 scripted "cases" — the pass must
        // still process ALL 6, in order, neither skipping ahead past the
        // firing case nor re-running an earlier one (there is no case-index
        // bookkeeping at all in this loop shape — it simply keeps iterating,
        // which this asserts by checking the exact call count).
        let driver = MidUnitScriptDriver { calls_per_suite: 6, hold_check_calls: Mutex::new(0) };
        let lock = ScriptMaxHoldLock { fire_on_call: Some(3), ..Default::default() };

        let outcome = block(driver.run_suite(
            "unreliable:32b", &[], uuid::Uuid::nil(), None, "gpu", None, &lock,
        ))
        .unwrap();

        assert_eq!(outcome.cases_run, 6, "the suite must run to completion after the valve fires");
        assert_eq!(*driver.hold_check_calls.lock().unwrap(), 6, "no case skipped or duplicated");
        assert_eq!(*lock.release_calls.lock().unwrap(), 1, "fired exactly once");
        assert_eq!(*lock.acquire_calls.lock().unwrap(), 1, "reacquired exactly once, to resume");
    }

    #[test]
    fn a_reacquire_that_ultimately_fails_aborts_this_pass_only_as_a_recorded_skip() {
        // If the mid-unit reacquire itself exhausts ITS bounded wait (the
        // same shape any other GPU reacquire failure takes), this pass must
        // abort — but ONLY this pass: the caller (`run_one_backend`) already
        // treats any `run_suite` `Err` as a Skipped-with-reason outcome, safe
        // to resume next run. It must never silently continue running
        // inference without the lock.
        let driver = MidUnitScriptDriver { calls_per_suite: 6, hold_check_calls: Mutex::new(0) };
        let lock = ScriptMaxHoldLock { fail_on_call: Some(2), ..Default::default() };

        let err = block(driver.run_suite(
            "wedged:70b", &[], uuid::Uuid::nil(), None, "gpu", None, &lock,
        ))
        .unwrap_err();

        assert!(
            format!("{err}").contains("mid-unit GPU safety-valve reacquire failed"),
            "got: {err}"
        );
        // Aborted immediately after the 2nd case's check — cases 3-6 never ran.
        assert_eq!(*driver.hold_check_calls.lock().unwrap(), 2);
    }
}
