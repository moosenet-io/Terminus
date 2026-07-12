//! CXEG-08: risk-gate waivers.
//!
//! A tracked exception to `review_run`'s Stage-5b escalation gate
//! (`review::mod`'s `maybe_escalate`): a project owner can record that a
//! specific rule/scope combination is intentionally accepted at elevated
//! risk, with a MANDATORY reason, so the escalation gate stops widening the
//! review panel for it while the waiver is active.
//!
//! ## No new database (S9)
//!
//! Waivers are stored on the SAME KGFIND-01 `FindingsStore`
//! (`crate::scribe::graph::findings_store`) every other `review_run` finding
//! goes through — `category: "waiver"`, `scope_kind: Global`, `scope_ref:
//! project_id`. This is deliberate: a waiver is itself an interesting,
//! trend-worthy event (over-waiving should surface the same way recurring
//! findings do), and it avoids opening a second finding-shaped access path
//! (S9) or standing up a dedicated `kg_waivers` table for what is, at its
//! core, just another kind of tracked observation.
//!
//! Each waiver's `(rule, scope, reason, author, expiry)` is carried in the
//! finding's `provenance` JSON (the store's existing per-record metadata
//! field), keyed into the description (`"waiver[<rule>]: <reason>"`) so a
//! genuinely distinct `(rule, reason)` pair is its own row while a *repeated*
//! record of the identical `(rule, reason)` bumps `occurrences` and merges
//! provenance rather than duplicating — the store's own exact-description
//! dedup path (`FindingsStore::record` with `embedding: None`) does this for
//! free, unchanged.
//!
//! ## Fail-open, always
//!
//! [`active_waiver`] propagates a `FindingsStore` error (unconfigured DSN,
//! unreachable database, a query failure) to its caller rather than treating
//! it as "no waiver" itself -- `review::mod::maybe_escalate` is the one that
//! decides how to degrade (treat a lookup failure as "no active waiver
//! found", never as "block the review"), matching this item's fail-open
//! contract: the waiver/escalation layer must never be able to block the
//! correctness gate, in either direction.

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::scribe::graph::findings_store::{FindingsStore, NewFinding, RecordOutcome, ScopeKind};
use crate::tool::RustTool;

use super::{require_str, validate_project_id, PROJECT_IDS};

/// The finding `category` every waiver is recorded under -- distinct from any
/// review-produced finding category (`"bug"`, `"consistency"`, ...), so
/// `FindingsStore::list(project_id, Some("global"), Some(WAIVER_CATEGORY),
/// None)` cleanly isolates waivers from ordinary findings in the same table.
pub const WAIVER_CATEGORY: &str = "waiver";

/// Well-known rule id `review::mod::maybe_escalate` waives against for a
/// `cortex_review` `"high"`-band escalation. A fixed constant (not a caller
/// argument) for THIS gate specifically -- `cortex_waive`'s own `rule`
/// argument is still free-form, so future gates can mint their own rule ids
/// against the same waiver mechanism without a schema change.
pub const HIGH_RISK_BAND_RULE: &str = "cortex_review_high_band";

/// An active (non-expired, rule/scope-matching) waiver, as resolved by
/// [`active_waiver`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveWaiver {
    pub finding_id: uuid::Uuid,
    pub rule: String,
    pub scope: String,
    pub reason: String,
    pub author: String,
    pub expiry: Option<chrono::DateTime<chrono::Utc>>,
    /// `true` when the waiver's own recorded `scope` is strictly broader than
    /// the `requested_scope` it was matched against (e.g. a project-wide `*`
    /// waiver covering one specific changed file) -- allowed, but flagged so
    /// a caller can surface "this waiver is broader than the change it's
    /// suppressing" rather than silently accepting it.
    pub broad: bool,
}

impl ActiveWaiver {
    pub fn to_json(&self) -> Value {
        json!({
            "finding_id": self.finding_id.to_string(),
            "rule": self.rule,
            "scope": self.scope,
            "reason": self.reason,
            "author": self.author,
            "expiry": self.expiry.map(|e| e.to_rfc3339()),
            "broad": self.broad,
        })
    }
}

