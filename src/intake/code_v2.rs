//! Realistic build-scenario code profiling (S83 MINT-02, harness v2).
//!
//! The v1 suite (see `code.rs`) tested COLD one-shot generation from a task
//! description — something no model in the build pipeline actually does, which
//! is why every model scored 0-1. v2 tests the REAL pipeline scenario:
//!
//!   "Here is a spec item (## Task / ## FILES / ## APPROACH / ## TEST PLAN) and
//!    the CURRENT FULL CONTENTS of the real file(s) it must modify, plus project
//!    context. Output the COMPLETE modified file(s)."
//!
//! Then we apply the output to a fresh /tmp copy of the case's standalone
//! workspace and validate (compile → tests → independent change check), with a
//! graduated 0-5 score and a retry pass when the first attempt nearly works.
//!
//! ## Corpus layout (`INTAKE_CORPUS_V2_DIR`, defaults to the deployed intake-corpus-v2 directory)
//!   manifest.json                 — array of CaseV2 entries
//!   _workspaces/<ws>/             — standalone, dep-minimal real-derived crates
//!   <lang>/<tier>/<case>/spec.md  — the build-pipeline spec item
//!   <lang>/<tier>/<case>/validate.sh — stage-marked validator (see below)
//!
//! A case targets a shared `_workspaces/<ws>` crate and modifies one or more
//! `files` within it. Multiple cases reuse the same workspace via different
//! tasks, so dependency compilation is amortized.
//!
//! ## Fast isolated validation
//! We NEVER build the whole repo per case. For each case the harness copies the
//! workspace SOURCE ONLY (no `target/`) to `/tmp/mint-test-<uuid>/`, applies the
//! model output, and runs `validate.sh` with:
//!   - `MINT_WORK`          = the temp copy (cwd for the validator),
//!   - `MINT_TARGET_CACHE`  = a persistent dir so Rust/TS validators point
//!                            `CARGO_TARGET_DIR`/build cache at PRE-WARMED deps
//!                            and every case is incremental (seconds).
//! Deps are pre-warmed once at deploy time (see `prewarm.sh`).
//!
//! ## validate.sh stage contract
//! The validator prints line markers we parse for graduated scoring:
//!   `STAGE:COMPILE ok|fail`   — did it compile / parse
//!   `STAGE:TESTS ok|fail`     — existing + model-authored tests pass
//!   `STAGE:CHANGE ok|fail`    — independent (hidden) behavior check passes
//!   `TOOLCHAIN:missing <bin>` — degrade gracefully (exit 3, score NULL)
//! and exits 0 on full pass, 3 when a toolchain is missing, non-zero otherwise.
//!
//! Toolchains live under the deploy tree's toolchains directory on the sweep-harness host; the deploy script
//! puts cargo/node on PATH. Missing toolchain → row error set, score NULL, run
//! continues.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::ToolError;
use crate::intake::code::{extract_files, have_tool, required_toolchain};
use crate::intake::context;
use crate::intake::gpu_authority::GpuLock;
use crate::intake::storage::{self, CodeRunRowV2};

/// Resolve the v2 corpus directory from `INTAKE_CORPUS_V2_DIR`. No
/// compiled-in default (PII remediation 2026-07): required at runtime —
/// fails clean with `NotConfigured` rather than silently pointing at a real
/// sweep-harness host path.
pub fn corpus_v2_dir() -> Result<PathBuf, ToolError> {
    std::env::var("INTAKE_CORPUS_V2_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::NotConfigured("INTAKE_CORPUS_V2_DIR is not set".into()))
}

/// Persistent build-cache root (pre-warmed deps) passed to validators as
/// `MINT_TARGET_CACHE`. Defaults next to the corpus so it survives across runs.
pub fn target_cache_dir() -> Result<PathBuf, ToolError> {
    if let Some(dir) = std::env::var("INTAKE_TARGET_CACHE").ok().filter(|s| !s.trim().is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(corpus_v2_dir()?.join("_target-cache"))
}

/// One v2 manifest entry.
#[derive(Debug, Clone, Deserialize)]
pub struct CaseV2 {
    pub id: String,
    pub language: String,
    /// blitz | standard | deep
    pub tier: String,
    /// Spec-item file (build-pipeline prompt body). Default `spec.md`.
    #[serde(default = "default_spec")]
    pub spec: String,
    /// Validator. Default `validate.sh`.
    #[serde(default = "default_validate")]
    pub validate: String,
    /// Case directory relative to the corpus root (holds spec + validate).
    pub dir: String,
    /// Shared workspace under `_workspaces/` this case modifies.
    pub workspace: String,
    /// Files (relative to the workspace) the model must output complete.
    pub files: Vec<String>,
    /// Per-case inference timeout (seconds). Default by tier.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub task_type: Option<String>,
}

fn default_spec() -> String { "spec.md".to_string() }
fn default_validate() -> String { "validate.sh".to_string() }

impl CaseV2 {
    /// Effective inference timeout (per-case override → tier default), with
    /// an additive reload-cost allowance layered on top for large models
    /// (`super::timeouts::reload_adjusted_timeout_secs` — see that module's
    /// doc for the root cause and reasoning: this DOES NOT change what the
    /// tier/override means for difficulty, it compensates for a separate,
    /// orthogonal Ollama-runner-reload cost this tier table was never
    /// designed to capture).
    pub fn timeout(&self, model_name: &str) -> Duration {
        let secs = self.timeout_s.unwrap_or_else(|| tier_default_timeout(&self.tier));
        let secs = super::timeouts::reload_adjusted_timeout_secs(secs, model_name);
        Duration::from_secs(secs)
    }
}

/// Default inference timeout per tier (blitz 60s, standard 120s, deep 300s).
/// Delegates to the canonical resolver (Phase 2 item 3) — same table, same
/// behavior.
pub fn tier_default_timeout(tier: &str) -> u64 {
    super::timeouts::tier_default_secs(tier)
}

/// Read + parse `manifest.json` from the v2 corpus.
pub fn read_manifest_v2(dir: &Path) -> Result<Vec<CaseV2>, ToolError> {
    let path = dir.join("manifest.json");
    let body = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::NotConfigured(format!("v2 corpus manifest not found at {}: {e}", path.display()))
    })?;
    serde_json::from_str(&body)
        .map_err(|e| ToolError::Execution(format!("v2 manifest parse error: {e}")))
}

/// Filter cases by language (case-insensitive); empty = all.
pub fn filter_by_language(cases: &[CaseV2], languages: &[String]) -> Vec<CaseV2> {
    if languages.is_empty() {
        return cases.to_vec();
    }
    let want: Vec<String> = languages.iter().map(|s| s.trim().to_lowercase()).collect();
    cases
        .iter()
        .filter(|c| want.iter().any(|w| w == &c.language.to_lowercase()))
        .cloned()
        .collect()
}

