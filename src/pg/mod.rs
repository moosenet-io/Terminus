//! Postgres tool suite (S115) — the single sanctioned Postgres door.
//!
//! Coder agents historically SSHed directly into DB hosts and ran `psql` for
//! schema/data/role changes: unaudited, ungoverned, host-level DB access.
//! This module is Phase A (PGT-01) of the full suite: the connection/identity
//! foundation everything else (`pg_query`, `pg_execute`, `pg_ddl`, `pg_admin`,
//! …) sits on.
//!
//! ## Identity model
//! A named connection identity (`readonly` / `writer` / `admin`, or any
//! operator-provisioned name) maps to a `POSTGRES_URL_<NAME>` secret carrying
//! a connection string authenticated as a DB ROLE scoped to that privilege
//! level. Every `pg_*` tool accepts an optional `identity` argument selecting
//! which connection/role a call uses (see [`conn::identity_param_schema`]);
//! the default is the least-privileged `readonly` — safe by default even if
//! a caller reaches a tool without specifying `identity`. This exactly
//! mirrors `crate::plane`'s `PLANE_PAT_<NAME>` per-identity convention.
//!
//! ## Registration
//! `pg` registers on the CORE tool registry ONLY (`crate::registry::register_all`,
//! alongside `crate::intake::register`) — Chord-served, never the
//! `terminus_personal`/<host> personal registry, matching how
//! `model_fleet_catalog` and the rest of the build-pipeline-facing core
//! surface are scoped (S9 posture).
//!
//! ## Guarding
//! `pg_identities` (PGT-01) and `pg_query` / `pg_list_tables` /
//! `pg_describe_table` (PGT-02, the read surface) are read-only and NOT
//! guarded — no per-occurrence approval is required to call them. The three
//! mutating tools, `pg_execute` / `pg_ddl` / `pg_admin` (PGT-03/04/05), ARE
//! guarded (PGT-06): each is listed in `crate::approval::GUARDED_BARE_NAMES`
//! AND calls `crate::approval::gate(...)` itself at the top of its
//! `execute`/`execute_structured`, after statement-class validation and
//! before any DB connection is attempted — every call requires per-
//! occurrence operator approval via the `tool_approvals` gate before it
//! reaches Postgres. Every future mutating `pg_*` tool added to this module
//! MUST be evaluated for the guarded set, per PGT-06's governance rule.
//!
//! ## Exemption boundary (load-bearing, do not blur)
//! This suite governs AGENT/admin/ad-hoc Postgres access. It does NOT
//! replace the application's own governed `sqlx` data paths — the MINT sweep
//! (`crate::intake::storage::get_pool`), the fleet-catalog/discovery
//! read+write tools, and any other in-process data path keep their direct
//! `PgPool`, unchanged and unrouted through this suite. See `S115`'s
//! "Grounding summary" for the full rationale.

pub mod admin;
pub mod conn;
pub mod ddl;
pub mod execute;
pub mod identities;
pub mod query;

use crate::registry::ToolRegistry;

/// A thin, stateless handle onto the `pg` connection/identity model. Exists
/// so callers that want a typed "the pg suite" reference (rather than
/// reaching into `crate::pg::conn` functions directly) have one — all real
/// state lives in `conn`'s env scan + pool cache, not here, so this type
/// carries no fields and is cheap to construct anywhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct PgConnections;

impl PgConnections {
    pub fn new() -> Self {
        Self
    }

    /// Names of every configured connection identity. Never a URL/secret.
    pub fn configured_identities(&self) -> Vec<String> {
        conn::configured_identities()
    }

    /// The default (least-privileged) identity name new `pg_*` calls use
    /// when `identity` is omitted.
    pub fn default_identity(&self) -> &'static str {
        conn::DEFAULT_IDENTITY
    }
}

/// Register every `pg_*` tool onto the given registry. Called from
/// `crate::registry::register_all` alongside `crate::intake::register` (core
/// registry only — see module docs).
pub fn register(registry: &mut ToolRegistry) {
    identities::register(registry);
    query::register(registry);
    execute::register(registry);
    ddl::register(registry);
    admin::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_connections_default_identity_is_readonly() {
        assert_eq!(PgConnections::new().default_identity(), "readonly");
    }

    #[test]
    fn register_adds_pg_identities() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("pg_identities"));
    }

    #[test]
    fn register_adds_the_pgt02_read_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("pg_query"));
        assert!(registry.contains("pg_list_tables"));
        assert!(registry.contains("pg_describe_table"));
    }

    // ------------------------------------------------------------------
    // PGT-06 — no dangling guard entry: every `pg_*` name that
    // `crate::approval::is_guarded` classifies as guarded must correspond
    // to a tool that is actually registered by this module (and vice
    // versa within the pg_* namespace) so the guarded set in
    // `GUARDED_BARE_NAMES` can never drift from what's really wired up.
    // ------------------------------------------------------------------
    #[test]
    fn pgt06_guarded_pg_tools_are_all_actually_registered() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);

        let guarded_pg_tools = ["pg_execute", "pg_ddl", "pg_admin"];
        for name in guarded_pg_tools {
            assert!(
                crate::approval::is_guarded(name),
                "{name} should be classified as guarded"
            );
            assert!(
                registry.contains(name),
                "{name} is in GUARDED_BARE_NAMES but is not a registered tool — dangling guard entry"
            );
        }

        let read_pg_tools = ["pg_query", "pg_list_tables", "pg_describe_table", "pg_identities"];
        for name in read_pg_tools {
            assert!(
                !crate::approval::is_guarded(name),
                "{name} is a read tool and must NOT be guarded"
            );
            assert!(registry.contains(name), "{name} should be registered");
        }
    }
}
