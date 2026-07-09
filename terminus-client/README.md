# terminus-client

Enrollment + mTLS transport client for a `terminus` primary's Gateway
(Phase P2, spec item TCLI-04). A small, standalone Rust library — not a
daemon on its own (that's `terminus-client-daemon`, TCLI-05) — meant to be
embedded into other programs that need to reach a terminus primary's MCP
tool surface over mTLS instead of the plain HTTP+JWT `/mcp` listener.

## What it does

1. **Enrollment** (`enroll()`): calls the primary's `/enroll` endpoint
   (TCLI-02) with a per-identity shared secret you supply, receives a
   short-lived CA-signed leaf certificate + private key + JWT + the
   primary's CA certificate, and persists that material locally (a
   restrictive-permission JSON file, `~/.terminus-client/credentials/<identity>.json`
   by default). A later call with still-valid local material skips
   re-enrollment; a call with expired/near-expiry/corrupt local material
   re-enrolls automatically (self-healing, no manual intervention needed).

2. **mTLS dial** (`connect()`): builds a `rustls` client configuration that
   presents the enrolled leaf cert and trusts *only* the CA certificate
   pinned at enrollment time (never the host's system trust store), then
   dials the primary's mTLS listener (TCLI-03, default port `8301`) and
   completes the handshake. Returns an [`MtlsTransport`] wrapping an
   already-authenticated `tokio_rustls::client::TlsStream` — `into_io()`
   hands you that stream directly, ready for an HTTP client (e.g. `hyper`)
   to drive request/response framing over it. This crate does not itself
   speak HTTP over the connection; that's the caller's job (TCLI-05).

## Why this crate has no dependency on `terminus-rs`

`terminus-client` is scoped to live in this repo for now (per the S107
spec's design decision #1), but it is meant to be pulled into other repos
later (Harmony, Lumina, Scribe — Phase P5) with its own versioning/release
cadence at that point. To keep that extraction cheap, it does not depend on
`terminus-rs` at all: it talks to a terminus primary purely over the wire
(HTTP JSON for `/enroll`, mTLS/TLS for the transport dial), and its
[`EnrolledCredential`] struct matches the server's
`terminus_rs::pki::enroll::EnrollmentResponse` JSON shape by field name via
`serde`, not by sharing a Rust type.

## Embedding it in another program

```rust,ignore
use terminus_client::{connect, enroll, ConnectConfig, EnrollConfig};

// The bootstrap shared secret comes from YOUR program's own secret store —
// this crate never reads it from the environment or hardcodes one.
let shared_secret = my_secret_store::get("TERMINUS_ENROLLMENT_SHARED_SECRET")?;

let enroll_cfg = EnrollConfig::new(
    "http://terminus-primary.internal:8300", // plain HTTP+JWT listener base URL
    "harmony-primary",                        // this client's identity
    shared_secret,
);
let credential = enroll(&enroll_cfg).await?;

let connect_cfg = ConnectConfig {
    host: "terminus-primary.internal".to_string(),
    port: 8301, // the primary's mTLS listener port (TCLI-03)
    server_name: "terminus-primary".to_string(), // the primary's server-cert identity
};
let transport = connect(&credential, &connect_cfg).await?;
let tls_stream = transport.into_io(); // AsyncRead + AsyncWrite, ready for an HTTP client
```

## Running the daemon (`terminus-client-daemon`, TCLI-05)

The `terminus-client-daemon` binary is the runnable half of this crate: it
presents a **plain, loopback-only** MCP endpoint (`POST /mcp`, JSON-RPC 2.0,
SSE-framed responses — the same wire protocol a terminus primary serves) and
forwards every `tools/list` / `tools/call` it receives to the primary over the
mTLS transport above. The local endpoint is plaintext, which is only safe
because it **never leaves loopback**: the bind address is the hardcoded
constant `127.0.0.1` (never sourced from an env var, so no config typo can
widen it to a LAN/internet-reachable bind), while the outbound hop to the
primary is mTLS the whole way.

On startup it enrolls (or reuses a valid cached credential) and completes one
mTLS handshake against the primary **before** accepting any local connection;
if that fails it prints a sanitized error to stderr and exits non-zero
(fail-fast, no partial startup, no hang). Forwarding re-dials a fresh mTLS
connection per request, attaching the enrolled JWT as `Authorization: Bearer`,
so the primary always sees the daemon's enrolled identity.

### Configuration (all env-sourced; no literals baked in)

| Env var | Required | Default | Meaning |
|---|---|---|---|
| `TERMINUS_CLIENT_IDENTITY` | **yes** | — | This daemon's enrollment identity (embedded in its cert CN/SAN and JWT `sub`). |
| `TERMINUS_ENROLLMENT_SHARED_SECRET` | **yes** | — | Bootstrap secret for that identity, materialized into the process env at deploy time — never hardcoded. |
| `TERMINUS_PRIMARY_URL` | no | `http://127.0.0.1:8300` | Primary's plain HTTP+JWT base URL, used only for the one-shot `/enroll` call. |
| `TERMINUS_MTLS_HOST` | no | `127.0.0.1` | Host of the primary's mTLS listener. |
| `TERMINUS_MTLS_PORT` | no | `8301` | Port of the primary's mTLS listener (matches `terminus_rs::config::mtls_port`). |
| `TERMINUS_MTLS_SERVER_IDENTITY` | no | `terminus-primary` | Primary's mTLS server-cert identity, used as the TLS `ServerName`. |
| `TERMINUS_CLIENT_LOCAL_PORT` | no | `8310` | Loopback port this daemon serves its local MCP endpoint on. |
| `TERMINUS_CLIENT_FORWARD_TIMEOUT_SECS` | no | `15` | Per-forwarded-request timeout. |
| `TERMINUS_CLIENT_CATALOG_TTL_SECS` | no | `60` | Tool-catalog cache TTL (refresh-on-miss, else on next access past the TTL). |

The daemon serves `POST /mcp` (MCP) and `GET /healthz` on
`127.0.0.1:${TERMINUS_CLIENT_LOCAL_PORT}`. To point a local MCP client
(Claude Code, per TCLI-06) at it, give the client an HTTP MCP server URL of
`http://127.0.0.1:8310/mcp`. Example invocation (the two required secrets are
materialized into the process environment by the deploy tooling / vault-agent
beforehand — never written inline into a script or unit file):

```sh
# TERMINUS_CLIENT_IDENTITY and TERMINUS_ENROLLMENT_SHARED_SECRET are already
# exported into this process's environment by the deploy step (from the vault).
export TERMINUS_PRIMARY_URL=http://<primary-host>:8300
export TERMINUS_MTLS_HOST=<primary-host>
terminus-client-daemon
```

The local client then sees the primary's full tool catalog (aggregated via
`tools/list`) and every `tools/call` is round-tripped to the primary over
mTLS and relayed back unchanged.

## Errors

Every enrollment/connection failure is a typed [`error::ClientError`]
variant, never a panic — see that module's doc comments for what each
variant means and which layer (bootstrap-secret rejection, network
unreachable, TLS handshake failure, ...) it corresponds to. This crate does
not retry-loop on its own; a caller (TCLI-05/06) decides fallback behavior.

## Secrets discipline

- The bootstrap shared secret is a required parameter to `EnrollConfig`,
  supplied by the calling program — never read from the environment or any
  file by this crate itself, and never hardcoded.
- The issued leaf cert / private key / JWT are persisted locally at
  `0600` (unix) permissions, never a plaintext file at an
  arbitrary/world-readable path.
- The mTLS dial trusts only the CA certificate pinned at enrollment time —
  never the host's system trust store — so a compromised/misconfigured
  system CA bundle cannot be used to intercept the connection to the
  primary.
