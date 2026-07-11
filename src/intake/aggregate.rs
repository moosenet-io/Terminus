//! MINT2-03: variance-aware aggregation of the coder sweep's multi-sample runs.
//!
//! WHY THIS EXISTS — the reliability signal best-of-N throws away:
//! The pre-existing `model_language_stats` matview (`assistant/schema.rs`) rolls
//! the `sample_index` repeats of each case up into per-(profile, language) point
//! aggregates. It is keyed by NEITHER `task_category`, `harness_version`, nor the
//! MINT2-01 config factors — so two quants (or two reasoning settings) of ONE
//! model collapse into a single misleading number, and what it reports is a
//! best-of-N-style rollup rather than the reliability VARIANCE across samples
//! that matters most for tuning (Böckeler's point: a model that solves a case 2
//! times in 7 is NOT the same as one that solves it 7 of 7, even though a
//! best-of-N view calls both "solved").
//!
//! This module computes, per
//!   (model, task_category, harness_version, quant, reasoning_enabled,
//!    context_window_launched, temperature, top_p)
//! the fraction of samples that PASSED (effective score >= [`PASS_THRESHOLD`]),
//! the sample count, the population stddev of the effective score, and a
//! low-confidence flag for cells with too few samples to trust. The computation
//! is PURE (rows in, aggregates out) so it is unit-testable without a DB; a thin
//! storage wrapper (`storage::persist_code_run_aggregates`) persists the result
//! into the `code_run_aggregates` table (MINT2-03 migration) for the catalog
//! (MINT2-07) to read cheaply.
//!
//! EPOCH SCOPING: legacy `'v1'`/`'v2'` rows are EXCLUDED from the current epoch's
//! aggregates ([`CURRENT_EPOCH`]) at compute time, so evolved-test results are
//! never blended with a prior harness epoch (MINT2-05 formalizes epochs).

use std::collections::BTreeMap;

use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::storage;

/// A sample's effective score must be at least this (on the 0-5 scale) to count
/// as a PASS: 4 = compiles + tests + change-correct. The AGGREGATE is the RATE of
/// such passes across the samples, NOT a best-of-N pick. Kept as the single
/// source of truth for "pass" so it can't drift between compute and tests.
pub const PASS_THRESHOLD: i32 = 4;

/// The current build-scenario harness epoch. Aggregates are scoped to this;
/// legacy epochs (`'v1'`/`'v2'`) are excluded so evolved tests aren't blended
/// with results measured under a prior harness.
///
/// MINT2-05 promoted the definition to the ONE canonical place
/// ([`crate::intake::CURRENT_EPOCH`] in `intake/mod.rs`); this is a re-export of
/// that single source of truth, NOT a second definition — so existing
/// `aggregate::CURRENT_EPOCH` references (e.g. `coder_sweep.rs`) keep working
/// while there is exactly one value to bump for a future `'v4'`.
pub use crate::intake::CURRENT_EPOCH;

/// A cell with at most this many samples is flagged low-confidence: a single
/// sample cannot express variance, so its pass_rate (0.0 or 1.0) must never be
/// read as a reliable rate. `n_samples <= LOW_CONFIDENCE_MAX_SAMPLES` → flagged.
pub const LOW_CONFIDENCE_MAX_SAMPLES: i64 = 1;

/// One per-case sample row, projected to exactly the fields aggregation needs.
/// `effective_score` is the already-resolved 0-5 effective score for the sample
/// (`max(first_pass, retry)`), so this stays pure and DB-agnostic.
#[derive(Debug, Clone)]
pub struct AggregateInputRow {
    pub model: String,
    pub task_category: Option<String>,
    pub harness_version: String,
    // ---- MINT2-01 config factors: part of the grouping key ----
    pub quant: Option<String>,
    pub reasoning_enabled: Option<bool>,
    pub context_window_launched: Option<i32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Effective 0-5 score for this one sample (`max(first_pass, retry)`).
    pub effective_score: i32,
}

/// The full grouping key. CRITICAL: it includes the MINT2-01 config factors in
/// addition to (model, task_category, harness_version) so two quants / reasoning
/// settings of one model are aggregated SEPARATELY, never blended into one rate.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateKey {
    pub model: String,
    pub task_category: Option<String>,
    pub harness_version: String,
    pub quant: Option<String>,
    pub reasoning_enabled: Option<bool>,
    pub context_window_launched: Option<i32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