/// Pure scope-coverage check: does `waiver_scope` cover `requested_scope`?
/// `"*"` covers everything (and is `broad` unless the request is ALSO `"*"`).
/// Otherwise both sides are treated as comma-separated path sets and coverage
/// is "every requested path is in the waiver's set" (a superset match);
/// `broad` is `true` when the waiver's set is strictly larger than what was
/// requested. An empty `requested_scope` never matches (nothing to cover).
/// No I/O -- fully unit-testable.
pub fn scope_covers(waiver_scope: &str, requested_scope: &str) -> (bool, bool) {
    let waiver_scope = waiver_scope.trim();
    let requested_scope = requested_scope.trim();

    if waiver_scope == "*" {
        return (true, requested_scope != "*");
    }

    let waiver_set: std::collections::HashSet<&str> =
        waiver_scope.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let requested_set: std::collections::HashSet<&str> =
        requested_scope.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();

    if requested_set.is_empty() || waiver_set.is_empty() {
        return (false, false);
    }

    let covers = requested_set.is_subset(&waiver_set);
    let broad = covers && waiver_set.len() > requested_set.len();
    (covers, broad)
}

/// Record a waiver as a `category:"waiver"` finding on the KGFIND-01 store.
/// `reason` is MANDATORY and non-blank -- an empty/whitespace-only reason is
/// rejected with `InvalidArgument` before any store I/O. `expiry`, when
/// present, is when the waiver stops being active (see [`active_waiver`]).
pub async fn record_waiver(
    project_id: &str,
    rule: &str,
    scope: &str,
    reason: &str,
    author: &str,
    expiry: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<RecordOutcome, ToolError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ToolError::InvalidArgument(
            "'reason' is required and must be non-empty to record a waiver".to_string(),
        ));
    }
    let rule = rule.trim();
    if rule.is_empty() {
        return Err(ToolError::InvalidArgument("'rule' is required and must be non-empty".to_string()));
    }
    let scope = if scope.trim().is_empty() { "*" } else { scope.trim() };
    let author = if author.trim().is_empty() { "unknown" } else { author.trim() };

    let store = FindingsStore::from_env().await?;

    let provenance = json!({
        "rule": rule,
        "scope": scope,
        "reason": reason,
        "author": author,
        "expiry": expiry.map(|e| e.to_rfc3339()),
        "waived_at": chrono::Utc::now().to_rfc3339(),
    });

    let new_finding = NewFinding {
        project_id: project_id.to_string(),
        category: WAIVER_CATEGORY.to_string(),
        severity: "info".to_string(),
        scope_kind: ScopeKind::Global,
        scope_ref: project_id.to_string(),
        // `rule` folded into the description (not just provenance) so two
        // waivers with the SAME reason text but DIFFERENT rules never
        // collide in the store's exact-description dedup bucket.
        description: format!("waiver[{rule}]: {reason}"),
        provenance,
    };

    store.record(new_finding, None).await
}

/// The most recent entry of a `kg_findings.provenance` jsonb array (as
/// `FindingsStore::record`/`merge_provenance` append and cap it) -- the
/// latest `(rule, scope, reason, author, expiry)` this waiver row was last
/// recorded with. `None` if `provenance` isn't a non-empty array (defensive;
/// should not happen for a row this module wrote).
fn latest_entry(provenance: &Value) -> Option<&Value> {
    provenance.as_array().and_then(|arr| arr.last())
}

