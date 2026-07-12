//! KGFIND-01: Atlas KG findings store.
//!
//! Owns the `kg_findings` table: durable, deduplicated records of analysis
//! findings (lint-like observations, review notes, anomalies, …) scoped to a
//! node/path/community/global bucket within a project. Repeated findings
//! that match an existing bucket entry (either by embedding similarity above
//! a threshold, or by exact text match when no embedding is supplied) bump
//! an `occurrences` counter and merge provenance instead of creating
//! duplicate rows.
//!
//! Reuse template this mirrors (see `vec_store.rs`'s `AtlasVecStore`):
//! - bounded `PgPool` sourced from `crate::config::atlas_database_url()`
//! - advisory-locked idempotent migration idiom
//! - `pgvector::Vector` binding and the `(1 - (embedding <=> $1))::real`
//!   float8 -> f32 cast
//! - `NotConfigured` when the DSN is unset; parameterized queries throughout

use serde_json::Value as JsonValue;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ToolError;

/// Embedding dimension for `kg_findings.embedding` (`vector(1024)`), matching
/// the default embeddings model used elsewhere in Atlas (EMBED-02:
/// Qwen3-Embedding via Chord's `/v1/embeddings` proxy — see
/// `vec_store::KG_EMBED_DIM`).
pub const FINDINGS_EMBED_DIM: usize = 1024;

/// Default cosine-similarity threshold at/above which a new finding is
/// treated as a recurrence of an existing one in the same bucket, rather
/// than a new row. Overridable via `KGFIND_DEDUP_THRESHOLD`.
pub const DEFAULT_DEDUP_THRESHOLD: f32 = 0.92;

/// Maximum number of provenance entries retained per finding row; older
/// entries are dropped (oldest-first) on merge.
const MAX_PROVENANCE_ENTRIES: usize = 20;

/// Fixed advisory-lock key for the `kg_findings` migration. Distinct from
/// other modules' keys (e.g. `vec_store::ADVISORY_LOCK_KEY`) so concurrent
/// migrations across subsystems never contend on the same lock.
const ADVISORY_LOCK_KEY: i64 = 5_216_408_773_902_517_663;

/// Scope a finding is attached to within a project's knowledge graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Node,
    Path,
    Community,
    Global,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Node => "node",
            ScopeKind::Path => "path",
            ScopeKind::Community => "community",
            ScopeKind::Global => "global",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(ScopeKind::Node),
            "path" => Some(ScopeKind::Path),
            "community" => Some(ScopeKind::Community),
            "global" => Some(ScopeKind::Global),
            _ => None,
        }
    }
}

/// A new finding to record. `provenance` is a single JSON entry (e.g. run id,
/// timestamp, source) describing where this observation came from; it is
/// appended to the row's provenance array on both create and recurrence.
#[derive(Debug, Clone)]
pub struct NewFinding {
    pub project_id: String,
    pub category: String,
    pub severity: String,
    pub scope_kind: ScopeKind,
    pub scope_ref: String,
    pub description: String,
    pub provenance: JsonValue,
}

/// A stored finding row, as read back via [`FindingsStore::list`].
#[derive(Debug, Clone)]
pub struct FindingRow {
    pub id: Uuid,
    pub project_id: String,
    pub category: String,
    pub severity: String,
    pub scope_kind: String,
    pub scope_ref: String,
    pub description: String,
    pub provenance: JsonValue,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub occurrences: i32,
    /// CXEG-09: crystallization-loop state for this finding — `None` (never
    /// processed), `Some("promoted")` (survived adversarial promotion and
    /// emitted a lint stub / prose rule), or `Some("refuted")` (a promotion
    /// panel argued it should NOT become a standing rule). Read by
    /// `crate::cortex::crystallize`'s candidate selection so a refuted (or
    /// already-promoted) finding does not re-enter every crystallization
    /// cycle — see that module's doc for the convergence contract. `None`
    /// for every row written before this column existed (migration adds it
    /// nullable, no backfill).
    pub crystallize_state: Option<String>,
}

