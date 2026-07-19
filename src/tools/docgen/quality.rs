//! DOCGEN-18: doc-quality scoring + prose-lint gate (S95, Plane TERM-169).
//!
//! Two-layer quality gate for a generated doc artifact, per the S95
//! research report (`RESEARCH-10-improvements.md` section 8) and the
//! Vale + LLM-as-judge landscape comparison:
//!
//! 1. **Deterministic prose lint** ([`lint_prose`]) -- a small, configurable,
//!    dependency-free linter (banned words, max sentence length, a
//!    passive-voice heuristic). This is the shipped path: `vale` (the
//!    external syntax-aware prose linter referenced in the research report)
//!    is NOT assumed to be installed on the build/serving hosts. When `vale`
//!    IS present on a host, an operator MAY additionally run it out-of-band
//!    against rendered artifacts -- this module does not shell out to it,
//!    but nothing here precludes layering it on top.
//! 2. **LLM-as-judge rubric** ([`judge_doc_quality`]) -- faithfulness (vs.
//!    the diff the doc was generated from), completeness, and coherence,
//!    scored by asking a model through the EXISTING [`super::generate::DocGenerator`]
//!    seam (DOCGEN-05's Chord router hookup) -- this module adds no second
//!    inference client. Mocked in tests via the same `DocGenerator` trait
//!    object other docgen modules mock.
//!
//! ## Paired, never a single fallible model
//! The research report is explicit that an LLM-judge has blind spots and
//! must be paired with a deterministic gate, not substituted for one.
//! [`run_quality_gate`] enforces that pairing structurally: the
//! deterministic lint ALWAYS runs and can fail the gate on its own; the
//! judge is best-effort (a missing generator, no diff context, or a judge
//! call failure all degrade to "judge unavailable" -- the combined score
//! then falls back to the lint score alone) and can never be the sole
//! reason an artifact passes when the lint itself found an error-level
//! issue. See `judge_unavailable_lint_alone_still_gates` below.
//!
//! ## Storage: quality score paired with DOCGEN-07 version metadata
//! [`QualityScoreStore`] keys scores by the SAME [`super::versioning::ArtifactKey`]
//! plus version number DOCGEN-07's `VersionStore` uses for artifact
//! history, so a stored score is unambiguously "the score for this
//! artifact's version N" -- the pairing the spec calls for -- without this
//! module reaching into or mutating `VersionStore`'s own history (that
//! store's docs are explicit that it is the engine's own append-only
//! record; this module treats it as a peer, not something to extend
//! in-place).
//!
//! ## No literals, no direct secret env reads
//! This module never reads a secret VALUE itself -- inference auth is
//! entirely the `DocGenerator` implementation's concern (see
//! `generate.rs`'s `ChordDocGenerator`, which already routes through
//! `crate::federation::mint_service_jwt`). There is no `std::env::var` call
//! anywhere in this file and no hardcoded URL/host/org literal.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::error::ToolError;

use super::diagram::is_generic_placeholder;
use super::generate::{all_symbol_names, DocGenerator};
use super::prompts::{anti_latch_lint, symbol_existence_lint, RepoIdentity};
use super::repo_facts::RepoFacts;
use super::versioning::ArtifactKey;

// ---------------------------------------------------------------------------
// DGRICH-09: repo-level landing gates (fail-closed, wired into place.rs's
// `place_repo_docs` -- see its doc comment)
// ---------------------------------------------------------------------------
//
// DGRICH-07 shipped these two checks (substance floor + generic-diagram
// lint) as a STOPGAP running inline inside `trigger::run_repo_level_trigger`
// before calling `place_docs`, with a note that DGRICH-09 was the item that
// would fold them into the placement door's own fail-closed set "for EVERY
// caller." This section is that consolidation: the checks themselves are
// unchanged (this module doesn't reimplement `check_landing_substance` --
// that stays in `readme_layers`, called directly by `place::place_repo_docs`
// alongside these two), but the generic-diagram check moves here as a
// LANDING-TEXT-level gate (extracting the embedded mermaid fence rather than
// requiring a separate `RepoFacts`/diagram-source argument threaded through
// `place_docs`), and the identity lint -- Pass 1's own anti-latch +
// symbol-existence lints, already enforced once during generation
// (`generate::run_identity_pass`) -- gets a second, independent enforcement
// point at the placement door itself: the DSN-guard lesson (a lone
// enforcement point is a single point of failure) applies here exactly as
// it does to every other fail-closed gate in this engine.

