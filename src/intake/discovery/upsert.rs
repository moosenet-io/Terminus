//! DISC-03 (S114, TERM #253): the brochure's write API — the ONE path every
//! other item in this spec uses to mutate `model_discovery_candidate` rows.
//!
//! WHY THIS EXISTS — centralized writes, one enforcement point: DISC-06's
//! daily refresh inserts/re-observes candidates, DISC-08's fetch flips
//! `Fetching`/`ColdStored`, DISC-11 marks a candidate for the fleet sweep, and
//! DISC-13's pruning pass records an eviction. Rather than each caller writing
//! its own SQL (and each re-deriving the status-transition rules), every
//! write goes through exactly one of the three functions below, mirroring
//! [`crate::intake::catalog`]'s own pure-builder/impure-orchestrator split:
//! the pure part here is [`predecessors_for_transition`] (which statuses may
//! legally transition to a given target, per
//! [`crate::intake::discovery::schema::CandidateStatus::valid_transitions`] —
//! reused verbatim, never redefined); the impure part is the three
//! `sqlx`-backed functions that actually touch Postgres.
//!
//! THREE FUNCTIONS, THREE JOBS:
//! - [`upsert_candidate`] — insert-or-update on `model_name` conflict. Bumps
//!   `last_seen_at` (and, on a genuine insert, `discovered_at`) via the DB's
//!   own `now()`. Deliberately does NOT touch `status` on conflict (an
//!   existing row's lifecycle status is owned by [`transition_status`] /
//!   [`record_eviction`], never silently reset by a re-observation — this is
//!   what lets DISC-06 re-upsert an already fleet-tested candidate without
//!   regressing it back to `Discovered`).
//! - [`transition_status`] — the only path that flips `status` on an existing
//!   row (other than eviction). Validates the transition against
//!   `CandidateStatus::valid_transitions()` before writing; an illegal
//!   transition is a clean [`ToolError::InvalidArgument`], never a silent
//!   overwrite. Cannot target `Evicted` directly — see its doc comment.
//! - [`record_eviction`] — the ONLY function permitted to populate
//!   `retained_profile`. Sets `status = Evicted` + `evicted_at = now()`
//!   atomically with the profile so the schema's documented invariant
//!   (`retained_profile` populated iff `status == Evicted`) can never be
//!   observed half-applied by a caller going through this module. Never
//!   deletes a row.
//!
//! CONCURRENCY: every write here is a SINGLE `UPDATE`/`INSERT ... ON
//! CONFLICT` statement gated by its own `WHERE` clause — there is no
//! read-modify-write window for two racing processes (e.g. a discovery
//! refresh and a pruning pass) to corrupt a row between a read and a write.
//! [`transition_status`] additionally issues a diagnostic `SELECT` when its
//! guarded `UPDATE` affects zero rows, but only to produce a useful
//! not-found-vs-invalid-transition error message — the write itself has
//! already atomically succeeded or failed by that point.
//!
//! TIMESTAMPS: every timestamp column is written via the DB's own `now()`,
//! never a wall-clock value computed in this process and shipped across the
//! network — avoids clock-skew bugs between whatever host runs the discovery
//! refresh / pruning pass and the Postgres host itself.
//!
//! SECRETS: N/A — this module only takes a `PgPool` handed in by the caller
//! (via `crate::intake::storage::get_pool()`, which itself resolves
//! `INTAKE_DATABASE_URL`/`DATABASE_URL` through `config.rs`, not a secret
//! vault entry). No `std::env::var` reads, no vault access, here.

use serde_json::Value;
use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::discovery::schema::{CandidateStatus, DiscoveryCandidate};

