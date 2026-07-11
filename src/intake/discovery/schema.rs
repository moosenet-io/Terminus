//! DISC-01 (S114, TERM #251): the brochure's storage schema — types + SQL only.
//!
//! WHY THIS EXISTS — a distinct lifecycle stage from the Model Fleet Catalog:
//! [`crate::intake::catalog`] (`model_fleet_catalog`/`model_fleet_catalog_cell`,
//! MINT2-07/08) answers "what has been TESTED, and how did it score?" for
//! models already in the fleet. This module's `model_discovery_candidate` table
//! answers a different, earlier question: "what's a CANDIDATE — newly available
//! on HuggingFace, not yet acquired or tested?" The two relate ONLY by a
//! `model_name` join; brochure fields never get added to the fleet-catalog cell
//! table, and fleet-catalog fields never get added here (see the S114 grounding
//! summary's "naming footgun avoided" note — this registry is always called the
//! **brochure**, never "catalog," to keep it distinct from both Terminus's model
//! fleet catalog and Chord's unrelated MCP *tool* catalog).
//!
//! This item is STORAGE ONLY: [`FleetCategory`]/[`CandidateStatus`] (mirroring
//! [`crate::intake::catalog::CoverageStatus`]'s explicit-enum-with-`as_str()`
//! pattern, plus a `from_str()` this module adds since callers — DISC-02's tool
//! args, DISC-03's transition API — need to parse an untrusted string back into
//! an enum, which `CoverageStatus` itself never needed to do), the
//! [`DiscoveryCandidate`] row type, and the migration SQL live here. No business
//! logic: DISC-03 owns the upsert/transition API that actually writes rows.

use crate::error::ToolError;

/// Which fleet category a candidate targets. Snake_case `as_str()`/`from_str()`
/// round-trip; an unrecognized string is a clean [`ToolError::InvalidArgument`],
/// never a silent default — every caller (DISC-02's tool filter, DISC-03's
/// upsert path) must handle "I don't know that category" explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FleetCategory {
    ToolRouter,
    WriterSlm,
    Assistant,
    Coder,
    Embedding,
    Visual,
    Voice,
}

impl FleetCategory {
    /// All variants, in the spec's documented order — used by tests and by any
    /// caller that needs to enumerate every category (e.g. DISC-06's per-
    /// category refresh loop).
    pub const ALL: [FleetCategory; 7] = [
        FleetCategory::ToolRouter,
        FleetCategory::WriterSlm,
        FleetCategory::Assistant,
        FleetCategory::Coder,
        FleetCategory::Embedding,
        FleetCategory::Visual,
        FleetCategory::Voice,
    ];

    /// The stable snake_case key persisted to `model_discovery_candidate.category`.
    pub fn as_str(&self) -> &'static str {
        match self {
            FleetCategory::ToolRouter => "tool_router",
            FleetCategory::WriterSlm => "writer_slm",
            FleetCategory::Assistant => "assistant",
            FleetCategory::Coder => "coder",
            FleetCategory::Embedding => "embedding",
            FleetCategory::Visual => "visual",
            FleetCategory::Voice => "voice",
        }
    }

    /// Parse a persisted/queried category string. An unrecognized value is a
    /// clean [`ToolError::InvalidArgument`] naming the bad input — never a
    /// silent default to some "unknown" variant, matching DISC-01's acceptance
    /// criteria.
    pub fn from_str(s: &str) -> Result<Self, ToolError> {
        match s {
            "tool_router" => Ok(FleetCategory::ToolRouter),
            "writer_slm" => Ok(FleetCategory::WriterSlm),
            "assistant" => Ok(FleetCategory::Assistant),
            "coder" => Ok(FleetCategory::Coder),
            "embedding" => Ok(FleetCategory::Embedding),
            "visual" => Ok(FleetCategory::Visual),
            "voice" => Ok(FleetCategory::Voice),
            other => Err(ToolError::InvalidArgument(format!(
                "unrecognized fleet category '{other}' (expected one of: tool_router, \
                 writer_slm, assistant, coder, embedding, visual, voice)"
            ))),
        }
    }
}

