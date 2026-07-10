# network

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/network/mod.rs`](../../../src/network/mod.rs)

The `network` module provides five diagnostic tools, ported 1:1 from a
legacy Python `network_tools.py` with identical names and parameters
(`src/network/mod.rs:1-8`). It is a hybrid module: two tools
(`net_ping`, `net_subnet_scan`) need ICMP echo, which the `RustTool`
contract's ban on shelling out makes impossible to do directly in pure
Rust without root — so they run a *fixed-form* `ping` command on a
configured diagnostics host via typed SSH (the same `ssh2` pattern as
`dura`). The other three (`net_port_check`, `net_dns_lookup`,
`net_check_services`) need no shell at all and run entirely in pure Rust
(`src/network/mod.rs:10-19`).

<img src="../../../assets/fleet-ssh-bridge-flow.svg" alt="Typed SSH bridge for the ICMP-dependent tools; the TCP/DNS tools bypass this path entirely" width="100%">

## Configuration

| Env var | Purpose | Default |
| --- | --- | --- |
| `NET_SSH_HOST` | Host to run `ping`/subnet-scan commands on | none — required for `net_ping`/`net_subnet_scan` |
| `NET_SSH_USER` | SSH user | `root` |
| `NET_SSH_KEY_PATH` | SSH private key path | none — required for the SSH tools |
| `NET_SERVICES` | `name=host:port,name2=host:port` list used by `net_check_services` | none — unset means that tool returns `NotConfigured` |

## Input validation (why SSH commands here are safe)

Every token that reaches a remote command string is validated first:

- `validate_host_token` (`src/network/mod.rs:122-138`): accepts only ASCII
  alphanumerics, `.`, `-`, `:` (for IPv6), max 253 chars — rejects
  whitespace, `;`, `` ` ``, `$(`, `|`, `&`, anything shell-meaningful.
- `validate_subnet_prefix` (`src/network/mod.rs:142-165`): one to three
  dotted numeric octets, each `0-255`.

Both reject their inputs with `InvalidArgument` *before* any SSH command is
built — confirmed by a regression test that a malicious host string like
`"1.1.1.1; rm -rf /"` never reaches `ssh_exec` (`src/network/mod.rs:882-889`).

## Tools

### `net_ping`

**Purpose.** Ping a host and report latency.

