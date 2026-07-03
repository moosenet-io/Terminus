//! S84 ASMT-09 — the consolidated, reboot-survivable profiling runner.
//!
//! Orchestrates, per the spec:
//!   1. read `nominations.json` (ASMT-08) from the reliable NAS staging dir;
//!   2. for each model: pick the acquisition path + backend strategy from its
//!      gfx1151 class ([`acquire`]); skip over-VRAM / hanging models with a
//!      RECORDED reason and keep going;
//!   3. a **bounded smoke** (1 case) per acquired model to fail fast on a broken
//!      acquisition before committing the full suite;
//!   4. the full ASMT-02..07 suite **per model per backend**, driving each backend
//!      via the P5 override (`infer::set_backend_override`) exactly like the S83
//!      coder harness — so a model is measured on BOTH gpu and cpu where tagged;
//!   5. **incremental persistence**: each dimension's rows are written the instant
//!      that dimension completes (via [`schema::insert_dimension_score`]) and a
//!      per-(model, backend, dimension) checkpoint is recorded, so a mid-run reboot
//!      RESUMES instead of restarting (mirrors S83's resilient runner);
//!   6. register survivors into the **Lumina** fleet as append-only rows that never
//!      clobber a model's existing **Harmony** rows ([`fleet`]).
//!
//! ## Why the orchestration is trait-driven
//! The real dimension runners ([`dim1_conversation::run_dim1`] … [`dim6_embeddings::run_dim6`])
//! each take a model-trait object + corpus and route inference through the unified
//! proxy path. To keep THIS file testable without a network, DB, or GPU, the three
//! collaborators it needs are abstractions:
//!   - [`SuiteDriver`] — runs the smoke + the six dimensions for one (model,
//!     backend); the live impl [`LiveSuiteDriver`] wires the real dim runners under
//!     `set_backend_override` (see [`LiveSuiteDriver::run_dimension`]); tests inject
//!     a deterministic driver.
//!   - [`Checkpoint`] — the resume ledger; the live impl persists to the NAS
//!     staging file, tests use an in-memory one.
//!   - [`super::fleet::FleetStore`] — append-only fleet rows.
//!
//! All inference stays on the unified proxy path; nothing here talks to Ollama
//! directly. All paths come from `config.rs`; no infra literals.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::config;
use crate::error::ToolError;

use super::acquire::{self, Acquirer, AcquisitionOutcome, Nomination, Nominations};
use super::fleet::{self, FleetStore};
use super::{schema, BackendTag, DimensionScore, ModelId};

/// The six assistant dimensions the suite runs, in order. Stored as the
/// checkpoint key so a resume skips exactly the dimensions already persisted.
pub const SUITE_DIMENSIONS: &[&str] = &[
    super::dim1_conversation::DIMENSION,
    super::dim2_toolchain::DIMENSION,
    super::dim3_memory::DIMENSION,
    super::dim4_ocean::DIMENSION,
    super::dim5_prompted::DIMENSION,
    super::dim6_embeddings::DIMENSION,
];

// ===========================================================================
// Resume checkpoint ledger
// ===========================================================================

/// One unit of completed work: a (model, backend, dimension) whose rows are
/// already persisted. The resume key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct CheckpointKey {
    pub model_id: String,
    pub backend_tag: String,
    pub dimension: String,
}

impl CheckpointKey {
    pub fn new(model_id: &ModelId, backend_tag: BackendTag, dimension: &str) -> Self {
        CheckpointKey {
            model_id: model_id.as_str().to_string(),
            backend_tag: backend_tag.as_str().to_string(),
            dimension: dimension.to_string(),
        }
    }
}

/// The reboot-survivable resume ledger. `done` is read once at startup to skip
/// already-persisted work; `mark` is called the instant a dimension's rows are
/// written so the ledger is always at least as advanced as the DB.
#[async_trait::async_trait]
pub trait Checkpoint: Send + Sync {
    /// All completed keys (read at startup to drive resume).
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String>;
    /// Record one completed (model, backend, dimension). Durable BEFORE the
    /// runner moves on, so a crash right after never re-does the dimension.
    async fn mark(&self, key: &CheckpointKey) -> Result<(), String>;
}

/// File-backed checkpoint on the RELIABLE NAS staging dir
/// ([`config::intake_checkpoint_path`]). Small-file IO, append-on-mark. Survives
/// reboots because the GGUFs (the expensive thing) are staged separately and the
/// per-dimension rows are already in Postgres.
pub struct FileCheckpoint {
    path: String,
}

impl FileCheckpoint {
    /// Resolve the checkpoint path from the NAS staging dir. `Err` (not a guess)
    /// when staging is unconfigured.
    pub fn open() -> Result<Self, ToolError> {
        let path = config::intake_checkpoint_path().ok_or_else(|| {
            ToolError::NotConfigured(
                "INTAKE_STAGING_DIR not set — the resume checkpoint needs the reliable NAS staging dir"
                    .into(),
            )
        })?;
        Ok(FileCheckpoint { path })
    }

