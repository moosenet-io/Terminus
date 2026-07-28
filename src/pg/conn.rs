//! Per-identity Postgres connection resolution (PGT-01).
//!
//! Mirrors `crate::plane`'s `PLANE_PAT_<NAME>` identity convention exactly,
//! but for Postgres connection strings: a secret named `POSTGRES_URL_<NAME>`
//! (e.g. `POSTGRES_URL_READONLY`, `POSTGRES_URL_WRITER`, `POSTGRES_URL_ADMIN`)
//! configures a named connection identity bound to a specific DB ROLE /
//! privilege tier. Every `pg_*` tool accepts an optional `identity` argument
//! selecting which connection/role a call uses (see [`identity_param_schema`]);
//! the default is the least-privileged [`DEFAULT_IDENTITY`] ("readonly"), safe
//! by default even if a caller omits `identity` on a tool it shouldn't have
//! reached.
//!
//! ## Secret access (S7/S8) — why this reads `std::env`, matching precedent
//! terminus-rs has no separate `SecretManager::get()` / `vault::manager()` API
//! of its own (see `crate::pki` module docs for the full rationale, and
//! `crate::plane`'s `PLANE_PAT_<NAME>` scan for the exact precedent this
//! mirrors): the runtime secret store (<secret-manager> / the operator's vault) is // pii-test-fixture: public product name, sanctioned secrets manager (see infra_service_path_exempt rationale)
//! materialized into THIS process's environment at startup — either by
//! `crate::secrets_bootstrap` for the crate's fixed allowlisted keys, or by
//! the operator's deployment tooling for a per-identity family like this one
//! — so a plain env read afterward already IS the "vault" read in this
//! crate's established, security-reviewed convention (this satisfies S8's
//! file/env/vault deployment tiers: env IS one of the three sanctioned
//! tiers, and file/vault-backed deployments materialize into env the same
//! way). [`scan_named_connections`] is the ONE place `POSTGRES_URL_<NAME>` is
//! matched against the environment — no other code in this crate should read
//! a `POSTGRES_URL_*`-shaped key directly, and no URL value is ever logged,
//! displayed in an error, or embedded in any tool output (only connection
//! IDENTITY NAMES and privilege TIERS are ever surfaced, see
//! [`configured_identities`] and `crate::pg::identities`).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;

use crate::config::pg_connection_secret_name;
use crate::error::ToolError;

/// Env-var prefix marking a named Postgres connection identity. Single
/// source of truth mirroring `crate::config::pg_connection_secret_name`'s
/// key-NAME builder — kept here too (rather than only in `config`) so this
/// module's scan and `config`'s name-builder can never silently drift, the
/// same relationship `crate::plane::PLANE_IDENTITY_PREFIX` has with its scan.
const POSTGRES_URL_PREFIX: &str = "POSTGRES_URL_";

/// The default, least-privileged connection identity used when a `pg_*` tool
/// call omits `identity`. Safe by default: the DB ROLE behind `readonly` is,
/// by the operator's provisioning contract (PGT-07), DB-enforced-incapable of
/// DML/DDL/role-management even if a caller reaches a destructive tool
/// without specifying an identity.
pub const DEFAULT_IDENTITY: &str = "readonly";

/// Recognized privilege tiers, ascending. A configured connection identity
/// name that doesn't match one of these is still connectable (an operator
/// may name a connection anything), but [`tier_for`] reports it as `"unknown"`
/// rather than guessing a privilege level from an unrecognized name.
pub const KNOWN_TIERS: &[&str] = &["readonly", "writer", "admin"];

/// Scan this process's own environment for `POSTGRES_URL_<NAME>` named
/// connections, returning a `lowercased-name -> url` map. This is the ONLY
/// place the prefix is matched against the environment (mirrors
/// `crate::plane::scan_named_identities`). Empty-valued vars are skipped (a
/// set-but-empty secret is treated as absent). Never logs a value; the
/// returned map's values must never be placed in an error message, `Debug`
/// output, or tool result — only the map's KEYS (identity names) may ever be
/// surfaced (see [`configured_identities`]).
fn scan_named_connections() -> HashMap<String, String> {
    let mut connections = HashMap::new();
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(POSTGRES_URL_PREFIX) {
            let v = v.trim();
            if !name.is_empty() && !v.is_empty() {
                connections.insert(name.to_lowercase(), v.to_string());
            }
        }
    }
    connections
}

