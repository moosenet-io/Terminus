# mesh

`src/mesh` — 258 KG symbols; small, but home to the single highest-ranked
function in the repository's call graph (`PrincipalResolver::map`).

Mesh is how Terminus servers find, trust, and identify each other. It has three
parts. The **upstream registry** turns federation from two hard-coded backends
into a config-driven list of upstream Terminus-shaped MCP servers, each dialed
generically over mTLS or bearer auth. The **`Principal` model** unifies the
three ways a caller's identity can arrive on a request — mTLS client-cert CN,
tailnet WhoIs, and the named-PAT convention — into exactly one canonical name
that drives both the gateway's allowlist/RBAC decision and downstream credential
selection. The **embedded tailnet listener** (feature-gated) lets the gateway
join a tailnet in-process via `libtailscale`, with no host daemon.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `mesh::principal::Principal` | struct | `src/mesh/principal.rs` | The unified caller identity: one canonical name in the same string space as `PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`. |
| `mesh::principal::PrincipalResolver` | struct | `src/mesh/principal.rs` | Config-driven mapping from transport identities to a `Principal`; fail-closed precedence. `map` is the repo's top call-graph hotspot. |
| `mesh::registry::UpstreamRegistry` | struct | `src/mesh/registry.rs` | Validated registry of federation targets (`from_json` over `TERMINUS_MESH_UPSTREAMS_JSON`). |
| `mesh::registry::UpstreamServer` | struct | `src/mesh/registry.rs` | One upstream entry; `resolve_secret` lazily reads its credential only right before dialing. |
| `mesh::registry::ResolvedSecret` | struct | `src/mesh/registry.rs` | Wrapper whose `Debug`/`Display` never print the value; `expose` is the single deliberate accessor. |
| `mesh::registry::validate` | fn | `src/mesh/registry.rs` | Structural validation of the registry at load time — no network, no secret reads. |
| `mesh::client::UpstreamClient` / `UpstreamPool` | structs | `src/mesh/client.rs` | Generic client-side MCP transport for dialing any registered upstream. |
| `mesh::tailnet::TailnetServer` | struct | `src/mesh/tailnet.rs` | Feature-gated (`tsnet`) embedded tailnet node: `start`, `hostname`, `serve(router)`. |
| `mesh::onboarding` / `mesh::client_onboarding` | modules | `src/mesh/onboarding.rs`, `client_onboarding.rs` | The `mesh_onboard_upstream` / `mesh_onboard_client` dry-run workflows: probe, catalog-collision check, trust readiness, merge preview — never mutating config. |

## How it connects

`terminus_primary` consults the resolver on every authenticated request before
`mcp_server` dispatches to the registry; `gateway_framework::GatewayFramework::guard`
takes a `Principal` for its allow/deny decision. `pki::mtls` supplies the cert
identity; the tailnet listener supplies WhoIs identity. The upstream client
reuses the same streamable-HTTP MCP framing `mcp_server` implements server-side.
The onboarding tools (`mesh_onboard_upstream`, `mesh_onboard_client`) are on the
core registry.

## Configuration

`TERMINUS_MESH_ENABLED`, `TERMINUS_MESH_UPSTREAMS_JSON`,
`TERMINUS_MESH_PRINCIPAL_MAP_JSON`, `TERMINUS_MESH_HEALTH_INTERVAL_SECS`,
`TERMINUS_MESH_TAILNET_ENABLED` (runtime gate, independent of the `tsnet`
compile feature), `TERMINUS_MESH_GATEWAY_MAGICDNS_NAME`. Upstream credentials
are named by `secret_key` per entry — the registry never reads values at
load/parse time.

## Notes and gaps

Building with `--features tsnet` requires a Go toolchain on the build host (the
`tsnet` crate compiles vendored Go via `go build -buildmode=c-archive`); default
builds never touch it. This page does not cover the enrollment/JWT flow (that is
`pki` + `terminus-client` — see [docs/architecture/auth.md](../architecture/auth.md))
or the federation relay to Chord's personal tools (`src/federation` — see
[docs/architecture/federation.md](../architecture/federation.md)).
