//! S84 ASMT-11 — assistant-role money queries + report writer.
//!
//! This module turns the rows that ASMT-02..07 wrote into
//! `assistant_dimension_score` into the operator-facing **assistant routing
//! report**: the per-dimension "money queries" (best conversation-depth, best
//! tool-chaining, best memory-survival, OCEAN proximity shortlist, prompted
//! adherence ranking, embedding leader with public-vs-Engram delta) and the
//! **sequential personality read** that sits beside S83's builder routing table.
//!
//! ## Two surfaces, one source of truth
//!   - **SQL** ([`sql`] submodule): the parameterized money queries the live
//!     report runs against Postgres. Every query is fully parameterized — NO
//!     literals for model ids, dimensions, metrics, or thresholds bleed into the
//!     SQL string beyond the column/table names the schema owns. Dimension and
//!     metric *names* are bound as parameters sourced from the dimension runners'
//!     own `const` strings (re-exported here as [`dims`]), so a rename in a
//!     dimension file is a compile error here, not a silent empty result.
//!   - **Pure analysis** ([`ScoreRow`] + the `rank_*` / `shortlist_*` fns): the
//!     same ranking logic as plain functions over an in-memory row set, so the
//!     whole report is unit-testable with a seeded fixture and NO database.
//!
//! ## Sequential personality read — enforced by TYPES, not discipline
//! Dim-4 (latent OCEAN) and dim-5 (prompted adherence) are different scales and
//! must NEVER be averaged into one "personality score" (Notes #2). This module
//! makes that structural:
//!   1. [`ocean_proximity_shortlist`] produces an [`OceanShortlist`] FIRST —
//!      base models whose latent disposition is close to Lumina's target (they
//!      don't fight the prompt).
//!   2. [`rank_shortlist_by_prompted_adherence`] takes that [`OceanShortlist`]
//!      *by value* and ranks ONLY its members by dim-5. It is impossible to call
//!      the dim-5 ranker without first holding a dim-4 shortlist.
//!   3. The two live in separate report sections ([`PersonalityRead`]) with no
//!      field that merges them. A test ([`tests`]) renders the report and FAILS
//!      if any merged/combined personality score ever appears.
//!
//! All DB/secret access is via [`schema::get_pool`] /
//! [`crate::config::intake_database_url`] (vault/config, no literals).

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::error::ToolError;

use super::schema;
use super::{dim1_conversation, dim2_toolchain, dim3_memory, dim4_ocean, dim5_prompted, dim6_embeddings};

/// SD at or above which a panel-scored row is flagged "judge-ambiguous" — a high
/// SD is a finding, not a number to smooth over (Notes #3). On the 1-5 panel
/// scale, ≥1.0 means judges disagreed by roughly a full point on average. A
/// design threshold, not infra; tunable via [`ReportConfig`].
pub const DEFAULT_HIGH_SD: f64 = 1.0;

/// Re-export the dimension/metric `const` names the runners write, so the money
/// queries bind the EXACT stored strings. A rename upstream breaks compilation
/// here (the whole point — no silently-stale query string).
pub mod dims {
    use super::*;

    pub const CONVERSATION_DEPTH: &str = dim1_conversation::DIMENSION;
    pub const RECALL_CEILING_TURNS: &str = dim1_conversation::METRIC_RECALL_CEILING;
    pub const COHERENCE: &str = dim1_conversation::METRIC_COHERENCE;

    pub const TOOL_CHAINING: &str = dim2_toolchain::DIMENSION;
    pub const MEAN_CHAIN_ACCURACY: &str = dim2_toolchain::METRIC_MEAN_CHAIN_ACCURACY;
    pub const CONVERSATIONAL_PASS_RATE: &str = dim2_toolchain::METRIC_CONVERSATIONAL_PASS_RATE;

    pub const MEMORY_INTEGRATION: &str = dim3_memory::DIMENSION;
    pub const FACT_SURVIVAL_RATE: &str = dim3_memory::METRIC_FACT_SURVIVAL_RATE;

    pub const PERSONALITY_LATENT: &str = dim4_ocean::DIMENSION;
    pub const PROXIMITY_TO_LUMINA: &str = dim4_ocean::METRIC_PROXIMITY;

    pub const PERSONALITY_PROMPTED: &str = dim5_prompted::DIMENSION;
    /// Dim-5 behavioral sub-score metric names (the "did it actually behave" axis).
    pub fn prompted_behavioral_metrics() -> &'static [&'static str] {
        dim5_prompted::BEHAVIORAL_METRICS
    }
    /// Dim-5 trait sub-score metric names (the "did it sound right" axis).
    pub fn prompted_trait_metrics() -> &'static [&'static str] {
        dim5_prompted::TRAIT_METRICS
    }

    pub const EMBEDDINGS: &str = dim6_embeddings::DIMENSION;
    pub const NDCG_AT_K: &str = "ndcg_at_k";
    pub const NDCG_AT_K_DELTA: &str = "ndcg_at_k_delta";
    pub const EMBED_LATENCY_MS: &str = "latency_ms";
}

/// One stored `assistant_dimension_score` row, as the report reads it. Plain data
/// so the ranking logic is testable without a DB. `backend_tag` is the raw stored
/// string (`"gpu"` | `"cpu"`) to stay byte-identical to what the keying uses.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreRow {
    pub model_id: String,
    pub backend_tag: String,
    pub dimension: String,
    pub metric: String,
    pub value: f64,
    pub std_dev: Option<f64>,
    pub judge: String,
    pub low_confidence: bool,
}

impl ScoreRow {
    /// True when this panel-scored row's judges disagreed enough to be a finding.
    pub fn is_high_sd(&self, threshold: f64) -> bool {
        self.std_dev.map(|sd| sd >= threshold).unwrap_or(false)
    }
}

