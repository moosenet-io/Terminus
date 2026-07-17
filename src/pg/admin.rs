//! `pg_admin` — role/user & privilege administration (PGT-05), GUARDED.
//!
//! The "user level control" door: `CREATE`/`ALTER`/`DROP ROLE`|`USER`,
//! `GRANT`, `REVOKE`. Requires the `admin` identity by default (the DB ROLE
//! behind that identity is the real privilege boundary — this tool's own
//! class gate is defense in depth, not the boundary itself). Registered as a
//! GUARDED tool — PGT-06 added `pg_admin` to
//! `crate::approval::GUARDED_BARE_NAMES` (see the module note below) — so
//! every call goes through the gateway's human-approval flow before it ever
//! reaches Postgres.
//!
//! ## S6 — password redaction is load-bearing, not cosmetic
//! A `CREATE ROLE ... PASSWORD '...'` (or `ALTER ROLE ... PASSWORD '...'`)
//! literal's password value MUST NEVER appear in ANY string this tool emits:
//! not the approval-gate summary, not the args bound into a pending/approved
//! `tool_approvals` row (the approval system's own audit trail), not the
//! tool's text/structured response, not a `Display`/error. [`redact_password`]
//! is the ONE place a `PASSWORD '...'` literal is ever rewritten, and
//! [`PgAdmin::execute_structured`] calls it before constructing ANYTHING that
//! leaves this function scope loggable-or-not — the only place the REAL
//! password value is ever held is the local `String` used to actually talk
//! to Postgres, which is dropped at the end of the call.
//!
//! ## Two input shapes
//! - **Structured** (`action`/`role`/`options`/`password`/…) — PREFERRED.
//!   [`render_structured`] assembles the SQL from typed fields, so a caller
//!   never has to hand-format a `PASSWORD '...'` literal into a loggable
//!   `sql` string at all; the real password only ever exists in the
//!   assembled statement string used for execution.
//! - **Raw `sql`** — kept for flexibility (the spec's own required negative
//!   test exercises this shape directly: a `CREATE ROLE ... PASSWORD 'x'`
//!   `sql` string). Redaction is mandatory on this path too — enforced by
//!   the same [`redact_password`] call before anything is emitted.
//!
//! ## Class gate
//! Only role/privilege statement classes are accepted: `CREATE`/`ALTER`/
//! `DROP ROLE`|`USER`, `GRANT`, `REVOKE`. DDL (`pg_ddl`), DML (`pg_execute`),
//! reads (`pg_query`), and multi-statement (`;`-chained) input are all a
//! clean [`ToolError::InvalidArgument`] pointing at the right tool. See
//! [`classify_admin_statement`].
//!
//! ## Guarding — NOTE for the reader
//! PGT-06 added `pg_execute`, `pg_ddl`, AND `pg_admin` together to
//! `crate::approval::GUARDED_BARE_NAMES`, with a test that enumerates the
//! registered `pg_*` names so no guard entry ever dangles. `pg_admin` also
//! calls [`crate::approval::gate`] itself at the top of `execute_structured`
//! (matching the `openhands`/`<secret-manager>` precedent: the tool owns its own
//! gate call; `GUARDED_BARE_NAMES` is a second, gateway-level check used for
//! federated/mesh dispatch, not the only enforcement point) — both layers
//! are in place.

use std::sync::OnceLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::conn;

/// Default connection identity for `pg_admin` calls that omit `identity` —
/// the real privilege boundary is always the DB ROLE behind this identity,
/// not this tool's own class gate.
const DEFAULT_ADMIN_IDENTITY: &str = "admin";

/// The recognized `pg_admin` statement classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminClass {
    CreateRole,
    AlterRole,
    DropRole,
    Grant,
    Revoke,
}

