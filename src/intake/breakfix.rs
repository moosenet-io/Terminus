//! MINT Phase 4: the breakfix subagent.
//!
//! ## What this is
//! [`supervisor::tick`] invokes [`supervisor::BreakfixHandler::handle_repeat_stuck`]
//! when the SAME `(model, backend, mem_config)` combo has jammed
//! [`supervisor::REPEAT_STUCK_THRESHOLD`]+ times within
//! [`supervisor::REPEAT_STUCK_WINDOW_SEC`] — a plain restart clearly isn't
//! fixing it. Phase 3 shipped only a logging-only default
//! ([`supervisor::LoggingBreakfixHandler`]); this module supplies the real
//! handler, [`SubagentBreakfix`], WITHOUT touching `tick()` or the supervisor
//! loop at all — it plugs into the existing trait seam.
//!
//! ## Known caveat this is built defensively around (flagged by both Opus and
//! Codex in the Phase 3 review)
//! [`ComboKey`] attribution during an active jam is based on the
//! most-recently-COMPLETED `code_profile_runs` row (batch-written at suite
//! completion in some code paths), not necessarily the actually-stuck combo —
//! it can misattribute right after a model switch. This handler NEVER acts on
//! the attributed combo blindly: every candidate fix is verified with an
//! actual single-case retest ([`RetestHook`]) before the loop decides
//! anything, and a failed retest just feeds back into the SAME loop (as
//! evidence for the next reasoning-backend call), it never short-circuits to
//! "must be broken" on the label alone.
//!
//! ## Reasoning backend chain
//! Primary: a headless `claude` CLI subprocess ([`ClaudeCliBackend`]).
//! Fallback (triggered on missing binary / auth error / any spawn failure):
//! local CPU-backed Ollama ([`OllamaCpuBackend`], deliberately NOT the GPU
//! backend — breakfix's own reasoning must never contend for the GPU it is
//! diagnosing). Both sit behind [`ReasoningBackend`] so tests inject a mock
//! and never make a real subprocess/network call ([`ChainedBackend`] wires
//! the two together with the same fallback rule the supervisor uses for
//! restart-recovery: never let one failure mode block the other's job).
//!
//! ## Bounded diagnostic loop (NOT an open-ended agentic loop)
//! [`decide_breakfix`] is a pure-over-traits loop: propose an alternate
//! config, verify it with a single-case retest, repeat up to
//! [`MAX_ATTEMPTS`] times, then MUST resolve to `drop` or `escalate` — see
//! that function's doc for the exact termination argument (this is also
//! covered by an adversarial "always retry" test in this module).
//!
//! ## `codefix` — DELIBERATE SCOPE NARROWING
//! A `codefix(...)` verdict is logged clearly as "requested but not yet
//! auto-executed in this phase" and then handled exactly like `escalate`.
//! Full autonomous code-fix-and-deploy (worktree/test/dual-review/merge/
//! deploy automation) is explicitly OUT OF SCOPE for Phase 4 — see the
//! `Decision::CodefixDeferred` arm in [`SubagentBreakfix::handle_async`] for
//! the TODO marking where a follow-up phase would plug in real execution.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::config;
use crate::error::ToolError;

use super::gpu_authority::{self, GpuMode};
use super::supervisor::{self, BreakfixHandler, BreakfixOutcome, ComboKey};

/// Hard ceiling on propose-alternate-config → single-case-retest cycles
/// before this handler MUST resolve to `drop` or `escalate`. See
/// [`decide_breakfix`] for the termination argument and
/// `tests::adversarial_always_retry_backend_terminates_at_budget` for the
/// proof.
pub const MAX_ATTEMPTS: u8 = 5;

// ── Structured verdict contract ──────────────────────────────────────────────

/// What a reasoning-backend reply decided, per the machine-parseable
/// `VERDICT:` line contract (see module doc / [`parse_verdict`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Try again with an alternate config (e.g. a different `mem_config` /
    /// context size / backend). `config` is the raw `key:value[,key:value]`
    /// payload from `retry(config=...)`, interpreted by [`RetestHook`].
    Retry { config: String },
    /// Stop trying this combo permanently; record it as a known-bad config.
    Drop { reason: String },
    /// The backend believes an actual code fix is needed. Phase 4 does NOT
    /// auto-execute this (see module doc) — handled like `escalate`.
    Codefix { detail: String },
    /// Hand off to a human; no further automated action.
    Escalate { reason: String },
}

/// Parse the backend's structured verdict. Deterministic: scans for a line
/// matching EXACTLY `VERDICT: <form>(...)` (after trimming), and parses only
/// that line — never prose-scans the rest of the response. Any of the four
/// known forms is recognized; anything else (a `VERDICT:` line in an
/// unrecognized shape, or NO `VERDICT:` line at all) is treated as
/// `escalate(reason="unparseable backend response")` — the safe default per
/// the Phase 4 spec ("if no parseable VERDICT line is found, treat as
/// escalate").
pub fn parse_verdict(text: &str) -> Verdict {
    for line in text.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("VERDICT:") else {
            continue;
        };
        let rest = rest.trim();
        if let Some(inner) = strip_call(rest, "retry") {
            let config = extract_kv(inner, "config").unwrap_or_default();
            return Verdict::Retry { config };
        }
        if let Some(inner) = strip_call(rest, "drop") {
            let reason = extract_kv(inner, "reason").unwrap_or_else(|| inner.to_string());
            return Verdict::Drop { reason };
        }
        if let Some(inner) = strip_call(rest, "codefix") {
            return Verdict::Codefix {
                detail: inner.to_string(),
            };
        }
        if let Some(inner) = strip_call(rest, "escalate") {
            let reason = extract_kv(inner, "reason").unwrap_or_else(|| inner.to_string());
            return Verdict::Escalate { reason };
        }
        // A `VERDICT:` line IS present but doesn't match any known form —
        // still a deterministic, clearly-labeled escalate, not a silent guess.
        return Verdict::Escalate {
            reason: format!("unparseable VERDICT line: {rest}"),
        };
    }
    Verdict::Escalate {
        reason: "unparseable backend response (no VERDICT line found)".to_string(),
    }
}

/// `name(...)` → the trimmed inner content, or `None` if `s` doesn't start
/// with `name(` / doesn't end in a matching `)`. Pure string surgery — no
/// regex dependency needed for this fixed grammar.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let s = s.strip_prefix(name)?;
    let s = s.trim_start();
    let s = s.strip_prefix('(')?;
    let s = s.strip_suffix(')')?;
    Some(s.trim())
}

