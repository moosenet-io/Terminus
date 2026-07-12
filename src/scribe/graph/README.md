# Atlas knowledge-graph subsystem

This directory holds the Rust-native Atlas knowledge-graph subsystem of the
Scribe documentation engine (see `mod.rs` for the module overview).

## Findings store (`findings_store.rs`, KGFIND-01)

`FindingsStore` owns the `kg_findings` Postgres/pgvector table: durable,
deduplicated records of analysis findings (lint-like observations, review
notes, anomalies, …) attached to a scope within a project's knowledge graph.

### Table shape

```sql
CREATE TABLE kg_findings (
    id uuid PRIMARY KEY,
    project_id text NOT NULL,
    category text NOT NULL,
    severity text NOT NULL,
    scope_kind text NOT NULL CHECK (scope_kind IN ('node','path','community','global')),
    scope_ref text NOT NULL,
    description text NOT NULL,
    embedding vector(768),
    provenance jsonb NOT NULL DEFAULT '[]'::jsonb,
    first_seen timestamptz NOT NULL DEFAULT now(),
    last_seen timestamptz NOT NULL DEFAULT now(),
    occurrences int NOT NULL DEFAULT 1
);
CREATE INDEX kg_findings_bucket ON kg_findings (project_id, scope_kind, scope_ref, category);
```

A best-effort HNSW cosine index (`kg_findings_embedding_hnsw`) is also
created on `embedding`; if the server's pgvector build doesn't support HNSW,
creation is logged and swallowed rather than failing migration — exact
top-K/threshold queries via `<=>` still work without it.

Migration is idempotent and advisory-locked (same idiom as
`vec_store::AtlasVecStore`, distinct lock key), safe to call repeatedly and
from concurrent callers.

### Dedup / recurrence semantics

`FindingsStore::record(finding, embedding)` looks for an existing row in the
same bucket — `(project_id, scope_kind, scope_ref, category)` — before
inserting a new one:

- **With an embedding**: the nearest existing row (by cosine similarity,
  restricted to rows in the bucket that have a stored embedding) is
  compared against a threshold. If similarity is `>= threshold`, the
  existing row is bumped (`occurrences += 1`, `last_seen = now()`,
  provenance merged) and `RecordOutcome::Recurred { id, occurrences }` is
  returned. Otherwise a new row is inserted.
- **Without an embedding**: dedup falls back to an exact match on
  `(scope, category, description)`.
- The threshold defaults to `0.92` and is overridable via the
  `KGFIND_DEDUP_THRESHOLD` environment variable (parsed as `f32`; falls back
  to the default if unset or unparsable).
- The pure decision logic lives in `dedup_decision(candidate, existing,
  threshold) -> Option<Uuid>` so it's unit-testable without a database.

Provenance is a JSON array; each `record()` call appends one new entry and
the array is capped to the most recent 20 entries (oldest dropped first).

### Configuration

Same DSN resolution as `AtlasVecStore`: `FindingsStore::from_env()` reads
`crate::config::atlas_database_url()` (backed by `ATLAS_DATABASE_URL`, no
`DATABASE_URL` fallback — the atlas store is an isolated database) and
returns `ToolError::NotConfigured` without attempting a connection if unset.

| Env var | Purpose | Default |
|---|---|---|
| `ATLAS_DATABASE_URL` | Postgres DSN for the Atlas pgvector database | none (`NotConfigured` if unset) |
| `KGFIND_DEDUP_THRESHOLD` | Cosine-similarity threshold for treating a new finding as a recurrence | `0.92` |

### Querying

`FindingsStore::list(project_id, scope_kind, category, min_occurrences)`
returns matching rows ordered by `occurrences DESC, last_seen DESC`. All
filters are optional and the query is built with bound parameters only (no
string interpolation of caller-supplied values).