impl AggregateKey {
    fn from_row(r: &AggregateInputRow) -> Self {
        AggregateKey {
            model: r.model.clone(),
            task_category: r.task_category.clone(),
            harness_version: r.harness_version.clone(),
            quant: r.quant.clone(),
            reasoning_enabled: r.reasoning_enabled,
            context_window_launched: r.context_window_launched,
            temperature: r.temperature,
            top_p: r.top_p,
        }
    }

    /// A canonical, collision-free string for use as a `BTreeMap` key. `f64`
    /// is not `Ord`/`Eq`, so the two floating factors are rendered with `{:?}`
    /// (`None` vs `Some(0.7)`), which is deterministic for values that came from
    /// the same DB round-trip — identical stored values format identically. A
    /// unit-separator that cannot appear in a model name / quant string keeps
    /// distinct field boundaries unambiguous.
    fn canonical(&self) -> String {
        const US: char = '\u{1f}'; // ASCII unit separator
        format!(
            "{}{US}{}{US}{}{US}{}{US}{}{US}{}{US}{:?}{US}{:?}",
            self.model,
            self.task_category.as_deref().unwrap_or("\u{0}"),
            self.harness_version,
            self.quant.as_deref().unwrap_or("\u{0}"),
            self.reasoning_enabled
                .map(|b| if b { "1" } else { "0" })
                .unwrap_or("\u{0}"),
            self.context_window_launched
                .map(|n| n.to_string())
                .unwrap_or_else(|| "\u{0}".to_string()),
            self.temperature,
            self.top_p,
        )
    }
}

/// The variance-aware aggregate for one key. The PRIMARY reported number is
/// `pass_rate` with its `n_samples` and `score_stddev` — never a best-of-N value
/// presented as if it were reliability.
#[derive(Debug, Clone, PartialEq)]
pub struct RunAggregate {
    pub key: AggregateKey,
    /// passes / n_samples, in [0.0, 1.0].
    pub pass_rate: f64,
    /// Total samples in the cell (denominator). Retained so a single-sample cell
    /// is visibly low-confidence rather than silently presented as a clean rate.
    pub n_samples: i64,
    /// How many of the samples passed (effective score >= [`PASS_THRESHOLD`]).
    pub passes: i64,
    /// Population stddev of the effective score across the samples (0.0 for a
    /// single sample — no variance is expressible from one point).
    pub score_stddev: f64,
    /// `n_samples <= LOW_CONFIDENCE_MAX_SAMPLES`: the rate exists but must not be
    /// read as reliable.
    pub low_confidence: bool,
}

/// Mutable accumulator per key while scanning the rows once.
struct Acc {
    key: AggregateKey,
    n: i64,
    passes: i64,
    sum: f64,
    sum_sq: f64,
}

/// Compute variance-aware aggregates from per-sample rows. PURE: same input →
/// same output, no DB, no clock, no env.
///
/// - Rows NOT in the [`CURRENT_EPOCH`] are excluded (epoch scoping — legacy
///   `'v1'`/`'v2'` never pollute the `'v3'` numbers).
/// - Grouped by the full [`AggregateKey`] (incl. the MINT2-01 config factors).
/// - `pass_rate = passes / n_samples`, a sample passing iff its effective score
///   is >= [`PASS_THRESHOLD`]. A cell with zero passes yields `pass_rate = 0.0`
///   WITH `n_samples` set (distinct from "not run" = no row at all).
/// - `score_stddev` is the POPULATION stddev (divides by n), so a single sample
///   yields exactly `0.0` and every n >= 1 is defined.
/// - Output is sorted by the canonical key for deterministic ordering.
pub fn compute_aggregates(rows: &[AggregateInputRow]) -> Vec<RunAggregate> {
    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();

    for r in rows {
        // Epoch scoping: skip anything that isn't the current epoch.
        if r.harness_version != CURRENT_EPOCH {
            continue;
        }
        let key = AggregateKey::from_row(r);
        let acc = groups.entry(key.canonical()).or_insert_with(|| Acc {
            key,
            n: 0,
            passes: 0,
            sum: 0.0,
            sum_sq: 0.0,
        });
        let s = r.effective_score as f64;
        acc.n += 1;
        if r.effective_score >= PASS_THRESHOLD {
            acc.passes += 1;
        }
        acc.sum += s;
        acc.sum_sq += s * s;
    }

    groups
        .into_values()
        .map(|a| {
            let n = a.n.max(1) as f64; // n >= 1 always here (only inserted on a row)
            let mean = a.sum / n;
            // Population variance = E[x^2] - E[x]^2; clamped at 0 to absorb any
            // floating-point negative dust so stddev is never NaN.
            let variance = (a.sum_sq / n - mean * mean).max(0.0);
            RunAggregate {
                pass_rate: a.passes as f64 / n,
                n_samples: a.n,
                passes: a.passes,
                score_stddev: variance.sqrt(),
                low_confidence: a.n <= LOW_CONFIDENCE_MAX_SAMPLES,
                key: a.key,
            }
        })
        .collect()
}

