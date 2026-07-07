//! Context stress test suite (S83 MINT-01).
//!
//! Builds a graduated context prompt out of *real* embedded repo files, plants
//! three recall facts at 25/50/75% depth, appends a recall query, runs the
//! model through Ollama, and measures throughput / TTFT / recall / memory.
//!
//! ## Filler corpus
//! The tool runs on the sweep-harness host where there is no repo checkout, so a representative
//! proportional sample of real repo files is embedded in the binary via
//! `include_str!` (~60% Rust, 20% Markdown, 10% TOML, 10% JSON). The embedded
//! content is concatenated and repeated to reach each target token count
//! (token estimate ≈ chars / 4).
//!
//! ## Pure vs. live
//! Everything in this file except `run_tier` (which performs the Ollama HTTP
//! call) is pure and unit-tested: corpus proportions, token-target sizing,
//! planted-fact insertion at correct depths, and recall scoring.

use std::error::Error as _;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// How long Ollama keeps a model resident after a request completes, set
/// explicitly on every `/api/generate` and `/api/chat` call in this module.
///
/// HFIX-03: `runner.rs`'s warm-up call sets a generous keep_alive before a
/// model's suite starts, but every actual inference request that followed
/// (through this module) omitted the field — and each request's keep_alive
/// (or its absence, which falls back to Ollama's 5-minute server default)
/// determines the model's *new* expiry once that request finishes. So the
/// very first real inference call silently downgraded the session from the
/// warm-up's generous window back down to 5 minutes. For a large model
/// (dynamic GTT pool; cold reloads can run well past a minute) a single
/// slow generate call can itself take close to 5 minutes, evicting the
/// model right before the next case's request arrives — forcing another
/// cold reload, which is itself slow enough to repeat the cycle. This
/// showed up as near-total per-case failure (timeouts, and historically
/// some "model not found" 404s from a stale/racing unload) for the fleet's
/// larger models, while small/fast-loading models were mostly unaffected.
/// Matches `runner.rs`'s warm-up value; 30 minutes comfortably outlasts a
/// single model's whole case suite, and `runner.rs` explicitly evicts
/// (`keep_alive: 0`) once that suite is done, so it never strands
/// residency into the next model's run.
pub(crate) const OLLAMA_KEEP_ALIVE: &str = "30m";

// ---------------------------------------------------------------------------
// Embedded filler corpus (real repo files)
// ---------------------------------------------------------------------------

// ~60% Rust
const F_RS_ERROR: &str = include_str!("../error.rs");
const F_RS_TOOL: &str = include_str!("../tool.rs");
const F_RS_REGISTRY: &str = include_str!("../registry.rs");
const F_RS_REMINDER: &str = include_str!("../reminder/mod.rs");
// ~20% Markdown.
// The stress corpus is a representative byte sample of real repo files; the
// specific content is irrelevant to behavior (it only supplies graduated token
// volume in the right language proportions). When terminus-rs was extracted
// into a standalone crate these were vendored byte-for-byte into
// `src/intake/corpus/` so the crate is self-contained and the corpus
// proportions are preserved exactly.
const F_MD_README: &str = include_str!("corpus/lumina-README.md");
const F_MD_ARCH: &str = include_str!("corpus/architecture.md");
// ~10% TOML
const F_TOML_WS: &str = include_str!("corpus/workspace-Cargo.toml");
const F_TOML_TERMINUS: &str = include_str!("../../Cargo.toml");
// ~10% JSON
const F_JSON_PROBE: &str = include_str!("corpus/conv-capacity-probe-chord.json");