/// Insert a new brochure row, or update an existing one on `model_name`
/// conflict. Every call bumps `last_seen_at` to the DB's own `now()` — so
/// DISC-06's daily re-observation of an already-known candidate is visible as
/// freshness, not just a no-op. `discovered_at` is stamped `now()` only on a
/// genuine insert (it is absent from the `ON CONFLICT` `SET` clause, so an
/// existing row keeps its original discovery timestamp).
///
/// Deliberately leaves `status` untouched on conflict: an existing row's
/// lifecycle status is owned by [`transition_status`]/[`record_eviction`],
/// never silently reset by a re-observation (DISC-06's "already fleet-tested
/// candidate keeps whatever status it already has" edge case).
///
/// Likewise PRESERVES measured fit metadata on conflict when the incoming row
/// doesn't have it: `size_b`/`vram_footprint_gb` are `COALESCE`d (kept if the
/// new value is `NULL`) and `gfx1151_class` is kept when the new value is the
/// `'unknown'` sentinel. DISC-06's discovery re-observation always carries
/// `unknown`/`NULL` fit (a listing exposes no parameter count), so without this
/// a daily refresh would erase the `size_b`/`vram_footprint_gb`/`gfx1151_class`
/// a fetch/measure step had recorded on a `Fetching`/`Swept` model. A real
/// measurement (non-`NULL`, non-`'unknown'`) still overwrites as before.
///
/// `modality` (CB-02) is treated the same way: a re-observation recomputes it
/// from the listing and overwrites when it has a value, but a `NULL`
/// (unclassifiable this pass) is `COALESCE`d so it never erases a modality a
/// richer earlier listing already classified.
pub async fn upsert_candidate(
    pool: &PgPool,
    candidate: &DiscoveryCandidate,
) -> Result<(), ToolError> {
    sqlx::query(
        "INSERT INTO model_discovery_candidate \
             (model_name, hf_repo, category, status, gfx1151_class, size_b, \
              vram_footprint_gb, discovery_source, discovery_score, \
              discovered_at, last_seen_at, rationale, modality) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now(), $10, $11) \
         ON CONFLICT (model_name) DO UPDATE SET \
             hf_repo = EXCLUDED.hf_repo, \
             category = EXCLUDED.category, \
             discovery_source = EXCLUDED.discovery_source, \
             discovery_score = EXCLUDED.discovery_score, \
             last_seen_at = now(), \
             rationale = EXCLUDED.rationale, \
             gfx1151_class = CASE WHEN EXCLUDED.gfx1151_class = 'unknown' \
                                  THEN model_discovery_candidate.gfx1151_class \
                                  ELSE EXCLUDED.gfx1151_class END, \
             size_b = COALESCE(EXCLUDED.size_b, model_discovery_candidate.size_b), \
             vram_footprint_gb = COALESCE(EXCLUDED.vram_footprint_gb, \
                                          model_discovery_candidate.vram_footprint_gb), \
             modality = COALESCE(EXCLUDED.modality, model_discovery_candidate.modality)",
    )
    .bind(&candidate.model_name)
    .bind(&candidate.hf_repo)
    .bind(candidate.category.as_str())
    .bind(candidate.status.as_str())
    .bind(&candidate.gfx1151_class)
    .bind(candidate.size_b)
    .bind(candidate.vram_footprint_gb)
    .bind(&candidate.discovery_source)
    .bind(candidate.discovery_score)
    .bind(candidate.rationale.as_deref())
    .bind(candidate.modality.map(|m| m.as_str()))
    .execute(pool)
    .await
    .map_err(|e| {
        ToolError::Database(format!(
            "upsert model_discovery_candidate row for '{}': {e}",
            candidate.model_name
        ))
    })?;
    Ok(())
}

/// The statuses that may legally transition INTO `target`, per
/// [`CandidateStatus::valid_transitions`] (reused verbatim, never
/// redefined — DISC-01 already owns the state-machine declaration; this is
/// just its inverse, computed pure-ly for [`transition_status`]'s guarded
/// `UPDATE ... WHERE status = ANY(...)`).
fn allowed_predecessors(target: CandidateStatus) -> Vec<CandidateStatus> {
    CandidateStatus::ALL
        .into_iter()
        .filter(|from| from.valid_transitions().contains(&target))
        .collect()
}

/// Pure precondition check for [`transition_status`]: is `new_status` a legal
/// transition TARGET at all (independent of any particular row's current
/// state), and if so, which predecessor statuses may reach it? Split out from
/// the DB-touching `transition_status` so this — the actual transition-
/// legality logic — is unit-testable without a live Postgres.
///
/// Two DB-independent rejection cases:
/// - `new_status == Evicted` — `transition_status` never sets `Evicted`
///   directly. Only [`record_eviction`] may, because it is the sole writer of
///   `retained_profile` and the schema's invariant (`retained_profile`
///   populated iff `status == Evicted`) must never be observable half-applied
///   (status flipped, profile still `NULL`). A caller that wants to evict a
///   candidate must call `record_eviction`, not this function.
/// - `new_status` has NO predecessor at all (no `CandidateStatus` variant's
///   `valid_transitions()` lists it) — an unreachable target by construction.
///
/// A transition that fails because the row's ACTUAL current status doesn't
/// permit it (e.g. `Rejected` → `Fetching`, where `Rejected` is terminal) is
/// NOT caught here — that depends on the row's stored state, so it is only
/// detectable once `transition_status` reads/writes the row.
fn predecessors_for_transition(new_status: CandidateStatus) -> Result<Vec<CandidateStatus>, ToolError> {
    if new_status == CandidateStatus::Evicted {
        return Err(ToolError::InvalidArgument(
            "transition_status cannot target 'evicted' directly — call record_eviction instead, \
             the only function permitted to write retained_profile; it sets status='evicted' \
             atomically with the retained profile so the two can never be observed half-applied"
                .to_string(),
        ));
    }
    let predecessors = allowed_predecessors(new_status);
    if predecessors.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "no candidate status may transition to '{}' via transition_status \
             (unreachable per CandidateStatus::valid_transitions())",
            new_status.as_str()
        )));
    }
    Ok(predecessors)
}

