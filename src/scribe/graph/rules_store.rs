//! KGRULE-01: Atlas KG rule store.
//!
//! Owns the `kg_rules` table: crystallized, durable "rules" governing a
//! scope (node/path/community/global) within a project's knowledge graph.
//! A rule starts life as a `candidate` (minted from recurring findings,
//! never enforced), and is only promoted to `active` after an adversarial
//! `review_run` panel argues it is earned (see KGRULE-03). Active rules
//! carry an enforcement level (`advisory` < `lint-candidate` < `blocking`)
//! and are bi-temporal (`valid_from`/`valid_to`) so they can be retired
//! without deleting history.
//!
//! Reuse template this mirrors (see `findings_store.rs`'s `FindingsStore`):
//! - bounded `PgPool` sourced from `crate::config::atlas_database_url()`
//! - advisory-locked idempotent migration idiom, distinct lock key
//! - atomic, idempotent create via a transaction-scoped advisory lock keyed
//!   by the dedup bucket
//! - `NotConfigured` when the DSN is unset; parameterized queries throughout
//! - manual `FromRow` decode via `sqlx::Row::try_get` (the `sqlx` `derive`
//!   feature is deliberately not enabled in this workspace)

use serde_json::Value as JsonValue;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ToolError;
pub use crate::scribe::graph::findings_store::ScopeKind;

/// Fixed advisory-lock key for the `kg_rules` migration. Distinct from other
/// modules' keys (`vec_store::ADVISORY_LOCK_KEY`,
/// `findings_store::ADVISORY_LOCK_KEY`) so concurrent migrations across
/// subsystems never contend on the same lock.
const ADVISORY_LOCK_KEY: i64 = 6_730_194_558_027_146_219;

/// Enforcement level of an active rule. Ordered `Advisory < LintCandidate <
/// Blocking` for [`RuleRow`] priority ordering in `list_active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Enforcement {
    Advisory,
    LintCandidate,
    Blocking,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Enforcement::Advisory => "advisory",
            Enforcement::LintCandidate => "lint-candidate",
            Enforcement::Blocking => "blocking",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "advisory" => Some(Enforcement::Advisory),
            "lint-candidate" => Some(Enforcement::LintCandidate),
            "blocking" => Some(Enforcement::Blocking),
            _ => None,
        }
    }

    /// Sort priority for `list_active` ordering: higher enforcement first.
    fn priority(self) -> i32 {
        match self {
            Enforcement::Blocking => 2,
            Enforcement::LintCandidate => 1,
            Enforcement::Advisory => 0,
        }
    }
}

/// Lifecycle status of a rule row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleStatus {
    Candidate,
    Active,
    Retired,
}

impl RuleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleStatus::Candidate => "candidate",
            RuleStatus::Active => "active",
            RuleStatus::Retired => "retired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(RuleStatus::Candidate),
            "active" => Some(RuleStatus::Active),
            "retired" => Some(RuleStatus::Retired),
            _ => None,
        }
    }
}

/// A new candidate rule to record. `provenance` is the JSON blob describing
/// where this rule came from (e.g. the findings it was crystallized from).
#[derive(Debug, Clone)]
pub struct NewRule {
    pub project_id: String,
    pub scope_kind: ScopeKind,
    pub scope_ref: String,
    pub category: String,
    pub guidance: String,
    pub provenance: JsonValue,
    pub recurrence_at_creation: Option<i32>,
    pub cortex_risk: Option<f32>,
}

/// A stored rule row, as read back via [`RulesStore::list_active`].
#[derive(Debug, Clone)]
pub struct RuleRow {
    pub id: Uuid,
    pub project_id: String,
    pub scope_kind: String,
    pub scope_ref: String,
    pub category: String,
    pub guidance: String,
    pub enforcement: String,
    pub status: String,
    pub provenance: JsonValue,
    pub recurrence_at_creation: Option<i32>,
    pub cortex_risk: Option<f32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub valid_from: chrono::DateTime<chrono::Utc>,
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
}

