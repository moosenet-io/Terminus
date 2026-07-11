//! Upstream Terminus mesh registry (MESH-01).
//!
//! terminus-rs today federates exactly one hard-coded upstream (the
//! personal-registry host, see `crate::federation::PersonalFederationClient`).
//! This module introduces a config-driven registry of *many* upstream
//! Terminus-shaped MCP servers to federate, so a later item (MESH-02) can
//! dial each one generically instead of the code growing a new hard-coded
//! client per upstream.
//!
//! MESH-01 only builds and validates the registry — it does not dial
//! anything. Nothing in this module makes a network call or reads a secret
//! VALUE; [`UpstreamServer::resolve_secret`] is a lazy accessor a later item
//! calls right before dialing, never during load/parse.
//!
//! ## Config surface (non-secret, `std::env::var`)
//! - `TERMINUS_MESH_ENABLED` — bool-ish (`1`/`true`/`yes`/`on`, case
//!   insensitive); anything else (including unset) is treated as disabled.
//! - `TERMINUS_MESH_UPSTREAMS_JSON` — a JSON array of upstream entries (see
//!   [`UpstreamServer`]). Both are plain structural config, not credentials —
//!   consistent with this crate's convention (see `crate::config`) that only
//!   secret VALUES go through the runtime-secret-store path, never structural
//!   knobs like URLs, flags, or feature toggles.
//!
//! ## Secrets: a key NAME only, never a value, resolved lazily
//! Each entry's `secret_key` is a NAME identifying a credential in this
//! crate's runtime-secret-store convention — the same "materialized into the
//! process environment at startup, plain env read afterward IS the secret
//! read" model documented in `crate::pki` and `crate::secrets_bootstrap`
//! (terminus-rs has no separate `SecretManager::get()`/`vault::manager()`
//! API of its own; see `crate::pki`'s module doc for why). The registry NEVER
//! reads the secret's value while loading/validating — only
//! [`UpstreamServer::resolve_secret`], called by a later dial step, does that
//! env read, and it wraps the result in [`ResolvedSecret`] so an accidental
//! `{:?}`/`{}` of the resolved value never leaks it into logs.
pub mod registry;

pub use registry::{
    MeshConfigError, ResolvedSecret, UpstreamRegistry, UpstreamServer, UpstreamTransport,
};

/// MESH-04 — embed tsnet so the gateway is its own tailnet node. Compiled
/// ONLY under the `tsnet` Cargo feature (off by default; see `Cargo.toml`'s
/// comment on the optional `tsnet` dependency) — a default `cargo build`
/// never sees this module's code at all, so it can never pull in the
/// tailscale C library or fail to build on a host lacking it. See
/// `tailnet`'s own module doc for the full design (binding choice, config
/// surface, WhoIs scope boundary with MESH-05).
#[cfg(feature = "tsnet")]
pub mod tailnet;
