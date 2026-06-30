//! Per-runtime launch + measure, the cell classifier, and the VRAM-release gate
//! (S85 SRV-02).
//!
//! This module is the layer that actually *touches* a runtime: it launches a
//! model on one tier (llama.cpp-rocm / ollama-rocm / CPU), drives the fixed
//! standard prompt, and reports a raw [`CellOutcome`] (loaded? failure shape?
//! tok/s / peak / cold-load). [`runner`](super::runner) sits above it and turns
//! raw outcomes into persisted [`ServingProfile`](super::ServingProfile) rows.
//!
//! ## Why it is trait-driven (the GPU-free test requirement)
//! Tests must run on CI with NO real GPU, NO llama-server, NO ollama. So the two
//! side-effecting surfaces are traits with a live impl AND a deterministic mock:
//!   - [`Launcher`] — launch a model on a tier, run the standard prompt, return a
//!     raw [`CellOutcome`]. The live impl ([`SystemLauncher`], stubbed here — the
//!     real process spawn lands with Chord in SRV-04) shells out; the test mock
//!     replays a scripted outcome per cell.
//!   - [`VramGate`] — read the amdgpu sysfs VRAM counter and confirm it returned
//!     to baseline between cells (one-model-in-VRAM-at-a-time). The live impl
//!     reads the sysfs path from config; the mock scripts the counter.
//!
//! ## The classifier (the load-bearing sweep lessons, in code)
//! [`classify`] maps a raw [`CellOutcome`] to the persisted
//! `(ExclusionReason, RecheckTrigger)` pair. It encodes the v1→v2 distinctions
//! that the manual sweep paid for in time:
//!   - unknown-arch loader error (`gptoss`, `glm4`) ⇒ `permanent-unknown-arch`
//!     (no recheck — the build will never grow the handler on a version bump
//!     alone; that is upstream's job, advisory `none`);
//!   - recognized-arch loader mismatch (`gemma4` tensor-graph, `qwen35moe`
//!     rope-metadata 4≠3) ⇒ `build-conditional` + `llama-cpp-version-bump`
//!     (MAY flip on a newer llama.cpp);
//!   - **`slow-load-exceeds-bound` is DISTINCT from a true arch hang** — a cold
//!     load that blew the (generous) bound is recorded as its own outcome, NOT
//!     classified as arch-unsupported. This is the exact v1→v2 lesson: v1 timed
//!     out NAS-staged mmap page-faults and mislabeled them `hang`; under
//!     `--no-mmap` they load. The runner records the slow load and (for big MoEs)
//!     keep-warms them — it does not exclude them.

use crate::intake::serving::{ExclusionReason, ModelId, RecheckTrigger, Runtime, ServingBackend};

/// The fixed standard prompt id used for every serve so tok/s is comparable cell
/// to cell (the same prompt the manual sweep used). The prompt TEXT is supplied
/// by the live launcher from its corpus; the runner only needs the id for the
/// env/provenance record — no infra literal here.
pub const STANDARD_PROMPT_ID: &str = "sweep-standard-v2";

