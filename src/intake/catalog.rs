//! MINT2-07: the Model Fleet Catalog — a derived, refreshable, per-model
//! coverage registry an agent reads to know the fleet WITHOUT running SQL.
//!
//! WHY THIS EXISTS — one object, the whole fleet at a glance:
//! Results live scattered across `code_run_aggregates` (MINT2-03, the coder
//! reliability source), `assistant_dimension_score` (the seven-dimension
//! assistant lineage), `model_operational_profiles` (serving/context facts) and
//! `agent_profile_runs` (tool-use). No single object answers, PER MODEL, "what
//! has and has NOT been tested, and how did it score?". This module builds that
//! object: a LONG-format set of coverage CELLS — one per
//! (model × test_type × task_category) — each carrying an explicit COVERAGE
//! STATUS, plus a per-model serving card. Coverage GAPS are the whole point: an
//! un-tested cell is a FIRST-CLASS [`CoverageStatus::NotRun`] row, never an
//! omission, so "which models have no multi_file result" is a one-line query.
//!
//! COVERAGE STATUS ([`CoverageStatus`]), decided per cell:
//!   - [`CoverageStatus::Run`]        — a CURRENT-epoch result is present. For
//!     coder cells this is a `code_run_aggregates` row at the current
//!     `harness_version` ([`crate::intake::CURRENT_EPOCH`]); carries pass_rate +
//!     n_samples + variance + last_run + harness_version.
//!   - [`CoverageStatus::Stale`]      — ONLY legacy-epoch ('v1'/'v2') coder
//!     results exist; nothing current. History exists, the current gap is real.
//!   - [`CoverageStatus::NonViable`]  — a `code_profile_runs.failure_class =
//!     'non_viable_vram'` skip was recorded. Read on its OWN axis from the
//!     failure rows — NOT inferred from aggregate cells, which EXCLUDE skips
//!     (MINT2-03) and so have none.
//!   - [`CoverageStatus::NotRun`]     — no result of any kind, ever. Enumerated
//!     from the fleet nomination list, so a never-swept fleet model appears with
//!     every cell `not_run`.
//!
//! The BUILDER ([`build_catalog`]) is PURE (input rows → cells): same input →
//! same output, no DB, no clock, no env — so the whole coverage-status logic is
//! unit-testable without Postgres. A thin impure orchestrator
//! ([`refresh_fleet_catalog`]) reads the sources tolerantly (a missing upstream
//! table degrades to `not_run` cells, never a crash — mirroring the MINT2-03/05
//! absence-tolerant pattern), builds, and persists. The read/build tolerates an
//! un-migrated host end to end. MINT2-08 exposes the read side as a core tool;
//! this item builds ONLY the builder + storage + refresh.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::storage::{self, StoredRunAggregate};
use crate::intake::{current_epoch, EpochSelector};
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// Coder task categories the catalog reports (the `code_v2` tier taxonomy —
/// `standard` maps to `multi_file`). One coder cell per category per model.
pub const CODER_CATEGORIES: &[&str] = &["blitz", "multi_file", "deep"];

/// Assistant sweep dimensions the catalog reports (the `DIMENSION` consts across
/// `assistant/dim*.rs`). One assistant cell per dimension per model.
pub const ASSISTANT_DIMENSIONS: &[&str] = &[
    "conversation_depth",
    "tool_chaining",
    "memory_integration",
    "personality_latent",
    "personality_prompted",
    "embeddings",
    "yarn_context_depth",
];

/// The test-family axis written to `model_fleet_catalog_cell.test_type`.
pub const TEST_TYPE_CODER: &str = "coder";
pub const TEST_TYPE_ASSISTANT: &str = "assistant";
pub const TEST_TYPE_SERVING: &str = "serving";
pub const TEST_TYPE_AGENT: &str = "agent";
/// MINT-DIFF-01: the diffusion suite's test-family tag (use-case quality +
/// performance, distinct from `coder`/`assistant`/`serving`/`agent`).
pub const TEST_TYPE_DIFFUSION: &str = "diffusion";
/// SUITE-EMB (S125 TERM #508): the embedding-retrieval suite's test-family tag
/// (IR quality — precision/recall/MRR/nDCG — plus dimensionality, throughput, and
/// a public-vs-domain delta). Results land in `assistant_dimension_score` under
/// `task_category = "embedding_retrieval"`; see
/// [`crate::intake::newcats::embedding_retrieval`].
pub const TEST_TYPE_EMBEDDING_RETRIEVAL: &str = "embedding_retrieval";
/// S125 SUITE-TOOL: the tool-routing suite's test-family tag (correct-tool@1,
/// parameter validity, decoy rejection, multi-step — distinct from the legacy
/// `agent` tool-use family, which stays a scalar accuracy on its own axis).
pub const TEST_TYPE_TOOL_ROUTING: &str = "tool_routing";
/// SUITE-VQA: the vision-QA suite's test-family tag (image → short answer;
/// accuracy / caption similarity / hallucination / latency / VRAM).
pub const TEST_TYPE_VISION_QA: &str = "vision_qa";
/// The vision-QA leaf `task_category` — the `image_parsing` module's own tag,
/// which the suite writes its `assistant_dimension_score` rows under.
pub const VISION_QA_CATEGORY: &str = "image_parsing";
/// The `dimension` the vision-QA suite writes (image_parsing::DIMENSION). A
/// vision_qa cell is `run` when a row for this dimension exists for the model
/// (read via `read_assistant_cells`, which does not filter `task_category`).
const VISION_QA_DIMENSION: &str = "vision_description";
/// SUITE-RRK: the reranking suite's test-family tag (nDCG uplift + latency,
/// distinct from the other families).
pub const TEST_TYPE_RERANKING: &str = "reranking";
/// SUITE-IMG (S125): the image-generation suite's test-family tag. Distinct from
/// `TEST_TYPE_DIFFUSION` — image generation (text→image, sd-turbo behind Chord's
/// `/v1/images/generations`) and the diffusion-language probe are separate suites
/// with separate `task_category`s (`newcats::image_generation::TASK_CATEGORY ==
/// "image_generation"` vs `newcats::diffusion::TASK_CATEGORY == "diffusion"`).
pub const TEST_TYPE_IMAGE_GENERATION: &str = "image_generation";

/// The single serving/context-profile leaf category.
pub const SERVING_CATEGORY: &str = "context_profile";
/// The single agent tool-use leaf category.
pub const AGENT_CATEGORY: &str = "tool_use";
/// The single embedding-retrieval leaf category (matches the `newcats` module's
/// `TASK_CATEGORY`).
pub const EMBEDDING_RETRIEVAL_CATEGORY: &str = "embedding_retrieval";
/// The single tool-routing leaf category (all four routing metrics roll up under
/// the one `"tool_routing"` dimension written by the suite).
pub const TOOL_ROUTING_CATEGORY: &str = "tool_routing";
/// SUITE-RRK: the single reranking leaf category. MUST equal
/// `newcats::reranking::DIMENSION` — the reranking suite writes its
/// `assistant_dimension_score` rows under that dimension, and the catalog cell
/// is derived by matching it (duplicated as a string here, matching how
/// `ASSISTANT_DIMENSIONS` duplicates the `assistant/dim*.rs` `DIMENSION` consts).
pub const RERANKING_CATEGORY: &str = "rerank_relevance";
/// SUITE-IMG: the image-generation leaf category — the `dimension` the suite
/// writes (`newcats::image_generation::DIMENSION == "text_to_image"`).
pub const IMAGE_GENERATION_CATEGORY: &str = "text_to_image";

/// A cell's coverage status. `not_run` is FIRST-CLASS — representing gaps is the
/// catalog's whole job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageStatus {
    /// A current-epoch result is present.
    Run,
    /// Only legacy-epoch coder results exist; nothing current.
    Stale,
    /// No result of any kind, ever (an explicit coverage gap).
    NotRun,
    /// A recorded `non_viable_vram` skip (over-VRAM pre-flight), read on its own
    /// axis from the failure rows — never inferred from aggregate cells.
    NonViable,
}

impl CoverageStatus {
    /// The stable snake_case key persisted to `model_fleet_catalog_cell.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            CoverageStatus::Run => "run",
            CoverageStatus::Stale => "stale",
            CoverageStatus::NotRun => "not_run",
            CoverageStatus::NonViable => "non_viable",
        }
    }
}

