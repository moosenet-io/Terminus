//! CXEG-09: rule crystallization loop — recurrence + adversarial promotion.
//!
//! A `category:consistency|elegance` finding in the Atlas KGFIND corpus
//! (`crate::scribe::graph::findings_store`) that (a) recurs at or above
//! `CortexConfig::crystallize_min_recurrence` AND (b) survives ADVERSARIAL
//! PROMOTION — a `review_run` `panel_majority` panel explicitly prompted to
//! REFUTE that it should become a durable house rule, majority must FAIL to
//! refute — graduates to either a Tier-A lint STUB (a scaffold appended to
//! [`CANDIDATE_LINT_STUB_FILE`], NOT auto-wired into the live CXEG-05
//! checker) or a prose house rule appended to `docs/house-style.md`. Nothing
//! gates on taste until data (recurrence) plus an adversarial check
//! (promotion) both agree.
//!
//! ## Reuse (S9 — single source, not a second implementation)
//!
//! - Candidate selection queries `kg_findings` through
//!   [`FindingsStore::list`] (KGFIND's own query path) — no parallel SQL
//!   query is written here.
//! - Adversarial promotion dispatches through `crate::review::ReviewRun`
//!   in-process (the single sanctioned review door, S9/v3.17) — never a
//!   hand-rolled reviewer. `structure="panel_majority"` (unlike
//!   `kg_rule_promote`'s `adversarial_pair`), so every provider gets
//!   `review::prompt::Role::Reviewer`'s plain `VERDICT: APPROVE` /
//!   `VERDICT: REQUEST_CHANGES` framing — the REFUTE instruction lives in
//!   the `criteria` text (see [`build_promotion_criteria`]), and
//!   `REQUEST_CHANGES` is read as "this provider refuted it."
//! - Rule-guidance text reuses
//!   `crate::scribe::graph::rules::derive_guidance` (KGRULE-02's pure
//!   guidance derivation) rather than re-deriving prose from a finding a
//!   second way.
//!
//! ## Convergence: state lives on the finding, not a side table
//!
//! `kg_findings` gained a `crystallize_state` column (CXEG-09; see
//! `findings_store.rs`) that this module is the sole writer of:
//! `Some("promoted")` or `Some("refuted")`. [`select_candidates`] excludes
//! any finding whose `crystallize_state` is already set — so a refuted
//! candidate does not re-enter every cycle, and neither does an
//! already-promoted one (it already produced its artifact). A candidate
//! whose promotion panel comes back *incomplete* (a provider didn't answer —
//! distinct from a *complete* panel voting to refute) is left unmarked and
//! is eligible again next cycle: a transient dispatch failure must not
//! permanently and silently discard a candidate that was never actually
//! adversarially argued. See [`PromotionOutcome`].
//!
//! ## Dry-run by default
//!
//! `cortex_crystallize`'s `apply` argument defaults to `false`: candidates
//! and their would-be promotion criteria are listed, nothing is written and
//! nothing is marked. `apply:true` is required to actually dispatch the
//! promotion panel and write an artifact. If no review-panel transport is
//! configured at all (`REVIEW_DAEMON_TOKEN` and `OPENROUTER_API_KEY` both
//! unset — checked via `ReviewConfig::from_env()`'s already-materialized
//! fields, never a raw `std::env::var` here), `apply` REFUSES outright
//! rather than silently falling back to recurrence-only crystallization —
//! the adversarial check is not optional.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cortex::CortexConfig;
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::review::{ReviewConfig, ReviewRun};
use crate::scribe::graph::findings_store::{FindingRow, FindingsStore};
use crate::scribe::graph::rules::derive_guidance;
use crate::tool::{RustTool, ToolOutput};

/// Default minimum KGFIND recurrence for a `consistency`/`elegance` finding
/// to even be considered a crystallization candidate, used when neither the
/// tool argument nor `CORTEX_CRYSTALLIZE_MIN_RECURRENCE` is set. Mirrors
/// `rules::DEFAULT_MIN_OCCURRENCES`'s order of magnitude (a separate knob —
/// see `CortexConfig::crystallize_min_recurrence`'s doc comment for why).
pub const DEFAULT_MIN_RECURRENCE: i32 = 3;

/// The only two finding categories this loop crystallizes. Case-insensitive
/// on the finding's stored `category` (CXEG-07 hit exactly this bug —
/// "Consistency" vs "consistency" splitting a group — so comparison here is
/// deliberately normalized).
const ELIGIBLE_CATEGORIES: &[&str] = &["consistency", "elegance"];

