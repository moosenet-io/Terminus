# The Postgres tool suite — the single sanctioned Postgres door (S115 / PGT)

The `pg_*` tools are the **one governed path** every agent, client, and tool uses to reach
Postgres. They replace direct `ssh <host>` + `psql` for ad-hoc database work, so every schema,
data, or role change is identity-scoped, guarded where destructive, and audited.

## The rule (S9 applied to Postgres)

Postgres has exactly one door: the Terminus `pg_*` core tools. Concretely:

- **No agent SSHes to a DB host to run `psql`** for schema/data/role changes. Use `pg_query`,
  `pg_execute`, `pg_ddl`, or `pg_admin`.
- **No script, CLI, or codebase reads a `POSTGRES_URL_*` secret to open its own admin/ad-hoc
  connection.** That is the same duplicated-access-path hazard S9 kills for GitHub/Gitea/Plane.
- Everything routes through the tools, so every mutation is attributable to one identity, gated
  where destructive, and lands in the sanitized audit log.

## The tools

| Tool | Purpose | Default identity | Guarded? |
|---|---|---|---|
| `pg_identities` | List configured connection identities + tiers (never secrets) | — | no |
| `pg_query` | One read-only statement (SELECT/WITH/EXPLAIN/SHOW), bound params, row-capped | `readonly` | no |
| `pg_list_tables` / `pg_describe_table` | Schema introspection | `readonly` | no |
| `pg_execute` | One parameterized DML (INSERT/UPDATE/DELETE [+RETURNING]) | `writer` | **yes** |
| `pg_ddl` | One DDL statement (CREATE/ALTER/DROP TABLE/INDEX/VIEW/SEQUENCE/SCHEMA) | `admin` | **yes** |
| `pg_admin` | One role/privilege statement (CREATE/ALTER/DROP ROLE·USER, GRANT, REVOKE) | `admin` | **yes** |

## Identity / role model (user-level control)

Each connection identity maps to a **DB role** of a specific privilege tier, resolved at call
time from `POSTGRES_URL_<NAME>` (materialized into the process env by the operator's secret
manager, the same convention `plane`'s `PLANE_PAT_<NAME>` uses — never a raw literal, never
logged). The `identity` arg on every tool selects the role:

- `readonly` — `SELECT` only. The default; safe by default. The DB itself rejects any write.
- `writer` — DML. Cannot alter schema or roles (DB-enforced).
- `admin` — DDL + role/privilege management.

This is defense in depth: **the DB role is the real privilege boundary**, on top of the
guarded-tool approval gate below. A `readonly` identity that somehow reached `pg_ddl` is still
refused by Postgres.

## Guarding + audit

The three mutating tools (`pg_execute`, `pg_ddl`, `pg_admin`) are in
`crate::approval::GUARDED_BARE_NAMES` and call `approval::gate(...)` at the top of their execute
(after statement-class validation, before any DB connection) — so each call is a per-occurrence
human-approved operation (via the `tool_approvals` gate), exactly like `openhands`/`<secret-manager>`.
The four read tools are not guarded.

Every `pg_*` call is captured by the gateway's sanitized audit pipeline. `pg_admin` additionally
redacts any `PASSWORD '...'` literal to `PASSWORD '***REDACTED***'` before anything reaches the
approval-gate summary, the audit args, or the tool's response — a real password only ever lives
in the local string used to execute the statement.

> Refinement option: guarding is currently whole-tool (every DML needs approval), matching the
> operator's control-first posture. `pg_execute` already emits a `destructive` flag (no-`WHERE`
> DELETE/UPDATE, TRUNCATE); a future change could gate only destructive-shaped DML and let
> ordinary WHERE-qualified writes through audited-but-ungated, if routine automated writes need
> to flow without per-call approval.

## The exemption boundary (why the sweep is never disrupted)

The `pg_*` suite governs **agent / admin / ad-hoc** access. It does **not** replace the
application's own governed `sqlx` data path:

- The MINT sweep (`intake_coder_sweep` / `intake_assistant_sweep`) keeps its direct `PgPool`
  (`intake::storage::get_pool`) for its high-throughput row writes.
- The fleet-catalog and discovery-brochure read/write tools keep their own in-process pool.

Routing millions of sweep row-writes through an MCP tool call would be absurd and would break the
running sweep. So those paths are explicitly out of scope — the exhaustive sweep runs through and
after this deploy untouched.

## Operator provisioning (ops, not code)

Before the tools can connect, the operator must, per target DB (starting with <host>
`lumina_intake`, which has pgvector):

1. Create the three DB roles: a read-only role (`SELECT`), a writer role (DML), and an admin role
   (DDL + role management) — least privilege each.
2. Materialize `POSTGRES_URL_READONLY` / `POSTGRES_URL_WRITER` / `POSTGRES_URL_ADMIN` into the
   Terminus gateway's runtime env (the same secret-materialization path the rest of the fleet
   uses) — never a literal in source or a committed `.env`.
3. Deploy the rebuilt gateway (`terminus-primary`) so the tools go live. This is a
   gateway-only restart — it does **not** touch chord or the sweep, and chord's RESIL-01 lease
   persistence would in any case keep a mid-sweep GPU lease across a chord restart.

Until the `POSTGRES_URL_*` secrets are provisioned, the tools register and are discoverable but
return a clean `NotConfigured` (naming the role, never a URL) — inert, not broken.

## See also

- `src/pg/` — the tool implementations. `src/approval.rs` — the guarded set + gate.
- `src/plane/mod.rs` — the identity convention this mirrors.
- The moosenet-spec skill's S9 (single-access-path) — the same principle, now covering Postgres.
