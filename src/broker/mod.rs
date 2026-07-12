//! Broker-side worker transport (TMOD-02).
//!
//! The broker (terminus-rs primary) reaches out-of-process "workers" —
//! separately-privileged tool implementations that should NOT run inside the
//! broker's own address space — over one of three pluggable, per-worker
//! selectable transports. See [`transport`] for the trait, tiers, and the
//! minimum-tier floor policy.

/// TMOD-04: the broker-owned, atomically-swappable tool-name → worker route
/// table, and the dispatch/merge helpers `src/mcp_server.rs` uses to fall
/// through to a worker on a compiled-in registry miss. See [`routes`] for
/// the full design.
pub mod routes;
pub mod transport;

/// TMOD-05: the authenticated admin control plane (register/deregister/
/// health/list) that mutates [`routes::RouteTable`] on a live path — see
/// [`control`] for the full design.
pub mod control;