/// Extract the inner source of the FIRST ` ```mermaid ` fenced code block in
/// `landing`, if any. A landing with no mermaid fence at all (e.g. the
/// `trigger::minimal_landing` degraded fallback, which never claims a
/// diagram it doesn't have) has nothing for [`check_landing_diagram`] to
/// gate -- this is `None`, not an error.
fn extract_mermaid_fence(landing: &str) -> Option<&str> {
    const OPEN: &str = "```mermaid\n";
    let start = landing.find(OPEN)? + OPEN.len();
    let rest = &landing[start..];
    let end = rest.find("\n```")?;
    Some(&rest[..end])
}

/// DGRICH-04/DGRICH-09 fail-closed gate: a landing whose embedded
/// architecture diagram is the generic `Client -> Core -> Output` template,
/// or has fewer than 5 real subsystem nodes ([`is_generic_placeholder`]),
/// must never ship. A landing with no embedded mermaid fence at all passes
/// trivially -- there is nothing generic to catch (see
/// [`extract_mermaid_fence`]'s doc comment).
pub fn check_landing_diagram(landing: &str) -> Result<(), String> {
    match extract_mermaid_fence(landing) {
        None => Ok(()),
        Some(source) if is_generic_placeholder(source) => Err(
            "architecture diagram lint (DGRICH-04 is_generic_placeholder): the landing's \
embedded diagram is the generic Client/Core/Output template, or has fewer than 5 real \
subsystem nodes -- withholding the cutover rather than shipping a latch-prone landing"
                .to_string(),
        ),
        Some(_) => Ok(()),
    }
}

/// DGRICH-02/DGRICH-09 fail-closed backstop: re-run the SAME anti-latch and
/// symbol-existence lints [`super::generate::run_identity_pass`] already
/// enforces (with a retry) before generation, one more time against the
/// FINAL `identity` + `facts` right at the placement door. This should be
/// unreachable in ordinary operation -- Pass 1 already gated `identity` --
/// but a future wiring change that skips or bypasses that gate can then
/// never ship an invented-symbol or single-subsystem-latched identity past
/// this door either, matching this engine's existing "never a single
/// fallible enforcement point" posture (see [`run_quality_gate`]'s own doc
/// comment for the same principle applied to the prose/judge pairing).
pub fn check_landing_identity(identity: &RepoIdentity, facts: &RepoFacts) -> Result<(), String> {
    let subsystem_names: Vec<String> = facts.subsystems.iter().map(|s| s.name.clone()).collect();
    if let Some(violation) =
        anti_latch_lint(&identity.tagline, &identity.what_is, &subsystem_names, "")
    {
        return Err(format!("identity anti-latch lint (DGRICH-02 backstop): {violation}"));
    }

    let feature_text: String =
        identity.feature_rows.iter().map(|f| f.description.as_str()).collect::<Vec<_>>().join(" ");
    let combined = format!("{} {} {}", identity.tagline, identity.what_is, feature_text);
    let symbol_names = all_symbol_names(facts);
    if let Some(violation) = symbol_existence_lint(&combined, &symbol_names) {
        return Err(format!("identity symbol-existence lint (DGRICH-02 backstop): {violation}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Deterministic prose linter
// ---------------------------------------------------------------------------

/// Configuration for [`lint_prose`]. All three rules are independently
/// tunable so a caller (or a future config-file-driven wiring) can adjust
/// house style without touching this module's code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProseLintConfig {
    /// Case-insensitive, whole-word banned terms (house-style / hedge
    /// words). An occurrence of any of these is an ERROR-level issue.
    pub banned_words: Vec<String>,
    /// A sentence with more than this many whitespace-separated words is a
    /// WARNING-level issue.
    pub max_sentence_words: usize,
    /// Whether the passive-voice heuristic (auxiliary verb immediately
    /// followed by a past-participle-shaped word) runs at all.
    pub passive_voice_check: bool,
}

impl Default for ProseLintConfig {
    fn default() -> Self {
        Self {
            banned_words: vec![
                "obviously".to_string(),
                "simply".to_string(),
                "just".to_string(),
                "easily".to_string(),
                "trivially".to_string(),
            ],
            max_sentence_words: 40,
            passive_voice_check: true,
        }
    }
}

/// How serious a [`LintIssue`] is. Only [`LintSeverity::Error`] issues fail
/// the deterministic layer on their own (see [`LintResult::is_clean`]);
/// [`LintSeverity::Warning`] issues still count against [`LintResult::score`]
/// but do not, alone, block an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Error,
    Warning,
}

/// One finding from [`lint_prose`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    /// Stable rule identifier, e.g. `"banned-word"`, `"max-sentence-length"`,
    /// `"passive-voice"`.
    pub rule: String,
    pub message: String,
    pub severity: LintSeverity,
    /// A short excerpt of the offending text, for surfacing to a reviewer.
    pub excerpt: String,
}

/// The full result of running [`lint_prose`] over one artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintResult {
    pub issues: Vec<LintIssue>,
}

