//! CXEG-07: Tier-C consistency/elegance lens for `review_run` -- ADVISORY
//! ONLY, never blocking.
//!
//! Injects CXEG-06's house-style exemplars (`cortex::house_style`) and
//! CXEG-04's structural risk signals (`cortex::review::compute_review`) for
//! the touched community/ies into a dedicated, pinned-provider prompt that
//! asks the reviewer to flag deviations FROM THIS REPO'S OWN patterns --
//! never a generic style opinion, and never a rule this codebase doesn't
//! itself already exhibit. Findings are captured via the existing KGFIND-03
//! `FindingsStore` record path (see `review::mod::maybe_record_findings`);
//! this module never opens a second findings-access path (S9).
//!
//! ## The load-bearing safety property
//! [`maybe_run`] is called from `review::mod::execute` strictly AFTER
//! `aggregate()` has already produced `aggregate_verdict`/`complete` from the
//! correctness panel. Nothing in this module can reach back and mutate those
//! values -- a lens/exemplar/provider failure degrades to an empty
//! [`ConsistencyRun`] (see `status`), never an `Err`, and the caller's
//! `aggregate_verdict`/`complete` are computed and fixed before this module
//! is even invoked. `CortexConfig::elegance_advisory_only` (default `true`)
//! documents this same posture at the config layer; this module doesn't need
//! to branch on it to stay advisory -- it structurally CANNOT flip the
//! verdict from where it runs.
//!
//! ## Pinning
//! [`ConsistencyReviewConfig::from_env`] fixes the lens's provider
//! (`CONSISTENCY_REVIEW_PROVIDER`, default a cheap/stable free-tier model)
//! and a target temperature (`CONSISTENCY_REVIEW_TEMPERATURE`, default
//! `0.0`). The provider pin is a hard guarantee (dispatch always targets
//! exactly this one provider, routed through the same
//! `dispatch::is_daemon_provider`/`openrouter_model_for`/`"free"` table
//! `review::mod::run_one_provider` uses -- via
//! [`super::dispatch_provider_raw`], S9 single-source). The temperature pin
//! is currently BEST-EFFORT ONLY: neither `ReviewConfig::dispatch_daemon` nor
//! `ReviewConfig::dispatch_openrouter` (`review::dispatch`) expose a
//! temperature knob today, so it is surfaced to the model as an explicit
//! prompt instruction rather than an API parameter -- a known, documented
//! gap (see `README.md`'s consistency-lens section) rather than a silent
//! over-claim.
//!
//! ## Disagreement, not escalation
//! [`merge_and_flag_disagreement`] groups every consistency/elegance-tagged
//! finding -- the lens's own output PLUS any correctness reviewer that
//! independently tagged `category: consistency|elegance` via the existing
//! KGFIND-02 `FINDINGS_JSON:` mechanism -- by `(category, file, symbol)`. A
//! group with two or more DISTINCT sources reporting DIFFERING description
//! text is marked `subjective: true` on every entry in that group. Per this
//! item's scope, a subjective finding is captured, never escalated --
//! CXEG-08 (if ever built) is the consumer of any future escalation signal.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::aggregate::{Finding, ProviderResult};
use super::dispatch::ReviewConfig;
use super::{kg_context, prompt};
use crate::cortex::house_style::HouseStyleCache;
use crate::cortex::review::compute_review as cortex_compute_review;
use crate::cortex::CortexConfig;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;

/// Sentinel the lens's prompt asks for, distinct from the correctness
/// lens's `FINDINGS_JSON:` marker (see `prompt::parse_findings_with_marker`).
const CONSISTENCY_MARKER: &str = "CONSISTENCY_FINDINGS_JSON:";

/// Default lens provider when `CONSISTENCY_REVIEW_PROVIDER` is unset/empty --
/// a free-tier, code-specialized OpenRouter model (see `dispatch::QWEN_CODER_MODEL`),
/// chosen as a cheap/stable default rather than one of the paid CLI-backed
/// daemon providers.
const DEFAULT_PROVIDER: &str = "qwen_coder";

