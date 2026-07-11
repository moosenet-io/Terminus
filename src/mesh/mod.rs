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
//!
//! ## MESH-11: onboarding a new upstream
//! [`onboarding`] adds the first-class, read-only dry-run workflow (and the
//! CORE tool `mesh_onboard_upstream`) an operator uses to try a candidate
//! upstream — probe it, discover its catalog, check namespace/name
//! collisions, confirm trust readiness, and preview the merge delta — before
//! hand-editing `TERMINUS_MESH_UPSTREAMS_JSON`. See that module's doc for why
//! it never mutates config or prints a secret value.
//!
//! ## MESH-03: merging catalogs across many upstreams + local core
//! [`merge`] merges the local core catalog with every healthy upstream's
//! `tools/list` into one `tools/list` result, namespacing every federated
//! tool `<namespace>__<tool>` (see [`merge::MESH_NS_SEP`]) so two upstreams
//! exporting the same bare tool name never collide, and builds the routing
//! a `tools/call` needs to strip that prefix and dispatch to the owning
//! upstream. See that module's doc for the two distinct routing paths it
//! exposes (`MergedCatalog::build` for the full `tools/list`,
//! `resolve_call_route` for a single cheap `tools/call` lookup).
pub mod client;
pub mod identity;
pub mod merge;
pub mod onboarding;
pub mod principal;
pub mod registry;

pub use client::{ToolMeta, UpstreamCallResult, UpstreamClient, UpstreamClientError, UpstreamPool};
pub use identity::TailnetIdentity;
pub use merge::{
    namespaced, resolve_call_route, split_namespaced, upstream_unavailable_text, CallRoute,
    MergedCatalog, Route, RoutingTable, MESH_NS_SEP,
};
pub use onboarding::{
    onboard_upstream, MeshOnboardUpstream, OnboardingError, OnboardingRequest, OnboardingReport,
    TrustStatus,
};
pub use principal::{AuthError, Principal, PrincipalMap, PrincipalResolver, PrincipalSource};
pub use registry::{
    MeshConfigError, ResolvedSecret, UpstreamRegistry, UpstreamServer, UpstreamTransport,
};

/// MESH-06 — [`principal::Principal`]/[`principal::PrincipalResolver`]
/// reconcile [`identity::TailnetIdentity`] and
/// `crate::pki::mtls::ClientIdentity` (mTLS cert CN) with the named PAT
/// identity model (`PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/`GITHUB_PAT_<NAME>`,
/// see `crate::plane`) into one canonical identity. Declared unconditionally
/// (not `#[cfg(feature = "tsnet")]`) for the same reason `identity` is — see
/// that module's doc — even though it depends on [`identity::TailnetIdentity`],
/// which is itself already ungated.
/// MESH-05 — [`identity::TailnetIdentity`] is declared in its own,
/// deliberately UNGATED module (`pub mod identity` above), not inside
/// `tailnet` below, so it's usable on DEFAULT features (no `tsnet` compile
/// feature required). See `identity`'s own module doc for the full
/// reasoning. Only the WhoIs RESOLUTION logic that produces one is gated, in
/// `tailnet` below.
/// MESH-04 — embed tsnet so the gateway is its own tailnet node. Compiled
/// ONLY under the `tsnet` Cargo feature (off by default; see `Cargo.toml`'s
/// comment on the optional `tsnet` dependency) — a default `cargo build`
/// never sees this module's code at all, so it can never pull in the
/// tailscale C library or fail to build on a host lacking it. See
/// `tailnet`'s own module doc for the full design (binding choice, config
/// surface, WhoIs scope boundary with MESH-05).
#[cfg(feature = "tsnet")]
pub mod tailnet;