/// A candidate's lifecycle status. `Discovered` is the entry state (DISC-06
/// found it, nothing fetched yet); `Evicted` is the only terminal-but-
/// re-enterable state (DISC-06 documents the one allowed `Evicted` →
/// `Discovered` re-entry transition when a pruned model reappears in a later
/// HF listing — enforced by DISC-03, not this module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateStatus {
    /// Found by DISC-06's discovery refresh, not yet fetched.
    Discovered,
    /// DISC-08's fetch is in flight (also the concurrency guard — see DISC-08).
    Fetching,
    /// Present in the cold archive, not yet marked for a fleet sweep.
    ColdStored,
    /// DISC-11 flipped it: sweep queued/running.
    MarkedForFleet,
    /// The fleet catalog now has a run/stale cell for this model.
    Swept,
    /// DISC-13 pruned the archive copy; `retained_profile` is populated.
    Evicted,
    /// Failed the VRAM/gfx1151 fit check — never fetched.
    Rejected,
}

impl CandidateStatus {
    /// All variants, in the spec's documented lifecycle order.
    pub const ALL: [CandidateStatus; 7] = [
        CandidateStatus::Discovered,
        CandidateStatus::Fetching,
        CandidateStatus::ColdStored,
        CandidateStatus::MarkedForFleet,
        CandidateStatus::Swept,
        CandidateStatus::Evicted,
        CandidateStatus::Rejected,
    ];

    /// The stable snake_case key persisted to `model_discovery_candidate.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateStatus::Discovered => "discovered",
            CandidateStatus::Fetching => "fetching",
            CandidateStatus::ColdStored => "cold_stored",
            CandidateStatus::MarkedForFleet => "marked_for_fleet",
            CandidateStatus::Swept => "swept",
            CandidateStatus::Evicted => "evicted",
            CandidateStatus::Rejected => "rejected",
        }
    }

    /// Parse a persisted/queried status string. An unrecognized value is a
    /// clean [`ToolError::InvalidArgument`] naming the bad input — never a
    /// silent default, matching DISC-01's acceptance criteria.
    pub fn from_str(s: &str) -> Result<Self, ToolError> {
        match s {
            "discovered" => Ok(CandidateStatus::Discovered),
            "fetching" => Ok(CandidateStatus::Fetching),
            "cold_stored" => Ok(CandidateStatus::ColdStored),
            "marked_for_fleet" => Ok(CandidateStatus::MarkedForFleet),
            "swept" => Ok(CandidateStatus::Swept),
            "evicted" => Ok(CandidateStatus::Evicted),
            "rejected" => Ok(CandidateStatus::Rejected),
            other => Err(ToolError::InvalidArgument(format!(
                "unrecognized candidate status '{other}' (expected one of: discovered, \
                 fetching, cold_stored, marked_for_fleet, swept, evicted, rejected)"
            ))),
        }
    }

    /// The statuses a candidate may legally move to FROM this one, per DISC-03's
    /// `transition_status` enforcement. Pure — unit-testable without a DB. This
    /// is a storage-schema-level DECLARATION of the state machine; DISC-03 is
    /// the only code path that actually calls it to gate a write.
    ///
    /// `Evicted` is documented as re-enterable to `Discovered` ONLY (DISC-06's
    /// one allowed re-entry transition, when a pruned model reappears in a
    /// fresh HF listing) — every other terminal-looking edge here is exactly
    /// what DISC-03's own doc comment enumerates.
    pub fn valid_transitions(&self) -> &'static [CandidateStatus] {
        match self {
            CandidateStatus::Discovered => {
                &[CandidateStatus::Fetching, CandidateStatus::Rejected]
            }
            CandidateStatus::Fetching => {
                &[CandidateStatus::ColdStored, CandidateStatus::Discovered]
            }
            CandidateStatus::ColdStored => &[CandidateStatus::MarkedForFleet],
            CandidateStatus::MarkedForFleet => &[CandidateStatus::Swept],
            CandidateStatus::Swept => &[CandidateStatus::Evicted],
            CandidateStatus::Evicted => &[CandidateStatus::Discovered],
            CandidateStatus::Rejected => &[],
        }
    }
}

