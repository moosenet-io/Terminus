# sundry

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/sundry/mod.rs`](../../../src/sundry/mod.rs)

`sundry` groups six small, unrelated utility tools that don't warrant a
dedicated module of their own — ported 1:1 from the legacy Python MCP
server on the source host, verified against its live `tools/list`/
`tools/call` behavior on 2026-07-06 (`src/sundry/mod.rs:1-40`). Five of the
six are pure in-process logic with no network dependency; only
`searxng_search` makes an outbound HTTP call.

<img src="../../../assets/sundry-flow.svg" alt="Five of six sundry tools are pure in-process logic; searxng_search is the sole tool that leaves the process, calling a SearXNG instance" width="100%">

## Configuration

| Env var | Purpose |
| --- | --- |
| `SEARXNG_URL` | Base URL of the SearXNG instance. Required only by `searxng_search`; if unset that tool returns `NotConfigured` — the other five tools are entirely unaffected. |

## Tools

### `health`

**Purpose.** Liveness ping.

**Input schema.** No parameters.

**Behavior.** Always returns `{"ok": true}`, pretty-printed. The legacy
Python tool's live docstring was empty; a short description was supplied
here because terminus-rs requires every registered tool to have a
non-empty description (`src/sundry/mod.rs:62-64`).

### `echo`

**Purpose.** Return the given text back verbatim — a connectivity check.

**Input schema** (`src/sundry/mod.rs:95-103`)

| Field | Type | Required |
| --- | --- | --- |
| `text` | string | yes |

**Behavior.** Returns `text` unchanged, with no wrapping — the tool result
*is* the input string, not a JSON envelope.

**Errors.** Missing `text` → `InvalidArgument`.

### `utc_now`

**Purpose.** Current UTC time as an ISO-8601 timestamp.

**Input schema.** No parameters.

**Behavior.** `Utc::now().format("%Y-%m-%dT%H:%M:%SZ")` — always exactly 20
characters, always ends in `Z`, second-precision (no fractional seconds).

**Output shape:** `"2026-07-10T14:32:07Z"` (bare string, not JSON).

### `constellation_version`

**Purpose.** Report Lumina Constellation version info and build metadata —
"use this to verify the MCP server is running and check deployment info."

**Input schema.** No parameters.

**Behavior.** Returns a body whose non-timestamp fields are **fixed
constants**, ported byte-for-byte from repeated live observations rather
than derived dynamically (`src/sundry/mod.rs:11-21`) — the module doc
comment explicitly flags this as a "1:1 stub" pending a later human audit
of whether these should become dynamic (e.g. sourced from
`CARGO_PKG_VERSION`). Only `timestamp` is computed live, via
`Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)`.

**Output shape:**
```json
{
  "constellation": "Lumina Constellation",
  "version": "0.12.0",
  "session": 12,
  "mcp_hub": "the Terminus MCP hub container",
  "agent_fleet": "the agent fleet host",
  "orchestrator": "the orchestrator container (agent runtime v0.24.0)",
  "plugin_architecture": true,
  "skills_standard": "agentskills.io",
  "timestamp": "2026-07-10T14:32:07.123456Z"
}
```

### `vector_onboard`

**Purpose.** Return the Vector operating manual — guardrails, submission
instructions, and cost limits. Any agent should call this before its first
Vector interaction in a session.

**Input schema.** No parameters.

**Behavior.** Returns a **static** JSON blob. `active_projects` and
`conventions` were observed empty on the live server and are ported as
empty arrays to match exactly (`src/sundry/mod.rs:22-26`) — this is not a
placeholder awaiting wiring, it's a faithful port of the live response
shape at the time of the port.

**Output shape:**
```json
{
  "agent": "vector",
  "version": "1.0",
  "status": "active",
  "system_guardrails": ["Never merge own PRs", "Write tests before committing", "Cost gate max $2/task"],
  "active_projects": [],
  "conventions": [],
  "how_to_submit": {
    "via_nexus": "nexus_send(from_agent='lumina', to_agent='vector', message_type='work_order', payload=json.dumps({'op':'maintenance','task':'<description>','repo':'<path>'}))",
    "via_mcp": "vector_submit(task='<description>', repo='<path>', cost_budget=2.0)"
  },
  "cost_limits": {"max_per_task": 2.0, "max_per_day": 10.0},
  "calx_active": true,
  "skill_aware": true
}
```

### `searxng_search`

**Purpose.** Query a MooseNet SearXNG instance and return raw JSON results
— the only tool in this module that leaves the process.

**Input schema** (`src/sundry/mod.rs:266-275`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `q` | string | yes | — search query |
| `categories` | string | no | `"general"` |
| `language` | string | no | `"en-US"` |

**Behavior.** GETs `{SEARXNG_URL}/search` with `q`, `categories`,
`language`, and `format=json` as query params, `User-Agent:
MooseNet-MCP/1.0`, 15s timeout. The response body is passed through
verbatim — matching the live server's pass-through shape: `query`,
`number_of_results`, `results`, `answers`, `corrections`, `infoboxes`,
`suggestions`, `unresponsive_engines`.

**Output shape:** whatever SearXNG returns, pretty-printed, unmodified.

**Errors.** `SEARXNG_URL` unset → `NotConfigured`. Missing `q` →
`InvalidArgument`. Non-2xx response → `ToolError::Http("SearXNG returned
HTTP <status>")`.

## Security model summary

- Five of six tools have zero external attack surface — no network I/O, no
  file I/O, no shell.
- `searxng_search` is the sole exception, and it only ever performs a GET
  against one operator-configured base URL — no user-supplied host or path
  segment.
- `constellation_version` and `vector_onboard` intentionally return static,
  non-secret data — there is nothing in either payload that varies by
  caller or leaks infrastructure detail.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
