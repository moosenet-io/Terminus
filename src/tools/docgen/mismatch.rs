//! DOCGEN-10: Behavior-contract mismatch detector (panel-adjudicated ->
//! Plane issue). S95, Plane TERM-152.
//!
//! At doc-generation time the doc engine already holds two independent
//! descriptions of the same system: the ACTUAL behavior (extracted from
//! the merged code) and the INTENDED behavior (an acceptance criterion,
//! an explicit behavior contract, or prior documentation). When these two
//! genuinely CONTRADICT each other -- not merely differ in phrasing or
//! summarize away detail -- a code reviewer (who only ever sees code-vs-
//! code) cannot catch it, because it lacks an independent notion of
//! intended behavior. This module is the feedback loop that closes that
//! gap.
//!
//! ## Safety in both directions (Notes-for-the-executing-agent #9)
//! When code and contract disagree, EITHER side can be the one that's
//! wrong. This module never assumes the code is at fault: a candidate
//! mismatch is adjudicated by the Terminus 5-agent review panel, asked an
//! explicit AUTHORITY/DIRECTION question ("which side is right, and what's
//! the resolution?"), never a code-quality prompt. A valid resolution is
//! either "fix the code" OR "the contract/spec is stale -- update it". No
//! consensus among the panel is treated as the ambiguity signal it is:
//! the mismatch is escalated to a human, never auto-queued for either
//! direction, and the loop never auto-rewrites code to match a contract
//! that might itself be stale.
//!
//! ## Reuse (do not reimplement)
//! This module deliberately EVOLVES the shipped SCRB-04 machinery
//! (`crate::scribe`) rather than duplicating it:
//!   - `crate::scribe::discrepancy_signature` / `build_discrepancy_title` --
//!     the same stable-signature-embedded-in-title dedup convention, reused
//!     verbatim (both now `pub(crate)` for this reuse).
//!   - `crate::scribe::find_duplicate_by_signature` -- the same
//!     text-listing scan for "already reported this", reused verbatim.
//!   - `crate::scribe::queue_discrepancy_locally` -- the same local-queue
//!     fallback (one JSON object per line) when Plane is unreachable,
//!     reused verbatim so a mismatch is never silently lost.
//!   - `crate::scribe::html_escape` -- the same HTML-escaping of
//!     quoted/interpolated code and doc text before it's embedded in a
//!     Plane `description_html` body, reused verbatim.
//! What's new here (not shared with SCRB-04, because SCRB-04 has no
//! equivalent): the tiered-sensitivity candidate gate, the panel-dispatch
//! authority-question prompt/parse, and the panel-consensus aggregation.
//!
//! ## Panel mechanism
//! The "Terminus 5-agent review panel" this item's spec refers to is
//! `review_run`'s panel machinery (`crate::review`): up to 5 providers
//! (`opus`, `codex`, `agy` via the review-daemon; `nemotron`, `qwen_coder`
//! via OpenRouter), dispatched concurrently through the exact same
//! `crate::review::ReviewConfig::dispatch_daemon` /
//! `dispatch_openrouter` HTTP calls `review_run` and Scribe's docs-gen
//! dispatch already use -- no new HTTP client. `council_convene`
//! (`crate::council`) was considered and rejected: it has no working
//! deliberation engine behind it (an intentional, documented stub -- see
//! `src/council/mod.rs`'s module doc comment), so it cannot actually
//! adjudicate anything. `review_run`'s OWN built-in prompt (`build_prompt`)
//! and verdict vocabulary (APPROVE/REQUEST_CHANGES) are for code-quality
//! review, not an authority/direction question -- reusing them as-is would
//! silently turn this into "review the code" and lose the "or the spec is
//! stale" branch entirely (the exact failure mode Notes-for-the-executing-
//! agent #9 warns against). So this module builds its OWN prompt
//! ([`build_authority_prompt`]) and verdict parser ([`parse_resolution`]),
//! dispatched through the SAME `ReviewConfig`/provider-routing machinery
//! `review_run` uses (via [`PanelDispatcher`] -- a real implementation,
//! [`RealPanelDispatcher`], for production, and an injectable mock for
//! tests, so unit tests never call a live model).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::review::ReviewConfig;
use crate::tool::RustTool;

use super::pii_gate::sweep_input;

// ---------------------------------------------------------------------------
// Tiered sensitivity (conservative: bias toward silence)
// ---------------------------------------------------------------------------

/// How binding the "intended behavior" side of a candidate mismatch is.
/// Drives whether a contradiction is even worth escalating to the panel at
/// all -- per the spec's tiered-sensitivity rule, this is deliberately
/// conservative: every filed issue must be a broken PROMISE about
/// behavior, never a summary/style/phrasing observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractTier {
    /// An acceptance criterion or an explicit, binding behavior contract.
    /// High sensitivity: any genuine contradiction here is high-confidence
    /// and always evaluated.
    AcceptanceCriteria,
    /// Documented prose behavior (a README claim, a doc comment's
    /// narrative description). Flagged only on a genuine contradiction --
    /// never on phrasing or an omission.
    DocumentedProse,
    /// Implementation detail, style, or a summary-level difference (docs
    /// summarize; that is not drift). NEVER flagged, regardless of how the
    /// text compares -- this tier exists specifically so callers have an
    /// explicit way to say "this difference doesn't matter" rather than
    /// relying on the contradiction heuristic to reject it correctly every
    /// time.
    ImplementationDetail,
}

/// Decide whether a candidate mismatch clears the tiered-sensitivity gate
/// and is worth escalating to the panel. `is_contradiction` is the result
/// of [`is_genuine_contradiction`] (or an equivalent caller-supplied
/// judgment) -- kept as a separate parameter so the tier policy and the
/// contradiction heuristic stay independently testable.
///
/// Bias toward silence: [`ContractTier::ImplementationDetail`] NEVER
/// clears the gate, even if `is_contradiction` is true -- docs summarizing
/// implementation detail away is expected, not drift.
pub fn candidate_clears_sensitivity_gate(tier: ContractTier, is_contradiction: bool) -> bool {
    match tier {
        ContractTier::ImplementationDetail => false,
        ContractTier::AcceptanceCriteria | ContractTier::DocumentedProse => is_contradiction,
    }
}

