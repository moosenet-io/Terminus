//! Cortex — code-graph / blast-radius / risk-scoring tools.
//!
//! ## CXEG-01: the SSH-relay era is retired
//!
//! Every previous revision of this module was a thin SSH-exec relay to a
//! script (`ops.py`) on a since-RETIRED external fleet host — the same
//! synchronous SSH-client-library-over-TCP transport pattern
//! `crucible`/`sentinel`/`vigil` still use. That host is gone. Cortex's successor is the in-process Atlas code
//! graph (`crate::scribe::graph`, tools `kg_search`/`kg_neighbors`/
//! `kg_subgraph`/`kg_path`/`kg_stats`/`kg_communities`/`kg_query`/
//! `kg_findings`, plus `scribe_kg_build`/`scribe_kg_status`), which builds,
//! persists, and queries a real graph locally — no SSH, no remote script, no
//! "relay whatever the other end says" response shape.
//!
//! This item (CXEG-01) is the foundation re-scaffold, not the full rebuild:
//!
//! - The 7 pure graph-relay tools (`cortex_stats`, `cortex_build`,
//!   `cortex_deps`, `cortex_recent`, `cortex_community`,
//!   `cortex_architecture`, `cortex_flows`) are REMOVED as live tools. Each
//!   has a structured deprecation-alias replacement in [`deprecated`]
//!   pointing at its `kg_*` (or `scribe_kg_build`) successor — no network, no
//!   SSH, just a pointer.
//! - `cortex_scope` and `cortex_review` keep their tool names/parameter
//!   surface (now keyed by `project_id` instead of the old `repo` enum) but
//!   their `execute` bodies are principled stubs — the real Atlas-backed
//!   blast-radius (`cortex_scope`) and risk-scoring (`cortex_review`) logic
//!   lands in **CXEG-02** and **CXEG-04** respectively. Until then they
//!   return a structured `{"status":"pending","item":"CXEG-0N"}` pointer
//!   rather than silently doing nothing or erroring opaquely.
//! - `cortex_audit` keeps its `url` parameter and its existing
//!   `validate_repo_url` front-gate (`audit.rs` — untouched, SSRF-hardened
//!   URL validation with no dependency on the deleted SSH helpers), but its
//!   `execute` body is likewise a stub: **CXEG-11** rebuilds its backend
//!   (presumably against a sandboxed local clone + Atlas build, not a remote
//!   relay). See the stub `execute` body below for the exact pending-item
//!   reference.
//!
//! Net result: this module registers 10 tool NAMES total (unchanged from
//! before, so no MCP-surface churn for callers listing tools), but only 3 are
//! "real" (Atlas-rebuild-pending) tools — `cortex_scope`/`cortex_review`/
//! `cortex_audit` — and the other 7 are pure deprecation aliases with no
//! backend at all. `test_cortex_tools_registered` below asserts this new
//! reality, not the old 10-live-relay-tools shape.
//!
//! ## `project_id`, not `repo`
//!
//! The old fixed two-repo-name allowlist named two repos on the retired
//! fleet-host layout. This module is now keyed
//! by the current Plane-project-prefix convention instead: `TERM`, `LUM`,
//! `HARM`, `CHRD`, `RAIL` (see [`PROJECT_IDS`] / [`validate_project_id`]) —
//! the same `project_id` vocabulary the Atlas KG tools use
//! (`crate::scribe::graph`'s `kg_*` tools all take a `project_id`).
//!
//! ## Secrets / config
//!
//! This crate has no separate `SecretManager::get()` / `vault::manager()` API
//! of its own — the runtime secret store is materialized into the process
//! environment at deploy time, so a plain env read via `crate::config` (or,
//! for the Atlas Postgres DSN specifically, `crate::config::atlas_database_url`)
//! already IS the sanctioned secret read, exactly as documented in
//! `crate::pki`'s module doc and `scribe::graph::vec_embed`'s module doc. Every
//! non-secret tuning flag below is read directly via `std::env::var` (matching
//! `crate::config`'s own `env_nonempty`-style local convention), and the one
//! secret-shaped value this module could reference — the Atlas KG's Postgres
//! DSN — is read exclusively through `crate::config::atlas_database_url()`,
//! never a raw `std::env::var("ATLAS_DATABASE_URL")` inline here.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub mod audit;
pub mod deprecated;

