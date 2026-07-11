//! Model intake profiling framework (S83 MINT-01).
//!
//! Three terminus tools that profile any fleet model and store results in the
//! shared Postgres (same DB as nexus / reminders):
//!   - `model_intake`         — run profiling suites (context implemented; code
//!                              and agent suites are MINT-02/03, stubbed).
//!   - `model_intake_status`  — return the stored operational profile.
//!   - `model_intake_compare` — comparison table across models for one metric.
//!
//! The context suite embeds real repo files as filler (the tool runs on the
//! sweep-harness host with no repo checkout), plants three recall facts at 25/50/75% depth, runs
//! the model through Ollama's `/api/generate`, and measures throughput, TTFT,
//! recall, and VRAM per graduated context tier. A derived operational profile
//! (safe/absolute context, degradation point, recommended timeouts) is computed
//! and stored after the run.
//!
//! VRAM policy: single hot model. If the target is already hot (the gpt-oss:20b
//! smoke case) no load/unload happens; otherwise the prior hot model is restored
//! after the run.

mod agent;
pub mod aggregate;
pub mod assistant;
pub mod breakfix;
pub mod checkpoint;
pub mod chord_pull;
mod code;
mod code_v2;
pub mod coder_case;
pub mod coder_gaps;
pub mod coder_sweep;
mod context;
pub mod gpu_authority;
pub mod infer;
pub mod lifecycle;
pub mod newcats;
mod runner;
pub mod serving;
mod storage;
pub mod supervisor;
mod timeouts;
pub mod vuln_scan;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Install a global `tracing` subscriber so `tracing::{info,warn,error}!` calls
/// throughout the intake sweeps actually go somewhere.
///
/// Every intake binary (`intake_coder_sweep`, `intake_assistant_sweep`,
/// `mint`, ...) previously called into library code that logs exclusively via
/// `tracing` macros, but no binary ever installed a subscriber — with no
/// subscriber registered, `tracing` events are silently dropped (this is not
/// a crash or an error, just a no-op), so the periodic "still waiting for the
/// GPU" progress logs added alongside the acquire-backoff fix never appeared
/// anywhere, even though the code emitting them was correct. Only the
/// top-level `eprintln!` of the final `Result` in each binary's `main` was
/// ever actually visible in the systemd-redirected log files.
///
/// Writes to stderr (already captured by each unit's `StandardError=append:
/// ...` systemd config), honors `RUST_LOG` for verbosity (defaulting to
/// `info` — the tier `tracing::warn!`/`tracing::error!` and the sweeps'
/// occasional `tracing::info!` calls need to actually surface without
/// requiring an operator to know to set `RUST_LOG=debug`), and is safe to
/// call more than once per process (`try_init` — a second call, e.g. from a
/// test harness that already installed its own, is a harmless no-op).
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

// Re-export pure pieces for cross-module/integration reference.
pub use runner::{FULL_TIERS, SMOKE_TIERS};
pub use code_v2::{
    corpus_v2_dir, filter_by_language, pass_at_k, pass_hat_k, read_manifest_v2,
    run_code_suite_v2, run_code_suite_v2_cases, samples_per_case, CaseV2, CodeV2Outcome,
};
pub use runner::create_profile_row;

// ---------------------------------------------------------------------------
// MINT2-05: harness-version EPOCHS — the single source of truth
// ---------------------------------------------------------------------------
//
// `harness_version` is the EPOCH partition key for the build-scenario coder
// rows (`code_profile_runs`) and their derived aggregates
// (`code_run_aggregates`). When a test evolves (Phase 1 changes what/how we
// measure), the epoch is bumped so old results become a distinct partition:
// they are never DELETED (provenance) but they also never blend into the
// current epoch's tuning numbers.
//
// This is the ONE place the current epoch string is stated. MINT2-03 originally
// held a local `CURRENT_EPOCH` in `aggregate.rs`; MINT2-05 promotes it here and
// `aggregate.rs` re-exports THIS const, so there is exactly one definition. A
// future `'v4'` is a one-line bump here and nowhere else.
//
// NOTE ON SCOPE: this epoch governs the CODER build-scenario lineage (`'v3'`).
// The assistant sweep (`assistant_profile_run.harness_version`) is a SEPARATE
// lineage with its own version string (`assistant::schema::HARNESS_VERSION`);
// its report scopes to that lineage, not to this `'v3'` — see
// `assistant/reporting.rs`.

/// The current build-scenario harness epoch. `harness_version` is the epoch
/// partition key: every current-epoch read/aggregate/catalog scopes to this
/// value by default so evolved-test results never blend with a prior harness's
/// rows, while legacy epochs (`'v1'`/`'v2'`) remain queryable via an explicit
/// [`EpochSelector`] but never pollute the current numbers. Bumping to a future
/// `'v4'` is a ONE-LINE change here — the single source of truth for the value.
pub const CURRENT_EPOCH: &str = "v3";

/// The current epoch string (helper form of [`CURRENT_EPOCH`]) — the value all
/// current-epoch reads default to.
pub fn current_epoch() -> &'static str {
    CURRENT_EPOCH
}