/// The shape of how a launch+serve ended. The launcher reports this; [`classify`]
/// turns it into the persisted exclusion bookkeeping. Each variant maps 1:1 to a
/// distinct measured outcome in the v2 master table.
#[derive(Debug, Clone, PartialEq)]
pub enum CellOutcome {
    /// The model loaded and served the standard prompt. Carries the measured
    /// numbers the row persists.
    Served {
        tok_s: f64,
        peak_gb: f64,
        cold_load_s: f64,
    },
    /// The runtime rejected the model with an UNKNOWN-architecture loader error
    /// (e.g. `unknown model architecture: 'gptoss'`). Permanent in this build.
    UnknownArch { error: String },
    /// The runtime RECOGNIZED the arch but this build's loader could not read the
    /// GGUF (e.g. gemma4 tensor-graph count mismatch, qwen35moe rope schema 4≠3).
    /// May flip on a newer llama.cpp build.
    BuildIncompatible { error: String },
    /// The runtime read the file but cannot handle its quantization / file_type
    /// (e.g. ollama qwen3moe runner nil-panics on BF16).
    QuantUnsupported { error: String },
    /// Out of HOST RAM — the ollama system-RAM pre-flight refused, or the genuine
    /// CPU tier cannot fit the weights in system RAM (llama3.3:70b @ 42GB > 31GB).
    OomHostRam { error: String },
    /// Out of GPU VRAM under the 96GB ceiling.
    OomVram { error: String },
    /// The cold load blew even the generous bound. **Recorded distinctly from an
    /// arch hang** — this is the v1 false-`hang` shape that `--no-mmap` fixed; the
    /// runner does NOT treat it as an exclusion (it surfaces a slow-load note).
    SlowLoadExceedsBound { bound_s: f64 },
    /// Weights are absent (not in store, not staged). No verdict is fabricated —
    /// the cell is recorded as an acquisition gap and serving is skipped (the
    /// glm-4.7-flash provenance case).
    AcquisitionGap { detail: String },
}

/// A model whose verdict could not be measured (weights absent). Distinct from a
/// measured exclusion: it carries no exclusion_reason a launch produced. The
/// runner records it with a provenance note and skips the serving cells.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquisitionGap(pub String);

/// The classifier verdict: the persisted exclusion bookkeeping plus whether the
/// cell actually served (so the runner knows to keep the measured numbers).
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The cell served. Carries the measured numbers.
    Works {
        tok_s: f64,
        peak_gb: f64,
        cold_load_s: f64,
    },
    /// The cell did not serve for a measured reason. Carries the coherent
    /// `(ExclusionReason, RecheckTrigger)` pair (always passes
    /// `ServingProfile::validate`).
    Excluded {
        reason: ExclusionReason,
        trigger: RecheckTrigger,
    },
    /// Cold load blew the bound — NOT an exclusion. The runner records the slow
    /// load (and, above the keep-warm threshold, would keep it warm) rather than
    /// excluding it. Preserves the v1→v2 distinction.
    SlowLoad { bound_s: f64 },
    /// Weights absent — no fabricated verdict. The runner records provenance and
    /// skips serving cells.
    AcquisitionGap { detail: String },
}

/// Map a raw [`CellOutcome`] to a [`Verdict`] with a COHERENT exclusion/recheck
/// pairing (one that always passes `ServingProfile::validate`).
///
/// This is the heart of the sweep-lessons-in-code. The pairing rules:
///   - `UnknownArch`      ⇒ `permanent-unknown-arch` + `none` (build will never
///                          grow the handler on a version bump alone).
///   - `BuildIncompatible`⇒ `build-conditional` + `llama-cpp-version-bump` (the
///                          ONLY pair that carries the version-bump trigger).
///   - `QuantUnsupported` ⇒ `quant-unsupported` + `none`.
///   - `OomHostRam`       ⇒ `oom-host-ram` + `none`.
///   - `OomVram`          ⇒ `oom-vram` + `none`.
///   - `SlowLoadExceedsBound` ⇒ `SlowLoad` (NOT an exclusion — the v1→v2 fix).
///   - `AcquisitionGap`   ⇒ `AcquisitionGap` (no fabricated verdict).
pub fn classify(outcome: &CellOutcome) -> Verdict {
    match outcome {
        CellOutcome::Served {
            tok_s,
            peak_gb,
            cold_load_s,
        } => Verdict::Works {
            tok_s: *tok_s,
            peak_gb: *peak_gb,
            cold_load_s: *cold_load_s,
        },
        CellOutcome::UnknownArch { .. } => Verdict::Excluded {
            reason: ExclusionReason::PermanentUnknownArch,
            trigger: RecheckTrigger::None,
        },
        CellOutcome::BuildIncompatible { .. } => Verdict::Excluded {
            reason: ExclusionReason::BuildConditional,
            trigger: RecheckTrigger::LlamaCppVersionBump,
        },
        CellOutcome::QuantUnsupported { .. } => Verdict::Excluded {
            reason: ExclusionReason::QuantUnsupported,
            trigger: RecheckTrigger::None,
        },
        CellOutcome::OomHostRam { .. } => Verdict::Excluded {
            reason: ExclusionReason::OomHostRam,
            trigger: RecheckTrigger::None,
        },
        CellOutcome::OomVram { .. } => Verdict::Excluded {
            reason: ExclusionReason::OomVram,
            trigger: RecheckTrigger::None,
        },
        CellOutcome::SlowLoadExceedsBound { bound_s } => Verdict::SlowLoad { bound_s: *bound_s },
        CellOutcome::AcquisitionGap { detail } => Verdict::AcquisitionGap {
            detail: detail.clone(),
        },
    }
}

