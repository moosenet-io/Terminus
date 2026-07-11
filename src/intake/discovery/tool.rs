//! DISC-02 (S114, TERM #252): `model_discovery_brochure` — the read-only MCP
//! core tool every agent (Lumina, Harmony, a reviewer) queries to discover new
//! HuggingFace model CANDIDATES for the gfx1151 fleet.
//!
//! Mirrors [`crate::intake::catalog`]'s read-side pattern exactly: a PURE
//! filter/render layer ([`filter_candidates`] / [`render_brochure_json`] /
//! [`render_brochure_markdown`]) over plain owned [`DiscoveryCandidate`]
//! structs — unit-testable without a live Postgres — plus a thin impure
//! [`ModelDiscoveryBrochure::run`] that does the one read via
//! [`crate::intake::discovery::storage::read_brochure`] and the one shared
//! pool via [`crate::intake::storage::get_pool`] (never a second pool).
//!
//! **The brochure/catalog distinction** (also stated in this tool's own
//! [`RustTool::description`]): this tool is the standing registry of
//! HuggingFace model CANDIDATES for the gfx1151 fleet — distinct from
//! `model_fleet_catalog` ([`crate::intake::catalog`]), which reports what has
//! been TESTED and how it scored. Query this tool first to discover new
//! models; query `model_fleet_catalog` to see test coverage/scores for a
//! model already in the fleet.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::intake::discovery::schema::{CandidateStatus, DiscoveryCandidate, FleetCategory};
use crate::intake::discovery::storage;
use crate::intake::storage as intake_storage;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// The optional filters the `model_discovery_brochure` tool accepts. All
/// `None` ⇒ every persisted candidate. Matches `catalog.rs::CatalogQuery`'s
/// "unknown filter value → empty + a note, never an error" convention for the
/// `model` exact-match filter (see [`filter_candidates`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrochureQuery {
    pub category: Option<FleetCategory>,
    pub status: Option<CandidateStatus>,
    pub min_discovery_score: Option<f64>,
    /// `acquire.rs::Gfx1151Class` value as plain text (`confirmed` |
    /// `experimental` | `unknown`) — see `schema.rs`'s note on why
    /// `gfx1151_class` stays a plain string in this module.
    pub gfx1151_class: Option<String>,
    /// Exact `model_name` match. Unknown ⇒ empty result + a note (the
    /// "unknown X" convention `catalog.rs::filter_cards` established).
    pub model: Option<String>,
}

/// Apply a [`BrochureQuery`] to the persisted candidates. PURE.
///
/// - `category`/`status`/`gfx1151_class` independently narrow the result set;
///   applying more than one is an AND.
/// - `min_discovery_score` keeps only candidates whose `discovery_score` is
///   present and `>=` the threshold. A candidate with no `discovery_score` at
///   all never matches a `min_discovery_score` filter. An empty result from
///   this filter carries NO note — a legitimate "nothing meets the bar" empty
///   result is not the same as the "unknown X" case (see EDGE CASES in the
///   DISC-02 spec item).
/// - `model` selects the one matching candidate; if none match, the result is
///   empty AND a note is set explaining the unknown model — the one filter
///   that distinguishes "you asked for something that doesn't exist" from "no
///   candidates meet your criteria."
/// - An `Evicted` candidate (its `retained_profile` populated) is never
///   hidden by default — only an explicit `status` filter excluding it
///   removes it from the result.
pub fn filter_candidates(
    candidates: &[DiscoveryCandidate],
    q: &BrochureQuery,
) -> (Vec<DiscoveryCandidate>, Option<String>) {
    let mut note = None;

    let selected: Vec<&DiscoveryCandidate> = match &q.model {
        Some(m) => {
            let v: Vec<&DiscoveryCandidate> =
                candidates.iter().filter(|c| &c.model_name == m).collect();
            if v.is_empty() {
                note = Some(format!("no such model '{m}' in the discovery brochure"));
            }
            v
        }
        None => candidates.iter().collect(),
    };

    let out: Vec<DiscoveryCandidate> = selected
        .into_iter()
        .filter(|c| q.category.map_or(true, |cat| c.category == cat))
        .filter(|c| q.status.map_or(true, |st| c.status == st))
        .filter(|c| {
            q.gfx1151_class
                .as_deref()
                .map_or(true, |g| c.gfx1151_class == g)
        })
        .filter(|c| match q.min_discovery_score {
            Some(min) => c.discovery_score.map_or(false, |s| s >= min),
            None => true,
        })
        .cloned()
        .collect();

    (out, note)
}