/// One fleet model to enumerate coverage for. Sourced from the nomination list
/// (so never-swept models are still enumerated) UNION the models present in the
/// result tables (so a dropped-from-fleet model is surfaced as historical). The
/// nomination list carries no quant (quant is a measured factor), so `quant` is
/// `None` here — coder cells take their quant from the aggregates instead.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetModel {
    pub model_name: String,
    /// `true` ⇒ present in the current nomination fleet; `false` ⇒ has results
    /// but is no longer nominated (surfaced as historical, not dropped).
    pub in_current_fleet: bool,
    /// Nomination footprint in GB (for the serving card's `vram_gb`), if known.
    pub vram_footprint_gb: Option<f64>,
}

/// A recorded `non_viable_vram` skip for a `(model, quant)` — read on its own
/// axis from `code_profile_runs.failure_class`, NOT from aggregates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonViableRow {
    pub model_name: String,
    pub quant: Option<String>,
}

/// A per-(model, dimension) rollup of the assistant sweep's dimension scores.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantCell {
    pub model_name: String,
    pub dimension: String,
    pub n_samples: i64,
    /// Mean of the dimension's `std_dev` values, when present.
    pub score_stddev: Option<f64>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A per-model serving/operational profile snapshot (latest).
#[derive(Debug, Clone, PartialEq)]
pub struct ServingRow {
    pub model_name: String,
    pub max_context_safe: Option<i32>,
    pub quality_degradation_point: Option<i32>,
    /// Representative throughput (tok/s at the 8k tier).
    pub throughput: Option<f64>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A per-model agent tool-use rollup.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRollup {
    pub model_name: String,
    pub n_samples: i64,
    /// Fraction of tests where the correct tool was selected, when measurable.
    pub tool_accuracy: Option<f64>,
}

/// All inputs the PURE [`build_catalog`] needs. Every field is plain owned data
/// so the builder never touches a DB — the impure [`refresh_fleet_catalog`]
/// reads these tolerantly and hands them in.
#[derive(Debug, Clone, Default)]
pub struct CatalogInputs {
    /// The enumerated fleet (nominations UNION result-table models).
    pub fleet: Vec<FleetModel>,
    /// Current-epoch coder aggregates (MINT2-03), the reliability source.
    pub coder_current: Vec<StoredRunAggregate>,
    /// Legacy-epoch coder aggregates (everything NOT the current epoch), used
    /// only to decide `stale`.
    pub coder_legacy: Vec<StoredRunAggregate>,
    /// Max run timestamp per (model_name, task_category) for the current epoch.
    pub coder_last_run: BTreeMap<(String, String), chrono::DateTime<chrono::Utc>>,
    /// `non_viable_vram` skip rows, read on their own axis from failure_class.
    pub non_viable: Vec<NonViableRow>,
    /// Assistant dimension rollups.
    pub assistant: Vec<AssistantCell>,
    /// Serving/operational profile snapshots.
    pub serving: Vec<ServingRow>,
    /// Agent tool-use rollups.
    pub agent: Vec<AgentRollup>,
    /// The coder epoch stamped on `run` coder cells (`current_epoch()` live).
    pub coder_epoch: String,
}

/// One coverage cell (a `model_fleet_catalog_cell` row). Metric fields are
/// `Option` — only a `run`/`stale` cell carries them; a `not_run`/`non_viable`
/// cell leaves them `None` (distinct from a measured `0.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogCell {
    pub model_name: String,
    pub quant: Option<String>,
    pub test_type: String,
    pub task_category: String,
    pub status: CoverageStatus,
    pub pass_rate: Option<f64>,
    pub n_samples: Option<i64>,
    pub score_stddev: Option<f64>,
    pub low_confidence: Option<bool>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub harness_version: Option<String>,
}

/// Serving/operational facts for a model's fleet card (persisted as JSONB).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServingFacts {
    pub max_context_safe: Option<i32>,
    pub quality_degradation_point: Option<i32>,
    pub throughput: Option<f64>,
    pub agent_tool_accuracy: Option<f64>,
    pub vram_gb: Option<f64>,
}

impl ServingFacts {
    /// `true` when no fact is populated (an all-`None` card → persist SQL NULL
    /// rather than an empty object, so "no serving facts" is unambiguous).
    pub fn is_empty(&self) -> bool {
        self.max_context_safe.is_none()
            && self.quality_degradation_point.is_none()
            && self.throughput.is_none()
            && self.agent_tool_accuracy.is_none()
            && self.vram_gb.is_none()
    }

    /// JSON object form for the `serving_json` column (`None` when empty).
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        Some(json!({
            "max_context_safe": self.max_context_safe,
            "quality_degradation_point": self.quality_degradation_point,
            "throughput": self.throughput,
            "agent_tool_accuracy": self.agent_tool_accuracy,
            "vram_gb": self.vram_gb,
        }))
    }
}

/// One model's full fleet card: its coverage cells, serving facts, and the gap
/// summary counts.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCatalog {
    pub model_name: String,
    /// Representative quant for the card (modal coder quant, else `None`).
    pub quant: Option<String>,
    pub in_current_fleet: bool,
    pub cells: Vec<CatalogCell>,
    pub serving: ServingFacts,
    pub not_run_count: usize,
    pub stale_count: usize,
}

// ---------------------------------------------------------------------------
// PURE builder
// ---------------------------------------------------------------------------

/// Roll several aggregate rows for ONE (model, category, quant) into a single
/// cell's metrics: n_samples SUMMED, pass_rate as the passes-weighted rate, the
/// stddev the WORST-CASE (max) dispersion across the configs, and low_confidence
/// when the pooled sample count is at most one. Returns `None` for an empty
/// slice. PURE.
fn rollup(rows: &[&StoredRunAggregate]) -> Option<(f64, i64, f64, bool)> {
    if rows.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    let mut passes: i64 = 0;
    let mut max_stddev = 0.0_f64;
    for r in rows {
        n += r.n_samples as i64;
        passes += r.passes as i64;
        if r.score_stddev > max_stddev {
            max_stddev = r.score_stddev;
        }
    }
    let pass_rate = if n > 0 { passes as f64 / n as f64 } else { 0.0 };
    Some((pass_rate, n, max_stddev, n <= 1))
}

/// The set of quants a model has coder data for in `cat` (from current + legacy
/// aggregates matching the category) UNION the quants of its non_viable rows
/// (which are category-agnostic — a whole-model VRAM skip). Empty ⇒ the caller
/// substitutes a single `None`-quant cell so the gap is still represented.
fn coder_quants_for<'a>(
    model: &str,
    cat: &str,
    current: &'a [StoredRunAggregate],
    legacy: &'a [StoredRunAggregate],
    non_viable: &'a [NonViableRow],
) -> BTreeSet<Option<String>> {
    let mut quants: BTreeSet<Option<String>> = BTreeSet::new();
    for r in current.iter().chain(legacy.iter()) {
        if r.model == model && r.task_category.as_deref() == Some(cat) {
            quants.insert(r.quant.clone());
        }
    }
    for nv in non_viable {
        if nv.model_name == model {
            quants.insert(nv.quant.clone());
        }
    }
    quants
}

