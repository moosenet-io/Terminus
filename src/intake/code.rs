//! Per-language code profiling suite (S83 MINT-02).
//!
//! Runs the model against the reusable test corpus under `INTAKE_CORPUS_DIR`
//! (default: the deployed intake-corpus directory). For each selected case it:
//!   1. builds a prompt from `task.md` + the case's `input/` file(s),
//!   2. calls the model via the SAME Ollama path the context suite uses
//!      (`context::generate`), non-streaming,
//!   3. extracts the code block(s) from the response and writes them into a
//!      temp copy of the case dir (replacing the targeted input file),
//!   4. runs the case's `validate.sh` (exit 0 = pass) to measure compiles /
//!      tests_pass / planted_bug_found,
//!   5. asks a small secondary model (qwen3:8b) to rate the output 1-5
//!      (`code_quality_score`; NULL if the judge is unavailable),
//!   6. stores one `code_profile_runs` row per case.
//!
//! Toolchain reality: the sweep-harness host has python3/g++/bash/sqlite3 but may lack cargo/node.
//! When a language's validator can't run for lack of a toolchain we DEGRADE
//! GRACEFULLY — record "toolchain unavailable: <lang>" in the row's error,
//! still store the LLM quality score, and never crash the run.
//!
//! Approval: a (language, complexity) pair passes if ≥3/4 task types pass. The
//! passing pairs are folded into `approved_languages` as "lang:complexity" tags.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::ToolError;
use crate::intake::context;
use crate::intake::storage::{self, CodeRunRow};

/// Resolve the corpus directory from `INTAKE_CORPUS_DIR`. No compiled-in
/// default (PII remediation 2026-07): required at runtime — fails clean
/// with `NotConfigured` rather than silently pointing at a real
/// sweep-harness host path.
pub fn corpus_dir() -> Result<PathBuf, ToolError> {
    std::env::var("INTAKE_CORPUS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::NotConfigured("INTAKE_CORPUS_DIR is not set".into()))
}

/// One manifest entry. Mirrors `tests/intake-corpus/manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct CodeCase {
    pub id: String,
    pub language: String,
    pub complexity: String,
    pub task_type: String,
    pub dir: String,
    pub input_files: Vec<String>,
    #[serde(default = "default_task_file")]
    pub task_file: String,
    #[serde(default = "default_validate")]
    pub validate: String,
    #[serde(default)]
    pub planted_bug: bool,
}

fn default_task_file() -> String { "task.md".to_string() }
fn default_validate() -> String { "validate.sh".to_string() }

/// Read + parse the manifest from `dir/manifest.json`.
pub fn read_manifest(dir: &Path) -> Result<Vec<CodeCase>, ToolError> {
    let path = dir.join("manifest.json");
    let body = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::NotConfigured(format!("intake corpus manifest not found at {}: {e}", path.display()))
    })?;
    serde_json::from_str(&body)
        .map_err(|e| ToolError::Execution(format!("manifest parse error: {e}")))
}

