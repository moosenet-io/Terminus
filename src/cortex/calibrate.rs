//! CXEG-10: pure, unit-testable false-positive-rate math for the calibration
//! harness (`src/bin/cortex_calibrate.rs`).
//!
//! Kept as a standalone module with NO I/O, NO network, and NO forge/LLM
//! dependency so the FP-rate computation itself is exhaustively unit-tested
//! against hand-checked samples, independent of whether a live Gitea corpus
//! is reachable. The harness binary is a thin driver: it fetches merged PRs
//! + diffs (via the Terminus git-private forge tool, S9 — see the binary's
//! module doc), scores each with `cortex::review::compute_review` +
//! `review::run_consistency_lens_dry` in dry/capture-only mode, folds the
//! result into a [`PrRecord`], and hands the whole corpus to
//! [`compute_fp_rate`] here.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};

/// Default minimum scored-PR sample size before calibration numbers are
/// considered trustworthy enough to recommend a threshold change. Below this,
/// the report still runs on whatever exists (never refuses), but is flagged
/// `sample_small: true` and the recommendation says so instead of guessing.
pub const DEFAULT_MIN_SAMPLE: usize = 20;

/// Default target false-positive rate (fraction, not percent) calibration
/// tries to hit: how often it is acceptable for the elegance/consistency
/// machinery to have flagged a PR that, in fact, merged and shipped fine.
pub const DEFAULT_TARGET_FP_RATE: f64 = 0.10;

/// One merged-PR replay record: what CXEG-04's structural review + CXEG-07's
/// consistency lens WOULD have flagged, scored against a PR that actually
/// merged (the FP proxy — it shipped, so a flag on it is a candidate false
/// positive unless the PR itself is excluded as noise).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrRecord {
    pub number: u64,
    pub title: String,
    /// True for a merged PR. Calibration only ever replays merged PRs — a
    /// non-merged PR is filtered out by the harness before this struct is
    /// built — but the flag is kept here (rather than assumed) so
    /// `compute_fp_rate` stays a total function over whatever it's handed,
    /// including in tests that construct fixtures directly.
    pub merged: bool,
    /// True when this PR's title/body looks like a revert or hotfix (see
    /// [`looks_like_revert_or_hotfix`]) — excludable so a rushed hotfix's
    /// noise doesn't skew the FP rate. Always computed; only removed from
    /// the SCORED sample when `exclude_reverts` is passed to
    /// [`compute_fp_rate`].
    pub is_revert_or_hotfix: bool,
    /// `cortex_review`'s band for this PR's diff: "low" / "elevated" /
    /// "high" / "unknown" (the last from a `configured:false` degrade).
    pub band: String,
    /// Named CXEG-03 structural signal kinds that fired, deduped.
    pub structural_signals: Vec<String>,
    /// CXEG-07 consistency-lens finding categories that fired, deduped.
    pub consistency_categories: Vec<String>,
    /// True when this PR's diff could not be resolved into a changed-files
    /// list (e.g. the forge compare response carried no file list) — the PR
    /// is still counted in `total_prs`/`diff_unavailable`, but excluded from
    /// the scored sample since there is nothing to have scored.
    pub diff_unavailable: bool,
}

impl PrRecord {
    /// A PR "would have been flagged": its structural band read elevated or
    /// high, OR the consistency lens raised at least one finding. A "low" or
    /// "unknown" band with zero consistency findings is not a flag.
    pub fn would_flag(&self) -> bool {
        matches!(self.band.as_str(), "elevated" | "high") || !self.consistency_categories.is_empty()
    }
}

/// Per-signal firing rate against the scored sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SignalRate {
    pub signal: String,
    pub fired: usize,
    pub sample: usize,
    pub rate: f64,
}

/// The full calibration report: sample composition + would-have-flagged rate
/// + per-signal breakdown + a plain-language recommendation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationReport {
    pub total_prs: usize,
    pub scored_prs: usize,
    pub excluded_revert_hotfix: usize,
    pub diff_unavailable: usize,
    pub would_have_flagged: usize,
    pub false_positive_rate: f64,
    pub target_fp_rate: f64,
    pub sample_small: bool,
    pub min_sample: usize,
    pub signal_rates: Vec<SignalRate>,
    pub recommendation: String,
}

