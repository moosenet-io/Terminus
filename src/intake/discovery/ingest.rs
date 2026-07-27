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

use serde::Serialize;

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
// Discovery pre-step dispatch (flag precedence)
// ---------------------------------------------------------------------------

/// What the discovery pre-step should do this run, resolved PURELY from the
/// three env flags so the precedence rule is unit-testable without touching a DB
/// or Chord. Fail-safe toward no live action: DRY-RUN wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryStep {
    /// DRY-RUN / SHADOW: read + rank the brochure and emit a `[ask4-shadow]`
    /// report, taking ZERO live action. Runs REGARDLESS of the action flags.
    Shadow,
    /// Live pre-step: run the Phase-1 select-merge and/or the Phase-2b ingest
    /// per their individual flags (`select`/`ingest`). When both are false this
    /// is a no-op — the default, byte-for-byte-unchanged sweep.
    Live { select: bool, ingest: bool },
}

/// Resolve the discovery pre-step from the three flags. PRECEDENCE: `dry_run`
/// wins over both action flags — when it is set the result is always
/// [`DiscoveryStep::Shadow`], so no live ingest/augment/DB-write path is ever
/// taken even if `ingest`/`select` are somehow also set (fail-safe toward
/// no-action during the audit window). Pure over its inputs.
pub fn plan_discovery_step(dry_run: bool, select: bool, ingest: bool) -> DiscoveryStep {
    if dry_run {
        DiscoveryStep::Shadow
    } else {
        DiscoveryStep::Live { select, ingest }
    }
}

// ---------------------------------------------------------------------------
// DRY-RUN / SHADOW report (audit window)
// ---------------------------------------------------------------------------

/// The greppable tag every shadow log line carries (as a `[ask4-shadow]` prefix
/// AND the report's own `tag` field) so an operator can `grep ask4-shadow` and
/// parse the JSON over several days.
pub const SHADOW_TAG: &str = "ask4-shadow";

/// The effective settings that produced a shadow decision — echoed so the audit
/// shows WHICH config drove the selection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowConfig {
    /// Per-run selection cap (`INTAKE_ASSISTANT_DISCOVERY_MAX`).
    pub cap: usize,
    /// Minimum size floor in billions (`INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`).
    pub min_size_b: f64,
    /// State of `INTAKE_ASSISTANT_DISCOVERY_SELECT` (would-augment when live).
    pub select_flag: bool,
    /// State of `INTAKE_ASSISTANT_DISCOVERY_INGEST` (would-pull when live).
    pub ingest_flag: bool,
    /// State of `INTAKE_ASSISTANT_DISCOVERY_DRY_RUN` (always true in a report).
    pub dry_run_flag: bool,
}

/// One would-select candidate row in the shadow report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowCandidate {
    pub model_name: String,
    pub hf_repo: String,
    pub size_b: Option<f64>,
    pub gfx1151_class: String,
    pub discovery_score: Option<f64>,
    /// The candidate's CURRENT brochure status (`CandidateStatus::as_str`).
    pub current_status: String,
    /// Whether a LIVE run would ingest this (i.e. it is not already cold-stored).
    pub would_ingest: bool,
}

/// The structured shadow report — serialized to ONE JSON line under the
/// `[ask4-shadow]` tag. Machine-auditable: an operator greps the tag and parses
/// the JSON to see exactly what the loop WOULD have done, over the audit window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowReport {
    /// Always [`SHADOW_TAG`] — lets a JSON consumer filter without the prefix.
    pub tag: &'static str,
    /// Run timestamp (RFC3339). Sourced from `chrono::Utc::now()` — the same
    /// clock `intake::jobs` already stamps its records with — injected by the
    /// caller so the builder stays pure/testable.
    pub timestamp: String,
    /// Total brochure rows scanned (pre-filter).
    pub scanned: usize,
    /// Number selected (post-filter, capped) = `would_select.len()`.
    pub selected: usize,
    /// How many of the selected would trigger a real HF pull when live.
    pub would_ingest_count: usize,
    /// How many of the selected are already cold-stored (a live run would skip).
    pub already_cold_count: usize,
    /// The effective config that produced this decision.
    pub config: ShadowConfig,
    /// The ranked top-N candidates (full rows).
    pub would_select: Vec<ShadowCandidate>,
    /// Model names that are NOT already cold-stored (a live run would ingest).
    pub would_ingest: Vec<String>,
    /// Model names already cold-stored (a live run would skip the ingest for).
    pub already_cold: Vec<String>,
    /// Model names that would be fed into the assistant sweep (all selected).
    pub would_test: Vec<String>,
}