/// Build the fleet catalog from its inputs. PURE — the ONE place the coverage
/// status is decided, so every branch (run / stale / not_run / non_viable) is
/// unit-testable without a DB.
///
/// Precedence per coder cell: a CURRENT-epoch aggregate ⇒ `run`; else a recorded
/// non_viable_vram skip ⇒ `non_viable`; else a legacy-epoch aggregate ⇒ `stale`;
/// else `not_run`. Assistant / serving / agent cells are `run` when a measured
/// row exists and `not_run` otherwise (those lineages are not epoch-partitioned
/// here, so they have no `stale`). Models are the UNION of the fleet nomination
/// list and every model appearing in a result table, so coverage gaps for
/// never-swept fleet models are enumerated, never omitted.
pub fn build_catalog(inputs: &CatalogInputs) -> Vec<ModelCatalog> {
    // Model universe: fleet ∪ every model with any result row. A fleet flag is
    // kept per model; a model only present in results is flagged not-in-fleet.
    let mut in_fleet: BTreeMap<String, bool> = BTreeMap::new();
    let mut vram: BTreeMap<String, Option<f64>> = BTreeMap::new();
    for f in &inputs.fleet {
        in_fleet.insert(f.model_name.clone(), f.in_current_fleet);
        vram.insert(f.model_name.clone(), f.vram_footprint_gb);
    }
    let mut universe: BTreeSet<String> = in_fleet.keys().cloned().collect();
    for r in inputs.coder_current.iter().chain(inputs.coder_legacy.iter()) {
        universe.insert(r.model.clone());
    }
    for nv in &inputs.non_viable {
        universe.insert(nv.model_name.clone());
    }
    for a in &inputs.assistant {
        universe.insert(a.model_name.clone());
    }
    for s in &inputs.serving {
        universe.insert(s.model_name.clone());
    }
    for a in &inputs.agent {
        universe.insert(a.model_name.clone());
    }

    let mut out = Vec::with_capacity(universe.len());
    for model in universe {
        let in_current_fleet = in_fleet.get(&model).copied().unwrap_or(false);
        let mut cells: Vec<CatalogCell> = Vec::new();
        let mut quant_tally: BTreeMap<Option<String>, usize> = BTreeMap::new();

        // ---- coder cells (per category × quant) ------------------------------
        for &cat in CODER_CATEGORIES {
            let quants = coder_quants_for(
                &model,
                cat,
                &inputs.coder_current,
                &inputs.coder_legacy,
                &inputs.non_viable,
            );
            let quants: Vec<Option<String>> = if quants.is_empty() {
                vec![None]
            } else {
                quants.into_iter().collect()
            };
            for q in quants {
                let cur: Vec<&StoredRunAggregate> = inputs
                    .coder_current
                    .iter()
                    .filter(|r| {
                        r.model == model
                            && r.task_category.as_deref() == Some(cat)
                            && r.quant == q
                    })
                    .collect();
                let leg: Vec<&StoredRunAggregate> = inputs
                    .coder_legacy
                    .iter()
                    .filter(|r| {
                        r.model == model
                            && r.task_category.as_deref() == Some(cat)
                            && r.quant == q
                    })
                    .collect();
                let is_non_viable = inputs
                    .non_viable
                    .iter()
                    .any(|nv| nv.model_name == model && nv.quant == q);
                let last_run = inputs
                    .coder_last_run
                    .get(&(model.clone(), cat.to_string()))
                    .copied();

                let cell = if let Some((pr, n, sd, lc)) = rollup(&cur) {
                    *quant_tally.entry(q.clone()).or_insert(0) += 1;
                    CatalogCell {
                        model_name: model.clone(),
                        quant: q.clone(),
                        test_type: TEST_TYPE_CODER.to_string(),
                        task_category: cat.to_string(),
                        status: CoverageStatus::Run,
                        pass_rate: Some(pr),
                        n_samples: Some(n),
                        score_stddev: Some(sd),
                        low_confidence: Some(lc),
                        last_run_at: last_run,
                        harness_version: Some(inputs.coder_epoch.clone()),
                    }
                } else if is_non_viable {
                    CatalogCell {
                        model_name: model.clone(),
                        quant: q.clone(),
                        test_type: TEST_TYPE_CODER.to_string(),
                        task_category: cat.to_string(),
                        status: CoverageStatus::NonViable,
                        pass_rate: None,
                        n_samples: None,
                        score_stddev: None,
                        low_confidence: None,
                        last_run_at: None,
                        harness_version: Some(inputs.coder_epoch.clone()),
                    }
                } else if let Some((pr, n, sd, lc)) = rollup(&leg) {
                    // Legacy epoch string of a representative row (all leg rows
                    // for a cell share the model/cat/quant but may differ by
                    // epoch; the first is representative for display).
                    let hv = leg.first().map(|r| r.harness_version.clone());
                    CatalogCell {
                        model_name: model.clone(),
                        quant: q.clone(),
                        test_type: TEST_TYPE_CODER.to_string(),
                        task_category: cat.to_string(),
                        status: CoverageStatus::Stale,
                        pass_rate: Some(pr),
                        n_samples: Some(n),
                        score_stddev: Some(sd),
                        low_confidence: Some(lc),
                        last_run_at: None,
                        harness_version: hv,
                    }
                } else {
                    CatalogCell {
                        model_name: model.clone(),
                        quant: q.clone(),
                        test_type: TEST_TYPE_CODER.to_string(),
                        task_category: cat.to_string(),
                        status: CoverageStatus::NotRun,
                        pass_rate: None,
                        n_samples: None,
                        score_stddev: None,
                        low_confidence: None,
                        last_run_at: None,
                        harness_version: None,
                    }
                };
                cells.push(cell);
            }
        }

        // ---- assistant cells (per dimension) ---------------------------------
        for &dim in ASSISTANT_DIMENSIONS {
            let a = inputs
                .assistant
                .iter()
                .find(|a| a.model_name == model && a.dimension == dim);
            let cell = match a {
                Some(a) if a.n_samples > 0 => CatalogCell {
                    model_name: model.clone(),
                    quant: None,
                    test_type: TEST_TYPE_ASSISTANT.to_string(),
                    task_category: dim.to_string(),
                    status: CoverageStatus::Run,
                    // Assistant dimension values are not a pass/fail rate; only
                    // n_samples + dispersion are meaningful here.
                    pass_rate: None,
                    n_samples: Some(a.n_samples),
                    score_stddev: a.score_stddev,
                    low_confidence: Some(a.n_samples <= 1),
                    last_run_at: a.last_run_at,
                    harness_version: None,
                },
                _ => not_run_cell(&model, TEST_TYPE_ASSISTANT, dim),
            };
            cells.push(cell);
        }

        // ---- serving / context-profile cell ----------------------------------
        let serving_row = inputs.serving.iter().find(|s| s.model_name == model);
        let serving_cell = match serving_row {
            Some(s) => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_SERVING.to_string(),
                task_category: SERVING_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                pass_rate: None,
                n_samples: None,
                score_stddev: None,
                low_confidence: None,
                last_run_at: s.last_run_at,
                harness_version: None,
            },
            None => not_run_cell(&model, TEST_TYPE_SERVING, SERVING_CATEGORY),
        };
        cells.push(serving_cell);

        // ---- agent tool-use cell ---------------------------------------------
        let agent_row = inputs.agent.iter().find(|a| a.model_name == model);
        let agent_cell = match agent_row {
            Some(a) if a.n_samples > 0 => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_AGENT.to_string(),
                task_category: AGENT_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                pass_rate: a.tool_accuracy,
                n_samples: Some(a.n_samples),
                score_stddev: None,
                low_confidence: Some(a.n_samples <= 1),
                last_run_at: None,
                harness_version: None,
            },
            _ => not_run_cell(&model, TEST_TYPE_AGENT, AGENT_CATEGORY),
        };
        cells.push(agent_cell);

        // ---- embedding_retrieval cell (SUITE-EMB) ----------------------------
        // The suite writes its results to `assistant_dimension_score` under
        // `task_category = "embedding_retrieval"`; no dedicated stored-aggregate
        // reader is threaded into `CatalogInputs` yet, so — like every other
        // uncovered family — this is emitted as a FIRST-CLASS `not_run` coverage
        // cell so the embedding-retrieval axis shows up (as an explicit gap) for
        // every model until a reader promotes it to `Run`. Adding that reader is a
        // follow-up, mirroring how `diffusion` introduced its `TEST_TYPE_*` before
        // a catalog reader existed.
        cells.push(not_run_cell(
            &model,
            TEST_TYPE_EMBEDDING_RETRIEVAL,
            EMBEDDING_RETRIEVAL_CATEGORY,
        ));
        // ---- tool-routing cell (S125 SUITE-TOOL) -----------------------------
        // Reads the same assistant-dimension rollups (`read_assistant_cells`
        // groups by dimension across every task_category), keyed on the suite's
        // own `"tool_routing"` dimension — so it never collides with the
        // hardcoded ASSISTANT_DIMENSIONS list above.
        let tool_routing_row = inputs
            .assistant
            .iter()
            .find(|a| a.model_name == model && a.dimension == TOOL_ROUTING_CATEGORY);
        let tool_routing_cell = match tool_routing_row {
            Some(a) if a.n_samples > 0 => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_TOOL_ROUTING.to_string(),
                task_category: TOOL_ROUTING_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                pass_rate: None,
                n_samples: Some(a.n_samples),
                score_stddev: a.score_stddev,
                low_confidence: Some(a.n_samples <= 1),
                last_run_at: a.last_run_at,
                harness_version: None,
            },
            _ => not_run_cell(&model, TEST_TYPE_TOOL_ROUTING, TOOL_ROUTING_CATEGORY),
        };
        cells.push(tool_routing_cell);

        // ---- vision_qa cell (SUITE-VQA image-QA suite) -----------------------
        // The vision_qa suite writes `assistant_dimension_score` rows under the
        // `vision_description` dimension (task_category "image_parsing"), read
        // into `inputs.assistant` — which does not filter task_category. A
        // vision_qa cell is `run` when such a row exists for this model, else
        // not_run (the explicit coverage gap, same as agent/serving).
        let vision_row = inputs
            .assistant
            .iter()
            .find(|a| a.model_name == model && a.dimension == VISION_QA_DIMENSION);
        let vision_cell = match vision_row {
            Some(a) if a.n_samples > 0 => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_VISION_QA.to_string(),
                task_category: VISION_QA_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                pass_rate: None,
                n_samples: Some(a.n_samples),
                score_stddev: a.score_stddev,
                low_confidence: Some(a.n_samples <= 1),
                last_run_at: a.last_run_at,
                harness_version: None,
            },
            _ => not_run_cell(&model, TEST_TYPE_VISION_QA, VISION_QA_CATEGORY),
        };
        cells.push(vision_cell);

        // ---- reranking cell (SUITE-RRK) --------------------------------------
        // The reranking suite writes its rows into `assistant_dimension_score`
        // under `dimension = RERANKING_CATEGORY` (see `newcats::reranking`), and
        // `read_assistant_cells` groups EVERY dimension (no task_category filter),
        // so a reranking rollup arrives in `inputs.assistant` keyed by that
        // dimension — no separate input source is needed. `Run` when rows exist,
        // `not_run` otherwise. The fixed assistant loop above never emits this
        // cell because RERANKING_CATEGORY is not in `ASSISTANT_DIMENSIONS`, so
        // there is no double-count.
        let rerank_row = inputs
            .assistant
            .iter()
            .find(|a| a.model_name == model && a.dimension == RERANKING_CATEGORY);
        let rerank_cell = match rerank_row {
            Some(a) if a.n_samples > 0 => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_RERANKING.to_string(),
                task_category: RERANKING_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                pass_rate: None,
                n_samples: Some(a.n_samples),
                score_stddev: a.score_stddev,
                low_confidence: Some(a.n_samples <= 1),
                last_run_at: a.last_run_at,
                harness_version: None,
            },
            _ => not_run_cell(&model, TEST_TYPE_RERANKING, RERANKING_CATEGORY),
        };
        cells.push(rerank_cell);

        // ---- image-generation cell (SUITE-IMG) -------------------------------
        // The suite writes `assistant_dimension_score` rows with dimension
        // `text_to_image` (task_category `image_generation`), which surface here
        // as an `AssistantCell` with that dimension (the assistant reader groups
        // by (model, dimension) and does NOT filter task_category — see
        // `newcats::mod`). It is NOT one of `ASSISTANT_DIMENSIONS`, so it is not
        // emitted as an assistant cell above; instead it becomes its own
        // image-generation coverage cell. `Run` when measured, else `not_run`.
        let imagegen_row = inputs
            .assistant
            .iter()
            .find(|a| a.model_name == model && a.dimension == IMAGE_GENERATION_CATEGORY);
        let imagegen_cell = match imagegen_row {
            Some(a) if a.n_samples > 0 => CatalogCell {
                model_name: model.clone(),
                quant: None,
                test_type: TEST_TYPE_IMAGE_GENERATION.to_string(),
                task_category: IMAGE_GENERATION_CATEGORY.to_string(),
                status: CoverageStatus::Run,
                // Image generation is a success/hardware probe, not a pass-rate;
                // only sample count + recency are meaningful (like the serving cell).
                pass_rate: None,
                n_samples: Some(a.n_samples),
                score_stddev: a.score_stddev,
                low_confidence: Some(a.n_samples <= 1),
                last_run_at: a.last_run_at,
                harness_version: None,
            },
            _ => not_run_cell(&model, TEST_TYPE_IMAGE_GENERATION, IMAGE_GENERATION_CATEGORY),
        };
        cells.push(imagegen_cell);

        // ---- serving facts (fleet card) --------------------------------------
        let serving = ServingFacts {
            max_context_safe: serving_row.and_then(|s| s.max_context_safe),
            quality_degradation_point: serving_row.and_then(|s| s.quality_degradation_point),
            throughput: serving_row.and_then(|s| s.throughput),
            agent_tool_accuracy: agent_row.and_then(|a| a.tool_accuracy),
            vram_gb: vram.get(&model).copied().flatten(),
        };

        // Representative quant: the modal quant across this model's run coder
        // cells (deterministic tie-break by quant order), else None.
        let quant = quant_tally
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .and_then(|(q, _)| q);

        let not_run_count = cells
            .iter()
            .filter(|c| c.status == CoverageStatus::NotRun)
            .count();
        let stale_count = cells
            .iter()
            .filter(|c| c.status == CoverageStatus::Stale)
            .count();

        out.push(ModelCatalog {
            model_name: model,
            quant,
            in_current_fleet,
            cells,
            serving,
            not_run_count,
            stale_count,
        });
    }
    out
}