/// Filter cases down to an explicit id list (exact match); `None` or empty ⇒
/// all. HFIX-06: lets a single case (or a small named set) be re-run to fill a
/// specific result gap, instead of always re-running a model's entire suite.
/// Pure, mirrors `filter_by_language`'s no-op-when-unset convention. Unknown
/// ids are silently absent from the result (not an error) — the caller (the
/// case-rerun tool) reports which requested ids were actually found.
pub fn filter_by_ids(cases: &[CaseV2], ids: Option<&[String]>) -> Vec<CaseV2> {
    let Some(ids) = ids else { return cases.to_vec() };
    if ids.is_empty() {
        return cases.to_vec();
    }
    cases
        .iter()
        .filter(|c| ids.iter().any(|w| w == &c.id))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Build-pipeline prompt (pure)
// ---------------------------------------------------------------------------

/// Build the EXACT build-pipeline prompt: spec item + current full file
/// contents + project/structure context, asking for the COMPLETE modified
/// file(s) with the filepath as a comment on the first line of each code block.
/// Pure — unit-tested.
pub fn build_pipeline_prompt(
    spec: &str,
    workspace: &str,
    files: &[(String, String)],
    structure: &str,
) -> String {
    let mut p = String::new();
    p.push_str(
        "You are a senior engineer working in the lumina-constellation project. \
         You are given a build task (a spec item) and the CURRENT, COMPLETE \
         contents of any existing file(s) it references. Implement the task by \
         editing those files, or by creating the new file(s) named in the spec's \
         FILES section when a target does not yet exist.\n\n",
    );
    p.push_str("=== PROJECT CONTEXT ===\n");
    p.push_str(&format!("Crate/workspace: {workspace}\n"));
    if !structure.trim().is_empty() {
        p.push_str("Structure:\n");
        p.push_str(structure.trim());
        p.push('\n');
    }
    p.push_str("\n=== SPEC ITEM ===\n");
    p.push_str(spec.trim());
    p.push_str("\n\n=== CURRENT FILE CONTENTS ===\n");
    if files.is_empty() {
        p.push_str(
            "\n(No existing target files — every file named in the SPEC ITEM's \
             FILES section is new; create it from scratch.)\n",
        );
    } else {
        for (name, body) in files {
            p.push_str(&format!("\n----- {name} -----\n```\n{body}\n```\n"));
        }
    }
    p.push_str(
        "\n=== OUTPUT FORMAT ===\n\
         Output the COMPLETE, FINAL contents of every file you changed — not a \
         diff, not a snippet. For EACH changed file, put the file path as a \
         comment on the FIRST line inside the code block, then the full file:\n\
         ```\n\
         // <relative/path/to/file>\n\
         <entire file contents>\n\
         ```\n\
         Use `#` for the path comment in Python/Bash/TOML and `//` for \
         Rust/TS/C. Do not abbreviate, do not write `// ... unchanged`. Only emit \
         files you actually changed.\n",
    );
    p
}

/// Build a short workspace structure listing (sorted relative file paths) for
/// the prompt's PROJECT CONTEXT. Best-effort; empty on error. Pure-ish (FS read).
pub fn workspace_structure(ws_dir: &Path) -> String {
    let mut paths = Vec::new();
    collect_files(ws_dir, ws_dir, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter(|p| !p.contains("/target/") && !p.starts_with("target/")
            && !p.contains("node_modules"))
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "target" || name == "node_modules" || name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            collect_files(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Output mapping (pure)
// ---------------------------------------------------------------------------

/// Map extracted (path_hint, code) blocks onto the case's declared `files` by
/// basename / suffix match. Unmatched single-file cases fall back to the one
/// declared file ← the largest block. Returns workspace-relative path → content.
/// Pure — unit-tested.
pub fn map_outputs(
    files: &[String],
    extracted: &[(Option<String>, String)],
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if extracted.is_empty() || files.is_empty() {
        return out;
    }
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();

    for (hint, code) in extracted {
        if let Some(h) = hint {
            let hb = base(h);
            if let Some(target) =
                files.iter().find(|f| base(f) == hb || f.as_str() == h || f.ends_with(h.as_str()))
            {
                out.insert(target.clone(), code.clone());
            }
        }
    }
    if !out.is_empty() {
        return out;
    }

    // No usable markers: single-file case → largest block.
    if files.len() == 1 {
        if let Some((_, code)) = extracted.iter().max_by_key(|(_, c)| c.len()) {
            out.insert(files[0].clone(), code.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Validate-output parsing + graduated scoring (pure)
// ---------------------------------------------------------------------------

/// Parsed stages from a validator's stdout/stderr.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidateStages {
    pub compile: Option<bool>,
    pub tests: Option<bool>,
    pub change: Option<bool>,
    pub toolchain_missing: Option<String>,
}

/// Parse the stage markers from combined validator output. Pure.
pub fn parse_stages(output: &str) -> ValidateStages {
    let mut s = ValidateStages::default();
    for line in output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("STAGE:COMPILE ") {
            s.compile = Some(rest.trim() == "ok");
        } else if let Some(rest) = t.strip_prefix("STAGE:TESTS ") {
            s.tests = Some(rest.trim() == "ok");
        } else if let Some(rest) = t.strip_prefix("STAGE:CHANGE ") {
            s.change = Some(rest.trim() == "ok");
        } else if let Some(rest) = t.strip_prefix("TOOLCHAIN:missing ") {
            s.toolchain_missing = Some(rest.trim().to_string());
        }
    }
    s
}

/// Graduated first-pass score 0-5 from validate stages + the LLM idiom rating.
///
///   5 compiles + tests pass + change correct + idiomatic (judge ≥4)
///   4 compiles + tests pass + change correct (judge <4 or unknown)
///   3 compiles + existing/model tests pass but change incomplete (CHANGE fail)
///   2 compiles but tests fail / partially correct
///   1 doesn't compile but a recognizable attempt was produced
///   0 no usable code / refusal / nothing extracted
///
/// `produced_code` = we extracted at least one mapped output file.
/// Pure — unit-tested.
pub fn graduated_score(
    stages: &ValidateStages,
    quality: Option<f64>,
    produced_code: bool,
) -> i32 {
    if !produced_code {
        return 0;
    }
    match stages.compile {
        Some(false) | None => 1, // attempt produced, doesn't compile / never reached compile
        Some(true) => {
            let tests_ok = stages.tests == Some(true);
            let change_ok = stages.change == Some(true);
            if tests_ok && change_ok {
                if quality.map(|q| q >= 4.0).unwrap_or(false) {
                    5
                } else {
                    4
                }
            } else if tests_ok && !change_ok {
                3
            } else {
                2
            }
        }
    }
}

/// Whether a first-pass score warrants a retry (1 or 2 = compiles-ish but
/// wrong, or doesn't compile but recognizable). Pure.
pub fn should_retry(first_pass: i32) -> bool {
    first_pass == 1 || first_pass == 2
}

// ---------------------------------------------------------------------------
// MINT2-01: tunable measurement factors (launch / sampling / quant config)
// ---------------------------------------------------------------------------

/// The launch/sampling/quant knobs a harness would actually tune, resolved once
/// per sweep run and recorded on every `'v3'` case row so pass-rate can be
/// analyzed against the config that was set, not just the model name. Threaded
/// into [`run_one_case_v2`] and written onto the per-case [`CodeRunRowV2`].
///
/// Resolved from optional env knobs (see [`MeasurementFactors::from_env`]) — the
/// same env-sourced, opt-in convention as [`samples_per_case`]. `quant` always
/// resolves to a concrete string (`"unknown"` when undeclared, NEVER guessed);
/// the sampling/reasoning/context knobs stay `None` when unset so a genuine
/// "unset" is distinguishable from a recorded value (three-state).
#[derive(Debug, Clone)]
pub struct MeasurementFactors {
    /// Quantization tag, e.g. `"Q4_K_M"`; `"unknown"` when undeclared.
    pub quant: String,
    /// Three-state reasoning flag: `Some(true)`/`Some(false)`/`None` (unset).
    pub reasoning_enabled: Option<bool>,
    /// The launched context window (`-c` / `num_ctx`), distinct from the
    /// per-prompt observed `context_tokens`. `None` when not configured.
    pub context_window_launched: Option<i32>,
    /// Sampling temperature; `None` = runtime default (not a recorded value).
    pub temperature: Option<f64>,
    /// Sampling top-p; `None` = runtime default.
    pub top_p: Option<f64>,
}

impl Default for MeasurementFactors {
    fn default() -> Self {
        MeasurementFactors {
            quant: "unknown".to_string(),
            reasoning_enabled: None,
            context_window_launched: None,
            temperature: None,
            top_p: None,
        }
    }
}

impl MeasurementFactors {
    /// Resolve the measurement factors for `model_name` from the optional sweep
    /// env knobs. All are opt-in: an unset/blank value leaves the corresponding
    /// factor at its "unset" default, so a production sweep records honest NULLs
    /// rather than fabricated values.
    ///
    /// - `SWEEP_QUANT` — explicit quant tag; when unset/blank, the quant is
    ///   parsed from the model id ([`parse_quant_from_model_id`]) and falls back
    ///   to `"unknown"` when the id doesn't declare one (never guessed).
    /// - `SWEEP_REASONING_ENABLED` — `1/true/on` → `Some(true)`,
    ///   `0/false/off` → `Some(false)`, unset/other → `None` (three-state).
    /// - `SWEEP_CONTEXT_WINDOW` — the launched `-c`; a positive integer or
    ///   `None`.
    /// - `SWEEP_TEMPERATURE` / `SWEEP_TOP_P` — finite floats or `None`.
    pub fn from_env(model_name: &str) -> Self {
        let quant = env_nonempty("SWEEP_QUANT")
            .or_else(|| parse_quant_from_model_id(model_name))
            .unwrap_or_else(|| "unknown".to_string());
        MeasurementFactors {
            quant,
            reasoning_enabled: env_three_state_bool("SWEEP_REASONING_ENABLED"),
            context_window_launched: env_nonempty("SWEEP_CONTEXT_WINDOW")
                .and_then(|s| s.parse::<i32>().ok())
                .filter(|n| *n > 0),
            temperature: env_finite_f64("SWEEP_TEMPERATURE"),
            top_p: env_finite_f64("SWEEP_TOP_P"),
        }
    }

    /// One-line human summary of the SAMPLING/LAUNCH knobs (model-independent),
    /// for the sweep startup banner. Quant is model-specific so it's omitted here.
    pub fn sampling_summary(&self) -> String {
        format!(
            "reasoning={} context_window={} temperature={} top_p={}",
            self.reasoning_enabled
                .map(|b| if b { "on".to_string() } else { "off".to_string() })
                .unwrap_or_else(|| "unset".to_string()),
            self.context_window_launched
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unset".to_string()),
            self.temperature
                .map(|t| t.to_string())
                .unwrap_or_else(|| "default".to_string()),
            self.top_p
                .map(|p| p.to_string())
                .unwrap_or_else(|| "default".to_string()),
        )
    }
}

/// Read an env var, trimmed, treating blank as unset. Local helper mirroring the
/// `filter(|s| !s.trim().is_empty())` idiom used across this module.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a three-state boolean env knob: `1/true/on/yes` → `Some(true)`,
/// `0/false/off/no` → `Some(false)`, unset/blank/unrecognized → `None`. Pure
/// over the env read so the recognized-token set is unit-testable.
fn env_three_state_bool(key: &str) -> Option<bool> {
    parse_three_state_bool(env_nonempty(key).as_deref())
}

/// Pure token → three-state mapping backing [`env_three_state_bool`].
pub fn parse_three_state_bool(raw: Option<&str>) -> Option<bool> {
    match raw.map(|s| s.trim().to_lowercase()).as_deref() {
        Some("1" | "true" | "on" | "yes") => Some(true),
        Some("0" | "false" | "off" | "no") => Some(false),
        _ => None,
    }
}

/// Read a finite `f64` env knob; `None` for unset/blank/unparsable/non-finite
/// (NaN/±inf are never recorded as a real sampling value).
fn env_finite_f64(key: &str) -> Option<f64> {
    env_nonempty(key)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Parse a known quantization tag out of a model id (e.g. the `Q4_K_M` in
/// `qwen3-coder:30b-a3b-q4_K_M`), returning the CANONICAL tag when one is
/// present. Returns `None` when the id declares no recognizable quant — the
/// caller then records `"unknown"` rather than guessing. Recognizes the common
/// llama.cpp/GGUF tags plus the float formats; matching is case-insensitive.
/// Pure — unit-tested.
pub fn parse_quant_from_model_id(id: &str) -> Option<String> {
    let lower = id.to_lowercase();
    // Ordered longest-first within each family so `q4_k_m` matches before `q4_0`
    // would, and `q4_k_s`/`q4_k_m` before a bare `q4_k`.
    const KNOWN: &[&str] = &[
        "q2_k", "q3_k_s", "q3_k_m", "q3_k_l", "q3_k", "q4_k_s", "q4_k_m", "q4_k", "q4_0", "q4_1",
        "q5_k_s", "q5_k_m", "q5_k", "q5_0", "q5_1", "q6_k", "q8_0", "iq2_xxs", "iq2_xs", "iq3_xxs",
        "iq4_nl", "iq4_xs", "bf16", "fp16", "f16", "fp32", "f32",
    ];
    // Find the longest matching known tag anywhere in the id.
    KNOWN
        .iter()
        .filter(|tag| lower.contains(*tag))
        .max_by_key(|tag| tag.len())
        .map(|tag| canonical_quant(tag))
}

/// Canonicalize a matched lowercase quant token to its conventional casing
/// (`q4_k_m` → `Q4_K_M`, `fp16`/`f16`/`bf16` stay lowercase). Pure.
fn canonical_quant(tag: &str) -> String {
    match tag {
        "bf16" | "fp16" | "f16" | "fp32" | "f32" => tag.to_string(),
        // The `qN_...` GGUF families are conventionally upper-cased.
        _ => tag.to_uppercase(),
    }
}

/// Map a corpus-manifest `tier` to the stored `task_category` factor
/// (`blitz`/`multi_file`/`deep`). This is the ONLY place the category is derived
/// and it derives ONLY from the manifest tier — NEVER from `file_count` — so
/// BLITZ/MULTI/DEEP is a first-class recorded factor (MINT2-01). The manifest's
/// middle tier is `standard`, which maps to the `multi_file` category. An
/// unrecognized tier is recorded verbatim (lowercased), never silently bucketed.
/// Pure — unit-tested.
pub fn task_category_from_tier(tier: &str) -> String {
    match tier.trim().to_lowercase().as_str() {
        "blitz" => "blitz".to_string(),
        "standard" | "multi_file" => "multi_file".to_string(),
        "deep" => "deep".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// MINT2-02: structured failure classification (kill survivorship bias)
// ---------------------------------------------------------------------------

/// Structured classification of WHY a (model × case × config) cell did not yield
/// a full-quality result — or [`FailureClass::None`] when it did. Stored as the
/// stable snake_case [`key`](FailureClass::key) string in
/// `code_profile_runs.failure_class`, so absence-of-data (no row / legacy NULL)
/// and genuine-failure are distinguishable in the data (the opposite of the
/// pre-MINT2-02 survivorship bias, where a timed-out / OOM'd / over-VRAM-skipped
/// cell produced NO row at all).
///
/// The variant NAMES deliberately MIRROR Harmony's `FailureCategory` taxonomy so
/// the two planes speak one language — but this is an intake-LOCAL enum, NOT an
/// import (Harmony is a separate repo; per one-project-per-repo discipline we
/// never take a cross-repo dependency for a shared vocabulary). The one addition
/// beyond Harmony's set is the intake-specific [`NonViableVram`](FailureClass::NonViableVram):
/// a model skipped pre-flight because its footprint exceeds the host VRAM
/// ceiling — a concept the build orchestrator's taxonomy has no equivalent for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// A genuinely clean run: compiled, tests passed, change was correct. This
    /// is written as `"none"` on a `'v3'` success — NEVER left NULL (NULL is
    /// reserved to mean "a legacy / pre-migration row").
    None,
    /// Output was cut off mid-generation (hit the token ceiling).
    Truncation,
    /// The model returned but emitted no usable/mappable code — a refusal or an
    /// empty answer. (Mirrors Harmony's `EmptyDiff`: nothing to apply.)
    EmptyDiff,
    /// Model-authored tests are vacuous / self-satisfying (assert nothing real).
    TautologicalTests,
    /// Produced code that does not compile / parse.
    CompilationError,
    /// Compiles, but the tests (or the independent hidden change-behavior check)
    /// fail.
    TestFailure,
    /// Rejected by a review gate.
    ReviewRejection,
    /// An inference / provider / toolchain error prevented a scored attempt —
    /// including an OOM (see [`classify`](FailureClass::classify): there is no
    /// dedicated OOM variant in the mirrored taxonomy; an OOM is the provider
    /// failing to serve, so it maps here).
    ProviderError,
    /// Exhausted the retry / iteration budget without converging.
    MaxIterations,
    /// Inference exceeded its per-case deadline.
    Timeout,
    /// A pipeline phase made no forward progress (stalled).
    PhaseStall,
    /// A failure that did not match any of the above.
    Unknown,
    /// Intake-specific: skipped pre-flight because the model's footprint exceeds
    /// the host VRAM ceiling — the cell was never attempted, but it EXISTS as a
    /// row (score 0) instead of silently vanishing from the data.
    NonViableVram,
}

impl FailureClass {
    /// The stable snake_case string persisted in `code_profile_runs.failure_class`.
    /// These strings are a DATA CONTRACT (queried by reporting / the catalog);
    /// changing one is a schema-visible change, not a rename.
    pub fn key(self) -> &'static str {
        match self {
            FailureClass::None => "none",
            FailureClass::Truncation => "truncation",
            FailureClass::EmptyDiff => "empty_diff",
            FailureClass::TautologicalTests => "tautological_tests",
            FailureClass::CompilationError => "compilation_error",
            FailureClass::TestFailure => "test_failure",
            FailureClass::ReviewRejection => "review_rejection",
            FailureClass::ProviderError => "provider_error",
            FailureClass::MaxIterations => "max_iterations",
            FailureClass::Timeout => "timeout",
            FailureClass::PhaseStall => "phase_stall",
            FailureClass::Unknown => "unknown",
            FailureClass::NonViableVram => "non_viable_vram",
        }
    }

    /// Map a terminal case outcome to a [`FailureClass`].
    ///
    /// Inputs (all the signals available at a cell's true end):
    /// - `stages`: the validator's parsed compile/tests/change stages — `Some`
    ///   ONLY when an actual scored attempt ran (code was produced and the
    ///   validator executed). `None` when no attempt was scored.
    /// - `error_text`: any free-text infra/inference error recorded on the row
    ///   (timeout message, provider error, toolchain-unavailable, …).
    /// - `oom`: whether generation OOM'd before producing output.
    /// - `skip_reason`: a pre-flight skip reason (over-VRAM), when the cell was
    ///   never attempted at all.
    ///
    /// ## Precedence (deterministic — documented per the spec's edge case)
    /// 1. **Pre-flight skip** wins over everything: a skipped cell was never
    ///    even attempted, so no runtime signal can apply.
    /// 2. **OOM before timeout.** When a run BOTH OOMs and times out, OOM wins:
    ///    it is the EARLIER-observed cause — an OOM aborts generation
    ///    immediately (an explicit resource verdict from the provider), whereas
    ///    a timeout only fires after the full deadline elapses. OOM maps to
    ///    [`ProviderError`](FailureClass::ProviderError) (the mirrored taxonomy
    ///    has no dedicated OOM variant).
    /// 3. **Error text** (timeout vs. other provider error) next.
    /// 4. **Validator stages** (an actually-scored attempt) last.
    /// 5. No signal at all after a clean return ⇒ the model produced nothing
    ///    ([`EmptyDiff`](FailureClass::EmptyDiff)); a clean PASS is only reached
    ///    via the all-ok stages branch above.
    pub fn classify(
        stages: Option<&ValidateStages>,
        error_text: Option<&str>,
        oom: bool,
        skip_reason: Option<&str>,
    ) -> FailureClass {
        // 1. Pre-flight skip — never attempted.
        if let Some(reason) = skip_reason {
            if reason.to_lowercase().contains("vram") {
                return FailureClass::NonViableVram;
            }
            // A skip we don't have a dedicated class for (no current caller
            // routes a non-VRAM skip here, but never silently misreport it).
            return FailureClass::Unknown;
        }
        // 2. OOM before timeout (earliest-observed cause wins).
        if oom {
            return FailureClass::ProviderError;
        }
        // 3. Infra/inference error text.
        if let Some(err) = error_text {
            let e = err.to_lowercase();
            if e.contains("timed out") || e.contains("timeout") || e.contains("deadline") {
                return FailureClass::Timeout;
            }
            return FailureClass::ProviderError;
        }
        // 4. A scored attempt: classify by the most specific failing stage.
        if let Some(st) = stages {
            return match st.compile {
                // Produced code that never compiled (or never reached compile).
                Some(false) | None => FailureClass::CompilationError,
                Some(true) => {
                    if st.tests == Some(false) {
                        FailureClass::TestFailure
                    } else if st.change == Some(false) {
                        // Compiles + tests pass but the independent hidden
                        // behavior check failed — the required change wasn't
                        // actually made. Treated as a test failure (the hidden
                        // check IS a behavior test).
                        FailureClass::TestFailure
                    } else {
                        // compiles + tests + change all ok ⇒ clean.
                        FailureClass::None
                    }
                }
            };
        }
        // 5. Returned cleanly but produced nothing scored — refusal / empty.
        FailureClass::EmptyDiff
    }
}

/// Compute the [`FailureClass`] for a FINISHED per-case row from its terminal
/// state (after any retry — the row's `compiles`/`tests_pass`/`change_correct`
/// already reflect the better attempt). This is the single classification point
/// for a scored/attempted case: called once per row at insert time so EVERY
/// case row (including a timed-out or OOM'd one, which already writes a row with
/// `error`/`oom` set) carries a structured `failure_class`.
///
/// An in-case row is never a pre-flight skip (that path lives in
/// `coder_sweep.rs`), so `skip_reason` is always `None` here.
pub fn classify_case_row(row: &CodeRunRowV2) -> FailureClass {
    let produced = row.well_formed.unwrap_or(false);
    let stages = ValidateStages {
        compile: row.compiles,
        tests: row.tests_pass,
        change: row.change_correct,
        toolchain_missing: None,
    };
    FailureClass::classify(
        if produced { Some(&stages) } else { None },
        row.error.as_deref(),
        row.oom,
        None,
    )
}

/// MINT2-02: write a single terminal `code_profile_runs` row recording that a
/// (model × backend × config) cell was skipped PRE-FLIGHT as non-viable (its
/// footprint exceeds the host VRAM ceiling), instead of the cell silently
/// vanishing from the data (survivorship bias). The row carries a fresh model
/// identity, the backend/mem_config/quant config, score 0, the free-text
/// `reason`, and `failure_class = "non_viable_vram"`. It is FINALIZED (a skip
/// has no follow-up idiom-judge pass) so the gap audit never treats it as
/// forever-incomplete.
///
/// DB URL resolved via the existing `config::intake_database_url()` (through
/// `storage::get_pool()`) — no raw env, no literal DSN. Called from the coder
/// sweep's over-VRAM skip path (via the `CoderSuiteDriver` trait so the fleet
/// loop stays unit-testable without a DB).
pub async fn record_non_viable_vram_row(
    model_name: &str,
    backend_tag: &str,
    reason: &str,
    mem_config: Option<&str>,
) -> Result<(), ToolError> {
    let pool = storage::get_pool().await?;
    let profile_id = storage::insert_model_profile(&pool, model_name, "ollama", None, None).await?;
    // Record the config dimension the same way a scored row would, so
    // "this quant of this model is non-viable" is queryable.
    let factors = MeasurementFactors::from_env(model_name);
    let row = CodeRunRowV2 {
        // No single language/case for a whole-model skip: this row represents
        // the cell's NON-VIABILITY, not a scored case. Empty language + a
        // descriptive task_type keep it self-identifying without inventing a
        // fake case identity.
        language: String::new(),
        task_type: Some("non_viable_skip".into()),
        first_pass_score: Some(0),
        well_formed: Some(false),
        oom: false,
        error: Some(reason.to_string()),
        backend_tag: Some(backend_tag.to_string()),
        mem_config: mem_config.map(str::to_string),
        quant: Some(factors.quant.clone()),
        reasoning_enabled: factors.reasoning_enabled,
        context_window_launched: factors.context_window_launched,
        temperature: factors.temperature,
        top_p: factors.top_p,
        failure_class: Some(FailureClass::NonViableVram.key().to_string()),
        ..Default::default()
    };
    let id = storage::insert_code_run_v2(&pool, profile_id, &row).await?;
    // `insert_code_run_v2` always writes finalized=false (the Phase-1 insert
    // shape); a skip row has no judge follow-up, so finalize it now (this call
    // also stamps first_pass_score=0) — the same finalization path every scored
    // case reaches at the end of its judge pass.
    storage::update_code_run_v2_judge(&pool, id, None, Some(0), None).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-case execution (live)
// ---------------------------------------------------------------------------

/// Outcome of running one v2 case INFERENCE (before the batched idiom judge +
/// DB insert). The idiom judge is DEFERRED to a batch pass at the end of the
/// suite so the test model is never evicted from VRAM per case — one judge swap
/// per model instead of one (or two) per case.
#[derive(Debug, Clone, Default)]
pub struct CaseV2Result {
    pub row: CodeRunRowV2,
    /// Effective score used for approval (retry if it ran, else first pass).
    /// Structural (pre-judge); finalized after the batched idiom judge.
    pub effective_score: i32,
    /// Task spec text, fed to the batched idiom judge.
    pub spec: String,
    /// First-pass response to judge (Some only when code was produced).
    pub first_response: Option<String>,
    /// Retry response to judge (Some only when a retry actually ran + produced).
    pub retry_response: Option<String>,
}

/// Read the case's target files from the workspace source.
fn read_target_files(ws_dir: &Path, files: &[String]) -> (Vec<(String, String)>, i32) {
    let mut out = Vec::new();
    let mut total_lines = 0i32;
    for rel in files {
        if let Ok(body) = std::fs::read_to_string(ws_dir.join(rel)) {
            total_lines += body.lines().count() as i32;
            out.push((rel.clone(), body));
        }
    }
    (out, total_lines)
}

/// Copy the workspace SOURCE (no target/, no node_modules) into a fresh temp
/// dir, returning its path. A self-referential symlink named after the
/// workspace (`<ws> -> .`) is created inside so validators may reference files
/// EITHER at the staged root (`$MINT_WORK/file`) OR via a workspace subdir
/// (`$MINT_WORK/<ws>/file`) — both resolve to the same files. This lets Rust /
/// Python / Bash validators (root-relative) and the TS validators (subdir
/// relative) share one staging convention.
fn stage_workspace(ws_dir: &Path, ws_name: &str) -> Result<PathBuf, ToolError> {
    let tmp = std::env::temp_dir().join(format!("mint-test-{}", uuid::Uuid::new_v4()));
    copy_source_only(ws_dir, &tmp)
        .map_err(|e| ToolError::Execution(format!("stage workspace failed: {e}")))?;
    // Best-effort self-symlink for subdir-relative validators.
    #[cfg(unix)]
    {
        let link = tmp.join(ws_name);
        if !link.exists() {
            let _ = std::os::unix::fs::symlink(".", &link);
        }
    }
    let _ = ws_name;
    Ok(tmp)
}

/// Remove orphaned `mint-test-*` staging dirs left by crashed validators.
/// Best-effort; runs once at suite start so a long fleet run cannot accrete
/// temp dirs under the temp filesystem.
fn sweep_stale_workspaces() {
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().starts_with("mint-test-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Recursive copy skipping build artifacts so the staged copy is tiny.
fn copy_source_only(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n == "target" || n == "node_modules" {
            continue;
        }
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(&name);
        if ty.is_dir() {
            copy_source_only(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Apply outputs and run the validator in a staged copy. Returns combined
/// output + exit code (None = spawn failure). The validator gets `MINT_WORK`
/// and `MINT_TARGET_CACHE`.
async fn apply_and_validate(
    ws_dir: &Path,
    ws_name: &str,
    validate_script: &Path,
    outputs: &BTreeMap<String, String>,
) -> Result<(String, Option<i32>), ToolError> {
    let work = stage_workspace(ws_dir, ws_name)?;
    for (rel, body) in outputs {
        let dest = work.join(rel);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&dest, body)
            .map_err(|e| ToolError::Execution(format!("write output {rel}: {e}")))?;
    }
    let cache = target_cache_dir()?;
    let _ = std::fs::create_dir_all(&cache);

    let out = tokio::process::Command::new("bash")
        .arg(validate_script)
        .current_dir(&work)
        .env("MINT_WORK", &work)
        .env("MINT_TARGET_CACHE", &cache)
        .output()
        .await;

    let res = match out {
        Ok(o) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            Ok((combined, o.status.code()))
        }
        Err(e) => Err(ToolError::Execution(format!("spawn validate: {e}"))),
    };
    let _ = std::fs::remove_dir_all(&work);
    res
}

/// Backoff delays (seconds) between retries of a transport-style error, in
/// order. HFIX-04: a single fixed 10s retry did not survive the SUSTAINED
/// (multi-minute) connectivity windows actually observed on the sweep-harness host — ollama's
/// own runner reloads are fast (all but 4 loads across the whole multi-day
/// sweep history finished under 10s), so a lone 10s wait was retrying too
/// early into a contention window that was still ongoing. Three escalating
/// waits give a slow-clearing window real room to pass before the case is
/// recorded as a hard failure.
const TRANSPORT_RETRY_BACKOFF_SECS: [u64; 3] = [10, 20, 40];

/// Generate, retrying on a transport-style error with escalating backoff
/// (see `TRANSPORT_RETRY_BACKOFF_SECS`). OOM and deterministic errors are not
/// retried. Mirrors the plan's per-case error policy so a transient
/// `error sending request` does not corrupt a row.
async fn generate_with_retry(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
    timeout: Duration,
) -> context::GenOutcome {
    let mut g = context::generate(client, model, prompt, timeout).await;
    for (attempt, delay_secs) in TRANSPORT_RETRY_BACKOFF_SECS.iter().enumerate() {
        if g.oom {
            break;
        }
        let Some(e) = &g.error else { break };
        // Phase 2 item 4: routed through the shared `ErrorClass` classifier
        // instead of an ad hoc `is_transport_error` call — `g.oom` above
        // already gates the OOM case for THIS outcome (set at generation
        // time from a live status code), so only `Transport` should retry
        // here; anything else (including `Other`) falls through to the
        // existing "stop retrying" behavior, unchanged.
        if context::classify_error(e, None) != context::ErrorClass::Transport {
            break;
        }
        tracing::warn!(
            "intake v2: transient inference error ({e}); retry {}/{} in {delay_secs}s",
            attempt + 1,
            TRANSPORT_RETRY_BACKOFF_SECS.len(),
        );
        tokio::time::sleep(Duration::from_secs(*delay_secs)).await;
        g = context::generate(client, model, prompt, timeout).await;
    }
    g
}

/// Run a single v2 case INFERENCE (first pass + optional retry). The idiom
/// judge is NOT called here — responses are returned for the batched judge so
/// the test model stays hot. `row.error` is set ONLY for infrastructure errors
/// and toolchain skips (excluded from averages); a model that simply produces
/// no usable code is a legitimate score-0 row with `error = NULL`.
async fn run_one_case_v2(
    client: &reqwest::Client,
    model_name: &str,
    case: &CaseV2,
    corpus: &Path,
    backend_tag: Option<&str>,
    mem_config: Option<&str>,
    factors: &MeasurementFactors,
) -> CaseV2Result {
    let mut row = CodeRunRowV2 {
        language: case.language.clone(),
        task_type: case.task_type.clone().or_else(|| Some("build_modify".into())),
        backend_tag: backend_tag.map(str::to_string),
        mem_config: mem_config.map(str::to_string),
        case_id: Some(case.id.clone()),
        // MINT2-01: record the tunable factors this run was configured with.
        // `quant` is always a concrete string ("unknown" when undeclared, never
        // guessed); reasoning/context/sampling stay None when unset (three-state).
        quant: Some(factors.quant.clone()),
        reasoning_enabled: factors.reasoning_enabled,
        context_window_launched: factors.context_window_launched,
        temperature: factors.temperature,
        top_p: factors.top_p,
        // `task_category` comes FROM THE MANIFEST tier, never re-derived from
        // file_count — this is the one place it's recorded on the write path.
        task_category: Some(task_category_from_tier(&case.tier)),
        ..Default::default()
    };
    let none = CaseV2Result::default;
    let ws_dir = corpus.join("_workspaces").join(&case.workspace);
    let case_dir = corpus.join(&case.dir);
    let validate_script = case_dir.join(&case.validate);

    // Spec + current file contents.
    let spec = match std::fs::read_to_string(case_dir.join(&case.spec)) {
        Ok(s) => s,
        Err(e) => {
            row.error = Some(format!("read spec failed: {e}"));
            return CaseV2Result { row, ..none() };
        }
    };
    // An infra-broken workspace (bad corpus deploy, typo'd `workspace` field,
    // etc.) means the whole case is unrunnable — that IS an infrastructure
    // error. An EMPTY `files` result is NOT necessarily one: some `## FILES`
    // entries name files the model must CREATE from scratch (e.g. ts-standard-t1
    // targets `wx-ts/typed.ts` + `wx-ts/typed.test.ts`, neither of which exists
    // on disk — the spec explicitly says "Create a new module"). Gating on
    // `files.is_empty()` misclassified every such creation-style case as a
    // "no readable target files" infra error, even though the workspace itself
    // was present and fully readable (HFIX-03).
    if !ws_dir.is_dir() {
        row.error = Some(format!("workspace directory not found: {}", case.workspace));
        return CaseV2Result { row, spec, ..none() };
    }
    let (files, total_lines) = read_target_files(&ws_dir, &case.files);
    row.file_count = Some(case.files.len() as i32);
    row.total_lines = Some(total_lines);

    let structure = workspace_structure(&ws_dir);
    let prompt = build_pipeline_prompt(&spec, &case.workspace, &files, &structure);
    row.context_tokens = Some(context::estimate_tokens(&prompt) as i32);

    // First inference (transient errors retried once).
    let gen = generate_with_retry(client, model_name, &prompt, case.timeout(model_name)).await;
    row.throughput_tok_per_sec = gen.throughput_tok_per_sec;
    row.total_time_ms = gen.total_time_ms;
    row.response_tokens = Some(context::estimate_tokens(&gen.response) as i32);
    if gen.oom {
        row.oom = true;
        row.error = Some(gen.error.unwrap_or_else(|| "OOM".into()));
        return CaseV2Result { row, spec, ..none() };
    }
    if let Some(e) = gen.error {
        // Infrastructure/inference error (survived the retry) — excluded from
        // averages, tagged via the error column (status = error).
        row.error = Some(e);
        return CaseV2Result { row, spec, ..none() };
    }

    // Toolchain gate (degrade gracefully — skip, not error).
    if let Some(bin) = required_toolchain(&case.language) {
        if !have_tool(bin) {
            row.error = Some(format!("toolchain unavailable: {} (needs {bin})", case.language));
            // Score NULL — we cannot validate.
            return CaseV2Result { row, spec, ..none() };
        }
    }

    let extracted = extract_files(&gen.response);
    let outputs = map_outputs(&case.files, &extracted);
    let produced = !outputs.is_empty();

    // multi-point-score-tracking: record well-formedness BEFORE the graduated
    // score is computed, so a 0 score from "nothing extracted"
    // (`well_formed = false`) is distinguishable from "extracted but wrong"
    // (`well_formed = true`, score still 0).
    row.well_formed = Some(produced);

    // security-scan-signal: heuristic vulnerability-pattern scan over the
    // materialized output files. NON-FATAL and SEPARATE from the correctness
    // score — a finding never changes `first_pass_score`/`effective`. `None`
    // (SQL NULL) when nothing was produced OR the language is unsupported by the
    // heuristic; `Some(0)` when scanned clean; `Some(N)` for N findings. This is
    // a coarse heuristic (see `intake::vuln_scan`), not a real SAST tool.
    row.vuln_finding_count = if produced {
        crate::intake::vuln_scan::scan_outputs(
            &case.language,
            outputs.values().map(String::as_str),
        )
    } else {
        None
    };

    // First-pass response to judge later (only when code was produced).
    let mut first_response: Option<String> = None;
    let mut retry_response: Option<String> = None;

    // Structural score uses quality = None; the batched judge applies the
    // idiom rating (and the 4→5 bump) afterwards.
    let (first_pass, stages) = if produced {
        first_response = Some(gen.response.clone());
        match apply_and_validate(&ws_dir, &case.workspace, &validate_script, &outputs).await {
            Ok((out, code)) => {
                if code == Some(3) {
                    // Validator self-reported a missing toolchain.
                    let st = parse_stages(&out);
                    row.error = Some(format!(
                        "toolchain unavailable: {}",
                        st.toolchain_missing.unwrap_or_else(|| case.language.clone())
                    ));
                    return CaseV2Result { row, spec, ..none() };
                }
                let st = parse_stages(&out);
                let score = graduated_score(&st, None, true);
                (score, st)
            }
            Err(e) => {
                // Validator could not be spawned — infrastructure error.
                row.error = Some(format!("validate error: {e}"));
                return CaseV2Result { row, spec, first_response, ..none() };
            }
        }
    } else {
        // Model produced no usable code: a legitimate score 0 (error = NULL,
        // counted in averages), NOT an infrastructure error.
        (0, ValidateStages::default())
    };

    row.first_pass_score = Some(first_pass);
    row.compiles = stages.compile;
    row.tests_pass = stages.tests;
    row.change_correct = stages.change;
    let mut effective = first_pass;

    // ---- Retry pass (only for 1-2) -------------------------------------
    if should_retry(first_pass) && produced {
        let err_excerpt = last_error_excerpt(&ws_dir, &case.workspace, &validate_script, &outputs).await;
        let retry_prompt = build_retry_prompt(&prompt, &gen.response, &err_excerpt);
        let gen2 = generate_with_retry(client, model_name, &retry_prompt, case.timeout(model_name)).await;
        if gen2.error.is_none() && !gen2.oom {
            let extracted2 = extract_files(&gen2.response);
            let outputs2 = map_outputs(&case.files, &extracted2);
            if !outputs2.is_empty() {
                if let Ok((out2, code2)) =
                    apply_and_validate(&ws_dir, &case.workspace, &validate_script, &outputs2).await
                {
                    if code2 != Some(3) {
                        let st2 = parse_stages(&out2);
                        let retry = graduated_score(&st2, None, true);
                        row.retry_score = Some(retry);
                        retry_response = Some(gen2.response.clone());
                        effective = retry.max(first_pass);
                        // Reflect the better attempt's compile/test state.
                        if retry >= first_pass {
                            row.compiles = st2.compile;
                            row.tests_pass = st2.tests;
                            row.change_correct = st2.change;
                        }
                    }
                }
            }
        }
    }

    CaseV2Result { row, effective_score: effective, spec, first_response, retry_response }
}

/// Re-run the validator once to capture a compile/test error excerpt to feed the
/// retry. Best-effort; empty string on failure.
async fn last_error_excerpt(
    ws_dir: &Path,
    ws_name: &str,
    validate_script: &Path,
    outputs: &BTreeMap<String, String>,
) -> String {
    match apply_and_validate(ws_dir, ws_name, validate_script, outputs).await {
        Ok((out, _)) => {
            // Keep the tail (where compiler errors usually are), capped.
            let tail: String = out.lines().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            tail.chars().take(4000).collect()
        }
        Err(_) => String::new(),
    }
}

/// Build the retry prompt: original task + the model's own output + the error.
/// Pure — unit-tested.
pub fn build_retry_prompt(original_prompt: &str, prior_output: &str, error: &str) -> String {
    let mut p = String::new();
    p.push_str(original_prompt.trim());
    p.push_str("\n\n=== YOUR PREVIOUS ATTEMPT ===\n");
    p.push_str(prior_output.trim());
    p.push_str("\n\n=== IT FAILED VALIDATION WITH ===\n");
    p.push_str(error.trim());
    p.push_str(
        "\n\nFix the problem. Output the COMPLETE corrected file(s) again in the \
         same format (file path as the first-line comment in each code block). Do \
         not abbreviate.\n",
    );
    p
}

/// Ask qwen3:8b to rate idiom/error-handling/style 1-5. NULL if unavailable.
async fn judge_idiom(client: &reqwest::Client, spec: &str, response: &str) -> Option<f64> {
    let judge = std::env::var("INTAKE_JUDGE_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    let prompt = format!(
        "You are a strict senior code reviewer. Considering the task, rate the \
         submitted code from 1 to 5 for IDIOM, ERROR-HANDLING, and STYLE combined \
         (5 = idiomatic, robust, clean; 1 = poor). Reply with ONLY the integer.\n\n\
         === TASK ===\n{}\n\n=== SUBMISSION ===\n{}\n\nRating (1-5):",
        spec.trim(),
        response.trim()
    );
    let out = context::generate(client, &judge, &prompt, Duration::from_secs(120)).await;
    if out.error.is_some() {
        return None;
    }
    crate::intake::code::parse_rating(&out.response)
}

// ---------------------------------------------------------------------------
// Approval aggregation (pure)
// ---------------------------------------------------------------------------

/// Per-(language,tier) approval: average effective first_pass_score ≥ 3.0 across
/// the tier's cases. Returns approved "lang:tier" tags sorted+deduped.
/// `results` = (language, tier, effective_score) per case. Pure — unit-tested.
pub fn compute_approvals_v2(results: &[(String, String, i32)]) -> Vec<String> {
    let mut acc: BTreeMap<(String, String), (i64, i64)> = BTreeMap::new();
    for (lang, tier, score) in results {
        let e = acc.entry((lang.to_lowercase(), tier.to_lowercase())).or_insert((0, 0));
        e.0 += *score as i64;
        e.1 += 1;
    }
    let mut tags: Vec<String> = acc
        .into_iter()
        .filter(|(_, (sum, n))| *n > 0 && (*sum as f64 / *n as f64) >= 3.0)
        .map(|((lang, tier), _)| format!("{lang}:{tier}"))
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

// ---------------------------------------------------------------------------
// Suite driver (live)
// ---------------------------------------------------------------------------

/// Outcome of the v2 code suite for the tool summary.
#[derive(Debug, Clone, Default)]
pub struct CodeV2Outcome {
    pub cases_run: usize,
    pub avg_first_pass: f64,
    pub avg_effective: f64,
    pub approved: Vec<String>,
    pub toolchain_skipped: Vec<String>,
    /// Cases scored (error = NULL) — the denominator of the averages.
    pub scored: usize,
    /// Cases tagged as infrastructure/inference errors (error set, not a
    /// toolchain skip) — excluded from the averages.
    pub errors: usize,
    /// Per-case (id, first_pass, retry) for the smoke summary.
    pub per_case: Vec<(String, i32, Option<i32>)>,
}

/// Run the realistic build-scenario code suite. Stores one v2
/// `code_profile_runs` row per case and patches the operational profile's
/// approved_languages with the approved "lang:tier" tags.
///
/// `gpu_lock` (S86 max-lock-hold safety valve): `Some` when the CALLER
/// already holds an exclusive [`GpuLock`] across this whole call (the fleet
/// sweep's per-(model, backend) pass) — after each case in Phase 1 below,
/// `gpu_lock.check_max_hold()` is called so a pass that runs unusually long
/// (e.g. a model with a high transport-error retry rate) yields the lock
/// MID-PASS instead of holding it for the pass's entire, potentially
/// hours-long duration. `None` for callers that don't manage a fleet-level
/// GPU lock around this call at all (the case-rerun tool, breakfix, the
/// legacy single-model path) — the safety valve is purely additive and never
/// activates unless a caller opts in.
pub async fn run_code_suite_v2(
    model_name: &str,
    languages: &[String],
    profile_id: uuid::Uuid,
    case_limit: Option<usize>,
    backend_tag: Option<&str>,
    mem_config: Option<&str>,
    gpu_lock: Option<&dyn GpuLock>,
) -> Result<CodeV2Outcome, ToolError> {
    run_code_suite_v2_cases(
        model_name,
        languages,
        None,
        profile_id,
        case_limit,
        backend_tag,
        mem_config,
        gpu_lock,
    )
    .await
}

/// Resolve the MINT2-01 measurement factors for `model_name` from the sweep env
/// config. A thin named wrapper over [`MeasurementFactors::from_env`] so the
/// suite driver and the fleet entrypoint share one resolution point.
pub fn measurement_factors(model_name: &str) -> MeasurementFactors {
    MeasurementFactors::from_env(model_name)
}

/// Like [`run_code_suite_v2`] but scoped to an explicit `case_ids` subset
/// (HFIX-06). `None`/empty ⇒ every case matching `languages`, i.e. identical
/// behavior to `run_code_suite_v2`. Lets a single case (or a small named set)
/// be re-run to fill a specific result gap without re-running a model's
/// entire suite — the case-rerun tool (`intake_coder_case`) is the intended
/// caller; the fleet sweep (`intake_coder_sweep`) always passes `None`.
///
/// `gpu_lock`: see [`run_code_suite_v2`]'s doc — the case-rerun tool passes
/// `None` (it holds its OWN `ExclusiveGuard` for its typically-small, bounded
/// rerun and has no need for a mid-unit safety valve).
#[allow(clippy::too_many_arguments)]
pub async fn run_code_suite_v2_cases(
    model_name: &str,
    languages: &[String],
    case_ids: Option<&[String]>,
    profile_id: uuid::Uuid,
    case_limit: Option<usize>,
    backend_tag: Option<&str>,
    mem_config: Option<&str>,
    gpu_lock: Option<&dyn GpuLock>,
) -> Result<CodeV2Outcome, ToolError> {
    sweep_stale_workspaces();
    let dir = corpus_v2_dir()?;
    let all = read_manifest_v2(&dir)?;
    let mut cases = filter_by_ids(&filter_by_language(&all, languages), case_ids);
    if let Some(n) = case_limit {
        cases.truncate(n);
    }
    if cases.is_empty() {
        return Err(ToolError::NotConfigured(
            "no v2 code cases match the requested languages/case_ids".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1200))
        .build()
        .map_err(|e| ToolError::Http(format!("client build failed: {e}")))?;
    let pool = storage::get_pool().await?;

    // MINT2-01: resolve the tunable measurement factors ONCE for this run
    // (env-sourced, opt-in — same pattern as `samples_per_case()` above), then
    // record them on every per-case row written below. `quant` is model-specific
    // (parsed from the id when the launch flags don't declare it), so it's keyed
    // on `model_name`; the sampling/launch knobs are run-global.
    let factors = MeasurementFactors::from_env(model_name);

    // ---- Phase 1: inference, with the TEST model hot, NO judging --------
    // INCR-01: persist each case's row IMMEDIATELY after it runs (instead of
    // deferring every insert to the very end of the model's full case suite).
    // A ~40-case suite at real generate() latency (tens of seconds/case) can
    // legitimately take 20-40+ minutes end to end; the old all-at-the-end
    // write pattern meant `code_profile_runs` saw a single 40-row burst once
    // per model and NOTHING in between, which made row-age-based liveness
    // checks (e.g. the host sweep-watchdog, STUCK_THRESHOLD_SEC=360s) treat a
    // healthy, progressing sweep as jammed and restart it mid-suite — and
    // because there is no per-case checkpoint (only a per-MODEL one, written
    // after this whole function returns `Ok`), every such restart discarded
    // all of that model's in-flight progress and restarted its case loop from
    // the top, so a sufficiently slow model could never finish. Writing a row
    // per case (kept as `Some(id)` in `row_ids`) restores a steady trickle
    // matching actual progress, so row-age liveness checks stay accurate.
    // MULTI-SAMPLE-CONSISTENCY: expand the case list to `n_samples` repeats per
    // case (each repeat carries its 0-based `sample_index`). `n_samples`
    // defaults to 1 (`samples_per_case()`), so the flat `sampled` list is
    // identical to `cases` unless `INTAKE_SAMPLES_PER_CASE` is set > 1 — Phases
    // 1/2/3 below iterate `sampled` instead of `cases`, keeping the one-row-per
    // (case, sample) structure that `row_ids`/`pending` are indexed by. Each
    // repeat writes its OWN `code_profile_runs` row (same `case_id`,
    // incrementing `sample_index`), so pass@k/pass^k can aggregate the repeats.
    let n_samples = samples_per_case();
    if n_samples > 1 {
        tracing::info!(
            "intake v2: multi-sample enabled (INTAKE_SAMPLES_PER_CASE={n_samples}) — \
             running {} case(s) × {n_samples} = {} inference passes for model {model_name}",
            cases.len(),
            cases.len() * n_samples as usize,
        );
    }
    let sampled: Vec<(&CaseV2, i16)> = cases
        .iter()
        .flat_map(|c| (0..n_samples).map(move |s| (c, s as i16)))
        .collect();

    let mut pending: Vec<CaseV2Result> = Vec::with_capacity(sampled.len());
    let mut row_ids: Vec<uuid::Uuid> = Vec::with_capacity(sampled.len());
    for &(case, sample_index) in &sampled {
        let mut cr =
            run_one_case_v2(&client, model_name, case, &dir, backend_tag, mem_config, &factors)
                .await;
        cr.row.sample_index = sample_index;
        // MINT2-02: stamp the structured failure_class from this case's terminal
        // state BEFORE persisting the row (a clean run → "none", never NULL;
        // a timeout/OOM/no-code case still writes a row, now with a queryable
        // class instead of only free-text `error`). The retry (if any) has
        // already run inside `run_one_case_v2`, so the row's compile/test/change
        // fields are final here.
        cr.row.failure_class = Some(classify_case_row(&cr.row).key().to_string());
        let id = storage::insert_code_run_v2(&pool, profile_id, &cr.row).await?;
        row_ids.push(id);
        pending.push(cr);

        // S86 max-lock-hold safety valve: checked HERE — AFTER this case's
        // inference (`run_one_case_v2`, which already ran its own bounded
        // transport-error retries to completion) and its row is durably
        // persisted, BEFORE the next case's `run_one_case_v2` call starts.
        // This is a safe request boundary by construction: no inference call
        // is ever in flight when this runs, and it reuses the exact
        // between-cases point INCR-01 already established as
        // durable/resumable for the ORIGINAL per-case persistence (see this
        // function's module-level doc above). A failure here (the mid-unit
        // reacquire itself exhausted its bounded wait) aborts the REST of
        // this suite via `?` — the caller records that as this pass's
        // ordinary Skipped-with-reason outcome (resumable next run, same as
        // any other GPU reacquire failure), never silently continuing
        // inference without the lock.
        if let Some(lock) = gpu_lock {
            lock.check_max_hold().await.map_err(|e| {
                ToolError::Execution(format!(
                    "mid-unit GPU safety-valve reacquire failed for {model_name} after case \
                     {}: {e}",
                    case.id
                ))
            })?;
        }
    }

    // ---- Phase 2: batched idiom judge, with the JUDGE model hot ---------
    // Running the judge here evicts the test model exactly once for the whole
    // suite (one swap) instead of once — or twice, with retries — per case.
    // Each case's row already exists (Phase 1); patch in the judge score (and
    // the judge-driven 4→5 bump) via `update_code_run_v2_judge`.
    for ((&(case, _), cr), &id) in sampled.iter().zip(pending.iter_mut()).zip(row_ids.iter()) {
        if let Some(resp) = cr.first_response.clone() {
            let q = judge_idiom(&client, &cr.spec, &resp).await;
            cr.row.code_quality_score = q;
            // The structural score caps a perfect case at 4; the judge promotes
            // it to 5 when the code is also idiomatic (rating ≥ 4).
            if cr.row.first_pass_score == Some(4) && q.map(|v| v >= 4.0).unwrap_or(false) {
                cr.row.first_pass_score = Some(5);
            }
        }
        if let Some(rresp) = cr.retry_response.clone() {
            let q2 = judge_idiom(&client, &cr.spec, &rresp).await;
            if cr.row.retry_score == Some(4) && q2.map(|v| v >= 4.0).unwrap_or(false) {
                cr.row.retry_score = Some(5);
            }
        }
        let fp = cr.row.first_pass_score.unwrap_or(0);
        cr.effective_score = cr.row.retry_score.map(|r| r.max(fp)).unwrap_or(fp);
        // S86 hardening: call this UNCONDITIONALLY for every case, not just
        // the ones that got judged. This is the one place every case's row
        // reaches `finalized = true` (see `update_code_run_v2_judge`'s doc) —
        // a case with no first_response/retry_response (e.g. it errored
        // during inference) still needs to be marked complete, or
        // `coder_gaps.rs`'s gap audit would treat it as forever-incomplete
        // even though the suite is genuinely done with it.
        // A missing/deleted row here is an infrastructure-level surprise, not
        // a reason to abort the rest of this model's suite: record it as a
        // per-case skip-with-reason (same convention as the toolchain/OOM/
        // inference-error cases above) and keep going, so one bad row can't
        // take down every other case still queued behind it.
        if let Err(e) = storage::update_code_run_v2_judge(
            &pool,
            id,
            cr.row.code_quality_score,
            cr.row.first_pass_score,
            cr.row.retry_score,
        )
        .await
        {
            tracing::warn!(
                "intake v2: judge checkpoint failed for case {} (row {id}): {e}; skipping",
                case.id,
            );
            if cr.row.error.is_none() {
                cr.row.error = Some(format!("judge checkpoint failed: {e}"));
            }
        }
    }

    // ---- Phase 3: aggregate (error rows excluded); rows already persisted ---
    let mut results: Vec<(String, String, i32)> = Vec::new();
    let mut first_sum = 0i64;
    let mut first_n = 0i64;
    let mut eff_sum = 0i64;
    let mut toolchain_skipped: Vec<String> = Vec::new();
    let mut per_case: Vec<(String, i32, Option<i32>)> = Vec::new();
    let mut errors = 0usize;

    for (&(case, _), cr) in sampled.iter().zip(pending.iter()) {
        let is_toolchain = cr
            .row
            .error
            .as_deref()
            .map(|e| e.starts_with("toolchain"))
            .unwrap_or(false);
        if is_toolchain && !toolchain_skipped.contains(&case.language) {
            toolchain_skipped.push(case.language.clone());
        }
        let fp = cr.row.first_pass_score.unwrap_or(0);
        match &cr.row.error {
            // status ok: a real measured score (including a legitimate 0).
            None => {
                first_sum += fp as i64;
                first_n += 1;
                eff_sum += cr.effective_score as i64;
                results.push((case.language.clone(), case.tier.clone(), cr.effective_score));
            }
            // infrastructure/inference error — excluded from the averages.
            Some(_) if !is_toolchain => errors += 1,
            // toolchain skip — neither scored nor counted as an error.
            Some(_) => {}
        }
        per_case.push((case.id.clone(), fp, cr.row.retry_score));
    }

    let approved = compute_approvals_v2(&results);
    let (max_good, max_marginal) = derive_file_limits_v2(&approved);
    storage::update_op_code(&pool, profile_id, &approved, max_good, max_marginal).await?;

    let avg_first = if first_n > 0 { first_sum as f64 / first_n as f64 } else { 0.0 };
    let avg_eff = if first_n > 0 { eff_sum as f64 / first_n as f64 } else { 0.0 };

    Ok(CodeV2Outcome {
        cases_run: cases.len(),
        avg_first_pass: avg_first,
        avg_effective: avg_eff,
        approved,
        toolchain_skipped,
        scored: first_n as usize,
        errors,
        per_case,
    })
}

/// How many times to run each case (multi-sample-consistency). Read from
/// `INTAKE_SAMPLES_PER_CASE`; defaults to `1` for exact backward compatibility
/// — multi-sampling is strictly OPT-IN, so an unset/blank/invalid/`< 1` value
/// never silently multiplies a production sweep's runtime. A caller that wants
/// pass@k / pass^k must set it explicitly (e.g. `3`).
pub fn samples_per_case() -> u32 {
    std::env::var("INTAKE_SAMPLES_PER_CASE")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
}

/// Unbiased pass@k estimator (multi-sample-consistency): the probability that
/// at least one of `k` samples drawn (without replacement) from `n` total
/// samples — of which `c` succeeded — is a success. This is the numerically
/// stable product form from the Codex/HumanEval paper,
/// `1 - Π_{i=n-c+1}^{n} (1 - k/i)`, NOT the biased shortcut `1 - (1 - c/n)^k`.
///
/// Returns `None` when `k > n` (pass@k is undefined — you cannot draw `k`
/// distinct samples from fewer than `k`); the caller decides how to surface an
/// under-sampled case (the `model_language_stats` matview maps it to SQL NULL).
/// Defensive against the impossible `c > n` (clamped to `c = n`, never panics).
/// Boundaries: `pass_at_k(n, 0, k) = 0.0`, `pass_at_k(n, n, k) = 1.0`, and
/// `pass_at_k(n, c, 1) = c / n`.
pub fn pass_at_k(n: u32, c: u32, k: u32) -> Option<f64> {
    if k == 0 || k > n {
        return None;
    }
    let c = c.min(n); // defensive: c > n cannot happen, but never panic on it.
    if n - c < k {
        // Fewer than k failures ⇒ every k-subset contains a success.
        return Some(1.0);
    }
    // Product over the `c` terms i = n-c+1 ..= n (equivalently, over the
    // failures): 1 - Π (1 - k/i). Iterating the high `i` end keeps each factor
    // close to 1 and the product numerically stable.
    let mut prod = 1.0_f64;
    for i in (n - c + 1)..=n {
        prod *= 1.0 - (k as f64) / (i as f64);
    }
    Some(1.0 - prod)
}

/// Plug-in pass^k estimator (multi-sample-consistency): the probability that
/// ALL `k` samples succeed, estimated as `(c/n)^k`. Anthropic's "flakiness"
/// signal — high pass@k with low pass^k marks a model that is *capable but
/// unreliable* (it can solve a case but not dependably).
///
/// CAVEAT: unlike [`pass_at_k`], this is a POINT ESTIMATE (the plug-in
/// `(c/n)^k`), not the exact without-replacement expectation — matching the
/// simpler estimator the research (Anthropic/community pass^k) uses. Returns
/// `None` for the same `k > n` under-sampled case as [`pass_at_k`] so the two
/// stay defined over an identical domain; `k = 0` also yields `None`.
/// Defensive against `c > n` (clamped). `pass_hat_k(n, c, 1) = c / n`.
pub fn pass_hat_k(n: u32, c: u32, k: u32) -> Option<f64> {
    if k == 0 || k > n {
        return None;
    }
    let c = c.min(n);
    Some((c as f64 / n as f64).powi(k as i32))
}

/// Map approved tiers to file-count bands for the op profile. Pure.
pub fn derive_file_limits_v2(approved: &[String]) -> (Option<i32>, Option<i32>) {
    let band = |tier: &str| match tier {
        "blitz" => 1,
        "standard" => 4,
        "deep" => 8,
        _ => 1,
    };
    let mut best = 0i32;
    for tag in approved {
        if let Some((_, tier)) = tag.split_once(':') {
            best = best.max(band(tier));
        }
    }
    if best == 0 {
        (None, None)
    } else {
        (Some(best), Some((best + 4).min(12)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial(intake_env)]
    fn corpus_v2_dir_env_override() {
        std::env::set_var("INTAKE_CORPUS_V2_DIR", "/tmp/corpus-v2-x");
        assert_eq!(corpus_v2_dir().unwrap(), PathBuf::from("/tmp/corpus-v2-x"));
        std::env::remove_var("INTAKE_CORPUS_V2_DIR");
        // PII remediation (2026-07): INTAKE_CORPUS_V2_DIR has no compiled-in
        // default anymore — missing it must fail clean with NotConfigured.
        match corpus_v2_dir() {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("INTAKE_CORPUS_V2_DIR")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn transport_errors_retry_deterministic_ones_dont() {
        // `is_transport_error` moved to `context.rs` (Phase 2 item 4, the
        // `ErrorClass::Transport` half of the shared classifier); these are
        // the ORIGINAL test cases, preserved verbatim against the new home.
        assert!(context::is_transport_error("error sending request for url"));
        assert!(context::is_transport_error("connection refused"));
        assert!(context::is_transport_error("operation timed out"));
        assert!(context::is_transport_error("unexpected EOF"));
        // Deterministic / model-level failures must NOT trigger a retry.
        assert!(!context::is_transport_error("model 'foo' not found"));
        assert!(!context::is_transport_error("invalid prompt"));
        assert!(!context::is_transport_error("out of memory"));
    }

    #[test]
    fn transport_retry_backoff_escalates_across_three_attempts() {
        // HFIX-04: a single fixed 10s wait did not survive the sustained
        // (multi-minute) connectivity windows observed on <host>. Three // pii-test-fixture
        // escalating waits (not a single fixed one, not unbounded) is the
        // load-bearing shape here.
        assert_eq!(TRANSPORT_RETRY_BACKOFF_SECS, [10, 20, 40]);
        assert_eq!(TRANSPORT_RETRY_BACKOFF_SECS.len(), 3);
        assert!(TRANSPORT_RETRY_BACKOFF_SECS.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn tier_timeouts() {
        assert_eq!(tier_default_timeout("blitz"), 60);
        assert_eq!(tier_default_timeout("standard"), 120);
        assert_eq!(tier_default_timeout("deep"), 300);
        assert_eq!(tier_default_timeout("other"), 120);
    }

    fn blitz_case(timeout_s: Option<u64>) -> CaseV2 {
        CaseV2 {
            id: "a".into(), language: "rust".into(), tier: "blitz".into(),
            spec: default_spec(), validate: default_validate(), dir: "rust/blitz/a".into(),
            workspace: "wx-config".into(), files: vec![], timeout_s, task_type: None,
        }
    }

    // ---- CaseV2::timeout(model_name) — reload-cost allowance wiring ----

    #[test]
    #[serial_test::serial(intake_env)]
    fn case_v2_timeout_unchanged_for_small_model() {
        // The production-stall fix must not touch the common case: a small
        // model on the blitz tier still gets exactly 60s, as before.
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        let case = blitz_case(None);
        assert_eq!(case.timeout("qwen2.5-coder:14b-instruct"), Duration::from_secs(60));
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn case_v2_timeout_adds_reload_allowance_for_large_model() {
        // The exact production scenario: qwen2.5-coder:32b-instruct on a
        // blitz-tier (60s) case. Default allowance is 45s -> 105s effective.
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        let case = blitz_case(None);
        assert_eq!(case.timeout("qwen2.5-coder:32b-instruct"), Duration::from_secs(105));
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn case_v2_timeout_allowance_applies_on_top_of_per_case_override_too() {
        // A per-case `timeout_s` override is still the base the allowance
        // layers onto — the allowance is orthogonal to WHERE the base
        // difficulty timeout came from.
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        let case = blitz_case(Some(200));
        assert_eq!(case.timeout("qwen2.5-coder:32b-instruct"), Duration::from_secs(245));
        assert_eq!(case.timeout("qwen3:8b"), Duration::from_secs(200));
    }

    /// The in-repo v2 corpus manifest must parse, hit the expected case counts,
    /// and every case's spec / validate / workspace / declared files must exist
    /// on disk. Guards against drift between the corpus and the harness schema.
    #[test]
    fn repo_corpus_manifest_is_valid() {
        // crate dir → repo root → tests/intake-corpus-v2
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/intake-corpus-v2");
        if !corpus.join("manifest.json").exists() {
            // Corpus not present in this checkout (e.g. packaged crate) — skip.
            return;
        }
        let cases = read_manifest_v2(&corpus).expect("manifest parses");
        assert_eq!(cases.len(), 40, "expected 40 cases");

        let mut by_lang: std::collections::BTreeMap<&str, usize> = Default::default();
        for c in &cases {
            *by_lang.entry(c.language.as_str()).or_default() += 1;
            let case_dir = corpus.join(&c.dir);
            assert!(case_dir.join(&c.spec).exists(), "missing spec for {}", c.id);
            assert!(case_dir.join(&c.validate).exists(), "missing validate for {}", c.id);
            let ws = corpus.join("_workspaces").join(&c.workspace);
            assert!(ws.is_dir(), "missing workspace {} for {}", c.workspace, c.id);
            assert!(!c.files.is_empty(), "case {} declares no files", c.id);
            // Declared files must be workspace-relative (no workspace prefix).
            for f in &c.files {
                assert!(!f.starts_with(&c.workspace), "file {f} carries ws prefix in {}", c.id);
            }
        }
        assert_eq!(by_lang.get("rust").copied(), Some(16));
        assert_eq!(by_lang.get("python").copied(), Some(9));
        assert_eq!(by_lang.get("typescript").copied(), Some(9));
        assert_eq!(by_lang.get("bash").copied(), Some(6));
    }

    #[test]
    fn build_pipeline_prompt_has_spec_files_and_format() {
        let p = build_pipeline_prompt(
            "## Task\nDo the thing",
            "wx-config",
            &[("src/validate.rs".into(), "fn a(){}".into())],
            "src/lib.rs\nsrc/validate.rs",
        );
        assert!(p.contains("Do the thing"));
        assert!(p.contains("wx-config"));
        assert!(p.contains("src/validate.rs"));
        assert!(p.contains("fn a(){}"));
        assert!(p.contains("CURRENT FILE CONTENTS"));
        assert!(p.contains("first line inside the code block") || p.contains("FIRST line"));
    }

    #[test]
    fn read_target_files_empty_for_new_file_targets_not_missing_workspace() {
        // Reproduces the HFIX-03 wx-ts bug: `ts-standard-t1` targets brand-new
        // files (`typed.ts`, `typed.test.ts`) that the model must CREATE, in a
        // workspace whose OTHER modules (querystring.ts, result.ts, ...) are
        // genuinely present on disk — mirroring the real `_workspaces/wx-ts`
        // layout. `read_target_files` must report an empty list here (nothing
        // to read yet), NOT an error — the workspace itself is fine.
        let dir = std::env::temp_dir().join(format!("wx-ts-hfix03-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("querystring.ts"), "export function parse() {}\n").unwrap();
        std::fs::write(dir.join("result.ts"), "export function Ok(v) { return v; }\n").unwrap();

        // Workspace directory is present and readable...
        assert!(dir.is_dir());
        // ...but the case's declared target files (a new module to create)
        // don't exist yet — that's expected, not an infra failure.
        let (files, total_lines) = read_target_files(
            &dir,
            &["typed.ts".to_string(), "typed.test.ts".to_string()],
        );
        assert!(files.is_empty(), "new-file targets should read as empty, not error");
        assert_eq!(total_lines, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_pipeline_prompt_handles_new_file_creation_case() {
        // No CURRENT file contents at all (every target is new) must still
        // produce a valid, non-panicking prompt that tells the model to
        // create the file(s) rather than implying files were dropped/unreadable.
        let p = build_pipeline_prompt(
            "## Task\nCreate a new module `wx-ts/typed.ts`.\n\n## FILES\n- wx-ts/typed.ts\n- wx-ts/typed.test.ts",
            "wx-ts",
            &[],
            "emitter.ts\nquerystring.ts\nresult.ts\nrouter.ts",
        );
        assert!(p.contains("wx-ts"));
        assert!(p.contains("Create a new module"));
        assert!(p.contains("create it from scratch"));
        assert!(!p.contains("----- "), "no per-file blocks when there are no existing files");
    }

    #[test]
    fn map_outputs_marker_match() {
        let files = vec!["src/validate.rs".to_string()];
        let extracted = vec![(Some("src/validate.rs".to_string()), "new".to_string())];
        let m = map_outputs(&files, &extracted);
        assert_eq!(m.get("src/validate.rs").map(|s| s.as_str()), Some("new"));
    }

    #[test]
    fn map_outputs_single_file_fallback_largest() {
        let files = vec!["src/a.rs".to_string()];
        let extracted = vec![(None, "short".to_string()), (None, "the bigger block".to_string())];
        let m = map_outputs(&files, &extracted);
        assert_eq!(m.get("src/a.rs").map(|s| s.as_str()), Some("the bigger block"));
    }

    #[test]
    fn map_outputs_basename_match_multifile() {
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let extracted = vec![
            (Some("a.rs".to_string()), "AAA".to_string()),
            (Some("b.rs".to_string()), "BBB".to_string()),
        ];
        let m = map_outputs(&files, &extracted);
        assert_eq!(m.get("src/a.rs").unwrap(), "AAA");
        assert_eq!(m.get("src/b.rs").unwrap(), "BBB");
    }

    #[test]
    fn parse_stages_reads_markers() {
        let out = "noise\nSTAGE:COMPILE ok\nmore\nSTAGE:TESTS fail\nSTAGE:CHANGE ok\n";
        let s = parse_stages(out);
        assert_eq!(s.compile, Some(true));
        assert_eq!(s.tests, Some(false));
        assert_eq!(s.change, Some(true));
        assert_eq!(s.toolchain_missing, None);
    }

    #[test]
    fn parse_stages_toolchain_missing() {
        let s = parse_stages("TOOLCHAIN:missing cargo\nSKIP\n");
        assert_eq!(s.toolchain_missing.as_deref(), Some("cargo"));
    }

    #[test]
    fn graduated_score_full_path() {
        let s = ValidateStages { compile: Some(true), tests: Some(true), change: Some(true), toolchain_missing: None };
        assert_eq!(graduated_score(&s, Some(5.0), true), 5);
        assert_eq!(graduated_score(&s, Some(3.0), true), 4);
        assert_eq!(graduated_score(&s, None, true), 4);
    }

    #[test]
    fn graduated_score_change_incomplete_is_3() {
        let s = ValidateStages { compile: Some(true), tests: Some(true), change: Some(false), toolchain_missing: None };
        assert_eq!(graduated_score(&s, Some(5.0), true), 3);
    }

    #[test]
    fn graduated_score_tests_fail_is_2() {
        let s = ValidateStages { compile: Some(true), tests: Some(false), change: Some(false), toolchain_missing: None };
        assert_eq!(graduated_score(&s, Some(5.0), true), 2);
    }

    #[test]
    fn graduated_score_no_compile_is_1_and_no_code_is_0() {
        let s = ValidateStages { compile: Some(false), ..Default::default() };
        assert_eq!(graduated_score(&s, Some(5.0), true), 1);
        assert_eq!(graduated_score(&ValidateStages::default(), Some(5.0), false), 0);
    }

    #[test]
    fn retry_only_for_1_and_2() {
        assert!(should_retry(1));
        assert!(should_retry(2));
        assert!(!should_retry(0));
        assert!(!should_retry(3));
        assert!(!should_retry(5));
    }

    #[test]
    fn build_retry_prompt_includes_error_and_prior() {
        let p = build_retry_prompt("orig task", "my code", "error E0382");
        assert!(p.contains("orig task"));
        assert!(p.contains("my code"));
        assert!(p.contains("E0382"));
        assert!(p.contains("PREVIOUS ATTEMPT"));
    }

    #[test]
    fn approvals_v2_threshold_3() {
        // rust:blitz avg = (5+4+3+1)/4 = 3.25 → approved.
        // rust:standard avg = (2+2+1)/3 = 1.67 → not.
        let r = vec![
            ("rust".into(), "blitz".into(), 5),
            ("rust".into(), "blitz".into(), 4),
            ("rust".into(), "blitz".into(), 3),
            ("rust".into(), "blitz".into(), 1),
            ("rust".into(), "standard".into(), 2),
            ("rust".into(), "standard".into(), 2),
            ("rust".into(), "standard".into(), 1),
        ];
        assert_eq!(compute_approvals_v2(&r), vec!["rust:blitz".to_string()]);
    }

    #[test]
    fn file_limits_v2_from_tiers() {
        assert_eq!(derive_file_limits_v2(&[]), (None, None));
        assert_eq!(derive_file_limits_v2(&["rust:blitz".into()]), (Some(1), Some(5)));
        assert_eq!(derive_file_limits_v2(&["rust:standard".into()]), (Some(4), Some(8)));
        assert_eq!(derive_file_limits_v2(&["rust:deep".into()]), (Some(8), Some(12)));
    }

    #[test]
    fn filter_by_language_works() {
        let cases = vec![
            CaseV2 { id: "a".into(), language: "rust".into(), tier: "blitz".into(),
                spec: default_spec(), validate: default_validate(), dir: "rust/blitz/a".into(),
                workspace: "wx-config".into(), files: vec![], timeout_s: None, task_type: None },
            CaseV2 { id: "b".into(), language: "python".into(), tier: "blitz".into(),
                spec: default_spec(), validate: default_validate(), dir: "python/blitz/b".into(),
                workspace: "py".into(), files: vec![], timeout_s: None, task_type: None },
        ];
        assert_eq!(filter_by_language(&cases, &[]).len(), 2);
        assert_eq!(filter_by_language(&cases, &["RUST".into()])[0].id, "a");
    }

    fn two_cases() -> Vec<CaseV2> {
        vec![
            CaseV2 { id: "a".into(), language: "rust".into(), tier: "blitz".into(),
                spec: default_spec(), validate: default_validate(), dir: "rust/blitz/a".into(),
                workspace: "wx-config".into(), files: vec![], timeout_s: None, task_type: None },
            CaseV2 { id: "b".into(), language: "python".into(), tier: "blitz".into(),
                spec: default_spec(), validate: default_validate(), dir: "python/blitz/b".into(),
                workspace: "py".into(), files: vec![], timeout_s: None, task_type: None },
        ]
    }

    #[test]
    fn filter_by_ids_none_means_all() {
        assert_eq!(filter_by_ids(&two_cases(), None).len(), 2);
    }

    #[test]
    fn filter_by_ids_empty_means_all() {
        assert_eq!(filter_by_ids(&two_cases(), Some(&[])).len(), 2);
    }

    #[test]
    fn filter_by_ids_selects_exact_matches_only() {
        let ids = vec!["b".to_string()];
        let got = filter_by_ids(&two_cases(), Some(&ids));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");
    }

    #[test]
    fn filter_by_ids_unknown_id_yields_empty_not_error() {
        // Pure filter — the caller (case-rerun tool) is responsible for
        // reporting which requested ids were not found; this function just
        // filters.
        let ids = vec!["nonexistent".to_string()];
        assert!(filter_by_ids(&two_cases(), Some(&ids)).is_empty());
    }

    // ---- multi-sample-consistency: pass@k / pass^k estimators -----------

    /// `samples_per_case()` is opt-in: unset/blank/invalid/`< 1` ⇒ 1, so a
    /// production sweep never silently multiplies its runtime.
    #[test]
    fn samples_per_case_defaults_to_one_and_is_opt_in() {
        std::env::remove_var("INTAKE_SAMPLES_PER_CASE");
        assert_eq!(samples_per_case(), 1);
        std::env::set_var("INTAKE_SAMPLES_PER_CASE", "3");
        assert_eq!(samples_per_case(), 3);
        std::env::set_var("INTAKE_SAMPLES_PER_CASE", "  ");
        assert_eq!(samples_per_case(), 1);
        std::env::set_var("INTAKE_SAMPLES_PER_CASE", "nonsense");
        assert_eq!(samples_per_case(), 1);
        std::env::set_var("INTAKE_SAMPLES_PER_CASE", "0");
        assert_eq!(samples_per_case(), 1);
        std::env::remove_var("INTAKE_SAMPLES_PER_CASE");
    }

    const EPS: f64 = 1e-12;

    /// pass@1 == pass^1 == c/n (the k=1 boundary), for both estimators.
    #[test]
    fn pass_at_and_hat_k1_equal_c_over_n() {
        for (n, c) in [(5u32, 0u32), (5, 2), (5, 5), (10, 3), (1, 0), (1, 1)] {
            let expected = c as f64 / n as f64;
            assert!((pass_at_k(n, c, 1).unwrap() - expected).abs() < EPS);
            assert!((pass_hat_k(n, c, 1).unwrap() - expected).abs() < EPS);
        }
    }

    /// Boundaries: all-fail ⇒ 0, all-pass ⇒ 1, for every k ≤ n.
    #[test]
    fn pass_at_k_boundaries_all_fail_and_all_pass() {
        for k in 1..=5u32 {
            assert!(pass_at_k(5, 0, k).unwrap().abs() < EPS, "all-fail pass@{k}");
            assert!((pass_at_k(5, 5, k).unwrap() - 1.0).abs() < EPS, "all-pass pass@{k}");
            assert!(pass_hat_k(5, 0, k).unwrap().abs() < EPS);
            assert!((pass_hat_k(5, 5, k).unwrap() - 1.0).abs() < EPS);
        }
    }

    /// Known value: the classic Codex-paper example n=10, c=3 gives an unbiased
    /// pass@5 ≈ 0.9166666... Computed by hand from `1 - C(7,5)/C(10,5)` =
    /// `1 - 21/252 = 1 - 0.08333... = 0.91666...`. pass^5 = (3/10)^5 = 0.00243.
    #[test]
    fn pass_at_k_known_codex_n10_c3_k5() {
        let got = pass_at_k(10, 3, 5).unwrap();
        assert!((got - (1.0 - 21.0 / 252.0)).abs() < 1e-9, "pass@5 was {got}");
        let hat = pass_hat_k(10, 3, 5).unwrap();
        assert!((hat - 0.3_f64.powi(5)).abs() < 1e-12, "pass^5 was {hat}");
    }

    /// Known value cross-check: n=3, c=1. pass@2 = 1 - C(2,2)/C(3,2) =
    /// 1 - 1/3 = 0.6666...; pass@3 = 1 (only one failure, every 3-subset — the
    /// whole set — contains the one success). pass^2 = (1/3)^2 = 0.1111...
    #[test]
    fn pass_at_k_known_small_n3_c1() {
        assert!((pass_at_k(3, 1, 2).unwrap() - (1.0 - 1.0 / 3.0)).abs() < 1e-12);
        assert!((pass_at_k(3, 1, 3).unwrap() - 1.0).abs() < EPS);
        assert!((pass_hat_k(3, 1, 2).unwrap() - (1.0 / 9.0)).abs() < 1e-12);
    }

    /// pass@k is non-decreasing in k; pass^k is non-increasing in k; both stay
    /// in [0, 1]. Checked across a spread of (n, c) with 1 ≤ k ≤ n.
    #[test]
    fn pass_at_k_monotone_up_pass_hat_k_monotone_down() {
        for n in 1..=10u32 {
            for c in 0..=n {
                let mut prev_at = -1.0_f64;
                let mut prev_hat = 2.0_f64;
                for k in 1..=n {
                    let at = pass_at_k(n, c, k).unwrap();
                    let hat = pass_hat_k(n, c, k).unwrap();
                    assert!((0.0..=1.0).contains(&at), "pass@{k}({n},{c})={at} out of [0,1]");
                    assert!((0.0..=1.0).contains(&hat), "pass^{k}({n},{c})={hat} out of [0,1]");
                    assert!(at >= prev_at - EPS, "pass@k not non-decreasing at n={n} c={c} k={k}");
                    assert!(hat <= prev_hat + EPS, "pass^k not non-increasing at n={n} c={c} k={k}");
                    prev_at = at;
                    prev_hat = hat;
                }
            }
        }
    }

    /// k > n is undefined (can't draw k distinct samples) ⇒ `None` for both;
    /// k = 0 ⇒ `None`; c > n never happens but is clamped, never panics.
    #[test]
    fn pass_at_k_edge_cases_none_and_defensive_clamp() {
        assert_eq!(pass_at_k(3, 1, 4), None);
        assert_eq!(pass_hat_k(3, 1, 4), None);
        assert_eq!(pass_at_k(0, 0, 1), None);
        assert_eq!(pass_at_k(5, 3, 0), None);
        assert_eq!(pass_hat_k(5, 3, 0), None);
        // c > n (impossible in practice) is clamped to c = n, not a panic.
        assert!((pass_at_k(3, 9, 2).unwrap() - 1.0).abs() < EPS);
        assert!((pass_hat_k(3, 9, 2).unwrap() - 1.0).abs() < EPS);
    }

    // ---- MINT2-01: measurement factors ---------------------------------

    /// `task_category` is derived from the corpus-manifest TIER, never from
    /// `file_count` — the whole point of promoting it to a stored factor. Same
    /// tier ⇒ same category regardless of file_count; different tier ⇒ different
    /// category even at identical file_count.
    #[test]
    fn task_category_comes_from_tier_not_file_count() {
        // `standard` tier → `multi_file` category (the manifest's middle tier).
        assert_eq!(task_category_from_tier("blitz"), "blitz");
        assert_eq!(task_category_from_tier("standard"), "multi_file");
        assert_eq!(task_category_from_tier("deep"), "deep");
        // Case-insensitive + trimmed; an already-canonical `multi_file` passes
        // through; an unknown tier is recorded verbatim (lowercased), not bucketed.
        assert_eq!(task_category_from_tier("  DEEP "), "deep");
        assert_eq!(task_category_from_tier("multi_file"), "multi_file");
        assert_eq!(task_category_from_tier("weird"), "weird");

        // The factor is a pure function of the tier string: two cases with the
        // SAME tier but very different file counts get the SAME category, and
        // two cases with DIFFERENT tiers but (hypothetically) the same file
        // count get DIFFERENT categories — file_count never enters this path.
        let blitz_1_file = CaseV2 {
            id: "b".into(), language: "rust".into(), tier: "blitz".into(),
            spec: default_spec(), validate: default_validate(), dir: "d".into(),
            workspace: "w".into(), files: vec!["a.rs".into()], timeout_s: None, task_type: None,
        };
        let deep_1_file = CaseV2 {
            id: "d".into(), language: "rust".into(), tier: "deep".into(),
            spec: default_spec(), validate: default_validate(), dir: "d".into(),
            workspace: "w".into(), files: vec!["a.rs".into()], timeout_s: None, task_type: None,
        };
        assert_eq!(blitz_1_file.files.len(), deep_1_file.files.len());
        assert_ne!(
            task_category_from_tier(&blitz_1_file.tier),
            task_category_from_tier(&deep_1_file.tier),
            "identical file_count must NOT collapse two tiers into one category"
        );
    }

    /// Quant is parsed from the model id when present, canonicalized, and is
    /// `None` (→ recorded as "unknown", never guessed) when the id declares none.
    #[test]
    fn parse_quant_from_model_id_recognizes_tags_else_none() {
        assert_eq!(parse_quant_from_model_id("qwen3-coder:30b-a3b-q4_K_M").as_deref(), Some("Q4_K_M"));
        assert_eq!(parse_quant_from_model_id("model:Q6_K").as_deref(), Some("Q6_K"));
        assert_eq!(parse_quant_from_model_id("some-model-fp16").as_deref(), Some("fp16"));
        assert_eq!(parse_quant_from_model_id("thing-bf16-gguf").as_deref(), Some("bf16"));
        // Longest-match wins so a `q4_k_m` id doesn't degrade to `q4_0`/`q4_k`.
        assert_eq!(parse_quant_from_model_id("x-q4_k_m-y").as_deref(), Some("Q4_K_M"));
        // No recognizable quant tag → None (caller records "unknown").
        assert_eq!(parse_quant_from_model_id("qwen3-coder:30b"), None);
        assert_eq!(parse_quant_from_model_id("qwen3:8b"), None);
    }

    /// Three-state bool: on/off recognized, everything else (incl. unset) None.
    #[test]
    fn parse_three_state_bool_is_three_state() {
        assert_eq!(parse_three_state_bool(Some("true")), Some(true));
        assert_eq!(parse_three_state_bool(Some("ON")), Some(true));
        assert_eq!(parse_three_state_bool(Some("1")), Some(true));
        assert_eq!(parse_three_state_bool(Some("false")), Some(false));
        assert_eq!(parse_three_state_bool(Some("off")), Some(false));
        assert_eq!(parse_three_state_bool(Some("0")), Some(false));
        // Unset / unrecognized ⇒ None (never coerced to false).
        assert_eq!(parse_three_state_bool(None), None);
        assert_eq!(parse_three_state_bool(Some("maybe")), None);
        assert_eq!(parse_three_state_bool(Some("")), None);
    }

    /// The default factors: quant "unknown", everything else unset/None — so a
    /// row written with no config records honest NULLs, not fabricated values.
    #[test]
    fn measurement_factors_default_is_unknown_quant_and_unset_rest() {
        let f = MeasurementFactors::default();
        assert_eq!(f.quant, "unknown");
        assert_eq!(f.reasoning_enabled, None);
        assert_eq!(f.context_window_launched, None);
        assert_eq!(f.temperature, None);
        assert_eq!(f.top_p, None);
    }

    /// `from_env` resolves each knob; unset quant falls back to the id-parsed
    /// tag, then to "unknown". Serialized (mutates process env).
    #[test]
    #[serial_test::serial(intake_env)]
    fn measurement_factors_from_env_resolves_knobs() {
        for k in [
            "SWEEP_QUANT", "SWEEP_REASONING_ENABLED", "SWEEP_CONTEXT_WINDOW",
            "SWEEP_TEMPERATURE", "SWEEP_TOP_P",
        ] {
            std::env::remove_var(k);
        }
        // Nothing set: quant parsed from the id (none here → "unknown"), rest None.
        let f = MeasurementFactors::from_env("qwen3:8b");
        assert_eq!(f.quant, "unknown");
        assert_eq!(f.reasoning_enabled, None);
        assert_eq!(f.context_window_launched, None);

        // Id declares a quant, still nothing in env → parsed from the id.
        assert_eq!(MeasurementFactors::from_env("m:30b-q6_k").quant, "Q6_K");

        // Explicit env overrides everything, including an id-declared quant.
        std::env::set_var("SWEEP_QUANT", "fp16");
        std::env::set_var("SWEEP_REASONING_ENABLED", "off");
        std::env::set_var("SWEEP_CONTEXT_WINDOW", "16384");
        std::env::set_var("SWEEP_TEMPERATURE", "0.2");
        std::env::set_var("SWEEP_TOP_P", "0.9");
        let f = MeasurementFactors::from_env("m:30b-q6_k");
        assert_eq!(f.quant, "fp16");
        assert_eq!(f.reasoning_enabled, Some(false));
        assert_eq!(f.context_window_launched, Some(16384));
        assert_eq!(f.temperature, Some(0.2));
        assert_eq!(f.top_p, Some(0.9));

        // A non-positive / garbage context window is rejected (stays None).
        std::env::set_var("SWEEP_CONTEXT_WINDOW", "0");
        assert_eq!(MeasurementFactors::from_env("m").context_window_launched, None);
        std::env::set_var("SWEEP_CONTEXT_WINDOW", "nope");
        assert_eq!(MeasurementFactors::from_env("m").context_window_launched, None);

        for k in [
            "SWEEP_QUANT", "SWEEP_REASONING_ENABLED", "SWEEP_CONTEXT_WINDOW",
            "SWEEP_TEMPERATURE", "SWEEP_TOP_P",
        ] {
            std::env::remove_var(k);
        }
    }

    // ---- MINT2-02: structured failure classification -------------------

    /// Every variant's `key()` is the exact stable snake_case string stored in
    /// the DB — this is the data contract with reporting/the catalog and with
    /// the migration's documented enum values, so it is asserted explicitly.
    #[test]
    fn failure_class_keys_are_stable_snake_case() {
        assert_eq!(FailureClass::None.key(), "none");
        assert_eq!(FailureClass::Truncation.key(), "truncation");
        assert_eq!(FailureClass::EmptyDiff.key(), "empty_diff");
        assert_eq!(FailureClass::TautologicalTests.key(), "tautological_tests");
        assert_eq!(FailureClass::CompilationError.key(), "compilation_error");
        assert_eq!(FailureClass::TestFailure.key(), "test_failure");
        assert_eq!(FailureClass::ReviewRejection.key(), "review_rejection");
        assert_eq!(FailureClass::ProviderError.key(), "provider_error");
        assert_eq!(FailureClass::MaxIterations.key(), "max_iterations");
        assert_eq!(FailureClass::Timeout.key(), "timeout");
        assert_eq!(FailureClass::PhaseStall.key(), "phase_stall");
        assert_eq!(FailureClass::Unknown.key(), "unknown");
        assert_eq!(FailureClass::NonViableVram.key(), "non_viable_vram");
    }

    fn stages(compile: Option<bool>, tests: Option<bool>, change: Option<bool>) -> ValidateStages {
        ValidateStages { compile, tests, change, toolchain_missing: None }
    }

    /// A genuinely clean run (compiles + tests + change all ok) → `None`/"none",
    /// NEVER null-on-success (null is reserved for legacy rows).
    #[test]
    fn classify_clean_pass_is_none() {
        let s = stages(Some(true), Some(true), Some(true));
        assert_eq!(FailureClass::classify(Some(&s), None, false, None), FailureClass::None);
        assert_eq!(FailureClass::classify(Some(&s), None, false, None).key(), "none");
    }

    /// A produced-but-doesn't-compile attempt → compilation_error.
    #[test]
    fn classify_compile_fail_is_compilation_error() {
        let s = stages(Some(false), None, None);
        assert_eq!(
            FailureClass::classify(Some(&s), None, false, None),
            FailureClass::CompilationError
        );
    }

    /// Compiles but tests fail → test_failure; compiles + tests ok but the
    /// hidden change-behavior check fails is ALSO test_failure (that check is a
    /// behavior test).
    #[test]
    fn classify_test_and_change_fail_are_test_failure() {
        let tests_fail = stages(Some(true), Some(false), Some(false));
        assert_eq!(
            FailureClass::classify(Some(&tests_fail), None, false, None),
            FailureClass::TestFailure
        );
        let change_fail = stages(Some(true), Some(true), Some(false));
        assert_eq!(
            FailureClass::classify(Some(&change_fail), None, false, None),
            FailureClass::TestFailure
        );
    }

    /// A timeout error text → timeout (matched case-insensitively, several
    /// phrasings).
    #[test]
    fn classify_timeout_from_error_text() {
        for msg in ["operation timed out", "request TIMEOUT", "deadline exceeded"] {
            assert_eq!(
                FailureClass::classify(None, Some(msg), false, None),
                FailureClass::Timeout,
                "msg={msg}"
            );
        }
    }

    /// A non-timeout infra/inference error → provider_error.
    #[test]
    fn classify_other_error_is_provider_error() {
        assert_eq!(
            FailureClass::classify(None, Some("connection refused"), false, None),
            FailureClass::ProviderError
        );
        assert_eq!(
            FailureClass::classify(None, Some("toolchain unavailable: rust (needs cargo)"), false, None),
            FailureClass::ProviderError
        );
    }

    /// OOM → provider_error, and OOM takes precedence over a concurrent timeout
    /// (earliest-observed cause wins — the documented precedence).
    #[test]
    fn classify_oom_maps_to_provider_error_and_beats_timeout() {
        assert_eq!(FailureClass::classify(None, None, true, None), FailureClass::ProviderError);
        // Both OOM and a timeout error text present → OOM wins.
        assert_eq!(
            FailureClass::classify(None, Some("operation timed out"), true, None),
            FailureClass::ProviderError
        );
    }

    /// A pre-flight over-VRAM skip → non_viable_vram, and it wins over every
    /// runtime signal (a skipped cell was never attempted).
    #[test]
    fn classify_non_viable_vram_skip_wins() {
        let reason = "over VRAM ceiling on GPU (131GB footprint > 96GB ceiling)";
        assert_eq!(
            FailureClass::classify(None, None, false, Some(reason)),
            FailureClass::NonViableVram
        );
        // Even if a stray runtime signal is also present, the skip dominates.
        assert_eq!(
            FailureClass::classify(None, Some("operation timed out"), true, Some(reason)),
            FailureClass::NonViableVram
        );
    }

    /// A refusal / empty output (returned cleanly, produced nothing scored) →
    /// empty_diff.
    #[test]
    fn classify_no_code_is_empty_diff() {
        assert_eq!(FailureClass::classify(None, None, false, None), FailureClass::EmptyDiff);
    }

    /// The row-level helper reflects the row's terminal state: a clean row →
    /// "none"; a no-code row (well_formed=false) → empty_diff; an OOM row →
    /// provider_error; a compile-fail row → compilation_error.
    #[test]
    fn classify_case_row_reads_terminal_state() {
        let clean = CodeRunRowV2 {
            well_formed: Some(true),
            compiles: Some(true),
            tests_pass: Some(true),
            change_correct: Some(true),
            ..Default::default()
        };
        assert_eq!(classify_case_row(&clean), FailureClass::None);

        let no_code = CodeRunRowV2 { well_formed: Some(false), ..Default::default() };
        assert_eq!(classify_case_row(&no_code), FailureClass::EmptyDiff);

        let oomed = CodeRunRowV2 {
            oom: true,
            error: Some("OOM".into()),
            ..Default::default()
        };
        assert_eq!(classify_case_row(&oomed), FailureClass::ProviderError);

        let compile_fail = CodeRunRowV2 {
            well_formed: Some(true),
            compiles: Some(false),
            ..Default::default()
        };
        assert_eq!(classify_case_row(&compile_fail), FailureClass::CompilationError);
    }
}
