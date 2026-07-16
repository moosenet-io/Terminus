//! DISC-06: the model-discovery refresh orchestrator — the "curator" that
//! actually POPULATES the brochure DISC-02 reads.
//!
//! The read tool (`model_discovery_brochure`, DISC-02), the HF Hub listing
//! client (`hf_client`, DISC-04), the candidate schema (DISC-01) and the
//! `upsert_candidate` write primitive (DISC-03) all shipped — but nothing ever
//! wired them together, so the brochure read an empty table forever. This
//! module is that missing wiring: for each fleet category it lists candidate
//! models from the HF Hub public listing, scores each by a popularity signal,
//! and upserts them as `status = Discovered`. Idempotent (upsert keyed on
//! `model_name`, and `upsert_candidate` deliberately does NOT touch `status` on
//! conflict — a model already `Fetching`/`Swept` is re-observed, never demoted),
//! so it is safe to run daily.
//!
//! It intentionally does NOT fetch/download models (that is DISC-08, onto cold
//! storage) — it only records CANDIDATES an operator/agent can then choose to
//! fetch. Size/VRAM/gfx1151-fit are left unknown here (a listing exposes no
//! parameter count); they are filled by the fetch/measure step.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::intake::discovery::hf_client::{HfCategory, HfHubClient, HfModelSummary};
use crate::intake::discovery::schema::{CandidateStatus, DiscoveryCandidate, FleetCategory};
use crate::intake::discovery::upsert::upsert_candidate;
use crate::intake::storage as intake_storage;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// Map the DISC-04 HF-listing category to the DISC-01 fleet schema category.
/// They are 1:1 by name today; kept as an explicit match so any future
/// divergence is a compile error rather than a silent mis-classification.
fn fleet_category(c: HfCategory) -> FleetCategory {
    match c {
        HfCategory::ToolRouter => FleetCategory::ToolRouter,
        HfCategory::WriterSlm => FleetCategory::WriterSlm,
        HfCategory::Assistant => FleetCategory::Assistant,
        HfCategory::Coder => FleetCategory::Coder,
        HfCategory::Embedding => FleetCategory::Embedding,
        HfCategory::Visual => FleetCategory::Visual,
        HfCategory::Voice => FleetCategory::Voice,
    }
}

/// A popularity-based discovery score, clamped to `[0, 100]`. Blends HF's own
/// `trending_score` (a curated recency/velocity signal) with log-scaled
/// downloads + likes, so a single viral spike doesn't dwarf steadily-popular
/// models and a model with zero of one signal isn't zeroed out. Deterministic
/// and offline (operates on already-listed summaries); weights are documented
/// so the ranking stays auditable. This is the DISC-05 scoring stand-in until a
/// real leaderboard feed lands (spec open question 3).
fn discovery_score(m: &HfModelSummary) -> f64 {
    let downloads = (1.0 + m.downloads as f64).ln() * 4.0; // ~0..55 across 1..1e6
    let likes = (1.0 + m.likes as f64).ln() * 2.0; // ~0..20
    let trend = m.trending_score.max(0.0); // HF's own signal, already curated
    (trend + downloads + likes).clamp(0.0, 100.0)
}

/// Derive a short display `model_name` from an HF repo id
/// (`"Qwen/Qwen3-8B-Instruct"` → `"Qwen3-8B-Instruct"`). Falls back to the whole
/// repo id if there is no `/`.
fn model_name_from_repo(repo: &str) -> String {
    repo.rsplit('/').next().unwrap_or(repo).to_string()
}

/// Build the `Discovered`-status candidate for one listed HF model.
fn candidate_from(summary: &HfModelSummary, cat: HfCategory, now: chrono::DateTime<chrono::Utc>) -> DiscoveryCandidate {
    DiscoveryCandidate {
        model_name: model_name_from_repo(&summary.hf_repo),
        hf_repo: summary.hf_repo.clone(),
        category: fleet_category(cat),
        status: CandidateStatus::Discovered,
        // Fit is unknown from a listing (no parameter count); the fetch/measure
        // step sets 'confirmed'/'experimental'. 'unknown' is the brochure's
        // documented sentinel for exactly this.
        gfx1151_class: "unknown".to_string(),
        size_b: None,
        vram_footprint_gb: None,
        discovery_source: "huggingface_hub".to_string(),
        discovery_score: Some(discovery_score(summary)),
        discovered_at: now,
        last_seen_at: now,
        fetched_at: None,
        marked_for_fleet_at: None,
        evicted_at: None,
        retained_profile: None,
        rationale: Some(format!(
            "hf listing ({}): {} downloads, {} likes, trending {:.1}",
            summary.pipeline_tag.as_deref().unwrap_or("?"),
            summary.downloads,
            summary.likes,
            summary.trending_score
        )),
    }
}