/// Extract `key`'s value from `inner`, which per the fixed VERDICT grammar
/// ALWAYS has the form `key=<value to end of string>` — i.e. `key` is the
/// sole field for that verdict form, so everything after `key=` (not just up
/// to the next comma) is the value. This matters because the value itself is
/// free-form and may legitimately contain commas — [`AltConfig`]'s own
/// `key:value,key:value` payload for `retry(config=...)`, or a `reason=...`
/// prose string with commas in it. Returns `None` if `inner` doesn't start
/// with `key=`.
fn extract_kv(inner: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    inner.strip_prefix(&prefix).map(|v| v.trim().to_string())
}

// ── Reasoning backend abstraction ────────────────────────────────────────────

/// What a [`ReasoningBackend`] call produced.
#[derive(Debug, Clone)]
pub enum BackendReply {
    /// Raw text response (to be parsed via [`parse_verdict`]).
    Text(String),
    /// The backend could not be used at all this call (missing binary, spawn
    /// failure, auth error, timeout, network error, ...). Never a panic.
    Unavailable(String),
}

/// A source of breakfix reasoning. Abstracted so tests inject a scripted mock
/// and never spawn a real subprocess or make a real network call.
#[async_trait::async_trait]
pub trait ReasoningBackend: Send + Sync {
    async fn ask(&self, prompt: &str) -> BackendReply;
}

/// Env vars that must NEVER reach the `claude` child process (or any log
/// line) — checked defensively even on TOP of the allowlist below, so a
/// future allowlist edit can't accidentally reintroduce a secret.
fn is_secret_like_env_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.ends_with("_TOKEN") || k.ends_with("_SECRET") || k.ends_with("_KEY") || k.starts_with("INFISICAL_") || k.starts_with("CHORD_JWT")
}

/// The ONLY env vars ever passed through to the `claude` child process — a
/// minimal, known-safe set (shell/locale plumbing the CLI itself may need),
/// never the parent's full environment. Each entry is ALSO re-checked against
/// [`is_secret_like_env_key`] at use time (belt-and-suspenders: even if this
/// list ever grows a bad entry, the secret-shaped filter still catches it).
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];

/// Build the sanitized env for the `claude` child: allowlist-only, with a
/// secret-shaped-key filter applied on top. Public (not `pub`) purely so a
/// unit test can assert on it directly without spawning anything.
fn sanitized_child_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| ENV_ALLOWLIST.contains(&k.as_str()))
        .filter(|(k, _)| !is_secret_like_env_key(k))
        .collect()
}

/// Heuristic auth-failure detection (mirrors
/// `assistant::judges::looks_like_auth_error`, duplicated rather than made
/// `pub` across modules to keep this module's subprocess handling
/// self-contained).
fn looks_like_auth_error(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [
        "not authenticated",
        "not logged in",
        "please log in",
        "please login",
        "unauthorized",
        "authentication failed",
        "no api key",
        "invalid api key",
        "login required",
        "auth error",
        "expired token",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// Truncate + strip control chars for safe audit/log storage (mirrors
/// `assistant::judges::redact`).
fn redact(s: &str) -> String {
    const RAW_AUDIT_MAX: usize = 2000;
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.len() > RAW_AUDIT_MAX {
        format!("{}…[truncated]", &trimmed[..RAW_AUDIT_MAX])
    } else {
        trimmed.to_string()
    }
}

/// Primary reasoning backend: a headless `claude` CLI subprocess —
/// `claude --model <model> -p <prompt> --output-format text --tools ""`.
/// Spawned with a SANITIZED environment (see [`sanitized_child_env`]) so no
/// operator secret (<secret-manager> tokens, Chord JWTs, API keys, ...) can leak to
/// the child process.
pub struct ClaudeCliBackend {
    cli: String,
    model: String,
    timeout: Duration,
}

impl ClaudeCliBackend {
    pub fn from_env() -> Self {
        ClaudeCliBackend {
            cli: config::breakfix_claude_cli(),
            model: config::breakfix_claude_model(),
            timeout: Duration::from_secs(config::breakfix_timeout_secs()),
        }
    }
}

#[async_trait::async_trait]
impl ReasoningBackend for ClaudeCliBackend {
    async fn ask(&self, prompt: &str) -> BackendReply {
        use tokio::process::Command;

        let mut cmd = Command::new(&self.cli);
        // `env_clear()` first: the child inherits NOTHING from this process
        // except what we explicitly re-add below. This is the load-bearing
        // line for secret-sanitization — everything after it is additive.
        cmd.env_clear();
        for (k, v) in sanitized_child_env() {
            cmd.env(k, v);
        }
        cmd.arg("--model")
            .arg(&self.model)
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("text")
            .arg("--tools")
            .arg("");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return BackendReply::Unavailable(format!("cannot launch '{}': {e}", self.cli));
            }
        };
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return BackendReply::Unavailable(format!("'{}' failed: {e}", self.cli)),
            Err(_) => return BackendReply::Unavailable(format!("'{}' timed out", self.cli)),
        };
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            if looks_like_auth_error(&stderr) || looks_like_auth_error(&stdout) {
                return BackendReply::Unavailable(format!("'{}' not authenticated", self.cli));
            }
            if stdout.trim().is_empty() {
                return BackendReply::Unavailable(format!(
                    "'{}' exited nonzero: {}",
                    self.cli,
                    redact(&stderr)
                ));
            }
        } else if looks_like_auth_error(&stdout) {
            return BackendReply::Unavailable(format!("'{}' not authenticated", self.cli));
        }
        BackendReply::Text(stdout)
    }
}

/// Fallback reasoning backend: local CPU-backed Ollama (`OLLAMA_CPU_URL`,
/// deliberately NOT the GPU backend). Used only when the primary is
/// unavailable (missing binary / spawn failure / auth error / timeout).
pub struct OllamaCpuBackend {
    url: String,
    model: String,
    timeout: Duration,
}

impl OllamaCpuBackend {
    pub fn from_env() -> Self {
        OllamaCpuBackend {
            url: config::breakfix_ollama_cpu_url(),
            model: config::breakfix_fallback_model(),
            timeout: Duration::from_secs(config::breakfix_timeout_secs()),
        }
    }
}

