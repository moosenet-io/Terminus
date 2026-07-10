# approval

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/approval.rs`](../../../src/approval.rs)

The `approval` module is two things: the shared **gate** (`approval::gate`)
that `ansible`, `<secret-manager>`, and any future guarded tool call at the top of
`execute()`, and two small MCP tools — `approval_grant` and
`approval_deny` — that are the *only* way a pending request ever becomes
approved or denied. This page documents both halves; the gate mechanics are
also summarized on [`ansible.md`](ansible.md#the-approval-gate-in-detail)
and [`<secret-manager>.md`](<secret-manager>.md) since those are the modules that consume it.

<img src="../../../assets/guarded-tool-gate.svg" alt="Guarded tool approval gate sequence: pending row created, operator approves out of band via Matrix, re-dispatch consumes the code exactly once" width="100%">

## The gate: `approval::gate(tool_name, args, summary) -> Gate`

`Gate` is an enum with three outcomes (`src/approval.rs:40-47`):

| Variant | Meaning | Caller's obligation |
| --- | --- | --- |
| `Granted` | Approved and just consumed (single-use) | The tool proceeds to do its real work |
| `Pending(String)` | No/invalid code was supplied; a new request was created | The tool returns this string as its **result**, without running the real action |
| `Denied(String)` | A code was supplied but is invalid/expired/used, or the approval system itself is unreachable | Same — return the string, do not run |

Both `Pending` and `Denied` are surfaced as `Ok(message)` from the guarded
tool's `execute()`, never as a Rust `Err` — this is deliberate so the model
sees a normal tool result explaining what to do next, not an MCP-level
error.

### Storage: `tool_approvals` (Postgres, `DATABASE_URL`)

Grants live in the `lumina_inbox` Postgres database, shared between this
crate (the sweep-harness/tool-hub host) and lumina-core (the orchestrator
container) — this is what lets an operator approve a request in Matrix chat
and have the *next* tool-call redeliver against the same row. If
`DATABASE_URL` is unset, `gate()` cannot even connect and returns
`Gate::Denied("Approval system unavailable: ...")` — this is the "approval
system down" failure mode distinct from "request denied."

### Code generation

`gen_code(seed, salt)` (`src/approval.rs:59-79`) produces a 6-character code
from an unambiguous 33-character alphabet
(`ABCDEFGHJKLMNPQRSTUVWXYZ23456789` — no `I`/`O`/`0`/`1`) seeded from the
current nanosecond timestamp, an FNV-1a-style hash of `"{tool_name}|{summary}"`,
and a salt. `gate()` tries salts `0..6` and retries on an insert collision
before giving up with `Gate::Denied("Could not create an approval request
(repeated code collision).")`.

### Content-binding (why args, not just tool name, are checked)

A code is scoped to `(tool_name, args)`, not just `tool_name`
(`src/approval.rs:18-29`). `content_of(args)` (`src/approval.rs:85-91`)
strips the `_approval_code` field and is used as **both** the value stored
at proposal time and the value compared at redemption time — the same
function on both sides, so they can never drift out of sync. Redemption is
a single atomic SQL statement:

```sql
UPDATE tool_approvals SET status = 'consumed', consumed_at = now()
WHERE code = $1 AND tool_name = $2 AND status = 'approved'
  AND expires_at > now() AND consumed_at IS NULL
  AND args_json = $3
RETURNING code
```

Without the `args_json = $3` clause, a code approved for one call (e.g. "run
this specific playbook") could be redeemed against a *different* set of
arguments for the same tool (e.g. a more destructive playbook), because
between approval and redemption the caller controls what args it resends.
This was an adversarial-review finding against a routines-tools port and
was fixed here so every guarded tool inherits the protection, not just
routines (`src/approval.rs:18-29`).

### Expiry

Pending requests expire 10 minutes after creation (enforced in the same SQL
`WHERE expires_at > now()` clause; the message text also tells the operator
"expires in 10 minutes").

## Tools

### `approval_grant`

**Purpose.** INTERNAL — mark a pending approval as approved and return the
stored tool name + args so the caller (lumina-core's deterministic
`approve <CODE>` handler) can re-dispatch the original call.

**Input schema** (`src/approval.rs:180`)

| Field | Type | Required |
| --- | --- | --- |
| `code` | string | yes |

**Behavior.**
```sql
UPDATE tool_approvals SET status='approved'
WHERE code=$1 AND status='pending' AND expires_at > now()
RETURNING tool_name, args_json
```
Success:
```json
{"approved": true, "tool_name": "ansible_run_playbook", "args": {"playbook_name": "ping"}}
```
No matching row (already handled, expired, or never existed):
```json
{"approved": false, "error": "No pending approval for code ABC123 (already handled or expired)."}
```

**Auth note.** This tool is never callable by the model — chord-proxy
hard-blocks `approval_grant`/`approval_deny` from the agentic loop entirely.
It is invoked exclusively by lumina-core's deterministic `approve <CODE>`
command handler, which is **not an LLM turn** — a plain string-match command
handler in the orchestrator, so the model can never manufacture its own
approval.

### `approval_deny`

**Purpose.** INTERNAL — reject a pending approval. Operator-only, same
non-LLM invocation path as `approval_grant`.

**Input schema.** Same single required `code: string` field.

**Behavior.**
```sql
UPDATE tool_approvals SET status='denied' WHERE code=$1 AND status='pending'
```
Returns `{"denied": true|false, "code": "ABC123"}` — `denied: false` when no
pending row matched (already handled/expired).

## Security model summary

- The gate is fail-closed: no `DATABASE_URL` means every guarded call is
  denied, never silently allowed.
- Content-binding means a code can only ever redeem the exact args it was
  issued for.
- `approval_grant`/`approval_deny` are architecturally unreachable from the
  model — they exist only for the operator's own deterministic chat
  command, enforced at the chord-proxy dispatch layer, not just by
  convention in this module.
- Codes are single-use (`consumed_at` set atomically on redemption) and
  time-boxed (10 minutes).

[← Infra & Ops index](README.md) · [← tool index](../README.md)
