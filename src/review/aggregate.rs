//! Aggregation logic: combine per-provider verdicts into a single
//! `aggregate_verdict` + `complete` flag, per structure.
//!
//! Pure and side-effect-free so every combination of available/errored
//! providers can be unit tested without any network I/O.

use super::prompt::Structure;
use serde::{Deserialize, Serialize};

/// KGFIND-02: one concrete issue a reviewer surfaced, structured beyond the
/// coarse `VERDICT:`/`reasoning` pair. Purely additive -- extracted
/// best-effort from an optional `FINDINGS_JSON:` block in the provider's raw
/// reply (see `prompt::parse_findings`); absence never affects verdict
/// parsing or aggregation.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Finding {
    pub category: String,
    pub severity: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    pub description: String,
    /// CXEG-07: set only by the Tier-C consistency/elegance lens's
    /// cross-source disagreement check (`review::consistency`) when two or
    /// more independent sources (the pinned lens provider, or a correctness
    /// reviewer's own `category:consistency|elegance` tag) reported a
    /// DIFFERING take on the same `(category, file, symbol)` anchor. `None`
    /// for every other finding (the plain KGFIND-02 correctness-reviewer
    /// path never sets this). Advisory metadata only -- never affects dedup
    /// keying, scope resolution, or (per CXEG-07's load-bearing safety
    /// property) `aggregate_verdict`/`complete`.
    #[serde(default)]
    pub subjective: Option<bool>,
}

/// RVXR-02: what a seat actually CONTRIBUTED to the panel. This is the
/// distinction the aggregate turns on -- not "did the HTTP call return", which
/// is what `error.is_none()` answered before.
///
/// **The invariant this type exists to enforce: an absent seat is not a vote,
/// and zero votes is not an approval.** A seat that was evicted mid-inference,
/// that errored, or that replied without a parseable `VERDICT:` produced NO
/// judgement. It must not count toward a panel verdict in EITHER direction --
/// it is neither an approval nor a dissent, and it must not sit in the
/// denominator of a majority.
///
/// **Why `Evicted` and `Errored` are separate variants but behave identically:**
/// both are non-voting, always. The split is REPORTING only -- an operator
/// wants to know "the seat was preempted for VRAM" apart from "the seat's auth
/// is broken", because the remedies differ. Crucially, this means a
/// MISCLASSIFICATION IN EITHER DIRECTION CANNOT AFFECT A VERDICT: nothing can
/// promote a non-vote into a vote by being labelled wrong. That is what lets
/// the eviction marker (produced by Chord, CHRD RVXR-01) land on its own
/// schedule without this half's safety depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The seat returned a real, parseable judgement. The ONLY voting variant.
    Voted,
    /// The seat was preempted mid-inference (model evicted to free VRAM).
    /// Non-voting.
    Evicted,
    /// Dispatch failed (unreachable, auth, rate limit, timeout, ...).
    /// Non-voting.
    Errored,
    /// Dispatch SUCCEEDED but the reply carried no parseable `VERDICT:` token.
    ///
    /// This is the quiet one, and it is the shape of the S130 failure: the
    /// transport was fine, so the old `error.is_none()` test called the seat
    /// "available" and counted it as a whole participant -- inflating the
    /// majority denominator and, worse, reporting `complete: true` for a panel
    /// that had a seat which never actually judged anything. Prose without a
    /// verdict is not a verdict.
    NoVerdict,
}

impl Outcome {
    /// Whether this seat's verdict counts toward the panel. Exactly one
    /// variant does.
    pub fn is_voting(self) -> bool {
        matches!(self, Outcome::Voted)
    }
}

/// One provider's outcome, as surfaced in the tool's `providers` output array.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderResult {
    pub provider: String,
    pub verdict: String,
    pub reasoning: String,
    pub error: Option<String>,
    /// RVXR-02: this seat's contribution class. See [`Outcome`].
    pub outcome: Outcome,
    /// KGFIND-02: structured findings parsed from the reply's optional
    /// `FINDINGS_JSON:` block. Empty when absent/malformed/not applicable
    /// (e.g. an errored/degraded provider) -- never affects `verdict`.
    #[serde(default)]
    pub findings: Vec<Finding>,
}

impl ProviderResult {
    /// Whether the DISPATCH succeeded. Retained for the diagnostic/reporting
    /// paths that legitimately mean "did the transport work" -- but NOT for
    /// deciding whether this seat votes. Use [`Self::is_voting`] for that: a
    /// dispatch can succeed and still produce no judgement (`NoVerdict`).
    pub fn is_available(&self) -> bool {
        self.error.is_none()
    }