/// A `not_run` cell (no result of any kind) — the explicit coverage gap.
fn not_run_cell(model: &str, test_type: &str, task_category: &str) -> CatalogCell {
    CatalogCell {
        model_name: model.to_string(),
        quant: None,
        test_type: test_type.to_string(),
        task_category: task_category.to_string(),
        status: CoverageStatus::NotRun,
        pass_rate: None,
        n_samples: None,
        score_stddev: None,
        low_confidence: None,
        last_run_at: None,
        harness_version: None,
    }
}

// ---------------------------------------------------------------------------
// IMPURE orchestrator
// ---------------------------------------------------------------------------

/// Read every catalog source TOLERANTLY (a missing upstream table on an
/// un-migrated host reads as "no rows", so its cells become `not_run` rather
/// than crashing), build the catalog, and persist it. Returns how many
/// per-model cards were written.
///
/// Best-effort by design: callers wire this at the end of a sweep like the
/// MINT2-03 aggregate refresh / MINT2-05 marker, so a DB hiccup or an
/// un-migrated `model_fleet_catalog` table degrades to "catalog not refreshed",
/// never a failed sweep. Also invokable on demand (MINT2-08's tool reads the
/// persisted result). PURE star is [`build_catalog`]; this only does the I/O.
pub async fn refresh_fleet_catalog(pool: &PgPool) -> Result<usize, ToolError> {
    let epoch = current_epoch().to_string();

    // Coder aggregates: current epoch (reliability source) + legacy for `stale`.
    let coder_current = storage::read_code_run_aggregates(pool, &epoch).await?;
    let coder_all = storage::read_code_run_aggregates_selected(pool, &EpochSelector::All).await?;
    let coder_legacy: Vec<StoredRunAggregate> = coder_all
        .into_iter()
        .filter(|r| r.harness_version != epoch)
        .collect();

    let coder_last_run = storage::read_coder_last_run(pool, &epoch).await?;
    let non_viable = storage::read_non_viable_rows(pool, &epoch).await?;
    let assistant = storage::read_assistant_cells(pool).await?;
    let serving = storage::read_serving_rows(pool).await?;
    let agent = storage::read_agent_rollups(pool).await?;

    // Fleet list: nominations UNION the models that appear in any result table.
    // Best-effort — an unreadable nominations file still yields a catalog built
    // from whatever result rows exist (those models flagged not-in-fleet).
    let fleet = build_fleet_list(&coder_current, &coder_legacy, &non_viable, &assistant, &serving);

    let inputs = CatalogInputs {
        fleet,
        coder_current,
        coder_legacy,
        coder_last_run,
        non_viable,
        assistant,
        serving,
        agent,
        coder_epoch: epoch,
    };
    let catalog = build_catalog(&inputs);
    storage::persist_fleet_catalog(pool, &catalog).await?;
    Ok(catalog.len())
}

