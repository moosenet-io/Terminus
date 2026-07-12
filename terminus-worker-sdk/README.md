# terminus-worker-sdk

Thin authoring surface for a Terminus "tool worker" process (spec item
TMOD-03). A worker is, in the common case, "`impl RustTool` for one or a
handful of tools + a few lines of `main.rs`" — this crate provides everything
around that: a re-export of the existing tool-authoring types, identity/
manifest plumbing, and a server for the `initialize` / `tools/list` /
`tools/call` MCP subset.

## Authoring a worker

```rust,no_run
use terminus_worker_sdk::{RustTool, ToolError, Worker};
use serde_json::Value;

struct Echo;

#[async_trait::async_trait]
impl RustTool for Echo {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echoes its input back" }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(args.get("text").and_then(Value::as_str).unwrap_or("").to_string())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Worker::builder("echo-worker", "0.1.0")
        .capability_class("core")
        .tool(Box::new(Echo))
        .serve("/tmp/echo-worker.sock")
        .await?;
    Ok(())
}
```

That's the whole surface: `Worker::builder(name, semver)`, zero or more
`.tool(Box::new(...))` calls, an optional `.capability_class(...)`, then
`.serve(socket_path)`, which binds a Unix domain socket and serves forever.

## What's re-exported vs. new

- **Re-exported from the main `terminus-rs` crate, unchanged**: `RustTool`
  (the trait every tool implements), `ToolOutput`, `ToolError`, `ToolInfo`.
  This crate does **not** relocate those types — they still live in
  `terminus-rs`'s `src/tool.rs` / `src/error.rs` / `src/registry.rs`; this
  crate depends on `terminus-rs` by path and re-exports them so a worker
  author only ever imports `terminus_worker_sdk`, never `terminus_rs`
  directly.
- **New in this crate**: `Worker` (the builder), `WorkerManifest` (the
  `{name, semver, capability_class, tools}` bundle a worker advertises on
  `initialize`), and `server` (the actual UDS listener).

## Wire protocol

`server::serve` binds a Unix domain socket and speaks newline-delimited
JSON-RPC 2.0: one JSON object per line in, one JSON object per line out. It
implements exactly the same three methods, with the same request/result
*shapes*, that the main `terminus-rs` daemon's HTTP `/mcp` listener speaks
(`mcp_server::handle_mcp` in the root crate):

- `initialize` → `{protocolVersion, capabilities, serverInfo, manifest}`
  (the extra `manifest` field carries `{name, semver, capabilityClass,
  tools}` — additive, ignored by a client that only understands the
  standard MCP `initialize` shape).
- `tools/list` → `{tools: [{name, description, inputSchema}, ...]}`.
- `tools/call` → `{content: [{type: "text", text}], isError,
  structuredContent?}`, dispatched to the registered `RustTool` by name (via
  `execute_structured`, so a tool that overrides it for EGJS-01-style
  structured output works unchanged); an unregistered name gets
  `isError: true` with an "Unknown tool" message, never a JSON-RPC protocol
  error, matching the daemon's own convention.
- A request with no `"id"` (a JSON-RPC notification) gets no reply line,
  same as the daemon's `notifications/initialized` handling.

This is deliberately **not** SSE-framed (unlike the daemon's HTTP listener)
— a worker socket is a private, local implementation detail behind a
daemon-side dispatcher, not a public streamable-HTTP endpoint, so
newline-delimited JSON is the simpler, sufficient framing.

## What this crate deliberately does NOT do (yet)

It does not depend on `terminus-rs::broker::transport` (a sibling item's
`WorkerTransport`, the daemon-side client counterpart, not merged as of this
writing) — this crate's `server` module is its own minimal, self-contained
listener, not that transport's matching server. **Reconciling the two wire
formats (so a daemon's `WorkerTransport` can actually dial and drive a
worker built with this SDK) is a follow-up item**, not part of TMOD-03.

## Validation / failure modes

`Worker::serve()` validates the whole manifest before binding anything, and
fails closed with a clear error rather than starting in a broken state:

- Empty worker name → `ManifestError::EmptyName`.
- Malformed semver → `ManifestError::InvalidSemver`. The version must be a
  real [SemVer 2.0.0](https://semver.org) string: a `MAJOR.MINOR.PATCH`
  core of non-negative integers (no leading zeros), with an **optional**
  `-prerelease` and/or `+build` suffix. So `1.2.3`, `1.0.0-beta`,
  `1.0.0-alpha.1`, and `1.0.0-beta+exp.sha.5114f85` are all accepted;
  `1.0`, `v1.0.0`, `01.2.3`, and `1.0.0-beta_1` are rejected.
- Two tools registered under the same `name()` on one worker →
  `ManifestError::DuplicateTool`.

A worker that registers zero tools is valid — it starts, advertises an
empty catalog, and answers `tools/call` for any name with "Unknown tool".

### Socket path safety

`serve()` never blindly deletes whatever is at `socket_path`. If a **stale
Unix socket** is there (left by a prior unclean shutdown) it is removed so
`bind()` can reclaim the path; if a **non-socket** (a regular file, symlink,
or directory) is there, `serve()` refuses with `ServeError::NotASocket` and
deletes nothing — so pointing a worker at a real file's path can never
destroy it.

No secrets or infra literals are needed by this crate: a Unix domain socket
is authorized by filesystem permissions on the socket path, not by any
credential this SDK manages.
