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

// The S128 `fit_score` missing-COLUMN classifier for the read fallback (a host
// that HAS the brochure table but not the S128 column — i.e. the code was
// deployed before the operator applied `S128-fitscore-persist.sql`). Delegates
// to the SHARED, SQLSTATE-`42703`-based classifier the WRITE path uses, so the
// two paths can never drift and the read fallback is NOT decided from message
// text alone (it classifies by the database error CODE, with `fit_score` as a
// secondary guard).
use crate::intake::discovery::upsert::is_missing_fit_score_column;

/// The full SELECT — every `model_discovery_candidate` column, in
/// [`DiscoveryCandidate`] field order, INCLUDING the S128 `fit_score`.
const READ_BROCHURE_SQL: &str = "SELECT model_name, hf_repo, category, status, gfx1151_class, \
     size_b, vram_footprint_gb, discovery_source, discovery_score, discovered_at, last_seen_at, \
     fetched_at, marked_for_fleet_at, evicted_at, retained_profile, rationale, modality, \
     published_at, updated_at, license, arch, is_instruct, gated, quant_dtype, has_gguf, fit_score \
     FROM model_discovery_candidate ORDER BY model_name";

/// LEGACY SELECT (pre-S128 migration) — identical to [`READ_BROCHURE_SQL`] but
/// WITHOUT the `fit_score` column, used as the fail-soft fallback when the column
/// doesn't exist yet. [`BrochureRow`]'s `FromRow` decodes `fit_score` via a
/// tolerant `try_get(...).unwrap_or(None)`, so a row from THIS query simply
/// carries `fit_score = None`.
const READ_BROCHURE_SQL_LEGACY: &str = "SELECT model_name, hf_repo, category, status, gfx1151_class, \
     size_b, vram_footprint_gb, discovery_source, discovery_score, discovered_at, last_seen_at, \
     fetched_at, marked_for_fleet_at, evicted_at, retained_profile, rationale, modality, \
     published_at, updated_at, license, arch, is_instruct, gated, quant_dtype, has_gguf \
     FROM model_discovery_candidate ORDER BY model_name";

