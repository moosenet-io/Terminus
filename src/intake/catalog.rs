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

use serde_json::json;
use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::storage::{self, StoredRunAggregate};
use crate::intake::{current_epoch, EpochSelector};

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

/// The single serving/context-profile leaf category.
pub const SERVING_CATEGORY: &str = "context_profile";
/// The single agent tool-use leaf category.
pub const AGENT_CATEGORY: &str = "tool_use";

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
        // Coder (3) + assistant (7) + serving (1) + agent (1) = 12 cells.
        assert_eq!(m.cells.len(), 12);
        assert_eq!(m.not_run_count, 12);
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
}
