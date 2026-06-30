//! Serving-profile runner — formalize the manual GPU serving sweep (S85 SRV-02).
//!
//! Turns the hand-run sweep (v1+v2+arch-confirmation) into a repeatable harness:
//! for each model × candidate runtime in tier order, bounded smoke-serve, measure,
//! classify, and persist a [`ServingProfile`](super::ServingProfile) row — keyed
//! on the S83-identical `model_id` + the three-tier `backend_tag`. Reboot-
//! survivable and resumable like the S84 ASMT-09 runner.
//!
//! ## Seed-driven, drift-aware
//! The runner is SEEDED from [`SERVING_SEED_JSON`] — the v2 master table embedded
//! as the expected baseline. A run replays each seeded cell through the (mockable)
//! [`Launcher`], classifies the outcome, and compares the resulting verdict to the
//! seed. Agreement ⇒ "reproduced v2". Divergence ⇒ a [`DriftEntry`] in the run's
//! [`DriftReport`] (e.g. a `build-conditional` model that now serves after a
//! llama.cpp bump). The seed is NOT a fixture the runner trusts blindly — it is
//! the thing the runner *checks against*.
//!
//! ## The hard rules (from the sweep, load-bearing)
//!   - Candidate runtimes in TIER ORDER per model (llama.cpp-rocm → ollama-rocm →
//!     CPU); a fixed standard prompt for comparable tok/s.
//!   - `mmap_flag=0` (`--no-mmap`) for any staged/large llama.cpp cell, RECORDED
//!     in `env_json` (the v2 fix vs v1 false-hangs).
//!   - One model in VRAM at a time: the [`VramGate`] must report release-to-
//!     baseline before the next cell launches.
//!   - Each completed cell is checkpointed to the reliable NAS; a resume skips
//!     exactly the cells already persisted.
//!   - `keep_warm` from `cold_load_s` vs the config threshold.
//!   - Sharded GGUF ⇒ a merge-first note; ollama host-RAM refusal ⇒ `oom-host-ram`
//!     with a llama.cpp `--no-mmap` fallback (NOT a dead end).
//!   - Weights absent ⇒ `acquisition-gap`, no fabricated verdict.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::ToolError;

use super::probes::{
    classify, is_sharded_gguf, requires_no_mmap, vram_released, CellOutcome, CellRequest, Launcher,
    Verdict, VramGate, STANDARD_PROMPT_ID, VRAM_BASELINE_TOLERANCE_BYTES,
};
use super::{ExclusionReason, ModelId, RecheckTrigger, Runtime, ServingBackend, ServingProfile};

/// The v2 master table, embedded as the expected baseline. Parsed into
/// [`SeedCell`]s the runner replays + diffs against.
pub const SERVING_SEED_JSON: &str = include_str!("corpora/serving_seed.json");

/// "Large" weight threshold (GB) above which a llama.cpp cell gets `--no-mmap`.
/// Below the big-MoE floor; above the mid-size local set that loads fine mmap'd.
/// Plain number, not an infra literal.
pub const LARGE_WEIGHT_THRESHOLD_GB: f64 = 30.0;

// ===========================================================================
// The seed (v2 master table)
// ===========================================================================

/// One seeded cell: the known v2 verdict for a (model × backend) the runner
/// reproduces. The `env` block records the expected `mmap_flag` etc.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedCell {
    pub model_id: String,
    pub backend_tag: String,
    pub best_runtime: String,
    pub env: SeedEnv,
    pub tok_s: Option<f64>,
    pub vram_or_ram_peak_gb: Option<f64>,
    pub cold_load_s: Option<f64>,
    pub keep_warm: bool,
    pub fallback_runtime: Option<String>,
    pub exclusion_reason: String,
    pub recheck_trigger: String,
    #[serde(default)]
    pub provenance: Option<String>,
    /// Whether the seed cell's weights are NAS-staged (drives `--no-mmap`).
    #[serde(default)]
    pub staged: bool,
    /// Approximate weight size (GB), if known (drives `--no-mmap` for large).
    #[serde(default)]
    pub weight_gb: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedEnv {
    #[serde(default)]
    pub gfx_override: bool,
    pub mmap_flag: Option<u8>,
    #[serde(default)]
    pub flash_attn: bool,
    pub cpu_lib: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SeedFile {
    keep_warm_threshold_secs: f64,
    cells: Vec<SeedCell>,
}

/// Parse the embedded seed into its cells. The threshold default is overridden by
/// [`config::serving_keep_warm_threshold_secs`] at run time.
pub fn load_seed() -> Result<Vec<SeedCell>, ToolError> {
    let f: SeedFile = serde_json::from_str(SERVING_SEED_JSON)
        .map_err(|e| ToolError::InvalidArgument(format!("parse serving_seed.json: {e}")))?;
    Ok(f.cells)
}

/// The seed's declared keep-warm threshold (the table was measured against it).
/// Used as a fallback when no env override is set.
pub fn seed_keep_warm_threshold() -> Result<f64, ToolError> {
    let f: SeedFile = serde_json::from_str(SERVING_SEED_JSON)
        .map_err(|e| ToolError::InvalidArgument(format!("parse serving_seed.json: {e}")))?;
    Ok(f.keep_warm_threshold_secs)
}

// ===========================================================================
// Resume checkpoint ledger (mirror S84 ASMT-09)
// ===========================================================================

/// One completed cell — the resume key. A run reads the ledger at startup and
/// skips exactly the `(model_id, backend_tag)` pairs already persisted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckpointKey {
    pub model_id: String,
    pub backend_tag: String,
}