use audit::validate_repo_url;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Valid `project_id`s, replacing the old fleet-host fixed repo-name allowlist.
/// Mirrors the current Plane-project-prefix convention (`CLAUDE.md`'s
/// "Current Plane project prefixes" table) and the `project_id` vocabulary
/// the Atlas KG (`kg_*`) tools already use.
pub const PROJECT_IDS: &[&str] = &["TERM", "LUM", "HARM", "CHRD", "RAIL"];

const MAX_TEXT_LEN: usize = 2000;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Atlas-backed Cortex config: thresholds and feature flags for the
/// CXEG-02/04/11 rebuilds, plus the Atlas KG's Postgres DSN. No SSH/remote-
/// script fields remain (see module doc).
#[derive(Debug, Clone)]
pub struct CortexConfig {
    /// Risk score (0-10 scale, matching `cortex_review`'s original
    /// description) at or above which a review should be flagged for
    /// escalation. From `CORTEX_RISK_SCORE_THRESHOLD`, default `7.0`.
    pub risk_score_threshold: f64,
    /// Feature flag gating the (not-yet-built) Tier B analysis pass. From
    /// `CORTEX_ENABLE_TIER_B`, default `false`.
    pub enable_tier_b: bool,
    /// Feature flag gating the (not-yet-built) Tier C analysis pass. From
    /// `CORTEX_ENABLE_TIER_C`, default `false`.
    pub enable_tier_c: bool,
    /// When `true` (the default), elegance/style findings are advisory-only
    /// and never block a review. From `CORTEX_ELEGANCE_ADVISORY_ONLY`,
    /// default `true`.
    pub elegance_advisory_only: bool,
    /// Cosine-similarity threshold (0.0-1.0) above which two code spans are
    /// considered near-duplicates for the (not-yet-built) dup-detection
    /// pass. From `CORTEX_DUP_COSINE_THRESHOLD`, default `0.85`.
    pub dup_cosine: f64,
    /// The Atlas KG's Postgres DSN, read exclusively through
    /// `crate::config::atlas_database_url()` (see module doc's "Secrets /
    /// config" section) — never a raw `std::env::var` in this module.
    /// `None` means the Atlas KG store is not configured; the CXEG-02/04/11
    /// rebuilds will raise `NotConfigured` rather than guess a DSN.
    pub atlas_database_url: Option<String>,
}

impl CortexConfig {
    pub fn from_env() -> Self {
        CortexConfig {
            risk_score_threshold: env_f64("CORTEX_RISK_SCORE_THRESHOLD", 7.0),
            enable_tier_b: env_flag("CORTEX_ENABLE_TIER_B", false),
            enable_tier_c: env_flag("CORTEX_ENABLE_TIER_C", false),
            elegance_advisory_only: env_flag("CORTEX_ELEGANCE_ADVISORY_ONLY", true),
            dup_cosine: env_f64("CORTEX_DUP_COSINE_THRESHOLD", 0.85),
            atlas_database_url: crate::config::atlas_database_url(),
        }
    }
}

/// Read a non-secret float tuning flag; falls back to `default` when unset
/// or unparseable. Mirrors `crate::config`'s own local env-parsing
/// convention (e.g. `serving_keep_warm_threshold_secs`).
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Read a non-secret boolean tuning flag (`"1"`/`"true"`/`"yes"` are
/// truthy, case-insensitively; anything else, or unset, falls back to
/// `default`).
fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => default,
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Validate a `project_id` against [`PROJECT_IDS`], replacing the old
/// fleet-host repo-name allowlist and its validation helper.
fn validate_project_id(project_id: &str) -> Result<(), ToolError> {
    if PROJECT_IDS.contains(&project_id) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "'project_id' must be one of: {}",
            PROJECT_IDS.join(", ")
        )))
    }
}

fn validate_text_len(s: &str, field: &str) -> Result<(), ToolError> {
    if s.chars().count() > MAX_TEXT_LEN {
        Err(ToolError::InvalidArgument(format!(
            "'{field}' exceeds {MAX_TEXT_LEN} character limit"
        )))
    } else {
        Ok(())
    }
}

fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args[field]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{field}' must be a string")))
}

// ---------------------------------------------------------------------------
// Tool: cortex_scope (stub — real Atlas-backed rebuild is CXEG-02)
// ---------------------------------------------------------------------------

pub struct CortexScope {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexScope {
    fn name(&self) -> &str {
        "cortex_scope"
    }