/// Default target temperature when `CONSISTENCY_REVIEW_TEMPERATURE` is
/// unset/unparseable -- low/deterministic, matching this lens's "cite what's
/// actually there, don't improvise" posture.
const DEFAULT_TEMPERATURE: f64 = 0.0;

/// Bounds how many touched communities a single review can pull exemplars
/// for -- mirrors `cortex::MAX_HOUSE_STYLE_COMMUNITIES`'s "cap, don't
/// silently do unbounded work" posture, sized much smaller since this is
/// injected into a review PROMPT (token budget), not a standalone report.
const MAX_TOUCHED_COMMUNITIES: usize = 5;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Pins the Tier-C consistency lens's provider + target temperature. See the
/// module doc's "Pinning" section for what is a hard guarantee vs. best-effort.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsistencyReviewConfig {
    pub provider: String,
    pub temperature: f64,
}

impl ConsistencyReviewConfig {
    pub fn from_env() -> Self {
        let provider = std::env::var("CONSISTENCY_REVIEW_PROVIDER")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
        let temperature = std::env::var("CONSISTENCY_REVIEW_TEMPERATURE")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(DEFAULT_TEMPERATURE);
        Self { provider, temperature }
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// One consistency/elegance finding plus its disagreement flag and which
/// source(s) contributed it -- `source` is `"lens:<provider>"` or
/// `"reviewer:<provider>"` (see [`merge_and_flag_disagreement`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ConsistencyFinding {
    pub finding: Finding,
    pub source: String,
    pub subjective: bool,
}

/// The full outcome of one [`maybe_run`] call, including a `status` label so
/// the caller can surface WHY zero findings came back (disabled vs. no
/// project vs. degraded vs. a genuinely clean pass) rather than collapsing
/// every no-findings case into a single ambiguous empty list -- mirrors
/// `cortex_review`'s own `findings: "unavailable"` vs `"empty"` distinction.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsistencyRun {
    pub status: &'static str,
    pub provider: Option<String>,
    pub degraded: bool,
    pub findings: Vec<ConsistencyFinding>,
}

impl ConsistencyRun {
    fn empty(status: &'static str) -> Self {
        ConsistencyRun { status, provider: None, degraded: false, findings: Vec::new() }
    }
}

// ---------------------------------------------------------------------------
// Exemplar + signal assembly
// ---------------------------------------------------------------------------

struct ExemplarContext {
    block: Value,
    degraded: bool,
}