impl CheckpointKey {
    pub fn new(model_id: &ModelId, backend: ServingBackend) -> Self {
        CheckpointKey {
            model_id: model_id.as_str().to_string(),
            backend_tag: backend.as_str().to_string(),
        }
    }
}

/// The reboot-survivable resume ledger. `done` is read once at startup; `mark` is
/// durable BEFORE the runner advances to the next cell (so an interruption never
/// loses a persisted row and never double-runs one).
#[async_trait::async_trait]
pub trait Checkpoint: Send + Sync {
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String>;
    async fn mark(&self, key: &CheckpointKey) -> Result<(), String>;
}

/// File-backed checkpoint on the reliable NAS staging dir (append-on-mark,
/// JSON-lines). Survives reboots because the persisted serving rows are in
/// Postgres and the expensive thing (staged GGUFs) is staged separately.
pub struct FileCheckpoint {
    path: String,
}

impl FileCheckpoint {
    /// Resolve the checkpoint path from the NAS staging dir. `Err` (not a guess)
    /// when staging is unconfigured.
    pub fn open() -> Result<Self, ToolError> {
        let dir = config::intake_staging_dir().ok_or_else(|| {
            ToolError::NotConfigured(
                "INTAKE_STAGING_DIR not set — the serving resume checkpoint needs the reliable NAS staging dir"
                    .into(),
            )
        })?;
        Ok(FileCheckpoint {
            path: format!("{}/srv02-checkpoint.json", dir.trim_end_matches('/')),
        })
    }

    fn read_all(&self) -> Vec<CheckpointKey> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => s
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<CheckpointKey>(l).ok())
                .collect(),
            Err(_) => Vec::new(),
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
// Persistence sink (canonical write path is schema::upsert_serving_profile)
// ===========================================================================

/// Where serving rows are written. The live impl is Postgres via
/// [`super::schema::upsert_serving_profile`]; tests collect rows in memory to
/// prove the incremental-persistence ordering (row lands BEFORE the checkpoint
/// mark) and the reproduced verdicts.
#[async_trait::async_trait]
pub trait ProfileSink: Send + Sync {
    async fn write(&self, profile: &ServingProfile) -> Result<(), String>;
}

// ===========================================================================
// VRAM-release gate (the one-model-in-VRAM invariant)
// ===========================================================================

/// Confirm VRAM is released to baseline before the next cell. Returns `Err` if the
/// gate is not satisfied — the runner FAILS SAFE (records a gate violation and
/// skips the next launch) rather than risking two models resident at once.
async fn ensure_vram_released(gate: &dyn VramGate, baseline_bytes: u64) -> Result<(), String> {
    let in_use = gate.vram_in_use_bytes().await?;
    if vram_released(in_use, baseline_bytes, VRAM_BASELINE_TOLERANCE_BYTES) {
        Ok(())
    } else {
        Err(format!(
            "VRAM not released to baseline before next cell: {in_use} bytes in use \
             (baseline {baseline_bytes} + tolerance {VRAM_BASELINE_TOLERANCE_BYTES})"
        ))
    }
}

// ===========================================================================
// Drift report (run verdict vs seed)
// ===========================================================================

/// One model whose measured verdict changed from the seed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriftEntry {
    pub model_id: String,
    pub backend_tag: String,
    /// What the seed said.
    pub seed_exclusion: String,
    /// What this run measured.
    pub run_exclusion: String,
    /// Human-readable summary (e.g. "build-conditional → works after llama.cpp bump").
    pub summary: String,
}

/// The run's drift report. Empty ⇒ the run reproduced the v2 master table exactly.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DriftReport {
    pub entries: Vec<DriftEntry>,
}

impl DriftReport {
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

// ===========================================================================
// Per-cell + per-run reporting
// ===========================================================================

/// What happened to one cell in a run.
#[derive(Debug, Clone, PartialEq)]
pub struct CellReport {
    pub model_id: String,
    pub backend_tag: String,
    /// `true` ⇒ the row was persisted this run; `false` ⇒ skipped (resumed, gate
    /// violation, or acquisition gap).
    pub persisted: bool,
    /// Skip reason when `persisted == false`.
    pub skip_reason: Option<String>,
    /// A note surfaced for this cell (slow-load, merge-first, host-ram fallback).
    pub note: Option<String>,
}

/// The whole run's outcome.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunReport {
    pub cells: Vec<CellReport>,
    pub drift: DriftReport,
    /// Cells resumed from the checkpoint (skipped this run).
    pub resumed: Vec<CheckpointKey>,
}

// ===========================================================================
// The orchestrator (trait-driven; hermetic in tests)
// ===========================================================================

