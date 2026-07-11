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
use crate::intake::gpu_authority;

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
    /// S86 max-lock-hold safety valve: `true` ⇒ this backend pass ended
    /// because `gpu_lock.check_max_hold()` failed to REACQUIRE the lock after
    /// releasing it mid-pass (its bounded wait was exhausted) — meaning this
    /// process no longer holds the exclusive GPU lock at all. The CALLER
    /// (`run_with`) MUST treat this as "stop processing this model" and skip
    /// any remaining backend passes rather than running them unlocked — see
    /// `run_with`'s per-backend loop. Distinct from an ordinary `dim_skips`
    /// entry (a single dimension degrading) precisely because it means the
    /// mutual-exclusion invariant this whole module exists to enforce no
    /// longer holds for the rest of this model.
    pub lock_lost: bool,
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
    gpu_lock: &dyn GpuLock,
) -> Result<RunReport, String> {
    let done = checkpoint.done().await?;
    let mut models = Vec::with_capacity(nominations.nominations.len());
    // Whether the PREVIOUS model actually took (and released) the GPU lock —
    // only then must we pause before the next real acquire attempt (see
    // `gpu_authority::INTER_UNIT_RELEASE_PAUSE`'s doc for why the pause must
    // follow a genuine release, not just any loop iteration).
    let mut did_prior_unit = false;

    for nom in &nominations.nominations {
        let model_id = nom.model_id();

        // ── acquire (over-VRAM / broken fetch → skip-with-reason, keep going) ──
        // This is the VRAM/fetch-feasibility check ([`Acquirer`]), NOT the GPU
        // exclusive lock — a model that fails HERE never touches the GPU at
        // all, so it costs nothing to check before ever taking the lock.
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

        // ── S86 fairness: acquire the exclusive GPU lock for THIS model
        //    (both its backend passes) instead of holding one guard for the
        //    whole run — see gpu_authority.rs's "Fairness" section for why. ──
        if did_prior_unit {
            tokio::time::sleep(gpu_lock.release_pause()).await;
        }
        if let Err(e) = gpu_lock.acquire().await {
            tracing::error!(
                "intake_assistant_sweep: could not reacquire the GPU for model={model_id} — \
                 skipping this model this run (resumable next run): {e}"
            );
            models.push(ModelRunReport {
                model_id,
                acquisition_skip: Some(format!("GPU reacquire failed: {e}")),
                backends: Vec::new(),
                fleet_rows: Vec::new(),
            });
            did_prior_unit = false; // never took the lock — no pause owed before the next try
            continue;
        }
        let _release_guard = ReleaseOnDrop { lock: gpu_lock };

        // ── per backend (the both-hardware passes), P5 override per pass ──
        let mut backend_reports = Vec::new();
        let mut fleet_rows = Vec::new();
        let mut lock_lost_mid_model = false;
        for (backend_tag, override_str) in nom.backend_strategy() {
            let report = run_one_backend(
                nom, &model_id, backend_tag, override_str, driver, sink, checkpoint, &done, gpu_lock,
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
            let lock_lost = report.lock_lost;
            backend_reports.push(report);

            // S86 max-lock-hold safety valve, correctness fix (independent
            // review finding): this model's GPU lock is held ONCE across
            // BOTH backend passes (see the acquire above) — if the mid-pass
            // safety valve inside `run_one_backend` failed to REACQUIRE it
            // (its bounded wait exhausted), this process no longer holds the
            // exclusive lock at all. Running the NEXT backend pass here
            // would do so with NO lock held — silently violating the exact
            // mutual-exclusion invariant this whole module exists to
            // enforce. Stop processing further backends for this model; the
            // next MODEL's iteration freshly re-acquires (or waits, bounded)
            // before touching the GPU again, same as any other reacquire
            // failure already does at the model level.
            if lock_lost {
                tracing::error!(
                    "intake_assistant_sweep: GPU lock lost mid-model for {model_id} (backend \
                     {backend_tag}) and could not be reacquired — skipping this model's \
                     remaining backend pass(es) rather than running them without the lock \
                     (resumable next run)"
                );
                lock_lost_mid_model = true;
                break;
            }
        }

        models.push(ModelRunReport {
            model_id,
            acquisition_skip: None,
            backends: backend_reports,
            fleet_rows,
        });
        // If the lock was lost mid-model, no pause is owed before the next
        // model's acquire attempt (there is no lock currently held to have
        // just been released-and-paused-after); otherwise this model's
        // normal end-of-unit release just happened (via `_release_guard`
        // below) and the usual inter-unit pause applies.
        did_prior_unit = !lock_lost_mid_model;
        // `_release_guard` drops here, at the end of this model's scope —
        // AFTER both backend passes' rows are persisted and checkpoints
        // written (`run_one_backend` persists-then-checkpoints internally),
        // never mid-write.
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
    gpu_lock: &dyn GpuLock,
) -> BackendRunReport {
    let mut persisted = Vec::new();
    let mut resumed = Vec::new();
    let mut dim_skips = Vec::new();
    // S86 max-lock-hold safety valve, correctness fix: `true` only when a
    // mid-pass `check_max_hold` reacquire genuinely failed (its bounded wait
    // exhausted) — meaning this process no longer holds the exclusive GPU
    // lock at all. The caller (`run_with`) MUST stop processing any further
    // backend passes for this model when this is set (see `BackendRunReport`'s
    // doc) — this model's lock is acquired ONCE, covering BOTH backend
    // passes, so losing it mid-pass-1 must not let pass-2 run unlocked.
    let mut lock_lost = false;

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
                lock_lost: false,
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

        // S86 max-lock-hold safety valve: checked after EACH dimension (this
        // backend pass's actual unit-of-work granularity) — between discrete
        // `run_dimension` calls, never mid-call — mirroring `code_v2.rs`'s
        // identical per-case check on the coder-sweep side. `Ok(_)` is the
        // overwhelming common case (the valve rarely fires at all, per
        // `gpu_authority`'s max-lock-hold module doc). A reacquire failure
        // aborts the REMAINING dimensions of THIS backend pass only —
        // resumable next run via whatever already persisted/checkpointed
        // above — never silently continuing without the lock.
        if let Err(e) = gpu_lock.check_max_hold().await {
            dim_skips.push((
                dim.to_string(),
                format!("GPU safety-valve reacquire failed after this dimension: {e}"),
            ));
            lock_lost = true;
            break;
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
        lock_lost,
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
// HFIX-09 / S86: bounded acquire-retry backoff + release-between-units
// ===========================================================================
//
// Root cause this section addresses: `intake_coder_sweep` intentionally
// holds its `ExclusiveGuard` for its ENTIRE multi-day run (one guard for the
// whole sweep, by design — see coder_sweep.rs). HFIX-09 stopped the
// assistant sweep's one-shot acquire from crash-looping on refusal (it used
// to `Err` immediately, and with systemd's `Restart=on-failure` +
// `RestartSec≈5min` that meant the unit crash-looped for the ENTIRE duration
// of a coder-sweep run — observed ~2 days straight in production, zero data
// the whole time) by waiting (bounded) instead of failing immediately.
//
// That fixed the crash-loop but NOT the underlying starvation: coder-sweep
// still held the lock for its whole multi-day run, so the assistant sweep's
// bounded wait just... waited, quietly, for as long as coder-sweep ran.
// Confirmed in production: 2+ days with zero `assistant_dimension_score`
// rows for the live `mem_config='dynamic_gtt'` run.
//
// S86 fixes the actual root cause: BOTH sweeps now acquire the exclusive
// lock fresh per unit of work (here: per model, mirroring `run_with`'s own
// loop granularity) and release it at the end of that unit, instead of
// holding one guard for the whole run — see `gpu_authority.rs`'s "Fairness"
// module section for the full design (including why the release pause is
// 90s, not the "1-2s" first considered). `acquire_with_backoff`,
// `AcquireClock`, `RealClock`, `is_live_holder_refusal`, and the shared poll
// constants now live in `gpu_authority.rs` — reused here AND by
// `coder_sweep.rs`, not duplicated.

/// Env var overriding the max total time a bounded GPU-acquire wait will
/// spend retrying a refused acquire before giving up and returning `Err`
/// (whole seconds; non-positive or unparsable values fall back to the
/// default). Applies to BOTH the initial acquire and every per-model
/// reacquire.
pub const ACQUIRE_MAX_WAIT_ENV: &str = "INTAKE_ASSISTANT_ACQUIRE_MAX_WAIT_SECS";

/// Default max wait: 4 hours. In practice a per-model reacquire should only
/// ever wait for coder-sweep's CURRENT (model, backend) pass to finish
/// (observed on the order of tens of minutes per model, per S83/HFIX
/// timings) — this cap is the safety net for a genuinely wedged GPU (e.g. a
/// crashed holder whose lock never clears), not the expected wait.
const ACQUIRE_MAX_WAIT_DEFAULT_SECS: u64 = 4 * 60 * 60;

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

/// Per-unit-of-work GPU lock, injected into [`run_with`] so the
/// acquire-per-model / release-per-model fairness policy (including the S86
/// max-lock-hold safety valve, `check_max_hold`) is unit-testable without a
/// real lock file or GPU — mirrors [`Acquirer`]/[`SuiteDriver`]'s existing
/// trait-injection pattern. The trait itself, and its live implementation,
/// now live in `gpu_authority.rs` (formerly duplicated near-identically here
/// and in `coder_sweep.rs` — consolidated so both sweeps share one
/// implementation of the fairness/safety-valve semantics rather than two
/// copies that could drift apart).
pub use gpu_authority::{GpuLock, LiveGpuLock};

/// RAII: releases `lock` on drop, exactly once — so a checkpoint-mark
/// failure (`?` early return) or a panic mid-unit still releases the GPU
/// rather than leaking it for the rest of this process's life. Deliberately
/// NOT `gpu_authority::ExclusiveGuard` itself: that type owns a `holder`
/// `String` and calls the raw (non-backoff) `release` directly, which is
/// fine, but keeping the release path behind the SAME [`GpuLock`] trait the
/// acquire path uses keeps both sides of one unit's lifecycle mockable
/// through one seam in tests.
struct ReleaseOnDrop<'a> {
    lock: &'a dyn GpuLock,
}

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.lock.release();
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

    // S86: exclusive GPU use is now claimed PER MODEL (see `run_with`'s loop),
    // not once for this whole multi-hour run — HFIX-08/HFIX-09's one-shot,
    // whole-run guard fixed the crash-loop (a refusal no longer fails `run()`
    // immediately; it waits, bounded) but NOT the underlying starvation:
    // `intake_coder_sweep` legitimately holds ITS guard for its entire
    // multi-day run, so a whole-run guard here just waited quietly for as
    // long as coder-sweep ran, producing zero data for 2+ days straight in
    // production. `LiveGpuLock` (below) reuses the exact same
    // `gpu_authority::acquire_with_backoff` bounded-wait for every per-model
    // (re)acquire.
    let gpu_lock = LiveGpuLock::new(GPU_HOLDER, acquire_max_wait());

    let acquirer = acquire::ShellAcquirer;
    let driver = LiveSuiteDriver::new();
    let sink = PgScoreSink::new(pool.clone(), run_id, mem_config.clone());
    let checkpoint = FileCheckpoint::open()?;
    let fleet_store = PgFleetStore::new(pool.clone(), run_id, mem_config);

    run_with(&nominations, &acquirer, &driver, &sink, &checkpoint, &fleet_store, &gpu_lock)
        .await
        .map_err(ToolError::Execution)
}

// ---------------------------------------------------------------------------
// Assistant sub-runner under the unified MINT harness (MINT2-04)
// ---------------------------------------------------------------------------

/// The assistant sub-runner registered into [`crate::intake::MintHarness`]. A
/// thin adapter over the existing consolidated [`run`] orchestrator: the seven
/// assistant dimensions and their measurement are unchanged — this only routes
/// the assistant sweep through the one shared harness surface, and owns the
/// end-of-run summary the standalone `intake_assistant_sweep` binary used to
/// print (so the binary is now merely `MintHarness::run(RunKind::Assistant)`).
pub struct AssistantSweepRunner;

impl AssistantSweepRunner {
    pub fn new() -> Self {
        AssistantSweepRunner
    }
}

impl Default for AssistantSweepRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::intake::SweepRunner for AssistantSweepRunner {
    fn kind(&self) -> crate::intake::RunKind {
        crate::intake::RunKind::Assistant
    }

    async fn run(&self) -> std::process::ExitCode {
        // Binary-specific orchestration moved here from the old binary `main`:
        // run the consolidated suite, then summarize the per-model report.
        match run().await {
            Ok(report) => {
                let total = report.models.len();
                let profiled = report
                    .models
                    .iter()
                    .filter(|m| {
                        m.acquisition_skip.is_none() && m.backends.iter().any(|b| b.survived)
                    })
                    .count();
                let skipped = report
                    .models
                    .iter()
                    .filter(|m| m.acquisition_skip.is_some())
                    .count();
                eprintln!(
                    "assistant sweep complete: {profiled}/{total} models profiled, \
                     {skipped} acquisition-skipped (scores persisted to the intake DB)"
                );
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                // Genericized — the error type already avoids infra leakage (S77).
                eprintln!("assistant sweep did not complete: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }
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
        // `enable_time()`: S86's `run_with` uses `tokio::time::sleep` for the
        // inter-unit release pause (the live `GpuLock` impl uses 90s;
        // `NoopGpuLock`/`ScriptGpuLock` return `Duration::ZERO`, but the
        // sleep call itself still needs a timer-enabled runtime to resolve).
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    // ── S86: GpuLock fakes (mock the lock, never a real lock file / GPU) ──

    /// Grants immediately, zero pause — for tests exercising `run_with`'s
    /// acquisition/backend/checkpoint orchestration, where the GPU-lock
    /// fairness mechanism itself is not under test (that has its own
    /// dedicated tests below, plus `gpu_authority.rs`'s alternation
    /// simulation).
    struct NoopGpuLock;
    #[async_trait::async_trait]
    impl GpuLock for NoopGpuLock {
        async fn acquire(&self) -> Result<(), String> {
            Ok(())
        }
        fn release(&self) {}
        fn release_pause(&self) -> Duration {
            Duration::ZERO
        }
        async fn check_max_hold(&self) -> Result<bool, String> {
            Ok(false)
        }
    }

    /// Scriptable GpuLock: counts acquire/release/pause calls, and can be
    /// told to refuse specific (1-based) acquire attempts — so tests can
    /// assert `run_with` acquires exactly ONCE PER MODEL (not per backend,
    /// not once for the whole run), pauses only BETWEEN models (never
    /// before the first), and treats a reacquire failure as a per-model
    /// skip that does not abort the rest of the fleet.
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
        fn release_pause(&self) -> Duration {
            *self.pause_calls.lock().unwrap() += 1;
            Duration::ZERO
        }
        async fn check_max_hold(&self) -> Result<bool, String> {
            Ok(false)
        }
    }

    // ── S86 max-lock-hold safety valve: wiring tests ────────────────────────
    //
    // The safety valve's own trigger-timing/reacquire-correctness logic is
    // fully unit-tested (with a fake clock) in `gpu_authority.rs`. These
    // tests cover the OTHER half: does `run_one_backend`'s per-dimension loop
    // correctly call `check_max_hold()` after each dimension, does a firing
    // (or failing) valve leave every OTHER dimension/backend unaffected, and
    // does a mid-loop reacquire failure abort only the REMAINING dimensions
    // of the CURRENT backend pass (recorded as a diagnosable skip), not the
    // whole model or fleet.

    /// A `GpuLock` whose `check_max_hold` fires (`Ok(true)`) or fails
    /// (`Err`) on a scripted call number, `Ok(false)` otherwise — mirrors
    /// `coder_sweep.rs::tests::ScriptMaxHoldLock`.
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
        fn release_pause(&self) -> Duration {
            Duration::ZERO
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
    fn well_behaved_model_never_triggers_the_dimension_level_safety_valve() {
        // 6 dims × 2 backends = 12 `check_max_hold` calls, never crossing the
        // (scripted-absent) threshold — zero valve activity for the common
        // case, and every dimension still persists normally.
        let n = noms(
            r#"{"nominations":[{"id":"command-r:35b","size_b":35,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptMaxHoldLock::default(); // never fires, never fails

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(sink.rows.lock().unwrap().len(), 12);
        assert_eq!(driver.dim_calls.lock().unwrap().len(), 12, "no dimension skipped");
        assert!(report.models[0].backends.iter().all(|b| b.survived));
        // Only the ordinary per-model acquire/release (1 model → 1 acquire, 1
        // release at model-end) — the safety valve never fired.
        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 1);
        assert_eq!(*gpu.release_calls.lock().unwrap(), 1);
    }

    #[test]
    fn slow_unreliable_model_triggers_the_valve_once_and_all_dimensions_still_run() {
        // The valve fires on the 3rd of 12 total `check_max_hold` calls (mid
        // first backend's dimension loop) — every dimension, on BOTH
        // backends, must still be attempted; nothing is skipped/duplicated
        // because the valve fired.
        let n = noms(
            r#"{"nominations":[{"id":"unreliable:32b","size_b":32,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptMaxHoldLock { fire_on_call: Some(3), ..Default::default() };

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(sink.rows.lock().unwrap().len(), 12, "every dimension on both backends still ran");
        assert_eq!(driver.dim_calls.lock().unwrap().len(), 12);
        assert!(report.models[0].backends.iter().all(|b| b.survived));
        // 1 ordinary acquire/release (model-level) + 1 extra pair from the
        // valve firing once.
        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 2);
        assert_eq!(*gpu.release_calls.lock().unwrap(), 2);
    }

    #[test]
    fn mid_dimension_reacquire_failure_aborts_the_rest_of_this_model_not_just_this_backend() {
        // Fails on the 2nd `check_max_hold` call (mid first backend's loop).
        //
        // CORRECTNESS FIX (found independently by both reviewers of this
        // safety valve): this model's GPU lock is acquired ONCE, covering
        // BOTH backend passes (`run_with` acquires per-MODEL, not
        // per-backend — see its own doc comment). If the mid-pass reacquire
        // genuinely fails, this process no longer holds the exclusive lock
        // AT ALL — so the model's SECOND backend pass must NOT run (it would
        // run unlocked, silently violating the exact mutual-exclusion
        // invariant this whole module exists to enforce). An earlier version
        // of this test asserted the opposite ("second backend still runs
        // normally") — that was the bug, not a verified-safe behavior.
        let n = noms(
            r#"{"nominations":[{"id":"wedged:70b","size_b":70,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptMaxHoldLock { fail_on_call: Some(2), ..Default::default() };

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        let backends = &report.models[0].backends;
        // Only the FIRST backend was ever attempted — the second is skipped
        // entirely, never touching the driver.
        assert_eq!(backends.len(), 1, "the second backend pass must be SKIPPED, not run unlocked");
        let aborted = &backends[0];
        assert!(aborted.lock_lost, "must be flagged as having lost the lock");
        // Aborted after its 2nd dimension — only 2 persisted, one dim_skip
        // carrying the safety-valve failure reason.
        assert_eq!(aborted.persisted_dims.len(), 2);
        assert!(
            aborted
                .dim_skips
                .iter()
                .any(|(_, reason)| reason.contains("GPU safety-valve reacquire failed")),
            "got: {:?}",
            aborted.dim_skips
        );
        // The driver was NEVER asked to run anything for the second backend —
        // proof this isn't just an empty report, the pass genuinely never started.
        assert!(
            driver.dim_calls.lock().unwrap().iter().all(|(_, b, _)| b == "gpu"),
            "no 'cpu' backend dimension call should have happened: {:?}",
            driver.dim_calls.lock().unwrap()
        );
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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();
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

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

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

        block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &NoopGpuLock)).unwrap();

        // Smoke was never called for the fully-resumed gpu backend.
        assert!(driver.smoke_calls.lock().unwrap().is_empty());
        // No new rows; still a survivor (resumed) → still gets a fleet row.
        assert!(sink.rows.lock().unwrap().is_empty());
        assert_eq!(fleet.rows.lock().unwrap().len(), 1);
    }

    // ── S86: GPU lock acquired/released PER MODEL, not once per whole run ──

    #[test]
    fn gpu_lock_acquired_and_released_exactly_once_per_model_not_per_backend() {
        // One model with BOTH backends (gpu + cpu) must take the GPU lock
        // exactly once, covering both backend passes — not once per backend,
        // and not held for the (in this test, single-model) whole run either;
        // the two are indistinguishable with one model, so see the two-model
        // test below for that distinction.
        let n = noms(
            r#"{"nominations":[{"id":"command-r:35b","size_b":35,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptGpuLock::default();

        block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 1);
        assert_eq!(*gpu.release_calls.lock().unwrap(), 1);
        // Both backend passes actually ran (proves the lock covered both).
        assert_eq!(driver.smoke_calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn gpu_lock_pauses_between_models_but_never_before_the_first() {
        let n = noms(
            r#"{"nominations":[
                {"id":"a:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]},
                {"id":"b:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}
            ]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptGpuLock::default();

        block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 2, "one acquire per model");
        assert_eq!(*gpu.release_calls.lock().unwrap(), 2, "one release per model");
        assert_eq!(
            *gpu.pause_calls.lock().unwrap(),
            1,
            "pause happens BETWEEN the two models' units of work, never before the first"
        );
    }

    #[test]
    fn gpu_lock_reacquire_failure_is_a_recorded_skip_and_the_fleet_continues() {
        // First model's acquire is scripted to be refused (as if the OTHER
        // sweep's own unit is still running and this side's bounded backoff
        // gave up). This must NOT abort the whole multi-model run — it must
        // be recorded as this model's acquisition_skip (mirroring the
        // existing over-VRAM skip path) and the fleet must continue to the
        // next model, which succeeds normally.
        let n = noms(
            r#"{"nominations":[
                {"id":"stuck:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]},
                {"id":"fine:8b","size_b":8,"gfx1151_class":"confirmed","acquisition":"ollama_pull","backends":["gpu"]}
            ]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::new() };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptGpuLock::failing_on(1); // refuse the very first acquire

        let report = block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(report.models.len(), 2);
        let stuck = &report.models[0];
        assert_eq!(stuck.model_id.as_str(), "stuck:8b");
        assert!(
            stuck.acquisition_skip.as_deref().unwrap_or("").contains("GPU reacquire failed"),
            "got: {:?}",
            stuck.acquisition_skip
        );
        assert!(stuck.backends.is_empty(), "a GPU-refused model must never reach the driver");

        let fine = &report.models[1];
        assert_eq!(fine.model_id.as_str(), "fine:8b");
        assert!(fine.acquisition_skip.is_none());
        assert!(!fine.backends.is_empty(), "the NEXT model must still run normally — no fatal abort");

        // The driver was only ever asked about the surviving model.
        assert_eq!(driver.smoke_calls.lock().unwrap().len(), 1);
        assert_eq!(driver.smoke_calls.lock().unwrap()[0].0, "fine:8b");
    }

    #[test]
    fn gpu_lock_never_acquired_for_a_vram_skipped_model() {
        // A model skipped by the (unrelated) VRAM/fetch Acquirer must never
        // touch the GPU lock at all — no needless acquire/release churn for
        // work that was never going to happen.
        let n = noms(
            r#"{"nominations":[{"id":"huge:400b","size_b":400,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
        );
        let acq = ScriptAcquirer { skip: BTreeSet::from(["huge:400b".to_string()]) };
        let driver = ScriptDriver::new();
        let sink = MemSink::default();
        let cp = MemCheckpoint::default();
        let fleet = MemFleet::default();
        let gpu = ScriptGpuLock::default();

        block(run_with(&n, &acq, &driver, &sink, &cp, &fleet, &gpu)).unwrap();

        assert_eq!(*gpu.acquire_calls.lock().unwrap(), 0);
        assert_eq!(*gpu.release_calls.lock().unwrap(), 0);
        assert_eq!(*gpu.pause_calls.lock().unwrap(), 0);
    }
}