/// Default 3-provider `panel_majority` panel for adversarial promotion.
/// Overridable per-call via the `providers` argument. Not required to be
/// exactly 2 (unlike `kg_rule_promote`'s `adversarial_pair`) — `panel_majority`
/// accepts 1-5.
pub const DEFAULT_PROMOTION_PANEL: [&str; 3] = ["codex", "agy", "nemotron"];

/// Where a promoted Tier-A candidate's lint scaffold is appended — inside the
/// CXEG-05 crate's own directory, but NOT a `.rs` file: this is deliberately
/// inert markdown, never compiled, never wired into `Checker`/`Rule` by this
/// tool. A human confirms the pattern is real and mechanically checkable,
/// then hand-writes the actual `Rule::` variant + AST-visitor logic.
const CANDIDATE_LINT_STUB_FILE: &str = "src/house_style/candidate_lint_stubs.md";
const LINT_STUB_SECTION_HEADER: &str = "# CXEG-09 candidate Tier-A lint stubs (NOT enforced)";
const LINT_STUB_SECTION_INTRO: &str = "\nAppended by `cortex_crystallize` on promotion of a \
mechanically-checkable `category:consistency|elegance` finding. **Nothing here is live** — a \
human must confirm the pattern is real, then hand-write a `Rule::` variant + `syn`-visitor logic \
in `src/house_style/mod.rs` and document it in `docs/house-style.md`. Entries are append-only \
history, not a queue to blindly implement.\n";

/// Where a promoted prose candidate is appended.
const HOUSE_STYLE_DOC_FILE: &str = "docs/house-style.md";
const PROSE_SECTION_HEADER: &str = "## Crystallized house rules (CXEG-09)";
const PROSE_SECTION_INTRO: &str = "\nRules below graduated from the KGFIND recurrence +\
adversarial-promotion loop (`cortex_crystallize`): each recurred at least\n\
`crystallize_min_recurrence` times across the KGFIND corpus AND survived a `review_run`\n\
`panel_majority` panel explicitly trying to REFUTE it. These are advisory prose guidance, not\n\
Tier-A mechanical lints (see the numbered rules above for those).\n";

// ---------------------------------------------------------------------------
// Pure: category eligibility + candidate selection
// ---------------------------------------------------------------------------

fn category_eligible(category: &str) -> bool {
    ELIGIBLE_CATEGORIES.contains(&category.trim().to_ascii_lowercase().as_str())
}

/// Pure text-normalized near-duplicate check between two candidate
/// descriptions: same category and the same trimmed/lowercased/whitespace-
/// collapsed description text. A lightweight backstop, NOT the full
/// embedding-cosine dedup `FindingsStore::record`/`dedup_decision` perform —
/// those already collapse near-duplicates recorded into the SAME
/// `(project_id, scope_kind, scope_ref, category)` bucket at write time
/// (this module never sees a within-bucket duplicate). This catches the
/// residual case a bucket-scoped dedup can't: the same underlying issue
/// crystallizing as separate candidates because it recurred under two
/// different `scope_ref`s. `FindingRow` here carries no embedding vector
/// (not selected by `FindingsStore::list`), so true cosine similarity isn't
/// available at this layer — text normalization is the pragmatic
/// approximation.
fn same_candidate_text(a: &FindingRow, b: &FindingRow) -> bool {
    fn norm(s: &str) -> String {
        s.trim().to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    }
    category_eligible(&a.category)
        && a.category.trim().to_ascii_lowercase() == b.category.trim().to_ascii_lowercase()
        && norm(&a.description) == norm(&b.description)
}

/// Pure crystallization candidate SELECTION: given the KGFIND findings for a
/// project (via [`FindingsStore::list`] — KGFIND's own query path, no
/// parallel SQL) and a recurrence threshold, decide which are eligible
/// crystallization candidates. No DB, no `review_run`, no I/O — fully
/// unit-testable.
///
/// A finding qualifies iff:
/// - its `category` (case-insensitively) is `consistency` or `elegance`,
/// - `occurrences >= min_recurrence`, and
/// - `crystallize_state` is `None` (never promoted OR refuted before — see
///   the module doc's convergence section).
///
/// Near-duplicate candidates (same category, same normalized description
/// text, different `scope_ref`) are collapsed to the single
/// highest-recurrence entry (see [`same_candidate_text`]).
///
/// Ordering is deterministic: `occurrences` descending, then
/// `(project_id, scope_kind, scope_ref, category, id)` ascending as a stable
/// tiebreak — independent of whatever order the caller's `Vec` arrived in.
pub fn select_candidates(findings: &[FindingRow], min_recurrence: i32) -> Vec<&FindingRow> {
    let mut candidates: Vec<&FindingRow> = findings
        .iter()
        .filter(|f| category_eligible(&f.category))
        .filter(|f| f.occurrences >= min_recurrence)
        .filter(|f| f.crystallize_state.is_none())
        .collect();

    candidates.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| a.project_id.cmp(&b.project_id))
            .then_with(|| a.scope_kind.cmp(&b.scope_kind))
            .then_with(|| a.scope_ref.cmp(&b.scope_ref))
            .then_with(|| a.category.cmp(&b.category))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut deduped: Vec<&FindingRow> = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !deduped.iter().any(|kept| same_candidate_text(kept, c)) {
            deduped.push(c);
        }
    }
    deduped
}

