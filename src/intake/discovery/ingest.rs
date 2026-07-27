//! Ask-4 Phase 2b (brochure → cold-storage INGEST pre-step): wire the Phase-1
//! candidate SELECTOR to Chord's Phase-2a HF→cold-storage ingest endpoint so a
//! selected-but-not-yet-cold-stored candidate is actually PULLED into cold
//! storage and its brochure status advanced — making it acquirable by the MINT
//! assistant sweep (`assistant::acquire::chord_acquire`, which only ever finds
//! models ALREADY in cold storage).
//!
//! ## What this is (and is NOT)
//! This is the pull/advance half of the "auto pull→test→promote" loop that
//! Phase 1 (`discovery::select`) deliberately deferred (see the TODO in
//! `select::nominations_from_selected`). Phase 1 ranks/maps brochure rows into
//! nominations but performs NO pull; a selected candidate not already cold-stored
//! just fails soft in the sweep's acquire. Phase 2b runs BEFORE that acquire and,
//! for each selected candidate whose status is not yet `ColdStored`, calls
//! Chord's ingest endpoint and — on success — advances the brochure status
//! (`Discovered`→`Fetching`→`ColdStored`) via the existing
//! [`super::upsert::transition_status`] write API (respecting the enum's
//! [`CandidateStatus::valid_transitions`]).
//!
//! ## Safety envelope
//! - **Gated, default-OFF** (`INTAKE_ASSISTANT_DISCOVERY_INGEST`,
//!   [`crate::intake::discovery_ingest_from_env`]), INDEPENDENT of Phase 1's
//!   `INTAKE_ASSISTANT_DISCOVERY_SELECT`. Both off ⇒ zero behavior change.
//! - **Fail-soft, always.** Every non-success ingest outcome
//!   (`gated_needs_token`/`too_large`/`disabled`/`error`/unauthorized/unreachable/
//!   not-configured) logs a clear reason, leaves the candidate un-advanced, and
//!   SKIPS it — never a crash, never blocking other candidates or the sweep.
//! - **Disk discipline.** Bounded by a per-run cap (reusing the discovery-max
//!   cap, `INTAKE_ASSISTANT_DISCOVERY_MAX`) — never a bulk ingest.
//! - **Idempotent + bounded.** An already-`ColdStored`/beyond candidate is a
//!   pure short-circuit (no ingest call). Each ingest call is wrapped in a hard
//!   per-candidate wall-clock timeout so a slow/hung ingest can never stall the
//!   whole sweep.
//!
//! ## Auth path reused, not reinvented
//! The live ingest client ([`ChordIngestor`]) calls
//! [`crate::intake::chord_pull::ingest_model`], which reuses the EXACT Chord-
//! control auth path the existing acquire/pull already uses:
//! `config::chord_control_url()` (`CHORD_CONTROL_URL`) + `CHORD_JWT` bearer. No
//! new secret, no second door (S7 / secrets discipline).

use std::time::Duration;

use crate::error::ToolError;
use crate::intake::chord_pull::{self, IngestOutcome};

use super::schema::{CandidateStatus, DiscoveryCandidate};

/// Config for one ingest pre-step run. Every field has a safe default;
/// [`DiscoveryIngestConfig::from_env`] overlays the env-tunable knobs. Pure data.
#[derive(Debug, Clone)]
pub struct DiscoveryIngestConfig {
    /// Max ingest ATTEMPTS this run (network calls made). Reuses the discovery
    /// selection cap (`INTAKE_ASSISTANT_DISCOVERY_MAX`, default
    /// [`super::select::DEFAULT_DISCOVERY_MAX`]) so a brochure sweep only ever
    /// trickles a few models into cold storage per window — never bulk-ingest.
    pub max_ingests: usize,
    /// Hard wall-clock bound on ONE ingest call. A slow/hung ingest hits this
    /// and is treated as a fail-soft skip, so it can never stall the sweep. From
    /// `INTAKE_ASSISTANT_DISCOVERY_INGEST_TIMEOUT_SECS` + a margin over the
    /// client's own reqwest timeout (see [`Self::from_env`]).
    pub per_candidate_timeout: Duration,
}