impl LintResult {
    /// True iff there are no [`LintSeverity::Error`]-level issues. Warnings
    /// alone do not block -- they still reduce [`Self::score`].
    pub fn is_clean(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == LintSeverity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == LintSeverity::Error).count()
    }

    /// A crude 0.0-1.0 density-based score: each issue (of either severity)
    /// costs a fixed penalty, floored at 0.0. Deliberately simple --
    /// good enough to combine with the judge's rubric scores in
    /// [`run_quality_gate`], not a publishable metric on its own.
    pub fn score(&self) -> f32 {
        let penalty = self.issues.len() as f32 * 0.08;
        (1.0 - penalty).clamp(0.0, 1.0)
    }
}

/// Run the deterministic prose linter over `text`. Always available, never
/// calls out to a model or the network -- this is the layer that runs
/// "even if the judge is unavailable" (spec requirement).
pub fn lint_prose(text: &str, config: &ProseLintConfig) -> LintResult {
    let mut issues = Vec::new();

    check_banned_words(text, config, &mut issues);

    for sentence in split_sentences(text) {
        let words: Vec<&str> = sentence.split_whitespace().collect();

        if words.len() > config.max_sentence_words {
            issues.push(LintIssue {
                rule: "max-sentence-length".to_string(),
                message: format!(
                    "sentence has {} words (max {})",
                    words.len(),
                    config.max_sentence_words
                ),
                severity: LintSeverity::Warning,
                excerpt: truncate(sentence, 80),
            });
        }

        if config.passive_voice_check && is_passive_ish(&words) {
            issues.push(LintIssue {
                rule: "passive-voice".to_string(),
                message: "possible passive-voice construction (auxiliary verb + past participle)"
                    .to_string(),
                severity: LintSeverity::Warning,
                excerpt: truncate(sentence, 80),
            });
        }
    }

    LintResult { issues }
}

fn check_banned_words(text: &str, config: &ProseLintConfig, issues: &mut Vec<LintIssue>) {
    if config.banned_words.is_empty() {
        return;
    }
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();

    for banned in &config.banned_words {
        let needle = banned.to_lowercase();
        if tokens.contains(&needle) {
            issues.push(LintIssue {
                rule: "banned-word".to_string(),
                message: format!("banned word '{banned}' found -- rewrite without hedge language"),
                severity: LintSeverity::Error,
                excerpt: banned.clone(),
            });
        }
    }
}