/// IMPURE orchestrator (the pure star is [`compute_aggregates`] above): read the
/// current epoch's per-sample rows, compute the variance-aware aggregates, and
/// persist them into `code_run_aggregates` for the catalog (MINT2-07) to read
/// cheaply. Returns how many aggregate cells were written.
///
/// Scoped to [`CURRENT_EPOCH`] end to end: only `'v3'` rows are read, computed,
/// and re-persisted — legacy epochs are never touched or blended. Safe to call
/// after every sweep: the persist is a DELETE-then-INSERT of ONLY this epoch's
/// rows in one transaction. If the `code_run_aggregates` table is absent
/// (un-migrated DB), the persist errors on the missing relation — callers wire
/// this best-effort so a not-yet-migrated host degrades to "no aggregates
/// refreshed" rather than failing the sweep (see the coder-sweep call site).
pub async fn recompute_and_persist_current_epoch(pool: &PgPool) -> Result<usize, ToolError> {
    let rows = storage::read_aggregate_input_rows(pool, CURRENT_EPOCH).await?;
    let aggregates = compute_aggregates(&rows);
    storage::persist_code_run_aggregates(pool, CURRENT_EPOCH, &aggregates).await?;
    Ok(aggregates.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, eff: i32) -> AggregateInputRow {
        AggregateInputRow {
            model: model.to_string(),
            task_category: Some("blitz".to_string()),
            harness_version: CURRENT_EPOCH.to_string(),
            quant: Some("Q4_K_M".to_string()),
            reasoning_enabled: Some(true),
            context_window_launched: Some(8192),
            temperature: Some(0.7),
            top_p: Some(0.9),
            effective_score: eff,
        }
    }

    /// 7 samples, 5 fail (score 0) and 2 pass (score 5): pass_rate ≈ 0.286,
    /// n_samples 7, nonzero stddev, not low-confidence.
    #[test]
    fn seven_samples_five_fail() {
        let mut rows = vec![row("m", 5), row("m", 5)];
        rows.extend((0..5).map(|_| row("m", 0)));
        let out = compute_aggregates(&rows);
        assert_eq!(out.len(), 1, "one cell");
        let a = &out[0];
        assert_eq!(a.n_samples, 7);
        assert_eq!(a.passes, 2);
        assert!((a.pass_rate - 2.0 / 7.0).abs() < 1e-9, "pass_rate {}", a.pass_rate);
        assert!((a.pass_rate - 0.2857).abs() < 1e-3);
        assert!(a.score_stddev > 0.0, "nonzero stddev, got {}", a.score_stddev);
        assert!(!a.low_confidence, "7 samples is not low-confidence");
    }

    /// All-pass with identical scores → pass_rate 1.0, zero stddev.
    #[test]
    fn all_pass_zero_variance() {
        let rows: Vec<_> = (0..5).map(|_| row("m", 5)).collect();
        let out = compute_aggregates(&rows);
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.n_samples, 5);
        assert_eq!(a.passes, 5);
        assert!((a.pass_rate - 1.0).abs() < 1e-12);
        assert!(a.score_stddev.abs() < 1e-12, "zero stddev, got {}", a.score_stddev);
        assert!(!a.low_confidence);
    }

    /// A single sample reports its rate but is flagged low-confidence (n=1),
    /// and its stddev is exactly 0 (no variance from one point).
    #[test]
    fn single_sample_low_confidence() {
        let out = compute_aggregates(&[row("m", 5)]);
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.n_samples, 1);
        assert!(a.low_confidence, "n=1 must be flagged low-confidence");
        assert!((a.pass_rate - 1.0).abs() < 1e-12);
        assert_eq!(a.score_stddev, 0.0);
    }

    /// Zero passing samples → pass_rate 0.0 WITH n_samples set (distinct from
    /// "not run" = no row → no aggregate at all).
    #[test]
    fn zero_passes_still_has_n() {
        let rows: Vec<_> = (0..3).map(|_| row("m", 0)).collect();
        let out = compute_aggregates(&rows);
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.pass_rate, 0.0);
        assert_eq!(a.n_samples, 3);
        assert_eq!(a.passes, 0);
    }

    /// Legacy-epoch rows (`'v1'`/`'v2'`) are excluded from the current-epoch
    /// aggregate: only the `'v3'` samples are counted.
    #[test]
    fn legacy_epoch_rows_excluded() {
        let mut rows = vec![row("m", 5), row("m", 5)]; // 2 current-epoch passes
        let mut legacy1 = row("m", 0);
        legacy1.harness_version = "v1".to_string();
        let mut legacy2 = row("m", 0);
        legacy2.harness_version = "v2".to_string();
        rows.push(legacy1);
        rows.push(legacy2);
        let out = compute_aggregates(&rows);
        assert_eq!(out.len(), 1, "only the current-epoch cell survives");
        let a = &out[0];
        assert_eq!(a.n_samples, 2, "legacy rows must not inflate n_samples");
        assert_eq!(a.passes, 2);
        assert_eq!(a.key.harness_version, CURRENT_EPOCH);
    }

    /// A run made up ENTIRELY of legacy rows yields NO current-epoch aggregate.
    #[test]
    fn all_legacy_yields_empty() {
        let mut r = row("m", 5);
        r.harness_version = "v2".to_string();
        assert!(compute_aggregates(&[r]).is_empty());
    }

    /// Two quants of ONE model do NOT blend: they produce two separate cells,
    /// each with its own pass_rate. This is the EDGE CASE the item calls out.
    #[test]
    fn different_quants_do_not_blend() {
        let q4 = |eff| AggregateInputRow {
            quant: Some("Q4_K_M".to_string()),
            ..row("m", eff)
        };
        let q8 = |eff| AggregateInputRow {
            quant: Some("Q8_0".to_string()),
            ..row("m", eff)
        };
        // Q4: 2 samples both fail → 0.0. Q8: 2 samples both pass → 1.0.
        let rows = vec![q4(0), q4(0), q8(5), q8(5)];
        let out = compute_aggregates(&rows);
        assert_eq!(out.len(), 2, "two quants → two distinct cells, never blended");
        let mut rates: Vec<(String, f64)> = out
            .iter()
            .map(|a| (a.key.quant.clone().unwrap(), a.pass_rate))
            .collect();
        rates.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(rates[0].0, "Q4_K_M");
        assert_eq!(rates[0].1, 0.0);
        assert_eq!(rates[1].0, "Q8_0");
        assert_eq!(rates[1].1, 1.0);
    }

    /// Two reasoning settings of one model likewise stay separate.
    #[test]
    fn different_reasoning_settings_do_not_blend() {
        let on = AggregateInputRow { reasoning_enabled: Some(true), ..row("m", 5) };
        let off = AggregateInputRow { reasoning_enabled: Some(false), ..row("m", 0) };
        let out = compute_aggregates(&[on, off]);
        assert_eq!(out.len(), 2);
    }

    /// A NULL config factor is its own bucket, distinct from any real value.
    #[test]
    fn null_factor_is_its_own_bucket() {
        let set = AggregateInputRow { temperature: Some(0.7), ..row("m", 5) };
        let unset = AggregateInputRow { temperature: None, ..row("m", 5) };
        let out = compute_aggregates(&[set, unset]);
        assert_eq!(out.len(), 2, "Some(0.7) and None temperature are distinct cells");
    }

    /// Different models never collide.
    #[test]
    fn distinct_models_distinct_cells() {
        let out = compute_aggregates(&[row("a", 5), row("b", 0)]);
        assert_eq!(out.len(), 2);
    }
}