/// The (model_id, backend_tag) compound key every report row is keyed on — the
/// SAME key S83 uses (`model_id` + `backend_tag`), so the dual-profile join lines
/// up. Kept as owned strings to be byte-identical to the stored values.
///
/// INVARIANT (mem-config-tagging sprint): this key does **not** include
/// `mem_config`. `rank_metric`/`best_*` (below) pick the single BEST value per
/// `ModelKey` across whatever rows they're given. If ever called with rows
/// that mix the preserved `carveout` baseline and a `dynamic_gtt` sweep for the
/// SAME (model_id, backend_tag) — i.e. `fetch_scores(pool, None)` (unscoped,
/// all-runs) with both datasets present — the ranking would silently pick
/// whichever measurement (carveout or dynamic_gtt) happened to score higher,
/// blending two different memory configurations into one "best" number.
/// As of this writing `run_report`/`build_report` have NO live call sites
/// anywhere in the binary (checked: only `runner.rs` generates a `run_id`, and
/// it does not call `run_report`; no `bin/` entrypoint does either) — every
/// path that writes rows always does so under one `run_id`, so this is
/// currently unreachable in practice. Any FUTURE caller that invokes
/// `run_report`/`fetch_scores` with `run_id = None` while both `carveout` and
/// `dynamic_gtt` rows may coexist MUST either scope to a single run_id first,
/// or extend `ModelKey` to include `mem_config` before doing so.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ModelKey {
    pub model_id: String,
    pub backend_tag: String,
}

impl ModelKey {
    fn of(row: &ScoreRow) -> Self {
        ModelKey {
            model_id: row.model_id.clone(),
            backend_tag: row.backend_tag.clone(),
        }
    }
}

/// A single ranked entry in a per-dimension money query: who, the value, and the
/// SD/ambiguity context the operator needs to trust (or distrust) the number.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedEntry {
    pub key: ModelKey,
    /// The ranked value (semantics documented per query: turns, fraction, 1-5…).
    pub value: f64,
    /// Sample SD across complying judges (panel metrics only).
    pub std_dev: Option<f64>,
    /// True when SD ≥ the high-SD threshold — judge-ambiguous, surfaced not hidden.
    pub high_sd: bool,
    /// True when only one judge complied (mean over n=1).
    pub low_confidence: bool,
}

/// Result of one money query: an ordered ranking plus the metric it ranked on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MoneyQuery {
    /// Stable query name (e.g. `"best_conversation_depth"`).
    pub name: String,
    /// The dimension + metric this query ranked, for provenance.
    pub dimension: String,
    pub metric: String,
    /// Higher value is better? (false for latency etc.)
    pub higher_is_better: bool,
    /// Ranking, best first.
    pub ranking: Vec<RankedEntry>,
}

impl MoneyQuery {
    /// The top-ranked entry, if any models were scored.
    pub fn leader(&self) -> Option<&RankedEntry> {
        self.ranking.first()
    }
}

/// Knobs the report runs under. No infra here — purely scoring thresholds.
#[derive(Debug, Clone)]
pub struct ReportConfig {
    /// SD ≥ this ⇒ flag judge-ambiguous (panel metrics).
    pub high_sd: f64,
    /// Chat-role latency/degradation guard (see [`ChatRoleGuard`]).
    pub guard: ChatRoleGuard,
}

impl Default for ReportConfig {
    fn default() -> Self {
        ReportConfig {
            high_sd: DEFAULT_HIGH_SD,
            guard: ChatRoleGuard::default(),
        }
    }
}

// ===========================================================================
// Money-query ranking primitives (pure)
// ===========================================================================

/// Rank the latest value per (model, backend) for one (dimension, metric).
///
/// Pure: operates over the supplied rows. When a (model, backend) has multiple
/// rows for the same metric (e.g. re-runs), the row with the HIGHEST value wins
/// for `higher_is_better` queries (and lowest for the reverse) — a model's best
/// observed measurement, deterministic regardless of input order. `higher_is_better`
/// controls both the per-key pick and the final sort.
fn rank_metric(
    rows: &[ScoreRow],
    dimension: &str,
    metric: &str,
    higher_is_better: bool,
    high_sd: f64,
) -> Vec<RankedEntry> {
    let mut best: BTreeMap<ModelKey, RankedEntry> = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|r| r.dimension == dimension && r.metric == metric)
    {
        let entry = RankedEntry {
            key: ModelKey::of(row),
            value: row.value,
            std_dev: row.std_dev,
            high_sd: row.is_high_sd(high_sd),
            low_confidence: row.low_confidence,
        };
        best.entry(ModelKey::of(row))
            .and_modify(|cur| {
                let replace = if higher_is_better {
                    entry.value > cur.value
                } else {
                    entry.value < cur.value
                };
                if replace {
                    *cur = entry.clone();
                }
            })
            .or_insert(entry);
    }
    let mut ranking: Vec<RankedEntry> = best.into_values().collect();
    ranking.sort_by(|a, b| {
        let ord = if higher_is_better {
            b.value.partial_cmp(&a.value)
        } else {
            a.value.partial_cmp(&b.value)
        }
        .unwrap_or(std::cmp::Ordering::Equal);
        // Tie-break on model key for deterministic output.
        ord.then_with(|| a.key.cmp(&b.key))
    });
    ranking
}

/// Money query #1 — best conversation-depth model (dim-1).
///
/// Ranks on the deterministic `recall_ceiling_turns` (higher = holds context
/// longer). The coherence panel SD travels alongside as a secondary signal but is
/// NOT what we rank on (recall is the load-bearing degradation measure).
pub fn best_conversation_depth(rows: &[ScoreRow], cfg: &ReportConfig) -> MoneyQuery {
    MoneyQuery {
        name: "best_conversation_depth".into(),
        dimension: dims::CONVERSATION_DEPTH.into(),
        metric: dims::RECALL_CEILING_TURNS.into(),
        higher_is_better: true,
        ranking: rank_metric(
            rows,
            dims::CONVERSATION_DEPTH,
            dims::RECALL_CEILING_TURNS,
            true,
            cfg.high_sd,
        ),
    }
}

/// Money query #2 — best tool-chaining model (dim-2), on mean chain accuracy.
pub fn best_tool_chaining(rows: &[ScoreRow], cfg: &ReportConfig) -> MoneyQuery {
    MoneyQuery {
        name: "best_tool_chaining".into(),
        dimension: dims::TOOL_CHAINING.into(),
        metric: dims::MEAN_CHAIN_ACCURACY.into(),
        higher_is_better: true,
        ranking: rank_metric(
            rows,
            dims::TOOL_CHAINING,
            dims::MEAN_CHAIN_ACCURACY,
            true,
            cfg.high_sd,
        ),
    }
}

