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

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use sqlx::PgPool;

use crate::error::ToolError;
use crate::intake::discovery::schema::{CandidateStatus, DiscoveryCandidate};

/// Latched once the first fail-soft "the `fit_score` column doesn't exist yet"
/// downgrade is logged, so a pre-migration deploy logs the degrade EXACTLY ONCE
/// per process rather than once per upserted candidate (an enrich pass upserts
/// many rows). Reset is intentionally impossible for a process lifetime.
static FIT_SCORE_MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

/// The shared column/VALUES/ON-CONFLICT-SET body every upsert uses, minus the
/// S128 `fit_score` column. `{FIT_COLS}` / `{FIT_VALS}` / `{FIT_SET}` are spliced
/// in (or left empty) by [`upsert_sql`] so the WITH-fit and LEGACY (pre-S128-
/// migration) statements are generated from ONE source of truth — the 19 shared
/// binds are identical between them, only the trailing `fit_score` bind differs.
const UPSERT_SQL_TEMPLATE: &str =
    "INSERT INTO model_discovery_candidate \
         (model_name, hf_repo, category, status, gfx1151_class, size_b, \
          vram_footprint_gb, discovery_source, discovery_score, \
          discovered_at, last_seen_at, rationale, modality, \
          published_at, updated_at, license, arch, is_instruct, gated, quant_dtype, \
          has_gguf{FIT_COLS}) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now(), $10, $11, \
             $12, $13, $14, $15, $16, $17, $18, $19{FIT_VALS}) \
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
         modality = COALESCE(EXCLUDED.modality, model_discovery_candidate.modality), \
         published_at = COALESCE(EXCLUDED.published_at, model_discovery_candidate.published_at), \
         updated_at = COALESCE(EXCLUDED.updated_at, model_discovery_candidate.updated_at), \
         license = COALESCE(EXCLUDED.license, model_discovery_candidate.license), \
         arch = COALESCE(EXCLUDED.arch, model_discovery_candidate.arch), \
         is_instruct = COALESCE(EXCLUDED.is_instruct, model_discovery_candidate.is_instruct), \
         gated = COALESCE(EXCLUDED.gated, model_discovery_candidate.gated), \
         quant_dtype = COALESCE(EXCLUDED.quant_dtype, model_discovery_candidate.quant_dtype), \
         has_gguf = COALESCE(EXCLUDED.has_gguf, model_discovery_candidate.has_gguf){FIT_SET}";

/// Render the upsert SQL, including the S128 `fit_score` column iff `with_fit`.
/// When included, `fit_score` is `COALESCE`-protected identically to the other
/// enrich-only columns (a MEASURE pass writes a real value; a bare listing
/// re-observation carries `NULL` and must never erase a computed score). The
/// LEGACY (no-fit) rendering is a byte-for-byte no-op of the new field, used as
/// the fail-soft fallback when the column doesn't exist yet (pre-migration).
fn upsert_sql(with_fit: bool) -> String {
    if with_fit {
        UPSERT_SQL_TEMPLATE
            .replace("{FIT_COLS}", ", fit_score")
            .replace("{FIT_VALS}", ", $20")
            .replace(
                "{FIT_SET}",
                ", fit_score = COALESCE(EXCLUDED.fit_score, model_discovery_candidate.fit_score)",
            )
    } else {
        UPSERT_SQL_TEMPLATE
            .replace("{FIT_COLS}", "")
            .replace("{FIT_VALS}", "")
            .replace("{FIT_SET}", "")
    }
}

