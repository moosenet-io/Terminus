//! terminus-rs: Rust fallback tool implementations for the Chord proxy.

/// Compiled-in semantic version of the terminus-rs tool library (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod ansible;
pub mod approval;
pub mod axon;
/// Broker-side pluggable worker transport (TMOD-02) — `WorkerTransport`
/// trait, the three T0/T1/T2 tiers, and the `MinTierPolicy` minimum-tier
/// floor. See `crate::broker::transport` for the full design.
pub mod broker;
pub mod commute;
/// Vendored byte-for-byte copies of the small lumina-core surfaces terminus
/// references (the S84 assistant-sweep prompt/conversation types), so the
/// crate builds standalone with no `lumina-core` dependency (see `compat`).
pub mod compat;
pub mod config;
/// CONST-02: the constellation aggregation API layer (`/api/*`, `/ws`, and
/// the `constellation-web` static-asset host) -- a compiled-in module of
/// the primary/gateway binary, merged into `crate::mcp_server::build_router`.
/// See `crate::constellation`'s own module doc and
/// `docs/architecture/broker.md` for why this is not a broker worker.
pub mod constellation;
pub mod cortex;
pub mod council;
pub mod crucible;
pub mod dgem;
pub mod dura;
pub mod error;
pub mod federation;
pub mod forge;
pub mod gitea;
pub mod dev;
pub mod gateway;
/// CXEG-05: deterministic `syn`-AST house-style checker (Tier-A lint set) —
/// see `docs/house-style.md` for the rule catalog. Shared by
/// `tests/house_style.rs` (the Stage-4 gate wiring) and the standalone
/// `house_style_check` bin (`src/bin/house_style_check.rs`, local runs).
pub mod house_style;
pub mod gateway_framework;
pub mod github;
pub mod <secret-manager>; // pii-test-fixture
pub mod inference_proxy;
pub mod intake;
pub mod network;
pub mod odyssey;
pub mod openhands;
pub mod google;
pub mod <media-service>; // pii-test-fixture
pub mod litellm;
pub mod lumina_ext;
/// S94 media domain — sovereign orchestration of the self-hosted media stack
/// (Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb). MEDIA-01 scaffold. // pii-test-fixture
pub mod media;
/// Standalone streamable-HTTP MCP server (backs the `terminus_personal` bin).
pub mod mcp_server;
pub mod meridian;
/// Upstream Terminus mesh registry (MESH-01) — config-driven federation
/// targets, replacing the two hard-coded backends.
pub mod mesh;
pub mod mint; // BLD-10: MINT test-harness idle-mode (release GPU/RAM for a compiler run)
pub mod model_advisor;
pub mod <container-mgr>; // pii-test-fixture
pub mod prometheus;
pub mod hearth;
pub mod ledger;
pub mod myelin;
pub mod news;
pub mod nexus;
pub mod pg;
pub mod pki;
pub mod plane;
pub mod ratelimit;
pub mod redis;
pub mod registry;
pub mod relay;
pub mod scribe;
pub mod secrets_bootstrap;
pub mod reminder;
pub mod review;
pub mod routines;
pub mod seer;
pub mod sentinel;
pub mod soma;
pub mod skills;
pub mod synapse;
/// Trivial one-off tools ported from the fleet host that don't warrant their own
/// module (health, echo, utc_now, constellation_version, vector_onboard,
/// searxng_search).
pub mod sundry;
pub mod sysversion;
pub mod time; // CLK-01: authoritative fleet clock (time_now)
pub mod tool;
pub mod tools;
pub mod vector;
pub mod vigil;
pub mod vitals;
pub mod weather;
pub mod wizard;

pub use error::ToolError;
pub use registry::{register_all, register_personal, ToolInfo, ToolRegistry};
pub use tool::RustTool;