/// One candidate as an output JSON object. Shared by [`render_brochure_json`].
fn candidate_json(c: &DiscoveryCandidate) -> Value {
    json!({
        "model_name": c.model_name,
        "hf_repo": c.hf_repo,
        "category": c.category.as_str(),
        "status": c.status.as_str(),
        "gfx1151_class": c.gfx1151_class,
        "size_b": c.size_b,
        "vram_footprint_gb": c.vram_footprint_gb,
        "discovery_source": c.discovery_source,
        "discovery_score": c.discovery_score,
        "discovered_at": c.discovered_at,
        "last_seen_at": c.last_seen_at,
        "fetched_at": c.fetched_at,
        "marked_for_fleet_at": c.marked_for_fleet_at,
        "evicted_at": c.evicted_at,
        "retained_profile": c.retained_profile,
        "rationale": c.rationale,
    })
}

/// Render the (already-filtered) candidates as the tool's structured JSON.
/// PURE. Shape: `{ candidates: [...], note?, summary: { total } }`. Valid,
/// non-error JSON with an empty `candidates` array for a fresh/empty
/// brochure or a filter that legitimately matches nothing.
pub fn render_brochure_json(candidates: &[DiscoveryCandidate], note: Option<&str>) -> Value {
    let models: Vec<Value> = candidates.iter().map(candidate_json).collect();
    let mut out = json!({
        "candidates": models,
        "summary": {
            "total": candidates.len(),
        },
    });
    if let Some(n) = note {
        out.as_object_mut()
            .unwrap()
            .insert("note".to_string(), json!(n));
    }
    out
}

/// Render the (already-filtered) candidates as a compact markdown table:
/// model | category | status | gfx1151_class | vram_gb | discovery_score |
/// last_seen_at. PURE — a human/agent display for `format=markdown`. Valid
/// markdown (header + zero rows) for an empty brochure, never an error.
pub fn render_brochure_markdown(candidates: &[DiscoveryCandidate], note: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("# Model discovery brochure\n\n");
    if let Some(n) = note {
        s.push_str(&format!("_{n}_\n\n"));
    }
    s.push_str("| model | category | status | gfx1151_class | vram_gb | discovery_score | last_seen_at |\n");
    s.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    if candidates.is_empty() {
        s.push_str("| _(no candidates)_ | | | | | | |\n");
        return s;
    }
    for c in candidates {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            c.model_name,
            c.category.as_str(),
            c.status.as_str(),
            c.gfx1151_class,
            c.vram_footprint_gb
                .map(|v| format!("{v:.1}"))
                .unwrap_or_default(),
            c.discovery_score
                .map(|v| format!("{v:.2}"))
                .unwrap_or_default(),
            c.last_seen_at,
        ));
    }
    s
}