    /// RVXR-02: whether this seat's verdict counts toward the panel.
    pub fn is_voting(&self) -> bool {
        self.outcome.is_voting()
    }
}

/// RVXR-02: the seat census for a run -- how many seats were SEATED versus how
/// many actually VOTED, and why the rest did not.
///
/// This exists because "the gate passed" is an incomplete report when a seat
/// was absent. Reporting it is the point: on the S130 epic a seat died for
/// twelve consecutive gates while the aggregate kept reporting verdicts as
/// though the panel were whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct Quorum {
    /// Seats dispatched (the panel the caller asked for).
    pub seated: usize,
    /// Seats that returned a real judgement -- the only ones that voted.
    pub voted: usize,
    pub evicted: usize,
    pub errored: usize,
    pub no_verdict: usize,
}

impl Quorum {
    pub fn of(results: &[ProviderResult]) -> Self {
        let mut q = Quorum { seated: results.len(), ..Default::default() };
        for r in results {
            match r.outcome {
                Outcome::Voted => q.voted += 1,
                Outcome::Evicted => q.evicted += 1,
                Outcome::Errored => q.errored += 1,
                Outcome::NoVerdict => q.no_verdict += 1,
            }
        }
        q
    }

    /// Names of the seats that did NOT vote, with why -- so a run can say
    /// which seats were absent instead of silently absorbing them.
    pub fn absent_seats(results: &[ProviderResult]) -> Vec<(String, Outcome)> {
        results
            .iter()
            .filter(|r| !r.is_voting())
            .map(|r| (r.provider.clone(), r.outcome))
            .collect()
    }
}

/// The verdict token reported when NOT ONE seat produced a judgement.
///
/// It is deliberately NOT `UNKNOWN` (which historically also meant "a provider
/// replied but I could not parse it") and emphatically not `APPROVE`. An empty
/// panel returning APPROVE would gate-pass code that nothing reviewed -- the
/// worst failure this system can have.
pub const NO_QUORUM: &str = "NO_QUORUM";

/// Aggregate per-provider results into `(aggregate_verdict, complete)`.
///
/// RVXR-02: every structure below counts VOTING seats only ([`Outcome::Voted`]),
/// and every structure returns [`NO_QUORUM`] with `complete: false` when not one
/// seat voted. `complete` means "every seated provider voted" -- not "every HTTP
/// call returned".
///
/// - `single`: mirrors the one provider's verdict; `complete` iff it voted.
/// - `panel_majority`: whichever verdict has strictly more than 50% of the
///   VOTING providers; ties or no-majority fail safe to `REQUEST_CHANGES`.
/// - `panel_unanimous`: `APPROVE` only if ALL voting providers said `APPROVE`
///   (and at least one voted), else `REQUEST_CHANGES`.
/// - `adversarial_pair`: providers\[0\] is "defend", providers\[1\] is "attack".
///   Reflects whether defend survived attack's refutation attempt:
///     - attack says `REFUTED` -> `REQUEST_CHANGES` (attack succeeded)
///     - defend says `REQUEST_CHANGES` -> `REQUEST_CHANGES`
///     - otherwise (defend `APPROVE`, attack `NOT_REFUTED`) -> `APPROVE`
///   `complete` iff both sides voted.
pub fn aggregate(structure: Structure, results: &[ProviderResult]) -> (String, bool) {
    match structure {
        Structure::Single => aggregate_single(results),
        Structure::PanelMajority => aggregate_panel_majority(results),
        Structure::PanelUnanimous => aggregate_panel_unanimous(results),
        Structure::AdversarialPair => aggregate_adversarial_pair(results),
        // The Epic capstone verdict is ADVISORY: it summarizes the audit (majority
        // of the auditors, fail-safe on a split) but never gates a merge. What
        // makes the capstone's END drive the KG refresh + doc engine is its
        // COMPLETION, not this verdict (see `run`'s post-hook gate) — so a
        // REQUEST_CHANGES epic that surfaced findings still refreshes docs/graph.
        Structure::Epic => aggregate_panel_majority(results),
    }
}

fn aggregate_single(results: &[ProviderResult]) -> (String, bool) {
    match results.first() {
        Some(r) if r.is_voting() => (r.verdict.clone(), true),
        // Includes the seat that dispatched fine but produced no verdict: one
        // seat that did not judge is a panel of zero votes.
        _ => (NO_QUORUM.to_string(), false),
    }
}