#[async_trait::async_trait]
impl ReasoningBackend for OllamaCpuBackend {
    async fn ask(&self, prompt: &str) -> BackendReply {
        let client = match reqwest::Client::builder().timeout(self.timeout).build() {
            Ok(c) => c,
            Err(e) => return BackendReply::Unavailable(format!("ollama client build failed: {e}")),
        };
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
        });
        let url = format!("{}/api/generate", self.url.trim_end_matches('/'));
        match client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    let text = v
                        .get("response")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if text.trim().is_empty() {
                        BackendReply::Unavailable("ollama fallback returned empty response".to_string())
                    } else {
                        BackendReply::Text(text)
                    }
                }
                Err(e) => BackendReply::Unavailable(format!("ollama fallback response parse failed: {e}")),
            },
            Ok(resp) => BackendReply::Unavailable(format!("ollama fallback HTTP {}", resp.status())),
            Err(e) => BackendReply::Unavailable(format!("ollama fallback request failed: {e}")),
        }
    }
}

/// Wires [`ClaudeCliBackend`] (primary) to [`OllamaCpuBackend`] (fallback):
/// any `Unavailable` from the primary (missing binary, auth error, spawn
/// failure, timeout — the spec's exact trigger list) falls through to the
/// fallback. If the fallback ALSO fails, the caller sees `Unavailable` and
/// [`decide_breakfix`] escalates (never crashes, never hangs).
pub struct ChainedBackend {
    primary: Box<dyn ReasoningBackend>,
    fallback: Box<dyn ReasoningBackend>,
}

impl ChainedBackend {
    pub fn new(primary: Box<dyn ReasoningBackend>, fallback: Box<dyn ReasoningBackend>) -> Self {
        ChainedBackend { primary, fallback }
    }
}

#[async_trait::async_trait]
impl ReasoningBackend for ChainedBackend {
    async fn ask(&self, prompt: &str) -> BackendReply {
        match self.primary.ask(prompt).await {
            BackendReply::Text(t) => BackendReply::Text(t),
            BackendReply::Unavailable(msg) => {
                tracing::warn!(
                    "breakfix: primary reasoning backend unavailable ({msg}); falling back to CPU ollama"
                );
                self.fallback.ask(prompt).await
            }
        }
    }
}

// ── Single-case retest hook ──────────────────────────────────────────────────

/// Result of retesting `combo` (with an alternate config applied) against one
/// representative case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetestResult {
    /// The case ran clean (scored, no error) under the alternate config.
    Success,
    /// The case still failed; `error_class` is a short, stable classification
    /// fed back into the next reasoning-backend prompt as evidence.
    Failure { error_class: String },
}

/// Verifies a candidate fix by ACTUALLY retesting the combo — never trusts
/// the attributed [`ComboKey`] blindly (see module doc's misattribution
/// caveat). Abstracted so tests inject a scripted mock and never touch the
/// GPU / Postgres / corpus manifest for real.
#[async_trait::async_trait]
pub trait RetestHook: Send + Sync {
    async fn retest(&self, combo: &ComboKey, alt_config: &str) -> RetestResult;
}

/// Parsed pieces of a `retry(config=...)` payload the backend may propose.
/// Free-form `key:value[,key:value]`; unknown keys are ignored (bounded
/// diagnostic surface, not an open-ended config language).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AltConfig {
    backend: Option<String>,
    mem_config: Option<String>,
}