impl FindingRow {
    /// Manual row mapping (the `sqlx::FromRow` derive needs the `derive`
    /// feature, which this workspace's `sqlx` dependency deliberately
    /// doesn't enable — see `Cargo.toml`), keyed by the column order/names
    /// selected in [`FindingsStore::list`].
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            category: row.try_get("category")?,
            severity: row.try_get("severity")?,
            scope_kind: row.try_get("scope_kind")?,
            scope_ref: row.try_get("scope_ref")?,
            description: row.try_get("description")?,
            provenance: row.try_get("provenance")?,
            first_seen: row.try_get("first_seen")?,
            last_seen: row.try_get("last_seen")?,
            occurrences: row.try_get("occurrences")?,
            crystallize_state: row.try_get("crystallize_state")?,
        })
    }
}

/// Result of [`FindingsStore::record`].
#[derive(Debug, Clone, PartialEq)]
pub enum RecordOutcome {
    /// A brand-new row was inserted with this id.
    Created(Uuid),
    /// An existing row was matched and bumped; `occurrences` is the new count.
    Recurred { id: Uuid, occurrences: i32 },
}

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS kg_findings ( \
    id uuid PRIMARY KEY, \
    project_id text NOT NULL, \
    category text NOT NULL, \
    severity text NOT NULL, \
    scope_kind text NOT NULL CHECK (scope_kind IN ('node','path','community','global')), \
    scope_ref text NOT NULL, \
    description text NOT NULL, \
    embedding vector(1024), \
    provenance jsonb NOT NULL DEFAULT '[]'::jsonb, \
    first_seen timestamptz NOT NULL DEFAULT now(), \
    last_seen timestamptz NOT NULL DEFAULT now(), \
    occurrences int NOT NULL DEFAULT 1, \
    crystallize_state text \
)";

// CXEG-09: idempotent column add for a `kg_findings` table created before
// `crystallize_state` existed -- `CREATE TABLE IF NOT EXISTS` above is a
// no-op against an already-existing table, so an explicit `ADD COLUMN IF NOT
// EXISTS` is the only way an upgrade actually gets the column. Nullable, no
// default, no CHECK constraint (validated in Rust at the call site instead --
// `ADD CONSTRAINT IF NOT EXISTS` is not supported by vanilla Postgres, so a
// CHECK here would not be safely idempotent).
const ADD_CRYSTALLIZE_STATE_COLUMN_SQL: &str =
    "ALTER TABLE kg_findings ADD COLUMN IF NOT EXISTS crystallize_state text";

/// CXEG-09: mark a finding's crystallization outcome. `state` must be
/// `"promoted"` or `"refuted"` (validated here, not via a DB CHECK — see
/// [`ADD_CRYSTALLIZE_STATE_COLUMN_SQL`]'s doc comment for why).
const MARK_CRYSTALLIZE_STATE_SQL: &str =
    "UPDATE kg_findings SET crystallize_state = $2 WHERE id = $1";

const CREATE_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS kg_findings_bucket \
    ON kg_findings (project_id, scope_kind, scope_ref, category)";

const CREATE_HNSW_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS kg_findings_embedding_hnsw \
    ON kg_findings USING hnsw (embedding vector_cosine_ops)";

const SELECT_BUCKET_SQL: &str = "SELECT id, embedding FROM kg_findings \
    WHERE project_id = $1 AND scope_kind = $2 AND scope_ref = $3 AND category = $4 \
    AND embedding IS NOT NULL";

const SELECT_EXACT_SQL: &str = "SELECT id FROM kg_findings \
    WHERE project_id = $1 AND scope_kind = $2 AND scope_ref = $3 AND category = $4 \
    AND description = $5 \
    LIMIT 1";

