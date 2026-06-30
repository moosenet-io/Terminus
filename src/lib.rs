//! terminus-rs: Rust fallback tool implementations for the Chord proxy.

/// Compiled-in semantic version of the terminus-rs tool library (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod ansible;
pub mod approval;
pub mod axon;
pub mod commute;
/// Vendored byte-for-byte copies of the small lumina-core surfaces terminus
/// references (the S84 assistant-sweep prompt/conversation types), so the
/// crate builds standalone with no `lumina-core` dependency (see `compat`).
pub mod compat;
pub mod config;
pub mod dgem;
pub mod dura;
pub mod error;
pub mod gitea;
pub mod dev;
pub mod gateway;
pub mod github;
pub mod <secret-manager>;
pub mod intake;
pub mod network;
pub mod openhands;
pub mod google;
pub mod <media-service>;
pub mod litellm;
pub mod <container-mgr>;
pub mod prometheus;
pub mod hearth;
pub mod ledger;
pub mod myelin;
pub mod news;
pub mod nexus;
pub mod plane;
pub mod registry;
pub mod relay;
pub mod reminder;
pub mod seer;
pub mod sysversion;
pub mod tool;
pub mod tools;
pub mod vector;
pub mod vitals;
pub mod weather;
pub mod wizard;

pub use error::ToolError;
pub use registry::{register_all, ToolInfo, ToolRegistry};
pub use tool::RustTool;
