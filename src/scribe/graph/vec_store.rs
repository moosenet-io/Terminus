//! KGEMB-01: Atlas KG semantic-embeddings vector store.
//!
//! Owns the `kg_embeddings` pgvector table and all its queries: a bounded
//! `PgPool` sourced from `ATLAS_DATABASE_URL` (falling back to the shared
//! `DATABASE_URL`, mirroring `config::intake_database_url`), an idempotent
//! advisory-locked migration (extension + table + HNSW index), and typed
//! upsert/delete/top-K/hash-diff methods. Every entry point returns
//! `ToolError::NotConfigured` cleanly when the DSN is unset so callers
//! (KGEMB-03's build wiring, KGEMB-04's search tool) can degrade to the
//! existing lexical `kg_search` path without failing a build or a query.
//!
//! Reuse templates this mirrors:
//! - bounded pool shape: `src/vector/mod.rs` (`PgPoolOptions::new().max_connections(..)`)
//! - advisory-locked idempotent migration idiom: `src/intake/assistant/schema.rs`

use std::collections::HashMap;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::error::ToolError;

/// Embedding dimension for `kg_embeddings.embedding` (`vector(768)`). Fixed by
/// the default embeddings model (`nomic-embed-text`, 768-dim); a future model
/// swap that changes dimension needs its own migration, not a const bump.
pub const KG_EMBED_DIM: usize = 768;

/// Fixed advisory-lock key for the `kg_embeddings` migration. Distinct from
/// other modules' keys (e.g. `src/intake/assistant/schema.rs`'s
/// `ADVISORY_LOCK_KEY`) so concurrent migrations across subsystems never
/// contend on the same lock.
const ADVISORY_LOCK_KEY: i64 = 4_812_775_209_331_884_411;

const CREATE_EXTENSION_SQL: &str = "CREATE EXTENSION IF NOT EXISTS vector";

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS kg_embeddings ( \
    project_id text NOT NULL, \
    node_id text NOT NULL, \
    model text NOT NULL, \
    dim int NOT NULL, \
    embedding vector(768) NOT NULL, \
    card_hash text NOT NULL, \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (project_id, node_id) \
)";

const CREATE_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS kg_embeddings_hnsw \
    ON kg_embeddings USING hnsw (embedding vector_cosine_ops)";

const UPSERT_SQL: &str = "INSERT INTO kg_embeddings \
    (project_id, node_id, model, dim, embedding, card_hash, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, now()) \
    ON CONFLICT (project_id, node_id) DO UPDATE SET \
        embedding = EXCLUDED.embedding, \
        card_hash = EXCLUDED.card_hash, \
        model = EXCLUDED.model, \
        dim = EXCLUDED.dim, \
        updated_at = now()";

const DELETE_SQL: &str =
    "DELETE FROM kg_embeddings WHERE project_id = $1 AND node_id = ANY($2)";

const EXISTING_HASHES_SQL: &str =
    "SELECT node_id, card_hash FROM kg_embeddings WHERE project_id = $1";

const QUERY_TOPK_SQL: &str = "SELECT node_id, 1 - (embedding <=> $1) AS score \
    FROM kg_embeddings WHERE project_id = $2 \
    ORDER BY embedding <=> $1 LIMIT $3";

/// Owns the `kg_embeddings` pgvector table and its pool.
pub struct AtlasVecStore {
    pool: PgPool,
}

impl AtlasVecStore {
    /// Resolve the DSN via `config::atlas_database_url()`, build a bounded
    /// pool, and run the idempotent migration. Returns `NotConfigured` (never
    /// attempting a connect) when no DSN is set.
    pub async fn from_env() -> Result<Self, ToolError> {
        let url = crate::config::atlas_database_url().ok_or_else(|| {
            ToolError::NotConfigured("ATLAS_DATABASE_URL (or DATABASE_URL) not set".into())
        })?;

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| ToolError::Database(format!("connect atlas vector store: {e}")))?;

        migrate(&pool).await?;

        Ok(Self { pool })
    }

    /// Batch upsert. Each row is `(node_id, card_hash, model, embedding)`.
    /// Empty `rows` is a no-op returning `Ok(0)`. Parameterized; no string
    /// interpolation of values.
    pub async fn upsert(
        &self,
        project_id: &str,
        rows: &[(String, String, String, Vec<f32>)],
    ) -> Result<u64, ToolError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await.map_err(|e| {
            ToolError::Database(format!("begin atlas vector store upsert tx: {e}"))
        })?;

        let mut affected: u64 = 0;
        for (node_id, card_hash, model, embedding) in rows {
            let dim = embedding.len() as i32;
            let vector = pgvector::Vector::from(embedding.clone());
            let result = sqlx::query(UPSERT_SQL)
                .bind(project_id)
                .bind(node_id)
                .bind(model)
                .bind(dim)
                .bind(vector)
                .bind(card_hash)
                .execute(&mut *tx)
                .await
                .map_err(|e| ToolError::Database(format!("atlas vector store upsert: {e}")))?;
            affected += result.rows_affected();
        }

        tx.commit().await.map_err(|e| {
            ToolError::Database(format!("commit atlas vector store upsert tx: {e}"))
        })?;

        Ok(affected)
    }

    /// Delete rows by `node_id` for a project. Empty `node_ids` is a no-op
    /// returning `Ok(0)`.
    pub async fn delete(&self, project_id: &str, node_ids: &[String]) -> Result<u64, ToolError> {
        if node_ids.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query(DELETE_SQL)
            .bind(project_id)
            .bind(node_ids)
            .execute(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas vector store delete: {e}")))?;

        Ok(result.rows_affected())
    }

    /// Map of `node_id -> card_hash` for the current rows of a project, used
    /// for incremental hash-diff skip logic.
    pub async fn existing_hashes(
        &self,
        project_id: &str,
    ) -> Result<HashMap<String, String>, ToolError> {
        let rows: Vec<(String, String)> = sqlx::query_as(EXISTING_HASHES_SQL)
            .bind(project_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas vector store existing_hashes: {e}")))?;

        Ok(rows.into_iter().collect())
    }

    /// Top-K nearest node_ids by cosine similarity (`1 - cosine_distance`),
    /// descending.
    pub async fn query_topk(
        &self,
        project_id: &str,
        q: &[f32],
        k: i64,
    ) -> Result<Vec<(String, f32)>, ToolError> {
        let vector = pgvector::Vector::from(q.to_vec());
        let rows: Vec<(String, f32)> = sqlx::query_as(QUERY_TOPK_SQL)
            .bind(vector)
            .bind(project_id)
            .bind(k)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ToolError::Database(format!("atlas vector store query_topk: {e}")))?;

        Ok(rows)
    }
}

