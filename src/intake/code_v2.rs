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
//! ## Corpus layout (`INTAKE_CORPUS_V2_DIR`, default `<path>/intake-corpus-v2`)
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
//! Toolchains live under `/opt/chord-data/toolchains` on <host>; the deploy script
//! puts cargo/node on PATH. Missing toolchain → row error set, score NULL, run
//! continues.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::ToolError;
use crate::intake::code::{extract_files, have_tool, required_toolchain};
use crate::intake::context;
use crate::intake::storage::{self, CodeRunRowV2};

/// Default v2 corpus location on <host>; overridable via `INTAKE_CORPUS_V2_DIR`.
const DEFAULT_CORPUS_V2_DIR: &str = "<path>/intake-corpus-v2";

/// Resolve the v2 corpus directory.
pub fn corpus_v2_dir() -> PathBuf {
    std::env::var("INTAKE_CORPUS_V2_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CORPUS_V2_DIR))
}

/// Persistent build-cache root (pre-warmed deps) passed to validators as
/// `MINT_TARGET_CACHE`. Defaults next to the corpus so it survives across runs.
pub fn target_cache_dir() -> PathBuf {
    std::env::var("INTAKE_TARGET_CACHE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| corpus_v2_dir().join("_target-cache"))
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
    /// Effective inference timeout (per-case override → tier default).
    pub fn timeout(&self) -> Duration {
        let secs = self.timeout_s.unwrap_or_else(|| tier_default_timeout(&self.tier));
        Duration::from_secs(secs)
    }
}

/// Default inference timeout per tier (blitz 60s, standard 120s, deep 300s).
pub fn tier_default_timeout(tier: &str) -> u64 {
    match tier.to_lowercase().as_str() {
        "blitz" => 60,
        "standard" => 120,
        "deep" => 300,
        _ => 120,
    }
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
    let cache = target_cache_dir();
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

/// Whether an inference error is a transport/connection failure worth one
/// retry (vs. a deterministic model/server rejection). Pure.
fn is_transport_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("error sending request")
        || l.contains("connection")
        || l.contains("timed out")
        || l.contains("timeout")
        || l.contains("broken pipe")
        || l.contains("reset by peer")
        || l.contains("eof")
}

/// Backoff delays (seconds) between retries of a transport-style error, in
/// order. HFIX-04: a single fixed 10s retry did not survive the SUSTAINED
/// (multi-minute) connectivity windows actually observed on <host> — ollama's
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
        if !is_transport_error(e) {
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
) -> CaseV2Result {
    let mut row = CodeRunRowV2 {
        language: case.language.clone(),
        task_type: case.task_type.clone().or_else(|| Some("build_modify".into())),
        backend_tag: backend_tag.map(str::to_string),
        mem_config: mem_config.map(str::to_string),
        case_id: Some(case.id.clone()),
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
    let gen = generate_with_retry(client, model_name, &prompt, case.timeout()).await;
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
        let gen2 = generate_with_retry(client, model_name, &retry_prompt, case.timeout()).await;
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
pub async fn run_code_suite_v2(
    model_name: &str,
    languages: &[String],
    profile_id: uuid::Uuid,
    case_limit: Option<usize>,
    backend_tag: Option<&str>,
    mem_config: Option<&str>,
) -> Result<CodeV2Outcome, ToolError> {
    run_code_suite_v2_cases(
        model_name,
        languages,
        None,
        profile_id,
        case_limit,
        backend_tag,
        mem_config,
    )
    .await
}

/// Like [`run_code_suite_v2`] but scoped to an explicit `case_ids` subset
/// (HFIX-06). `None`/empty ⇒ every case matching `languages`, i.e. identical
/// behavior to `run_code_suite_v2`. Lets a single case (or a small named set)
/// be re-run to fill a specific result gap without re-running a model's
/// entire suite — the case-rerun tool (`intake_coder_case`) is the intended
/// caller; the fleet sweep (`intake_coder_sweep`) always passes `None`.
pub async fn run_code_suite_v2_cases(
    model_name: &str,
    languages: &[String],
    case_ids: Option<&[String]>,
    profile_id: uuid::Uuid,
    case_limit: Option<usize>,
    backend_tag: Option<&str>,
    mem_config: Option<&str>,
) -> Result<CodeV2Outcome, ToolError> {
    sweep_stale_workspaces();
    let dir = corpus_v2_dir();
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
    let mut pending: Vec<CaseV2Result> = Vec::with_capacity(cases.len());
    let mut row_ids: Vec<uuid::Uuid> = Vec::with_capacity(cases.len());
    for case in &cases {
        let cr = run_one_case_v2(&client, model_name, case, &dir, backend_tag, mem_config).await;
        let id = storage::insert_code_run_v2(&pool, profile_id, &cr.row).await?;
        row_ids.push(id);
        pending.push(cr);
    }

    // ---- Phase 2: batched idiom judge, with the JUDGE model hot ---------
    // Running the judge here evicts the test model exactly once for the whole
    // suite (one swap) instead of once — or twice, with retries — per case.
    // Each case's row already exists (Phase 1); patch in the judge score (and
    // the judge-driven 4→5 bump) via `update_code_run_v2_judge`.
    for ((case, cr), &id) in cases.iter().zip(pending.iter_mut()).zip(row_ids.iter()) {
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

    for (case, cr) in cases.iter().zip(pending.iter()) {
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
    fn corpus_v2_dir_env_override() {
        std::env::set_var("INTAKE_CORPUS_V2_DIR", "/tmp/corpus-v2-x");
        assert_eq!(corpus_v2_dir(), PathBuf::from("/tmp/corpus-v2-x"));
        std::env::remove_var("INTAKE_CORPUS_V2_DIR");
        assert_eq!(corpus_v2_dir(), PathBuf::from(DEFAULT_CORPUS_V2_DIR));
    }

    #[test]
    fn transport_errors_retry_deterministic_ones_dont() {
        assert!(is_transport_error("error sending request for url"));
        assert!(is_transport_error("connection refused"));
        assert!(is_transport_error("operation timed out"));
        assert!(is_transport_error("unexpected EOF"));
        // Deterministic / model-level failures must NOT trigger a retry.
        assert!(!is_transport_error("model 'foo' not found"));
        assert!(!is_transport_error("invalid prompt"));
        assert!(!is_transport_error("out of memory"));
    }

    #[test]
    fn transport_retry_backoff_escalates_across_three_attempts() {
        // HFIX-04: a single fixed 10s wait did not survive the sustained
        // (multi-minute) connectivity windows observed on <host>. Three
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
}