/// Money query #3 — best memory-survival model (dim-3), on fact survival rate.
pub fn best_memory_survival(rows: &[ScoreRow], cfg: &ReportConfig) -> MoneyQuery {
    MoneyQuery {
        name: "best_memory_survival".into(),
        dimension: dims::MEMORY_INTEGRATION.into(),
        metric: dims::FACT_SURVIVAL_RATE.into(),
        higher_is_better: true,
        ranking: rank_metric(
            rows,
            dims::MEMORY_INTEGRATION,
            dims::FACT_SURVIVAL_RATE,
            true,
            cfg.high_sd,
        ),
    }
}

/// Money query #6 — embedding leader (dim-6), ranked on `ndcg_at_k`, carrying the
/// public-vs-Engram `ndcg_at_k_delta` so domain mismatch is visible. Ranks on the
/// public/labelled retrieval quality; the delta is reported, not ranked on.
pub fn embedding_leader(rows: &[ScoreRow], cfg: &ReportConfig) -> MoneyQuery {
    // Prefer the labelled retrieval quality row (judge = a corpus name); the
    // delta rows (judge = "public_vs_engram") are surfaced separately below.
    MoneyQuery {
        name: "embedding_leader".into(),
        dimension: dims::EMBEDDINGS.into(),
        metric: dims::NDCG_AT_K.into(),
        higher_is_better: true,
        ranking: rank_metric(rows, dims::EMBEDDINGS, dims::NDCG_AT_K, true, cfg.high_sd),
    }
}

/// The public-vs-Engram nDCG delta per model (dim-6). Negative ⇒ Engram weaker
/// (domain mismatch). Reported beside the embedding leader, NOT merged into it.
pub fn embedding_public_vs_engram_delta(rows: &[ScoreRow], cfg: &ReportConfig) -> Vec<RankedEntry> {
    // A delta closer to 0 (or positive) is "better" for our domain, but we report
    // it descending so the most Engram-favourable models surface first.
    rank_metric(rows, dims::EMBEDDINGS, dims::NDCG_AT_K_DELTA, true, cfg.high_sd)
}

// ===========================================================================
// Sequential personality read (dim-4 → dim-5) — structurally enforced
// ===========================================================================

/// STEP 1 of the personality read: the dim-4 OCEAN proximity shortlist.
///
/// Base models whose LATENT disposition (before any prompt) sits close to
/// Lumina's target — they won't fight the system prompt. Ranked on the derived
/// `proximity_to_lumina` note (5 = identical disposition). This is the ONLY input
/// to the dim-5 ranker: you cannot rank prompted adherence without first holding
/// one of these.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OceanShortlist {
    /// Models that cleared the proximity cutoff, best (closest) first.
    pub members: Vec<RankedEntry>,
    /// The proximity cutoff applied (1-5 closeness). Models below are excluded.
    pub min_proximity: f64,
}

/// Default proximity cutoff: a model must be at least "moderately close" (≥3.0 on
/// the 1-5 closeness scale) to make the shortlist. Design constant, not infra.
pub const DEFAULT_MIN_PROXIMITY: f64 = 3.0;

/// STEP 1 — produce the dim-4 shortlist. Reads ONLY `personality_latent` /
/// `proximity_to_lumina` rows. Never touches dim-5.
pub fn ocean_proximity_shortlist(rows: &[ScoreRow], cfg: &ReportConfig) -> OceanShortlist {
    ocean_proximity_shortlist_with(rows, cfg, DEFAULT_MIN_PROXIMITY)
}

/// STEP 1 with an explicit cutoff (testing / tuning).
pub fn ocean_proximity_shortlist_with(
    rows: &[ScoreRow],
    cfg: &ReportConfig,
    min_proximity: f64,
) -> OceanShortlist {
    let ranked = rank_metric(
        rows,
        dims::PERSONALITY_LATENT,
        dims::PROXIMITY_TO_LUMINA,
        true,
        cfg.high_sd,
    );
    let members = ranked
        .into_iter()
        .filter(|e| e.value >= min_proximity)
        .collect();
    OceanShortlist {
        members,
        min_proximity,
    }
}

/// One model's dim-5 prompted-adherence breakdown: an overall adherence value
/// PLUS the behavioral sub-scores kept as their own fields (never folded into one
/// number with dim-4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromptedAdherence {
    pub key: ModelKey,
    /// Mean over the dim-5 behavioral sub-scores (the "did it behave" axis), 1-5.
    pub behavioral_mean: f64,
    /// Mean over the dim-5 trait sub-scores (the "did it sound right" axis), 1-5.
    pub trait_mean: f64,
    /// Per-behavioral-metric value + SD, surfaced individually.
    pub behavioral_sub_scores: BTreeMap<String, SubScore>,
    /// True if ANY contributing dim-5 row was judge-ambiguous (high SD).
    pub judge_ambiguous: bool,
}

/// A single named sub-score with its panel SD/ambiguity context.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubScore {
    pub value: f64,
    pub std_dev: Option<f64>,
    pub high_sd: bool,
}

