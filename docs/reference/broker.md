# broker

`src/broker` — 213 KG symbols.

The broker lets tools run *outside* the gateway's address space. A worker is a
separately-privileged process implementing one or a few tools (authored with
the `terminus-worker-sdk` crate — `impl RustTool` plus a few lines of `main`);
the broker inside `terminus_primary` routes registry misses to workers over a
pluggable, per-worker-selectable transport, so a compromised or crashing tool
implementation cannot take the hub down with it, and a privileged tool can run
under a different uid than the gateway.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `broker::routes::RouteTable` | struct | `src/broker/routes.rs` | The broker-owned, atomically-swappable tool-name → worker route table (`new`, `remove`, `default`). |
| `broker::routes::RouteTableSnapshot` | struct | `src/broker/routes.rs` | Point-in-time view used per-dispatch so a live mutation never tears a call. |
| `broker::routes::prune_worker_gen` | fn | `src/broker/routes.rs` | Drops routes belonging to a superseded worker generation (rollout hygiene). |
| `broker::transport` | module | `src/broker/transport/mod.rs` | The `WorkerTransport` trait, the three transport tiers, and the `MinTierPolicy` minimum-tier floor. |
| `broker::transport::uds_peercred::UdsPeercredTransport` | struct | `src/broker/transport/uds_peercred.rs` | T0: Unix domain socket with SO_PEERCRED uid verification (`connect` checks the peer's uid). |
| `broker::transport::uds_mtls` | module | `src/broker/transport/uds_mtls.rs` | T1: UDS with mTLS on top. |
| `broker::transport::mtls_tcp` | module | `src/broker/transport/mtls_tcp.rs` | T2: mTLS over TCP for off-host workers; `MinTierPolicy` floors `write_scoped`/`secret_holding` capability classes at the stronger tiers. |
| `broker::transport::roundtrip` | fn | `src/broker/transport/mod.rs` | One request/response exchange against a worker over its configured transport. |
| `broker::control` | module | `src/broker/control.rs` | The authenticated admin control plane: register/deregister/health/list, mutating the live `RouteTable`. |
| `broker::rollout::route` / `broker::rollout::tool_info` | fns | `src/broker/rollout.rs` | Health-gated blue-green rollout for a worker update: the new generation must pass health before routes flip, with rollback on failure. |

## How it connects

`mcp_server` consults the `RouteTable` when a `tools/call` name misses the
compiled-in registry — the same fall-through hook that otherwise ends in
personal-tool federation. Worker definitions load from the worker transport
registry in `crate::config` (`WorkerTransportRegistry`). The
`terminus-worker-sdk` workspace crate re-exports the `RustTool`/`ToolOutput`/
`ToolError`/`ToolInfo` authoring types from this crate (path dependency, not
duplication) and provides `Worker::builder(..).capability_class(..).tool(..)
.serve(socket)` — the worker-side server for the `initialize`/`tools/list`/
`tools/call` MCP subset.

## Configuration

`TERMINUS_BROKER_WORKERS_JSON` — the worker registry (name, transport tier,
socket/address, capability class). Worker credentials for the mTLS tiers come
from the same PKI material as the gateway (`TERMINUS_CA_*`).

## Notes and gaps

The full design rationale — why the broker is not how the `constellation`
aggregation module is hosted, tier threat models, rollout state machine — lives
in [docs/architecture/broker.md](../architecture/broker.md). This page does not
cover worker packaging/deployment.
