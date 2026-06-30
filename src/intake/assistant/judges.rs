//! 3-judge panel harness (S84 ASMT-01).
//!
//! Each judge shells out to a provider OAuth CLI (`claude` / `gemini` / `codex`)
//! the way the validator harness shells out to `bash` (see `intake::code_v2`):
//! `tokio::process::Command`, prompt on **stdin**, output captured from stdout,
//! with the CLI command + model sourced from [`crate::config`] (NO literals).
//!
//! NOTE (environment): this repo has no pre-existing `claude`/`gemini`/`codex`
//! CLI reviewer; the closest patterns are `dgem::review` (HTTP DiffusionGemma)
//! and the `bash` validator subprocess in `code_v2`. We mirror the SUBPROCESS
//! shape the spec describes (`<cli> --model <model> --print`, prompt on stdin)
//! and DEGRADE gracefully: if a CLI isn't installed / logged in, that judge
//! abstains and the panel still produces a result. Unit tests use mock judges.
//!
//! ## Output contract (enforced)
//! Every judge prompt MUST end with the [`JSON_CONTRACT_SUFFIX`] line. Each judge
//! must return ONLY a JSON object mapping each requested trait to an integer
//! 1–5. The [`extract_json_object`] extractor tolerates a single leading/trailing
//! ``` fence. On a contract violation we retry ONCE with a terse reminder; a
//! second failure ⇒ that judge `abstains` (raw output stored, redacted).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::config::{self, JudgeProvider};

use super::{JudgeOutcome, PanelResult};

/// Appended to every judge prompt to enforce the JSON-only output contract.
pub const JSON_CONTRACT_SUFFIX: &str =
    "respond with ONLY a JSON object mapping each trait to an integer 1\u{2013}5, \
     no prose, no markdown fences.";

/// Terse reminder sent on the single retry after a contract violation.
const RETRY_REMINDER: &str = "Your previous reply was not valid. Reply again with \
     ONLY a JSON object mapping each requested trait to an integer 1-5. No prose, \
     no markdown, no code fences.";

/// Max bytes of raw judge output retained for audit on abstain (truncated).
const RAW_AUDIT_MAX: usize = 2000;

/// What a single judge invocation can produce.
pub enum JudgeReply {
    /// Raw text from the CLI (to be parsed/validated by the panel).
    Text(String),
    /// CLI signalled an auth failure (not logged in / token expired). Treated as
    /// abstain for the whole run with an operator warning. Never a crash.
    AuthError(String),
    /// CLI couldn't be run at all (missing binary, spawn failure, timeout).
    Unavailable(String),
}

/// A judge: invoked twice at most (initial + one retry) per item.
///
/// `invoke` receives the FULL prompt (already ending in [`JSON_CONTRACT_SUFFIX`]
/// on the first call, or the retry reminder appended on the second) and returns
/// raw text or a degradation signal. It must never panic; transport/auth issues
/// map to [`JudgeReply::AuthError`] / [`JudgeReply::Unavailable`].
#[async_trait::async_trait]
pub trait Judge: Send + Sync {
    /// Stable judge id (stored in the `judge` column), e.g. `"claude"`.
    fn id(&self) -> &str;

    /// Run the judge once with `prompt`. `attempt` is 1 (initial) or 2 (retry).
    async fn invoke(&self, prompt: &str, attempt: u8) -> JudgeReply;
}

/// A provider-CLI-backed judge (Claude / Gemini / Codex).
pub struct CliJudge {
    provider: JudgeProvider,
}

impl CliJudge {
    pub fn new(provider: JudgeProvider) -> Self {
        CliJudge { provider }
    }

    /// The three-provider panel, in order.
    pub fn panel() -> Vec<Box<dyn Judge>> {
        JudgeProvider::all()
            .into_iter()
            .map(|p| Box::new(CliJudge::new(p)) as Box<dyn Judge>)
            .collect()
    }
}