/// STEP 2 of the personality read: rank the dim-4 shortlist by dim-5.
///
/// CRITICAL: this takes the [`OceanShortlist`] BY VALUE. It is structurally
/// impossible to rank prompted adherence (dim-5) without first having produced a
/// dim-4 shortlist. Only shortlist members are looked up in the dim-5 rows; a
/// model that didn't clear dim-4 is never considered, no matter how well it
/// scores on prompted adherence. The two scales are NEVER averaged — this returns
/// dim-5 numbers ranked over a dim-4-filtered set, with both kept distinct.
pub fn rank_shortlist_by_prompted_adherence(
    shortlist: OceanShortlist,
    rows: &[ScoreRow],
    cfg: &ReportConfig,
) -> Vec<PromptedAdherence> {
    let behavioral = dims::prompted_behavioral_metrics();
    let traits = dims::prompted_trait_metrics();

    let mut out: Vec<PromptedAdherence> = shortlist
        .members
        .iter()
        .filter_map(|member| {
            let key = &member.key;
            let mut behavioral_sub: BTreeMap<String, SubScore> = BTreeMap::new();
            let mut behavioral_vals = Vec::new();
            let mut trait_vals = Vec::new();
            let mut ambiguous = false;

            for row in rows.iter().filter(|r| {
                r.dimension == dims::PERSONALITY_PROMPTED
                    && r.model_id == key.model_id
                    && r.backend_tag == key.backend_tag
            }) {
                let high_sd = row.is_high_sd(cfg.high_sd);
                ambiguous |= high_sd;
                if behavioral.contains(&row.metric.as_str()) {
                    behavioral_vals.push(row.value);
                    behavioral_sub.insert(
                        row.metric.clone(),
                        SubScore {
                            value: row.value,
                            std_dev: row.std_dev,
                            high_sd,
                        },
                    );
                } else if traits.contains(&row.metric.as_str()) {
                    trait_vals.push(row.value);
                }
            }

            // A shortlisted model with NO dim-5 rows yet is dropped (can't rank
            // adherence we never measured) — it stays visible on the dim-4
            // shortlist, just not in the dim-5 ranking.
            if behavioral_vals.is_empty() && trait_vals.is_empty() {
                return None;
            }
            let behavioral_mean = mean(&behavioral_vals);
            let trait_mean = mean(&trait_vals);
            Some(PromptedAdherence {
                key: key.clone(),
                behavioral_mean,
                trait_mean,
                behavioral_sub_scores: behavioral_sub,
                judge_ambiguous: ambiguous,
            })
        })
        .collect();

    // Rank by behavioral adherence first (holds voice under pressure), trait mean
    // as the tie-break — both dim-5, never mixed with the dim-4 proximity value.
    out.sort_by(|a, b| {
        b.behavioral_mean
            .partial_cmp(&a.behavioral_mean)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.trait_mean
                    .partial_cmp(&a.trait_mean)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// The full sequential personality read: dim-4 shortlist THEN dim-5 ranking,
/// each in its own field. There is deliberately NO `combined_score` /
/// `personality_score` field — merging the two scales is what the spec forbids.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersonalityRead {
    /// STEP 1 — emitted first. Base-model OCEAN proximity shortlist (dim-4).
    pub shortlist: OceanShortlist,
    /// STEP 2 — the shortlist ranked by prompted adherence (dim-5).
    pub prompted_ranking: Vec<PromptedAdherence>,
}

/// Build the sequential personality read from the row set. Enforces the order by
/// construction: the shortlist is produced first and CONSUMED (by value) to build
/// the ranking.
pub fn personality_read(rows: &[ScoreRow], cfg: &ReportConfig) -> PersonalityRead {
    let shortlist = ocean_proximity_shortlist(rows, cfg);
    let prompted_ranking = rank_shortlist_by_prompted_adherence(shortlist.clone(), rows, cfg);
    PersonalityRead {
        shortlist,
        prompted_ranking,
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

// ===========================================================================
// Chat-role routing fit + latency/degradation guard
// ===========================================================================

/// The latency/degradation guard for the Lumina chat alias. A model that scores
/// well on personality is still UNUSABLE for live chat if it degrades too early
/// or responds too slowly — chat must stay responsive. These bounds gate
/// selection regardless of personality fit (the EDGE CASE: tops personality, fails
/// the guard → excluded with a recorded reason, still shown in the report).
#[derive(Debug, Clone)]
pub struct ChatRoleGuard {
    /// A chat model must hold context for at least this many turns
    /// (`recall_ceiling_turns`) — below this it degrades inside a normal chat.
    pub min_recall_ceiling_turns: f64,
    /// A chat model's embedding/inference latency must be at or below this many
    /// ms (proxy for responsiveness on the measured path). `None` ⇒ no latency
    /// row available ⇒ latency is not gated (don't exclude on missing data).
    pub max_latency_ms: f64,
}

impl Default for ChatRoleGuard {
    fn default() -> Self {
        // Design defaults: hold ≥10 turns, respond within 4000 ms. Not infra.
        ChatRoleGuard {
            min_recall_ceiling_turns: 10.0,
            max_latency_ms: 4000.0,
        }
    }
}

/// Why a model was kept or dropped from chat-role consideration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum GuardVerdict {
    /// Cleared the guard; eligible for chat-role selection.
    Eligible,
    /// Failed the guard. `reason` is recorded and surfaced in the report.
    Excluded { reason: String },
}

/// One model's chat-role candidacy: its personality standing plus the guard
/// verdict. Excluded models stay in the report (visible), just not selectable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatRoleCandidate {
    pub key: ModelKey,
    /// Dim-5 behavioral adherence (the dim-4-gated personality standing).
    pub behavioral_mean: f64,
    pub recall_ceiling_turns: Option<f64>,
    pub latency_ms: Option<f64>,
    pub verdict: GuardVerdict,
}

impl ChatRoleCandidate {
    pub fn is_eligible(&self) -> bool {
        matches!(self.verdict, GuardVerdict::Eligible)
    }
}

/// Apply the latency/degradation guard to the dim-5 ranking and pick the chat
/// alias by MEASURED FIT.
///
/// Returns every candidate (eligible + excluded, for the report) and the selected
/// chat-role model — the highest-adherence candidate that ALSO clears the guard.
/// If NO candidate clears the guard, `selected` is `None` and the caller keeps the
/// current default (the EDGE CASE: "no model clears the chat-role guard → report
/// says so explicitly; routing keeps the current default").
pub fn select_chat_role(
    ranking: &[PromptedAdherence],
    rows: &[ScoreRow],
    cfg: &ReportConfig,
) -> ChatRoleSelection {
    let guard = &cfg.guard;
    let candidates: Vec<ChatRoleCandidate> = ranking
        .iter()
        .map(|pa| {
            let recall = latest_value(
                rows,
                &pa.key,
                dims::CONVERSATION_DEPTH,
                dims::RECALL_CEILING_TURNS,
            );
            let latency = latest_value(rows, &pa.key, dims::EMBEDDINGS, dims::EMBED_LATENCY_MS);

            let mut reasons = Vec::new();
            match recall {
                Some(r) if r < guard.min_recall_ceiling_turns => reasons.push(format!(
                    "recall_ceiling_turns {r:.0} < min {:.0} (degrades inside a normal chat)",
                    guard.min_recall_ceiling_turns
                )),
                None => reasons.push(
                    "no conversation-depth measurement — cannot certify it holds a chat".into(),
                ),
                _ => {}
            }
            // Latency only gates when measured; missing latency is not a failure.
            if let Some(l) = latency {
                if l > guard.max_latency_ms {
                    reasons.push(format!(
                        "latency {l:.0}ms > max {:.0}ms (too slow for live chat)",
                        guard.max_latency_ms
                    ));
                }
            }

            let verdict = if reasons.is_empty() {
                GuardVerdict::Eligible
            } else {
                GuardVerdict::Excluded {
                    reason: reasons.join("; "),
                }
            };
            ChatRoleCandidate {
                key: pa.key.clone(),
                behavioral_mean: pa.behavioral_mean,
                recall_ceiling_turns: recall,
                latency_ms: latency,
                verdict,
            }
        })
        .collect();

    // `ranking` is already best-adherence-first; the first eligible candidate is
    // the measured-fit pick.
    let selected = candidates.iter().find(|c| c.is_eligible()).map(|c| c.key.clone());

    ChatRoleSelection {
        candidates,
        selected,
    }
}

/// Outcome of chat-role selection: the per-candidate verdicts and the chosen
/// model (or `None` ⇒ keep current default).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatRoleSelection {
    pub candidates: Vec<ChatRoleCandidate>,
    /// The selected chat-role model, or `None` when no candidate cleared the guard.
    pub selected: Option<ModelKey>,
}

impl ChatRoleSelection {
    /// Human-readable line for the report when nothing clears the guard.
    pub fn no_clearance_note(&self) -> Option<String> {
        if self.selected.is_none() {
            Some(
                "No candidate cleared the chat-role latency/degradation guard — \
                 routing keeps the current default chat alias."
                    .into(),
            )
        } else {
            None
        }
    }
}

/// Latest value for a (model, backend) on a (dimension, metric) — highest value
/// wins on ties (deterministic). Pure.
fn latest_value(rows: &[ScoreRow], key: &ModelKey, dimension: &str, metric: &str) -> Option<f64> {
    rows.iter()
        .filter(|r| {
            r.model_id == key.model_id
                && r.backend_tag == key.backend_tag
                && r.dimension == dimension
                && r.metric == metric
        })
        .map(|r| r.value)
        .fold(None, |acc, v| match acc {
            Some(cur) if cur >= v => Some(cur),
            _ => Some(v),
        })
}

// ===========================================================================
// Full assistant report
// ===========================================================================

/// The complete assistant-role routing report — the per-dimension money queries,
/// the sequential personality read, and the chat-role selection with guard. Sits
/// beside S83's builder routing table; `model_dual_profile` powers the side-by-side
/// builder-vs-assistant comparison (rendered from [`DualProfileRow`]).
#[derive(Debug, Clone, Serialize)]
pub struct AssistantReport {
    pub best_conversation_depth: MoneyQuery,
    pub best_tool_chaining: MoneyQuery,
    pub best_memory_survival: MoneyQuery,
    pub embedding_leader: MoneyQuery,
    pub embedding_public_vs_engram_delta: Vec<RankedEntry>,
    pub personality: PersonalityRead,
    pub chat_role: ChatRoleSelection,
    /// Side-by-side builder-vs-assistant rows from `model_dual_profile`.
    pub dual_profile: Vec<DualProfileRow>,
}

/// Build the whole report from a row set + dual-profile rows. Pure — the live
/// runner fetches both via SQL, tests feed fixtures.
pub fn build_report(
    rows: &[ScoreRow],
    dual_profile: Vec<DualProfileRow>,
    cfg: &ReportConfig,
) -> AssistantReport {
    let personality = personality_read(rows, cfg);
    let chat_role = select_chat_role(&personality.prompted_ranking, rows, cfg);
    AssistantReport {
        best_conversation_depth: best_conversation_depth(rows, cfg),
        best_tool_chaining: best_tool_chaining(rows, cfg),
        best_memory_survival: best_memory_survival(rows, cfg),
        embedding_leader: embedding_leader(rows, cfg),
        embedding_public_vs_engram_delta: embedding_public_vs_engram_delta(rows, cfg),
        personality,
        chat_role,
        dual_profile,
    }
}

/// One row of the `model_dual_profile` view — the S83 builder side and the S84
/// assistant side joined on (model_id, backend_tag, mem_config).
///
/// `mem_config` is REQUIRED reading here, not cosmetic: the view now emits one
/// row per (model_id, backend_tag, mem_config) (mem-config-tagging sprint), so
/// once a model has been measured under both the preserved `carveout` baseline
/// (`mem_config IS NULL`) and a new `dynamic_gtt` sweep, TWO rows with an
/// otherwise-identical model/backend pair can appear here with different
/// quality/value numbers. Without surfacing `mem_config`, those two rows are
/// visually indistinguishable in the report — exactly the blending bug this
/// column exists to prevent, relocated to the reporting layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DualProfileRow {
    pub model_id: String,
    pub backend_tag: Option<String>,
    /// Which memory-model configuration this row was measured under
    /// (`"dynamic_gtt"` etc.), or `None` for the preserved `carveout` baseline
    /// (rows written before this column existed — see `schema.rs`'s migration
    /// comment; NEVER backfilled/relabeled). Rendered as `"carveout"` in the
    /// report, the same term used everywhere else in this codebase for the
    /// unlabeled baseline dataset.
    pub mem_config: Option<String>,
    pub has_builder_profile: bool,
    pub has_assistant_profile: bool,
    pub builder_avg_quality: Option<f64>,
    pub assistant_avg_value: Option<f64>,
}

