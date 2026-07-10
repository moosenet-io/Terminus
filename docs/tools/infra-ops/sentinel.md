# sentinel

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/sentinel/mod.rs`](../../../src/sentinel/mod.rs)

Sentinel triggers operational checks and logging on the fleet host and
refreshes the live MooseNet status page. Three tools: `sentinel_run` (kick
off a check), `sentinel_status` (read the latest result), and
`sentinel_refresh_status` (force a status-page regeneration).

<img src="../../../assets/fleet-ssh-bridge-flow.svg" alt="sentinel_run and sentinel_refresh_status bridge over typed SSH; sentinel_status reads results back from Gitea via GiteaClient" width="100%">

## Simplification versus the Python original

The legacy Python `sentinel_status` SSHed to the fleet host to run an
*inline* <secret-manager>-auth + `curl` pipeline against the Gitea contents API,
just to fetch a status file. This port replaces that entire chain with a
direct call into this crate's own `gitea` module
(`GiteaClient::from_env()`), because Terminus already holds `GITEA_URL` and
the resolved `GITEA_PAT_<NAME>` identity token locally — the Python version
only needed the SSH detour because the *Python* MCP process didn't have
those credentials (`src/sentinel/mod.rs:9-20`). Same end result (latest
check content from Gitea), no extra SSH hop, no inline shell script that
authenticates to a secrets backend and shells out to `curl`/`python3 -c` —
exactly the kind of subprocess/shell chain the `RustTool` contract forbids.

## Configuration

| Env var | Purpose | Default |
| --- | --- | --- |
| `SENTINEL_SSH_HOST` | SSH host of the fleet box | none — required for `sentinel_run`/`sentinel_refresh_status` |
| `SENTINEL_SSH_USER` | SSH user | `root` |
| `SENTINEL_SSH_KEY_PATH` | SSH private key path | none — required |
| `SENTINEL_SCRIPT` | Remote ops script invocation | none — required (no compiled-in fallback) |
| `SENTINEL_STATUS_GENERATOR_CMD` | Remote command for the status-page generator | none — required (no compiled-in fallback) |
| `SENTINEL_STATUS_PAGE_URL` | URL of the live status page | optional — omitted from responses if unset |
| `SENTINEL_REPO` | Gitea repo Sentinel writes results to | `lumina-sentinel` |

## Operation allowlist

`VALID_OPS` (`src/sentinel/mod.rs:67-78`): `plex-health`, `self-health`,
`vm901-watchdog`, `gitea-health`, `system-snapshot`, `commute-tracker`,
`daily-log`, `reflection`, `tool-usage-log`, `memory-curation`.

`STATUS_TRIGGERING_OPS` (`src/sentinel/mod.rs:64`) — `system-snapshot`,
`self-health`, `plex-health` — automatically fire a background status-page
refresh after completing.

`CHECK_CATEGORY_OPS` (`src/sentinel/mod.rs:81-88`) determine whether a
result lives under Gitea's `checks/` (health-check-shaped ops:
`plex-health`, `self-health`, `vm901-watchdog`, `gitea-health`,
`system-snapshot`, `commute-tracker`) or `logs/` (everything else) —
`category_for()` picks the path prefix.

## Tools

### `sentinel_run`

**Purpose.** Run one operational check or logging task on the fleet host.

**Input schema** (`src/sentinel/mod.rs:306-322`)

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `operation` | string (enum) | yes | Must be one of `VALID_OPS` |
| `args` | string | no (default `""`) | Only meaningful for `commute-tracker` (`"morning"`/`"afternoon"`); restricted to `[A-Za-z0-9_-]` via `validate_args` (`src/sentinel/mod.rs:174-185`) |

**Behavior.** Rejects an unknown `operation` or an `args` value containing
any shell metacharacter before building a command. Runs `{script}
{operation}[ {args}]` over SSH (120s timeout). If `operation` is in
`STATUS_TRIGGERING_OPS`, fires `trigger_status_page` in the background
(detached `spawn_blocking`, result not awaited — a refresh failure is only
logged, never surfaces in the tool's own response).

**Output shape:**
```json
{
  "status": "complete",
  "operation": "self-health",
  "output": "<script stdout, trimmed>",
  "latest_path": "checks/latest-self-health.md",
  "repo": "moosenet/lumina-sentinel",
  "status_page": "https://status.example/",
  "status_page_refreshed": true
}
```
`status_page`/`status_page_refreshed` are only present when the operation
is status-triggering **and** `SENTINEL_STATUS_PAGE_URL` is configured.

**Errors.** Missing `operation` → `InvalidArgument`. Unknown `operation` →
`InvalidArgument` listing `VALID_OPS`. `args` with disallowed characters →
`InvalidArgument`. `SENTINEL_SSH_HOST`/`SENTINEL_SCRIPT` unset →
`NotConfigured`. A non-zero remote exit status → `ToolError::Execution`
("Remote command exited with status N") — unlike `synapse`, this module
**does** treat a non-zero exit as a hard error.

### `sentinel_status`

**Purpose.** Check the latest result for one operation, or list all valid
operations if none is specified.

**Input schema** (`src/sentinel/mod.rs:395-406`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `operation` | string | no | `""` — empty means "list valid operations" |

**Behavior.** With `operation` empty, returns
`{"message": "Specify an operation to check status", "valid_operations": [...], "status_page": "..."}`
without touching Gitea. With a valid `operation`, resolves the category
(`checks/` or `logs/`) and fetches `{category}/latest-{operation}.md` via
`GiteaClient::fetch_file_text`. A `NotFound` from Gitea becomes
`"content": "No data found"` rather than an error — any other Gitea error
propagates as-is.

**Output shape:**
```json
{"operation": "self-health", "status_page": "https://status.example/", "content": "<file text>"}
```

**Errors.** Unknown `operation` (non-empty but not in `VALID_OPS`) →
`InvalidArgument`.

### `sentinel_refresh_status`

**Purpose.** Force an immediate status-page regeneration and dashboard rebuild.

**Input schema.** No parameters.

**Behavior.** Runs `SENTINEL_STATUS_GENERATOR_CMD` over SSH (60s timeout).
Unlike most tools in this domain, a failure here does **not** propagate as
an `Err` — the response always reports `"status": "refreshed"`, with either
`"output"` populated (success) or `"error"` populated (failure), because the
Python original backgrounds this command and does not treat its failure as
fatal to the caller.

**Output shape:**
```json
{"status": "refreshed", "output": "<generator stdout>", "error": "", "status_page": "https://status.example/"}
```

**Errors.** `SENTINEL_STATUS_GENERATOR_CMD` unset → `NotConfigured` (the one
hard-error path — a missing command can't even be attempted).

## Security model summary

- `operation` is validated against a fixed allowlist before it reaches a
  remote command string.
- `args` is restricted to a safe character class — no shell metacharacters
  can reach the remote command, even for the one operation (`commute-tracker`)
  that accepts a free-form-looking argument.
- `sentinel_status` no longer needs its own SSH+<secret-manager>+curl chain —
  removing an entire subprocess/shell surface by reusing Terminus's own
  Gitea credentials.
- Connection-level SSH failures are genericized, matching `dura`/`vigil`/
  `gateway` (not `synapse`'s deliberate exception).

[← Infra & Ops index](README.md) · [← tool index](../README.md)