/// One embedded filler file with its language class and content.
struct FillerFile {
    lang: FillerLang,
    body: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillerLang {
    Rust,
    Markdown,
    Toml,
    Json,
}

/// The embedded corpus, ordered to interleave by language. Weighting toward
/// Rust is achieved by the byte volume of the Rust files (registry + reminder
/// are large) — see `corpus_proportions` for the measured split.
fn corpus() -> Vec<FillerFile> {
    vec![
        FillerFile { lang: FillerLang::Rust, body: F_RS_ERROR },
        FillerFile { lang: FillerLang::Rust, body: F_RS_TOOL },
        FillerFile { lang: FillerLang::Rust, body: F_RS_REGISTRY },
        FillerFile { lang: FillerLang::Rust, body: F_RS_REMINDER },
        FillerFile { lang: FillerLang::Markdown, body: F_MD_README },
        FillerFile { lang: FillerLang::Markdown, body: F_MD_ARCH },
        FillerFile { lang: FillerLang::Toml, body: F_TOML_WS },
        FillerFile { lang: FillerLang::Toml, body: F_TOML_TERMINUS },
        FillerFile { lang: FillerLang::Json, body: F_JSON_PROBE },
    ]
}

/// Measured byte proportion of each language class in the embedded corpus.
/// Returned as fractions summing to ~1.0. Used by tests to assert the corpus is
/// roughly 60/20/10/10.
pub fn corpus_proportions() -> [(FillerLang, f64); 4] {
    let files = corpus();
    let total: usize = files.iter().map(|f| f.body.len()).sum();
    let total = total.max(1) as f64;
    let mut rs = 0usize;
    let mut md = 0usize;
    let mut toml = 0usize;
    let mut json = 0usize;
    for f in &files {
        match f.lang {
            FillerLang::Rust => rs += f.body.len(),
            FillerLang::Markdown => md += f.body.len(),
            FillerLang::Toml => toml += f.body.len(),
            FillerLang::Json => json += f.body.len(),
        }
    }
    [
        (FillerLang::Rust, rs as f64 / total),
        (FillerLang::Markdown, md as f64 / total),
        (FillerLang::Toml, toml as f64 / total),
        (FillerLang::Json, json as f64 / total),
    ]
}

// ---------------------------------------------------------------------------
// Token estimation + filler sizing
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 chars per token. Cheap, provider-agnostic, good
/// enough for graduated stress tiers (we are not billing on it).
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Build a filler string of approximately `target_tokens` tokens by
/// concatenating and repeating the embedded corpus in interleaved order.
///
/// Stops as soon as the running token estimate reaches the target, so the
/// result is at least `target_tokens` (never short — a short prompt would
/// under-stress the model). Empty corpus yields an empty string.
pub fn build_filler(target_tokens: usize) -> String {
    let target_chars = target_tokens.saturating_mul(4);
    let files = corpus();
    if files.is_empty() || target_chars == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(target_chars + 256);
    let mut i = 0usize;
    while out.len() < target_chars {
        let f = &files[i % files.len()];
        out.push_str("\n\n// ---- file ----\n");
        out.push_str(f.body);
        i += 1;
        // Safety: even a tiny corpus repeated will reach target; but guard
        // against a pathological empty body loop.
        if f.body.is_empty() && i > files.len() {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Planted facts
// ---------------------------------------------------------------------------

/// The three planted facts (CONV-01 technique). Each is a sentence inserted at
/// a target depth in the filler.
pub const FACT_A: &str = "The capital of Freedonia is Zubrovka.";
pub const FACT_B: &str = "Project Falcon launched on March 7, 2019.";
pub const FACT_C: &str = "The maximum safe temperature for the reactor is 847 degrees.";

/// The recall query appended after the filler.
pub const RECALL_QUERY: &str = "Based ONLY on the document above, answer all three questions: \
1) What is the capital of Freedonia? \
2) When did Project Falcon launch? \
3) What is the maximum safe temperature for the reactor?";

/// Insert the three planted facts into `filler` at 25%, 50%, 75% depth (by
/// character offset, snapped to the next newline so a fact is never spliced
/// mid-line) and append the recall query at the end. Returns the full prompt.
pub fn plant_facts(filler: &str) -> String {
    let facts = [(0.25, FACT_A), (0.50, FACT_B), (0.75, FACT_C)];
    let len = filler.len();

    // Compute snapped insertion byte offsets, ascending.
    let mut points: Vec<(usize, &str)> = facts
        .iter()
        .map(|(depth, fact)| {
            let raw = (len as f64 * depth) as usize;
            (snap_to_newline(filler, raw), *fact)
        })
        .collect();
    points.sort_by_key(|(off, _)| *off);

    let mut out = String::with_capacity(len + 512);
    let mut cursor = 0usize;
    for (off, fact) in points {
        let off = off.min(len);
        out.push_str(&filler[cursor..off]);
        out.push_str("\n\nNOTE: ");
        out.push_str(fact);
        out.push('\n');
        cursor = off;
    }
    out.push_str(&filler[cursor..]);
    out.push_str("\n\n");
    out.push_str(RECALL_QUERY);
    out
}

/// Snap a byte offset to the start of the next line (or end of string) so we
/// never split a UTF-8 char or a source line.
fn snap_to_newline(s: &str, mut off: usize) -> usize {
    if off >= s.len() {
        return s.len();
    }
    // Advance to a char boundary first.
    while off < s.len() && !s.is_char_boundary(off) {
        off += 1;
    }
    match s[off..].find('\n') {
        Some(rel) => (off + rel + 1).min(s.len()),
        None => s.len(),
    }
}

// ---------------------------------------------------------------------------
// Recall scoring
// ---------------------------------------------------------------------------

/// Score planted-fact recall 0-3 by checking the response for the key tokens of
/// each fact. Case-insensitive substring match.
/// - Fact A → "Zubrovka"
/// - Fact B → "March 7, 2019" OR "2019"
/// - Fact C → "847"
pub fn score_recall(response: &str) -> i32 {
    let lc = response.to_lowercase();
    let mut score = 0;
    if lc.contains("zubrovka") {
        score += 1;
    }
    if lc.contains("march 7, 2019") || lc.contains("march 7 2019") || lc.contains("2019") {
        score += 1;
    }
    if lc.contains("847") {
        score += 1;
    }
    score
}

// ---------------------------------------------------------------------------
// Per-tier inference (live)
// ---------------------------------------------------------------------------

/// Resolve the Ollama base URL — same fallback chain other modules use:
/// `OLLAMA_URL` → `OLLAMA_BASE_URL` → `OLLAMA_CPU_URL` → default.
pub fn ollama_base() -> String {
    for k in ["OLLAMA_URL", "OLLAMA_BASE_URL", "OLLAMA_CPU_URL"] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().trim_end_matches('/');
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "http://127.0.0.1:11434".to_string() // pii-test-fixture
}

/// Measured result of one context tier.
#[derive(Debug, Clone)]
pub struct TierResult {
    pub context_tokens: usize,
    pub throughput_tok_per_sec: Option<f64>,
    pub ttft_ms: Option<i32>,
    pub total_time_ms: Option<i32>,
    pub recall_score: Option<i32>,
    pub coherence_score: Option<f64>,
    pub memory_usage_mb: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
    /// Raw model response (kept for an optional coherence judge; not stored).
    pub response: String,
}

#[derive(Deserialize)]
struct GenResponse {
    #[serde(default)]
    response: String,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    eval_duration: Option<u64>, // nanoseconds
    #[serde(default)]
    prompt_eval_duration: Option<u64>, // nanoseconds (prefill ≈ TTFT)
    #[serde(default)]
    error: Option<String>,
}

/// Heuristic OOM / overload classifier from an error/status. Used so the runner
/// can stop escalating tiers without crashing.
pub fn is_oom_like(msg: &str, status: Option<u16>) -> bool {
    if matches!(status, Some(500) | Some(503)) {
        return true;
    }
    let lc = msg.to_lowercase();
    lc.contains("out of memory")
        || lc.contains("oom")
        || lc.contains("killed")
        || lc.contains("cuda")
        || lc.contains("insufficient memory")
        || lc.contains("failed to allocate")
}

/// Whether an inference error is a transport/connection failure worth one
/// retry (vs. a deterministic model/server rejection). Pure. Originally
/// private to `code_v2.rs`'s retry loop; promoted here (Phase 2 item 4) as the
/// `Transport` half of [`ErrorClass`], alongside [`is_oom_like`]'s `Oom` half,
/// so both live next to each other as the two heuristic error predicates the
/// intake suites share.
pub fn is_transport_error(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("error sending request")
        || l.contains("connection")
        || l.contains("timed out")
        || l.contains("timeout")
        || l.contains("broken pipe")
        || l.contains("reset by peer")
        || l.contains("eof")
}

/// Coarse classification of an inference error, unifying the two ad hoc
/// predicates ([`is_oom_like`], [`is_transport_error`]) into one public
/// vocabulary (Phase 2 item 4). Named `ErrorClass` — not `RetryReason` or
/// similar — because its use isn't limited to "should I retry?": it is also
/// meant as a stable trigger condition a future automated "breakfix"
/// subagent can match on (e.g. "escalate `Oom` classifications to a
/// GPU-authority alert", "silently retry `Transport`", "surface `Other` for
/// human review").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Out-of-memory / overload — the model process was killed, or the host
    /// rejected the request outright ([`is_oom_like`]'s conditions).
    Oom,
    /// A transient connection failure worth one retry, not a deterministic
    /// model/server rejection ([`is_transport_error`]'s conditions).
    Transport,
    /// Deterministic failure (bad prompt, model not found, validation error,
    /// unrecognized dimension, …) — neither of the above.
    Other,
}

/// Classify an inference error/status into an [`ErrorClass`]. `Oom` is
/// checked first: an error whose text matches BOTH `is_oom_like` and
/// `is_transport_error` (e.g. a message mentioning both "connection" and
/// "killed") classifies as `Oom`, not `Transport` — a message that reads as
/// possibly-OOM should never be silently retried as if it were a plain
/// transient network blip.
pub fn classify_error(msg: &str, status: Option<u16>) -> ErrorClass {
    if is_oom_like(msg, status) {
        ErrorClass::Oom
    } else if is_transport_error(msg) {
        ErrorClass::Transport
    } else {
        ErrorClass::Other
    }
}

/// Failure-class for a `reqwest::Error` on the request path (as opposed to an
/// HTTP-level error status, which is handled separately). reqwest's own
/// `Display` for a `Kind::Request` error never surfaces WHY the request
/// failed — confirmed by reading reqwest 0.12.28's own source
/// (`error.rs` ~line 227-272): it renders only `"error sending request for
/// url (...)"`, with the real cause (timeout vs. connection failure vs.
/// body-read failure) reachable ONLY via `.is_timeout()`/`.is_connect()`/
/// `.is_body()`/`.source()`. Every `Err(e) => { let msg = e.to_string(); ... }`
/// site in this module discarded all of that before this type existed,
/// which is exactly what turned the `qwen2.5-coder:32b-instruct` production
/// stall into a multi-hour blind-diagnosis cycle: the stored error text gave
/// no way to tell "timed out after Xs" from "connection reset" from "TCP
/// connect failed" without live reproduction. This is NOT deliberate S77
/// error-genericization (see `describe_request_error`'s doc) — it's just
/// reqwest's default `Display`, unpreserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFailureClass {
    /// `.is_timeout()` — the request (or the tier/case timeout wrapping it)
    /// elapsed before a response came back.
    Timeout,
    /// `.is_connect()` — the TCP/TLS connect itself failed (refused, reset,
    /// unreachable, DNS, etc.), before any request bytes were exchanged.
    Connect,
    /// `.is_body()` — reqwest's `Kind::Body`: an error streaming the
    /// REQUEST body while sending it. Distinct from a failure reading the
    /// RESPONSE body (that's `Kind::Decode`, surfaced by `.is_decode()`,
    /// not tracked by this enum — see `classify_request_error_response_
    /// read_failure_is_not_misclassified` in this module's tests). In
    /// practice unreachable from this module's calls: every request body
    /// here is a fully-buffered `serde_json::json!` value, never a stream,
    /// so `Kind::Body` cannot occur today — tracked anyway per the fix's
    /// spec (capture `is_body()` alongside the other two) and so it's ready
    /// if a future call site ever streams a request body.
    Body,
    /// None of the above — reqwest didn't classify it as one of its three
    /// named kinds (rare for a plain request-path error, but not impossible).
    Other,
}

impl RequestFailureClass {
    /// Short, generic label safe for an operator-facing error column. Adds
    /// failure-CLASS detail only — no hostnames, ports, or other internals
    /// beyond what reqwest's own message (still included alongside this
    /// label by `describe_request_error`) already carries.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Timeout => "timed out",
            Self::Connect => "connection failed",
            Self::Body => "response read failed",
            Self::Other => "request failed",
        }
    }
}