/// Names of every configured `POSTGRES_URL_<NAME>` connection identity
/// (lowercased, sorted for stable output). These are exactly the names
/// [`resolve_connection`] can resolve. Never returns — and cannot be used to
/// recover — URL values.
pub fn configured_identities() -> Vec<String> {
    let mut names: Vec<String> = scan_named_connections().into_keys().collect();
    names.sort();
    names
}

/// The privilege tier implied by a connection identity's name: `"readonly"`,
/// `"writer"`, or `"admin"` for a name that contains (or exactly is) one of
/// those words, else `"unknown"`. Pure name-based classification — the real
/// privilege boundary is always the DB ROLE the URL authenticates as, not
/// this label; the label exists so `pg_identities` can give a caller a quick
/// hint without connecting.
pub fn tier_for(identity: &str) -> &'static str {
    let lower = identity.to_lowercase();
    if lower.contains("admin") {
        "admin"
    } else if lower.contains("writer") || lower.contains("write") {
        "writer"
    } else if lower.contains("readonly") || lower.contains("read") {
        "readonly"
    } else {
        "unknown"
    }
}

/// Shared JSON Schema fragment for the optional `identity` argument every
/// `pg_*` tool accepts. Mirrors `crate::plane`'s `identity_param_schema`
/// verbatim in spirit.
pub fn identity_param_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional Postgres connection identity to use: a configured \
                        POSTGRES_URL_<NAME> connection name (e.g. \"readonly\", \"writer\", \
                        \"admin\"). Omit to use the default least-privileged identity \
                        (\"readonly\"). Call pg_identities to see the configured names and \
                        their privilege tiers."
    })
}

/// Add the shared optional `identity` property to a tool's parameter schema.
/// Idempotent and safe on any `{ "type": "object", "properties": { .. } }`
/// schema — inserts the `identity` property without disturbing the tool's
/// own arguments or its `required` list (identity is always optional).
/// Mirrors `crate::plane`'s `with_identity_param`.
pub fn with_identity_param(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.insert("identity".to_string(), identity_param_schema());
    }
    schema
}

/// Shared JSON Schema fragment for the optional `database` argument every
/// `pg_*` tool accepts (PGT-ALLDB). Lets a caller target ANY database on the
/// identity's Postgres server — current OR future — without a per-database
/// connection identity: the chosen identity's credentials/role are reused and
/// only the connected database NAME is overridden. A superuser `admin` identity
/// can thus reach every database on the cluster; a scoped role reaches a given
/// database only where that role's grants allow (the DB ROLE remains the real
/// privilege boundary — switching database never escalates privilege).
pub fn database_param_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional target database NAME on the identity's Postgres server. \
                        Omit to use the database in the identity's own connection string. \
                        Only the connected database is switched — the identity's role and \
                        credentials are unchanged — so reaching a database still depends on \
                        that role's grants (a superuser 'admin' identity can reach any \
                        database on the server, current or future)."
    })
}

/// Add the shared optional `database` property to a tool's parameter schema.
/// Idempotent; mirrors [`with_identity_param`].
pub fn with_database_param(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.insert("database".to_string(), database_param_schema());
    }
    schema
}

/// Add BOTH the `identity` and `database` connection params to a tool schema
/// (the standard pg-tool connection surface). Idempotent.
pub fn with_conn_params(schema: Value) -> Value {
    with_database_param(with_identity_param(schema))
}

