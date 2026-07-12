//! `pg_ddl` — schema DDL: CREATE/ALTER/DROP TABLE/INDEX/VIEW/SEQUENCE/SCHEMA
//! (PGT-04). GUARDED (PGT-06 wired `pg_ddl` into
//! `crate::approval::GUARDED_BARE_NAMES` AND this tool calls
//! [`crate::approval::gate`] itself at the top of `execute_structured`,
//! after the statement-class gate and before any DB connection — see the
//! module note at the bottom of this file), audited, destructive by design:
//! the operator's explicit goal is an audited door replacing SSH+psql schema
//! edits.
//!
//! ## Statement-class gate (pure string checks, no DB required)
//! [`classify_ddl`] accepts ONLY a single DDL statement whose leading keyword
//! is `CREATE`/`ALTER`/`DROP` and whose target object is `TABLE`/`INDEX`/
//! `VIEW`/`SEQUENCE`/`SCHEMA`. It rejects:
//! - DML/read statements (wrong leading keyword) — clean `InvalidArgument`
//!   pointing at `pg_query`/`pg_execute`.
//! - Role/privilege management (`ROLE`/`USER`/`GROUP`/`GRANT`/`REVOKE`) —
//!   clean `InvalidArgument` pointing at `pg_admin` (PGT-05).
//! - Multi-statement input (a `;` anywhere other than a single optional
//!   trailing terminator) — never partially executes a chain.
//!
//! `DROP` statements, and an `ALTER` that itself contains a `DROP` (dropping
//! a column/constraint/default — the irreversible shapes), are flagged
//! `irreversible: true` in the classification so the response summary and
//! any approval prompt clearly state the blast radius (S115 PGT-04 approach
//! step 3).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::conn;

/// Default identity for `pg_ddl` calls that omit `identity`. Deliberately
/// NOT `conn::DEFAULT_IDENTITY` ("readonly") — schema DDL needs the `admin`
/// DB role; the role is the real privilege boundary (the gate below is a
/// classification/audit aid, not the enforcement mechanism).
const DEFAULT_DDL_IDENTITY: &str = "admin";

/// Object types this tool accepts DDL for. Anything else (a role/user/group,
/// or an unrecognized DDL object) is rejected by [`classify_ddl`].
const RECOGNIZED_OBJECTS: &[&str] = &["TABLE", "INDEX", "VIEW", "SEQUENCE", "SCHEMA"];

/// Tokens that mark role/privilege-management DDL, out of scope for this
/// tool (PGT-05's `pg_admin` handles them) even though `CREATE ROLE` /
/// `ALTER ROLE` / `DROP ROLE` / `DROP USER` share a leading keyword with
/// schema DDL.
const ROLE_MGMT_TOKENS: &[&str] = &["ROLE", "USER", "GROUP"];

/// A classified, gate-passed DDL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlClassification {
    /// `"CREATE"` / `"ALTER"` / `"DROP"`.
    pub statement_class: String,
    /// Lowercase target object: `"table"` / `"index"` / `"view"` /
    /// `"sequence"` / `"schema"`.
    pub object: String,
    /// `true` for any `DROP`, or an `ALTER` that itself drops a
    /// column/constraint/default — the irreversible shapes PGT-04's
    /// ## APPROACH step 3 calls out.
    pub irreversible: bool,
}