/// Classify a `reqwest::Error` from the request path into a
/// [`RequestFailureClass`]. Checked in `is_timeout` → `is_connect` →
/// `is_body` order (reqwest guarantees these are mutually exclusive for a
/// `Kind::Request` error, so order does not matter for correctness, but this
/// matches the order they're documented in reqwest's own API).
pub fn classify_request_error(e: &reqwest::Error) -> RequestFailureClass {
    if e.is_timeout() {
        RequestFailureClass::Timeout
    } else if e.is_connect() {
        RequestFailureClass::Connect
    } else if e.is_body() {
        RequestFailureClass::Body
    } else {
        RequestFailureClass::Other
    }
}

/// Build the stored error string for a failed request-path `reqwest::Error`:
/// the existing message (`e.to_string()`, unchanged — so this never leaks
/// anything the pre-existing behavior didn't already leak) plus a `(class:
/// ...)` suffix distinguishing WHY it failed. The full picture — message,
/// all three predicates, and the underlying `.source()` (e.g. the OS-level
/// connect-refused detail, or the inner timer-elapsed error) — is logged via
/// `tracing::warn!`, NEVER returned, so it never lands in an operator-facing
/// report row; an operator debugging a stuck sweep gets it from the logs
/// instead of another multi-hour blind-diagnosis cycle. `context` is a short
/// caller tag (e.g. `"run_tier"`, `"generate_at"`) so the log line identifies
/// which call site failed.
pub fn describe_request_error(context: &str, e: &reqwest::Error) -> String {
    let class = classify_request_error(e);
    let msg = e.to_string();
    tracing::warn!(
        "{context}: inference request failed ({}); is_timeout={} is_connect={} is_body={} msg={msg} cause={:?}",
        class.label(),
        e.is_timeout(),
        e.is_connect(),
        e.is_body(),
        e.source().map(|s| s.to_string()),
    );
    format!("{msg} (class: {})", class.label())
}

