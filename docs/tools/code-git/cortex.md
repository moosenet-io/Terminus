[← docs index](../../README.md)

# Cortex — Atlas-backed code-intelligence gate (CXEG-01 re-scaffold)

Cortex is a 10-tool-name module (`src/cortex/mod.rs`, `src/cortex/deprecated.rs`,
`src/cortex/audit.rs`), but as of **CXEG-01** only 3 of those names are "real"
tools — the rest are structured deprecation aliases. This page describes the
current, post-retirement shape; see the "History" section at the bottom for
what changed and why.

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
| `cortex_scope` | **pending rebuild (CXEG-02)** | Validates `project_id`/`changed_files`, returns a structured `{"status":"pending","item":"CXEG-02",...}` pointer. No blast-radius analysis happens yet. |
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

## `cortex_scope` (pending — CXEG-02)

**Input schema**: `project_id` (enum, required, one of `PROJECT_IDS`),
`changed_files` (string, required, comma-separated file paths, ≤2000 chars).

**Behavior**: validates both fields, then returns:

```json
{
  "status": "pending",
  "item": "CXEG-02",
  "tool": "cortex_scope",
  "project_id": "TERM",
  "message": "cortex_scope's SSH-relay-era backend has been retired; an Atlas-backed blast-radius implementation lands in CXEG-02. In the meantime, query kg_neighbors / kg_subgraph directly against the Atlas KG.",
  "tier_b_enabled": false
}
```

**Error/edge cases**: `InvalidArgument` for an unknown `project_id` or an
oversized `changed_files`. No `NotConfigured`/`Execution` errors are possible
— there is no network path to fail.

**In the meantime**: call `kg_neighbors` / `kg_subgraph` directly against the
Atlas KG for a manual blast-radius query.

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
| `CORTEX_ENABLE_TIER_B` | bool | `false` | Feature flag for a not-yet-built Tier B analysis pass; echoed in `cortex_scope`'s response. |
| `CORTEX_ENABLE_TIER_C` | bool | `false` | Feature flag for a not-yet-built Tier C analysis pass. |
| `CORTEX_ELEGANCE_ADVISORY_ONLY` | bool | `true` | Whether elegance/style findings are advisory-only; echoed in `cortex_review`'s response. |
| `CORTEX_DUP_COSINE_THRESHOLD` | f64 | `0.85` | Cosine-similarity threshold for a not-yet-built dup-detection pass; echoed in `cortex_audit`'s response. |
| `ATLAS_DATABASE_URL` | secret-shaped | none | Read exclusively through `crate::config::atlas_database_url()` — this crate has no separate `SecretManager`/`vault::manager()` API of its own; the runtime secret store is materialized into the process environment at deploy time (same convention as `crate::pki` and `scribe::graph::vec_embed`). `None` means the Atlas KG store is not configured. |

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
registers the 3 real (pending) tools against it, then delegates to
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
unknowns and the old legacy repo names), free-text length capping, each of
the 3 real tools' `InvalidArgument` rejection paths and their pending-pointer
success shape, `cortex_audit`'s unchanged SSRF-guard rejections, and full
registration (`register()` yields exactly 10 tool names, all `cortex_*`).
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
Cortex can query."

---

[← docs index](../../README.md)