/// Build the [`CellRequest`] for a seeded cell, applying the `mmap_flag=0` rule
/// for staged/large llama.cpp cells. The recorded flag mirrors the seed's env so
/// the env_json round-trips; the rule is RE-DERIVED here (not trusted from the
/// seed) so a new model added to the seed without the flag still gets it.
fn build_request(cell: &SeedCell, load_bound_s: f64) -> Result<CellRequest, ToolError> {
    let backend = ServingBackend::parse(&cell.backend_tag)
        .ok_or_else(|| ToolError::InvalidArgument(format!("bad backend_tag: {}", cell.backend_tag)))?;
    let runtime = Runtime::parse(&cell.best_runtime)
        .ok_or_else(|| ToolError::InvalidArgument(format!("bad best_runtime: {}", cell.best_runtime)))?;

    // Re-derive the no-mmap requirement; for a llama.cpp cell this OVERRIDES the
    // seed's mmap field if the seed under-specified it.
    let needs_no_mmap = requires_no_mmap(backend, cell.staged, cell.weight_gb, LARGE_WEIGHT_THRESHOLD_GB);
    let mmap_flag = match backend {
        ServingBackend::LlamaGpu => {
            if needs_no_mmap {
                Some(0) // --no-mmap, the v2 fix; RECORDED in env_json.
            } else {
                // Small local llama cell: keep the seed's flag (default mmap on).
                cell.env.mmap_flag.or(Some(1))
            }
        }
        // ollama / cpu tiers have no llama mmap path.
        _ => None,
    };

    Ok(CellRequest {
        model_id: ModelId::from(cell.model_id.as_str()),
        backend,
        runtime,
        mmap_flag,
        gfx_override: cell.env.gfx_override,
        cpu_lib: cell.env.cpu_lib.clone(),
        flash_attn: cell.env.flash_attn,
        load_bound_s,
    })
}

/// `keep_warm` from `cold_load_s` vs the threshold (the config rule).
pub fn keep_warm_from_cold_load(cold_load_s: Option<f64>, threshold_s: f64) -> bool {
    cold_load_s.map(|c| c > threshold_s).unwrap_or(false)
}

/// Turn a classified [`Verdict`] + the cell request into the persisted
/// [`ServingProfile`] (with a coherent enum pairing). Returns `None` for cells
/// that persist no row (acquisition gap — recorded as a report note instead).
fn profile_for(
    cell: &SeedCell,
    req: &CellRequest,
    verdict: &Verdict,
    threshold_s: f64,
) -> Option<ServingProfile> {
    let fallback = cell
        .fallback_runtime
        .as_deref()
        .and_then(Runtime::parse);

    let (tok_s, peak, cold, excl, trig) = match verdict {
        Verdict::Works { tok_s, peak_gb, cold_load_s } => (
            Some(*tok_s),
            Some(*peak_gb),
            Some(*cold_load_s),
            ExclusionReason::None,
            RecheckTrigger::None,
        ),
        Verdict::Excluded { reason, trigger } => (None, None, None, *reason, *trigger),
        // A slow load that blew the bound is NOT an exclusion — record it as a
        // served-ish row carrying the measured numbers (the runner attaches a
        // note + keeps it warm). Here we have no measured numbers (it timed out),
        // so we persist an excluded-none row but with a slow-load note upstream.
        Verdict::SlowLoad { .. } => (None, None, None, ExclusionReason::None, RecheckTrigger::None),
        Verdict::AcquisitionGap { .. } => return None,
    };

    let keep_warm = keep_warm_from_cold_load(cold, threshold_s);

    Some(ServingProfile {
        model_id: req.model_id.clone(),
        backend_tag: req.backend,
        best_runtime: req.runtime,
        env_json: req.env_json(),
        tok_s,
        vram_or_ram_peak_gb: peak,
        cold_load_s: cold,
        keep_warm,
        fallback_runtime: fallback,
        exclusion_reason: excl,
        recheck_trigger: trig,
        provenance: cell.provenance.clone(),
    })
}

/// Compare a run verdict against the seed cell and emit a drift entry if they
/// disagree on the exclusion reason (the routing-relevant axis).
fn drift_for(cell: &SeedCell, verdict: &Verdict) -> Option<DriftEntry> {
    let run_excl = match verdict {
        Verdict::Works { .. } => "none",
        Verdict::Excluded { reason, .. } => reason.as_str(),
        Verdict::SlowLoad { .. } => "none", // slow-load is a working-but-warm row
        Verdict::AcquisitionGap { .. } => return None, // no measured verdict to diff
    };
    if run_excl != cell.exclusion_reason {
        Some(DriftEntry {
            model_id: cell.model_id.clone(),
            backend_tag: cell.backend_tag.clone(),
            seed_exclusion: cell.exclusion_reason.clone(),
            run_exclusion: run_excl.to_string(),
            summary: format!(
                "{} on {}: seed='{}' run='{}'",
                cell.model_id, cell.backend_tag, cell.exclusion_reason, run_excl
            ),
        })
    } else {
        None
    }
}