/// Enumerate the fleet: every nomination (so never-swept models are present,
/// flagged in-fleet) UNION every model appearing in a result source (flagged
/// not-in-fleet when absent from nominations). Reading the nominations file is
/// best-effort — a missing/unreadable file just yields the result-only universe.
fn build_fleet_list(
    coder_current: &[StoredRunAggregate],
    coder_legacy: &[StoredRunAggregate],
    non_viable: &[NonViableRow],
    assistant: &[AssistantCell],
    serving: &[ServingRow],
) -> Vec<FleetModel> {
    let mut by_name: BTreeMap<String, FleetModel> = BTreeMap::new();

    // Nominations first (authoritative in-fleet membership + footprint).
    if let Ok(noms) = crate::intake::assistant::acquire::Nominations::load() {
        for n in &noms.nominations {
            by_name.insert(
                n.id.clone(),
                FleetModel {
                    model_name: n.id.clone(),
                    in_current_fleet: true,
                    vram_footprint_gb: Some(n.vram_footprint_gb()),
                },
            );
        }
    }

    // Every model with results but NOT nominated → historical, flagged.
    let add_result_model = |name: &str, by_name: &mut BTreeMap<String, FleetModel>| {
        by_name.entry(name.to_string()).or_insert_with(|| FleetModel {
            model_name: name.to_string(),
            in_current_fleet: false,
            vram_footprint_gb: None,
        });
    };
    for r in coder_current.iter().chain(coder_legacy.iter()) {
        add_result_model(&r.model, &mut by_name);
    }
    for nv in non_viable {
        add_result_model(&nv.model_name, &mut by_name);
    }
    for a in assistant {
        add_result_model(&a.model_name, &mut by_name);
    }
    for s in serving {
        add_result_model(&s.model_name, &mut by_name);
    }

    by_name.into_values().collect()
}

// ===========================================================================
// MINT2-08: the READ side — a query API over the PERSISTED catalog, and the
// `model_fleet_catalog` core Terminus tool that exposes it.
//
// The refresh above (MINT2-07) WRITES the two catalog tables; this side only
// READS them — it never recomputes. The pure filter/render helpers
// ([`filter_cards`], [`render_catalog_json`], [`render_catalog_markdown`]) take
// plain owned [`StoredCatalogCard`]s so the whole query surface is unit-testable
// over a seeded fixture WITHOUT a live DB — the same pure/impure seam MINT2-07's
// [`build_catalog`] uses. Only [`storage::read_fleet_catalog`] touches Postgres.
// ===========================================================================

/// The four coverage-status keys a `status` filter may take (the persisted
/// `model_fleet_catalog_cell.status` values). Used to validate the tool arg.
pub const VALID_STATUSES: &[&str] = &["run", "stale", "not_run", "non_viable"];

/// One persisted coverage cell, read back from `model_fleet_catalog_cell`.
/// `status` is kept as the stored snake_case string (no re-parse needed for the
/// read path — the tool filters/emits it directly).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCatalogCell {
    pub model_name: String,
    pub quant: Option<String>,
    pub test_type: String,
    pub task_category: String,
    pub status: String,
    pub pass_rate: Option<f64>,
    pub n_samples: Option<i64>,
    pub score_stddev: Option<f64>,
    pub low_confidence: Option<bool>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub harness_version: Option<String>,
}

/// One persisted per-model card (a `model_fleet_catalog` row) plus its cells.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCatalogCard {
    pub model_name: String,
    pub quant: Option<String>,
    pub in_current_fleet: bool,
    pub serving_json: Option<Value>,
    pub not_run_count: i64,
    pub stale_count: i64,
    pub refreshed_at: chrono::DateTime<chrono::Utc>,
    pub cells: Vec<StoredCatalogCell>,
}

/// The optional filters the `model_fleet_catalog` tool accepts. All `None` ⇒ the
/// whole current fleet card.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogQuery {
    /// Restrict to one model's card (unknown ⇒ empty + a note).
    pub model: Option<String>,
    /// Restrict to cells with this coverage status (e.g. `not_run` — "what has
    /// NOT been run").
    pub status: Option<String>,
    /// Restrict to cells of this test family (`coder`/`assistant`/`serving`/`agent`).
    pub test_type: Option<String>,
}

/// Apply a [`CatalogQuery`] to the persisted cards. PURE. Returns the filtered
/// cards (each carrying only the cells that survive the cell-level filters) plus
/// an optional human note (set when a `model` filter matched nothing — the
/// "unknown model" case: an empty result with an explanation, never an error).
///
/// A `model` filter selects the one matching card. A `status`/`test_type` filter
/// prunes each card's cells; a card left with NO cells by an active cell-level
/// filter is dropped (so `status=not_run` yields exactly the models WITH a gap).
/// With no cell-level filter every card is kept as-is — so the default
/// (no-filter) card lists every fleet model exactly once.
pub fn filter_cards(
    cards: &[StoredCatalogCard],
    q: &CatalogQuery,
) -> (Vec<StoredCatalogCard>, Option<String>) {
    let mut note = None;
    let selected: Vec<&StoredCatalogCard> = match &q.model {
        Some(m) => {
            let v: Vec<&StoredCatalogCard> = cards.iter().filter(|c| &c.model_name == m).collect();
            if v.is_empty() {
                note = Some(format!("no such model '{m}' in the fleet catalog"));
            }
            v
        }
        None => cards.iter().collect(),
    };

    let cell_filter_active = q.status.is_some() || q.test_type.is_some();
    let mut out = Vec::new();
    for card in selected {
        let cells: Vec<StoredCatalogCell> = card
            .cells
            .iter()
            .filter(|c| q.status.as_deref().map_or(true, |s| c.status == s))
            .filter(|c| q.test_type.as_deref().map_or(true, |t| c.test_type == t))
            .cloned()
            .collect();
        if cell_filter_active && cells.is_empty() {
            continue;
        }
        out.push(StoredCatalogCard {
            cells,
            ..card.clone()
        });
    }
    (out, note)
}

/// One cell as an output JSON object.
fn cell_json(c: &StoredCatalogCell) -> Value {
    json!({
        "test_type": c.test_type,
        "task_category": c.task_category,
        "quant": c.quant,
        "status": c.status,
        "pass_rate": c.pass_rate,
        "n_samples": c.n_samples,
        "score_stddev": c.score_stddev,
        "low_confidence": c.low_confidence,
        "last_run_at": c.last_run_at,
        "harness_version": c.harness_version,
    })
}

/// Render the (already-filtered) cards as the tool's structured JSON. PURE.
///
/// Shape: `{ epoch, refreshed_at, note?, models: [ { model_name, quant,
/// in_current_fleet, serving, cells: [...], not_run_count, stale_count } ],
/// summary: { total_models, total_not_run, total_stale, not_run_cells: [...] } }`.
/// `epoch` is always the current epoch (even for an empty catalog). The
/// `summary` counts the cells ACTUALLY present in this (filtered) view so
/// "what's missing" is one field away; the per-model `not_run_count`/`stale_count`
/// are the card's stored full-model totals.
pub fn render_catalog_json(
    cards: &[StoredCatalogCard],
    note: Option<&str>,
    epoch: &str,
) -> Value {
    let refreshed_at = cards.iter().map(|c| c.refreshed_at).max();
    let mut total_not_run = 0i64;
    let mut total_stale = 0i64;
    let mut not_run_cells: Vec<Value> = Vec::new();

    let models: Vec<Value> = cards
        .iter()
        .map(|card| {
            let cells: Vec<Value> = card
                .cells
                .iter()
                .map(|c| {
                    match c.status.as_str() {
                        "not_run" => {
                            total_not_run += 1;
                            not_run_cells.push(json!({
                                "model_name": c.model_name,
                                "test_type": c.test_type,
                                "task_category": c.task_category,
                            }));
                        }
                        "stale" => total_stale += 1,
                        _ => {}
                    }
                    cell_json(c)
                })
                .collect();
            json!({
                "model_name": card.model_name,
                "quant": card.quant,
                "in_current_fleet": card.in_current_fleet,
                "serving": card.serving_json,
                "cells": cells,
                "not_run_count": card.not_run_count,
                "stale_count": card.stale_count,
            })
        })
        .collect();

    let mut out = json!({
        "epoch": epoch,
        "refreshed_at": refreshed_at,
        "models": models,
        "summary": {
            "total_models": cards.len(),
            "total_not_run": total_not_run,
            "total_stale": total_stale,
            "not_run_cells": not_run_cells,
        },
    });
    if let Some(n) = note {
        out.as_object_mut()
            .unwrap()
            .insert("note".to_string(), json!(n));
    }
    out
}