impl Default for DiscoveryIngestConfig {
    fn default() -> Self {
        DiscoveryIngestConfig {
            max_ingests: super::select::DEFAULT_DISCOVERY_MAX,
            // Default: the client's 900s reqwest timeout + a 60s margin, so the
            // client's own timeout normally fires first and this hard wrap only
            // catches a genuinely wedged future.
            per_candidate_timeout: Duration::from_secs(960),
        }
    }
}

impl DiscoveryIngestConfig {
    /// Overlay the env-tunable knobs on [`Default`]. None of these values is
    /// secret-shaped, so plain `std::env::var` is correct (matches
    /// `select::DiscoverySelectConfig::from_env` / `acquire::vram_ceiling_gb`);
    /// no `SecretManager` involvement, per S7.
    pub fn from_env() -> Self {
        let mut cfg = DiscoveryIngestConfig::default();
        cfg.max_ingests = super::select::parse_discovery_max(
            std::env::var("INTAKE_ASSISTANT_DISCOVERY_MAX")
                .ok()
                .as_deref(),
        );
        cfg.per_candidate_timeout = Duration::from_secs(
            parse_ingest_timeout_secs(
                std::env::var("INTAKE_ASSISTANT_DISCOVERY_INGEST_TIMEOUT_SECS")
                    .ok()
                    .as_deref(),
            )
            .saturating_add(60),
        );
        cfg
    }
}

/// Parse `INTAKE_ASSISTANT_DISCOVERY_INGEST_TIMEOUT_SECS` (the ingest client's
/// reqwest timeout). Zero/unparseable/missing → 900 (15 min). Pure over input.
/// Kept byte-consistent with `chord_pull::ingest_timeout`'s own default so the
/// step's hard wrap is always a strict superset of the client's timeout.
pub fn parse_ingest_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(900)
}

// ---------------------------------------------------------------------------
// Injectable collaborators (mocked in tests — no live Chord, no live DB)
// ---------------------------------------------------------------------------

/// The ingest surface [`ingest_selected`] depends on. The live impl
/// ([`ChordIngestor`]) calls [`chord_pull::ingest_model`]; tests inject a
/// deterministic mock. Implementations MUST NOT panic — map every failure to an
/// [`IngestOutcome`] variant.
#[async_trait::async_trait]
pub trait CandidateIngestor: Send + Sync {
    async fn ingest(&self, candidate: &DiscoveryCandidate) -> IngestOutcome;
}

/// The brochure-status-advance surface [`ingest_selected`] depends on. The live
/// impl ([`DbStatusAdvancer`]) wraps [`super::upsert::transition_status`]; tests
/// inject a recorder so the advancement logic is verified without a live
/// Postgres.
#[async_trait::async_trait]
pub trait StatusAdvancer: Send + Sync {
    async fn advance(&self, model_name: &str, to: CandidateStatus) -> Result<(), ToolError>;
}

/// Live ingest client: `POST {CHORD_CONTROL_URL}/api/models/ingest` via the
/// shared Chord-control auth path (see module doc). A `NotConfigured`
/// (`CHORD_CONTROL_URL` unset) maps to a fail-soft [`IngestOutcome::Error`].
pub struct ChordIngestor;

#[async_trait::async_trait]
impl CandidateIngestor for ChordIngestor {
    async fn ingest(&self, candidate: &DiscoveryCandidate) -> IngestOutcome {
        match chord_pull::ingest_model(&candidate.hf_repo, &candidate.model_name, None).await {
            Ok(outcome) => outcome,
            Err(not_configured) => IngestOutcome::Error {
                message: format!("chord: {not_configured}"),
            },
        }
    }
}

/// Live status advancer: the existing DISC-03 write API. `pool` is the shared
/// intake pool the caller already holds.
pub struct DbStatusAdvancer<'a> {
    pub pool: &'a sqlx::PgPool,
}

