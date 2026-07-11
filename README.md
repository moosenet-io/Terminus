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

## License

MIT — see [`LICENSE`](LICENSE).