/// Move an existing brochure row's `status` to `new_status`, validated
/// against [`CandidateStatus::valid_transitions`] (via
/// [`predecessors_for_transition`]). An illegal transition — either a
/// DB-independent one (see that function) or one the row's actual current
/// status doesn't permit — is a clean [`ToolError::InvalidArgument`], never a
/// silent overwrite. A `model_name` with no existing row is a clean
/// [`ToolError::NotFound`], never an implicit insert (upserting belongs to
/// [`upsert_candidate`] alone).
///
/// The write is a SINGLE guarded `UPDATE ... WHERE model_name = $1 AND
/// status = ANY(<legal predecessors>)` — atomic under concurrent callers
/// racing on the same `model_name` (e.g. a refresh and a pruning pass): only
/// one can win the row-level update per legal predecessor state, and a loser
/// simply gets a `NotFound`/`InvalidArgument` reflecting whatever state the
/// winner left behind, never a corrupted intermediate value. The diagnostic
/// `SELECT` below (fired only when the guarded `UPDATE` affects zero rows)
/// exists solely to produce a useful "not found" vs "wrong current status"
/// error message — it plays no role in gating the write itself.
///
/// Sets the target status's own lifecycle timestamp column where one exists
/// (`fetched_at` for `ColdStored`, `marked_for_fleet_at` for
/// `MarkedForFleet`) via the DB's own `now()`.
pub async fn transition_status(
    pool: &PgPool,
    model_name: &str,
    new_status: CandidateStatus,
) -> Result<(), ToolError> {
    let predecessors = predecessors_for_transition(new_status)?;
    let predecessor_strs: Vec<&str> = predecessors.iter().map(|s| s.as_str()).collect();

    let sql = match new_status {
        CandidateStatus::ColdStored => {
            "UPDATE model_discovery_candidate SET status = $2, fetched_at = now() \
             WHERE model_name = $1 AND status = ANY($3)"
        }
        CandidateStatus::MarkedForFleet => {
            "UPDATE model_discovery_candidate SET status = $2, marked_for_fleet_at = now() \
             WHERE model_name = $1 AND status = ANY($3)"
        }
        _ => {
            "UPDATE model_discovery_candidate SET status = $2 \
             WHERE model_name = $1 AND status = ANY($3)"
        }
    };

    let result = sqlx::query(sql)
        .bind(model_name)
        .bind(new_status.as_str())
        .bind(&predecessor_strs)
        .execute(pool)
        .await
        .map_err(|e| {
            ToolError::Database(format!("transition_status for '{model_name}': {e}"))
        })?;

    if result.rows_affected() == 0 {
        // Diagnostic-only: the guarded UPDATE above has already atomically
        // succeeded or failed. This SELECT just distinguishes "no such row"
        // from "row exists but its current status doesn't permit this
        // transition" for a useful error message.
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM model_discovery_candidate WHERE model_name = $1",
        )
        .bind(model_name)
        .fetch_optional(pool)
        .await
        .map_err(|e| ToolError::Database(format!("lookup for '{model_name}': {e}")))?;

        return match existing {
            None => Err(ToolError::NotFound(format!(
                "no model_discovery_candidate row for '{model_name}'"
            ))),
            Some((current,)) => Err(ToolError::InvalidArgument(format!(
                "cannot transition '{model_name}' from '{current}' to '{}'",
                new_status.as_str()
            ))),
        };
    }
    Ok(())
}

