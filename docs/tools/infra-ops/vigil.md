# vigil

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/vigil/mod.rs`](../../../src/vigil/mod.rs)

Vigil generates morning/afternoon briefings on the fleet host — gathering
live data (news, weather, commute, crypto, sports), formatting it, and
writing the result to a Gitea repo. Two tools: `vigil_generate` (trigger
generation) and `vigil_status` (poll for readiness).

<img src="../../../assets/fleet-ssh-bridge-flow.svg" alt="vigil_generate bridges over typed SSH to the remote briefing script; vigil_status reads the result back from Gitea" width="100%">

## Simplification versus the Python original

Identical rationale to [`sentinel.md`](sentinel.md#simplification-versus-the-python-original):
the legacy Python `vigil_status` SSHed to the fleet host to run an inline
<secret-manager>-auth + `curl` pipeline against the Gitea contents API just to
check whether a file exists. This port calls `GiteaClient::from_env()`
directly instead — Terminus already holds `GITEA_URL` and the resolved
`GITEA_PAT_<NAME>` identity token locally, so the extra SSH hop and inline
shell script are both unnecessary (`src/vigil/mod.rs:1-19`).

## Configuration

| Env var | Purpose | Default |
| --- | --- | --- |
| `VIGIL_SSH_HOST` | SSH host of the fleet box | none — required for `vigil_generate` |
| `VIGIL_SSH_USER` | SSH user | `root` |
| `VIGIL_SSH_KEY_PATH` | SSH private key path | none — required |
| `VIGIL_SCRIPT` | Remote briefing script invocation | none — required (no compiled-in fallback) |
| `VIGIL_REPO` | Gitea repo Vigil briefings live in | `lumina-vigil` |

`briefing_type` is validated against `{"morning", "afternoon"}`
(`validate_briefing_type`, `src/vigil/mod.rs:59-67`) before it is ever
placed into a remote command string or a Gitea path — for both tools.

## Tools

### `vigil_generate`

**Purpose.** Trigger briefing generation on the fleet host — gathers live
data and writes the finished briefing to Gitea. Takes roughly 30-60 seconds.

**Input schema** (`src/vigil/mod.rs:216-228`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `briefing_type` | string (enum) | no | `"morning"` | Must be `"morning"` or `"afternoon"` |

**Behavior.** Validates `briefing_type`, then runs `{script}
{briefing_type}` over SSH with a **120-second** timeout (matching the
tool's own documented "30-60 seconds" plus headroom).

**Output shape:**
```json
{
  "status": "ready",
  "briefing_type": "morning",
  "latest_path": "briefings/latest-morning.md",
  "repo": "moosenet/lumina-vigil",
  "message": "Briefing is ready. Read it from Gitea: moosenet/lumina-vigil/briefings/latest-morning.md",
  "output": "<script stdout, trimmed>"
}
```

**Errors.** Invalid `briefing_type` (anything other than
`"morning"`/`"afternoon"`) → `InvalidArgument`, checked before any SSH
attempt — including injection attempts like `"morning; rm -rf /"`.
`VIGIL_SSH_HOST`/`VIGIL_SCRIPT` unset → `NotConfigured`. Non-zero remote
exit → `ToolError::Execution("Remote command exited with status N")`.

### `vigil_status`

**Purpose.** Check whether the latest briefing is available on Gitea —
intended for light polling instead of regenerating.

**Input schema** (`src/vigil/mod.rs:279-291`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `briefing_type` | string (enum) | no | `"morning"` |

**Behavior.** Fetches `briefings/latest-{briefing_type}.md` via
`GiteaClient::fetch_file_text`. On success, additionally calls
`get_file_sha` and truncates the result to its first 8 characters — mirroring
the Python source's `file_info`, which reports the first 8 chars of the
file's SHA alongside size/exists (`src/vigil/mod.rs:302-317`). A separate
Gitea call for the SHA that itself fails degrades to an empty string rather
than failing the whole tool.

**Output shape (found):**
```json
{
  "status": "ready",
  "briefing_type": "morning",
  "latest_path": "briefings/latest-morning.md",
  "repo": "moosenet/lumina-vigil",
  "file_info": {"exists": true, "size": 4213, "sha": "0123abcd"}
}
```
**Output shape (not found):**
```json
{"status": "not_found", "briefing_type": "morning", "message": "No briefing found. Run vigil_generate first."}
```

**Errors.** Invalid `briefing_type` → `InvalidArgument`. Any Gitea error
other than "file not found" propagates as-is (e.g. auth failure, network
error).

## Security model summary

- `briefing_type` is a two-value enum validated before it reaches either an
  SSH command or a Gitea file path — no injection surface on either leg.
- `vigil_status` performs two Gitea calls per lookup (content + SHA); a SHA
  fetch failure never fails the whole tool, only degrades the `sha` field.
- No credentials in this module beyond what `GiteaClient::from_env()`
  already resolves — nothing is fetched or stored locally.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