/// Split into naive sentences on `.`/`!`/`?`. Good enough for a length/
/// passive-voice heuristic; not a full sentence tokenizer.
fn split_sentences(text: &str) -> Vec<&str> {
    text.split(['.', '!', '?']).map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// A deliberately simple heuristic: an auxiliary verb ("is"/"was"/"were"/
/// "are"/"been"/"be"/"being") immediately followed by a word ending in
/// "ed" (the common past-participle shape) is flagged as *possible*
/// passive voice. This will both miss real passive constructions and
/// occasionally flag legitimate active prose ("was rated" as a noun
/// phrase, etc.) -- it is a lightweight signal for a human/reviewer to
/// weigh, not a grammatical proof, consistent with "a few configurable
/// rules" scope for the shipped built-in linter.
fn is_passive_ish(words: &[&str]) -> bool {
    const AUX: &[&str] = &["is", "was", "were", "are", "been", "be", "being"];
    for pair in words.windows(2) {
        let w0 = pair[0].trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        let w1 = pair[1].trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if AUX.contains(&w0.as_str()) && w1.len() > 3 && w1.ends_with("ed") {
            return true;
        }
    }
    false
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

// ---------------------------------------------------------------------------
// LLM-as-judge rubric
// ---------------------------------------------------------------------------

/// The three rubric axes an LLM judge scores, each clamped to `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JudgeScores {
    /// Does the doc accurately reflect the diff it was generated from, with
    /// no fabricated claims?
    pub faithfulness: f32,
    /// Does the doc cover what the diff actually changed?
    pub completeness: f32,
    /// Is the doc well-organized and readable?
    pub coherence: f32,
}

impl JudgeScores {
    pub fn overall(&self) -> f32 {
        (self.faithfulness + self.completeness + self.coherence) / 3.0
    }
}

/// Ask `generator` (the existing DOCGEN-05 [`DocGenerator`] Chord seam -- no
/// second inference client here) to judge `doc_content` against
/// `diff_context` on the faithfulness/completeness/coherence rubric.
///
/// Returns `Err` on any transport failure or an unparsable response --
/// callers (see [`run_quality_gate`]) treat that as "judge unavailable" and
/// fall back to the deterministic lint alone, per the research report's
/// warning that an LLM-judge has blind spots and must never be the single
/// gate.
pub async fn judge_doc_quality(
    generator: &dyn DocGenerator,
    diff_context: &str,
    doc_content: &str,
) -> Result<JudgeScores, ToolError> {
    let prompt = build_judge_prompt(diff_context, doc_content);
    let raw = generator.generate(&prompt).await?;
    parse_judge_response(&raw)
}

fn build_judge_prompt(diff_context: &str, doc_content: &str) -> String {
    format!(
        "You are a documentation quality judge. Score the DOC below against the DIFF it \
was generated from, on three axes, each a float from 0.0 to 1.0:\n\
- faithfulness: does the doc accurately reflect the diff, with no fabricated claims?\n\
- completeness: does the doc cover what the diff actually changed?\n\
- coherence: is the doc well-organized and readable?\n\n\
Respond with ONLY a single JSON object, no other text: \
{{\"faithfulness\": <float>, \"completeness\": <float>, \"coherence\": <float>}}\n\n\
DIFF:\n{diff_context}\n\nDOC:\n{doc_content}"
    )
}

/// Parse a judge response's JSON object out of `raw` (models sometimes wrap
/// JSON in prose or a code fence despite the prompt asking for JSON only,
/// so this looks for the first `{`..last `}` span rather than requiring
/// the whole response to be bare JSON).
fn parse_judge_response(raw: &str) -> Result<JudgeScores, ToolError> {
    let start = raw
        .find('{')
        .ok_or_else(|| ToolError::Execution("docgen-quality: judge response contained no JSON object".to_string()))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| ToolError::Execution("docgen-quality: judge response contained no JSON object".to_string()))?;
    if end < start {
        return Err(ToolError::Execution(
            "docgen-quality: judge response had malformed JSON delimiters".to_string(),
        ));
    }

    let slice = &raw[start..=end];
    let parsed: Value = serde_json::from_str(slice).map_err(|e| {
        ToolError::Execution(format!("docgen-quality: could not parse judge JSON: {e}"))
    })?;

    let field = |name: &str| -> Result<f32, ToolError> {
        parsed
            .get(name)
            .and_then(Value::as_f64)
            .map(|f| f as f32)
            .ok_or_else(|| {
                ToolError::Execution(format!("docgen-quality: judge response missing/invalid '{name}'"))
            })
    };

    Ok(JudgeScores {
        faithfulness: field("faithfulness")?.clamp(0.0, 1.0),
        completeness: field("completeness")?.clamp(0.0, 1.0),
        coherence: field("coherence")?.clamp(0.0, 1.0),
    })
}

// ---------------------------------------------------------------------------
// Combined quality gate
// ---------------------------------------------------------------------------

/// Default combined-score threshold below which an artifact is
/// [`QualityVerdict::Failed`]. Callers may pass their own threshold to
/// [`run_quality_gate`] instead.
pub const DEFAULT_QUALITY_THRESHOLD: f32 = 0.7;

#[derive(Debug, Clone, PartialEq)]
pub enum QualityVerdict {
    Passed,
    Failed { reason: String },
}

/// The full result of [`run_quality_gate`] for one artifact: both layers'
/// raw findings plus the combined verdict. This is the value
/// [`QualityScoreStore`] persists, paired with DOCGEN-07's `ArtifactKey` +
/// version.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityScore {
    pub lint: LintResult,
    pub lint_score: f32,
    /// `None` when the judge did not run or was unavailable -- see the
    /// module doc comment's "paired, never a single fallible model" note.
    pub judge: Option<JudgeScores>,
    pub combined_score: f32,
    pub verdict: QualityVerdict,
}

impl QualityScore {
    /// Whether this artifact is considered publishable -- the one bit the
    /// spec says the quality score gates (never WHERE it's placed; this
    /// module returns the score, it does not place anything).
    pub fn is_publishable(&self) -> bool {
        matches!(self.verdict, QualityVerdict::Passed)
    }
}