/// The testable core. Replays the seed through the (mockable) launcher + VRAM
/// gate, classifies, persists, checkpoints, and diffs against the seed. No DB,
/// no network, no GPU when the traits are mocked.
///
/// `baseline_vram_bytes` is the host's idle VRAM (read from the gate at startup
/// by the live caller). `threshold_s` is the keep-warm threshold (config).
#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    cells: &[SeedCell],
    threshold_s: f64,
    baseline_vram_bytes: u64,
    launcher: &dyn Launcher,
    sink: &dyn ProfileSink,
    gate: &dyn VramGate,
    checkpoint: &dyn Checkpoint,
) -> Result<RunReport, ToolError> {
    let done = checkpoint
        .done()
        .await
        .map_err(|e| ToolError::Execution(format!("read checkpoint: {e}")))?;

    let mut report = RunReport::default();
    let mut launched_prev = false; // did the previous cell put a model in VRAM?

    for cell in cells {
        let model_id = ModelId::from(cell.model_id.as_str());
        let backend = match ServingBackend::parse(&cell.backend_tag) {
            Some(b) => b,
            None => {
                report.cells.push(CellReport {
                    model_id: cell.model_id.clone(),
                    backend_tag: cell.backend_tag.clone(),
                    persisted: false,
                    skip_reason: Some(format!("unknown backend_tag {}", cell.backend_tag)),
                    note: None,
                });
                continue;
            }
        };
        let key = CheckpointKey::new(&model_id, backend);

        // Resume: skip cells already persisted.
        if done.contains(&key) {
            report.resumed.push(key);
            report.cells.push(CellReport {
                model_id: cell.model_id.clone(),
                backend_tag: cell.backend_tag.clone(),
                persisted: false,
                skip_reason: Some("resumed (already persisted)".into()),
                note: None,
            });
            continue;
        }

        // One-model-in-VRAM gate: before launching, the PREVIOUS cell's VRAM must
        // have been released to baseline. (The very first cell has nothing to
        // release.) Fail safe — record the violation and skip, never double-load.
        if launched_prev {
            if let Err(e) = ensure_vram_released(gate, baseline_vram_bytes).await {
                report.cells.push(CellReport {
                    model_id: cell.model_id.clone(),
                    backend_tag: cell.backend_tag.clone(),
                    persisted: false,
                    skip_reason: Some(format!("VRAM-release gate: {e}")),
                    note: None,
                });
                continue;
            }
        }

        let req = build_request(cell, cell.cold_load_s.map(|c| c.max(900.0)).unwrap_or(900.0))?;

        // Sharded-GGUF merge-first note (the ollama nil-panic-on-shard-1 lesson).
        let mut note: Option<String> = None;
        if backend == ServingBackend::OllamaGpu && is_sharded_gguf(&cell.model_id) {
            note = Some(
                "sharded GGUF: ollama create from shard-1 imports metadata only (nil-panic) — \
                 merge first (llama-gguf-split --merge) before serving"
                    .into(),
            );
        }

        // Launch + measure (mocked in tests). This cell now occupies VRAM unless
        // it never loaded (an exclusion / acquisition gap).
        let outcome = launcher.launch_and_measure(&req).await;
        launched_prev = matches!(outcome, CellOutcome::Served { .. } | CellOutcome::SlowLoadExceedsBound { .. });

        let verdict = classify(&outcome);

        // Surface the load-bearing notes.
        match &verdict {
            Verdict::SlowLoad { bound_s } => {
                note = Some(format!(
                    "slow-load-exceeds-bound ({bound_s}s) — DISTINCT from arch hang; keep-warm candidate"
                ));
            }
            Verdict::Excluded { reason: ExclusionReason::OomHostRam, .. } => {
                // ollama host-RAM refusal: fallback llama.cpp --no-mmap, not a dead end.
                if backend == ServingBackend::OllamaGpu {
                    note = Some(
                        "ollama host-RAM pre-flight refusal → fallback llama.cpp-rocm --no-mmap \
                         (bypasses the host-RAM check)"
                            .into(),
                    );
                }
            }
            _ => {}
        }

        // Drift vs seed.
        if let Some(d) = drift_for(cell, &verdict) {
            report.drift.entries.push(d);
        }

        // Acquisition gap: no fabricated verdict — record provenance, no row.
        if let Verdict::AcquisitionGap { detail } = &verdict {
            report.cells.push(CellReport {
                model_id: cell.model_id.clone(),
                backend_tag: cell.backend_tag.clone(),
                persisted: false,
                skip_reason: Some(format!("acquisition-gap: {detail}")),
                note: cell.provenance.clone(),
            });
            // Still checkpoint it — the cell is "done" (no verdict to retry).
            checkpoint
                .mark(&key)
                .await
                .map_err(|e| ToolError::Execution(format!("checkpoint mark: {e}")))?;
            continue;
        }

        let profile = profile_for(cell, &req, &verdict, threshold_s)
            .expect("non-acquisition-gap verdict always yields a profile");

        // INCREMENTAL PERSISTENCE: the row lands BEFORE the checkpoint mark, so a
        // crash between the two re-runs the cell (idempotent UPSERT), never loses
        // it.
        sink.write(&profile)
            .await
            .map_err(|e| ToolError::Execution(format!("persist serving row: {e}")))?;
        checkpoint
            .mark(&key)
            .await
            .map_err(|e| ToolError::Execution(format!("checkpoint mark: {e}")))?;

        report.cells.push(CellReport {
            model_id: cell.model_id.clone(),
            backend_tag: cell.backend_tag.clone(),
            persisted: true,
            skip_reason: None,
            note,
        });
    }

    Ok(report)
}

// ===========================================================================
// SRV-03: build-conditional recheck mode (advisory, operator-invoked)
// ===========================================================================
//
// A deliberate `--recheck-build-conditional` selector: after a llama.cpp upgrade
// the operator re-tests ONLY the rows the profile flagged as `build-conditional`
// (recheck_trigger = llama-cpp-version-bump) on the llama.cpp-rocm tier — the
// build that might now read those GGUFs. This is NOT a background sweep and NOT
// wired to any version detection; it runs ONLY when [`recheck_with`] is called.
// The profile knows WHAT to re-test (the trigger flag); a human pulls the trigger.

/// Loads the rows the recheck mode operates on. The live impl reads
/// `serving_profile` from Postgres (`SELECT ... WHERE recheck_trigger =
/// 'llama-cpp-version-bump'`); tests supply rows in memory. Decoupled from
/// [`ProfileSink`] (the write side) so the read side is mockable on its own.
#[async_trait::async_trait]
pub trait RecheckSource: Send + Sync {
    /// All serving rows that are candidates for recheck. The selector
    /// [`select_build_conditional`] filters these to the build-conditional set —
    /// the source MAY return every row (the selector is the authority) or
    /// pre-filter; either is correct.
    async fn load_rows(&self) -> Result<Vec<ServingProfile>, String>;
}