#[async_trait::async_trait]
impl Judge for CliJudge {
    fn id(&self) -> &str {
        self.provider.id()
    }

    async fn invoke(&self, prompt: &str, _attempt: u8) -> JudgeReply {
        let cli = config::judge_cli(self.provider);
        let model = config::judge_model(self.provider);
        let timeout = Duration::from_secs(config::judge_timeout_secs());
        run_cli(&cli, model.as_deref(), prompt, timeout).await
    }
}

/// Shell out to a provider CLI: `<cli> [--model <model>] --print`, prompt on
/// stdin, stdout captured. Mirrors the `tokio::process::Command` pattern used by
/// the `code_v2` validator. Classifies auth/missing-binary failures so the panel
/// can degrade rather than crash.
async fn run_cli(
    cli: &str,
    model: Option<&str>,
    prompt: &str,
    timeout: Duration,
) -> JudgeReply {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut cmd = Command::new(cli);
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    // `--print` (non-interactive, print result and exit) mirrors how the
    // operator's reviewer CLIs are driven; prompt is fed on stdin.
    cmd.arg("--print");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return JudgeReply::Unavailable(format!("cannot launch '{cli}': {e}"));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        // Best-effort: a write failure becomes an empty-output abstain downstream.
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return JudgeReply::Unavailable(format!("'{cli}' failed: {e}")),
        Err(_) => return JudgeReply::Unavailable(format!("'{cli}' timed out")),
    };

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if looks_like_auth_error(&stderr) || looks_like_auth_error(&stdout) {
            return JudgeReply::AuthError(format!("'{cli}' not authenticated"));
        }
        // Non-zero but produced stdout we can still try to parse; otherwise unavailable.
        if stdout.trim().is_empty() {
            let snippet = redact(&stderr);
            return JudgeReply::Unavailable(format!("'{cli}' exited nonzero: {snippet}"));
        }
    } else if looks_like_auth_error(&stdout) {
        return JudgeReply::AuthError(format!("'{cli}' not authenticated"));
    }

    JudgeReply::Text(stdout)
}