/// Run the two-layer quality gate over `doc_content`.
///
/// - The deterministic lint (`lint_config`) ALWAYS runs, regardless of
///   whether a judge is available.
/// - The LLM-judge runs only when both `generator` and `diff_context` are
///   supplied; a missing generator, missing diff context, or a judge-call
///   failure all degrade to `judge: None` -- never an error returned to the
///   caller, since a quality *score* should always be produced even when
///   the optional judge layer can't run.
/// - An artifact with any ERROR-level lint issue is `Failed` outright,
///   regardless of the judge's opinion (this is the "not a single fallible
///   model" guarantee: the judge alone can never rescue a lint failure).
/// - Otherwise, `combined_score` is the mean of the lint score and the
///   judge's overall score when a judge ran, or the lint score alone when
///   it did not; the verdict is `Failed` when that combined score is below
///   `threshold`.
pub async fn run_quality_gate(
    doc_content: &str,
    diff_context: Option<&str>,
    generator: Option<&dyn DocGenerator>,
    lint_config: &ProseLintConfig,
    threshold: f32,
) -> QualityScore {
    let lint = lint_prose(doc_content, lint_config);
    let lint_score = lint.score();

    let judge = match (generator, diff_context) {
        (Some(gen), Some(diff)) => judge_doc_quality(gen, diff, doc_content).await.ok(),
        _ => None,
    };

    let combined_score = match &judge {
        Some(j) => (lint_score + j.overall()) / 2.0,
        None => lint_score,
    };

    let verdict = if !lint.is_clean() {
        QualityVerdict::Failed {
            reason: format!(
                "deterministic prose lint found {} error-level issue(s)",
                lint.error_count()
            ),
        }
    } else if combined_score < threshold {
        QualityVerdict::Failed {
            reason: format!(
                "combined quality score {combined_score:.2} below threshold {threshold:.2}"
            ),
        }
    } else {
        QualityVerdict::Passed
    };

    QualityScore { lint, lint_score, judge, combined_score, verdict }
}

// ---------------------------------------------------------------------------
// Quality score storage (paired with DOCGEN-07 version metadata)
// ---------------------------------------------------------------------------

/// Stores [`QualityScore`]s keyed by the SAME `(ArtifactKey, version)` pair
/// DOCGEN-07's `VersionStore` uses to key `ArtifactVersion`s -- pairing a
/// score with "this artifact's version N" without mutating or extending
/// `VersionStore` itself. In-process, `Mutex`-guarded, mirroring
/// `VersionStore`'s own concurrency posture. Never overwrites silently:
/// [`Self::record`] replaces any prior score for the same key/version,
/// which is correct here (re-scoring the same version, e.g. after a
/// threshold change) unlike `VersionStore`'s append-only artifact history.
#[derive(Default)]
pub struct QualityScoreStore {
    inner: Mutex<BTreeMap<(ArtifactKey, u64), QualityScore>>,
}

impl QualityScoreStore {
    pub fn new() -> Self {
        Self { inner: Mutex::new(BTreeMap::new()) }
    }

    pub fn record(&self, key: ArtifactKey, version: u64, score: QualityScore) {
        let mut guard = self.inner.lock().expect("QualityScoreStore mutex poisoned");
        guard.insert((key, version), score);
    }

