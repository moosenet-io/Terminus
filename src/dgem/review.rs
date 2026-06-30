//! `dgem_review` — structured code/PR review via DiffusionGemma.
//!
//! Builds the standardized review prompt (matches spec skill Stage 5 format), sends it to the daemon,
//! and parses a structured verdict (APPROVED / CHANGES_REQUESTED + issues list). The model's thinking
//! trace is separated out so the pipeline can post the verdict + reasoning to a PR comment without the
//! noisy trace.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::{split_thinking, DgemConfig};

pub struct DgemReview {
    cfg: DgemConfig,
}

impl DgemReview {
    pub(crate) fn new(cfg: DgemConfig) -> Self {
        Self { cfg }
    }
}

/// Construct the standardized review prompt. Kept verbatim with the S80 spec template.
pub(crate) fn build_review_prompt(item_title: &str, acceptance_criteria: &str, diff: &str) -> String {
    format!(
        "You are reviewing a PR for the Lumina Constellation project (Rust/TypeScript).\n\
This PR implements: {item_title}\n\n\
Review for:\n\
1. CORRECTNESS: Does the code match the acceptance criteria?\n\
2. SECURITY: Any hardcoded IPs, tokens, secrets, org names? Any std::env::var for secrets instead of vault::manager().get()?\n\
3. ARCHITECTURE: Any unnecessary complexity?\n\
4. ERROR HANDLING: Are edge cases handled?\n\
5. TEST COVERAGE: Do tests cover the acceptance criteria? At least one negative test?\n\n\
Respond with EXACTLY one of:\n\
- APPROVED — if all checks pass\n\
- CHANGES_REQUESTED — followed by a numbered list of specific issues to fix\n\n\
Acceptance criteria:\n{acceptance_criteria}\n\n\
Diff:\n{diff}\n"
    )
}

/// Parsed verdict from the model's (post-thinking) answer.
pub(crate) struct Verdict {
    pub verdict: &'static str, // "APPROVED" | "CHANGES_REQUESTED"
    pub issues: Vec<String>,
}

/// Parse the verdict + issues from the answer text. CHANGES_REQUESTED wins when both tokens appear
/// (a reviewer that lists problems and also says "approved" is requesting changes). Issues are the
/// numbered/bulleted lines following the verdict.
pub(crate) fn parse_verdict(answer: &str) -> Verdict {
    let upper = answer.to_uppercase();
    let changes = upper.contains("CHANGES_REQUESTED") || upper.contains("CHANGES REQUESTED");
    // A bare "APPROVED" substring is not enough: "NOT APPROVED" / "CANNOT BE APPROVED" are rejections.
    // Treat any negative-approval phrasing (or an explicit changes request) as a rejection.
    let rejected = changes
        || upper.contains("NOT APPROVED")
        || upper.contains("NOT_APPROVED")
        || upper.contains("CANNOT BE APPROVED")
        || upper.contains("CANNOT APPROVE");
    let approved = upper.contains("APPROVED");

    let verdict = if !rejected && approved {
        "APPROVED"
    } else {
        // Default to CHANGES_REQUESTED when no clear APPROVED is present — fail safe, never rubber-stamp.
        "CHANGES_REQUESTED"
    };

    let issues = if verdict == "CHANGES_REQUESTED" {
        extract_issues(answer)
    } else {
        Vec::new()
    };

    Verdict { verdict, issues }
}

/// Extract numbered/bulleted issue lines from the answer. Anchored to the text AFTER the
/// CHANGES_REQUESTED marker (when present) so the model's restated checklist or pre-verdict reasoning
/// is not harvested as "issues".
fn extract_issues(answer: &str) -> Vec<String> {
    let upper = answer.to_uppercase();
    let anchor = upper
        .find("CHANGES_REQUESTED")
        .or_else(|| upper.find("CHANGES REQUESTED"));
    let scope = match anchor {
        // Skip past the marker line itself; scan everything after it.
        Some(pos) => &answer[pos..],
        None => answer,
    };

    let mut out = Vec::new();
    for raw in scope.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let is_numbered = line
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
            && line.contains(['.', ')', ':']);
        let is_bullet = line.starts_with("- ") || line.starts_with("* ");
        if is_numbered || is_bullet {
            // Strip the leading marker for a clean issue string.
            let cleaned = line
                .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ':' | '-' | '*' | ' '))
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
    }
    out
}

#[async_trait]
impl RustTool for DgemReview {
    fn name(&self) -> &str {
        "dgem_review"
    }

