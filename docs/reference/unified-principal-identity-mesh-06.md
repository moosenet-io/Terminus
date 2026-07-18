## Unified `Principal` identity (MESH-06)

Terminus can see a caller's identity through up to two independent
transports — the mTLS client cert's Subject CN (`crate::pki::mtls::ClientIdentity`)
and the tailnet WhoIs identity (`crate::mesh::TailnetIdentity`, MESH-05) — plus
a third, separate identity concept: the named-PAT credential model
(`PLANE_PAT_<NAME>` / `GITEA_PAT_<NAME>` / `GITHUB_PAT_<NAME>`) used to
authenticate outbound calls. `crate::mesh::Principal` and
`crate::mesh::PrincipalResolver` reconcile these into one canonical identity
`name`, in the same string space the named-PAT lookups already use, that
drives both the gateway allowlist/RBAC decision
(`crate::gateway_framework::GatewayFramework::guard`, which now takes a
`Principal` rather than a raw `ClientIdentity`) and downstream PAT selection.

Configured via `TERMINUS_MESH_PRINCIPAL_MAP_JSON` — non-secret structural
JSON, same convention as `TERMINUS_MESH_UPSTREAMS_JSON` above:

```json
{
  "cert_cn": { "harmony-primary.example.test": "harmony" },
  "tailnet_login": { "<email>": "moose" },
  "tailnet_tag": { "tag:ci": "claude" }
}
```

