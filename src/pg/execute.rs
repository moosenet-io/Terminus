//! `pg_execute` — parameterized DML (PGT-03).
//!
//! Runs exactly one bound-parameter `INSERT`/`UPDATE`/`DELETE` (optionally
//! with `RETURNING`) against a configured `POSTGRES_URL_<NAME>` connection
//! identity (see `crate::pg::conn`). This is NOT a general SQL executor:
//! reads, DDL, role/privilege management, and multi-statement input are all
//! rejected up front with a clean [`ToolError::InvalidArgument`] pointing the
//! caller at the right tool (`pg_query`, `pg_ddl`, `pg_admin`) — `pg_execute`
//! is the DML door only.
//!
//! ## Destructive-shape detection
//! An unqualified `DELETE`/`UPDATE` (no `WHERE`) is treated as destructive:
//! it mutates or removes an entire table's rows in one call, the single
//! easiest way to silently mass-mutate data through this suite. The call
//! still RUNS (this module does not itself gate it — see "Guarding" below)
//! but the response always carries a `destructive: bool` flag so the
//! gateway audit trail and any guarding logic can see the shape without
//! re-parsing the SQL. [`is_destructive_shape`] is `pub` and intentionally
//! generic (it also recognizes bare `TRUNCATE`) so later `pg_*` items
//! (`pg_ddl`'s own `TRUNCATE`/`DROP` handling, PGT-06's guarding sweep) can
//! reuse the exact same pure-string classification rather than re-deriving
//! it — even though `pg_execute`'s own statement-class gate never lets a
//! `TRUNCATE` reach that flag (`TRUNCATE` is DDL-shaped here and is rejected
//! with a message pointing at `pg_ddl`, per S115's DML/DDL split).
//!
//! ## Guarding (PGT-06)
//! `pg_execute` is a GUARDED tool: it is listed in
//! `crate::approval::GUARDED_BARE_NAMES` (checked by the gateway,
//! `src/mcp_server.rs`, for federated/mesh dispatch) AND calls
//! [`crate::approval::gate`] itself at the top of `execute_structured`
//! (after the statement-class + destructive-shape checks, before any DB
//! connection is attempted) — matching the `pg_admin`/`openhands`/
//! `<secret-manager>` precedent: the tool owns its own gate call as the real
//! enforcement point; `GUARDED_BARE_NAMES` is a second, gateway-level
//! classification used for federated dispatch, not the only place the
//! approval requirement is enforced.
//!
//! ## Params — no `json` sqlx feature
//! This crate's `sqlx` dependency does not enable the `json` feature (see
//! `Cargo.toml`), so bound parameters are mapped from JSON to the small set
//! of primitive Postgres wire types `sqlx` can bind without it (text,
//! int8/f8, bool, and SQL `NULL`); a JSON array/object parameter is bound as
//! its JSON text form (best-effort — there is no vendored `jsonb` bind path
//! here). `RETURNING` rows are decoded through the same primitive set; a
//! column whose Postgres type isn't one of those decodes as a placeholder
//! string rather than failing the whole call. A fuller, richer row/parameter
//! codec is `pg_query`'s (PGT-02) concern, not duplicated here.

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::{Column, Postgres, Row};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::conn;

/// `pg_execute`'s own default connection identity: `writer`, NOT
/// `conn::DEFAULT_IDENTITY` ("readonly"). A DML call with no identity
/// specified authenticates as the writer role — the DB role, not this tool,
/// is the real privilege boundary (a `readonly` role is DB-enforced-incapable
/// of DML regardless of what this tool would allow).
pub const DEFAULT_EXECUTE_IDENTITY: &str = "writer";

/// The single DML statement classes `pg_execute` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementClass {
    Insert,
    Update,
    Delete,
}

impl StatementClass {
    fn as_str(self) -> &'static str {
        match self {
            StatementClass::Insert => "INSERT",
            StatementClass::Update => "UPDATE",
            StatementClass::Delete => "DELETE",
        }
    }
}

/// Collapse all whitespace runs to a single space and uppercase — the one
/// normalization every pure-string classifier in this module shares, so
/// keyword lookups (`WHERE`, `RETURNING`, the leading verb) are
/// whitespace/case insensitive without a SQL parser.
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase()
}

/// Does `normalized` (already [`normalize`]d) contain `word` as a standalone
/// token — not as a substring of a longer identifier (e.g. `WHEREVER` must
/// not match `WHERE`)? Tokenizes on any non-alphanumeric, non-underscore
/// byte, which is conservative enough for this suite's "pure string checks"
/// contract (it does not understand string literals/comments, matching the
/// spec's explicitly documented scope).
fn contains_word(normalized: &str, word: &str) -> bool {
    normalized.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|tok| tok == word)
}

