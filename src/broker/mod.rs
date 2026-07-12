//! Broker-side worker transport (TMOD-02).
//!
//! The broker (terminus-rs primary) reaches out-of-process "workers" —
//! separately-privileged tool implementations that should NOT run inside the
//! broker's own address space — over one of three pluggable, per-worker
//! selectable transports. See [`transport`] for the trait, tiers, and the
//! minimum-tier floor policy.

pub mod transport;
