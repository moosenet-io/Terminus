# dura

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/dura/mod.rs`](../../../src/dura/mod.rs)

`dura` is a hardened rewrite (CHORD-11) of a legacy Python `dura_tools.py`
that the module doc comment grades "Grade C: shell=True with nested SSH and
journalctl grep" (`src/dura/mod.rs:1-6`). Seven sysadmin/health-check tools
combine typed SSH (`ssh2`, no `shell=True`, no `std::process::Command`) with
direct Prometheus HTTP queries — replacing the Python original's
`journalctl`-over-SSH-over-grep chain with a real PromQL query for anything
metric-shaped.

<img src="../../../assets/fleet-ssh-bridge-flow.svg" alt="Typed SSH bridge for fixed and allowlist-validated commands, plus a direct Prometheus HTTP leg for health queries" width="100%">

## Configuration

| Env var | Purpose | Default |
| --- | --- | --- |
| `DURA_SSH_HOST` | SSH host of the target server | none — required for the SSH tools |
| `DURA_SSH_USER` | SSH user | `root` |
| `DURA_SSH_KEY_PATH` | SSH private key path | none — required |
| `PROMETHEUS_URL` | Base URL for `dura_constellation_health` / `dura_service_check` | none — required for those two tools |
| `DURA_ALLOWED_SERVICES` | Comma-separated allowlist for `service`/`service_name` args | `"lumina,chord,terminus,matrix,postgres"` |

Unlike `ansible`/`synapse`/`sentinel`/`vigil`/`gateway`, `dura`'s service
allowlist **does** ship a compiled-in default — a fixed, generic set of
constellation component names rather than real infrastructure identifiers,
so it was not in scope for the 2026-07 PII remediation pass that stripped
fallbacks elsewhere.

## Security model

- SSH commands are fixed strings wherever possible; only `service` and
  `last_n_lines` are user-supplied, and both are validated
  (`src/dura/mod.rs:8-15`).
- `service`/`service_name` must be on `DURA_ALLOWED_SERVICES` (case-sensitive
  exact match) before being interpolated into a command or PromQL label
  value.
- `last_n_lines` is parsed as `u32` and capped at 1000 regardless of the
  requested value.
- Prometheus label values are safe by construction (Prometheus escapes them
  server-side) once the service name itself is allowlist-validated.
- No `shell=true`, no `std::process::Command`, no string-interpolated
  command with raw user input.

## Tools

### `dura_smoke_test`

**Purpose.** Verify SSH connectivity and basic host health.

**Input schema.** No parameters.

**Behavior.** Runs the fixed command `hostname && uptime` — no user input
in the command at all. Returns `"Target server reachable: OK\n\nHost
info:\n<output>"`.

**Errors.** `DURA_SSH_HOST`/`DURA_SSH_KEY_PATH` unset → `NotConfigured`.
Connection failures pass through `ssh_exec`'s already-genericized error
(no host/IP/port/`"ssh"` in the text — verified at `src/dura/mod.rs:917-934`).

### `dura_backup_status`

**Purpose.** List the `/backup/` directory contents.

**Input schema.** No parameters.

**Behavior.** Fixed command `ls -la /backup/`. Returns `"Backup directory
listing:\n<output>"`.

### `dura_log_query`

**Purpose.** Fetch the last N lines of a systemd service's journal log.

**Input schema** (`src/dura/mod.rs:333-350`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `service` | string | yes | — | Must be on `DURA_ALLOWED_SERVICES` |
| `last_n_lines` | integer | no | `50` | Capped at 1000 (`raw_n.min(1000)`) |

**Behavior.** Rejects a disallowed service with `InvalidArgument` naming the
allowed set, *before* building any command. Builds `journalctl -u {service}
-n {n} --no-pager` — safe to interpolate because `service` already passed
the allowlist check and `n` is a bounded integer.

**Output shape:** `"Journal log for service '<service>' (last <n>
lines):\n<journalctl output>"` (plain text, not JSON).

### `dura_constellation_health`

**Purpose.** Health/status of all servers, hosts, and services via
Prometheus's `up` metric — the tool to reach for "how are my servers doing."

**Input schema.** No parameters.

**Behavior.** Runs the fixed PromQL query `up` (no user input) against
`PROMETHEUS_URL`, then `format_prometheus_result(&data, "job")`
(`src/dura/mod.rs:200-225`) renders each result as `"  <job>: UP"` or `"
<job>: DOWN"` based on whether the metric value is `"1"`.

**Output shape:** `"Constellation service health (Prometheus \`up\`
metric):\n  chord: UP\n  lumina: UP\n  ..."` (plain text).

**Errors.** `PROMETHEUS_URL` unset → `NotConfigured`.

### `dura_container_status`

**Purpose.** List running systemd services on the target host.

**Input schema.** No parameters.

**Behavior.** Fixed command `systemctl list-units --type=service
--state=running --no-pager`.

### `dura_disk_usage`

**Purpose.** Report disk usage.

**Input schema.** No parameters.

**Behavior.** Fixed command `df -h`.

### `dura_service_check`

**Purpose.** Prometheus UP/DOWN status for one named service.

**Input schema** (`src/dura/mod.rs:544-554`)

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `service_name` | string | yes | Must be on `DURA_ALLOWED_SERVICES` |

**Behavior.** Rejects a disallowed name with `InvalidArgument` before
querying. Builds `up{job="<service_name>"}` — safe because the value was
already allowlist-validated — and renders via the same
`format_prometheus_result` helper.

**Output shape:** `"Service check for '<name>':\n  <name>: UP"`, or `"No
Prometheus data found for service '<name>'."` when the query returns no
series.

## Security model summary

- Four tools (`dura_smoke_test`, `dura_backup_status`,
  `dura_container_status`, `dura_disk_usage`) run entirely fixed commands —
  a unit test asserts none of them contain `$(`, `` ` ``, or an unexpected
  `;` (`src/dura/mod.rs:784-809`).
- The two service-scoped tools reject any name off the allowlist before it
  reaches a command or PromQL string.
- All SSH connection-level failures are genericized — a dedicated
  regression test confirms no `"ssh"`, `"127.0.0.1"`, or `":22"` ever
  appears in a user-facing error (`src/dura/mod.rs:900-934`).

[← Infra & Ops index](README.md) · [← tool index](../README.md)
