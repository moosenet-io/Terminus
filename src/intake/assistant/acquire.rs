//! S84 ASMT-09 — model acquisition + backend strategy (gfx1151-aware).
//!
//! The consolidated runner reads `nominations.json` (ASMT-08 output) and, for
//! each nominated model, must (a) decide HOW to get the weights onto the host
//! (`ollama pull`, register an already-staged span GGUF, or fetch via the S83
//! `gguf_path` binary for sharded/HF models), and (b) decide WHICH backend(s) to
//! profile it on, honoring its gfx1151 runnability class:
//!
//!   - **confirmed** → Vulkan/Ollama GPU first; CPU pass too.
//!   - **experimental** → MoE-on-Vulkan is known to hang, so bring it up on ROCm
//!     with `HSA_OVERRIDE_GFX_VERSION`; if it still hangs on BOTH, skip-with-reason.
//!   - **unknown** → needs the bounded smoke test before committing to the suite.
//!
//! ## Resilient staging (mirrors S83)
//! Write-heavy small-file IO (nominations, checkpoint) lives on the **reliable
//! NAS** ([`config::intake_staging_dir`]); read-heavy model GGUF loads come from
//! the **local span** ([`config::intake_model_span_dir`]) with a **NAS fallback**
//! ([`config::intake_model_nas_dir`]) for the USB-card-drop recovery path. Every
//! path resolves through `config.rs` — NEVER a literal (the `pii_gate` hook would
//! otherwise see a hardcoded mount).
//!
//! ## VRAM ceiling
//! A nomination that cannot fit the host VRAM ceiling (e.g. Command A+ 218B)
//! is a **clean skip-with-reason**, never a crash — see [`AcquisitionOutcome`].
//!
//! Acquisition itself is abstracted behind the [`Acquirer`] trait so the runner
//! is testable without touching the network or a real Ollama/HF endpoint; the
//! live impl ([`ShellAcquirer`]) shells out the way the S83 acquisition path does.

use serde::Deserialize;

use crate::config;

use super::{BackendTag, ModelId};

/// Host VRAM ceiling (GB) used for the fit check. Read from `INTAKE_VRAM_CEILING_GB`
/// via [`vram_ceiling_gb`]; the constant is only the documented default for the
/// current host class (<host> ~96GB), not an infra literal pinned into logic.
const DEFAULT_VRAM_CEILING_GB: f64 = 96.0;

/// Host VRAM ceiling in GB for the fit check (from `INTAKE_VRAM_CEILING_GB`,
/// default [`DEFAULT_VRAM_CEILING_GB`]).
pub fn vram_ceiling_gb() -> f64 {
    std::env::var("INTAKE_VRAM_CEILING_GB")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_VRAM_CEILING_GB)
}

// ===========================================================================
// Nomination model (ASMT-08 `nominations.json`)
// ===========================================================================

/// gfx1151 runnability class carried by each ASMT-08 nomination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gfx1151Class {
    /// Dense / Vulkan-validated — Ollama GPU first, then CPU.
    Confirmed,
    /// MoE-on-Vulkan likely to hang — try ROCm + HSA override; may still skip.
    Experimental,
    /// Needs the bounded smoke test to decide.
    Unknown,
}

impl Gfx1151Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Gfx1151Class::Confirmed => "confirmed",
            Gfx1151Class::Experimental => "experimental",
            Gfx1151Class::Unknown => "unknown",
        }
    }
}

/// How a model's weights are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionPath {
    /// `ollama pull <id>` — the model is in the Ollama library.
    OllamaPull,
    /// The GGUF is already staged on the span/NAS; just register it (no fetch).
    RegisterSpan,
    /// Sharded / Hugging Face model fetched via the S83 `gguf_path` binary.
    HfFetch,
}

/// One nomination record from ASMT-08's `nominations.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Nomination {
    /// S83-byte-identical model id (the chord registry key / `model_name`).
    pub id: String,
    /// Total parameter size in **billions** (used for the VRAM fit check).
    #[serde(default)]
    pub size_b: f64,
    /// gfx1151 runnability class.
    pub gfx1151_class: Gfx1151Class,
    /// How to acquire the weights.
    pub acquisition: AcquisitionPath,
    /// Optional Hugging Face repo (required for `HfFetch`).
    #[serde(default)]
    pub hf_repo: Option<String>,
    /// Backends this model is tagged to profile on. Empty ⇒ derive from the
    /// gfx1151 class via [`Nomination::backend_strategy`].
    #[serde(default)]
    pub backends: Vec<String>,
    /// Free-text rationale (audit only).
    #[serde(default)]
    pub rationale: String,
}