/// Row shape the brochure SELECT decodes into, before `category`/`status` are
/// parsed into their Rust enums. A named struct with a MANUAL
/// [`sqlx::FromRow`] impl (by column name) rather than a tuple: with CB-02's
/// `modality` column the brochure now has 17 columns, and sqlx only implements
/// `FromRow` for tuples up to arity 16 — a 17-tuple does not decode. The `sqlx`
/// pin in this crate is built WITHOUT the `macros`/`derive` feature (see
/// `Cargo.toml`), so `#[derive(sqlx::FromRow)]` is unavailable; the impl is
/// hand-written via `Row::try_get`, matching the manual-decode pattern used
/// elsewhere (e.g. `scribe::graph::rules_store`). Field names match the
/// `READ_BROCHURE_SQL` column list one-for-one.
struct BrochureRow {
    model_name: String,
    hf_repo: String,
    category: String,
    status: String,
    gfx1151_class: String,
    size_b: Option<f64>,
    vram_footprint_gb: Option<f64>,
    discovery_source: String,
    discovery_score: Option<f64>,
    discovered_at: chrono::DateTime<chrono::Utc>,
    last_seen_at: chrono::DateTime<chrono::Utc>,
    fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    marked_for_fleet_at: Option<chrono::DateTime<chrono::Utc>>,
    evicted_at: Option<chrono::DateTime<chrono::Utc>>,
    retained_profile: Option<serde_json::Value>,
    rationale: Option<String>,
    /// CB-02; NULL = unclassified.
    modality: Option<String>,
    // Ask-4 practical-ranking metadata (S127); NULL = not yet enriched.
    published_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    license: Option<String>,
    arch: Option<String>,
    is_instruct: Option<bool>,
    gated: Option<bool>,
    quant_dtype: Option<String>,
    /// S127b GGUF-availability; NULL = not yet measured.
    has_gguf: Option<bool>,
    /// S128 persisted blended practical fit score; NULL = not yet scored. Decoded
    /// tolerantly (see `FromRow`) so a legacy (pre-S128) row missing the column
    /// simply reads as `None`.
    fit_score: Option<f64>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for BrochureRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(BrochureRow {
            model_name: row.try_get("model_name")?,
            hf_repo: row.try_get("hf_repo")?,
            category: row.try_get("category")?,
            status: row.try_get("status")?,
            gfx1151_class: row.try_get("gfx1151_class")?,
            size_b: row.try_get("size_b")?,
            vram_footprint_gb: row.try_get("vram_footprint_gb")?,
            discovery_source: row.try_get("discovery_source")?,
            discovery_score: row.try_get("discovery_score")?,
            discovered_at: row.try_get("discovered_at")?,
            last_seen_at: row.try_get("last_seen_at")?,
            fetched_at: row.try_get("fetched_at")?,
            marked_for_fleet_at: row.try_get("marked_for_fleet_at")?,
            evicted_at: row.try_get("evicted_at")?,
            retained_profile: row.try_get("retained_profile")?,
            rationale: row.try_get("rationale")?,
            modality: row.try_get("modality")?,
            published_at: row.try_get("published_at")?,
            updated_at: row.try_get("updated_at")?,
            license: row.try_get("license")?,
            arch: row.try_get("arch")?,
            is_instruct: row.try_get("is_instruct")?,
            gated: row.try_get("gated")?,
            quant_dtype: row.try_get("quant_dtype")?,
            has_gguf: row.try_get("has_gguf")?,
            // Tolerant: the legacy (pre-S128) SELECT omits this column entirely,
            // so `try_get` returns a ColumnNotFound error — treat that (and a SQL
            // NULL) alike as `None`. Never propagates as a decode failure.
            fit_score: row.try_get("fit_score").unwrap_or(None),
        })
    }
}

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
    let rows = match sqlx::query_as::<_, BrochureRow>(READ_BROCHURE_SQL)
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
            // Fail-soft (forward/backward compatible): the code may be deployed
            // before the operator applies the S128 migration, so the table exists
            // but the `fit_score` column doesn't. Classify by the DB error CODE
            // (SQLSTATE 42703, via the shared write-path classifier — never by
            // message text alone), then retry with the legacy SELECT that omits
            // it; those rows decode with `fit_score = None`.
            if is_missing_fit_score_column(&e) {
                match sqlx::query_as::<_, BrochureRow>(READ_BROCHURE_SQL_LEGACY)
                    .fetch_all(pool)
                    .await
                {
                    Ok(rows) => rows,
                    Err(e2) => {
                        return Err(ToolError::Database(format!(
                            "Failed to read model_discovery_candidate (fit_score fallback): {e2}"
                        )));
                    }
                }
            } else {
                return Err(ToolError::Database(format!(
                    "Failed to read model_discovery_candidate: {msg}"
                )));
            }
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let BrochureRow {
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
            published_at,
            updated_at,
            license,
            arch,
            is_instruct,
            gated,
            quant_dtype,
            has_gguf,
            fit_score,
        } = row;
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
            published_at,
            updated_at,
            license,
            arch,
            is_instruct,
            gated,
            quant_dtype,
            has_gguf,
            fit_score,
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

    #[test]
    fn read_fallback_classifier_is_not_decided_from_message_text_alone() {
        // The gpt56 finding: the read fallback must NOT trigger purely because an
        // error's TEXT contains the words — it must classify by the DB error CODE
        // (SQLSTATE 42703). A non-database error whose message literally reads
        // like a missing-fit_score-column error must be REJECTED, because it
        // carries no 42703 code. (The live 42703 positive path is exercised
        // against a real Postgres — no public constructor for a PgDatabaseError —
        // same DB-gated convention as the rest of this module's SQL bodies.)
        let text_lookalike = sqlx::Error::Protocol(
            "column \"fit_score\" of relation \"model_discovery_candidate\" does not exist"
                .to_string(),
        );
        assert!(
            !is_missing_fit_score_column(&text_lookalike),
            "a non-DB error with matching text must NOT trigger the fallback (no 42703 code)"
        );
        // A DB-less sentinel error likewise never triggers it.
        assert!(!is_missing_fit_score_column(&sqlx::Error::RowNotFound));
    }

    #[test]
    fn legacy_select_omits_fit_score_but_full_select_includes_it() {
        assert!(READ_BROCHURE_SQL.contains("fit_score"));
        assert!(!READ_BROCHURE_SQL_LEGACY.contains("fit_score"));
        // The legacy SELECT is otherwise the full column list up through has_gguf.
        assert!(READ_BROCHURE_SQL_LEGACY.contains("has_gguf"));
    }
}