/// Outcome of one refresh pass, surfaced in the tool result.
pub struct RefreshOutcome {
    pub categories_scanned: usize,
    pub candidates_seen: usize,
    pub candidates_upserted: usize,
    /// `(category_or_repo, error)` — a per-item/per-category failure that did
    /// NOT abort the whole pass.
    pub errors: Vec<(String, String)>,
}

/// Run one discovery refresh over `categories`, upserting every listed
/// candidate. A category whose listing fails (rate limit, transport) is
/// recorded and skipped — one bad category never aborts the pass (DISC-04's
/// contract). When `dry_run` is true, candidates are listed + scored but NOT
/// written (nothing touches the brochure). `pool` is `None` iff `dry_run`.
pub async fn refresh(
    pool: Option<&sqlx::PgPool>,
    client: &HfHubClient,
    categories: &[HfCategory],
    now: chrono::DateTime<chrono::Utc>,
) -> RefreshOutcome {
    let mut seen = 0usize;
    let mut upserted = 0usize;
    let mut errors = Vec::new();
    for &cat in categories {
        match client.list_models(cat).await {
            Ok(models) => {
                for m in &models {
                    seen += 1;
                    let cand = candidate_from(m, cat, now);
                    match pool {
                        None => upserted += 1, // dry-run: count what WOULD be written
                        Some(p) => match upsert_candidate(p, &cand).await {
                            Ok(()) => upserted += 1,
                            Err(e) => errors.push((m.hf_repo.clone(), e.to_string())),
                        },
                    }
                }
            }
            Err(e) => errors.push((cat.as_str().to_string(), format!("{e:?}"))),
        }
    }
    RefreshOutcome {
        categories_scanned: categories.len(),
        candidates_seen: seen,
        candidates_upserted: upserted,
        errors,
    }
}

/// Parse the optional `categories` arg (array of category strings) → the HF
/// categories to scan; absent/empty means all seven.
fn parse_categories(args: &Value) -> Result<Vec<HfCategory>, ToolError> {
    let Some(v) = args.get("categories") else {
        return Ok(HfCategory::all().to_vec());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgument("'categories' must be an array of strings".into()))?;
    if arr.is_empty() {
        return Ok(HfCategory::all().to_vec());
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("each 'categories' entry must be a string".into()))?;
        let cat = HfCategory::all()
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| ToolError::InvalidArgument(format!("unknown category '{s}'")))?;
        out.push(cat);
    }
    Ok(out)
}

/// DISC-06 MCP tool: `model_discovery_refresh` — the curator that populates the
/// brochure. Distinct from the read-only `model_discovery_brochure`.
pub struct ModelDiscoveryRefresh;

impl ModelDiscoveryRefresh {
    async fn run(&self, args: Value) -> Result<Value, ToolError> {
        let categories = parse_categories(&args)?;
        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
        let client = HfHubClient::new();
        let now = chrono::Utc::now();
        let pool = if dry_run {
            None
        } else {
            Some(intake_storage::get_pool().await?)
        };
        let outcome = refresh(pool.as_ref(), &client, &categories, now).await;
        Ok(json!({
            "dry_run": dry_run,
            "categories_scanned": outcome.categories_scanned,
            "candidates_seen": outcome.candidates_seen,
            "candidates_upserted": outcome.candidates_upserted,
            "errors": outcome.errors.iter().map(|(k, v)| json!({"where": k, "error": v})).collect::<Vec<_>>(),
            "note": if dry_run {
                "dry run — nothing was written to the brochure"
            } else {
                "brochure refreshed; query model_discovery_brochure to read the candidates"
            },
        }))
    }
}

#[async_trait]
impl RustTool for ModelDiscoveryRefresh {
    fn name(&self) -> &str {
        "model_discovery_refresh"
    }

