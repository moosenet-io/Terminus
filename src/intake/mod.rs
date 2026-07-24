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
pub mod catalog;
pub mod checkpoint;
pub mod chord_pull;
pub mod chord_session;
mod code;
mod code_v2;
pub mod coder_case;
pub mod coder_gaps;
pub mod coder_sweep;
mod context;
pub mod discovery;
pub mod gpu_authority;
pub mod infer;
pub mod jobs;
pub mod lifecycle;
pub mod newcats;
mod runner;
pub mod serving;
pub(crate) mod storage;
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
// MINT2-06: stale-cell re-run planner core (PURE)
// ---------------------------------------------------------------------------
//
// After an epoch bump (or a newly-added fleet model, or a raised sample target)
// most (model × test × config) cells no longer have a current-epoch result at
// the sample target and must be re-run — but re-running the WHOLE sweep is
// wasteful. This is the shared, family-agnostic heart of "re-run tests that
// evolve": given the intended coverage grid and how many current-epoch samples
// each cell already has, return EXACTLY the cells that still need work.
//
// It is deliberately generic over the caller's own coverage-cell key `K` (the
// coder planner keys on (model, category, config); the assistant planner keys on
// (model, dimension)) and takes the current-epoch sample counts as plain data,
// so it is a PURE function — grid + counts + target → stale list — unit-testable
// with no DB, clock, or env. Each family's own planner (in `coder_sweep.rs` /
// `assistant/runner.rs`) builds the grid + counts from ITS OWN epoch lineage
// (the coder [`CURRENT_EPOCH`] vs the assistant's separate
// `schema::HARNESS_VERSION`) and calls this.

/// PURE stale-cell planner core (MINT2-06). Given the intended coverage `grid`
/// and how many current-epoch samples each cell already has (`existing_counts`),
/// return EXACTLY the grid cells that are STALE: a cell is stale iff it has
/// `0` current-epoch samples (absent from `existing_counts`) OR fewer than
/// `target_samples`. A cell already at (or above) the target is NOT re-run.
///
/// The complement is computed over the GRID, so a cell only present in
/// `existing_counts` but NOT in the grid (e.g. a model dropped from the fleet, or
/// a legacy category no longer measured) is never returned — dropped work is not
/// re-run. The result is sorted + deduped for a deterministic work list. Same
/// input → same output; no DB, no clock, no env.
pub fn stale_cells<K>(
    grid: &[K],
    existing_counts: &std::collections::BTreeMap<K, i64>,
    target_samples: i64,
) -> Vec<K>
where
    K: Ord + Clone,
{
    let mut out: Vec<K> = grid
        .iter()
        .filter(|c| existing_counts.get(*c).copied().unwrap_or(0) < target_samples)
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Parse the per-cell stale sample target from a raw env value
/// (`INTAKE_STALE_TARGET_SAMPLES`). Default `1` (any current-epoch sample covers
/// the cell — the epoch-bump default: after a bump nothing is current, so the
/// planner returns the full grid). A larger value tops up under-sampled cells.
/// Clamped to at least `1` (a target of `0`/negative would mark every cell
/// covered, defeating the point). Pure over its input.
pub fn parse_stale_target(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1)
}

/// The per-cell stale sample target from the environment
/// (`INTAKE_STALE_TARGET_SAMPLES`, default `1`).
pub fn stale_target_from_env() -> i64 {
    parse_stale_target(std::env::var("INTAKE_STALE_TARGET_SAMPLES").ok().as_deref())
}

