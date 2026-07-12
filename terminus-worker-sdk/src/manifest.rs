//! `WorkerManifest`: the small bundle of identity metadata a worker
//! advertises on `initialize` — its name, a semver version, a coarse
//! "capability class" hint (e.g. which served tier/pool it belongs to), and
//! its tool catalog.
//!
//! Kept deliberately tiny: no dependency on a full semver crate. A worker's
//! version is operator-authored (a literal in its `main.rs`), so validation
//! here exists to catch a typo at `Worker::serve()` startup time (fail
//! closed, with a clear error) rather than to support real range/compat
//! matching — that belongs to whatever consumes the manifest later (the
//! daemon-side broker), not to this crate.

use serde::{Deserialize, Serialize};

use terminus_rs::registry::ToolInfo;

/// Identity + capability metadata a worker advertises in its `initialize`
/// response, alongside the standard MCP `protocolVersion`/`serverInfo`
/// fields `crate::server` already frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerManifest {
    /// Stable worker identity (e.g. `"gitea-worker"`). Analogous to
    /// `serverInfo.name` in the daemon's own MCP framing.
    pub name: String,
    /// A validated `MAJOR.MINOR.PATCH` semver string (see [`validate_semver`]).
    pub semver: String,
    /// Coarse capability/tier hint (e.g. `"core"`, `"personal"`,
    /// `"mesh-upstream"`) -- opaque to this crate, interpreted by whatever
    /// dispatches work to the worker.
    pub capability_class: String,
    /// The worker's tool catalog, in registration order.
    #[serde(skip)]
    pub tools: Vec<ToolInfo>,
}

/// Errors constructing a [`WorkerManifest`] / building a [`crate::Worker`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("invalid semver \"{0}\": expected MAJOR.MINOR.PATCH numeric segments")]
    InvalidSemver(String),
    #[error("duplicate tool name \"{0}\" registered on the same worker")]
    DuplicateTool(String),
    #[error("worker name must not be empty")]
    EmptyName,
}

/// Validate a `MAJOR.MINOR.PATCH` string. Deliberately strict (exactly three
/// numeric, non-negative, non-empty segments, no pre-release/build metadata)
/// — a worker author who needs more should bring their own semver crate;
/// this is just enough to "refuse to start with a clear error" per the
/// TMOD-03 acceptance criteria, not a general semver implementation.
pub fn validate_semver(version: &str) -> Result<(), ManifestError> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return Err(ManifestError::InvalidSemver(version.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_semver_ok() {
        assert!(validate_semver("1.0.0").is_ok());
        assert!(validate_semver("0.1.0").is_ok());
        assert!(validate_semver("12.34.567").is_ok());
    }

    #[test]
    fn malformed_semver_rejected() {
        assert!(validate_semver("1.0").is_err());
        assert!(validate_semver("1.0.0-beta").is_err());
        assert!(validate_semver("v1.0.0").is_err());
        assert!(validate_semver("1.0.0.0").is_err());
        assert!(validate_semver("").is_err());
        assert!(validate_semver("a.b.c").is_err());
    }
}
