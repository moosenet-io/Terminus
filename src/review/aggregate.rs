//! Aggregation logic: combine per-provider verdicts into a single
//! `aggregate_verdict` + `complete` flag, per structure.
//!
//! Pure and side-effect-free so every combination of available/errored
//! providers can be unit tested without any network I/O.

use super::prompt::Structure;
use serde::Serialize;

/// One provider's outcome, as surfaced in the tool's `providers` output array.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderResult {
    pub provider: String,
    pub verdict: String,
    pub reasoning: String,
    pub error: Option<String>,
}

impl ProviderResult {
    pub fn is_available(&self) -> bool {
        self.error.is_none()
    }
}

/// Aggregate per-provider results into `(aggregate_verdict, complete)`.
///
/// - `single`: mirrors the one provider's verdict; `complete` iff it's available.
/// - `panel_majority`: whichever verdict has strictly more than 50% of the
///   AVAILABLE (non-errored) providers; ties or no-majority fail safe to
///   `REQUEST_CHANGES`. `complete` iff every provider was available.
/// - `panel_unanimous`: `APPROVE` only if ALL available providers said
///   `APPROVE` (and at least one was available), else `REQUEST_CHANGES`.
///   `complete` iff every provider was available.
/// - `adversarial_pair`: providers\[0\] is "defend", providers\[1\] is "attack".
///   Reflects whether defend survived attack's refutation attempt:
///     - attack says `REFUTED` -> `REQUEST_CHANGES` (attack succeeded)
///     - defend says `REQUEST_CHANGES` -> `REQUEST_CHANGES`
///     - otherwise (defend `APPROVE`, attack `NOT_REFUTED`) -> `APPROVE`
///   `complete` iff both sides were available.
pub fn aggregate(structure: Structure, results: &[ProviderResult]) -> (String, bool) {
    match structure {
        Structure::Single => aggregate_single(results),
        Structure::PanelMajority => aggregate_panel_majority(results),
        Structure::PanelUnanimous => aggregate_panel_unanimous(results),
        Structure::AdversarialPair => aggregate_adversarial_pair(results),
    }
}

fn aggregate_single(results: &[ProviderResult]) -> (String, bool) {
    match results.first() {
        Some(r) if r.is_available() => (r.verdict.clone(), true),
        _ => ("UNKNOWN".to_string(), false),
    }
}

fn aggregate_panel_majority(results: &[ProviderResult]) -> (String, bool) {
    let available: Vec<&ProviderResult> = results.iter().filter(|r| r.is_available()).collect();
    let complete = available.len() == results.len();
    if available.is_empty() {
        return ("UNKNOWN".to_string(), complete);
    }
    let total = available.len();
    let approve = available.iter().filter(|r| r.verdict == "APPROVE").count();
    let reject = available.iter().filter(|r| r.verdict == "REQUEST_CHANGES").count();
    let verdict = if approve * 2 > total {
        "APPROVE"
    } else if reject * 2 > total {
        "REQUEST_CHANGES"
    } else {
        // No strict majority (tie, or split across UNKNOWN/other tokens) --
        // fail safe, never rubber-stamp.
        "REQUEST_CHANGES"
    };
    (verdict.to_string(), complete)
}

fn aggregate_panel_unanimous(results: &[ProviderResult]) -> (String, bool) {
    let available: Vec<&ProviderResult> = results.iter().filter(|r| r.is_available()).collect();
    let complete = available.len() == results.len();
    if available.is_empty() {
        return ("UNKNOWN".to_string(), complete);
    }
    let all_approve = available.iter().all(|r| r.verdict == "APPROVE");
    (if all_approve { "APPROVE" } else { "REQUEST_CHANGES" }.to_string(), complete)
}

