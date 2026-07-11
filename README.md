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
| **Tools** | ~53, one per integrated service (GitHub, Plane, Prometheus, …). Each tool exposes a set of **actions** that vary with the backing service and change over time — ~300 individual MCP callables in total across all tools. |
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
| [`docs/architecture/`](docs/architecture/) | Federation (how `terminus-primary` aggregates core + personal tools), auth (mTLS identity model), and the Chord-integration boundary/wire contract. |
| [`docs/networking/`](docs/networking/) | WireGuard and Tailscale transport options for reaching a Terminus deployment off-LAN, including the optional embedded-tsnet mode (MESH-04, `tsnet` Cargo feature — no host `tailscaled` required; see [`docs/networking/tailscale.md`](docs/networking/tailscale.md#alternative-embedded-tsnet-mesh-04--no-host-tailscaled-at-all)). |
| [`docs/deploy/`](docs/deploy/) | Client enrollment/deploy guide and the personal-services (`terminus_personal`/`terminus_primary`) deployment guide. |
| [`docs/tools/`](docs/tools/README.md) | The full tool index — all 53 modules grouped by domain, plus the **MINT** flagship harness. |

## Atlas — knowledge-graph query tools

Atlas (the knowledge-graph subsystem of the Scribe documentation engine, spec
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

## License

MIT — see [`LICENSE`](LICENSE).
