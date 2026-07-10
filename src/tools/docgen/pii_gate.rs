//! DOCGEN-02: unconditional, destination-agnostic, pre-inference PII input
//! gate (S95, Plane TERM-144).
//!
//! The doc engine (docgen) builds inference requests from a feat's diff,
//! spec text, and/or touched source. Before ANY such request is built --
//! regardless of whether the request will ultimately route to a local
//! model or a cloud provider (Chord's router picks that later; by the time
//! it does, it's too late to sweep) -- the raw input MUST pass through this
//! gate. This is the load-bearing safety net named in the item title.
//!
//! Policy: REDACT-PREFERRED. A detected PII span is replaced in place with
//! a `[REDACTED:{category}]` placeholder so the doc engine can still write
//! meaningful documentation about the feature ("the tool connects to an
//! internal service" rather than leaking the literal hostname). Content is
//! only BLOCKED outright when redaction cannot preserve enough surrounding
//! meaning to be worth passing on -- see [`sweep_input`]'s doc for the
//! exact rule.
//!
//! Detection reuses the canonical Rust sweep engine
//! ([`crate::github::pii`]) end to end -- [`crate::github::pii::scan_and_redact`]
//! shares the exact same [`crate::github::pii::scan_for_pii`] / [`crate::github::pii::pii_gate`]
//! pattern set (private IPs, `CT###` container ids, internal hostnames,
//! emails, API keys, etc.), including the `// pii-test-fixture` whitelist
//! convention. This module adds NO new detection logic -- only the
//! redact-vs-block decision and sanitized logging on top.

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::github::pii::{scan_and_redact, PiiViolation};

/// A line is BLOCKED (not redacted) when redacting its matches would strip
/// more than this fraction of its non-whitespace characters. Past this
/// point there is essentially no surrounding text left for docs to draw
/// meaning from -- the line IS the secret, not a sentence that happens to
/// contain one (spec EDGE CASE: "Infra detail intrinsic to the meaning ->
/// redact + note the doc will be generic there, don't leak" implies the
/// opposite case -- a line that is ONLY the secret -- must block instead).
const BLOCK_REDACTION_RATIO: f64 = 0.6;

/// Outcome of running the docgen PII input gate on one piece of content
/// (a diff, spec text, or source excerpt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiiGateOutcome {
    /// No PII detected; `content` is returned unchanged and is safe to pass
    /// on to an inference request builder.
    Clean { content: String },
    /// PII was detected and successfully redacted in place. `content` is
    /// the SANITIZED text -- safe to pass on. `redacted_count` and
    /// `categories` are metadata for logging (counts/types only, never the
    /// secret values themselves).
    Redacted {
        content: String,
        redacted_count: usize,
        categories: Vec<String>,
    },
}

impl PiiGateOutcome {
    /// The sanitized content this outcome cleared for onward use. Both
    /// variants carry safe-to-use content -- this is the one accessor an
    /// inference request builder should call; there is deliberately no way
    /// to reach the pre-sweep raw content from an [`PiiGateOutcome`].
    pub fn sanitized_content(&self) -> &str {
        match self {
            PiiGateOutcome::Clean { content } => content,
            PiiGateOutcome::Redacted { content, .. } => content,
        }
    }

    pub fn was_redacted(&self) -> bool {
        matches!(self, PiiGateOutcome::Redacted { .. })
    }
}

/// Where an eventual inference request would route. The gate below is
/// deliberately indifferent to this value (see [`sweep_input_for_routing`])
/// -- it exists only so callers/tests can assert destination-agnosticism
/// explicitly, not because the gate branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDestination {
    Local,
    Cloud,
}