/// Run a single context tier against the model via Ollama `/api/generate`
/// (non-streaming — Ollama returns `prompt_eval_duration` and `eval_duration`
/// which give us TTFT and throughput without needing a stream).
///
/// `memory_usage_mb` is filled in by the runner (it queries `/api/ps`); this
/// function leaves it `None`.
pub async fn run_tier(
    client: &reqwest::Client,
    model_name: &str,
    target_tokens: usize,
    timeout: Duration,
) -> TierResult {
    let filler = build_filler(target_tokens);
    let prompt = plant_facts(&filler);
    let actual_tokens = estimate_tokens(&prompt);

    let base = ollama_base();
    let body = serde_json::json!({
        "model": model_name,
        "prompt": prompt,
        "stream": false,
        "keep_alive": OLLAMA_KEEP_ALIVE,
        "options": { "num_ctx": next_pow2_ctx(actual_tokens) }
    });

    let started = Instant::now();
    let resp = client
        .post(format!("{base}/api/generate"))
        .json(&body)
        .timeout(timeout)
        .send()
        .await;

    let mut result = TierResult {
        context_tokens: actual_tokens,
        throughput_tok_per_sec: None,
        ttft_ms: None,
        total_time_ms: None,
        recall_score: None,
        coherence_score: None,
        memory_usage_mb: None,
        oom: false,
        error: None,
        response: String::new(),
    };

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            result.oom = is_oom_like(&e.to_string(), None);
            result.error = Some(describe_request_error("run_tier", &e));
            return result;
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let txt = resp.text().await.unwrap_or_default();
        result.oom = is_oom_like(&txt, Some(code));
        result.error = Some(format!("Ollama HTTP {code}: {txt}"));
        return result;
    }

    let total_ms = started.elapsed().as_millis() as i32;
    let parsed: GenResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            result.error = Some(format!("response parse error: {e}"));
            result.total_time_ms = Some(total_ms);
            return result;
        }
    };

    if let Some(err) = parsed.error {
        result.oom = is_oom_like(&err, None);
        result.error = Some(err);
        result.total_time_ms = Some(total_ms);
        return result;
    }

    result.total_time_ms = Some(total_ms);
    result.recall_score = Some(score_recall(&parsed.response));
    result.response = parsed.response;

    // TTFT ≈ prompt eval (prefill) duration, ns → ms.
    if let Some(ns) = parsed.prompt_eval_duration {
        result.ttft_ms = Some((ns / 1_000_000) as i32);
    }
    // Throughput = completion tokens / generation seconds.
    if let (Some(toks), Some(ns)) = (parsed.eval_count, parsed.eval_duration) {
        let secs = ns as f64 / 1_000_000_000.0;
        if secs > 0.0 {
            result.throughput_tok_per_sec = Some(toks as f64 / secs);
        }
    }

    result
}

