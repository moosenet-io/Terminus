//! `WorkerManifest`: the small bundle of identity metadata a worker
//! advertises on `initialize` — its name, a semver version, a coarse
//! "capability class" hint (e.g. which served tier/pool it belongs to), and
//! its tool catalog.
//!
//! The manifest is the authoritative wire contract: it serializes to exactly
//! the JSON shape the server sends in its `initialize` result's `manifest`
//! field (`{name, semver, capabilityClass, tools: [{name, description,
//! inputSchema}]}`), so the advertised type and the framing never drift. The
//! server delegates to this type's `Serialize` impl rather than hand-building
//! a lookalike object.
//!
//! Semver validation accepts real SemVer 2.0.0 (including `-prerelease` and
//! `+build` metadata, per <https://semver.org>), not just three numeric
//! segments — a worker's version is operator-authored (a literal in its
//! `main.rs`), so this validation exists to catch a genuine typo at
//! `Worker::serve()` startup time (fail closed, with a clear error). A
//! dedicated `semver` crate is deliberately not pulled in: it isn't already
//! in this workspace's dependency tree (Cargo.lock has no `semver` entry),
//! and the grammar below is small and self-contained.

use serde::ser::{Serialize, SerializeStruct, Serializer};

use terminus_rs::registry::ToolInfo;

/// Identity + capability metadata a worker advertises in its `initialize`
/// response, alongside the standard MCP `protocolVersion`/`serverInfo`
/// fields `crate::server` already frames.
#[derive(Debug, Clone)]
pub struct WorkerManifest {
    /// Stable worker identity (e.g. `"gitea-worker"`). Analogous to
    /// `serverInfo.name` in the daemon's own MCP framing.
    pub name: String,
    /// A validated SemVer 2.0.0 string (see [`validate_semver`]).
    pub semver: String,
    /// Coarse capability/tier hint (e.g. `"core"`, `"personal"`,
    /// `"mesh-upstream"`) -- opaque to this crate, interpreted by whatever
    /// dispatches work to the worker.
    pub capability_class: String,
    /// The worker's tool catalog, in registration order.
    pub tools: Vec<ToolInfo>,
}

impl Serialize for WorkerManifest {
    /// Serialize to the wire contract: `{name, semver, capabilityClass,
    /// tools: [{name, description, inputSchema}]}`. `ToolInfo` (defined in
    /// `terminus-rs`) is not itself `Serialize`, so its three fields are
    /// mapped explicitly here into the same `name`/`description`/`inputSchema`
    /// tool shape the daemon's own `tools/list` uses.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let tools: Vec<serde_json::Value> = self
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.parameters,
                })
            })
            .collect();

        let mut s = serializer.serialize_struct("WorkerManifest", 4)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("semver", &self.semver)?;
        s.serialize_field("capabilityClass", &self.capability_class)?;
        s.serialize_field("tools", &tools)?;
        s.end()
    }
}

/// Errors constructing a [`WorkerManifest`] / building a [`crate::Worker`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("invalid semver \"{0}\": expected a SemVer 2.0.0 version (MAJOR.MINOR.PATCH, optional -prerelease and +build)")]
    InvalidSemver(String),
    #[error("duplicate tool name \"{0}\" registered on the same worker")]
    DuplicateTool(String),
    #[error("worker name must not be empty")]
    EmptyName,
}

