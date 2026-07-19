# plane

`src/plane` — 514 KG symbols.

The Plane subsystem wraps the Plane CE project-tracking REST API in 43 typed
tools — issues, projects, cycles, comments, states, and the rest of the
project-management surface the build pipeline runs on. It is the sanctioned door
to Plane for every agent in the fleet. Two design points dominate the module:
**multi-identity authentication** (each call can act as a named identity whose
PAT is selected at call time — a deliberate replacement for the legacy design
that scanned other agents' `.env` files for token substrings) and **shared
pacing** (a proactive rate limiter and GET cache, optionally coordinated
through Redis so every Terminus process shares one rate budget against the same
Plane instance).

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `plane::PlaneClient` | struct | `src/plane/mod.rs` | The typed Plane CE REST client (reqwest); one instance per identity. |
| `plane::PlaneClient::for_identity` | fn | `src/plane/mod.rs` | Derive a client authenticated as a named `PLANE_PAT_<NAME>` identity. |
| `plane::with_identity_param` | fn | `src/plane/mod.rs` | Wrap a tool's JSON schema with the optional `identity` argument (the shared multi-identity convention). |
| `plane::identity_param_schema` | fn | `src/plane/mod.rs` | The schema fragment describing the `identity` parameter. |
| `plane::GetCache` | struct | `src/plane/mod.rs` | TTL-bounded GET response cache (`get`), in-process or Redis-backed. |
| `plane::redis_cache_key` | fn | `src/plane/mod.rs` | Stable cache key derivation for the shared Redis backend. |
| `plane::prefix` | module | `src/plane/prefix.rs` | Project-prefix bookkeeping (baseline store) used by prefix-aware tools. |
| `plane::mock_client` / `plane::mock_projects` / `plane::multi_identity_client` | fns | `src/plane/mod.rs` | Test constructors — the suite runs against mocks, no live Plane needed. |

## How it connects

Registered on **both** registries (`register_all` and `register_personal`) — one
of only four module groups that are (plane, gitea, github, sundry), which is why
the gateway never merges the two registries locally (see
`registry::core_personal_name_collisions`). The `mesh::principal` resolver's
canonical name is the same string space as `PLANE_PAT_<NAME>`, so a caller's
transport identity selects its Plane credential. `review` and the build pipeline
create/update issues through these tools; nothing else in the crate talks to
Plane directly.

## Configuration

- `PLANE_API_URL`, `PLANE_API_KEY` — instance + default token (tools register
  without them and return `NotConfigured` at call time).
- `PLANE_PAT_<NAME>`, `PLANE_IDENTITY_NAME` — named identities and the default.
- `PLANE_WORKSPACE` — workspace slug.
- `PLANE_RPM`, `PLANE_RATE_SHARE` — proactive pacing (default 60 RPM shared
  across 3 consumers → 3s minimum spacing).
- `PLANE_CACHE_TTL_SECS` — GET cache TTL (default 5s).
- `PLANE_REDIS_URL`, `PLANE_REDIS_PASSWORD`, `PLANE_REDIS_TIMEOUT_MS` — optional
  shared Redis cache/limiter; fail-open (Redis down never blocks a Plane call).

## Notes and gaps

Plane CE API quirks the tools work around (client-side state filtering because
the `state_group` query param is unreliable; full project UUIDs required) are
handled inside the tools, not surfaced to callers. This page does not list all
43 tool schemas — see
[docs/tools/project-planning/README.md](../tools/project-planning/README.md)
for per-tool documentation.