/// Assemble the `{"house_style_exemplars": [...], "risk_signals": [...]}`
/// block for the touched communities in `project_id`'s Atlas graph. `None`
/// when there is no stored graph, no touched community, or every touched
/// community is too small a sample to trust (`profile != "ready"`) -- never
/// fabricates exemplars for an unstable/absent community (see module doc).
async fn assemble(
    project_id: &str,
    changed_files: &[String],
    config: &CortexConfig,
    cache: &HouseStyleCache,
) -> Option<ExemplarContext> {
    let store = GraphStore::from_config(&ScribeConfig::from_env());
    let graph = match store.load(project_id) {
        Ok(Some(g)) => g,
        Ok(None) | Err(_) => return None,
    };

    let changed: HashSet<&str> = changed_files.iter().map(String::as_str).collect();
    let mut communities: Vec<u32> = graph
        .current_nodes()
        .filter(|n| changed.contains(n.path.as_str()))
        .filter_map(|n| n.cluster)
        .collect();
    communities.sort_unstable();
    communities.dedup();
    communities.truncate(MAX_TOUCHED_COMMUNITIES);

    if communities.is_empty() {
        return None;
    }

    let mut degraded = false;
    let mut exemplars = Vec::new();
    for community in &communities {
        let profile = cache.get_or_compute(&graph, project_id, *community, config.house_style_exemplars_k).await;
        if profile.profile != "ready" {
            // Too small a sample to trust -- skip this scope rather than
            // fabricate a rule for it; the OTHER touched communities (if
            // any) are unaffected.
            degraded = true;
            continue;
        }
        degraded |= profile.degraded || profile.sparse;
        exemplars.push(json!({
            "community": community,
            "modal_facts": profile.facts,
            "exemplars_by_kind": profile.exemplars_by_kind,
        }));
    }

    if exemplars.is_empty() {
        // Every touched community was unstable -- nothing usable to compare
        // against; a total skip, not a degraded-but-present block.
        return None;
    }

    let review = cortex_compute_review(project_id, changed_files, config, false).await;
    let risk_signals = review.get("risk_signals").cloned().unwrap_or_else(|| json!([]));

    Some(ExemplarContext {
        block: json!({"house_style_exemplars": exemplars, "risk_signals": risk_signals}),
        degraded,
    })
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build the Tier-C consistency lens prompt: criteria + the exemplar/signal
/// block assembled by [`assemble`], with an explicit instruction to flag
/// ONLY deviations from what is actually evidenced, and to end (optionally)
/// with a `CONSISTENCY_FINDINGS_JSON:` block distinct from the correctness
/// lens's `FINDINGS_JSON:` sentinel.
fn build_prompt_block(criteria: &str, block: &Value, cfg: &ConsistencyReviewConfig) -> String {
    let block_str = serde_json::to_string_pretty(block).unwrap_or_else(|_| block.to_string());
    format!(
        "You are reviewing a code change for CONSISTENCY with THIS REPOSITORY'S OWN \
established patterns -- not generic style opinion, and not a pattern this codebase \
doesn't already use.\n\n\
Criteria for this change:\n{criteria}\n\n\
This codebase's own house-style exemplars (from cortex_house_style) for the touched \
community/ies, plus structural risk signals for this change (from cortex_review):\n\
{block_str}\n\n\
Flag ONLY deviations from the patterns evidenced above. If a scope has no exemplars to \
compare against, say so rather than inventing a rule. Respond with a short analysis, \
then optionally end with EXACTLY one line, verbatim:\n\
{CONSISTENCY_MARKER} [...]\n\
followed by a JSON array of {{\"category\":\"consistency\"|\"elegance\", \"severity\":..., \
\"file\":..., \"symbol\":..., \"description\":...}} objects (an empty array `[]` if none). \
This output is ADVISORY ONLY -- it never blocks or changes this change's correctness \
verdict, and is captured for pattern-tracking purposes only. Aim for a deterministic, \
low-temperature judgment (target temperature {temp}); cite the specific exemplar(s) your \
finding deviates from rather than asserting a preference.",
        temp = cfg.temperature
    )
}

/// This repo's own category vocabulary for the consistency lens -- any other
/// (or missing/blank) category the model emits is coerced to `"consistency"`
/// rather than silently dropped or left non-conforming, since the lens's
/// entire purpose is emitting consistency/elegance findings (unlike the
/// general-purpose correctness `FINDINGS_JSON:` categories, which are
/// free-form).
fn normalize_category(raw: &str) -> String {
    if raw.trim().eq_ignore_ascii_case("elegance") {
        "elegance".to_string()
    } else {
        "consistency".to_string()
    }
}

fn is_consistency_like(category: &str) -> bool {
    matches!(category.trim().to_ascii_lowercase().as_str(), "consistency" | "elegance")
}

// ---------------------------------------------------------------------------
// Disagreement grouping
// ---------------------------------------------------------------------------

/// Group the lens's own findings plus any correctness reviewer's
/// independently-tagged `consistency`/`elegance` findings by `(category,
/// file, symbol)`; a group with 2+ DISTINCT sources reporting DIFFERING
/// description text is flagged `subjective: true` on every entry in it.
/// Capture-only -- never escalates, never drops a finding, never mutates
/// `panel_results` (see module doc's "Disagreement, not escalation").
fn merge_and_flag_disagreement(
    lens_provider: &str,
    lens_findings: &[Finding],
    panel_results: &[ProviderResult],
) -> Vec<ConsistencyFinding> {
    #[derive(Default)]
    struct Group {
        entries: Vec<(String, Finding)>,
    }

    let mut groups: std::collections::BTreeMap<(String, Option<String>, Option<String>), Group> = std::collections::BTreeMap::new();

    for f in lens_findings {
        // Normalize category case/whitespace for the grouping key so a
        // reviewer's "Consistency" and the lens's "consistency" collapse into
        // the SAME group (agy review finding). file/symbol stay case-sensitive
        // (paths/identifiers are case-significant on Linux).
        let key = (
            f.category.trim().to_ascii_lowercase(),
            f.file.clone(),
            f.symbol.clone(),
        );
        groups.entry(key).or_default().entries.push((format!("lens:{lens_provider}"), f.clone()));
    }
    for r in panel_results {
        for f in &r.findings {
            if !is_consistency_like(&f.category) {
                continue;
            }
            // Normalize category case/whitespace for the grouping key so a
        // reviewer's "Consistency" and the lens's "consistency" collapse into
        // the SAME group (agy review finding). file/symbol stay case-sensitive
        // (paths/identifiers are case-significant on Linux).
        let key = (
            f.category.trim().to_ascii_lowercase(),
            f.file.clone(),
            f.symbol.clone(),
        );
            groups.entry(key).or_default().entries.push((format!("reviewer:{}", r.provider), f.clone()));
        }
    }

    let mut out = Vec::new();
    for group in groups.into_values() {
        let distinct_sources: HashSet<&str> = group.entries.iter().map(|(s, _)| s.as_str()).collect();
        let distinct_descriptions: HashSet<&str> = group.entries.iter().map(|(_, f)| f.description.as_str()).collect();
        let subjective = distinct_sources.len() > 1 && distinct_descriptions.len() > 1;
        for (source, finding) in group.entries {
            out.push(ConsistencyFinding { finding, source, subjective });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the Tier-C consistency lens for one `review_run` call, or degrade to
/// an empty, labeled [`ConsistencyRun`] -- NEVER an error, and never able to
/// influence the correctness verdict (see module doc). Must be called AFTER
/// `aggregate()` has already produced the correctness `aggregate_verdict`.
pub async fn maybe_run(
    context: &Value,
    criteria: &str,
    panel_results: &[ProviderResult],
    review_cfg: &ReviewConfig,
    cortex_config: &CortexConfig,
    house_style_cache: &HouseStyleCache,
) -> ConsistencyRun {
    if !cortex_config.enable_tier_c {
        return ConsistencyRun::empty("disabled");
    }

    let Some(project_id) = context.get("project_id").and_then(Value::as_str).map(str::to_string) else {
        return ConsistencyRun::empty("no_project_id");
    };

    let changed_files = kg_context::derive_changed_files(context);
    if changed_files.is_empty() {
        return ConsistencyRun::empty("no_changed_files");
    }

    let Some(ctx) = assemble(&project_id, &changed_files, cortex_config, house_style_cache).await else {
        tracing::info!(
            "CXEG-07: no usable house-style exemplars for '{project_id}' (no stored graph, \
             no touched community, or every touched community too small a sample) -- \
             consistency lens skipped for this review, correctness gate unaffected"
        );
        return ConsistencyRun::empty("no_graph_or_exemplars");
    };

    let lens_cfg = ConsistencyReviewConfig::from_env();
    let prompt_text = build_prompt_block(criteria, &ctx.block, &lens_cfg);

    let raw = match super::dispatch_provider_raw(review_cfg, &lens_cfg.provider, &prompt_text).await {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(
                "CXEG-07: consistency lens provider '{}' unavailable ({e}) -- \
                 skipping, correctness gate unaffected",
                lens_cfg.provider
            );
            return ConsistencyRun {
                status: "lens_unavailable",
                provider: Some(lens_cfg.provider),
                degraded: ctx.degraded,
                findings: Vec::new(),
            };
        }
    };

    let lens_findings: Vec<Finding> = prompt::parse_findings_with_marker(&raw, CONSISTENCY_MARKER)
        .into_iter()
        .map(|mut f| {
            f.category = normalize_category(&f.category);
            f
        })
        .collect();

    let findings = merge_and_flag_disagreement(&lens_cfg.provider, &lens_findings, panel_results);

    ConsistencyRun { status: "ok", provider: Some(lens_cfg.provider), degraded: ctx.degraded, findings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{KgNode, KnowledgeGraph, NodeKind};

    fn cortex_cfg(enable_tier_c: bool) -> CortexConfig {
        CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: false,
            enable_tier_c,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: crate::cortex::house_style::DEFAULT_EXEMPLARS_K,
            risk_weight_centrality_spike: 2.0,
            risk_weight_complexity_spike: 1.5,
            risk_weight_fan_out_explosion: 1.5,
            risk_weight_community_boundary_crossing: 2.5,
            risk_weight_semantic_duplication: 10.0,
            risk_weight_recurrence: 1.0,
            risk_band_elevated_cut: 4.0,
            audit_clone_timeout_secs: 60,
            audit_max_clone_bytes: 200_000_000,
            escalation_enabled: true,
            escalation_add_provider: "agy".to_string(),
        }
    }

    fn finding(category: &str, file: Option<&str>, symbol: Option<&str>, description: &str) -> Finding {
        Finding {
            category: category.to_string(),
            severity: "medium".to_string(),
            file: file.map(str::to_string),
            symbol: symbol.map(str::to_string),
            description: description.to_string(),
            subjective: None,
        }
    }

    fn provider_with(provider: &str, findings: Vec<Finding>) -> ProviderResult {
        ProviderResult {
            provider: provider.to_string(),
            verdict: "REQUEST_CHANGES".to_string(),
            reasoning: String::new(),
            error: None,
            findings,
        }
    }

    // ── ConsistencyReviewConfig ─────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn config_defaults_when_unset() {
        std::env::remove_var("CONSISTENCY_REVIEW_PROVIDER");
        std::env::remove_var("CONSISTENCY_REVIEW_TEMPERATURE");
        let cfg = ConsistencyReviewConfig::from_env();
        assert_eq!(cfg.provider, DEFAULT_PROVIDER);
        assert_eq!(cfg.temperature, DEFAULT_TEMPERATURE);
    }

    #[test]
    #[serial_test::serial]
    fn config_reads_env_overrides() {
        std::env::set_var("CONSISTENCY_REVIEW_PROVIDER", "nemotron");
        std::env::set_var("CONSISTENCY_REVIEW_TEMPERATURE", "0.2");
        let cfg = ConsistencyReviewConfig::from_env();
        assert_eq!(cfg.provider, "nemotron");
        assert_eq!(cfg.temperature, 0.2);
        std::env::remove_var("CONSISTENCY_REVIEW_PROVIDER");
        std::env::remove_var("CONSISTENCY_REVIEW_TEMPERATURE");
    }

    // ── normalize_category / is_consistency_like ────────────────────────

    #[test]
    fn normalize_category_keeps_elegance_case_insensitively() {
        assert_eq!(normalize_category("elegance"), "elegance");
        assert_eq!(normalize_category("Elegance"), "elegance");
    }

    #[test]
    fn normalize_category_coerces_anything_else_to_consistency() {
        assert_eq!(normalize_category("style"), "consistency");
        assert_eq!(normalize_category(""), "consistency");
        assert_eq!(normalize_category("bug"), "consistency");
    }

    #[test]
    fn is_consistency_like_matches_only_the_two_categories() {
        assert!(is_consistency_like("consistency"));
        assert!(is_consistency_like("Elegance"));
        assert!(!is_consistency_like("bug"));
        assert!(!is_consistency_like("style"));
    }

    // ── merge_and_flag_disagreement ──────────────────────────────────────

    #[test]
    fn single_source_finding_is_never_subjective() {
        let lens = vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "deviates from from_env idiom")];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &[]);
        assert_eq!(out.len(), 1);
        assert!(!out[0].subjective);
        assert_eq!(out[0].source, "lens:qwen_coder");
    }

    #[test]
    fn two_sources_agreeing_verbatim_is_not_subjective() {
        let lens = vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "missing from_env idiom")];
        let panel = vec![provider_with(
            "opus",
            vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "missing from_env idiom")],
        )];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &panel);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| !f.subjective), "identical descriptions from two sources is agreement, not disagreement");
    }

    #[test]
    fn two_sources_disagreeing_is_flagged_subjective_on_both_entries() {
        let lens = vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "this deviates from house style")];
        let panel = vec![provider_with(
            "opus",
            vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "this is fine, matches other exemplars")],
        )];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &panel);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| f.subjective), "differing descriptions at the same anchor must all be flagged subjective");
    }

    #[test]
    fn category_case_mismatch_still_groups_and_flags_subjective() {
        // agy review finding: a reviewer emitting "Consistency" (capitalized)
        // and the lens emitting "consistency" must collapse into ONE group so
        // the disagreement is detected — the grouping key normalizes case.
        let lens = vec![finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "deviates from house style")];
        let panel = vec![provider_with(
            "opus",
            vec![finding("Consistency", Some("src/a.rs"), Some("crate::a::foo"), "looks fine to me")],
        )];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &panel);
        assert_eq!(out.len(), 2, "case-variant categories at the same anchor must group together");
        assert!(out.iter().all(|f| f.subjective), "a case-only category difference must not hide a real disagreement");
    }

    #[test]
    fn panel_findings_outside_consistency_categories_are_ignored() {
        let lens = vec![];
        let panel = vec![provider_with("opus", vec![finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "off-by-one")])];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &panel);
        assert!(out.is_empty(), "a plain 'bug' finding must never be pulled into the consistency merge");
    }

    #[test]
    fn distinct_anchors_are_not_grouped_together() {
        let lens = vec![
            finding("consistency", Some("src/a.rs"), Some("crate::a::foo"), "d1"),
            finding("consistency", Some("src/b.rs"), Some("crate::b::bar"), "d2"),
        ];
        let out = merge_and_flag_disagreement("qwen_coder", &lens, &[]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| !f.subjective));
    }

    // ── build_prompt_block ───────────────────────────────────────────────

    #[test]
    fn prompt_block_contains_criteria_block_and_marker_never_generic_taste() {
        let block = json!({"house_style_exemplars": [{"community": 1}], "risk_signals": []});
        let cfg = ConsistencyReviewConfig { provider: "qwen_coder".to_string(), temperature: 0.0 };
        let p = build_prompt_block("must compile", &block, &cfg);
        assert!(p.contains("must compile"));
        assert!(p.contains("house_style_exemplars"));
        assert!(p.contains(CONSISTENCY_MARKER));
        assert!(p.to_lowercase().contains("advisory only"));
        assert!(p.to_lowercase().contains("this codebase"));
    }

    // ── assemble: degrade contract (no live network/DB needed) ──────────

    fn tmp_store(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("atlas-consistency-test-{}-{}", tag, std::process::id()))
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn assemble_none_when_no_stored_graph() {
        let store_dir = tmp_store("nograph");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let cache = HouseStyleCache::new();
        let out = assemble("TERM", &["src/a.rs".to_string()], &cortex_cfg(true), &cache).await;
        assert!(out.is_none());
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn assemble_none_when_no_community_touched() {
        let store_dir = tmp_store("nocommunity");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        let mut g = KnowledgeGraph::new("TERM");
        // No `cluster` set -> filter_map(|n| n.cluster) yields nothing.
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs"));
        store.save("TERM", &g).unwrap();

        let cache = HouseStyleCache::new();
        let out = assemble("TERM", &["src/a.rs".to_string()], &cortex_cfg(true), &cache).await;
        assert!(out.is_none());

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn assemble_none_when_touched_community_is_unstable() {
        let store_dir = tmp_store("unstable");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        let mut g = KnowledgeGraph::new("TERM");
        // A single-member community is below house_style::MIN_COMMUNITY_SIZE (2).
        let mut n = KgNode::new("crate::a::solo", NodeKind::Function, "solo", "src/a.rs");
        n.cluster = Some(9);
        g.insert_node(n);
        store.save("TERM", &g).unwrap();

        let cache = HouseStyleCache::new();
        let out = assemble("TERM", &["src/a.rs".to_string()], &cortex_cfg(true), &cache).await;
        assert!(out.is_none(), "an unstable-only touched community must never fabricate exemplars");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // ── maybe_run: gating + degrade (no live daemon/OpenRouter needed) ──

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_run_disabled_when_enable_tier_c_false_is_a_clean_noop() {
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let review_cfg = ReviewConfig::default();
        let cache = HouseStyleCache::new();
        let run = maybe_run(&context, "criteria", &[], &review_cfg, &cortex_cfg(false), &cache).await;
        assert_eq!(run.status, "disabled");
        assert!(run.findings.is_empty());
        assert_eq!(run.provider, None);
    }

    #[tokio::test]
    async fn maybe_run_no_project_id() {
        let context = json!({"changed_files": ["src/a.rs"]});
        let review_cfg = ReviewConfig::default();
        let cache = HouseStyleCache::new();
        let run = maybe_run(&context, "criteria", &[], &review_cfg, &cortex_cfg(true), &cache).await;
        assert_eq!(run.status, "no_project_id");
    }

    #[tokio::test]
    async fn maybe_run_no_changed_files() {
        let context = json!({"project_id": "TERM"});
        let review_cfg = ReviewConfig::default();
        let cache = HouseStyleCache::new();
        let run = maybe_run(&context, "criteria", &[], &review_cfg, &cortex_cfg(true), &cache).await;
        assert_eq!(run.status, "no_changed_files");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_run_no_graph_or_exemplars_degrades_cleanly() {
        let store_dir = tmp_store("mayberun-nograph");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let review_cfg = ReviewConfig::default();
        let cache = HouseStyleCache::new();
        let run = maybe_run(&context, "criteria", &[], &review_cfg, &cortex_cfg(true), &cache).await;
        assert_eq!(run.status, "no_graph_or_exemplars");
        assert!(run.findings.is_empty());
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_run_lens_unavailable_when_no_daemon_or_openrouter_configured() {
        let store_dir = tmp_store("mayberun-unavail");
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let store = GraphStore::from_config(&ScribeConfig::from_env());
        let mut g = KnowledgeGraph::new("TERM");
        for i in 0..3 {
            let mut n = KgNode::new(format!("crate::a::f{i}"), NodeKind::Function, format!("f{i}"), "src/a.rs");
            n.cluster = Some(1);
            g.insert_node(n);
        }
        store.save("TERM", &g).unwrap();

        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::set_var("EMBEDDINGS_URL", "http://127.0.0.1:1/v1/embeddings"); // pii-test-fixture

        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let review_cfg = ReviewConfig::from_env();
        let cache = HouseStyleCache::new();
        let run = maybe_run(&context, "criteria", &[], &review_cfg, &cortex_cfg(true), &cache).await;
        // house-style exemplar selection degrades (embeddings unreachable) but
        // still returns exemplars (centrality fallback) since community size
        // is 3 -- so we reach the dispatch step and hit the unavailable path.
        assert_eq!(run.status, "lens_unavailable", "{run:?}");
        assert!(run.findings.is_empty());

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
        std::env::remove_var("EMBEDDINGS_URL");
    }
}