#[async_trait::async_trait]
impl StatusAdvancer for DbStatusAdvancer<'_> {
    async fn advance(&self, model_name: &str, to: CandidateStatus) -> Result<(), ToolError> {
        super::upsert::transition_status(self.pool, model_name, to).await
    }
}

// ---------------------------------------------------------------------------
// Pure decision logic
// ---------------------------------------------------------------------------

/// Whether a candidate in this status NEEDS ingesting — i.e. it is not yet in
/// cold storage. FAIL-CLOSED: an exhaustive match with NO wildcard, so a future
/// [`CandidateStatus`] forces a compile-time decision here.
///
/// `Discovered`/`Fetching` are pre-cold-storage and need the pull. Everything
/// else is either already cold-stored (`ColdStored`/`MarkedForFleet`/`Swept` —
/// the "skip if already cold" short-circuit) or terminal/ineligible
/// (`Evicted`/`Rejected` — never re-ingested). NB: `select_discovery_candidates`
/// only ever emits nominatable statuses (`Discovered`/`Fetching`/`ColdStored`/
/// `MarkedForFleet`), so in practice this only decides between the first two and
/// the cold ones; the terminal arms are defensive.
pub fn needs_ingest(status: CandidateStatus) -> bool {
    match status {
        CandidateStatus::Discovered | CandidateStatus::Fetching => true,
        CandidateStatus::ColdStored
        | CandidateStatus::MarkedForFleet
        | CandidateStatus::Swept
        | CandidateStatus::Evicted
        | CandidateStatus::Rejected => false,
    }
}

/// The ordered list of status transitions to reach `ColdStored` FROM `from`,
/// each hop validated against [`CandidateStatus::valid_transitions`]. Returns
/// `[]` for a status that doesn't need advancement (already cold or terminal).
///
/// `Discovered` → `[Fetching, ColdStored]` (two legal hops),
/// `Fetching` → `[ColdStored]` (one legal hop). The path is walked one hop at a
/// time and each hop is asserted-legal by construction, so if the enum's state
/// machine ever changes, [`advancement_path_hops_are_all_legal_transitions`]
/// fails rather than this silently emitting an illegal hop.
pub fn advancement_path(from: CandidateStatus) -> Vec<CandidateStatus> {
    let mut path = Vec::new();
    let mut cur = from;
    // Bounded walk toward ColdStored; the guard caps iterations defensively so
    // a malformed state machine can never loop forever.
    for _ in 0..CandidateStatus::ALL.len() {
        if cur == CandidateStatus::ColdStored {
            break;
        }
        let next = match cur {
            CandidateStatus::Discovered => CandidateStatus::Fetching,
            CandidateStatus::Fetching => CandidateStatus::ColdStored,
            // Any other status either is already cold or cannot legally reach
            // cold via this pre-step — stop (yields `[]` for those).
            _ => break,
        };
        // Respect the enum's declared state machine: only emit a hop the
        // predecessor actually permits.
        if !cur.valid_transitions().contains(&next) {
            break;
        }
        path.push(next);
        cur = next;
    }
    path
}

/// Whether an [`IngestOutcome`] means the model is now in cold storage (so the
/// caller advances the brochure status). Both a fresh `Ingested` and an
/// `AlreadyPresent` count — from the brochure's perspective the model IS cold-
/// stored either way. Every other variant is a fail-soft skip.
pub fn outcome_is_cold_stored(outcome: &IngestOutcome) -> bool {
    matches!(
        outcome,
        IngestOutcome::Ingested { .. } | IngestOutcome::AlreadyPresent { .. }
    )
}

/// A human-readable fail-soft reason for a non-success [`IngestOutcome`], for
/// the skip log line. Never called for the two success variants.
fn fail_soft_reason(outcome: &IngestOutcome) -> String {
    match outcome {
        IngestOutcome::Ingested { .. } | IngestOutcome::AlreadyPresent { .. } => {
            "not a failure (cold-stored)".to_string()
        }
        IngestOutcome::GatedNeedsToken { message } => format!("gated_needs_token: {message}"),
        IngestOutcome::TooLarge { message } => format!("too_large: {message}"),
        IngestOutcome::Disabled { message } => format!("disabled: {message}"),
        IngestOutcome::Error { message } => format!("error: {message}"),
        IngestOutcome::Unauthorized => "unauthorized (missing/invalid CHORD_JWT)".to_string(),
        IngestOutcome::Unreachable { detail } => format!("unreachable: {detail}"),
    }
}