/// True when a Postgres error indicates the `fit_score` COLUMN does not exist
/// (an un-migrated host that has the table but not the S128 column). Postgres
/// reports SQLSTATE `42703` (`undefined_column`), e.g.
/// `column "fit_score" of relation "model_discovery_candidate" does not exist`.
/// Checked via the SQLSTATE code (robust to message wording) AND a `fit_score`
/// mention, so this only ever swallows the specific new-column case and never
/// masks an unrelated undefined-column bug. Pure over its input.
fn is_missing_fit_score_column(e: &sqlx::Error) -> bool {
    let is_undefined_column = e
        .as_database_error()
        .and_then(|d| d.code())
        .map(|c| c == "42703")
        .unwrap_or(false);
    is_undefined_column && e.to_string().to_lowercase().contains("fit_score")
}

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
///
/// The Ask-4 practical-ranking columns (`published_at`/`updated_at`/`license`/
/// `arch`/`is_instruct`/`gated`/`quant_dtype`, S127) are all `COALESCE`-
/// protected identically: a MEASURE/ENRICH pass writes real values, and a later
/// bare-listing re-observation (which carries them as `NULL`) never erases them.
/// The S127b `has_gguf` serveability flag is `COALESCE`-protected the same way.
/// The S128 `fit_score` (the persisted blended practical rank score) is
/// `COALESCE`-protected identically — a MEASURE pass writes a real score; a bare
/// listing re-observation carries `NULL` and never erases it.
///
/// FAIL-SOFT ON A MISSING `fit_score` COLUMN (forward/backward compatible): this
/// code is designed to deploy BEFORE the S128 migration is applied. The write
/// first attempts the statement INCLUDING `fit_score`; if Postgres reports the
/// column does not exist (SQLSTATE 42703, see [`is_missing_fit_score_column`]),
/// it logs ONCE per process and RETRIES with a legacy statement that omits the
/// column — a graceful no-op of the new field, never a crash of the intake pass.
/// Once the operator applies the migration the first path simply succeeds.
///
/// `gfx1151_class` keeps its CASE (not COALESCE) semantics — see
/// [`resolve_gfx1151_on_conflict`] for the pure mirror this SQL implements: a
/// derived non-`'unknown'` value REPLACES a stored one (including replacing a
/// prior `'unknown'` sentinel, since Ask-4 now DERIVES the class from `arch`),
/// while an incoming `'unknown'` (a bare re-observation) preserves whatever real
/// class an earlier enrich step recorded. This is exactly why a plain COALESCE
/// would be wrong here: the `'unknown'` sentinel is a non-NULL string, so
/// COALESCE would treat it as a real value and let a re-observation clobber a
/// derived class — the CASE avoids that.
pub async fn upsert_candidate(
    pool: &PgPool,
    candidate: &DiscoveryCandidate,
) -> Result<(), ToolError> {
    // First attempt: persist EVERYTHING including the S128 `fit_score` column.
    match execute_upsert(pool, candidate, true).await {
        Ok(()) => Ok(()),
        Err(e) if is_missing_fit_score_column(&e) => {
            // Fail-soft: the S128 migration hasn't been applied yet. Log once per
            // process (an enrich pass upserts many rows — don't spam), then retry
            // WITHOUT `fit_score`. This is exactly what lets the code deploy
            // BEFORE the operator-gated DDL runs.
            if !FIT_SCORE_MISSING_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "model_discovery_candidate.fit_score column is absent (S128 migration \
                     not yet applied) — persisting candidates WITHOUT fit_score until an \
                     operator applies migrations/S128-fitscore-persist.sql; ranking is \
                     unaffected (fit_score is recomputed transiently at selection time)"
                );
            }
            execute_upsert(pool, candidate, false).await.map_err(|e| {
                ToolError::Database(format!(
                    "upsert model_discovery_candidate row for '{}' (fit_score fallback): {e}",
                    candidate.model_name
                ))
            })
        }
        Err(e) => Err(ToolError::Database(format!(
            "upsert model_discovery_candidate row for '{}': {e}",
            candidate.model_name
        ))),
    }
}

/// Execute one upsert, WITH or WITHOUT the S128 `fit_score` column. The 19
/// shared binds are identical between the two renderings; only the trailing
/// `fit_score` bind (`$20`) is added when `with_fit`. Returns the raw
/// [`sqlx::Error`] so the caller can classify a missing-column error and fall
/// back — see [`upsert_candidate`].
async fn execute_upsert(
    pool: &PgPool,
    candidate: &DiscoveryCandidate,
    with_fit: bool,
) -> Result<(), sqlx::Error> {
    let sql = upsert_sql(with_fit);
    let mut q = sqlx::query(&sql)
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
        .bind(candidate.published_at)
        .bind(candidate.updated_at)
        .bind(candidate.license.as_deref())
        .bind(candidate.arch.as_deref())
        .bind(candidate.is_instruct)
        .bind(candidate.gated)
        .bind(candidate.quant_dtype.as_deref())
        .bind(candidate.has_gguf);
    if with_fit {
        q = q.bind(candidate.fit_score);
    }
    q.execute(pool).await.map(|_| ())
}

