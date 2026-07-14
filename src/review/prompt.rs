//! Review prompt construction and verdict parsing.
//!
//! Pure, side-effect-free functions so the exact prompt text per structure/role
//! and the exact verdict-extraction logic can be unit tested without any
//! network I/O.

use serde_json::Value;

use super::aggregate::Finding;

/// KGFIND-02: appended to every `build_prompt` role arm, after the VERDICT
/// sentinel instruction. Purely optional/additive on the model's side --
/// `parse_findings` treats an absent or malformed block as zero findings, and
/// this text never alters or replaces the VERDICT line, which remains the
/// sole authoritative outcome.
const FINDINGS_INSTRUCTION: &str = "\n\nAfter the VERDICT line, optionally emit a line \
`FINDINGS_JSON:` followed by a JSON array of concrete issues you found, each object \
{\"category\":..., \"severity\":..., \"file\":..., \"symbol\":..., \"description\":...} \
(empty array if none). This structured output is optional and MUST NOT change or replace \
your VERDICT line above, which remains authoritative.\n";

/// The four review structures `review_run` supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    Single,
    AdversarialPair,
    PanelMajority,
    PanelUnanimous,
    /// The Epic Review capstone (S111C): a whole-repo strategic audit that runs
    /// ONCE at the end of a build (all sprints merged+verified). Providers audit
    /// the ENTIRE codebase against the spec/behavior contracts — surfacing deep
    /// correctness bugs, cross-module integration gaps, and architecture drift a
    /// per-diff review can't see — and EMIT findings (never edit code). It is
    /// ADVISORY: its verdict summarizes the audit but never gates a merge, and its
    /// completion (not its verdict) drives the KG refresh + doc engine at the end.
    Epic,
}

impl Structure {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "single" => Some(Structure::Single),
            "adversarial_pair" => Some(Structure::AdversarialPair),
            "panel_majority" => Some(Structure::PanelMajority),
            "panel_unanimous" => Some(Structure::PanelUnanimous),
            "epic" => Some(Structure::Epic),
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
    /// The Epic Review capstone auditor: audits the WHOLE codebase against the
    /// contracts (not a diff) and emits findings; advisory, never fixes code.
    Auditor,
    /// The "attack" side of an adversarial pair -- explicitly instructed to try
    /// to refute the change / find rejection reasons.
    Attack,
    /// REVCAP-01 PART B: an INTENSIVE-SUBSTITUTE reviewer -- this provider is
    /// standing in for a currently-DOWN frontier reviewer (codex/agy/opus) and
    /// must review HARDER than parity, per the operator's explicit ask.
    /// Refutation-first, single-deep-pass framing (deliberately NOT the
    /// `Attack` role's two-sided-debate framing, and NOT a literal 2-pass
    /// self-critique loop -- that shape is what timed out at the routine 120s
    /// daemon backstop in the incident that motivated this; a longer
    /// [`crate::review::dispatch::DaemonOpts::intensive`] budget plus a
    /// deliberately harder single pass is the safer shape under a time limit).
    /// Still emits a plain `VERDICT: APPROVE`/`REQUEST_CHANGES` line (like
    /// [`Role::Reviewer`]), NOT `Attack`'s `REFUTED`/`NOT_REFUTED` pair, so this
    /// substitute's verdict aggregates into the SAME panel as every other
    /// non-substitute member without special-casing `aggregate()`.
    IntensiveReviewer,
}