/// Which epoch(s) a current-epoch-partitioned read should cover.
///
/// Reads default to [`EpochSelector::Current`] (only the current epoch), so
/// legacy rows never pollute current-epoch numbers. Provenance queries pass
/// [`EpochSelector::Only`] for a specific prior epoch, or [`EpochSelector::All`]
/// to include every epoch. This ONLY changes which rows a read returns — legacy
/// rows are partitioned by filter and are NEVER deleted or mutated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EpochSelector {
    /// Only the current epoch ([`current_epoch`]) — the default for every
    /// current-epoch read.
    #[default]
    Current,
    /// Only this one explicit epoch (e.g. `"v1"` / `"v2"` for a legacy
    /// provenance query).
    Only(String),
    /// Every epoch — no `harness_version` filter (the all-provenance view).
    All,
}

impl EpochSelector {
    /// The concrete epoch string to filter on, or `None` for
    /// [`EpochSelector::All`] (no filter). [`EpochSelector::Current`] resolves to
    /// [`current_epoch`], so "current" is always the one central value.
    pub fn epoch(&self) -> Option<&str> {
        match self {
            EpochSelector::Current => Some(current_epoch()),
            EpochSelector::Only(e) => Some(e.as_str()),
            EpochSelector::All => None,
        }
    }
}

/// The SQL `WHERE`-fragment that scopes a `harness_version`-partitioned query to
/// `selector`, binding the epoch at positional `$idx`.
///
/// `Current` / `Only` yield `harness_version = $idx` (the caller binds the value
/// from [`EpochSelector::epoch`]); `All` yields `TRUE` (no bind consumed) so a
/// caller can always splice the fragment into its `WHERE` unconditionally. This
/// is the ONE place the epoch filter shape is defined. PURE — unit-tested.
pub fn epoch_where_fragment(selector: &EpochSelector, idx: usize) -> String {
    match selector.epoch() {
        Some(_) => format!("harness_version = ${idx}"),
        None => "TRUE".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Unified MINT harness (MINT2-04)
// ---------------------------------------------------------------------------
//
// The coder sweep (`intake_coder_sweep`) and the Lumina assistant sweep
// (`intake_assistant_sweep`) already share this `src/intake/` tree and the
// `lumina_intake` Postgres, but historically each binary drove its own
// orchestration and reporting, so there was no single "MINT harness" surface.
//
// `MintHarness` is that one surface. It owns the common run lifecycle —
// resolve config, confirm the shared intake DB is reachable via the ONE
// canonical resolver both sweep families use (`config::intake_database_url`),
// stamp a run-identity for log correlation, then dispatch to a per-kind
// sub-runner — and the two binaries become thin `MintHarness::run(RunKind::…)`
// entrypoints. This is a STRUCTURAL unification only: neither sweep's
// measurement changes (the coder cases and the assistant's seven dimensions
// run exactly as before, each under its existing sub-runner).

/// Which sweep family a [`MintHarness`] run drives. One process runs exactly
/// one kind; the two kinds are independent (running one never blocks the
/// other — they are separate binaries against the same shared DB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// The S83/MINT coder (code-generation) fleet sweep.
    Coder,
    /// The S84 Lumina assistant (seven-dimension) fleet sweep.
    Assistant,
}

impl RunKind {
    /// Stable snake_case label used in logs / the run banner. Not written to
    /// the DB by the harness itself (each sub-runner keeps its own row schema).
    pub fn as_str(&self) -> &'static str {
        match self {
            RunKind::Coder => "coder",
            RunKind::Assistant => "assistant",
        }
    }
}

/// A sweep sub-runner registered under the unified MINT harness. Each kind
/// (coder → [`runner::CoderSweepRunner`], assistant →
/// [`assistant::runner::AssistantSweepRunner`]) implements this thin trait;
/// [`MintHarness`] owns the common lifecycle and dispatches to the sub-runner.
///
/// The sub-runner drives its family's existing fleet driver unchanged and
/// returns a process exit code (both binaries ultimately produce one), so the
/// binaries carry no orchestration of their own.
#[async_trait]
pub trait SweepRunner: Send + Sync {
    /// The kind this sub-runner drives (used by the harness/tests to confirm
    /// dispatch selected the right family).
    fn kind(&self) -> RunKind;

    /// Run this sweep to completion, returning the process exit code. Any
    /// per-model failure is a recorded skip inside the underlying driver, not
    /// an error here — the exit code reflects only whether the sweep could
    /// start and finish its bookkeeping.
    async fn run(&self) -> std::process::ExitCode;
}

/// The single unified MINT harness surface. Both `intake_coder_sweep` and
/// `intake_assistant_sweep` route through here so "the MINT harness" is one
/// thing with one lifecycle, rather than two disconnected binaries.
pub struct MintHarness {
    kind: RunKind,
    /// Harness-level run-identity, stamped for log correlation across the
    /// lifecycle. Distinct from (and does not replace) the per-sweep run rows
    /// the sub-runners already write to their own tables.
    run_id: uuid::Uuid,
}

impl MintHarness {
    /// Build a harness for `kind` with a fresh run-identity.
    pub fn new(kind: RunKind) -> Self {
        MintHarness {
            kind,
            run_id: uuid::Uuid::new_v4(),
        }
    }

    /// The run kind this harness drives.
    pub fn kind(&self) -> RunKind {
        self.kind
    }