impl RuleRow {
    /// Manual row mapping (see module docs: the `sqlx::FromRow` derive
    /// feature is not enabled in this workspace), keyed by the column
    /// order/names selected in [`RulesStore::list_active`].
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            scope_kind: row.try_get("scope_kind")?,
            scope_ref: row.try_get("scope_ref")?,
            category: row.try_get("category")?,
            guidance: row.try_get("guidance")?,
            enforcement: row.try_get("enforcement")?,
            status: row.try_get("status")?,
            provenance: row.try_get("provenance")?,
            recurrence_at_creation: row.try_get("recurrence_at_creation")?,
            cortex_risk: row.try_get("cortex_risk")?,
            created_at: row.try_get("created_at")?,
            valid_from: row.try_get("valid_from")?,
            valid_to: row.try_get("valid_to")?,
        })
    }
}

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS kg_rules ( \
    id uuid PRIMARY KEY, \
    project_id text NOT NULL, \
    scope_kind text NOT NULL CHECK (scope_kind IN ('node','path','community','global')), \
    scope_ref text NOT NULL, \
    category text NOT NULL, \
    guidance text NOT NULL, \
    enforcement text NOT NULL DEFAULT 'advisory' CHECK (enforcement IN ('advisory','lint-candidate','blocking')), \
    status text NOT NULL DEFAULT 'candidate' CHECK (status IN ('candidate','active','retired')), \
    provenance jsonb NOT NULL DEFAULT '{}'::jsonb, \
    recurrence_at_creation int, \
    cortex_risk real, \
    created_at timestamptz NOT NULL DEFAULT now(), \
    valid_from timestamptz NOT NULL DEFAULT now(), \
    valid_to timestamptz \
)";

const CREATE_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS kg_rules_scope \
    ON kg_rules (project_id, scope_kind, scope_ref, category, status)";

const SELECT_BUCKET_SQL: &str = "SELECT id FROM kg_rules \
    WHERE project_id = $1 AND scope_kind = $2 AND scope_ref = $3 AND category = $4 \
    AND status IN ('candidate', 'active') \
    LIMIT 1";

const INSERT_SQL: &str = "INSERT INTO kg_rules \
    (id, project_id, scope_kind, scope_ref, category, guidance, enforcement, status, \
     provenance, recurrence_at_creation, cortex_risk, created_at, valid_from) \
    VALUES ($1, $2, $3, $4, $5, $6, 'advisory', 'candidate', $7, $8, $9, now(), now())";

const PROMOTE_SQL: &str = "UPDATE kg_rules SET \
    status = 'active', enforcement = $2, provenance = $3, valid_from = now() \
    WHERE id = $1 AND status = 'candidate' \
    RETURNING id";

const SELECT_STATUS_SQL: &str = "SELECT status FROM kg_rules WHERE id = $1";

const RETIRE_SQL: &str = "UPDATE kg_rules SET status = 'retired', valid_to = now() \
    WHERE id = $1 \
    RETURNING id";

/// Owns the `kg_rules` table and its pool.
pub struct RulesStore {
    pool: PgPool,
}

impl RulesStore {
    /// Resolve the DSN via `config::atlas_database_url()`, build a bounded
    /// pool, and run the idempotent migration. Returns `NotConfigured`
    /// (never attempting a connect) when no DSN is set.
    pub async fn from_env() -> Result<Self, ToolError> {
        let url = crate::config::atlas_database_url()
            .ok_or_else(|| ToolError::NotConfigured("ATLAS_DATABASE_URL not set".into()))?;

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| ToolError::Database(format!("connect atlas rules store: {e}")))?;

        migrate(&pool).await?;