/// Render the (already-filtered) cards as a compact markdown coverage matrix:
/// models as rows, `test_type/task_category` cells as columns, the coverage
/// status in each cell (blank when a model has no such cell). PURE — a human/agent
/// display for `format=markdown`.
pub fn render_catalog_markdown(
    cards: &[StoredCatalogCard],
    note: Option<&str>,
    epoch: &str,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Fleet coverage matrix (epoch {epoch})\n\n"));
    if let Some(n) = note {
        s.push_str(&format!("_{n}_\n\n"));
    }
    if cards.is_empty() {
        s.push_str("_(no models)_\n");
        return s;
    }

    // Column axis: the sorted union of every present cell's test_type/category.
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for card in cards {
        for c in &card.cells {
            columns.insert(format!("{}/{}", c.test_type, c.task_category));
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();

    // Header.
    s.push_str("| model |");
    for col in &columns {
        s.push_str(&format!(" {col} |"));
    }
    s.push('\n');
    s.push_str("| --- |");
    for _ in &columns {
        s.push_str(" --- |");
    }
    s.push('\n');

    // One row per model; each column carries the cell's status, or blank.
    for card in cards {
        let mut by_col: BTreeMap<String, &str> = BTreeMap::new();
        for c in &card.cells {
            by_col.insert(format!("{}/{}", c.test_type, c.task_category), c.status.as_str());
        }
        s.push_str(&format!("| {} |", card.model_name));
        for col in &columns {
            s.push_str(&format!(" {} |", by_col.get(col).copied().unwrap_or("")));
        }
        s.push('\n');
    }
    s
}

/// Parse + validate the tool args into a [`CatalogQuery`] and the output format.
/// Empty/whitespace filters are treated as absent. An unrecognized `status` or
/// `format` is a clean [`ToolError::InvalidArgument`], not a silent no-op.
fn parse_catalog_args(args: &Value) -> Result<(CatalogQuery, String), ToolError> {
    let opt_str = |k: &str| -> Option<String> {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let status = opt_str("status");
    if let Some(s) = &status {
        if !VALID_STATUSES.contains(&s.as_str()) {
            return Err(ToolError::InvalidArgument(format!(
                "'status' must be one of {VALID_STATUSES:?}, got '{s}'"
            )));
        }
    }

    let format = opt_str("format").unwrap_or_else(|| "json".to_string());
    if format != "json" && format != "markdown" {
        return Err(ToolError::InvalidArgument(format!(
            "'format' must be 'json' or 'markdown', got '{format}'"
        )));
    }

    let query = CatalogQuery {
        model: opt_str("model"),
        status,
        test_type: opt_str("test_type"),
    };
    Ok((query, format))
}

/// The `model_fleet_catalog` core Terminus tool: a read-only, SQL-free window on
/// the persisted Model Fleet Catalog. Any agent (Harmony, Lumina, a reviewer)
/// calls it to see, per model, what has and has NOT been tested and how it
/// scored — with a `status=not_run` filter for "what's missing" in one shot.
pub struct ModelFleetCatalog;

impl ModelFleetCatalog {
    /// Shared read+filter+render used by both `execute` (text) and
    /// `execute_structured` (text + structured JSON). Reads the PERSISTED
    /// catalog (never recomputes); a not-yet-migrated host surfaces a clean
    /// [`ToolError::NotConfigured`] from [`storage::read_fleet_catalog`].
    async fn run(&self, args: Value) -> Result<(String, Option<Value>), ToolError> {
        let (query, format) = parse_catalog_args(&args)?;
        let pool = storage::get_pool().await?;
        let cards = storage::read_fleet_catalog(&pool).await?;
        let (filtered, note) = filter_cards(&cards, &query);
        let epoch = current_epoch();
        if format == "markdown" {
            let md = render_catalog_markdown(&filtered, note.as_deref(), epoch);
            Ok((md, None))
        } else {
            let value = render_catalog_json(&filtered, note.as_deref(), epoch);
            let text = serde_json::to_string_pretty(&value)
                .unwrap_or_else(|_| value.to_string());
            Ok((text, Some(value)))
        }
    }
}

#[async_trait]
impl RustTool for ModelFleetCatalog {
    fn name(&self) -> &str {
        "model_fleet_catalog"
    }

    fn description(&self) -> &str {
        "Read the Model Fleet Catalog — the per-model test-coverage registry — WITHOUT SQL. \
         Returns, per model, one cell per (test_type × task_category) carrying its coverage \
         status (run | stale | not_run | non_viable) plus metrics (pass_rate, n_samples, \
         variance), last run, and harness_version, with a not_run/stale gap summary so \
         'what has NOT been run' is one field away. All filters optional: 'model' (one card), \
         'status' (e.g. not_run for gaps), 'test_type' (coder|assistant|serving|agent). \
         'format' is 'json' (default, structured) or 'markdown' (a compact coverage matrix). \
         Read-only; reads the persisted catalog (refreshed at the end of each MINT harness run)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Restrict to one model's card. Unknown model → empty models with a note."
                },
                "status": {
                    "type": "string",
                    "enum": ["run", "stale", "not_run", "non_viable"],
                    "description": "Restrict to cells with this coverage status. Use 'not_run' for 'what has NOT been run'."
                },
                "test_type": {
                    "type": "string",
                    "description": "Restrict to one test family: 'coder', 'assistant', 'serving', or 'agent'."
                },
                "format": {
                    "type": "string",
                    "enum": ["json", "markdown"],
                    "description": "Output format. 'json' (default) is structured; 'markdown' renders the coverage matrix as a table."
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

/// The `model_fleet_catalog_refresh` core tool: on-demand (re)derivation of the
/// persisted Model Fleet Catalog from the raw intake profile tables.
///
/// The catalog is a DERIVED registry. It is otherwise refreshed only at the tail
/// of a CLI MINT harness run or a coder/assistant sweep (see
/// `MintHarness::refresh_catalog_best_effort`) — the `model_intake` (single) and
/// `model_intake_fleet` MCP tool paths historically did NOT refresh it, so a
/// profiling run driven purely through the MCP surface left `model_fleet_catalog`
/// stale/empty even though the raw `model_profiles`/`*_profile_runs` tables held
/// the data. This tool closes that gap for on-demand reconciliation; the fleet
/// tool now also refreshes on its tail.
pub struct ModelFleetCatalogRefresh;

#[async_trait]
impl RustTool for ModelFleetCatalogRefresh {
    fn name(&self) -> &str {
        "model_fleet_catalog_refresh"
    }

    fn description(&self) -> &str {
        "(Re)derive and persist the Model Fleet Catalog from the raw intake profile tables, then \
         report how many model cards were written. The catalog is a DERIVED registry, normally \
         refreshed only at the end of a CLI MINT harness or coder/assistant sweep — so profiling \
         driven purely through the `model_intake`/`model_intake_fleet` MCP tools can leave it \
         stale. Call this (no args) to reconcile the catalog with the latest profile data so \
         `model_fleet_catalog` reflects it. Idempotent and safe to call anytime: it only rewrites \
         a fully re-derivable table."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = storage::get_pool().await?;
        let n = refresh_fleet_catalog(&pool).await?;
        Ok(format!(
            "Model Fleet Catalog refreshed: {n} model card(s) persisted from the current profile \
             data. View with `model_fleet_catalog`."
        ))
    }
}

/// Register the read-only `model_fleet_catalog` tool + the on-demand
/// `model_fleet_catalog_refresh` tool on the CORE registry (called from
/// `crate::intake::register`, itself wired into `register_all` — the same
/// Chord-served core surface as `plane`/`gitea`). No personal registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelFleetCatalog));
    registry.register_or_replace(Box::new(ModelFleetCatalogRefresh));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg(model: &str, cat: &str, quant: &str, epoch: &str, passes: i32, n: i32) -> StoredRunAggregate {
        StoredRunAggregate {
            model: model.to_string(),
            task_category: Some(cat.to_string()),
            harness_version: epoch.to_string(),
            quant: Some(quant.to_string()),
            reasoning_enabled: None,
            context_window_launched: None,
            temperature: None,
            top_p: None,
            pass_rate: passes as f64 / n as f64,
            n_samples: n,
            passes,
            score_stddev: 0.5,
            low_confidence: n <= 1,
        }
    }

    fn base_inputs() -> CatalogInputs {
        CatalogInputs {
            coder_epoch: current_epoch().to_string(),
            ..Default::default()
        }
    }

    /// Find the one cell for a (model, test_type, task_category).
    fn cell<'a>(
        cat: &'a [ModelCatalog],
        model: &str,
        test_type: &str,
        task_category: &str,
    ) -> &'a CatalogCell {
        cat.iter()
            .find(|m| m.model_name == model)
            .unwrap_or_else(|| panic!("model {model} not in catalog"))
            .cells
            .iter()
            .find(|c| c.test_type == test_type && c.task_category == task_category)
            .unwrap_or_else(|| panic!("no {test_type}/{task_category} cell for {model}"))
    }

    /// A model with a current-epoch multi_file pass → a `run` cell carrying its
    /// pass_rate.
    #[test]
    fn current_epoch_result_is_run() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "m".into(),
            in_current_fleet: true,
            vram_footprint_gb: None,
        }];
        inp.coder_current = vec![agg("m", "multi_file", "Q4_K_M", current_epoch(), 5, 7)];
        let cat = build_catalog(&inp);
        let c = cell(&cat, "m", "coder", "multi_file");
        assert_eq!(c.status, CoverageStatus::Run);
        assert_eq!(c.n_samples, Some(7));
        assert!((c.pass_rate.unwrap() - 5.0 / 7.0).abs() < 1e-9);
        assert_eq!(c.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(c.harness_version.as_deref(), Some(current_epoch()));
    }

    /// A model with ONLY legacy-epoch coder results → `stale` (history exists,
    /// current gap is real).
    #[test]
    fn only_legacy_is_stale() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "m".into(),
            in_current_fleet: true,
            vram_footprint_gb: None,
        }];
        inp.coder_legacy = vec![agg("m", "blitz", "Q4_K_M", "v2", 3, 4)];
        let cat = build_catalog(&inp);
        let c = cell(&cat, "m", "coder", "blitz");
        assert_eq!(c.status, CoverageStatus::Stale);
        assert_eq!(c.harness_version.as_deref(), Some("v2"));
        assert_eq!(c.n_samples, Some(4));
    }

    /// A fleet model with NO rows anywhere → every cell `not_run` (the catalog's
    /// core job: gaps represented, not omitted).
    #[test]
    fn fleet_model_with_no_rows_is_all_not_run() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "ghost".into(),
            in_current_fleet: true,
            vram_footprint_gb: None,
        }];
        let cat = build_catalog(&inp);
        let m = cat.iter().find(|m| m.model_name == "ghost").unwrap();
        assert!(
            m.cells.iter().all(|c| c.status == CoverageStatus::NotRun),
            "every cell must be not_run"
        );
        // Coder (3) + assistant (7) + serving (1) + agent (1) +
        // embedding_retrieval (1, SUITE-EMB) + tool_routing (1, SUITE-TOOL) + vision_qa (1, SUITE-VQA) + reranking (1, SUITE-RRK) = 16 cells.
        assert_eq!(m.cells.len(), 16);
        assert_eq!(m.not_run_count, 16);
        // multi_file gap is explicitly present, not omitted.
        assert_eq!(
            cell(&cat, "ghost", "coder", "multi_file").status,
            CoverageStatus::NotRun
        );
    }

    /// A `non_viable_vram` model → `non_viable` coder cells (read off the failure
    /// axis, NOT inferred from aggregates — there are none for a skip).
    #[test]
    fn non_viable_vram_is_non_viable() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "big".into(),
            in_current_fleet: true,
            vram_footprint_gb: Some(200.0),
        }];
        inp.non_viable = vec![NonViableRow {
            model_name: "big".into(),
            quant: Some("Q8_0".into()),
        }];
        let cat = build_catalog(&inp);
        for &c in CODER_CATEGORIES {
            let cell = cell(&cat, "big", "coder", c);
            assert_eq!(cell.status, CoverageStatus::NonViable, "cat {c}");
            assert_eq!(cell.quant.as_deref(), Some("Q8_0"));
        }
    }

    /// A current-epoch pass wins over a recorded non_viable skip (the model DID
    /// run on some backend; aggregates exclude the skip).
    #[test]
    fn run_wins_over_non_viable() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "m".into(),
            in_current_fleet: true,
            vram_footprint_gb: None,
        }];
        inp.coder_current = vec![agg("m", "deep", "Q4_K_M", current_epoch(), 2, 3)];
        inp.non_viable = vec![NonViableRow {
            model_name: "m".into(),
            quant: Some("Q4_K_M".into()),
        }];
        let cat = build_catalog(&inp);
        assert_eq!(
            cell(&cat, "m", "coder", "deep").status,
            CoverageStatus::Run
        );
    }

    /// An assistant dimension with rows → `run`; the others → `not_run`.
    #[test]
    fn assistant_presence_is_run_others_not_run() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "m".into(),
            in_current_fleet: true,
            vram_footprint_gb: None,
        }];
        inp.assistant = vec![AssistantCell {
            model_name: "m".into(),
            dimension: "tool_chaining".into(),
            n_samples: 12,
            score_stddev: Some(0.3),
            last_run_at: None,
        }];
        let cat = build_catalog(&inp);
        assert_eq!(
            cell(&cat, "m", "assistant", "tool_chaining").status,
            CoverageStatus::Run
        );
        assert_eq!(
            cell(&cat, "m", "assistant", "conversation_depth").status,
            CoverageStatus::NotRun
        );
    }

    /// Serving + agent presence → `run` cells and populated serving facts.
    #[test]
    fn serving_and_agent_facts() {
        let mut inp = base_inputs();
        inp.fleet = vec![FleetModel {
            model_name: "m".into(),
            in_current_fleet: true,
            vram_footprint_gb: Some(18.5),
        }];
        inp.serving = vec![ServingRow {
            model_name: "m".into(),
            max_context_safe: Some(32000),
            quality_degradation_point: Some(48000),
            throughput: Some(41.2),
            last_run_at: None,
        }];
        inp.agent = vec![AgentRollup {
            model_name: "m".into(),
            n_samples: 20,
            tool_accuracy: Some(0.85),
        }];
        let cat = build_catalog(&inp);
        assert_eq!(
            cell(&cat, "m", "serving", "context_profile").status,
            CoverageStatus::Run
        );
        assert_eq!(
            cell(&cat, "m", "agent", "tool_use").status,
            CoverageStatus::Run
        );
        let m = cat.iter().find(|m| m.model_name == "m").unwrap();
        assert_eq!(m.serving.max_context_safe, Some(32000));
        assert_eq!(m.serving.agent_tool_accuracy, Some(0.85));
        assert_eq!(m.serving.vram_gb, Some(18.5));
        assert!(m.serving.to_json().is_some());
    }

    /// A model with results but absent from the fleet nominations is surfaced as
    /// historical (in_current_fleet = false), not dropped.
    #[test]
    fn dropped_from_fleet_flagged_historical() {
        let mut inp = base_inputs();
        // fleet is EMPTY; the model only appears via a result row.
        inp.coder_current = vec![agg("orphan", "blitz", "Q4_K_M", current_epoch(), 1, 2)];
        let cat = build_catalog(&inp);
        let m = cat.iter().find(|m| m.model_name == "orphan").unwrap();
        assert!(!m.in_current_fleet, "must be flagged not-in-current-fleet");
        assert_eq!(
            cell(&cat, "orphan", "coder", "blitz").status,
            CoverageStatus::Run
        );
    }

    /// Empty inputs (fresh cutover / un-migrated everything) build an empty
    /// catalog — never a panic.
    #[test]
    fn empty_inputs_build_empty_catalog() {
        let cat = build_catalog(&base_inputs());
        assert!(cat.is_empty());
    }

    /// Rollup pools sample counts and passes across configs, and takes the
    /// worst-case stddev.
    #[test]
    fn rollup_pools_samples() {
        let a = agg("m", "blitz", "Q4_K_M", current_epoch(), 1, 3); // stddev 0.5
        let mut b = agg("m", "blitz", "Q4_K_M", current_epoch(), 4, 4);
        b.score_stddev = 1.2;
        let rows = vec![&a, &b];
        let (pr, n, sd, lc) = rollup(&rows).unwrap();
        assert_eq!(n, 7);
        assert!((pr - 5.0 / 7.0).abs() < 1e-9);
        assert!((sd - 1.2).abs() < 1e-9, "worst-case stddev");
        assert!(!lc);
    }

    // ---- MINT2-08: read-side query API (pure, DB-free over a fixture) ----

    fn scell(model: &str, tt: &str, cat: &str, status: &str) -> StoredCatalogCell {
        StoredCatalogCell {
            model_name: model.into(),
            quant: None,
            test_type: tt.into(),
            task_category: cat.into(),
            status: status.into(),
            pass_rate: None,
            n_samples: None,
            score_stddev: None,
            low_confidence: None,
            last_run_at: None,
            harness_version: None,
        }
    }

    fn scard(model: &str, cells: Vec<StoredCatalogCell>) -> StoredCatalogCard {
        let not_run = cells.iter().filter(|c| c.status == "not_run").count() as i64;
        let stale = cells.iter().filter(|c| c.status == "stale").count() as i64;
        StoredCatalogCard {
            model_name: model.into(),
            quant: Some("Q4_K_M".into()),
            in_current_fleet: true,
            serving_json: Some(json!({"vram_gb": 18.5})),
            not_run_count: not_run,
            stale_count: stale,
            refreshed_at: chrono::Utc::now(),
            cells,
        }
    }

    /// A two-model fixture: `alpha` has a run + a not_run coder cell; `beta` has
    /// every cell not_run (a never-swept fleet model).
    fn fixture() -> Vec<StoredCatalogCard> {
        vec![
            scard(
                "alpha",
                vec![
                    {
                        let mut c = scell("alpha", "coder", "blitz", "run");
                        c.pass_rate = Some(0.8);
                        c.n_samples = Some(10);
                        c.quant = Some("Q4_K_M".into());
                        c.harness_version = Some(current_epoch().into());
                        c
                    },
                    scell("alpha", "coder", "multi_file", "not_run"),
                    scell("alpha", "assistant", "tool_chaining", "run"),
                ],
            ),
            scard(
                "beta",
                vec![
                    scell("beta", "coder", "blitz", "not_run"),
                    scell("beta", "coder", "multi_file", "not_run"),
                ],
            ),
        ]
    }

    /// get-all: every fixture model appears exactly once; epoch is the current
    /// epoch; the gap summary counts the not_run cells.
    #[test]
    fn render_all_lists_every_model_once_with_epoch_and_summary() {
        let (cards, note) = filter_cards(&fixture(), &CatalogQuery::default());
        assert!(note.is_none());
        let v = render_catalog_json(&cards, note.as_deref(), current_epoch());
        assert_eq!(v["epoch"], current_epoch());
        let models = v["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        let names: Vec<&str> = models.iter().map(|m| m["model_name"].as_str().unwrap()).collect();
        assert_eq!(names.iter().filter(|n| **n == "alpha").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "beta").count(), 1);
        // alpha(1) + beta(2) = 3 not_run cells across the view.
        assert_eq!(v["summary"]["total_not_run"], 3);
        assert_eq!(v["summary"]["total_models"], 2);
    }

    /// A model with no results has all cells `not_run` (beta in the fixture).
    #[test]
    fn model_with_no_results_is_all_not_run() {
        let (cards, _) = filter_cards(&fixture(), &CatalogQuery::default());
        let beta = cards.iter().find(|c| c.model_name == "beta").unwrap();
        assert!(beta.cells.iter().all(|c| c.status == "not_run"));
    }

    /// `status=not_run` returns ONLY not_run cells (and drops models with none).
    #[test]
    fn status_not_run_returns_only_gap_cells() {
        let q = CatalogQuery {
            status: Some("not_run".into()),
            ..Default::default()
        };
        let (cards, note) = filter_cards(&fixture(), &q);
        assert!(note.is_none());
        for card in &cards {
            assert!(
                card.cells.iter().all(|c| c.status == "not_run"),
                "only not_run cells survive the filter"
            );
        }
        // alpha keeps its 1 gap cell; beta keeps its 2 — both remain.
        assert_eq!(cards.len(), 2);
        let v = render_catalog_json(&cards, None, current_epoch());
        assert_eq!(v["summary"]["total_not_run"], 3);
    }

    /// `model=<x>` returns exactly that one card.
    #[test]
    fn model_filter_returns_one_card() {
        let q = CatalogQuery {
            model: Some("alpha".into()),
            ..Default::default()
        };
        let (cards, note) = filter_cards(&fixture(), &q);
        assert!(note.is_none());
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].model_name, "alpha");
    }

    /// Unknown `model` → empty models with a note, still a 200-shaped response.
    #[test]
    fn unknown_model_is_empty_with_note_not_error() {
        let q = CatalogQuery {
            model: Some("nope".into()),
            ..Default::default()
        };
        let (cards, note) = filter_cards(&fixture(), &q);
        assert!(cards.is_empty());
        let note = note.expect("a note explaining the unknown model");
        assert!(note.contains("nope"));
        let v = render_catalog_json(&cards, Some(&note), current_epoch());
        assert_eq!(v["epoch"], current_epoch());
        assert!(v["models"].as_array().unwrap().is_empty());
        assert!(v["note"].as_str().unwrap().contains("nope"));
    }

    /// `test_type` filter keeps only that family's cells.
    #[test]
    fn test_type_filter_keeps_only_that_family() {
        let q = CatalogQuery {
            test_type: Some("assistant".into()),
            ..Default::default()
        };
        let (cards, _) = filter_cards(&fixture(), &q);
        // Only alpha has an assistant cell; beta is dropped (no matching cells).
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].model_name, "alpha");
        assert!(cards[0].cells.iter().all(|c| c.test_type == "assistant"));
    }

    /// `format=markdown` renders a table (header + separator + a model row).
    #[test]
    fn markdown_renders_a_table() {
        let (cards, _) = filter_cards(&fixture(), &CatalogQuery::default());
        let md = render_catalog_markdown(&cards, None, current_epoch());
        assert!(md.contains("| model |"), "has a header row");
        assert!(md.contains("| --- |"), "has a markdown separator");
        assert!(md.contains("coder/multi_file"), "has a matrix column");
        assert!(md.contains("| alpha |") && md.contains("| beta |"), "has model rows");
        assert!(md.contains(current_epoch()));
    }

    /// An empty catalog renders valid JSON with the current epoch and no models
    /// — the fresh-cutover case is data, not an error.
    #[test]
    fn empty_catalog_renders_epoch_and_no_models() {
        let v = render_catalog_json(&[], None, current_epoch());
        assert_eq!(v["epoch"], current_epoch());
        assert!(v["models"].as_array().unwrap().is_empty());
        assert_eq!(v["summary"]["total_models"], 0);
    }

    /// Arg parsing validates status/format and treats blank filters as absent.
    #[test]
    fn parse_args_validates_and_trims() {
        let (q, fmt) = parse_catalog_args(&json!({
            "model": "  alpha ", "status": "not_run", "test_type": " coder ", "format": "markdown"
        }))
        .unwrap();
        assert_eq!(q.model.as_deref(), Some("alpha"));
        assert_eq!(q.status.as_deref(), Some("not_run"));
        assert_eq!(q.test_type.as_deref(), Some("coder"));
        assert_eq!(fmt, "markdown");

        // Defaults: no format → json; blank strings → absent filters.
        let (q, fmt) = parse_catalog_args(&json!({"model": "  "})).unwrap();
        assert!(q.model.is_none());
        assert_eq!(fmt, "json");

        // Invalid status / format → clean InvalidArgument.
        assert!(matches!(
            parse_catalog_args(&json!({"status": "bogus"})),
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            parse_catalog_args(&json!({"format": "xml"})),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    /// The tool's identity + schema are stable and read-only-shaped.
    #[test]
    fn tool_metadata_is_stable() {
        let t = ModelFleetCatalog;
        assert_eq!(t.name(), "model_fleet_catalog");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        // All filters optional — no `required` array.
        assert!(p.get("required").is_none());
        assert!(p["properties"]["status"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "not_run"));
    }

    /// The tool registers on a core registry under its stable name.
    #[test]
    fn tool_registers_on_core_registry() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("model_fleet_catalog"));
    }
}