    /// This run's harness-level identity (log correlation).
    pub fn run_id(&self) -> uuid::Uuid {
        self.run_id
    }

    /// The per-kind sub-runner. Construction is DB-free (env-sourced config
    /// only), so it is safe to build in a unit test without touching Postgres.
    fn sub_runner(&self) -> Box<dyn SweepRunner> {
        match self.kind {
            RunKind::Coder => Box::new(runner::CoderSweepRunner::from_env()),
            RunKind::Assistant => Box::new(assistant::runner::AssistantSweepRunner::new()),
        }
    }

    /// Thin entrypoint both binaries call: build the harness for `kind` and run
    /// the shared lifecycle.
    pub async fn run(kind: RunKind) -> std::process::ExitCode {
        MintHarness::new(kind).execute().await
    }

    /// The common run lifecycle: acquire config → confirm the shared intake DB
    /// URL resolves via `config::intake_database_url()` (surfacing a clean
    /// per-kind NotConfigured instead of crashing deeper in a sub-runner) →
    /// dispatch to the sub-runner.
    async fn execute(&self) -> std::process::ExitCode {
        tracing::info!(
            "MINT harness starting: kind={}, run_id={}",
            self.kind.as_str(),
            self.run_id
        );

        // Both sweep families connect their pool through this ONE resolver
        // (`storage::get_pool` and `assistant::schema::get_pool` each delegate
        // to it). Checking it here first means an unconfigured host reports a
        // clear per-kind NotConfigured up front rather than failing partway
        // through the sub-runner's own connect.
        if crate::config::intake_database_url().is_none() {
            eprintln!(
                "MINT harness ({}) not configured: neither INTAKE_DATABASE_URL nor \
                 DATABASE_URL is set — the intake sweep requires a Postgres connection",
                self.kind.as_str()
            );
            return std::process::ExitCode::FAILURE;
        }

        self.sub_runner().run().await
    }
}

/// Parse the optional `suites` arg into a deduped, validated list. When absent
/// (or empty), default to the per-model purpose inference for `model_name`.
fn parse_suites(args: &Value, model_name: &str) -> Vec<String> {
    match args.get("suites").and_then(|v| v.as_array()) {
        Some(arr) => {
            let mut out: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            out.dedup();
            if out.is_empty() {
                default_suites_for(model_name)
            } else {
                out
            }
        }
        None => default_suites_for(model_name),
    }
}

// ---------------------------------------------------------------------------
// Per-model purpose routing
// ---------------------------------------------------------------------------

/// Infer the default suite list for a model from its name (the operator's
/// "correct tests for intended purposes"):
///   - "coder"                  → [context, code]
///   - "gpt-oss"                → [context, agent]
///   - "qwen3:8b" / "harness"   → [context, code, agent]
///   - "diffusiongemma"/"dgem"  → [context, code]  (note: non-Ollama daemon —
///                                the Ollama suites can't load it; callers skip
///                                it in the Ollama fleet)
///   - default                  → [context]
/// Pure.
pub fn default_suites_for(model_name: &str) -> Vec<String> {
    let n = model_name.to_lowercase();
    let v = if n.contains("coder") {
        vec!["context", "code"]
    } else if n.contains("gpt-oss") {
        vec!["context", "agent"]
    } else if n.contains("qwen3:8b") || n.contains("harness") {
        vec!["context", "code", "agent"]
    } else if n.contains("diffusiongemma") || n.contains("dgem") {
        vec!["context", "code"]
    } else {
        vec!["context"]
    };
    v.into_iter().map(String::from).collect()
}

/// Whether a model is a non-Ollama daemon model that the Ollama-based suites
/// cannot load (DiffusionGemma / dgem runs as its own C++ daemon). Pure.
pub fn is_non_ollama_daemon(model_name: &str) -> bool {
    let n = model_name.to_lowercase();
    n.contains("diffusiongemma") || n.contains("dgem")
}

/// Pick code-suite languages by model purpose: coder models get the full P0/P1
/// set; everyone else gets a lighter, fast set. Empty vec = "all languages in
/// the corpus" for coder models. Pure.
///
/// Returned tags are corpus `language` values (see manifest.json):
/// rust, typescript, python, bash, htmlcss, cpp, sql, config.
pub fn code_languages_for(model_name: &str) -> Vec<String> {
    let n = model_name.to_lowercase();
    let set: &[&str] = if n.contains("coder") {
        // Coder models: P0 + P1 (skip the P2-only config/sql/cpp heavy set to
        // keep the run bounded; rust/ts/python/bash/htmlcss covers the codebase).
        &["rust", "typescript", "python", "bash", "htmlcss"]
    } else {
        // Non-coder models: a light, fast, toolchain-present subset.
        &["python", "bash"]
    };
    set.iter().map(|s| s.to_string()).collect()
}