/// Parse + validate the tool args into a [`BrochureQuery`] and the output
/// format. Empty/whitespace string filters are treated as absent. An
/// unrecognized `category`/`status`/`format` enum value is a clean
/// [`ToolError::InvalidArgument`], never a panic or silent no-op.
fn parse_brochure_args(args: &Value) -> Result<(BrochureQuery, String), ToolError> {
    let opt_str = |k: &str| -> Option<String> {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let category = opt_str("category")
        .map(|s| FleetCategory::from_str(&s))
        .transpose()?;
    let status = opt_str("status")
        .map(|s| CandidateStatus::from_str(&s))
        .transpose()?;

    let format = opt_str("format").unwrap_or_else(|| "json".to_string());
    if format != "json" && format != "markdown" {
        return Err(ToolError::InvalidArgument(format!(
            "'format' must be 'json' or 'markdown', got '{format}'"
        )));
    }

    let min_discovery_score = match args.get("min_discovery_score") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_f64().ok_or_else(|| {
            ToolError::InvalidArgument(format!(
                "'min_discovery_score' must be a number, got {v}"
            ))
        })?),
    };

    let query = BrochureQuery {
        category,
        status,
        min_discovery_score,
        gfx1151_class: opt_str("gfx1151_class"),
        model: opt_str("model"),
    };
    Ok((query, format))
}

/// The `model_discovery_brochure` core Terminus tool: a read-only, SQL-free
/// window on the persisted discovery brochure.
pub struct ModelDiscoveryBrochure;

impl ModelDiscoveryBrochure {
    /// Shared read+filter+render used by both `execute` (text) and
    /// `execute_structured` (text + structured JSON). Reads the PERSISTED
    /// brochure (never recomputes/refreshes — DISC-06 owns the refresh); a
    /// not-yet-migrated host surfaces a clean [`ToolError::NotConfigured`]
    /// from [`storage::read_brochure`].
    async fn run(&self, args: Value) -> Result<(String, Option<Value>), ToolError> {
        let (query, format) = parse_brochure_args(&args)?;
        let pool = intake_storage::get_pool().await?;
        let candidates = storage::read_brochure(&pool).await?;
        let (filtered, note) = filter_candidates(&candidates, &query);
        if format == "markdown" {
            let md = render_brochure_markdown(&filtered, note.as_deref());
            Ok((md, None))
        } else {
            let value = render_brochure_json(&filtered, note.as_deref());
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            Ok((text, Some(value)))
        }
    }
}

#[async_trait]
impl RustTool for ModelDiscoveryBrochure {
    fn name(&self) -> &str {
        "model_discovery_brochure"
    }

    fn description(&self) -> &str {
        "Read the model discovery brochure — the standing registry of HuggingFace model \
         CANDIDATES for the gfx1151 fleet — WITHOUT SQL. This is distinct from \
         'model_fleet_catalog', which reports what has been TESTED and how it scored for \
         models already in the fleet: query THIS tool first to discover new models; query \
         'model_fleet_catalog' to see test coverage/scores for a model already in the fleet. \
         All filters optional: 'category' (tool_router|writer_slm|assistant|coder|embedding| \
         visual|voice), 'status' (discovered|fetching|cold_stored|marked_for_fleet|swept| \
         evicted|rejected), 'min_discovery_score' (numeric threshold), 'gfx1151_class' \
         (confirmed|experimental|unknown), 'model' (exact model_name match — unknown value \
         returns empty with a note, never an error). 'format' is 'json' (default, structured) \
         or 'markdown' (a compact table: model | category | status | gfx1151_class | vram_gb | \
         discovery_score | last_seen_at). Read-only; reads the persisted brochure (refreshed \
         daily by the DISC-06 discovery-refresh job)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "enum": ["tool_router", "writer_slm", "assistant", "coder", "embedding", "visual", "voice"],
                    "description": "Restrict to candidates targeting this fleet category."
                },
                "status": {
                    "type": "string",
                    "enum": ["discovered", "fetching", "cold_stored", "marked_for_fleet", "swept", "evicted", "rejected"],
                    "description": "Restrict to candidates with this lifecycle status."
                },
                "min_discovery_score": {
                    "type": "number",
                    "description": "Restrict to candidates whose discovery_score is >= this threshold. Candidates with no discovery_score never match."
                },
                "gfx1151_class": {
                    "type": "string",
                    "description": "Restrict to candidates with this gfx1151 fit class: 'confirmed', 'experimental', or 'unknown'."
                },
                "model": {
                    "type": "string",
                    "description": "Restrict to one candidate's exact model_name. Unknown model → empty candidates with a note."
                },
                "format": {
                    "type": "string",
                    "enum": ["json", "markdown"],
                    "description": "Output format. 'json' (default) is structured; 'markdown' renders a compact candidate table."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let (text, _structured) = self.run(args).await?;
        Ok(text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured })
    }
}

