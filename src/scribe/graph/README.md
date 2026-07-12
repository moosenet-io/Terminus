# Atlas knowledge-graph subsystem

This directory holds the Rust-native Atlas knowledge-graph subsystem of the
Scribe documentation engine (see `mod.rs` for the module overview).

## Cortex bridge (`cortex_bridge.rs`, KGRULE-05)

`cortex_risk_for_scope(scope_kind, scope_ref) -> Option<f32>` turns a KG scope
(`path`/`node`/`community`/`global`, matching `findings_store::ScopeKind`)
into a best-effort Cortex risk score in `0.0..=1.0`, so rule crystallization
(KGRULE-02) can prioritize high-risk recurring findings. It is intentionally
thin: it does not talk SSH itself, does not read `CORTEX_SSH_*` beyond a
single "is it set" check, and does not know anything about Cortex's wire
protocol — it calls the existing `crate::cortex` tool (`cortex_review`)
through a scratch `ToolRegistry` and parses its JSON response.

**Degrade contract — this function can never fail the caller:**

- Returns `None`, not an `Err` — the return type is `Option<f32>`. It never
  panics.
- `scope_kind` of `"community"` or `"global"` → `None` immediately (Cortex
  has no per-community/global risk concept; only `"path"` and `"node"` are
  supported).
- `CORTEX_SSH_HOST` unset/empty → `None` immediately, with **no SSH attempt
  at all** — this is checked before `crate::cortex` is touched.
- Any failure in the underlying `cortex_review` call (unreachable host, auth
  failure, remote error, `NotConfigured` surfacing late, task join error) →
  `None`.
- A well-formed but risk-less response (including `crate::cortex`'s
  `{"raw": "..."}` shape for non-JSON remote stdout) → `None`.

Risk extraction (`extract_risk`, private, pure, fully unit-tested) looks for
a numeric `risk` or `score` field at the top level and one level deep under
`result`; any value found is clamped to `[0.0, 1.0]` (Cortex's own
documented `risk_score` field is `0-10`, but this bridge's contract is a
normalized `0.0..=1.0` signal, so values are clamped rather than rescaled
against an unverified upstream range). Non-numeric values at those keys are
treated as absent, not as a parse error.

Uses `cortex_review` (not `cortex_scope`) because its documented purpose is
a post-hoc risk *score* for a set of files — exactly what this bridge needs
— whereas `cortex_scope` returns blast-radius with no risk score, and the
crate's other eight Cortex tools return stats/architecture/dependency shapes
with no risk field at all. `repo` is fixed to `"lumina-terminus"` (this
crate), one of `crate::cortex`'s own two known-repo values — not an
infra/host literal.

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

## Rules store (`rules_store.rs`, KGRULE-01)

`RulesStore` owns the `kg_rules` Postgres table: crystallized, durable rules
governing a scope within a project's knowledge graph. A rule is born as a
`candidate` (never enforced) and is only promoted to `active` by an
adversarial `review_run` panel (KGRULE-03) that argues it is earned. Active
rules carry an `enforcement` level and are bi-temporal (`valid_from`/
`valid_to`) so they can be retired without deleting history.

### Table shape

```sql
CREATE TABLE kg_rules (
    id uuid PRIMARY KEY,
    project_id text NOT NULL,
    scope_kind text NOT NULL CHECK (scope_kind IN ('node','path','community','global')),
    scope_ref text NOT NULL,
    category text NOT NULL,
    guidance text NOT NULL,
    enforcement text NOT NULL DEFAULT 'advisory' CHECK (enforcement IN ('advisory','lint-candidate','blocking')),
    status text NOT NULL DEFAULT 'candidate' CHECK (status IN ('candidate','active','retired')),
    provenance jsonb NOT NULL DEFAULT '{}'::jsonb,
    recurrence_at_creation int,
    cortex_risk real,
    created_at timestamptz NOT NULL DEFAULT now(),
    valid_from timestamptz NOT NULL DEFAULT now(),
    valid_to timestamptz
);
CREATE INDEX kg_rules_scope ON kg_rules (project_id, scope_kind, scope_ref, category, status);
```

Migration is idempotent and advisory-locked (same idiom as
`findings_store::FindingsStore`, a distinct lock key), safe to call
repeatedly and from concurrent callers.

### Lifecycle

- **`create_candidate(NewRule)`** — idempotent per `(project_id, scope_kind,
  scope_ref, category)`: if a `candidate` or `active` row already exists for
  that bucket, its id is returned rather than inserting a duplicate. Atomic
  via a transaction-scoped advisory lock keyed by the bucket (mirrors
  `FindingsStore::record`'s TOCTOU-safe pattern), so concurrent crystallize
  calls for the same bucket never double-insert.
- **`promote(id, enforcement, provenance)`** — `candidate` → `active`, sets
  `enforcement` and `provenance` (typically the promotion review result),
  refreshes `valid_from`. Already-`active` is a no-op success (idempotent).
  A missing id is `ToolError::NotFound`; a `retired` id is
  `ToolError::Conflict` (not silently ignored).
- **`retire(id, reason)`** — sets `status = 'retired'`, `valid_to = now()`.
  `reason` is accepted for interface clarity; callers that need it to be
  durable should fold it into `provenance` before calling, as it is not
  persisted as its own column. A missing id is `ToolError::NotFound`.
- **`list_active(project_id, scope_kind?, scope_ref?, category?)`** —
  returns rows with `status = 'active' AND valid_to IS NULL`, ordered by
  enforcement priority (`blocking` > `lint-candidate` > `advisory`) then
  `created_at DESC`. All filters are optional and bound.
- **`is_active(status, valid_to)`** — the pure predicate mirroring the
  `list_active` WHERE clause (`status == "active" && valid_to.is_none()`),
  unit-tested without a database.

### Configuration

Same DSN resolution as `FindingsStore`/`AtlasVecStore`: `RulesStore::from_env()`
reads `crate::config::atlas_database_url()` (`ATLAS_DATABASE_URL`) and returns
`ToolError::NotConfigured` without attempting a connection if unset.

| Env var | Purpose | Default |
|---|---|---|
| `ATLAS_DATABASE_URL` | Postgres DSN for the Atlas database (shared with the findings/vec stores) | none (`NotConfigured` if unset) |