fn parse_alt_config(raw: &str) -> AltConfig {
    let mut out = AltConfig::default();
    for part in raw.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once(':') {
            match k.trim() {
                "backend" => out.backend = Some(v.trim().to_string()),
                "mem_config" => out.mem_config = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    out
}

/// The GPU-authority holder label this retest acquires under — DISTINCT from
/// `coder_sweep`'s and `coder_case`'s labels (see `coder_case::GPU_HOLDER`'s
/// doc for why the label must not collide): a breakfix retest must refuse to
/// start (not race) while a real sweep or an operator's ad hoc case rerun
/// holds the GPU.
pub const BREAKFIX_GPU_HOLDER: &str = "mint_breakfix";

/// Live [`RetestHook`]: acquires the GPU exclusively under
/// [`BREAKFIX_GPU_HOLDER`], picks the FIRST case in the v2 corpus manifest as
/// a stable, fast representative case (a full diagnostic corpus sweep is out
/// of scope — this is a bounded sanity check, not a re-profiling run), and
/// runs it through the existing single-case suite driver
/// (`intake::run_code_suite_v2_cases`) with whatever `alt_config` overrides
/// parse out.
pub struct LiveRetestHook;

#[async_trait::async_trait]
impl RetestHook for LiveRetestHook {
    async fn retest(&self, combo: &ComboKey, alt_config: &str) -> RetestResult {
        let overrides = parse_alt_config(alt_config);
        let backend = overrides.backend.unwrap_or_else(|| combo.backend.clone());
        let mem_config = overrides.mem_config.or_else(|| combo.mem_config.clone());

        let dir = super::corpus_v2_dir();
        let cases = match super::read_manifest_v2(&dir) {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => {
                return RetestResult::Failure {
                    error_class: "corpus_v2_manifest_empty".to_string(),
                }
            }
            Err(e) => {
                return RetestResult::Failure {
                    error_class: format!("manifest_read_failed: {e}"),
                }
            }
        };
        let case_id = cases[0].id.clone();

        let _guard = match gpu_authority::ExclusiveGuard::acquire(GpuMode::Exclusive, BREAKFIX_GPU_HOLDER) {
            Ok(g) => g,
            Err(e) => {
                return RetestResult::Failure {
                    error_class: format!("gpu_authority_unavailable: {e}"),
                }
            }
        };

        let override_str = super::coder_case::override_str_for_backend(&backend);
        super::infer::set_backend_override(Some(override_str.to_string()));
        struct ClearOverride;
        impl Drop for ClearOverride {
            fn drop(&mut self) {
                super::infer::set_backend_override(None);
            }
        }
        let _clear = ClearOverride;

        let profile_id = match super::create_profile_row(&combo.model).await {
            Ok(id) => id,
            Err(e) => {
                return RetestResult::Failure {
                    error_class: format!("profile_row_create_failed: {e}"),
                }
            }
        };

        let outcome = super::run_code_suite_v2_cases(
            &combo.model,
            &[],
            Some(std::slice::from_ref(&case_id)),
            profile_id,
            None,
            Some(&backend),
            mem_config.as_deref(),
        )
        .await;

        match outcome {
            Ok(o) if o.errors == 0 && o.scored > 0 => RetestResult::Success,
            Ok(o) => RetestResult::Failure {
                error_class: format!("case_retest_failed(scored={},errors={})", o.scored, o.errors),
            },
            Err(e) => RetestResult::Failure {
                error_class: format!("case_retest_error: {e}"),
            },
        }
    }
}

// ── Bounded decision loop (pure over traits — the tested core) ──────────────

/// The handler's final decision for one repeat-stuck escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A retry succeeded — the combo works under the alternate config.
    Resolved,
    Drop {
        reason: String,
        attempts: u8,
        last_error_class: String,
    },
    Escalate {
        reason: String,
        attempts: u8,
    },
    /// A `codefix` verdict was returned — deliberately NOT auto-executed in
    /// this phase (see module doc). The caller treats this like `Escalate`.
    CodefixDeferred {
        detail: String,
        attempts: u8,
    },
}

/// Build the prompt handed to the reasoning backend for attempt `attempt`
/// (1-indexed). Includes the combo, how many times it has repeat-stuck, the
/// gathered diagnostic context, and — from the second attempt on — the
/// previous attempt's retest failure class as fresh evidence. Always ends in
/// the exact instruction to reply with the `VERDICT:` contract line.
fn build_prompt(
    combo: &ComboKey,
    recovery_count: usize,
    attempt: u8,
    context: &str,
    last_error_class: Option<&str>,
) -> String {
    let mut p = format!(
        "You are the MINT breakfix subagent diagnosing a jammed model-profiling combo.\n\
         combo: model={} backend={} mem_config={}\n\
         This combo has been repeat-stuck {recovery_count} times within the escalation window.\n\
         This is attempt {attempt} of {MAX_ATTEMPTS}.\n\n\
         Diagnostic context (recent supervisor log lines and DB rows for this combo):\n{context}\n",
        combo.model,
        combo.backend,
        combo.mem_config.as_deref().unwrap_or("NULL"),
    );
    if let Some(class) = last_error_class {
        p.push_str(&format!(
            "\nThe previous attempt's single-case retest FAILED with: {class}\n"
        ));
    }
    p.push_str(
        "\nDecide one of: retry an alternate config (propose backend and/or mem_config \
         overrides, format `key:value[,key:value]`, e.g. `backend:cpu,mem_config:carveout`), \
         drop this config permanently, request a code fix (out of scope for automatic \
         execution this phase — will be logged and escalated), or escalate to a human.\n\
         Your reply MUST end with EXACTLY one line in this format (nothing after it):\n\
         VERDICT: retry(config=...) | drop(reason=...) | codefix(...) | escalate(reason=...)\n",
    );
    p
}

/// The bounded diagnostic loop: propose an alternate config, verify with a
/// single-case retest, up to [`MAX_ATTEMPTS`] times.
///
/// ## Termination argument (why this can never loop unbounded)
/// The `for attempt in 1..=MAX_ATTEMPTS` loop only continues past an
/// iteration on `Verdict::Retry` + `RetestResult::Failure` — every OTHER
/// combination (`Drop`, `Escalate`, `Codefix`, a `Retry` whose retest
/// SUCCEEDS, or the backend itself being `Unavailable`) returns immediately.
/// So the only way to reach `attempt == MAX_ATTEMPTS` without returning is an
/// adversarial (or genuinely persistent) backend that says `retry` every
/// single time AND every retest fails — and even then, the loop body itself
/// is bounded by the `for` range: after the `MAX_ATTEMPTS`-th iteration the
/// loop simply ends, and the function falls through to a forced `Drop`
/// below. There is no recursion, no unbounded `loop {}`, and no path that
/// re-enters the loop after it ends — this is proven by
/// `tests::adversarial_always_retry_backend_terminates_at_budget`, which
/// injects a mock backend that ALWAYS replies `retry` and a mock retest hook
/// that ALWAYS fails, and asserts both that the backend/retest were each
/// called exactly [`MAX_ATTEMPTS`] times and that the function returns
/// (rather than hanging) with a `Drop` decision.
pub async fn decide_breakfix(
    combo: &ComboKey,
    recovery_count: usize,
    backend: &dyn ReasoningBackend,
    retest: &dyn RetestHook,
    context: &str,
) -> Decision {
    let mut last_error_class: Option<String> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        let prompt = build_prompt(combo, recovery_count, attempt, context, last_error_class.as_deref());
        let reply = backend.ask(&prompt).await;
        let text = match reply {
            BackendReply::Text(t) => t,
            BackendReply::Unavailable(msg) => {
                // Both primary and fallback are unavailable (ChainedBackend
                // already tried both) — nothing left to reason with.
                return Decision::Escalate {
                    reason: format!("reasoning backend unavailable: {msg}"),
                    attempts: attempt,
                };
            }
        };

        match parse_verdict(&text) {
            Verdict::Retry { config } => match retest.retest(combo, &config).await {
                RetestResult::Success => return Decision::Resolved,
                RetestResult::Failure { error_class } => {
                    last_error_class = Some(error_class);
                    continue;
                }
            },
            Verdict::Drop { reason } => {
                return Decision::Drop {
                    reason,
                    attempts: attempt,
                    last_error_class: last_error_class.unwrap_or_else(|| "none".to_string()),
                };
            }
            Verdict::Codefix { detail } => {
                return Decision::CodefixDeferred { detail, attempts: attempt };
            }
            Verdict::Escalate { reason } => {
                return Decision::Escalate { reason, attempts: attempt };
            }
        }
    }

    // Budget exhausted: every attempt proposed `retry` and every retest
    // failed. MUST resolve now — never loop unbounded. A `Drop` (not
    // `Escalate`) because we have concrete, repeated retest-failure evidence
    // that this exact combo does not work; that is exactly what
    // `mint_dropped_configs` exists to record.
    Decision::Drop {
        reason: "attempt budget exhausted: all retry attempts' retests failed".to_string(),
        attempts: MAX_ATTEMPTS,
        last_error_class: last_error_class.unwrap_or_else(|| "none".to_string()),
    }
}