    fn description(&self) -> &str {
        "Refresh the model discovery brochure (DISC-06, the 'curator'): list HuggingFace Hub \
         model CANDIDATES per fleet category, score each by popularity (downloads/likes/trending), \
         and upsert them as status='discovered'. This is the WRITE side of 'model_discovery_brochure' \
         (which only reads). Idempotent + safe to run daily: re-observing a model bumps last_seen_at \
         and never demotes one already being fetched/swept. Does NOT download models (that is a \
         separate fetch step); size/VRAM/gfx1151-fit are left 'unknown' until a model is fetched. \
         Args (all optional): 'categories' (array subset of tool_router|writer_slm|assistant|coder| \
         embedding|visual|voice; default all seven), 'dry_run' (bool; list+score but write nothing). \
         Returns per-pass counts + any per-category listing errors (a rate-limited category is \
         skipped, never fatal)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "categories": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["tool_router", "writer_slm", "assistant", "coder", "embedding", "visual", "voice"]
                    },
                    "description": "Subset of fleet categories to refresh. Omit or empty = all seven."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, list + score candidates but write nothing to the brochure (preview)."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let v = self.run(args).await?;
        Ok(serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let v = self.run(args).await?;
        let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
        Ok(ToolOutput { text, structured: Some(v) })
    }
}

/// Register the DISC-06 curator tool on the CORE registry (wired into
/// `crate::intake::discovery::register` alongside the DISC-02 read tool).
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelDiscoveryRefresh));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(repo: &str, downloads: u64, likes: u64, trending: f64) -> HfModelSummary {
        HfModelSummary {
            hf_repo: repo.to_string(),
            pipeline_tag: Some("text-generation".to_string()),
            downloads,
            likes,
            trending_score: trending,
            tags: vec![],
        }
    }

    #[test]
    fn hf_category_maps_one_to_one_to_fleet_category() {
        // Every HF category has a fleet category with the SAME string form —
        // the mapping is total and name-preserving (a divergence would fail here).
        for c in HfCategory::all() {
            assert_eq!(fleet_category(c).as_str(), c.as_str(), "category '{}' must map by name", c.as_str());
        }
    }

    #[test]
    fn discovery_score_is_monotonic_in_each_signal_and_bounded() {
        let base = summary("org/m", 1000, 10, 5.0);
        let more_dl = summary("org/m", 100_000, 10, 5.0);
        let more_likes = summary("org/m", 1000, 5000, 5.0);
        let more_trend = summary("org/m", 1000, 10, 50.0);
        let b = discovery_score(&base);
        assert!(discovery_score(&more_dl) > b, "more downloads must score higher");
        assert!(discovery_score(&more_likes) > b, "more likes must score higher");
        assert!(discovery_score(&more_trend) > b, "higher trending must score higher");
        // Bounded even for absurd inputs.
        let huge = summary("org/m", u64::MAX, u64::MAX, 1e9);
        let s = discovery_score(&huge);
        assert!((0.0..=100.0).contains(&s), "score must clamp to [0,100], got {s}");
        // A zero-signal model still yields a finite, non-negative score.
        let zero = summary("org/m", 0, 0, 0.0);
        assert!((0.0..=100.0).contains(&discovery_score(&zero)));
    }

    #[test]
    fn model_name_is_the_repo_leaf() {
        assert_eq!(model_name_from_repo("Qwen/Qwen3-8B-Instruct"), "Qwen3-8B-Instruct");
        assert_eq!(model_name_from_repo("no-slash"), "no-slash");
    }

    #[test]
    fn candidate_from_sets_discovered_status_and_unknown_fit() {
        let now = chrono::Utc::now();
        let c = candidate_from(&summary("Org/Cool-Coder-7B", 5000, 40, 12.0), HfCategory::Coder, now);
        assert_eq!(c.model_name, "Cool-Coder-7B");
        assert_eq!(c.hf_repo, "Org/Cool-Coder-7B");
        assert!(matches!(c.category, FleetCategory::Coder));
        assert!(matches!(c.status, CandidateStatus::Discovered), "new candidate starts Discovered");
        assert_eq!(c.gfx1151_class, "unknown", "fit unknown from a listing");
        assert!(c.size_b.is_none() && c.vram_footprint_gb.is_none(), "size unknown from a listing");
        assert!(c.discovery_score.unwrap() > 0.0);
        assert_eq!(c.discovery_source, "huggingface_hub");
    }

    #[test]
    fn parse_categories_defaults_to_all_and_rejects_unknown() {
        assert_eq!(parse_categories(&json!({})).unwrap().len(), 7);
        assert_eq!(parse_categories(&json!({"categories": []})).unwrap().len(), 7);
        let one = parse_categories(&json!({"categories": ["coder"]})).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].as_str(), "coder");
        assert!(parse_categories(&json!({"categories": ["nonsense"]})).is_err());
        assert!(parse_categories(&json!({"categories": "coder"})).is_err(), "must be an array");
    }
}