    fn read_all(&self) -> Vec<CheckpointKey> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => s
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<CheckpointKey>(l).ok())
                .collect(),
            Err(_) => Vec::new(), // absent ledger ⇒ fresh run
        }
    }
}

#[async_trait::async_trait]
impl Checkpoint for FileCheckpoint {
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String> {
        Ok(self.read_all().into_iter().collect())
    }

    async fn mark(&self, key: &CheckpointKey) -> Result<(), String> {
        use std::io::Write;
        let line = serde_json::to_string(key).map_err(|e| e.to_string())?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open checkpoint {}: {e}", self.path))?;
        writeln!(f, "{line}").map_err(|e| format!("append checkpoint: {e}"))?;
        Ok(())
    }
}

// ===========================================================================
// Suite driver: smoke + the six dimensions for ONE (model, backend)
// ===========================================================================

/// Drives the bounded smoke + each dimension for one (model, backend). The live
/// impl wires the real dim runners under the P5 backend override; tests inject a
/// deterministic driver. Every method maps failure to a recorded outcome — it
/// MUST NOT panic or abort the run.
#[async_trait::async_trait]
pub trait SuiteDriver: Send + Sync {
    /// Bounded smoke (1 case) for a freshly acquired model on `backend`. `Ok(())`
    /// ⇒ proceed to the full suite; `Err(reason)` ⇒ skip-with-reason (broken
    /// acquisition / model hangs), run continues.
    async fn smoke(&self, model_id: &ModelId, backend: BackendTag, backend_override: &str)
        -> Result<(), String>;

    /// Run ONE dimension for `(model, backend)` and return its storage rows.
    /// `backend_override` is the P5 override string (`"llama-gpu"`|`"ollama"`) the
    /// implementation sets around the pass and clears after. `Err(reason)` ⇒ the
    /// dimension degraded/hung; the runner records the reason and continues to the
    /// next dimension.
    ///
    /// `yarn` is `Some` only when `dimension == dim7_yarn_depth::DIMENSION`
    /// (the runner only ever passes it for a `yarn_capable` nomination's
    /// extra dimension) — every other dimension ignores it.
    async fn run_dimension(
        &self,
        model_id: &ModelId,
        backend: BackendTag,
        backend_override: &str,
        dimension: &str,
        yarn: Option<&super::acquire::YarnConfig>,
    ) -> Result<Vec<DimensionScore>, String>;
}

// ===========================================================================
// Persistence sink (the canonical write path is schema::insert_dimension_score)
// ===========================================================================

/// Where dimension rows are written. The live impl is Postgres via
/// [`schema::insert_dimension_score`]; tests collect rows in memory to prove the
/// incremental-persistence ordering (rows land BEFORE the checkpoint mark).
#[async_trait::async_trait]
pub trait ScoreSink: Send + Sync {
    async fn write(&self, rows: &[DimensionScore]) -> Result<(), String>;
}

/// Live sink: each row through the canonical ASMT-01 insert path, against one run.
pub struct PgScoreSink {
    pool: PgPool,
    run_id: uuid::Uuid,
    /// Memory-model configuration this run measured under (e.g. `dynamic_gtt`
    /// vs the preserved `carveout` baseline). `None` writes SQL `NULL` — the
    /// mem-config-tagging sprint's contract (see [`schema::insert_dimension_score_with_category_and_mem_config`]).
    mem_config: Option<String>,
}

impl PgScoreSink {
    pub fn new(pool: PgPool, run_id: uuid::Uuid, mem_config: Option<String>) -> Self {
        PgScoreSink { pool, run_id, mem_config }
    }
}