/// Compute the calibration report from a corpus of merged-PR replay records.
/// Pure function — no I/O, no network, exhaustively unit-testable.
///
/// `exclude_reverts` controls whether `is_revert_or_hotfix` PRs are dropped
/// from the SCORED sample (they are always counted in
/// `total_prs`/`excluded_revert_hotfix` regardless of this flag). A record
/// with `diff_unavailable: true` is never scored (nothing to score), and a
/// record with `merged: false` is dropped too — defensive, since the harness
/// should already only hand this function merged PRs, but this keeps the
/// function a true total function over whatever fixture/corpus it's given.
pub fn compute_fp_rate(
    records: &[PrRecord],
    exclude_reverts: bool,
    min_sample: usize,
    target_fp_rate: f64,
) -> CalibrationReport {
    let total_prs = records.len();
    let excluded_revert_hotfix = records.iter().filter(|r| r.is_revert_or_hotfix).count();
    let diff_unavailable = records.iter().filter(|r| r.diff_unavailable).count();

    let scored: Vec<&PrRecord> = records
        .iter()
        .filter(|r| r.merged)
        .filter(|r| !r.diff_unavailable)
        .filter(|r| !(exclude_reverts && r.is_revert_or_hotfix))
        .collect();
    let scored_prs = scored.len();

    let would_have_flagged = scored.iter().filter(|r| r.would_flag()).count();
    let false_positive_rate =
        if scored_prs == 0 { 0.0 } else { would_have_flagged as f64 / scored_prs as f64 };

    // Per-signal firing rate (structural + consistency, same namespace),
    // against the same scored denominator. A signal counts at most once per
    // PR even if it fired on multiple touched nodes within that PR.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in &scored {
        let mut seen: HashSet<&str> = HashSet::new();
        for s in r.structural_signals.iter().chain(r.consistency_categories.iter()) {
            if seen.insert(s.as_str()) {
                *counts.entry(s.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut signal_rates: Vec<SignalRate> = counts
        .into_iter()
        .map(|(signal, fired)| SignalRate {
            signal,
            fired,
            sample: scored_prs,
            rate: if scored_prs == 0 { 0.0 } else { fired as f64 / scored_prs as f64 },
        })
        .collect();
    signal_rates.sort_by(|a, b| {
        b.rate
            .partial_cmp(&a.rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.signal.cmp(&b.signal))
    });

    let sample_small = scored_prs < min_sample;
    let recommendation =
        build_recommendation(false_positive_rate, target_fp_rate, sample_small, &signal_rates);

    CalibrationReport {
        total_prs,
        scored_prs,
        excluded_revert_hotfix,
        diff_unavailable,
        would_have_flagged,
        false_positive_rate,
        target_fp_rate,
        sample_small,
        min_sample,
        signal_rates,
        recommendation,
    }
}

fn build_recommendation(
    fp_rate: f64,
    target: f64,
    sample_small: bool,
    signal_rates: &[SignalRate],
) -> String {
    if sample_small {
        return "sample too small to recommend threshold changes -- collect more merged PRs \
                 before tuning (see 'sample_small'/'min_sample')"
            .to_string();
    }
    if fp_rate <= target {
        return format!(
            "would-have-flagged rate ({:.1}%) is at or below the target ({:.1}%) -- no threshold \
             change recommended",
            fp_rate * 100.0,
            target * 100.0
        );
    }
    match signal_rates.first() {
        Some(s) => format!(
            "would-have-flagged rate ({:.1}%) exceeds target ({:.1}%); '{}' fires on {:.1}% of \
             scored merged PRs and is the top contributor -- raise its risk weight / percentile \
             threshold (CORTEX_RISK_WEIGHT_* / CORTEX_TIER_B_PERCENTILE) or the band cut-point \
             (CORTEX_RISK_BAND_ELEVATED_CUT) first, then re-run calibration",
            fp_rate * 100.0,
            target * 100.0,
            s.signal,
            s.rate * 100.0
        ),
        None => format!(
            "would-have-flagged rate ({:.1}%) exceeds target ({:.1}%) but no single signal \
             dominates -- consider raising CORTEX_RISK_BAND_ELEVATED_CUT / \
             CORTEX_RISK_SCORE_THRESHOLD broadly",
            fp_rate * 100.0,
            target * 100.0
        ),
    }
}

/// Render a [`CalibrationReport`] as JSON (for machine consumption / the
/// harness's own report-writing).
pub fn report_to_json(r: &CalibrationReport) -> Value {
    json!({
        "total_prs": r.total_prs,
        "scored_prs": r.scored_prs,
        "excluded_revert_hotfix": r.excluded_revert_hotfix,
        "diff_unavailable": r.diff_unavailable,
        "would_have_flagged": r.would_have_flagged,
        "false_positive_rate": r.false_positive_rate,
        "target_fp_rate": r.target_fp_rate,
        "sample_small": r.sample_small,
        "min_sample": r.min_sample,
        "signal_rates": r.signal_rates.iter().map(|s| json!({
            "signal": s.signal,
            "fired": s.fired,
            "sample": s.sample,
            "rate": s.rate,
        })).collect::<Vec<_>>(),
        "recommendation": r.recommendation,
    })
}

/// Heuristic: does this PR's title/body look like a revert or hotfix? Kept
/// deliberately simple and case-insensitive. A false negative here just means
/// a genuine revert/hotfix stays in the scored sample (no safety impact,
/// this is an excludable noise filter, not a gate) — a caller who wants a
/// stricter/looser filter can widen this without touching the FP-rate math.
pub fn looks_like_revert_or_hotfix(title: &str, body: Option<&str>) -> bool {
    let hay = format!("{title} {}", body.unwrap_or("")).to_ascii_lowercase();
    hay.starts_with("revert")
        || hay.contains(" revert ")
        || hay.contains("revert \"")
        || hay.contains("this reverts commit")
        || hay.contains("hotfix")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(number: u64, band: &str, structural: &[&str], consistency: &[&str]) -> PrRecord {
        PrRecord {
            number,
            title: format!("pr {number}"),
            merged: true,
            is_revert_or_hotfix: false,
            band: band.to_string(),
            structural_signals: structural.iter().map(|s| s.to_string()).collect(),
            consistency_categories: consistency.iter().map(|s| s.to_string()).collect(),
            diff_unavailable: false,
        }
    }

    // ── would_flag ────────────────────────────────────────────────────────

    #[test]
    fn low_band_no_consistency_does_not_flag() {
        assert!(!rec(1, "low", &[], &[]).would_flag());
    }

    #[test]
    fn unknown_band_no_consistency_does_not_flag() {
        assert!(!rec(1, "unknown", &[], &[]).would_flag());
    }

    #[test]
    fn elevated_band_flags() {
        assert!(rec(1, "elevated", &[], &[]).would_flag());
    }

    #[test]
    fn high_band_flags() {
        assert!(rec(1, "high", &[], &[]).would_flag());
    }

    #[test]
    fn low_band_with_consistency_finding_still_flags() {
        assert!(rec(1, "low", &[], &["duplication"]).would_flag());
    }

    // ── compute_fp_rate: hand-checked sample ────────────────────────────────

    #[test]
    fn fp_rate_hand_checked_sample() {
        // 10 merged PRs: 3 would-flag (elevated/high or consistency finding),
        // 7 would not. Expected rate: 3/10 = 0.3.
        let records = vec![
            rec(1, "low", &[], &[]),
            rec(2, "low", &[], &[]),
            rec(3, "elevated", &["centrality_spike"], &[]),
            rec(4, "low", &[], &[]),
            rec(5, "high", &["fan_out_explosion"], &[]),
            rec(6, "low", &[], &[]),
            rec(7, "low", &[], &["duplication"]),
            rec(8, "low", &[], &[]),
            rec(9, "unknown", &[], &[]),
            rec(10, "low", &[], &[]),
        ];
        let report = compute_fp_rate(&records, true, 5, 0.10);
        assert_eq!(report.total_prs, 10);
        assert_eq!(report.scored_prs, 10);
        assert_eq!(report.would_have_flagged, 3);
        assert!((report.false_positive_rate - 0.3).abs() < 1e-9);
        assert!(!report.sample_small);
    }

    #[test]
    fn fp_rate_is_zero_for_empty_scored_sample() {
        let report = compute_fp_rate(&[], true, 5, 0.10);
        assert_eq!(report.scored_prs, 0);
        assert_eq!(report.false_positive_rate, 0.0);
        assert!(report.sample_small);
    }

    // ── small-sample flag ────────────────────────────────────────────────

    #[test]
    fn small_sample_is_flagged_below_min() {
        let records: Vec<PrRecord> = (1..=4).map(|i| rec(i, "low", &[], &[])).collect();
        let report = compute_fp_rate(&records, true, DEFAULT_MIN_SAMPLE, DEFAULT_TARGET_FP_RATE);
        assert!(report.sample_small);
        assert!(report.recommendation.contains("too small"));
    }

    #[test]
    fn sample_at_or_above_min_is_not_flagged_small() {
        let records: Vec<PrRecord> = (1..=20).map(|i| rec(i, "low", &[], &[])).collect();
        let report = compute_fp_rate(&records, true, 20, DEFAULT_TARGET_FP_RATE);
        assert!(!report.sample_small);
    }

    // ── revert/hotfix exclusion ──────────────────────────────────────────

    #[test]
    fn revert_hotfix_excluded_from_scored_sample_when_flag_set() {
        let mut records = vec![
            rec(1, "high", &["centrality_spike"], &[]),
            rec(2, "low", &[], &[]),
        ];
        records[0].is_revert_or_hotfix = true;

        let excluded = compute_fp_rate(&records, true, 1, 0.10);
        assert_eq!(excluded.scored_prs, 1);
        assert_eq!(excluded.excluded_revert_hotfix, 1);
        // total_prs still counts everything, regardless of exclusion.
        assert_eq!(excluded.total_prs, 2);
        assert_eq!(excluded.would_have_flagged, 0, "the flagged revert must be dropped");

        let included = compute_fp_rate(&records, false, 1, 0.10);
        assert_eq!(included.scored_prs, 2);
        assert_eq!(included.would_have_flagged, 1);
    }

    #[test]
    fn diff_unavailable_prs_are_counted_but_never_scored() {
        let mut records = vec![rec(1, "unknown", &[], &[])];
        records[0].diff_unavailable = true;
        records.push(rec(2, "low", &[], &[]));

        let report = compute_fp_rate(&records, true, 1, 0.10);
        assert_eq!(report.total_prs, 2);
        assert_eq!(report.diff_unavailable, 1);
        assert_eq!(report.scored_prs, 1);
    }

    #[test]
    fn non_merged_records_are_never_scored() {
        let mut records = vec![rec(1, "high", &["centrality_spike"], &[])];
        records[0].merged = false;
        let report = compute_fp_rate(&records, true, 1, 0.10);
        assert_eq!(report.total_prs, 1);
        assert_eq!(report.scored_prs, 0);
    }

    // ── signal breakdown ─────────────────────────────────────────────────

    #[test]
    fn signal_rates_dedup_within_a_single_pr_and_rank_by_rate() {
        let records = vec![
            rec(1, "elevated", &["centrality_spike", "centrality_spike"], &[]),
            rec(2, "elevated", &["centrality_spike"], &[]),
            rec(3, "elevated", &["fan_out_explosion"], &[]),
            rec(4, "low", &[], &[]),
        ];
        let report = compute_fp_rate(&records, true, 1, 0.10);
        let centrality = report.signal_rates.iter().find(|s| s.signal == "centrality_spike").unwrap();
        // fired on 2 distinct PRs (1 and 2), NOT 3 (the duplicate within PR 1
        // must not double count).
        assert_eq!(centrality.fired, 2);
        assert_eq!(centrality.sample, 4);
        assert!((centrality.rate - 0.5).abs() < 1e-9);
        // Ranked first (higher rate than fan_out_explosion's 0.25).
        assert_eq!(report.signal_rates[0].signal, "centrality_spike");
    }

    #[test]
    fn recommendation_names_top_signal_when_over_target() {
        let records = vec![
            rec(1, "elevated", &["centrality_spike"], &[]),
            rec(2, "elevated", &["centrality_spike"], &[]),
            rec(3, "low", &[], &[]),
        ];
        let report = compute_fp_rate(&records, true, 1, 0.10);
        assert!(report.false_positive_rate > report.target_fp_rate);
        assert!(report.recommendation.contains("centrality_spike"));
    }

    #[test]
    fn recommendation_notes_no_change_needed_when_at_or_below_target() {
        let records: Vec<PrRecord> = (1..=10).map(|i| rec(i, "low", &[], &[])).collect();
        let report = compute_fp_rate(&records, true, 1, 0.10);
        assert_eq!(report.false_positive_rate, 0.0);
        assert!(report.recommendation.contains("no threshold change"));
    }

    // ── looks_like_revert_or_hotfix ──────────────────────────────────────

    #[test]
    fn detects_common_revert_and_hotfix_phrasings() {
        assert!(looks_like_revert_or_hotfix("Revert \"feat: add thing\"", None));
        assert!(looks_like_revert_or_hotfix("fix(terminus): hotfix for prod outage", None));
        assert!(looks_like_revert_or_hotfix(
            "fix: rollback bad change",
            Some("This reverts commit abc123.")
        ));
        assert!(!looks_like_revert_or_hotfix("feat(terminus): add cortex_calibrate", None));
    }

    // ── report_to_json ────────────────────────────────────────────────────

    #[test]
    fn report_to_json_round_trips_key_fields() {
        let records = vec![rec(1, "elevated", &["centrality_spike"], &[]), rec(2, "low", &[], &[])];
        let report = compute_fp_rate(&records, true, 1, 0.10);
        let v = report_to_json(&report);
        assert_eq!(v["total_prs"], 2);
        assert_eq!(v["scored_prs"], 2);
        assert_eq!(v["would_have_flagged"], 1);
        assert!(v["signal_rates"].as_array().unwrap().iter().any(|s| s["signal"] == "centrality_spike"));
    }
}
