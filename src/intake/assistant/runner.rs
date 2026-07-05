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
use crate::intake::gpu_authority::{self, GpuMode};

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
///
/// Wraps the generic [`crate::intake::checkpoint::FileCheckpoint`] (MINT Phase
/// 2 item 1) — this type now just adapts that shared, key-agnostic file
/// ledger onto the async [`Checkpoint`] trait this module's callers expect;
/// the on-disk format and every read/append semantic are unchanged.
pub struct FileCheckpoint {
    inner: crate::intake::checkpoint::FileCheckpoint<CheckpointKey>,
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
        Ok(FileCheckpoint {
            inner: crate::intake::checkpoint::FileCheckpoint::at(path),
        })
    }
}

#[async_trait::async_trait]
impl Checkpoint for FileCheckpoint {
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String> {
        Ok(self.inner.done())
    }

    async fn mark(&self, key: &CheckpointKey) -> Result<(), String> {
        self.inner.mark(key)
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

/// GPU-authority holder label this suite acquires under (see
/// [`gpu_authority::ExclusiveGuard`]). `pub` so `mint`'s dispatcher can
/// pre-acquire under the IDENTICAL label before calling [`run`] (MINT Phase 2
/// item 7) — see [`crate::intake::coder_sweep::GPU_HOLDER`]'s doc comment for
/// why the label must match exactly, not just be "some guard for this
/// subcommand".
pub const GPU_HOLDER: &str = "intake_assistant_sweep";

// ===========================================================================
// HFIX-09: bounded acquire-retry backoff
// ===========================================================================
//
// Root cause this section fixes: `intake_coder_sweep` intentionally holds its
// `ExclusiveGuard` for its ENTIRE multi-day run (one guard for the whole
// sweep, by design — see coder_sweep.rs). Before this fix, the assistant
// sweep's one-shot `ExclusiveGuard::acquire()` above would see that refusal,
// return `Err` immediately, and `intake_assistant_sweep`'s `main()` would
// exit `FAILURE`. With systemd's `Restart=on-failure` + a ~5min `RestartSec`,
// that meant the assistant-sweep unit crash-looped every ~5 minutes for the
// ENTIRE duration of a coder-sweep run (observed: ~2 days straight in
// production), producing zero data the whole time.
//
// The fix is caller-side only: instead of failing on the first refusal, poll
// for the GPU to free up, bounded by a max total wait so systemd's restart
// cycle remains the ultimate safety net (just at a far lower frequency, not
// every 5 minutes forever) if something is truly wedged rather than merely
// "coder-sweep is mid-model". `gpu_authority.rs` itself is untouched.

/// Env var overriding the max total time [`acquire_with_backoff`] will spend
/// retrying a refused GPU acquire before giving up and returning `Err` (whole
/// seconds; non-positive or unparsable values fall back to the default).
pub const ACQUIRE_MAX_WAIT_ENV: &str = "INTAKE_ASSISTANT_ACQUIRE_MAX_WAIT_SECS";

/// Default max wait: 4 hours. `intake_coder_sweep` holds ONE guard for its
/// ENTIRE multi-day run (all models, by design — see coder_sweep.rs), so
/// this cap does NOT guarantee the assistant sweep gets in during a single
/// wait; it bounds each systemd restart cycle. The practical effect: instead
/// of `intake_assistant_sweep`'s unit crash-looping every ~5 minutes
/// (`RestartSec`) for the ENTIRE multi-day duration of a coder-sweep run, it
/// now retries silently every `ACQUIRE_POLL_INTERVAL` and only exits/restarts
/// roughly every 4 hours — a large reduction in restart frequency and log
/// noise, not a guarantee of getting in within 4 hours. 4h is long enough to
/// outlast a single coder-sweep model's acquisition + full ASMT-02..07 suite
/// run (observed on the order of tens of minutes per model, per S83/HFIX
/// timings) without waiting so long that a genuinely wedged GPU (e.g. a
/// crashed holder whose lock never clears) goes unreported for the better
/// part of a day.
const ACQUIRE_MAX_WAIT_DEFAULT_SECS: u64 = 4 * 60 * 60;

/// How often to retry a refused acquire.
const ACQUIRE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often to re-log while still waiting, so a long wait is observable in
/// the log file / journalctl without spamming a line every poll (60s).
const ACQUIRE_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Read [`ACQUIRE_MAX_WAIT_ENV`], falling back to
/// [`ACQUIRE_MAX_WAIT_DEFAULT_SECS`] when unset, unparsable, or non-positive.
fn acquire_max_wait() -> Duration {
    std::env::var(ACQUIRE_MAX_WAIT_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(ACQUIRE_MAX_WAIT_DEFAULT_SECS))
}

/// Injectable clock so [`acquire_with_backoff`] is unit-testable without
/// real time passing. Production uses [`RealClock`] (real `Instant` +
/// `tokio::time::sleep`); tests use a fake that advances a virtual clock
/// instantly.
#[async_trait::async_trait]
pub(crate) trait AcquireClock: Send + Sync {
    fn now(&self) -> std::time::Instant;
    async fn sleep(&self, dur: Duration);
}

pub(crate) struct RealClock;

#[async_trait::async_trait]
impl AcquireClock for RealClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Acquire via `try_acquire`, retrying with backoff instead of failing
/// immediately when it returns `Err` (e.g. the GPU is exclusively held by
/// another sweep) — bounded by `max_wait`. Returns the same `Err` shape a
/// one-shot acquire would once the cap is hit, so systemd's
/// `Restart=on-failure` remains the ultimate safety net, just at a much
/// lower frequency than every retry.
///
/// Only retry a refusal that looks like "someone else currently, actively
/// holds the exclusive lock" (the local lock file per [`gpu_authority`]'s
/// `is_blocked` path, OR Chord's own remote lock via `ChordCall::Held`) —
/// the actual coder-sweep-is-mid-run scenario this backoff exists for. Both
/// of those refusal paths return `Err` BEFORE `gpu_authority::acquire()`
/// stops any services, so retrying them is side-effect-free.
///
/// Deliberately NOT retried: a misconfigured `CHORD_JWT` (`Unauthorized`), a
/// generic Chord/network failure (`Failed`), or a `systemctl`/lock-file-write
/// failure inside `acquire()` — those are NOT "someone else has it right
/// now," they're a broken acquire, and `acquire()` stops
/// `policy.stop_services` (e.g. `lemonade-coder.service`) BEFORE it can fail
/// on those paths. Retrying one of those every `poll_interval` for up to
/// `max_wait` would repeatedly stop/restart a production serving unit for
/// hours on a persistent, non-transient error instead of failing fast and
/// visibly the way the pre-HFIX-09 one-shot acquire did. So those fail
/// immediately, same as before this change — systemd's crash-loop remains
/// the (louder, faster) safety net for a genuinely broken acquire, while the
/// bounded wait only kicks in for the expected "coder-sweep has it" case.
///
/// Matching is by substring against `gpu_authority`'s current error text —
/// deliberately fail-CLOSED (an unrecognized message is treated as
/// NON-retryable) so a message-format drift in `gpu_authority.rs` degrades to
/// "fail fast, like before," never to "silently retry forever."
pub(crate) fn is_live_holder_refusal(err: &str) -> bool {
    err.contains("held exclusively by")
}

/// Generic over the acquired value `T`, the clock, and the retry predicate
/// so this is testable with a fake acquire function, a fake (instant) clock,
/// and an arbitrary `is_retryable` — no real `sleep()` in tests.
pub(crate) async fn acquire_with_backoff<T, F, C>(
    clock: &C,
    mut try_acquire: F,
    is_retryable: impl Fn(&str) -> bool,
    poll_interval: Duration,
    progress_log_interval: Duration,
    max_wait: Duration,
) -> Result<T, String>
where
    F: FnMut() -> Result<T, String>,
    C: AcquireClock,
{
    let start = clock.now();
    let mut waiting = false;
    let mut last_progress_log = start;

    loop {
        match try_acquire() {
            Ok(v) => {
                if waiting {
                    tracing::info!(
                        "intake_assistant_sweep: GPU acquired after waiting {:.0?} for another holder to release it",
                        clock.now().duration_since(start)
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                if !is_retryable(&e) {
                    tracing::error!(
                        "intake_assistant_sweep: GPU acquire failed with a non-transient error \
                         ({e}) — not retrying (this is not the \"another holder has it right \
                         now\" case; waiting would just repeatedly bounce production services)"
                    );
                    return Err(e);
                }
                let elapsed = clock.now().duration_since(start);
                if elapsed >= max_wait {
                    tracing::error!(
                        "intake_assistant_sweep: giving up waiting for the GPU after {:.0?} \
                         (cap {:.0?}); last refusal: {e}",
                        elapsed,
                        max_wait
                    );
                    return Err(format!(
                        "gave up waiting for the GPU after {elapsed:.0?} (cap {max_wait:.0?}): {e}"
                    ));
                }
                if !waiting {
                    waiting = true;
                    last_progress_log = start;
                    tracing::warn!(
                        "intake_assistant_sweep: GPU acquire refused ({e}) — waiting for it to \
                         free up, retrying every {poll_interval:.0?} (giving up after {max_wait:.0?})"
                    );
                } else if clock.now().duration_since(last_progress_log) >= progress_log_interval {
                    last_progress_log = clock.now();
                    tracing::warn!(
                        "intake_assistant_sweep: still waiting for the GPU after {:.0?} ({e})",
                        elapsed
                    );
                }
                clock.sleep(poll_interval).await;
            }
        }
    }
}

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

    // HFIX-08/HFIX-09: proactively claim exclusive GPU use BEFORE running a
    // single model — mirrors intake_coder_sweep.rs exactly (same
    // GpuMode::Exclusive, same holder-label-per-binary convention, same lock
    // file at /run/gpu-authority.lock via gpu_authority::ExclusiveGuard).
    // Prior to HFIX-08, the assistant sweep never checked or acquired this
    // lock at all, so it could — and in production did — fire its own Ollama
    // requests while the coder sweep held exclusive use, evicting the coder
    // sweep's resident model out from under it (and vice versa) under the
    // host's OLLAMA_MAX_LOADED_MODELS=1 policy, causing continuous
    // model-reload thrashing.
    //
    // HFIX-09: a refusal no longer fails `run()` immediately — coder-sweep
    // legitimately holds this guard for its entire multi-day run, so an
    // immediate `Err` here crash-looped the whole unit every ~5 minutes for
    // days. Instead we wait (bounded — see `acquire_with_backoff`) for the
    // other holder to release it. Held for the duration of `run()` via the
    // `_gpu_guard` binding's scope; released on drop (including on early
    // return via `?` below).
    let _gpu_guard = match acquire_with_backoff(
        &RealClock,
        || gpu_authority::ExclusiveGuard::acquire(GpuMode::Exclusive, GPU_HOLDER),
        is_live_holder_refusal,
        ACQUIRE_POLL_INTERVAL,
        ACQUIRE_PROGRESS_LOG_INTERVAL,
        acquire_max_wait(),
    )
    .await
    {
        Ok(g) => g,
        Err(e) => return Err(ToolError::Execution(format!("assistant sweep did not start: {e}"))),
    };

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

    // ── HFIX-09 acquire-retry backoff ──

    /// A fake clock: `now()` returns a virtual instant that only advances
    /// when `sleep()` is called (by the amount requested) — no real time
    /// passes. Also records how many times `sleep` was called and the total
    /// requested sleep duration, so tests can assert on retry counts without
    /// depending on wall-clock timing at all.
    struct FakeClock {
        elapsed: Mutex<Duration>,
        sleep_calls: Mutex<u32>,
    }

    impl FakeClock {
        fn new() -> Self {
            FakeClock { elapsed: Mutex::new(Duration::ZERO), sleep_calls: Mutex::new(0) }
        }

        fn sleep_call_count(&self) -> u32 {
            *self.sleep_calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl AcquireClock for FakeClock {
        fn now(&self) -> std::time::Instant {
            // `Instant` cannot be constructed at an arbitrary offset directly,
            // so anchor a fixed base instant once and add the virtual elapsed
            // duration recorded so far.
            use std::sync::OnceLock;
            static BASE: OnceLock<std::time::Instant> = OnceLock::new();
            let base = *BASE.get_or_init(std::time::Instant::now);
            base + *self.elapsed.lock().unwrap()
        }

        async fn sleep(&self, dur: Duration) {
            *self.sleep_calls.lock().unwrap() += 1;
            *self.elapsed.lock().unwrap() += dur;
            // Deliberately NOT a real sleep — the whole point of the fake
            // clock is that tests run instantly regardless of `dur`.
        }
    }

    #[tokio::test]
    async fn acquire_with_backoff_retries_then_succeeds() {
        let clock = FakeClock::new();
        let attempts = Mutex::new(0u32);
        let result = acquire_with_backoff(
            &clock,
            || {
                let mut n = attempts.lock().unwrap();
                *n += 1;
                if *n < 4 {
                    Err(format!("refused (attempt {n})"))
                } else {
                    Ok(*n)
                }
            },
            |_| true, // treat every refusal as retryable for this test
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600),
        )
        .await;

        assert_eq!(result, Ok(4), "must return the value from the attempt that finally succeeded");
        assert_eq!(*attempts.lock().unwrap(), 4, "must have tried exactly 4 times (3 refusals + 1 success)");
        assert_eq!(
            clock.sleep_call_count(),
            3,
            "must sleep between refusals only, never after a success"
        );
    }

    #[tokio::test]
    async fn acquire_with_backoff_succeeds_immediately_without_sleeping() {
        let clock = FakeClock::new();
        let result: Result<u32, String> = acquire_with_backoff(
            &clock,
            || Ok(42),
            |_| true,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600),
        )
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(clock.sleep_call_count(), 0, "a first-try success must never sleep");
    }

    #[tokio::test]
    async fn acquire_with_backoff_gives_up_after_max_wait_and_stops_retrying() {
        let clock = FakeClock::new();
        let attempts = Mutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("GPU is held exclusively by 'intake_coder_sweep'".to_string())
            },
            is_live_holder_refusal,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(300), // max_wait: 5 poll intervals
        )
        .await;

        assert!(result.is_err(), "must give up rather than retry forever");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("gave up waiting for the GPU"),
            "give-up error must be self-explanatory in a log/journalctl, got: {msg}"
        );
        assert!(
            msg.contains("intake_coder_sweep"),
            "give-up error should carry the last underlying refusal for diagnosis, got: {msg}"
        );

        // With a 60s poll interval and a 300s cap, the loop must terminate
        // (bounded attempts), not spin unboundedly.
        let tries = *attempts.lock().unwrap();
        assert!(tries >= 5 && tries <= 6, "expected roughly max_wait/poll_interval attempts, got {tries}");
    }