impl AdminClass {
    fn label(self) -> &'static str {
        match self {
            AdminClass::CreateRole => "CREATE ROLE",
            AdminClass::AlterRole => "ALTER ROLE",
            AdminClass::DropRole => "DROP ROLE",
            AdminClass::Grant => "GRANT",
            AdminClass::Revoke => "REVOKE",
        }
    }

    /// `DROP ROLE`/`REVOKE` are the high-impact, hard-to-undo shapes —
    /// flagged in the response + (via the redacted summary) the audit trail.
    fn high_impact(self) -> bool {
        matches!(self, AdminClass::DropRole | AdminClass::Revoke)
    }
}

/// Does `upper`'s (already-uppercased, already-single-statement) token
/// stream start with exactly `words`, in order?
fn starts_with_words(upper: &str, words: &[&str]) -> bool {
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    tokens.len() >= words.len() && tokens.iter().zip(words.iter()).all(|(t, w)| t == w)
}

/// Classify a single SQL statement as a `pg_admin`-eligible role/privilege
/// statement, or reject it with a clean [`ToolError::InvalidArgument`]
/// pointing at the right tool. Pure string-level checks — no DB round trip,
/// exactly like the sibling `pg_query`/`pg_execute`/`pg_ddl` class gates.
///
/// Rejects multi-statement (`;`-chained) input: at most one optional
/// trailing `;` is tolerated; any other `;` means a second statement.
fn classify_admin_statement(sql: &str) -> Result<AdminClass, ToolError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ToolError::InvalidArgument("sql must not be empty".into()));
    }
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if body.contains(';') {
        return Err(ToolError::InvalidArgument(
            "pg_admin accepts only a single statement — multi-statement (`;`-chained) input is \
             rejected"
                .into(),
        ));
    }

    let upper = body.to_uppercase();
    if starts_with_words(&upper, &["CREATE", "ROLE"]) || starts_with_words(&upper, &["CREATE", "USER"]) {
        Ok(AdminClass::CreateRole)
    } else if starts_with_words(&upper, &["ALTER", "ROLE"]) || starts_with_words(&upper, &["ALTER", "USER"]) {
        Ok(AdminClass::AlterRole)
    } else if starts_with_words(&upper, &["DROP", "ROLE"]) || starts_with_words(&upper, &["DROP", "USER"]) {
        Ok(AdminClass::DropRole)
    } else if starts_with_words(&upper, &["GRANT"]) {
        Ok(AdminClass::Grant)
    } else if starts_with_words(&upper, &["REVOKE"]) {
        Ok(AdminClass::Revoke)
    } else {
        let hint = if starts_with_words(&upper, &["CREATE", "TABLE"])
            || starts_with_words(&upper, &["ALTER", "TABLE"])
            || starts_with_words(&upper, &["DROP", "TABLE"])
            || starts_with_words(&upper, &["CREATE", "INDEX"])
            || starts_with_words(&upper, &["DROP", "INDEX"])
            || starts_with_words(&upper, &["CREATE", "VIEW"])
            || starts_with_words(&upper, &["CREATE", "SEQUENCE"])
            || starts_with_words(&upper, &["CREATE", "SCHEMA"])
        {
            "this looks like DDL — use pg_ddl"
        } else if starts_with_words(&upper, &["INSERT"])
            || starts_with_words(&upper, &["UPDATE"])
            || starts_with_words(&upper, &["DELETE"])
        {
            "this looks like DML — use pg_execute"
        } else if starts_with_words(&upper, &["SELECT"])
            || starts_with_words(&upper, &["WITH"])
            || starts_with_words(&upper, &["EXPLAIN"])
            || starts_with_words(&upper, &["SHOW"])
        {
            "this looks like a read statement — use pg_query"
        } else {
            "not a recognized role/privilege statement"
        };
        Err(ToolError::InvalidArgument(format!(
            "pg_admin only accepts role/privilege statements (CREATE/ALTER/DROP ROLE|USER, \
             GRANT, REVOKE) — {hint}"
        )))
    }
}

fn password_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)PASSWORD\s+'([^']*)'").expect("valid PASSWORD regex"))
}