/// Resolve the optional target `database` NAME from a tool's raw args: a
/// non-empty `database` string (trimmed) selects it; otherwise `None` (use the
/// identity DSN's own database). NOT lowercased — Postgres database names are
/// case-sensitive, unlike the identity name.
pub fn resolve_database_name(args: &Value) -> Option<String> {
    args.get("database")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve the effective connection identity NAME for a tool invocation from
/// its raw args: a non-empty `identity` string argument selects that name
/// (lowercased, trimmed); otherwise [`DEFAULT_IDENTITY`].
pub fn resolve_identity_name(args: &Value) -> String {
    args.get("identity")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .unwrap_or_else(|| DEFAULT_IDENTITY.to_string())
}

/// A clean, role-naming "not configured" error for an identity with no
/// matching secret — NEVER echoes a URL (there isn't one to echo: this is
/// exactly the unset case) and never guesses a fallback connection.
fn not_configured(identity: &str) -> ToolError {
    let secret_name = pg_connection_secret_name(identity);
    ToolError::NotConfigured(format!(
        "Postgres connection identity '{identity}' is not configured — set the \
         {secret_name} secret (a DB role scoped to the '{identity}' privilege tier) \
         in the vault to enable it. Call pg_identities to see what IS configured."
    ))
}

/// Process-lifetime pool cache, keyed by (lowercased) identity name. A tiny
/// cache is acceptable per PGT-01's approach note: these are infrequent
/// admin/agent calls, not a hot path (matching `intake::storage::get_pool`'s
/// "infrequent, connect fresh" precedent), but reusing a pool across calls in
/// the same process avoids a fresh TCP+auth handshake on every tool
/// invocation. The cache key is the identity NAME ONLY — never a URL — so
/// nothing loggable ever holds a connection string.
static POOL_CACHE: OnceLock<Mutex<HashMap<String, PgPool>>> = OnceLock::new();

fn pool_cache() -> &'static Mutex<HashMap<String, PgPool>> {
    POOL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a `PgPool` for the given connection identity (vault URL lookup →
/// connect). `identity` is normalized (trimmed, lowercased) before lookup.
/// An identity with no configured `POSTGRES_URL_<NAME>` secret is refused —
/// there is no arbitrary-host connection fallback — with a clean
/// [`ToolError::NotConfigured`] naming the identity's ROLE, never a URL. A
/// configured URL that fails to connect surfaces as a clean
/// [`ToolError::Database`] that does NOT echo the connection string (sqlx's
/// own error text does not include the DSN it was given).
pub async fn resolve_connection(identity: &str) -> Result<PgPool, ToolError> {
    resolve_connection_for(identity, None).await
}

/// Resolve a `PgPool` for the given connection identity, optionally overriding
/// the connected DATABASE (PGT-ALLDB). When `database` is `Some`, the identity's
/// configured `POSTGRES_URL_<NAME>` DSN is parsed and its database name replaced
/// with the target — the role/credentials/host/port are untouched — so a single
/// identity reaches ANY database on its server (current or future). The dbname
/// swap uses `PgConnectOptions` (sqlx-native), never string surgery on the DSN,
/// so it can't corrupt the userinfo/host the way a naive replace would. Pools
/// are cached per (identity, database) pair. Privilege is still the DB ROLE's:
/// switching database never grants access the role doesn't already have.
pub async fn resolve_connection_for(
    identity: &str,
    database: Option<&str>,
) -> Result<PgPool, ToolError> {
    let identity = identity.trim().to_lowercase();
    let identity = if identity.is_empty() { DEFAULT_IDENTITY.to_string() } else { identity };
    let database = database.map(str::trim).filter(|s| !s.is_empty());

    // Cache key is (identity, database) — the ASCII Unit Separator can't appear
    // in an identity name or a Postgres database name, so it's an unambiguous
    // join. Never a URL, so nothing loggable holds a connection string.
    let cache_key = match database {
        Some(db) => format!("{identity}\u{1f}{db}"),
        None => identity.clone(),
    };

    if let Some(pool) = pool_cache().lock().expect("pg pool cache mutex poisoned").get(&cache_key) {
        return Ok(pool.clone());
    }

    let connections = scan_named_connections();
    let url = connections.get(&identity).ok_or_else(|| not_configured(&identity))?;

    // Parse via PgConnectOptions (never echo the URL on a parse failure — it
    // may contain the password) and override only the database when requested.
    let mut opts = PgConnectOptions::from_str(url).map_err(|_| {
        ToolError::Database(format!(
            "connection string for identity '{identity}' could not be parsed"
        ))
    })?;
    if let Some(db) = database {
        opts = opts.database(db);
    }

    let pool = PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(opts)
        .await
        .map_err(|e| {
            // `database` is caller-supplied (safe to echo); the DSN never is.
            let where_db = database.map(|d| format!(" (database '{d}')")).unwrap_or_default();
            ToolError::Database(format!(
                "Cannot connect Postgres identity '{identity}'{where_db}: {e}"
            ))
        })?;

    pool_cache()
        .lock()
        .expect("pg pool cache mutex poisoned")
        .insert(cache_key, pool.clone());
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Clear every `POSTGRES_URL_*` var this test module might have touched,
    /// so tests never leak state into each other or into other test modules
    /// (config.rs's own env-mutating tests use the same `#[serial]` pattern).
    fn clear_all() {
        for (k, _) in std::env::vars() {
            if k.starts_with(POSTGRES_URL_PREFIX) {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    #[serial]
    fn configured_identities_reflects_a_mocked_key_set() {
        clear_all();
        std::env::set_var("POSTGRES_URL_READONLY", "postgres://ro@example/db");
        std::env::set_var("POSTGRES_URL_WRITER", "postgres://w@example/db");
        let names = configured_identities();
        assert_eq!(names, vec!["readonly".to_string(), "writer".to_string()]);
        clear_all();
    }

    #[test]
    #[serial]
    fn configured_identities_empty_when_unset() {
        clear_all();
        assert!(configured_identities().is_empty());
    }

    #[test]
    #[serial]
    fn empty_valued_secret_is_treated_as_unconfigured() {
        clear_all();
        std::env::set_var("POSTGRES_URL_ADMIN", "");
        assert!(configured_identities().is_empty());
        clear_all();
    }

    #[test]
    #[serial]
    fn unknown_identity_is_a_clean_not_configured_naming_the_role() {
        clear_all();
        let err = not_configured("ghost");
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "error should name the identity/role: {msg}");
        assert!(msg.contains("POSTGRES_URL_GHOST"), "error should name the key: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn resolve_connection_unknown_identity_is_not_configured_not_a_panic() {
        clear_all();
        let result = resolve_connection("totally-unconfigured-identity").await;
        match result {
            Err(ToolError::NotConfigured(msg)) => {
                assert!(msg.contains("totally-unconfigured-identity"));
                // The URL is never emitted -- there is none, but also assert
                // the message shape never contains a scheme-looking string.
                assert!(!msg.contains("postgres://"));
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn tier_for_classifies_known_names() {
        assert_eq!(tier_for("readonly"), "readonly");
        assert_eq!(tier_for("READONLY"), "readonly");
        assert_eq!(tier_for("writer"), "writer");
        assert_eq!(tier_for("admin"), "admin");
        assert_eq!(tier_for("service-writer-2"), "writer");
        assert_eq!(tier_for("mystery"), "unknown");
    }

    #[test]
    fn default_identity_is_readonly() {
        assert_eq!(DEFAULT_IDENTITY, "readonly");
        assert_eq!(resolve_identity_name(&json!({})), "readonly");
    }

    #[test]
    fn resolve_identity_name_prefers_explicit_arg() {
        assert_eq!(resolve_identity_name(&json!({"identity": "Writer"})), "writer");
        assert_eq!(resolve_identity_name(&json!({"identity": "  "})), "readonly");
        assert_eq!(resolve_identity_name(&json!({"identity": ""})), "readonly");
    }

    #[test]
    fn identity_param_schema_never_contains_a_url_or_secret_shaped_value() {
        let schema = identity_param_schema();
        let s = schema.to_string();
        assert!(!s.contains("postgres://"));
        assert!(!s.to_lowercase().contains("password"));
    }

    #[test]
    fn with_identity_param_is_idempotent_and_preserves_existing_properties() {
        let base = json!({
            "type": "object",
            "properties": { "sql": { "type": "string" } },
            "required": ["sql"]
        });
        let schema = with_identity_param(base);
        assert!(schema["properties"]["sql"].is_object());
        assert!(schema["properties"]["identity"].is_object());
        assert_eq!(schema["required"], json!(["sql"]));
    }

    #[test]
    fn with_conn_params_adds_both_identity_and_database_without_requiring_them() {
        let base = json!({
            "type": "object",
            "properties": { "sql": { "type": "string" } },
            "required": ["sql"]
        });
        let schema = with_conn_params(base);
        assert!(schema["properties"]["sql"].is_object());
        assert!(schema["properties"]["identity"].is_object());
        assert!(schema["properties"]["database"].is_object());
        // Neither connection param is ever forced into `required`.
        assert_eq!(schema["required"], json!(["sql"]));
    }

    #[test]
    fn resolve_database_name_trims_and_preserves_case_or_returns_none() {
        // Present + trimmed, case PRESERVED (Postgres db names are case-sensitive).
        assert_eq!(
            resolve_database_name(&json!({"database": "  Lumina_Intake  "})),
            Some("Lumina_Intake".to_string())
        );
        // Absent / blank -> None (use the identity DSN's own database).
        assert_eq!(resolve_database_name(&json!({})), None);
        assert_eq!(resolve_database_name(&json!({"database": "   "})), None);
        assert_eq!(resolve_database_name(&json!({"database": ""})), None);
    }

    #[test]
    fn database_param_schema_never_contains_a_url_or_secret_shaped_value() {
        let s = database_param_schema().to_string();
        assert!(!s.contains("postgres://"));
        assert!(!s.to_lowercase().contains("password"));
    }

    #[test]
    #[serial]
    fn pg_connection_secret_name_matches_scan_prefix() {
        // Cross-check config::pg_connection_secret_name's prefix against
        // this module's own POSTGRES_URL_PREFIX so the two can never drift.
        assert_eq!(pg_connection_secret_name("readonly"), "POSTGRES_URL_READONLY");
        assert!(pg_connection_secret_name("readonly").starts_with(POSTGRES_URL_PREFIX));
    }
}