// ---------------------------------------------------------------------------
// Pure: adversarial promotion prompt + decision
// ---------------------------------------------------------------------------

/// Build the REFUTE-framed criteria text for a candidate's `panel_majority`
/// promotion review. Every provider gets `review::prompt::Role::Reviewer`'s
/// plain `VERDICT: APPROVE`/`VERDICT: REQUEST_CHANGES` sentinel (panel_majority
/// never assigns `Role::Attack`), so the REFUTE instruction — and the
/// explicit "default to refute when uncertain" instruction — has to live in
/// this criteria text rather than in a role variant.
pub fn build_promotion_criteria(finding: &FindingRow) -> String {
    let guidance = derive_guidance(&finding.category, &finding.description);
    format!(
        "A candidate house-style rule has been crystallized from {} recurring KGFIND findings \
(category '{}') on {}:{}. Candidate rule: '{guidance}'. Try to REFUTE that this should become a \
durable, ENFORCED house-style rule for this codebase: is it spurious, overfit to a handful of \
findings, merely a matter of taste, already covered by an existing lint/compiler warning, or not \
actually generalizable beyond this one spot? If you find a valid, concrete reason it should NOT \
become a standing rule, end your response with exactly VERDICT: REQUEST_CHANGES (you have \
refuted it). Only respond VERDICT: APPROVE if, after genuinely trying to refute it, you cannot \
find a valid reason to reject it and it is worth enforcing going forward. When uncertain, DEFAULT \
to VERDICT: REQUEST_CHANGES (refute) rather than guessing APPROVE.",
        finding.occurrences, finding.category, finding.scope_kind, finding.scope_ref,
    )
}

/// Build the `review_run` call args for a candidate's promotion:
/// `structure="panel_majority"` (see the module doc for why, not
/// `adversarial_pair`).
pub fn build_promotion_review_args(providers: &[String], finding: &FindingRow) -> Value {
    json!({
        "structure": "panel_majority",
        "providers": providers,
        "criteria": build_promotion_criteria(finding),
        "context": {
            "finding_id": finding.id.to_string(),
            "project_id": finding.project_id,
            "scope_kind": finding.scope_kind,
            "scope_ref": finding.scope_ref,
            "category": finding.category,
            "occurrences": finding.occurrences,
        }
    })
}

/// Outcome of an adversarial promotion attempt. No I/O — pure over the
/// `review_run` response's already-computed `aggregate_verdict`/`complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// Complete panel, majority did NOT refute (aggregate `APPROVE`) —
    /// graduates to an artifact.
    Promoted,
    /// Complete panel, majority DID refute (aggregate anything but
    /// `APPROVE` — `review_run`'s own `panel_majority` aggregation already
    /// fails safe to `REQUEST_CHANGES` on a tie or split, which is exactly
    /// "default refuted on uncertainty" one layer up). Marked and excluded
    /// from re-selection.
    Refuted,
    /// The panel did not complete (a provider didn't answer at all) — this
    /// candidate was never actually adversarially argued, so it is neither
    /// promoted nor marked refuted; it remains eligible next cycle.
    Incomplete,
}

/// Pure promotion DECISION: given a `review_run` `panel_majority` call's
/// `aggregate_verdict` and `complete` flag, decide the outcome. No I/O —
/// fully unit-testable. Mirrors `rules::promotion_decision`'s fail-closed
/// posture (incomplete never promotes) but distinguishes "explicitly
/// refuted" from "never actually argued" so only the former converges via a
/// persisted state mark.
pub fn promotion_outcome(aggregate_verdict: &str, complete: bool) -> PromotionOutcome {
    if !complete {
        return PromotionOutcome::Incomplete;
    }
    if aggregate_verdict == "APPROVE" {
        PromotionOutcome::Promoted
    } else {
        PromotionOutcome::Refuted
    }
}

// ---------------------------------------------------------------------------
// Pure: classification (lint-able vs prose)
// ---------------------------------------------------------------------------

/// Whether a promoted candidate graduates to a Tier-A lint stub scaffold or a
/// prose house rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Mechanically expressible as an AST-shape check (a scaffold only —
    /// see [`CANDIDATE_LINT_STUB_FILE`]'s doc comment).
    LintStub,
    /// Everything else — appended as advisory prose to `docs/house-style.md`.
    Prose,
}