**Input schema** (`src/network/mod.rs:320-328`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `host` | string | yes | — | IP or hostname; validated via `validate_host_token` |
| `count` | integer | no | `3` | Clamped to `1..=10` |

**Behavior.** Builds the fixed-form command `ping -c {count} -W 3 {host}`
and runs it over SSH on `NET_SSH_HOST`. `parse_ping_output`
(`src/network/mod.rs:246-258`) extracts the `"packets transmitted"` line
and the `rtt`/`round-trip` summary line from the ping output.

**Output shape:**
```json
{"host": "203.0.113.10", "reachable": true, "stats": "3 packets transmitted, 3 received, 0% packet loss, time 2003ms",
 "rtt": "rtt min/avg/max/mdev = 0.450/0.475/0.500/0.025 ms", "output": "<full ping stdout>"}
```
`reachable` reflects the SSH-command exit code (`0` = reachable), not a
parse of the loss percentage.

**Errors / edge cases.** Missing/empty `host` → `InvalidArgument`. Host with
disallowed characters → `InvalidArgument` mentioning `"disallowed"`, before
any SSH attempt. `NET_SSH_HOST`/`NET_SSH_KEY_PATH` unset →
`NotConfigured`, again checked before any network I/O attempt (validation
always runs first, config-check second).

### `net_port_check`

**Purpose.** Check whether a TCP port is open — pure Rust, no SSH.

**Input schema** (`src/network/mod.rs:382-392`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `host` | string | yes | — |
| `port` | integer | yes | — must be `1..=65535` |
| `timeout` | integer | no | `3` seconds, clamped `1..=60` |

**Behavior.** Resolves `host:port` via `to_socket_addrs()` and attempts
`TcpStream::connect_timeout`. DNS resolution failure is folded into a soft
`Ok` result (`{"open": false, "error": "DNS resolution failed"}`) rather
than a hard error, so a bad hostname doesn't crash the caller's flow.

**Output shape:**
```json
{"host": "gitea.example", "port": 3000, "open": true, "status": "open"}
```

### `net_dns_lookup`

**Purpose.** Resolve a hostname via the system resolver — pure Rust.

**Input schema.** `hostname` (string, required).

**Behavior.** `(hostname, 0u16).to_socket_addrs()` — port `0` lets the OS
resolver return name-only results (matching Python's
`getaddrinfo(host, None)`). Results are sorted and de-duplicated.

**Output shape (success):**
```json
{"hostname": "gitea.internal", "resolved": true, "addresses": ["203.0.113.23"]}
```
**Output shape (failure — still `Ok`, not `Err`):**
```json
{"hostname": "nonexistent.invalid", "resolved": false, "error": "<resolver error text>"}
```

### `net_subnet_scan`

**Purpose.** Quick parallel ping sweep of an IP range via SSH.

**Input schema** (`src/network/mod.rs:534-542`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `subnet_prefix` | string | no | a private-range /24 prefix (see source) | 1-3 dotted octets, validated |
| `start` | integer | no | `1` | First host octet, `0-255` |
| `end` | integer | no | `254` | Last host octet, `0-255`; capped to `start + 254` if the range exceeds 254 hosts |

**Behavior.** Builds a shell loop that pings each host in the range in the
background and echoes the ones that respond:
```
for i in $(seq {start} {end}); do (ping -c 1 -W 1 {prefix}.$i >/dev/null 2>&1 && echo {prefix}.$i) & done; wait
```
All interpolated tokens (`start`, `end`, `prefix`) are validated numeric/
dotted-octet values — no user string reaches this template unchecked.
`parse_subnet_hosts` (`src/network/mod.rs:262-275`) collects non-empty
output lines, sorts by final octet, and de-duplicates.

**Output shape:**
```json
{"subnet": "203.0.113.1-254", "hosts_up": 12, "hosts": ["203.0.113.1", "203.0.113.2", "203.0.113.10", ...]}
```

**Errors / edge cases.** `start > end` → `InvalidArgument`. `start`/`end` >
255 → `InvalidArgument`. Bad prefix (e.g. 4 octets, non-numeric, out of
range) → `InvalidArgument` from `validate_subnet_prefix`. Missing
`NET_SSH_HOST` → `NotConfigured`, after validation succeeds.

### `net_check_services`

**Purpose.** Quick TCP-reachability health check of named MooseNet services
— pure Rust, no SSH.

**Input schema.** No parameters — targets come entirely from `NET_SERVICES`.

**Behavior.** `parse_services` (`src/network/mod.rs:90-112`) parses
`"Name=host:port,Name2=host:port"`, silently skipping malformed entries
(missing `=`, missing `:`, non-numeric port, empty name/host) rather than
erroring on the whole list. For each configured target, attempts a 2-second
`tcp_port_open` and reports `up`/`down`.

**Output shape:**
```json
{"total": 3, "up": 2, "down": 1, "services": [
  {"service": "Gitea", "host": "gitea.example:3000", "status": "up"},
  {"service": "Plane", "host": "plane.example:80", "status": "down"}
]}
```

**Errors / edge cases.** `NET_SERVICES` unset or parses to zero valid
entries → `ToolError::NotConfigured("NET_SERVICES is not set (format:
'Name=host:port,Name2=host:port')")`.

## Security model summary

- SSH commands are always either fully fixed or built only from
  characters/ranges already validated — never raw user text.
- Connection-level SSH failures are genericized (no host/IP/port/`"ssh"`
  leaked in the error text) — verified by a dedicated regression test
  (`src/network/mod.rs:701-723`).
- The three pure-Rust tools have no shell surface at all.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