fn aggregate_panel_majority(results: &[ProviderResult]) -> (String, bool) {
    // RVXR-02: the denominator is VOTING seats, not "seats whose HTTP call
    // returned". A seat that produced no judgement is not half a vote against
    // and not a body to divide by -- it simply is not there.
    let voting: Vec<&ProviderResult> = results.iter().filter(|r| r.is_voting()).collect();
    let complete = voting.len() == results.len();
    if voting.is_empty() {
        // THE invariant. Nothing reviewed this; say so, never APPROVE.
        return (NO_QUORUM.to_string(), false);
    }
    let total = voting.len();
    let approve = voting.iter().filter(|r| r.verdict == "APPROVE").count();
    let reject = voting.iter().filter(|r| r.verdict == "REQUEST_CHANGES").count();
    let verdict = if approve * 2 > total {
        "APPROVE"
    } else if reject * 2 > total {
        "REQUEST_CHANGES"
    } else {
        // No strict majority (a tie) -- fail safe, never rubber-stamp.
        "REQUEST_CHANGES"
    };
    (verdict.to_string(), complete)
}

fn aggregate_panel_unanimous(results: &[ProviderResult]) -> (String, bool) {
    let voting: Vec<&ProviderResult> = results.iter().filter(|r| r.is_voting()).collect();
    let complete = voting.len() == results.len();
    if voting.is_empty() {
        return (NO_QUORUM.to_string(), false);
    }
    let all_approve = voting.iter().all(|r| r.verdict == "APPROVE");
    (if all_approve { "APPROVE" } else { "REQUEST_CHANGES" }.to_string(), complete)
}

