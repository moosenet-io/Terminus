//! S85 Serving Intake Profiling — foundation (SRV-01).
//!
//! This module is the BASE that the serving harness (SRV-02/03) *produces* and
//! Chord (SRV-04..06) *consumes*. It owns:
//!   - the serving-profile DB schema + idempotent migration ([`schema`]),
//!   - the shared types every writer/reader uses ([`ServingProfile`],
//!     [`Runtime`], [`ExclusionReason`], [`RecheckTrigger`], [`ServingBackend`]).
//!
//! ## Model identity (CRITICAL — byte-identical to S83/S84)
//! The serving dimension joins the S83 builder side and the S84 assistant side on
//! the same `model_id`. S83/MINT does NOT normalize model names (it stores the
//! chord registry key verbatim, e.g. `"qwen3:8b"`), and S84 REUSES that exact
//! pass-through. We do the SAME: [`super::assistant::ModelId`] is re-exported and
//! reused here unchanged — inventing a new normalization would silently break the
//! `model_full_profile` join. See [`super::assistant::ModelId::from_registry_key`]
//! (a documented pass-through, no lowering / trimming / tag stripping).
//!
//! ## Backend tag (`llama-gpu` | `ollama-gpu` | `cpu`)
//! Serving is keyed on a THREE-tier backend, not the S84 two-tier (`gpu`/`cpu`)
//! hardware tag: a model can serve under llama.cpp-rocm OR ollama-rocm on the
//! same GPU, and those are distinct serving rows with different runtimes/env.
//! [`ServingBackend`] carries that three-tier tag; it is the `backend_tag` column
//! of `serving_profile` and the join key into `model_full_profile`.
//!
//! ## The serving record
//! One [`ServingProfile`] row per (model × serving backend): the chosen
//! `best_runtime` + its env (gfx override / cpu lib / mmap flag / flash-attn),
//! measured `tok_s` / `vram_or_ram_peak_gb` / `cold_load_s`, a `keep_warm` flag
//! for big slow-loading MoEs, a nullable `fallback_runtime`, and the
//! `exclusion_reason` / `recheck_trigger` enums explaining why a faster runtime
//! was not chosen and whether a llama.cpp build bump should prompt a recheck.

pub mod probes;
pub mod runner;
pub mod schema;

use std::fmt;

// REUSE the S83/S84 model identity verbatim — do NOT define a new normalization.
pub use super::assistant::ModelId;

/// A concrete serving runtime a model can be launched under. Distinct from the
/// coarser [`ServingBackend`] tier: e.g. both `LlamaCpp` and `Cpu` runtimes can
/// share infrastructure, but `LlamaCpp` is a GPU-tier runtime while `Cpu` is the
/// CPU-tier runtime. Stored as the lowercase wire string in `best_runtime` /
/// `fallback_runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    /// llama.cpp-rocm (HIP `llama-server`): broadest + most VRAM-efficient,
    /// `--no-mmap` for staged/large weights.
    LlamaCpp,
    /// ollama-rocm (primary GPU unit): serves the archs the `b1258` llama.cpp
    /// build rejects (gemma4 / gpt-oss / glm / qwen3.5-6).
    Ollama,
    /// genuine CPU (secondary unit, cpu lib): the slow last-resort fallback.
    Cpu,
}

impl Runtime {
    /// Lowercase kebab wire string stored in the DB.
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::LlamaCpp => "llama-cpp",
            Runtime::Ollama => "ollama",
            Runtime::Cpu => "cpu",
        }
    }

    /// Parse the lowercase wire string. Anything else ⇒ `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "llama-cpp" => Some(Runtime::LlamaCpp),
            "ollama" => Some(Runtime::Ollama),
            "cpu" => Some(Runtime::Cpu),
            _ => None,
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three-tier serving backend a row is keyed on (the `backend_tag` column).
///
/// Unlike S84's two-value hardware tag (`gpu`/`cpu`), serving distinguishes the
/// two GPU runtimes because a model can serve under one but not the other on the
/// same card (the llama.cpp arch-rejection lesson). The three tiers in routing
/// order: `llama-gpu` → `ollama-gpu` → `cpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServingBackend {
    /// llama.cpp-rocm on the GPU.
    LlamaGpu,
    /// ollama-rocm on the GPU.
    OllamaGpu,
    /// genuine CPU.
    Cpu,
}