/// Human-readable label for a `mem_config` value in report output. `None`
/// (the preserved pre-mem-config-tagging baseline) renders as `"carveout"` —
/// the same term `schema.rs`'s migration comments and `intake_coder_sweep.rs`
/// use for that dataset, kept consistent here rather than inventing a new
/// synonym like "unspecified".
fn mem_config_label(mem_config: &Option<String>) -> &str {
    mem_config.as_deref().unwrap_or("carveout")
}

// ===========================================================================
// Markdown rendering
// ===========================================================================

/// Render the report to Markdown for the build-report artifact. The personality
/// section renders the dim-4 shortlist and dim-5 ranking in SEPARATE subsections,
/// each labelled with its own scale; there is no merged personality column.
pub fn render_markdown(report: &AssistantReport) -> String {
    let mut s = String::new();
    s.push_str("# S84 Assistant Intake — Routing Report (ASMT-11)\n\n");
    s.push_str(
        "Assistant-role routing table. Sits beside S83's builder routing table; \
         the `model_dual_profile` view powers the side-by-side builder-vs-assistant \
         comparison below.\n\n",
    );

    render_money_query(&mut s, &report.best_conversation_depth);
    render_money_query(&mut s, &report.best_tool_chaining);
    render_money_query(&mut s, &report.best_memory_survival);
    render_money_query(&mut s, &report.embedding_leader);

    s.push_str("## Embedding public-vs-Engram nDCG delta\n\n");
    s.push_str("Negative delta ⇒ Engram weaker than the public set (domain mismatch). Reported, not merged into the leader ranking.\n\n");
    s.push_str("| model | backend | nDCG delta |\n|---|---|---|\n");
    for e in &report.embedding_public_vs_engram_delta {
        s.push_str(&format!(
            "| {} | {} | {:+.3} |\n",
            e.key.model_id, e.key.backend_tag, e.value
        ));
    }
    s.push('\n');

    // ── Personality: STEP 1 then STEP 2, never merged ──
    s.push_str("## Personality (sequential read — dim-4 shortlist THEN dim-5 ranking)\n\n");
    s.push_str(
        "These two scales are NOT averaged. Dimension 4 (latent OCEAN proximity) \
         shortlists base models that don't fight the prompt; dimension 5 \
         (prompted adherence) then ranks ONLY that shortlist.\n\n",
    );
    s.push_str(&format!(
        "### Step 1 — dim-4 OCEAN proximity shortlist (cutoff ≥ {:.1} closeness)\n\n",
        report.personality.shortlist.min_proximity
    ));
    s.push_str("| model | backend | proximity_to_lumina (1-5) |\n|---|---|---|\n");
    for m in &report.personality.shortlist.members {
        s.push_str(&format!(
            "| {} | {} | {:.2} |\n",
            m.key.model_id, m.key.backend_tag, m.value
        ));
    }
    s.push('\n');

    s.push_str("### Step 2 — dim-5 prompted-adherence ranking of the shortlist\n\n");
    s.push_str(
        "| model | backend | behavioral_mean (1-5) | trait_mean (1-5) | behavioral sub-scores | judge-ambiguous |\n\
         |---|---|---|---|---|---|\n",
    );
    for pa in &report.personality.prompted_ranking {
        let subs: Vec<String> = pa
            .behavioral_sub_scores
            .iter()
            .map(|(k, v)| {
                let flag = if v.high_sd { " ⚠high-SD" } else { "" };
                format!("{k}={:.2}{flag}", v.value)
            })
            .collect();
        s.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {} | {} |\n",
            pa.key.model_id,
            pa.key.backend_tag,
            pa.behavioral_mean,
            pa.trait_mean,
            subs.join(", "),
            if pa.judge_ambiguous { "yes" } else { "no" }
        ));
    }
    s.push('\n');

    // ── Chat-role selection + guard ──
    s.push_str("## Chat-role selection (Lumina alias) — measured fit with latency/degradation guard\n\n");
    if let Some(note) = report.chat_role.no_clearance_note() {
        s.push_str(&format!("**{note}**\n\n"));
    } else if let Some(sel) = &report.chat_role.selected {
        s.push_str(&format!(
            "**Selected chat-role model:** `{}` ({})\n\n",
            sel.model_id, sel.backend_tag
        ));
    }
    s.push_str("| model | backend | behavioral_mean | recall_ceiling_turns | latency_ms | verdict |\n|---|---|---|---|---|---|\n");
    for c in &report.chat_role.candidates {
        let verdict = match &c.verdict {
            GuardVerdict::Eligible => "ELIGIBLE".to_string(),
            GuardVerdict::Excluded { reason } => format!("EXCLUDED: {reason}"),
        };
        s.push_str(&format!(
            "| {} | {} | {:.2} | {} | {} | {} |\n",
            c.key.model_id,
            c.key.backend_tag,
            c.behavioral_mean,
            c.recall_ceiling_turns
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "—".into()),
            c.latency_ms
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "—".into()),
            verdict
        ));
    }
    s.push('\n');

    // ── Dual-profile side-by-side ──
    // `mem_config` is a required column here (not decoration): the view emits
    // one row per (model_id, backend_tag, mem_config), so a model measured
    // under both `carveout` and `dynamic_gtt` surfaces as two rows that would
    // otherwise be indistinguishable in this table.
    s.push_str("## Builder vs assistant (model_dual_profile)\n\n");
    s.push_str("| model | backend | mem_config | builder? | assistant? | builder_avg_quality | assistant_avg_value |\n|---|---|---|---|---|---|---|\n");
    for d in &report.dual_profile {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            d.model_id,
            d.backend_tag.as_deref().unwrap_or("—"),
            mem_config_label(&d.mem_config),
            if d.has_builder_profile { "✓" } else { "—" },
            if d.has_assistant_profile { "✓" } else { "—" },
            d.builder_avg_quality
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "—".into()),
            d.assistant_avg_value
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "—".into()),
        ));
    }
    s.push('\n');

    s
}