    fn description(&self) -> &str {
        "PENDING REBUILD (CXEG-02): blast-radius for a planned code change. \
         The SSH-relay-era implementation has been retired; this tool \
         currently returns a structured pending pointer instead of \
         performing a live analysis. project_id: one of TERM/LUM/HARM/CHRD/ \
         RAIL. changed_files: comma-separated list of files. In the \
         meantime, use kg_neighbors / kg_subgraph directly against the \
         Atlas KG for a manual blast-radius query."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "changed_files": { "type": "string", "description": "Comma-separated list of file paths e.g. 'src/cortex/mod.rs,src/cortex/audit.rs'" }
            },
            "required": ["project_id", "changed_files"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        let changed_files = require_str(&args, "changed_files")?;
        validate_project_id(project_id)?;
        validate_text_len(changed_files, "changed_files")?;

        let response = json!({
            "status": "pending",
            "item": "CXEG-02",
            "tool": "cortex_scope",
            "project_id": project_id,
            "message": "cortex_scope's SSH-relay-era backend has been retired; \
                an Atlas-backed blast-radius implementation lands in CXEG-02. \
                In the meantime, query kg_neighbors / kg_subgraph directly \
                against the Atlas KG.",
            "tier_b_enabled": self.config.enable_tier_b,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_review (stub — real Atlas-backed rebuild is CXEG-04)
// ---------------------------------------------------------------------------

pub struct CortexReview {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexReview {
    fn name(&self) -> &str {
        "cortex_review"
    }

    fn description(&self) -> &str {
        "PENDING REBUILD (CXEG-04): post-change risk assessment for modified \
         files. The SSH-relay-era implementation has been retired; this tool \
         currently returns a structured pending pointer instead of a live \
         risk score. project_id: one of TERM/LUM/HARM/CHRD/RAIL. \
         changed_files: comma-separated list of modified file paths. In the \
         meantime, use kg_findings / kg_query directly against the Atlas KG."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "changed_files": { "type": "string", "description": "Comma-separated file paths that were modified" }
            },
            "required": ["project_id", "changed_files"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        let changed_files = require_str(&args, "changed_files")?;
        validate_project_id(project_id)?;
        validate_text_len(changed_files, "changed_files")?;

        let response = json!({
            "status": "pending",
            "item": "CXEG-04",
            "tool": "cortex_review",
            "project_id": project_id,
            "message": "cortex_review's SSH-relay-era backend has been \
                retired; an Atlas-backed risk-scoring implementation lands \
                in CXEG-04. In the meantime, query kg_findings / kg_query \
                directly against the Atlas KG.",
            "risk_score_threshold": self.config.risk_score_threshold,
            "elegance_advisory_only": self.config.elegance_advisory_only,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_audit (stub — real backend rebuild is CXEG-11)
// ---------------------------------------------------------------------------

pub struct CortexAudit {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexAudit {
    fn name(&self) -> &str {
        "cortex_audit"
    }

    fn description(&self) -> &str {
        "PENDING REBUILD (CXEG-11): audit an external public Git repository. \
         The SSH-relay-era implementation (which delegated clone + graph \
         build + report generation to a script on the now-retired fleet \
         host) has been retired. The url argument still passes through the \
         existing SSRF-hardened validator (only public http/https URLs are \
         accepted), but execute() currently returns a structured pending \
         pointer rather than performing a live audit — the real backend \
         (presumably a sandboxed local clone + Atlas build) lands in \
         CXEG-11."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Public git repo URL e.g. 'https://github.com/owner/repo'" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let url = require_str(&args, "url")?;
        // Front-gate unchanged from the SSH-relay era: SSRF-hardened URL
        // validation (`audit.rs`, no dependency on the deleted SSH helpers)
        // runs BEFORE anything else, same as it always has.
        validate_repo_url(url)?;

        // CXEG-11 rebuilds this tool's actual backend (sandboxed local
        // clone + Atlas KG build, replacing the retired remote-script
        // relay). Until then, a valid URL gets a structured pending
        // pointer instead of a live audit -- no network I/O happens here.
        let response = json!({
            "status": "pending",
            "item": "CXEG-11",
            "tool": "cortex_audit",
            "url": url,
            "message": "cortex_audit's SSH-relay-era backend has been \
                retired; a locally-sandboxed clone + Atlas-build \
                implementation lands in CXEG-11. The url has passed \
                SSRF-hardened validation but no audit has been performed.",
            "dup_cosine_threshold": self.config.dup_cosine,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Cortex tools into the ToolRegistry: the 3 Atlas-rebuild-
/// pending stubs (`cortex_scope`/`cortex_review`/`cortex_audit`) plus the 7
/// deprecation aliases for the retired pure-relay tools (see [`deprecated`]).
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(CortexConfig::from_env());

    let _ = registry.register(Box::new(CortexScope {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexReview {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexAudit { config }));

    deprecated::register(registry);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<CortexConfig> {
        Arc::new(CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: false,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
        })
    }

    // --- validation ----------------------------------------------------------

    #[test]
    fn test_validate_project_id_accepts_known_values() {
        for id in PROJECT_IDS {
            assert!(validate_project_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn test_validate_project_id_rejects_unknown() {
        let err = validate_project_id("NOPE").unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => {
                for id in PROJECT_IDS {
                    assert!(msg.contains(id), "expected {id} listed in: {msg}");
                }
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn test_validate_project_id_rejects_legacy_repo_names() {
        // The old fleet-host repo names must no longer validate.
        assert!(validate_project_id("lumina-fleet").is_err());
        assert!(validate_project_id("lumina-terminus").is_err());
    }

    #[test]
    fn test_validate_text_len_rejects_oversized() {
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        assert!(validate_text_len(&huge, "changed_files").is_err());
        assert!(validate_text_len("short", "changed_files").is_ok());
    }

    // --- cortex_scope (stub) ---------------------------------------------------

    #[tokio::test]
    async fn test_scope_rejects_unknown_project_id() {
        let tool = CortexScope { config: test_config() };
        let err = tool
            .execute(json!({"project_id": "NOPE", "changed_files": "a.rs"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_scope_rejects_oversized_changed_files() {
        let tool = CortexScope { config: test_config() };
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = tool
            .execute(json!({"project_id": "TERM", "changed_files": huge}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_scope_returns_pending_pointer_for_valid_input() {
        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "changed_files": "src/cortex/mod.rs"}))
            .await
            .expect("valid input must succeed with a pending pointer, not an error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "pending");
        assert_eq!(v["item"], "CXEG-02");
        assert_eq!(v["project_id"], "TERM");
    }

    // --- cortex_review (stub) --------------------------------------------------

    #[tokio::test]
    async fn test_review_rejects_unknown_project_id() {
        let tool = CortexReview { config: test_config() };
        let err = tool
            .execute(json!({"project_id": "NOPE", "changed_files": "a.rs"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_review_returns_pending_pointer_for_valid_input() {
        let tool = CortexReview { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "LUM", "changed_files": "src/lib.rs"}))
            .await
            .expect("valid input must succeed with a pending pointer, not an error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "pending");
        assert_eq!(v["item"], "CXEG-04");
        assert_eq!(v["project_id"], "LUM");
        assert_eq!(v["risk_score_threshold"], 7.0);
    }

    // --- cortex_audit (stub, still SSRF-gated) ---------------------------------

    #[tokio::test]
    async fn test_audit_rejects_non_public_url_before_stub_response() {
        // test fixture: RFC 1918 private-range address (SSRF-guard test)
        let tool = CortexAudit { config: test_config() };
        let err = tool
            .execute(json!({"url": "https://<internal-ip>/internal"})) // pii-test-fixture
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_audit_rejects_ssh_scheme_url() {
        let tool = CortexAudit { config: test_config() };
        let err = tool
            .execute(json!({"url": "ssh://<email>/owner/repo"})) // pii-test-fixture
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_audit_returns_pending_pointer_for_valid_url() {
        let tool = CortexAudit { config: test_config() };
        let out = tool
            .execute(json!({"url": "https://github.com/octocat/Hello-World"}))
            .await
            .expect("valid url must succeed with a pending pointer, not an error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "pending");
        assert_eq!(v["item"], "CXEG-11");
        assert_eq!(v["url"], "https://github.com/octocat/Hello-World");
    }

    // --- registration -----------------------------------------------------------

    #[test]
    fn test_cortex_tools_registered() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        // 3 Atlas-rebuild-pending stubs + 7 deprecation aliases = 10 names,
        // matching the pre-CXEG-01 tool-name surface (no MCP-listing churn)
        // even though only 3 are "real" tools now.
        assert_eq!(registry.len(), 10);
        for name in [
            "cortex_scope",
            "cortex_review",
            "cortex_audit",
            "cortex_stats",
            "cortex_build",
            "cortex_architecture",
            "cortex_deps",
            "cortex_recent",
            "cortex_community",
            "cortex_flows",
        ] {
            assert!(registry.contains(name), "missing tool {name}");
        }
    }
}