/// Filter cases to the requested languages (case-insensitive). Empty = all.
pub fn filter_cases(cases: &[CodeCase], languages: &[String]) -> Vec<CodeCase> {
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

// ---------------------------------------------------------------------------
// Prompt building + code extraction (pure)
// ---------------------------------------------------------------------------

/// Build the model prompt from the task description and the case input files.
/// The model is told to return the COMPLETE corrected file in a single fenced
/// code block per file, prefixed with a `// FILE: <path>` marker so multi-file
/// outputs can be split. Pure.
pub fn build_prompt(task: &str, files: &[(String, String)]) -> String {
    let mut p = String::new();
    p.push_str("You are an expert software engineer. Complete the task below.\n\n");
    p.push_str("=== TASK ===\n");
    p.push_str(task.trim());
    p.push_str("\n\n=== INPUT FILES ===\n");
    for (name, body) in files {
        p.push_str(&format!("\n--- {name} ---\n```\n{body}\n```\n"));
    }
    p.push_str(
        "\n=== OUTPUT FORMAT ===\n\
         Return the COMPLETE, FINAL contents of every file you changed. For each \
         file, emit a line `// FILE: <relative/path>` immediately followed by a \
         single fenced code block with the full file contents. Do not abbreviate, \
         do not use placeholders like `// ... unchanged`. If you changed only one \
         file, emit only that one.\n",
    );
    p
}

/// Extract files from a model response. Recognises `// FILE: path` markers
/// preceding fenced code blocks; falls back to returning all fenced code blocks
/// keyed by index when no markers are present. Returns (path_hint, code) pairs.
/// Pure.
pub fn extract_files(response: &str) -> Vec<(Option<String>, String)> {
    let blocks = extract_code_blocks(response);
    if blocks.is_empty() {
        return Vec::new();
    }
    // Find FILE markers and the byte offset of each fenced block to associate
    // a marker with the block that follows it.
    let mut out = Vec::new();
    let mut search_from = 0usize;
    for (idx, code) in blocks.iter().enumerate() {
        // Locate this block in the response (by its content) to find the
        // preceding marker. Cheap because blocks are unique substrings.
        let hint = if let Some(pos) = response[search_from..].find(code.as_str()) {
            let abs = search_from + pos;
            search_from = abs + code.len();
            file_marker_before(&response[..abs])
        } else {
            None
        };
        let _ = idx;
        out.push((hint, code.clone()));
    }
    out
}

/// Pull the path out of the last `// FILE: <path>` (or `# FILE:` / `<!-- FILE:`)
/// marker appearing in `prefix`. Pure.
fn file_marker_before(prefix: &str) -> Option<String> {
    let mut found = None;
    for line in prefix.lines() {
        let t = line.trim();
        for marker in ["// FILE:", "# FILE:", "<!-- FILE:", "FILE:"] {
            if let Some(rest) = t.strip_prefix(marker) {
                let path = rest.trim().trim_end_matches("-->").trim();
                if !path.is_empty() {
                    found = Some(path.to_string());
                }
            }
        }
    }
    found
}

/// Extract the inner text of every fenced ``` code block. Pure.
pub fn extract_code_blocks(response: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut buf = String::new();
    for line in response.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_block {
                blocks.push(std::mem::take(&mut buf));
                in_block = false;
            } else {
                in_block = true;
                buf.clear();
            }
            continue;
        }
        if in_block {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    // Unterminated final block — keep what we have (model may have been cut off).
    if in_block && !buf.trim().is_empty() {
        blocks.push(buf);
    }
    blocks
}

/// Choose which input file the model's output should replace. Strategy:
/// 1. If any extracted block carries a FILE marker matching (basename) one of
///    the case's input files, map each marked block to that input path.
/// 2. Otherwise, the single largest code block replaces the PRIMARY input file
///    (the last entry in `input_files` is typically the top-level script;
///    we instead target the first non-lib file, falling back to input[0]).
/// Returns a map of relative-input-path -> new content. Pure.
pub fn map_outputs_to_inputs(
    input_files: &[String],
    extracted: &[(Option<String>, String)],
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if extracted.is_empty() || input_files.is_empty() {
        return out;
    }
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();

    // Pass 1: marker-matched.
    for (hint, code) in extracted {
        if let Some(h) = hint {
            let hb = basename(h);
            if let Some(target) = input_files.iter().find(|f| basename(f) == hb || f.as_str() == h) {
                out.insert(target.clone(), code.clone());
            }
        }
    }
    if !out.is_empty() {
        return out;
    }

    // Pass 2: no usable markers — pick the primary input file and give it the
    // largest extracted block.
    let primary = primary_input(input_files);
    let largest = extracted
        .iter()
        .max_by_key(|(_, c)| c.len())
        .map(|(_, c)| c.clone())
        .unwrap_or_default();
    out.insert(primary, largest);
    out
}

/// Heuristic: the primary file is the first input not under a `lib/` path, else
/// the first input. Pure.
pub fn primary_input(input_files: &[String]) -> String {
    input_files
        .iter()
        .find(|f| !f.contains("/lib/") && !f.contains("lib/"))
        .or_else(|| input_files.first())
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Toolchain detection + graceful degradation (pure-ish)
// ---------------------------------------------------------------------------

/// Map a language to the executable its validator needs. `None` = always
/// runnable with bash/python/g++/sqlite3 (assumed present on the sweep-harness host).
pub fn required_toolchain(language: &str) -> Option<&'static str> {
    match language.to_lowercase().as_str() {
        "rust" => Some("cargo"),
        "typescript" | "ts" => Some("node"),
        "java" => Some("javac"),
        "go" => Some("go"),
        _ => None, // bash, python, cpp, sql, htmlcss, config
    }
}

/// The canonical (lowercase) language values that [`required_toolchain`] has an
/// explicit toolchain-check arm for. Kept as an explicit list rather than
/// derived from `required_toolchain` (which returns `None` for its catch-all,
/// so its checked languages can't be enumerated by probing it) — the
/// corpus-coverage reconciliation in `coder_sweep` needs the set, and this is
/// the single place that must stay in sync with the match above. `"ts"` is an
/// alias of `"typescript"` and is intentionally omitted (corpus cases use the
/// canonical `"typescript"`); listing both would double-warn.
pub fn toolchain_checked_languages() -> &'static [&'static str] {
    &["rust", "typescript", "java", "go"]
}