/// Deliberately conservative, deterministic keyword heuristic: a description
/// that names a concrete syntactic construct (`std::env::var`, `panic!`,
/// `.unwrap()`, …) is the shape a `syn`-AST pass can actually check
/// mechanically; anything else (naming conventions, "prefer X over Y" taste
/// calls, architectural guidance) is not reliably AST-shape-checkable and
/// defaults to prose. This is a coarse first pass, not a claim of
/// completeness — a human still confirms every lint-stub candidate before
/// it's wired in (see the module doc), so a false "LintStub" classification
/// costs a discarded scaffold, never a bad live lint.
const MECHANICAL_MARKERS: &[&str] = &[
    "std::env::var",
    "env::var",
    "panic!",
    ".unwrap()",
    "println!",
    "todo!",
    "unimplemented!",
    ".expect(",
];

pub fn classify(description: &str) -> Classification {
    let lower = description.to_ascii_lowercase();
    if MECHANICAL_MARKERS.iter().any(|m| lower.contains(&m.to_ascii_lowercase())) {
        Classification::LintStub
    } else {
        Classification::Prose
    }
}

// ---------------------------------------------------------------------------
// Pure: artifact rendering
// ---------------------------------------------------------------------------

fn render_lint_stub_entry(finding: &FindingRow) -> String {
    let guidance = derive_guidance(&finding.category, &finding.description);
    format!(
        "\n### Candidate: {}:{} ({})\n\n\
- **Category**: {}\n\
- **Recurrence at crystallization**: {} occurrence(s)\n\
- **Finding id**: `{}`\n\
- **Crystallized**: {}\n\
- **Guidance**: {guidance}\n\
- **Status**: scaffold only — NOT wired into `Checker`/`Rule` in `src/house_style/mod.rs`.\n",
        finding.scope_kind,
        finding.scope_ref,
        finding.project_id,
        finding.category,
        finding.occurrences,
        finding.id,
        chrono::Utc::now().to_rfc3339(),
    )
}

fn render_prose_rule_entry(finding: &FindingRow) -> String {
    let guidance = derive_guidance(&finding.category, &finding.description);
    format!(
        "\n### {} — {}:{} ({})\n\n\
{guidance}\n\n\
*Recurred {} time(s) across the KGFIND corpus; crystallized {} (finding `{}`).*\n",
        finding.category,
        finding.scope_kind,
        finding.scope_ref,
        finding.project_id,
        finding.occurrences,
        chrono::Utc::now().to_rfc3339(),
        finding.id,
    )
}

/// Append `entry` under `header` in the file at `path`, inserting `header` +
/// `intro` once (only if the file doesn't already contain `header`) before
/// the first entry ever appended. Creates the file if it doesn't exist.
/// Returns the path written, for the tool's response.
fn append_under_section(path: &Path, header: &str, intro: &str, entry: &str) -> Result<String, ToolError> {
    let mut contents = std::fs::read_to_string(path).unwrap_or_default();
    if !contents.contains(header) {
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        if !contents.is_empty() {
            contents.push('\n');
        }
        contents.push_str(header);
        contents.push('\n');
        contents.push_str(intro);
    }
    contents.push_str(entry);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError::Execution(format!("create dir {}: {e}", parent.display())))?;
    }
    std::fs::write(path, &contents)
        .map_err(|e| ToolError::Execution(format!("write {}: {e}", path.display())))?;
    Ok(path.display().to_string())
}

fn write_lint_stub(repo_root: &Path, finding: &FindingRow) -> Result<String, ToolError> {
    append_under_section(
        &repo_root.join(CANDIDATE_LINT_STUB_FILE),
        LINT_STUB_SECTION_HEADER,
        LINT_STUB_SECTION_INTRO,
        &render_lint_stub_entry(finding),
    )
}

fn write_prose_rule(repo_root: &Path, finding: &FindingRow) -> Result<String, ToolError> {
    append_under_section(
        &repo_root.join(HOUSE_STYLE_DOC_FILE),
        PROSE_SECTION_HEADER,
        PROSE_SECTION_INTRO,
        &render_prose_rule_entry(finding),
    )
}

fn candidate_summary(f: &FindingRow) -> Value {
    json!({
        "finding_id": f.id.to_string(),
        "project_id": f.project_id,
        "category": f.category,
        "scope_kind": f.scope_kind,
        "scope_ref": f.scope_ref,
        "occurrences": f.occurrences,
        "description": f.description,
        "would_classify_as": match classify(&f.description) {
            Classification::LintStub => "lint_stub",
            Classification::Prose => "prose",
        },
    })
}

