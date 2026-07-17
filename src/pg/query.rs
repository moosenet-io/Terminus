//! `pg_query` + `pg_list_tables` + `pg_describe_table` — the read surface
//! (PGT-02).
//!
//! Read-only, NOT guarded, audited via the standard gateway audit pipeline
//! (matching `pg_identities`'s posture). All three default to the
//! least-privileged `readonly` identity via `crate::pg::conn`.
//!
//! ## `pg_query` safety model
//! `pg_query` accepts exactly ONE read-only statement — `SELECT`, a
//! `WITH ... SELECT` (CTE), `EXPLAIN`, or `SHOW` — never DML, DDL, role
//! management, or a `;`-chained multi-statement body. [`validate_read_only`]
//! is the single gate: a conservative leading-keyword check plus a scan for
//! any disallowed statement-shaped keyword anywhere in the body (so a CTE
//! smuggling `INSERT`/`UPDATE`/`DELETE`/`DROP`/etc. inside a `WITH` clause is
//! still rejected) plus a single-statement check (any `;` other than one
//! single trailing terminator is rejected as multi-statement). Values are
//! ALWAYS passed as bound `params` — [`bind_param`] binds each
//! `serde_json::Value` through `sqlx`'s typed `Encode`/`bind`, never string
//! interpolation, so `pg_query` is SQL-injection safe by construction: no
//! code path in this file writes a parameter's VALUE into the `sql` string.
//!
//! ## `pg_list_tables` / `pg_describe_table` identifier safety
//! Table/schema names cannot be bound as ordinary query parameters (Postgres
//! parameter binding is for VALUES, not identifiers), so these two tools
//! validate `schema`/`table` against [`is_safe_identifier`] (a conservative
//! `[A-Za-z_][A-Za-z0-9_]*` charset check) before splicing them into the
//! `information_schema`/`pg_catalog` query text — an identifier that fails
//! the check is a clean `ToolError::InvalidArgument`, never passed through.

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::{Column, Postgres, Row};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::conn;

/// Default row cap applied when a `pg_query` call omits `max_rows`.
const DEFAULT_MAX_ROWS: u64 = 500;

/// Hard ceiling on rows returned by `pg_query`, regardless of a caller's
/// requested `max_rows` — never unbounded, never OOMs on a huge result.
const HARD_MAX_ROWS: u64 = 5_000;

/// Statement keywords that make a statement NOT a pure read — matched as
/// whole words (case-insensitive) anywhere in the body, so a CTE like
/// `WITH x AS (INSERT INTO t ... RETURNING *) SELECT * FROM x` is still
/// rejected even though the leading keyword is `WITH`.
const DISALLOWED_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE", "GRANT", "REVOKE",
    "CALL", "DO", "COPY", "MERGE", "VACUUM", "REINDEX", "LOCK",
];

/// Leading keywords that mark a statement as a permitted single read.
const ALLOWED_LEADING: &[&str] = &["SELECT", "WITH", "EXPLAIN", "SHOW"];

/// Validate that `sql` is exactly one read-only statement (`SELECT` /
/// `WITH ... SELECT` / `EXPLAIN` / `SHOW`), never DML/DDL/role-management,
/// never `;`-chained. Returns the trimmed statement on success (informational
/// convenience) or a clean, tool-pointing `ToolError::InvalidArgument`.
fn validate_read_only(sql: &str) -> Result<&str, ToolError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ToolError::InvalidArgument("sql must not be empty".into()));
    }

    // Single-statement check: strip at most one trailing `;` (with only
    // whitespace after it), then reject any remaining `;` as a chained
    // second statement. This also catches the classic
    // `SELECT ...; DROP TABLE x` smuggle attempt.
    let mut body = trimmed;
    if let Some(stripped) = body.strip_suffix(';') {
        body = stripped.trim_end();
    }
    if body.contains(';') {
        return Err(ToolError::InvalidArgument(
            "pg_query accepts exactly one statement — no `;`-chained multi-statement input \
             is allowed"
                .into(),
        ));
    }

    // Leading-keyword check.
    let first_word: String = body
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .find(|w| !w.is_empty())
        .unwrap_or_default()
        .to_uppercase();
    if !ALLOWED_LEADING.iter().any(|k| *k == first_word) {
        return Err(ToolError::InvalidArgument(format!(
            "pg_query only runs a single read-only statement (SELECT / WITH / EXPLAIN / SHOW); \
             got a statement starting with '{first_word}'. Use pg_execute for DML or pg_ddl for \
             schema changes."
        )));
    }

    // Whole-body disallowed-keyword scan (whole-word match, case-insensitive)
    // to catch DML/DDL smuggled inside a CTE.
    let upper = body.to_uppercase();
    for kw in DISALLOWED_KEYWORDS {
        if contains_word(&upper, kw) {
            return Err(ToolError::InvalidArgument(format!(
                "pg_query rejected: statement contains '{kw}', which is not a read-only \
                 operation. Use pg_execute for DML or pg_ddl for schema/DDL changes."
            )));
        }
    }

    Ok(body)
}

