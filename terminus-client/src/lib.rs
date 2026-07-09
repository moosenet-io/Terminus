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
//! It deliberately does NOT build a local MCP endpoint, forward tool calls,
//! or expose a daemon binary -- that's TCLI-05, layered on top of this
//! crate's public API.
//!
//! See `README.md` for the embedding story (how another Rust program, e.g.
//! Harmony/Lumina/Scribe, is meant to pull this crate in).

pub mod enroll;
pub mod error;
pub mod transport;

pub use enroll::{enroll, EnrollConfig, EnrolledCredential};
pub use error::ClientError;
pub use transport::{connect, ConnectConfig, MtlsTransport};