// ---------------------------------------------------------------------------
// Contradiction heuristic
// ---------------------------------------------------------------------------

/// Negation markers used to detect an asserted-vs-negated flip between the
/// contract and the observed code behavior. Deliberately small and
/// explicit rather than a full NLP negation detector -- a false negative
/// here (missing a real contradiction) is far cheaper than a false
/// positive (spamming Plane with phrasing noise), matching the spec's
/// "bias toward silence" instruction.
const NEGATION_MARKERS: &[&str] = &[
    "not ", "never ", "no ", "cannot", "can't", "won't", "doesn't", "does not",
    "fails to", "failed to", "unable to", "without ",
];

fn normalize_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

fn jaccard_similarity(a: &[String], b: &[String]) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let intersection = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

fn contains_negation(text: &str) -> bool {
    let lower = text.to_lowercase();
    NEGATION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Above this token-overlap ratio, two texts are considered close enough
/// to be the SAME claim reworded (a phrasing/summary-level difference,
/// never a contradiction) regardless of any negation marker present.
const PHRASING_SIMILARITY_THRESHOLD: f64 = 0.7;
/// Minimum shared-subject overlap (on the non-negation content words)
/// required before a negation-marker asymmetry is treated as a genuine
/// contradiction rather than two unrelated statements that happen to
/// contain "not" for unrelated reasons.
const SHARED_SUBJECT_THRESHOLD: f64 = 0.3;

/// Heuristic: is `contract_text` (the stated/intended behavior) genuinely
/// contradicted by `code_behavior` (the actual/observed behavior)?
///
/// Deliberately conservative (bias toward silence, per spec):
///   - High token overlap (>= [`PHRASING_SIMILARITY_THRESHOLD`]) -> the two
///     texts are close enough to be the same claim reworded -> NOT a
///     contradiction, regardless of wording differences.
///   - Otherwise, a genuine contradiction requires BOTH a negation-marker
///     asymmetry (exactly one side uses a negation the other doesn't) AND
///     meaningful shared-subject overlap (the two texts are clearly about
///     the same thing, just asserting opposite outcomes about it).
///   - Low overlap with no negation asymmetry -> the two texts are simply
///     about different things (or one is a summary/omission) -> NOT a
///     contradiction; this is what makes a summary-level difference
///     negative-test cleanly.
pub fn is_genuine_contradiction(contract_text: &str, code_behavior: &str) -> bool {
    let contract_tokens = normalize_tokens(contract_text);
    let code_tokens = normalize_tokens(code_behavior);

    if jaccard_similarity(&contract_tokens, &code_tokens) >= PHRASING_SIMILARITY_THRESHOLD {
        return false;
    }

    let contract_negated = contains_negation(contract_text);
    let code_negated = contains_negation(code_behavior);
    if contract_negated == code_negated {
        // Either both assert the same polarity, or neither does -- no
        // asymmetric negation signal to hang a contradiction on.
        return false;
    }

    // Shared-subject check: strip the negation words themselves out before
    // comparing overlap, so "not X" vs "X" registers as sharing subject X.
    let strip_negations = |tokens: &[String]| -> Vec<String> {
        tokens
            .iter()
            .filter(|t| {
                let t = t.as_str();
                !matches!(t, "not" | "never" | "no" | "cannot" | "cant" | "wont" | "doesnt" | "without")
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let contract_subject = strip_negations(&contract_tokens);
    let code_subject = strip_negations(&code_tokens);
    jaccard_similarity(&contract_subject, &code_subject) >= SHARED_SUBJECT_THRESHOLD
}

// ---------------------------------------------------------------------------
// Panel: authority-question prompt + resolution parsing
// ---------------------------------------------------------------------------

/// Build the panel's prompt for one candidate mismatch. This is
/// deliberately NOT `crate::review::build_prompt` (that prompt asks for a
/// code-quality APPROVE/REQUEST_CHANGES verdict) -- it asks the explicit
/// AUTHORITY/DIRECTION question the spec requires: which side is right,
/// and what's the resolution. Both artifacts (the actual code behavior and
/// the stated contract) are included verbatim (already PII-swept by the
/// caller before this is built -- see [`adjudicate_mismatch`]).
pub fn build_authority_prompt(module_path: &str, contract_text: &str, code_behavior: &str) -> String {
    format!(
        "You are adjudicating a disagreement between DOCUMENTED/CONTRACTED behavior and \
ACTUAL code behavior for module `{module_path}`.\n\n\
This is NOT a code-quality review. Do not comment on style, structure, or general \
correctness. Your ONLY job is to answer an AUTHORITY question: these two artifacts \
disagree -- which one is right, and what is the resolution?\n\n\
Artifact A -- the stated contract (acceptance criterion / behavior contract / \
documentation):\n{contract_text}\n\n\
Artifact B -- the actual code behavior (as observed/extracted from the merged code):\n\
{code_behavior}\n\n\
Two resolutions are EQUALLY VALID -- do not default to assuming the code is wrong:\n\
  - The code is wrong: it should be changed to match the contract.\n\
  - The contract/spec is stale: the code is correct and the documentation/contract \
should be updated to match it.\n\n\
If the contract itself is too ambiguous to judge, or you genuinely cannot tell which \
side is authoritative, say so plainly -- do not guess.\n\n\
Give your reasoning, then end your response with EXACTLY one line, verbatim:\n\
RESOLUTION: CODE_WRONG\n\
or\n\
RESOLUTION: SPEC_STALE\n\
or\n\
RESOLUTION: UNCLEAR\n"
    )
}

/// One panel member's parsed direction/authority judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelResolution {
    /// The code is wrong; it should be changed to match the contract.
    CodeWrong,
    /// The contract/spec is stale; the code is correct.
    SpecStale,
    /// The panel member could not judge (or gave no parseable answer).
    Unclear,
}

impl PanelResolution {
    fn as_str(&self) -> &'static str {
        match self {
            PanelResolution::CodeWrong => "CODE_WRONG",
            PanelResolution::SpecStale => "SPEC_STALE",
            PanelResolution::Unclear => "UNCLEAR",
        }
    }
}

/// Extract the `RESOLUTION: ...` token from a panel member's raw response,
/// mirroring `crate::review::parse_verdict`'s scan-from-the-end / ASCII-only-
/// uppercase approach (same rationale: a model echoing the instruction text
/// earlier in its response must not be mistaken for its real final answer,
/// and ASCII-only uppercasing keeps byte offsets in sync for non-ASCII
/// reasoning text -- see that function's doc comment for the full
/// justification, not duplicated here).
pub fn parse_resolution(raw: &str) -> (PanelResolution, String) {
    let upper = raw.to_ascii_uppercase();
    let anchor = upper.rfind("RESOLUTION:");

    let Some(pos) = anchor else {
        return (PanelResolution::Unclear, raw.trim().to_string());
    };

    let after = &raw[pos + "RESOLUTION:".len()..];
    let token_line = after.lines().next().unwrap_or("").trim().to_uppercase();

    let resolution = if token_line.contains("CODE_WRONG") || token_line.contains("CODE WRONG") {
        PanelResolution::CodeWrong
    } else if token_line.contains("SPEC_STALE") || token_line.contains("SPEC STALE") {
        PanelResolution::SpecStale
    } else {
        PanelResolution::Unclear
    };

    let reasoning = raw[..pos].trim().to_string();
    let reasoning = if reasoning.is_empty() { raw.trim().to_string() } else { reasoning };

    (resolution, reasoning)
}

/// The fixed 5-provider roster asked for adjudication -- the same roster
/// `review_run`'s panel structures dispatch to (`opus`, `codex`, `agy` via
/// the review-daemon; `nemotron`, `qwen_coder` via OpenRouter). Fixed
/// rather than caller-supplied: an authority adjudication should always
/// hear from the full panel, not a caller-narrowed subset.
pub const PANEL_PROVIDERS: &[&str] = &["opus", "codex", "agy", "nemotron", "qwen_coder"];

/// One panel member's outcome, including a raw `error` (mirroring
/// `review_run`'s `ProviderResult` degrade contract: a single provider's
/// failure never fails the whole panel, it just doesn't count toward
/// consensus).
#[derive(Debug, Clone)]
pub struct PanelVote {
    pub provider: String,
    pub resolution: PanelResolution,
    pub reasoning: String,
    pub error: Option<String>,
}

impl PanelVote {
    fn is_available(&self) -> bool {
        self.error.is_none()
    }
}

/// Abstraction over "ask one panel provider a prompt, get raw text back",
/// so the real HTTP-backed panel and a test double share the exact same
/// call shape. Production code uses [`RealPanelDispatcher`] (thin wrapper
/// over `ReviewConfig`, the same dispatch machinery `review_run` and
/// Scribe's docs-gen use); unit tests use an in-memory mock -- the panel is
/// NEVER live-called from a `cargo test` run.
#[async_trait]
pub trait PanelDispatcher: Send + Sync {
    async fn dispatch(&self, provider: &str, prompt: &str) -> Result<String, String>;
}

/// Production panel dispatcher: routes each provider to the review-daemon
/// (`opus`/`codex`/`agy`) or OpenRouter (`nemotron`/`qwen_coder`) via the
/// exact same `ReviewConfig` methods `review_run` and
/// `scribe::dispatch_docs_generation` already call, using
/// `crate::review::dispatch::{is_daemon_provider, openrouter_model_for}`
/// directly (that module is now `pub(crate)`, per this item, specifically
/// so this routing table has exactly one source of truth instead of a
/// second copy) -- no new HTTP client, no duplicated provider-routing
/// logic. See the module doc comment for why `review_run`'s own built-in
/// prompt/verdict path (APPROVE/REQUEST_CHANGES) isn't reused wholesale.
pub struct RealPanelDispatcher {
    pub cfg: ReviewConfig,
}

#[async_trait]
impl PanelDispatcher for RealPanelDispatcher {
    async fn dispatch(&self, provider: &str, prompt: &str) -> Result<String, String> {
        use crate::review::dispatch::{is_daemon_provider, openrouter_model_for};
        if is_daemon_provider(provider) {
            self.cfg.dispatch_daemon(provider, prompt).await
        } else if let Some(model) = openrouter_model_for(provider) {
            self.cfg.dispatch_openrouter(model, prompt).await
        } else {
            Err(format!("unavailable: unknown panel provider '{provider}'"))
        }
    }
}

/// Dispatch the authority prompt to every provider in [`PANEL_PROVIDERS`]
/// concurrently, mirroring `review_run`'s own concurrent-dispatch shape
/// (`tokio::task::JoinSet`, one task per provider, a panicking task
/// degrading to an `error` slot rather than failing the whole call).
pub async fn dispatch_panel(dispatcher: Arc<dyn PanelDispatcher>, prompt: &str) -> Vec<PanelVote> {
    let mut set = tokio::task::JoinSet::new();
    for provider in PANEL_PROVIDERS {
        let dispatcher = dispatcher.clone();
        let prompt = prompt.to_string();
        let provider = provider.to_string();
        set.spawn(async move {
            let raw = dispatcher.dispatch(&provider, &prompt).await;
            match raw {
                Ok(text) => {
                    let (resolution, reasoning) = parse_resolution(&text);
                    PanelVote { provider, resolution, reasoning, error: None }
                }
                Err(reason) => PanelVote {
                    provider,
                    resolution: PanelResolution::Unclear,
                    reasoning: String::new(),
                    error: Some(reason),
                },
            }
        });
    }

    let mut votes = Vec::with_capacity(PANEL_PROVIDERS.len());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(vote) => votes.push(vote),
            Err(join_err) => votes.push(PanelVote {
                provider: "unknown".to_string(),
                resolution: PanelResolution::Unclear,
                reasoning: String::new(),
                error: Some(format!("unavailable: task join error: {join_err}")),
            }),
        }
    }
    // Deterministic order for callers/tests (JoinSet completion order is
    // not guaranteed): sort by the fixed roster's declared order.
    let order: HashMap<&str, usize> =
        PANEL_PROVIDERS.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    votes.sort_by_key(|v| order.get(v.provider.as_str()).copied().unwrap_or(usize::MAX));
    votes
}