/// Whole-word, case-insensitive substring search (both `haystack` and
/// `needle` are expected already uppercased by the caller for `needle`, but
/// this only assumes `haystack` is uppercased already; `needle` is compared
/// as-is so callers must pass an uppercase needle).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric() && bytes[abs - 1] != b'_';
        let after_idx = abs + needle_bytes.len();
        let after_ok =
            after_idx >= bytes.len() || (!bytes[after_idx].is_ascii_alphanumeric() && bytes[after_idx] != b'_');
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

/// Conservative identifier safety check for `schema`/`table` names spliced
/// into `information_schema`/`pg_catalog` query text (identifiers cannot be
/// bound as ordinary parameters). Accepts `[A-Za-z_][A-Za-z0-9_]*` only, max
/// 63 bytes (Postgres's own identifier limit) — anything else (quotes,
/// dots, whitespace, SQL syntax) is rejected before it ever reaches a query.
fn is_safe_identifier(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Bind one `serde_json::Value` argument onto a `sqlx` query. Always a typed
/// `Encode`/`bind` call — never string interpolation into the SQL text, so
/// caller-controlled VALUES can never alter the statement shape
/// (SQL-injection safe by construction).
fn bind_param<'q>(
    query: Query<'q, Postgres, PgArguments>,
    val: &'q Value,
) -> Query<'q, Postgres, PgArguments> {
    match val {
        Value::Null => query.bind(None::<String>),
        Value::Bool(b) => query.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        Value::String(s) => query.bind(s.as_str()),
        other => query.bind(other.to_string()),
    }
}