/// Pure set-difference for the corpus-coverage reconciliation
/// (multi-point-score-tracking): given the distinct languages actually present
/// in the corpus and the languages that have a toolchain check, return the
/// toolchain-checked languages with ZERO corpus cases — i.e. a validator gate
/// exists for them but nothing exercises it. Both inputs are compared
/// case-insensitively (lowercased) so a corpus `"Rust"` still covers a
/// toolchain-checked `"rust"`. Result is sorted for deterministic,
/// once-per-sweep warning order. Read-only: callers only log the gap.
pub fn toolchain_coverage_gaps(
    corpus_languages: &std::collections::BTreeSet<String>,
    toolchain_languages: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let corpus_lc: std::collections::BTreeSet<String> =
        corpus_languages.iter().map(|l| l.to_lowercase()).collect();
    toolchain_languages
        .iter()
        .map(|l| l.to_lowercase())
        .filter(|l| !corpus_lc.contains(l))
        .collect()
}

/// Pure computation of the `quality_per_gpu_second` routing signal — the same
/// number the `model_language_stats` matview derives in SQL, kept
/// independently testable outside the DB (same split-pure-logic-from-DB-I/O
/// pattern as [`toolchain_coverage_gaps`] above).
///
/// ## What it means
/// On the sweep-harness host a coder sweep runs under [`crate::intake::gpu_authority`]'s
/// `Exclusive` mode — competing services stopped, one Ollama-resident model at
/// a time — so wall-clock `total_time_ms` per case IS the GPU-time cost per
/// case (no other job is contending for the GPU during the sweep). Summed
/// across a model's cases and divided by 1000 that's `total_gpu_seconds`; per
/// scored case that's a mean GPU-seconds cost. This ratio answers "how much
/// quality did the model buy per GPU-second of budget it spent" — higher is
/// better (good quality achieved cheaply), the single cost-aware signal Chord's
/// batch-suitability score consumes.
///
/// `mean_score / (total_gpu_seconds / n_scored)`.
///
/// Returns `None` (not a panic, not an error) for the degenerate cases a real
/// model row can hit — zero scored cases or zero accumulated time — mirroring
/// the matview's `NULLIF` guards so a model that never produced a usable,
/// timed result yields a NULL cost signal rather than crashing the rollup.
pub fn quality_per_gpu_second(mean_score: f64, total_gpu_seconds: f64, n_scored: u64) -> Option<f64> {
    if n_scored == 0 || total_gpu_seconds == 0.0 {
        return None;
    }
    let cost_per_case_seconds = total_gpu_seconds / n_scored as f64;
    if cost_per_case_seconds == 0.0 {
        return None;
    }
    Some(mean_score / cost_per_case_seconds)
}