/// Parse the `--only-stale` run-mode flag from a raw env value
/// (`MINT_ONLY_STALE`). Truthy = `1`/`true`/`yes`/`on` (case-insensitive);
/// anything else (including unset) is `false` so the FULL sweep stays the
/// default. Pure over its input.
pub fn parse_only_stale(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Whether the unified harness should run in `--only-stale` mode
/// (`MINT_ONLY_STALE`, default `false` → full sweep).
pub fn only_stale_from_env() -> bool {
    parse_only_stale(std::env::var("MINT_ONLY_STALE").ok().as_deref())
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

        let exit = self.sub_runner().run().await;

        // MINT2-07: refresh the Model Fleet Catalog at the end of EVERY unified
        // harness run — coder AND assistant — not just the coder sweep. This is
        // the shared lifecycle, so it fires EXACTLY ONCE per run for both
        // `RunKind`s (the per-sweep code no longer calls it — no double-refresh).
        self.refresh_catalog_best_effort().await;

        exit
    }

    /// Re-derive and persist the Model Fleet Catalog after a sweep — BEST-EFFORT.
    ///
    /// The catalog spans both families (coder aggregates + assistant dimension
    /// scores + serving/agent profiles), so an assistant-only run that adds
    /// fresh `assistant_dimension_score` rows must re-derive the catalog too, or
    /// its assistant cells stay stale until a coder sweep happens to run — hence
    /// this lives in the shared lifecycle, kind-agnostic, invoked for BOTH kinds.
    ///
    /// Same posture as MINT2-03's aggregate refresh / MINT2-05's marker: a DB
    /// hiccup, a host with no DB configured, or an un-migrated host missing the
    /// `model_fleet_catalog` table(s) is LOGGED and swallowed — it NEVER turns an
    /// otherwise-successful sweep into a failure (the catalog is fully
    /// re-derivable next run). The DB URL resolves via the same
    /// `config::intake_database_url()` resolver `storage::get_pool()` uses — no
    /// raw env, no literal DSN. `refresh_fleet_catalog` stays a `pub fn` so
    /// MINT2-08's tool reads the persisted result on demand. Extracted from
    /// [`MintHarness::execute`] so the shared-lifecycle refresh is unit-testable
    /// without a live DB (a no-DB host exercises the swallow path).
    async fn refresh_catalog_best_effort(&self) {
        match storage::get_pool().await {
            Ok(pool) => match catalog::refresh_fleet_catalog(&pool).await {
                Ok(n) => eprintln!(
                    "MINT harness ({}): refreshed fleet catalog ({n} model card(s))",
                    self.kind.as_str()
                ),
                Err(e) => eprintln!(
                    "MINT harness ({}): could not refresh fleet catalog (continuing — \
                     catalog is derived, recomputes next run): {e}",
                    self.kind.as_str()
                ),
            },
            Err(e) => eprintln!(
                "MINT harness ({}): could not connect to refresh fleet catalog \
                 (continuing — catalog recomputes next run): {e}",
                self.kind.as_str()
            ),
        }
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
///   - "diffusiongemma"/"dgem"  → [diffusion]  (MINT-DIFF-01: a non-Ollama
///                                daemon model — the Ollama-based suites
///                                can't load it, so its default is the
///                                diffusion suite, not `context`/`code`)
///   - "nomic-embed"/"bge"/…    → [embedding_retrieval]  (SUITE-EMB: an
///                                embedding model can't run the chat-shaped
///                                suites; see [`is_embedding_model`])
///   - default                  → [context]
/// Pure.
pub fn default_suites_for(model_name: &str) -> Vec<String> {
    let n = model_name.to_lowercase();
    let v = if n.contains("diffusiongemma") || n.contains("dgem") {
        vec!["diffusion"]
    } else if is_embedding_model(&n) {
        // SUITE-EMB (TERM #508): an embedding model can't run the chat-shaped
        // context/code/agent suites — its default is the IR-retrieval suite.
        vec!["embedding_retrieval"]
    } else if is_vision_model(&n) {
        // SUITE-VQA: a vision/VLM model's default is the image-QA suite (the
        // Ollama context suite is text-only and doesn't exercise its vision path).
        vec!["vision_qa"]
    } else if n.contains("coder") {
        vec!["context", "code"]
    } else if n.contains("gpt-oss") {
        vec!["context", "agent"]
    } else if n.contains("qwen3:8b") || n.contains("harness") {
        vec!["context", "code", "agent"]
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

/// Whether a model is a text-embedding model (SUITE-EMB): matched by the common
/// embedding-model name markers in this fleet's registry (nomic-embed, bge,
/// mxbai-embed, gte, e5, embeddinggemma, or a bare `-embed`/`embedding` tag).
/// Pure. Deliberately conservative substring matching — a chat model won't carry
/// these markers, and a false negative just falls back to the `context` default.
pub fn is_embedding_model(model_name: &str) -> bool {
    let n = model_name.to_lowercase();
    n.contains("embed")
        || n.contains("nomic")
        || n.contains("bge-")
        || n.contains("mxbai")
        || n.contains("gte-")
        || n.starts_with("gte")
        || n.contains("e5-")
}

/// SUITE-VQA: whether a model name looks like a vision-capable (VLM) model that
/// the image-QA suite should profile. Matches the common local VLM families.
/// Pure. `model_name` is expected already-lowercased by the caller path, but is
/// lowercased again defensively.
pub fn is_vision_model(model_name: &str) -> bool {
    let n = model_name.to_lowercase();
    n.contains("llava")
        || n.contains("bakllava")
        || n.contains("minicpm-v")
        || n.contains("vision")
        || n.contains("-vl")
        || n.contains(":vl")
        || n.contains("moondream")
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
         'diffusion' (MINT-DIFF-01: DiffusionGemma/dgem use-case QUALITY + PERFORMANCE probe, run \
         over the dgem daemon, not Ollama). If 'suites' is omitted it is inferred from the model \
         name (coder→context+code, gpt-oss→context+agent, qwen3:8b/harness→all three, \
         diffusiongemma/dgem→diffusion, default→context). DiffusionGemma/dgem is a non-Ollama \
         daemon model: the context/code/agent suites cannot load it (skipped), but 'diffusion' runs \
         via its own daemon path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model_name": { "type": "string", "description": "Ollama model name, e.g. 'gpt-oss:20b'" },
                "suites": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["context", "code", "agent", "diffusion", "tool_routing", "vision_qa"] },
                    "description": "Which suites to run. Default: inferred from the model name (per-model purpose routing). 'diffusion' profiles a non-Ollama daemon model (DiffusionGemma/dgem) via its own daemon path — the other suites don't apply to it. 'tool_routing' profiles function-calling over Chord's OpenAI-compatible /v1/chat/completions (correct-tool@1, parameter validity, decoy rejection, multi-step) — a first-class generalization of the 'agent' suite's tool-selection path. 'vision_qa' profiles a vision/VLM model on image-QA via Chord's chat/vision route (accuracy, caption similarity, hallucination, latency, VRAM)."
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
        // Ollama-based suites (context/code/agent). MINT-DIFF-01: the guard is
        // now RELAXED for the "diffusion" suite specifically — a daemon model
        // requesting "diffusion" runs that suite via its own daemon path below;
        // any Ollama-based suites also requested alongside it are still not
        // applicable and are skipped with a note, never silently dropped.
        if is_non_ollama_daemon(model_name) {
            if suites.iter().any(|s| s == "diffusion") {
                let res = runner::run_diffusion_suite(model_name).await?;
                out.push_str("=== Diffusion suite ===\n");
                out.push_str(&format!(
                    "use cases run: {}\navg use_case_success: {:.2}\navg time_to_output_ms: {:.0}\n",
                    res.use_cases_run, res.avg_use_case_success, res.avg_time_to_output_ms,
                ));
                for line in &res.per_use_case {
                    out.push_str(&format!("  {line}\n"));
                }
                if suites.iter().any(|s| s != "diffusion") {
                    out.push_str(
                        "Note: Ollama-based suites (context/code/agent) are not applicable to this \
                         non-Ollama daemon model and were skipped.\n",
                    );
                }
                return Ok(out);
            }
            out.push_str(
                "Note: this is a non-Ollama daemon model (DiffusionGemma/dgem). \
                 The Ollama-based intake suites cannot load it — skipped. Request the \
                 'diffusion' suite to profile it via its own daemon harness.\n",
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

        // Ensure a parent profile row for code/agent/tool_routing-only runs.
        let needs_profile = suites.iter().any(|s| s == "code" || s == "agent" || s == "tool_routing");
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

            // MINT-CODE-AGG: the `model_intake` code path persists raw
            // `code_profile_runs`, but the catalog reads the DERIVED
            // `code_run_aggregates` TABLE — so without recomputing it here the
            // coder cells stay `not_run` even though the runs landed. Refresh
            // this epoch's aggregates now, exactly as the CLI coder sweep does
            // (see `aggregate::recompute_and_persist_current_epoch`). Best-effort:
            // a DB hiccup / un-migrated host is logged, never fails the sweep
            // (the aggregate is fully re-derivable next run).
            match storage::get_pool().await {
                Ok(pool) => match crate::intake::aggregate::recompute_and_persist_current_epoch(&pool).await {
                    Ok(n) => out.push_str(&format!("Code-run aggregates refreshed: {n} cell(s).\n\n")),
                    Err(e) => {
                        out.push_str(&format!("(code-run aggregate refresh skipped — recomputes next run: {e})\n\n"))
                    }
                },
                Err(e) => out.push_str(&format!("(code-run aggregate refresh skipped — no DB pool: {e})\n\n")),
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

        // SUITE-EMB (TERM #508): IR-retrieval profiling for embedding models.
        // Self-contained (creates its own profile row, loads its own corpora),
        // so it runs independently of the context/code/agent profile_id above.
        if suites.iter().any(|s| s == "embedding_retrieval") {
            let res = runner::run_embedding_retrieval_suite(model_name).await?;
            out.push_str("=== Embedding-retrieval suite (SUITE-EMB) ===\n");
            if res.skipped {
                out.push_str(&format!("skipped: {}\n\n", res.summary));
            } else {
                out.push_str(&format!("{}\n\n", res.summary));
            }
        }
        if suites.iter().any(|s| s == "tool_routing") {
            let pid = profile_id.expect("profile_id set");
            let limit = args.get("scenario_limit").and_then(|v| v.as_u64()).map(|n| n as usize);
            let res = runner::run_tool_routing_suite(model_name, pid, limit).await?;
            out.push_str("=== Tool-routing suite ===\n");
            out.push_str(&format!(
                "scenarios run: {} ({} rows, {} errored/skipped)\n",
                res.scenarios_run, res.rows_written, res.errored
            ));
            let pct = |v: Option<f64>| v.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "n/a".into());
            out.push_str(&format!(
                "correct_tool@1: {} | parameter_validity: {} | decoy_rejection: {} | multi_step: {}\n\n",
                pct(res.correct_tool_at_1),
                pct(res.parameter_validity),
                pct(res.decoy_rejection),
                pct(res.multi_step_success),
            ));
        }

        // SUITE-VQA: the vision-QA suite loads its own image corpus and profiles
        // via Chord's chat/vision route, creating its own profile row (like the
        // diffusion suite) — it does not share the context/code/agent profile_id.
        if suites.iter().any(|s| s == "vision_qa") {
            let res = runner::run_vision_qa_suite(model_name).await?;
            out.push_str("=== Vision-QA suite ===\n");
            out.push_str(&format!(
                "items run: {}\naccuracy: {:.2}\nhallucination_rate: {:.2}\navg_latency_ms: {:.0}\n",
                res.items_run, res.accuracy, res.hallucination_rate, res.avg_latency_ms,
            ));
            for line in &res.per_item {
                out.push_str(&format!("  {line}\n"));
            }
            out.push('\n');
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

/// The actual fleet sweep, shared by the synchronous and async `model_intake_fleet`
/// paths. `job_id` is `Some` for the async path, so per-model progress is mirrored
/// into the [`jobs`] registry as the sweep runs; the synchronous path passes `None`
/// and pays no registry overhead beyond a no-op closure call per model.
///
/// Extracted from the old inline `execute` body so it can run either inline
/// (blocking, `async=false`, current behavior) or inside a `tokio::spawn`ed
/// background task (`async=true`) without duplicating the sweep logic.
async fn run_fleet_sweep(args: &Value, job_id: Option<&str>) -> Result<String, ToolError> {
    let tiers = parse_tiers(args);
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
        |done, total, model, suites| {
            if let Some(jid) = job_id {
                let m = if model.is_empty() { None } else { Some(model) };
                let s = if suites.is_empty() { None } else { Some(suites) };
                jobs::update_progress(jid, done, total, m, s);
            }
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

    // The Model Fleet Catalog is DERIVED and, unlike the CLI MINT harness, the
    // MCP fleet path did not historically refresh it — so an MCP-driven sweep
    // left `model_fleet_catalog` stale. Refresh best-effort here (same posture
    // as `MintHarness::refresh_catalog_best_effort`): a DB hiccup / un-migrated
    // host is LOGGED and swallowed, never turning an otherwise-successful sweep
    // into a failure (the catalog is fully re-derivable and also reachable via
    // the `model_fleet_catalog_refresh` tool).
    match storage::get_pool().await {
        Ok(pool) => {
            // MINT-CODE-AGG: recompute this epoch's `code_run_aggregates`
            // BEFORE refreshing the catalog — the catalog's coder cells read
            // the derived aggregate TABLE, not the raw `code_profile_runs`, so
            // skipping this leaves coder coverage `not_run` despite real runs.
            match crate::intake::aggregate::recompute_and_persist_current_epoch(&pool).await {
                Ok(n) => out.push_str(&format!("Code-run aggregates refreshed: {n} cell(s).\n")),
                Err(e) => out.push_str(&format!(
                    "(code-run aggregate refresh skipped — recomputes next run: {e})\n"
                )),
            }
            match catalog::refresh_fleet_catalog(&pool).await {
                Ok(n) => out.push_str(&format!("Fleet catalog refreshed: {n} model card(s).\n")),
                Err(e) => out.push_str(&format!(
                    "(fleet catalog refresh skipped — derived, recomputes next run: {e})\n"
                )),
            }
        }
        Err(e) => out.push_str(&format!("(fleet catalog refresh skipped — no DB pool: {e})\n")),
    }

    Ok(out)
}

#[async_trait]
impl RustTool for ModelIntakeFleet {
    fn name(&self) -> &str { "model_intake_fleet" }
    fn description(&self) -> &str {
        "Profile the ENTIRE model catalog overnight, picking suites PER MODEL by purpose \
         (coder→context+code, gpt-oss→context+agent, qwen3:8b/harness→all three, default→context). \
         DiffusionGemma/dgem is skipped (non-Ollama daemon). Loads, profiles, and unloads each \
         model in turn, restoring the daily-driver only at the very end — the agent is unavailable \
         during the run. Optional 'models' (default: all Ollama chat models), 'tiers', and \
         'model_suites' (explicit per-model override, e.g. {\"qwen3:8b\":[\"context\",\"agent\"]}). \
         BLD-ASYNC: pass 'async'=true to avoid blocking the whole sweep behind the loopback MCP's \
         900s forward timeout — the sweep runs in the background and this call returns a job_id \
         immediately; poll it with `model_intake_job_status`. Default 'async'=false keeps the prior \
         blocking behavior (fine for scoped/short runs that fit under 900s)."
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
                    "description": "Explicit per-model suite override: {model: [suites]}. Overrides purpose inference for that model." },
                "async": { "type": "boolean",
                    "description": "Run the sweep as a non-blocking background job (default false = blocking, current behavior). When true, returns a job_id immediately — poll with model_intake_job_status." }
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let is_async = args.get("async").and_then(|v| v.as_bool()).unwrap_or(false);

        if !is_async {
            return run_fleet_sweep(&args, None).await;
        }

        // Async path: register the job, hand its id back immediately, and let
        // the sweep run to completion in the background — this is exactly the
        // shape BLD-ASYNC exists for: the sweep can run well past the loopback
        // MCP's 900s forward timeout without the CALL itself timing out.
        //
        // Concurrency guard (opus review, MEDIUM): only ONE fleet sweep may be
        // in flight at a time — two overlapping sweeps would contend for the
        // single GPU. `try_start_job` atomically claims the slot; if a sweep is
        // already queued/running it returns that job's id and we reject with a
        // clear "already running, poll <id>" message rather than spawning a
        // second, GPU-contending sweep.
        let job_id = match jobs::try_start_job() {
            Ok(id) => id,
            Err(active_id) => {
                return Ok(format!(
                    "A fleet intake sweep is already in progress (job {active_id}). Only one runs \
                     at a time (they would contend for the GPU). Poll `model_intake_job_status` \
                     with job_id=\"{active_id}\", or wait for it to finish before starting another."
                ));
            }
        };
        let spawned_id = job_id.clone();
        let spawned_args = args.clone();
        tokio::spawn(async move {
            jobs::mark_running(&spawned_id);
            match run_fleet_sweep(&spawned_args, Some(&spawned_id)).await {
                Ok(summary) => jobs::mark_completed(&spawned_id, summary),
                Err(e) => jobs::mark_failed(&spawned_id, e.to_string()),
            }
        });

        Ok(format!(
            "Started async fleet intake job {job_id} (running in the background — this call did \
             NOT block on the sweep). Poll `model_intake_job_status` with job_id=\"{job_id}\" for \
             progress and the final summary."
        ))
    }
}

// ---------------------------------------------------------------------------
// model_intake_job_status (BLD-ASYNC poll surface)
// ---------------------------------------------------------------------------

/// Poll surface for an async `model_intake_fleet` run. Named `_job_status`
/// (not `_status`) to avoid colliding with the pre-existing `model_intake_status`
/// tool, which looks up a model's stored PROFILE (a different concept — a
/// per-model DB row, not a job's in-flight run state).
pub struct ModelIntakeJobStatus;

#[async_trait]
impl RustTool for ModelIntakeJobStatus {
    fn name(&self) -> &str { "model_intake_job_status" }

    fn description(&self) -> &str {
        "Poll an async model_intake_fleet job (BLD-ASYNC). Pass 'job_id' to get that job's status \
         (queued/running/completed/failed), per-model progress, and — once completed/failed — the \
         final summary or error. Omit 'job_id' to list recent jobs (most recent first)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "Job id returned by model_intake_fleet(async=true). Omit to list recent jobs." },
                "limit": { "type": "integer", "description": "Max jobs to list when 'job_id' is omitted (default 10)." }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        if let Some(job_id) = args.get("job_id").and_then(|v| v.as_str()) {
            let job_id = job_id.trim();
            if job_id.is_empty() {
                return Err(ToolError::InvalidArgument("'job_id' must not be empty".into()));
            }
            return match jobs::get_job(job_id) {
                Some(s) => Ok(format_job_state(&s)),
                None => Ok(format!("job {job_id}: not found (unknown id, or the process restarted since it ran)")),
            };
        }

        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(10);
        let recent = jobs::list_jobs(limit);
        if recent.is_empty() {
            return Ok("No intake jobs recorded (nothing has run async since this process started).".to_string());
        }
        let mut out = format!("Recent intake jobs ({}):\n\n", recent.len());
        for s in &recent {
            out.push_str(&format_job_state(&s));
            out.push('\n');
        }
        Ok(out)
    }
}

/// Render one [`jobs::JobState`] as a readable text block.
fn format_job_state(s: &jobs::JobState) -> String {
    let mut out = format!(
        "job {}: {} | progress {}/{}",
        s.job_id,
        s.status.as_str(),
        s.progress.models_done,
        s.progress.models_total,
    );
    if let Some(m) = &s.progress.current_model {
        out.push_str(&format!(" | current: {m} [{}]", s.progress.current_suites.as_deref().unwrap_or("")));
    }
    out.push_str(&format!(" | started {}\n", s.started_at.to_rfc3339()));
    if let Some(summary) = &s.summary {
        out.push_str(summary);
        if !summary.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(err) = &s.error {
        out.push_str(&format!("error: {err}\n"));
    }
    out
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelIntake));
    registry.register_or_replace(Box::new(ModelIntakeStatus));
    registry.register_or_replace(Box::new(ModelIntakeCompare));
    registry.register_or_replace(Box::new(ModelIntakeFleet));
    // BLD-ASYNC: poll surface for an async model_intake_fleet job.
    registry.register_or_replace(Box::new(ModelIntakeJobStatus));
    // MINT2-08: the read-only `model_fleet_catalog` core tool (its own register()
    // in `catalog.rs`, mirroring how plane/gitea keep registration next to the
    // tool). Core registry only — no personal registry.
    catalog::register(registry);
    // DISC-02 (S114): the read-only `model_discovery_brochure` core tool (its
    // own register() in `discovery/tool.rs`, wired through `discovery::register`).
    // Core registry only — no personal registry.
    discovery::register(registry);
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
        assert_eq!(default_suites_for("diffusiongemma-26b-a4b"), vec!["diffusion"]);
        assert_eq!(default_suites_for("dgem-secondary"), vec!["diffusion"]);
        assert_eq!(default_suites_for("llama3:8b"), vec!["context"]);
        // SUITE-EMB: embedding models route to the embedding_retrieval suite.
        assert_eq!(default_suites_for("nomic-embed-text:latest"), vec!["embedding_retrieval"]);
        assert_eq!(default_suites_for("bge-large-en"), vec!["embedding_retrieval"]);
        assert_eq!(default_suites_for("mxbai-embed-large"), vec!["embedding_retrieval"]);
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

    // ---- MINT2-06: pure stale-cell planner core ----

    #[test]
    fn stale_set_is_exactly_the_complement() {
        // Grid of 4 cells; two already have >= target current-epoch samples, two
        // do not (one under-sampled, one absent) → the stale set is EXACTLY the
        // two uncovered cells, no more, no less.
        use std::collections::BTreeMap;
        let grid = vec!["a", "b", "c", "d"];
        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        counts.insert("a", 5); // at target → covered
        counts.insert("b", 2); // below target → stale
        counts.insert("c", 5); // at target → covered
        // "d" absent → 0 samples → stale
        let stale = stale_cells(&grid, &counts, 5);
        assert_eq!(stale, vec!["b", "d"]);
    }

    #[test]
    fn cell_already_at_target_is_not_rerun() {
        use std::collections::BTreeMap;
        let grid = vec!["x"];
        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        counts.insert("x", 7);
        // At target → not stale.
        assert!(stale_cells(&grid, &counts, 7).is_empty());
        // Above target → still not stale.
        assert!(stale_cells(&grid, &counts, 5).is_empty());
    }

    #[test]
    fn empty_counts_makes_everything_stale() {
        // The un-migrated-DB / fresh-epoch case: NO current-epoch samples exist →
        // the planner returns the WHOLE grid (correct: everything must be run).
        use std::collections::BTreeMap;
        let grid = vec!["a", "b", "c"];
        let counts: BTreeMap<&str, i64> = BTreeMap::new();
        assert_eq!(stale_cells(&grid, &counts, 1), vec!["a", "b", "c"]);
    }

    #[test]
    fn counts_outside_grid_are_never_rerun() {
        // A cell present in the counts but NOT in the grid (dropped-from-fleet
        // model / retired category) is never returned — dropped work isn't run.
        use std::collections::BTreeMap;
        let grid = vec!["a"];
        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        counts.insert("gone", 0); // 0 samples but not in the grid
        counts.insert("a", 5);
        assert!(stale_cells(&grid, &counts, 5).is_empty());
    }

    #[test]
    fn raising_target_makes_below_target_cells_stale() {
        // A cell covered at target=3 becomes stale when the target is raised to 6.
        use std::collections::BTreeMap;
        let grid = vec!["a"];
        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        counts.insert("a", 4);
        assert!(stale_cells(&grid, &counts, 3).is_empty(), "4 >= 3 → covered");
        assert_eq!(stale_cells(&grid, &counts, 6), vec!["a"], "4 < 6 → stale");
    }

    #[test]
    fn parse_stale_target_defaults_and_clamps() {
        assert_eq!(parse_stale_target(None), 1);
        assert_eq!(parse_stale_target(Some("")), 1);
        assert_eq!(parse_stale_target(Some("garbage")), 1);
        assert_eq!(parse_stale_target(Some("0")), 1, "0 clamps up to 1");
        assert_eq!(parse_stale_target(Some("-3")), 1, "negative clamps up to 1");
        assert_eq!(parse_stale_target(Some(" 7 ")), 7);
    }

    #[test]
    fn parse_only_stale_is_false_unless_explicitly_truthy() {
        assert!(!parse_only_stale(None), "unset → full sweep (default)");
        assert!(!parse_only_stale(Some("")));
        assert!(!parse_only_stale(Some("0")));
        assert!(!parse_only_stale(Some("false")));
        assert!(parse_only_stale(Some("1")));
        assert!(parse_only_stale(Some("true")));
        assert!(parse_only_stale(Some("YES")));
        assert!(parse_only_stale(Some(" On ")));
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

    /// MINT2-07: the fleet-catalog refresh is part of the SHARED lifecycle, so
    /// it is reachable and best-effort for BOTH run kinds (coder AND assistant),
    /// not just the coder sweep. With no DB configured, the shared refresh path
    /// takes its swallow branch (a clean `NotConfigured` connect error, logged)
    /// and returns without panicking or failing — exactly the posture a live run
    /// relies on. Proving it for BOTH kinds proves an assistant-only run also
    /// triggers the refresh (the acceptance criterion). No live DB needed.
    #[tokio::test]
    #[serial_test::serial]
    async fn catalog_refresh_runs_for_both_kinds_best_effort() {
        std::env::remove_var("DATABASE_URL");
        std::env::remove_var("INTAKE_DATABASE_URL");
        // Both kinds reach the same shared helper; neither panics nor blocks on
        // the missing DB — the swallow branch runs and returns unit.
        MintHarness::new(RunKind::Coder)
            .refresh_catalog_best_effort()
            .await;
        MintHarness::new(RunKind::Assistant)
            .refresh_catalog_best_effort()
            .await;
    }

    #[test]
    fn registration_adds_intake_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("model_intake"));
        assert!(reg.contains("model_intake_status"));
        assert!(reg.contains("model_intake_compare"));
        assert!(reg.contains("model_intake_fleet"));
        // BLD-ASYNC: the async poll surface registers alongside the intake tools.
        assert!(reg.contains("model_intake_job_status"));
        // MINT2-08: the read-only fleet-catalog tool registers on the same core
        // path as the rest of the intake module (→ register_all → Chord).
        assert!(reg.contains("model_fleet_catalog"));
    }

    // ---- BLD-ASYNC: async model_intake_fleet + model_intake_job_status ----

    /// Drain (complete) every active job so a slot-sensitive async test starts
    /// from a free sweep slot despite the shared global registry.
    fn drain_active_jobs() {
        for j in jobs::list_jobs(usize::MAX) {
            if matches!(j.status, jobs::JobStatus::Queued | jobs::JobStatus::Running) {
                jobs::mark_completed(&j.job_id, "test-drain".into());
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn intake_fleet_async_returns_job_id_without_blocking_on_the_sweep() {
        // A synchronous sweep needs a live Ollama host + Postgres and can run for
        // minutes — neither is available in a unit test. `async=true` must return
        // a job_id immediately (the whole point of BLD-ASYNC: the CALL itself
        // never blocks on the sweep), well under any reasonable test timeout,
        // even though the spawned background task will itself fail fast (no
        // reachable host) rather than actually profiling anything.
        // Free the single sweep slot first (the concurrency guard would otherwise
        // reject this submit if a parallel test held it).
        drain_active_jobs();
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ModelIntakeFleet.execute(json!({
                "async": true,
                "models": ["unit-test-placeholder-model"]
            })),
        )
        .await
        .expect("execute(async=true) must return well within 5s — it must not block on the sweep");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));

        let out = result.expect("async=true path returns Ok with a job_id, never runs the sweep inline");
        assert!(out.contains("Started async fleet intake job"));
        assert!(out.contains("model_intake_job_status"));

        // Extract the job id backed into the registry and confirm it is actually
        // tracked (queued/running — the background task may or may not have
        // gotten its first poll in yet, but the id must be a live job).
        let job_id = out
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .expect("response embeds the job id");
        assert!(jobs::get_job(job_id).is_some(), "job id from the response must be a real, tracked job");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn intake_fleet_async_rejects_a_second_concurrent_sweep() {
        // Concurrency guard (opus review): with one fleet sweep already in
        // flight, a second async submit must be REJECTED (clear message naming
        // the active job) rather than spawn a second, GPU-contending sweep.
        drain_active_jobs();
        // Claim + hold the single slot to simulate an in-flight sweep.
        let held = {
            let mut got = None;
            for _ in 0..100 {
                match jobs::try_start_job() {
                    Ok(id) => {
                        got = Some(id);
                        break;
                    }
                    Err(a) => jobs::mark_completed(&a, "test-drain".into()),
                }
            }
            got.expect("claimed the sweep slot")
        };
        jobs::mark_running(&held);

        let out = ModelIntakeFleet
            .execute(json!({"async": true, "models": ["x"]}))
            .await
            .expect("a rejected concurrent submit is a normal Ok(message), not an error");
        assert!(out.contains("already in progress"), "response signals the guard rejected it");
        assert!(out.contains(&held), "response names the in-flight job to poll");
        // Must NOT have spawned a second job.
        assert!(!out.contains("Started async fleet intake job"));

        jobs::mark_completed(&held, "cleanup".into());
    }

    #[tokio::test]
    async fn intake_fleet_sync_default_does_not_create_a_job() {
        // async defaults to false — must keep exactly the prior blocking-path
        // behavior (empty catalog + no reachable Ollama → NotConfigured) and
        // must never mention a job id, since it never spawns one. (Doesn't
        // assert on the global job registry's SIZE — this suite runs tests in
        // parallel and other tests legitimately create jobs concurrently, so a
        // before/after count would be racy; the job-id-shaped response text is
        // the real, non-racy signature of "did this path go async".)
        let r = ModelIntakeFleet.execute(json!({"models": []})).await;
        // With models=[] and (in this sandboxed test env) no reachable Ollama to
        // auto-enumerate from, this resolves to the pre-existing "no models to
        // profile" error path — proving the sync path still runs INLINE (it
        // returns an error synchronously) rather than spawning a job.
        match r {
            Err(e) => assert!(!e.to_string().contains("Started async fleet intake job")),
            Ok(out) => assert!(!out.contains("Started async fleet intake job")),
        }
    }

    #[test]
    fn job_status_tool_lists_recent_jobs_and_looks_up_by_id() {
        let id = jobs::create_job();
        jobs::mark_running(&id);
        jobs::update_progress(&id, 1, 4, Some("gpt-oss:20b"), Some("context"));
        jobs::mark_completed(&id, "Fleet intake complete: 4 model(s)".to_string());

        let s = jobs::get_job(&id).expect("job present");
        let rendered = format_job_state(&s);
        assert!(rendered.contains(&id));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("1/4"));
        assert!(rendered.contains("gpt-oss:20b"));
        assert!(rendered.contains("Fleet intake complete"));
    }

    #[tokio::test]
    async fn job_status_tool_unknown_id_reports_not_found_not_error() {
        let out = ModelIntakeJobStatus
            .execute(json!({"job_id": "totally-unknown-id"}))
            .await
            .expect("unknown id is a normal Ok(\"not found\") response, not an error");
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn job_status_tool_rejects_empty_job_id() {
        let r = ModelIntakeJobStatus.execute(json!({"job_id": "   "})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }
}