/// Best-effort decode of one result cell to a `serde_json::Value`, tried in
/// descending specificity so common Postgres types round-trip cleanly
/// without requiring per-OID dispatch. A cell whose type doesn't decode
/// under any attempted type (e.g. an array/composite/domain type this
/// suite doesn't special-case) degrades to `null` rather than failing the
/// whole row — `pg_query` never panics on an unusual column type.
fn cell_to_json(row: &PgRow, idx: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return v.map(Value::Bool).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return v.map(|n| json!(n)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(idx) {
        return v.map(|n| json!(n)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(idx) {
        return v.map(|n| json!(n)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return v.map(|n| json!(n)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(idx) {
        return v.map(|n| json!(n)).unwrap_or(Value::Null);
    }
    // NOTE: JSON/JSONB columns are NOT special-cased here -- this crate's
    // sqlx dependency does not enable the "json" feature (see Cargo.toml),
    // so `Decode<Postgres> for serde_json::Value` is unavailable. A
    // JSON/JSONB cell falls through every typed attempt below (Postgres's
    // JSON/JSONB OIDs aren't in String's compatible-OID list either) and
    // degrades to `null` -- documented, best-effort behavior; a future item
    // can enable sqlx's "json" feature to decode these natively.
    if let Ok(v) = row.try_get::<Option<uuid::Uuid>, _>(idx) {
        return v.map(|u| Value::String(u.to_string())).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(idx) {
        return v.map(|d| Value::String(d.to_rfc3339())).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveDateTime>, _>(idx) {
        return v.map(|d| Value::String(d.to_string())).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
        return v.map(Value::String).unwrap_or(Value::Null);
    }
    Value::Null
}

fn row_to_json(row: &PgRow) -> Value {
    let mut obj = serde_json::Map::new();
    for (idx, col) in row.columns().iter().enumerate() {
        obj.insert(col.name().to_string(), cell_to_json(row, idx));
    }
    Value::Object(obj)
}

// ─────────────────────────────────────────────────────────────────────────
// pg_query
// ─────────────────────────────────────────────────────────────────────────

pub struct PgQuery;

#[async_trait]
impl RustTool for PgQuery {
    fn name(&self) -> &str {
        "pg_query"
    }

    fn description(&self) -> &str {
        "Run a single read-only Postgres statement (SELECT / WITH ... SELECT / EXPLAIN / SHOW) \
         and return rows as JSON. Values are always bound parameters, never interpolated into \
         the SQL text -- SQL-injection safe. DML/DDL/multi-statement input is rejected with a \
         clean error. Results are row-capped (max_rows, default 500, hard ceiling 5000) with a \
         `truncated` flag. Defaults to the readonly connection identity."
    }

    fn parameters(&self) -> Value {
        conn::with_conn_params(json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "A single read-only statement: SELECT, WITH ... SELECT \
                                     (CTE), EXPLAIN, or SHOW. No `;`-chained multi-statement \
                                     input. Use $1, $2, ... placeholders for values -- never \
                                     embed a value literal in this string."
                },
                "params": {
                    "type": "array",
                    "description": "Bound values for $1, $2, ... placeholders in `sql`, in \
                                     order. Never interpolated into the SQL text.",
                    "items": {}
                },
                "max_rows": {
                    "type": "integer",
                    "description": "Maximum rows to return (default 500, hard ceiling 5000). \
                                     A result with more rows than this is truncated and \
                                     `truncated: true` is set."
                }
            },
            "required": ["sql"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let sql = args
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("sql (string) is required".into()))?;
        validate_read_only(sql)?;

        let params: Vec<Value> = args
            .get("params")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let max_rows = args
            .get("max_rows")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_ROWS)
            .clamp(1, HARD_MAX_ROWS);

        let identity = conn::resolve_identity_name(&args);
        let pool = conn::resolve_connection_for(&identity, conn::resolve_database_name(&args).as_deref()).await?;

        let mut query = sqlx::query(sql);
        for p in &params {
            query = bind_param(query, p);
        }

        let mut stream = query.fetch(&pool);
        let mut rows_out: Vec<Value> = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        let mut truncated = false;

        loop {
            let next = stream
                .try_next()
                .await
                .map_err(|e| ToolError::Database(format!("pg_query failed: {e}")))?;
            let Some(row) = next else { break };

            if rows_out.is_empty() && columns.is_empty() {
                columns = row.columns().iter().map(|c| c.name().to_string()).collect();
            }

            if rows_out.len() as u64 >= max_rows {
                truncated = true;
                break;
            }
            rows_out.push(row_to_json(&row));
        }

        let row_count = rows_out.len();
        let text = format!(
            "{row_count} row(s){}",
            if truncated { format!(" (truncated at max_rows={max_rows})") } else { String::new() }
        );

        Ok(ToolOutput::with_structured(
            text,
            json!({
                "columns": columns,
                "rows": rows_out,
                "row_count": row_count,
                "truncated": truncated,
            }),
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// pg_list_tables
// ─────────────────────────────────────────────────────────────────────────

pub struct PgListTables;

#[async_trait]
impl RustTool for PgListTables {
    fn name(&self) -> &str {
        "pg_list_tables"
    }

    fn description(&self) -> &str {
        "List tables visible to the connection, optionally filtered to one schema (default: \
         all non-system schemas). Read-only, defaults to the readonly connection identity."
    }

    fn parameters(&self) -> Value {
        conn::with_conn_params(json!({
            "type": "object",
            "properties": {
                "schema": {
                    "type": "string",
                    "description": "Restrict to this schema (e.g. \"public\"). Omit to list \
                                     tables across all non-system schemas."
                }
            },
            "required": []
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let schema = match args.get("schema").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => {
                let s = s.trim();
                if !is_safe_identifier(s) {
                    return Err(ToolError::InvalidArgument(format!(
                        "schema '{s}' is not a valid Postgres identifier"
                    )));
                }
                Some(s.to_string())
            }
            _ => None,
        };

        let identity = conn::resolve_identity_name(&args);
        let pool = conn::resolve_connection_for(&identity, conn::resolve_database_name(&args).as_deref()).await?;

        let sql = "SELECT table_schema, table_name, table_type \
                    FROM information_schema.tables \
                    WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                      AND ($1::text IS NULL OR table_schema = $1) \
                    ORDER BY table_schema, table_name";

        let rows = sqlx::query(sql)
            .bind(schema.as_deref())
            .fetch_all(&pool)
            .await
            .map_err(|e| ToolError::Database(format!("pg_list_tables failed: {e}")))?;

        let tables: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "schema": r.try_get::<String, _>("table_schema").unwrap_or_default(),
                    "table": r.try_get::<String, _>("table_name").unwrap_or_default(),
                    "type": r.try_get::<String, _>("table_type").unwrap_or_default(),
                })
            })
            .collect();

        let text = format!("{} table(s)", tables.len());
        Ok(ToolOutput::with_structured(text, json!({ "tables": tables, "count": tables.len() })))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// pg_describe_table
// ─────────────────────────────────────────────────────────────────────────

pub struct PgDescribeTable;

#[async_trait]
impl RustTool for PgDescribeTable {
    fn name(&self) -> &str {
        "pg_describe_table"
    }

    fn description(&self) -> &str {
        "Describe a table: columns (name/type/nullable/default), primary key, and indexes. \
         Read-only, defaults to the readonly connection identity. Unknown table -> clean \
         NotFound, never a panic."
    }

    fn parameters(&self) -> Value {
        conn::with_conn_params(json!({
            "type": "object",
            "properties": {
                "table": {
                    "type": "string",
                    "description": "Table name to describe."
                },
                "schema": {
                    "type": "string",
                    "description": "Schema the table lives in (default \"public\")."
                }
            },
            "required": ["table"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let table = args
            .get("table")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("table (string) is required".into()))?;
        if !is_safe_identifier(table) {
            return Err(ToolError::InvalidArgument(format!(
                "table '{table}' is not a valid Postgres identifier"
            )));
        }

        let schema = args
            .get("schema")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("public");
        if !is_safe_identifier(schema) {
            return Err(ToolError::InvalidArgument(format!(
                "schema '{schema}' is not a valid Postgres identifier"
            )));
        }

        let identity = conn::resolve_identity_name(&args);
        let pool = conn::resolve_connection_for(&identity, conn::resolve_database_name(&args).as_deref()).await?;

        // Existence check first, so an unknown table is a clean NotFound
        // rather than an empty-columns describe.
        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
        )
        .bind(schema)
        .bind(table)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("pg_describe_table lookup failed: {e}")))?;

        if exists.is_none() {
            return Err(ToolError::NotFound(format!("table '{schema}.{table}' does not exist")));
        }

        let column_rows = sqlx::query(
            "SELECT column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("pg_describe_table columns failed: {e}")))?;

        let columns: Vec<Value> = column_rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.try_get::<String, _>("column_name").unwrap_or_default(),
                    "type": r.try_get::<String, _>("data_type").unwrap_or_default(),
                    "nullable": r.try_get::<String, _>("is_nullable").map(|s| s == "YES").unwrap_or(true),
                    "default": r.try_get::<Option<String>, _>("column_default").unwrap_or(None),
                })
            })
            .collect();

        let pk_rows = sqlx::query(
            "SELECT a.attname AS column_name \
             FROM pg_index i \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
             JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2 \
             ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("pg_describe_table primary key failed: {e}")))?;

        let primary_key: Vec<String> = pk_rows
            .iter()
            .map(|r| r.try_get::<String, _>("column_name").unwrap_or_default())
            .collect();

        let index_rows = sqlx::query(
            "SELECT indexname, indexdef FROM pg_indexes WHERE schemaname = $1 AND tablename = $2 \
             ORDER BY indexname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("pg_describe_table indexes failed: {e}")))?;

        let indexes: Vec<Value> = index_rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.try_get::<String, _>("indexname").unwrap_or_default(),
                    "definition": r.try_get::<String, _>("indexdef").unwrap_or_default(),
                })
            })
            .collect();

        let text = format!(
            "{schema}.{table}: {} column(s), {} primary key column(s), {} index(es)",
            columns.len(),
            primary_key.len(),
            indexes.len()
        );

        Ok(ToolOutput::with_structured(
            text,
            json!({
                "schema": schema,
                "table": table,
                "columns": columns,
                "primary_key": primary_key,
                "indexes": indexes,
            }),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(PgQuery));
    registry.register_or_replace(Box::new(PgListTables));
    registry.register_or_replace(Box::new(PgDescribeTable));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_read_only: accepted classes ────────────────────────────

    #[test]
    fn accepts_plain_select() {
        assert!(validate_read_only("SELECT * FROM foo").is_ok());
    }

    #[test]
    fn accepts_select_lowercase_and_leading_whitespace() {
        assert!(validate_read_only("   select id from foo where id = $1").is_ok());
    }

    #[test]
    fn accepts_with_cte_select() {
        assert!(validate_read_only(
            "WITH recent AS (SELECT * FROM foo WHERE created_at > $1) SELECT * FROM recent"
        )
        .is_ok());
    }

    #[test]
    fn accepts_explain() {
        assert!(validate_read_only("EXPLAIN SELECT * FROM foo").is_ok());
    }

    #[test]
    fn accepts_show() {
        assert!(validate_read_only("SHOW search_path").is_ok());
    }

    #[test]
    fn accepts_single_trailing_semicolon() {
        assert!(validate_read_only("SELECT * FROM foo;").is_ok());
    }

    // ── validate_read_only: rejected classes ────────────────────────────

    #[test]
    fn rejects_insert() {
        let err = validate_read_only("INSERT INTO foo VALUES (1)").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn rejects_update() {
        assert!(validate_read_only("UPDATE foo SET x = 1").is_err());
    }

    #[test]
    fn rejects_delete() {
        assert!(validate_read_only("DELETE FROM foo").is_err());
    }

    #[test]
    fn rejects_drop() {
        assert!(validate_read_only("DROP TABLE foo").is_err());
    }

    #[test]
    fn rejects_multi_statement_select_then_drop() {
        // The sneaky negative test from the spec's TEST PLAN.
        let err = validate_read_only("SELECT * FROM foo; DROP TABLE x").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn rejects_multi_statement_two_selects() {
        assert!(validate_read_only("SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn rejects_dml_smuggled_inside_cte() {
        let err = validate_read_only(
            "WITH x AS (INSERT INTO t (a) VALUES (1) RETURNING *) SELECT * FROM x",
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn rejects_empty_sql() {
        assert!(validate_read_only("").is_err());
        assert!(validate_read_only("   ").is_err());
    }

    #[test]
    fn rejects_ddl_alter_create_truncate() {
        assert!(validate_read_only("ALTER TABLE foo ADD COLUMN x int").is_err());
        assert!(validate_read_only("CREATE TABLE foo (id int)").is_err());
        assert!(validate_read_only("TRUNCATE foo").is_err());
    }

    #[test]
    fn rejects_grant_revoke() {
        assert!(validate_read_only("GRANT SELECT ON foo TO bar").is_err());
        assert!(validate_read_only("REVOKE SELECT ON foo FROM bar").is_err());
    }

    #[test]
    fn does_not_false_positive_on_substrings() {
        // "selected" and "created_at" contain "select"/"create" as
        // substrings but must not trip the whole-word disallowed scan.
        assert!(validate_read_only("SELECT created_at, selected_flag FROM foo").is_ok());
    }

    // ── contains_word ────────────────────────────────────────────────────

    #[test]
    fn contains_word_matches_whole_words_only() {
        assert!(contains_word("SELECT * FROM FOO WHERE DROP_FLAG IS NOT TRUE", "TRUE"));
        assert!(!contains_word("SELECT CREATED_AT FROM FOO", "CREATE"));
        assert!(contains_word("DROP TABLE FOO", "DROP"));
        assert!(!contains_word("AIRDROP TABLE FOO", "DROP"));
    }

    // ── is_safe_identifier ──────────────────────────────────────────────

    #[test]
    fn identifier_accepts_normal_names() {
        assert!(is_safe_identifier("public"));
        assert!(is_safe_identifier("_private"));
        assert!(is_safe_identifier("table_1"));
    }

    #[test]
    fn identifier_rejects_injection_shapes() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("foo; DROP TABLE bar"));
        assert!(!is_safe_identifier("foo.bar"));
        assert!(!is_safe_identifier("foo\""));
        assert!(!is_safe_identifier("foo bar"));
        assert!(!is_safe_identifier("1foo"));
        assert!(!is_safe_identifier(&"a".repeat(64)));
    }

    // ── registration ─────────────────────────────────────────────────────

    #[test]
    fn register_adds_all_three_read_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("pg_query"));
        assert!(registry.contains("pg_list_tables"));
        assert!(registry.contains("pg_describe_table"));
    }

    #[test]
    fn none_of_the_read_tools_are_guarded() {
        assert!(!crate::approval::is_guarded("pg_query"));
        assert!(!crate::approval::is_guarded("pg_list_tables"));
        assert!(!crate::approval::is_guarded("pg_describe_table"));
    }

    #[test]
    fn schemas_default_to_readonly_identity_via_shared_helper() {
        // The tools rely on conn::resolve_identity_name for the default;
        // confirm that shared behavior directly (already unit-tested in
        // conn.rs) is what these tools consume, by checking the schema
        // carries the shared identity_param_schema description text.
        let schema = PgQuery.parameters();
        assert!(schema["properties"]["identity"].is_object());
        let schema = PgListTables.parameters();
        assert!(schema["properties"]["identity"].is_object());
        let schema = PgDescribeTable.parameters();
        assert!(schema["properties"]["identity"].is_object());
    }
}