fn structured(v: Value) -> Result<ToolOutput, ToolError> {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    Ok(ToolOutput { text, structured: Some(v) })
}

// ---------------------------------------------------------------------------
// Tool: cortex_crystallize
// ---------------------------------------------------------------------------

pub struct CortexCrystallize;

#[async_trait]
impl RustTool for CortexCrystallize {
    fn name(&self) -> &str {
        "cortex_crystallize"
    }

    fn description(&self) -> &str {
        "CXEG-09: rule crystallization loop. Scans a project's Atlas KGFIND findings \
(kg_findings) for category:consistency|elegance findings whose recurrence meets \
crystallize_min_recurrence, then (on apply:true) runs an adversarial review_run \
panel_majority panel -- each provider tries to REFUTE the candidate, defaulting to refute \
on uncertainty -- before promoting. A promoted candidate is classified as a Tier-A lint \
STUB (a scaffold appended under src/house_style/, never auto-wired into the live CXEG-05 \
checker) or a prose house rule appended to docs/house-style.md. Dry-run by default: lists \
candidates and their would-be classification, writes and marks nothing. apply:true \
dispatches the promotion panel and writes the classified artifact for each candidate that \
survives it; refuted candidates are marked so they never re-enter a later cycle. Refuses \
to apply (falls back to a dry listing) if no review-panel transport is configured at all -- \
crystallization never happens on recurrence alone."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "min_recurrence": {"type": "integer", "description": "override crystallize_min_recurrence for this call"},
                "apply": {"type": "boolean", "description": "false (default): dry-run, writes/marks nothing. true: run adversarial promotion and write classified artifacts."},
                "providers": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "override the panel_majority promotion panel (default 3 providers)"
                }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument("'project_id' is required and must be a non-empty string".into())
            })?;

        let config = CortexConfig::from_env();
        let min_recurrence = args
            .get("min_recurrence")
            .and_then(|v| v.as_i64())
            // Saturating clamp, never `as i32`: a huge JSON int would wrap to a
            // negative threshold that accepts every finding (mirrors
            // `kg_rule_crystallize`'s own guard on this exact argument shape).
            .map(|v| v.clamp(1, i32::MAX as i64) as i32)
            .unwrap_or(config.crystallize_min_recurrence);

        let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);

        let providers: Vec<String> = match args.get("providers").and_then(|v| v.as_array()) {
            None => DEFAULT_PROMOTION_PANEL.iter().map(|s| s.to_string()).collect(),
            Some(arr) => {
                let parsed: Option<Vec<String>> =
                    arr.iter().map(|v| v.as_str().map(|s| s.to_string())).collect();
                let parsed = parsed.ok_or_else(|| {
                    ToolError::InvalidArgument("each entry in 'providers' must be a string".into())
                })?;
                if parsed.is_empty() {
                    return Err(ToolError::InvalidArgument("'providers' must be non-empty".into()));
                }
                parsed
            }
        };

        let findings_store = match FindingsStore::from_env().await {
            Ok(s) => s,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({"configured": false, "project_id": project_id}));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "project_id": project_id, "error": e.to_string(),
                }));
            }
        };

        // REUSE (S9): KGFIND's own query path -- no parallel SQL here.
        let findings = findings_store.list(&project_id, None, None, Some(min_recurrence)).await?;
        let candidates = select_candidates(&findings, min_recurrence);

        if !apply {
            let listed: Vec<Value> = candidates.iter().map(|f| candidate_summary(f)).collect();
            return structured(json!({
                "configured": true,
                "project_id": project_id,
                "dry_run": true,
                "applied": false,
                "min_recurrence": min_recurrence,
                "candidate_count": listed.len(),
                "candidates": listed,
            }));
        }

        // Apply mode: refuse outright without a reachable panel transport --
        // reads ReviewConfig's already-materialized fields (not a raw
        // std::env::var here), per the module doc's degrade contract.
        let review_cfg = ReviewConfig::from_env();
        if review_cfg.daemon_token.is_none() && review_cfg.openrouter_key.is_none() {
            return structured(json!({
                "configured": true,
                "project_id": project_id,
                "dry_run": false,
                "applied": false,
                "refused": true,
                "reason": "no review-panel transport configured (REVIEW_DAEMON_TOKEN / \
OPENROUTER_API_KEY both unset) -- refusing to crystallize without an adversarial check",
                "candidate_count": candidates.len(),
            }));
        }

        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut promoted = Vec::new();
        let mut refuted = Vec::new();
        let mut incomplete = Vec::new();

        for finding in candidates {
            let review_args = build_promotion_review_args(&providers, finding);
            let review_result: Value = match ReviewRun::new().execute(review_args).await {
                Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| {
                    json!({"aggregate_verdict": "UNKNOWN", "complete": false, "parse_error": true})
                }),
                Err(e) => json!({"aggregate_verdict": "UNKNOWN", "complete": false, "error": e.to_string()}),
            };
            let aggregate_verdict = review_result
                .get("aggregate_verdict")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");
            let complete = review_result.get("complete").and_then(|v| v.as_bool()).unwrap_or(false);

            match promotion_outcome(aggregate_verdict, complete) {
                PromotionOutcome::Incomplete => {
                    incomplete.push(json!({
                        "finding_id": finding.id.to_string(),
                        "reason": "promotion panel incomplete -- not marked, eligible again next cycle",
                    }));
                }
                PromotionOutcome::Refuted => {
                    findings_store.mark_crystallize_state(finding.id, "refuted").await?;
                    refuted.push(json!({
                        "finding_id": finding.id.to_string(),
                        "aggregate_verdict": aggregate_verdict,
                    }));
                }
                PromotionOutcome::Promoted => {
                    let classification = classify(&finding.description);
                    let write_result = match classification {
                        Classification::LintStub => write_lint_stub(repo_root, finding),
                        Classification::Prose => write_prose_rule(repo_root, finding),
                    };
                    match write_result {
                        Ok(artifact_path) => {
                            findings_store.mark_crystallize_state(finding.id, "promoted").await?;
                            promoted.push(json!({
                                "finding_id": finding.id.to_string(),
                                "classification": match classification {
                                    Classification::LintStub => "lint_stub",
                                    Classification::Prose => "prose",
                                },
                                "artifact": artifact_path,
                            }));
                        }
                        Err(e) => {
                            // Write failed -- do NOT mark the finding (never
                            // record an outcome for an artifact that was never
                            // actually produced); eligible again next cycle.
                            incomplete.push(json!({
                                "finding_id": finding.id.to_string(),
                                "reason": format!("artifact write failed, not marked: {e}"),
                            }));
                        }
                    }
                }
            }
        }

        structured(json!({
            "configured": true,
            "project_id": project_id,
            "dry_run": false,
            "applied": true,
            "min_recurrence": min_recurrence,
            "promoted": promoted,
            "refuted": refuted,
            "incomplete": incomplete,
        }))
    }
}

