//! Model Advisor tools — ported from the Python `model_advisor_tools.py` on
//! the fleet host (DT.7). Recommends model fleets based on available VRAM/unified
//! memory and use case, checks whether a specific model+quant fits a given
//! VRAM budget, and cross-references an Ollama instance's installed models
//! against the capability matrix.
//!
//! Unlike the SSH-based modules in this crate, Model Advisor is pure local
//! data (bundled YAML) plus one outbound HTTP call (`model_advisor_query_ollama`
//! hits the caller-specified Ollama host's `/api/tags`).
//!
//! ## Tools (identical names to the Python source)
//!   model_advisor_recommend   — recommend a model fleet for VRAM + use case
//!   model_advisor_check_fit   — check if a model+quant fits in a VRAM budget
//!   model_advisor_query_ollama — cross-reference installed vs. recommended models
//!
//! ## Configuration (environment only — no hardcoded paths)
//!   MODEL_PRESETS_PATH — override path to `model_presets.yaml` on disk.
//!                         Unset -> use the bundled default (compiled in via
//!                         `include_str!`, mirrors the shipped `deploy/` copy).
//!   MODEL_MATRIX_PATH   — override path to `model_matrix.yaml` on disk.
//!                         Unset -> use the bundled default.
//!   OLLAMA_HOST         — default Ollama base URL for `model_advisor_query_ollama`
//!                         when the `ollama_host` argument is empty. Default
//!                         "http://localhost:11434" (matches the Python source; // pii-test-fixture
//!                         this is a well-known local-loopback default, not an
//!                         infra secret).
//!
//! A path override that fails to read/parse degrades to an empty preset/matrix
//! map — mirroring the Python source, which silently returns `{}` when the
//! YAML file is missing or the `yaml` module isn't importable. Callers see
//! `preset_name: "cpu_only"` (the safe fallback) rather than a hard error.

use std::collections::HashMap;
use std::env;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Bundled defaults
// ---------------------------------------------------------------------------

const DEFAULT_PRESETS_YAML: &str = include_str!("data/model_presets.yaml");
const DEFAULT_MATRIX_YAML: &str = include_str!("data/model_matrix.yaml");

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    pub role: String,
    #[allow(dead_code)]
    pub quant: String,
    pub vram_gb: f64,
    #[allow(dead_code)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPreset {
    #[allow(dead_code)]
    pub description: String,
    pub vram_allocation_gb: f64,
    #[serde(default)]
    pub ollama_env: HashMap<String, String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuantInfo {
    pub vram_gb: f64,
    #[serde(default)]
    pub quality_penalty: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatrixEntry {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub quants: HashMap<String, QuantInfo>,
    #[serde(default)]
    pub quality: Value,
    #[serde(default)]
    pub best_for: Vec<String>,
    #[serde(default)]
    pub ollama_name: Option<String>,
    // CONST-21 additions (Model Library identity section, spec §6.1/§8): these
    // fields were always present in `data/model_matrix.yaml` (see its header
    // comment) but never deserialized because nothing read them until now.
    // Purely additive `#[serde(default)]` fields — every existing caller of
    // this struct/`load_matrix()` is unaffected.
    #[serde(default)]
    pub params_b: Option<f64>,
    #[serde(default)]
    pub active_b: Option<f64>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub avoid_for: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

type PresetMap = HashMap<String, ModelPreset>;
pub(crate) type MatrixMap = HashMap<String, MatrixEntry>;

/// Load presets: from `MODEL_PRESETS_PATH` if set (empty map on read/parse
/// failure), otherwise the bundled default.
fn load_presets() -> PresetMap {
    match env::var("MODEL_PRESETS_PATH").ok().filter(|s| !s.is_empty()) {
        Some(path) => std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_default(),
        None => serde_yaml::from_str(DEFAULT_PRESETS_YAML).unwrap_or_default(),
    }
}

/// Load the model matrix: from `MODEL_MATRIX_PATH` if set (empty map on
/// read/parse failure), otherwise the bundled default.
///
/// `pub(crate)` (CONST-21): the Constellation Model Library identity section
/// (`src/constellation/models_api.rs`) reads the SAME bundled/overridden
/// matrix this module's own tools already use — no second YAML load path, no
/// duplicated env-var handling.
pub(crate) fn load_matrix() -> MatrixMap {
    match env::var("MODEL_MATRIX_PATH").ok().filter(|s| !s.is_empty()) {
        Some(path) => std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_default(),
        None => serde_yaml::from_str(DEFAULT_MATRIX_YAML).unwrap_or_default(),
    }
}

/// Select the best preset name for the given VRAM/platform. Mirrors the
/// Python `_pick_preset_for_vram`.
fn pick_preset_for_vram(presets: &PresetMap, vram_gb: f64, platform: &str) -> String {
    if presets.is_empty() {
        return "cpu_only".to_string();
    }

    let platform_lower = platform.to_lowercase();
    let mut candidates: Vec<(f64, bool, String)> = presets
        .iter()
        .filter_map(|(name, preset)| {
            let alloc = preset.vram_allocation_gb;
            if alloc <= vram_gb {
                let platform_match =
                    platform_lower == "generic" || name.to_lowercase().contains(&platform_lower);
                Some((alloc, platform_match, name.clone()))
            } else {
                None
            }
        })
        .collect();

    if candidates.is_empty() {
        return "cpu_only".to_string();
    }

    // Sort by (alloc, platform_match) descending — mirrors Python's
    // `sort(key=lambda x: (x[0], x[1]), reverse=True)`.
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
    });

    candidates[0].2.clone()
}