/// Pure mirror of the `gfx1151_class` `ON CONFLICT` CASE expression in
/// [`upsert_candidate`]'s SQL — extracted so the "treat `'unknown'` as
/// not-yet-classified, let a derived value overwrite it" rule is unit-testable
/// without a live Postgres. Returns the class string the upsert would persist.
///
/// Rule (matches the SQL CASE byte-for-byte in intent):
/// - `incoming == "unknown"` → keep `stored` (a bare re-observation never
///   downgrades a real class back to the sentinel);
/// - otherwise → take `incoming` (a DERIVED class — `"confirmed"`/
///   `"experimental"`/`"no"`, or even a re-derived `"unknown"` is only kept out
///   by the first arm — REPLACES the stored value, INCLUDING replacing a stored
///   `"unknown"`; this is the Ask-4 requirement that a derived value fills an
///   un-classified row).
pub fn resolve_gfx1151_on_conflict<'a>(stored: &'a str, incoming: &'a str) -> &'a str {
    if incoming == "unknown" {
        stored
    } else {
        incoming
    }
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
fn predecessors_for_transition(
    new_status: CandidateStatus,
) -> Result<Vec<CandidateStatus>, ToolError> {
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
        .map_err(|e| ToolError::Database(format!("transition_status for '{model_name}': {e}")))?;

    if result.rows_affected() == 0 {
        // Diagnostic-only: the guarded UPDATE above has already atomically
        // succeeded or failed. This SELECT just distinguishes "no such row"
        // from "row exists but its current status doesn't permit this
        // transition" for a useful error message.
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT status FROM model_discovery_candidate WHERE model_name = $1")
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

    // ---- S128 fit_score persistence: SQL rendering + fail-soft classifier ----
    //
    // The live INSERT/UPDATE body is thin sqlx over a real Postgres (same
    // DB-gated convention as the rest of this module), so it isn't unit-tested
    // against a DB here. The load-bearing NEW logic — that the WITH-fit and
    // LEGACY renderings differ ONLY by the fit_score column (so the fallback is a
    // true no-op of the field), and that the missing-column classifier fires only
    // on the specific 42703/fit_score case — is pure and fully covered below.

    #[test]
    fn with_fit_sql_persists_fit_score_and_coalesce_protects_it() {
        let sql = upsert_sql(true);
        // Column, value placeholder, and the COALESCE-on-conflict SET are all present.
        assert!(sql.contains(", fit_score)"), "fit_score in the column list");
        assert!(sql.contains(", $20)"), "the $20 bind for fit_score");
        assert!(
            sql.contains(
                "fit_score = COALESCE(EXCLUDED.fit_score, model_discovery_candidate.fit_score)"
            ),
            "fit_score COALESCE-protected on conflict so a bare re-observation never erases it"
        );
        // No unrendered template markers leaked into the SQL.
        assert!(!sql.contains("{FIT"), "all template markers substituted: {sql}");
    }

    #[test]
    fn legacy_sql_is_a_true_no_op_of_fit_score() {
        // The fallback (pre-S128-migration) statement must not mention fit_score
        // ANYWHERE — no column, no bind, no SET — so it is a graceful no-op of the
        // new field against a host whose table lacks the column.
        let sql = upsert_sql(false);
        assert!(
            !sql.contains("fit_score"),
            "legacy fallback SQL must omit fit_score entirely: {sql}"
        );
        // ...and it must still only bind through $19 (the 19 shared params).
        assert!(sql.contains("$19)"), "legacy VALUES ends at $19");
        assert!(!sql.contains("$20"), "legacy SQL has no $20 bind");
        assert!(!sql.contains("{FIT"), "all template markers substituted: {sql}");
    }

    #[test]
    fn with_fit_and_legacy_share_the_same_19_param_body() {
        // Everything except the fit_score additions must be byte-identical, proving
        // the two renderings can't drift in the shared binds.
        let with = upsert_sql(true)
            .replace(", fit_score)", ")")
            .replace(", $20)", ")")
            .replace(
                ", fit_score = COALESCE(EXCLUDED.fit_score, model_discovery_candidate.fit_score)",
                "",
            );
        assert_eq!(
            with,
            upsert_sql(false),
            "WITH-fit minus its fit_score additions must equal the legacy rendering"
        );
    }

    #[test]
    fn missing_fit_score_classifier_ignores_unrelated_errors() {
        // A non-database error (e.g. a pool/protocol error) must NOT be treated as
        // a missing-column case — the fallback is reserved for the exact 42703/
        // fit_score scenario. (RowNotFound carries no SQLSTATE, so it is rejected.)
        assert!(!is_missing_fit_score_column(&sqlx::Error::RowNotFound));
    }

    // ---- gfx1151_class ON CONFLICT resolution (the 'unknown'-overwrite rule) ----

    #[test]
    fn derived_gfx_class_overwrites_a_stored_unknown() {
        // The Ask-4 case: MEASURE derives a real class for a row that was
        // inserted with the 'unknown' sentinel — the derived value must win.
        assert_eq!(
            resolve_gfx1151_on_conflict("unknown", "confirmed"),
            "confirmed"
        );
        assert_eq!(
            resolve_gfx1151_on_conflict("unknown", "experimental"),
            "experimental"
        );
        // A derived UNSERVABLE verdict ("no") also replaces the sentinel.
        assert_eq!(resolve_gfx1151_on_conflict("unknown", "no"), "no");
    }

    #[test]
    fn bare_reobservation_never_downgrades_a_real_class_to_unknown() {
        // A discovery re-observation always carries 'unknown' (a listing exposes
        // no arch) — it must PRESERVE whatever real class an enrich step recorded.
        assert_eq!(
            resolve_gfx1151_on_conflict("confirmed", "unknown"),
            "confirmed"
        );
        assert_eq!(
            resolve_gfx1151_on_conflict("experimental", "unknown"),
            "experimental"
        );
        assert_eq!(resolve_gfx1151_on_conflict("no", "unknown"), "no");
    }

    #[test]
    fn a_re_derived_real_class_replaces_an_earlier_one() {
        // Two enrich passes both derive a real (non-unknown) class — the newer
        // derivation wins (e.g. an arch remap between passes).
        assert_eq!(
            resolve_gfx1151_on_conflict("experimental", "confirmed"),
            "confirmed"
        );
        assert_eq!(resolve_gfx1151_on_conflict("unknown", "unknown"), "unknown");
    }

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