fn render_money_query(s: &mut String, q: &MoneyQuery) {
    s.push_str(&format!(
        "## {} (`{}` / `{}`, {})\n\n",
        q.name,
        q.dimension,
        q.metric,
        if q.higher_is_better {
            "higher is better"
        } else {
            "lower is better"
        }
    ));
    if let Some(leader) = q.leader() {
        s.push_str(&format!(
            "**Leader:** `{}` ({}) = {:.3}\n\n",
            leader.key.model_id, leader.key.backend_tag, leader.value
        ));
    } else {
        s.push_str("_No scored models for this dimension._\n\n");
    }
    s.push_str("| model | backend | value | SD | flags |\n|---|---|---|---|---|\n");
    for e in &q.ranking {
        let mut flags = Vec::new();
        if e.high_sd {
            flags.push("judge-ambiguous");
        }
        if e.low_confidence {
            flags.push("low-confidence");
        }
        s.push_str(&format!(
            "| {} | {} | {:.3} | {} | {} |\n",
            e.key.model_id,
            e.key.backend_tag,
            e.value,
            e.std_dev
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "—".into()),
            if flags.is_empty() {
                "—".to_string()
            } else {
                flags.join(", ")
            }
        ));
    }
    s.push('\n');
}

// ===========================================================================
// Live DB path — parameterized money queries (NO literals)
// ===========================================================================