const INSERT_SQL: &str = "INSERT INTO kg_findings \
    (id, project_id, category, severity, scope_kind, scope_ref, description, embedding, \
     provenance, first_seen, last_seen, occurrences) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now(), 1)";

const BUMP_SQL: &str = "UPDATE kg_findings SET \
    occurrences = occurrences + 1, last_seen = now(), provenance = $2 \
    WHERE id = $1 \
    RETURNING occurrences";

/// Owns the `kg_findings` pgvector table and its pool.
pub struct FindingsStore {
    pool: PgPool,
}

impl FindingsStore {
    /// Resolve the DSN via `config::atlas_database_url()`, build a bounded
    /// pool, and run the idempotent migration. Returns `NotConfigured`
    /// (never attempting a connect) when no DSN is set.
    pub async fn from_env() -> Result<Self, ToolError> {
        let url = crate::config::atlas_database_url().ok_or_else(|| {
            ToolError::NotConfigured("ATLAS_DATABASE_URL not set".into())
        })?;

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| ToolError::Database(format!("connect atlas findings store: {e}")))?;

        migrate(&pool).await?;

        Ok(Self { pool })
    }

    /// Record a finding: dedup against the existing bucket
    /// `(project_id, scope_kind, scope_ref, category)`, either by embedding
    /// similarity (when `embedding` is supplied) or exact description match
    /// (when it is not), and either bump an existing row or insert a new one.
    pub async fn record(
        &self,
        f: NewFinding,
        embedding: Option<Vec<f32>>,
    ) -> Result<RecordOutcome, ToolError> {
        let threshold = dedup_threshold();

        // Atomicity: the dedup SELECT and the follow-up INSERT/UPDATE must be one
        // unit — otherwise two concurrent records for the same bucket can BOTH
        // miss the match and insert duplicate rows (a TOCTOU race). Run them in
        // one transaction guarded by a transaction-scoped advisory lock keyed by
        // the bucket, so same-bucket records serialize while different buckets
        // still proceed in parallel. The xact lock auto-releases on commit/rollback.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ToolError::Database(format!("atlas findings store begin: {e}")))?;

        let bucket = format!(
            "{}|{}|{}|{}",
            f.project_id,
            f.scope_kind.as_str(),
            f.scope_ref,
            f.category
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&bucket)
            .execute(&mut *tx)
            .await
            .map_err(|e| ToolError::Database(format!("atlas findings store bucket lock: {e}")))?;

        let matched_id = match &embedding {
            Some(candidate) => {
                let existing: Vec<(Uuid, Option<pgvector::Vector>)> = sqlx::query_as(
                    SELECT_BUCKET_SQL,
                )
                .bind(&f.project_id)
                .bind(f.scope_kind.as_str())
                .bind(&f.scope_ref)
                .bind(&f.category)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| {
                    ToolError::Database(format!("atlas findings store select bucket: {e}"))
                })?;

                let existing: Vec<(Uuid, Vec<f32>)> = existing
                    .into_iter()
                    .filter_map(|(id, v)| v.map(|v| (id, v.to_vec())))
                    .collect();

                // Primary: embedding-similarity match among rows that HAVE a
                // stored embedding (SELECT_BUCKET_SQL filters `embedding IS NOT
                // NULL`). EMBED-02 fallback: if none matched, still fall back to
                // an exact-description match — `SELECT_BUCKET_SQL` deliberately
                // skips NULL-embedding rows, so after the 768->1024 migration
                // reset existing `kg_findings.embedding` to NULL, a repeated
                // exact finding would otherwise DUPLICATE against the migrated
                // (NULL-embedding) row until it's re-embedded. This makes the
                // healthy `Some(embedding)` path dedup exact recurrences even
                // when the stored row's embedding is NULL (or when embedding
                // generation is transiently unavailable), matching the
                // exact-text path the `None` branch already uses.
                match dedup_decision(candidate, &existing, threshold) {
                    Some(id) => Some(id),
                    None => exact_description_match(&mut tx, &f).await?,
                }
            }
            None => exact_description_match(&mut tx, &f).await?,
        };

        let outcome = if let Some(id) = matched_id {
            // Fetch current provenance to merge, then cap to the last N entries.
            let current: (JsonValue,) =
                sqlx::query_as("SELECT provenance FROM kg_findings WHERE id = $1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| {
                        ToolError::Database(format!(
                            "atlas findings store fetch provenance: {e}"
                        ))
                    })?;

            let merged = merge_provenance(current.0, f.provenance);

            let (occurrences,): (i32,) = sqlx::query_as(BUMP_SQL)
                .bind(id)
                .bind(&merged)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| ToolError::Database(format!("atlas findings store bump: {e}")))?;

            RecordOutcome::Recurred { id, occurrences }
        } else {
            let id = Uuid::new_v4();
            let vector = embedding.map(pgvector::Vector::from);
            let provenance = merge_provenance(JsonValue::Array(Vec::new()), f.provenance);

            sqlx::query(INSERT_SQL)
                .bind(id)
                .bind(&f.project_id)
                .bind(&f.category)
                .bind(&f.severity)
                .bind(f.scope_kind.as_str())
                .bind(&f.scope_ref)
                .bind(&f.description)
                .bind(vector)
                .bind(&provenance)
                .execute(&mut *tx)
                .await
                .map_err(|e| ToolError::Database(format!("atlas findings store insert: {e}")))?;

            RecordOutcome::Created(id)
        };

        tx.commit()
            .await
            .map_err(|e| ToolError::Database(format!("atlas findings store commit: {e}")))?;

        Ok(outcome)
    }

    /// List findings for a project, optionally filtered by scope kind,
    /// category, and a minimum occurrence count. Ordered by
    /// `occurrences DESC, last_seen DESC`. Parameterized throughout — the
    /// WHERE clause is built dynamically but every value is bound, never
    /// interpolated.
    pub async fn list(
        &self,
        project_id: &str,
        scope_kind: Option<&str>,
        category: Option<&str>,
        min_occurrences: Option<i32>,
    ) -> Result<Vec<FindingRow>, ToolError> {
        let mut sql = String::from(
            "SELECT id, project_id, category, severity, scope_kind, scope_ref, description, \
             provenance, first_seen, last_seen, occurrences, crystallize_state \
             FROM kg_findings WHERE project_id = $1",
        );

        let mut idx = 1;
        if scope_kind.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND scope_kind = ${idx}"));
        }
        if category.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND category = ${idx}"));
        }
        if min_occurrences.is_some() {
            idx += 1;
            sql.push_str(&format!(" AND occurrences >= ${idx}"));
        }
        sql.push_str(" ORDER BY occurrences DESC, last_seen DESC");

        let mut query = sqlx::query(&sql).bind(project_id.to_string());
        if let Some(sk) = scope_kind {
            query = query.bind(sk.to_string());
        }
        if let Some(c) = category {
            query = query.bind(c.to_string());
        }
        if let Some(m) = min_occurrences {
            query = query.bind(m);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas findings store list: {e}")))?;

        rows.iter()
            .map(FindingRow::from_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::Database(format!("atlas findings store list decode: {e}")))
    }

    /// CXEG-09: record a finding's crystallization outcome (`"promoted"` or
    /// `"refuted"`) so `crate::cortex::crystallize`'s candidate selection
    /// never re-selects it — the convergence guarantee for the
    /// crystallization loop. `state` is validated here rather than via a DB
    /// CHECK constraint (see [`ADD_CRYSTALLIZE_STATE_COLUMN_SQL`]'s doc
    /// comment). A no-op success (not an error) if `id` doesn't match any
    /// row — mirrors this store's other best-effort update semantics.
    pub async fn mark_crystallize_state(&self, id: Uuid, state: &str) -> Result<(), ToolError> {
        if state != "promoted" && state != "refuted" {
            return Err(ToolError::InvalidArgument(format!(
                "crystallize_state must be 'promoted' or 'refuted', got '{state}'"
            )));
        }
        sqlx::query(MARK_CRYSTALLIZE_STATE_SQL)
            .bind(id)
            .bind(state)
            .execute(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas findings store mark crystallize state: {e}")))?;
        Ok(())
    }
}