/// The file shape of `nominations.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Nominations {
    pub nominations: Vec<Nomination>,
}

impl Nominations {
    /// Parse from JSON text.
    pub fn from_json(s: &str) -> Result<Nominations, String> {
        serde_json::from_str(s).map_err(|e| format!("invalid nominations.json: {e}"))
    }

    /// Load from the reliable NAS staging path ([`config::intake_nominations_path`]).
    pub fn load() -> Result<Nominations, String> {
        let path = config::intake_nominations_path()
            .ok_or_else(|| "INTAKE_STAGING_DIR not set — cannot locate nominations.json".to_string())?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read nominations at {path}: {e}"))?;
        Self::from_json(&raw)
    }
}

impl Nomination {
    /// The model id as the S83-byte-identical [`ModelId`] (pass-through).
    pub fn model_id(&self) -> ModelId {
        ModelId::from_registry_key(self.id.clone())
    }

    /// Ordered backend strategy for this nomination: the `(BackendTag, override)`
    /// passes the runner drives, each as the P5 `set_backend_override` argument.
    ///
    /// When the nomination carries explicit `backends`, those map verbatim; when
    /// empty, the strategy is derived from the gfx1151 class:
    ///   - confirmed → GPU (`llama-gpu`) then CPU (`ollama`),
    ///   - experimental → GPU only, on ROCm (still `llama-gpu`; the HSA override is
    ///     applied as an env at bring-up by the acquirer), then CPU,
    ///   - unknown → GPU then CPU (the smoke gate decides whether the suite runs).
    ///
    /// The `&'static str` is the exact override string `set_backend_override`
    /// expects (`"llama-gpu"` | `"ollama"`), matching the S83 both-hardware path.
    pub fn backend_strategy(&self) -> Vec<(BackendTag, &'static str)> {
        if !self.backends.is_empty() {
            return self
                .backends
                .iter()
                .filter_map(|b| match b.as_str() {
                    "gpu" | "llama-gpu" => Some((BackendTag::Gpu, "llama-gpu")),
                    "cpu" | "ollama" => Some((BackendTag::Cpu, "ollama")),
                    _ => None,
                })
                .collect();
        }
        // Default: both passes (GPU largest-first, CPU for the small models) —
        // matching S83's both-hardware sizing comparison.
        vec![(BackendTag::Gpu, "llama-gpu"), (BackendTag::Cpu, "ollama")]
    }

    /// Rough VRAM footprint (GB) for the fit check. Q4-class weights run ~0.6
    /// GB/B-param; this is the conservative ceiling check, not a precise loader
    /// estimate. A model whose footprint exceeds [`vram_ceiling_gb`] is skipped.
    pub fn vram_footprint_gb(&self) -> f64 {
        // ~0.6 GB per billion params at Q4 (matches the S83 sizing heuristic).
        self.size_b * 0.6
    }

    /// True when this nomination cannot fit the host VRAM ceiling on the GPU pass
    /// (the Command A+ 218B case → clean skip-with-reason on the GPU backend).
    pub fn exceeds_vram(&self) -> bool {
        self.size_b > 0.0 && self.vram_footprint_gb() > vram_ceiling_gb()
    }
}

// ===========================================================================
// Acquisition outcome
// ===========================================================================

/// Result of acquiring a model's weights (before any profiling). A failure is a
/// recorded skip-with-reason, NEVER an error that aborts the run.
#[derive(Debug, Clone, PartialEq)]
pub enum AcquisitionOutcome {
    /// Weights ready; `local_path` is where the loader reads them (span, with NAS
    /// fallback already resolved) or `None` for an Ollama-managed model.
    Ready { local_path: Option<String> },
    /// Acquisition declined/failed cleanly — record the reason and skip the model.
    Skipped { reason: String },
}

impl AcquisitionOutcome {
    pub fn is_ready(&self) -> bool {
        matches!(self, AcquisitionOutcome::Ready { .. })
    }