impl ServingBackend {
    /// Lowercase wire string stored as `backend_tag`.
    pub fn as_str(self) -> &'static str {
        match self {
            ServingBackend::LlamaGpu => "llama-gpu",
            ServingBackend::OllamaGpu => "ollama-gpu",
            ServingBackend::Cpu => "cpu",
        }
    }

    /// Parse the lowercase wire string. Anything else ⇒ `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "llama-gpu" => Some(ServingBackend::LlamaGpu),
            "ollama-gpu" => Some(ServingBackend::OllamaGpu),
            "cpu" => Some(ServingBackend::Cpu),
            _ => None,
        }
    }

    /// All three tiers in routing order (broadest GPU runtime first, CPU last).
    pub fn all() -> [ServingBackend; 3] {
        [
            ServingBackend::LlamaGpu,
            ServingBackend::OllamaGpu,
            ServingBackend::Cpu,
        ]
    }
}

impl fmt::Display for ServingBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a faster runtime was NOT chosen for a model on a given backend.
///
/// `none` means no exclusion (the recorded runtime is the chosen one). The other
/// variants record the measured reason a tier was skipped. `permanent-unknown-arch`
/// (the build has no handler at all) is permanent until upstream adds it;
/// `build-conditional` (arch recognized but this build's loader can't read the
/// GGUF) MAY flip on a newer llama.cpp build — only that case carries a
/// [`RecheckTrigger::LlamaCppVersionBump`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExclusionReason {
    /// No exclusion — the recorded runtime is the chosen one.
    None,
    /// The build has no handler for the arch (e.g. gpt-oss→gptoss, glm→glm4).
    /// Permanent until upstream adds it.
    PermanentUnknownArch,
    /// Arch recognized but this build's loader can't read the GGUF (e.g. gemma4
    /// tensor-graph, qwen3.5/3.6 rope-metadata schema 4≠3). May flip on a newer
    /// llama.cpp build.
    BuildConditional,
    /// The model's quantization is unsupported by the runtime.
    QuantUnsupported,
    /// Out of host RAM (ollama's host-RAM pre-flight / CPU residency).
    OomHostRam,
    /// Out of VRAM on the GPU.
    OomVram,
}

impl ExclusionReason {
    /// Lowercase kebab wire string stored as `exclusion_reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            ExclusionReason::None => "none",
            ExclusionReason::PermanentUnknownArch => "permanent-unknown-arch",
            ExclusionReason::BuildConditional => "build-conditional",
            ExclusionReason::QuantUnsupported => "quant-unsupported",
            ExclusionReason::OomHostRam => "oom-host-ram",
            ExclusionReason::OomVram => "oom-vram",
        }
    }

    /// Parse the lowercase wire string. Anything else ⇒ `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(ExclusionReason::None),
            "permanent-unknown-arch" => Some(ExclusionReason::PermanentUnknownArch),
            "build-conditional" => Some(ExclusionReason::BuildConditional),
            "quant-unsupported" => Some(ExclusionReason::QuantUnsupported),
            "oom-host-ram" => Some(ExclusionReason::OomHostRam),
            "oom-vram" => Some(ExclusionReason::OomVram),
            _ => None,
        }
    }
}

impl fmt::Display for ExclusionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What event should prompt a re-test of a row's exclusion.
///
/// `none` for permanent reasons (nothing will change the verdict);
/// `llama-cpp-version-bump` for `build-conditional` rows — advisory metadata that
/// SRV-03's operator-invoked `--recheck-build-conditional` mode keys off. It is
/// NOT an automated background sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecheckTrigger {
    /// No recheck expected to change the verdict.
    None,
    /// Re-test on a llama.cpp build bump (the build-conditional case only).
    LlamaCppVersionBump,
}

impl RecheckTrigger {
    /// Lowercase kebab wire string stored as `recheck_trigger`.
    pub fn as_str(self) -> &'static str {
        match self {
            RecheckTrigger::None => "none",
            RecheckTrigger::LlamaCppVersionBump => "llama-cpp-version-bump",
        }
    }

    /// Parse the lowercase wire string. Anything else ⇒ `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(RecheckTrigger::None),
            "llama-cpp-version-bump" => Some(RecheckTrigger::LlamaCppVersionBump),
            _ => None,
        }
    }
}

impl fmt::Display for RecheckTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validation error for a [`ServingProfile`] whose enum combination is
/// self-contradictory (caught BEFORE it can reach the DB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingProfileInvalid(pub String);

impl fmt::Display for ServingProfileInvalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid serving profile: {}", self.0)
    }
}

impl std::error::Error for ServingProfileInvalid {}

