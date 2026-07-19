# Getting Started

This walkthrough goes from clone to a working MCP connection against the
`terminus_primary` gateway. Every command names a real binary from
`Cargo.toml`/`src/bin/`; every configuration key is a real env var name — values
come from your secret store at runtime and are never written into source or
committed files.

## Prerequisites

- Rust (the repo pins its toolchain via `rust-toolchain.toml`; rustup will
  auto-install it on first build).
- `pkg-config` + OpenSSL development headers (for `reqwest`/TLS).
- Optional: a Postgres instance for the `pg_*` / intake storage tools, a Redis
  instance for the shared Plane cache and compiler queue.
- Not required: Go. The `tsnet` feature (embedded tailnet) is off by default and
  is the only thing that needs a Go toolchain.

## Build

```sh
git clone <your-remote>/Terminus
cd Terminus
cargo build --release
```

This builds the workspace: the `terminus-rs` library plus its binaries,
`terminus-client` (enrollment + mTLS client), and `terminus-worker-sdk`.

Verify the quality gates locally the same way the pipeline does:

```sh
cargo run --bin house_style_check   # deterministic house-style lint, exit 1 on violation
cargo test -p terminus-rs           # includes the registry and tool tests
```

## Configure

Terminus reads configuration exclusively from environment variables. Tools whose
integrations are unconfigured still register and return a clean `NotConfigured`
error when called — you can start the server with nothing set and add
integrations incrementally.

| Area | Keys (names only) |
|---|---|
| Gateway listener | `TERMINUS_PRIMARY_PORT` (default 8310), `TERMINUS_PRIMARY_BIND` (default loopback) |
| Gateway CA / auth | `TERMINUS_CA_CERT`, `TERMINUS_CA_KEY`, `TERMINUS_CA_STORE_PATH`, `TERMINUS_JWT_SIGNING_KEY`, `TERMINUS_ENROLLMENT_SHARED_SECRET_<NAME>`, `TERMINUS_GATEWAY_ALLOWLIST_JSON` |
| Plane | `PLANE_API_URL`, `PLANE_API_KEY`, `PLANE_PAT_<NAME>`, `PLANE_WORKSPACE`, `PLANE_REDIS_URL` (optional shared cache) |
| Gitea | `GITEA_URL`, `GITEA_PAT_<NAME>`, `GITEA_IDENTITY_NAME`, `GITEA_OWNER` |
| GitHub | `GITHUB_PAT_<NAME>`, `GITHUB_IDENTITY_NAME`, `GITHUB_ORG` |
| Postgres suite | `POSTGRES_URL_<NAME>` (e.g. `readonly` / `writer` / `admin` identities) |
| Review | `REVIEW_DAEMON_URL`, `REVIEW_DAEMON_TOKEN`, `OPENROUTER_API_KEY` |
| Atlas / scribe | `SCRIBE_KG_STORE_DIR`, `SCRIBE_REPO_PATH`, `SCRIBE_ALLOWED_REPO_ROOTS`, `SCRIBE_WORKTREE_ROOT` |
| Mesh federation | `TERMINUS_MESH_ENABLED`, `TERMINUS_MESH_UPSTREAMS_JSON`, `TERMINUS_MESH_PRINCIPAL_MAP_JSON` |
| Workers | `TERMINUS_BROKER_WORKERS_JSON` |

The full per-subsystem configuration surface is documented on each
[reference page](reference/index.md).

## Run the gateway

```sh
cargo run --release --bin terminus_primary
```

The gateway binds loopback by default (defense in depth — put a reverse proxy or
the mTLS/tailnet front door in front for anything wider). Check liveness:

```sh
curl -s http://127.0.0.1:8310/healthz
```

To run the personal/admin deployment instead (189-tool subset, streamable-HTTP
MCP on `TERMINUS_PERSONAL_PORT`, default 8300, optional
`TERMINUS_PERSONAL_TOKEN` bearer auth):

```sh
cargo run --release --bin terminus_personal
```

## Connect an MCP client

The wire protocol is JSON-RPC 2.0 over streamable HTTP (protocol version
`2024-11-05`): `POST` an `initialize` request, then `tools/list` and
`tools/call`. A raw smoke test:

```sh
curl -s -X POST http://127.0.0.1:8310/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Any MCP-capable client can consume the same endpoint. For programmatic access
from another Rust service, embed the `terminus-client` crate: `enroll` once
against the primary's `/enroll` endpoint, then `connect` over mTLS with the
enrolled leaf cert — or run `terminus-client-daemon`, which presents a plain
loopback MCP endpoint and forwards over mTLS for you.

## Next steps

- [Run a model-intake sweep](guides/run-a-model-intake-sweep.md)
- [Run a review panel](guides/run-a-review-panel.md)
- [Run the git-public mirror](guides/run-the-git-public-mirror.md)
- [Architecture](architecture.md) — how a call flows through mesh → principal → registry → tool.