/// Whether `bin` is on PATH (synchronous `which`-style check).
pub fn have_tool(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Per-case execution (live)
// ---------------------------------------------------------------------------

/// Per-case inference timeout (overridable via `INTAKE_CODE_TIMEOUT_SEC`).
/// Delegates to the canonical resolver (Phase 2 item 3) — same default
/// (300s), same env var, same behavior.
fn code_timeout() -> Duration {
    super::timeouts::env_timeout("INTAKE_CODE_TIMEOUT_SEC", 300)
}

/// Result of running one code case (before DB insert).
#[derive(Debug, Clone, Default)]
pub struct CaseResult {
    pub passed: bool,
    pub row: CodeRunRow,
}

/// Copy a case dir into a fresh temp dir, overwrite the targeted input files
/// with the model's output, run validate.sh, and return pass/fail + captured
/// metrics. Toolchain-missing → graceful degrade (error set, passed=false).
async fn run_one_case(
    client: &reqwest::Client,
    model_name: &str,
    case: &CodeCase,
    corpus: &Path,
) -> CaseResult {
    let mut row = CodeRunRow {
        language: case.language.clone(),
        task_type: Some(case.task_type.clone()),
        ..Default::default()
    };
    let case_dir = corpus.join(&case.dir);

    // Read task + input files.
    let task = match std::fs::read_to_string(case_dir.join(&case.task_file)) {
        Ok(t) => t,
        Err(e) => {
            row.error = Some(format!("read task failed: {e}"));
            return CaseResult { passed: false, row };
        }
    };
    let mut files: Vec<(String, String)> = Vec::new();
    let mut total_lines = 0i32;
    for rel in &case.input_files {
        match std::fs::read_to_string(case_dir.join(rel)) {
            Ok(body) => {
                total_lines += body.lines().count() as i32;
                files.push((rel.clone(), body));
            }
            Err(_) => { /* binary/fixture file — skip from prompt, copied verbatim later */ }
        }
    }
    row.file_count = Some(case.input_files.len() as i32);
    row.total_lines = Some(total_lines);

    // Inference.
    let prompt = build_prompt(&task, &files);
    row.context_tokens = Some(context::estimate_tokens(&prompt) as i32);
    let gen = context::generate(client, model_name, &prompt, code_timeout()).await;
    row.throughput_tok_per_sec = gen.throughput_tok_per_sec;
    row.total_time_ms = gen.total_time_ms;
    if gen.oom {
        row.oom = true;
        row.error = Some(gen.error.unwrap_or_else(|| "OOM".into()));
        return CaseResult { passed: false, row };
    }
    if let Some(e) = gen.error {
        row.error = Some(e);
        return CaseResult { passed: false, row };
    }

    // Secondary LLM quality judge (best-effort; NULL on failure).
    row.code_quality_score = judge_quality(client, &task, &gen.response).await;

    // Toolchain gate.
    if let Some(bin) = required_toolchain(&case.language) {
        if !have_tool(bin) {
            row.error = Some(format!("toolchain unavailable: {}", case.language));
            return CaseResult { passed: false, row };
        }
    }

    // Materialise into a temp dir and run validate.sh.
    let extracted = extract_files(&gen.response);
    let outputs = map_outputs_to_inputs(&case.input_files, &extracted);
    if outputs.is_empty() {
        row.error = Some("no code block extracted from model response".into());
        return CaseResult { passed: false, row };
    }

    match materialise_and_validate(&case_dir, &case.validate, &outputs).await {
        Ok((passed, _stdout)) => {
            row.compiles = Some(passed);
            row.tests_pass = Some(passed);
            if case.planted_bug {
                row.planted_bug_found = Some(passed);
            }
            if !passed {
                row.error = Some("validate.sh failed".into());
            }
            CaseResult { passed, row }
        }
        Err(e) => {
            row.error = Some(format!("validate error: {e}"));
            CaseResult { passed: false, row }
        }
    }
}

/// Copy `case_dir` to a temp dir, overwrite targeted files, run `validate`.
/// Returns (passed, combined_output).
async fn materialise_and_validate(
    case_dir: &Path,
    validate: &str,
    outputs: &BTreeMap<String, String>,
) -> Result<(bool, String), ToolError> {
    let tmp = std::env::temp_dir().join(format!("intake-{}", uuid::Uuid::new_v4()));
    copy_dir_recursive(case_dir, &tmp)
        .map_err(|e| ToolError::Execution(format!("copy case failed: {e}")))?;

    for (rel, body) in outputs {
        let dest = tmp.join(rel);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&dest, body)
            .map_err(|e| ToolError::Execution(format!("write output {rel}: {e}")))?;
    }

    let script = tmp.join(validate);
    let _ = std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755));

    let out = tokio::process::Command::new("bash")
        .arg(&script)
        .current_dir(&tmp)
        .output()
        .await;
    let res = match out {
        Ok(o) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            Ok((o.status.success(), combined))
        }
        Err(e) => Err(ToolError::Execution(format!("spawn validate: {e}"))),
    };
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