fn aggregate_adversarial_pair(results: &[ProviderResult]) -> (String, bool) {
    let defend = results.first();
    let attack = results.get(1);
    let complete = defend.map(|d| d.is_available()).unwrap_or(false)
        && attack.map(|a| a.is_available()).unwrap_or(false);

    match (defend, attack) {
        (Some(d), _) if !d.is_available() => ("UNKNOWN".to_string(), false),
        (Some(d), Some(a)) if a.is_available() => {
            let verdict = if a.verdict == "REFUTED" {
                "REQUEST_CHANGES"
            } else if d.verdict == "REQUEST_CHANGES" {
                "REQUEST_CHANGES"
            } else {
                "APPROVE"
            };
            (verdict.to_string(), complete)
        }
        (Some(d), _) => {
            // Attack side unavailable: best-effort mirror of defend alone,
            // but never claim completeness.
            (d.verdict.clone(), false)
        }
        (None, _) => ("UNKNOWN".to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(provider: &str, verdict: &str) -> ProviderResult {
        ProviderResult {
            provider: provider.into(),
            verdict: verdict.into(),
            reasoning: "r".into(),
            error: None,
        }
    }

    fn err(provider: &str, reason: &str) -> ProviderResult {
        ProviderResult {
            provider: provider.into(),
            verdict: "UNKNOWN".into(),
            reasoning: String::new(),
            error: Some(reason.into()),
        }
    }

    // ── single ───────────────────────────────────────────────────────────

    #[test]
    fn single_mirrors_the_one_provider() {
        let results = vec![ok("opus", "APPROVE")];
        assert_eq!(aggregate(Structure::Single, &results), ("APPROVE".to_string(), true));
    }

    #[test]
    fn single_degrades_when_provider_unavailable() {
        let results = vec![err("opus", "unavailable: timeout")];
        assert_eq!(aggregate(Structure::Single, &results), ("UNKNOWN".to_string(), false));
    }

    // ── panel_majority ───────────────────────────────────────────────────

    #[test]
    fn panel_majority_all_approve() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), ok("agy", "APPROVE")];
        assert_eq!(aggregate(Structure::PanelMajority, &results), ("APPROVE".to_string(), true));
    }

    #[test]
    fn panel_majority_two_of_three_approve_wins() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), ok("agy", "REQUEST_CHANGES")];
        assert_eq!(aggregate(Structure::PanelMajority, &results), ("APPROVE".to_string(), true));
    }

    #[test]
    fn panel_majority_mixed_with_one_errored_computes_over_available_only() {
        // 2 available (1 approve, 1 reject) -> tie among available -> fail safe REQUEST_CHANGES;
        // and complete=false because one provider errored out.
        let results = vec![ok("opus", "APPROVE"), ok("codex", "REQUEST_CHANGES"), err("agy", "unavailable: binary_not_found")];
        assert_eq!(
            aggregate(Structure::PanelMajority, &results),
            ("REQUEST_CHANGES".to_string(), false)
        );
    }

    #[test]
    fn panel_majority_majority_survives_despite_one_error() {
        // 2 available both approve -> majority APPROVE, but complete=false (agy errored).
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), err("agy", "unavailable: timeout")];
        assert_eq!(aggregate(Structure::PanelMajority, &results), ("APPROVE".to_string(), false));
    }

    #[test]
    fn panel_majority_all_errored_is_unknown_incomplete() {
        let results = vec![err("opus", "x"), err("codex", "y")];
        assert_eq!(aggregate(Structure::PanelMajority, &results), ("UNKNOWN".to_string(), false));
    }

    // ── panel_unanimous ──────────────────────────────────────────────────

    #[test]
    fn panel_unanimous_all_approve_is_approve() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE")];
        assert_eq!(aggregate(Structure::PanelUnanimous, &results), ("APPROVE".to_string(), true));
    }

    #[test]
    fn panel_unanimous_one_dissent_is_request_changes() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "REQUEST_CHANGES")];
        assert_eq!(aggregate(Structure::PanelUnanimous, &results), ("REQUEST_CHANGES".to_string(), true));
    }

    #[test]
    fn panel_unanimous_ignores_errored_provider_for_verdict_but_flags_incomplete() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), err("agy", "unavailable: auth_required")];
        assert_eq!(aggregate(Structure::PanelUnanimous, &results), ("APPROVE".to_string(), false));
    }

    // ── adversarial_pair ─────────────────────────────────────────────────

    #[test]
    fn adversarial_pair_defend_survives_attack() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "NOT_REFUTED")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("APPROVE".to_string(), true));
    }

    #[test]
    fn adversarial_pair_attack_refutes_defend() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "REFUTED")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("REQUEST_CHANGES".to_string(), true));
    }

    #[test]
    fn adversarial_pair_defend_itself_requests_changes() {
        let results = vec![ok("opus", "REQUEST_CHANGES"), ok("codex", "NOT_REFUTED")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("REQUEST_CHANGES".to_string(), true));
    }

    #[test]
    fn adversarial_pair_defend_unavailable_is_unknown_incomplete() {
        let results = vec![err("opus", "unavailable: timeout"), ok("codex", "NOT_REFUTED")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("UNKNOWN".to_string(), false));
    }

    #[test]
    fn adversarial_pair_attack_unavailable_mirrors_defend_but_incomplete() {
        let results = vec![ok("opus", "APPROVE"), err("codex", "unavailable: binary_not_found")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("APPROVE".to_string(), false));
    }
}
