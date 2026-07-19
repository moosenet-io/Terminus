# pg

`src/pg` — 219 KG symbols.

The `pg_*` suite is the single sanctioned Postgres door for agents. It replaced
the historical pattern of agents SSHing into DB hosts and running `psql` —
unaudited, host-level access — with a governed tool surface: named connection
identities mapped to privilege-scoped DB roles, statement classification that
forces each kind of SQL through the right tool, and per-occurrence operator
approval on every mutating call. The default identity is the least-privileged
`readonly`, so a caller that never specifies an identity is safe by
construction.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `pg::conn` | module | `src/pg/conn.rs` | Connection/identity foundation: resolves a named identity to its `POSTGRES_URL_<NAME>` connection; shares the `identity` schema convention (`with_identity_param`, `identity_param_schema`). |
| `pg::execute::classify_dml` | fn | `src/pg/execute.rs` | Validates that `pg_execute` input is exactly one bound-parameter `INSERT`/`UPDATE`/`DELETE` (optionally `RETURNING`); reads, DDL, admin, and multi-statement input are rejected toward the right tool. |
| `pg::ddl::classify_ddl` | fn | `src/pg/ddl.rs` | Statement classifier for `pg_ddl` (schema changes only). |
| `pg::admin::classify_admin_statement` | fn | `src/pg/admin.rs` | Classifier for `pg_admin` (roles/privileges/administration). |
| `pg::admin::redact_password` | fn | `src/pg/admin.rs` | Redacts password material from admin SQL before it is logged or echoed (regex via `password_regex`). |
| `pg::admin::render_structured` | fn | `src/pg/admin.rs` | Renders the structured admin-call result (statement + class) for `execute_structured` output. |

## Tools

7 tools: `pg_identities` and the read surface (`pg_query`, `pg_list_tables`,
`pg_describe_table`) are read-only and unguarded; the three mutating tools —
`pg_execute` (DML), `pg_ddl` (schema), `pg_admin` (roles/admin) — are listed in
`approval::GUARDED_BARE_NAMES` *and* call `approval::gate(...)` at the top of
their execute path, after statement-class validation and before any DB
connection is attempted. Every future mutating `pg_*` tool must be evaluated
for the guarded set.

## How it connects

Registered on the **core registry only** (`register_all`) — Chord-served, never
the personal deployment, matching the scoping of the rest of the
build-pipeline-facing surface. Unqualified `DELETE`/`UPDATE` (no `WHERE`) is
detected as destructive-shaped and surfaced as such through the approval gate.
The exemption boundary is deliberate and load-bearing: this suite governs
agent/ad-hoc access; services' own runtime DB pools (e.g. intake storage via
`DATABASE_URL`/`INTAKE_DATABASE_URL`) are not routed through it.

## Configuration

`POSTGRES_URL_<NAME>` — one connection string per named identity
(`readonly`/`writer`/`admin` by convention, any operator-provisioned name
works), each authenticated as a DB role scoped to that privilege level. Names
only; values from the vault.

## Notes and gaps

This page does not cover the approval-gate mechanics themselves
(`src/approval.rs`, the `tool_approvals` flow) or per-tool JSON schemas — see
[docs/tools/postgres-suite.md](../tools/postgres-suite.md) and the
[S115 reference page](postgres-tool-suite-the-single-sanctioned-postgres-door-s115.md).