/// Build the review prompt for one provider: role framing + criteria +
/// serialized context. Every prompt ends with an explicit instruction to
/// terminate the response with a `VERDICT: ...` line so [`parse_verdict`] can
/// extract it deterministically.
pub fn build_prompt(role: Role, criteria: &str, context: &Value) -> String {
    let context_str = serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string());
    // KGREV-01: when `context` carries a `knowledge_graph` block (injected by
    // `crate::review::kg_context::inject`, best-effort and only when a
    // `project_id` resolves to a stored Atlas graph), prepend a one-line
    // pointer to it. The block itself is already surfaced via `context_str`
    // above (it's just another key in the serialized context) -- this is
    // purely a framing nudge, not a second copy of the data. Absent the key
    // (the common/no-`project_id` path), this is a no-op string, keeping the
    // prompt byte-for-byte identical to pre-KGREV-01 behavior.
    let kg_pointer = if context.get("knowledge_graph").is_some() {
        "A `knowledge_graph` section below gives the structural blast radius (callers/callees/subsystem) \
of the changed symbols -- weigh cross-module impact.\n\n"
    } else {
        ""
    };

    let base = match role {
        Role::Reviewer => format!(
            "You are an independent code/change reviewer.\n\n\
{kg_pointer}\
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
{kg_pointer}\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Argue why the change satisfies the criteria above. Give your reasoning, then end your \
response with EXACTLY one line, verbatim:\n\
VERDICT: APPROVE\n\
or\n\
VERDICT: REQUEST_CHANGES\n"
        ),
        Role::Auditor => format!(
            "You are conducting an EPIC REVIEW — a whole-system capstone audit run ONCE at the end \
of a completed build (all sprints merged + verified). You are NOT reviewing a diff: audit the \
ENTIRE codebase and its stated contracts as a whole.\n\n\
{kg_pointer}\
Contracts / acceptance criteria for the build:\n{criteria}\n\n\
Context (whole-repo pointers / spec contracts / description):\n{context_str}\n\n\
Your job is a STRATEGIC audit, not a line edit: surface deep correctness bugs, cross-module \
integration gaps, contract violations, and architecture drift that per-diff reviews cannot see. \
Do NOT fix code and do NOT nitpick per-function style — produce durable, actionable FINDINGS that \
cheaper implementer agents can turn into work items. Give your reasoning, emit your findings in the \
FINDINGS block, then end your response with EXACTLY one line, verbatim:\n\
VERDICT: APPROVE\n\
(if the build faithfully satisfies its contracts with no material findings)\n\
or\n\
VERDICT: REQUEST_CHANGES\n\
(if you surfaced material findings for follow-up — this is ADVISORY, it does not revert the build)\n"
        ),
        Role::IntensiveReviewer => format!(
            "You are an INTENSIVE substitute reviewer, standing in for a frontier reviewer that is \
currently unavailable (rate-limited or shelved). Because adversarial coverage is reduced for this \
review, you must review HARDER than a normal single pass: actively try to REFUTE this change before \
you approve it. Work through this checklist and only reach VERDICT: APPROVE if the change survives \
every step with no surviving counterexample:\n\
1. Try to construct a concrete input, sequence, or edge case that breaks the change.\n\
2. Check every claim in the change's own reasoning/comments against the actual code -- do not take \
stated intent at face value.\n\
3. Look for what the criteria below does NOT explicitly cover, and check the change is still correct \
there (silent gaps are not passes).\n\
4. Only if you worked through 1-3 and found no valid, concrete counterexample, approve.\n\n\
{kg_pointer}\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Give your reasoning (including what you tried to break and why it held, or what you found), then end \
your response with EXACTLY one line, verbatim:\n\
VERDICT: APPROVE\n\
(only if no counterexample survived your attempt to refute it)\n\
or\n\
VERDICT: REQUEST_CHANGES\n\
(if you found a valid, concrete issue)\n"
        ),
        Role::Attack => format!(
            "You are ATTACKING this change. Your job is to actively try to REFUTE it: find every \
plausible reason it should be rejected against the criteria below. Do not be charitable -- \
assume the defender is wrong until proven otherwise, and argue the strongest case for rejection \
you can construct.\n\n\
{kg_pointer}\
Criteria to check against:\n{criteria}\n\n\
Context (diff/files/description):\n{context_str}\n\n\
Give your reasoning, then end your response with EXACTLY one line, verbatim:\n\
VERDICT: REFUTED\n\
(if you found a valid, concrete reason to reject the change)\n\
or\n\
VERDICT: NOT_REFUTED\n\
(if the change genuinely withstands your attempt to refute it)\n"
        ),
    };
    format!("{base}{FINDINGS_INSTRUCTION}")
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

/// Cap on findings accepted from one provider reply (bounds a runaway/malicious reply).
const MAX_FINDINGS: usize = 50;

/// KGFIND-02: extract structured findings from a provider's raw reply, if it
/// emitted an optional `FINDINGS_JSON:` block (see [`FINDINGS_INSTRUCTION`]).
/// Thin wrapper over [`parse_findings_with_marker`] pinned to the
/// correctness-review marker -- see that function for the actual
/// tolerant-parse behavior/degrade contract.
pub fn parse_findings(text: &str) -> Vec<Finding> {
    parse_findings_with_marker(text, "FINDINGS_JSON:")
}

