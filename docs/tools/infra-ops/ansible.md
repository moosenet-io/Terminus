# ansible

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/ansible/mod.rs`](../../../src/ansible/mod.rs)

The `ansible` module runs allowlisted Ansible playbooks on a dedicated
control host over a typed SSH session. It is a Tier-2 port of a legacy
Python `ansible_tools.py` that shelled out to `ansible-playbook` via
`subprocess` — this Rust version replaces the subprocess call with the
`ssh2` crate (no `shell=True`, no string-built shell pipelines) and adds a
hard, fail-closed playbook allowlist (`src/ansible/mod.rs:1-24`).

**All four tools are GUARDED.** Every `execute()` calls
[`approval::gate()`](approval.md) before doing any real work
(`src/ansible/mod.rs:283-290,415-420,491-496,549-555`). A call arrives
without operator sign-off, gets refused with an "APPROVAL REQUIRED" message,
and the operator approves out of band in chat.

<img src="../../../assets/guarded-tool-gate.svg" alt="Guarded tool approval gate sequence: pending row created, operator approves out of band, re-dispatch consumes the code exactly once" width="100%">

## Configuration

All configuration is environment-only (`AnsibleConfig::from_env`,
`src/ansible/mod.rs:75-100`) — no hardcoded host, user, key path, or
playbook names.

| Env var | Purpose | Default |
| --- | --- | --- |
| `ANSIBLE_HOST` | SSH host of the ansible control node | none — required for any SSH-executing tool |
| `ANSIBLE_USER` | SSH user | `root` |
| `ANSIBLE_SSH_KEY` | Path to the SSH private key file | none — required |
| `ANSIBLE_PLAYBOOK_ROOT` | Directory holding playbooks on the host | none — required |
| `ANSIBLE_INVENTORY_PATH` | Inventory file path on the host | none — required |
| `ANSIBLE_PLAYBOOK_ALLOWLIST` | Comma-separated list of playbook names that may be run | none — **fail closed**: every playbook is refused until this is set (`src/ansible/mod.rs:64-70,110-116`) |

As of the 2026-07 PII remediation, `ANSIBLE_PLAYBOOK_ALLOWLIST` has **no
compiled-in fallback list** of real playbook names. Missing it means
`AnsibleConfig::allowlist` is `None`, `is_allowed()` returns `false` for
every name, and `require_allowlist()` returns
`ToolError::NotConfigured("ANSIBLE_PLAYBOOK_ALLOWLIST is not set")` before
any SSH attempt (`src/ansible/mod.rs:102-116`, tests at
`src/ansible/mod.rs:647-659`).

## The approval gate in detail

`approval::gate(tool_name, args, summary)` (defined in
[`src/approval.rs`](../../../src/approval.rs), shared by every guarded tool
in this domain) is called first in every `ansible_*` tool's `execute()`:

1. If `args` carries `_approval_code` and that code is `approved`,
   unexpired, unconsumed, **and** the rest of the args (with the code
   stripped) match byte-for-byte what was pending when the operator
   approved it, the row is atomically flipped to `consumed` and the tool
   proceeds (`Gate::Granted`).
2. Otherwise a fresh 6-character code (unambiguous alphabet, no `I`/`O`/`0`/`1`)
   is inserted as a `pending` row in `tool_approvals` (Postgres,
   `DATABASE_URL`) and the tool returns, without executing,
   `"⚠️ APPROVAL REQUIRED — ... Reply `approve <CODE>` ..."`.
3. `DATABASE_URL` unset means the approval system itself is unavailable —
   `Gate::Denied("Approval system unavailable: ...")`, and the tool still
   does not execute.

The content-binding check (args match, not just tool name) exists so a code
approved for one call cannot be replayed against a *different* set of
arguments for the same tool. `approval_grant`/`approval_deny`
(`approval.md`) are the only way a pending row becomes `approved` — both are
hard-blocked from the agentic loop by chord-proxy, so the model can never
approve its own request.

## Tools

### `ansible_run_playbook`

**Purpose.** Run one allowlisted playbook via `ansible-playbook -i
<inventory>` over SSH.

**Input schema** (`src/ansible/mod.rs:263-274`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `playbook_name` | string | yes | — | Name without `.yml`. Must be on `ANSIBLE_PLAYBOOK_ALLOWLIST`. |

**Behavior.**

1. Validate `playbook_name` is a string (`InvalidArgument` otherwise).
2. Run the approval gate; a pending/denied result is returned verbatim as
   the tool's `Ok` result — the real action never runs.
3. `require_allowlist()` — `NotConfigured` if the allowlist env var is
   unset at all.
4. `is_allowed(playbook_name)` — if not on the list, returns (not errors) a
   JSON body `{"allowed": false, "playbook": ..., "returncode": null,
   "stdout": "", "stderr": "Playbook '<name>' is not on the allowlist:
   [...]"}` (`src/ansible/mod.rs:296-308`).
5. If allowed, builds `ansible-playbook <root>/<name>.yml -i <inventory>` —
   `playbook_name` is known-safe because it was just checked against the
   allowlist, so it is not arbitrary shell input even though it is
   string-interpolated.
6. Runs over SSH (120s timeout), records the result in an in-memory
   `LastRun` state shared with `ansible_last_run_status` /
   `ansible_view_run_log`, and returns
   `{"allowed": true, "playbook", "returncode", "stdout", "stderr"}`.

**Output shape (success path):**

```json
{
  "allowed": true,
  "playbook": "ping",
  "returncode": 0,
  "stdout": "...",
  "stderr": ""
}
```

**Errors / edge cases.**
- `playbook_name` not a string → `ToolError::InvalidArgument`.
- Allowlist unset → `ToolError::NotConfigured("ANSIBLE_PLAYBOOK_ALLOWLIST is not set")`.
- Allowlist set but name not on it → `Ok` result with `"allowed": false` (not an error).
- `ANSIBLE_PLAYBOOK_ROOT` / `ANSIBLE_INVENTORY_PATH` unset → `NotConfigured`, checked *after* the allowlist check.
- SSH connect/handshake/auth failure → `ToolError::Execution` with a genericized message ("The target server is unreachable." style — see `dura`/`network` for the same pattern; `ansible` itself does not have a dedicated regression test for wording genericization, but uses the same `ssh_exec` shape).

**Auth / guard notes.** GUARDED. Requires operator approval every call — no
"remember this approval" behavior; each `_approval_code` is single-use.

**Worked example.**

Request:
```json
{"tool": "ansible_run_playbook", "arguments": {"playbook_name": "ping"}}
```
First call (no code) → response text: `"⚠️ APPROVAL REQUIRED — \`ansible_run_playbook\` is a guarded tool and was NOT run.\nAction: Run Ansible playbook 'ping' on the ansible control host via ansible-playbook\nReply \`approve ABC123\` to authorize this single call (expires in 10 minutes), or \`deny ABC123\` to reject."`

After the operator replies `approve ABC123` in chat, lumina-core
re-dispatches the same call with `_approval_code: "ABC123"` added, and the
tool now runs for real, returning the `{"allowed": true, ...}` shape above.

### `ansible_list_playbooks`

**Purpose.** List every `*.yml` file under `ANSIBLE_PLAYBOOK_ROOT` on the
host and flag which are allowlisted.

**Input schema.** No parameters (`src/ansible/mod.rs:406-412`).

**Behavior.**

1. Approval gate first.
2. `ANSIBLE_PLAYBOOK_ROOT` required (`NotConfigured` otherwise).
3. Runs the fixed command `ls -1 <root>/*.yml 2>/dev/null` over SSH (30s
   timeout) — no user input in the command at all.
4. On a non-zero SSH exit, returns `{"error": <stderr>, "playbooks": []}`
   rather than erroring the tool.
5. On success, `require_allowlist()` runs (so an unset allowlist still fails
   here even though the `ls` succeeded), then
   `parse_playbook_listing(stdout, allowlist)` (`src/ansible/mod.rs:370-393`)
   strips each line to a bare name, tags `allowed: true/false`, and sorts
   allowed-first, then alphabetically within each group.

**Output shape:**

```json
{
  "total": 4,
  "allowed_count": 2,
  "allowlist": ["deploy-plane", "ping"],
  "playbooks": [
    {"name": "deploy-plane", "allowed": true,  "path": "/opt/ansible/playbooks/deploy-plane.yml"},
    {"name": "ping",         "allowed": true,  "path": "/opt/ansible/playbooks/ping.yml"},
    {"name": "another-unlisted", "allowed": false, "path": "..."},
    {"name": "zeta-unknown",     "allowed": false, "path": "..."}
  ]
}
```

**Errors / edge cases.** `ANSIBLE_PLAYBOOK_ROOT` unset → `NotConfigured`.
`ANSIBLE_PLAYBOOK_ALLOWLIST` unset → `NotConfigured` (checked after the SSH
listing succeeds, so a listing failure is reported first if both are
wrong). Non-zero `ls` exit → soft `{"error": ...}` shape, not a hard error.

### `ansible_last_run_status`

**Purpose.** Report the outcome of the last playbook run *this process*
performed. In-memory only — resets on restart, and is not persisted or
shared across replicas.

**Input schema.** No parameters.

**Behavior.** Approval gate first. If no playbook has run yet, returns
`{"message": "No playbook has been run since the MCP server started."}`.
Otherwise returns a summary (not the full log — see `ansible_view_run_log`
for stdout/stderr):

```json
{
  "playbook": "ping",
  "returncode": 0,
  "success": true,
  "timestamp": "2026-07-06T08:00:00Z",
  "has_output": true
}
```

**Errors / edge cases.** The approval-gate test suite specifically confirms
this tool does not leak the stored playbook name through a denied-gate
response (`src/ansible/mod.rs:830-859`) — a gated-out call never contains
`"playbook"` in its output text.

### `ansible_view_run_log`

**Purpose.** Retrieve the full stdout/stderr of the last run this process
performed.

**Input schema.** No parameters.

**Behavior.** Approval gate first, then returns the same "no run yet"
message as `ansible_last_run_status` if nothing has run, or:

```json
{
  "playbook": "ping",
  "returncode": 0,
  "timestamp": "2026-07-06T08:00:00Z",
  "stdout": "...",
  "stderr": ""
}
```

## Security model summary

- Every tool is GUARDED via the shared approval gate.
- `ANSIBLE_PLAYBOOK_ALLOWLIST` fails closed — unset means every playbook is
  refused, not "allow all" or a guessed default.
- `playbook_name` only ever reaches a command string after an exact-match
  allowlist check, so it cannot be used for command injection even though
  it is string-interpolated.
- All host/user/key/path values come from environment variables — no
  hardcoded IPs, users, or credentials anywhere in this module.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