/// Panel consensus outcome. Strict majority ( > 50% ) of AVAILABLE
/// (non-errored) votes agreeing on the SAME non-Unclear resolution ->
/// consensus. Anything else (a tie, a plurality that isn't a majority, all
/// unclear/errored, or a majority landing on `Unclear`) -> no consensus --
/// fails safe to human escalation, never guesses a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelConsensus {
    CodeWrong,
    SpecStale,
    NoConsensus,
}

pub fn aggregate_panel(votes: &[PanelVote]) -> PanelConsensus {
    let available: Vec<&PanelVote> = votes.iter().filter(|v| v.is_available()).collect();
    if available.is_empty() {
        return PanelConsensus::NoConsensus;
    }
    let total = available.len();
    let code_wrong = available.iter().filter(|v| v.resolution == PanelResolution::CodeWrong).count();
    let spec_stale = available.iter().filter(|v| v.resolution == PanelResolution::SpecStale).count();

    if code_wrong * 2 > total {
        PanelConsensus::CodeWrong
    } else if spec_stale * 2 > total {
        PanelConsensus::SpecStale
    } else {
        PanelConsensus::NoConsensus
    }
}

// ---------------------------------------------------------------------------
// Plane filing (reuses SCRB-04's signature/dedup/queue/escape machinery)
// ---------------------------------------------------------------------------