/// Redact any `PASSWORD '...'` literal in `sql` to `PASSWORD '***REDACTED***'`.
/// The ONE place a password literal is ever rewritten (S6) — see the module
/// doc's "S6 — password redaction is load-bearing" note. Matches
/// `ENCRYPTED PASSWORD '...'` too (the regex only anchors on `PASSWORD '…'`,
/// so an `ENCRYPTED`/`UNENCRYPTED` prefix ahead of it is untouched and the
/// literal after `PASSWORD` is still redacted).
pub fn redact_password(sql: &str) -> String {
    password_regex().replace_all(sql, "PASSWORD '***REDACTED***'").into_owned()
}

/// A bare SQL identifier: letters/digits/underscore, not starting with a
/// digit. Deliberately conservative — `pg_admin` builds statements by string
/// assembly (role/privilege DDL has no bind-parameter form in Postgres), so
/// this is the injection guard for the structured-arg path: a role name
/// containing `;`, quotes, or whitespace is rejected rather than assembled
/// into a statement.
fn valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{field}' is required and must not be empty")))
}

fn validated_identifier_list(raw: &str, field: &str) -> Result<Vec<String>, ToolError> {
    let names: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        return Err(ToolError::InvalidArgument(format!("'{field}' must not be empty")));
    }
    for name in &names {
        if !valid_identifier(name) {
            return Err(ToolError::InvalidArgument(format!(
                "'{field}' contains an invalid role identifier '{name}' — only letters, digits, \
                 and underscores are allowed"
            )));
        }
    }
    Ok(names)
}

/// Reject a free-form fragment (privileges/on-target) that could smuggle a
/// second statement or a password literal into the assembled SQL.
fn validate_fragment(raw: &str, field: &str) -> Result<(), ToolError> {
    if raw.contains(';') {
        return Err(ToolError::InvalidArgument(format!("'{field}' must not contain ';'")));
    }
    if raw.to_uppercase().contains("PASSWORD") {
        return Err(ToolError::InvalidArgument(format!(
            "'{field}' must not contain PASSWORD — use the dedicated 'password' argument"
        )));
    }
    Ok(())
}

/// Render the structured `{ action, role, ... }` argument shape into a SQL
/// statement. Returns the assembled SQL (which may contain a REAL password
/// value — this is the only function that ever sees one) and the
/// [`AdminClass`] it belongs to. The caller MUST redact the returned SQL via
/// [`redact_password`] before it touches anything loggable.
fn render_structured(args: &Value) -> Result<(String, AdminClass), ToolError> {
    let action = require_str(args, "action")?.to_lowercase();

    match action.as_str() {
        "create_role" | "alter_role" => {
            let role = require_str(args, "role")?;
            if !valid_identifier(role) {
                return Err(ToolError::InvalidArgument(format!(
                    "'role' contains an invalid identifier '{role}' — only letters, digits, and \
                     underscores are allowed"
                )));
            }
            let options = args.get("options").and_then(Value::as_str).map(str::trim).unwrap_or("");
            if !options.is_empty() {
                validate_fragment(options, "options")?;
            }
            let password = args.get("password").and_then(Value::as_str).filter(|p| !p.is_empty());

            let verb = if action == "create_role" { "CREATE ROLE" } else { "ALTER ROLE" };
            let mut sql = format!("{verb} {role}");
            if !options.is_empty() {
                sql.push(' ');
                sql.push_str(options);
            }
            if let Some(pw) = password {
                sql.push_str(&format!(" PASSWORD '{}'", escape_sql_string(pw)));
            }
            let class = if action == "create_role" { AdminClass::CreateRole } else { AdminClass::AlterRole };
            Ok((sql, class))
        }
        "drop_role" => {
            let role = require_str(args, "role")?;
            let names = validated_identifier_list(role, "role")?;
            Ok((format!("DROP ROLE {}", names.join(", ")), AdminClass::DropRole))
        }
        "grant" => {
            let privileges = require_str(args, "privileges")?;
            let on = require_str(args, "on")?;
            let to = require_str(args, "to")?;
            validate_fragment(privileges, "privileges")?;
            validate_fragment(on, "on")?;
            let to_names = validated_identifier_list(to, "to")?;
            Ok((format!("GRANT {privileges} ON {on} TO {}", to_names.join(", ")), AdminClass::Grant))
        }
        "revoke" => {
            let privileges = require_str(args, "privileges")?;
            let on = require_str(args, "on")?;
            let from = require_str(args, "from")?;
            validate_fragment(privileges, "privileges")?;
            validate_fragment(on, "on")?;
            let from_names = validated_identifier_list(from, "from")?;
            Ok((format!("REVOKE {privileges} ON {on} FROM {}", from_names.join(", ")), AdminClass::Revoke))
        }
        other => Err(ToolError::InvalidArgument(format!(
            "unknown action '{other}' — expected one of: create_role, alter_role, drop_role, grant, revoke"
        ))),
    }
}