/// Parse an optional `tiers` override (a JSON array of ints). When absent, use
/// the full graduated tier list. The smoke run passes a short list.
fn parse_tiers(args: &Value) -> Vec<usize> {
    match args.get("tiers").and_then(|v| v.as_array()) {
        Some(arr) => {
            let v: Vec<usize> = arr
                .iter()
                .filter_map(|x| x.as_u64())
                .map(|n| n as usize)
                .filter(|n| *n > 0)
                .collect();
            if v.is_empty() {
                FULL_TIERS.to_vec()
            } else {
                v
            }
        }
        None => FULL_TIERS.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// model_intake
// ---------------------------------------------------------------------------

pub struct ModelIntake;

#[async_trait]
impl RustTool for ModelIntake {
    fn name(&self) -> &str { "model_intake" }

    fn description(&self) -> &str {
        "Profile a fleet model and store an operational profile in Postgres. Suites: \
         'context' (graduated context stress: throughput, TTFT, planted-fact recall, VRAM, OOM), \
         'code' (per-language code generation against the intake corpus → language whitelist), \
         'agent' (tool selection, multi-step, instruction following, hallucination, personality). \
         If 'suites' is omitted it is inferred from the model name (coder→context+code, \
         gpt-oss→context+agent, qwen3:8b/harness→all three, default→context). DiffusionGemma/dgem \
         is a non-Ollama daemon model and is skipped by these Ollama-based suites."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model_name": { "type": "string", "description": "Ollama model name, e.g. 'gpt-oss:20b'" },
                "suites": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["context", "code", "agent"] },
                    "description": "Which suites to run. Default: inferred from the model name (per-model purpose routing)."
                },
                "tiers": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Optional context-token tiers (default the full 2K..128K ladder). Pass a short list, e.g. [2000,8000,16000], for a quick/smoke run that won't swap the hot model."
                },
                "languages": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Code-suite languages (corpus tags: rust,typescript,python,bash,htmlcss,cpp,sql,config). Default: inferred by purpose (coder→P0/P1 set, else python+bash)."
                },
                "scenario_limit": {
                    "type": "integer",
                    "description": "Cap the number of agent scenarios (smoke runs). Default: all 55."
                },
                "case_limit": {
                    "type": "integer",
                    "description": "Cap the number of code-suite cases (smoke runs). Default: all matching cases."
                },
                "code_harness": {
                    "type": "string",
                    "enum": ["v1", "v2"],
                    "description": "Which code-suite harness to run. 'v2' (default) is the realistic build-scenario harness: real files + spec in context, graduated 0-5 score, retry, rows tagged harness_version='v3'. 'v1' is the legacy cold one-shot suite (additive, original rows)."
                },
                "backend": {
                    "type": "string",
                    "description": "Force this run onto a specific backend by name (e.g. 'llama-gpu' or 'ollama'), overriding the model's registry tag — used to profile the same model on both GPU and CPU sizing. Default: the model's tagged backend."
                }
            },
            "required": ["model_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let model_name = args["model_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'model_name' must be a string".into()))?
            .trim();
        if model_name.is_empty() {
            return Err(ToolError::InvalidArgument("'model_name' must not be empty".into()));
        }
        let suites = parse_suites(&args, model_name);
        let tiers = parse_tiers(&args);
        // Optional per-model code-language override; else inferred by purpose.
        let code_langs: Vec<String> = args
            .get("languages")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| code_languages_for(model_name));

        let mut out = String::new();
        out.push_str(&format!("Model intake: {model_name}\n"));
        out.push_str(&format!("Suites requested: {}\n\n", suites.join(", ")));

        // Daemon-model guard: DiffusionGemma/dgem can't be loaded by the
        // Ollama-based suites. Note it and run nothing.
        if is_non_ollama_daemon(model_name) {
            out.push_str(
                "Note: this is a non-Ollama daemon model (DiffusionGemma/dgem). \
                 The Ollama-based intake suites cannot load it — skipped. Profile it \
                 via its own daemon harness.\n",
            );
            return Ok(out);
        }

        // P5: optional backend override — force this run onto a specific backend
        // (e.g. `ollama` for the CPU-sizing pass, `llama-gpu` for the GPU pass)
        // regardless of the model's tag. A drop guard clears it on every exit.
        let backend_override = args
            .get("backend")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        crate::intake::infer::set_backend_override(backend_override.clone());
        struct ClearOverride;
        impl Drop for ClearOverride {
            fn drop(&mut self) {
                crate::intake::infer::set_backend_override(None);
            }
        }
        let _clear_override = ClearOverride;
        if let Some(b) = &backend_override {
            out.push_str(&format!("Backend override: {b}\n"));
        }

        // Resolve a profile_id: the context suite creates one; otherwise we make
        // a fresh model_profiles row so code/agent rows have a parent.
        let mut profile_id: Option<uuid::Uuid> = None;

        if suites.iter().any(|s| s == "context") {
            let res = runner::run_context_suite(model_name, &tiers, true).await?;
            profile_id = Some(res.profile_id);
            out.push_str("=== Context suite ===\n");
            out.push_str(&format!("Tiers run: {}", res.tiers_run));
            if res.stopped_on_oom {
                out.push_str(" (stopped early on OOM)");
            }
            out.push('\n');
            if let Some(prior) = &res.prior_hot {
                out.push_str(&format!("Prior hot model: {prior}\n"));
            }
            let op = &res.op;
            out.push_str(&format!(
                "max_context_safe: {}\nmax_context_absolute: {}\nquality_degradation_point: {}\n",
                op.max_context_safe.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                op.max_context_absolute.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                op.quality_degradation_point.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
            ));
            out.push_str(&format!(
                "recommended timeouts (sec): chat={} build={} deep={}\n",
                op.recommended_timeout_chat_sec.unwrap_or(0),
                op.recommended_timeout_build_sec.unwrap_or(0),
                op.recommended_timeout_deep_sec.unwrap_or(0),
            ));
            out.push_str(&format!(
                "overall_tier: {}\n\n",
                op.overall_tier.as_deref().unwrap_or("n/a")
            ));
        }

        // Ensure a parent profile row for code/agent-only runs.
        let needs_profile = suites.iter().any(|s| s == "code" || s == "agent");
        if needs_profile && profile_id.is_none() {
            profile_id = Some(runner::create_profile_row(model_name).await?);
        }

        if suites.iter().any(|s| s == "code") {
            let pid = profile_id.expect("profile_id set");
            let case_limit = args.get("case_limit").and_then(|v| v.as_u64()).map(|n| n as usize);
            // Default to the realistic v2 harness; v1 stays available + additive.
            let harness = args
                .get("code_harness")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .unwrap_or_else(|| "v2".to_string());

            if harness == "v1" {
                let res = code::run_code_suite_limited(model_name, &code_langs, pid, case_limit).await?;
                out.push_str("=== Code suite (v1: cold one-shot) ===\n");
                out.push_str(&format!(
                    "languages: {}\ncases run: {} ({} passed)\n",
                    if code_langs.is_empty() { "all".into() } else { code_langs.join(", ") },
                    res.cases_run,
                    res.cases_passed,
                ));
                out.push_str(&format!(
                    "approved (lang:complexity): {}\n",
                    if res.approved.is_empty() { "none".into() } else { res.approved.join(", ") }
                ));
                if !res.toolchain_skipped.is_empty() {
                    out.push_str(&format!("toolchain unavailable for: {}\n", res.toolchain_skipped.join(", ")));
                }
                out.push('\n');
            } else {
                let res = code_v2::run_code_suite_v2(model_name, &code_langs, pid, case_limit, None, None, None).await?;
                out.push_str("=== Code suite (v2: realistic build scenario) ===\n");
                out.push_str(&format!(
                    "languages: {}\ncases run: {} ({} scored, {} errored)\navg first_pass: {:.2} | avg effective (incl retry): {:.2}\n",
                    if code_langs.is_empty() { "all".into() } else { code_langs.join(", ") },
                    res.cases_run,
                    res.scored,
                    res.errors,
                    res.avg_first_pass,
                    res.avg_effective,
                ));
                out.push_str(&format!(
                    "approved (lang:tier): {}\n",
                    if res.approved.is_empty() { "none".into() } else { res.approved.join(", ") }
                ));
                for (id, fp, retry) in &res.per_case {
                    out.push_str(&format!(
                        "  {id}: first_pass={fp}{}\n",
                        retry.map(|r| format!(" retry={r}")).unwrap_or_default()
                    ));
                }
                if !res.toolchain_skipped.is_empty() {
                    out.push_str(&format!("toolchain unavailable for: {}\n", res.toolchain_skipped.join(", ")));
                }
                out.push('\n');
            }
        }

        if suites.iter().any(|s| s == "agent") {
            let pid = profile_id.expect("profile_id set");
            let limit = args.get("scenario_limit").and_then(|v| v.as_u64()).map(|n| n as usize);
            let res = agent::run_agent_suite(model_name, pid, limit).await?;
            let a = &res.aggregate;
            out.push_str("=== Agent suite ===\n");
            out.push_str(&format!(
                "scenarios run: {} ({} rows)\n",
                res.scenarios_run, res.rows_written
            ));
            out.push_str(&format!(
                "tool accuracy: {} (at 200 tools: {})\n",
                a.tool_accuracy_overall.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                a.tool_accuracy_at_200.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
            ));
            out.push_str(&format!(
                "multistep: {} | instruction: {} | hallucination: {} | personality: {}\n",
                a.multistep_rate.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                a.instruction_adherence.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                a.hallucination_rate.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                a.personality_quality.map(|v| format!("{v:.1}/5")).unwrap_or_else(|| "n/a".into()),
            ));
            out.push_str(&format!("recommended_role: {}\n\n", a.recommended_role));
        }

        out.push_str("Note: coherence_score stored as NULL (LLM-judge deferred).\n");

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// model_intake_status
// ---------------------------------------------------------------------------