/// Run the unconditional PII input gate on `content`. MUST be called
/// before any inference request referencing `content` is constructed --
/// callers must use [`PiiGateOutcome::sanitized_content`] (never the
/// original `content` argument) to build that request.
///
/// Decision rule, per line:
///   - No PII on the line -> pass through unchanged.
///   - PII on the line, and redacting it leaves at least
///     `1 - BLOCK_REDACTION_RATIO` of the line's non-whitespace characters
///     intact -> redact in place (`[REDACTED:{category}]`), keep going.
///   - PII on the line, and redacting it would strip away most of the
///     line's content (the line IS essentially the secret, e.g. a bare API
///     key or an SSH private key marker with nothing else on it) ->
///     redaction cannot preserve meaning; the ENTIRE input is blocked
///     rather than passed on half-sanitized.
///
/// A line tagged `// pii-test-fixture` is exempt (per the established
/// whitelist convention shared with [`crate::github::pii`]).
pub fn sweep_input(content: &str) -> Result<PiiGateOutcome, ToolError> {
    let (redacted, violations) = scan_and_redact(content);

    if violations.is_empty() {
        tracing::info!(
            target: "docgen.pii_gate",
            outcome = "clean",
            count = 0,
            "docgen input PII gate: clean"
        );
        return Ok(PiiGateOutcome::Clean {
            content: content.to_string(),
        });
    }

    if let Some(unsafe_line) = first_unredactable_line(content, &violations) {
        tracing::warn!(
            target: "docgen.pii_gate",
            outcome = "blocked",
            line = unsafe_line,
            count = violations.len(),
            "docgen input PII gate: blocked -- redaction could not preserve meaning"
        );
        return Err(ToolError::InvalidArgument(format!(
            "BLOCKED: PII on line {unsafe_line} could not be safely redacted (redaction \
would strip nearly all content on that line, leaving no meaning to document). Refusing to \
pass this input to an inference request. {} total PII violation(s) detected.",
            violations.len()
        )));
    }

    let mut categories: Vec<String> = violations.iter().map(|v| v.category.clone()).collect();
    categories.sort();
    categories.dedup();

    tracing::warn!(
        target: "docgen.pii_gate",
        outcome = "redacted",
        count = violations.len(),
        categories = ?categories,
        "docgen input PII gate: redacted before inference request built"
    );

    Ok(PiiGateOutcome::Redacted {
        content: redacted,
        redacted_count: violations.len(),
        categories,
    })
}

/// Same gate as [`sweep_input`], but explicit about the fact that the
/// caller's eventual routing destination is irrelevant to the decision --
/// the input is swept identically whether the resulting inference request
/// will go to a local model or a cloud provider. Chord's router picks the
/// destination AFTER this gate runs; gating post-routing would be too late
/// (the input could already be en route to a cloud API by the time the
/// router decides). There is no `if destination == Cloud { .. }` branch in
/// this module, by design -- `_destination` is accepted only so tests can
/// assert both call sites produce identical results.
pub fn sweep_input_for_routing(
    content: &str,
    _destination: RoutingDestination,
) -> Result<PiiGateOutcome, ToolError> {
    sweep_input(content)
}

/// Whether any violation's line cannot be safely redacted -- i.e.
/// redacting every match on that line would strip more than
/// [`BLOCK_REDACTION_RATIO`] of the line's non-whitespace characters,
/// leaving no surrounding meaning for docs to preserve. Returns the
/// (1-based) line number of the first such line, if any.
fn first_unredactable_line(content: &str, violations: &[PiiViolation]) -> Option<usize> {
    let lines: Vec<&str> = content.lines().collect();
    let mut checked: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

    for v in violations {
        if !checked.insert(v.line) {
            continue;
        }
        let Some(line) = lines.get(v.line.saturating_sub(1)) else {
            continue;
        };
        let non_ws = line.chars().filter(|c| !c.is_whitespace()).count();
        if non_ws == 0 {
            continue;
        }
        // Re-derive the redacted form of just this one line to measure the
        // actual stripped fraction precisely (cheap: single-line input).
        let (redacted_line, _) = scan_and_redact(line);
        let placeholder_markers = redacted_line.matches("[REDACTED:").count();
        if placeholder_markers == 0 {
            continue;
        }
        // Approximate remaining meaningful characters: non-placeholder,
        // non-whitespace characters left in the redacted line.
        let remaining_non_ws = redacted_line
            .chars()
            .filter(|c| !c.is_whitespace())
            .count();
        // Each placeholder token itself contributes non-whitespace chars
        // (`[REDACTED:category]`) that aren't "meaning" -- subtract a
        // rough estimate of that to avoid undercounting the stripped ratio.
        let placeholder_overhead: usize = redacted_line
            .split("[REDACTED:")
            .skip(1)
            .filter_map(|rest| rest.find(']').map(|i| i + 1 + "[REDACTED:".len()))
            .sum();
        let meaningful_remaining = remaining_non_ws.saturating_sub(placeholder_overhead);
        let ratio_stripped = 1.0 - (meaningful_remaining as f64 / non_ws.max(1) as f64);
        if ratio_stripped >= BLOCK_REDACTION_RATIO {
            return Some(v.line);
        }
    }
    None
}

