//! Model intake profiling framework (S83 MINT-01).
//!
//! Three terminus tools that profile any fleet model and store results in the
//! shared Postgres (same DB as nexus / reminders):
//!   - `model_intake`         — run profiling suites (context implemented; code
//!                              and agent suites are MINT-02/03, stubbed).
//!   - `model_intake_status`  — return the stored operational profile.
//!   - `model_intake_compare` — comparison table across models for one metric.
//!
//! The context suite embeds real repo files as filler (the tool runs on <host>
//! with no repo checkout), plants three recall facts at 25/50/75% depth, runs
//! the model through Ollama's `/api/generate`, and measures throughput, TTFT,
//! recall, and VRAM per graduated context tier. A derived operational profile
//! (safe/absolute context, degradation point, recommended timeouts) is computed
//! and stored after the run.
//!
//! VRAM policy: single hot model. If the target is already hot (the gpt-oss:20b
//! smoke case) no load/unload happens; otherwise the prior hot model is restored
//! after the run.

mod agent;
pub mod assistant;
mod code;
mod code_v2;
mod context;
pub mod infer;
pub mod lifecycle;
pub mod newcats;
mod runner;
pub mod serving;
mod storage;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// Re-export pure pieces for cross-module/integration reference.
pub use runner::{FULL_TIERS, SMOKE_TIERS};
pub use code_v2::{run_code_suite_v2, CodeV2Outcome};
pub use runner::create_profile_row;

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
                    "description": "Which code-suite harness to run. 'v2' (default) is the realistic build-scenario harness: real files + spec in context, graduated 0-5 score, retry, rows tagged harness_version='v2'. 'v1' is the legacy cold one-shot suite (additive, original rows)."
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
                let res = code_v2::run_code_suite_v2(model_name, &code_langs, pid, case_limit, None, None).await?;
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
    fn registration_adds_three_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("model_intake"));
        assert!(reg.contains("model_intake_status"));
        assert!(reg.contains("model_intake_compare"));
    }
}