/// Validate a SemVer 2.0.0 version string (<https://semver.org>): a
/// `MAJOR.MINOR.PATCH` core of non-negative integers with no leading zeros,
/// an optional `-prerelease` (dot-separated identifiers of `[0-9A-Za-z-]`,
/// numeric identifiers without leading zeros), and an optional `+build`
/// (dot-separated identifiers of `[0-9A-Za-z-]`, leading zeros allowed).
///
/// This is a real (if compact) SemVer grammar check, not the earlier
/// three-numeric-segments-only heuristic — `1.0.0-beta` and `1.0.0+build.1`
/// are valid and accepted.
pub fn validate_semver(version: &str) -> Result<(), ManifestError> {
    let invalid = || ManifestError::InvalidSemver(version.to_string());

    // Split off build metadata (everything after the FIRST '+'), then
    // pre-release (everything after the FIRST '-' in what remains).
    let (core_and_pre, build) = match version.split_once('+') {
        Some((lhs, rhs)) => (lhs, Some(rhs)),
        None => (version, None),
    };
    let (core, pre) = match core_and_pre.split_once('-') {
        Some((lhs, rhs)) => (lhs, Some(rhs)),
        None => (core_and_pre, None),
    };

    // Core: exactly three numeric identifiers.
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| is_numeric_identifier(p)) {
        return Err(invalid());
    }

    // Pre-release: one-or-more dot-separated identifiers; alphanumeric-or-hyphen,
    // and numeric identifiers must not carry leading zeros.
    if let Some(pre) = pre {
        if pre.is_empty() {
            return Err(invalid());
        }
        for ident in pre.split('.') {
            if ident.is_empty() || !is_alnum_hyphen(ident) {
                return Err(invalid());
            }
            // A purely-numeric pre-release identifier must not have a leading zero.
            if ident.bytes().all(|b| b.is_ascii_digit()) && !is_numeric_identifier(ident) {
                return Err(invalid());
            }
        }
    }

    // Build metadata: one-or-more dot-separated identifiers; alphanumeric-or-hyphen.
    // Leading zeros ARE allowed in build identifiers (per the spec).
    if let Some(build) = build {
        if build.is_empty() {
            return Err(invalid());
        }
        for ident in build.split('.') {
            if ident.is_empty() || !is_alnum_hyphen(ident) {
                return Err(invalid());
            }
        }
    }

    Ok(())
}

/// A numeric identifier: all ASCII digits, non-empty, and no leading zero
/// unless the identifier is exactly `"0"`.
fn is_numeric_identifier(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    !(s.len() > 1 && s.starts_with('0'))
}

fn is_alnum_hyphen(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
    fn valid_prerelease_and_build_accepted() {
        // Finding #2: real SemVer with pre-release / build metadata.
        assert!(validate_semver("1.0.0-beta").is_ok());
        assert!(validate_semver("1.0.0-alpha.1").is_ok());
        assert!(validate_semver("1.0.0-0.3.7").is_ok());
        assert!(validate_semver("1.0.0-x-y-z.--").is_ok());
        assert!(validate_semver("1.0.0+build.1").is_ok());
        assert!(validate_semver("1.0.0<phone>").is_ok());
        assert!(validate_semver("1.0.0-beta+exp.sha.5114f85").is_ok());
        assert!(validate_semver("1.0.0+21AF26D3---117B344092BD").is_ok());
    }

    #[test]
    fn malformed_semver_rejected() {
        assert!(validate_semver("1.0").is_err());
        assert!(validate_semver("1.0.0.0").is_err());
        assert!(validate_semver("v1.0.0").is_err());
        assert!(validate_semver("").is_err());
        assert!(validate_semver("a.b.c").is_err());
        // Leading zero in a core numeric identifier.
        assert!(validate_semver("01.0.0").is_err());
        // Leading zero in a numeric pre-release identifier.
        assert!(validate_semver("1.0.0-01").is_err());
        // Empty pre-release / build.
        assert!(validate_semver("1.0.0-").is_err());
        assert!(validate_semver("1.0.0+").is_err());
        // Illegal character in an identifier.
        assert!(validate_semver("1.0.0-beta_1").is_err());
    }

    #[test]
    fn manifest_serializes_tool_catalog() {
        // Finding #1: the manifest TYPE serializes {name, semver,
        // capabilityClass, tools:[...]} -- tools are not skipped.
        let manifest = WorkerManifest {
            name: "w".to_string(),
            semver: "1.2.3".to_string(),
            capability_class: "core".to_string(),
            tools: vec![ToolInfo {
                name: "echo".to_string(),
                description: "Echoes".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        };
        let v = serde_json::to_value(&manifest).unwrap();
        assert_eq!(v["name"], "w");
        assert_eq!(v["semver"], "1.2.3");
        assert_eq!(v["capabilityClass"], "core");
        assert_eq!(v["tools"][0]["name"], "echo");
        assert_eq!(v["tools"][0]["description"], "Echoes");
        assert_eq!(v["tools"][0]["inputSchema"]["type"], "object");
    }
}