/// Exact-description dedup lookup within a bucket: the id of an existing
/// `kg_findings` row with the same `(project_id, scope_kind, scope_ref,
/// category, description)`, if any. Shared by both `record` paths — the
/// `None`-embedding branch (its only dedup signal) and, since EMBED-02, the
/// `Some(embedding)` branch's fallback when no embedding-similarity match is
/// found (so exact recurrences still dedup against NULL-embedding rows left
/// by the 768->1024 migration). Runs inside the caller's transaction so it
/// shares the bucket advisory lock.
async fn exact_description_match(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    f: &NewFinding,
) -> Result<Option<Uuid>, ToolError> {
    let row: Option<(Uuid,)> = sqlx::query_as(SELECT_EXACT_SQL)
        .bind(&f.project_id)
        .bind(f.scope_kind.as_str())
        .bind(&f.scope_ref)
        .bind(&f.category)
        .bind(&f.description)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| ToolError::Database(format!("atlas findings store select exact: {e}")))?;
    Ok(row.map(|(id,)| id))
}

/// Merge a new provenance entry into an existing jsonb array, keeping only
/// the most recent [`MAX_PROVENANCE_ENTRIES`] entries. If `current` isn't a
/// JSON array (unexpected but defensive), it is treated as empty.
fn merge_provenance(current: JsonValue, new_entry: JsonValue) -> JsonValue {
    let mut arr = match current {
        JsonValue::Array(a) => a,
        _ => Vec::new(),
    };
    arr.push(new_entry);
    if arr.len() > MAX_PROVENANCE_ENTRIES {
        let drop = arr.len() - MAX_PROVENANCE_ENTRIES;
        arr.drain(0..drop);
    }
    JsonValue::Array(arr)
}