/// Strip exactly one trailing `;` (plus surrounding whitespace), the only
/// semicolon `pg_execute` tolerates — a single terminator on an otherwise
/// single statement. Anything left containing a `;` after this is
/// multi-statement input.
fn strip_one_trailing_semicolon(s: &str) -> &str {
    let t = s.trim_end();
    t.strip_suffix(';').map(str::trim_end).unwrap_or(t)
}

fn is_multi_statement(sql: &str) -> bool {
    strip_one_trailing_semicolon(sql.trim()).contains(';')
}

/// Classify `sql` as a single accepted DML statement, or return a clean
/// [`ToolError::InvalidArgument`] naming the right tool for anything else.
/// Multi-statement input (an embedded `;`) is rejected before classification
/// even runs, so it can never be partially matched as "the first statement".
fn classify_dml(sql: &str) -> Result<StatementClass, ToolError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ToolError::InvalidArgument("sql must not be empty".to_string()));
    }
    if is_multi_statement(trimmed) {
        return Err(ToolError::InvalidArgument(
            "pg_execute accepts exactly one statement; multi-statement input (an embedded ';') \
             is rejected"
                .to_string(),
        ));
    }

    let normalized = normalize(trimmed);
    let first = normalized.split(' ').next().unwrap_or("");
    match first {
        "INSERT" => Ok(StatementClass::Insert),
        "UPDATE" => Ok(StatementClass::Update),
        "DELETE" => Ok(StatementClass::Delete),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "TABLE" => Err(ToolError::InvalidArgument(
            format!("pg_execute does not run read statements ('{first}') -- use pg_query instead"),
        )),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "COMMENT" | "VACUUM" | "REINDEX" => {
            Err(ToolError::InvalidArgument(format!(
                "pg_execute does not run schema/DDL statements ('{first}') -- use pg_ddl instead"
            )))
        }
        "GRANT" | "REVOKE" => Err(ToolError::InvalidArgument(format!(
            "pg_execute does not run role/privilege statements ('{first}') -- use pg_admin instead"
        ))),
        "" => Err(ToolError::InvalidArgument("sql must not be empty".to_string())),
        other => Err(ToolError::InvalidArgument(format!(
            "pg_execute only accepts INSERT/UPDATE/DELETE statements, got '{other}'"
        ))),
    }
}

/// Pure-string destructive-shape detector, reusable outside `pg_execute`
/// (see the module docs' "Destructive-shape detection" section): a bare
/// `TRUNCATE`, or a `DELETE`/`UPDATE` with no top-level `WHERE` token.
/// Anything else (including every `INSERT`, and any `WHERE`-qualified
/// `DELETE`/`UPDATE`) is not destructive by this definition.
pub fn is_destructive_shape(sql: &str) -> bool {
    let trimmed = strip_one_trailing_semicolon(sql.trim());
    if trimmed.is_empty() {
        return false;
    }
    let normalized = normalize(trimmed);
    let first = normalized.split(' ').next().unwrap_or("");
    match first {
        "TRUNCATE" => true,
        "DELETE" | "UPDATE" => !contains_word(&normalized, "WHERE"),
        _ => false,
    }
}

/// Bind one JSON parameter value onto a query builder. See the module docs'
/// "Params — no `json` sqlx feature" note for the primitive-type mapping and
/// its known limitation.
fn bind_param<'q>(
    query: Query<'q, Postgres, PgArguments>,
    value: &'q Value,
) -> Query<'q, Postgres, PgArguments> {
    match value {
        Value::Null => query.bind(Option::<String>::None),
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

/// Best-effort decode of one `RETURNING` row into a JSON object, keyed by
/// column name. Tries the primitive types this crate's `sqlx` build can
/// decode without the `json` feature (text, int8, float8, bool); a column
/// whose type doesn't match any of those decodes as a placeholder string
/// rather than failing the whole call (see the module docs' params note —
/// `pg_query`, PGT-02, owns a fuller row codec).
fn row_to_json(row: &PgRow) -> Value {
    let mut obj = serde_json::Map::new();
    for (idx, col) in row.columns().iter().enumerate() {
        let value = if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
            v.map(Value::String).unwrap_or(Value::Null)
        } else if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
            v.map(|n| json!(n)).unwrap_or(Value::Null)
        } else if let Ok(v) = row.try_get::<Option<i32>, _>(idx) {
            v.map(|n| json!(n)).unwrap_or(Value::Null)
        } else if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
            v.map(|n| json!(n)).unwrap_or(Value::Null)
        } else if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
            v.map(Value::Bool).unwrap_or(Value::Null)
        } else {
            Value::String("<unrepresentable>".to_string())
        };
        obj.insert(col.name().to_string(), value);
    }
    Value::Object(obj)
}