/// CXEG-07: like [`parse_findings`], but scans for an arbitrary `marker`
/// instead of the hardcoded `"FINDINGS_JSON:"` -- reused by the Tier-C
/// consistency/elegance lens (`review::consistency`), which asks the
/// reviewer for a DIFFERENT sentinel (`"CONSISTENCY_FINDINGS_JSON:"`) so its
/// advisory-only output can never be confused with (or accidentally
/// double-count against) the correctness lens's own `FINDINGS_JSON:` block
/// in the same or a different reply. Mirrors
/// `scribe::graph::semantic::extract_json_array`'s tolerant
/// brace/bracket-matching approach (first `[` after the marker to its
/// matching `]`), tolerating any prose/fencing around it. Absent marker,
/// unparseable JSON, or a JSON shape that doesn't deserialize into
/// `Vec<Finding>` all resolve to an empty `Vec` -- this function never
/// panics and never errors, since findings are strictly best-effort and must
/// never affect verdict parsing or aggregation. Results are capped at
/// [`MAX_FINDINGS`].
pub fn parse_findings_with_marker(text: &str, marker: &str) -> Vec<Finding> {
    // Anchor the marker to a LINE START (index 0 or immediately after a '\n').
    // A plain `text.find(marker)` cross-matches: the correctness marker
    // `FINDINGS_JSON:` is a suffix-substring of the lens marker
    // `CONSISTENCY_FINDINGS_JSON:`, so an unanchored find would let the
    // correctness parser latch onto a consistency block (and vice-versa).
    let marker_pos = {
        let bytes = text.as_bytes();
        let mut from = 0usize;
        let mut found = None;
        while let Some(rel) = text[from..].find(marker) {
            let abs = from + rel;
            if abs == 0 || bytes[abs - 1] == b'\n' {
                found = Some(abs);
                break;
            }
            from = abs + marker.len();
        }
        found
    };
    let Some(marker_pos) = marker_pos else {
        return Vec::new();
    };
    let after = &text[marker_pos + marker.len()..];

    let Some(start) = after.find('[') else {
        return Vec::new();
    };
    // Depth-match from the first '[' to ITS matching ']', respecting string
    // contents (a ']' inside a description must not terminate the array) and
    // backslash escapes. `rfind(']')` was greedy — trailing prose, a fenced
    // block, or a second FINDINGS_JSON: marker with a later ']' overshot the
    // slice and made deserialization fail. `[`/`]`/`"`/`\` are all ASCII, so
    // byte iteration lands on char boundaries for the slice below.
    let bytes = after.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut end: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let json_slice = &after[start..=end];

    let findings: Vec<Finding> = serde_json::from_str(json_slice).unwrap_or_default();
    findings.into_iter().take(MAX_FINDINGS).collect()
}