/// Look up an active (matching `rule`, scope-covering, non-expired) waiver
/// for `project_id`, or `None` if none matches. Propagates a `FindingsStore`
/// error (unconfigured/unreachable) to the caller -- see module doc's
/// "Fail-open, always": THIS function does not itself decide fail-open
/// behavior, `review::mod::maybe_escalate` does, by treating an `Err` here as
/// "no active waiver" rather than blocking anything.
///
/// When multiple rows match, the FIRST is returned; `FindingsStore::list`
/// orders by `occurrences DESC, last_seen DESC`, so the most
/// frequently-recorded / most recently-touched matching waiver wins
/// deterministically.
pub async fn active_waiver(project_id: &str, rule: &str, requested_scope: &str) -> Result<Option<ActiveWaiver>, ToolError> {
    let store = FindingsStore::from_env().await?;
    let rows = store
        .list(project_id, Some(ScopeKind::Global.as_str()), Some(WAIVER_CATEGORY), None)
        .await?;

    let now = chrono::Utc::now();
    for row in rows {
        let Some(entry) = latest_entry(&row.provenance) else { continue };
        let Some(entry_rule) = entry.get("rule").and_then(Value::as_str) else { continue };
        if entry_rule != rule {
            continue;
        }
        let entry_scope = entry.get("scope").and_then(Value::as_str).unwrap_or("*");
        let (covers, broad) = scope_covers(entry_scope, requested_scope);
        if !covers {
            continue;
        }

        let expiry = entry
            .get("expiry")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));
        if let Some(exp) = expiry {
            if exp <= now {
                continue; // expired -- not active, keep looking
            }
        }

        let reason = entry.get("reason").and_then(Value::as_str).unwrap_or_default().to_string();
        let author = entry.get("author").and_then(Value::as_str).unwrap_or("unknown").to_string();

        return Ok(Some(ActiveWaiver {
            finding_id: row.id,
            rule: rule.to_string(),
            scope: entry_scope.to_string(),
            reason,
            author,
            expiry,
            broad,
        }));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Tool: cortex_waive (CXEG-08)
// ---------------------------------------------------------------------------

pub struct CortexWaive;

#[async_trait::async_trait]
impl RustTool for CortexWaive {
    fn name(&self) -> &str {
        "cortex_waive"
    }