/// Tokenize `sql` into uppercase alphanumeric/underscore runs, discarding all
/// punctuation/whitespace/string-literal-quote characters as delimiters.
/// Pure string-level scan (no SQL parser) — deliberately simple, matching
/// this tool's "pure string checks" test-plan contract; it is a
/// classification aid, not a security boundary (the DB role is).
fn tokenize(sql: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in sql.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_uppercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Classify a single SQL statement as accepted schema DDL, or reject it with
/// a clean [`ToolError::InvalidArgument`] naming the right tool to use
/// instead. Pure string logic — no DB connection required, so this is fully
/// unit-testable (per PGT-04's `## TEST PLAN`).
pub fn classify_ddl(sql: &str) -> Result<DdlClassification, ToolError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ToolError::InvalidArgument("pg_ddl: `sql` must not be empty".to_string()));
    }

    // Multi-statement gate: allow at most one optional trailing `;`; any
    // other `;` means more than one statement was submitted.
    let core = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if core.contains(';') {
        return Err(ToolError::InvalidArgument(
            "pg_ddl accepts only a single DDL statement — multi-statement input \
             (`;`-separated) is rejected; submit one statement per call."
                .to_string(),
        ));
    }
    if core.is_empty() {
        return Err(ToolError::InvalidArgument("pg_ddl: `sql` must not be empty".to_string()));
    }

    let tokens = tokenize(core);
    let leading = tokens.first().map(String::as_str).unwrap_or("");

    let statement_class = match leading {
        "CREATE" | "ALTER" | "DROP" => leading.to_string(),
        _ => {
            return Err(ToolError::InvalidArgument(format!(
                "pg_ddl only accepts CREATE/ALTER/DROP statements (got leading keyword \
                 '{leading}'). Use pg_query for reads or pg_execute for DML \
                 (INSERT/UPDATE/DELETE)."
            )))
        }
    };

    // Scan the remaining tokens for the first recognized object keyword OR a
    // role-management token, whichever comes first — role/privilege
    // management is out of scope here even though it shares a leading
    // keyword with schema DDL (`CREATE ROLE`, `ALTER ROLE`, `DROP ROLE`,
    // `DROP USER`).
    let mut object: Option<&str> = None;
    for tok in tokens.iter().skip(1) {
        let tok = tok.as_str();
        if ROLE_MGMT_TOKENS.contains(&tok) {
            return Err(ToolError::InvalidArgument(format!(
                "pg_ddl does not handle role/privilege management ('{tok}' statement) — \
                 use pg_admin for CREATE/ALTER/DROP ROLE|USER, GRANT, REVOKE."
            )));
        }
        if tok == "MATERIALIZED" {
            // `CREATE MATERIALIZED VIEW ...` — the object keyword that
            // follows is VIEW; let the loop continue to find it.
            continue;
        }
        if RECOGNIZED_OBJECTS.contains(&tok) {
            object = Some(tok);
            break;
        }
    }

    let object = object.ok_or_else(|| {
        ToolError::InvalidArgument(
            "pg_ddl could not identify a recognized DDL object (TABLE/INDEX/VIEW/SEQUENCE/ \
             SCHEMA) in the statement — either this is DML/a read (use pg_query/pg_execute) \
             or an unsupported DDL shape."
                .to_string(),
        )
    })?;

    // Irreversibility: any DROP is irreversible by definition. An ALTER that
    // itself contains a DROP (DROP COLUMN / DROP CONSTRAINT / DROP DEFAULT)
    // is the destructive-ALTER shape called out in the spec's approach.
    let irreversible = statement_class == "DROP"
        || (statement_class == "ALTER" && tokens.iter().any(|t| t == "DROP"));

    Ok(DdlClassification { statement_class, object: object.to_lowercase(), irreversible })
}

pub struct PgDdl;

#[async_trait]
impl RustTool for PgDdl {
    fn name(&self) -> &str {
        "pg_ddl"
    }

    fn description(&self) -> &str {
        "Run a single schema-DDL statement (CREATE/ALTER/DROP on TABLE/INDEX/VIEW/SEQUENCE/ \
         SCHEMA) against a Postgres identity's connection. GUARDED (requires operator \
         approval) and audited. DML, reads, role/privilege management, and multi-statement \
         input are rejected with a clean error naming the right tool. DROP and destructive \
         ALTER statements are flagged irreversible in the response. Default identity: admin."
    }

