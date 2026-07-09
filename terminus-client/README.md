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