    fn description(&self) -> &str {
        "Record a tracked waiver against review_run's Stage-5b risk-gate \
         escalation (a high cortex_review band). project_id: one of \
         TERM/LUM/HARM/CHRD/RAIL. rule: the gate rule id being waived (use \
         'cortex_review_high_band' for the built-in risk-band escalation \
         gate). scope: '*' for project-wide, or a comma-separated list of \
         file paths the waiver covers (default '*'). reason is MANDATORY \
         and non-blank -- rejected with InvalidArgument if empty. author: \
         who recorded the waiver (default 'unknown'). expiry: optional \
         RFC3339 timestamp after which the waiver stops being active (a \
         waiver with no expiry never expires on its own). The waiver is \
         recorded as a category:'waiver' finding on the same Atlas KGFIND \
         store every other review_run finding uses, so over-waiving surfaces \
         in the normal findings/trend tooling. A waiver whose scope is \
         broader than the change it later suppresses is still accepted -- \
         the gate flags broad:true rather than rejecting it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "rule": { "type": "string", "description": "The gate rule id being waived, e.g. 'cortex_review_high_band'" },
                "scope": { "type": "string", "description": "'*' (project-wide, default) or a comma-separated list of file paths" },
                "reason": { "type": "string", "description": "MANDATORY, non-blank justification for the waiver" },
                "author": { "type": "string", "description": "Who recorded the waiver (default 'unknown')" },
                "expiry": { "type": "string", "description": "Optional RFC3339 timestamp after which the waiver stops being active" }
            },
            "required": ["project_id", "rule", "reason"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        validate_project_id(project_id)?;
        let rule = require_str(&args, "rule")?;
        let reason = require_str(&args, "reason")?;
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("*");
        let author = args.get("author").and_then(Value::as_str).unwrap_or("unknown");

        let expiry = match args.get("expiry") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArgument("'expiry' must be an RFC3339 timestamp string".to_string()))?;
                let dt = chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| ToolError::InvalidArgument(format!("'expiry' is not a valid RFC3339 timestamp: {e}")))?;
                Some(dt.with_timezone(&chrono::Utc))
            }
        };

        let outcome = record_waiver(project_id, rule, scope, reason, author, expiry).await?;

        let (waiver_id, occurrences, created) = match outcome {
            RecordOutcome::Created(id) => (id, 1, true),
            RecordOutcome::Recurred { id, occurrences } => (id, occurrences, false),
        };

        let response = json!({
            "recorded": true,
            "created": created,
            "waiver_id": waiver_id.to_string(),
            "occurrences": occurrences,
            "project_id": project_id,
            "rule": rule,
            "scope": scope,
            "reason": reason,
            "author": author,
            "expiry": expiry.map(|e| e.to_rfc3339()),
        });
        serde_json::to_string_pretty(&response).map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scope_covers: pure, no I/O ──────────────────────────────────────

    #[test]
    fn wildcard_waiver_covers_anything_and_is_broad_unless_request_is_also_wildcard() {
        assert_eq!(scope_covers("*", "src/a.rs"), (true, true));
        assert_eq!(scope_covers("*", "src/a.rs,src/b.rs"), (true, true));
        assert_eq!(scope_covers("*", "*"), (true, false));
    }

    #[test]
    fn exact_match_covers_and_is_not_broad() {
        assert_eq!(scope_covers("src/a.rs", "src/a.rs"), (true, false));
        assert_eq!(scope_covers("src/a.rs,src/b.rs", "src/a.rs,src/b.rs"), (true, false));
    }

    #[test]
    fn superset_waiver_covers_and_is_broad() {
        assert_eq!(scope_covers("src/a.rs,src/b.rs,src/c.rs", "src/a.rs"), (true, true));
    }

    #[test]
    fn disjoint_or_narrower_waiver_does_not_cover() {
        assert_eq!(scope_covers("src/a.rs", "src/b.rs"), (false, false));
        assert_eq!(scope_covers("src/a.rs", "src/a.rs,src/b.rs"), (false, false));
    }

    #[test]
    fn empty_requested_scope_never_matches() {
        assert_eq!(scope_covers("*", ""), (false, false));
        assert_eq!(scope_covers("src/a.rs", ""), (false, false));
    }

    #[test]
    fn whitespace_around_paths_is_trimmed() {
        assert_eq!(scope_covers(" src/a.rs , src/b.rs ", "src/a.rs,src/b.rs"), (true, false));
    }

    // ── record_waiver: argument validation (no store needed to fail) ────

    #[tokio::test]
    async fn record_waiver_rejects_blank_reason() {
        let err = record_waiver("TERM", "cortex_review_high_band", "*", "   ", "alice", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn record_waiver_rejects_empty_reason() {
        let err = record_waiver("TERM", "cortex_review_high_band", "*", "", "alice", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn record_waiver_rejects_blank_rule() {
        let err = record_waiver("TERM", "  ", "*", "accepted risk for the sprint", "alice", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── active_waiver: degrade without a configured store ────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn active_waiver_not_configured_without_dsn() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // a real DSN is live in this process; skip
        }
        let err = active_waiver("TERM", HIGH_RISK_BAND_RULE, "src/a.rs").await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── cortex_waive tool: argument validation ───────────────────────────

    #[tokio::test]
    async fn tool_rejects_unknown_project_id() {
        let err = CortexWaive
            .execute(json!({"project_id": "NOPE", "rule": "r", "reason": "because"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn tool_rejects_missing_reason() {
        let err = CortexWaive
            .execute(json!({"project_id": "TERM", "rule": "cortex_review_high_band"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn tool_rejects_blank_reason() {
        let err = CortexWaive
            .execute(json!({"project_id": "TERM", "rule": "cortex_review_high_band", "reason": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn tool_rejects_missing_rule() {
        let err = CortexWaive
            .execute(json!({"project_id": "TERM", "reason": "because"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn tool_rejects_malformed_expiry() {
        let err = CortexWaive
            .execute(json!({
                "project_id": "TERM",
                "rule": "cortex_review_high_band",
                "reason": "because",
                "expiry": "not-a-timestamp"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