// ─── SCRB-02: docs-generation prompt (Scribe) ───────────────────────────────
//
// A second prompt *shape*, alongside `build_prompt`'s review-role framing --
// still dispatched through the exact same `ReviewConfig::dispatch_daemon`
// HTTP call (see `src/scribe/mod.rs`), so no daemon-side change was needed:
// `POST /dispatch` already accepts any opaque `prompt` string (verified by
// reading `src/bin/review_daemon/main.rs`'s `DispatchBody` -- it has no
// prompt-shape/kind field at all, the daemon is prompt-shape-agnostic). The
// original spec's FILES section named `src/bin/review_daemon/provider.rs` as
// the likely home for a `PromptKind::Docs` variant; that assumption doesn't
// match the verified architecture -- prompt *construction* lives here,
// client-side, next to `build_prompt`, and the daemon only ever sees a
// finished string. Kept as a distinct function (not a new `Structure`/`Role`
// variant on `build_prompt`) because a docs-generation prompt needs full
// file contents + existing-doc content, not a diff -- a genuinely different
// shape, not a role variant of review.
/// Build a documentation-generation prompt for one module. `context` is the
/// module's bundled source excerpts (doc comments, public signatures) plus
/// any existing README, serialized as JSON -- see
/// `crate::scribe::inspect::ModuleBundle`. Ends with an explicit instruction
/// to write the README as plain Markdown only, so the daemon's raw text
/// response can be used directly without further extraction (unlike
/// `build_prompt`'s `VERDICT:` sentinel, there is no structured sentinel to
/// parse out of a docs-generation response).
pub fn build_docs_prompt(module_path: &str, git_ref: &str, context: &Value) -> String {
    let context_str = serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string());
    format!(
        "You are a technical documentation writer generating a README for a single \
source module in a Rust codebase.\n\n\
Module path: {module_path}\n\
Git ref this content was generated against: {git_ref}\n\n\
Module context (doc comments, public function/struct/enum signatures, and any \
existing README content, extracted from the real source files):\n{context_str}\n\n\
Write a README for this module: what it does, its public API surface, and any \
configuration (env vars) it reads. Base every claim ONLY on the context above --\
 never invent behavior that isn't evidenced by the doc comments or signatures \
shown. If the existing README content (if any) contradicts what the signatures \
show, prefer the signatures (the code is truth) and note the discrepancy rather \
than silently reconciling it. Respond with ONLY the README's Markdown content -- \
no preamble, no meta-commentary, no code fences wrapping the whole response.\n"
    )
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
        assert_eq!(Structure::parse("epic"), Some(Structure::Epic));
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
    fn intensive_reviewer_prompt_instructs_refutation_first_and_uses_reviewer_sentinel() {
        let ctx = json!({"diff": "+ fn foo() {}"});
        let p = build_prompt(Role::IntensiveReviewer, "must compile", &ctx);
        assert!(p.to_uppercase().contains("REFUTE"), "must instruct refutation attempts");
        assert!(p.contains("must compile"));
        assert!(p.contains("fn foo()"));
        // Same VERDICT sentinel as Role::Reviewer (APPROVE/REQUEST_CHANGES), NOT
        // Attack's REFUTED/NOT_REFUTED -- so this substitute's verdict aggregates
        // into the same panel as any other member without special-casing.
        assert!(p.contains("VERDICT: APPROVE"));
        assert!(p.contains("VERDICT: REQUEST_CHANGES"));
        assert!(!p.contains("VERDICT: REFUTED"));
        assert!(!p.contains("VERDICT: NOT_REFUTED"));
    }

    #[test]
    fn intensive_reviewer_prompt_also_carries_the_findings_instruction() {
        let ctx = json!({});
        let p = build_prompt(Role::IntensiveReviewer, "criteria", &ctx);
        assert!(p.contains("FINDINGS_JSON:"));
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
    fn auditor_prompt_frames_whole_repo_capstone_audit() {
        let ctx = json!({});
        let p = build_prompt(Role::Auditor, "the build contracts", &ctx);
        assert!(p.contains("EPIC REVIEW"), "epic capstone framing");
        assert!(p.to_uppercase().contains("ENTIRE CODEBASE") || p.contains("whole-system"));
        assert!(p.contains("FINDINGS"), "auditor must emit findings");
        assert!(p.contains("VERDICT: APPROVE") && p.contains("VERDICT: REQUEST_CHANGES"));
        // It is an audit, not a fix: never instructs code edits.
        assert!(p.contains("ADVISORY"));
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

    #[test]
    fn docs_prompt_contains_module_path_ref_and_context() {
        let ctx = serde_json::json!({"doc_comments": ["//! a module"], "existing_readme": null});
        let prompt = build_docs_prompt("src/sundry", "abc123", &ctx);
        assert!(prompt.contains("src/sundry"));
        assert!(prompt.contains("abc123"));
        assert!(prompt.contains("a module"));
    }

    #[test]
    fn docs_prompt_instructs_markdown_only_no_fabrication() {
        let ctx = serde_json::json!({});
        let prompt = build_docs_prompt("src/x", "HEAD", &ctx);
        assert!(prompt.to_lowercase().contains("markdown"));
        assert!(prompt.to_lowercase().contains("never invent"));
    }

    #[test]
    fn reviewer_prompt_adds_kg_pointer_only_when_knowledge_graph_present() {
        let ctx_plain = json!({"diff": "+ fn foo() {}"});
        let p_plain = build_prompt(Role::Reviewer, "criteria", &ctx_plain);
        assert!(!p_plain.contains("knowledge_graph` section"), "no pointer without the key");

        let ctx_kg = json!({"diff": "+ fn foo() {}", "knowledge_graph": {"files": []}});
        let p_kg = build_prompt(Role::Reviewer, "criteria", &ctx_kg);
        assert!(p_kg.contains("knowledge_graph` section"), "pointer present when key is set");
        assert!(p_kg.contains("blast radius"));
    }

    #[test]
    fn reviewer_prompt_contains_findings_instruction() {
        let ctx = json!({});
        let p = build_prompt(Role::Reviewer, "criteria", &ctx);
        assert!(p.contains("FINDINGS_JSON:"));
        assert!(p.contains("VERDICT: APPROVE"), "findings instruction must not displace VERDICT sentinel");
    }

    #[test]
    fn parse_findings_extracts_two_findings() {
        let raw = "VERDICT: APPROVE\nFINDINGS_JSON: [\
{\"category\":\"style\",\"severity\":\"low\",\"file\":\"a.rs\",\"symbol\":\"foo\",\"description\":\"nit\"},\
{\"category\":\"bug\",\"severity\":\"high\",\"description\":\"panic risk\"}]";
        let findings = parse_findings(raw);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].category, "style");
        assert_eq!(findings[0].file.as_deref(), Some("a.rs"));
        assert_eq!(findings[1].category, "bug");
        assert_eq!(findings[1].file, None);
    }

    #[test]
    fn parse_findings_empty_when_no_marker() {
        let raw = "VERDICT: APPROVE\nLooks good, no issues.";
        assert!(parse_findings(raw).is_empty());
    }

    #[test]
    fn parse_findings_empty_on_malformed_json_no_panic() {
        let raw = "VERDICT: REQUEST_CHANGES\nFINDINGS_JSON: [ malformed";
        assert!(parse_findings(raw).is_empty());
    }

    #[test]
    fn parse_findings_empty_array_yields_empty_vec() {
        let raw = "VERDICT: APPROVE\nFINDINGS_JSON: []";
        assert!(parse_findings(raw).is_empty());
    }

    #[test]
    fn parse_findings_extracts_array_despite_trailing_prose() {
        let raw = "VERDICT: APPROVE\nFINDINGS_JSON: [{\"category\":\"c\",\"severity\":\"s\",\"description\":\"d\"}]\n\nThanks for reading.";
        let findings = parse_findings(raw);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "d");
    }

    #[test]
    fn parse_findings_ignores_brackets_in_trailing_prose_and_strings() {
        // The bug rfind(']') hit: a ']' AFTER the array (in prose, a fenced
        // block, or a second marker) OR a ']' inside a description string made
        // the greedy slice overshoot and fail to deserialize. Depth-matching
        // from the first '[' to ITS matching ']' fixes both.
        let raw = "VERDICT: REQUEST_CHANGES\n\
FINDINGS_JSON: [{\"category\":\"bug\",\"severity\":\"high\",\"description\":\"index [0] out of bounds\"}]\n\
See also FINDINGS_JSON: [ignore this] and arr[3] in the notes.";
        let findings = parse_findings(raw);
        assert_eq!(findings.len(), 1, "must stop at the first array's matching ]");
        assert_eq!(findings[0].description, "index [0] out of bounds");
    }

    #[test]
    fn parse_findings_caps_at_fifty() {
        let items: Vec<String> = (0..75)
            .map(|i| format!("{{\"category\":\"c\",\"severity\":\"s\",\"description\":\"d{i}\"}}"))
            .collect();
        let raw = format!("VERDICT: APPROVE\nFINDINGS_JSON: [{}]", items.join(","));
        let findings = parse_findings(&raw);
        assert_eq!(findings.len(), 50);
    }

    // ── CXEG-07: parse_findings_with_marker (distinct-marker reuse) ────────

    #[test]
    fn parse_findings_with_marker_uses_a_different_sentinel() {
        let raw = "VERDICT: APPROVE\nCONSISTENCY_FINDINGS_JSON: [{\"category\":\"consistency\",\"severity\":\"low\",\"description\":\"d\"}]";
        // The correctness-review marker must NOT match this text at all.
        assert!(parse_findings(raw).is_empty());
        let findings = parse_findings_with_marker(raw, "CONSISTENCY_FINDINGS_JSON:");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, "consistency");
    }

    #[test]
    fn parse_findings_with_marker_does_not_cross_match_other_markers() {
        // A reply carrying BOTH sentinels must resolve each independently.
        let raw = "VERDICT: APPROVE\n\
FINDINGS_JSON: [{\"category\":\"bug\",\"severity\":\"high\",\"description\":\"x\"}]\n\
CONSISTENCY_FINDINGS_JSON: [{\"category\":\"elegance\",\"severity\":\"low\",\"description\":\"y\"}]";
        let correctness = parse_findings(raw);
        let consistency = parse_findings_with_marker(raw, "CONSISTENCY_FINDINGS_JSON:");
        assert_eq!(correctness.len(), 1);
        assert_eq!(correctness[0].category, "bug");
        assert_eq!(consistency.len(), 1);
        assert_eq!(consistency[0].category, "elegance");
    }

    #[test]
    fn parse_verdict_still_correct_when_findings_json_follows() {
        let raw = "Some reasoning here.\nVERDICT: REQUEST_CHANGES\nFINDINGS_JSON: [{\"category\":\"bug\",\"severity\":\"high\",\"description\":\"x\"}]";
        let (v, reasoning) = parse_verdict(raw);
        assert_eq!(v, Verdict::RequestChanges);
        assert_eq!(reasoning, "Some reasoning here.");
        let findings = parse_findings(raw);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn docs_prompt_has_no_verdict_sentinel_unlike_review_prompts() {
        // Docs generation has no structured sentinel to parse back out --
        // distinguishes this prompt shape from build_prompt's review roles.
        let ctx = serde_json::json!({});
        let prompt = build_docs_prompt("src/x", "HEAD", &ctx);
        assert!(!prompt.contains("VERDICT:"));
    }
}