/// Build the shadow report from the already-ranked `selected` candidates (PURE —
/// no I/O, timestamp injected — so the field shape is unit-tested without a
/// clock, DB, or Chord). `scanned` is the pre-filter brochure row count.
pub fn build_shadow_report(
    scanned: usize,
    selected: &[DiscoveryCandidate],
    config: ShadowConfig,
    timestamp: String,
) -> ShadowReport {
    let would_select: Vec<ShadowCandidate> = selected
        .iter()
        .map(|c| ShadowCandidate {
            model_name: c.model_name.clone(),
            hf_repo: c.hf_repo.clone(),
            size_b: c.size_b,
            gfx1151_class: c.gfx1151_class.clone(),
            discovery_score: c.discovery_score,
            current_status: c.status.as_str().to_string(),
            would_ingest: needs_ingest(c.status),
        })
        .collect();
    let would_ingest: Vec<String> = selected
        .iter()
        .filter(|c| needs_ingest(c.status))
        .map(|c| c.model_name.clone())
        .collect();
    let already_cold: Vec<String> = selected
        .iter()
        .filter(|c| !needs_ingest(c.status))
        .map(|c| c.model_name.clone())
        .collect();
    let would_test: Vec<String> = selected.iter().map(|c| c.model_name.clone()).collect();

    ShadowReport {
        tag: SHADOW_TAG,
        timestamp,
        scanned,
        selected: selected.len(),
        would_ingest_count: would_ingest.len(),
        already_cold_count: already_cold.len(),
        config,
        would_select,
        would_ingest,
        already_cold,
        would_test,
    }
}