/// Measured outcome of a single non-streaming generation. Reused by the code
/// and agent suites (MINT-02/03) so they hit the SAME Ollama inference path the
/// context suite uses.
#[derive(Debug, Clone, Default)]
pub struct GenOutcome {
    pub response: String,
    pub throughput_tok_per_sec: Option<f64>,
    pub total_time_ms: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
}

/// Non-streaming `/api/generate` call returning the response text plus timing.
/// `num_ctx` is sized to the prompt automatically. Never panics — transport and
/// HTTP errors are returned in `error` (with `oom` set when they look OOM-like).
pub async fn generate(
    client: &reqwest::Client,
    model_name: &str,
    prompt: &str,
    timeout: Duration,
) -> GenOutcome {
    // P5: route through the backend-aware path so each model runs on its tagged
    // backend (GPU vs CPU). Untagged models / legacy registries resolve to the
    // default Ollama base, so behavior is unchanged until models are tagged.
    let m = crate::intake::infer::infer_with_metrics(client, model_name, prompt, timeout).await;
    GenOutcome {
        response: m.response,
        throughput_tok_per_sec: m.throughput_tok_per_sec,
        total_time_ms: m.total_time_ms,
        oom: m.oom,
        error: m.error,
    }
}

/// Like [`generate`] but against an explicit backend base URL — the Ollama HTTP
/// root, e.g. `http://localhost:11435` (P5 backend-aware routing). `generate` // pii-test-fixture
/// is the convenience wrapper that targets the default `ollama_base()`.
pub async fn generate_at(
    client: &reqwest::Client,
    base: &str,
    model_name: &str,
    prompt: &str,
    timeout: Duration,
) -> GenOutcome {
    let body = serde_json::json!({
        "model": model_name,
        "prompt": prompt,
        "stream": false,
        "keep_alive": OLLAMA_KEEP_ALIVE,
        "options": { "num_ctx": next_pow2_ctx(estimate_tokens(prompt)) }
    });
    let started = Instant::now();
    let mut out = GenOutcome::default();
    let resp = client
        .post(format!("{base}/api/generate"))
        .json(&body)
        .timeout(timeout)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            out.oom = is_oom_like(&e.to_string(), None);
            out.error = Some(describe_request_error("generate_at", &e));
            return out;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let txt = resp.text().await.unwrap_or_default();
        out.oom = is_oom_like(&txt, Some(code));
        out.error = Some(format!("Ollama HTTP {code}: {txt}"));
        return out;
    }
    let total_ms = started.elapsed().as_millis() as i32;
    let parsed: GenResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            out.error = Some(format!("response parse error: {e}"));
            out.total_time_ms = Some(total_ms);
            return out;
        }
    };
    if let Some(err) = parsed.error {
        out.oom = is_oom_like(&err, None);
        out.error = Some(err);
        out.total_time_ms = Some(total_ms);
        return out;
    }
    out.total_time_ms = Some(total_ms);
    out.response = parsed.response;
    if let (Some(toks), Some(ns)) = (parsed.eval_count, parsed.eval_duration) {
        let secs = ns as f64 / 1_000_000_000.0;
        if secs > 0.0 {
            out.throughput_tok_per_sec = Some(toks as f64 / secs);
        }
    }
    out
}