// ---------------------------------------------------------------------------
// The ingest pre-step
// ---------------------------------------------------------------------------

/// Tally of one [`ingest_selected`] run (for logging + return; also asserted in
/// tests). Every selected candidate lands in exactly one bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Ingest calls made (bounded by [`DiscoveryIngestConfig::max_ingests`]).
    pub attempted: usize,
    /// Candidates advanced to `ColdStored` this run (success + full advance).
    pub cold_stored: usize,
    /// Candidates skipped because they were already cold-stored/beyond (no call).
    pub skipped_already_cold: usize,
    /// Candidates whose ingest failed soft (call made, non-success outcome).
    pub failed_soft: usize,
    /// Candidates ingested successfully but whose brochure status advance hit a
    /// DB error (the model IS cold-stored per Chord; the status will self-heal
    /// on a later `already_present` re-run). Counted apart from `cold_stored`.
    pub advance_failed: usize,
    /// Candidates not attempted because the per-run cap was already reached.
    pub capped_out: usize,
}

/// The Phase-2b pre-step: for each SELECTED candidate not yet in cold storage,
/// call Chord's ingest endpoint (bounded by the per-run cap + a per-candidate
/// timeout) and, on success, advance its brochure status to `ColdStored`. Pure
/// orchestration over the two injected collaborators — no direct I/O — so it is
/// fully unit-testable without a live Chord or Postgres. Never returns an error:
/// every per-candidate failure is folded into the [`IngestReport`] and logged.
pub async fn ingest_selected(
    selected: &[DiscoveryCandidate],
    ingestor: &dyn CandidateIngestor,
    advancer: &dyn StatusAdvancer,
    cfg: &DiscoveryIngestConfig,
) -> IngestReport {
    let mut report = IngestReport::default();

    for candidate in selected {
        // Short-circuit: already cold-stored (or terminal) ⇒ no ingest call.
        if !needs_ingest(candidate.status) {
            report.skipped_already_cold += 1;
            continue;
        }

        // Disk discipline: never exceed the per-run cap of ingest calls.
        if report.attempted >= cfg.max_ingests {
            report.capped_out += 1;
            continue;
        }

        report.attempted += 1;

        // Hard per-candidate timeout so a slow/hung ingest can never stall the
        // sweep, regardless of the client impl's own timeout.
        let outcome =
            match tokio::time::timeout(cfg.per_candidate_timeout, ingestor.ingest(candidate)).await
            {
                Ok(o) => o,
                Err(_) => {
                    report.failed_soft += 1;
                    tracing::warn!(
                        "discovery-ingest: '{}' timed out after {:?} — skipping (fail-soft)",
                        candidate.model_name,
                        cfg.per_candidate_timeout,
                    );
                    continue;
                }
            };

        if !outcome_is_cold_stored(&outcome) {
            report.failed_soft += 1;
            tracing::info!(
                "discovery-ingest: '{}' not cold-stored ({}) — leaving un-advanced, skipping (fail-soft)",
                candidate.model_name,
                fail_soft_reason(&outcome),
            );
            continue;
        }

        // Success: advance Discovered→Fetching→ColdStored via the existing
        // DISC-03 write API, one legal hop at a time.
        let mut advance_ok = true;
        for hop in advancement_path(candidate.status) {
            if let Err(e) = advancer.advance(&candidate.model_name, hop).await {
                advance_ok = false;
                tracing::warn!(
                    "discovery-ingest: '{}' ingested but status advance to '{}' failed ({e}) — \
                     will self-heal on a later already_present run",
                    candidate.model_name,
                    hop.as_str(),
                );
                break;
            }
        }

        if advance_ok {
            report.cold_stored += 1;
            tracing::info!(
                "discovery-ingest: '{}' cold-stored and brochure status advanced to '{}'",
                candidate.model_name,
                CandidateStatus::ColdStored.as_str(),
            );
        } else {
            report.advance_failed += 1;
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::discovery::schema::{FleetCategory, Modality};
    use chrono::Utc;
    use std::sync::Mutex;

    fn cand(name: &str, status: CandidateStatus) -> DiscoveryCandidate {
        DiscoveryCandidate {
            model_name: name.to_string(),
            hf_repo: format!("org/{name}"),
            category: FleetCategory::Assistant,
            status,
            modality: Some(Modality::TextGeneration),
            gfx1151_class: "confirmed".to_string(),
            size_b: Some(8.0),
            vram_footprint_gb: None,
            discovery_source: "hf_trending".to_string(),
            discovery_score: Some(10.0),
            discovered_at: Utc::now(),
            last_seen_at: Utc::now(),
            fetched_at: None,
            marked_for_fleet_at: None,
            evicted_at: None,
            retained_profile: None,
            rationale: None,
        }
    }

    /// Ingestor that returns a fixed outcome and counts the calls it received.
    struct FixedIngestor {
        outcome: IngestOutcome,
        calls: Mutex<Vec<String>>,
    }
    impl FixedIngestor {
        fn new(outcome: IngestOutcome) -> Self {
            FixedIngestor {
                outcome,
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl CandidateIngestor for FixedIngestor {
        async fn ingest(&self, candidate: &DiscoveryCandidate) -> IngestOutcome {
            self.calls
                .lock()
                .unwrap()
                .push(candidate.model_name.clone());
            self.outcome.clone()
        }
    }

    /// Ingestor that never resolves — models a hung ingest for the timeout test.
    struct HangingIngestor;
    #[async_trait::async_trait]
    impl CandidateIngestor for HangingIngestor {
        async fn ingest(&self, _candidate: &DiscoveryCandidate) -> IngestOutcome {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    /// Advancer that records every (model, to) hop; optionally fails.
    struct RecordingAdvancer {
        hops: Mutex<Vec<(String, CandidateStatus)>>,
        fail: bool,
    }
    impl RecordingAdvancer {
        fn new() -> Self {
            RecordingAdvancer {
                hops: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            RecordingAdvancer {
                hops: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }
    #[async_trait::async_trait]
    impl StatusAdvancer for RecordingAdvancer {
        async fn advance(&self, model_name: &str, to: CandidateStatus) -> Result<(), ToolError> {
            self.hops.lock().unwrap().push((model_name.to_string(), to));
            if self.fail {
                Err(ToolError::Database("simulated advance failure".into()))
            } else {
                Ok(())
            }
        }
    }

    fn cfg(max: usize) -> DiscoveryIngestConfig {
        DiscoveryIngestConfig {
            max_ingests: max,
            per_candidate_timeout: Duration::from_secs(30),
        }
    }

    // ---- pure decision logic ----

    #[test]
    fn needs_ingest_only_pre_cold_statuses() {
        assert!(needs_ingest(CandidateStatus::Discovered));
        assert!(needs_ingest(CandidateStatus::Fetching));
        assert!(!needs_ingest(CandidateStatus::ColdStored));
        assert!(!needs_ingest(CandidateStatus::MarkedForFleet));
        assert!(!needs_ingest(CandidateStatus::Swept));
        assert!(!needs_ingest(CandidateStatus::Evicted));
        assert!(!needs_ingest(CandidateStatus::Rejected));
    }

    #[test]
    fn advancement_path_discovered_and_fetching_and_none_for_cold() {
        assert_eq!(
            advancement_path(CandidateStatus::Discovered),
            vec![CandidateStatus::Fetching, CandidateStatus::ColdStored]
        );
        assert_eq!(
            advancement_path(CandidateStatus::Fetching),
            vec![CandidateStatus::ColdStored]
        );
        assert!(advancement_path(CandidateStatus::ColdStored).is_empty());
        assert!(advancement_path(CandidateStatus::MarkedForFleet).is_empty());
    }

    #[test]
    fn advancement_path_hops_are_all_legal_transitions() {
        // Every emitted hop must be permitted by the predecessor's own
        // valid_transitions() — so this can never emit an illegal transition the
        // DB write API would reject.
        for start in [CandidateStatus::Discovered, CandidateStatus::Fetching] {
            let mut cur = start;
            for hop in advancement_path(start) {
                assert!(
                    cur.valid_transitions().contains(&hop),
                    "{cur:?} -> {hop:?} is not a valid transition"
                );
                cur = hop;
            }
            assert_eq!(
                cur,
                CandidateStatus::ColdStored,
                "path must end cold-stored"
            );
        }
    }

    #[test]
    fn outcome_cold_stored_classification() {
        assert!(outcome_is_cold_stored(&IngestOutcome::Ingested {
            cold_storage_ref: None,
            bytes: None
        }));
        assert!(outcome_is_cold_stored(&IngestOutcome::AlreadyPresent {
            cold_storage_ref: None
        }));
        assert!(!outcome_is_cold_stored(&IngestOutcome::Disabled {
            message: "off".into()
        }));
        assert!(!outcome_is_cold_stored(&IngestOutcome::TooLarge {
            message: "big".into()
        }));
        assert!(!outcome_is_cold_stored(&IngestOutcome::GatedNeedsToken {
            message: "gated".into()
        }));
        assert!(!outcome_is_cold_stored(&IngestOutcome::Error {
            message: "boom".into()
        }));
        assert!(!outcome_is_cold_stored(&IngestOutcome::Unauthorized));
        assert!(!outcome_is_cold_stored(&IngestOutcome::Unreachable {
            detail: "refused".into()
        }));
    }

    // ---- ingest_selected: advancement on success ----

    #[tokio::test]
    async fn ingested_advances_discovered_through_fetching_to_cold_stored() {
        let ingestor = FixedIngestor::new(IngestOutcome::Ingested {
            cold_storage_ref: Some("cs://x".into()),
            bytes: Some(1),
        });
        let advancer = RecordingAdvancer::new();
        let selected = vec![cand("m1", CandidateStatus::Discovered)];
        let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.cold_stored, 1);
        assert_eq!(report.failed_soft, 0);
        // Both hops issued, in order.
        assert_eq!(
            *advancer.hops.lock().unwrap(),
            vec![
                ("m1".to_string(), CandidateStatus::Fetching),
                ("m1".to_string(), CandidateStatus::ColdStored),
            ]
        );
    }

    #[tokio::test]
    async fn already_present_advances_fetching_candidate_one_hop() {
        let ingestor = FixedIngestor::new(IngestOutcome::AlreadyPresent {
            cold_storage_ref: None,
        });
        let advancer = RecordingAdvancer::new();
        let selected = vec![cand("m1", CandidateStatus::Fetching)];
        let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;

        assert_eq!(report.cold_stored, 1);
        assert_eq!(
            *advancer.hops.lock().unwrap(),
            vec![("m1".to_string(), CandidateStatus::ColdStored)]
        );
    }

    // ---- ingest_selected: each fail-soft branch leaves status unchanged ----

    #[tokio::test]
    async fn each_fail_soft_outcome_leaves_candidate_unadvanced() {
        for outcome in [
            IngestOutcome::GatedNeedsToken {
                message: "gated".into(),
            },
            IngestOutcome::TooLarge {
                message: "big".into(),
            },
            IngestOutcome::Disabled {
                message: "off".into(),
            },
            IngestOutcome::Error {
                message: "boom".into(),
            },
            IngestOutcome::Unauthorized,
            IngestOutcome::Unreachable {
                detail: "refused".into(),
            },
        ] {
            let ingestor = FixedIngestor::new(outcome.clone());
            let advancer = RecordingAdvancer::new();
            let selected = vec![cand("m1", CandidateStatus::Discovered)];
            let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;

            assert_eq!(report.attempted, 1, "outcome={outcome:?}");
            assert_eq!(report.cold_stored, 0, "outcome={outcome:?}");
            assert_eq!(report.failed_soft, 1, "outcome={outcome:?}");
            assert!(
                advancer.hops.lock().unwrap().is_empty(),
                "no status advance on fail-soft outcome={outcome:?}"
            );
        }
    }

    // ---- ingest_selected: skip-if-already-cold short-circuit ----

    #[tokio::test]
    async fn already_cold_candidate_short_circuits_without_calling_ingest() {
        let ingestor = FixedIngestor::new(IngestOutcome::Ingested {
            cold_storage_ref: None,
            bytes: None,
        });
        let advancer = RecordingAdvancer::new();
        let selected = vec![
            cand("cold", CandidateStatus::ColdStored),
            cand("marked", CandidateStatus::MarkedForFleet),
        ];
        let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;

        assert_eq!(
            report.attempted, 0,
            "no ingest calls for already-cold candidates"
        );
        assert_eq!(report.skipped_already_cold, 2);
        assert!(
            ingestor.calls.lock().unwrap().is_empty(),
            "ingest client never called"
        );
        assert!(advancer.hops.lock().unwrap().is_empty());
    }

    // ---- ingest_selected: per-run cap ----

    #[tokio::test]
    async fn per_run_cap_bounds_ingest_calls_and_marks_the_rest_capped() {
        let ingestor = FixedIngestor::new(IngestOutcome::Ingested {
            cold_storage_ref: None,
            bytes: None,
        });
        let advancer = RecordingAdvancer::new();
        let selected = vec![
            cand("m1", CandidateStatus::Discovered),
            cand("m2", CandidateStatus::Discovered),
            cand("m3", CandidateStatus::Discovered),
        ];
        let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(2)).await;

        assert_eq!(report.attempted, 2, "cap of 2 bounds the ingest calls");
        assert_eq!(report.cold_stored, 2);
        assert_eq!(
            report.capped_out, 1,
            "the third is capped out, not attempted"
        );
        assert_eq!(
            ingestor.calls.lock().unwrap().len(),
            2,
            "only two ingest calls made"
        );
    }

    // ---- ingest_selected: advance-failure accounting (fail-soft on DB error) ----

    #[tokio::test]
    async fn ingested_but_advance_db_error_is_counted_apart_and_never_panics() {
        let ingestor = FixedIngestor::new(IngestOutcome::Ingested {
            cold_storage_ref: None,
            bytes: None,
        });
        let advancer = RecordingAdvancer::failing();
        let selected = vec![cand("m1", CandidateStatus::Discovered)];
        let report = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.cold_stored, 0);
        assert_eq!(report.advance_failed, 1);
    }

    // ---- ingest_selected: per-candidate timeout ----

    #[tokio::test(start_paused = true)]
    async fn hung_ingest_hits_per_candidate_timeout_and_fails_soft() {
        let advancer = RecordingAdvancer::new();
        let selected = vec![cand("m1", CandidateStatus::Discovered)];
        let cfg = DiscoveryIngestConfig {
            max_ingests: 5,
            per_candidate_timeout: Duration::from_secs(5),
        };
        // With the paused clock, the timeout elapses in virtual time; a
        // regression that awaited the pending future forever would hang the test.
        let report = ingest_selected(&selected, &HangingIngestor, &advancer, &cfg).await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.failed_soft, 1);
        assert_eq!(report.cold_stored, 0);
        assert!(advancer.hops.lock().unwrap().is_empty());
    }

    // ---- config from_env ----

    #[test]
    fn parse_ingest_timeout_secs_defaults_and_clamps() {
        assert_eq!(parse_ingest_timeout_secs(None), 900);
        assert_eq!(parse_ingest_timeout_secs(Some("0")), 900);
        assert_eq!(parse_ingest_timeout_secs(Some("bad")), 900);
        assert_eq!(parse_ingest_timeout_secs(Some("120")), 120);
    }
}