/// Render a [`PiiGateOutcome`] as sanitized JSON for logging / tool
/// responses -- content is included (it is, by construction, already
/// sanitized), counts and categories are included, but nothing about the
/// original raw match values ever appears.
pub fn outcome_to_json(outcome: &PiiGateOutcome) -> Value {
    match outcome {
        PiiGateOutcome::Clean { content } => json!({
            "outcome": "clean",
            "redacted_count": 0,
            "categories": [],
            "content": content,
        }),
        PiiGateOutcome::Redacted {
            content,
            redacted_count,
            categories,
        } => json!({
            "outcome": "redacted",
            "redacted_count": redacted_count,
            "categories": categories,
            "content": content,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for "the inference request builder" -- accepts ONLY
    /// already-swept content, by type (a `&str` that must have come from
    /// `PiiGateOutcome::sanitized_content()`). Used by the negative test
    /// below to prove unsanitized content can never reach it.
    fn build_inference_request(sanitized_content: &str) -> String {
        format!("INFERENCE_REQUEST[{sanitized_content}]")
    }

    #[test]
    fn clean_content_passes_through_unchanged() {
        let input = "This tool posts a summary to the team's issue tracker.";
        let outcome = sweep_input(input).unwrap();
        assert!(!outcome.was_redacted());
        assert_eq!(outcome.sanitized_content(), input);
    }

    /// Negative test (spec TEST PLAN item 1 / ACCEPTANCE CRITERIA item 1):
    /// a diff containing a CT### id / private IP / internal hostname is
    /// redacted BEFORE the inference request is built -- unsanitized
    /// content never reaches the request builder.
    #[test]
    fn diff_with_container_id_ip_and_hostname_is_redacted_before_request_built() {
        let diff = "+ deployed to <host> at <internal-ip>, built on <host>"; // pii-test-fixture
        let outcome = sweep_input(diff).unwrap();
        assert!(outcome.was_redacted());
        let sanitized = outcome.sanitized_content();

        // The unsanitized raw literals must never appear in what's handed
        // to the request builder.
        assert!(!sanitized.contains("<host>")); // pii-test-fixture
        assert!(!sanitized.contains("<internal-ip>")); // pii-test-fixture
        assert!(!sanitized.contains("<host>")); // pii-test-fixture

        let request = build_inference_request(sanitized);
        assert!(!request.contains("<host>")); // pii-test-fixture
        assert!(!request.contains("<internal-ip>")); // pii-test-fixture
        assert!(!request.contains("<host>")); // pii-test-fixture

        // Re-scanning the sanitized content with the canonical scanner
        // must find nothing left.
        assert!(
            crate::github::pii::scan_for_pii(sanitized).is_empty(),
            "sanitized content must be clean per the canonical scanner: {sanitized:?}"
        );
    }

    /// Negative test (spec TEST PLAN item 2 / ACCEPTANCE CRITERIA item 2):
    /// content that can't be safely redacted is BLOCKED, not passed onward
    /// -- e.g. a line that is essentially nothing but a bare API key, where
    /// redacting it would leave no surrounding meaning for docs to keep.
    #[test]
    fn unredactable_content_is_blocked_not_passed() {
        let bare_secret = "<REDACTED-SECRET>"; // pii-test-fixture
        let result = sweep_input(bare_secret);
        assert!(result.is_err(), "a line that IS the secret must be blocked: {result:?}");
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("BLOCKED"));
        // The raw secret must never leak into the error message either.
        assert!(!msg.contains("XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"));
    }

    /// Negative test: a mostly-meaningful sentence that merely mentions an
    /// infra detail is redacted, not blocked -- confirms the block path is
    /// reserved for genuinely unredactable content, not triggered on every
    /// hit.
    #[test]
    fn sentence_containing_infra_detail_is_redacted_not_blocked() {
        let text = "The scheduler service on <host> polls every 30 seconds and posts results back to the internal dashboard for review."; // pii-test-fixture
        let outcome = sweep_input(text).unwrap();
        assert!(outcome.was_redacted(), "expected redaction, got: {outcome:?}");
        assert!(!outcome.sanitized_content().contains("<host>")); // pii-test-fixture
        assert!(outcome.sanitized_content().contains("scheduler service"));
    }

    /// Spec TEST PLAN item 3 / ACCEPTANCE CRITERIA item 3: the gate runs
    /// identically for both local and cloud routing -- destination-
    /// agnostic, gates the INPUT before Chord's router even sees it.
    #[test]
    fn gate_runs_identically_for_local_and_cloud_routing() {
        let diff = "+ connects to <internal-ip> for status"; // pii-test-fixture
        let local = sweep_input_for_routing(diff, RoutingDestination::Local).unwrap();
        let cloud = sweep_input_for_routing(diff, RoutingDestination::Cloud).unwrap();
        assert_eq!(local, cloud, "gate must be destination-agnostic");
        assert!(!local.sanitized_content().contains("<internal-ip>")); // pii-test-fixture
    }

    /// Spec TEST PLAN item 4 / ACCEPTANCE CRITERIA item 4: redaction is
    /// logged as sanitized metadata (count/categories), never the secret
    /// value. This test checks the outcome's own public surface (what a
    /// caller would log) carries no infra literal.
    #[test]
    fn redaction_metadata_is_sanitized_no_infra_literal() {
        let diff = "+ <host> was retired, traffic moved to <internal-ip>"; // pii-test-fixture
        let outcome = sweep_input(diff).unwrap();
        let PiiGateOutcome::Redacted {
            redacted_count,
            categories,
            ..
        } = &outcome
        else {
            panic!("expected Redacted outcome: {outcome:?}");
        };
        assert!(*redacted_count >= 2);
        assert!(categories.contains(&"container_id".to_string()));
        assert!(categories.contains(&"private_ip".to_string()));
        for c in categories {
            assert!(!c.contains("192.168")); // pii-test-fixture
            assert!(!c.contains("<host>")); // pii-test-fixture
        }

        let json = outcome_to_json(&outcome);
        let json_str = json.to_string();
        assert!(!json_str.contains("<host>")); // pii-test-fixture
        assert!(!json_str.contains("<internal-ip>")); // pii-test-fixture
    }

    /// EDGE CASE: `// pii-test-fixture`-tagged content respects the
    /// established whitelist convention -- it is not swept/redacted.
    #[test]
    fn pii_test_fixture_tagged_line_is_whitelisted() {
        let tagged = "server at <internal-ip> // pii-test-fixture";
        let outcome = sweep_input(tagged).unwrap();
        assert!(!outcome.was_redacted());
        assert_eq!(outcome.sanitized_content(), tagged);
    }

    /// EDGE CASE: a huge diff is swept completely, not sampled -- a missed
    /// chunk would be a leak. Build a large multi-line diff with a single
    /// planted violation near the end and confirm it's still caught.
    #[test]
    fn large_diff_is_swept_completely_not_sampled() {
        let mut diff = String::new();
        for i in 0..5000 {
            diff.push_str(&format!("line {i}: nothing interesting here\n"));
        }
        diff.push_str("+ oops leaked <internal-ip> near the end\n"); // pii-test-fixture
        let outcome = sweep_input(&diff).unwrap();
        assert!(outcome.was_redacted());
        assert!(!outcome.sanitized_content().contains("<internal-ip>")); // pii-test-fixture
    }

    /// EDGE CASE: multiple independent violations on the same line are all
    /// redacted, and the surrounding sentence still reads (not blocked).
    #[test]
    fn multiple_violations_same_line_all_redacted() {
        let text =
            "Deployed to <host> (<internal-ip>) and <host> (<internal-ip>) this morning."; // pii-test-fixture
        let outcome = sweep_input(text).unwrap();
        assert!(outcome.was_redacted());
        let sanitized = outcome.sanitized_content();
        assert!(!sanitized.contains("<host>")); // pii-test-fixture
        assert!(!sanitized.contains("<host>")); // pii-test-fixture
        assert!(!sanitized.contains("<internal-ip>")); // pii-test-fixture
        assert!(!sanitized.contains("<internal-ip>")); // pii-test-fixture
        assert!(sanitized.contains("Deployed to"));
    }
}
