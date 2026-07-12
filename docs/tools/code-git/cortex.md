[← docs index](../../README.md)

# Cortex — Atlas-backed code-intelligence gate (CXEG-01/02)

Cortex is a 10-tool-name module (`src/cortex/mod.rs`, `src/cortex/scope.rs`,
`src/cortex/deprecated.rs`, `src/cortex/audit.rs`), but as of **CXEG-01** only
3 of those names are "real" tools — the rest are structured deprecation
aliases. As of **CXEG-02**, `cortex_scope` is the first of those 3 to be a
fully live, Atlas-backed implementation rather than a pending-pointer stub.
This page describes the current shape; see the "History" section at the
bottom for what changed and why.

## The single most important fact about this module: **the SSH-relay era is retired**

Every previous revision of this module was a thin SSH-exec relay to a script
on a since-**retired** external fleet host — the same transport pattern
`crucible`/`sentinel`/`vigil` still use. That host is gone, and CXEG-01
deleted the entire SSH transport (no SSH client library, no `TcpStream`, no
remote-script invocation) from this module. Cortex's successor is the
in-process **Atlas code graph** (`crate::scribe::graph`, the `kg_*` tool
family), which builds, persists, and queries a real graph locally — no SSH,
no remote script, no "relay whatever the other end says" response shape.

## What's here now

| Tool | Status | What it does |
| --- | --- | --- |
| `cortex_scope` | **live (CXEG-02)** | Resolves `project_id` + `changed_files`/`diff` against the project's Atlas graph and returns the blast radius: touched symbols, their 1-hop callers/callees, affected communities, `blast_count`, `token_reduction_pct`. Degrades to `configured:false` (no error) when the project has no stored graph. |
| `cortex_review` | **pending rebuild (CXEG-04)** | Validates `project_id`/`changed_files`, returns `{"status":"pending","item":"CXEG-04",...}`. No risk scoring happens yet. |
| `cortex_audit` | **pending rebuild (CXEG-11)** | Runs its existing SSRF-hardened `url` validation (unchanged, see below), then returns `{"status":"pending","item":"CXEG-11",...}`. No clone/graph-build happens yet. |
| `cortex_stats` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_stats",...}`. Call `kg_stats` instead. |
| `cortex_build` | **deprecated alias** | Returns `{"deprecated":true,"use":"scribe_kg_build",...}`. Call `scribe_kg_build` instead. |
| `cortex_deps` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_neighbors",...}`. Call `kg_neighbors` instead. |
| `cortex_recent` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_query",...}`. Call `kg_query` instead. |
| `cortex_community` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_communities",...}`. Call `kg_communities` instead. |
| `cortex_architecture` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_communities",...}`. Call `kg_communities` instead. |
| `cortex_flows` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_path",...}`. Call `kg_path` instead. |

All 10 tool NAMES stay registered (no MCP-listing churn for a caller that
enumerates tools), but 7 of them do **zero I/O** — no network, no SSH, no
filesystem, no database — they only build and return a small JSON pointer
object (`src/cortex/deprecated.rs`).

## `project_id`, not `repo`

The old fixed two-repo-name allowlist (`"lumina-fleet"` / `"lumina-terminus"`)
named two repos on the retired fleet-host layout. `cortex_scope` and
`cortex_review` are now keyed by `project_id` instead, validated against
`crate::cortex::PROJECT_IDS` (`src/cortex/mod.rs`):

```
TERM, LUM, HARM, CHRD, RAIL
```

This is the same `project_id` vocabulary the Atlas KG (`kg_*`) tools already
use, and matches the current Plane-project-prefix convention (Terminus, Lumina,
Harmony, Chord, Civic-Rail). Any other value is rejected with
`ToolError::InvalidArgument` before the stub response is built.

## `cortex_scope` — live, Atlas-backed blast radius (CXEG-02)

The pipeline's pre-dispatch scoping call: "if I touch these files, what else
might I break, and how much of the project can I safely ignore?"

**Input schema**: `project_id` (enum, required, one of `PROJECT_IDS`), plus
EITHER `changed_files` (a comma-separated string OR a JSON array of file
paths — the comma-separated form is kept for backward compatibility with the
CXEG-01 stub's original schema) OR `diff` (a unified diff; changed files are
parsed from its `+++ b/<path>` headers). At least one of `changed_files`/
`diff` must yield a non-empty file list.

**Reuse**: both the CSV/array/diff parsing and the graph queries are shared
with `review_run`'s KGREV-01 grounding, not reimplemented:
- `crate::review::kg_context::derive_changed_files` does the actual `diff`/
  array parsing (`src/cortex/scope.rs`'s `changed_files_from_args` only
  adapts `cortex_scope`'s own CSV-string/array argument shapes into the
  `{"changed_files"|"diff": ...}` value `derive_changed_files` expects).