/// Register `cortex_crystallize` on the core registry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(CortexCrystallize));
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn finding(
        category: &str,
        occurrences: i32,
        description: &str,
        crystallize_state: Option<&str>,
    ) -> FindingRow {
        let now = chrono::Utc::now();
        FindingRow {
            id: Uuid::new_v4(),
            project_id: "TERM".to_string(),
            category: category.to_string(),
            severity: "low".to_string(),
            scope_kind: "path".to_string(),
            scope_ref: "src/lib.rs".to_string(),
            description: description.to_string(),
            provenance: json!([]),
            first_seen: now,
            last_seen: now,
            occurrences,
            crystallize_state: crystallize_state.map(|s| s.to_string()),
        }
    }

    // ── select_candidates: threshold, category, state ──────────────────────

    #[test]
    fn select_candidates_respects_recurrence_threshold() {
        let findings = vec![
            finding("consistency", 2, "a", None),
            finding("consistency", 3, "b", None),
            finding("consistency", 5, "c", None),
        ];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|f| f.occurrences >= 3));
    }

    #[test]
    fn select_candidates_excludes_non_eligible_categories() {
        let findings = vec![
            finding("consistency", 5, "a", None),
            finding("elegance", 5, "b", None),
            finding("security", 5, "c", None),
            finding("bug", 5, "d", None),
        ];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|f| f.category == "consistency" || f.category == "elegance"));
    }

    #[test]
    fn select_candidates_category_check_is_case_insensitive() {
        let findings = vec![finding("Consistency", 5, "a", None), finding("ELEGANCE", 5, "b", None)];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_candidates_excludes_already_processed_refuted() {
        let findings = vec![
            finding("consistency", 5, "a", Some("refuted")),
            finding("consistency", 5, "b", None),
        ];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].description, "b");
    }

    #[test]
    fn select_candidates_excludes_already_promoted() {
        let findings = vec![
            finding("consistency", 5, "a", Some("promoted")),
            finding("consistency", 5, "b", None),
        ];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].description, "b");
    }

    #[test]
    fn select_candidates_converges_a_refuted_candidate_never_reselected() {
        // Simulates two crystallization cycles: cycle 1 sees the candidate,
        // "refutes" it (state set), cycle 2 must not see it again.
        let mut findings = vec![finding("consistency", 5, "flaky", None)];
        let cycle1 = select_candidates(&findings, 3);
        assert_eq!(cycle1.len(), 1);

        findings[0].crystallize_state = Some("refuted".to_string());
        let cycle2 = select_candidates(&findings, 3);
        assert!(cycle2.is_empty(), "a refuted candidate must not re-enter a later cycle");
    }

    #[test]
    fn select_candidates_empty_input_is_empty_output() {
        assert!(select_candidates(&[], 3).is_empty());
    }

    #[test]
    fn select_candidates_exact_threshold_boundary_included() {
        let findings = vec![finding("consistency", 3, "a", None)];
        assert_eq!(select_candidates(&findings, 3).len(), 1);
    }

    #[test]
    fn select_candidates_ordering_is_deterministic_by_occurrences_desc() {
        let findings = vec![
            finding("consistency", 3, "low", None),
            finding("elegance", 9, "high", None),
            finding("consistency", 5, "mid", None),
        ];
        let selected = select_candidates(&findings, 3);
        let occ: Vec<i32> = selected.iter().map(|f| f.occurrences).collect();
        assert_eq!(occ, vec![9, 5, 3]);
    }

    #[test]
    fn select_candidates_dedups_near_identical_text_across_scope_refs() {
        let mut a = finding("consistency", 5, "  Unused   import  found  ", None);
        let mut b = finding("consistency", 4, "unused import found", None);
        a.scope_ref = "src/a.rs".to_string();
        b.scope_ref = "src/b.rs".to_string();
        let findings = vec![a, b];
        let selected = select_candidates(&findings, 3);
        assert_eq!(selected.len(), 1, "near-identical text across scope_refs must collapse");
        assert_eq!(selected[0].occurrences, 5, "the higher-recurrence entry is kept");
    }

    #[test]
    fn select_candidates_does_not_dedup_genuinely_different_findings() {
        let findings = vec![
            finding("consistency", 5, "unused import", None),
            finding("elegance", 5, "duplicated logic", None),
        ];
        assert_eq!(select_candidates(&findings, 3).len(), 2);
    }

    // ── promotion_outcome: pure decision, default-refute on uncertainty ────

    #[test]
    fn promotion_outcome_complete_approve_is_promoted() {
        assert_eq!(promotion_outcome("APPROVE", true), PromotionOutcome::Promoted);
    }

    #[test]
    fn promotion_outcome_complete_request_changes_is_refuted() {
        assert_eq!(promotion_outcome("REQUEST_CHANGES", true), PromotionOutcome::Refuted);
    }

    #[test]
    fn promotion_outcome_incomplete_panel_never_promotes_even_on_approve() {
        // review_run's own aggregate CAN return APPROVE with complete:false
        // (majority survives despite one erroring provider) -- promotion
        // must still refuse to graduate an incomplete panel.
        assert_eq!(promotion_outcome("APPROVE", false), PromotionOutcome::Incomplete);
    }

    #[test]
    fn promotion_outcome_unknown_verdict_complete_panel_is_refuted_not_promoted() {
        // Default-refute on uncertainty: a complete panel with no strict
        // majority (review_run's own panel_majority fails safe to
        // REQUEST_CHANGES, never returns bare "UNKNOWN" when complete) must
        // never promote.
        assert_eq!(promotion_outcome("UNKNOWN", true), PromotionOutcome::Refuted);
    }

    #[test]
    fn promotion_outcome_incomplete_and_non_approve_is_incomplete_not_refuted() {
        // Incomplete always wins over the verdict token -- a panel that
        // never actually completed its adversarial argument is not a
        // definitive refutation.
        assert_eq!(promotion_outcome("REQUEST_CHANGES", false), PromotionOutcome::Incomplete);
    }

    // ── classify: deterministic, both branches reachable ────────────────────

    #[test]
    fn classify_mechanical_marker_is_lint_stub() {
        assert_eq!(classify("raw std::env::var(\"FOO_TOKEN\") inline"), Classification::LintStub);
        assert_eq!(classify("uses panic! on bad input"), Classification::LintStub);
        assert_eq!(classify("calls .unwrap() on unvalidated input"), Classification::LintStub);
    }

    #[test]
    fn classify_case_insensitive_marker_match() {
        assert_eq!(classify("Raw STD::ENV::VAR usage"), Classification::LintStub);
    }

    #[test]
    fn classify_non_mechanical_description_is_prose() {
        assert_eq!(classify("inconsistent naming convention across modules"), Classification::Prose);
        assert_eq!(classify("prefer composition over a god-object pattern"), Classification::Prose);
    }

    #[test]
    fn classify_is_deterministic() {
        let d = "raw std::env::var read";
        assert_eq!(classify(d), classify(d));
    }

    // ── build_promotion_criteria / review_args: pure prompt construction ───

    #[test]
    fn promotion_criteria_instructs_refute_and_default_refute_on_uncertainty() {
        let f = finding("consistency", 4, "raw std::env::var read", None);
        let criteria = build_promotion_criteria(&f);
        assert!(criteria.contains("REFUTE"));
        assert!(criteria.to_uppercase().contains("DEFAULT"));
        assert!(criteria.contains("VERDICT: REQUEST_CHANGES"));
        assert!(criteria.contains("VERDICT: APPROVE"));
    }

    #[test]
    fn promotion_review_args_uses_panel_majority_structure() {
        let f = finding("elegance", 4, "duplicated logic", None);
        let providers = vec!["codex".to_string(), "agy".to_string(), "nemotron".to_string()];
        let args = build_promotion_review_args(&providers, &f);
        assert_eq!(args["structure"], "panel_majority");
        assert_eq!(args["providers"], json!(providers));
        assert_eq!(args["context"]["finding_id"], f.id.to_string());
        assert_eq!(args["context"]["category"], "elegance");
    }

    #[test]
    fn default_promotion_panel_has_three_distinct_entries() {
        assert_eq!(DEFAULT_PROMOTION_PANEL.len(), 3);
        let unique: std::collections::HashSet<&str> = DEFAULT_PROMOTION_PANEL.iter().copied().collect();
        assert_eq!(unique.len(), 3);
    }

    // ── rendering: pure, both artifact shapes ───────────────────────────────

    #[test]
    fn render_lint_stub_entry_carries_scope_and_status_marker() {
        let f = finding("consistency", 4, "raw std::env::var read", None);
        let rendered = render_lint_stub_entry(&f);
        assert!(rendered.contains("src/lib.rs"));
        assert!(rendered.contains("TERM"));
        assert!(rendered.contains("scaffold only"));
        assert!(rendered.contains(&f.id.to_string()));
    }

    #[test]
    fn render_prose_rule_entry_carries_guidance_and_recurrence() {
        let f = finding("elegance", 6, "duplicated logic across modules", None);
        let rendered = render_prose_rule_entry(&f);
        assert!(rendered.contains("duplicated logic across modules"));
        assert!(rendered.contains("6 time(s)"));
        assert!(rendered.contains(&f.id.to_string()));
    }

    // ── append_under_section: file I/O against a tmp dir, header once ──────

    #[test]
    fn append_under_section_creates_file_with_header_once() {
        let dir = std::env::temp_dir().join(format!("cxeg09-append-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");

        append_under_section(&path, "## Header", "intro\n", "entry-one\n").unwrap();
        append_under_section(&path, "## Header", "intro\n", "entry-two\n").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("## Header").count(), 1, "header appended exactly once");
        assert!(contents.contains("entry-one"));
        assert!(contents.contains("entry-two"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_lint_stub_and_write_prose_rule_land_in_distinct_files() {
        let dir = std::env::temp_dir().join(format!("cxeg09-write-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let f = finding("consistency", 4, "raw std::env::var read", None);
        let lint_path = write_lint_stub(&dir, &f).unwrap();
        let prose_path = write_prose_rule(&dir, &f).unwrap();

        assert!(lint_path.ends_with(CANDIDATE_LINT_STUB_FILE));
        assert!(prose_path.ends_with(HOUSE_STYLE_DOC_FILE));
        assert!(std::path::Path::new(&lint_path).exists());
        assert!(std::path::Path::new(&prose_path).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── candidate_summary: pure projection ──────────────────────────────────

    #[test]
    fn candidate_summary_includes_would_classify_as() {
        let f = finding("consistency", 4, "raw std::env::var read", None);
        let v = candidate_summary(&f);
        assert_eq!(v["would_classify_as"], "lint_stub");
        assert_eq!(v["occurrences"], 4);
    }

    // ── cortex_crystallize tool: degrade + validation ───────────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn crystallize_unconfigured_store_degrades_not_errors() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let out = CortexCrystallize
            .execute_structured(json!({"project_id": "TERM"}))
            .await
            .unwrap();
        let v = out.structured.expect("structured payload");
        assert_eq!(v["configured"], false);
    }

    #[tokio::test]
    async fn crystallize_missing_project_id_is_invalid_argument() {
        let err = CortexCrystallize.execute_structured(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn crystallize_empty_providers_array_is_invalid_argument() {
        let err = CortexCrystallize
            .execute_structured(json!({"project_id": "TERM", "providers": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── registration ──────────────────────────────────────────────────────

    #[test]
    fn test_crystallize_registers() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("cortex_crystallize"));
    }
}