// ── mint_dropped_configs (new table) ─────────────────────────────────────────

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the new drop-ledger table —
/// same convention as `storage::ensure_operational_profile`'s self-healing
/// schema guard. Safe to call on every drop (cheap, no-op once it exists).
pub async fn ensure_dropped_configs_table(pool: &PgPool) -> Result<(), ToolError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mint_dropped_configs ( \
            id BIGSERIAL PRIMARY KEY, \
            model TEXT NOT NULL, \
            backend TEXT NOT NULL, \
            mem_config TEXT, \
            dropped_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            reason TEXT NOT NULL, \
            attempts_made INTEGER NOT NULL, \
            last_error_class TEXT \
         )",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create mint_dropped_configs: {e}")))?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mint_dropped_configs_model \
         ON mint_dropped_configs(model, backend)",
    )
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("create idx_mint_dropped_configs_model: {e}")))?;
    Ok(())
}

/// SQL for [`write_dropped_config`] — pulled into a const so a unit test can
/// assert on its shape (parameterized, no string-interpolated values) without
/// needing a live DB.
const INSERT_DROPPED_CONFIG_SQL: &str = "INSERT INTO mint_dropped_configs \
     (model, backend, mem_config, reason, attempts_made, last_error_class) \
     VALUES ($1, $2, $3, $4, $5, $6)";

/// Persist a `drop` decision. Fully parameterized (`$1..$6`) — no
/// string-interpolated SQL, so operator-influenced free text (`reason`,
/// `last_error_class`, and the combo's own `model`/`mem_config` strings,
/// which ultimately trace back to a model registry entry an operator
/// controls) can never be interpreted as SQL.
pub async fn write_dropped_config(
    pool: &PgPool,
    combo: &ComboKey,
    attempts_made: u8,
    reason: &str,
    last_error_class: &str,
) -> Result<(), ToolError> {
    ensure_dropped_configs_table(pool).await?;
    sqlx::query(INSERT_DROPPED_CONFIG_SQL)
        .bind(&combo.model)
        .bind(&combo.backend)
        .bind(combo.mem_config.as_deref())
        .bind(reason)
        .bind(attempts_made as i32)
        .bind(last_error_class)
        .execute(pool)
        .await
        .map_err(|e| ToolError::Database(format!("insert mint_dropped_configs: {e}")))?;
    Ok(())
}

// ── Diagnostic context gathering ─────────────────────────────────────────────

/// Tail of the supervisor's own tick-log lines (shared with the operator's
/// monitoring — see `supervisor::LOG_PATH`'s doc), last `n` lines. Best
/// effort: an unreadable/missing log file yields an empty context, never an
/// error (diagnostic context is a nice-to-have for the prompt, not a
/// precondition).
fn read_log_tail(path: &str, n: usize) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].iter().map(|s| s.to_string()).collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Recent `code_profile_runs` rows for `combo`'s (model, backend) — up to
/// `limit`, most recent first. Best effort: a query failure yields an empty
/// `Vec` (logged, never propagated) so a DB hiccup never blocks the
/// diagnostic loop from running.
async fn recent_rows_for_combo(pool: &PgPool, combo: &ComboKey, limit: i64) -> Vec<String> {
    let rows = sqlx::query_as::<_, (Option<String>, Option<String>, Option<bool>, Option<String>)>(
        "SELECT r.case_id, r.error, r.oom, r.mem_config \
         FROM code_profile_runs r JOIN model_profiles p ON r.profile_id = p.id \
         WHERE p.model_name = $1 AND r.backend_tag = $2 \
         ORDER BY r.created_at DESC LIMIT $3",
    )
    .bind(&combo.model)
    .bind(&combo.backend)
    .bind(limit)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|(case_id, error, oom, mem_config)| {
                format!(
                    "case_id={} error={} oom={} mem_config={}",
                    case_id.as_deref().unwrap_or("?"),
                    error.as_deref().unwrap_or("-"),
                    oom.unwrap_or(false),
                    mem_config.as_deref().unwrap_or("NULL"),
                )
            })
            .collect(),
        Err(e) => {
            tracing::warn!("breakfix: recent_rows_for_combo query failed: {e}");
            Vec::new()
        }
    }
}

async fn gather_context(pool: &PgPool, combo: &ComboKey) -> String {
    let log_tail = read_log_tail(supervisor::LOG_PATH, 20).join("\n");
    let rows = recent_rows_for_combo(pool, combo, 5).await;
    format!(
        "Recent supervisor tick log (last 20 lines):\n{}\n\n\
         Recent code_profile_runs rows for this combo (up to 5, most recent first):\n{}",
        if log_tail.is_empty() { "(none available)".to_string() } else { log_tail },
        if rows.is_empty() { "(none available)".to_string() } else { rows.join("\n") },
    )
}

/// Append one line to the SAME shared log file the supervisor writes to
/// (`supervisor::LOG_PATH`), with a `BREAKFIX` token so it is trivially
/// greppable/distinguishable from tick and `ESCALATION` lines by the
/// operator's `mint gaps`-style review (a query CLI is a nice-to-have, not
/// required this phase — see the Phase 4 spec). Best-effort, same as
/// `LiveEnv::log_line`: a logging failure must never crash the handler.
fn log_breakfix(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(supervisor::LOG_PATH)
    {
        let _ = writeln!(f, "{line}");
    }
    tracing::info!("{line}");
}

// ── The handler itself ───────────────────────────────────────────────────────

/// The Phase-4 [`BreakfixHandler`] implementation. Constructed once at daemon
/// startup ([`SubagentBreakfix::new`]) with the real reasoning-backend chain
/// and a live Postgres pool.
pub struct SubagentBreakfix {
    backend: Arc<dyn ReasoningBackend>,
    pool: PgPool,
}

impl SubagentBreakfix {
    pub fn new(pool: PgPool) -> Self {
        let primary: Box<dyn ReasoningBackend> = Box::new(ClaudeCliBackend::from_env());
        let fallback: Box<dyn ReasoningBackend> = Box::new(OllamaCpuBackend::from_env());
        SubagentBreakfix {
            backend: Arc::new(ChainedBackend::new(primary, fallback)),
            pool,
        }
    }

