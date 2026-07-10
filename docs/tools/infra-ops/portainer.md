# <container-mgr>

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/<container-mgr>/mod.rs`](../../../src/<container-mgr>/mod.rs)

The `<container-mgr>` module is a read-only Docker container-management client
over the <container-mgr> API, running against a self-signed-TLS internal host
(`src/<container-mgr>/mod.rs:1-2`). It mirrors a legacy Python
`portainer_tools.py` exactly and exposes four tools: server status,
environment (endpoint) listing, container listing within an environment,
and container log tailing.

<img src="../../../assets/readonly-http-probe-flow.svg" alt="Read-only HTTP probe flow: GET the LAN service, shape the response, return JSON or a genericized error" width="100%">

## Configuration

| Env var | Purpose |
| --- | --- |
| `PORTAINER_URL` | Base URL, e.g. `https://<container-mgr>.example:9443` (self-signed cert) |
| `PORTAINER_API_TOKEN` | <container-mgr> access token, sent as the `X-API-Key` header |

If either is unset, `register()` installs `NotConfiguredStub` tools for all
four names, each returning `ToolError::NotConfigured("PORTAINER_URL and
PORTAINER_API_TOKEN must be set")` (`src/<container-mgr>/mod.rs:393-409,411-423`).

**TLS note.** Because <container-mgr> runs with a self-signed certificate on an
internal host, this module's HTTP client is built with
`.danger_accept_invalid_certs(true)` (`src/<container-mgr>/mod.rs:48-56`) —
matching the Python original's `ssl.CERT_NONE` — scoped only to this
client, not globally.

## Tools

### `portainer_status`

**Purpose.** <container-mgr> server health and version.

**Input schema.** No parameters.

**Behavior.** GET `/api/status`, shaped via `parse_status`
(`src/<container-mgr>/mod.rs:100-106`): `{"version": <Version|"unknown">,
"instance_id": <InstanceID|"">, "healthy": true}`. `healthy` is always
`true` if the HTTP call itself succeeded — a non-2xx status short-circuits
earlier as an `Err`, so this tool never returns `healthy: false`.

**Output shape:**
```json
{"version": "2.19.4", "instance_id": "abc-123", "healthy": true}
```

### `portainer_list_environments`

**Purpose.** List every Docker environment (endpoint) <container-mgr> manages.

**Input schema.** No parameters.

**Behavior.** GET `/api/endpoints`, shaped via `parse_environments`
(`src/<container-mgr>/mod.rs:109-127`): each entry maps `Id→id`, `Name→name`
(default `"unknown"`), `URL→url`, `Type→type`, `Status` (1 → `"up"`,
anything else → `"down"`), and `Snapshots` array length →
`snapshots`.

**Output shape:**
```json
{"count": 2, "environments": [
  {"id": 1, "name": "local", "url": "unix://", "type": 1, "status": "up", "snapshots": 1},
  {"id": 2, "name": "remote", "url": "tcp://x", "type": 2, "status": "down", "snapshots": 0}
]}
```

**Errors / edge cases.** A non-array response body → `ToolError::Http("Unexpected response format")`.

### `portainer_list_containers`

**Purpose.** List Docker containers in an environment.

**Input schema** (`src/<container-mgr>/mod.rs:298-306`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `environment_id` | integer | no | `0` | `0` = auto-detect: fetches `/api/endpoints` and uses the first entry's `Id` (default `1` if the list is empty) |
| `all_containers` | boolean | no | `true` | Include stopped containers |

**Behavior.** Resolves the environment ID (auto-detecting if `0`), then GETs
`/api/endpoints/{id}/docker/containers/json` with `?all=true` when
`all_containers` is set. `parse_containers` (`src/<container-mgr>/mod.rs:139-209`)
maps each Docker API container object into a trimmed summary: `name`
(first entry of `Names`, leading `/` stripped, falling back to a 12-char
short ID if unnamed), `image`, `state`, `status`, `ports` (formatted
`"{public}->{private}/{type}"`, entries without a `PublicPort` dropped —
this is why a container's internal-only ports never show up), and a
12-character `id`. Sorted running-first, then alphabetically by name.

**Output shape:**
```json
{"environment_id": 1, "total": 2, "running": 1, "stopped": 1, "containers": [
  {"name": "running-a", "image": "img:2", "state": "running", "status": "Up 2 hours", "ports": ["8080->80/tcp"], "id": "aaaaaaaaaaaa"},
  {"name": "stopped-b", "image": "img:1", "state": "exited", "status": "Exited (0)", "ports": [], "id": "zzzzzzzzzzzz"}
]}
```

### `portainer_container_logs`

**Purpose.** Tail logs from one container.

**Input schema** (`src/<container-mgr>/mod.rs:333-343`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `container_id` | string | yes | — | Container ID or name; trimmed, empty after trim → `InvalidArgument` |
| `environment_id` | integer | no | `0` | Same auto-detect-first-environment semantics as `portainer_list_containers` |
| `tail` | integer | no | `100` | Number of lines from the end |

**Behavior.** Fetches the raw log stream directly (not JSON) from
`/api/endpoints/{id}/docker/containers/{container_id}/logs?stdout=true&stderr=true&tail={n}`.
`parse_logs` (`src/<container-mgr>/mod.rs:215-243`) cleans Docker's multiplexed
log-stream framing: for any non-empty line longer than 8 characters whose
first byte is `0x00`/`0x01`/`0x02` (a stream-type marker), the leading 8
characters are stripped; shorter or unmarked lines pass through unchanged.
Blank lines are dropped before tailing.

**Output shape:**
```json
{"container": "chord", "lines": 100, "tail": 100, "logs": "line1\nline2\n..."}
```

**Errors / edge cases.** Empty/whitespace-only `container_id` →
`InvalidArgument` before any HTTP call. All HTTP failures (unreachable,
non-2xx) map to `ToolError::Http("The container service (<container-mgr>) is
unreachable.")` — the underlying reqwest error is logged, never returned
verbatim.

## Security model summary

- Fully read-only.
- Self-signed TLS is explicitly accepted, scoped to this client only.
- `PORTAINER_API_TOKEN` is sent as a header, never logged or embedded in an
  error message.
- Auto-detect-first-environment (`environment_id: 0`) is a convenience, not
  a security boundary — it always resolves to whichever endpoint <container-mgr>
  lists first.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