- The graph load + touched-node + 1-hop-neighbor walk use the same
  `scribe::graph::store::GraphStore` / `KnowledgeGraph` API
  `review::kg_context::build_kg_block` and the `kg_neighbors`/`kg_subgraph`
  tools (`src/scribe/graph/tools.rs`) use — there is exactly one in-process
  graph-query backend in this crate.

**Behavior**:
1. Validates `project_id` (`InvalidArgument` if not one of `PROJECT_IDS`).
2. Derives `changed_files` from the input (`InvalidArgument` if both
   `changed_files` and `diff` are absent/empty).
3. Loads the project's Atlas graph. If none is stored yet (`scribe_kg_build`
   hasn't run for this `project_id`, or the store itself failed to load),
   returns a `"configured": false` response with each entry of
   `changed_files` echoed back into `blast_radius` as an unresolved literal
   entry — **never an error**, so a dispatch caller always gets a usable
   answer even against an unindexed project.
4. Otherwise, resolves each changed file to the current graph nodes it
   defines (`role: "touched"`), any changed file with no matching node is
   ALSO echoed back as an unresolved literal entry (e.g. a brand-new file),
   then walks the 1-hop callers/callees of every touched node
   (`role: "caller"`/`"callee"`), collecting each resolved node's community
   (`cluster`) into `affected_communities`.
5. Computes `token_reduction_pct` as `1 - (blast-radius node-card bytes /
   total-project node-card bytes) * 100`, clamped to `[0, 100]` — the same
   `node_card` text `scribe_kg_build`'s embedding pipeline embeds
   (`crate::scribe::graph::vec_embed::node_card`), used here as a proxy for
   "how much smaller is the context a model needs to read than the whole
   project."
6. If the walk would enumerate more than `CORTEX_MAX_BLAST_NODES` nodes (see
   "Configuration" below), it stops and sets `"truncated": true` — plus a
   `tracing::warn!` noting the drop — rather than silently capping.

**Response shape** (live graph):

```json
{
  "configured": true,
  "project_id": "TERM",
  "changed_files": ["src/cortex/mod.rs"],
  "blast_radius": [
    { "id": "crate::cortex::validate_project_id", "path": "src/cortex/mod.rs", "kind": "function", "resolved": true, "role": "touched" },
    { "id": "crate::cortex::CortexScope::execute", "path": "src/cortex/mod.rs", "kind": "function", "resolved": true, "role": "caller" }
  ],
  "affected_communities": [1],
  "blast_count": 2,
  "token_reduction_pct": 92.5
}
```

**Response shape** (no stored graph — degrade):

```json
{
  "configured": false,
  "project_id": "TERM",
  "changed_files": ["src/cortex/mod.rs"],
  "blast_radius": [
    { "id": "src/cortex/mod.rs", "path": "src/cortex/mod.rs", "kind": "file", "resolved": false, "role": "touched" }
  ],
  "affected_communities": [],
  "blast_count": 1,
  "token_reduction_pct": 0.0
}
```

**Error/edge cases**: `InvalidArgument` for an unknown `project_id`, an
oversized `changed_files` CSV string (`≤2000` chars), or neither
`changed_files` nor `diff` yielding any file. A missing/unloadable Atlas
graph is NOT an error (see step 3 above) — that is the one deliberate
exception to "validate first, then act" in this tool, since blast-radius
unavailability is a data-availability fact, not a caller mistake.

## `cortex_review` (pending — CXEG-04)

**Input schema**: identical shape to `cortex_scope` — `project_id` (enum,
required), `changed_files` (string, required, ≤2000 chars, comma-separated
modified file paths).

**Behavior**: validates both fields, then returns:

```json
{
  "status": "pending",
  "item": "CXEG-04",
  "tool": "cortex_review",
  "project_id": "TERM",
  "message": "cortex_review's SSH-relay-era backend has been retired; an Atlas-backed risk-scoring implementation lands in CXEG-04. In the meantime, query kg_findings / kg_query directly against the Atlas KG.",
  "risk_score_threshold": 7.0,
  "elegance_advisory_only": true
}
```

`risk_score_threshold` and `elegance_advisory_only` are read from
`CortexConfig` (see "Configuration" below) and echoed here so the CXEG-04
rebuild's threshold config is already visible/testable even though nothing
consumes it yet.

**Error/edge cases**: same as `cortex_scope`.

**In the meantime**: call `kg_findings` / `kg_query` directly against the
Atlas KG.

## `cortex_audit` — the one tool that still does real validation work

**Input schema**: `url` (string, required) — a public git repository URL,
e.g. `"https://github.com/owner/repo"`.