/// Emit `report` as a single greppable `[ask4-shadow] {json}` log line. The only
/// side effect of the whole shadow path — pure read + this log, ZERO live action.
pub fn emit_shadow_report(report: &ShadowReport) {
    match serde_json::to_string(report) {
        Ok(json) => tracing::info!("[{SHADOW_TAG}] {json}"),
        Err(e) => tracing::warn!("[{SHADOW_TAG}] failed to serialize shadow report: {e}"),
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

    // ---- DRY-RUN / shadow: precedence ----

    #[test]
    fn dry_run_wins_over_action_flags_precedence() {
        // DRY_RUN set ⇒ always Shadow, regardless of the two action flags — so no
        // live ingest/augment path can ever run during the audit window.
        assert_eq!(
            plan_discovery_step(true, false, false),
            DiscoveryStep::Shadow
        );
        assert_eq!(
            plan_discovery_step(true, true, false),
            DiscoveryStep::Shadow
        );
        assert_eq!(
            plan_discovery_step(true, false, true),
            DiscoveryStep::Shadow
        );
        assert_eq!(plan_discovery_step(true, true, true), DiscoveryStep::Shadow);
    }

    #[test]
    fn dry_run_off_yields_live_reflecting_the_action_flags() {
        assert_eq!(
            plan_discovery_step(false, false, false),
            DiscoveryStep::Live {
                select: false,
                ingest: false
            }
        );
        assert_eq!(
            plan_discovery_step(false, true, false),
            DiscoveryStep::Live {
                select: true,
                ingest: false
            }
        );
        assert_eq!(
            plan_discovery_step(false, false, true),
            DiscoveryStep::Live {
                select: false,
                ingest: true
            }
        );
    }

    // ---- DRY-RUN / shadow: zero live action even with an action flag set ----

    #[tokio::test]
    async fn dry_run_takes_zero_chord_and_zero_db_action_even_with_ingest_flag_on() {
        // Simulate run_mode's dispatch with DRY_RUN on AND the ingest flag on:
        // precedence must route to Shadow, so the live `ingest_selected` path
        // (the ONLY code that touches the Chord client + the status-advance DB
        // write) is never taken. The mocks record ZERO calls.
        let ingestor = FixedIngestor::new(IngestOutcome::Ingested {
            cold_storage_ref: None,
            bytes: None,
        });
        let advancer = RecordingAdvancer::new();
        let selected = vec![cand("m1", CandidateStatus::Discovered)];

        match plan_discovery_step(
            /*dry_run*/ true, /*select*/ false, /*ingest*/ true,
        ) {
            DiscoveryStep::Shadow => {
                // Shadow path: read + build report only, no ingestor/advancer use.
                let config = ShadowConfig {
                    cap: 5,
                    min_size_b: 7.0,
                    select_flag: false,
                    ingest_flag: true,
                    dry_run_flag: true,
                };
                let _report =
                    build_shadow_report(1, &selected, config, "2026-07-27T00:00:00Z".into());
            }
            DiscoveryStep::Live { ingest, .. } => {
                if ingest {
                    // Must NOT be reached under DRY_RUN precedence.
                    let _ = ingest_selected(&selected, &ingestor, &advancer, &cfg(5)).await;
                }
            }
        }

        assert!(
            ingestor.calls.lock().unwrap().is_empty(),
            "dry-run must make ZERO Chord ingest calls"
        );
        assert!(
            advancer.hops.lock().unwrap().is_empty(),
            "dry-run must make ZERO brochure status-advance DB writes"
        );
    }

    // ---- DRY-RUN / shadow: report contents ----

    #[test]
    fn shadow_report_contains_expected_fields_and_ingest_partition() {
        // Two selected: one Discovered (would ingest) + one already ColdStored
        // (would skip). The report must partition them and echo the config.
        let selected = vec![
            cand("m_new", CandidateStatus::Discovered),
            cand("m_cold", CandidateStatus::ColdStored),
        ];
        let config = ShadowConfig {
            cap: 5,
            min_size_b: 7.0,
            select_flag: false,
            ingest_flag: false,
            dry_run_flag: true,
        };
        let report =
            build_shadow_report(42, &selected, config.clone(), "2026-07-27T12:00:00Z".into());

        assert_eq!(report.tag, "ask4-shadow");
        assert_eq!(report.timestamp, "2026-07-27T12:00:00Z");
        assert_eq!(report.scanned, 42);
        assert_eq!(report.selected, 2);
        assert_eq!(report.config, config);

        // would_select carries the full ranked rows with per-row would_ingest.
        assert_eq!(report.would_select.len(), 2);
        let new_row = report
            .would_select
            .iter()
            .find(|c| c.model_name == "m_new")
            .unwrap();
        assert_eq!(new_row.hf_repo, "org/m_new");
        assert_eq!(new_row.current_status, "discovered");
        assert_eq!(new_row.gfx1151_class, "confirmed");
        assert_eq!(new_row.size_b, Some(8.0));
        assert_eq!(new_row.discovery_score, Some(10.0));
        assert!(
            new_row.would_ingest,
            "a Discovered candidate would be ingested live"
        );
        let cold_row = report
            .would_select
            .iter()
            .find(|c| c.model_name == "m_cold")
            .unwrap();
        assert!(
            !cold_row.would_ingest,
            "an already-cold candidate would be skipped"
        );

        // Partition + counts.
        assert_eq!(report.would_ingest, vec!["m_new".to_string()]);
        assert_eq!(report.already_cold, vec!["m_cold".to_string()]);
        assert_eq!(report.would_ingest_count, 1);
        assert_eq!(report.already_cold_count, 1);
        // would_test = everything selected (fed into the sweep when live).
        assert_eq!(
            report.would_test,
            vec!["m_new".to_string(), "m_cold".to_string()]
        );
    }

    #[test]
    fn shadow_report_serializes_to_a_single_greppable_json_line() {
        // The audit consumer greps `ask4-shadow` and parses ONE JSON line.
        let selected = vec![cand("m1", CandidateStatus::Discovered)];
        let config = ShadowConfig {
            cap: 3,
            min_size_b: 7.0,
            select_flag: true,
            ingest_flag: false,
            dry_run_flag: true,
        };
        let report = build_shadow_report(5, &selected, config, "2026-07-27T00:00:00Z".into());
        let json = serde_json::to_string(&report).expect("serializes");
        // Single line (no embedded newlines) and carries the tag + key fields.
        assert!(!json.contains('\n'));
        assert!(json.contains("\"tag\":\"ask4-shadow\""));
        assert!(json.contains("\"would_ingest\":[\"m1\"]"));
        assert!(json.contains("\"cap\":3"));
        assert!(json.contains("\"dry_run_flag\":true"));
    }
}