/// Resolve the SQL statement to run from either the structured (`action`)
/// or raw (`sql`) argument shape. Returns the REAL (possibly
/// password-bearing) SQL plus its class — same redaction obligation on the
/// caller as [`render_structured`].
fn resolve_statement(args: &Value) -> Result<(String, AdminClass), ToolError> {
    let has_sql = args.get("sql").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false);
    let has_action = args.get("action").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false);

    match (has_sql, has_action) {
        (true, true) => Err(ToolError::InvalidArgument(
            "provide either 'sql' or 'action' (structured form), not both".into(),
        )),
        (true, false) => {
            let sql = require_str(args, "sql")?.to_string();
            let class = classify_admin_statement(&sql)?;
            Ok((sql, class))
        }
        (false, true) => render_structured(args),
        (false, false) => Err(ToolError::InvalidArgument(
            "provide either 'sql' (a single role/privilege statement) or the structured 'action' \
             form ({action, role, ...})"
                .into(),
        )),
    }
}

pub struct PgAdmin;

#[async_trait]
impl RustTool for PgAdmin {
    fn name(&self) -> &str {
        "pg_admin"
    }

    fn description(&self) -> &str {
        "GUARDED: role/user and privilege administration — CREATE/ALTER/DROP ROLE|USER, GRANT, \
         REVOKE. Accepts either a structured {action, role, options, password, privileges, on, \
         to, from} form (preferred — a raw password never has to be hand-formatted into a \
         loggable sql string) or a raw single-statement 'sql' string. Any PASSWORD '...' \
         literal is redacted to '***REDACTED***' in every log/audit/response emission. \
         DROP ROLE and REVOKE are flagged high-impact. Requires operator approval \
         (guarded) and the 'admin' connection identity by default."
    }

    fn parameters(&self) -> Value {
        conn::with_conn_params(json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "A single role/privilege statement (CREATE/ALTER/DROP ROLE|USER, \
                                    GRANT, REVOKE). Mutually exclusive with 'action'. Any PASSWORD \
                                    literal is redacted before it is ever logged/audited/returned."
                },
                "action": {
                    "type": "string",
                    "enum": ["create_role", "alter_role", "drop_role", "grant", "revoke"],
                    "description": "Structured form (preferred). Mutually exclusive with 'sql'."
                },
                "role": {
                    "type": "string",
                    "description": "Role name for create_role/alter_role/drop_role (comma-separated \
                                     for drop_role to drop multiple roles)."
                },
                "options": {
                    "type": "string",
                    "description": "Extra role options for create_role/alter_role (e.g. 'LOGIN \
                                     CREATEDB') — must NOT contain PASSWORD; use 'password' instead."
                },
                "password": {
                    "type": "string",
                    "description": "Password for create_role/alter_role. NEVER echoed back — redacted \
                                     to ***REDACTED*** in every response/log/audit line."
                },
                "privileges": {
                    "type": "string",
                    "description": "Privilege list for grant/revoke, e.g. 'SELECT, INSERT'."
                },
                "on": {
                    "type": "string",
                    "description": "Target object for grant/revoke, e.g. 'TABLE foo' or 'ALL TABLES IN SCHEMA bar'."
                },
                "to": {
                    "type": "string",
                    "description": "Role(s) receiving privileges for grant (comma-separated for multiple)."
                },
                "from": {
                    "type": "string",
                    "description": "Role(s) losing privileges for revoke (comma-separated for multiple)."
                }
            },
            "required": []
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (sql, class) = resolve_statement(&args)?;
        // From this point on, `sql` (which may hold a REAL password) must
        // NEVER be used directly in anything returned, logged, or gated on.
        // `redacted_sql` is the only form that leaves this scope in a
        // loggable/emittable shape.
        let redacted_sql = redact_password(&sql);

