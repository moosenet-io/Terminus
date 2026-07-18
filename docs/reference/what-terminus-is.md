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