**Behavior**: `url` passes through the **unchanged**, SSRF-hardened
`validate_repo_url()` front-gate (`src/cortex/audit.rs` — this file was not
touched by CXEG-01; it has no dependency on the deleted SSH transport). Only
`http`/`https` URLs to public, non-private/loopback/link-local/metadata hosts
are accepted — see `audit.rs`'s own doc comments for the full numeric-host
SSRF-hardening rationale (decimal-integer, hex, octal-leading-zero, shorthand
dotted-quad, and IPv4-mapped-IPv6 encodings of loopback/private addresses are
all rejected, fail-closed). Once a `url` passes validation, `execute` returns:

```json
{
  "status": "pending",
  "item": "CXEG-11",
  "tool": "cortex_audit",
  "url": "https://github.com/octocat/Hello-World",
  "message": "cortex_audit's SSH-relay-era backend has been retired; a locally-sandboxed clone + Atlas-build implementation lands in CXEG-11. The url has passed SSRF-hardened validation but no audit has been performed.",
  "dup_cosine_threshold": 0.85
}
```

**No clone, no graph build, no HTML report generation happens yet** — CXEG-11
is expected to rebuild this as a locally-sandboxed clone + Atlas KG build,
replacing the old remote-script relay entirely (the retired implementation
never actually performed the clone in this process either — it delegated
that to the remote fleet-host script, so this is not a regression in local
sandboxing, just a currently-unimplemented rebuild).

**Error/edge cases**: `InvalidArgument` for any URL rejected by
`validate_repo_url` (empty, oversized, wrong scheme, embedded credentials,
shell metacharacters, whitespace/control chars, or a disallowed host) — all
caught before the stub response is built, so a malicious/malformed URL never
gets even a pending-pointer response, only a rejection.

## Configuration

`CortexConfig::from_env()` (`src/cortex/mod.rs`) builds one shared
`Arc<CortexConfig>` for all 3 real tools. No SSH/remote-script fields remain.