/// Resolve the dedup similarity threshold from `KGFIND_DEDUP_THRESHOLD`,
/// falling back to [`DEFAULT_DEDUP_THRESHOLD`] when unset or unparsable.
fn dedup_threshold() -> f32 {
    std::env::var("KGFIND_DEDUP_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(DEFAULT_DEDUP_THRESHOLD)
}

/// Pure dedup decision: given a candidate embedding and the existing
/// embeddings in the bucket, return the id of the nearest existing row if
/// its cosine similarity is `>= threshold`, else `None`. No I/O, fully
/// unit-testable.
pub fn dedup_decision(
    candidate: &[f32],
    existing: &[(Uuid, Vec<f32>)],
    threshold: f32,
) -> Option<Uuid> {
    let mut best: Option<(Uuid, f32)> = None;
    for (id, vec) in existing {
        let sim = cosine_similarity(candidate, vec);
        if best.map(|(_, best_sim)| sim > best_sim).unwrap_or(true) {
            best = Some((*id, sim));
        }
    }
    match best {
        Some((id, sim)) if sim >= threshold => Some(id),
        _ => None,
    }
}

/// Cosine similarity between two vectors. Returns `0.0` if either vector is
/// empty or zero-magnitude (rather than dividing by zero / NaN).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Idempotent, advisory-locked migration: `kg_findings` table, its bucket
/// index, and a best-effort HNSW cosine index on the embedding column.
/// Mirrors `vec_store::migrate` exactly (same connection-discard-on-drop and
/// advisory-lock idiom, distinct lock key).
async fn migrate(pool: &PgPool) -> Result<(), ToolError> {
    let mut conn = pool.acquire().await.map_err(|e| {
        ToolError::Database(format!(
            "acquire dedicated connection for atlas findings store migrate: {e}"
        ))
    })?;

    conn.close_on_drop();

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            ToolError::Database(format!(
                "acquire atlas findings store migrate advisory lock: {e}"
            ))
        })?;

    let result = migrate_locked(&mut conn).await;

    if let Err(unlock_err) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            "atlas findings store migrate: failed to release advisory lock {}: {unlock_err} \
             (harmless — the lock is released automatically when this connection closes)",
            ADVISORY_LOCK_KEY
        );
    }

    result
}