        // pg_admin's default identity is `admin` (not conn's crate-wide
        // `readonly` default) — the operator's own explicit `identity` arg
        // always wins either way.
        let identity = args
            .get("identity")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .unwrap_or_else(|| DEFAULT_ADMIN_IDENTITY.to_string());

        let high_impact = class.high_impact();
        let summary = format!(
            "pg_admin {} (identity: {identity}{}): {redacted_sql}",
            class.label(),
            if high_impact { ", HIGH-IMPACT" } else { "" }
        );

        // Build a redaction-safe args payload for the approval gate: the
        // gate persists its `args` into the `tool_approvals` audit row, so
        // this MUST NEVER carry the raw `sql`/`password` fields — only the
        // already-redacted statement and non-secret metadata (S6).
        let safe_args = json!({
            "statement_class": class.label(),
            "identity": identity,
            "high_impact": high_impact,
            "sql_redacted": redacted_sql,
        });

        match gate(self.name(), &safe_args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(ToolOutput::text_only(msg)),
        }

        let pool = conn::resolve_connection_for(&identity, conn::resolve_database_name(&args).as_deref()).await?;
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .map_err(|e| ToolError::Database(format!("pg_admin {} failed: {e}", class.label())))?;

        let text = format!(
            "pg_admin: {} succeeded (identity: {identity}){}. Statement: {redacted_sql}",
            class.label(),
            if high_impact { " — HIGH-IMPACT operation" } else { "" }
        );
        Ok(ToolOutput::with_structured(
            text,
            json!({
                "ok": true,
                "statement_class": class.label(),
                "identity": identity,
                "high_impact": high_impact,
                "sql_redacted": redacted_sql,
            }),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(PgAdmin));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── class gate ──────────────────────────────────────────────────────

    #[test]
    fn accepts_create_alter_drop_role_and_user() {
        assert!(matches!(classify_admin_statement("CREATE ROLE bob"), Ok(AdminClass::CreateRole)));
        assert!(matches!(classify_admin_statement("create user bob"), Ok(AdminClass::CreateRole)));
        assert!(matches!(classify_admin_statement("ALTER ROLE bob LOGIN"), Ok(AdminClass::AlterRole)));
        assert!(matches!(classify_admin_statement("alter user bob nologin"), Ok(AdminClass::AlterRole)));
        assert!(matches!(classify_admin_statement("DROP ROLE bob"), Ok(AdminClass::DropRole)));
        assert!(matches!(classify_admin_statement("drop user bob"), Ok(AdminClass::DropRole)));
    }

    #[test]
    fn accepts_grant_and_revoke() {
        assert!(matches!(
            classify_admin_statement("GRANT SELECT ON foo TO bob"),
            Ok(AdminClass::Grant)
        ));
        assert!(matches!(
            classify_admin_statement("REVOKE SELECT ON foo FROM bob"),
            Ok(AdminClass::Revoke)
        ));
    }

    #[test]
    fn rejects_ddl() {
        for sql in ["CREATE TABLE foo (id int)", "ALTER TABLE foo ADD COLUMN x int", "DROP TABLE foo", "CREATE INDEX idx ON foo(id)"] {
            let err = classify_admin_statement(sql).unwrap_err();
            assert!(matches!(err, ToolError::InvalidArgument(_)));
            assert!(err.to_string().contains("pg_ddl"), "{sql} error should point at pg_ddl: {err}");
        }
    }

    #[test]
    fn rejects_dml() {
        for sql in ["INSERT INTO foo VALUES (1)", "UPDATE foo SET x = 1", "DELETE FROM foo"] {
            let err = classify_admin_statement(sql).unwrap_err();
            assert!(err.to_string().contains("pg_execute"), "{sql} error should point at pg_execute: {err}");
        }
    }

    #[test]
    fn rejects_reads() {
        for sql in ["SELECT * FROM foo", "WITH x AS (SELECT 1) SELECT * FROM x", "EXPLAIN SELECT 1", "SHOW search_path"] {
            let err = classify_admin_statement(sql).unwrap_err();
            assert!(err.to_string().contains("pg_query"), "{sql} error should point at pg_query: {err}");
        }
    }

    #[test]
    fn rejects_multi_statement() {
        let err = classify_admin_statement("CREATE ROLE bob; DROP TABLE foo").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("single statement"));
    }