fn aggregate_adversarial_pair(results: &[ProviderResult]) -> (String, bool) {
    let defend = results.first();
    let attack = results.get(1);
    let complete = defend.map(|d| d.is_voting()).unwrap_or(false)
        && attack.map(|a| a.is_voting()).unwrap_or(false);

    match (defend, attack) {
        // No judgement from the defence at all -- there is nothing to attack
        // and nothing to approve.
        (Some(d), _) if !d.is_voting() => (NO_QUORUM.to_string(), false),
        (Some(d), Some(a)) if a.is_voting() => {
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
            // Attack side did not judge: best-effort mirror of a defence that
            // DID judge (guaranteed by the guard arm above), but never claim
            // completeness -- the adversarial half of the structure is absent.
            (d.verdict.clone(), false)
        }
        (None, _) => (NO_QUORUM.to_string(), false),
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
            outcome: Outcome::Voted,
            findings: Vec::new(),
        }
    }

    fn err(provider: &str, reason: &str) -> ProviderResult {
        ProviderResult {
            provider: provider.into(),
            verdict: "UNKNOWN".into(),
            reasoning: String::new(),
            error: Some(reason.into()),
            outcome: Outcome::Errored,
            findings: Vec::new(),
        }
    }

    /// RVXR-02: a seat preempted mid-inference (Chord evicted the model).
    fn evicted(provider: &str) -> ProviderResult {
        ProviderResult {
            provider: provider.into(),
            verdict: "UNKNOWN".into(),
            reasoning: String::new(),
            error: Some("unavailable: chord http 409: model_evicted".into()),
            outcome: Outcome::Evicted,
            findings: Vec::new(),
        }
    }

    /// RVXR-02: the quiet failure -- the dispatch SUCCEEDED (no error at all),
    /// the model produced prose, but there is no parseable `VERDICT:`. Under
    /// the old `error.is_none()` test this seat counted as a full participant.
    fn no_verdict(provider: &str) -> ProviderResult {
        ProviderResult {
            provider: provider.into(),
            verdict: "UNKNOWN".into(),
            reasoning: "I have thoughts but never committed to a verdict.".into(),
            error: None,
            outcome: Outcome::NoVerdict,
            findings: Vec::new(),
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
        assert_eq!(aggregate(Structure::Single, &results), (NO_QUORUM.to_string(), false));
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
    fn panel_majority_all_errored_is_no_quorum_incomplete() {
        let results = vec![err("opus", "x"), err("codex", "y")];
        assert_eq!(aggregate(Structure::PanelMajority, &results), (NO_QUORUM.to_string(), false));
    }

    // ── RVXR-02: the invariant, stated directly ──────────────────────────
    //
    // "An absent seat is not a vote, and zero votes is not an approval."
    // These are the tests a mutation must not survive.

    /// THE one that matters most: every seat evicted mid-inference. Nothing
    /// reviewed the code. The run must NOT approve it, under ANY structure.
    #[test]
    fn all_seats_evicted_is_never_approve_in_any_structure() {
        let results = vec![evicted("opus"), evicted("codex"), evicted("agy")];
        for structure in [
            Structure::Single,
            Structure::PanelMajority,
            Structure::PanelUnanimous,
            Structure::AdversarialPair,
            Structure::Epic,
        ] {
            let (verdict, complete) = aggregate(structure, &results);
            assert_ne!(verdict, "APPROVE", "{structure:?} approved an all-evicted panel");
            assert_eq!(verdict, NO_QUORUM, "{structure:?} must report NO_QUORUM");
            assert!(!complete, "{structure:?} claimed completeness with zero votes");
        }
    }

    /// The mixed-cause version: not one seat produced a judgement, but for
    /// three different reasons. Still zero votes, still never an approval.
    #[test]
    fn no_seat_voted_for_mixed_reasons_is_still_no_quorum() {
        let results = vec![evicted("opus"), err("codex", "auth"), no_verdict("agy")];
        for structure in [Structure::PanelMajority, Structure::PanelUnanimous, Structure::Epic] {
            let (verdict, complete) = aggregate(structure, &results);
            assert_eq!(verdict, NO_QUORUM, "{structure:?}");
            assert!(!complete, "{structure:?}");
        }
    }

    /// One of three evicted: the majority is computed over the TWO that
    /// actually voted, not over three. Both voters approved, so 2/2 is a
    /// majority -- but the panel is NOT complete, because a seat is missing.
    #[test]
    fn one_of_three_evicted_computes_majority_over_the_remaining_two() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), evicted("agy")];
        assert_eq!(
            aggregate(Structure::PanelMajority, &results),
            ("APPROVE".to_string(), false),
            "majority over 2 voters, and never 'complete' with an absent seat"
        );

        // And the split case: 1-1 among the two voters is a TIE, which fails
        // safe. If the evicted seat were wrongly counted in the denominator
        // this would be 1 of 3 either way -- also REQUEST_CHANGES -- so the
        // discriminating assertion is the census below, not the verdict.
        let split = vec![ok("opus", "APPROVE"), ok("codex", "REQUEST_CHANGES"), evicted("agy")];
        assert_eq!(
            aggregate(Structure::PanelMajority, &split),
            ("REQUEST_CHANGES".to_string(), false)
        );
        let q = Quorum::of(&split);
        assert_eq!((q.seated, q.voted, q.evicted), (3, 2, 1));
    }

    /// A lone dissent among voters must still beat an evicted majority: two
    /// seats gone and the one survivor says REQUEST_CHANGES. If absent seats
    /// were counted as anything at all, 1-of-3 would lose its majority and the
    /// dissent would be diluted away.
    #[test]
    fn a_single_surviving_dissent_carries_the_panel() {
        let results = vec![ok("opus", "REQUEST_CHANGES"), evicted("codex"), evicted("agy")];
        assert_eq!(
            aggregate(Structure::PanelMajority, &results),
            ("REQUEST_CHANGES".to_string(), false)
        );
    }

    /// The converse, and the sharpest false-pass risk: a lone survivor that
    /// approves is a majority of ONE. The verdict may be APPROVE -- that is
    /// the honest reading of the votes cast -- but `complete` MUST be false so
    /// the gate does not read it as a whole panel.
    #[test]
    fn a_lone_surviving_approval_is_never_reported_as_complete() {
        let results = vec![ok("opus", "APPROVE"), evicted("codex"), evicted("agy")];
        let (verdict, complete) = aggregate(Structure::PanelMajority, &results);
        assert_eq!(verdict, "APPROVE");
        assert!(!complete, "a 1-of-3 panel must never claim completeness");
    }

    /// The S130 shape: the seat dispatched FINE (no error) but returned prose
    /// with no verdict. It must not be counted as a participant, and the panel
    /// must not report itself whole.
    #[test]
    fn a_dispatch_that_returned_no_verdict_does_not_count_as_a_seat() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), no_verdict("agy")];
        let (verdict, complete) = aggregate(Structure::PanelMajority, &results);
        assert_eq!(verdict, "APPROVE");
        assert!(
            !complete,
            "a seat that replied without a verdict left the panel incomplete, \
             even though its HTTP call succeeded"
        );
        // The seat is NOT errored -- `is_available()` still says the transport
        // worked. That is exactly why the voting test cannot be `is_available`.
        assert!(results[2].is_available());
        assert!(!results[2].is_voting());
    }

    #[test]
    fn quorum_census_counts_every_class_and_names_the_absent() {
        let results = vec![
            ok("opus", "APPROVE"),
            evicted("codex"),
            err("agy", "auth"),
            no_verdict("free"),
        ];
        let q = Quorum::of(&results);
        assert_eq!(q, Quorum { seated: 4, voted: 1, evicted: 1, errored: 1, no_verdict: 1 });

        let absent = Quorum::absent_seats(&results);
        assert_eq!(
            absent,
            vec![
                ("codex".to_string(), Outcome::Evicted),
                ("agy".to_string(), Outcome::Errored),
                ("free".to_string(), Outcome::NoVerdict),
            ],
            "absent seats must be NAMED with a cause, never silently absorbed"
        );
    }

    /// Found by mutation M7: every other all-absent test used seats that were
    /// ALSO errored, so `is_voting()` and `is_available()` agreed and the
    /// adversarial guard could be silently weakened to the availability check
    /// without any test noticing. The discriminating case is a defence that
    /// DISPATCHED FINE and merely never committed to a verdict: available,
    /// but not a judgement. Approving that would let prose defend a change.
    #[test]
    fn adversarial_defence_that_only_produced_prose_is_no_quorum_not_approve() {
        let results = vec![no_verdict("opus"), ok("codex", "NOT_REFUTED")];
        assert!(results[0].is_available(), "the defence's dispatch succeeded");
        assert!(!results[0].is_voting(), "...but it produced no judgement");
        assert_eq!(
            aggregate(Structure::AdversarialPair, &results),
            (NO_QUORUM.to_string(), false),
            "a defence that never judged must not be carried to APPROVE by an \
             unrefuting attacker"
        );
    }

    /// The unanimous counterpart of the same blind spot: seats that dispatched
    /// fine and said nothing. `is_available()` is true for all of them, so only
    /// a voting-based emptiness check reports NO_QUORUM here.
    #[test]
    fn unanimous_panel_of_prose_only_seats_is_no_quorum_not_approve() {
        let results = vec![no_verdict("opus"), no_verdict("codex")];
        assert!(results.iter().all(|r| r.is_available()));
        assert_eq!(
            aggregate(Structure::PanelUnanimous, &results),
            (NO_QUORUM.to_string(), false)
        );
        assert_eq!(
            aggregate(Structure::PanelMajority, &results),
            (NO_QUORUM.to_string(), false)
        );
    }

    #[test]
    fn single_seat_that_did_not_vote_is_no_quorum() {
        assert_eq!(
            aggregate(Structure::Single, &[evicted("opus")]),
            (NO_QUORUM.to_string(), false)
        );
        assert_eq!(
            aggregate(Structure::Single, &[no_verdict("opus")]),
            (NO_QUORUM.to_string(), false)
        );
    }

    #[test]
    fn unanimous_ignores_a_non_voting_seat_but_never_calls_it_complete() {
        let results = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), evicted("agy")];
        assert_eq!(
            aggregate(Structure::PanelUnanimous, &results),
            ("APPROVE".to_string(), false)
        );
    }

    // ── panel_unanimous ──────────────────────────────────────────────────

    #[test]
    fn epic_capstone_verdict_is_advisory_majority() {
        // Epic aggregates as a fail-safe majority (its verdict is advisory; the
        // capstone never gates a merge — see `should_run_kg_rebuild`).
        let all_approve = vec![ok("opus", "APPROVE"), ok("codex", "APPROVE"), ok("agy", "APPROVE")];
        assert_eq!(aggregate(Structure::Epic, &all_approve), ("APPROVE".to_string(), true));
        // Findings from a majority ⇒ advisory REQUEST_CHANGES, still "complete".
        let with_findings = vec![ok("opus", "REQUEST_CHANGES"), ok("codex", "REQUEST_CHANGES"), ok("agy", "APPROVE")];
        assert_eq!(aggregate(Structure::Epic, &with_findings), ("REQUEST_CHANGES".to_string(), true));
    }

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
    fn adversarial_pair_defend_unavailable_is_no_quorum_incomplete() {
        let results = vec![err("opus", "unavailable: timeout"), ok("codex", "NOT_REFUTED")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), (NO_QUORUM.to_string(), false));
    }

    #[test]
    fn adversarial_pair_attack_unavailable_mirrors_defend_but_incomplete() {
        let results = vec![ok("opus", "APPROVE"), err("codex", "unavailable: binary_not_found")];
        assert_eq!(aggregate(Structure::AdversarialPair, &results), ("APPROVE".to_string(), false));
    }
}