async fn migrate_locked(conn: &mut sqlx::PgConnection) -> Result<(), ToolError> {
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create vector extension: {e}")))?;

    // EMBED-02: an existing `kg_findings` table from before the 768 -> 1024
    // dim change (Qwen3-Embedding via Chord's `/v1/embeddings` proxy) has
    // `embedding` values that are invalid against the new model — unlike
    // `vec_store::AtlasVecStore`'s single-purpose table, `kg_findings` carries
    // durable non-embedding data (description, provenance, occurrences) that
    // must survive the migration, so this drops+recreates just the nullable
    // `embedding` column (existing rows keep everything else, embedding reset
    // to NULL) rather than the whole table. Ops re-runs the KG build
    // separately to repopulate embeddings at the new dimension; dedup by
    // exact-text match (the `embedding = None` path in `record`) still works
    // in the meantime.
    drop_embedding_column_if_dim_mismatch(conn, FINDINGS_EMBED_DIM as i32).await?;

    sqlx::query(CREATE_TABLE_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create kg_findings table: {e}")))?;

    // CXEG-09: idempotent upgrade for a table that pre-dates crystallize_state
    // (CREATE TABLE IF NOT EXISTS above is a no-op on an existing table).
    sqlx::query(ADD_CRYSTALLIZE_STATE_COLUMN_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("add kg_findings.crystallize_state column: {e}")))?;

    sqlx::query(CREATE_INDEX_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create kg_findings_bucket index: {e}")))?;

    // Best-effort: some pgvector builds may not ship the hnsw access method
    // (or may reject index creation under certain server configs). The table
    // itself is fully functional without it, just without the ANN speedup.
    if let Err(e) = sqlx::query(CREATE_HNSW_INDEX_SQL).execute(&mut *conn).await {
        tracing::warn!(
            "atlas findings store migrate: hnsw index creation failed (best-effort, \
             continuing without ANN index): {e}"
        );
    }

    Ok(())
}

/// EMBED-02: idempotent dim-migration step for `kg_findings.embedding`. If
/// the table already exists with an `embedding` column whose pgvector
/// dimension (`pg_attribute.atttypmod`, which pgvector stores as the raw
/// dimension, not offset) doesn't match `expected_dim`, the column is dropped
/// and re-added (nullable, no default — same shape as the original) so the
/// `CREATE TABLE IF NOT EXISTS` that follows is a no-op and the column ends
/// up at the new dimension. A no-op when the table doesn't exist yet
/// (`to_regclass` is `NULL`) or already matches.
async fn drop_embedding_column_if_dim_mismatch(
    conn: &mut sqlx::PgConnection,
    expected_dim: i32,
) -> Result<(), ToolError> {
    let existing: Option<(i32,)> = sqlx::query_as(
        "SELECT atttypmod FROM pg_attribute \
         WHERE attrelid = to_regclass('kg_findings') AND attname = 'embedding' \
         AND NOT attisdropped",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| ToolError::Database(format!("kg_findings embedding dim check: {e}")))?;

    if let Some((typmod,)) = existing {
        if typmod != expected_dim {
            tracing::warn!(
                "kg_findings: existing embedding column is vector({typmod}), expected \
                 vector({expected_dim}) — dropping and re-adding the column at the new \
                 dimension (EMBED-02); other finding fields are preserved, ops re-runs \
                 the KG build to repopulate embeddings"
            );
            sqlx::query("ALTER TABLE kg_findings DROP COLUMN embedding")
                .execute(&mut *conn)
                .await
                .map_err(|e| {
                    ToolError::Database(format!("drop stale kg_findings.embedding: {e}"))
                })?;
            sqlx::query("ALTER TABLE kg_findings ADD COLUMN embedding vector(1024)")
                .execute(&mut *conn)
                .await
                .map_err(|e| {
                    ToolError::Database(format!("re-add kg_findings.embedding: {e}"))
                })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn vec3(x: f32, y: f32, z: f32) -> Vec<f32> {
        vec![x, y, z]
    }

    #[test]
    fn test_findings_embed_dim_is_1024() {
        assert_eq!(FINDINGS_EMBED_DIM, 1024);
    }

    #[test]
    fn test_scope_kind_str_roundtrip() {
        for k in [
            ScopeKind::Node,
            ScopeKind::Path,
            ScopeKind::Community,
            ScopeKind::Global,
        ] {
            assert_eq!(ScopeKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ScopeKind::parse("bogus"), None);
    }

    #[test]
    fn test_cosine_similarity_identical_is_one() {
        let a = vec3(1.0, 2.0, 3.0);
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal_is_zero() {
        let a = vec3(1.0, 0.0, 0.0);
        let b = vec3(0.0, 1.0, 0.0);
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty_is_zero() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn test_dedup_decision_near_match_returns_id() {
        let candidate = vec3(1.0, 0.0, 0.0);
        let close_id = Uuid::new_v4();
        let far_id = Uuid::new_v4();
        let existing = vec![
            (far_id, vec3(0.0, 1.0, 0.0)),
            (close_id, vec3(0.999, 0.001, 0.0)),
        ];
        let result = dedup_decision(&candidate, &existing, 0.9);
        assert_eq!(result, Some(close_id));
    }

    #[test]
    fn test_dedup_decision_all_far_returns_none() {
        let candidate = vec3(1.0, 0.0, 0.0);
        let existing = vec![
            (Uuid::new_v4(), vec3(0.0, 1.0, 0.0)),
            (Uuid::new_v4(), vec3(0.0, 0.0, 1.0)),
        ];
        assert_eq!(dedup_decision(&candidate, &existing, 0.9), None);
    }

    #[test]
    fn test_dedup_decision_exact_boundary_is_recurrence() {
        let candidate = vec3(1.0, 0.0, 0.0);
        let id = Uuid::new_v4();
        let existing = vec![(id, vec3(1.0, 0.0, 0.0))]; // similarity == 1.0
        assert_eq!(dedup_decision(&candidate, &existing, 1.0), Some(id));
    }

    #[test]
    fn test_dedup_decision_empty_existing_returns_none() {
        let candidate = vec3(1.0, 0.0, 0.0);
        assert_eq!(dedup_decision(&candidate, &[], 0.9), None);
    }

    #[test]
    fn test_merge_provenance_appends() {
        let current = JsonValue::Array(vec![JsonValue::String("a".into())]);
        let merged = merge_provenance(current, JsonValue::String("b".into()));
        assert_eq!(
            merged,
            JsonValue::Array(vec![
                JsonValue::String("a".into()),
                JsonValue::String("b".into())
            ])
        );
    }

    #[test]
    fn test_merge_provenance_caps_at_max_entries() {
        let mut entries: Vec<JsonValue> = (0..MAX_PROVENANCE_ENTRIES)
            .map(|i| JsonValue::Number(i.into()))
            .collect();
        let current = JsonValue::Array(entries.clone());
        let merged = merge_provenance(current, JsonValue::Number(9999.into()));
        let JsonValue::Array(arr) = merged else {
            panic!("expected array");
        };
        assert_eq!(arr.len(), MAX_PROVENANCE_ENTRIES);
        // oldest entry (0) dropped, newest (9999) present at the end
        entries.remove(0);
        entries.push(JsonValue::Number(9999.into()));
        assert_eq!(arr, entries);
    }

    #[test]
    fn test_merge_provenance_non_array_current_treated_as_empty() {
        let merged = merge_provenance(JsonValue::Null, JsonValue::String("first".into()));
        assert_eq!(merged, JsonValue::Array(vec![JsonValue::String("first".into())]));
    }

    #[test]
    fn test_dedup_threshold_default() {
        if std::env::var("KGFIND_DEDUP_THRESHOLD").is_ok() {
            return;
        }
        assert_eq!(dedup_threshold(), DEFAULT_DEDUP_THRESHOLD);
    }

    #[test]
    fn test_migration_sql_contains_vector_1024() {
        assert!(CREATE_TABLE_SQL.contains("vector(1024)"));
    }

    #[test]
    fn test_migration_sql_contains_kg_findings() {
        assert!(CREATE_TABLE_SQL.contains("kg_findings"));
        assert!(CREATE_INDEX_SQL.contains("kg_findings"));
    }

    #[test]
    fn test_migration_sql_contains_occurrences() {
        assert!(CREATE_TABLE_SQL.contains("occurrences"));
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_not_configured_without_env() {
        // Mirrors vec_store's shape: if a real DSN happens to be configured
        // in this process, skip gracefully (never attempt a live connection
        // from a unit test) rather than mutating global env state.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // skip — a real DSN is available, not testing NotConfigured
        }

        let result = FindingsStore::from_env().await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    /// EMBED-02 regression: after the 768->1024 migration resets existing
    /// `kg_findings.embedding` to NULL, a repeated exact finding recorded on
    /// the healthy `Some(embedding)` path must still dedup against the
    /// migrated NULL-embedding row (via the exact-description fallback), not
    /// insert a duplicate. Requires a live Atlas DB; skips cleanly when
    /// `ATLAS_DATABASE_URL` is unset, mirroring the DSN-gating convention the
    /// other DB-touching tests here use.
    #[tokio::test]
    #[serial]
    async fn record_with_embedding_dedups_against_null_embedding_row() {
        if std::env::var("ATLAS_DATABASE_URL").is_err() {
            return; // no DB available — integration assertion skipped
        }

        let store = FindingsStore::from_env().await.expect("store from_env");
        // Unique bucket per run so parallel/repeat runs never interfere.
        let project_id = format!("EMBED02-TEST-{}", Uuid::new_v4());

        let mk = || NewFinding {
            project_id: project_id.clone(),
            category: "lint".into(),
            severity: "low".into(),
            scope_kind: ScopeKind::Path,
            scope_ref: "src/lib.rs".into(),
            description: "unused import: std::io".into(),
            provenance: serde_json::json!({ "run": "embed02-test" }),
        };

        // Simulate the migrated state: an existing row whose embedding is NULL
        // (recording without an embedding leaves the column NULL, exactly as
        // the migration's DROP/re-add of the embedding column does).
        let created_id = match store.record(mk(), None).await.expect("first record") {
            RecordOutcome::Created(id) => id,
            other => panic!("expected Created on first record, got {other:?}"),
        };

        // Healthy path: same finding WITH an embedding. The NULL-embedding row
        // is invisible to the similarity search (SELECT_BUCKET_SQL filters
        // `embedding IS NOT NULL`), so this must dedup via the exact-description
        // fallback — bumping the existing row rather than inserting a duplicate.
        match store
            .record(mk(), Some(vec![0.1_f32; FINDINGS_EMBED_DIM]))
            .await
            .expect("second record")
        {
            RecordOutcome::Recurred { id, occurrences } => {
                assert_eq!(id, created_id, "must bump the existing NULL-embedding row");
                assert_eq!(occurrences, 2, "occurrences bumped, not a new row");
            }
            other => panic!("expected Recurred (exact-fallback dedup), got {other:?}"),
        }

        // And exactly one row exists for the bucket — no duplicate inserted.
        let rows = store
            .list(&project_id, None, None, None)
            .await
            .expect("list");
        assert_eq!(rows.len(), 1, "exactly one finding row — no duplicate");
    }
}
