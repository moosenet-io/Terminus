//! Review prompt construction and verdict parsing.
//!
//! Pure, side-effect-free functions so the exact prompt text per structure/role
//! and the exact verdict-extraction logic can be unit tested without any
//! network I/O.

use serde_json::Value;

/// The four review structures `review_run` supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    Single,
    AdversarialPair,
    PanelMajority,
    PanelUnanimous,
}

impl Structure {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "single" => Some(Structure::Single),
            "adversarial_pair" => Some(Structure::AdversarialPair),
            "panel_majority" => Some(Structure::PanelMajority),
            "panel_unanimous" => Some(Structure::PanelUnanimous),
            _ => None,
        }
    }
}

/// The role a single provider plays within a structure's prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Plain independent reviewer (single, panel_majority, panel_unanimous).
    Reviewer,
    /// The "defend" side of an adversarial pair.
    Defend,
    /// The "attack" side of an adversarial pair -- explicitly instructed to try
    /// to refute the change / find rejection reasons.
    Attack,
}

/// Build the review prompt for one provider: role framing + criteria +
/// serialized context. Every prompt ends with an explicit instruction to
/// terminate the response with a `VERDICT: ...` line so [`parse_verdict`] can
/// extract it deterministically.
pub fn build_prompt(role: Role, criteria: &str, context: &Value) -> String {
    let context_str = serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string());

    match role {
        Role::Reviewer => format!(
            "You are an independent code/change reviewer.\n\n\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Review the change against the criteria above. Give your reasoning, then end your \
response with EXACTLY one line, verbatim:\n\
VERDICT: APPROVE\n\
or\n\
VERDICT: REQUEST_CHANGES\n"
        ),
        Role::Defend => format!(
            "You are DEFENDING this change as sound and ready to ship.\n\n\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Argue why the change satisfies the criteria above. Give your reasoning, then end your \
response with EXACTLY one line, verbatim:\n\
VERDICT: APPROVE\n\
or\n\
VERDICT: REQUEST_CHANGES\n"
        ),
        Role::Attack => format!(
            "You are ATTACKING this change. Your job is to actively try to REFUTE it: find every \
plausible reason it should be rejected against the criteria below. Do not be charitable -- \
assume the defender is wrong until proven otherwise, and argue the strongest case for rejection \
you can construct.\n\n\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Give your reasoning, then end your response with EXACTLY one line, verbatim:\n\
VERDICT: REFUTED\n\
(if you found a valid, concrete reason to reject the change)\n\
or\n\
VERDICT: NOT_REFUTED\n\
(if the change genuinely withstands your attempt to refute it)\n"
        ),
    }
}

/// Parsed verdict token. `Unknown` covers a response that never produced a
/// recognizable `VERDICT:` line -- treated as a fail-safe non-approval by
/// aggregation, never silently coerced into APPROVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Refuted,
    NotRefuted,
    Unknown,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Refuted => "REFUTED",
            Verdict::NotRefuted => "NOT_REFUTED",
            Verdict::Unknown => "UNKNOWN",
        }
    }
}