/// One serving-profile row: how a single model is best served on a single
/// backend tier, with the measured numbers and the exclusion bookkeeping.
///
/// This is the unit the SRV-02 runner writes (UPSERT on `(model_id, backend_tag)`)
/// and SRV-04 Chord reads. `model_id` is byte-identical to S83/S84.
#[derive(Debug, Clone, PartialEq)]
pub struct ServingProfile {
    /// Model under test (S83-identical id).
    pub model_id: ModelId,
    /// The three-tier serving backend this row is keyed on.
    pub backend_tag: ServingBackend,
    /// The runtime chosen to serve the model on this backend.
    pub best_runtime: Runtime,
    /// Serialized launch env (gfx override / cpu lib / mmap flag / flash-attn).
    /// JSON object string; the runner builds it, Chord replays it verbatim.
    pub env_json: String,
    /// Measured throughput (tokens/sec) on the standard sweep prompt. `None` when
    /// the cell never served (an exclusion row).
    pub tok_s: Option<f64>,
    /// Peak VRAM (GPU tiers) or RAM (CPU tier) in GB during the serve. `None` for
    /// exclusion rows that never loaded.
    pub vram_or_ram_peak_gb: Option<f64>,
    /// Cold-load wall-clock seconds. Drives `keep_warm`. `None` for exclusion rows.
    pub cold_load_s: Option<f64>,
    /// True for big slow-loading models Chord holds resident (never cold-launched
    /// per request).
    pub keep_warm: bool,
    /// The runtime to fall back to on launch failure. `None` ⇒ no fallback (CPU
    /// rows, or an exclusion row).
    pub fallback_runtime: Option<Runtime>,
    /// Why a faster runtime was not chosen on this backend.
    pub exclusion_reason: ExclusionReason,
    /// What event should prompt a recheck of this exclusion.
    pub recheck_trigger: RecheckTrigger,
    /// Optional provenance note (e.g. the glm-4.7-flash "verdict by inference,
    /// weights absent at confirmation time" honesty case). `None` for a fresh
    /// measurement.
    pub provenance: Option<String>,
}