/// One `model_discovery_candidate` row. Mirrors the table's columns 1:1;
/// timestamps are `None` until the corresponding lifecycle event sets them
/// (DISC-03 owns every write; this is a plain owned struct with no DB access).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryCandidate {
    /// Primary key. Matches `model_fleet_catalog.model_name` byte-for-byte per
    /// the S83 join convention `acquire.rs` documents.
    pub model_name: String,
    pub hf_repo: String,
    pub category: FleetCategory,
    pub status: CandidateStatus,
    /// `acquire.rs::Gfx1151Class::as_str()` value — kept as a plain string here
    /// (not a second copy of that enum) since this module never branches on it;
    /// DISC-05's classifier owns the enum-to-string conversion.
    pub gfx1151_class: String,
    pub size_b: Option<f64>,
    pub vram_footprint_gb: Option<f64>,
    /// Free text: which DISC-04 signal found it (e.g. `"hf_trending"`).
    pub discovery_source: String,
    /// The numeric signal DISC-05 computed (HF likes/downloads/trending, or a
    /// real leaderboard score once available — see the spec's open question 3).
    pub discovery_score: Option<f64>,
    pub discovered_at: chrono::DateTime<chrono::Utc>,
    /// Bumped every refresh a still-listed candidate is re-observed, so
    /// staleness is queryable.
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    pub marked_for_fleet_at: Option<chrono::DateTime<chrono::Utc>>,
    pub evicted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `NULL` until an eviction populates it (DISC-13, via DISC-03's
    /// `record_eviction` — the ONLY permitted write site). Invariant:
    /// populated ⟺ `status == Evicted` (see `model.rs`'s EDGE CASES doc / the
    /// migration's comment — enforced at the application layer by DISC-03,
    /// not a DB CHECK, since the existing migration style has no precedent for
    /// a cross-column CHECK and this crate's convention is to enforce such
    /// invariants at the write-API layer, matching `record_eviction`'s
    /// "the ONLY call site" design in DISC-03).
    pub retained_profile: Option<serde_json::Value>,
    /// Free text, mirrors `Nomination::rationale` — DISC-08's failure reason,
    /// DISC-05's classification rationale, etc.
    pub rationale: Option<String>,
}

/// The migration SQL, applied out-of-band by an operator (matching
/// `model_fleet_catalog`'s MINT2-07 convention — `src/intake/storage.rs` is
/// authoritative that the harness only INSERTs/SELECTs, never issues DDL). This
/// constant exists so a test can assert the SQL text is well-formed / contains
/// the expected guards without needing a live Postgres; the canonical copy that
/// an operator actually applies lives in `migrations/` (see
/// `S114-disc01-brochure.sql`), kept byte-identical to this constant.
pub const MODEL_DISCOVERY_CANDIDATE_MIGRATION_SQL: &str = include_str!(
    "../../../migrations/S114-disc01-brochure.sql"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_category_round_trips_every_variant() {
        for cat in FleetCategory::ALL {
            let s = cat.as_str();
            let parsed = FleetCategory::from_str(s).expect("round trip");
            assert_eq!(parsed, cat, "round trip failed for {s}");
        }
    }

    #[test]
    fn fleet_category_rejects_unrecognized_string() {
        let err = FleetCategory::from_str("not_a_category").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn candidate_status_round_trips_every_variant() {
        for status in CandidateStatus::ALL {
            let s = status.as_str();
            let parsed = CandidateStatus::from_str(s).expect("round trip");
            assert_eq!(parsed, status, "round trip failed for {s}");
        }
    }

    #[test]
    fn candidate_status_rejects_unrecognized_string() {
        let err = CandidateStatus::from_str("not_a_status").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn candidate_status_as_str_values_are_snake_case_and_stable() {
        // Locks the exact persisted strings (a rename here is a migration, not
        // a refactor) so a future edit doesn't accidentally reshuffle them.
        assert_eq!(CandidateStatus::Discovered.as_str(), "discovered");
        assert_eq!(CandidateStatus::Fetching.as_str(), "fetching");
        assert_eq!(CandidateStatus::ColdStored.as_str(), "cold_stored");
        assert_eq!(CandidateStatus::MarkedForFleet.as_str(), "marked_for_fleet");
        assert_eq!(CandidateStatus::Swept.as_str(), "swept");
        assert_eq!(CandidateStatus::Evicted.as_str(), "evicted");
        assert_eq!(CandidateStatus::Rejected.as_str(), "rejected");
    }

    #[test]
    fn evicted_re_enters_only_to_discovered() {
        assert_eq!(
            CandidateStatus::Evicted.valid_transitions(),
            &[CandidateStatus::Discovered]
        );
    }

    #[test]
    fn rejected_is_terminal() {
        assert!(CandidateStatus::Rejected.valid_transitions().is_empty());
    }

    #[test]
    fn migration_sql_creates_the_table_with_unique_model_name() {
        let sql = MODEL_DISCOVERY_CANDIDATE_MIGRATION_SQL;
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS model_discovery_candidate"));
        assert!(sql.contains("PRIMARY KEY (model_name)"));
        assert!(sql.contains("idx_discovery_candidate_status"));
        assert!(sql.contains("idx_discovery_candidate_category"));
        assert!(sql.contains("idx_discovery_candidate_last_seen"));
    }
}