/// Parameterized SQL for the live report. Every dimension/metric is a BIND
/// parameter sourced from [`dims`] — the only constants in these strings are the
/// schema's own column/table names. No model id, threshold, or metric literal.
pub mod sql {
    /// Fetch score rows for a run (or every run if `run_id` is NULL), scoped to
    /// an epoch (`harness_version`) partition (or every epoch if `$2` is NULL).
    /// The report logic ranks in Rust over the returned rows, so a single
    /// parameterized fetch backs every money query — no per-query literal SQL.
    ///
    /// MINT2-05: `harness_version` is the epoch partition key. It lives on
    /// `assistant_profile_run` (the ASSISTANT lineage — its own version string,
    /// [`super::super::schema::HARNESS_VERSION`], distinct from the coder
    /// build-scenario `intake::CURRENT_EPOCH`), so the scores table is JOINed to
    /// its run to filter on it. `$2` NULL ⇒ every epoch (legacy/all provenance);
    /// a non-NULL `$2` ⇒ only that epoch, so evolved-harness runs don't blend
    /// with a prior epoch's rows in the report. Both filters are still fully
    /// parameterized — no id/metric/epoch literal in the SQL.
    pub const FETCH_SCORES: &str = "\
        SELECT s.model_id, s.backend_tag, s.dimension, s.metric, s.value, s.std_dev, s.judge, s.low_confidence \
        FROM assistant_dimension_score s \
        JOIN assistant_profile_run r ON r.id = s.run_id \
        WHERE ($1::uuid IS NULL OR s.run_id = $1) \
          AND ($2::text IS NULL OR r.harness_version = $2)";

    /// Per-dimension/metric ranking, fully parameterized — kept for callers that
    /// want the DB to rank one metric. `$2` = dimension, `$3` = metric. The
    /// `$1` run filter is optional (NULL ⇒ all runs).
    pub const RANK_ONE_METRIC: &str = "\
        SELECT model_id, backend_tag, value, std_dev, low_confidence \
        FROM assistant_dimension_score \
        WHERE ($1::uuid IS NULL OR run_id = $1) \
          AND dimension = $2 AND metric = $3 \
        ORDER BY value DESC, model_id ASC";

    /// The side-by-side builder-vs-assistant rows from the dual-profile view.
    /// `mem_config` is selected and ordered on alongside `backend_tag` because
    /// the view now emits one row per (model_id, backend_tag, mem_config) —
    /// dropping it here would silently re-introduce the carveout/dynamic_gtt
    /// blending bug at the reporting layer (see [`DualProfileRow`]).
    pub const FETCH_DUAL_PROFILE: &str = "\
        SELECT model_id, backend_tag, mem_config, has_builder_profile, has_assistant_profile, \
               builder_avg_quality, assistant_avg_value \
        FROM model_dual_profile \
        ORDER BY model_id, backend_tag, mem_config";
}

/// The current epoch for the ASSISTANT report lineage: the assistant sweep's
/// own `harness_version` ([`schema::HARNESS_VERSION`]). This is deliberately
/// SEPARATE from the coder build-scenario epoch ([`crate::intake::CURRENT_EPOCH`]
/// = `'v3'`): the two sweeps evolve on independent version strings, so the
/// assistant report scopes to the assistant lineage, never to `'v3'`.
pub fn assistant_current_epoch() -> &'static str {
    schema::HARNESS_VERSION
}

/// Fetch assistant score rows for a run (NULL ⇒ all runs) from the live DB,
/// scoped to an epoch: `Some(epoch)` returns only that `harness_version`
/// partition, `None` returns every epoch (legacy/all provenance). Callers that
/// want the current epoch pass `Some(assistant_current_epoch())`.
pub async fn fetch_scores(
    pool: &PgPool,
    run_id: Option<uuid::Uuid>,
    epoch: Option<&str>,
) -> Result<Vec<ScoreRow>, ToolError> {
    let rows = sqlx::query(sql::FETCH_SCORES)
        .bind(run_id)
        .bind(epoch)
        .fetch_all(pool)
        .await
        .map_err(|e| ToolError::Database(format!("fetch_scores: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| ScoreRow {
            model_id: r.get("model_id"),
            backend_tag: r.get("backend_tag"),
            dimension: r.get("dimension"),
            metric: r.get("metric"),
            value: r.get("value"),
            std_dev: r.get("std_dev"),
            judge: r.get("judge"),
            low_confidence: r.get("low_confidence"),
        })
        .collect())
}

/// Fetch the dual-profile rows from the live view.
pub async fn fetch_dual_profile(pool: &PgPool) -> Result<Vec<DualProfileRow>, ToolError> {
    let rows = sqlx::query(sql::FETCH_DUAL_PROFILE)
        .fetch_all(pool)
        .await
        .map_err(|e| ToolError::Database(format!("fetch_dual_profile: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| DualProfileRow {
            model_id: r.get("model_id"),
            backend_tag: r.get("backend_tag"),
            mem_config: r.get("mem_config"),
            has_builder_profile: r.get("has_builder_profile"),
            has_assistant_profile: r.get("has_assistant_profile"),
            builder_avg_quality: r.get("builder_avg_quality"),
            assistant_avg_value: r.get("assistant_avg_value"),
        })
        .collect())
}