/// Register the read-only `model_discovery_brochure` tool on the CORE
/// registry (called from `crate::intake::discovery::register`, itself wired
/// into `crate::intake::register` — the same Chord-served core surface as
/// `model_fleet_catalog`). No personal registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelDiscoveryBrochure));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    fn candidate(
        model_name: &str,
        category: FleetCategory,
        status: CandidateStatus,
        gfx1151_class: &str,
        discovery_score: Option<f64>,
    ) -> DiscoveryCandidate {
        DiscoveryCandidate {
            model_name: model_name.to_string(),
            hf_repo: format!("org/{model_name}"),
            category,
            status,
            gfx1151_class: gfx1151_class.to_string(),
            size_b: Some(7.0),
            vram_footprint_gb: Some(4.2),
            discovery_source: "hf_trending".to_string(),
            discovery_score,
            discovered_at: ts(2026, 7, 1),
            last_seen_at: ts(2026, 7, 10),
            fetched_at: None,
            marked_for_fleet_at: None,
            evicted_at: None,
            retained_profile: None,
            rationale: None,
        }
    }

    fn fixture() -> Vec<DiscoveryCandidate> {
        vec![
            candidate(
                "alpha",
                FleetCategory::Coder,
                CandidateStatus::Discovered,
                "confirmed",
                Some(0.8),
            ),
            candidate(
                "beta",
                FleetCategory::Assistant,
                CandidateStatus::MarkedForFleet,
                "experimental",
                Some(0.3),
            ),
            {
                let mut c = candidate(
                    "gamma",
                    FleetCategory::Coder,
                    CandidateStatus::Evicted,
                    "unknown",
                    None,
                );
                c.retained_profile = Some(json!({"note": "pruned"}));
                c.evicted_at = Some(ts(2026, 7, 5));
                c
            },
        ]
    }

    #[test]
    fn no_filter_lists_every_candidate_once() {
        let (out, note) = filter_candidates(&fixture(), &BrochureQuery::default());
        assert_eq!(out.len(), 3);
        assert!(note.is_none());
    }

    #[test]
    fn status_filter_returns_only_that_status() {
        let q = BrochureQuery {
            status: Some(CandidateStatus::Discovered),
            ..Default::default()
        };
        let (out, note) = filter_candidates(&fixture(), &q);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model_name, "alpha");
        assert!(note.is_none());
    }

    #[test]
    fn category_filter_returns_only_that_category() {
        let q = BrochureQuery {
            category: Some(FleetCategory::Coder),
            ..Default::default()
        };
        let (out, _note) = filter_candidates(&fixture(), &q);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.category == FleetCategory::Coder));
    }

    #[test]
    fn combined_category_and_status_filters_are_anded() {
        let q = BrochureQuery {
            category: Some(FleetCategory::Coder),
            status: Some(CandidateStatus::Evicted),
            ..Default::default()
        };
        let (out, _note) = filter_candidates(&fixture(), &q);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model_name, "gamma");
    }

    #[test]
    fn unknown_model_filter_returns_empty_with_a_note() {
        let q = BrochureQuery {
            model: Some("does-not-exist".to_string()),
            ..Default::default()
        };
        let (out, note) = filter_candidates(&fixture(), &q);
        assert!(out.is_empty());
        assert!(note.unwrap().contains("does-not-exist"));
    }

    #[test]
    fn min_discovery_score_with_no_matches_is_empty_with_no_note() {
        // A legitimate "nothing meets the bar" empty result — must NOT carry
        // the "unknown X" note (the two empty-result conventions are distinct).
        let q = BrochureQuery {
            min_discovery_score: Some(0.99),
            ..Default::default()
        };
        let (out, note) = filter_candidates(&fixture(), &q);
        assert!(out.is_empty());
        assert!(note.is_none());
    }

    #[test]
    fn min_discovery_score_excludes_candidates_with_no_score() {
        // "gamma" has discovery_score = None; it must never match a
        // min_discovery_score filter, even a low threshold.
        let q = BrochureQuery {
            min_discovery_score: Some(0.0),
            ..Default::default()
        };
        let (out, _note) = filter_candidates(&fixture(), &q);
        assert!(out.iter().all(|c| c.model_name != "gamma"));
    }

    #[test]
    fn evicted_candidate_appears_by_default_unfiltered() {
        let (out, _note) = filter_candidates(&fixture(), &BrochureQuery::default());
        assert!(out.iter().any(|c| c.model_name == "gamma" && c.status == CandidateStatus::Evicted));
    }

    #[test]
    fn empty_brochure_renders_valid_json_with_zero_models() {
        let value = render_brochure_json(&[], None);
        assert_eq!(value["candidates"].as_array().unwrap().len(), 0);
        assert_eq!(value["summary"]["total"], 0);
        assert!(value.get("note").is_none());
    }

    #[test]
    fn empty_brochure_renders_valid_markdown_with_zero_models() {
        let md = render_brochure_markdown(&[], None);
        assert!(md.contains("| model | category | status"));
        assert!(md.contains("no candidates"));
    }

    #[test]
    fn render_json_includes_note_when_present() {
        let value = render_brochure_json(&[], Some("no such model 'x'"));
        assert_eq!(value["note"], "no such model 'x'");
    }

    #[test]
    fn render_markdown_has_header_and_rows() {
        let candidates = fixture();
        let md = render_brochure_markdown(&candidates, None);
        assert!(md.contains("| model | category | status | gfx1151_class | vram_gb | discovery_score | last_seen_at |"));
        assert!(md.contains("alpha"));
        assert!(md.contains("beta"));
        assert!(md.contains("gamma"));
    }

    #[test]
    fn parse_args_defaults_to_json_and_no_filters() {
        let (q, format) = parse_brochure_args(&json!({})).unwrap();
        assert_eq!(format, "json");
        assert_eq!(q, BrochureQuery::default());
    }

    #[test]
    fn parse_args_parses_every_filter() {
        let (q, format) = parse_brochure_args(&json!({
            "category": "coder",
            "status": "discovered",
            "min_discovery_score": 0.5,
            "gfx1151_class": "confirmed",
            "model": "alpha",
            "format": "markdown",
        }))
        .unwrap();
        assert_eq!(format, "markdown");
        assert_eq!(q.category, Some(FleetCategory::Coder));
        assert_eq!(q.status, Some(CandidateStatus::Discovered));
        assert_eq!(q.min_discovery_score, Some(0.5));
        assert_eq!(q.gfx1151_class.as_deref(), Some("confirmed"));
        assert_eq!(q.model.as_deref(), Some("alpha"));
    }

    #[test]
    fn parse_args_rejects_invalid_category_with_invalid_argument() {
        let err = parse_brochure_args(&json!({"category": "not_a_category"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn parse_args_rejects_invalid_status_with_invalid_argument() {
        let err = parse_brochure_args(&json!({"status": "not_a_status"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn parse_args_rejects_invalid_format() {
        let err = parse_brochure_args(&json!({"format": "yaml"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn parse_args_rejects_non_numeric_min_discovery_score() {
        let err = parse_brochure_args(&json!({"min_discovery_score": "high"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn tool_metadata_states_the_brochure_catalog_distinction() {
        let tool = ModelDiscoveryBrochure;
        assert_eq!(tool.name(), "model_discovery_brochure");
        assert!(tool.description().contains("model_fleet_catalog"));
        assert!(tool.description().contains("CANDIDATES"));
    }
}