/// Heuristic auth-failure detection across CLIs.
fn looks_like_auth_error(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [
        "not authenticated",
        "not logged in",
        "please log in",
        "please login",
        "unauthorized",
        "401",
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

/// Truncate + strip control chars for safe audit storage.
fn redact(s: &str) -> String {
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

/// Strict JSON-object extractor. Tolerates a SINGLE leading/trailing markdown
/// fence (```json … ``` or ``` … ```) and surrounding whitespace/prose around a
/// single top-level `{ … }`. Returns `None` for anything else (garbage).
pub fn extract_json_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();

    // 1. Strip a single fenced block if present.
    let candidate = strip_single_fence(trimmed).unwrap_or(trimmed);

    // 2. Direct parse.
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(candidate.trim()) {
        return Some(v);
    }

    // 3. Prose-wrapped: take the first balanced top-level `{...}` span.
    let span = first_balanced_object(candidate)?;
    match serde_json::from_str::<Value>(span) {
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    }
}

/// If `s` is exactly one ```-fenced block, return its inner content; else None.
fn strip_single_fence(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with("```") {
        return None;
    }
    // drop the opening fence line (``` or ```json)
    let after_open = s.find('\n').map(|i| &s[i + 1..]).unwrap_or("");
    // require a closing fence
    let close = after_open.rfind("```")?;
    Some(after_open[..close].trim())
}

/// Find the first balanced `{...}` span (string-aware, escape-aware).
fn first_balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        let c = b as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Validate a parsed object against the trait contract: every `required` trait
/// present, every value an integer in [1,5]. Returns the validated map or an
/// error reason.
pub fn validate_traits(
    obj: &Value,
    required: &[&str],
) -> Result<BTreeMap<String, i64>, String> {
    let map = obj.as_object().ok_or("not a JSON object")?;
    let mut out = BTreeMap::new();
    for &trait_name in required {
        let v = map
            .get(trait_name)
            .ok_or_else(|| format!("missing trait '{trait_name}'"))?;
        let n = v
            .as_i64()
            .ok_or_else(|| format!("trait '{trait_name}' is not an integer"))?;
        // reject floats encoded as JSON numbers with fractional part
        if v.is_f64() && v.as_f64().map(|f| f.fract() != 0.0).unwrap_or(false) {
            return Err(format!("trait '{trait_name}' is not an integer"));
        }
        if !(1..=5).contains(&n) {
            return Err(format!("trait '{trait_name}' = {n} out of range [1,5]"));
        }
        out.insert(trait_name.to_string(), n);
    }
    Ok(out)
}

/// Run ONE judge over an item: invoke, parse/validate, retry once on failure,
/// abstain on second failure / auth / unavailability. Pure outcome — no DB.
///
/// `base_prompt` MUST already end with [`JSON_CONTRACT_SUFFIX`]. `required` is
/// the trait set the prompt asked for.
pub async fn run_one_judge(
    judge: &dyn Judge,
    base_prompt: &str,
    required: &[&str],
) -> (JudgeOutcome, Option<String>) {
    // attempt 1
    match judge.invoke(base_prompt, 1).await {
        JudgeReply::AuthError(msg) => {
            return (
                JudgeOutcome::Abstained {
                    judge: judge.id().to_string(),
                    reason: format!("auth error: {msg}"),
                    raw: None,
                },
                Some(format!("judge '{}' abstained — {msg}", judge.id())),
            );
        }
        JudgeReply::Unavailable(msg) => {
            return (
                JudgeOutcome::Abstained {
                    judge: judge.id().to_string(),
                    reason: format!("unavailable: {msg}"),
                    raw: None,
                },
                Some(format!("judge '{}' unavailable — {msg}", judge.id())),
            );
        }
        JudgeReply::Text(text) => {
            if let Some(traits) = parse_and_validate(&text, required) {
                return (
                    JudgeOutcome::Scored {
                        judge: judge.id().to_string(),
                        traits,
                    },
                    None,
                );
            }
            // fall through to retry
            let retry_prompt = format!("{base_prompt}\n\n{RETRY_REMINDER}");
            match judge.invoke(&retry_prompt, 2).await {
                JudgeReply::AuthError(msg) => (
                    JudgeOutcome::Abstained {
                        judge: judge.id().to_string(),
                        reason: format!("auth error on retry: {msg}"),
                        raw: Some(redact(&text)),
                    },
                    Some(format!("judge '{}' abstained — {msg}", judge.id())),
                ),
                JudgeReply::Unavailable(msg) => (
                    JudgeOutcome::Abstained {
                        judge: judge.id().to_string(),
                        reason: format!("unavailable on retry: {msg}"),
                        raw: Some(redact(&text)),
                    },
                    Some(format!("judge '{}' unavailable — {msg}", judge.id())),
                ),
                JudgeReply::Text(text2) => {
                    if let Some(traits) = parse_and_validate(&text2, required) {
                        (
                            JudgeOutcome::Scored {
                                judge: judge.id().to_string(),
                                traits,
                            },
                            None,
                        )
                    } else {
                        (
                            JudgeOutcome::Abstained {
                                judge: judge.id().to_string(),
                                reason: "invalid output after retry".to_string(),
                                raw: Some(redact(&text2)),
                            },
                            None,
                        )
                    }
                }
            }
        }
    }
}

/// Parse + validate one raw judge reply. `None` ⇒ contract violation.
fn parse_and_validate(text: &str, required: &[&str]) -> Option<BTreeMap<String, i64>> {
    let obj = extract_json_object(text)?;
    validate_traits(&obj, required).ok()
}

/// Run the FULL panel over one item and aggregate. Each judge runs independently
/// (with its own retry/abstain), then [`PanelResult::aggregate`] computes the
/// per-trait mean + sample SD over complying judges. Operator warnings (auth /
/// unavailability) are collected, never fatal.
pub async fn run_panel(
    judges: &[Box<dyn Judge>],
    dimension: &str,
    base_prompt: &str,
    required: &[&str],
) -> PanelResult {
    let mut outcomes = Vec::with_capacity(judges.len());
    let mut warnings = Vec::new();
    // Sequential: judge CLIs may contend for the same machine; keeps it simple
    // and matches the sequential intake-run assumption (`infer::set_backend_override`).
    for j in judges {
        let (outcome, warn) = run_one_judge(j.as_ref(), base_prompt, required).await;
        if let Some(w) = warn {
            warnings.push(w);
        }
        outcomes.push(outcome);
    }
    PanelResult::aggregate(dimension, outcomes, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JSON extractor: clean / fenced / prose-wrapped / garbage ──

    #[test]
    fn extract_clean_object() {
        let v = extract_json_object(r#"{"clarity": 4, "tone": 3}"#).unwrap();
        assert_eq!(v["clarity"], 4);
    }

    #[test]
    fn extract_fenced_object() {
        let raw = "```json\n{\"clarity\": 5}\n```";
        let v = extract_json_object(raw).unwrap();
        assert_eq!(v["clarity"], 5);
        // bare fence too
        let raw2 = "```\n{\"clarity\": 2}\n```";
        assert_eq!(extract_json_object(raw2).unwrap()["clarity"], 2);
    }

    #[test]
    fn extract_prose_wrapped_object() {
        let raw = "Sure! Here is my assessment:\n{\"clarity\": 3, \"tone\": 4}\nHope that helps.";
        let v = extract_json_object(raw).unwrap();
        assert_eq!(v["tone"], 4);
    }

    #[test]
    fn extract_garbage_is_none() {
        assert!(extract_json_object("I cannot comply with that.").is_none());
        assert!(extract_json_object("").is_none());
        assert!(extract_json_object("[1,2,3]").is_none()); // array, not object
    }

    // ── validation ──

    #[test]
    fn validate_accepts_in_range_integers() {
        let v: Value = serde_json::from_str(r#"{"a":1,"b":5,"c":3}"#).unwrap();
        let m = validate_traits(&v, &["a", "b", "c"]).unwrap();
        assert_eq!(m["b"], 5);
    }

    #[test]
    fn validate_rejects_missing_out_of_range_and_floats() {
        let v: Value = serde_json::from_str(r#"{"a":1}"#).unwrap();
        assert!(validate_traits(&v, &["a", "b"]).is_err()); // missing b
        let v2: Value = serde_json::from_str(r#"{"a":6}"#).unwrap();
        assert!(validate_traits(&v2, &["a"]).is_err()); // out of range
        let v3: Value = serde_json::from_str(r#"{"a":3.5}"#).unwrap();
        assert!(validate_traits(&v3, &["a"]).is_err()); // float
        let v4: Value = serde_json::from_str(r#"{"a":0}"#).unwrap();
        assert!(validate_traits(&v4, &["a"]).is_err()); // below range
    }

    // ── mock judges: clean / needs-retry / garbage-twice / auth ──

    struct ScriptedJudge {
        id: String,
        replies: std::sync::Mutex<Vec<JudgeReply>>,
    }

    impl ScriptedJudge {
        fn new(id: &str, replies: Vec<JudgeReply>) -> Self {
            ScriptedJudge {
                id: id.to_string(),
                replies: std::sync::Mutex::new(replies),
            }
        }
    }

    #[async_trait::async_trait]
    impl Judge for ScriptedJudge {
        fn id(&self) -> &str {
            &self.id
        }
        async fn invoke(&self, _prompt: &str, _attempt: u8) -> JudgeReply {
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                JudgeReply::Unavailable("script exhausted".into())
            } else {
                r.remove(0)
            }
        }
    }

    fn text(s: &str) -> JudgeReply {
        JudgeReply::Text(s.to_string())
    }

    #[tokio::test]
    async fn judge_clean_first_try() {
        let j = ScriptedJudge::new("claude", vec![text(r#"{"clarity":4}"#)]);
        let (out, warn) = run_one_judge(&j, "...prompt", &["clarity"]).await;
        assert!(warn.is_none());
        assert!(out.complied());
    }

    #[tokio::test]
    async fn judge_retry_then_succeeds() {
        let j = ScriptedJudge::new(
            "gemini",
            vec![text("no idea, sorry"), text(r#"{"clarity":3}"#)],
        );
        let (out, _) = run_one_judge(&j, "...prompt", &["clarity"]).await;
        match out {
            JudgeOutcome::Scored { traits, .. } => assert_eq!(traits["clarity"], 3),
            _ => panic!("expected scored after retry"),
        }
    }

    #[tokio::test]
    async fn judge_abstains_after_two_failures() {
        let j = ScriptedJudge::new("codex", vec![text("garbage"), text("still garbage")]);
        let (out, _) = run_one_judge(&j, "...prompt", &["clarity"]).await;
        assert!(!out.complied());
        match out {
            JudgeOutcome::Abstained { raw, .. } => assert!(raw.is_some()),
            _ => panic!("expected abstain"),
        }
    }

    #[tokio::test]
    async fn judge_auth_error_abstains_with_warning() {
        let j = ScriptedJudge::new("claude", vec![JudgeReply::AuthError("not logged in".into())]);
        let (out, warn) = run_one_judge(&j, "...prompt", &["clarity"]).await;
        assert!(!out.complied());
        assert!(warn.unwrap().contains("claude"));
    }

    // ── full panel: 3 mocked judges ──

    #[tokio::test]
    async fn panel_three_stub_judges_aggregates() {
        let judges: Vec<Box<dyn Judge>> = vec![
            Box::new(ScriptedJudge::new("claude", vec![text(r#"{"clarity":3}"#)])),
            Box::new(ScriptedJudge::new("gemini", vec![text(r#"{"clarity":4}"#)])),
            Box::new(ScriptedJudge::new("codex", vec![text(r#"{"clarity":5}"#)])),
        ];
        let pr = run_panel(&judges, "instruction_following", "p", &["clarity"]).await;
        assert_eq!(pr.complying, 3);
        let agg = &pr.aggregates["clarity"];
        assert!((agg.mean - 4.0).abs() < 1e-9); // [3,4,5] → 4.0
        assert!((agg.std_dev.unwrap() - 1.0).abs() < 1e-9); // SD 1.0
        assert!(pr.warnings.is_empty());
    }

    #[tokio::test]
    async fn panel_mixed_compliance_and_warning() {
        let judges: Vec<Box<dyn Judge>> = vec![
            Box::new(ScriptedJudge::new("claude", vec![text(r#"{"clarity":4}"#)])),
            Box::new(ScriptedJudge::new(
                "gemini",
                vec![JudgeReply::AuthError("unauthorized".into())],
            )),
            Box::new(ScriptedJudge::new("codex", vec![text("junk"), text("junk")])),
        ];
        let pr = run_panel(&judges, "tone", "p", &["clarity"]).await;
        assert_eq!(pr.complying, 1);
        assert!(pr.aggregates["clarity"].low_confidence);
        assert_eq!(pr.aggregates["clarity"].std_dev, None);
        assert!(pr.warnings.iter().any(|w| w.contains("gemini")));
    }

    #[tokio::test]
    async fn panel_all_abstain_is_unscored() {
        let judges: Vec<Box<dyn Judge>> = vec![
            Box::new(ScriptedJudge::new("claude", vec![text("no"), text("no")])),
            Box::new(ScriptedJudge::new("gemini", vec![text("no"), text("no")])),
            Box::new(ScriptedJudge::new("codex", vec![text("no"), text("no")])),
        ];
        let pr = run_panel(&judges, "creativity", "p", &["clarity"]).await;
        assert!(pr.is_unscored());
        assert_eq!(pr.complying, 0);
    }
}