    #[tokio::test]
    async fn acquire_with_backoff_never_retries_a_first_try_beyond_max_wait_zero() {
        // A max_wait of 0 means: try once, and if it fails, give up immediately
        // (no sleep at all) — the cap is honored even on the very first refusal.
        let clock = FakeClock::new();
        let attempts = Mutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("refused".to_string())
            },
            |_| true,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::ZERO,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(*attempts.lock().unwrap(), 1, "max_wait=0 must not retry at all");
        assert_eq!(clock.sleep_call_count(), 0);
    }

    #[tokio::test]
    async fn acquire_with_backoff_fails_fast_on_a_non_retryable_error_without_sleeping() {
        // The masking hazard this predicate exists to prevent: a persistent,
        // non-transient acquire failure (e.g. misconfigured CHORD_JWT) must
        // NOT be retried for up to max_wait — gpu_authority::acquire() stops
        // production services (e.g. lemonade-coder.service) before it can
        // fail on that path, so retrying it every poll_interval would bounce
        // that service repeatedly for hours instead of failing immediately.
        let clock = FakeClock::new();
        let attempts = Mutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("chord rejected the GPU-exclusive acquire (401/403) — set CHORD_JWT \
                     to a valid lumina token for this harness host"
                    .to_string())
            },
            is_live_holder_refusal,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600 * 4),
        )
        .await;

        assert!(result.is_err(), "a non-retryable refusal must still fail");
        assert_eq!(
            *attempts.lock().unwrap(),
            1,
            "must try exactly once and give up immediately, not retry a non-transient error"
        );
        assert_eq!(
            clock.sleep_call_count(),
            0,
            "must never sleep before failing fast on a non-retryable error"
        );
        assert!(
            result.unwrap_err().contains("CHORD_JWT"),
            "the original error detail must be preserved for diagnosis"
        );
    }

    #[test]
    fn is_live_holder_refusal_recognizes_both_local_and_chord_held_messages() {
        // These are gpu_authority.rs's ACTUAL current error strings (local
        // lock block, and Chord's remote `Held` refusal) — if that wording
        // ever drifts, this test should be updated alongside it so the
        // predicate keeps recognizing the retryable case rather than
        // silently falling back to "fail fast" for the expected scenario.
        assert!(is_live_holder_refusal(
            "GPU is held exclusively by 'intake_coder_sweep' (pid 123, mode exclusive, \
             since epoch 100) — refusing to acquire for 'intake_assistant_sweep'"
        ));
        assert!(is_live_holder_refusal(
            "chord reports the GPU is already held exclusively by 'intake_coder_sweep' \
             — refusing to start"
        ));
    }

    #[test]
    fn is_live_holder_refusal_rejects_non_transient_errors() {
        assert!(!is_live_holder_refusal(
            "chord rejected the GPU-exclusive acquire (401/403) — set CHORD_JWT to a valid \
             lumina token for this harness host"
        ));
        assert!(!is_live_holder_refusal("chord GPU-exclusive acquire failed: connection reset"));
        assert!(!is_live_holder_refusal("failed to write GPU lock file: permission denied"));
        assert!(!is_live_holder_refusal("some entirely unrecognized message"));
    }

    #[test]
    #[serial]
    fn acquire_max_wait_defaults_when_env_unset_or_invalid() {
        let _guard = EnvVarGuard::unset(ACQUIRE_MAX_WAIT_ENV);
        assert_eq!(acquire_max_wait(), Duration::from_secs(ACQUIRE_MAX_WAIT_DEFAULT_SECS));
    }

    #[test]
    #[serial]
    fn acquire_max_wait_defaults_on_non_positive_or_unparsable() {
        {
            let _guard = EnvVarGuard::set(ACQUIRE_MAX_WAIT_ENV, "0");
            assert_eq!(acquire_max_wait(), Duration::from_secs(ACQUIRE_MAX_WAIT_DEFAULT_SECS));
        }
        {
            let _guard = EnvVarGuard::set(ACQUIRE_MAX_WAIT_ENV, "not-a-number");
            assert_eq!(acquire_max_wait(), Duration::from_secs(ACQUIRE_MAX_WAIT_DEFAULT_SECS));
        }
        {
            let _guard = EnvVarGuard::set(ACQUIRE_MAX_WAIT_ENV, "-5");
            assert_eq!(acquire_max_wait(), Duration::from_secs(ACQUIRE_MAX_WAIT_DEFAULT_SECS));
        }
    }

    #[test]
    #[serial]
    fn acquire_max_wait_honors_a_valid_override() {
        let _guard = EnvVarGuard::set(ACQUIRE_MAX_WAIT_ENV, "120");
        assert_eq!(acquire_max_wait(), Duration::from_secs(120));
    }

    /// RAII helper so env-var tests can't leak state into other tests even on
    /// panic (mirrors the pattern already used for `SWEEP_MEM_CONFIG` above,
    /// generalized). All three `acquire_max_wait_*` tests mutate the SAME
    /// process env var, so they are `#[serial]` alongside the mem_config pair.
    struct EnvVarGuard {
        key: &'static str,
    }
    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            std::env::set_var(key, value);
            EnvVarGuard { key }
        }
        fn unset(key: &'static str) -> Self {
            std::env::remove_var(key);
            EnvVarGuard { key }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.key);
        }
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
