## Postgres tool suite — the single sanctioned Postgres door (S115)

Coder agents historically SSHed directly into DB hosts and ran `psql` for
schema/data/role changes: unaudited, ungoverned, host-level DB access. The
`pg_*` tools (`src/pg/`) are the ONE sanctioned, audited, identity-scoped
door for all agent/client/tool Postgres access — no more direct SSH+`psql`.
This is the same S9 single-door posture Terminus already applies to
GitHub/Gitea/Plane, applied to Postgres.

**Status:** PGT-01 shipped the connection/identity foundation and the
read-only `pg_identities` tool. PGT-02 adds the read surface (`pg_query` /
`pg_list_tables` / `pg_describe_table`); PGT-04 adds `pg_ddl` (schema DDL);
PGT-03 adds `pg_execute` (DML); PGT-05 adds `pg_admin` (roles/GRANT/REVOKE).
PGT-06 wires all three mutating tools into the gateway's per-occurrence
approval gate (see "Governance" below) — the suite is now fully guarded.

### Read tools (PGT-02)

All three default to the least-privileged `readonly` connection identity and
are **not** guarded (read-only, no destructive capability) — same audit
posture as every other tool call.

- **`pg_query`** — runs exactly ONE read-only statement: `SELECT`,
  `WITH ... SELECT` (a CTE), `EXPLAIN`, or `SHOW`. Args:
  `{ sql, params?, identity?, max_rows? }`. `sql` must contain a single
  statement — no `;`-chained multi-statement input — and no DML/DDL
  keyword anywhere in the body (this also rejects a CTE that smuggles an
  `INSERT`/`UPDATE`/`DELETE`/`DROP`/etc. inside a `WITH` clause). Any
  violation is a clean `InvalidArgument` pointing at `pg_execute`/`pg_ddl`
  instead. Values are passed as bound `$1, $2, ...` `params` and are
  **always** bound via `sqlx`'s typed `Encode`, never string-interpolated
  into the SQL text — SQL-injection safe by construction. Results are
  row-capped (`max_rows`, default 500, hard ceiling 5000) and the response
  reports `{ columns, rows, row_count, truncated }`.
- **`pg_list_tables`** — lists tables visible to the connection (via
  `information_schema.tables`), optionally restricted to one `schema`. Args:
  `{ schema?, identity? }`.
- **`pg_describe_table`** — describes one table's columns
  (name/type/nullable/default), primary key, and indexes. Args:
  `{ table, schema? (default "public"), identity? }`. A non-existent table
  is a clean `NotFound`, not a panic.

`pg_list_tables`/`pg_describe_table` validate `schema`/`table` against a
conservative Postgres-identifier charset (`[A-Za-z_][A-Za-z0-9_]*`, max 63
bytes) before splicing them into the introspection query (identifiers cannot
be bound as ordinary query parameters); a name that fails it is a clean
`InvalidArgument`.

### `pg_ddl` — schema DDL (PGT-04)