    pub fn skip_reason(&self) -> Option<&str> {
        match self {
            AcquisitionOutcome::Skipped { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Resolve the model-load root: prefer the local span, fall back to the NAS (the
/// card-drop recovery path). `None` when neither is configured.
pub fn model_load_root() -> Option<String> {
    config::intake_model_span_dir().or_else(config::intake_model_nas_dir)
}

// ===========================================================================
// Acquirer trait (live shell-out; mocked in tests)
// ===========================================================================

/// The acquisition surface the runner depends on. The live impl
/// ([`ShellAcquirer`]) shells out to `ollama` / the `gguf_path` binary the way the
/// S83 acquisition path does; tests inject a deterministic mock so the runner is
/// hermetic. Implementations MUST map every failure to
/// `AcquisitionOutcome::Skipped` (never panic, never abort the run).
#[async_trait::async_trait]
pub trait Acquirer: Send + Sync {
    /// Acquire `nom`'s weights per its [`AcquisitionPath`], honoring resilient
    /// staging. Pre-checked for VRAM by the runner, but a defensive re-check here
    /// is allowed.
    async fn acquire(&self, nom: &Nomination) -> AcquisitionOutcome;
}

/// Live acquirer: ollama-pull / register-span / HF-fetch via `gguf_path`,
/// honoring the gfx1151 class (Vulkan first; experimental MoE via ROCm + HSA
/// override). Read-heavy loads come from [`model_load_root`] (span→NAS); the
/// `gguf_path` binary name comes from [`config::gguf_path_bin`].
pub struct ShellAcquirer;

#[async_trait::async_trait]
impl Acquirer for ShellAcquirer {
    async fn acquire(&self, nom: &Nomination) -> AcquisitionOutcome {
        if nom.exceeds_vram() {
            return AcquisitionOutcome::Skipped {
                reason: format!(
                    "exceeds VRAM ceiling: ~{:.0}GB footprint > {:.0}GB host ceiling",
                    nom.vram_footprint_gb(),
                    vram_ceiling_gb()
                ),
            };
        }
        match nom.acquisition {
            AcquisitionPath::OllamaPull => self.ollama_pull(nom).await,
            AcquisitionPath::RegisterSpan => self.register_span(nom),
            AcquisitionPath::HfFetch => self.hf_fetch(nom).await,
        }
    }
}

impl ShellAcquirer {
    async fn ollama_pull(&self, nom: &Nomination) -> AcquisitionOutcome {
        // `ollama pull <id>` — Ollama manages the blob store; no local_path needed.
        let status = tokio::process::Command::new("ollama")
            .arg("pull")
            .arg(&nom.id)
            .status()
            .await;
        match status {
            Ok(s) if s.success() => AcquisitionOutcome::Ready { local_path: None },
            Ok(s) => AcquisitionOutcome::Skipped {
                reason: format!("ollama pull failed (exit {:?})", s.code()),
            },
            Err(e) => AcquisitionOutcome::Skipped {
                reason: format!("ollama pull could not start: {e}"),
            },
        }
    }

    fn register_span(&self, nom: &Nomination) -> AcquisitionOutcome {
        // The GGUF is already staged: resolve the load root (span→NAS) and expect
        // `<root>/<id>.gguf`. A missing root is a clean skip, not a crash.
        match model_load_root() {
            Some(root) => {
                let path = format!("{}/{}.gguf", root.trim_end_matches('/'), sanitize_id(&nom.id));
                if std::path::Path::new(&path).exists() {
                    AcquisitionOutcome::Ready {
                        local_path: Some(path),
                    }
                } else {
                    AcquisitionOutcome::Skipped {
                        reason: format!("span GGUF not found at {path}"),
                    }
                }
            }
            None => AcquisitionOutcome::Skipped {
                reason: "no model load root configured (INTAKE_MODEL_SPAN_DIR / _NAS_DIR)".into(),
            },
        }
    }

    async fn hf_fetch(&self, nom: &Nomination) -> AcquisitionOutcome {
        let repo = match &nom.hf_repo {
            Some(r) => r,
            None => {
                return AcquisitionOutcome::Skipped {
                    reason: "HfFetch nomination missing hf_repo".into(),
                }
            }
        };
        let root = match model_load_root() {
            Some(r) => r,
            None => {
                return AcquisitionOutcome::Skipped {
                    reason: "no model load root configured for HF fetch".into(),
                }
            }
        };
        // Resumable by construction: the gguf_path binary re-fetches into the same
        // staged path, so a mid-download crash resumes rather than restarting the
        // whole run (spec EDGE CASE).
        let out_dir = format!("{}/{}", root.trim_end_matches('/'), sanitize_id(&nom.id));
        let mut cmd = tokio::process::Command::new(config::gguf_path_bin());
        cmd.arg("--repo").arg(repo).arg("--out").arg(&out_dir);
        // Experimental MoE on the gfx1151 class: bring up on ROCm with the HSA
        // override so the loader recognizes the GPU. Applied as an env at the
        // acquisition boundary; the value comes from config, never a literal.
        if nom.gfx1151_class == Gfx1151Class::Experimental {
            if let Some(hsa) = config::hsa_override_gfx_version() {
                cmd.env("HSA_OVERRIDE_GFX_VERSION", hsa);
            }
        }
        match cmd.status().await {
            Ok(s) if s.success() => AcquisitionOutcome::Ready {
                local_path: Some(out_dir),
            },
            Ok(s) => AcquisitionOutcome::Skipped {
                reason: format!("gguf_path HF fetch failed (exit {:?})", s.code()),
            },
            Err(e) => AcquisitionOutcome::Skipped {
                reason: format!("gguf_path binary could not start: {e}"),
            },
        }
    }
}

/// Make a model id filesystem-safe for a staged path (`:` and `/` → `_`).
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c == ':' || c == '/' { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nom(id: &str, size_b: f64, class: Gfx1151Class, acq: AcquisitionPath) -> Nomination {
        Nomination {
            id: id.to_string(),
            size_b,
            gfx1151_class: class,
            acquisition: acq,
            hf_repo: None,
            backends: vec![],
            rationale: String::new(),
        }
    }

    #[test]
    fn nominations_parse_from_json() {
        let raw = r#"{
          "nominations": [
            {"id":"command-r:35b","size_b":35,"gfx1151_class":"confirmed","acquisition":"ollama_pull"},
            {"id":"command-a-plus:218b","size_b":218,"gfx1151_class":"experimental","acquisition":"hf_fetch","hf_repo":"cohere/command-a-plus"}
          ]
        }"#;
        let n = Nominations::from_json(raw).expect("parses");
        assert_eq!(n.nominations.len(), 2);
        assert_eq!(n.nominations[0].model_id().as_str(), "command-r:35b");
        assert_eq!(n.nominations[1].gfx1151_class, Gfx1151Class::Experimental);
    }

    #[test]
    fn model_id_is_byte_identical_passthrough() {
        // S83 join correctness: no normalization of the nominated id.
        let n = nom("Qwen3.6:32B", 32.0, Gfx1151Class::Experimental, AcquisitionPath::OllamaPull);
        assert_eq!(n.model_id().as_str(), "Qwen3.6:32B");
    }

    #[test]
    fn command_a_plus_exceeds_vram_clean_skip() {
        // 218B → ~131GB footprint > 96GB ceiling → flagged for skip-with-reason.
        std::env::remove_var("INTAKE_VRAM_CEILING_GB");
        let big = nom("command-a-plus:218b", 218.0, Gfx1151Class::Experimental, AcquisitionPath::HfFetch);
        assert!(big.exceeds_vram());
        let fits = nom("command-r:35b", 35.0, Gfx1151Class::Confirmed, AcquisitionPath::OllamaPull);
        assert!(!fits.exceeds_vram());
    }

    #[test]
    fn shell_acquirer_skips_over_vram_without_touching_network() {
        // Even the live acquirer must skip an over-VRAM model BEFORE any shell-out.
        std::env::remove_var("INTAKE_VRAM_CEILING_GB");
        let big = nom("command-a-plus:218b", 218.0, Gfx1151Class::Experimental, AcquisitionPath::HfFetch);
        let outcome = futures_block_on(ShellAcquirer.acquire(&big));
        assert!(!outcome.is_ready());
        assert!(outcome.skip_reason().unwrap().contains("VRAM"));
    }

    #[test]
    fn backend_strategy_defaults_to_both_passes() {
        let n = nom("m:8b", 8.0, Gfx1151Class::Confirmed, AcquisitionPath::OllamaPull);
        let s = n.backend_strategy();
        assert_eq!(s, vec![(BackendTag::Gpu, "llama-gpu"), (BackendTag::Cpu, "ollama")]);
    }

    #[test]
    fn backend_strategy_honors_explicit_tags() {
        let mut n = nom("m:8b", 8.0, Gfx1151Class::Confirmed, AcquisitionPath::OllamaPull);
        n.backends = vec!["cpu".into()];
        assert_eq!(n.backend_strategy(), vec![(BackendTag::Cpu, "ollama")]);
        n.backends = vec!["llama-gpu".into(), "ollama".into()];
        assert_eq!(
            n.backend_strategy(),
            vec![(BackendTag::Gpu, "llama-gpu"), (BackendTag::Cpu, "ollama")]
        );
    }

    #[test]
    fn sanitize_id_makes_path_safe() {
        assert_eq!(sanitize_id("qwen3:8b"), "qwen3_8b");
        assert_eq!(sanitize_id("org/model:tag"), "org_model_tag");
    }

    // tiny hermetic block-on so the sync test can drive the async skip path.
    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