/// `true` for a llama.cpp cell that must serve with `--no-mmap` (`mmap_flag=0`):
/// any NAS-staged or large weight. The v2 lesson — mmap over NFS page-faults into
/// a false hang; `--no-mmap` streams once straight to VRAM.
///
/// "Large" is decided by the weight size in GB vs a threshold, OR an explicit
/// staged flag the acquirer sets. Only meaningful on the llama.cpp tier (ollama
/// manages its own loading; CPU has no mmap path here).
pub fn requires_no_mmap(backend: ServingBackend, staged: bool, weight_gb: Option<f64>, large_threshold_gb: f64) -> bool {
    if backend != ServingBackend::LlamaGpu {
        return false;
    }
    staged || weight_gb.map(|g| g >= large_threshold_gb).unwrap_or(false)
}

/// Sharded-GGUF detection: ollama `create` from shard-1 imports metadata only
/// (0 tensors → nil-panic), so a sharded model must be MERGED first (or routed to
/// llama.cpp pointing at shard-1, which auto-loads the rest). The runner consults
/// this before an ollama cell.
pub fn is_sharded_gguf(path_or_name: &str) -> bool {
    // The canonical llama.cpp split naming: `-00001-of-00003.gguf`.
    let lower = path_or_name.to_ascii_lowercase();
    lower.contains("-of-") && lower.contains("-0000")
}

/// What a launcher needs to bring up one cell. Carries no infra literal — the
/// launcher resolves binaries/endpoints/sysfs from config/vault.
#[derive(Debug, Clone, PartialEq)]
pub struct CellRequest {
    pub model_id: ModelId,
    pub backend: ServingBackend,
    pub runtime: Runtime,
    /// `0` (false) ⇒ `--no-mmap`. `None` ⇒ tier has no mmap path (ollama / cpu).
    pub mmap_flag: Option<u8>,
    /// `true` ⇒ set the gfx override; `false` ⇒ empty override (CPU tier).
    pub gfx_override: bool,
    /// The cpu library override for the CPU tier (`OLLAMA_CPU_LIBRARY`), if any.
    pub cpu_lib: Option<String>,
    /// FlashAttention on (ollama tiers).
    pub flash_attn: bool,
    /// Generous per-cell cold-load bound (seconds).
    pub load_bound_s: f64,
}

