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
//!
//! ## MESH-02: dialing the registered upstreams
//! [`client`] generalizes terminus-rs's client-side MCP transport (mirroring
//! the streamable-HTTP request/response framing `crate::mcp_server` already
//! implements server-side — see that module's doc comment) into
//! [`client::UpstreamClient`]/[`client::UpstreamPool`], which dial ANY
//! registered [`UpstreamServer`] over mTLS or bearer. Nothing before this
//! item made a network call against a registered upstream; this module only
//! parses/validates config. See [`client`]'s own module doc for why this is
//! new client logic rather than a refactor of
//! `crate::federation::PersonalFederationClient` (that client speaks a
//! different, Chord-specific wire protocol entirely).
pub mod client;
pub mod registry;

pub use client::{ToolMeta, UpstreamCallResult, UpstreamClient, UpstreamClientError, UpstreamPool};
pub use registry::{
    MeshConfigError, ResolvedSecret, UpstreamRegistry, UpstreamServer, UpstreamTransport,
};