pub struct PgExecute;

#[async_trait]
impl RustTool for PgExecute {
    fn name(&self) -> &str {
        "pg_execute"
    }

    fn description(&self) -> &str {
        "Run a single, bound-parameter INSERT/UPDATE/DELETE statement (optionally with \
         RETURNING) against a configured Postgres connection identity. Rejects reads, DDL, \
         role/privilege statements, and multi-statement input -- use pg_query/pg_ddl/pg_admin \
         instead. An unqualified DELETE/UPDATE with no WHERE clause is flagged destructive in \
         the response. Defaults to the 'writer' connection identity. Values must be passed as \
         bound params, never interpolated into sql."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "Exactly one INSERT/UPDATE/DELETE statement, optionally with \
                                     RETURNING. Use $1, $2, ... placeholders for values -- never \
                                     interpolate values into this string."
                },
                "params": {
                    "type": "array",
                    "items": {},
                    "description": "Bound values for the statement's $1, $2, ... placeholders, \
                                     in order. Omit or use [] for a statement with no parameters."
                },
                "identity": {
                    "type": "string",
                    "description": "Optional Postgres connection identity to use: a configured \
                                     POSTGRES_URL_<NAME> connection name (e.g. \"writer\", \
                                     \"admin\"). Omit to use pg_execute's default, 'writer' (NOT \
                                     the suite-wide 'readonly' default -- DML needs a writer-tier \
                                     role). Call pg_identities to see the configured names and \
                                     their privilege tiers."
                }
            },
            "required": ["sql"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let sql = args
            .get("sql")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("sql is required and must be a non-empty string".to_string()))?
            .to_string();

        let class = classify_dml(&sql)?;
        let destructive = is_destructive_shape(&sql);

        let identity = args
            .get("identity")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .unwrap_or_else(|| DEFAULT_EXECUTE_IDENTITY.to_string());

        let params: Vec<Value> = args.get("params").and_then(|v| v.as_array()).cloned().unwrap_or_default();

        // GUARDED (PGT-06): gate BEFORE any DB connection is attempted. The
        // SQL text has no secret shape (unlike pg_admin's PASSWORD literals),
        // so it is passed through to the approval audit trail as-is — same
        // standard gate call, no redaction needed on this path.
        let summary = format!(
            "pg_execute {} via identity '{identity}'{}: {sql}",
            class.as_str(),
            if destructive { ", DESTRUCTIVE (no WHERE clause)" } else { "" }
        );
        let safe_args = json!({
            "statement_class": class.as_str(),
            "identity": identity,
            "destructive": destructive,
            "sql": sql,
            "params": params,
        });
        match gate(self.name(), &safe_args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(ToolOutput::text_only(msg)),
        }

        let pool = conn::resolve_connection(&identity).await?;

        let normalized = normalize(&sql);
        let returning = contains_word(&normalized, "RETURNING");

        let (affected, returning_rows): (u64, Option<Vec<Value>>) = if returning {
            let mut query = sqlx::query(&sql);
            for p in &params {
                query = bind_param(query, p);
            }
            let rows = query
                .fetch_all(&pool)
                .await
                .map_err(|e| ToolError::Database(format!("pg_execute failed: {e}")))?;
            let json_rows: Vec<Value> = rows.iter().map(row_to_json).collect();
            (json_rows.len() as u64, Some(json_rows))
        } else {
            let mut query = sqlx::query(&sql);
            for p in &params {
                query = bind_param(query, p);
            }
            let result = query
                .execute(&pool)
                .await
                .map_err(|e| ToolError::Database(format!("pg_execute failed: {e}")))?;
            (result.rows_affected(), None)
        };

        let mut text = format!(
            "{} via identity '{}': {} row(s) affected",
            class.as_str(),
            identity,
            affected
        );
        if returning_rows.is_some() {
            text.push_str(" (RETURNING rows included)");
        }
        if destructive {
            text.push_str(
                " -- DESTRUCTIVE: no WHERE clause qualified this statement; pg_execute is a \
                 guarded tool and this call must be approved before it can run on a live gateway",
            );
        }

        Ok(ToolOutput::with_structured(
            text,
            json!({
                "affected": affected,
                "returning": returning_rows,
                "destructive": destructive,
                "statement_class": class.as_str(),
                "identity": identity,
            }),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(PgExecute));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── statement-class gate ────────────────────────────────────────────

    #[test]
    fn accepts_insert_update_delete() {
        assert_eq!(classify_dml("INSERT INTO t (a) VALUES ($1)").unwrap(), StatementClass::Insert);
        assert_eq!(classify_dml("update t set a = $1 where id = $2").unwrap(), StatementClass::Update);
        assert_eq!(classify_dml("DELETE FROM t WHERE id = $1").unwrap(), StatementClass::Delete);
    }

    #[test]
    fn rejects_select_and_read_statements() {
        for sql in ["SELECT * FROM t", "WITH x AS (SELECT 1) SELECT * FROM x", "EXPLAIN SELECT 1", "SHOW search_path"] {
            let err = classify_dml(sql).unwrap_err();
            assert!(matches!(err, ToolError::InvalidArgument(_)));
            assert!(err.to_string().contains("pg_query"), "{sql} error should point at pg_query: {err}");
        }
    }

    #[test]
    fn rejects_ddl_statements() {
        for sql in ["CREATE TABLE t (id int)", "ALTER TABLE t ADD COLUMN b int", "DROP TABLE t", "TRUNCATE t"] {
            let err = classify_dml(sql).unwrap_err();
            assert!(err.to_string().contains("pg_ddl"), "{sql} error should point at pg_ddl: {err}");
        }
    }

    #[test]
    fn rejects_role_statements() {
        for sql in ["GRANT SELECT ON t TO foo", "REVOKE SELECT ON t FROM foo"] {
            let err = classify_dml(sql).unwrap_err();
            assert!(err.to_string().contains("pg_admin"), "{sql} error should point at pg_admin: {err}");
        }
    }

    #[test]
    fn rejects_multi_statement_input() {
        let err = classify_dml("DELETE FROM t WHERE id = 1; DROP TABLE t").unwrap_err();
        assert!(err.to_string().contains("multi-statement"));

        // A single trailing semicolon is fine.
        assert_eq!(classify_dml("DELETE FROM t WHERE id = 1;").unwrap(), StatementClass::Delete);
    }

    #[test]
    fn rejects_empty_sql() {
        assert!(classify_dml("").is_err());
        assert!(classify_dml("   ").is_err());
    }

    // ── destructive-shape detector ──────────────────────────────────────

    #[test]
    fn delete_with_no_where_is_flagged_destructive() {
        assert!(is_destructive_shape("DELETE FROM x"));
        assert!(is_destructive_shape("delete from x"));
        assert!(is_destructive_shape("DELETE FROM x;"));
    }

    #[test]
    fn delete_with_where_is_not_destructive() {
        assert!(!is_destructive_shape("DELETE FROM x WHERE id = $1"));
        assert!(!is_destructive_shape("DELETE FROM x WHERE id = 1;"));
    }

    #[test]
    fn update_with_no_where_is_flagged_destructive() {
        assert!(is_destructive_shape("UPDATE x SET a = 1"));
        assert!(!is_destructive_shape("UPDATE x SET a = 1 WHERE id = $1"));
    }

    #[test]
    fn truncate_is_always_flagged_destructive() {
        assert!(is_destructive_shape("TRUNCATE x"));
        assert!(is_destructive_shape("TRUNCATE TABLE x"));
    }

    #[test]
    fn insert_is_never_destructive() {
        assert!(!is_destructive_shape("INSERT INTO x (a) VALUES (1)"));
    }

    #[test]
    fn where_lookalike_column_name_does_not_false_positive() {
        // A column/table literally named "wherever" must not satisfy the WHERE
        // token check -- contains_word tokenizes, it doesn't substring-match.
        assert!(is_destructive_shape("UPDATE x SET wherever = 1"));
    }

    // ── tool wiring ──────────────────────────────────────────────────────

    #[test]
    fn registers_as_pg_execute() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("pg_execute"));
    }

    #[test]
    fn default_identity_is_writer_not_readonly() {
        assert_eq!(DEFAULT_EXECUTE_IDENTITY, "writer");
        assert_ne!(DEFAULT_EXECUTE_IDENTITY, conn::DEFAULT_IDENTITY);
    }

    #[tokio::test]
    async fn read_statement_is_a_clean_invalid_argument_not_a_panic() {
        let err = PgExecute.execute_structured(json!({"sql": "SELECT 1"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn missing_sql_is_a_clean_invalid_argument() {
        let err = PgExecute.execute_structured(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
