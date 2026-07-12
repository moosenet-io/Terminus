<p align="center"><img src="assets/banner.svg" alt="Terminus" width="640"></p>

<p align="center"><img src="assets/badges.svg" alt="badges"></p>

# Terminus

**The Rust MCP tool hub and federated gateway for the Lumina constellation — one
authenticated front door for agent tool egress, with ~53 tools, one per
integrated service.**

## What Terminus is

Terminus is a Model Context Protocol (MCP) tool hub written in Rust: a single
governed registry through which agents reach every external system a fleet
needs — git forges, project trackers, infrastructure, finance, calendars,
secrets, model-profiling primitives, and dozens of personal/life-admin
integrations. Rather than each agent embedding its own HTTP clients and
credentials, agents speak MCP to one surface, and Terminus dispatches each
call to a typed, sandboxed tool implementation (`RustTool`: a stable name, a
JSON Schema, a description, and an async `execute`).

Terminus is also the constellation's **gateway** — its `terminus-primary`
binary is the mTLS front door agents actually connect to. A client that
authenticates to `terminus-primary` sees an *aggregated* surface: the core
tool registry served locally, plus the personal-registry tools federated in
from a `terminus_personal` deployment, so one connection reaches both without
the caller needing to know which process owns which tool. **Terminus is the
primary entry point for the fleet; [Chord](https://github.com/moosenet-io/Chord)
is a separate process that bolts on for inference** — `terminus-primary`
forwards chat-completion/inference routes straight through to Chord over the
same federated transport, streamed chunk by chunk, so a single front door
covers both tool calls and model inference without the caller juggling two
endpoints. See [`docs/architecture/chord-integration.md`](docs/architecture/chord-integration.md)
for the full boundary and wire contract.

Every tool implements the same small trait, uses typed HTTP clients
(`reqwest`) and parameterized SQL (`sqlx`) for all external I/O — never
shell-outs — and registers into a central `ToolRegistry` that handles
dispatch, duplicate detection, and catalog listing.

## Architecture

<img src="assets/architecture.svg" alt="Terminus architecture" width="100%">

MCP clients connect over stdio or HTTP/mTLS transports to the Terminus core
MCP server, which owns dispatch, JSON-Schema validation, and governance.
Governance is mandatory and layered: a path-jailed filesystem, vault-only
secret access (no raw environment reads for secrets), a PII gate, and a
sanitized audit log — tools are read-only by default, write scopes are
explicit. Behind the registry sit the 52 domain tool modules, each owning its
own typed client and credentials. See
[`docs/architecture/`](docs/architecture/) for the federation, auth, and
Chord-integration deep-dives.

## At a glance

| | |
| --- | --- |
| **Tools** | ~53, one per integrated service (GitHub, Plane, Prometheus, …). Each tool exposes a set of **actions** that vary with the backing service and change over time — ~306 individual MCP callables in total across all tools. |
| **Transport** | stdio (local/SSH) and HTTP, with an mTLS listener for federated/remote clients |
| **Auth** | per-identity mTLS client certificates (`crate::pki`); named-identity tokens (`GITEA_PAT_<NAME>`, `PLANE_PAT_<NAME>`) for outbound git-forge/tracker calls |
| **Governance** | path-jailed filesystem access, vault-only secrets (never a raw `env::var` for a credential), a mandatory Rust PII gate on every public-forge write, sanitized audit logging |
| **Flagship harness** | **MINT** — the model-intake/serving-profile CLI and tool suite. One `MintHarness` orchestrator drives both sweep families (`RunKind::Coder` for the code sweep, `RunKind::Assistant` for the Lumina seven-dimension sweep) through one lifecycle over the shared `lumina_intake` DB; the two standalone sweep binaries are thin `MintHarness::run(RunKind::…)` entrypoints. See [`docs/tools/README.md`](docs/tools/README.md#mint-flagship) |
| **Inference** | proxied to the separate [Chord](https://github.com/moosenet-io/Chord) process — Terminus does tool egress, Chord does inference egress |

## Mesh: federating multiple upstream Terminus servers

Beyond the single personal-registry upstream `terminus-primary` federates by
default, Terminus can federate an arbitrary set of upstream Terminus-shaped
MCP servers through a config-driven **mesh registry** (`crate::mesh`). Rather
than a hard-coded client per backend, each upstream is declared as data and
validated at startup.

Configuration is entirely non-secret and environment-driven (structural
config only — credentials are never inlined):

| Variable | Meaning |
| --- | --- |
| `TERMINUS_MESH_ENABLED` | Master switch. Truthy (`1`/`true`/`yes`/`on`, case-insensitive) enables the mesh; anything else (including unset) leaves it dormant — an empty registry, never an error. |
| `TERMINUS_MESH_UPSTREAMS_JSON` | A JSON array of upstream entries (see below). Unset/blank while enabled is a dormant no-op; malformed while enabled is a clear startup error naming the offending field. |

Each entry in the JSON array declares:

| Field | Meaning |
| --- | --- |
| `name` | Stable, unique identifier for the upstream. |
| `url` | Reachable base URL (must be non-empty). |
| `transport` | `"mtls"` or `"bearer"` (case-insensitive). |
| `namespace` | Unique prefix its federated tools are namespaced under; must match `^[a-z0-9]{2,16}$`. |
| `secret_key` | **NAME only** of the credential in the runtime secret store (for `bearer`); omit for pure-mTLS upstreams. Never an inline token value. |
| `enabled` | Optional bool, default `true`. A `false` entry is parsed/validated but excluded from dialing. |

```json
[
  { "name": "personal", "url": "https://personal.example.internal:8443",
    "transport": "mtls", "namespace": "personal" },
  { "name": "fleet-b", "url": "https://fleet-b.example.internal:8443",
    "transport": "bearer", "namespace": "fleetb",
    "secret_key": "TERMINUS_MESH_FLEETB_TOKEN", "enabled": false }
]
```

Credentials are referenced by secret-key **name** only and resolved lazily,
right before a dial — never at registry-load time, and never stored as a value
on the registry — following the same "materialized into the process
environment at startup, plain env read afterward IS the secret read"
convention the rest of the crate uses (see `crate::pki`). Registry loading,
validation, and inspection perform zero secret-store reads.

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

## Quickstart

```sh
git clone <your-terminus-repo-url>
cd terminus-rs
cargo build --release
```

Terminus ships three binaries:

- **`terminus_personal`** — the personal/admin registry (ledger, vitals,
  crucible, relay, meridian, odyssey, and other life-admin + git/tracker
  tools), served over a plain listener plus an optional mTLS listener.
- **`terminus_primary`** — the gateway binary: serves the **core** registry
  locally and federates in the personal registry's tools from a
  `terminus_personal` deployment, over the same mTLS/`enroll` front door, plus
  forwards inference routes to Chord.
- **`pii_gate`** — the standalone PII/secret-scanning binary used as a git
  pre-push/pre-commit hook and by the public-mirror engine.

Configuration is entirely environment-driven — every credential is expected
to already be materialized into the process environment by your own secret
manager at startup (Terminus never reads a raw literal token from config).
For a full walkthrough of standing up a client against a running Terminus —
enrollment, mTLS certs, and the personal-services deployment shape — see the
deployment guides:

- [`docs/deploy/client.md`](docs/deploy/client.md) — connecting a new MCP
  client (enrollment, certs, transport selection).
- [`docs/deploy/personal-services.md`](docs/deploy/personal-services.md) —
  standing up `terminus_personal` / `terminus_primary`.

## Documentation

This README is the front door; everything past "at a glance" lives in
[`docs/`](docs/README.md), organized by area:

| Area | What's there |
| --- | --- |
| [`docs/README.md`](docs/README.md) | The documentation site index — start here for the full table of contents. |
| [`docs/architecture/`](docs/architecture/) | Federation (how `terminus-primary` aggregates core + personal tools), the [mesh](docs/architecture/mesh.md) (N-upstream federation, identity/RBAC, tailnet exposure, onboarding, known gaps), auth (mTLS identity model), and the Chord-integration boundary/wire contract. |
| [`docs/networking/`](docs/networking/) | WireGuard and Tailscale transport options for reaching a Terminus deployment off-LAN, including the optional embedded-tsnet mode (MESH-04, `tsnet` Cargo feature — no host `tailscaled` required; see [`docs/networking/tailscale.md`](docs/networking/tailscale.md#alternative-embedded-tsnet-mesh-04--no-host-tailscaled-at-all)). |
| [`docs/deploy/`](docs/deploy/) | Client enrollment/deploy guide and the personal-services (`terminus_personal`/`terminus_primary`) deployment guide. |
| [`docs/tools/`](docs/tools/README.md) | The full tool index — all 53 modules grouped by domain, plus the **MINT** flagship harness. |

## Atlas — knowledge-graph query tools

Atlas builds a per-project knowledge graph from **any of ~14 languages** (Rust, Python, JavaScript/TypeScript, Go, Java, C, C++, Ruby, Lua, C#, PHP, Bash) via tree-sitter, not just Rust (KGRAPH-17). Atlas (the knowledge-graph subsystem of the Scribe documentation engine, spec
`S112-knowledge-graph-docs`) builds a per-project graph of a codebase — nodes
are code entities (functions/structs/…), edges are calls/imports/references
tagged with a confidence tier — and exposes it to local models as `kg_*` tools
on the core registry, so a model can query the graph instead of grepping source:

| Tool | What it answers |
| --- | --- |
| `kg_search` | Find entities by name or id substring. |
| `kg_neighbors` | What a node calls/imports/references, and what references it. |
| `kg_subgraph` | The local neighborhood (blast radius) around a symbol, to a depth. |
| `kg_path` | The shortest path connecting two entities. |
| `kg_stats` | Node/edge counts, clusters, top-degree hotspots, orphans. |
| `kg_communities` | The community structure (level-0 clusters + a coarser level-1), each with members and — when a model is available — a short summary, for answering subsystem/architecture questions at the right zoom. |
| `kg_query` | Answer a natural-language question — routes automatically to entity-level retrieval (specific symbols) or community-level retrieval (architecture/subsystems), returns the context plus a synthesized answer when a model is available. |
| `kg_file_symbols` | The symbols a given repo-relative file defines, sorted by PageRank importance. |
| `kg_semantic_search` | Meaning-based (embedding) search — finds nodes related to `query` even without a shared substring. Degrades to `configured:false` when embeddings aren't set up; see [KGEMB-04](#kg-semantic-search-tool-kgemb-04) below. |
| `kg_findings` | Lists captured analysis findings (lint-like observations, review notes, anomalies) for a project, ordered by recurrence, with optional `scope`/`category`/`min_occurrences` filters. Degrades to `configured:false` when the findings store isn't set up; see [KGFIND-04](#kg-findings-tool-kgfind-04) below. |

All take a `project_id` and read the per-project graph store
(`SCRIBE_KG_STORE_DIR`); a project with no graph yet returns `found: false`
rather than an error. Graphs are produced/refreshed by the build pipeline's
docs stage (`scribe_kg_build`).

A graph is produced end-to-end by **`scribe_kg_build`** (`project_id`,
`repo_path` under `SCRIBE_ALLOWED_REPO_ROOTS`; `incremental` + `changed_files`
to patch only those files) — it walks the repo, extracts → clusters → lays out
→ renders, stores the graph JSON, and writes the visual artifacts.
**`scribe_kg_status`** reports a project's counts, freshness, and which
artifacts exist. When `scribe_generate_readme` is given a `project_id` whose
graph has been built, it appends the rendered map (`map.svg` + confidence
legend) to the generated README as an **"## Architecture map"** section — so the
graph informs the doc's visual output; projects without a graph are unchanged.

A graph also renders to three visual artifacts (all from one shared
force-directed layout, so they agree): a static **`map.svg`** — nodes colored by
cluster, sized by degree, edges styled by confidence (solid EXTRACTED / dashed
INFERRED / dotted AMBIGUOUS) with a legend — which Scribe embeds directly in the
README/wiki/vault; a **`graph.graphml`** interchange file for Gephi/yEd/
Cytoscape; and a self-contained interactive **`graph.html`** (inline SVG with
vanilla-JS pan/zoom/search, no external hosts).

### `review_run` is KG-grounded (KGREV-01)

`review_run` best-effort grounds every dispatched review in the project's
Atlas graph: before building each provider's prompt, it looks for two optional
keys on `context`:

| Context key | Type | Purpose |
| --- | --- | --- |
| `project_id` | string | Which project's stored Atlas graph (`SCRIBE_KG_STORE_DIR`) to consult. Omit this and nothing below happens — the review is byte-for-byte identical to a build with no Atlas awareness at all. |
| `changed_files` | array of repo-relative path strings | The files under review. If omitted, they're parsed from `context.diff`'s unified-diff `+++ b/<path>` headers instead. |

When `project_id` resolves to a graph with at least one node defined in a
changed file, `review_run` injects a bounded `knowledge_graph` block into
`context` — the touched symbols (id/name/kind/cluster) plus up to a few 1-hop
callers and callees each (≤ 40 symbols total, ≤ ~2 KB serialized; a
`"truncated": true` marker appears if the cap was hit) — and every provider's
prompt gets a one-line pointer to it ("... weigh cross-module impact").
Grounding is entirely best-effort: no `project_id`, no stored graph, or no
node matching any changed file all silently skip injection — never an error,
never a partial/empty block.

### `review_run` rebuilds the graph on pass + holds a per-project lock (KGREV-02)

When a dispatched review's aggregate verdict is `APPROVE` and `complete`, and
`context` carries both `project_id` and `repo_path` (an absolute path under
`SCRIBE_ALLOWED_REPO_ROOTS`), `review_run` incrementally rebuilds that
project's Atlas graph via `scribe_kg_build` (`incremental: true`,
`changed_files` reusing the same derivation KGREV-01 uses) — so the graph the
*next* review consults reflects the change that was just approved.

While that rebuild is in flight, `review_run` holds a per-project lock keyed
by `project_id`. Another call with the SAME `project_id` short-circuits
immediately at the top of `execute()`:

```json
{ "structure": "...", "providers": [], "aggregate_verdict": "UNKNOWN",
  "complete": false, "locked": true,
  "reason": "KG rebuild in progress for <project>; retry when ready" }
```

No providers are dispatched on a locked call. Reviews of *different*
`project_id`s never block each other. The lock is released via an RAII guard
on every path — rebuild success, rebuild error, or a panic-unwind — so it can
never deadlock a project.

The rebuild is entirely non-blocking to the review result: a rebuild failure
(bad `repo_path`, disallowed root, etc.) is logged and reported in a
`kg_rebuild` field, and never turns an `APPROVE` into a tool error or changes
the aggregate verdict. Every `review_run` result now includes `kg_rebuild`:

| Shape | Meaning |
| --- | --- |
| `{"ran": false, "reason": "..."}` | Not an approved+complete pass, or `project_id`/`repo_path` missing — no lock taken, backward compatible. |
| `{"ran": true, "ok": true, "nodes": …, "edges": …, "clusters": …, "mode": "incremental"}` | Rebuild succeeded. |
| `{"ran": true, "ok": false, "error": "..."}` | Rebuild failed; review verdict is unaffected. |

### `review_run` refreshes docs through the SCRIBE door on pass (KGREV-03)

When a dispatched review's aggregate verdict is `APPROVE` and `complete`, and
`context` also carries both `project` and `spec_id`, `review_run` drives a doc
refresh through the ONE sanctioned doc-generation door — the existing
`docgen_run` tool (`crate::tools::docgen::trigger::DocgenRun`), called
in-process. This runs **after** the KGREV-02 rebuild above, so the doc engine
sees the just-refreshed Atlas graph.

| Context key | Type | Purpose |
| --- | --- | --- |
| `project` | string | Passed through to `docgen_run` as `project`. Required (with `spec_id`) to trigger a doc refresh at all. |
| `spec_id` | string | Passed through to `docgen_run` as `spec_id`. Required (with `project`). |
| `git_ref` | string, optional | Passed through to `docgen_run` as `git_ref`. Defaults to `"unknown"` if omitted. |
| `module_path` | string, optional | Passed through to `docgen_run` as `module_path`. Defaults to `"."` if omitted. |
| `project_config` | object, optional | Passed through to `docgen_run` as `project_config` (the project's doc-target config). Omitting it means `docgen_run`'s own opt-in gate skips cleanly — no doc-target config declared. |
| `diff` | string, optional | Passed through to `docgen_run` as the unswept `feat_context` (`docgen_run` runs its own PII sweep before anything else touches it). |

If `project`/`spec_id` are absent, this is a no-op — most reviews won't supply
doc params; the wire only fires for real merge-time reviews that do. The doc
refresh is entirely non-blocking to the review result: `docgen_run` is
already structurally non-blocking (an internal doc-gen failure surfaces as
`outcome: "failed"`, never a tool error), and any unexpected error calling it
is caught, logged, and reported rather than propagated — it never turns an
`APPROVE` into a tool error or changes the aggregate verdict. Every
`review_run` result now includes `scribe_docs`:

| Shape | Meaning |
| --- | --- |
| `{"ran": false, "reason": "not an approved pass"}` | Not an approved+complete pass. |
| `{"ran": false, "reason": "no doc params"}` | `project`/`spec_id` missing — no `docgen_run` call. |
| `{"ran": true, "outcome": "skipped"\|"completed"\|"failed", "docgen": {...}}` | `docgen_run` was called; `docgen` carries its full structured result. |
| `{"ran": true, "ok": false, "error": "..."}` | Calling `docgen_run` itself errored unexpectedly; review verdict is unaffected. |

No direct doc-generation HTTP/Chord call is made from `review_run` — the only
doc path is the existing `docgen_run` tool (S9 single door).

### Atlas vector store (KGEMB-01)

Phase 1 of KG-as-behavioral-correction adds semantic (meaning-based) retrieval
alongside the lexical `kg_search` above. `AtlasVecStore`
(`src/scribe/graph/vec_store.rs`) owns a dedicated Postgres table,
`kg_embeddings`, holding one 768-dim [pgvector](https://github.com/pgvector/pgvector)
embedding per `(project_id, node_id)`, plus the `card_hash` of the text that
was embedded (so a rebuild can skip re-embedding unchanged nodes) and an HNSW
cosine-similarity index for fast top-K search.

- **`ATLAS_DATABASE_URL`** — the dedicated Postgres DSN for the embeddings
  store. This is the ONLY source for the store's DSN — there is deliberately no
  fallback to a shared `DATABASE_URL`, so the store stays isolated to its own
  database. When `ATLAS_DATABASE_URL` is unset, `AtlasVecStore::from_env()`
  returns `NotConfigured` cleanly — no connection is attempted, and callers (the
  build-time embed step and the `kg_semantic_search` tool) degrade to the
  existing lexical path rather than failing.
- The migration (`CREATE EXTENSION IF NOT EXISTS vector`, the table, and its
  `hnsw (embedding vector_cosine_ops)` index) is idempotent and
  advisory-lock-serialized, safe to run on every `from_env()` call including
  from concurrent processes. HNSW index creation is best-effort: if a given
  pgvector build rejects it, the table still works (exact top-K scan via
  `<=>`), just without the ANN speedup.
- Typed methods: `upsert` (batched, parameterized, `ON CONFLICT` update),
  `delete` (by `node_id` list), `existing_hashes` (for incremental
  hash-diff skip), and `query_topk` (cosine similarity, descending).
- This module lands only the store. The embeddings client, the gated
  build-time wiring, and the `kg_semantic_search` tool are later items in
  spec `S113-kg-semantic-embeddings` (KGEMB-02/03/04).

### KG embeddings client (KGEMB-02)

`EmbedClient` (`src/scribe/graph/vec_embed.rs`) turns text into a vector
against a configurable endpoint, provider-agnostic between the local Ollama
shape and hosted OpenAI-style APIs, auto-detected from the URL:

- Ollama (`/api/embeddings`, `{"model","prompt"}` → `{"embedding":[...]}`) —
  the default, matching the CPU-tier ollama unit already used elsewhere.
- OpenAI-style (any URL containing `/v1/embeddings`, `{"model","input"}` →
  `{"data":[{"embedding":[...]}]}`) — for hosted providers (e.g. an
  OpenRouter-compatible embeddings endpoint), with bearer auth.

Config (non-secret, via `crate::config`):

- **`EMBEDDINGS_URL`** — the embeddings endpoint. Defaults to the secondary
  (CPU) ollama unit's `OLLAMA_CPU_URL` + `/api/embeddings`; with neither set,
  falls back to a loopback CPU-ollama default (never a real non-loopback host
  baked in).
- **`EMBEDDINGS_MODEL`** — the model name sent on each request. Defaults to
  `nomic-embed-text`.
- **`EMBEDDINGS_TIMEOUT_MS`** — per-request timeout. Defaults to 30000 (30s).

**`EMBEDDINGS_API_KEY`** (optional, for hosted providers) is secret material
and is read directly from the env-materialized runtime secret store inside
`vec_embed` itself, not from `crate::config` — this crate has no separate
`SecretManager`/`vault` API of its own (same convention as `crate::pki`'s CA
material and `review::dispatch`'s `OPENROUTER_API_KEY`: the deployment's
secret store materializes into env at startup, so a plain env read afterward
already IS the SecretManager read). When unset, no `Authorization` header is
sent (Ollama needs none).

`EmbedClient::embed`/`embed_batch` never panic: transport, HTTP-status, and
parse failures all become a `ToolError` for the caller to log and skip — a
best-effort contract, since KGEMB-03's build-time wiring must never block on
an embeddings outage.

`node_card(node, callers, callees)` builds the deterministic short text that
gets embedded for a `KgNode`: `"{kind} {name} in {path}"`, plus (if any
neighbors) `" — calls: ...; called by: ..."`, each neighbor list capped at 6
names and the whole card capped at 512 characters (truncated on a char
boundary).

This item ships only the client + card builder — it is not yet wired into
`scribe_kg_build` (that's KGEMB-03).

### `kg_semantic_search` tool (KGEMB-04)

`kg_semantic_search(project_id, query, limit?)` (`src/scribe/graph/tools.rs`)
is the query-side counterpart to KGEMB-01/02/03: it embeds `query` with
`EmbedClient`, asks `AtlasVecStore::query_topk` for the nearest node ids by
cosine similarity, joins the hits against the project's currently-loaded
Atlas graph, and returns `{id,name,kind,path,score,cluster}` per hit ordered
by similarity (descending — the store's own order is preserved, never
re-sorted). `limit` is optional (default 10) and clamped to `[1, 50]`.

**Degrade-to-lexical contract:** this tool is safe to call unconditionally,
including in a deployment that has never enabled embeddings:

| Condition | Result |
| --- | --- |
| `AtlasVecStore::from_env()` returns `NotConfigured` (`ATLAS_DATABASE_URL` unset) | `{"configured": false, "found": false, "results": []}` — a normal result, not a tool error. Callers should fall back to `kg_search`. |
| The store is configured but some other error occurs (e.g. connect failure) | Also degrades to `{"configured": false, "found": false, "results": [], "error": "..."}` rather than a hard error. |
| The embeddings endpoint is down/unreachable at query time | `{"configured": true, "found": false, "results": [], "error": "..."}` — the store IS configured, but the query embedding itself failed. |
| No knowledge graph exists for `project_id` yet | `{"configured": true, "found": false, "count": 0, "message": "..."}` — a genuine empty result, not a config problem (run `scribe_kg_build` first). |
| Both are up, query ran | `{"configured": true, "found": <has-results>, "project_id", "count", "results": [...]}` — `found` reflects whether there were actual matches (zero hits, or every hit dropped as a stale row, is `found:false`). |

A vector-store row whose `node_id` is no longer present in the currently
loaded graph (e.g. the graph was rebuilt and the symbol was removed/renamed)
is silently dropped from the results rather than surfaced — stale-row
tolerance, so a query never returns a dangling reference.

### `kg_findings` tool (KGFIND-04)

`kg_findings(project_id, scope?, category?, min_occurrences?, limit?)`
(`src/scribe/graph/tools.rs`) is the read-only query counterpart to the
KGFIND-01 `FindingsStore`: it lists a project's captured findings ordered by
recurrence (`occurrences DESC, last_seen DESC`), so the corpus is inspectable
independent of the write path. `scope` filters to one of
`node`/`path`/`community`/`global`; `category` and `min_occurrences` narrow
further; `limit` is optional (default 50) and clamped to `[1, 200]`.

**Degrade contract**, mirroring `kg_semantic_search`:

| Condition | Result |
| --- | --- |
| `FindingsStore::from_env()` returns `NotConfigured` (`ATLAS_DATABASE_URL` unset) | `{"configured": false, "found": false, "results": []}` — a normal result, not a tool error. |
| The store is configured but some other error occurs (e.g. connect failure) | Also degrades to `{"configured": false, "found": false, "results": [], "error": "..."}` rather than a hard error. |
| Store configured, query ran, no matching rows | `{"configured": true, "found": false, "project_id", "count": 0, "results": []}` — a genuine empty result, not a config problem. |
| Store configured, matches found | `{"configured": true, "found": true, "project_id", "count", "results": [{id, category, severity, scope_kind, scope_ref, description, occurrences, first_seen, last_seen}, ...]}` ordered by recurrence. |

## Cortex — code-elegance / risk gate (Atlas-backed, S115/CXEG)

Cortex is the pipeline's code-elegance, consistency, and risk gate. It was
originally a thin SSH-exec relay to a script on an external fleet host; that
host is retired and the relay with it. As of **CXEG-01** the module is
re-scaffolded in-process, keyed by `project_id` (`TERM`/`LUM`/`HARM`/`CHRD`/
`RAIL`), and built on the live Atlas knowledge graph rather than a subprocess.
Its risk/elegance surface is rebuilt over the following S115 items:

- `cortex_scope` — pre-change blast radius for a planned change, live as of
  **CXEG-02**: given `project_id` + `changed_files` (comma-separated string
  or array) or a unified `diff`, it resolves the touched symbols against the
  project's Atlas graph and walks their 1-hop callers/callees via the shared
  `scribe::graph::query::one_hop_neighbors` helper (the same single-source walk
  `kg_neighbors` uses), filtered to the current bi-temporal view so a
  since-removed symbol never appears. Returns a JSON object with fields
  `configured` (bool), `project_id`, `changed_files`, `blast_radius[]` (each
  entry `{id, path, kind, resolved, role}` where `role` is
  `touched`/`caller`/`callee`), `affected_communities` (sorted cluster ids),
  `blast_count`, `token_reduction_pct` (how much smaller the blast radius is
  than the whole project), and `truncated` (present only when a cap fired).
  Degrades to `configured:false` (the literal `changed_files` echoed back as
  unresolved entries) instead of erroring when the project has no stored Atlas
  graph yet — dispatch never breaks on a missing graph. Sets `truncated:true`
  (with a distinct logged warning on the live AND degrade paths, never a
  silent drop) for either the input-file cap (`MAX_CHANGED_FILES`) or the
  blast-node cap (`CORTEX_MAX_BLAST_NODES`, default 200). An oversized-*by-file
  -count* list/diff truncates (with `truncated:true`) rather than erroring;
  `InvalidArgument` is reserved for genuinely abusive/malformed input (a single
  path over `MAX_TEXT_LEN`, a DoS-scale `diff`/string over `MAX_DIFF_LEN`, or an
  array over `MAX_CHANGED_FILES_ARG` — ceilings set far above the file-count
  cap so real diffs truncate, not reject).
- `cortex_review` — post-change `risk_score` (0–10) + named `risk_signals`
  from Atlas structural metrics and KGFIND recurrence (stub pending **CXEG-04**).
- `cortex_audit` — audit an external public repo URL (stub pending **CXEG-11**);
  its SSRF-hardened `validate_repo_url` front-gate (`src/cortex/audit.rs`) is
  live now — it rejects non-http(s) schemes, embedded credentials, shell
  metacharacters, and loopback/private/link-local/metadata hosts in their
  common obfuscated encodings (fail-closed).

The seven retired graph-relay tools are kept only as zero-I/O **deprecation
aliases** (`src/cortex/deprecated.rs`) that return a structured
`{"deprecated": true, "use": "kg_..."}` pointer to their live Atlas
equivalents: `cortex_stats`→`kg_stats`, `cortex_build`→`scribe_kg_build`,
`cortex_deps`→`kg_neighbors`, `cortex_recent`→`kg_query`,
`cortex_community`/`cortex_architecture`→`kg_communities`,
`cortex_flows`→`kg_path`.

## Postgres tool suite — the single sanctioned Postgres door (S115)

Coder agents historically SSHed directly into DB hosts and ran `psql` for
schema/data/role changes: unaudited, ungoverned, host-level DB access. The
`pg_*` tools (`src/pg/`) are the ONE sanctioned, audited, identity-scoped
door for all agent/client/tool Postgres access — no more direct SSH+`psql`.
This is the same S9 single-door posture Terminus already applies to
GitHub/Gitea/Plane, applied to Postgres.

**Status:** PGT-01 shipped the connection/identity foundation and the
read-only `pg_identities` tool. PGT-02 adds the read surface (`pg_query` /
`pg_list_tables` / `pg_describe_table`); PGT-04 adds `pg_ddl` (schema DDL);
PGT-03 adds `pg_execute` (DML); PGT-05 adds `pg_admin` (roles/GRANT/REVOKE).
PGT-06 wires all three mutating tools into the gateway's per-occurrence
approval gate (see "Governance" below) — the suite is now fully guarded.

### Read tools (PGT-02)

All three default to the least-privileged `readonly` connection identity and
are **not** guarded (read-only, no destructive capability) — same audit
posture as every other tool call.

- **`pg_query`** — runs exactly ONE read-only statement: `SELECT`,
  `WITH ... SELECT` (a CTE), `EXPLAIN`, or `SHOW`. Args:
  `{ sql, params?, identity?, max_rows? }`. `sql` must contain a single
  statement — no `;`-chained multi-statement input — and no DML/DDL
  keyword anywhere in the body (this also rejects a CTE that smuggles an
  `INSERT`/`UPDATE`/`DELETE`/`DROP`/etc. inside a `WITH` clause). Any
  violation is a clean `InvalidArgument` pointing at `pg_execute`/`pg_ddl`
  instead. Values are passed as bound `$1, $2, ...` `params` and are
  **always** bound via `sqlx`'s typed `Encode`, never string-interpolated
  into the SQL text — SQL-injection safe by construction. Results are
  row-capped (`max_rows`, default 500, hard ceiling 5000) and the response
  reports `{ columns, rows, row_count, truncated }`.
- **`pg_list_tables`** — lists tables visible to the connection (via
  `information_schema.tables`), optionally restricted to one `schema`. Args:
  `{ schema?, identity? }`.
- **`pg_describe_table`** — describes one table's columns
  (name/type/nullable/default), primary key, and indexes. Args:
  `{ table, schema? (default "public"), identity? }`. A non-existent table
  is a clean `NotFound`, not a panic.

`pg_list_tables`/`pg_describe_table` validate `schema`/`table` against a
conservative Postgres-identifier charset (`[A-Za-z_][A-Za-z0-9_]*`, max 63
bytes) before splicing them into the introspection query (identifiers cannot
be bound as ordinary query parameters); a name that fails it is a clean
`InvalidArgument`.

### `pg_ddl` — schema DDL (PGT-04)

Runs a single schema-DDL statement: `CREATE`/`ALTER`/`DROP` on `TABLE` /
`INDEX` / `VIEW` (including `MATERIALIZED VIEW`) / `SEQUENCE` / `SCHEMA`.
Args: `{ sql, identity? }`. Default identity: **`admin`** (the DB role is the
real privilege boundary, matching every other `pg_*` tool's identity model).

A pure string-level statement-class gate (`src/pg/ddl.rs::classify_ddl`, unit
tested without a DB connection) runs before any connection is attempted:

- Accepts only a single statement (one optional trailing `;`; any other `;`
  is rejected as multi-statement input).
- Accepts only `CREATE`/`ALTER`/`DROP` as the leading keyword — DML
  (`INSERT`/`UPDATE`/`DELETE`) and reads (`SELECT`/`EXPLAIN`/`SHOW`) are
  rejected with a clean `InvalidArgument` pointing at `pg_execute`/`pg_query`.
- Rejects role/privilege management (`CREATE`/`ALTER`/`DROP ROLE`/`USER`/
  `GROUP`, `GRANT`, `REVOKE`) even though some share a leading keyword with
  schema DDL — those belong to `pg_admin` (PGT-05).
- Rejects a DDL statement whose target object isn't one of
  `TABLE`/`INDEX`/`VIEW`/`SEQUENCE`/`SCHEMA` (e.g. `CREATE EXTENSION`).

`DROP` statements, and `ALTER` statements that themselves contain a `DROP`
(dropping a column/constraint/default), are flagged `irreversible: true` in
both the response summary and structured payload, so an approval prompt or
audit reviewer can immediately see the blast radius. Returns
`{ statement_class, object, irreversible, identity, ok }`.

`pg_ddl` is destructive by design and is **GUARDED** (PGT-06): it is in
`crate::approval::GUARDED_BARE_NAMES` and calls `crate::approval::gate(...)`
itself at the top of `execute_structured`, after the statement-class gate and
before any DB connection is attempted — every call requires per-occurrence
operator approval. See the note at the bottom of `src/pg/ddl.rs`.

### `pg_execute` — parameterized DML (PGT-03)

`pg_execute` runs exactly one bound-parameter `INSERT`/`UPDATE`/`DELETE`
(optionally with `RETURNING`) against a connection identity — args
`{ sql, params?, identity? }`. Anything that isn't a single DML statement is
a clean `InvalidArgument` pointing at the right tool: a read (`SELECT`/
`WITH`/`EXPLAIN`/`SHOW`) → `pg_query`; DDL (`CREATE`/`ALTER`/`DROP`/
`TRUNCATE`/...) → `pg_ddl`; role/privilege statements (`GRANT`/`REVOKE`) →
`pg_admin`; multi-statement input (an embedded `;`) is rejected outright.
Values are always bound `params` (`$1`, `$2`, ...), never interpolated into
`sql`.

`pg_execute` defaults to the `writer` connection identity (not the
suite-wide `readonly` default — DML needs a writer-tier DB role), and
returns `{ affected, returning?, destructive, statement_class, identity }`.

**Destructive-shape detection.** A `DELETE`/`UPDATE` with no `WHERE` clause
mutates or removes an entire table's rows in one call — the response's
`destructive: true` flag surfaces that shape (pure string/token check, no
SQL parser) so the audit trail and any guarding logic can see it without
re-parsing the SQL. The same detector (`crate::pg::execute::is_destructive_shape`,
`pub` for reuse) also recognizes a bare `TRUNCATE`, even though
`pg_execute`'s own statement-class gate rejects `TRUNCATE` outright as
DDL-shaped (pointing the caller at `pg_ddl`) — the detector exists as one
shared, reusable classifier for later `pg_*` items, not only for what
`pg_execute` itself accepts.

`pg_execute` is a mutating tool and is **GUARDED** (PGT-06): it is in
`crate::approval::GUARDED_BARE_NAMES` and calls `crate::approval::gate(...)`
itself at the top of `execute_structured`, after the statement-class and
destructive-shape checks and before any DB connection is attempted — every
call requires per-occurrence operator approval, on top of the DB-role
privilege boundary and the standard gateway audit trail.

### `pg_admin` — role/privilege management (PGT-05, guarded)

`pg_admin` runs exactly one role/privilege statement — `CREATE`/`ALTER`/`DROP ROLE`|`USER`,
`GRANT`, or `REVOKE` — via either a structured `{ action, role, options, password, privileges,
on, to, from }` form (preferred, so a password never has to be hand-formatted into a loggable
`sql` string) or a raw single-statement `sql` string. Anything else (DDL/DML/reads/multi-statement)
is a clean `InvalidArgument` pointing at `pg_ddl`/`pg_execute`/`pg_query`. Default identity:
**`admin`**. Guarded — it calls the approval gate at the top of its execute.

**Password redaction (mandatory).** Any `PASSWORD '...'` literal is rewritten to
`PASSWORD '***REDACTED***'` before anything reaches the approval-gate summary, the audit args, or
the tool response — the real password only ever lives in the local string used to run the
statement. `DROP ROLE`/`REVOKE` are flagged `high_impact`.

### Identity / connection model

Every `pg_*` tool accepts an optional `identity` argument selecting which
Postgres connection/DB-ROLE the call authenticates as — exactly mirroring how
every Plane tool accepts an optional `identity` argument for `PLANE_PAT_<NAME>`
(see "Unified `Principal` identity" above). A connection identity `<name>` is
configured by setting a `POSTGRES_URL_<NAME>` secret (e.g.
`POSTGRES_URL_READONLY`, `POSTGRES_URL_WRITER`, `POSTGRES_URL_ADMIN`) to a
connection string authenticated as a DB ROLE scoped to that privilege level —
the DB role, not the tool code, is the real privilege boundary. Omitting
`identity` uses the least-privileged `readonly` — safe by default, even for a
call that reaches a tool it shouldn't have.

`pg_identities` lists the configured connection NAMES and a name-derived
privilege tier (`readonly`/`writer`/`admin`/`unknown`) — never a secret
value. Read-only, not guarded.

### Secret access

terminus-rs has no separate `SecretManager::get()` / `vault::manager()` API
of its own (see the `crate::pki` module docs for the full rationale): the
runtime secret store is materialized into this process's environment at
startup by the operator's secret manager, so a plain env read afterward
already IS the "vault" read in this crate's established convention — the
same convention `PLANE_PAT_<NAME>` uses. `src/pg/conn.rs`'s
`scan_named_connections` is the ONE place `POSTGRES_URL_<NAME>` is read; no
URL value is ever logged, displayed in an error, or embedded in a tool
result — only identity NAMES and tiers are ever surfaced. An identity with no
configured secret is refused with a clean "not configured" error naming the
role, never guessing a fallback connection.

### Governance and the exemption boundary

Full governance runbook (single-door rule, identity/role model, exemption boundary, operator provisioning): [`docs/tools/postgres-suite.md`](docs/tools/postgres-suite.md).


This suite is the single door for AGENT/admin/ad-hoc Postgres access. It does
**not** replace the application's own governed `sqlx` data paths — the MINT
sweep (`crate::intake::storage::get_pool`), the fleet-catalog/discovery
read+write tools, and any other in-process data path keep their direct
`PgPool`, unrouted through this suite and undisturbed by it.

The three mutating `pg_*` tools — `pg_execute`, `pg_ddl`, `pg_admin` — are
**guarded** (PGT-06): each is registered in
`crate::approval::GUARDED_BARE_NAMES` (so a federated/mesh call is gated at
the gateway before it can be laundered through a remote upstream) AND each
calls `crate::approval::gate(...)` itself at the top of its
`execute`/`execute_structured`, after statement-class validation (and, for
`pg_admin`, after password redaction — see `src/pg/admin.rs`'s S6 note) and
before any DB connection is attempted — no mutating call reaches Postgres
without per-occurrence operator approval via the `tool_approvals` gate. This
is on top of, not instead of, the DB-role privilege boundary and the
standard gateway audit trail every tool call already gets. The four
read-only tools — `pg_query`, `pg_list_tables`, `pg_describe_table`,
`pg_identities` — are deliberately **not** guarded. Every future mutating
`pg_*` tool added to this suite MUST be evaluated for the guarded set.

`pg` registers on the CORE tool registry only (`crate::registry::register_all`,
alongside `crate::intake::register`) — Chord-served, never the
`terminus_personal`/<host> personal registry.

## License

MIT — see [`LICENSE`](LICENSE).