| Env var | Type | Default | Notes |
| --- | --- | --- | --- |
| `CORTEX_RISK_SCORE_THRESHOLD` | f64 | `7.0` | Echoed in `cortex_review`'s pending-pointer response; will gate the CXEG-04 rebuild's escalation logic. |
| `CORTEX_ENABLE_TIER_B` | bool | `false` | Feature flag for a not-yet-built Tier B analysis pass. No longer consumed by `cortex_scope` as of CXEG-02 (the pending stub used to echo it). |
| `CORTEX_ENABLE_TIER_C` | bool | `false` | Feature flag for a not-yet-built Tier C analysis pass. |
| `CORTEX_ELEGANCE_ADVISORY_ONLY` | bool | `true` | Whether elegance/style findings are advisory-only; echoed in `cortex_review`'s response. |
| `CORTEX_DUP_COSINE_THRESHOLD` | f64 | `0.85` | Cosine-similarity threshold for a not-yet-built dup-detection pass; echoed in `cortex_audit`'s response. |
| `CORTEX_MAX_BLAST_NODES` | usize | `200` | `cortex_scope`'s (CXEG-02) cap on the number of nodes enumerated into `blast_radius` before it sets `truncated:true` and stops walking. A zero/unparseable value falls back to the default rather than dropping every node. |
| `ATLAS_DATABASE_URL` | secret-shaped | none | Read exclusively through `crate::config::atlas_database_url()` — this crate has no separate `SecretManager`/`vault::manager()` API of its own; the runtime secret store is materialized into the process environment at deploy time (same convention as `crate::pki` and `scribe::graph::vec_embed`). `None` means the Atlas KG store is not configured (`cortex_scope` still degrades cleanly in this case, via `GraphStore`/`ScribeConfig`'s own `SCRIBE_KG_STORE_DIR`, which is independent of the Postgres DSN). |

Boolean flags accept `"1"`/`"true"`/`"yes"` (case-insensitive) as truthy;
anything else (including unset) falls back to the default.

## `cortex_stats` / `cortex_build` / `cortex_deps` / `cortex_recent` / `cortex_community` / `cortex_architecture` / `cortex_flows` — deprecation aliases

Each of these 7 tool names is registered (`src/cortex/deprecated.rs`) purely
so a caller using the old name doesn't get a bare "tool not found" — its
`execute` performs **no I/O of any kind** and always returns:

```json
{
  "deprecated": true,
  "use": "<replacement tool name>",
  "message": "'<old name>' was retired in CXEG-01 along with the rest of Cortex's SSH-relay-era transport to the now-retired fleet host. Call '<replacement>' against the in-process Atlas KG instead."
}
```

Replacement map:

| Retired tool | Replacement |
| --- | --- |
| `cortex_stats` | `kg_stats` |
| `cortex_build` | `scribe_kg_build` |
| `cortex_deps` | `kg_neighbors` |
| `cortex_recent` | `kg_query` |
| `cortex_community` | `kg_communities` |
| `cortex_architecture` | `kg_communities` |
| `cortex_flows` | `kg_path` |

These accept any argument shape (their `parameters()` schema is deliberately
permissive, `additionalProperties: true`) since they never inspect their
arguments — the pointer is returned unconditionally.

## Registration

`register()` (`src/cortex/mod.rs`) builds one shared `Arc<CortexConfig>`,
registers the 3 real tools against it (`cortex_scope` live as of CXEG-02;
`cortex_review`/`cortex_audit` still pending), then delegates to
`crate::cortex::deprecated::register()` for the 7 aliases. Cortex is wired
into **both** top-level registries in `src/registry.rs`: `register_all` (the
core registry, served by `terminus-primary`/Chord) and `register_personal`
(the personal registry) — unchanged from before CXEG-01.

## `crate::scribe::graph::cortex_bridge` — the one internal caller

`src/scribe/graph/cortex_bridge.rs` (KGRULE-05) calls `cortex_review`
internally to attach a best-effort risk signal to KG findings. As of CXEG-01
it always gets `None` back — `cortex_review`'s pending-stub response carries
no `risk_score` field for `cortex_bridge::extract_risk` to find — which is
within `cortex_bridge`'s own documented degrade contract ("returns `None`...
whenever... the tool call errors for any reason... [or] carries no numeric
`risk`/`score` field"). No code change is needed there once CXEG-04 lands a
real `risk_score`; the bridge is forward-compatible as-is.

## Testing notes for this module

`src/cortex/mod.rs`'s test module covers, without any network access:
`project_id` validation (accepts `TERM`/`LUM`/`HARM`/`CHRD`/`RAIL`, rejects
unknowns and the old legacy repo names), free-text length capping,
`cortex_review`/`cortex_audit`'s `InvalidArgument` rejection paths and
pending-pointer success shape, `cortex_scope`'s argument-validation/wiring
(`project_id` rejection, oversized CSV rejection, "neither changed_files nor
diff" rejection, array-form and diff-only-form acceptance, and a
`configured:false` degrade smoke test against an empty store dir),
`cortex_audit`'s unchanged SSRF-guard rejections, and full registration
(`register()` yields exactly 10 tool names, all `cortex_*`).
`src/cortex/scope.rs`'s test module covers the full blast-radius derivation
against a small fixture graph (2 files, a `calls` edge and a `references`
edge, 2 distinct clusters): `changed_files_from_args`'s array/CSV/diff
parsing agree on the same file set; a touched node's documented caller AND
callee both appear in `blast_radius`; a changed file with no matching graph
node is echoed back as an unresolved literal entry alongside resolved
symbols; `compute_scope` against an unconfigured/empty store degrades to
`configured:false` with every `changed_files` entry unresolved; an
artificially low `max_blast_nodes` sets `truncated:true` and caps the
returned `blast_radius`; and `token_reduction_pct` is `0.0` for an empty
graph and high when only a small fraction of a larger graph is touched.
`src/cortex/deprecated.rs`'s test module covers: all 7 aliases register,
each returns a `{"deprecated":true,"use":...}` pointer regardless of input
shape (including empty args), and no alias's `execute` does any I/O.
`src/cortex/audit.rs`'s test module is unchanged — it separately covers every
branch of `validate_repo_url()`, including the SSRF bypass-encoding
regression tests.

## History

Before CXEG-01, this module was a 10-tool SSH-exec relay to a script (`ops.py`)
on the fleet host, ported from a legacy Python source, mirroring
`crucible`/`sentinel`/`vigil`'s SSH-exec mechanics exactly (same SSH client
library usage, same non-infra-leaking generic error messages, same
`CORTEX_SSH_HOST`/`CORTEX_SSH_USER`/`CORTEX_SSH_KEY_PATH`/`CORTEX_SCRIPT`-env
config surface). That fleet host is now retired, and this whole transport
(including 7 of the original 10 tools' entire reason for existing — querying
a graph that only ever lived on the remote host) no longer has anywhere to
connect to. CXEG-01 deleted the transport and the 7 pure graph-relay tools,
kept `cortex_scope`/`cortex_review`/`cortex_audit`'s names/parameter surfaces
as principled stubs pending their Atlas-backed rebuilds (CXEG-02/CXEG-04/
CXEG-11), and added 7 zero-I/O deprecation aliases pointing at the in-process
Atlas KG's `kg_*` tool family, which is the actual successor to "a code graph
Cortex can query." **CXEG-02** then replaced `cortex_scope`'s pending-pointer
stub with the real Atlas-backed blast-radius implementation described above
(`src/cortex/scope.rs`), reusing `review::kg_context::derive_changed_files`
and the same `GraphStore`/`KnowledgeGraph` query API `kg_neighbors`/
`build_kg_block` already use rather than standing up a second graph-walk.
`cortex_review`/`cortex_audit` remain pending CXEG-04/CXEG-11.

---

[← docs index](../../README.md)