    fn parameters(&self) -> Value {
        conn::with_identity_param(json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "A single DDL statement: CREATE/ALTER/DROP TABLE, \
                                     CREATE/DROP INDEX, CREATE/ALTER/DROP VIEW, \
                                     CREATE/ALTER/DROP SEQUENCE, or CREATE/DROP SCHEMA. \
                                     Exactly one statement (an optional single trailing ';' \
                                     is fine); role/privilege management belongs in pg_admin."
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
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("pg_ddl requires a non-empty `sql` string".to_string()))?;

        let classification = classify_ddl(sql)?;

        let identity = args
            .get("identity")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .unwrap_or_else(|| DEFAULT_DDL_IDENTITY.to_string());

        // GUARDED (PGT-06): gate BEFORE any DB connection is attempted. The
        // SQL text has no secret shape (unlike pg_admin's PASSWORD
        // literals), so it is passed through to the approval audit trail
        // as-is — same standard gate call, no redaction needed on this path.
        let summary = format!(
            "pg_ddl {} {} via identity '{identity}'{}: {sql}",
            classification.statement_class,
            classification.object,
            if classification.irreversible { ", IRREVERSIBLE" } else { "" }
        );
        let safe_args = json!({
            "statement_class": classification.statement_class,
            "object": classification.object,
            "identity": identity,
            "irreversible": classification.irreversible,
            "sql": sql,
        });
        match gate(self.name(), &safe_args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(ToolOutput::text_only(msg)),
        }

        let pool = conn::resolve_connection(&identity).await?;

        sqlx::query(sql)
            .execute(&pool)
            .await
            .map_err(|e| ToolError::Database(format!("pg_ddl statement failed: {e}")))?;

        let blast_radius = if classification.irreversible {
            "IRREVERSIBLE — this statement drops or destructively alters a schema object; \
             there is no undo through this tool."
        } else {
            "Non-destructive (CREATE, or a non-dropping ALTER)."
        };

        let text = format!(
            "pg_ddl: {} {} succeeded via identity '{identity}'. {blast_radius}",
            classification.statement_class, classification.object
        );

        Ok(ToolOutput::with_structured(
            text,
            json!({
                "statement_class": classification.statement_class,
                "object": classification.object,
                "irreversible": classification.irreversible,
                "identity": identity,
                "ok": true,
            }),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(PgDdl));
}

// PGT-06: `pg_ddl` is now guarded — see `crate::approval::GUARDED_BARE_NAMES`
// (alongside `pg_execute`/`pg_admin`) and the `gate(...)` call in
// `execute_structured` above.

#[cfg(test)]
mod tests {
    use super::*;

    // ── accepts: CREATE/ALTER/DROP × TABLE/INDEX/VIEW/SEQUENCE/SCHEMA ──

    #[test]
    fn accepts_create_table() {
        let c = classify_ddl("CREATE TABLE widgets (id serial primary key)").unwrap();
        assert_eq!(c.statement_class, "CREATE");
        assert_eq!(c.object, "table");
        assert!(!c.irreversible);
    }

    #[test]
    fn accepts_alter_table_add_column() {
        let c = classify_ddl("ALTER TABLE widgets ADD COLUMN name text").unwrap();
        assert_eq!(c.statement_class, "ALTER");
        assert_eq!(c.object, "table");
        assert!(!c.irreversible);
    }

    #[test]
    fn accepts_drop_table() {
        let c = classify_ddl("DROP TABLE widgets").unwrap();
        assert_eq!(c.statement_class, "DROP");
        assert_eq!(c.object, "table");
        assert!(c.irreversible);
    }

    #[test]
    fn accepts_create_index() {
        let c = classify_ddl("CREATE INDEX idx_widgets_name ON widgets (name)").unwrap();
        assert_eq!(c.statement_class, "CREATE");
        assert_eq!(c.object, "index");
    }

    #[test]
    fn accepts_create_unique_index_concurrently() {
        let c =
            classify_ddl("CREATE UNIQUE INDEX CONCURRENTLY idx_x ON widgets (id)").unwrap();
        assert_eq!(c.object, "index");
    }

    #[test]
    fn accepts_drop_index() {
        let c = classify_ddl("DROP INDEX idx_widgets_name").unwrap();
        assert_eq!(c.statement_class, "DROP");
        assert_eq!(c.object, "index");
        assert!(c.irreversible);
    }

    #[test]
    fn accepts_create_view_and_materialized_view() {
        let c = classify_ddl("CREATE VIEW v AS SELECT 1").unwrap();
        assert_eq!(c.object, "view");

        let c2 = classify_ddl("CREATE MATERIALIZED VIEW mv AS SELECT 1").unwrap();
        assert_eq!(c2.object, "view");
    }

    #[test]
    fn accepts_create_sequence() {
        let c = classify_ddl("CREATE SEQUENCE seq_1").unwrap();
        assert_eq!(c.object, "sequence");
    }

    #[test]
    fn accepts_create_and_drop_schema() {
        assert_eq!(classify_ddl("CREATE SCHEMA reporting").unwrap().object, "schema");
        let c = classify_ddl("DROP SCHEMA reporting").unwrap();
        assert_eq!(c.object, "schema");
        assert!(c.irreversible);
    }

    #[test]
    fn accepts_trailing_semicolon() {
        let c = classify_ddl("CREATE TABLE t (id int);").unwrap();
        assert_eq!(c.object, "table");
    }

    // ── rejects: DML ──

    #[test]
    fn rejects_insert() {
        let err = classify_ddl("INSERT INTO widgets (id) VALUES (1)").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn rejects_update_and_delete() {
        assert!(classify_ddl("UPDATE widgets SET name = 'x'").is_err());
        assert!(classify_ddl("DELETE FROM widgets").is_err());
    }

    // ── rejects: read ──

    #[test]
    fn rejects_select_and_explain_and_show() {
        assert!(classify_ddl("SELECT * FROM widgets").is_err());
        assert!(classify_ddl("EXPLAIN SELECT * FROM widgets").is_err());
        assert!(classify_ddl("SHOW search_path").is_err());
    }

    // ── rejects: role/privilege management (belongs in pg_admin) ──

    #[test]
    fn rejects_create_role() {
        let err = classify_ddl("CREATE ROLE app_writer").unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("pg_admin")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn rejects_alter_role_drop_role_drop_user() {
        assert!(classify_ddl("ALTER ROLE app_writer WITH PASSWORD 'x'").is_err());
        assert!(classify_ddl("DROP ROLE app_writer").is_err());
        assert!(classify_ddl("DROP USER legacy_user").is_err());
    }

    #[test]
    fn rejects_grant_and_revoke() {
        assert!(classify_ddl("GRANT SELECT ON widgets TO app_reader").is_err());
        assert!(classify_ddl("REVOKE SELECT ON widgets FROM app_reader").is_err());
    }

    // ── rejects: multi-statement ──

    #[test]
    fn rejects_multi_statement() {
        let err = classify_ddl("CREATE TABLE x (id int); DROP TABLE y").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn rejects_multi_statement_ddl_then_dml() {
        assert!(classify_ddl("CREATE TABLE x (id int); DELETE FROM x").is_err());
    }

    // ── rejects: empty / unsupported object ──

    #[test]
    fn rejects_empty_sql() {
        assert!(classify_ddl("").is_err());
        assert!(classify_ddl("   ").is_err());
    }

    #[test]
    fn rejects_unrecognized_ddl_object() {
        // CREATE EXTENSION isn't in the recognized-object list for this tool.
        let err = classify_ddl("CREATE EXTENSION pgvector").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── irreversibility flagging ──

    #[test]
    fn drop_statements_always_irreversible() {
        assert!(classify_ddl("DROP TABLE t").unwrap().irreversible);
        assert!(classify_ddl("DROP INDEX i").unwrap().irreversible);
        assert!(classify_ddl("DROP VIEW v").unwrap().irreversible);
        assert!(classify_ddl("DROP SEQUENCE s").unwrap().irreversible);
        assert!(classify_ddl("DROP SCHEMA sch").unwrap().irreversible);
    }

    #[test]
    fn alter_drop_column_is_irreversible() {
        let c = classify_ddl("ALTER TABLE widgets DROP COLUMN name").unwrap();
        assert!(c.irreversible);
    }

    #[test]
    fn alter_drop_constraint_is_irreversible() {
        let c = classify_ddl("ALTER TABLE widgets DROP CONSTRAINT widgets_pkey").unwrap();
        assert!(c.irreversible);
    }

    #[test]
    fn alter_add_column_is_not_irreversible() {
        let c = classify_ddl("ALTER TABLE widgets ADD COLUMN name text").unwrap();
        assert!(!c.irreversible);
    }

    #[test]
    fn create_statements_never_irreversible() {
        assert!(!classify_ddl("CREATE TABLE t (id int)").unwrap().irreversible);
        assert!(!classify_ddl("CREATE INDEX i ON t (id)").unwrap().irreversible);
    }

    // ── tool metadata / registration ──

    #[test]
    fn tool_name_and_default_identity() {
        assert_eq!(PgDdl.name(), "pg_ddl");
        assert_eq!(DEFAULT_DDL_IDENTITY, "admin");
    }

    #[test]
    fn registers_pg_ddl() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("pg_ddl"));
    }

    #[tokio::test]
    async fn missing_sql_arg_is_invalid_argument() {
        let err = PgDdl.execute_structured(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn dml_sql_arg_is_rejected_before_any_connection_attempt() {
        // No POSTGRES_URL_ADMIN is configured in the test environment; if the
        // gate didn't short-circuit before `resolve_connection`, this would
        // instead surface a NotConfigured/Database error. Getting a clean
        // InvalidArgument here proves the class gate runs first.
        let err = PgDdl
            .execute_structured(json!({"sql": "DELETE FROM widgets"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