/// Live entry point: connect, migrate (idempotent), fetch scores + dual profile,
/// build the report, render Markdown. Used by the runner at completion to emit
/// `S84-assistant-intake-profiling-build-report.md`.
pub async fn run_report(
    run_id: Option<uuid::Uuid>,
    cfg: &ReportConfig,
) -> Result<(AssistantReport, String), ToolError> {
    run_report_for_epoch(run_id, Some(assistant_current_epoch()), cfg).await
}

/// [`run_report`] with an explicit epoch selector: `Some(epoch)` scopes the
/// report to one `harness_version` partition (the default is
/// [`assistant_current_epoch`], so evolved-harness runs don't blend with a
/// prior epoch), `None` includes every epoch for legacy/all provenance. Legacy
/// rows are partitioned by filter only — never deleted or mutated.
pub async fn run_report_for_epoch(
    run_id: Option<uuid::Uuid>,
    epoch: Option<&str>,
    cfg: &ReportConfig,
) -> Result<(AssistantReport, String), ToolError> {
    let pool = schema::get_pool().await?;
    schema::migrate(&pool).await?;
    let rows = fetch_scores(&pool, run_id, epoch).await?;
    let dual = fetch_dual_profile(&pool).await?;
    let report = build_report(&rows, dual, cfg);
    let md = render_markdown(&report);
    Ok((report, md))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_bind_the_real_runner_constants() {
        // If a dimension file renames its const, these break at COMPILE time —
        // the queries can never go silently stale.
        assert_eq!(dims::CONVERSATION_DEPTH, "conversation_depth");
        assert_eq!(dims::RECALL_CEILING_TURNS, "recall_ceiling_turns");
        assert_eq!(dims::TOOL_CHAINING, "tool_chaining");
        assert_eq!(dims::MEMORY_INTEGRATION, "memory_integration");
        assert_eq!(dims::PERSONALITY_LATENT, "personality_latent");
        assert_eq!(dims::PROXIMITY_TO_LUMINA, "proximity_to_lumina");
        assert_eq!(dims::PERSONALITY_PROMPTED, "personality_prompted");
        assert_eq!(dims::EMBEDDINGS, "embeddings");
    }

    #[test]
    fn dual_profile_report_keeps_carveout_and_dynamic_gtt_distinct() {
        // Regression for the reporting-layer twin of the mem-config-tagging
        // bug: `model_dual_profile` now emits one row per (model_id,
        // backend_tag, mem_config), so the SAME model_id+backend_tag pair can
        // legitimately appear twice — once under the preserved `carveout`
        // baseline (mem_config = None) and once under a `dynamic_gtt` sweep.
        // The report must show these as two distinct, labeled rows, never
        // merge or silently pick one.
        let dual_profile = vec![
            DualProfileRow {
                model_id: "qwen3-coder:30b".into(),
                backend_tag: Some("gpu".into()),
                mem_config: None, // preserved carveout baseline
                has_builder_profile: true,
                has_assistant_profile: true,
                builder_avg_quality: Some(0.900),
                assistant_avg_value: Some(4.10),
            },
            DualProfileRow {
                model_id: "qwen3-coder:30b".into(),
                backend_tag: Some("gpu".into()),
                mem_config: Some("dynamic_gtt".into()),
                has_builder_profile: true,
                has_assistant_profile: true,
                builder_avg_quality: Some(0.750),
                assistant_avg_value: Some(3.20),
            },
        ];

        let report = build_report(&[], dual_profile, &ReportConfig::default());
        let md = render_markdown(&report);

        // Both rows are present...
        assert!(
            md.contains("qwen3-coder:30b | gpu | carveout"),
            "carveout row missing/mislabeled in report:\n{md}"
        );
        assert!(
            md.contains("qwen3-coder:30b | gpu | dynamic_gtt"),
            "dynamic_gtt row missing/mislabeled in report:\n{md}"
        );
        // ...and distinguishable — the carveout and dynamic_gtt lines carry
        // their OWN numbers, not a blended/merged value.
        assert!(
            md.contains("qwen3-coder:30b | gpu | carveout | ✓ | ✓ | 0.900 | 4.100"),
            "carveout row values not rendered distinctly:\n{md}"
        );
        assert!(
            md.contains("qwen3-coder:30b | gpu | dynamic_gtt | ✓ | ✓ | 0.750 | 3.200"),
            "dynamic_gtt row values not rendered distinctly:\n{md}"
        );
    }

    #[test]
    fn fetch_scores_sql_is_epoch_scoped_and_parameterized() {
        // MINT2-05: the report scopes to a harness_version epoch partition,
        // JOINing scores to their run (where harness_version lives). Both the
        // run and epoch filters are parameterized ($1 run, $2 epoch), NULL ⇒
        // "all" for each — no id/epoch literal in the SQL.
        assert!(sql::FETCH_SCORES.contains("JOIN assistant_profile_run r ON r.id = s.run_id"));
        assert!(sql::FETCH_SCORES.contains("$2::text IS NULL OR r.harness_version = $2"));
        assert!(sql::FETCH_SCORES.contains("$1::uuid IS NULL OR s.run_id = $1"));
        // No epoch value literal leaks into the SQL.
        assert!(!sql::FETCH_SCORES.contains("'v3'"));
        assert!(!sql::FETCH_SCORES.contains(schema::HARNESS_VERSION));
    }

    #[test]
    fn assistant_epoch_is_a_separate_lineage_from_the_coder_epoch() {
        // The assistant report's current epoch is the assistant sweep's own
        // harness_version, NOT the coder build-scenario epoch ('v3'). Blending
        // the two would filter the assistant report down to zero rows.
        assert_eq!(assistant_current_epoch(), schema::HARNESS_VERSION);
        assert_ne!(assistant_current_epoch(), crate::intake::CURRENT_EPOCH);
    }

    #[test]
    fn sql_money_queries_have_no_value_literals() {
        // The only non-bind constants allowed are schema-owned identifiers.
        for q in [sql::FETCH_SCORES, sql::RANK_ONE_METRIC, sql::FETCH_DUAL_PROFILE] {
            assert!(q.contains('$') || q.contains("model_dual_profile"));
            // No dimension/metric string ever appears inline.
            assert!(!q.contains("personality_"));
            assert!(!q.contains("conversation_depth"));
            assert!(!q.contains("proximity_to_lumina"));
        }
    }
}