    /// The actual async work. Split out from the trait method (which must be
    /// synchronous — see [`BreakfixHandler::handle_repeat_stuck`]'s
    /// signature) so it can be driven via `block_in_place` +
    /// `Handle::current().block_on(...)` there, and unit-tested directly here
    /// (with a mock backend/pool-free path) without needing that bridge.
    async fn handle_async(&self, combo: &ComboKey, recovery_count: usize) -> BreakfixOutcome {
        let context = gather_context(&self.pool, combo).await;
        let decision = decide_breakfix(
            combo,
            recovery_count,
            self.backend.as_ref(),
            &LiveRetestHook,
            &context,
        )
        .await;

        match decision {
            Decision::Resolved => {
                log_breakfix(&format!(
                    "BREAKFIX combo={} RESOLVED via retry (single-case retest succeeded)",
                    combo.label()
                ));
            }
            Decision::Drop {
                reason,
                attempts,
                last_error_class,
            } => {
                if let Err(e) = write_dropped_config(&self.pool, combo, attempts, &reason, &last_error_class).await {
                    tracing::warn!(
                        "breakfix: failed to write mint_dropped_configs for {}: {e}",
                        combo.label()
                    );
                }
                log_breakfix(&format!(
                    "BREAKFIX combo={} DROP reason={reason:?} attempts={attempts} last_error_class={last_error_class:?}",
                    combo.label()
                ));
            }
            Decision::Escalate { reason, attempts } => {
                log_breakfix(&format!(
                    "BREAKFIX combo={} ESCALATE reason={reason:?} attempts={attempts}",
                    combo.label()
                ));
            }
            Decision::CodefixDeferred { detail, attempts } => {
                // DELIBERATE SCOPE NARROWING (Phase 4 spec): full autonomous
                // code-fix-and-deploy (worktree/test/dual-review/merge/deploy)
                // is NOT implemented here. TODO(mint-phase-5-or-later): once
                // the diagnostic/retry/drop path above is proven in
                // production, a follow-up phase can wire a `codefix` verdict
                // to real automation. Until then, every `codefix` verdict is
                // logged clearly (this line) and then falls back to the exact
                // same escalate behavior as a genuine `escalate` verdict.
                log_breakfix(&format!(
                    "BREAKFIX combo={} CODEFIX-REQUESTED-BUT-NOT-EXECUTED detail={detail:?} attempts={attempts} \
                     (auto code-fix-and-deploy is out of scope for MINT Phase 4 — deferred to a follow-up \
                     phase; falling back to escalate)",
                    combo.label()
                ));
                log_breakfix(&format!(
                    "BREAKFIX combo={} ESCALATE reason=\"codefix deferred, see CODEFIX-REQUESTED line above\" attempts={attempts}",
                    combo.label()
                ));
            }
        }
        // Always `Handled`: this handler always takes SOME action (reasons,
        // logs, and either resolves/drops/escalates) for every repeat-stuck
        // call — never a silent no-op. The supervisor loop restart-recovers
        // regardless, per `BreakfixOutcome::Handled`'s own doc (safe backstop).
        BreakfixOutcome::Handled
    }
}