Runs a single schema-DDL statement: `CREATE`/`ALTER`/`DROP` on `TABLE` /
`INDEX` / `VIEW` (including `MATERIALIZED VIEW`) / `SEQUENCE` / `SCHEMA`.
Args: `{ sql, identity? }`. Default identity: **`admin`** (the DB role is the
real privilege boundary, matching every other `pg_*` tool's identity model).

A pure string-level statement-class gate (`src/pg/ddl.rs::classify_ddl`, unit
tested without a DB connection) runs before any connection is attempted:

- Accepts only a single statement (one optional trailing `;`; any other `;`
  is rejected as multi-statement input).
- Accepts only `CREATE`/`ALTER`/`DROP` as the leading keyword — DML
  (`INSERT`/`UPDATE`/`DELETE`) and reads (`SELECT`/`EXPLAIN`/`SHOW`) are
  rejected with a clean `InvalidArgument` pointing at `pg_execute`/`pg_query`.
- Rejects role/privilege management (`CREATE`/`ALTER`/`DROP ROLE`/`USER`/
  `GROUP`, `GRANT`, `REVOKE`) even though some share a leading keyword with
  schema DDL — those belong to `pg_admin` (PGT-05).
- Rejects a DDL statement whose target object isn't one of
  `TABLE`/`INDEX`/`VIEW`/`SEQUENCE`/`SCHEMA` (e.g. `CREATE EXTENSION`).

`DROP` statements, and `ALTER` statements that themselves contain a `DROP`
(dropping a column/constraint/default), are flagged `irreversible: true` in
both the response summary and structured payload, so an approval prompt or
audit reviewer can immediately see the blast radius. Returns
`{ statement_class, object, irreversible, identity, ok }`.

`pg_ddl` is destructive by design and is **GUARDED** (PGT-06): it is in
`crate::approval::GUARDED_BARE_NAMES` and calls `crate::approval::gate(...)`
itself at the top of `execute_structured`, after the statement-class gate and
before any DB connection is attempted — every call requires per-occurrence
operator approval. See the note at the bottom of `src/pg/ddl.rs`.

### `pg_execute` — parameterized DML (PGT-03)

`pg_execute` runs exactly one bound-parameter `INSERT`/`UPDATE`/`DELETE`
(optionally with `RETURNING`) against a connection identity — args
`{ sql, params?, identity? }`. Anything that isn't a single DML statement is
a clean `InvalidArgument` pointing at the right tool: a read (`SELECT`/
`WITH`/`EXPLAIN`/`SHOW`) → `pg_query`; DDL (`CREATE`/`ALTER`/`DROP`/
`TRUNCATE`/...) → `pg_ddl`; role/privilege statements (`GRANT`/`REVOKE`) →
`pg_admin`; multi-statement input (an embedded `;`) is rejected outright.
Values are always bound `params` (`$1`, `$2`, ...), never interpolated into
`sql`.

`pg_execute` defaults to the `writer` connection identity (not the
suite-wide `readonly` default — DML needs a writer-tier DB role), and
returns `{ affected, returning?, destructive, statement_class, identity }`.

**Destructive-shape detection.** A `DELETE`/`UPDATE` with no `WHERE` clause
mutates or removes an entire table's rows in one call — the response's
`destructive: true` flag surfaces that shape (pure string/token check, no
SQL parser) so the audit trail and any guarding logic can see it without
re-parsing the SQL. The same detector (`crate::pg::execute::is_destructive_shape`,
`pub` for reuse) also recognizes a bare `TRUNCATE`, even though
`pg_execute`'s own statement-class gate rejects `TRUNCATE` outright as
DDL-shaped (pointing the caller at `pg_ddl`) — the detector exists as one
shared, reusable classifier for later `pg_*` items, not only for what
`pg_execute` itself accepts.

`pg_execute` is a mutating tool and is **GUARDED** (PGT-06): it is in
`crate::approval::GUARDED_BARE_NAMES` and calls `crate::approval::gate(...)`
itself at the top of `execute_structured`, after the statement-class and
destructive-shape checks and before any DB connection is attempted — every
call requires per-occurrence operator approval, on top of the DB-role
privilege boundary and the standard gateway audit trail.

### `pg_admin` — role/privilege management (PGT-05, guarded)

`pg_admin` runs exactly one role/privilege statement — `CREATE`/`ALTER`/`DROP ROLE`|`USER`,
`GRANT`, or `REVOKE` — via either a structured `{ action, role, options, password, privileges,
on, to, from }` form (preferred, so a password never has to be hand-formatted into a loggable
`sql` string) or a raw single-statement `sql` string. Anything else (DDL/DML/reads/multi-statement)
is a clean `InvalidArgument` pointing at `pg_ddl`/`pg_execute`/`pg_query`. Default identity:
**`admin`**. Guarded — it calls the approval gate at the top of its execute.

**Password redaction (mandatory).** Any `PASSWORD '...'` literal is rewritten to
`PASSWORD '***REDACTED***'` before anything reaches the approval-gate summary, the audit args, or
the tool response — the real password only ever lives in the local string used to run the
statement. `DROP ROLE`/`REVOKE` are flagged `high_impact`.

### Identity / connection model

Every `pg_*` tool accepts an optional `identity` argument selecting which
Postgres connection/DB-ROLE the call authenticates as — exactly mirroring how
every Plane tool accepts an optional `identity` argument for `PLANE_PAT_<NAME>`
(see "Unified `Principal` identity" above). A connection identity `<name>` is
configured by setting a `POSTGRES_URL_<NAME>` secret (e.g.
`POSTGRES_URL_READONLY`, `POSTGRES_URL_WRITER`, `POSTGRES_URL_ADMIN`) to a
connection string authenticated as a DB ROLE scoped to that privilege level —
the DB role, not the tool code, is the real privilege boundary. Omitting
`identity` uses the least-privileged `readonly` — safe by default, even for a
call that reaches a tool it shouldn't have.

`pg_identities` lists the configured connection NAMES and a name-derived
privilege tier (`readonly`/`writer`/`admin`/`unknown`) — never a secret
value. Read-only, not guarded.

### Secret access

terminus-rs has no separate `SecretManager::get()` / `vault::manager()` API
of its own (see the `crate::pki` module docs for the full rationale): the
runtime secret store is materialized into this process's environment at
startup by the operator's secret manager, so a plain env read afterward
already IS the "vault" read in this crate's established convention — the
same convention `PLANE_PAT_<NAME>` uses. `src/pg/conn.rs`'s
`scan_named_connections` is the ONE place `POSTGRES_URL_<NAME>` is read; no
URL value is ever logged, displayed in an error, or embedded in a tool
result — only identity NAMES and tiers are ever surfaced. An identity with no
configured secret is refused with a clean "not configured" error naming the
role, never guessing a fallback connection.

### Governance and the exemption boundary

Full governance runbook (single-door rule, identity/role model, exemption boundary, operator provisioning): [`docs/tools/postgres-suite.md`](docs/tools/postgres-suite.md).


This suite is the single door for AGENT/admin/ad-hoc Postgres access. It does
**not** replace the application's own governed `sqlx` data paths — the MINT
sweep (`crate::intake::storage::get_pool`), the fleet-catalog/discovery
read+write tools, and any other in-process data path keep their direct
`PgPool`, unrouted through this suite and undisturbed by it.

The three mutating `pg_*` tools — `pg_execute`, `pg_ddl`, `pg_admin` — are
**guarded** (PGT-06): each is registered in
`crate::approval::GUARDED_BARE_NAMES` (so a federated/mesh call is gated at
the gateway before it can be laundered through a remote upstream) AND each
calls `crate::approval::gate(...)` itself at the top of its
`execute`/`execute_structured`, after statement-class validation (and, for
`pg_admin`, after password redaction — see `src/pg/admin.rs`'s S6 note) and
before any DB connection is attempted — no mutating call reaches Postgres
without per-occurrence operator approval via the `tool_approvals` gate. This
is on top of, not instead of, the DB-role privilege boundary and the
standard gateway audit trail every tool call already gets. The four
read-only tools — `pg_query`, `pg_list_tables`, `pg_describe_table`,
`pg_identities` — are deliberately **not** guarded. Every future mutating
`pg_*` tool added to this suite MUST be evaluated for the guarded set.

`pg` registers on the CORE tool registry only (`crate::registry::register_all`,
alongside `crate::intake::register`) — Chord-served, never the
`terminus_personal`/<host> personal registry.

