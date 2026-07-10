//! `terminus-client` -- the enrollment + mTLS transport client for a
//! terminus primary (Gateway P2, TCLI-04).
//!
//! This crate is the foundation TCLI-05's local MCP-forwarding daemon sits
//! on top of. It does exactly two things, per the TCLI-04 spec item's
//! scope:
//! 1. [`enroll`] -- one-shot (then cached, self-renewing) enrollment
//!    against a terminus primary's `/enroll` endpoint (TCLI-02), yielding a
//!    short-lived CA-signed leaf cert + JWT.
//! 2. [`connect`] -- dial the primary's mTLS listener (TCLI-03) presenting
//!    that enrolled identity, trusting only the CA cert pinned at
//!    enrollment time.
//!
//! TCLI-05 layers a local MCP-forwarding daemon on top of this crate's
//! public API: [`forward`] drives one HTTP/1.1 request/response over a
//! [`connect`]-established mTLS session, and [`mcp_server`] presents a
//! plain, loopback-only MCP endpoint that dispatches to it -- see the
//! `terminus-client-daemon` binary (`src/bin/terminus-client-daemon.rs`)
//! for how those two are wired into a runnable daemon.
//!
//! See `README.md` for the embedding story (how another Rust program, e.g.
//! Harmony/Lumina/Scribe, is meant to pull this crate in).

pub mod enroll;
pub mod error;
pub mod forward;
pub mod mcp_server;
pub mod transport;

pub use enroll::{enroll, EnrollConfig, EnrolledCredential};
pub use error::ClientError;
pub use forward::{
    forward, forward_stream, forward_stream_with_idle_timeout, PrimaryConfig,
    DEFAULT_STREAM_IDLE_TIMEOUT, DEFAULT_STREAM_OPEN_TIMEOUT,
};
pub use mcp_server::DaemonState;
pub use transport::{connect, ConnectConfig, MtlsTransport};