impl BreakfixHandler for SubagentBreakfix {
    fn handle_repeat_stuck(&self, combo: &ComboKey, recovery_count: usize) -> BreakfixOutcome {
        // `tick()` (the only production caller) runs under
        // `#[tokio::main(flavor = "multi_thread")]` (see `bin/mint.rs`), so
        // `block_in_place` + `Handle::current().block_on(...)` is the correct
        // bridge from this REQUIRED-synchronous trait method into the async
        // reasoning-backend/retest/DB work above — it moves other tasks off
        // this worker thread for the duration, rather than blocking the
        // whole runtime. This panics if called outside a multi-thread tokio
        // runtime context (documented Tokio behavior); every test that
        // exercises this exact method uses `#[tokio::test(flavor =
        // "multi_thread", ...)]` for that reason.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.handle_async(combo, recovery_count))
        })
    }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- VERDICT-line parsing: all 4 variants + malformed/missing ----

    #[test]
    fn parses_retry_verdict() {
        let v = parse_verdict("some reasoning text\nVERDICT: retry(config=mem_config:carveout)");
        assert_eq!(
            v,
            Verdict::Retry {
                config: "mem_config:carveout".to_string()
            }
        );
    }

    #[test]
    fn parses_retry_verdict_multi_key_config() {
        let v = parse_verdict("VERDICT: retry(config=backend:cpu,mem_config:dynamic_gtt)");
        assert_eq!(
            v,
            Verdict::Retry {
                config: "backend:cpu,mem_config:dynamic_gtt".to_string()
            }
        );
    }

    #[test]
    fn parses_drop_verdict() {
        let v = parse_verdict("VERDICT: drop(reason=repeated OOM across configs)");
        assert_eq!(
            v,
            Verdict::Drop {
                reason: "repeated OOM across configs".to_string()
            }
        );
    }

    #[test]
    fn parses_codefix_verdict() {
        let v = parse_verdict("VERDICT: codefix(context clamp needed in infer.rs)");
        assert_eq!(
            v,
            Verdict::Codefix {
                detail: "context clamp needed in infer.rs".to_string()
            }
        );
    }

    #[test]
    fn parses_escalate_verdict() {
        let v = parse_verdict("VERDICT: escalate(reason=needs operator judgment)");
        assert_eq!(
            v,
            Verdict::Escalate {
                reason: "needs operator judgment".to_string()
            }
        );
    }

    #[test]
    fn missing_verdict_line_escalates() {
        let v = parse_verdict("I looked into it but have no conclusion.");
        match v {
            Verdict::Escalate { reason } => assert!(reason.contains("unparseable")),
            other => panic!("expected escalate fallback, got {other:?}"),
        }
    }

    #[test]
    fn malformed_verdict_line_escalates() {
        let v = parse_verdict("VERDICT: maybe-retry-this-thing");
        match v {
            Verdict::Escalate { reason } => assert!(reason.contains("unparseable")),
            other => panic!("expected escalate fallback, got {other:?}"),
        }
    }

    #[test]
    fn empty_response_escalates() {
        match parse_verdict("") {
            Verdict::Escalate { .. } => {}
            other => panic!("expected escalate fallback, got {other:?}"),
        }
    }

    #[test]
    fn verdict_line_must_start_the_line_not_be_prose_scanned() {
        // A "VERDICT:" that only appears mid-sentence (not as the line's own
        // leading token, after trim) is NOT recognized as the contract
        // line — the parser never prose-scans for the substring anywhere in
        // the text, only a line whose trimmed form STARTS WITH "VERDICT:".
        let v = parse_verdict("First thought: VERDICT: retry(config=backend:cpu)");
        match v {
            Verdict::Escalate { reason } => assert!(reason.contains("unparseable")),
            other => panic!("expected escalate fallback (no matching line), got {other:?}"),
        }
    }

    #[test]
    fn first_matching_verdict_line_wins_when_multiple_present() {
        // If a (malformed) response contains more than one genuine VERDICT
        // line, parsing is deterministic: the FIRST one found wins, never a
        // "best" or "last" one.
        let v = parse_verdict("VERDICT: retry(config=backend:cpu)\nVERDICT: drop(reason=x)");
        assert_eq!(
            v,
            Verdict::Retry {
                config: "backend:cpu".to_string()
            }
        );
    }

    // ---- Env sanitization ----

    #[test]
    fn sanitized_env_never_includes_secret_shaped_keys() {
        std::env::set_var("SOME_API_TOKEN", "leaked-if-present");
        std::env::set_var("INFISICAL_CLIENT_SECRET", "leaked-if-present");
        std::env::set_var("CHORD_JWT_SIGNING_KEY", "leaked-if-present");
        std::env::set_var("MY_SUPER_SECRET", "leaked-if-present");
        let env = sanitized_child_env();
        for (k, _) in &env {
            assert!(!is_secret_like_env_key(k), "leaked secret-shaped key: {k}");
        }
        assert!(!env.iter().any(|(k, _)| k == "SOME_API_TOKEN"));
        assert!(!env.iter().any(|(k, _)| k == "INFISICAL_CLIENT_SECRET"));
        assert!(!env.iter().any(|(k, _)| k == "CHORD_JWT_SIGNING_KEY"));
        assert!(!env.iter().any(|(k, _)| k == "MY_SUPER_SECRET"));
        std::env::remove_var("SOME_API_TOKEN");
        std::env::remove_var("INFISICAL_CLIENT_SECRET");
        std::env::remove_var("CHORD_JWT_SIGNING_KEY");
        std::env::remove_var("MY_SUPER_SECRET");
    }

    #[test]
    fn sanitized_env_only_contains_allowlisted_keys() {
        std::env::set_var("SOME_RANDOM_HARMLESS_VAR", "value");
        let env = sanitized_child_env();
        assert!(env.iter().all(|(k, _)| ENV_ALLOWLIST.contains(&k.as_str())));
        assert!(!env.iter().any(|(k, _)| k == "SOME_RANDOM_HARMLESS_VAR"));
        std::env::remove_var("SOME_RANDOM_HARMLESS_VAR");
    }

    #[test]
    fn is_secret_like_matches_the_spec_patterns() {
        assert!(is_secret_like_env_key("FOO_TOKEN"));
        assert!(is_secret_like_env_key("FOO_SECRET"));
        assert!(is_secret_like_env_key("FOO_KEY"));
        assert!(is_secret_like_env_key("INFISICAL_ANYTHING"));
        assert!(is_secret_like_env_key("CHORD_JWT_X"));
        assert!(!is_secret_like_env_key("PATH"));
        assert!(!is_secret_like_env_key("HOME"));
    }

    // ---- Attempt-budget enforcement (adversarial mock) ----

    struct AlwaysRetryBackend {
        calls: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl ReasoningBackend for AlwaysRetryBackend {
        async fn ask(&self, _prompt: &str) -> BackendReply {
            *self.calls.lock().unwrap() += 1;
            BackendReply::Text("VERDICT: retry(config=mem_config:carveout)".to_string())
        }
    }

    struct AlwaysFailRetest {
        calls: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl RetestHook for AlwaysFailRetest {
        async fn retest(&self, _combo: &ComboKey, _alt_config: &str) -> RetestResult {
            *self.calls.lock().unwrap() += 1;
            RetestResult::Failure {
                error_class: "adversarial_always_fails".to_string(),
            }
        }
    }

    fn test_combo() -> ComboKey {
        ComboKey {
            model: "qwen3-coder:30b".to_string(),
            backend: "gpu".to_string(),
            mem_config: Some("dynamic_gtt".to_string()),
        }
    }

    #[tokio::test]
    async fn adversarial_always_retry_backend_terminates_at_budget() {
        let backend = AlwaysRetryBackend { calls: Mutex::new(0) };
        let retest = AlwaysFailRetest { calls: Mutex::new(0) };
        let combo = test_combo();

        let decision = decide_breakfix(&combo, 3, &backend, &retest, "ctx").await;

        assert_eq!(*backend.calls.lock().unwrap(), MAX_ATTEMPTS as u32);
        assert_eq!(*retest.calls.lock().unwrap(), MAX_ATTEMPTS as u32);
        match decision {
            Decision::Drop { attempts, .. } => assert_eq!(attempts, MAX_ATTEMPTS),
            other => panic!("expected forced Drop at budget exhaustion, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_that_succeeds_before_budget_resolves_immediately() {
        struct SucceedOnThirdRetest {
            calls: Mutex<u32>,
        }
        #[async_trait::async_trait]
        impl RetestHook for SucceedOnThirdRetest {
            async fn retest(&self, _combo: &ComboKey, _alt: &str) -> RetestResult {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                if *c >= 3 {
                    RetestResult::Success
                } else {
                    RetestResult::Failure {
                        error_class: "still_failing".to_string(),
                    }
                }
            }
        }
        let backend = AlwaysRetryBackend { calls: Mutex::new(0) };
        let retest = SucceedOnThirdRetest { calls: Mutex::new(0) };
        let combo = test_combo();

        let decision = decide_breakfix(&combo, 3, &backend, &retest, "ctx").await;

        assert_eq!(*retest.calls.lock().unwrap(), 3, "should stop retrying once resolved");
        assert_eq!(*backend.calls.lock().unwrap(), 3);
        assert_eq!(decision, Decision::Resolved);
    }

    #[tokio::test]
    async fn drop_verdict_resolves_immediately_without_exhausting_budget() {
        struct DropBackend;
        #[async_trait::async_trait]
        impl ReasoningBackend for DropBackend {
            async fn ask(&self, _prompt: &str) -> BackendReply {
                BackendReply::Text("VERDICT: drop(reason=known incompatible)".to_string())
            }
        }
        struct NeverCalledRetest;
        #[async_trait::async_trait]
        impl RetestHook for NeverCalledRetest {
            async fn retest(&self, _combo: &ComboKey, _alt: &str) -> RetestResult {
                panic!("retest must not be called for a drop verdict");
            }
        }
        let combo = test_combo();
        let decision = decide_breakfix(&combo, 3, &DropBackend, &NeverCalledRetest, "ctx").await;
        match decision {
            Decision::Drop { attempts, reason, .. } => {
                assert_eq!(attempts, 1);
                assert_eq!(reason, "known incompatible");
            }
            other => panic!("expected immediate Drop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn escalate_verdict_resolves_immediately() {
        struct EscalateBackend;
        #[async_trait::async_trait]
        impl ReasoningBackend for EscalateBackend {
            async fn ask(&self, _prompt: &str) -> BackendReply {
                BackendReply::Text("VERDICT: escalate(reason=needs human judgment)".to_string())
            }
        }
        struct NeverCalledRetest;
        #[async_trait::async_trait]
        impl RetestHook for NeverCalledRetest {
            async fn retest(&self, _combo: &ComboKey, _alt: &str) -> RetestResult {
                panic!("retest must not be called for an escalate verdict");
            }
        }
        let combo = test_combo();
        let decision = decide_breakfix(&combo, 3, &EscalateBackend, &NeverCalledRetest, "ctx").await;
        assert_eq!(
            decision,
            Decision::Escalate {
                reason: "needs human judgment".to_string(),
                attempts: 1
            }
        );
    }

    #[tokio::test]
    async fn codefix_verdict_defers_without_exhausting_budget() {
        struct CodefixBackend;
        #[async_trait::async_trait]
        impl ReasoningBackend for CodefixBackend {
            async fn ask(&self, _prompt: &str) -> BackendReply {
                BackendReply::Text("VERDICT: codefix(patch the context clamp in infer.rs)".to_string())
            }
        }
        struct NeverCalledRetest;
        #[async_trait::async_trait]
        impl RetestHook for NeverCalledRetest {
            async fn retest(&self, _combo: &ComboKey, _alt: &str) -> RetestResult {
                panic!("retest must not be called for a codefix verdict");
            }
        }
        let combo = test_combo();
        let decision = decide_breakfix(&combo, 3, &CodefixBackend, &NeverCalledRetest, "ctx").await;
        match decision {
            Decision::CodefixDeferred { attempts, detail } => {
                assert_eq!(attempts, 1);
                assert!(detail.contains("context clamp"));
            }
            other => panic!("expected CodefixDeferred, got {other:?}"),
        }
    }

    // ---- Reasoning-backend fallback chain (mock, no real subprocess/network) ----

    struct UnavailableBackend {
        msg: &'static str,
    }
    #[async_trait::async_trait]
    impl ReasoningBackend for UnavailableBackend {
        async fn ask(&self, _prompt: &str) -> BackendReply {
            BackendReply::Unavailable(self.msg.to_string())
        }
    }

    struct TextBackend {
        text: &'static str,
    }
    #[async_trait::async_trait]
    impl ReasoningBackend for TextBackend {
        async fn ask(&self, _prompt: &str) -> BackendReply {
            BackendReply::Text(self.text.to_string())
        }
    }

    #[tokio::test]
    async fn chained_backend_falls_back_when_primary_unavailable() {
        let chain = ChainedBackend::new(
            Box::new(UnavailableBackend {
                msg: "binary missing",
            }),
            Box::new(TextBackend {
                text: "VERDICT: escalate(reason=fallback answered)",
            }),
        );
        match chain.ask("prompt").await {
            BackendReply::Text(t) => assert!(t.contains("fallback answered")),
            other => panic!("expected fallback text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chained_backend_uses_primary_when_available() {
        let chain = ChainedBackend::new(
            Box::new(TextBackend {
                text: "VERDICT: escalate(reason=primary answered)",
            }),
            Box::new(UnavailableBackend { msg: "should not be called" }),
        );
        match chain.ask("prompt").await {
            BackendReply::Text(t) => assert!(t.contains("primary answered")),
            other => panic!("expected primary text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chained_backend_escalates_when_both_unavailable() {
        let chain = ChainedBackend::new(
            Box::new(UnavailableBackend { msg: "primary down" }),
            Box::new(UnavailableBackend { msg: "fallback down too" }),
        );
        struct NeverCalledRetest;
        #[async_trait::async_trait]
        impl RetestHook for NeverCalledRetest {
            async fn retest(&self, _combo: &ComboKey, _alt: &str) -> RetestResult {
                panic!("retest must not be called when the backend is unavailable");
            }
        }
        let combo = test_combo();
        let decision = decide_breakfix(&combo, 3, &chain, &NeverCalledRetest, "ctx").await;
        match decision {
            Decision::Escalate { reason, attempts } => {
                assert!(reason.contains("fallback down too"));
                assert_eq!(attempts, 1);
            }
            other => panic!("expected escalate on double-unavailable, got {other:?}"),
        }
    }

    // ---- mint_dropped_configs write path (SQL-shape unit test — no live DB) ----

    #[test]
    fn dropped_config_insert_sql_is_fully_parameterized() {
        // No string-interpolated values — every value is a bind parameter.
        assert!(INSERT_DROPPED_CONFIG_SQL.contains("VALUES ($1, $2, $3, $4, $5, $6)"));
        assert!(INSERT_DROPPED_CONFIG_SQL.contains("model, backend, mem_config, reason, attempts_made, last_error_class"));
        // Never a literal quote-embedding pattern like `'{}'`.
        assert!(!INSERT_DROPPED_CONFIG_SQL.contains("{}"));
    }

    #[test]
    fn alt_config_parses_known_keys_and_ignores_unknown() {
        let c = parse_alt_config("backend:cpu,mem_config:carveout,bogus:xyz");
        assert_eq!(c.backend.as_deref(), Some("cpu"));
        assert_eq!(c.mem_config.as_deref(), Some("carveout"));
    }

    #[test]
    fn alt_config_empty_string_yields_all_none() {
        let c = parse_alt_config("");
        assert_eq!(c, AltConfig::default());
    }

    // ---- extract_kv / strip_call edge cases ----

    #[test]
    fn extract_kv_takes_everything_after_key_equals() {
        // `key=` must be the leading token; the value runs to the end of the
        // string (commas in a prose `reason=...` are part of the value, not a
        // field separator — see `extract_kv`'s doc for why).
        assert_eq!(
            extract_kv("reason=x, attempts=2", "reason"),
            Some("x, attempts=2".to_string())
        );
        assert_eq!(extract_kv("attempts=2", "reason"), None);
    }

    #[test]
    fn strip_call_requires_matching_parens() {
        assert_eq!(strip_call("retry(config=x)", "retry"), Some("config=x"));
        assert_eq!(strip_call("retry(config=x", "retry"), None);
        assert_eq!(strip_call("dropx(reason=y)", "drop"), None);
    }
}