/// The selector: pick EXACTLY the build-conditional rows from a row set.
///
/// A row is build-conditional iff `recheck_trigger == LlamaCppVersionBump`. By the
/// [`ServingProfile::validate`] invariant that trigger is coherent ONLY with
/// `exclusion_reason == BuildConditional`, so keying on the trigger picks the
/// build-conditional set and NOTHING else:
///   - permanent-unknown-arch rows (`recheck_trigger = none`) are SKIPPED;
///   - working rows (`exclusion_reason = none`, `recheck_trigger = none`) are
///     SKIPPED;
///   - quant / oom rows (`recheck_trigger = none`) are SKIPPED.
/// This is the negative-test guarantee: only the rows a llama.cpp bump could flip
/// are re-tested.
pub fn select_build_conditional(rows: &[ServingProfile]) -> Vec<ServingProfile> {
    rows.iter()
        .filter(|r| r.recheck_trigger == RecheckTrigger::LlamaCppVersionBump)
        .cloned()
        .collect()
}

/// Rebuild the [`CellRequest`] for a recheck of an existing serving row, on the
/// llama.cpp-rocm tier. The env (gfx override / mmap flag / flash-attn / cpu lib)
/// is recovered from the row's recorded `env_json` so the recheck launches the
/// model exactly as the original sweep did — including the `mmap_flag=0` the
/// staged/large rule recorded.
fn recheck_request(row: &ServingProfile, load_bound_s: f64) -> CellRequest {
    let env: serde_json::Value =
        serde_json::from_str(&row.env_json).unwrap_or_else(|_| serde_json::json!({}));
    let mmap_flag = env
        .get("mmap_flag")
        .and_then(|v| v.as_u64())
        .map(|u| u as u8)
        // Build-conditional rows are llama.cpp cells; a missing flag means "mmap
        // on" (1), never None (which would read as "no mmap path").
        .or(Some(1));
    CellRequest {
        model_id: row.model_id.clone(),
        // Re-tested on the llama.cpp-rocm tier — the build that might now support it.
        backend: ServingBackend::LlamaGpu,
        runtime: Runtime::LlamaCpp,
        mmap_flag,
        gfx_override: env.get("gfx_override").and_then(|v| v.as_bool()).unwrap_or(true),
        cpu_lib: env.get("cpu_lib").and_then(|v| v.as_str()).map(|s| s.to_string()),
        flash_attn: env.get("flash_attn").and_then(|v| v.as_bool()).unwrap_or(false),
        load_bound_s,
    }
}

/// One row's recheck result, surfaced in the [`RecheckReport`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecheckCell {
    pub model_id: String,
    pub backend_tag: String,
    /// `true` ⇒ the build-conditional row FLIPPED to working this recheck.
    pub flipped: bool,
    /// The note recorded for this row (the "still build-incompatible at build X"
    /// line for an unchanged row, or the flip summary).
    pub note: String,
}

/// The whole recheck run's outcome. `flipped` is the set of rows that now work;
/// `drift` carries a line per flip ("rechecked against build X"). Empty `cells`
/// (and the `nothing_to_recheck` flag) ⇒ there were no build-conditional rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecheckReport {
    /// The build id every cell was rechecked against (provenance).
    pub build_id: String,
    pub cells: Vec<RecheckCell>,
    pub drift: DriftReport,
    /// Rows resumed from the checkpoint (already rechecked this run).
    pub resumed: Vec<CheckpointKey>,
    /// `true` ⇒ no build-conditional rows existed; the mode exits cleanly.
    pub nothing_to_recheck: bool,
}

