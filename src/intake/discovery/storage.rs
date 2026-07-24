//! DISC-02 (S114, TERM #252): the brochure's ONE Postgres read touch point.
//!
//! Mirrors [`crate::intake::storage::read_fleet_catalog`]'s shape exactly:
//! a single `SELECT * FROM model_discovery_candidate` decoded into owned
//! [`DiscoveryCandidate`] rows, tolerant of an un-migrated host (a missing
//! `model_discovery_candidate` relation is a clean [`ToolError::NotConfigured`],
//! never a crash and never a masked empty result — the caller needs to know
//! the difference between "no candidates yet" and "brochure not configured").
//! Any other DB error propagates as [`ToolError::Database`].
//!
//! This module reuses the ONE shared pool helper
//! ([`crate::intake::storage::get_pool`]) — `tool.rs` calls that directly; this
//! module never opens its own pool, per DISC-02's "do not open a second pool"
//! requirement.
//!
//! No secrets are read here (the pool's connection string is resolved by the
//! shared `storage::get_pool()`, which is out of scope for this item) — this
//! item's TEST PLAN "secrets via SecretManager" line is N/A, noted explicitly
//! per the spec's requirement to state the exemption rather than omit it.

use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::discovery::schema::{
    CandidateStatus, DiscoveryCandidate, FleetCategory, Modality,
};

/// True when a Postgres error text indicates a MISSING RELATION (the
/// `model_discovery_candidate` table does not exist — an un-migrated host),
/// so the read path can degrade to a clean [`ToolError::NotConfigured`]
/// rather than propagating a raw SQL error. Postgres reports
/// `error: relation "model_discovery_candidate" does not exist` (SQLSTATE
/// 42P01). Pure over its input; a local copy of
/// `crate::intake::storage::is_missing_relation_error` (private to that
/// module) rather than a cross-module reach, matching this crate's existing
/// convention of small, self-contained storage modules.
fn is_missing_relation_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("relation") && m.contains("does not exist")
}

/// The full SELECT — every `model_discovery_candidate` column, in
/// [`DiscoveryCandidate`] field order.
const READ_BROCHURE_SQL: &str = "SELECT model_name, hf_repo, category, status, gfx1151_class, \
     size_b, vram_footprint_gb, discovery_source, discovery_score, discovered_at, last_seen_at, \
     fetched_at, marked_for_fleet_at, evicted_at, retained_profile, rationale, modality \
     FROM model_discovery_candidate ORDER BY model_name";

/// Row shape the brochure SELECT decodes into, before `category`/`status` are
/// parsed into their Rust enums.
type BrochureRowTuple = (
    String,                                // model_name
    String,                                // hf_repo
    String,                                // category
    String,                                // status
    String,                                // gfx1151_class
    Option<f64>,                           // size_b
    Option<f64>,                           // vram_footprint_gb
    String,                                // discovery_source
    Option<f64>,                           // discovery_score
    chrono::DateTime<chrono::Utc>,         // discovered_at
    chrono::DateTime<chrono::Utc>,         // last_seen_at
    Option<chrono::DateTime<chrono::Utc>>, // fetched_at
    Option<chrono::DateTime<chrono::Utc>>, // marked_for_fleet_at
    Option<chrono::DateTime<chrono::Utc>>, // evicted_at
    Option<serde_json::Value>,             // retained_profile
    Option<String>,                        // rationale
    Option<String>,                        // modality (CB-02; NULL = unclassified)
);

/// Read every persisted brochure row. NEVER recomputes/filters — that's
/// `tool.rs`'s pure [`crate::intake::discovery::tool::filter_candidates`]
/// layer's job over this function's output.
///
/// An un-migrated host (the `model_discovery_candidate` table absent) is a
/// clean [`ToolError::NotConfigured`]. A row whose persisted `category` or
/// `status` string fails to parse back into its enum is a
/// [`ToolError::Database`] naming the offending row — this should never
/// happen given DISC-03 is the only write path and always writes
/// `as_str()`-derived values, but a read-side parse failure must surface
/// loudly rather than silently drop/default a row.
pub async fn read_brochure(pool: &PgPool) -> Result<Vec<DiscoveryCandidate>, ToolError> {
    let rows = match sqlx::query_as::<_, BrochureRowTuple>(READ_BROCHURE_SQL)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            let msg = e.to_string();
            if is_missing_relation_error(&msg) {
                return Err(ToolError::NotConfigured(
                    "the model discovery brochure is not configured on this host \
                     (model_discovery_candidate table absent — run the DISC-01 migration)"
                        .into(),
                ));
            }
            return Err(ToolError::Database(format!(
                "Failed to read model_discovery_candidate: {msg}"
            )));
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for (
        model_name,
        hf_repo,
        category,
        status,
        gfx1151_class,
        size_b,
        vram_footprint_gb,
        discovery_source,
        discovery_score,
        discovered_at,
        last_seen_at,
        fetched_at,
        marked_for_fleet_at,
        evicted_at,
        retained_profile,
        rationale,
        modality,
    ) in rows
    {
        let category = FleetCategory::from_str(&category).map_err(|e| {
            ToolError::Database(format!(
                "model_discovery_candidate row '{model_name}' has an unparseable category \
                 '{category}': {e}"
            ))
        })?;
        let status = CandidateStatus::from_str(&status).map_err(|e| {
            ToolError::Database(format!(
                "model_discovery_candidate row '{model_name}' has an unparseable status \
                 '{status}': {e}"
            ))
        })?;
        // `modality` is NULLABLE (CB-02): a NULL column is an unclassified
        // candidate (`None`), NOT an error. A NON-NULL but unparseable value is
        // surfaced loudly — same "never silently drop/default a row" contract as
        // category/status above (DISC-03 only ever writes `Modality::as_str()`
        // values, so this should be unreachable in practice).
        let modality = match modality {
            None => None,
            Some(s) => Some(Modality::from_str(&s).map_err(|e| {
                ToolError::Database(format!(
                    "model_discovery_candidate row '{model_name}' has an unparseable modality \
                     '{s}': {e}"
                ))
            })?),
        };
        out.push(DiscoveryCandidate {
            model_name,
            hf_repo,
            category,
            status,
            gfx1151_class,
            size_b,
            vram_footprint_gb,
            discovery_source,
            discovery_score,
            discovered_at,
            last_seen_at,
            fetched_at,
            marked_for_fleet_at,
            evicted_at,
            retained_profile,
            rationale,
            modality,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_missing_relation_error_matches_only_missing_relation() {
        assert!(is_missing_relation_error(
            "error returned from database: relation \"model_discovery_candidate\" does not exist"
        ));
        assert!(!is_missing_relation_error("connection refused"));
        assert!(!is_missing_relation_error("column \"foo\" does not exist"));
    }
}
