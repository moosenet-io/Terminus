# The Mesh: federating N upstream Terminus servers

The [federation](federation.md) page covers `terminus-primary`'s single,
hard-coded personal-registry federation. This page covers the separate,
newer **mesh** (`crate::mesh`, MESH-01..12): a config-driven registry of an
*arbitrary number* of upstream Terminus-shaped MCP servers, each namespaced
into one merged catalog, with its own identity/RBAC/audit/approval-gate
layer on top. The two federation mechanisms coexist — a deployment can use
either, both, or neither; nothing here changes the single-upstream personal
federation described in [federation.md](federation.md).

This page is the map. Each linked page/module doc is the authoritative
detail for its piece; this page's job is to show how the pieces fit and to
be honest about what's shipped versus what's still a gap.

See also: [federation.md](federation.md) · [auth.md](auth.md) ·
[../networking/tailscale.md](../networking/tailscale.md) ·
[../deploy/personal-services.md](../deploy/personal-services.md) ·
[../deploy/client.md](../deploy/client.md) · the
[top-level README's Mesh section](../../README.md#mesh-federating-multiple-upstream-terminus-servers).

## 1. Overview

```
                         tools/list, tools/call
   caller  ───mTLS/bearer───▶  terminus-primary (gateway)
                                   │
                    ┌──────────────┼──────────────┐
                    │              │               │
              local core     upstream A       upstream B
              (unprefixed)   namespace "nsa"   namespace "nsb"
                              tools as         tools as
                              nsa__<tool>      nsb__<tool>
```

- **Registry** (`src/mesh/registry.rs`): a validated list of upstream
  Terminus-shaped MCP servers, each declared as data
  (`TERMINUS_MESH_UPSTREAMS_JSON`) rather than a hard-coded client per
  backend. Gated by `TERMINUS_MESH_ENABLED`; disabled or unconfigured ⇒ an
  empty, dormant registry, never an error.
- **Client/pool** (`src/mesh/client.rs`): `UpstreamClient`/`UpstreamPool`
  dial a registered upstream over mTLS or bearer, tracking per-upstream
  health so one bad upstream never takes the others (or local dispatch)
  down.
- **Merge** (`src/mesh/merge.rs`): every currently-healthy upstream's tools
  are merged into the local `tools/list` catalog, namespaced
  `<namespace>__<tool>` (see [`§2`](#2-namespacing-and-routing) below).
  `tools/call` routes a namespaced name back to its owning upstream, or a
  clean "unavailable" tool-error if that upstream is down.
- **Identity & RBAC** (`src/mesh/principal.rs`,
  `src/gateway_framework/`): a unified `Principal` reconciles mTLS cert CN,
  tailnet WhoIs, and the named-PAT identity space into one canonical name
  that drives the same allowlist/RBAC decision for local and namespaced
  mesh tools alike.
- **Tailnet exposure** (`src/mesh/tailnet.rs`, `src/mesh/identity.rs`):
  optionally, the gateway can be its own embedded Tailscale node (`tsnet`),
  feature-gated off by default.
- **Onboarding** (`src/mesh/onboarding.rs`,
  `src/mesh/client_onboarding.rs`): read-only dry-run workflows
  (`mesh_onboard_upstream`, `mesh_onboard_client`) to try a candidate
  upstream or provision a new client identity before hand-editing config.
- **Approval-gate propagation** (`src/approval.rs`): a guarded tool
  (`ansible_*`, `infisical_*`, `openhands_*`, the `routines_*`/
  `git_public_mirror_*` state-mutators) is gated at the gateway even when
  it lives on a remote mesh upstream — federation is never a way to dodge
  human approval.

For the exact env-var reference table (`TERMINUS_MESH_ENABLED`,
`TERMINUS_MESH_UPSTREAMS_JSON`, per-entry fields) and a worked
`TERMINUS_MESH_UPSTREAMS_JSON` example, see the
[README's Mesh section](../../README.md#mesh-federating-multiple-upstream-terminus-servers)
— it is not duplicated here.

## 2. Namespacing and routing

Every mesh-sourced tool is advertised as `<namespace>__<tool>` (`MESH_NS_SEP
= "__"`, `src/mesh/merge.rs`) — local core tools and the pre-existing
personal-registry federation stay unprefixed, unchanged. This lets two
upstreams each export a tool with the same bare name (e.g. both export
`echo`) without colliding: they show up as `nsa__echo` and `nsb__echo`, and
the routing table records each advertised name's provenance.

Two distinct routing paths, deliberately different costs:

- `MergedCatalog::build` — the expensive, complete path `tools/list` uses:
  calls `list_tools()` on every currently-healthy upstream (one round trip
  each). An upstream whose call fails is excluded from this build (logged,
  not fatal).
- `resolve_call_route` — the cheap, per-call path `tools/call` uses: routes
  a single namespaced name to its owning upstream by consulting the pool's
  *current* health state only, with zero network calls. This is
  deliberately not just a `RoutingTable` lookup from the last `tools/list`
  build, because that table can go stale the moment an upstream's health
  flips.

`resolve_call_route` returns one of three outcomes (`CallRoute`):

| Outcome | When | Behavior |
| --- | --- | --- |
| `Local` | Plain name, or a `__`-shaped name whose prefix isn't a known mesh namespace | Dispatches through the existing local/personal-federated path, unchanged |
| `Upstream { client, bare_name }` | `<namespace>__<tool>` where `namespace` is a currently-healthy upstream | Dispatched to that upstream with the prefix stripped |
| `Unavailable { namespace }` | `<namespace>__<tool>` where `namespace` is registered but currently unhealthy (or excluded from the pool at startup, e.g. a missing credential) | A clean tool-error ("mesh upstream `"<namespace>"` is currently unavailable"), never a panic, 500, or silent fallback to local dispatch |

## 3. Identity & RBAC

### Principal resolution

`crate::mesh::Principal` / `PrincipalResolver` (`src/mesh/principal.rs`)
reconcile up to two transport identities into one canonical name:

1. **mTLS client cert CN** (`crate::pki::mtls::ClientIdentity`) — checked
   first and *exclusively* when present. Mapped ⇒ that name wins outright,
   even over a conflicting tailnet mapping. Unmapped ⇒ denied
   (`AuthError::UnmappedIdentity`) — never silently falls back to
   consulting the tailnet identity instead.
2. **Tailnet WhoIs identity** (`crate::mesh::TailnetIdentity`) — consulted
   only when no cert is presented: login checked first, then tags (first
   configured match wins; tag order on a node is not itself meaningful).
   Unmapped ⇒ denied.
3. Neither presented ⇒ denied (`AuthError::NoIdentityPresented`).

Configured via `TERMINUS_MESH_PRINCIPAL_MAP_JSON` (non-secret structural
JSON, three independent optional tables — `cert_cn`, `tailnet_login`,
`tailnet_tag`):

```json
{
  "cert_cn": { "harmony-primary.example.test": "harmony" },
  "tailnet_login": { "<email>": "moose" },
  "tailnet_tag": { "tag:ci": "claude" }
}
```

An absent/blank env var yields an entirely empty map — every `resolve()`
call then fails-closed, never panics, never trusts the raw transport
identity as-is. A malformed value (present but invalid JSON/shape) is a
hard startup error, not a silent empty map.

The resolved `name` lives in the same string space
`PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/`GITHUB_PAT_<NAME>` already use, so it
feeds both the allowlist/RBAC decision below and downstream PAT selection
without a translation step. See [auth.md](auth.md#unified-principal-identity-mesh-06)
for the full precedence rule, edge cases, and the current legacy-passthrough
posture (`Principal::from(&ClientIdentity)`, used until `PrincipalResolver`
is wired into the live request path — see [§6](#6-known-gaps)).

### Per-upstream, per-tool grants

`crate::gateway_framework::AllowlistPolicy` (`TERMINUS_GATEWAY_ALLOWLIST_JSON`)
grants a `Principal` access by tool/route name — local or namespaced alike:

| Entry | Grants |
| --- | --- |
| `"*"` | every tool/route, local or namespaced |
| `"ct322__*"` | every tool currently exported by the upstream registered under namespace `ct322` (any `*`-suffixed entry is a prefix wildcard) |
| `"ct322__ledger_add"` | exactly that one namespaced tool |
| `"ledger_add"` | a plain local tool (unchanged, pre-mesh behavior) |

A `deny` prefix (`Grant::AllowDeny` form only) is checked against both the
action as given and, for a namespaced action, its bare (post-`__`) tool
name — so a deny prefix authored against bare names (e.g. `"github_"`)
closes off a sensitive tool no matter which upstream namespace re-exports
it. Deny always wins over an overlapping allow, including `allow: ["*"]`.

**Visibility == enforcement, by construction**: `tools/list` filters the
merged catalog down to exactly what the resolved `Principal` may call, and
`tools/call` gates on the same namespaced name via the same decision — a
tool is never advertised to a caller who couldn't then call it. An
unmapped `Principal` sees an empty catalog and has every call denied
(default-deny). A grant referencing a namespace with no live upstream is
inert, not an error — an operator can pre-author a grant ahead of
deployment.

### Approval-gate propagation

Guarded tools (`crate::approval::is_guarded` — `infisical_*`, `ansible_*`,
`openhands_*`, the state-mutating `routines_*`/`git_public_mirror_*`
actions) are gated **at this gateway**, even when the tool actually lives
on a remote mesh upstream:

- `tools/call` resolving to `CallRoute::Upstream` checks `is_guarded`
  against the bare (de-namespaced) tool name and, if guarded, runs the same
  `approval::gate()` local tools use — *before* the call is forwarded.
- The gated content is bound to the target upstream's namespace
  (`approval::mesh_gate_args`), so an approval code for one upstream's tool
  can never be replayed against another upstream's (or the local) tool of
  the same bare name.
- This gate is authoritative and independent of whatever approval gate the
  upstream itself may also run — double-gating is expected, never skipped.
- If an approved call then fails to reach the upstream (a transport
  failure), the one-time code is **not** treated as spent — the grant is
  rolled back (`approval::unconsume`) so the same approval can be retried
  once the upstream recovers.

### Federated audit fields

Every gated `tools/call` produces exactly one S6-sanitized
`crate::gateway_framework::audit::AuditEntry`. As of MESH-10, that entry
carries the full federated shape:

| Field | Meaning |
| --- | --- |
| `principal` | The resolved caller (`Principal::name()`) |
| `upstream` | The mesh namespace this call routed to, or absent for a local call |
| `tool_advertised` | The tool name exactly as the caller sent it (namespaced for a federated call) |
| `tool_bare` | The tool name actually dispatched (prefix stripped for a federated call) |
| `decision` | One of `allow`, `deny`, `approval_required`, `transport_failure` |
| `result` | `success`/`failure` (dispatched), or `denied_no_identity`/`denied_not_allowlisted`/`denied_rate_limited` (never dispatched) |
| `detail` | Sanitized, truncated context — never a raw payload or unredacted secret |

Every outcome is audited, including the ones easy to drop silently: a
pre-routing denial (identity/allowlist/rate-limit), a routed call to a
healthy upstream, an unreachable/unhealthy upstream
(`decision: "transport_failure"`, deliberately distinct from an ordinary
`result: "failure"`), and a guarded call requiring approval
(`decision: "approval_required"`).

## 4. Tailscale / tsnet deployment

The mesh can optionally make the gateway its own embedded Tailscale node
(`src/mesh/tailnet.rs`) — no host `tailscaled` daemon required — as an
alternative to (not a replacement for) joining a host-level tailnet the
ordinary way. See [../networking/tailscale.md](../networking/tailscale.md#alternative-embedded-tsnet-mesh-04--no-host-tailscaled-at-all)
for the full walkthrough; summarized here:

- **Compile-time**: `--features tsnet` (off by default). This pulls in the
  `tsnet` crate, whose build script invokes `go build -buildmode=c-archive`
  against vendored Go source — **a Go toolchain (`go` on `$PATH`) is
  required on the build host**, in addition to Rust. A host without Go
  cannot build this feature at all; that is expected, not a bug.
- **Runtime**: `TERMINUS_MESH_TAILNET_ENABLED` (bool-ish, same truthiness
  rule as `TERMINUS_MESH_ENABLED`) plus:
  - `TERMINUS_TSNET_HOSTNAME` — the MagicDNS hostname this node advertises.
  - `TERMINUS_TSNET_STATE_DIR` — local directory tsnet persists node
    state/keys under (created if missing, probed for write access).
  - `TERMINUS_TSNET_AUTHKEY` — the tailnet auth key, <secret-manager>-hydrated
    into the process environment like every other secret this crate reads
    (never a literal). Read via `std::env::var`, immediately wrapped in
    `ResolvedSecret` so a stray `{:?}`/`{}` can never leak it.
- Both the compile feature **and** the runtime flag must be on for
  `terminus_primary` to bind the tailnet listener; either off leaves it
  byte-for-byte unchanged from a non-mesh deployment.

**WhoIs limitation (`tsnet` 0.1)**: the crate's vendored `libtailscale`
snapshot predates upstream `libtailscale` adding a `tailscale_whois` C
symbol at all, and even with that symbol available, `tsnet::Server` exposes
no accessor for the underlying handle a WhoIs call would need. Both gaps
are structural to the pinned dependency version, not a wiring oversight —
see [§6](#6-known-gaps) and `src/mesh/tailnet.rs`'s module doc for the full
detail, including what *is* already real, tested code today (the FFI
declaration and the pure JSON-response parser) versus what's blocked on a
future dependency bump.

## 5. Onboarding runbooks

### Adding an upstream: `mesh_onboard_upstream`

A **read-only dry-run** (`src/mesh/onboarding.rs`) — never mutates
`TERMINUS_MESH_UPSTREAMS_JSON` or anything else. Run it against a
candidate before hand-editing config:

1. Probes the candidate (`initialize` + `tools/list`, best-effort
   `GET /healthz`) via a real client built for it.
2. Checks the proposed `name`/`namespace` against the currently-configured
   registry — a taken namespace is rejected with up to three free
   alternative suggestions.
3. Confirms trust readiness: for `mtls`, that this node's embedded CA
   bootstraps and can mint the client identity the candidate will trust
   (mesh peers share one embedded-CA trust domain — no separate remote
   enrollment step); for `bearer`, that the named `secret_key` resolves
   from the process environment (the value is never read into, or printed
   by, the report).
4. Previews the namespaced catalog delta the merge step would add.
5. On success, **emits** the validated JSON entry for the operator to
   append to `TERMINUS_MESH_UPSTREAMS_JSON` themselves and reload/restart.

```json
{
  "name": "mesh_onboard_upstream",
  "arguments": {
    "name": "fleet-c",
    "url": "https://fleet-c.example.test:8443",
    "transport": "bearer",
    "namespace": "fleetc",
    "secret_key": "TERMINUS_MESH_FLEETC_TOKEN"
  }
}
```

A candidate reachable but exporting zero tools still onboards (with a
warning); an unreachable candidate fails cleanly with nothing written. See
[../deploy/personal-services.md](../deploy/personal-services.md#onboarding-this-deployment-as-a-mesh-upstream-mesh_onboard_upstream)
for the operator-facing version of this runbook (what to hand the operator
onboarding *your* deployment).

### Adding a client: `mesh_onboard_client`, least-privilege seed

The companion tool for the other direction — bringing a new remote client
onto the mesh (`src/mesh/client_onboarding.rs`), also a dry-run that emits
config rather than writing it:

1. Establishes the client's identity, one of two mechanisms:
   - `"mtls_cert"` — mints a fresh short-lived leaf cert via this node's
     embedded CA (reusing the same issuance code the `/enroll` HTTP route
     uses), CN == the requested canonical name.
   - `"tailnet"` — records a tailnet login (+ optional ACL tags) →
     canonical name mapping only; no cert issued. Valid even before that
     login has ever been seen by WhoIs — enforced on first connection.
2. Rejects a requested name already mapped to an existing principal — never
   silently re-targets an existing identity.
3. **Seeds a least-privilege allowlist grant**: a small, explicit
   read-only tool list (`dev_read_file`, `dev_list_workspaces`,
   `dev_open_workspace` — never `"*"`, never the broader allow-minus-deny
   shape reserved for the `lumina`/`harmony` scaffold). A default-allow
   seed is a hard review failure for this tool.
4. Emits a ready-to-use client connection profile (gateway MagicDNS name
   from `TERMINUS_MESH_GATEWAY_MAGICDNS_NAME` if configured, transport,
   identity) — never a CA private key; only the client's own freshly-minted
   key (mTLS mechanism), which the client legitimately must hold locally.
5. On success, emits the JSON snippets to merge into
   `TERMINUS_MESH_PRINCIPAL_MAP_JSON` and `TERMINUS_GATEWAY_ALLOWLIST_JSON`.

```json
{
  "name": "mesh_onboard_client",
  "arguments": { "name": "dev-box-claude-code", "mechanism": "mtls_cert" }
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

See [../deploy/client.md](../deploy/client.md) for the full
`terminus-client-daemon` deployment this onboards a client *for*.

## 6. Known gaps

Documented honestly, not glossed over — these are real, current
limitations, not hypothetical edge cases:

- **`mesh_pool` is not yet constructed in any binary's startup.**
  `McpServerState.mesh_pool` (`src/mcp_server.rs`) exists and every piece
  of dispatch/merge/audit logic that consults it is implemented and
  tested, but every production constructor in `src/bin/terminus_primary.rs`
  currently passes `mesh_pool: None` — there is no `UpstreamRegistry::from_env()`
  + `UpstreamPool::from_registry()` call wired into `terminus_primary`'s
  startup path yet. Practically: setting `TERMINUS_MESH_ENABLED=1` and
  `TERMINUS_MESH_UPSTREAMS_JSON` today has **no effect** on a running
  `terminus_primary` — the mesh registry parses and validates correctly if
  exercised directly (e.g. in a test or a future startup wire-up), but
  end-to-end activation from those two env vars into a live process is not
  yet connected. Closing this is a small, mechanical follow-up (call the
  same two constructors `src/mcp_server.rs`'s own tests already use, at
  startup, gated the same way `state.gateway`/`state.personal_federation`
  already are) — not a design gap, an activation gap.
- **Real tailnet WhoIs needs a `tsnet` dependency upgrade.** See
  [§4](#4-tailscale--tsnet-deployment) — the pinned `tsnet` 0.1 crate's
  vendored `libtailscale` snapshot has no `tailscale_whois` C symbol to
  link against, and no accessor for the handle a working implementation
  would need even if it did. The FFI declaration and the JSON-response
  parser are real, tested code; the call connecting them to a live tailnet
  is not, and can't be until a future item bumps the dependency (this
  dev/build sandbox has no network egress to identify or fetch a fixed
  version).
- **Mesh `CallRoute::Upstream` calls don't yet carry signed-principal
  propagation.** The personal-registry federation path
  (`crate::federation::PersonalFederationClient`) forwards the caller's
  resolved identity to Chord as the `X-Terminus-Client-Identity` header
  alongside a signed service JWT (see [federation.md](federation.md#reaching-personal-tools-federation-not-a-second-local-registry)).
  `UpstreamClient::call_tool` (`src/mesh/client.rs`) has no equivalent —
  a mesh upstream currently sees only the credential the gateway itself
  dials with (the mTLS client cert this node mints for itself, or the
  configured bearer token), never a signal identifying *which* resolved
  `Principal` on this side originated the call. This matters for a mesh
  upstream that wants to make its own downstream authorization/audit
  decisions per-caller rather than per-gateway — today it cannot, since
  every call from a given gateway looks identical regardless of which
  local principal triggered it. This gateway's own gating (allowlist,
  approval, audit — all of §3) still applies correctly *before* dispatch;
  the gap is specifically about what the upstream itself can see.

## 7. Troubleshooting

- **An upstream disappears from `tools/list`.** Its `list_tools()` call
  failed during the last `MergedCatalog::build` (check `tracing::warn!` on
  the `mesh` target for "excluding upstream"). Non-fatal by design — the
  rest of the catalog builds normally. Check that upstream's own
  reachability/health first (`GET /healthz` if it exposes one), then its
  registered `url`/`transport`/`secret_key` in
  `TERMINUS_MESH_UPSTREAMS_JSON`.
- **A `tools/call` to a namespaced name returns "mesh upstream is currently
  unavailable".** `resolve_call_route` found the namespace registered but
  not currently healthy (`CallRoute::Unavailable`) — this is deliberately
  not the same failure mode as "unknown namespace" (which falls through to
  `Local` and a plain "unknown tool" error instead). Check the same
  upstream health signal as above; this is a health-tracking state, not a
  config error.
- **A caller with a valid mTLS cert or tailnet identity gets denied
  outright.** Most likely `PrincipalResolver::resolve` returned
  `AuthError::UnmappedIdentity` — the presented cert CN (or tailnet
  login/tags) has no entry in `TERMINUS_MESH_PRINCIPAL_MAP_JSON`. Fail-closed
  by design: add the missing mapping entry rather than assuming the raw
  transport identity should pass through. If `TERMINUS_MESH_PRINCIPAL_MAP_JSON`
  is entirely unset, the resolver is unconfigured and MESH-07's legacy
  passthrough (`Principal::from(&ClientIdentity)`) applies instead — see
  [auth.md](auth.md#unified-principal-identity-mesh-06).
- **A call is denied even though the caller resolved to a `Principal` you
  expect to have access.** Check `TERMINUS_GATEWAY_ALLOWLIST_JSON` for that
  exact resolved name, and remember a `deny` prefix wins over an
  overlapping `allow` (including `allow: ["*"]`) — a `deny` entry authored
  against a bare tool name also blocks every namespaced re-export of that
  same bare name.
- **Mesh appears to do nothing at all.** Check `TERMINUS_MESH_ENABLED`
  first — unset/falsy is a deliberate no-op (empty, dormant registry,
  never an error), and as of this writing it's *always* a no-op in
  `terminus_primary` regardless of that flag — see the first item in
  [§6](#6-known-gaps).

---

Cross-reference: [federation.md](federation.md) covers the single-upstream
personal-registry federation this mesh is additive to;
[auth.md](auth.md) covers the mTLS handshake and CA in depth;
[../networking/tailscale.md](../networking/tailscale.md) covers both
host-level tailnet join and the embedded-tsnet alternative;
[../deploy/personal-services.md](../deploy/personal-services.md) and
[../deploy/client.md](../deploy/client.md) cover onboarding a
`terminus_personal` deployment or a `terminus-client-daemon` host onto the
mesh from the other side.