/// The testable recheck core (the `--recheck-build-conditional` selector).
///
/// Loads the candidate rows from `source`, selects EXACTLY the build-conditional
/// ones, and re-runs each on llama.cpp-rocm through the (mockable) `launcher`:
///   - **flip** (the row now SERVES): rewrite it to a working row — best_runtime
///     stays llama.cpp, fallback CLEARED (it serves on its best tier now),
///     `exclusion_reason → none`, `recheck_trigger → none`, measured numbers +
///     keep_warm recorded — persist it, and emit a drift line "rechecked against
///     build X: build-conditional → works".
///   - **no change** (still build-incompatible, or any other non-serving outcome):
///     LEAVE the row's verdict untouched, record "still build-incompatible at
///     build X" as its note. The unchanged row is re-persisted idempotently so the
///     recheck's run_id/updated_at reflect the attempt (verdict bytes identical).
///
/// One model in VRAM at a time (same [`VramGate`] gate as the main runner) and
/// resumable from the same checkpoint ledger. Build id is passed in (the live
/// caller reads [`config::llama_cpp_build_id`]); an empty build id is rejected by
/// [`recheck_build_conditional`] BEFORE this core runs.
#[allow(clippy::too_many_arguments)]
pub async fn recheck_with(
    build_id: &str,
    baseline_vram_bytes: u64,
    threshold_s: f64,
    source: &dyn RecheckSource,
    launcher: &dyn Launcher,
    sink: &dyn ProfileSink,
    gate: &dyn VramGate,
    checkpoint: &dyn Checkpoint,
) -> Result<RecheckReport, ToolError> {
    let all_rows = source
        .load_rows()
        .await
        .map_err(|e| ToolError::Execution(format!("load serving rows for recheck: {e}")))?;
    let targets = select_build_conditional(&all_rows);

    let mut report = RecheckReport {
        build_id: build_id.to_string(),
        ..Default::default()
    };

    // No build-conditional rows ⇒ exit cleanly ("nothing to recheck").
    if targets.is_empty() {
        report.nothing_to_recheck = true;
        return Ok(report);
    }

    let done = checkpoint
        .done()
        .await
        .map_err(|e| ToolError::Execution(format!("read checkpoint: {e}")))?;

    let mut launched_prev = false;

    for row in &targets {
        let backend = row.backend_tag;
        let key = CheckpointKey::new(&row.model_id, backend);

        // Resume: skip rows already rechecked this run.
        if done.contains(&key) {
            report.resumed.push(key);
            continue;
        }

        // One-model-in-VRAM gate, same as the main runner — fail safe.
        if launched_prev {
            if let Err(e) = ensure_vram_released(gate, baseline_vram_bytes).await {
                report.cells.push(RecheckCell {
                    model_id: row.model_id.as_str().to_string(),
                    backend_tag: backend.as_str().to_string(),
                    flipped: false,
                    note: format!("VRAM-release gate: {e}"),
                });
                continue;
            }
        }

        let req = recheck_request(row, row.cold_load_s.map(|c| c.max(900.0)).unwrap_or(900.0));
        let outcome = launcher.launch_and_measure(&req).await;
        launched_prev = matches!(
            outcome,
            CellOutcome::Served { .. } | CellOutcome::SlowLoadExceedsBound { .. }
        );
        let verdict = classify(&outcome);

        let (updated, flipped, note) = match &verdict {
            // FLIP: the build now serves the model.
            Verdict::Works { tok_s, peak_gb, cold_load_s } => {
                let keep_warm = keep_warm_from_cold_load(Some(*cold_load_s), threshold_s);
                let updated = ServingProfile {
                    model_id: row.model_id.clone(),
                    backend_tag: backend,
                    best_runtime: Runtime::LlamaCpp,
                    env_json: req.env_json(),
                    tok_s: Some(*tok_s),
                    vram_or_ram_peak_gb: Some(*peak_gb),
                    cold_load_s: Some(*cold_load_s),
                    keep_warm,
                    // It serves on its best tier now → no fallback needed.
                    fallback_runtime: None,
                    exclusion_reason: ExclusionReason::None,
                    recheck_trigger: RecheckTrigger::None,
                    provenance: Some(format!("flipped build-conditional → works at llama.cpp build {build_id}")),
                };
                let note = format!(
                    "rechecked against build {build_id}: build-conditional → works (flip)"
                );
                (updated, true, note)
            }
            // NO CHANGE: still build-incompatible (or any other non-serving
            // outcome) — leave the row's verdict, record the build it failed at.
            _ => {
                let note = format!("still build-incompatible at build {build_id}");
                // Re-persist the row UNCHANGED (verdict bytes identical) so the
                // attempt is recorded without altering the verdict.
                (row.clone(), false, note)
            }
        };

        // Drift line ONLY on a flip (the verdict changed from the stored row).
        if flipped {
            report.drift.entries.push(DriftEntry {
                model_id: row.model_id.as_str().to_string(),
                backend_tag: backend.as_str().to_string(),
                seed_exclusion: row.exclusion_reason.as_str().to_string(),
                run_exclusion: updated.exclusion_reason.as_str().to_string(),
                summary: format!(
                    "{} on {}: build-conditional → works after llama.cpp build {build_id}",
                    row.model_id.as_str(),
                    backend.as_str()
                ),
            });
        }

        // INCREMENTAL PERSISTENCE: row lands BEFORE the checkpoint mark (resumable,
        // idempotent UPSERT — same invariant as the main runner).
        sink.write(&updated)
            .await
            .map_err(|e| ToolError::Execution(format!("persist recheck row: {e}")))?;
        checkpoint
            .mark(&key)
            .await
            .map_err(|e| ToolError::Execution(format!("checkpoint mark: {e}")))?;

        report.cells.push(RecheckCell {
            model_id: row.model_id.as_str().to_string(),
            backend_tag: backend.as_str().to_string(),
            flipped,
            note,
        });
    }

    Ok(report)
}

/// Live entry for the `--recheck-build-conditional` mode. Resolves the CURRENT
/// llama.cpp build id from [`config::llama_cpp_build_id`] (NotConfigured if unset —
/// never recheck against a guessed/empty build) and the keep-warm threshold, then
/// runs [`recheck_with`] against the live source/launcher/sink/gate/checkpoint.
///
/// This function is the ONLY entry into the recheck mode. It runs when invoked and
/// at no other time — there is NO background scheduler, timer, or llama.cpp
/// version-watcher that calls it (the advisory-not-automated rule). The `cfg(test)`
/// `no_background_recheck_trigger` test asserts that property over this module.
#[allow(clippy::too_many_arguments)]
pub async fn recheck_build_conditional(
    baseline_vram_bytes: u64,
    source: &dyn RecheckSource,
    launcher: &dyn Launcher,
    sink: &dyn ProfileSink,
    gate: &dyn VramGate,
    checkpoint: &dyn Checkpoint,
) -> Result<RecheckReport, ToolError> {
    let build_id = config::llama_cpp_build_id().ok_or_else(|| {
        ToolError::NotConfigured(
            "LLAMA_CPP_BUILD_ID not set — the build-conditional recheck must record \
             the llama.cpp build it tested against; set it to the build you upgraded to"
                .into(),
        )
    })?;
    let threshold_s = resolve_threshold().await;
    recheck_with(
        &build_id,
        baseline_vram_bytes,
        threshold_s,
        source,
        launcher,
        sink,
        gate,
        checkpoint,
    )
    .await
}