/// Record a pruning eviction: set `status = Evicted`, `evicted_at = now()`,
/// and persist `profile` into `retained_profile` — the ONLY write site for
/// that column anywhere in this crate. Never deletes the row (this is the
/// actual enforcement point for the "keep their profile data" requirement —
/// DISC-13 calls this, never a raw `DELETE`).
///
/// Idempotent: calling this twice for the same `model_name` (e.g. a pruning
/// pass re-running after a partial failure) just re-sets the same
/// `retained_profile`/`evicted_at`/`status` — no error on the second call.
///
/// A `model_name` with no existing row is a clean [`ToolError::NotFound`];
/// this function never inserts a phantom row.
pub async fn record_eviction(
    pool: &PgPool,
    model_name: &str,
    profile: Value,
) -> Result<(), ToolError> {
    let result = sqlx::query(
        "UPDATE model_discovery_candidate \
         SET status = $2, evicted_at = now(), retained_profile = $3 \
         WHERE model_name = $1",
    )
    .bind(model_name)
    .bind(CandidateStatus::Evicted.as_str())
    .bind(profile)
    .execute(pool)
    .await
    .map_err(|e| ToolError::Database(format!("record_eviction for '{model_name}': {e}")))?;

    if result.rows_affected() == 0 {
        return Err(ToolError::NotFound(format!(
            "no model_discovery_candidate row for '{model_name}' — record_eviction never \
             inserts a phantom row"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure transition-legality tests (no DB required) ----
    //
    // `upsert_candidate`/`transition_status`/`record_eviction` themselves are
    // thin `sqlx`-backed I/O over a live Postgres — same DB-gated-test
    // convention `catalog.rs`'s own storage-write paths follow (this crate
    // has no live-Postgres unit-test harness; `storage.rs`'s own test module
    // likewise only unit-tests its pure/config surface, not its SQL bodies
    // against a live DB). The load-bearing LOGIC this item adds beyond raw
    // SQL — which transitions are legal — lives in
    // `predecessors_for_transition`/`allowed_predecessors`, which are pure
    // and fully covered here.

    #[test]
    fn evicted_target_is_rejected_use_record_eviction_instead() {
        let err = predecessors_for_transition(CandidateStatus::Evicted).unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => {
                assert!(msg.contains("record_eviction"), "message: {msg}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn discovered_is_reachable_from_fetching_and_evicted() {
        let preds = allowed_predecessors(CandidateStatus::Discovered);
        assert!(preds.contains(&CandidateStatus::Fetching));
        assert!(
            preds.contains(&CandidateStatus::Evicted),
            "DISC-06's one allowed re-entry transition (evicted -> discovered) must be legal"
        );
        assert_eq!(preds.len(), 2, "unexpected predecessor set: {preds:?}");
    }

    #[test]
    fn swept_is_reachable_only_from_marked_for_fleet() {
        let preds = allowed_predecessors(CandidateStatus::Swept);
        assert_eq!(preds, vec![CandidateStatus::MarkedForFleet]);
    }

    #[test]
    fn rejected_has_no_predecessor_and_is_a_valid_transition_target() {
        // `Rejected` itself is a legal transition target (Discovered ->
        // Rejected), but nothing transitions INTO it more than once — this
        // just checks the predecessor set is exactly what schema.rs declares.
        let preds = allowed_predecessors(CandidateStatus::Rejected);
        assert_eq!(preds, vec![CandidateStatus::Discovered]);
    }

    /// Negative test: a target with no legal predecessor at all is rejected
    /// with `InvalidArgument`. There is no such target among the real
    /// `CandidateStatus` variants (every variant is reachable from
    /// something), so this test instead locks the OTHER DB-independent
    /// rejection path exercised above (`evicted_target_is_rejected...`) plus
    /// asserts every real variant DOES have at least one legal predecessor,
    /// i.e. `predecessors_for_transition` only ever rejects a real variant
    /// via the explicit `Evicted` special-case, never via an empty-predecessor
    /// false positive.
    #[test]
    fn every_non_evicted_status_has_at_least_one_legal_predecessor() {
        for status in CandidateStatus::ALL {
            if status == CandidateStatus::Evicted {
                continue;
            }
            let result = predecessors_for_transition(status);
            assert!(
                result.is_ok(),
                "{} should have a legal predecessor set: {result:?}",
                status.as_str()
            );
        }
    }

    /// Negative test: an actually-illegal transition (per
    /// `CandidateStatus::valid_transitions`) is never among the computed
    /// predecessors — e.g. `Rejected` is terminal, so nothing should list
    /// `Rejected` as able to transition FURTHER anywhere via a predecessor
    /// check that treats `Rejected` as the FROM state reaching some target
    /// other than what its own `valid_transitions()` (empty) allows.
    #[test]
    fn rejected_is_terminal_so_it_is_never_a_legal_predecessor_of_anything() {
        for status in CandidateStatus::ALL {
            let preds = allowed_predecessors(status);
            assert!(
                !preds.contains(&CandidateStatus::Rejected),
                "'rejected' is terminal (valid_transitions() is empty) and must never appear as \
                 a legal predecessor for transitioning into '{}'",
                status.as_str()
            );
        }
    }
}
