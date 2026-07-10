# dashboard (gateway)

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/gateway/mod.rs`](../../../src/gateway/mod.rs)

The crate module is named `gateway`; all six tools it registers are
`dashboard_*`-prefixed, surfacing the Lumina API Gateway / Homepage
dashboard that runs on the fleet host. Five are thin read-only endpoint
wrappers generated from one shared struct; the sixth
(`dashboard_refresh`) triggers a composer rebuild.

<img src="../../../assets/fleet-ssh-bridge-flow.svg" alt="Every dashboard_* tool runs a fixed curl command over typed SSH against the gateway's localhost HTTP API" width="100%">

## How it reaches the gateway

The legacy Python original shelled out via `ssh ... 'curl ...'` with
`shell=True`. This port uses the `ssh2` crate for typed SSH execution and
runs a **fixed** `curl` command template against the gateway's own
localhost HTTP endpoints (`src/gateway/mod.rs:1-8`). The only variable
parts of the command are the endpoint path — chosen from a small fixed
internal set, never user input — and the API key, sent as an HTTP header,
single-quoted, with single quotes in the key explicitly rejected so it
cannot break out of the quoting (`build_curl_command`,
`src/gateway/mod.rs:138-151`). None of the six tools take any user-supplied
argument at all.

## Configuration

| Env var | Purpose | Default |
| --- | --- | --- |
| `GATEWAY_SSH_HOST` | SSH host of the gateway box | none — required |
| `GATEWAY_SSH_USER` | SSH user | `root` |
| `GATEWAY_SSH_KEY_PATH` | SSH private key path | none — required |
| `GATEWAY_URL` | Base URL of the gateway (as seen from the SSH host — typically localhost) | `http://localhost:8080` |
| `DASHBOARD_API_KEY` | Value sent as the `x-api-key` header | none — required for every endpoint tool |
| `GATEWAY_COMPOSER_CMD` | Command run for `dashboard_refresh` | none — required (no compiled-in fallback, 2026-07 PII remediation) |

## Tools

### `dashboard_status`, `dashboard_calendar`, `dashboard_tasks`, `dashboard_insights`, `dashboard_inbox`

All five share one implementation, `DashboardEndpointTool`
(`src/gateway/mod.rs:258-286`), differing only by name, description, and
target endpoint:

| Tool | Endpoint | What it returns |
| --- | --- | --- |
| `dashboard_status` | `/api/health` | Health of all Lumina dashboard gateway endpoints |
| `dashboard_calendar` | `/api/calendar` | Today's calendar events |
| `dashboard_tasks` | `/api/tasks` | Urgent/high-priority Plane tasks |
| `dashboard_insights` | `/api/insights` | Current rotating dashboard insights |
| `dashboard_inbox` | `/api/inbox` | Nexus inbox pending-message count |

**Input schema.** No parameters for any of the five.

**Behavior.** `call_endpoint()` (`src/gateway/mod.rs:238-251`) resolves
`DASHBOARD_API_KEY` first (so a missing key surfaces as `NotConfigured`
before any SSH attempt), builds `curl -s -H 'x-api-key: {key}'
{gateway_url}{endpoint}`, runs it over SSH (10s timeout), and parses the
response body as JSON via `parse_gateway_response`
(`src/gateway/mod.rs:156-164`) — mirroring the Python `_gw` helper: on a
JSON decode failure it returns `{"error": "invalid JSON: <first 100 chars>"}`
rather than failing the tool call.

**Output shape (example — `dashboard_status`):** whatever `/api/health`
returns, pretty-printed; on a malformed response:
```json
{"error": "invalid JSON: <preview>"}
```

**Errors.** `DASHBOARD_API_KEY` unset → `NotConfigured`, before SSH.
`GATEWAY_SSH_HOST`/`GATEWAY_SSH_KEY_PATH` unset → `NotConfigured`. An API
key containing a single quote → `InvalidArgument("DASHBOARD_API_KEY must
not contain a single quote")` — this can only happen from a misconfigured
environment, not caller input, since none of these tools take arguments.

### `dashboard_refresh`

**Purpose.** Trigger an immediate dashboard composer run, bypassing the
2 AM schedule — regenerates the Homepage YAML config from current module
state.

**Input schema.** No parameters.

**Behavior.** Runs `GATEWAY_COMPOSER_CMD` over SSH with a **60-second**
timeout (matching the Python composer invocation). Matches the Python
output contract: `{"status": "triggered", "output": <last 300 chars of
stdout>}` — truncation is by Unicode scalar count, not byte count
(`src/gateway/mod.rs:323-330`), so it never splits a multi-byte character.

**Output shape:**
```json
{"status": "triggered", "output": "...last 300 chars of composer stdout..."}
```

**Errors.** `GATEWAY_COMPOSER_CMD` unset → `NotConfigured`. `GATEWAY_SSH_HOST`
unset → `NotConfigured`.

## Security model summary

- All SSH commands are built from fixed templates; the endpoint path is
  chosen from an internal fixed set, never derived from caller input.
- The API key is the only variable inserted into the shell command text,
  and it is rejected outright if it contains a single quote — closing the
  one theoretical way it could break out of its quoted header argument.
- None of the six tools accept caller-supplied parameters, eliminating an
  entire class of injection surface by construction.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