/// Live entry: load the seed, resolve the keep-warm threshold + VRAM baseline,
/// and run against the real launcher/sink/gate/checkpoint. (The concrete live
/// `SystemLauncher` / `PgProfileSink` / `SysfsVramGate` land alongside Chord in
/// SRV-04; this signature is the seam the live wiring plugs into.) Documented here
/// so the test core and the live path share one orchestrator.
pub async fn resolve_threshold() -> f64 {
    // Env override wins; else the seed's declared threshold; else the config
    // default — never an infra literal.
    let cfg = config::serving_keep_warm_threshold_secs();
    if cfg != 120.0 {
        return cfg;
    }
    seed_keep_warm_threshold().unwrap_or(cfg)
}

/// The fixed standard prompt id every cell serves under (re-exported for the live
/// launcher + provenance).
pub fn standard_prompt_id() -> &'static str {
    STANDARD_PROMPT_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_parses_and_is_coherent() {
        let cells = load_seed().expect("seed parses");
        assert!(!cells.is_empty());
        // Every seed cell's (exclusion, recheck) pairing must be coherent — turn
        // each into a profile and validate.
        let thresh = seed_keep_warm_threshold().unwrap();
        for cell in &cells {
            let req = build_request(cell, 900.0).expect("request");
            let verdict = seed_verdict(cell);
            if let Some(p) = profile_for(cell, &req, &verdict, thresh) {
                p.validate()
                    .unwrap_or_else(|e| panic!("seed cell {} {} invalid: {e}", cell.model_id, cell.backend_tag));
            }
        }
    }

    #[test]
    fn seed_records_no_mmap_for_staged_large_llama_cells() {
        let cells = load_seed().unwrap();
        // minimax-m2.7 on llama-gpu is large (~77GB) → must record mmap_flag=0.
        let mm = cells
            .iter()
            .find(|c| c.model_id == "minimax-m2.7" && c.backend_tag == "llama-gpu")
            .unwrap();
        let req = build_request(mm, 900.0).unwrap();
        assert_eq!(req.mmap_flag, Some(0));
        let val: serde_json::Value = serde_json::from_str(&req.env_json()).unwrap();
        assert_eq!(val["mmap_flag"], serde_json::json!(0));

        // qwen3:8b on llama-gpu is small/local → normal mmap (flag 1).
        let small = cells
            .iter()
            .find(|c| c.model_id == "qwen3:8b" && c.backend_tag == "llama-gpu")
            .unwrap();
        let req = build_request(small, 900.0).unwrap();
        assert_eq!(req.mmap_flag, Some(1));
    }

    #[test]
    fn keep_warm_threshold_rule() {
        // > threshold ⇒ warm.
        assert!(keep_warm_from_cold_load(Some(599.0), 120.0));
        // <= threshold ⇒ cold.
        assert!(!keep_warm_from_cold_load(Some(9.0), 120.0));
        // no cold-load (exclusion row) ⇒ not warm.
        assert!(!keep_warm_from_cold_load(None, 120.0));
    }

    #[test]
    fn seed_keep_warm_matches_recomputed() {
        // Every seeded keep_warm flag must agree with the rule recomputed from
        // its cold_load_s (the runner recomputes and must reproduce the seed).
        let cells = load_seed().unwrap();
        let thresh = seed_keep_warm_threshold().unwrap();
        for cell in &cells {
            let recomputed = keep_warm_from_cold_load(cell.cold_load_s, thresh);
            assert_eq!(
                recomputed, cell.keep_warm,
                "keep_warm mismatch for {} {}: cold_load_s={:?} thresh={} seed={} recomputed={}",
                cell.model_id, cell.backend_tag, cell.cold_load_s, thresh, cell.keep_warm, recomputed
            );
        }
    }

    /// Helper: the verdict a CLEAN reproduction of a seed cell would produce
    /// (used by the seed-coherence test). Mirrors the seed's recorded exclusion.
    pub(super) fn seed_verdict(cell: &SeedCell) -> Verdict {
        match cell.exclusion_reason.as_str() {
            "none" => {
                if let (Some(t), Some(p), Some(c)) =
                    (cell.tok_s, cell.vram_or_ram_peak_gb, cell.cold_load_s)
                {
                    Verdict::Works { tok_s: t, peak_gb: p, cold_load_s: c }
                } else {
                    // a 'none' cell with no numbers is degenerate; treat as works-0.
                    Verdict::Works { tok_s: 0.0, peak_gb: 0.0, cold_load_s: 0.0 }
                }
            }
            "permanent-unknown-arch" => Verdict::Excluded {
                reason: ExclusionReason::PermanentUnknownArch,
                trigger: RecheckTrigger::None,
            },
            "build-conditional" => Verdict::Excluded {
                reason: ExclusionReason::BuildConditional,
                trigger: RecheckTrigger::LlamaCppVersionBump,
            },
            "quant-unsupported" => Verdict::Excluded {
                reason: ExclusionReason::QuantUnsupported,
                trigger: RecheckTrigger::None,
            },
            "oom-host-ram" => Verdict::Excluded {
                reason: ExclusionReason::OomHostRam,
                trigger: RecheckTrigger::None,
            },
            "oom-vram" => Verdict::Excluded {
                reason: ExclusionReason::OomVram,
                trigger: RecheckTrigger::None,
            },
            other => panic!("seed has unknown exclusion_reason {other}"),
        }
    }

    #[test]
    fn drift_clean_when_run_matches_seed() {
        let cells = load_seed().unwrap();
        for cell in &cells {
            // glm-4.7-flash llama-gpu seed is an acquisition gap in spirit, but
            // recorded as permanent-unknown-arch with provenance — a clean
            // reproduction of its recorded verdict drifts none.
            let v = seed_verdict(cell);
            assert!(drift_for(cell, &v).is_none(), "{} {} should not drift", cell.model_id, cell.backend_tag);
        }
    }

    #[test]
    fn drift_entry_when_build_conditional_now_works() {
        let cells = load_seed().unwrap();
        let bc = cells
            .iter()
            .find(|c| c.exclusion_reason == "build-conditional")
            .unwrap();
        // Simulate a llama.cpp bump: the cell now serves.
        let now_works = Verdict::Works { tok_s: 50.0, peak_gb: 13.0, cold_load_s: 6.0 };
        let d = drift_for(bc, &now_works).expect("a flipped build-conditional drifts");
        assert_eq!(d.seed_exclusion, "build-conditional");
        assert_eq!(d.run_exclusion, "none");
    }

    // ── SRV-03 unit coverage ──

    fn bc_row(model: &str) -> ServingProfile {
        ServingProfile {
            model_id: ModelId::from(model),
            backend_tag: ServingBackend::LlamaGpu,
            best_runtime: Runtime::LlamaCpp,
            env_json: r#"{"gfx_override":true,"mmap_flag":1,"flash_attn":false,"cpu_lib":null}"#.into(),
            tok_s: None,
            vram_or_ram_peak_gb: None,
            cold_load_s: None,
            keep_warm: false,
            fallback_runtime: None,
            exclusion_reason: ExclusionReason::BuildConditional,
            recheck_trigger: RecheckTrigger::LlamaCppVersionBump,
            provenance: None,
        }
    }

    fn working_row(model: &str) -> ServingProfile {
        ServingProfile {
            model_id: ModelId::from(model),
            backend_tag: ServingBackend::LlamaGpu,
            best_runtime: Runtime::LlamaCpp,
            env_json: "{}".into(),
            tok_s: Some(70.0),
            vram_or_ram_peak_gb: Some(18.0),
            cold_load_s: Some(8.0),
            keep_warm: false,
            fallback_runtime: Some(Runtime::Ollama),
            exclusion_reason: ExclusionReason::None,
            recheck_trigger: RecheckTrigger::None,
            provenance: None,
        }
    }

    fn permanent_row(model: &str) -> ServingProfile {
        ServingProfile {
            model_id: ModelId::from(model),
            backend_tag: ServingBackend::LlamaGpu,
            best_runtime: Runtime::LlamaCpp,
            env_json: "{}".into(),
            tok_s: None,
            vram_or_ram_peak_gb: None,
            cold_load_s: None,
            keep_warm: false,
            fallback_runtime: None,
            exclusion_reason: ExclusionReason::PermanentUnknownArch,
            recheck_trigger: RecheckTrigger::None,
            provenance: None,
        }
    }

    #[test]
    fn selector_picks_only_build_conditional_rows() {
        let rows = vec![
            working_row("qwen3:8b"),
            permanent_row("gpt-oss:20b"),
            bc_row("gemma4:26b"),
            bc_row("qwen3.5:35b"),
        ];
        let picked = select_build_conditional(&rows);
        let ids: Vec<&str> = picked.iter().map(|r| r.model_id.as_str()).collect();
        assert_eq!(ids, ["gemma4:26b", "qwen3.5:35b"]);
        // every picked row IS build-conditional (and nothing else is).
        assert!(picked.iter().all(|r| r.exclusion_reason == ExclusionReason::BuildConditional));
    }

    #[test]
    fn recheck_request_recovers_env_and_targets_llama_gpu() {
        let row = bc_row("gemma4:26b");
        let req = recheck_request(&row, 900.0);
        assert_eq!(req.backend, ServingBackend::LlamaGpu);
        assert_eq!(req.runtime, Runtime::LlamaCpp);
        assert_eq!(req.mmap_flag, Some(1));
        assert!(req.gfx_override);
    }

    /// SRV-03 acceptance: NO background/automatic trigger exists. This module
    /// exposes the recheck mode ONLY through the explicit `recheck_with` /
    /// `recheck_build_conditional` entry points; it wires no timer, interval,
    /// scheduler, watcher, or version-detector that could invoke them unattended.
    /// We assert that property over the module SOURCE: the only call sites of the
    /// recheck entries are tests and the operator-invoked live entry — there is no
    /// `tokio::spawn` / `interval` / `cron` / `watch` driving them.
    #[test]
    fn no_background_recheck_trigger_wired() {
        // Scan ONLY the non-test source (everything before this test module), so
        // the forbidden-string literals in this very assertion don't false-trip.
        let full = include_str!("runner.rs");
        let src = full
            .split("#[cfg(test)]")
            .next()
            .expect("runner.rs has a non-test prefix");
        // The recheck mode must not be driven by any background mechanism in this
        // module. None of these scheduling primitives appear in the runner source.
        for forbidden in [
            "tokio::spawn",
            "tokio::time::interval",
            "spawn_blocking",
            "cron",
            "Scheduler",
            "watch_version",
            "set_interval",
            "thread::spawn",
        ] {
            assert!(
                !src.contains(forbidden),
                "recheck mode must be operator-invoked only — found background primitive `{forbidden}` in runner.rs"
            );
        }
        // And the live entry is reachable only by an explicit call (it reads the
        // build id eagerly — there is no self-scheduling re-entry).
        assert!(src.contains("pub async fn recheck_build_conditional"));
    }
}