/// Build the Plane issue title for a filed mismatch. Reuses
/// `crate::scribe::discrepancy_signature`/`build_discrepancy_title`'s exact
/// dedup convention (a stable signature embedded as `[scribe-disc:<sig>]`
/// in the title, scanned for by `find_duplicate_by_signature`) so both
/// SCRB-04 and this detector dedup against the SAME signature space and
/// scanning logic -- one dedup mechanism, two callers, not two.
fn mismatch_signature(module_path: &str, contract_text: &str) -> String {
    crate::scribe::discrepancy_signature(module_path, contract_text)
}

/// Human-readable status tag embedded in the issue title, distinguishing
/// the three possible outcomes at a glance in a Plane issue list without
/// needing to resolve a state UUID (`plane_create_work_item`'s `state`
/// param takes a UUID, which this in-process caller has no way to resolve
/// generically per-project -- priority + a title tag is the same
/// discoverability SCRB-04 already relies on for its own signature tag).
fn status_tag(consensus: PanelConsensus) -> &'static str {
    match consensus {
        PanelConsensus::CodeWrong => "READY-FOR-BUILD: fix code",
        PanelConsensus::SpecStale => "SPEC-UPDATE",
        PanelConsensus::NoConsensus => "NEEDS-HUMAN",
    }
}

fn priority_for(consensus: PanelConsensus) -> &'static str {
    match consensus {
        // No consensus is the ambiguity signal itself -- surfaced with
        // urgency so it doesn't sit unseen, but explicitly NOT queued for
        // build (see the description body's explicit language below).
        PanelConsensus::NoConsensus => "high",
        PanelConsensus::CodeWrong | PanelConsensus::SpecStale => "medium",
    }
}

fn build_mismatch_title(module_path: &str, signature: &str, consensus: PanelConsensus) -> String {
    format!(
        "Behavior-contract mismatch [{}]: {module_path} [mismatch-sig:{signature}]",
        status_tag(consensus)
    )
}

/// Build the (HTML-escaped) issue body: both artifacts quoted verbatim
/// (PII-swept by the caller before this is built), the panel's per-member
/// votes, and the consensus resolution / human-escalation language.
/// Reuses `crate::scribe::html_escape` for every interpolated field --
/// same reasoning as SCRB-04's own description builder: `contract_text`/
/// `code_behavior`/panel reasoning are model/doc-derived text, not a fully
/// trusted operator, and `description_html` is rendered as raw HTML by
/// Plane.
fn build_mismatch_description(
    module_path: &str,
    contract_text: &str,
    code_behavior: &str,
    votes: &[PanelVote],
    consensus: PanelConsensus,
) -> String {
    use crate::scribe::html_escape;

    let votes_html: String = votes
        .iter()
        .map(|v| {
            if let Some(err) = &v.error {
                format!("<li><strong>{}:</strong> unavailable ({})</li>", html_escape(&v.provider), html_escape(err))
            } else {
                format!(
                    "<li><strong>{}:</strong> {}</li>",
                    html_escape(&v.provider),
                    html_escape(v.resolution.as_str())
                )
            }
        })
        .collect();

    let resolution_html = match consensus {
        PanelConsensus::CodeWrong => {
            "<p><strong>Panel consensus:</strong> the code is wrong -- it should be changed to \
match the stated contract.</p>"
        }
        PanelConsensus::SpecStale => {
            "<p><strong>Panel consensus:</strong> the contract/spec is stale -- the code is \
correct; the documentation/contract should be updated to match it. This is NOT a request to \
change the code.</p>"
        }
        PanelConsensus::NoConsensus => {
            "<p><strong>No panel consensus.</strong> The panel could not converge on which side \
is authoritative. This issue is filed as a NEEDS-HUMAN-DECISION item and must NOT be \
auto-queued for build in either direction -- panel disagreement is itself the ambiguity \
signal.</p>"
        }
    };

    format!(
        "<p><strong>Module:</strong> {module}</p>\
<p><strong>Artifact A -- stated contract:</strong> {contract}</p>\
<p><strong>Artifact B -- actual code behavior:</strong> {code}</p>\
<p><strong>Panel votes:</strong></p><ul>{votes}</ul>\
{resolution}\
<p><em>Filed automatically by the DOCGEN-10 mismatch detector. No code fix has been \
attempted -- this detector has no commit/push capability by design.</em></p>",
        module = html_escape(module_path),
        contract = html_escape(contract_text),
        code = html_escape(code_behavior),
        votes = votes_html,
        resolution = resolution_html,
    )
}