/// The role preference order for a use case. Mirrors Python's `use_case_roles`.
fn preferred_roles(use_case: &str) -> Vec<&'static str> {
    match use_case {
        "coding" => vec!["code", "primary"],
        "reasoning" => vec!["reasoning", "primary"],
        "fast" => vec!["fast", "primary"],
        "research" => vec!["reasoning", "primary", "fast"],
        "general" => vec!["primary", "fast", "code"],
        _ => vec!["primary", "fast"],
    }
}

// ---------------------------------------------------------------------------
// Tool: model_advisor_recommend
// ---------------------------------------------------------------------------

pub struct ModelAdvisorRecommend;

#[async_trait]
impl RustTool for ModelAdvisorRecommend {
    fn name(&self) -> &str {
        "model_advisor_recommend"
    }

    fn description(&self) -> &str {
        "Recommend a model fleet based on available VRAM (or unified memory for \
         Apple Silicon / Strix Halo) and use case. Returns preset_name, models, \
         total_vram_gb, and recommended `ollama pull` commands."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vram_gb": {
                    "type": "number",
                    "description": "Available VRAM in GB (or unified memory for Apple Silicon / Strix Halo)"
                },
                "use_case": {
                    "type": "string",
                    "description": "'general', 'coding', 'reasoning', 'fast', or 'research'",
                    "default": "general"
                },
                "platform": {
                    "type": "string",
                    "description": "'strix_halo', 'apple_silicon', 'nvidia', 'amd', or 'generic'",
                    "default": "generic"
                }
            },
            "required": ["vram_gb"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vram_gb = args["vram_gb"]
            .as_f64()
            .ok_or_else(|| ToolError::InvalidArgument("'vram_gb' must be a number".into()))?;
        let use_case = args["use_case"].as_str().unwrap_or("general");
        let platform = args["platform"].as_str().unwrap_or("generic");

        let presets = load_presets();
        let preset_name = pick_preset_for_vram(&presets, vram_gb, platform);
        let empty_preset = ModelPreset {
            description: String::new(),
            vram_allocation_gb: 0.0,
            ollama_env: HashMap::new(),
            models: Vec::new(),
        };
        let preset = presets.get(&preset_name).unwrap_or(&empty_preset);

        let roles = preferred_roles(use_case);
        let mut sorted_models = preset.models.clone();
        sorted_models.sort_by_key(|m| {
            roles
                .iter()
                .position(|r| *r == m.role)
                .unwrap_or(99)
        });

        let pull_commands: Vec<String> = sorted_models
            .iter()
            .map(|m| format!("ollama pull {}", m.name))
            .collect();
        let total_vram: f64 = sorted_models.iter().map(|m| m.vram_gb).sum();
        let headroom = ((vram_gb - total_vram) * 10.0).round() / 10.0;

        let models_json: Vec<Value> = sorted_models
            .iter()
            .map(|m| {
                json!({
                    "name": m.name,
                    "role": m.role,
                    "quant": m.quant,
                    "vram_gb": m.vram_gb,
                    "description": m.description,
                })
            })
            .collect();

        let response = json!({
            "ok": true,
            "preset_name": preset_name,
            "models": models_json,
            "total_model_vram_gb": total_vram,
            "vram_available_gb": vram_gb,
            "headroom_gb": headroom,
            "ollama_pull_commands": pull_commands,
            "ollama_env": preset.ollama_env,
            "note": format!(
                "Use case: {use_case}. All models fit simultaneously with {headroom}GB headroom for KV cache."
            ),
        });

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: model_advisor_check_fit
// ---------------------------------------------------------------------------

pub struct ModelAdvisorCheckFit;

#[async_trait]
impl RustTool for ModelAdvisorCheckFit {
    fn name(&self) -> &str {
        "model_advisor_check_fit"
    }

    fn description(&self) -> &str {
        "Check if a specific model + quantization fits in available VRAM. \
         Returns fits, model_vram_gb, headroom_gb, and a recommendation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model_name": {
                    "type": "string",
                    "description": "Ollama model name (e.g. 'qwen3.5:35b-a3b')"
                },
                "quant": {
                    "type": "string",
                    "description": "Quantization level (e.g. 'Q4_K_M', 'Q8_0', 'F16')",
                    "default": "Q4_K_M"
                },
                "vram_gb": {
                    "type": "number",
                    "description": "Available VRAM in GB",
                    "default": 24.0
                }
            },
            "required": ["model_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let model_name = args["model_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'model_name' must be a string".into()))?;
        let quant = args["quant"].as_str().unwrap_or("Q4_K_M");
        let vram_gb = args["vram_gb"].as_f64().unwrap_or(24.0);

        let matrix = load_matrix();
        let lookup_key = model_name.replace('/', ".");
        let model_info = matrix.get(&lookup_key).or_else(|| matrix.get(model_name));

        let model_info = match model_info {
            Some(info) => info,
            None => {
                let response = json!({
                    "ok": false,
                    "error": format!(
                        "Model '{model_name}' not in matrix. Estimate: ~6GB per 7B params at Q4_K_M."
                    ),
                    "fits": Value::Null,
                });
                return serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")));
            }
        };

        let quant_info = match model_info.quants.get(quant) {
            Some(q) => q,
            None => {
                let available: Vec<&String> = model_info.quants.keys().collect();
                let response = json!({
                    "ok": false,
                    "error": format!(
                        "Quantization '{quant}' not available. Options: {:?}", available
                    ),
                    "fits": Value::Null,
                });
                return serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")));
            }
        };

        let model_vram = quant_info.vram_gb;
        let fits = model_vram <= vram_gb;
        let headroom = ((vram_gb - model_vram) * 10.0).round() / 10.0;

        let recommendation = if fits {
            format!("\u{2713} Fits with {headroom}GB headroom for KV cache")
        } else {
            let shortage = ((model_vram - vram_gb) * 10.0).round() / 10.0;
            let mut alt_candidates: Vec<(&String, f64)> = model_info
                .quants
                .iter()
                .filter(|(_, info)| info.vram_gb <= vram_gb)
                .map(|(q, info)| (q, info.vram_gb))
                .collect();
            alt_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((best_alt, alt_vram)) = alt_candidates.first() {
                format!("\u{2717} Doesn't fit ({shortage}GB short). Try {best_alt} ({alt_vram}GB).")
            } else {
                let min_vram = model_info
                    .quants
                    .values()
                    .map(|q| q.vram_gb)
                    .fold(f64::INFINITY, f64::min);
                format!("\u{2717} No quant fits in {vram_gb}GB. Minimum: {min_vram}GB")
            }
        };

        let response = json!({
            "ok": true,
            "model": model_name,
            "quant": quant,
            "model_vram_gb": model_vram,
            "vram_available_gb": vram_gb,
            "fits": fits,
            "headroom_gb": headroom,
            "recommendation": recommendation,
            "quality_penalty": quant_info.quality_penalty,
        });

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: model_advisor_query_ollama
// ---------------------------------------------------------------------------

pub struct ModelAdvisorQueryOllama;

#[async_trait]
impl RustTool for ModelAdvisorQueryOllama {
    fn name(&self) -> &str {
        "model_advisor_query_ollama"
    }

    fn description(&self) -> &str {
        "Query an Ollama instance to see what's installed vs. what fits/is \
         recommended by the model matrix. Returns installed_models and a \
         matrix cross-reference summary."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ollama_host": {
                    "type": "string",
                    "description": "Ollama API base URL (default: OLLAMA_HOST env var or http://localhost:11434)", // pii-test-fixture
                    "default": ""
                },
                "vram_gb": {
                    "type": "number",
                    "description": "Available VRAM for a fit check (0 = skip fit check)",
                    "default": 0
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let host_arg = args["ollama_host"].as_str().unwrap_or("");
        let vram_gb = args["vram_gb"].as_f64().unwrap_or(0.0);

        let host = if !host_arg.is_empty() {
            host_arg.to_string()
        } else {
            env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string()) // pii-test-fixture
        };
        let host = host.trim_end_matches('/').to_string();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;

        // Mirrors the established convention elsewhere in this crate (e.g.
        // `weather::mod`): both an unreachable host and a non-2xx response are
        // surfaced as `Err(ToolError::Http(_))`, not a synthetic `{ok:false}`
        // success body — a tool-execution error is genuinely an error, not a
        // result to render.
        let resp = client
            .get(format!("{host}/api/tags"))
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Cannot reach Ollama at {host}: {e}")))?;

        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Cannot reach Ollama at {host}: HTTP {}",
                resp.status()
            )));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Cannot parse Ollama response: {e}")))?;

        let installed: Vec<String> = data["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let matrix = load_matrix();
        let mut summary: Vec<Value> = Vec::new();
        for (model_name, info) in &matrix {
            let ollama_name = info.ollama_name.clone().unwrap_or_else(|| model_name.clone());
            let is_installed = installed
                .iter()
                .any(|m| ollama_name.contains(m.as_str()) || m.contains(ollama_name.as_str()));

            let mut entry = json!({
                "model": ollama_name,
                "installed": is_installed,
                "quality": info.quality,
                "best_for": info.best_for.iter().take(3).collect::<Vec<_>>(),
            });

            if vram_gb > 0.0 {
                let q4 = info.quants.get("Q4_K_M");
                let fits_q4km = q4.map(|q| q.vram_gb <= vram_gb).unwrap_or(false);
                entry["fits_q4km"] = json!(fits_q4km);
                entry["vram_q4km"] = match q4 {
                    Some(q) => json!(q.vram_gb),
                    None => json!("?"),
                };
            }
            summary.push(entry);
        }

        let response = json!({
            "ok": true,
            "ollama_host": host,
            "installed_count": installed.len(),
            "installed_models": installed,
            "matrix_summary": summary,
        });

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Model Advisor tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(ModelAdvisorRecommend));
    let _ = registry.register(Box::new(ModelAdvisorCheckFit));
    let _ = registry.register(Box::new(ModelAdvisorQueryOllama));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    // --- bundled data loads -----------------------------------------------

    #[test]
    fn test_default_presets_parse() {
        let presets = load_presets();
        assert!(presets.contains_key("cpu_only"));
        assert!(presets.contains_key("strix_halo_96"));
    }

    #[test]
    fn test_default_matrix_parses() {
        let matrix = load_matrix();
        assert!(matrix.contains_key("qwen3.5:9b"));
        let entry = &matrix["qwen3.5:9b"];
        assert!(entry.quants.contains_key("Q4_K_M"));
    }

    #[test]
    #[serial]
    fn test_missing_override_path_yields_empty_map() {
        std::env::set_var("MODEL_PRESETS_PATH", "/nonexistent/does-not-exist.yaml");
        let presets = load_presets();
        assert!(presets.is_empty());
        std::env::remove_var("MODEL_PRESETS_PATH");
    }

    // --- pick_preset_for_vram ------------------------------------------

    #[test]
    fn test_pick_preset_empty_presets_is_cpu_only() {
        let empty: PresetMap = HashMap::new();
        assert_eq!(pick_preset_for_vram(&empty, 24.0, "generic"), "cpu_only");
    }

    #[test]
    fn test_pick_preset_no_candidates_fits_is_cpu_only() {
        let presets = load_presets();
        // 0.5GB fits nothing except cpu_only (vram_allocation_gb: 0)
        let name = pick_preset_for_vram(&presets, 0.5, "generic");
        assert_eq!(name, "cpu_only");
    }

    #[test]
    fn test_pick_preset_picks_highest_fitting_allocation() {
        let presets = load_presets();
        // 96GB fits strix_halo_96 (72) but not strix_halo_128 (96 <= 96 actually fits)
        let name = pick_preset_for_vram(&presets, 24.0, "generic");
        assert_eq!(name, "discrete_24gb");
    }

    #[test]
    fn test_pick_preset_prefers_platform_match_on_tie() {
        // Construct a tie: two presets with identical vram_allocation_gb, one
        // matching platform "amd" and one not.
        let mut presets: PresetMap = HashMap::new();
        presets.insert(
            "amd_box".to_string(),
            ModelPreset {
                description: "amd".into(),
                vram_allocation_gb: 20.0,
                ollama_env: HashMap::new(),
                models: vec![],
            },
        );
        presets.insert(
            "generic_box".to_string(),
            ModelPreset {
                description: "generic".into(),
                vram_allocation_gb: 20.0,
                ollama_env: HashMap::new(),
                models: vec![],
            },
        );
        let name = pick_preset_for_vram(&presets, 20.0, "amd");
        assert_eq!(name, "amd_box");
    }

    // --- preferred_roles --------------------------------------------------

    #[test]
    fn test_preferred_roles_coding() {
        assert_eq!(preferred_roles("coding"), vec!["code", "primary"]);
    }

    #[test]
    fn test_preferred_roles_unknown_use_case_defaults() {
        assert_eq!(preferred_roles("not-a-real-use-case"), vec!["primary", "fast"]);
    }

    // --- tool metadata ------------------------------------------------

    #[test]
    fn test_recommend_metadata() {
        let tool = ModelAdvisorRecommend;
        assert_eq!(tool.name(), "model_advisor_recommend");
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "vram_gb"));
    }

    #[test]
    fn test_check_fit_metadata() {
        let tool = ModelAdvisorCheckFit;
        assert_eq!(tool.name(), "model_advisor_check_fit");
    }

    #[test]
    fn test_query_ollama_metadata() {
        let tool = ModelAdvisorQueryOllama;
        assert_eq!(tool.name(), "model_advisor_query_ollama");
    }

    // --- model_advisor_recommend: happy path --------------------------

    #[tokio::test]
    async fn test_recommend_happy_path_discrete_24gb() {
        let tool = ModelAdvisorRecommend;
        let result = tool
            .execute(json!({"vram_gb": 24.0, "use_case": "coding", "platform": "generic"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["preset_name"], "discrete_24gb");
        assert!(v["models"].as_array().unwrap().len() >= 1);
        assert!(v["ollama_pull_commands"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .starts_with("ollama pull"));
    }

    #[tokio::test]
    async fn test_recommend_missing_vram_gb_rejected() {
        let tool = ModelAdvisorRecommend;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_recommend_zero_vram_is_cpu_only() {
        let tool = ModelAdvisorRecommend;
        let result = tool.execute(json!({"vram_gb": 0.0})).await.unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["preset_name"], "cpu_only");
    }

    // --- model_advisor_check_fit: happy path + edge case ---------------

    #[tokio::test]
    async fn test_check_fit_happy_path_fits() {
        let tool = ModelAdvisorCheckFit;
        let result = tool
            .execute(json!({"model_name": "qwen3.5:9b", "quant": "Q4_K_M", "vram_gb": 24.0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["fits"], true);
        assert_eq!(v["model_vram_gb"], 6.0);
    }

    #[tokio::test]
    async fn test_check_fit_unknown_model_returns_ok_false() {
        let tool = ModelAdvisorCheckFit;
        let result = tool
            .execute(json!({"model_name": "totally-not-a-model", "vram_gb": 24.0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["fits"].is_null());
    }

    #[tokio::test]
    async fn test_check_fit_does_not_fit_suggests_alternative() {
        let tool = ModelAdvisorCheckFit;
        // qwen3.5:122b-a10b Q4_K_M needs 45GB, IQ4_XS needs 38GB. At 40GB the
        // requested Q4_K_M doesn't fit, but IQ4_XS does — should be suggested.
        let result = tool
            .execute(json!({"model_name": "qwen3.5:122b-a10b", "quant": "Q4_K_M", "vram_gb": 40.0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["fits"], false);
        let rec = v["recommendation"].as_str().unwrap();
        assert!(rec.contains("Doesn't fit"), "unexpected recommendation: {rec}");
        assert!(rec.contains("IQ4_XS"), "expected IQ4_XS alternative: {rec}");
    }

    #[tokio::test]
    async fn test_check_fit_does_not_fit_no_alternative_available() {
        let tool = ModelAdvisorCheckFit;
        // At 20GB, neither Q4_K_M (45GB) nor IQ4_XS (38GB) fits — must report
        // "no quant fits" rather than fabricating an alternative.
        let result = tool
            .execute(json!({"model_name": "qwen3.5:122b-a10b", "quant": "Q4_K_M", "vram_gb": 20.0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["fits"], false);
        let rec = v["recommendation"].as_str().unwrap();
        assert!(rec.contains("No quant fits"), "unexpected recommendation: {rec}");
    }

    #[tokio::test]
    async fn test_check_fit_unknown_quant_lists_options() {
        let tool = ModelAdvisorCheckFit;
        let result = tool
            .execute(json!({"model_name": "qwen3.5:9b", "quant": "Q2_K", "vram_gb": 24.0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("not available"));
    }

    #[tokio::test]
    async fn test_check_fit_missing_model_name_rejected() {
        let tool = ModelAdvisorCheckFit;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- model_advisor_query_ollama -------------------------------------

    #[tokio::test]
    async fn test_query_ollama_happy_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/tags");
            then.status(200).json_body(json!({
                "models": [ { "name": "qwen3.5:9b" }, { "name": "phi4-mini" } ]
            }));
        });

        let tool = ModelAdvisorQueryOllama;
        let result = tool
            .execute(json!({"ollama_host": server.base_url(), "vram_gb": 0}))
            .await
            .unwrap();
        mock.assert();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["installed_count"], 2);
        let summary = v["matrix_summary"].as_array().unwrap();
        let qwen_entry = summary.iter().find(|e| e["model"] == "qwen3.5:9b").unwrap();
        assert_eq!(qwen_entry["installed"], true);
    }

    #[tokio::test]
    async fn test_query_ollama_unreachable_host_returns_error() {
        let tool = ModelAdvisorQueryOllama;
        let err = tool
            .execute(json!({"ollama_host": "http://127.0.0.1:1", "vram_gb": 0}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Http(_)));
    }

    #[tokio::test]
    async fn test_query_ollama_bad_status_returns_error() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/tags");
            then.status(500);
        });

        let tool = ModelAdvisorQueryOllama;
        let err = tool
            .execute(json!({"ollama_host": server.base_url(), "vram_gb": 0}))
            .await
            .unwrap_err();
        mock.assert();
        assert!(matches!(err, ToolError::Http(_)));
    }

    // --- registration -----------------------------------------------------

    #[test]
    fn test_register_adds_three_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
        assert!(registry.contains("model_advisor_recommend"));
        assert!(registry.contains("model_advisor_check_fit"));
        assert!(registry.contains("model_advisor_query_ollama"));
    }
}