/// Non-streaming `/api/chat` call with a tool catalog. Returns the assistant
/// message's `tool_calls` (function names + raw argument JSON) and any text
/// content. Used by the agent suite to score tool selection. `tools` is the
/// Ollama tool-spec array. Never panics.
pub async fn chat_with_tools(
    client: &reqwest::Client,
    model_name: &str,
    user_prompt: &str,
    tools: &serde_json::Value,
    timeout: Duration,
) -> ChatOutcome {
    let base = ollama_base();
    let mut body = serde_json::json!({
        "model": model_name,
        "messages": [ { "role": "user", "content": user_prompt } ],
        "stream": false,
        "keep_alive": OLLAMA_KEEP_ALIVE,
    });
    if tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        body["tools"] = tools.clone();
    }
    let started = Instant::now();
    let mut out = ChatOutcome::default();
    let resp = client
        .post(format!("{base}/api/chat"))
        .json(&body)
        .timeout(timeout)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            out.oom = is_oom_like(&e.to_string(), None);
            out.error = Some(describe_request_error("chat_with_tools", &e));
            return out;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let txt = resp.text().await.unwrap_or_default();
        out.oom = is_oom_like(&txt, Some(code));
        out.error = Some(format!("Ollama HTTP {code}: {txt}"));
        return out;
    }
    out.total_time_ms = Some(started.elapsed().as_millis() as i32);
    let val: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            out.error = Some(format!("chat parse error: {e}"));
            return out;
        }
    };
    let msg = &val["message"];
    out.content = msg["content"].as_str().unwrap_or_default().to_string();
    if let Some(calls) = msg["tool_calls"].as_array() {
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or_default().to_string();
            let args = c["function"]["arguments"].clone();
            if !name.is_empty() {
                out.tool_calls.push((name, args));
            }
        }
    }
    out
}

/// Outcome of one `/api/chat` tool-calling turn.
#[derive(Debug, Clone, Default)]
pub struct ChatOutcome {
    /// Assistant text content (may be empty when the model chose a tool).
    pub content: String,
    /// (function_name, arguments_json) for each tool call, in order.
    pub tool_calls: Vec<(String, serde_json::Value)>,
    pub total_time_ms: Option<i32>,
    pub oom: bool,
    pub error: Option<String>,
}