Resolution is fail-closed and deterministic: a present mTLS cert CN is
checked first and exclusively — mapped, it wins outright (even over a
conflicting tailnet mapping); unmapped, the request is denied without
falling back to the tailnet identity. The tailnet login/tag maps are only
consulted when no cert is presented at all. Neither transport identity
present, or the one presented has no mapping entry, is always denied — never
a silent pass-through of the raw transport identity. See
[`docs/architecture/auth.md`](docs/architecture/auth.md#unified-principal-identity-mesh-06)
for the full precedence rule and edge cases (e.g. a resolved name with no
provisioned PAT credential).

MESH-06 delivers the model, the resolver, and `guard()`'s new signature.
Wiring the resolver into the live request path (replacing the interim
`sub="lumina"` pin / `X-Terminus-Client-Identity` header workaround) is
MESH-07 — existing callers keep working today via a direct, resolver-bypassing
conversion (`Principal::from(&ClientIdentity)`) that uses the raw cert CN as
the principal name, unchanged from pre-MESH-06 behavior.

### Catalog merge, namespacing, and routing

`tools/list` on `/mcp` merges the local core catalog with every currently
healthy mesh upstream's tools into one list (`crate::mesh::merge`). Local
core tools (and the pre-existing single personal-registry federation) are
advertised **unprefixed**, exactly as before the mesh existed. Every tool
sourced from a mesh upstream is advertised as:

```
<namespace>__<tool>
```

using that upstream's registered `namespace` (see the table above) as the
prefix, separated by a literal double underscore (`__`). This means two
upstreams can each export a tool with the same bare name (e.g. both export
`echo`) without colliding on the merged catalog — they show up as
`nsa__echo` and `nsb__echo`, each with an unambiguous, explicit source. Only
the FIRST `__` in a name is treated as the namespace boundary, so an
upstream tool whose own bare name happens to contain `__` still round-trips
correctly (`namespaced("ns", "foo__bar") == "ns__foo__bar"`, which splits
back to `("ns", "foo__bar")`).

`tools/call` routes on this same convention: a namespaced name has its
`<namespace>__` prefix stripped and is dispatched to the owning upstream; any
other name (including a `__`-shaped name whose prefix isn't a currently
known mesh namespace) dispatches locally, unchanged from pre-mesh behavior.
If a namespaced call's owning upstream is currently unhealthy or was
excluded from the pool entirely (e.g. a missing credential at startup), the
call returns a clean tool-error ("mesh upstream \"<namespace>\" is currently
unavailable") rather than a panic, a 500, or a silent fallback to local
dispatch. When the mesh registry/pool is empty or disabled
(`TERMINUS_MESH_ENABLED` unset), this is all a no-op: `tools/list`/
`tools/call` behave exactly as they did before the mesh existed.

### Per-upstream, per-tool RBAC over namespaced tools (MESH-08)

`crate::gateway_framework::AllowlistPolicy` (`TERMINUS_GATEWAY_ALLOWLIST_JSON`,
see `.env.example`) grants a `Principal` access by tool/route NAME — as of
MESH-08 that name may be a plain local tool, or a mesh namespaced name
(`<namespace>__<tool>`, see the catalog-merge section above), so one policy
covers both. An allow entry (in either the legacy plain-array `Grant::List`
form or the `{"allow": [...], "deny": [...]}` `Grant::AllowDeny` form) may be:

| Entry | Grants |
| --- | --- |
| `"*"` | every tool/route, local or namespaced |
| `"ct322__*"` | every tool currently exported by the mesh upstream registered under namespace `ct322` (any entry ending in `*` is a prefix wildcard — not just the bare `"*"` entry) |
| `"ct322__ledger_add"` | exactly that one namespaced tool |
| `"ledger_add"` | a plain local tool (unchanged, pre-mesh behavior) |

A `deny` PREFIX (`Grant::AllowDeny` only) is checked against the action as
given **and**, for a namespaced action, against its bare (post-`__`) tool
name too — so `DEFAULT_SENSITIVE_DENY_PREFIXES` entries authored against bare
names (e.g. `"github_"`) keep closing off a sensitive tool no matter which
upstream namespace re-exports it: `deny: ["github_"]` blocks both
`github_push_repo` and `ct322__github_push_repo`. Deny always wins over an
overlapping `allow`, including `allow: ["*"]` — unchanged from LHEG-07.

**Visibility == enforcement, by construction.** `tools/list` filters the
merged catalog down to exactly the tools the resolved `Principal` may call
(`GatewayFramework::filter_catalog_for_principal`, driven by
`AllowlistPolicy::filter_tools`) and `tools/call` gates on the same namespaced
name via the same `AllowlistPolicy::is_allowed` decision — a tool is never
advertised to a caller who couldn't then call it, and never callable without
first being visible. An unmapped `Principal` (no entry in
`TERMINUS_GATEWAY_ALLOWLIST_JSON` at all, and not one of the
`SCAFFOLDED_IDENTITIES`) sees an EMPTY catalog and has every call denied —
default-deny, exactly like the pre-MESH-08 single-namespace allowlist. A
grant that references a namespace with no live/registered upstream is inert
(matches nothing, no error) — an operator can pre-author a grant for an
upstream that isn't deployed yet.

Example — grant `ct322-viewer` every `ct322` tool except its sensitive
`vitals_*` ones, and nothing else at all:

```json
{"ct322-viewer": {"allow": ["ct322__*"], "deny": ["ct322__vitals_"]}}
```

### git-public full-history replay (GHIST)

The git-public mirror engine can publish a repo's ENTIRE commit history as a
PII-scrubbed derivative, not just a single swept snapshot — so a public mirror
carries genuine, dated development history. `forge::mirror::history::replay_full_history`
drives `git fast-export` on the (read-only) source, rewrites the byte stream
in-process — every text blob through the native `DeterministicCleaner`, binary/
oversized/non-UTF-8 blobs byte-identical — and `git fast-import`s the result into a
fresh work-dir. The commit graph, messages, and author DATES are preserved (so the
public contribution history matches internal), while every historical blob is
scrubbed. A full-history PII gate (scanning every replayed commit's tree, not just
the tip) and contribution-attribution remapping build on this; the one-time backfill
and per-commit going-forward replay are driven by the mirror history tools:
`git_public_history_status` (lineage state — is a backfill established, internal vs
work-dir commit counts, how far behind) and `git_public_history_backfill` (produce/
update the scrubbed full-history mirror + gate EVERY commit; NEVER pushes — a
gate-clean result is a blessable snapshot for the operator to spot-check and force
re-baseline; requires `TERMINUS_MIRROR_AUTHOR_MAP` so authors are remapped).

### Approval-gate propagation across the mesh (MESH-09)

Federation is never a way to dodge human approval. Guarded tools
(`infisical_*`, `ansible_*`, `openhands_*`, and the state-mutating
`routines_propose`/`routines_pending`/`routines_approve`/
`git_public_mirror_approve`/`git_public_mirror_push` — see
`approval::is_guarded`) are enforced **at this gateway**, even when the
guarded tool actually lives on a remote mesh upstream:

- `tools/call` resolving a namespaced name to `CallRoute::Upstream` checks
  `approval::is_guarded` against the **bare** (de-namespaced) tool name —
  `ct322__ansible_run_playbook` is gated exactly like a local
  `ansible_run_playbook` — and, if guarded, runs the same
  `approval::gate()` local tools use, **before** the call is ever forwarded
  to the upstream. Federation never bypasses the human-approval gate; it is
  not something an upstream is trusted to enforce on our behalf.
- The gated content includes the target upstream's namespace
  (`approval::mesh_gate_args`), so a code approved for one upstream's tool
  cannot be replayed against another upstream's tool of the same bare name
  (or against the local tool of that name) — cross-upstream replay is
  rejected the same way a differing-args replay already is (see
  "Content-binding" in `src/approval.rs`).
- This gateway gate is **authoritative and independent** of any approval
  gate the upstream itself may also run for the same tool — double-gating
  is fine and expected, never skipped on the assumption the upstream
  already checked.
- If the call is approved but then fails to actually reach the upstream
  (a transport/connectivity error), the one-time code is **not** treated as
  spent — the grant is rolled back (`approval::unconsume`) so the operator's
  same approval can be retried once the upstream is healthy again, instead
  of requiring a fresh `approve <CODE>` for a call that never ran.

### Onboarding a new upstream (`mesh_onboard_upstream`)

Adding an entry to `TERMINUS_MESH_UPSTREAMS_JSON` by hand risks a typo'd
namespace collision or an unreachable/misconfigured candidate you only
discover after restarting. The CORE tool `mesh_onboard_upstream`
(`crate::mesh::onboarding`) is a **read-only dry-run** workflow to try a
candidate first:

1. Probes the candidate (`initialize` + `tools/list`, plus a best-effort
   `GET /healthz`) via a real `UpstreamClient` built for it.
2. Checks the proposed `name`/`namespace` against the currently-configured
   mesh registry (loaded from `TERMINUS_MESH_UPSTREAMS_JSON`) — a taken
   namespace is rejected with up to three free alternative suggestions.
3. Confirms trust readiness: for `mtls`, that this node's embedded CA
   (`crate::pki::ca`) bootstraps and can mint the client identity the
   candidate will trust (mesh peers share one embedded-CA trust domain, so
   there is no separate remote "enroll" call to drive here); for `bearer`,
   that the named `secret_key` resolves from the process environment. A
   missing/unresolvable credential blocks onboarding with a clear message —
   the secret's **value** is never read into, or printed by, this tool.
4. Previews the namespaced catalog delta (`<namespace>__<tool>` for every
   discovered tool) the merge step would add.
5. On success, **emits** the validated JSON entry for the operator to append
   to `TERMINUS_MESH_UPSTREAMS_JSON` themselves and reload/restart — the tool
   never writes that file, or any other live config, itself.

A candidate reachable but exporting zero tools is still allowed to onboard
(with a warning); an unreachable candidate fails cleanly with nothing
written.

```json
{
  "name": "mesh_onboard_upstream",
  "arguments": {
    "name": "fleet-c",
    "url": "https://fleet-c.example.internal:8443",
    "transport": "bearer",
    "namespace": "fleetc",
    "secret_key": "TERMINUS_MESH_FLEETC_TOKEN"
  }
}
```

### Federated audit trail (MESH-10)

Every `tools/call` gated by `crate::gateway_framework` (see MESH-08 above) is
audited via `crate::gateway_framework::audit::AuditEntry` — S6-sanitized
(secret-shaped `key=value`/`Bearer <token>` values redacted to
`***REDACTED***`, bodies truncated past 200 chars), one entry per request,
whether the request was denied, dispatched-and-succeeded, or
dispatched-and-failed. As of MESH-10 that entry carries the FULL federated
shape, not just identity/action/result:

| Field | Meaning |
| --- | --- |
| `principal` | The resolved caller (`crate::mesh::Principal::name()`) — same value as `identity`, but the field a federated-audit reviewer keys on. |
| `upstream` | The mesh namespace this call routed to (e.g. `"ct322"` for a `ct322__ledger_add` call), or `null`/absent for a local (non-federated) call. |
| `tool_advertised` | The tool name exactly as the caller sent it — namespaced for a federated call. |
| `tool_bare` | The tool name actually dispatched (namespace prefix stripped for a federated call; identical to `tool_advertised` for a local call). |
| `decision` | One of `allow`, `deny`, `approval_required`, `transport_failure` — the gate's decision, independent of whether a dispatched call then itself succeeded or failed (see `result` below). |
| `result` | `success` / `failure` (dispatched; underlying call succeeded/errored) or `denied_no_identity` / `denied_not_allowlisted` / `denied_rate_limited` (never dispatched). |
| `detail` | Sanitized, truncated human-readable context — a denial reason, or a summarized tool-error/args string. Never a raw payload; never an unredacted secret. |

A federated call is **always** audited, at every outcome — including the
ones easy to accidentally drop silently:

- **Denied before routing** (no identity / not allowlisted / rate-limited):
  audited with `decision: "deny"`, `upstream` populated from parsing the
  namespaced name (mesh routing itself hasn't run yet at this point, since
  the gate runs first) — see the `tools/call` handler's `Err(denial)` arm in
  `crate::mcp_server`.
- **Routed to a healthy upstream**: audited with `decision: "allow"` and
  `result` reflecting whether the upstream's own response was
  success/error.
- **Upstream unreachable or unhealthy** (`crate::mesh::CallRoute::Unavailable`,
  or a network-level failure calling a upstream the pool still believed was
  healthy): audited with `decision: "transport_failure"` — deliberately
  distinct from an ordinary `result: "failure"`, and never a silent drop
  (`GatewayContext::record_transport_failure`).
- **A guarded local tool requiring human approval** (`crate::approval`'s
  "APPROVAL REQUIRED" gate): audited with `decision: "approval_required"`.

### Onboarding a new remote client (`mesh_onboard_client`)

`mesh_onboard_upstream` (above) brings a new *server* into the mesh; this is
the companion tool for the other direction — bringing a new *client* (an
outside machine running `terminus-client-daemon`, see
[`docs/deploy/client.md`](docs/deploy/client.md)) onto it. The CORE tool
`mesh_onboard_client` (`crate::mesh::client_onboarding`):

1. Establishes the client's identity, one of two ways:
   - `"mtls_cert"` — mints a fresh short-lived leaf certificate via this
     node's embedded CA (`crate::pki::ca`, reusing the same issuance code
     TCLI-02's `/enroll` HTTP route uses), CN == the requested canonical
     name.
   - `"tailnet"` — records a tailnet login (+ optional ACL tags) → canonical
     name mapping only; no cert is issued. The mapping is valid even if the
     login has never yet been seen by tailnet WhoIs — it's enforced the
     first time that login actually connects.
2. Rejects a requested name that's already mapped to an existing principal
   in `TERMINUS_MESH_PRINCIPAL_MAP_JSON` (cert CN, tailnet login, or
   tailnet tag) — an onboarding attempt never silently re-targets an
   existing identity.
3. Seeds a **least-privilege** allowlist grant for the new name — a small,
   explicit read-only tool list (never a `"*"` wildcard, and never the
   broader allow-minus-deny shape reserved for the `lumina`/`harmony`
   scaffold). A default-allow seed is a hard review failure for this tool.
4. Emits a ready-to-use client connection profile (gateway MagicDNS name
   from `TERMINUS_MESH_GATEWAY_MAGICDNS_NAME` if configured, transport,
   identity) — never a CA private key, only the client's own freshly-minted
   key (mTLS mechanism) which the client legitimately must hold locally.
5. On success, **emits** the validated JSON snippets for the operator to
   merge into `TERMINUS_MESH_PRINCIPAL_MAP_JSON` and
   `TERMINUS_GATEWAY_ALLOWLIST_JSON` themselves and reload/restart — same as
   `mesh_onboard_upstream`, this tool never writes those files, or any other
   live config, itself. (The mTLS mechanism's cert/key ARE already
   live-issued by the embedded CA at call time — only the mesh-side mapping
   and grant config remain to be applied.)

```json
{
  "name": "mesh_onboard_client",
  "arguments": {
    "name": "dev-box-claude-code",
    "mechanism": "mtls_cert"
  }
}
```

```json
{
  "name": "mesh_onboard_client",
  "arguments": {
    "name": "moose-laptop",
    "mechanism": "tailnet",
    "tailnet_login": "<email>",
    "tailnet_tags": ["tag:remote-client"]
  }
}
```