/// Full candidate-mismatch adjudication + Plane filing flow. Non-blocking
/// by construction: every fallible step downstream of the tier gate is
/// caught by the caller ([`DocgenMismatchDetect::execute`]) so a detector
/// failure NEVER fails the feat/doc-gen it's attached to (spec EDGE CASE /
/// ACCEPTANCE CRITERION).
///
/// `contract_text`/`code_behavior` are swept through the DOCGEN-02 PII
/// input gate ([`sweep_input`]) before EITHER touches the panel prompt or
/// the eventual Plane issue body -- "both-sides quoting is PII-swept" is a
/// hard acceptance criterion, not best-effort.
pub async fn adjudicate_mismatch(
    dispatcher: Arc<dyn PanelDispatcher>,
    project_id: &str,
    module_path: &str,
    tier: ContractTier,
    contract_text_raw: &str,
    code_behavior_raw: &str,
) -> Result<String, ToolError> {
    let contradiction = is_genuine_contradiction(contract_text_raw, code_behavior_raw);
    if !candidate_clears_sensitivity_gate(tier, contradiction) {
        return Ok(format!(
            "No candidate mismatch: tier={tier:?}, contradiction={contradiction} -- below the \
sensitivity gate, not escalated (bias toward silence)."
        ));
    }

    let contract_swept = sweep_input(contract_text_raw)?;
    let code_swept = sweep_input(code_behavior_raw)?;
    let contract_text = contract_swept.sanitized_content();
    let code_behavior = code_swept.sanitized_content();

    let prompt = build_authority_prompt(module_path, contract_text, code_behavior);
    let votes = dispatch_panel(dispatcher, &prompt).await;
    let consensus = aggregate_panel(&votes);

    let signature = mismatch_signature(module_path, contract_text);
    let title = build_mismatch_title(module_path, &signature, consensus);
    let description_html =
        build_mismatch_description(module_path, contract_text, code_behavior, &votes, consensus);
    let priority = priority_for(consensus);

    let client = Arc::new(crate::plane::PlaneClient::from_env());
    if !client.configured() {
        return Err(ToolError::NotConfigured(
            "PLANE_API_URL and PLANE_API_KEY must be set to file mismatch issues via Plane".into(),
        ));
    }

    let queue_path = crate::scribe::ScribeConfig::from_env().pending_queue_path;

    let lister = crate::plane::PlaneListWorkItemsFiltered::new(client.clone());
    let listing_text = match lister.execute(json!({"project_id": project_id, "limit": 200})).await {
        Ok(text) => text,
        Err(ToolError::Http(detail)) => {
            return crate::scribe::queue_discrepancy_locally(
                &queue_path,
                project_id,
                module_path,
                &title,
                &description_html,
                &format!("listing existing issues failed: {detail}"),
            );
        }
        Err(e) => return Err(e),
    };

    if let Some(existing_line) = crate::scribe::find_duplicate_by_signature(&listing_text, &signature) {
        return Ok(format!(
            "Duplicate mismatch -- an existing open issue already matches this signature, not \
creating another: {}",
            existing_line.trim()
        ));
    }

    let creator = crate::plane::PlaneCreateWorkItem::new(client);
    let create_args = json!({
        "project_id": project_id,
        "name": title,
        "description_html": description_html,
        "priority": priority,
    });
    match creator.execute(create_args).await {
        Ok(result) => Ok(result),
        Err(ToolError::Http(detail)) => crate::scribe::queue_discrepancy_locally(
            &queue_path,
            project_id,
            module_path,
            &title,
            &description_html,
            &format!("creating the issue failed: {detail}"),
        ),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Tool: docgen_mismatch_detect
// ---------------------------------------------------------------------------

pub struct DocgenMismatchDetect;

fn parse_tier(s: &str) -> Result<ContractTier, ToolError> {
    match s {
        "acceptance_criteria" => Ok(ContractTier::AcceptanceCriteria),
        "documented_prose" => Ok(ContractTier::DocumentedProse),
        "implementation_detail" => Ok(ContractTier::ImplementationDetail),
        other => Err(ToolError::InvalidArgument(format!(
            "tier must be one of acceptance_criteria|documented_prose|implementation_detail, got '{other}'"
        ))),
    }
}

#[async_trait]
impl RustTool for DocgenMismatchDetect {
    fn name(&self) -> &str {
        "docgen_mismatch_detect"
    }

    fn description(&self) -> &str {
        "Behavior-contract mismatch detector: compares a stated contract (acceptance \
criterion / behavior contract / documentation) against actual code behavior. On a \
genuine contradiction (tiered sensitivity -- never flags implementation-detail/style/ \
summary-level differences), dispatches the Terminus 5-agent review panel with an \
explicit AUTHORITY question (which side is right: is the code wrong, or is the \
contract/spec stale?) -- never a code-quality review. Consensus files a Plane issue \
with the resolution (fix-code OR update-spec, both valid); no consensus files as \
needs-human, never auto-queued. Never fails the caller's feat/doc-gen: detector \
failures are reported as an error result for THIS tool call only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {
                    "type": "string",
                    "description": "Plane project UUID or identifier to file the mismatch in (e.g. \"TERM\")"
                },
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative path where the mismatch was found"
                },
                "tier": {
                    "type": "string",
                    "enum": ["acceptance_criteria", "documented_prose", "implementation_detail"],
                    "description": "How binding the stated contract is; implementation_detail never files an issue"
                },
                "contract_text": {
                    "type": "string",
                    "description": "The stated/intended behavior (acceptance criterion, behavior contract, or documentation prose)"
                },
                "code_behavior": {
                    "type": "string",
                    "description": "The actual behavior observed/extracted from the merged code"
                }
            },
            "required": ["project_id", "module_path", "tier", "contract_text", "code_behavior"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("project_id is required and must not be empty".into()))?;
        let module_path = args
            .get("module_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("module_path is required and must not be empty".into()))?;
        let tier_str = args
            .get("tier")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("tier is required".into()))?;
        let tier = parse_tier(tier_str)?;
        let contract_text = args
            .get("contract_text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("contract_text is required and must not be empty".into()))?;
        let code_behavior = args
            .get("code_behavior")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("code_behavior is required and must not be empty".into()))?;

        let dispatcher: Arc<dyn PanelDispatcher> =
            Arc::new(RealPanelDispatcher { cfg: ReviewConfig::from_env() });

        // Non-blocking contract (spec ACCEPTANCE CRITERION / EDGE CASE): a
        // detector failure never fails the feat/doc-gen it's attached to.
        // At the `RustTool::execute` boundary that means this call itself
        // still returns `Err` (a tool call has to report SOMETHING), but the
        // caller wiring this into a doc-gen pipeline step (DOCGEN-08+) MUST
        // treat that `Err` as non-fatal to the surrounding feat -- log/flag
        // and continue, exactly like Scribe's build-diary hook treats a
        // `NotConfigured`/failed vault write as a logged known-issue, never
        // a pipeline abort. Documented here at the boundary this item owns;
        // wiring the "don't abort the feat" caller behavior is DOCGEN-08's
        // job once it exists.
        adjudicate_mismatch(dispatcher, project_id, module_path, tier, contract_text, code_behavior).await
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenMismatchDetect));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ─── Tiered sensitivity gate ─────────────────────────────────────────

    #[test]
    fn acceptance_criteria_tier_flags_a_contradiction() {
        assert!(candidate_clears_sensitivity_gate(ContractTier::AcceptanceCriteria, true));
    }

    #[test]
    fn acceptance_criteria_tier_does_not_flag_without_contradiction() {
        assert!(!candidate_clears_sensitivity_gate(ContractTier::AcceptanceCriteria, false));
    }

    #[test]
    fn documented_prose_tier_flags_only_genuine_contradiction() {
        assert!(candidate_clears_sensitivity_gate(ContractTier::DocumentedProse, true));
        assert!(!candidate_clears_sensitivity_gate(ContractTier::DocumentedProse, false));
    }

    #[test]
    fn implementation_detail_tier_never_flags_even_with_contradiction() {
        // Negative test: bias toward silence -- this tier is a hard NEVER,
        // regardless of the contradiction heuristic's own verdict.
        assert!(!candidate_clears_sensitivity_gate(ContractTier::ImplementationDetail, true));
        assert!(!candidate_clears_sensitivity_gate(ContractTier::ImplementationDetail, false));
    }

    // ─── Contradiction heuristic ─────────────────────────────────────────

    #[test]
    fn clear_contradiction_is_detected() {
        // AC says X, code does not-X -- shared subject ("returns", "error"
        // vocabulary), asymmetric negation.
        let contract = "The function returns an error when the input is empty.";
        let code = "The function does not return an error when the input is empty.";
        assert!(is_genuine_contradiction(contract, code));
    }

    #[test]
    fn phrasing_only_difference_is_not_a_contradiction() {
        // Negative test: same claim, reworded -- high token overlap, no
        // negation asymmetry. Must NOT be flagged as a contradiction.
        let contract = "The function returns an error when the input is empty.";
        let code = "The function returns an error if the input is empty.";
        assert!(!is_genuine_contradiction(contract, code));
    }

    #[test]
    fn summary_level_difference_is_not_a_contradiction() {
        // Negative test: docs summarize implementation detail away -- low
        // overlap, but no negation asymmetry either, so this must not be
        // mistaken for a contradiction.
        let contract = "The module validates input.";
        let code = "The module parses the JSON body, checks required fields, coerces \
types, and applies default values before returning a typed struct.";
        assert!(!is_genuine_contradiction(contract, code));
    }

    #[test]
    fn unrelated_negation_without_shared_subject_is_not_a_contradiction() {
        // Both mention negation, but about unrelated subjects -- must not
        // be flagged.
        let contract = "The cache does not persist across restarts.";
        let code = "The scheduler is not multi-threaded.";
        assert!(!is_genuine_contradiction(contract, code));
    }

    // ─── Resolution parsing ──────────────────────────────────────────────

    #[test]
    fn parse_resolution_extracts_code_wrong() {
        let (r, reasoning) = parse_resolution("The contract is clear and binding.\nRESOLUTION: CODE_WRONG");
        assert_eq!(r, PanelResolution::CodeWrong);
        assert_eq!(reasoning, "The contract is clear and binding.");
    }

    #[test]
    fn parse_resolution_extracts_spec_stale() {
        let (r, _) = parse_resolution("The code is correct; the doc is outdated.\nRESOLUTION: SPEC_STALE");
        assert_eq!(r, PanelResolution::SpecStale);
    }

    #[test]
    fn parse_resolution_extracts_unclear() {
        let (r, _) = parse_resolution("Ambiguous.\nRESOLUTION: UNCLEAR");
        assert_eq!(r, PanelResolution::Unclear);
    }

    #[test]
    fn parse_resolution_unknown_marker_defaults_unclear() {
        let (r, reasoning) = parse_resolution("No marker in this response at all.");
        assert_eq!(r, PanelResolution::Unclear);
        assert_eq!(reasoning, "No marker in this response at all.");
    }

    #[test]
    fn parse_resolution_uses_last_occurrence_not_first() {
        let raw = "Instructions mentioned RESOLUTION: CODE_WRONG as one option.\n\
                   My actual answer is:\nRESOLUTION: SPEC_STALE";
        let (r, _) = parse_resolution(raw);
        assert_eq!(r, PanelResolution::SpecStale);
    }

    // ─── Authority prompt (assert it's the authority question, not code quality) ──

    #[test]
    fn authority_prompt_asks_direction_not_code_quality() {
        let p = build_authority_prompt("src/x", "contract says X", "code does not-X");
        assert!(p.contains("AUTHORITY"));
        assert!(p.to_lowercase().contains("not a code-quality review"));
        assert!(p.contains("contract says X"));
        assert!(p.contains("code does not-X"));
        assert!(p.contains("RESOLUTION: CODE_WRONG"));
        assert!(p.contains("RESOLUTION: SPEC_STALE"));
        assert!(p.contains("RESOLUTION: UNCLEAR"));
        // Both directions explicitly presented as equally valid -- the
        // safety-in-both-directions requirement.
        assert!(p.to_lowercase().contains("equally valid"));
    }

    #[test]
    fn authority_prompt_embeds_both_artifacts_verbatim() {
        let p = build_authority_prompt("src/y", "UNIQUE_CONTRACT_MARKER", "UNIQUE_CODE_MARKER");
        assert!(p.contains("UNIQUE_CONTRACT_MARKER"));
        assert!(p.contains("UNIQUE_CODE_MARKER"));
    }

    // ─── Panel dispatch (mocked -- never a live model in unit tests) ──────

    struct MockDispatcher {
        responses: HashMap<&'static str, Result<String, String>>,
    }

    #[async_trait]
    impl PanelDispatcher for MockDispatcher {
        async fn dispatch(&self, provider: &str, _prompt: &str) -> Result<String, String> {
            self.responses
                .get(provider)
                .cloned()
                .unwrap_or_else(|| Err(format!("unavailable: no mock response for '{provider}'")))
        }
    }

    fn approve_all(resolution_line: &str) -> Arc<dyn PanelDispatcher> {
        let mut responses = HashMap::new();
        for p in PANEL_PROVIDERS {
            responses.insert(*p, Ok(format!("reasoning\nRESOLUTION: {resolution_line}")));
        }
        Arc::new(MockDispatcher { responses })
    }

    #[tokio::test]
    async fn dispatch_panel_returns_a_vote_per_provider_in_fixed_order() {
        let dispatcher = approve_all("CODE_WRONG");
        let votes = dispatch_panel(dispatcher, "prompt").await;
        assert_eq!(votes.len(), PANEL_PROVIDERS.len());
        for (v, expected) in votes.iter().zip(PANEL_PROVIDERS.iter()) {
            assert_eq!(v.provider, *expected);
            assert_eq!(v.resolution, PanelResolution::CodeWrong);
        }
    }

    #[tokio::test]
    async fn dispatch_panel_degrades_a_single_unavailable_provider() {
        let mut responses = HashMap::new();
        for p in PANEL_PROVIDERS {
            responses.insert(*p, Ok("x\nRESOLUTION: SPEC_STALE".to_string()));
        }
        responses.insert("agy", Err("unavailable: daemon down".to_string()));
        let dispatcher: Arc<dyn PanelDispatcher> = Arc::new(MockDispatcher { responses });
        let votes = dispatch_panel(dispatcher, "prompt").await;
        let agy_vote = votes.iter().find(|v| v.provider == "agy").unwrap();
        assert!(agy_vote.error.is_some());
        assert!(!agy_vote.is_available());
    }

    // ─── Panel consensus aggregation ───────────────────────────────────────

    fn vote(provider: &str, resolution: PanelResolution) -> PanelVote {
        PanelVote { provider: provider.to_string(), resolution, reasoning: "r".to_string(), error: None }
    }
    fn err_vote(provider: &str, reason: &str) -> PanelVote {
        PanelVote {
            provider: provider.to_string(),
            resolution: PanelResolution::Unclear,
            reasoning: String::new(),
            error: Some(reason.to_string()),
        }
    }

    #[test]
    fn consensus_code_wrong_on_strict_majority() {
        let votes = vec![
            vote("opus", PanelResolution::CodeWrong),
            vote("codex", PanelResolution::CodeWrong),
            vote("agy", PanelResolution::CodeWrong),
            vote("nemotron", PanelResolution::SpecStale),
            vote("qwen_coder", PanelResolution::Unclear),
        ];
        assert_eq!(aggregate_panel(&votes), PanelConsensus::CodeWrong);
    }

    #[test]
    fn consensus_spec_stale_on_strict_majority() {
        let votes = vec![
            vote("opus", PanelResolution::SpecStale),
            vote("codex", PanelResolution::SpecStale),
            vote("agy", PanelResolution::SpecStale),
            vote("nemotron", PanelResolution::CodeWrong),
            vote("qwen_coder", PanelResolution::CodeWrong),
        ];
        assert_eq!(aggregate_panel(&votes), PanelConsensus::SpecStale);
    }

    #[test]
    fn no_consensus_on_a_tie() {
        // Negative test: 2 vs 2 vs 1 unclear -- no strict majority -> human,
        // never auto-queued in either direction.
        let votes = vec![
            vote("opus", PanelResolution::CodeWrong),
            vote("codex", PanelResolution::CodeWrong),
            vote("agy", PanelResolution::SpecStale),
            vote("nemotron", PanelResolution::SpecStale),
            vote("qwen_coder", PanelResolution::Unclear),
        ];
        assert_eq!(aggregate_panel(&votes), PanelConsensus::NoConsensus);
    }

    #[test]
    fn no_consensus_when_all_errored() {
        let votes = vec![
            err_vote("opus", "unavailable: x"),
            err_vote("codex", "unavailable: y"),
            err_vote("agy", "unavailable: z"),
            err_vote("nemotron", "unavailable: w"),
            err_vote("qwen_coder", "unavailable: v"),
        ];
        assert_eq!(aggregate_panel(&votes), PanelConsensus::NoConsensus);
    }

    #[test]
    fn consensus_survives_despite_one_errored_provider() {
        let votes = vec![
            vote("opus", PanelResolution::CodeWrong),
            vote("codex", PanelResolution::CodeWrong),
            vote("agy", PanelResolution::CodeWrong),
            vote("nemotron", PanelResolution::SpecStale),
            err_vote("qwen_coder", "unavailable: timeout"),
        ];
        assert_eq!(aggregate_panel(&votes), PanelConsensus::CodeWrong);
    }

    // ─── Title/description building ────────────────────────────────────────

    #[test]
    fn mismatch_title_embeds_status_tag_and_signature() {
        let sig = mismatch_signature("src/x", "claim");
        let title = build_mismatch_title("src/x", &sig, PanelConsensus::NoConsensus);
        assert!(title.contains("NEEDS-HUMAN"));
        assert!(title.contains(&format!("[mismatch-sig:{sig}]")));
    }

    #[test]
    fn description_html_escapes_and_includes_both_artifacts_and_votes() {
        let votes = vec![
            vote("opus", PanelResolution::CodeWrong),
            err_vote("codex", "unavailable: <script>bad</script>"),
        ];
        let desc = build_mismatch_description(
            "src/x",
            "contract has </p><script>x</script>",
            "code has & \"quotes\"",
            &votes,
            PanelConsensus::CodeWrong,
        );
        assert!(!desc.contains("<script>bad</script>"));
        assert!(desc.contains("&lt;script&gt;"));
        assert!(desc.contains("&amp;"));
        assert!(desc.contains("CODE_WRONG"));
        assert!(desc.to_lowercase().contains("consensus"));
    }

    #[test]
    fn description_for_no_consensus_explicitly_says_not_auto_queued() {
        let votes = vec![vote("opus", PanelResolution::CodeWrong), vote("codex", PanelResolution::SpecStale)];
        let desc = build_mismatch_description("src/x", "a", "b", &votes, PanelConsensus::NoConsensus);
        assert!(desc.to_lowercase().contains("must not"));
        assert!(desc.to_lowercase().contains("needs-human"));
    }

    #[test]
    fn description_for_spec_stale_explicitly_says_not_a_code_change_request() {
        // Safety-in-both-directions: the filed issue for a spec-stale
        // resolution must not read as "go fix the code".
        let votes = vec![vote("opus", PanelResolution::SpecStale), vote("codex", PanelResolution::SpecStale)];
        let desc = build_mismatch_description("src/x", "a", "b", &votes, PanelConsensus::SpecStale);
        assert!(desc.to_lowercase().contains("not a request to"));
    }

    // ─── End-to-end: below the sensitivity gate never touches the panel ────

    #[tokio::test]
    async fn adjudicate_mismatch_below_sensitivity_gate_never_dispatches_the_panel() {
        // If the panel were dispatched, MockDispatcher below would return
        // an error for every provider (empty responses map) and this would
        // still succeed via NoConsensus -- so instead we assert on the
        // returned message explicitly stating it was gated, which only
        // happens on the early-return path before any dispatch call.
        struct PanicIfCalled;
        #[async_trait]
        impl PanelDispatcher for PanicIfCalled {
            async fn dispatch(&self, _provider: &str, _prompt: &str) -> Result<String, String> {
                panic!("panel must not be dispatched below the sensitivity gate");
            }
        }
        let dispatcher: Arc<dyn PanelDispatcher> = Arc::new(PanicIfCalled);
        let result = adjudicate_mismatch(
            dispatcher,
            "TERM",
            "src/x",
            ContractTier::ImplementationDetail,
            "contract says X",
            "code does not-X",
        )
        .await
        .unwrap();
        assert!(result.contains("No candidate mismatch"));
    }

    // ─── Non-blocking: detector failure (Plane unconfigured) surfaces as Err,
    //     never a panic -- the caller (execute()) is documented to treat
    //     this as non-fatal to the surrounding feat/doc-gen. ────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn adjudicate_mismatch_with_plane_unconfigured_is_a_clean_error_not_panic() {
        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        let dispatcher = approve_all("CODE_WRONG");
        let result = adjudicate_mismatch(
            dispatcher,
            "TERM",
            "src/x",
            ContractTier::AcceptanceCriteria,
            "The function returns an error when the input is empty.",
            "The function does not return an error when the input is empty.",
        )
        .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn tool_missing_required_field_is_invalid_argument() {
        let tool = DocgenMismatchDetect;
        let result = tool
            .execute(json!({"module_path": "src/x", "tier": "acceptance_criteria", "contract_text": "a", "code_behavior": "b"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn tool_unknown_tier_is_invalid_argument() {
        let tool = DocgenMismatchDetect;
        let result = tool
            .execute(json!({
                "project_id": "TERM",
                "module_path": "src/x",
                "tier": "bogus",
                "contract_text": "a",
                "code_behavior": "b"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn registers_expected_tool() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("docgen_mismatch_detect"));
    }

    #[test]
    fn tool_has_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for info in reg.list() {
            assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
        }
    }

    // Guard against unused-import warnings if the Mutex import above ever
    // becomes dead code in a future edit pass.
    #[allow(dead_code)]
    fn _unused_guard(_m: &Mutex<()>) {}
}
