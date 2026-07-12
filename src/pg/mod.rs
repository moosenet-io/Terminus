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
//! ## Guarding (future items)
//! `pg_identities` (this item) is read-only and NOT guarded. `pg_execute` /
//! `pg_ddl` / `pg_admin` (PGT-03/04/05) are destructive and MUST be added to
//! `crate::approval::GUARDED_BARE_NAMES` when they land — every mutating
//! `pg_*` tool added to this module MUST be evaluated for the guarded set,
//! per PGT-06's governance rule.
//!
//! ## Exemption boundary (load-bearing, do not blur)
//! This suite governs AGENT/admin/ad-hoc Postgres access. It does NOT
//! replace the application's own governed `sqlx` data paths — the MINT sweep
//! (`crate::intake::storage::get_pool`), the fleet-catalog/discovery
//! read+write tools, and any other in-process data path keep their direct
//! `PgPool`, unchanged and unrouted through this suite. See `S115`'s
//! "Grounding summary" for the full rationale.

pub mod conn;
pub mod identities;

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
}