        Ok(Self { pool })
    }

    /// Create a candidate rule, idempotent per
    /// `(project_id, scope_kind, scope_ref, category)`: if a row with
    /// status `candidate` or `active` already exists for that bucket, its
    /// id is returned rather than inserting a duplicate. Atomic via a
    /// transaction-scoped advisory lock keyed by the bucket (mirrors
    /// `findings_store::record`'s TOCTOU-safe pattern), so concurrent
    /// crystallize calls for the same bucket never double-insert.
    pub async fn create_candidate(&self, r: NewRule) -> Result<Uuid, ToolError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store begin: {e}")))?;

        let bucket = format!(
            "{}|{}|{}|{}",
            r.project_id,
            r.scope_kind.as_str(),
            r.scope_ref,
            r.category
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&bucket)
            .execute(&mut *tx)
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store bucket lock: {e}")))?;

        let existing: Option<(Uuid,)> = sqlx::query_as(SELECT_BUCKET_SQL)
            .bind(&r.project_id)
            .bind(r.scope_kind.as_str())
            .bind(&r.scope_ref)
            .bind(&r.category)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store select bucket: {e}")))?;

        let id = if let Some((existing_id,)) = existing {
            existing_id
        } else {
            let id = Uuid::new_v4();
            sqlx::query(INSERT_SQL)
                .bind(id)
                .bind(&r.project_id)
                .bind(r.scope_kind.as_str())
                .bind(&r.scope_ref)
                .bind(&r.category)
                .bind(&r.guidance)
                .bind(&r.provenance)
                .bind(r.recurrence_at_creation)
                .bind(r.cortex_risk)
                .execute(&mut *tx)
                .await
                .map_err(|e| ToolError::Database(format!("atlas rules store insert: {e}")))?;
            id
        };

        tx.commit()
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store commit: {e}")))?;

        Ok(id)
    }

    /// Promote a candidate rule to `active` with the given enforcement
    /// level, recording `provenance` (typically the promotion review
    /// result). If the rule is already `active`, this is a no-op success
    /// (idempotent promotion). If the rule does not exist, or exists but is
    /// `retired` (not `candidate`), returns a clear error rather than
    /// silently doing nothing.
    pub async fn promote(
        &self,
        id: Uuid,
        enforcement: Enforcement,
        provenance: JsonValue,
    ) -> Result<(), ToolError> {
        let updated: Option<(Uuid,)> = sqlx::query_as(PROMOTE_SQL)
            .bind(id)
            .bind(enforcement.as_str())
            .bind(&provenance)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store promote: {e}")))?;

        if updated.is_some() {
            return Ok(());
        }

        // Nothing was updated: either the id doesn't exist, or it exists but
        // isn't a candidate (already active — no-op-Ok; retired — error).
        let status: Option<(String,)> = sqlx::query_as(SELECT_STATUS_SQL)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                ToolError::Database(format!("atlas rules store promote status check: {e}"))
            })?;

        match status {
            None => Err(ToolError::NotFound(format!("kg_rules id {id} not found"))),
            Some((s, ..)) if s == RuleStatus::Active.as_str() => Ok(()),
            Some((s, ..)) => Err(ToolError::Conflict(format!(
                "kg_rules id {id} is '{s}', not 'candidate' — cannot promote"
            ))),
        }
    }

    /// Retire a rule: sets `status = 'retired'` and `valid_to = now()`.
    /// `reason` is accepted for interface clarity (callers should fold it
    /// into provenance before calling if it needs to be durable) but is not
    /// itself persisted as a separate column.
    pub async fn retire(&self, id: Uuid, reason: &str) -> Result<(), ToolError> {
        let _ = reason;
        let updated: Option<(Uuid,)> = sqlx::query_as(RETIRE_SQL)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store retire: {e}")))?;

        match updated {
            Some(_) => Ok(()),
            None => Err(ToolError::NotFound(format!("kg_rules id {id} not found"))),
        }
    }

    /// List active, non-expired rules for a project, optionally filtered by
    /// scope kind, scope ref, and category. Ordered by enforcement priority
    /// (blocking > lint-candidate > advisory) then `created_at DESC`. All
    /// filters are bound parameters — the WHERE clause is built dynamically
    /// but nothing is ever interpolated.
    pub async fn list_active(
        &self,
        project_id: &str,
        scope_kind: Option<&str>,
        scope_ref: Option<&str>,
        category: Option<&str>,
    ) -> Result<Vec<RuleRow>, ToolError> {
        let mut sql = String::from(
            "SELECT id, project_id, scope_kind, scope_ref, category, guidance, enforcement, \
             status, provenance, recurrence_at_creation, cortex_risk, created_at, valid_from, \
             valid_to \
             FROM kg_rules WHERE project_id = $1 AND status = 'active' AND valid_to IS NULL",
        );

        let mut idx = 1;
        if scope_kind.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND scope_kind = ${idx}"));
        }
        if scope_ref.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND scope_ref = ${idx}"));
        }
        if category.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND category = ${idx}"));
        }
        sql.push_str(
            " ORDER BY (CASE enforcement \
                WHEN 'blocking' THEN 2 \
                WHEN 'lint-candidate' THEN 1 \
                ELSE 0 END) DESC, created_at DESC",
        );

        let mut query = sqlx::query(&sql).bind(project_id.to_string());
        if let Some(sk) = scope_kind {
            query = query.bind(sk.to_string());
        }
        if let Some(sr) = scope_ref {
            query = query.bind(sr.to_string());
        }
        if let Some(c) = category {
            query = query.bind(c.to_string());
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas rules store list_active: {e}")))?;

        rows.iter()
            .map(RuleRow::from_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::Database(format!("atlas rules store list decode: {e}")))
    }
}

/// Pure predicate: a rule is active for consumption iff its status is
/// `"active"` and it has not expired (`valid_to.is_none()`). No I/O, fully
/// unit-testable — mirrors the WHERE clause in `list_active`.
pub fn is_active(status: &str, valid_to: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    status == RuleStatus::Active.as_str() && valid_to.is_none()
}