    pub fn get(&self, key: &ArtifactKey, version: u64) -> Option<QualityScore> {
        let guard = self.inner.lock().expect("QualityScoreStore mutex poisoned");
        guard.get(&(key.clone(), version)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    // ── Deterministic prose lint ─────────────────────────────────────

    /// Acceptance criterion: the deterministic lint catches a banned word.
    #[test]
    fn lint_catches_banned_word() {
        let config = ProseLintConfig::default();
        let result = lint_prose("This is obviously the correct approach.", &config);
        assert!(!result.is_clean());
        assert!(result.issues.iter().any(|i| i.rule == "banned-word"));
        assert_eq!(result.error_count(), 1);
    }

    /// Acceptance criterion: the deterministic lint catches an over-long
    /// sentence.
    #[test]
    fn lint_catches_over_long_sentence() {
        let config = ProseLintConfig { max_sentence_words: 5, ..ProseLintConfig::default() };
        let result = lint_prose("This sentence has way more than five words in it total.", &config);
        assert!(result.issues.iter().any(|i| i.rule == "max-sentence-length"));
        // A warning-level issue alone must not mark the lint unclean.
        assert!(result.is_clean());
    }

    #[test]
    fn lint_flags_possible_passive_voice() {
        let config = ProseLintConfig::default();
        let result = lint_prose("The request was rejected by the server.", &config);
        assert!(result.issues.iter().any(|i| i.rule == "passive-voice"));
    }

    #[test]
    fn lint_disables_passive_voice_check_when_configured_off() {
        let config = ProseLintConfig { passive_voice_check: false, ..ProseLintConfig::default() };
        let result = lint_prose("The request was rejected by the server.", &config);
        assert!(!result.issues.iter().any(|i| i.rule == "passive-voice"));
    }

    /// Negative test: clean prose produces no issues at all.
    #[test]
    fn lint_clean_prose_has_no_issues() {
        let config = ProseLintConfig::default();
        let result = lint_prose("The server rejected the request. It logged the reason.", &config);
        assert!(result.issues.is_empty());
        assert!(result.is_clean());
        assert_eq!(result.score(), 1.0);
    }

    #[test]
    fn lint_banned_words_are_case_insensitive_whole_word() {
        let config = ProseLintConfig::default();
        // "Simply" (capitalized) should still match; "simplyfied" (not a
        // whole-word match) should NOT match "simply".
        let result = lint_prose("Simplyfied prose has no banned word here.", &config);
        assert!(!result.issues.iter().any(|i| i.rule == "banned-word"));

        let result2 = lint_prose("Simply put, this works.", &config);
        assert!(result2.issues.iter().any(|i| i.rule == "banned-word"));
    }

    #[test]
    fn lint_score_decreases_with_issue_count() {
        let config = ProseLintConfig::default();
        let clean = lint_prose("The server rejected the request.", &config);
        let dirty = lint_prose(
            "This is obviously simply just an easily trivially bad sentence honestly.",
            &config,
        );
        assert!(dirty.score() < clean.score());
    }

    // ── LLM-as-judge rubric (mocked) ──────────────────────────────────

    struct MockJudgeGenerator {
        response: String,
    }

    #[async_trait]
    impl DocGenerator for MockJudgeGenerator {
        async fn generate(&self, _prompt: &str) -> Result<String, ToolError> {
            Ok(self.response.clone())
        }
    }

    struct FailingGenerator;

    #[async_trait]
    impl DocGenerator for FailingGenerator {
        async fn generate(&self, _prompt: &str) -> Result<String, ToolError> {
            Err(ToolError::Http("chord unreachable".to_string()))
        }
    }

    /// Acceptance criterion: the LLM-judge (mocked) scores faithfulness/
    /// completeness/coherence.
    #[tokio::test]
    async fn judge_parses_scores_from_mocked_response() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 0.9, "completeness": 0.8, "coherence": 0.95}"#.to_string(),
        };
        let scores = judge_doc_quality(&gen, "+ added widget factory", "# Widget\n\nBuilds widgets.")
            .await
            .unwrap();
        assert_eq!(scores.faithfulness, 0.9);
        assert_eq!(scores.completeness, 0.8);
        assert_eq!(scores.coherence, 0.95);
        assert!((scores.overall() - 0.8833333).abs() < 0.001);
    }

    /// Judge responses are sometimes wrapped in prose/code fences -- the
    /// parser must still find the JSON object.
    #[tokio::test]
    async fn judge_parses_scores_wrapped_in_prose() {
        let gen = MockJudgeGenerator {
            response: "Here is my assessment:\n```json\n{\"faithfulness\": 0.5, \"completeness\": 0.5, \"coherence\": 0.5}\n```\nHope that helps!".to_string(),
        };
        let scores = judge_doc_quality(&gen, "diff", "doc").await.unwrap();
        assert_eq!(scores.faithfulness, 0.5);
    }

    /// Negative test: an unparsable judge response is an error, not a
    /// silently fabricated score.
    #[tokio::test]
    async fn judge_unparsable_response_is_error() {
        let gen = MockJudgeGenerator { response: "not json at all".to_string() };
        let result = judge_doc_quality(&gen, "diff", "doc").await;
        assert!(result.is_err());
    }

    /// Negative test: out-of-range scores are clamped, not rejected or
    /// left to silently violate the 0.0-1.0 contract.
    #[tokio::test]
    async fn judge_clamps_out_of_range_scores() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 1.5, "completeness": -0.2, "coherence": 0.5}"#.to_string(),
        };
        let scores = judge_doc_quality(&gen, "diff", "doc").await.unwrap();
        assert_eq!(scores.faithfulness, 1.0);
        assert_eq!(scores.completeness, 0.0);
    }

    /// Negative test: generator/transport failure propagates as an error,
    /// not a fabricated judge score.
    #[tokio::test]
    async fn judge_generator_failure_propagates_as_error() {
        let result = judge_doc_quality(&FailingGenerator, "diff", "doc").await;
        assert!(result.is_err());
    }

    // ── Combined gate: pairing + threshold ────────────────────────────

    /// Acceptance criterion: a below-threshold artifact is failed/flagged.
    #[tokio::test]
    async fn below_threshold_combined_score_fails_gate() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 0.2, "completeness": 0.2, "coherence": 0.2}"#.to_string(),
        };
        let config = ProseLintConfig::default();
        let score = run_quality_gate(
            "The server rejected the request.",
            Some("+ trivial change"),
            Some(&gen),
            &config,
            DEFAULT_QUALITY_THRESHOLD,
        )
        .await;
        assert!(!score.is_publishable());
        assert!(matches!(score.verdict, QualityVerdict::Failed { .. }));
    }

    /// Acceptance criterion: a high-scoring artifact with clean lint passes.
    #[tokio::test]
    async fn above_threshold_clean_artifact_passes_gate() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 0.95, "completeness": 0.9, "coherence": 0.9}"#.to_string(),
        };
        let config = ProseLintConfig::default();
        let score = run_quality_gate(
            "The server rejected the request. It logged the reason.",
            Some("+ added rejection logging"),
            Some(&gen),
            &config,
            DEFAULT_QUALITY_THRESHOLD,
        )
        .await;
        assert!(score.is_publishable());
        assert_eq!(score.verdict, QualityVerdict::Passed);
    }

    /// Core "not a single fallible model" guarantee: an ERROR-level lint
    /// issue fails the gate even when the judge scores the artifact
    /// perfectly.
    #[tokio::test]
    async fn lint_error_fails_gate_even_with_perfect_judge_scores() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 1.0, "completeness": 1.0, "coherence": 1.0}"#.to_string(),
        };
        let config = ProseLintConfig::default();
        let score = run_quality_gate(
            "This is obviously correct and needs no further review at all.",
            Some("+ trivial change"),
            Some(&gen),
            &config,
            DEFAULT_QUALITY_THRESHOLD,
        )
        .await;
        assert!(!score.is_publishable());
        assert!(score.judge.is_some(), "judge should still have run and be reported");
    }

    /// Acceptance criterion: deterministic checks run even if the judge is
    /// unavailable -- not a single-model gate. Here the generator fails
    /// entirely (simulating Chord being unreachable); the lint alone must
    /// still produce a verdict.
    #[tokio::test]
    async fn judge_unavailable_lint_alone_still_gates() {
        let config = ProseLintConfig::default();
        let score = run_quality_gate(
            "The server rejected the request. It logged the reason.",
            Some("+ trivial change"),
            Some(&FailingGenerator),
            &config,
            DEFAULT_QUALITY_THRESHOLD,
        )
        .await;
        assert!(score.judge.is_none(), "failed judge call must degrade to None, not propagate");
        // Lint alone is clean and its score is 1.0, so the gate still
        // passes purely on the deterministic layer.
        assert!(score.is_publishable());
        assert_eq!(score.combined_score, score.lint_score);
    }

    /// No generator supplied at all (e.g. offline mode) -- same "lint
    /// alone still gates" guarantee.
    #[tokio::test]
    async fn no_generator_supplied_lint_alone_still_gates() {
        let config = ProseLintConfig::default();
        let score =
            run_quality_gate("The server rejected the request.", None, None, &config, DEFAULT_QUALITY_THRESHOLD)
                .await;
        assert!(score.judge.is_none());
        assert!(score.is_publishable());
    }

    /// No diff context supplied (generator present but nothing to judge
    /// against) -- also degrades to lint-alone rather than erroring.
    #[tokio::test]
    async fn no_diff_context_lint_alone_still_gates() {
        let gen = MockJudgeGenerator {
            response: r#"{"faithfulness": 0.9, "completeness": 0.9, "coherence": 0.9}"#.to_string(),
        };
        let config = ProseLintConfig::default();
        let score = run_quality_gate(
            "The server rejected the request.",
            None,
            Some(&gen),
            &config,
            DEFAULT_QUALITY_THRESHOLD,
        )
        .await;
        assert!(score.judge.is_none());
    }

    // ── Storage: paired with DOCGEN-07 version metadata ───────────────

    /// Acceptance criterion: the score is stored keyed the same way
    /// DOCGEN-07 keys artifact versions (`ArtifactKey` + version number).
    #[test]
    fn quality_score_store_records_and_fetches_by_artifact_key_and_version() {
        let store = QualityScoreStore::new();
        let key = ArtifactKey::new("terminus", "readme");
        let score = QualityScore {
            lint: LintResult { issues: vec![] },
            lint_score: 1.0,
            judge: Some(JudgeScores { faithfulness: 0.9, completeness: 0.9, coherence: 0.9 }),
            combined_score: 0.95,
            verdict: QualityVerdict::Passed,
        };
        store.record(key.clone(), 1, score.clone());

        let fetched = store.get(&key, 1).expect("score must be retrievable");
        assert_eq!(fetched, score);
    }

    /// Negative test: an unrecorded (key, version) has no score.
    #[test]
    fn quality_score_store_returns_none_for_unrecorded_version() {
        let store = QualityScoreStore::new();
        let key = ArtifactKey::new("terminus", "readme");
        assert!(store.get(&key, 1).is_none());
    }

    /// Different targets (and different versions of the same target) have
    /// independent scores, mirroring `VersionStore`'s own per-target
    /// independence.
    #[test]
    fn quality_score_store_keys_are_independent_across_target_and_version() {
        let store = QualityScoreStore::new();
        let readme_key = ArtifactKey::new("terminus", "readme");
        let wiki_key = ArtifactKey::new("terminus", "wiki");

        let passing = QualityScore {
            lint: LintResult { issues: vec![] },
            lint_score: 1.0,
            judge: None,
            combined_score: 1.0,
            verdict: QualityVerdict::Passed,
        };
        let failing = QualityScore {
            lint: LintResult { issues: vec![] },
            lint_score: 0.1,
            judge: None,
            combined_score: 0.1,
            verdict: QualityVerdict::Failed { reason: "too low".to_string() },
        };

        store.record(readme_key.clone(), 1, passing.clone());
        store.record(readme_key.clone(), 2, failing.clone());
        store.record(wiki_key.clone(), 1, passing.clone());

        assert_eq!(store.get(&readme_key, 1).unwrap().verdict, QualityVerdict::Passed);
        assert_eq!(
            store.get(&readme_key, 2).unwrap().verdict,
            QualityVerdict::Failed { reason: "too low".to_string() }
        );
        assert_eq!(store.get(&wiki_key, 1).unwrap().verdict, QualityVerdict::Passed);
    }

    // ── DGRICH-09: repo-level landing gates ───────────────────────────

    fn sample_facts_with_subsystems(names: &[&str]) -> RepoFacts {
        let mut facts = RepoFacts { project_id: "TERM".to_string(), git_ref: "abc123".to_string(), ..Default::default() };
        facts.subsystems = names
            .iter()
            .map(|n| super::super::repo_facts::Subsystem { name: n.to_string(), ..Default::default() })
            .collect();
        facts
    }

    fn sample_identity(tagline: &str, what_is: &str) -> RepoIdentity {
        RepoIdentity {
            tagline: tagline.to_string(),
            what_is: what_is.to_string(),
            audience: "operators".to_string(),
            subsystems: Vec::new(),
            feature_rows: Vec::new(),
            guide_topics: Vec::new(),
        }
    }

    #[test]
    fn check_landing_diagram_passes_a_landing_with_no_mermaid_fence_at_all() {
        assert!(check_landing_diagram("# Hello\n\nJust prose, no diagram.\n").is_ok());
    }

    #[test]
    fn check_landing_diagram_fails_the_generic_client_core_output_template() {
        let landing = "# Hello\n\n```mermaid\nflowchart LR\n    A[Client] --> B[Core] --> C[Output]\n```\n";
        let err = check_landing_diagram(landing).unwrap_err();
        assert!(err.contains("generic"), "{err}");
    }

    #[test]
    fn check_landing_diagram_fails_a_sub_five_node_diagram() {
        let landing = "# Hello\n\n```mermaid\nflowchart LR\n    MESH[mesh (12 symbols)] --> REG[registry (34 symbols)]\n```\n";
        assert!(check_landing_diagram(landing).is_err());
    }

    #[test]
    fn check_landing_diagram_passes_a_real_diagram_with_at_least_five_nodes() {
        let landing = "# Hello\n\n```mermaid\nflowchart LR\n    A[a (10 symbols)] --> B[b (9 symbols)]\n    B --> C[c (8 symbols)]\n    C --> D[d (7 symbols)]\n    D --> E[e (6 symbols)]\n```\n";
        assert!(check_landing_diagram(landing).is_ok(), "{:?}", check_landing_diagram(landing));
    }

    #[test]
    fn check_landing_identity_passes_a_balanced_hub_identity() {
        let facts = sample_facts_with_subsystems(&["intake", "scribe", "cortex"]);
        let identity = sample_identity(
            "The fleet's MCP tool hub.",
            "Exposes fleet tools over MCP behind an mTLS mesh gateway.",
        );
        assert!(check_landing_identity(&identity, &facts).is_ok());
    }

    /// Negative test: a tagline that only names one of several kept
    /// subsystems is caught by the anti-latch backstop.
    #[test]
    fn check_landing_identity_fails_a_tagline_latched_onto_a_single_subsystem() {
        let facts = sample_facts_with_subsystems(&["intake", "scribe", "cortex"]);
        let identity = sample_identity(
            "The intake model-discovery engine.",
            "Built around intake, the model-discovery and profiling engine.",
        );
        let err = check_landing_identity(&identity, &facts).unwrap_err();
        assert!(err.contains("anti-latch"), "{err}");
    }

    /// Negative test: an invented (never-real) symbol in the identity text
    /// is caught by the symbol-existence backstop.
    #[test]
    fn check_landing_identity_fails_an_invented_symbol() {
        let mut facts = sample_facts_with_subsystems(&["intake", "scribe"]);
        facts.subsystems[0].top_symbols =
            vec![super::super::repo_facts::SymbolRef { id: "crate::intake::Discover".to_string(), kind: "fn", path: "src/intake/mod.rs".to_string(), rank: 0.5 }];
        let identity = sample_identity(
            "The fleet's tool hub.",
            "Powered by `crate::intake::Discover` and the entirely invented `crate::ghost::Phantom`.",
        );
        let err = check_landing_identity(&identity, &facts).unwrap_err();
        assert!(err.contains("symbol-existence"), "{err}");
        assert!(err.contains("crate::ghost::Phantom"), "{err}");
    }
}