/// Idempotent, advisory-locked migration: `vector` extension, `kg_embeddings`
/// table, and its HNSW cosine index. Safe to call repeatedly and from
/// concurrent callers (serialized by the advisory lock).
async fn migrate(pool: &PgPool) -> Result<(), ToolError> {
    let mut conn = pool.acquire().await.map_err(|e| {
        ToolError::Database(format!(
            "acquire dedicated connection for atlas vector store migrate: {e}"
        ))
    })?;

    // Mirrors `src/intake/assistant/schema.rs::migrate`: mark the connection
    // to be discarded (never returned to the pool) on every exit path so a
    // future borrower of this exact physical connection can never silently
    // inherit an already-held session-scoped advisory lock.
    conn.close_on_drop();

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            ToolError::Database(format!("acquire atlas vector store migrate advisory lock: {e}"))
        })?;

    let result = migrate_locked(&mut conn).await;

    if let Err(unlock_err) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            "atlas vector store migrate: failed to release advisory lock {}: {unlock_err} \
             (harmless — the lock is released automatically when this connection closes)",
            ADVISORY_LOCK_KEY
        );
    }

    result
}

async fn migrate_locked(conn: &mut sqlx::PgConnection) -> Result<(), ToolError> {
    sqlx::query(CREATE_EXTENSION_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create vector extension: {e}")))?;

    sqlx::query(CREATE_TABLE_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| ToolError::Database(format!("create kg_embeddings table: {e}")))?;

    // Best-effort: some pgvector builds may not ship the hnsw access method
    // (or may reject index creation under certain server configs). The table
    // itself is fully functional (exact top-K via `ORDER BY <=>`) without the
    // index, just without the ANN speedup, so a failure here is logged and
    // swallowed rather than failing the whole migration.
    if let Err(e) = sqlx::query(CREATE_INDEX_SQL).execute(&mut *conn).await {
        tracing::warn!(
            "atlas vector store migrate: hnsw index creation failed (best-effort, \
             continuing without ANN index): {e}"
        );
    }

    Ok(())
}

/// Deterministic, stable hash of a card's text (FNV-1a 64-bit, hex-encoded).
/// Used to skip re-embedding nodes whose card is unchanged since the last
/// build. Not cryptographic — this is a change-detection digest, not a
/// security boundary, so a dependency-free hasher is preferred over pulling
/// in a crypto crate for it.
pub fn card_hash(card: &str) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in card.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_kg_embed_dim_is_768() {
        assert_eq!(KG_EMBED_DIM, 768);
    }

    #[test]
    fn test_card_hash_deterministic() {
        let a = card_hash("fn foo() in src/lib.rs");
        let b = card_hash("fn foo() in src/lib.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn test_card_hash_differs_on_different_input() {
        let a = card_hash("fn foo() in src/lib.rs");
        let b = card_hash("fn bar() in src/lib.rs");
        assert_ne!(a, b);
    }

    #[test]
    fn test_card_hash_is_hex() {
        let h = card_hash("some card text");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_migration_sql_contains_vector_768() {
        assert!(CREATE_TABLE_SQL.contains("vector(768)"));
    }

    #[test]
    fn test_migration_sql_contains_hnsw() {
        assert!(CREATE_INDEX_SQL.contains("hnsw"));
        assert!(CREATE_INDEX_SQL.contains("vector_cosine_ops"));
    }

    #[test]
    fn test_query_topk_sql_uses_cosine_distance_operator() {
        assert!(QUERY_TOPK_SQL.contains("<=>"));
    }

    #[test]
    fn test_upsert_sql_is_parameterized_on_conflict() {
        assert!(UPSERT_SQL.contains("ON CONFLICT (project_id, node_id) DO UPDATE"));
        assert!(UPSERT_SQL.contains('$'));
    }

    #[test]
    fn test_delete_sql_uses_any_array_param() {
        assert!(DELETE_SQL.contains("= ANY($2)"));
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_not_configured_without_env() {
        // Mirrors src/vector/mod.rs:812's shape: if a real DSN happens to be
        // configured in this process, skip gracefully (never attempt a live
        // connection from a unit test) rather than mutating global env state.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() || std::env::var("DATABASE_URL").is_ok() {
            return; // skip — a real DSN is available, not testing NotConfigured
        }

        let result = AtlasVecStore::from_env().await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }
}