/// Extract the `VERDICT: ...` token from a provider's raw response text, and
/// return `(verdict, reasoning)` where `reasoning` is the response with the
/// verdict line stripped (trimmed). Scans from the END of the text backwards
/// so a model that restates "VERDICT: APPROVE | REQUEST_CHANGES" as part of
/// its instructions-echo earlier in the response doesn't get picked up
/// instead of its actual final answer.
pub fn parse_verdict(raw: &str) -> (Verdict, String) {
    // ASCII-only uppercasing (not `to_uppercase()`): some Unicode uppercasing
    // is NOT byte-length-preserving (e.g. U+01F0 'ǰ' -> "J" + combining caron
    // grows from 2 bytes to 3), which would desync `pos` (a byte offset into
    // `upper`) from `raw`'s byte offsets -- best case mis-sliced reasoning,
    // worst case an out-of-bounds/non-char-boundary slice panic on
    // model-generated prose containing such characters. `to_ascii_uppercase`
    // only remaps ASCII bytes (0x00-0x7F) and passes every other byte through
    // unchanged, so it's exactly byte-length-preserving and `VERDICT:` is
    // itself pure ASCII, so the search is unaffected.
    let upper = raw.to_ascii_uppercase();
    let anchor = upper.rfind("VERDICT:");

    let Some(pos) = anchor else {
        return (Verdict::Unknown, raw.trim().to_string());
    };

    let after = &raw[pos + "VERDICT:".len()..];
    let token_line = after.lines().next().unwrap_or("").trim().to_uppercase();

    let verdict = if token_line.contains("REQUEST_CHANGES") || token_line.contains("REQUEST CHANGES") {
        Verdict::RequestChanges
    } else if token_line.contains("NOT_REFUTED") || token_line.contains("NOT REFUTED") {
        Verdict::NotRefuted
    } else if token_line.contains("REFUTED") {
        Verdict::Refuted
    } else if token_line.contains("APPROVE") {
        Verdict::Approve
    } else {
        Verdict::Unknown
    };

    let reasoning = raw[..pos].trim().to_string();
    let reasoning = if reasoning.is_empty() { raw.trim().to_string() } else { reasoning };

    (verdict, reasoning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structure_parse_recognizes_all_four() {
        assert_eq!(Structure::parse("single"), Some(Structure::Single));
        assert_eq!(Structure::parse("adversarial_pair"), Some(Structure::AdversarialPair));
        assert_eq!(Structure::parse("panel_majority"), Some(Structure::PanelMajority));
        assert_eq!(Structure::parse("panel_unanimous"), Some(Structure::PanelUnanimous));
        assert_eq!(Structure::parse("bogus"), None);
    }

    #[test]
    fn reviewer_prompt_contains_criteria_and_context_and_verdict_instruction() {
        let ctx = json!({"diff": "+ fn foo() {}"});
        let p = build_prompt(Role::Reviewer, "must compile", &ctx);
        assert!(p.contains("must compile"));
        assert!(p.contains("fn foo()"));
        assert!(p.contains("VERDICT: APPROVE"));
        assert!(p.contains("VERDICT: REQUEST_CHANGES"));
        assert!(!p.to_uppercase().contains("REFUTED"));
    }

    #[test]
    fn attack_prompt_explicitly_instructs_refutation() {
        let ctx = json!({});
        let p = build_prompt(Role::Attack, "criteria", &ctx);
        assert!(p.to_uppercase().contains("REFUTE"));
        assert!(p.contains("VERDICT: REFUTED"));
        assert!(p.contains("VERDICT: NOT_REFUTED"));
    }

    #[test]
    fn defend_prompt_frames_defense_role() {
        let ctx = json!({});
        let p = build_prompt(Role::Defend, "criteria", &ctx);
        assert!(p.contains("DEFENDING"));
        assert!(p.contains("VERDICT: APPROVE"));
        assert!(p.contains("VERDICT: REQUEST_CHANGES"));
    }

    #[test]
    fn parse_verdict_extracts_approve() {
        let (v, reasoning) = parse_verdict("Looks fine.\n\nVERDICT: APPROVE");
        assert_eq!(v, Verdict::Approve);
        assert_eq!(reasoning, "Looks fine.");
    }

    #[test]
    fn parse_verdict_extracts_request_changes() {
        let (v, _) = parse_verdict("Needs work.\nVERDICT: REQUEST_CHANGES");
        assert_eq!(v, Verdict::RequestChanges);
    }

    #[test]
    fn parse_verdict_extracts_refuted() {
        let (v, _) = parse_verdict("Found a bug.\nVERDICT: REFUTED");
        assert_eq!(v, Verdict::Refuted);
    }

    #[test]
    fn parse_verdict_extracts_not_refuted_and_does_not_confuse_with_refuted() {
        let (v, _) = parse_verdict("Survived scrutiny.\nVERDICT: NOT_REFUTED");
        assert_eq!(v, Verdict::NotRefuted);
    }

    #[test]
    fn parse_verdict_unknown_when_no_marker_present() {
        let (v, reasoning) = parse_verdict("I have thoughts but no marker.");
        assert_eq!(v, Verdict::Unknown);
        assert_eq!(reasoning, "I have thoughts but no marker.");
    }

    #[test]
    fn parse_verdict_handles_non_length_preserving_uppercase_chars_without_panicking() {
        // U+01F0 'ǰ' (2 bytes in UTF-8) uppercases via `str::to_uppercase()` to
        // "J\u{30C}" (3 bytes) -- NOT byte-length-preserving. A byte offset
        // found in a `to_uppercase()`'d copy would desync from the original
        // string's byte offsets here, corrupting the slice or panicking on a
        // non-char-boundary. `to_ascii_uppercase()` must be used instead so
        // this reasoning text (before the verdict marker) doesn't break
        // parsing. This is a regression test for that bug.
        let raw = "The character \u{1F0} appears in this review.\nVERDICT: APPROVE";
        let (v, reasoning) = parse_verdict(raw);
        assert_eq!(v, Verdict::Approve);
        assert!(reasoning.contains('\u{1F0}'));
    }

    #[test]
    fn parse_verdict_uses_last_occurrence_not_first() {
        // A model that echoes the instruction text ("...VERDICT: APPROVE...")
        // earlier in its response must not have that echo mistaken for its
        // real final answer -- the LAST marker in the text wins.
        let raw = "Instructions mentioned VERDICT: APPROVE as one valid option.\n\
                   After review, my actual answer is:\nVERDICT: REQUEST_CHANGES";
        let (v, _) = parse_verdict(raw);
        assert_eq!(v, Verdict::RequestChanges);
    }
}