    #[test]
    fn tolerates_one_trailing_semicolon() {
        assert!(matches!(classify_admin_statement("CREATE ROLE bob;"), Ok(AdminClass::CreateRole)));
    }

    #[test]
    fn rejects_empty() {
        assert!(classify_admin_statement("").is_err());
        assert!(classify_admin_statement("   ").is_err());
    }

    #[test]
    fn rejects_unrecognized_statement() {
        let err = classify_admin_statement("VACUUM foo").unwrap_err();
        assert!(err.to_string().contains("not a recognized role/privilege statement"));
    }

    #[test]
    fn drop_role_and_revoke_are_high_impact() {
        assert!(AdminClass::DropRole.high_impact());
        assert!(AdminClass::Revoke.high_impact());
        assert!(!AdminClass::CreateRole.high_impact());
        assert!(!AdminClass::AlterRole.high_impact());
        assert!(!AdminClass::Grant.high_impact());
    }

    // ── S6 password redaction — the REQUIRED negative/security test ───────

    #[test]
    fn create_role_password_is_redacted() {
        let sql = "CREATE ROLE bob LOGIN PASSWORD 'x'";
        let redacted = redact_password(sql);
        assert!(!redacted.contains("'x'"), "raw password value must never appear: {redacted}");
        assert!(redacted.contains("***REDACTED***"), "redacted marker must be present: {redacted}");
        assert!(redacted.contains("CREATE ROLE bob LOGIN"), "surrounding statement preserved: {redacted}");
    }

    #[test]
    fn redaction_handles_encrypted_password_and_special_chars() {
        let sql = "ALTER ROLE svc ENCRYPTED PASSWORD 'p@ss!w0rd#123'";
        let redacted = redact_password(sql);
        assert!(!redacted.contains("p@ss!w0rd#123"));
        assert!(redacted.contains("***REDACTED***"));
        assert!(redacted.contains("ENCRYPTED"));
    }

    #[test]
    fn redaction_is_case_insensitive_on_keyword() {
        let sql = "create role bob password 'secretvalue'";
        let redacted = redact_password(sql);
        assert!(!redacted.contains("secretvalue"));
        assert!(redacted.contains("***REDACTED***"));
    }

    #[test]
    fn redaction_handles_multiple_password_literals_in_one_string() {
        // Defense in depth: even if two PASSWORD-shaped literals somehow
        // appear, neither raw value survives.
        let sql = "CREATE ROLE a PASSWORD 'first'; CREATE ROLE b PASSWORD 'second'";
        let redacted = redact_password(sql);
        assert!(!redacted.contains("'first'"));
        assert!(!redacted.contains("'second'"));
        assert_eq!(redacted.matches("***REDACTED***").count(), 2);
    }

    #[test]
    fn no_password_literal_is_a_no_op() {
        let sql = "GRANT SELECT ON foo TO bob";
        assert_eq!(redact_password(sql), sql);
    }

    // ── structured render — redaction applies to this path too ────────────

    #[test]
    fn structured_create_role_renders_and_is_redactable() {
        let args = json!({"action": "create_role", "role": "bob", "password": "supersecret", "options": "LOGIN"});
        let (sql, class) = render_structured(&args).unwrap();
        assert!(matches!(class, AdminClass::CreateRole));
        assert!(sql.contains("supersecret"), "the real sql must carry the real password for execution");
        let redacted = redact_password(&sql);
        assert!(!redacted.contains("supersecret"), "raw password must never survive redaction: {redacted}");
        assert!(redacted.contains("***REDACTED***"));
        assert!(redacted.contains("CREATE ROLE bob LOGIN"));
    }