/// Round a token count up to the next power-of-two context window (with a small
/// headroom for the model's own response), clamped to a sane range. Keeps
/// Ollama from over-allocating KV cache while ensuring the prompt fits.
pub fn next_pow2_ctx(prompt_tokens: usize) -> usize {
    let needed = prompt_tokens + 1024; // headroom for generation
    let mut ctx = 2048usize;
    while ctx < needed {
        ctx = ctx.saturating_mul(2);
        if ctx >= 262_144 {
            break;
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- RequestFailureClass / describe_request_error: real reqwest errors ----
    //
    // reqwest::Error has no public constructor, so these inject REAL failures
    // of each kind over loopback TCP rather than mocking the type: a
    // connect-refused port, a listener that accepts but never responds
    // (client-side timeout), and a listener that sends a truncated body (a
    // body-read failure) — the three classes `describe_request_error` exists
    // to distinguish.

    /// Spawn a one-shot TCP helper on an ephemeral loopback port; returns the
    /// port. `handler` runs once, on its own thread, when a connection
    /// arrives.
    fn spawn_tcp_helper<F: FnOnce(std::net::TcpStream) + Send + 'static>(handler: F) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handler(stream);
            }
        });
        port
    }

    #[tokio::test]
    async fn classify_request_error_connect_refused() {
        // Port 1 on loopback is a real "nothing listening" target — an
        // instant TCP connect-refused, no server needed.
        let client = reqwest::Client::new();
        let err = client
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connecting to a closed port must fail");
        assert!(err.is_connect(), "sanity: reqwest must classify this is_connect()");
        assert!(!err.is_timeout());
        assert_eq!(classify_request_error(&err), RequestFailureClass::Connect);
        let desc = describe_request_error("test", &err);
        assert!(desc.contains("connection failed"), "got: {desc}");
        assert!(
            desc.starts_with(&err.to_string()),
            "must preserve the original message verbatim, got: {desc}"
        );
    }

    #[tokio::test]
    async fn classify_request_error_timeout() {
        // A listener that accepts the TCP connection (so this is NOT a
        // connect failure) but never writes a response — the client's short
        // timeout must fire.
        let port = spawn_tcp_helper(|stream| {
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(80))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("a server that never responds must time out");
        assert!(err.is_timeout(), "sanity: reqwest must classify this is_timeout()");
        assert!(!err.is_connect());
        assert_eq!(classify_request_error(&err), RequestFailureClass::Timeout);
        let desc = describe_request_error("test", &err);
        assert!(desc.contains("timed out"), "got: {desc}");
    }

    #[tokio::test]
    async fn classify_request_error_response_read_failure_is_not_misclassified() {
        // A server that promises a Content-Length it doesn't deliver, then
        // closes the connection — headers arrive fine (so `.send()` itself
        // succeeds), but reading the body fails partway through. reqwest
        // surfaces this as `Kind::Decode` (`.is_decode()`), NOT `Kind::Body`
        // (`.is_body()` is reserved for errors streaming the REQUEST body on
        // the way out — unreachable from this codebase's fully-buffered
        // `serde_json::json!` bodies). This test exists to pin that fact so
        // a future reqwest upgrade that changes it doesn't silently make
        // `classify_request_error` miscategorize a real response-read
        // failure as `Other` without anyone noticing: today it correctly
        // falls into `Other` (not `Timeout`/`Connect`/`Body`), which is
        // still a truthful, non-misleading bucket — just not as specific as
        // it could be if a future iteration adds `is_decode()` tracking.
        use std::io::{Read, Write};
        let port = spawn_tcp_helper(|mut stream| {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nshort");
            let _ = stream.flush();
            // Dropping here closes the socket before the promised 1000 bytes
            // arrive, so the client's body read fails.
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("headers arrive fine; only the body read should fail");
        let err = resp
            .bytes()
            .await
            .expect_err("a truncated body (short of Content-Length) must fail to read");
        assert!(err.is_decode(), "sanity: reqwest classifies a truncated response body as is_decode()");
        assert!(!err.is_timeout());
        assert!(!err.is_connect());
        assert!(!err.is_body(), "is_body() is the REQUEST-body-streaming kind, distinct from is_decode()");
        assert_eq!(
            classify_request_error(&err),
            RequestFailureClass::Other,
            "a decode failure isn't Timeout/Connect/Body, so Other is correct (not silently swallowed as one of those)"
        );
    }

    #[test]
    fn request_failure_class_labels_are_distinct() {
        // Each class must be genuinely distinguishable in the stored text —
        // the whole point of this fix — not just internally different enum
        // variants that render to the same string.
        let labels = [
            RequestFailureClass::Timeout.label(),
            RequestFailureClass::Connect.label(),
            RequestFailureClass::Body.label(),
            RequestFailureClass::Other.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "each class must have a distinct label");
    }

    #[test]
    fn ollama_keep_alive_matches_runner_warmup_value() {
        // Guards against a future edit accidentally setting this to "0" or ""
        // (both of which mean "unload immediately" / "use server default" in
        // Ollama's API), which would silently reintroduce the eviction cycle
        // this constant exists to prevent. Also pinned to match runner.rs's
        // warm-up literal ("30m") so the two never drift apart.
        assert_eq!(OLLAMA_KEEP_ALIVE, "30m");
    }

    #[test]
    fn all_three_ollama_request_builders_set_keep_alive() {
        // HFIX-03: run_tier, generate_at, and chat_with_tools each build their
        // own request body (no shared constructor), so nothing at the type
        // level stops one of them from losing the field on a future edit.
        // This test reads the source directly so it fails loudly if that
        // happens, rather than only showing up as a live-fleet regression.
        let src = include_str!("context.rs");
        let call_sites = src.matches("\"keep_alive\": OLLAMA_KEEP_ALIVE").count();
        assert_eq!(
            call_sites, 3,
            "expected run_tier, generate_at, and chat_with_tools to each set keep_alive"
        );
    }

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2); // ceil(5/4)
        assert_eq!(estimate_tokens(&"x".repeat(4000)), 1000);
    }

    #[test]
    fn corpus_proportions_roughly_60_20_10_10() {
        let props = corpus_proportions();
        let get = |l: FillerLang| props.iter().find(|(k, _)| *k == l).unwrap().1;
        let rs = get(FillerLang::Rust);
        let md = get(FillerLang::Markdown);
        let toml = get(FillerLang::Toml);
        let json = get(FillerLang::Json);
        // Sum to ~1.
        let sum = rs + md + toml + json;
        assert!((sum - 1.0).abs() < 0.001, "sum={sum}");
        // Rust dominates (target ~60%, accept 45-75% given real file sizes).
        assert!(rs > 0.45 && rs < 0.75, "rust proportion {rs}");
        // Markdown is the second biggest class.
        assert!(md > 0.10, "md proportion {md}");
        // toml + json are the small classes.
        assert!(toml < md && json < md, "toml={toml} json={json} md={md}");
    }

    #[test]
    fn build_filler_reaches_target() {
        for target in [2000usize, 8000, 16000] {
            let f = build_filler(target);
            let got = estimate_tokens(&f);
            assert!(got >= target, "target={target} got={got}");
            // Not absurdly over (within one corpus-file overshoot).
            assert!(got < target + 30_000, "target={target} got={got}");
        }
    }

    #[test]
    fn build_filler_zero_is_empty() {
        assert!(build_filler(0).is_empty());
    }

    #[test]
    fn plant_facts_inserts_all_three_and_query() {
        let filler = build_filler(2000);
        let prompt = plant_facts(&filler);
        assert!(prompt.contains(FACT_A));
        assert!(prompt.contains(FACT_B));
        assert!(prompt.contains(FACT_C));
        assert!(prompt.ends_with(RECALL_QUERY));
    }

    #[test]
    fn plant_facts_orders_by_depth() {
        let filler = build_filler(4000);
        let prompt = plant_facts(&filler);
        let a = prompt.find(FACT_A).unwrap();
        let b = prompt.find(FACT_B).unwrap();
        let c = prompt.find(FACT_C).unwrap();
        // A (25%) before B (50%) before C (75%).
        assert!(a < b, "A at {a} should precede B at {b}");
        assert!(b < c, "B at {b} should precede C at {c}");
        // And all are in roughly the right region of the (now larger) prompt.
        let total = prompt.len();
        assert!(a < total / 2, "A {a} should be in first half of {total}");
        assert!(c > total / 2, "C {c} should be in second half of {total}");
    }

    #[test]
    fn plant_facts_handles_empty_filler() {
        let prompt = plant_facts("");
        // All facts + query still present even with no filler.
        assert!(prompt.contains(FACT_A));
        assert!(prompt.contains(FACT_C));
        assert!(prompt.ends_with(RECALL_QUERY));
    }

    #[test]
    fn score_recall_all_three() {
        let r = "The capital is Zubrovka, Falcon launched March 7, 2019, and the limit is 847 degrees.";
        assert_eq!(score_recall(r), 3);
    }

    #[test]
    fn score_recall_partial_and_case_insensitive() {
        assert_eq!(score_recall("It is ZUBROVKA and 847"), 2);
        assert_eq!(score_recall("launched in 2019"), 1);
        assert_eq!(score_recall("I don't know anything."), 0);
    }

    #[test]
    fn is_oom_like_detects() {
        assert!(is_oom_like("CUDA out of memory", None));
        assert!(is_oom_like("process killed", None));
        assert!(is_oom_like("", Some(500)));
        assert!(is_oom_like("", Some(503)));
        assert!(!is_oom_like("model not found", Some(404)));
        assert!(!is_oom_like("ok", None));
    }

    // ---- ErrorClass (Phase 2 item 4): unifies is_oom_like + is_transport_error ----

    #[test]
    fn is_transport_error_detects() {
        // Preserves code_v2.rs's original test cases for the predicate this
        // module now owns.
        assert!(is_transport_error("error sending request for url"));
        assert!(is_transport_error("connection refused"));
        assert!(is_transport_error("operation timed out"));
        assert!(is_transport_error("unexpected EOF"));
        assert!(!is_transport_error("model 'foo' not found"));
        assert!(!is_transport_error("invalid prompt"));
        assert!(!is_transport_error("out of memory"));
    }

    #[test]
    fn classify_error_oom_cases() {
        assert_eq!(classify_error("CUDA out of memory", None), ErrorClass::Oom);
        assert_eq!(classify_error("process killed", None), ErrorClass::Oom);
        assert_eq!(classify_error("", Some(500)), ErrorClass::Oom);
        assert_eq!(classify_error("", Some(503)), ErrorClass::Oom);
    }

    #[test]
    fn classify_error_transport_cases() {
        assert_eq!(
            classify_error("error sending request for url", None),
            ErrorClass::Transport
        );
        assert_eq!(classify_error("connection refused", None), ErrorClass::Transport);
        assert_eq!(classify_error("operation timed out", None), ErrorClass::Transport);
        assert_eq!(classify_error("unexpected EOF", None), ErrorClass::Transport);
    }

    #[test]
    fn classify_error_other_cases() {
        assert_eq!(classify_error("model not found", Some(404)), ErrorClass::Other);
        assert_eq!(classify_error("ok", None), ErrorClass::Other);
        assert_eq!(classify_error("model 'foo' not found", None), ErrorClass::Other);
        assert_eq!(classify_error("invalid prompt", None), ErrorClass::Other);
    }

    #[test]
    fn classify_error_oom_wins_over_transport_on_a_dual_match() {
        // A message matching BOTH predicates (e.g. mentions "connection" AND
        // "killed") must classify as Oom, not Transport: an error that reads
        // as possibly-OOM should never be silently retried as if it were a
        // plain transient network blip. Documents the deliberate precedence
        // (see `classify_error`'s doc comment).
        let dual = "connection to worker lost: process killed";
        assert!(is_transport_error(dual), "sanity: message DOES match the transport predicate too");
        assert_eq!(classify_error(dual, None), ErrorClass::Oom);
    }

    #[test]
    fn next_pow2_ctx_grows() {
        assert_eq!(next_pow2_ctx(100), 2048);
        assert_eq!(next_pow2_ctx(2000), 4096); // 2000+1024 > 2048 → 4096
        assert!(next_pow2_ctx(60000) >= 65536);
        assert!(next_pow2_ctx(1_000_000) <= 262_144);
    }

    #[test]
    fn ollama_base_default_and_env() {
        for k in ["OLLAMA_URL", "OLLAMA_BASE_URL", "OLLAMA_CPU_URL"] {
            std::env::remove_var(k);
        }
        assert_eq!(ollama_base(), "http://127.0.0.1:11434"); // pii-test-fixture
        std::env::set_var("OLLAMA_URL", "http://live:11434/");
        assert_eq!(ollama_base(), "http://live:11434");
        std::env::remove_var("OLLAMA_URL");
    }
}