/// Idempotent, advisory-locked migration: `kg_rules` table + its scope
/// index. Mirrors `findings_store::migrate` exactly (same
/// connection-discard-on-drop and advisory-lock idiom, distinct lock key).
async fn migrate(pool: &PgPool) -> Result<(), ToolError> {
    let mut conn = pool.acquire().await.map_err(|e| {
        ToolError::Database(format!(
            "acquire dedicated connection for atlas rules store migrate: {e}"
        ))
    })?;

    conn.close_on_drop();

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            ToolError::Database(format!(
                "acquire atlas rules store migrate advisory lock: {e}"
            ))
        })?;

    let result = migrate_locked(&mut conn).await;

    if let Err(unlock_err) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            "atlas rules store migrate: failed to release advisory lock {}: {unlock_err} \
             (harmless — the lock is released automatically when this connection closes)",
            ADVISORY_LOCK_KEY
        );
    }

    result
}

async fn migrate_locked(conn: &mut sqlx::PgConnection) -> Result<(), ToolError> {
    sqlx::query(CREATE_TABLE_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create kg_rules table: {e}")))?;

    sqlx::query(CREATE_INDEX_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create kg_rules_scope index: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_enforcement_str_roundtrip() {
        for e in [
            Enforcement::Advisory,
            Enforcement::LintCandidate,
            Enforcement::Blocking,
        ] {
            assert_eq!(Enforcement::parse(e.as_str()), Some(e));
        }
        assert_eq!(Enforcement::parse("bogus"), None);
    }

    #[test]
    fn test_enforcement_ordering() {
        assert!(Enforcement::Advisory < Enforcement::LintCandidate);
        assert!(Enforcement::LintCandidate < Enforcement::Blocking);
        assert!(Enforcement::Advisory.priority() < Enforcement::Blocking.priority());
    }

    #[test]
    fn test_rule_status_str_roundtrip() {
        for s in [
            RuleStatus::Candidate,
            RuleStatus::Active,
            RuleStatus::Retired,
        ] {
            assert_eq!(RuleStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(RuleStatus::parse("bogus"), None);
    }

    #[test]
    fn test_scope_kind_reexported_roundtrip() {
        for k in [
            ScopeKind::Node,
            ScopeKind::Path,
            ScopeKind::Community,
            ScopeKind::Global,
        ] {
            assert_eq!(ScopeKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn test_is_active_true_when_active_and_not_expired() {
        assert!(is_active("active", None));
    }

    #[test]
    fn test_is_active_false_when_expired() {
        assert!(!is_active("active", Some(chrono::Utc::now())));
    }

    #[test]
    fn test_is_active_false_when_candidate() {
        assert!(!is_active("candidate", None));
    }

    #[test]
    fn test_is_active_false_when_retired() {
        assert!(!is_active("retired", Some(chrono::Utc::now())));
        assert!(!is_active("retired", None));
    }

    #[test]
    fn test_migration_sql_contains_kg_rules() {
        assert!(CREATE_TABLE_SQL.contains("kg_rules"));
        assert!(CREATE_INDEX_SQL.contains("kg_rules"));
    }

    #[test]
    fn test_migration_sql_contains_enforcement_check_values() {
        assert!(CREATE_TABLE_SQL.contains("'advisory'"));
        assert!(CREATE_TABLE_SQL.contains("'lint-candidate'"));
        assert!(CREATE_TABLE_SQL.contains("'blocking'"));
    }

    #[test]
    fn test_migration_sql_contains_status_check_values() {
        assert!(CREATE_TABLE_SQL.contains("'candidate'"));
        assert!(CREATE_TABLE_SQL.contains("'active'"));
        assert!(CREATE_TABLE_SQL.contains("'retired'"));
    }

    #[test]
    fn test_migration_sql_contains_scope_kind_check_values() {
        assert!(CREATE_TABLE_SQL.contains("'node'"));
        assert!(CREATE_TABLE_SQL.contains("'path'"));
        assert!(CREATE_TABLE_SQL.contains("'community'"));
        assert!(CREATE_TABLE_SQL.contains("'global'"));
    }

    #[test]
    fn test_migration_sql_contains_bitemporal_columns() {
        assert!(CREATE_TABLE_SQL.contains("valid_from"));
        assert!(CREATE_TABLE_SQL.contains("valid_to"));
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_not_configured_without_env() {
        // Mirrors findings_store's shape: if a real DSN happens to be
        // configured in this process, skip gracefully (never attempt a live
        // connection from a unit test) rather than mutating global env state.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // skip — a real DSN is available, not testing NotConfigured
        }

        let result = RulesStore::from_env().await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }
}