    fn description(&self) -> &str {
        "Review a code diff locally with DiffusionGemma ($0, no cloud call) — the build pipeline's \
secondary reviewer. Provide the 'diff', the 'acceptance_criteria' it must satisfy, and the 'item_title'. \
Returns JSON with a 'verdict' (APPROVED or CHANGES_REQUESTED), a numbered 'issues' list, the 'reasoning' \
(safe to post to a PR comment), and the raw 'thinking' trace (omit from PR comments)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "diff": {
                    "type": "string",
                    "description": "The unified diff to review."
                },
                "acceptance_criteria": {
                    "type": "string",
                    "description": "The acceptance criteria the diff must satisfy."
                },
                "item_title": {
                    "type": "string",
                    "description": "Short title of what the PR implements."
                }
            },
            "required": ["diff", "acceptance_criteria", "item_title"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let diff = args["diff"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'diff' is required".into()))?;
        let acceptance_criteria = args["acceptance_criteria"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'acceptance_criteria' is required".into()))?;
        let item_title = args["item_title"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'item_title' is required".into()))?;

        // Dual-threshold guard on the diff: above the latency threshold (~4K tokens) the pipeline should
        // use Haiku for speed; above the OOM cap (~10K) it must. Each returns a distinct error so the
        // caller knows which fallback to take. Measured on the diff (it dominates the prompt) to match
        // how the pipeline reasons about diff size.
        self.cfg.check_review_size(diff)?;

        let prompt = build_review_prompt(item_title, acceptance_criteria, diff);

        let resp = self.cfg.generate("", &prompt, self.cfg.default_max_tokens()).await?;
        let (thinking, reasoning) = split_thinking(&resp.text);
        let parsed = parse_verdict(&reasoning);

        Ok(json!({
            "verdict": parsed.verdict,
            "reasoning": reasoning,
            "issues": parsed.issues,
            "thinking": thinking,
            "time_ms": resp.time_ms,
            "model_load_ms": resp.model_load_ms,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> DgemReview {
        DgemReview::new(DgemConfig::test_default())
    }

    #[test]
    fn prompt_contains_template_sections() {
        let p = build_review_prompt("My Feature", "must do X", "diff body");
        assert!(p.contains("This PR implements: My Feature"));
        assert!(p.contains("1. CORRECTNESS"));
        assert!(p.contains("vault::manager().get()"));
        assert!(p.contains("Acceptance criteria:\nmust do X"));
        assert!(p.contains("Diff:\ndiff body"));
    }

    #[test]
    fn parse_verdict_approved() {
        let v = parse_verdict("APPROVED — all checks pass.");
        assert_eq!(v.verdict, "APPROVED");
        assert!(v.issues.is_empty());
    }

    #[test]
    fn parse_verdict_changes_requested_with_issues() {
        let answer = "CHANGES_REQUESTED\n1. Hardcoded IP at line 5.\n2. Missing negative test.\n";
        let v = parse_verdict(answer);
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
        assert_eq!(v.issues.len(), 2);
        assert_eq!(v.issues[0], "Hardcoded IP at line 5.");
        assert_eq!(v.issues[1], "Missing negative test.");
    }

    #[test]
    fn parse_verdict_bulleted_issues() {
        let answer = "CHANGES REQUESTED\n- First problem\n- Second problem";
        let v = parse_verdict(answer);
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
        assert_eq!(v.issues, vec!["First problem", "Second problem"]);
    }

    #[test]
    fn parse_verdict_not_approved_is_rejection() {
        // "NOT APPROVED" must not be read as APPROVED via naive substring match (fail-open bug).
        let v = parse_verdict("This is NOT APPROVED until the hardcoded IP is removed.");
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
        let v2 = parse_verdict("The PR cannot be APPROVED as written.");
        assert_eq!(v2.verdict, "CHANGES_REQUESTED");
    }

    #[test]
    fn extract_issues_ignores_pre_verdict_checklist() {
        // The model restates the criteria before the verdict; those lines must not become "issues".
        let answer = "Checking:\n1. CORRECTNESS: ok\n2. SECURITY: ok\n\nCHANGES_REQUESTED\n1. Add a negative test.";
        let v = parse_verdict(answer);
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
        assert_eq!(v.issues, vec!["Add a negative test."]);
    }

    #[test]
    fn parse_verdict_defaults_to_changes_when_unclear() {
        // No explicit APPROVED → fail safe, never rubber-stamp.
        let v = parse_verdict("The code looks mostly fine but I'm not sure.");
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
    }

    #[test]
    fn parse_verdict_changes_wins_over_approved() {
        let v = parse_verdict("Some parts APPROVED but overall CHANGES_REQUESTED\n1. Fix this.");
        assert_eq!(v.verdict, "CHANGES_REQUESTED");
    }

    #[tokio::test]
    async fn requires_all_fields() {
        let err = tool()
            .execute(json!({"diff": "d", "acceptance_criteria": "a"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