#[async_trait::async_trait]
impl ScoreSink for PgScoreSink {
    async fn write(&self, rows: &[DimensionScore]) -> Result<(), String> {
        for row in rows {
            schema::insert_dimension_score_with_category_and_mem_config(
                &self.pool,
                self.run_id,
                row,
                "assistant",
                self.mem_config.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Live fleet store: append-only inserts via the canonical path.
pub struct PgFleetStore {
    pool: PgPool,
    run_id: uuid::Uuid,
    /// See [`PgScoreSink::mem_config`] — same contract, same run.
    mem_config: Option<String>,
}

impl PgFleetStore {
    pub fn new(pool: PgPool, run_id: uuid::Uuid, mem_config: Option<String>) -> Self {
        PgFleetStore { pool, run_id, mem_config }
    }
}

#[async_trait::async_trait]
impl FleetStore for PgFleetStore {
    async fn insert_membership(&self, row: &DimensionScore) -> Result<(), String> {
        // A plain INSERT — never an UPDATE — so a Lumina row can never clobber a
        // Harmony row (see fleet.rs no-clobber invariant).
        schema::insert_dimension_score_with_category_and_mem_config(
            &self.pool,
            self.run_id,
            row,
            "assistant",
            self.mem_config.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())
    }
}

// ===========================================================================
// Per-model / per-run outcome reporting
// ===========================================================================

/// What happened to one nominated model across the whole run.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRunReport {
    pub model_id: ModelId,
    /// Acquisition skip reason, if the model never got past acquisition.
    pub acquisition_skip: Option<String>,
    /// Per-backend results: backend_tag → (dims persisted this run, dims resumed,
    /// per-dimension skip reasons, smoke skip reason).
    pub backends: Vec<BackendRunReport>,
    /// Lumina-fleet rows written for this model (one per surviving backend).
    pub fleet_rows: Vec<DimensionScore>,
}

/// Per-(model, backend) result line.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendRunReport {
    pub backend_tag: BackendTag,
    /// Smoke skip reason; `None` ⇒ smoke passed (or was resumed past).
    pub smoke_skip: Option<String>,
    /// Dimensions persisted in THIS run (excludes resumed ones).
    pub persisted_dims: Vec<String>,
    /// Dimensions skipped because already persisted (resume).
    pub resumed_dims: Vec<String>,
    /// dimension → skip reason for dimensions that degraded/hung this run.
    pub dim_skips: Vec<(String, String)>,
    /// True ⇒ at least one dimension persisted/resumed ⇒ eligible for the Lumina
    /// fleet on this backend.
    pub survived: bool,
}

/// The whole run's report.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub models: Vec<ModelRunReport>,
}

// ===========================================================================
// The orchestrator (trait-driven; hermetic in tests)
// ===========================================================================

/// Run the consolidated suite over `nominations`, using the injected collaborators.
///
/// This is the testable core: `acquirer`, `driver`, `sink`, `checkpoint`, and
/// `fleet_store` are all traits, so the full acquire → smoke → suite → resume →
/// fleet-register flow runs hermetically. [`run`] wires the live impls.
#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    nominations: &Nominations,
    acquirer: &dyn Acquirer,
    driver: &dyn SuiteDriver,
    sink: &dyn ScoreSink,
    checkpoint: &dyn Checkpoint,
    fleet_store: &dyn FleetStore,
) -> Result<RunReport, String> {
    let done = checkpoint.done().await?;
    let mut models = Vec::with_capacity(nominations.nominations.len());

    for nom in &nominations.nominations {
        let model_id = nom.model_id();

        // ── acquire (over-VRAM / broken fetch → skip-with-reason, keep going) ──
        let acq = acquirer.acquire(nom).await;
        if let AcquisitionOutcome::Skipped { reason } = &acq {
            models.push(ModelRunReport {
                model_id,
                acquisition_skip: Some(reason.clone()),
                backends: Vec::new(),
                fleet_rows: Vec::new(),
            });
            continue;
        }

        // ── per backend (the both-hardware passes), P5 override per pass ──
        let mut backend_reports = Vec::new();
        let mut fleet_rows = Vec::new();
        for (backend_tag, override_str) in nom.backend_strategy() {
            let report = run_one_backend(
                nom, &model_id, backend_tag, override_str, driver, sink, checkpoint, &done,
            )
            .await;

            // Register a survivor into the Lumina fleet (append-only; never
            // clobbers an existing Harmony row for this model/backend).
            if report.survived {
                if let Ok(row) = fleet::register_lumina(
                    fleet_store,
                    &model_id,
                    backend_tag,
                    format!("S84 assistant survivor ({})", nom.gfx1151_class.as_str()),
                )
                .await
                {
                    fleet_rows.push(row);
                }
            }
            backend_reports.push(report);
        }

        models.push(ModelRunReport {
            model_id,
            acquisition_skip: None,
            backends: backend_reports,
            fleet_rows,
        });
    }

    Ok(RunReport { models })
}