impl ServingProfile {
    /// Schema-level validation of the enum combination. Rejects the contradictory
    /// combos so a self-inconsistent row can never reach the DB:
    ///   - a `recheck_trigger = llama-cpp-version-bump` is ONLY valid with
    ///     `exclusion_reason = build-conditional` (the only reason a llama.cpp
    ///     bump can flip — e.g. `permanent-unknown-arch + llama-cpp-version-bump`
    ///     is a contradiction);
    ///   - `exclusion_reason = build-conditional` is meaningless WITHOUT the
    ///     `llama-cpp-version-bump` trigger (a build-conditional row that says it
    ///     will never be rechecked contradicts its own definition).
    ///
    /// Called by every write path (and asserted in the SRV-01 negative test)
    /// before the row is persisted.
    pub fn validate(&self) -> Result<(), ServingProfileInvalid> {
        match (self.exclusion_reason, self.recheck_trigger) {
            // The only coherent build-conditional pairing.
            (ExclusionReason::BuildConditional, RecheckTrigger::LlamaCppVersionBump) => Ok(()),
            // build-conditional MUST carry the version-bump trigger.
            (ExclusionReason::BuildConditional, RecheckTrigger::None) => Err(ServingProfileInvalid(
                "exclusion_reason=build-conditional requires \
                 recheck_trigger=llama-cpp-version-bump"
                    .into(),
            )),
            // Any non-build-conditional reason MUST NOT carry the version-bump
            // trigger (permanent-unknown-arch + llama-cpp-version-bump, oom +
            // version-bump, none + version-bump, …).
            (other, RecheckTrigger::LlamaCppVersionBump) => Err(ServingProfileInvalid(format!(
                "recheck_trigger=llama-cpp-version-bump is only valid with \
                 exclusion_reason=build-conditional, not {}",
                other.as_str()
            ))),
            // Every other reason with no trigger is fine.
            (_, RecheckTrigger::None) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_profile() -> ServingProfile {
        ServingProfile {
            model_id: ModelId::from("qwen3:8b"),
            backend_tag: ServingBackend::LlamaGpu,
            best_runtime: Runtime::LlamaCpp,
            env_json: "{}".into(),
            tok_s: Some(42.0),
            vram_or_ram_peak_gb: Some(7.5),
            cold_load_s: Some(12.0),
            keep_warm: false,
            fallback_runtime: Some(Runtime::Ollama),
            exclusion_reason: ExclusionReason::None,
            recheck_trigger: RecheckTrigger::None,
            provenance: None,
        }
    }

    #[test]
    fn model_id_is_pass_through_byte_identical_to_s83() {
        // Reuses the EXACT S83/S84 ModelId — must NOT normalize.
        for raw in ["qwen3:8b", "Qwen3:8B", "  spaced  ", "gpt-oss:20b", "glm-4.7-flash"] {
            assert_eq!(ModelId::from_registry_key(raw).as_str(), raw);
        }
        // And it is literally the same type re-exported from assistant.
        let a: crate::intake::assistant::ModelId = ModelId::from("glm:9b");
        assert_eq!(a.as_str(), "glm:9b");
    }

    #[test]
    fn runtime_round_trips() {
        for r in [Runtime::LlamaCpp, Runtime::Ollama, Runtime::Cpu] {
            assert_eq!(Runtime::parse(r.as_str()), Some(r));
            // serde wire form matches the as_str kebab form.
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(json, format!("\"{}\"", r.as_str()));
            let back: Runtime = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
        assert_eq!(Runtime::parse("vllm"), None);
    }

    #[test]
    fn serving_backend_round_trips() {
        for b in ServingBackend::all() {
            assert_eq!(ServingBackend::parse(b.as_str()), Some(b));
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(json, format!("\"{}\"", b.as_str()));
            assert_eq!(serde_json::from_str::<ServingBackend>(&json).unwrap(), b);
        }
        assert_eq!(ServingBackend::parse("gpu"), None);
        assert_eq!(
            ServingBackend::all().map(|b| b.as_str()),
            ["llama-gpu", "ollama-gpu", "cpu"]
        );
    }

    #[test]
    fn exclusion_reason_round_trips() {
        let all = [
            ExclusionReason::None,
            ExclusionReason::PermanentUnknownArch,
            ExclusionReason::BuildConditional,
            ExclusionReason::QuantUnsupported,
            ExclusionReason::OomHostRam,
            ExclusionReason::OomVram,
        ];
        for e in all {
            assert_eq!(ExclusionReason::parse(e.as_str()), Some(e));
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(json, format!("\"{}\"", e.as_str()));
            assert_eq!(serde_json::from_str::<ExclusionReason>(&json).unwrap(), e);
        }
        assert_eq!(ExclusionReason::parse("bogus"), None);
    }

    #[test]
    fn recheck_trigger_round_trips() {
        for t in [RecheckTrigger::None, RecheckTrigger::LlamaCppVersionBump] {
            assert_eq!(RecheckTrigger::parse(t.as_str()), Some(t));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()));
            assert_eq!(serde_json::from_str::<RecheckTrigger>(&json).unwrap(), t);
        }
        assert_eq!(RecheckTrigger::parse("os-reboot"), None);
    }

    #[test]
    fn validate_accepts_coherent_rows() {
        // none + none
        assert!(ok_profile().validate().is_ok());
        // permanent-unknown-arch + none
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::PermanentUnknownArch;
        p.recheck_trigger = RecheckTrigger::None;
        assert!(p.validate().is_ok());
        // build-conditional + llama-cpp-version-bump
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::BuildConditional;
        p.recheck_trigger = RecheckTrigger::LlamaCppVersionBump;
        assert!(p.validate().is_ok());
        // oom-vram + none
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::OomVram;
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_rejects_contradictory_combos() {
        // The headline contradiction: permanent + version-bump.
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::PermanentUnknownArch;
        p.recheck_trigger = RecheckTrigger::LlamaCppVersionBump;
        let err = p.validate().unwrap_err();
        assert!(err.0.contains("permanent-unknown-arch"));
        assert!(err.0.contains("build-conditional"));

        // none + version-bump is also incoherent.
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::None;
        p.recheck_trigger = RecheckTrigger::LlamaCppVersionBump;
        assert!(p.validate().is_err());

        // oom-host-ram + version-bump is incoherent.
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::OomHostRam;
        p.recheck_trigger = RecheckTrigger::LlamaCppVersionBump;
        assert!(p.validate().is_err());

        // build-conditional WITHOUT the trigger is incoherent (it claims it will
        // never be rechecked, contradicting its own definition).
        let mut p = ok_profile();
        p.exclusion_reason = ExclusionReason::BuildConditional;
        p.recheck_trigger = RecheckTrigger::None;
        let err = p.validate().unwrap_err();
        assert!(err.0.contains("build-conditional"));
        assert!(err.0.contains("llama-cpp-version-bump"));
    }
}