pub struct ModelIntakeStatus;

#[async_trait]
impl RustTool for ModelIntakeStatus {
    fn name(&self) -> &str { "model_intake_status" }

    fn description(&self) -> &str {
        "Retrieve the stored operational profile for a model (context ceilings, throughput curve, \
         recommended timeouts, tier label), or report 'not profiled'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model_name": { "type": "string", "description": "Model name to look up" }
            },
            "required": ["model_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let model_name = args["model_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'model_name' must be a string".into()))?
            .trim();

        let pool = storage::get_pool().await?;
        match storage::read_latest_profile(&pool, model_name).await? {
            None => Ok(format!("{model_name}: not profiled")),
            Some(p) => Ok(format_status(&p)),
        }
    }
}

/// Render a stored profile as a readable text block.
fn format_status(p: &storage::StoredProfile) -> String {
    let op = &p.op;
    let mut s = String::new();
    s.push_str(&format!("Profile for {}\n", p.model_name));
    s.push_str(&format!(
        "  tier: {}\n",
        op.overall_tier.as_deref().unwrap_or("n/a")
    ));
    s.push_str(&format!(
        "  max_context_safe: {}\n  max_context_absolute: {}\n  quality_degradation_point: {}\n",
        op.max_context_safe.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
        op.max_context_absolute.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
        op.quality_degradation_point.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
    ));
    s.push_str(&format!(
        "  timeouts(sec): chat={} build={} deep={}\n",
        op.recommended_timeout_chat_sec.unwrap_or(0),
        op.recommended_timeout_build_sec.unwrap_or(0),
        op.recommended_timeout_deep_sec.unwrap_or(0),
    ));
    s.push_str("  tiers:\n");
    for t in &p.tiers {
        s.push_str(&format!(
            "    {:>7} tok | recall {} | {} tok/s | {} | {}\n",
            t.context_tokens,
            t.recall_score.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            t.throughput_tok_per_sec
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into()),
            t.memory_usage_mb
                .map(|v| format!("{v}MB"))
                .unwrap_or_else(|| "-".into()),
            if t.oom { "OOM" } else { "ok" },
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// model_intake_compare
// ---------------------------------------------------------------------------

pub struct ModelIntakeCompare;

#[async_trait]
impl RustTool for ModelIntakeCompare {
    fn name(&self) -> &str { "model_intake_compare" }

    fn description(&self) -> &str {
        "Compare stored profiles across models on a single metric \
         (throughput_at_2k|8k|16k|32k|64k, max_context_safe, max_context_absolute, \
          quality_degradation_point, recommended_timeout_chat_sec). Returns a table."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "models": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Model names to compare"
                },
                "metric": {
                    "type": "string",
                    "description": "Metric column to compare (e.g. 'throughput_at_16k', 'max_context_safe')"
                }
            },
            "required": ["models", "metric"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let models: Vec<String> = args["models"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArgument("'models' must be an array".into()))?
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if models.is_empty() {
            return Err(ToolError::InvalidArgument("'models' must not be empty".into()));
        }
        let metric = args["metric"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'metric' must be a string".into()))?
            .trim();

        let pool = storage::get_pool().await?;
        let mut rows: Vec<(String, Option<f64>)> = Vec::new();
        for m in &models {
            let val = match storage::read_latest_profile(&pool, m).await? {
                Some(p) => metric_value(&p.op, metric),
                None => None,
            };
            rows.push((m.clone(), val));
        }
        Ok(format_compare(metric, &rows))
    }
}

/// Extract a numeric metric from an operational profile by name.
fn metric_value(op: &storage::OperationalProfileRow, metric: &str) -> Option<f64> {
    match metric {
        "throughput_at_2k" => op.throughput_at_2k,
        "throughput_at_8k" => op.throughput_at_8k,
        "throughput_at_16k" => op.throughput_at_16k,
        "throughput_at_32k" => op.throughput_at_32k,
        "throughput_at_64k" => op.throughput_at_64k,
        "max_context_safe" => op.max_context_safe.map(|v| v as f64),
        "max_context_absolute" => op.max_context_absolute.map(|v| v as f64),
        "quality_degradation_point" => op.quality_degradation_point.map(|v| v as f64),
        "recommended_timeout_chat_sec" => op.recommended_timeout_chat_sec.map(|v| v as f64),
        "recommended_timeout_build_sec" => op.recommended_timeout_build_sec.map(|v| v as f64),
        "recommended_timeout_deep_sec" => op.recommended_timeout_deep_sec.map(|v| v as f64),
        _ => None,
    }
}

/// Format a comparison table for one metric. Pure — unit-tested.
pub fn format_compare(metric: &str, rows: &[(String, Option<f64>)]) -> String {
    let name_w = rows
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(5)
        .max("model".len());
    let mut s = String::new();
    s.push_str(&format!("Comparison — metric: {metric}\n"));
    s.push_str(&format!("{:<name_w$}  {}\n", "model", metric, name_w = name_w));
    s.push_str(&format!("{}  {}\n", "-".repeat(name_w), "-".repeat(metric.len().max(8))));
    for (name, val) in rows {
        let v = match val {
            Some(x) => {
                if (x.fract()).abs() < 1e-9 {
                    format!("{}", *x as i64)
                } else {
                    format!("{x:.1}")
                }
            }
            None => "not profiled".to_string(),
        };
        s.push_str(&format!("{name:<name_w$}  {v}\n", name_w = name_w));
    }
    s
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Fleet profiling — runs the context suite across the whole model catalog with
/// the simplified overnight VRAM lifecycle (record hot → per model: load →
/// profile → unload → restore the daily driver only at the very end). Intended
/// to run when nobody is talking to Lumina; the agent is offline during it.
pub struct ModelIntakeFleet;

#[async_trait]
impl RustTool for ModelIntakeFleet {
    fn name(&self) -> &str { "model_intake_fleet" }
    fn description(&self) -> &str {
        "Profile the ENTIRE model catalog overnight, picking suites PER MODEL by purpose \
         (coder→context+code, gpt-oss→context+agent, qwen3:8b/harness→all three, default→context). \
         DiffusionGemma/dgem is skipped (non-Ollama daemon). Loads, profiles, and unloads each \
         model in turn, restoring the daily-driver only at the very end — the agent is unavailable \
         during the run. Optional 'models' (default: all Ollama chat models), 'tiers', and \
         'model_suites' (explicit per-model override, e.g. {\"qwen3:8b\":[\"context\",\"agent\"]})."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "models": { "type": "array", "items": {"type": "string"},
                    "description": "Models to profile (default: all non-embedding Ollama models)." },
                "tiers": { "type": "array", "items": {"type": "integer"},
                    "description": "Context-token tiers (default full 2K..128K ladder)." },
                "model_suites": { "type": "object",
                    "description": "Explicit per-model suite override: {model: [suites]}. Overrides purpose inference for that model." }
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let tiers = parse_tiers(&args);
        // Explicit model list, or auto-enumerate the catalog.
        let mut models: Vec<String> = args
            .get("models")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if models.is_empty() {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| ToolError::Http(e.to_string()))?;
            models = runner::list_chat_models(&client).await;
        }
        if models.is_empty() {
            return Err(ToolError::NotConfigured("no models to profile (catalog empty)".into()));
        }

        // Per-model suite override map.
        let overrides = args.get("model_suites").cloned().unwrap_or(Value::Null);

        let resolve_suites = move |m: &str| -> Vec<String> {
            if let Some(arr) = overrides.get(m).and_then(|v| v.as_array()) {
                let v: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_lowercase()))
                    .filter(|s| !s.is_empty())
                    .collect();
                if !v.is_empty() {
                    return v;
                }
            }
            default_suites_for(m)
        };

        let results = runner::run_fleet_suites(
            &models,
            &tiers,
            resolve_suites,
            |m: &str| code_languages_for(m),
            |m: &str| is_non_ollama_daemon(m),
            |model, langs, pid| {
                Box::pin(async move {
                    code::run_code_suite(&model, &langs, pid)
                        .await
                        .map(|r| format!("{} cases, {} approved", r.cases_run, r.approved.len()))
                })
            },
            |model, pid| {
                Box::pin(async move {
                    agent::run_agent_suite(&model, pid, None)
                        .await
                        .map(|r| format!("{} scenarios, role={}", r.scenarios_run, r.aggregate.recommended_role))
                })
            },
        )
        .await;

        let mut out = format!(
            "Fleet intake complete: {} model(s), tiers {:?}\n\n",
            results.len(),
            tiers
        );
        for r in &results {
            let mark = if r.skipped { "⏭" } else { "✅" };
            out.push_str(&format!("{} {} [{}]: {}\n", mark, r.model, r.suites.join("+"), r.summary));
        }
        out.push_str("\nDaily driver restored. Results stored in Postgres (model_intake_compare / _status).\n");
        Ok(out)
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelIntake));
    registry.register_or_replace(Box::new(ModelIntakeStatus));
    registry.register_or_replace(Box::new(ModelIntakeCompare));
    registry.register_or_replace(Box::new(ModelIntakeFleet));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::storage::OperationalProfileRow;

    #[test]
    fn parse_suites_default_inferred_by_purpose() {
        // No suites → per-model purpose routing.
        assert_eq!(parse_suites(&json!({}), "qwen3-coder:30b"), vec!["context", "code"]);
        assert_eq!(parse_suites(&json!({}), "gpt-oss:20b"), vec!["context", "agent"]);
        assert_eq!(parse_suites(&json!({}), "qwen3:8b"), vec!["context", "code", "agent"]);
        assert_eq!(parse_suites(&json!({}), "mystery:7b"), vec!["context"]);
    }

    #[test]
    fn parse_suites_explicit_and_normalized() {
        let s = parse_suites(&json!({"suites": ["Context", " CODE "]}), "anything");
        assert_eq!(s, vec!["context", "code"]);
    }

    #[test]
    fn init_tracing_is_safe_to_call_more_than_once() {
        // Guards against a regression where a second call (e.g. a binary's
        // main() plus a test harness that also installs a subscriber) would
        // panic instead of being a harmless no-op via try_init.
        init_tracing();
        init_tracing();
    }

    #[test]
    fn parse_suites_empty_array_falls_back_to_purpose() {
        let s = parse_suites(&json!({"suites": []}), "gpt-oss:20b");
        assert_eq!(s, vec!["context", "agent"]);
    }

    #[test]
    fn default_suites_for_routing() {
        assert_eq!(default_suites_for("qwen3-coder:30b"), vec!["context", "code"]);
        assert_eq!(default_suites_for("gpt-oss:20b"), vec!["context", "agent"]);
        assert_eq!(default_suites_for("harness-1"), vec!["context", "code", "agent"]);
        assert_eq!(default_suites_for("diffusiongemma-26b-a4b"), vec!["context", "code"]);
        assert_eq!(default_suites_for("llama3:8b"), vec!["context"]);
    }

    #[test]
    fn daemon_and_languages_routing() {
        assert!(is_non_ollama_daemon("diffusiongemma-26b-a4b"));
        assert!(is_non_ollama_daemon("dgem-secondary"));
        assert!(!is_non_ollama_daemon("qwen3:8b"));
        assert_eq!(code_languages_for("qwen3:8b"), vec!["python", "bash"]);
        let coder = code_languages_for("qwen3-coder:30b");
        assert!(coder.contains(&"rust".to_string()));
        assert!(coder.contains(&"typescript".to_string()));
    }

    #[test]
    fn parse_tiers_default_full() {
        let t = parse_tiers(&json!({}));
        assert_eq!(t, FULL_TIERS.to_vec());
    }

    #[test]
    fn parse_tiers_short_smoke_list() {
        let t = parse_tiers(&json!({"tiers": [2000, 8000, 16000]}));
        assert_eq!(t, vec![2000, 8000, 16000]);
    }

    #[test]
    fn metric_value_lookup() {
        let mut op = OperationalProfileRow::default();
        op.throughput_at_16k = Some(123.4);
        op.max_context_safe = Some(16000);
        assert_eq!(metric_value(&op, "throughput_at_16k"), Some(123.4));
        assert_eq!(metric_value(&op, "max_context_safe"), Some(16000.0));
        assert_eq!(metric_value(&op, "bogus"), None);
    }

    #[test]
    fn format_compare_renders_values_and_missing() {
        let rows = vec![
            ("gpt-oss:20b".to_string(), Some(200.0)),
            ("qwen3:8b".to_string(), Some(412.5)),
            ("missing:1b".to_string(), None),
        ];
        let table = format_compare("throughput_at_16k", &rows);
        assert!(table.contains("throughput_at_16k"));
        assert!(table.contains("gpt-oss:20b"));
        assert!(table.contains("200")); // integer-formatted
        assert!(table.contains("412.5"));
        assert!(table.contains("not profiled"));
    }

    #[test]
    fn metadata_and_required_fields() {
        let i = ModelIntake;
        assert_eq!(i.name(), "model_intake");
        assert!(i.parameters()["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "model_name"));

        let s = ModelIntakeStatus;
        assert_eq!(s.name(), "model_intake_status");

        let c = ModelIntakeCompare;
        assert_eq!(c.name(), "model_intake_compare");
        let cparams = c.parameters();
        let req = cparams["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "models"));
        assert!(req.iter().any(|v| v == "metric"));
    }

    #[tokio::test]
    async fn model_intake_empty_name_is_invalid() {
        let r = ModelIntake.execute(json!({"model_name": "  "})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn compare_empty_models_is_invalid() {
        let r = ModelIntakeCompare
            .execute(json!({"models": [], "metric": "max_context_safe"}))
            .await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn current_epoch_is_the_single_source_of_truth() {
        // MINT2-05: the ONE place the current epoch is stated. `current_epoch()`
        // and the const agree, and `aggregate.rs` re-exports THIS value (proven
        // by `aggregate`'s own tests keying rows off `CURRENT_EPOCH`).
        assert_eq!(CURRENT_EPOCH, "v3");
        assert_eq!(current_epoch(), CURRENT_EPOCH);
    }

    #[test]
    fn epoch_selector_resolves_current_legacy_and_all() {
        // Default is Current → resolves to the one central value.
        assert_eq!(EpochSelector::default(), EpochSelector::Current);
        assert_eq!(EpochSelector::Current.epoch(), Some(current_epoch()));
        // An explicit legacy epoch stays queryable.
        assert_eq!(
            EpochSelector::Only("v1".to_string()).epoch(),
            Some("v1")
        );
        // All = no filter.
        assert_eq!(EpochSelector::All.epoch(), None);
    }

    #[test]
    fn epoch_where_fragment_appends_filter_or_true() {
        // Current/Only append the epoch filter at the given bind index; All is a
        // bind-free always-true fragment so callers can splice unconditionally.
        assert_eq!(
            epoch_where_fragment(&EpochSelector::Current, 1),
            "harness_version = $1"
        );
        assert_eq!(
            epoch_where_fragment(&EpochSelector::Only("v2".into()), 3),
            "harness_version = $3"
        );
        assert_eq!(epoch_where_fragment(&EpochSelector::All, 1), "TRUE");
    }

    #[test]
    fn run_kind_labels_are_stable() {
        assert_eq!(RunKind::Coder.as_str(), "coder");
        assert_eq!(RunKind::Assistant.as_str(), "assistant");
    }

    #[test]
    fn mint_harness_constructs_and_dispatches_both_kinds() {
        // MINT2-04: both run kinds construct under one MintHarness and dispatch
        // to the correct sub-runner, with a mocked/skipped DB — sub_runner()
        // construction is env-sourced only and touches no Postgres, so this
        // exercises the unify-and-dispatch structure without a live DB.
        let coder = MintHarness::new(RunKind::Coder);
        assert_eq!(coder.kind(), RunKind::Coder);
        assert_eq!(coder.sub_runner().kind(), RunKind::Coder);

        let assistant = MintHarness::new(RunKind::Assistant);
        assert_eq!(assistant.kind(), RunKind::Assistant);
        assert_eq!(assistant.sub_runner().kind(), RunKind::Assistant);

        // Distinct harness instances get distinct run identities.
        assert_ne!(coder.run_id(), assistant.run_id());
    }

    #[test]
    fn registration_adds_three_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("model_intake"));
        assert!(reg.contains("model_intake_status"));
        assert!(reg.contains("model_intake_compare"));
    }
}