/// Run the smoke + six dimensions (plus the `yarn_context_depth` seventh, for a
/// `yarn_capable` nomination) for one (model, backend), honoring resume.
#[allow(clippy::too_many_arguments)]
async fn run_one_backend(
    nom: &Nomination,
    model_id: &ModelId,
    backend_tag: BackendTag,
    override_str: &str,
    driver: &dyn SuiteDriver,
    sink: &dyn ScoreSink,
    checkpoint: &dyn Checkpoint,
    done: &BTreeSet<CheckpointKey>,
) -> BackendRunReport {
    let mut persisted = Vec::new();
    let mut resumed = Vec::new();
    let mut dim_skips = Vec::new();

    // ── this (model)'s dimension list: the fixed six, plus yarn_context_depth
    //    when the nomination is yarn_capable with a config to act on ──
    let mut dims: Vec<&str> = SUITE_DIMENSIONS.to_vec();
    if nom.yarn_config().is_some() {
        dims.push(super::dim7_yarn_depth::DIMENSION);
    } else if nom.yarn_misconfigured() {
        // yarn_capable but no yarn config — an authoring error, not silence.
        dim_skips.push((
            super::dim7_yarn_depth::DIMENSION.to_string(),
            "yarn_capable is true but nominations.json has no `yarn` config for this model"
                .to_string(),
        ));
    }

    // If EVERY dimension is already checkpointed, the backend is fully resumed —
    // we can skip the smoke entirely (the model already ran here).
    let all_done = dims
        .iter()
        .all(|d| done.contains(&CheckpointKey::new(model_id, backend_tag, d)));

    if !all_done {
        // ── bounded smoke (1 case) — fail fast on a broken acquisition / hang ──
        if let Err(reason) = driver.smoke(model_id, backend_tag, override_str).await {
            return BackendRunReport {
                backend_tag,
                smoke_skip: Some(reason),
                persisted_dims: persisted,
                resumed_dims: resumed,
                dim_skips,
                survived: false,
            };
        }
    }

    // ── full suite, dimension by dimension, persisting + checkpointing each ──
    for dim in dims {
        let key = CheckpointKey::new(model_id, backend_tag, dim);
        if done.contains(&key) {
            resumed.push(dim.to_string());
            continue; // already persisted on a prior run → resume past it
        }

        let yarn = if dim == super::dim7_yarn_depth::DIMENSION {
            nom.yarn_config()
        } else {
            None
        };

        match driver
            .run_dimension(model_id, backend_tag, override_str, dim, yarn)
            .await
        {
            Ok(rows) => {
                // CRITICAL ORDERING: persist rows FIRST, then mark the checkpoint.
                // If a reboot lands between, the worst case is re-running a
                // dimension whose rows already landed (idempotent enough), never a
                // checkpoint that claims work the DB doesn't have.
                if let Err(e) = sink.write(&rows).await {
                    dim_skips.push((dim.to_string(), format!("persist failed: {e}")));
                    continue;
                }
                if let Err(e) = checkpoint.mark(&key).await {
                    dim_skips.push((dim.to_string(), format!("checkpoint failed: {e}")));
                    continue;
                }
                persisted.push(dim.to_string());
            }
            Err(reason) => {
                // Hanging/OOM/degraded dimension → record reason, keep going.
                dim_skips.push((dim.to_string(), reason));
            }
        }
    }

    let survived = !persisted.is_empty() || !resumed.is_empty();
    BackendRunReport {
        backend_tag,
        smoke_skip: None,
        persisted_dims: persisted,
        resumed_dims: resumed,
        dim_skips,
        survived,
    }
}

// ===========================================================================
// Live entry point — wires the production collaborators
// ===========================================================================

/// Production entry: connect the intake DB, migrate, open a run, load nominations
/// from the NAS staging dir, and run the suite with the live collaborators.
///
/// The live [`LiveSuiteDriver`] wires the REAL dimension runners under the P5
/// backend override; all inference stays on the unified proxy path.
pub async fn run() -> Result<RunReport, ToolError> {
    let pool = schema::get_pool().await?;
    schema::migrate(&pool).await?;
    let run_id = schema::insert_run(&pool).await?;

    let nominations = Nominations::load().map_err(ToolError::NotConfigured)?;
    let mem_config = mem_config_from_env();

    let acquirer = acquire::ShellAcquirer;
    let driver = LiveSuiteDriver::new();
    let sink = PgScoreSink::new(pool.clone(), run_id, mem_config.clone());
    let checkpoint = FileCheckpoint::open()?;
    let fleet_store = PgFleetStore::new(pool.clone(), run_id, mem_config);

    run_with(&nominations, &acquirer, &driver, &sink, &checkpoint, &fleet_store)
        .await
        .map_err(ToolError::Execution)
}