/// Recursively copy a directory tree (std-only). Skips symlinks.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Ask qwen3:8b to rate the model's code 1-5. Returns None if unavailable/parse
/// failure (stored as NULL). Best-effort; never errors.
async fn judge_quality(client: &reqwest::Client, task: &str, response: &str) -> Option<f64> {
    let judge_model =
        std::env::var("INTAKE_JUDGE_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    let prompt = format!(
        "You are a strict code reviewer. Rate the following solution to the task on a \
         scale of 1 to 5 (5 = excellent, idiomatic, correct; 1 = broken/irrelevant). \
         Reply with ONLY the integer.\n\n=== TASK ===\n{}\n\n=== SOLUTION ===\n{}\n\nRating (1-5):",
        task.trim(),
        response.trim()
    );
    let out = context::generate(client, &judge_model, &prompt, Duration::from_secs(120)).await;
    if out.error.is_some() {
        return None;
    }
    parse_rating(&out.response)
}

/// Parse a 1-5 rating from judge text. Pure.
pub fn parse_rating(s: &str) -> Option<f64> {
    for tok in s.split(|c: char| !c.is_ascii_digit() && c != '.') {
        if let Ok(v) = tok.parse::<f64>() {
            if (1.0..=5.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Approval aggregation (pure)
// ---------------------------------------------------------------------------

/// Given (language, complexity, passed) tuples, compute the approved
/// "lang:complexity" tags: a pair is approved when ≥3 of its (up to 4) task
/// types pass. Pure. Returns sorted, deduped tags.
pub fn compute_approvals(results: &[(String, String, bool)]) -> Vec<String> {
    let mut counts: BTreeMap<(String, String), (u32, u32)> = BTreeMap::new();
    for (lang, complexity, passed) in results {
        let e = counts.entry((lang.clone(), complexity.clone())).or_insert((0, 0));
        e.1 += 1;
        if *passed {
            e.0 += 1;
        }
    }
    let mut tags: Vec<String> = counts
        .into_iter()
        .filter(|(_, (pass, _total))| *pass >= 3)
        .map(|((lang, complexity), _)| format!("{lang}:{complexity}"))
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// Map a complexity label to its file-count band (small=1, medium=3, large=6).
/// Pure — used to derive max_files_good / max_files_marginal.
fn complexity_files(complexity: &str) -> i32 {
    match complexity {
        "small" => 1,
        "medium" => 3,
        "large" => 6,
        _ => 1,
    }
}

/// From approval tags, derive max_files_good (largest approved complexity's band)
/// and max_files_marginal (one band beyond, capped). Pure.
pub fn derive_file_limits(approved: &[String]) -> (Option<i32>, Option<i32>) {
    let mut best = 0i32;
    for tag in approved {
        if let Some((_, complexity)) = tag.split_once(':') {
            best = best.max(complexity_files(complexity));
        }
    }
    if best == 0 {
        (None, None)
    } else {
        (Some(best), Some((best * 2).min(12)))
    }
}

// ---------------------------------------------------------------------------
// Suite driver (live)
// ---------------------------------------------------------------------------

/// Outcome of the code suite for the tool summary.
pub struct CodeSuiteOutcome {
    pub cases_run: usize,
    pub cases_passed: usize,
    pub approved: Vec<String>,
    pub toolchain_skipped: Vec<String>,
}

/// Run the code suite end-to-end. `profile_id` ties rows to an existing
/// model_profiles row (the context suite creates one; standalone runs pass a
/// freshly-created id from the runner). Stores one `code_profile_runs` row per
/// case and patches the operational profile with approvals + file limits.
pub async fn run_code_suite(
    model_name: &str,
    languages: &[String],
    profile_id: uuid::Uuid,
) -> Result<CodeSuiteOutcome, ToolError> {
    run_code_suite_limited(model_name, languages, profile_id, None).await
}

/// Like `run_code_suite`, but caps the number of cases (smoke runs).
pub async fn run_code_suite_limited(
    model_name: &str,
    languages: &[String],
    profile_id: uuid::Uuid,
    case_limit: Option<usize>,
) -> Result<CodeSuiteOutcome, ToolError> {
    let dir = corpus_dir()?;
    let all = read_manifest(&dir)?;
    let mut cases = filter_cases(&all, languages);
    if let Some(n) = case_limit {
        cases.truncate(n);
    }
    if cases.is_empty() {
        return Err(ToolError::NotConfigured(
            "no code cases match the requested languages".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(format!("client build failed: {e}")))?;
    let pool = storage::get_pool().await?;

    let mut results: Vec<(String, String, bool)> = Vec::new();
    let mut cases_passed = 0usize;
    let mut toolchain_skipped: Vec<String> = Vec::new();

    for case in &cases {
        let cr = run_one_case(&client, model_name, case, &dir).await;
        if cr.passed {
            cases_passed += 1;
        }
        if let Some(err) = &cr.row.error {
            if err.starts_with("toolchain unavailable") && !toolchain_skipped.contains(&case.language) {
                toolchain_skipped.push(case.language.clone());
            }
        }
        results.push((case.language.clone(), case.complexity.clone(), cr.passed));
        storage::insert_code_run(&pool, profile_id, &cr.row).await?;
    }

    let approved = compute_approvals(&results);
    let (max_good, max_marginal) = derive_file_limits(&approved);
    storage::update_op_code(&pool, profile_id, &approved, max_good, max_marginal).await?;

    Ok(CodeSuiteOutcome {
        cases_run: cases.len(),
        cases_passed,
        approved,
        toolchain_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial(intake_env)]
    fn corpus_dir_env_override() {
        std::env::remove_var("INTAKE_CORPUS_V2_DIR");
        std::env::set_var("INTAKE_CORPUS_DIR", "/tmp/corpus-x");
        assert_eq!(corpus_dir().unwrap(), PathBuf::from("/tmp/corpus-x"));
        std::env::remove_var("INTAKE_CORPUS_DIR");
        // PII remediation (2026-07): INTAKE_CORPUS_DIR has no compiled-in
        // default anymore — missing it must fail clean with NotConfigured.
        match corpus_dir() {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("INTAKE_CORPUS_DIR")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn extract_code_blocks_basic_and_unterminated() {
        let r = "intro\n```rust\nfn a(){}\n```\nmid\n```\nlet b=1;\n```\n";
        let b = extract_code_blocks(r);
        assert_eq!(b.len(), 2);
        assert!(b[0].contains("fn a(){}"));
        assert!(b[1].contains("let b=1;"));
        // Unterminated trailing block is still captured.
        let r2 = "```\npartial code\nno close";
        let b2 = extract_code_blocks(r2);
        assert_eq!(b2.len(), 1);
        assert!(b2[0].contains("partial code"));
    }

    #[test]
    fn extract_files_with_markers() {
        let r = "// FILE: src/a.rs\n```rust\nfn a(){}\n```\n// FILE: src/b.rs\n```rust\nfn b(){}\n```";
        let f = extract_files(r);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].0.as_deref(), Some("src/a.rs"));
        assert_eq!(f[1].0.as_deref(), Some("src/b.rs"));
    }

    #[test]
    fn map_outputs_marker_match_by_basename() {
        let inputs = vec!["input/extract.sh".to_string(), "input/sample.csv".to_string()];
        let extracted = vec![(Some("extract.sh".to_string()), "new body".to_string())];
        let m = map_outputs_to_inputs(&inputs, &extracted);
        assert_eq!(m.get("input/extract.sh").map(|s| s.as_str()), Some("new body"));
        assert!(!m.contains_key("input/sample.csv"));
    }

    #[test]
    fn map_outputs_fallback_to_primary() {
        let inputs = vec!["input/lib/backoff.sh".to_string(), "input/run.sh".to_string()];
        let extracted = vec![(None, "short".to_string()), (None, "the larger block here".to_string())];
        let m = map_outputs_to_inputs(&inputs, &extracted);
        // primary = first non-lib = input/run.sh, gets the largest block.
        assert_eq!(m.get("input/run.sh").map(|s| s.as_str()), Some("the larger block here"));
    }

    #[test]
    fn primary_input_prefers_non_lib() {
        assert_eq!(primary_input(&["a/lib/x.sh".into(), "a/main.sh".into()]), "a/main.sh");
        assert_eq!(primary_input(&["only.sh".into()]), "only.sh");
    }

    #[test]
    fn required_toolchain_mapping() {
        assert_eq!(required_toolchain("rust"), Some("cargo"));
        assert_eq!(required_toolchain("TypeScript"), Some("node"));
        assert_eq!(required_toolchain("java"), Some("javac"));
        assert_eq!(required_toolchain("Java"), Some("javac"));
        assert_eq!(required_toolchain("go"), Some("go"));
        assert_eq!(required_toolchain("Go"), Some("go"));
        assert_eq!(required_toolchain("python"), None);
        assert_eq!(required_toolchain("bash"), None);
        assert_eq!(required_toolchain("cpp"), None);
    }

    #[test]
    fn toolchain_checked_languages_matches_required_toolchain_arms() {
        // Every listed language must genuinely have a toolchain check, and the
        // canonical spellings only (no `"ts"` alias).
        for lang in toolchain_checked_languages() {
            assert!(required_toolchain(lang).is_some(), "{lang} should be checked");
        }
        assert!(toolchain_checked_languages().contains(&"rust"));
        assert!(toolchain_checked_languages().contains(&"typescript"));
        assert!(toolchain_checked_languages().contains(&"java"));
        assert!(toolchain_checked_languages().contains(&"go"));
        assert!(!toolchain_checked_languages().contains(&"ts"));
    }

    #[test]
    fn toolchain_coverage_gaps_flags_missing_and_sorts() {
        use std::collections::BTreeSet;
        let toolchain: BTreeSet<String> =
            ["rust", "typescript"].iter().map(|s| s.to_string()).collect();

        // typescript has zero corpus cases → reported; rust is covered.
        let corpus: BTreeSet<String> =
            ["rust", "python", "bash"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            toolchain_coverage_gaps(&corpus, &toolchain),
            vec!["typescript".to_string()]
        );

        // Case-insensitive: "Rust" in the corpus still covers "rust".
        let corpus_mixed: BTreeSet<String> =
            ["Rust", "TypeScript"].iter().map(|s| s.to_string()).collect();
        assert!(toolchain_coverage_gaps(&corpus_mixed, &toolchain).is_empty());

        // Both missing → both reported, sorted.
        let corpus_none: BTreeSet<String> = ["python"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            toolchain_coverage_gaps(&corpus_none, &toolchain),
            vec!["rust".to_string(), "typescript".to_string()]
        );
    }

    #[test]
    fn quality_per_gpu_second_basic_ratio() {
        // 4 cases, 20 GPU-seconds total ⇒ 5 s/case; mean_score 4.0 ⇒ 0.8.
        let q = quality_per_gpu_second(4.0, 20.0, 4).unwrap();
        assert!((q - 0.8).abs() < 1e-9, "got {q}");
    }

    #[test]
    fn quality_per_gpu_second_higher_is_better_when_cheaper() {
        // Same quality, less GPU time ⇒ a strictly higher (better) signal.
        let cheap = quality_per_gpu_second(4.0, 10.0, 4).unwrap();
        let dear = quality_per_gpu_second(4.0, 40.0, 4).unwrap();
        assert!(cheap > dear, "cheaper model must score higher: {cheap} vs {dear}");
    }

    #[test]
    fn quality_per_gpu_second_zero_scored_is_none() {
        // A model with no scored cases yields NULL, not a divide-by-zero.
        assert_eq!(quality_per_gpu_second(4.0, 20.0, 0), None);
    }

    #[test]
    fn quality_per_gpu_second_zero_time_is_none() {
        // Zero accumulated GPU time (e.g. every case errored before timing)
        // yields NULL rather than an infinity/crash.
        assert_eq!(quality_per_gpu_second(4.0, 0.0, 4), None);
    }

    #[test]
    fn parse_rating_extracts_1_to_5() {
        assert_eq!(parse_rating("4"), Some(4.0));
        assert_eq!(parse_rating("I rate this a 3 out of 5"), Some(3.0));
        assert_eq!(parse_rating("rating: 5"), Some(5.0));
        assert_eq!(parse_rating("no number"), None);
        assert_eq!(parse_rating("99"), None); // out of range
    }

    #[test]
    fn compute_approvals_threshold() {
        // rust:small 3/4 pass → approved; rust:medium 2/4 → not.
        let r = vec![
            ("rust".into(), "small".into(), true),
            ("rust".into(), "small".into(), true),
            ("rust".into(), "small".into(), true),
            ("rust".into(), "small".into(), false),
            ("rust".into(), "medium".into(), true),
            ("rust".into(), "medium".into(), true),
            ("rust".into(), "medium".into(), false),
            ("rust".into(), "medium".into(), false),
        ];
        let a = compute_approvals(&r);
        assert_eq!(a, vec!["rust:small".to_string()]);
    }

    #[test]
    fn derive_file_limits_from_approvals() {
        assert_eq!(derive_file_limits(&[]), (None, None));
        let (g, m) = derive_file_limits(&["rust:small".into(), "rust:medium".into()]);
        assert_eq!(g, Some(3)); // medium band
        assert_eq!(m, Some(6));
        let (g2, _) = derive_file_limits(&["python:large".into()]);
        assert_eq!(g2, Some(6));
    }

    #[test]
    fn filter_cases_by_language() {
        let cases = vec![
            CodeCase { id: "a".into(), language: "bash".into(), complexity: "small".into(),
                task_type: "bug_fix".into(), dir: "bash/small/bug_fix/a".into(),
                input_files: vec![], task_file: default_task_file(), validate: default_validate(),
                planted_bug: true },
            CodeCase { id: "b".into(), language: "rust".into(), complexity: "small".into(),
                task_type: "bug_fix".into(), dir: "rust/small/bug_fix/b".into(),
                input_files: vec![], task_file: default_task_file(), validate: default_validate(),
                planted_bug: false },
        ];
        assert_eq!(filter_cases(&cases, &[]).len(), 2);
        assert_eq!(filter_cases(&cases, &["bash".into()]).len(), 1);
        assert_eq!(filter_cases(&cases, &["RUST".into()])[0].id, "b");
    }

    #[test]
    fn build_prompt_includes_task_and_files() {
        let p = build_prompt("Fix the bug", &[("a.sh".into(), "echo hi".into())]);
        assert!(p.contains("Fix the bug"));
        assert!(p.contains("a.sh"));
        assert!(p.contains("echo hi"));
        assert!(p.contains("FILE:"));
    }
}