impl CellRequest {
    /// Build the env JSON recorded on the row (`env_json`). Includes the
    /// `mmap_flag` so a downstream reader (Chord) can replay the exact launch —
    /// the v2 requirement to RECORD the flag, not just use it.
    pub fn env_json(&self) -> String {
        let v = serde_json::json!({
            "gfx_override": self.gfx_override,
            "mmap_flag": self.mmap_flag,
            "flash_attn": self.flash_attn,
            "cpu_lib": self.cpu_lib,
            "standard_prompt_id": STANDARD_PROMPT_ID,
        });
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Launch a model on a tier, run the standard prompt, return a raw outcome. The
/// live impl spawns the runtime process; the test mock replays a scripted result.
/// MUST NOT panic — every failure maps to a [`CellOutcome`] variant.
#[async_trait::async_trait]
pub trait Launcher: Send + Sync {
    async fn launch_and_measure(&self, req: &CellRequest) -> CellOutcome;
}

/// Read the amdgpu VRAM counter and confirm it returned to baseline between
/// cells (one-model-in-VRAM-at-a-time). The live impl reads the sysfs path from
/// config; the mock scripts the counter so the gate is testable GPU-free.
#[async_trait::async_trait]
pub trait VramGate: Send + Sync {
    /// Current VRAM in use (bytes). The runner waits for this to fall back to
    /// baseline before launching the next cell.
    async fn vram_in_use_bytes(&self) -> Result<u64, String>;
}

/// Default baseline tolerance (bytes) — amdgpu idles at ~147MB on this host; a
/// reading at/below baseline + tolerance counts as "released". Sourced as a
/// number, never an infra literal.
pub const VRAM_BASELINE_TOLERANCE_BYTES: u64 = 256 * 1024 * 1024;

/// Has VRAM been released back to (near) baseline? The runner gates the next cell
/// on this returning `true` — the one-model-in-VRAM invariant.
pub fn vram_released(in_use_bytes: u64, baseline_bytes: u64, tolerance_bytes: u64) -> bool {
    in_use_bytes <= baseline_bytes.saturating_add(tolerance_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::serving::ServingProfile;

    fn profile_from_verdict(v: &Verdict, backend: ServingBackend, runtime: Runtime) -> ServingProfile {
        let (excl, trig, tok, peak, cold) = match v {
            Verdict::Works { tok_s, peak_gb, cold_load_s } => (
                ExclusionReason::None,
                RecheckTrigger::None,
                Some(*tok_s),
                Some(*peak_gb),
                Some(*cold_load_s),
            ),
            Verdict::Excluded { reason, trigger } => (*reason, *trigger, None, None, None),
            Verdict::SlowLoad { .. } => (ExclusionReason::None, RecheckTrigger::None, None, None, None),
            Verdict::AcquisitionGap { .. } => (ExclusionReason::None, RecheckTrigger::None, None, None, None),
        };
        ServingProfile {
            model_id: ModelId::from("test:model"),
            backend_tag: backend,
            best_runtime: runtime,
            env_json: "{}".into(),
            tok_s: tok,
            vram_or_ram_peak_gb: peak,
            cold_load_s: cold,
            keep_warm: false,
            fallback_runtime: None,
            exclusion_reason: excl,
            recheck_trigger: trig,
            provenance: None,
        }
    }

    #[test]
    fn classify_clean_serve_is_works() {
        let v = classify(&CellOutcome::Served {
            tok_s: 74.0,
            peak_gb: 18.0,
            cold_load_s: 9.0,
        });
        assert!(matches!(v, Verdict::Works { tok_s, .. } if (tok_s - 74.0).abs() < 1e-9));
    }

    #[test]
    fn classify_unknown_arch_is_permanent_no_recheck() {
        // gptoss / glm4 shape.
        let v = classify(&CellOutcome::UnknownArch {
            error: "unknown model architecture: 'gptoss'".into(),
        });
        assert_eq!(
            v,
            Verdict::Excluded {
                reason: ExclusionReason::PermanentUnknownArch,
                trigger: RecheckTrigger::None,
            }
        );
        // ...and the pairing is coherent (validate passes).
        assert!(profile_from_verdict(&v, ServingBackend::LlamaGpu, Runtime::LlamaCpp)
            .validate()
            .is_ok());
    }

    #[test]
    fn classify_build_incompatible_is_build_conditional_with_version_bump() {
        // gemma4 tensor-graph / qwen35moe rope-metadata shape.
        let v = classify(&CellOutcome::BuildIncompatible {
            error: "qwen35moe.rope.dimension_sections has wrong array length; expected 4, got 3".into(),
        });
        assert_eq!(
            v,
            Verdict::Excluded {
                reason: ExclusionReason::BuildConditional,
                trigger: RecheckTrigger::LlamaCppVersionBump,
            }
        );
        assert!(profile_from_verdict(&v, ServingBackend::LlamaGpu, Runtime::LlamaCpp)
            .validate()
            .is_ok());
    }

    #[test]
    fn classify_quant_host_ram_vram() {
        assert!(matches!(
            classify(&CellOutcome::QuantUnsupported { error: "BF16 file_type".into() }),
            Verdict::Excluded { reason: ExclusionReason::QuantUnsupported, trigger: RecheckTrigger::None }
        ));
        assert!(matches!(
            classify(&CellOutcome::OomHostRam { error: "37.6 GiB system > 33.8".into() }),
            Verdict::Excluded { reason: ExclusionReason::OomHostRam, trigger: RecheckTrigger::None }
        ));
        assert!(matches!(
            classify(&CellOutcome::OomVram { error: "ceiling".into() }),
            Verdict::Excluded { reason: ExclusionReason::OomVram, trigger: RecheckTrigger::None }
        ));
    }

    #[test]
    fn slow_load_is_distinct_from_arch_hang() {
        // The v1->v2 distinction: a blown cold-load bound is NOT an exclusion.
        let v = classify(&CellOutcome::SlowLoadExceedsBound { bound_s: 900.0 });
        assert_eq!(v, Verdict::SlowLoad { bound_s: 900.0 });
        // It is explicitly NOT a build-conditional / unknown-arch verdict.
        assert!(!matches!(v, Verdict::Excluded { .. }));
    }

    #[test]
    fn acquisition_gap_fabricates_no_verdict() {
        let v = classify(&CellOutcome::AcquisitionGap {
            detail: "weights absent from host".into(),
        });
        assert!(matches!(v, Verdict::AcquisitionGap { .. }));
        assert!(!matches!(v, Verdict::Excluded { .. }));
    }

    #[test]
    fn no_mmap_required_for_staged_or_large_llama_cells_only() {
        // staged small llama cell -> no-mmap.
        assert!(requires_no_mmap(ServingBackend::LlamaGpu, true, Some(5.0), 30.0));
        // large local llama cell -> no-mmap.
        assert!(requires_no_mmap(ServingBackend::LlamaGpu, false, Some(77.0), 30.0));
        // small local llama cell -> normal mmap.
        assert!(!requires_no_mmap(ServingBackend::LlamaGpu, false, Some(5.0), 30.0));
        // ollama / cpu tiers never get the llama --no-mmap flag.
        assert!(!requires_no_mmap(ServingBackend::OllamaGpu, true, Some(77.0), 30.0));
        assert!(!requires_no_mmap(ServingBackend::Cpu, true, Some(77.0), 30.0));
    }

    #[test]
    fn env_json_records_mmap_flag() {
        let req = CellRequest {
            model_id: ModelId::from("minimax-m2.7"),
            backend: ServingBackend::LlamaGpu,
            runtime: Runtime::LlamaCpp,
            mmap_flag: Some(0),
            gfx_override: true,
            cpu_lib: None,
            flash_attn: false,
            load_bound_s: 900.0,
        };
        let env = req.env_json();
        let val: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert_eq!(val["mmap_flag"], serde_json::json!(0));
        assert_eq!(val["gfx_override"], serde_json::json!(true));
    }

    #[test]
    fn sharded_gguf_detected() {
        assert!(is_sharded_gguf("/stage/qwen3-coder-bf16-00001-of-00003.gguf"));
        assert!(!is_sharded_gguf("/stage/qwen3-coder-q4.gguf"));
    }

    #[test]
    fn vram_release_gate() {
        let baseline = 147 * 1024 * 1024;
        // back at baseline -> released.
        assert!(vram_released(baseline, baseline, VRAM_BASELINE_TOLERANCE_BYTES));
        // a few hundred MB over -> within tolerance, released.
        assert!(vram_released(baseline + 100 * 1024 * 1024, baseline, VRAM_BASELINE_TOLERANCE_BYTES));
        // 10 GB still resident -> NOT released (gate blocks the next cell).
        assert!(!vram_released(10 * 1024 * 1024 * 1024, baseline, VRAM_BASELINE_TOLERANCE_BYTES));
    }
}