    #[test]
    fn structured_drop_role_is_high_impact() {
        let args = json!({"action": "drop_role", "role": "bob"});
        let (sql, class) = render_structured(&args).unwrap();
        assert_eq!(sql, "DROP ROLE bob");
        assert!(class.high_impact());
    }

    #[test]
    fn structured_grant_and_revoke_render() {
        let grant = json!({"action": "grant", "privileges": "SELECT, INSERT", "on": "TABLE foo", "to": "bob"});
        let (sql, class) = render_structured(&grant).unwrap();
        assert_eq!(sql, "GRANT SELECT, INSERT ON TABLE foo TO bob");
        assert!(matches!(class, AdminClass::Grant));

        let revoke = json!({"action": "revoke", "privileges": "SELECT", "on": "TABLE foo", "from": "bob"});
        let (sql, class) = render_structured(&revoke).unwrap();
        assert_eq!(sql, "REVOKE SELECT ON TABLE foo FROM bob");
        assert!(class.high_impact());
    }

    #[test]
    fn structured_rejects_invalid_role_identifier() {
        let args = json!({"action": "create_role", "role": "bob; DROP TABLE foo"});
        let err = render_structured(&args).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn structured_rejects_password_smuggled_via_options() {
        let args = json!({"action": "create_role", "role": "bob", "options": "PASSWORD 'sneaky'"});
        let err = render_structured(&args).unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn structured_rejects_semicolon_in_fragment() {
        let args = json!({"action": "grant", "privileges": "SELECT; DROP TABLE foo", "on": "bar", "to": "bob"});
        assert!(render_structured(&args).is_err());
    }

    #[test]
    fn structured_unknown_action_is_invalid_argument() {
        let args = json!({"action": "truncate_everything", "role": "bob"});
        let err = render_structured(&args).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── resolve_statement — sql vs action dispatch ─────────────────────────

    #[test]
    fn resolve_statement_rejects_both_sql_and_action() {
        let args = json!({"sql": "CREATE ROLE bob", "action": "create_role", "role": "bob"});
        let err = resolve_statement(&args).unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    fn resolve_statement_rejects_neither_sql_nor_action() {
        let err = resolve_statement(&json!({})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn resolve_statement_raw_sql_path_classifies() {
        let (sql, class) = resolve_statement(&json!({"sql": "DROP ROLE bob"})).unwrap();
        assert_eq!(sql, "DROP ROLE bob");
        assert!(matches!(class, AdminClass::DropRole));
    }

    // ── identifier validation ──────────────────────────────────────────────

    #[test]
    fn valid_identifier_accepts_and_rejects() {
        assert!(valid_identifier("bob"));
        assert!(valid_identifier("_svc_writer"));
        assert!(valid_identifier("bob2"));
        assert!(!valid_identifier("2bob"));
        assert!(!valid_identifier("bob; drop table x"));
        assert!(!valid_identifier("bob-writer"));
        assert!(!valid_identifier(""));
    }

    #[test]
    fn validated_identifier_list_splits_and_validates() {
        assert_eq!(validated_identifier_list("bob, alice", "to").unwrap(), vec!["bob", "alice"]);
        assert!(validated_identifier_list("bob; drop table x", "to").is_err());
        assert!(validated_identifier_list("", "to").is_err());
    }

    // ── description / schema hygiene ───────────────────────────────────────

    #[test]
    fn description_mentions_guarded() {
        assert!(PgAdmin.description().to_uppercase().contains("GUARDED"));
    }

    #[test]
    fn schema_never_contains_a_secret_shaped_default() {
        let schema = PgAdmin.parameters();
        let s = schema.to_string();
        assert!(!s.to_lowercase().contains("postgres://"));
    }

    #[test]
    fn name_is_pg_admin() {
        assert_eq!(PgAdmin.name(), "pg_admin");
    }
}