/// Read the memory-model configuration tag from the SAME env var the coder
/// sweep uses (`SWEEP_MEM_CONFIG`) — this describes the physical host's memory
/// config (e.g. `dynamic_gtt` dynamic-GTT pool vs the preserved `carveout`
/// baseline), not a sweep-specific setting, so both sweeps share the knob.
/// A blank value is treated as unset (`None`), never as an empty-string tag.
fn mem_config_from_env() -> Option<String> {
    std::env::var("SWEEP_MEM_CONFIG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Live suite driver: runs the bounded smoke and each real dimension runner under
/// the P5 backend override, exactly like the S83 coder harness. The override is
/// set before the pass and CLEARED after (the override is process-global and
/// intake runs are sequential — see [`crate::intake::infer::set_backend_override`]).
pub struct LiveSuiteDriver {
    client: reqwest::Client,
    timeout: Duration,
    judges: Arc<Vec<Box<dyn super::judges::Judge>>>,
}

impl Default for LiveSuiteDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveSuiteDriver {
    pub fn new() -> Self {
        LiveSuiteDriver {
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(config::judge_timeout_secs()),
            judges: Arc::new(super::judges::CliJudge::panel()),
        }
    }

    /// Set the P5 override, run `f`, then ALWAYS clear it (even on early return).
    /// This is the both-hardware profiling guard the spec requires.
    async fn with_backend<F, Fut, T>(override_str: &str, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        crate::intake::infer::set_backend_override(Some(override_str.to_string()));
        let out = f().await;
        crate::intake::infer::set_backend_override(None);
        out
    }
}

#[async_trait::async_trait]
impl SuiteDriver for LiveSuiteDriver {
    async fn smoke(
        &self,
        model_id: &ModelId,
        _backend: BackendTag,
        override_str: &str,
    ) -> Result<(), String> {
        use super::dim1_conversation::{ConversationModel, LiveModel};
        let client = self.client.clone();
        let model = model_id.as_str().to_string();
        let timeout = self.timeout;
        // One bounded conversational turn through the unified path. A degraded
        // reply (timeout / OOM / hang / empty) → skip-with-reason; the run goes on.
        Self::with_backend(override_str, || async move {
            let live = LiveModel::new(client, model, timeout);
            let reply = live.respond(&[], "Briefly introduce yourself in one sentence.").await;
            if reply.degraded {
                Err(reply
                    .degrade_reason
                    .unwrap_or_else(|| "smoke degraded".to_string()))
            } else {
                Ok(())
            }
        })
        .await
    }

    async fn run_dimension(
        &self,
        model_id: &ModelId,
        backend: BackendTag,
        override_str: &str,
        dimension: &str,
        yarn: Option<&super::acquire::YarnConfig>,
    ) -> Result<Vec<DimensionScore>, String> {
        let client = self.client.clone();
        let timeout = self.timeout;
        let model_name = model_id.as_str().to_string();
        let model_id = model_id.clone();
        let judges = self.judges.clone();
        let yarn = yarn.cloned();

        // Each arm constructs the dimension's LIVE model (unified proxy path),
        // runs the real `run_dimN`, and flattens to rows — all under the P5
        // override so the pass is measured on `backend`.
        Self::with_backend(override_str, || async move {
            use super::*;
            match dimension {
                d if d == dim1_conversation::DIMENSION => {
                    let corpus = dim1_conversation::load_corpus()?;
                    let model =
                        dim1_conversation::LiveModel::new(client, model_name, timeout);
                    let out = dim1_conversation::run_dim1(&model, &judges, &corpus).await;
                    Ok(out.into_dimension_scores(&model_id, backend))
                }
                d if d == dim2_toolchain::DIMENSION => {
                    let corpus = dim2_toolchain::load_corpus()?;
                    let (_found, missing) = dim2_toolchain::load_s83_referenced_explicit_scenarios(
                        &corpus.s83_reference_ids,
                    );
                    let model =
                        dim2_toolchain::LiveToolModel::new(client, model_name, timeout);
                    let out = dim2_toolchain::run_dim2(&model, &corpus, missing).await;
                    Ok(out.into_dimension_scores(&model_id, backend))
                }
                d if d == dim3_memory::DIMENSION => {
                    let corpus = dim3_memory::load_corpus()?;
                    let summarizer = dim3_memory::fixed_summarizer_model();
                    let model = dim3_memory::LiveMemoryModel::new(
                        client,
                        model_name,
                        summarizer.clone(),
                        timeout,
                    );
                    let out = dim3_memory::run_dim3(&model, &corpus, summarizer).await;
                    Ok(out.into_dimension_scores(&model_id, backend))
                }
                d if d == dim4_ocean::DIMENSION => {
                    let corpus = dim4_ocean::load_corpus()?;
                    let model = dim4_ocean::RawModel::new(client, model_name, timeout);
                    let out = dim4_ocean::run_dim4(&model, &judges, &corpus).await;
                    Ok(out.into_dimension_scores(&model_id, backend))
                }
                d if d == dim5_prompted::DIMENSION => {
                    // The REAL 5-layer Lumina prompt (asserted real, not a stub).
                    let system_prompt = dim5_prompted::load_lumina_prompt("lumina")
                        .map_err(|e| format!("load Lumina prompt: {e}"))?;
                    let raw = include_str!("corpora/prompted_pressure.json");
                    let corpus = dim5_prompted::Corpus::from_json(raw)
                        .map_err(|e| format!("dim5 corpus: {e}"))?;
                    let candidate =
                        dim5_prompted::ChordCandidate::new(model_id.clone(), backend);
                    let mut rows = Vec::new();
                    for scenario in &corpus.scenarios {
                        let outcome = dim5_prompted::run_scenario(
                            &candidate,
                            &judges,
                            &system_prompt,
                            scenario,
                        )
                        .await;
                        rows.extend(outcome.into_dimension_scores(&model_id, backend));
                    }
                    Ok(rows)
                }
                d if d == dim6_embeddings::DIMENSION => {
                    let public_raw = include_str!("corpora/embeddings_public.json");
                    let engram_raw = include_str!("corpora/embeddings_engram.json");
                    let public = dim6_embeddings::Corpus::from_json(public_raw).ok();
                    let engram = dim6_embeddings::Corpus::from_json(engram_raw)
                        .map_err(|e| format!("dim6 engram corpus: {e}"))?;
                    let embedder =
                        dim6_embeddings::ChordEmbedder::new(model_id.clone(), backend);
                    let report =
                        dim6_embeddings::run_dim6(&embedder, public.as_ref(), &engram).await;
                    Ok(report.into_dimension_scores())
                }
                d if d == dim7_yarn_depth::DIMENSION => {
                    let cfg = yarn.ok_or_else(|| {
                        "yarn_context_depth dimension requested with no YarnConfig".to_string()
                    })?;
                    let corpus = dim7_yarn_depth::load_corpus()?;
                    let model =
                        dim1_conversation::LiveModel::new(client, model_name, timeout);
                    let out = dim7_yarn_depth::run_yarn_depth(
                        &model,
                        &judges,
                        &corpus,
                        cfg.native_ctx,
                        cfg.extended_ctx,
                    )
                    .await;
                    Ok(out.into_dimension_scores(
                        &model_id,
                        backend,
                        cfg.native_ctx,
                        cfg.extended_ctx,
                    ))
                }
                other => Err(format!("unknown dimension: {other}")),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn suite_dimensions_match_the_six_dim_runners() {
        // Guard: the suite list IS the six dimension constants, in order.
        assert_eq!(SUITE_DIMENSIONS.len(), 6);
        assert_eq!(SUITE_DIMENSIONS[0], super::super::dim1_conversation::DIMENSION);
        assert_eq!(SUITE_DIMENSIONS[5], super::super::dim6_embeddings::DIMENSION);
    }

    // ── mem_config env wiring (mirrors the coder sweep's own test pair) ──
    // #[serial]: both tests mutate the shared SWEEP_MEM_CONFIG process env var.

    use serial_test::serial;

    #[test]
    #[serial]
    fn mem_config_from_env_reads_and_trims_set_value() {
        std::env::set_var("SWEEP_MEM_CONFIG", "  dynamic_gtt  ");
        assert_eq!(mem_config_from_env(), Some("dynamic_gtt".to_string()));
        std::env::remove_var("SWEEP_MEM_CONFIG");
    }

    #[test]
    #[serial]
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

    // ── in-memory collaborators for hermetic orchestration tests ──

    #[derive(Default)]
    struct MemCheckpoint {
        keys: Mutex<Vec<CheckpointKey>>,
    }
    #[async_trait::async_trait]
    impl Checkpoint for MemCheckpoint {
        async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String> {
            Ok(self.keys.lock().unwrap().iter().cloned().collect())
        }
        async fn mark(&self, key: &CheckpointKey) -> Result<(), String> {
            self.keys.lock().unwrap().push(key.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemSink {
        rows: Mutex<Vec<DimensionScore>>,
    }
    #[async_trait::async_trait]
    impl ScoreSink for MemSink {
        async fn write(&self, rows: &[DimensionScore]) -> Result<(), String> {
            self.rows.lock().unwrap().extend_from_slice(rows);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemFleet {
        rows: Mutex<Vec<DimensionScore>>,
    }
    #[async_trait::async_trait]
    impl FleetStore for MemFleet {
        async fn insert_membership(&self, row: &DimensionScore) -> Result<(), String> {
            self.rows.lock().unwrap().push(row.clone());
            Ok(())
        }
    }

    /// Scriptable driver: smoke ok unless model in `smoke_fail`; each dimension
    /// yields one row unless (model,dim) in `dim_fail` (hang/degrade).
    struct ScriptDriver {
        smoke_fail: BTreeSet<String>,
        dim_fail: BTreeSet<(String, String)>,
        smoke_calls: Mutex<Vec<(String, String)>>,
        dim_calls: Mutex<Vec<(String, String, String)>>,
    }
    impl ScriptDriver {
        fn new() -> Self {
            ScriptDriver {
                smoke_fail: BTreeSet::new(),
                dim_fail: BTreeSet::new(),
                smoke_calls: Mutex::new(Vec::new()),
                dim_calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl SuiteDriver for ScriptDriver {
        async fn smoke(
            &self,
            model_id: &ModelId,
            backend: BackendTag,
            _o: &str,
        ) -> Result<(), String> {
            self.smoke_calls
                .lock()
                .unwrap()
                .push((model_id.as_str().to_string(), backend.as_str().to_string()));
            if self.smoke_fail.contains(model_id.as_str()) {
                Err("smoke hang".to_string())
            } else {
                Ok(())
            }
        }
        async fn run_dimension(
            &self,
            model_id: &ModelId,
            backend: BackendTag,
            _o: &str,
            dimension: &str,
            _yarn: Option<&super::acquire::YarnConfig>,
        ) -> Result<Vec<DimensionScore>, String> {
            self.dim_calls.lock().unwrap().push((
                model_id.as_str().to_string(),
                backend.as_str().to_string(),
                dimension.to_string(),
            ));
            if self
                .dim_fail
                .contains(&(model_id.as_str().to_string(), dimension.to_string()))
            {
                return Err("dimension OOM".to_string());
            }
            Ok(vec![DimensionScore {
                model_id: model_id.clone(),
                backend_tag: backend,
                dimension: dimension.to_string(),
                metric: "m".to_string(),
                value: 1.0,
                std_dev: None,
                judge: "deterministic".to_string(),
                low_confidence: false,
                raw_json: None,
            }])
        }
    }

    /// Acquirer that skips ids in `skip` (with a reason), else Ready.
    struct ScriptAcquirer {
        skip: BTreeSet<String>,
    }
    #[async_trait::async_trait]
    impl Acquirer for ScriptAcquirer {
        async fn acquire(&self, nom: &Nomination) -> AcquisitionOutcome {
            if self.skip.contains(&nom.id) {
                AcquisitionOutcome::Skipped {
                    reason: "over VRAM".to_string(),
                }
            } else {
                AcquisitionOutcome::Ready { local_path: None }
            }
        }
    }

    fn noms(json: &str) -> Nominations {
        Nominations::from_json(json).unwrap()
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn full_run_acquire_smoke_suite_fleet_end_to_end() {
        // One confirmed model → both backends → 6 dims each → fleet rows on both.
        let n = noms(
            r#"{"nominations":[{"id":"command-r:35b","size_b":35,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        // 6 dims × 2 backends = 12 rows persisted, 12 checkpoints.
        assert_eq!(sink.rows.lock().unwrap().len(), 12);
        assert_eq!(cp.keys.lock().unwrap().len(), 12);
        // Survived on both backends → 2 Lumina fleet rows.
        assert_eq!(fleet.rows.lock().unwrap().len(), 2);
        for r in fleet.rows.lock().unwrap().iter() {
            assert_eq!(r.dimension, fleet::DIMENSION);
            assert_eq!(r.metric, "lumina");
        }
        let m = &report.models[0];
        assert!(m.acquisition_skip.is_none());
        assert_eq!(m.backends.len(), 2);
        assert!(m.backends.iter().all(|b| b.survived));
    }

    #[test]
    fn yarn_capable_nomination_runs_the_seventh_dimension() {
        // A yarn_capable model with a config gets 7 dims per backend, not 6 —
        // and one of them is genuinely `yarn_context_depth` (S86), not just a
        // count coincidence.
        let n = noms(
            r#"{"nominations":[{"id":"smollm3:3b","size_b":3,"gfx1151_class":"confirmed",
                "acquisition":"ollama_pull","yarn_capable":true,
                "yarn":{"native_ctx":65536,"extended_ctx":131072,"rope_scale":2.0,"yarn_orig_ctx":65536}}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        // 7 dims × 2 backends = 14 rows persisted, 14 checkpoints.
        assert_eq!(sink.rows.lock().unwrap().len(), 14);
        assert_eq!(cp.keys.lock().unwrap().len(), 14);
        assert!(
            driver
                .dim_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, dim)| dim == super::super::dim7_yarn_depth::DIMENSION),
            "the driver must have actually been asked to run yarn_context_depth"
        );
        let m = &report.models[0];
        assert!(m.backends.iter().all(|b| b.dim_skips.is_empty()));
    }

    #[test]
    fn yarn_capable_without_config_is_a_visible_skip_not_silence() {
        // yarn_capable:true with no `yarn` block is an authoring error — it
        // must show up as a dim_skip, not vanish (and must not crash the run).
        let n = noms(
            r#"{"nominations":[{"id":"broken:1b","size_b":1,"gfx1151_class":"confirmed",
                "acquisition":"ollama_pull","yarn_capable":true}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        // Still only the standard 6 dims ran (dim7 never got invoked) ...
        assert!(
            !driver
                .dim_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, dim)| dim == super::super::dim7_yarn_depth::DIMENSION),
            "misconfigured yarn_capable must not invoke the dimension"
        );
        // ... but every backend surfaces the misconfiguration as a dim_skip.
        let m = &report.models[0];
        assert!(m.backends.iter().all(|b| b
            .dim_skips
            .iter()
            .any(|(dim, _)| dim == super::super::dim7_yarn_depth::DIMENSION)));
    }

    #[test]
    fn over_vram_model_skips_with_reason_run_continues() {
        // Command A+ (218B) is acquisition-skipped; the next model still runs.
        let n = noms(
            r#"{"nominations":[
              {"id":"command-a-plus:218b","size_b":218,"gfx1151_class":"experimental","acquisition":"hf_fetch","hf_repo":"x/y"},
              {"id":"phi-4:14b","size_b":14,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}
            ]}"#,
        );
        let acq = ScriptAcquirer {
            skip: ["command-a-plus:218b".to_string()].into_iter().collect(),
        };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        let big = &report.models[0];
        assert_eq!(big.model_id.as_str(), "command-a-plus:218b");
        assert!(big.acquisition_skip.as_ref().unwrap().contains("VRAM"));
        assert!(big.backends.is_empty()); // never profiled
        // The second model fully ran (run continued).
        let small = &report.models[1];
        assert_eq!(small.backends.len(), 2);
        assert!(small.backends.iter().all(|b| b.survived));
    }

    #[test]
    fn smoke_failure_skips_backend_with_reason() {
        let n = noms(
            r#"{"nominations":[{"id":"hangy:32b","size_b":32,"gfx1151_class":"experimental","acquisition":"ollama_pull"}]}"#,
        );
        let mut driver = ScriptDriver::new();
        driver.smoke_fail.insert("hangy:32b".to_string());
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        // No dimensions ran (smoke gate), no rows, no fleet membership.
        assert!(sink.rows.lock().unwrap().is_empty());
        assert!(fleet.rows.lock().unwrap().is_empty());
        for b in &report.models[0].backends {
            assert_eq!(b.smoke_skip.as_deref(), Some("smoke hang"));
            assert!(!b.survived);
        }
    }

    #[test]
    fn hanging_dimension_records_reason_and_continues() {
        let n = noms(
            r#"{"nominations":[{"id":"m:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}]}"#,
        );
        let mut driver = ScriptDriver::new();
        driver
            .dim_fail
            .insert(("m:8b".to_string(), super::super::dim3_memory::DIMENSION.to_string()));
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();
        let b = &report.models[0].backends[0];
        // 5 dims persisted, dim3 recorded as a skip with reason, run continued.
        assert_eq!(b.persisted_dims.len(), 5);
        assert_eq!(b.dim_skips.len(), 1);
        assert_eq!(b.dim_skips[0].0, super::super::dim3_memory::DIMENSION);
        assert!(b.dim_skips[0].1.contains("OOM"));
        // Still a survivor (other dims landed) → fleet row written.
        assert!(b.survived);
        assert_eq!(fleet.rows.lock().unwrap().len(), 1);
    }

    #[test]
    fn resume_skips_already_checkpointed_dimensions() {
        // Pre-seed the checkpoint as if a reboot happened mid-run: gpu pass had
        // dims 1-3 done. Resume must run ONLY dims 4-6 on gpu (+ all of cpu) and
        // must NOT re-smoke a partially-done backend's already-done dims.
        let n = noms(
            r#"{"nominations":[{"id":"m:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let model = ModelId::from("m:8b");
        let cp = MemCheckpoint::default();
        for d in &SUITE_DIMENSIONS[..3] {
            cp.keys
                .lock()
                .unwrap()
                .push(CheckpointKey::new(&model, BackendTag::Gpu, d));
        }
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let fleet = MemFleet::default();

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        let gpu = report.models[0]
            .backends
            .iter()
            .find(|b| b.backend_tag == BackendTag::Gpu)
            .unwrap();
        assert_eq!(gpu.resumed_dims.len(), 3);
        assert_eq!(gpu.persisted_dims.len(), 3); // dims 4,5,6 ran this time
                                                  // Only 3 NEW gpu rows + 6 cpu rows persisted this run = 9.
        assert_eq!(sink.rows.lock().unwrap().len(), 9);
        // The gpu driver was asked to run ONLY the 3 remaining dims (not 1-3).
        let gpu_dim_calls: Vec<_> = driver
            .dim_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, b, _)| b == "gpu")
            .cloned()
            .collect();
        assert_eq!(gpu_dim_calls.len(), 3);
        for (_, _, dim) in &gpu_dim_calls {
            assert!(SUITE_DIMENSIONS[3..].contains(&dim.as_str()));
        }
    }

    #[test]
    fn fully_resumed_backend_skips_smoke_entirely() {
        // All 6 gpu dims already checkpointed → gpu pass is pure resume, no smoke.
        let n = noms(
            r#"{"nominations":[{"id":"m:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}]}"#,
        );
        let model = ModelId::from("m:8b");
        let cp = MemCheckpoint::default();
        for d in SUITE_DIMENSIONS {
            cp.keys
                .lock()
                .unwrap()
                .push(CheckpointKey::new(&model, BackendTag::Gpu, d));
        }
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let fleet = MemFleet::default();

        block(run_with(&n, &acq, &driver, &sink, &cp, &fleet)).unwrap();

        // Smoke was never called for the fully-resumed gpu backend.
        assert!(driver.smoke_calls.lock().unwrap().is_empty());
        // No new rows; still a survivor (resumed) → still gets a fleet row.
        assert!(sink.rows.lock().unwrap().is_empty());
        assert_eq!(fleet.rows.lock().unwrap().len(), 1);
    }
}
