//! Docgen: the sovereign, in-house documentation engine (DOCGEN-01 scaffold,
//! S95, Plane TERM-143). Replaces Mintlify: triggered after every feat by
//! the build skill (DOCGEN-08, later), it reads what was actually built
//! (the merged diff + spec), deepens a project's documentation, and renders
//! variable output artifacts per project (README, wiki, PDF, Notion/
//! Obsidian notes, dev blog) as declared in that project's doc-target
//! config.
//!
//! ## Scope of THIS item (DOCGEN-01)
//! Module skeleton + core types + registration + the per-project doc-target
//! config schema ([`config`]) ONLY. No generation, rendering, or versioning
//! yet -- those land in DOCGEN-05/06/07. This item's single registered tool
//! ([`DocgenStatus`]) is a read-only config-inspection tool, mirroring how
//! `src/scribe/mod.rs` (SCRB-01) shipped `scribe_status` alongside its own
//! scaffold stubs.
//!
//! ## Reuse plan (reference only -- NOT reimplemented here)
//! Later docgen items reuse existing modules rather than duplicating them:
//!   - `crate::scribe::{inspect, vault}` -- reading a real worktree's
//!     current docs (DOCGEN-05) and writing versioned artifacts into the
//!     Obsidian-compatible knowledge vault (DOCGEN-07) reuse Scribe's
//!     existing worktree-inspection and vault-write plumbing rather than a
//!     second implementation of either.
//!   - `crate::github::pii` -- the PII sweep gate DOCGEN-02 puts in front of
//!     every inference request reuses the same sweep engine the git-public
//!     mirror already runs, not a second scanner.
//!   - Chord owns model routing (DOCGEN-03); this module never picks a
//!     model itself, per the S95 design overview's seam.
//!
//! ## Registration
//! Docgen registers on Chord's core `register_all()` (`src/registry.rs`, via
//! `crate::tools::register` -> `docgen::register`) -- the SAME single
//! registration path every other core tool (`plane`/`gitea`/`github`/
//! `scribe`) uses. There is no separate "personal-only" registry for it.
//!
//! ## Secrets (S95 Pre-flight: `OPENROUTER_API_KEY`, `NOTION_TOKEN`, etc.)
//! This scaffold reads no secret VALUES at all -- see [`config`]'s module
//! doc comment. Vault key NAMES a target may need are named by
//! [`config::DocTargetType::credential_key`]; resolving them to actual
//! values via `vault::manager().get()` / `SecretManager::get()` is deferred
//! to the generation/render items that actually call out to Chord or a
//! target's API.

pub mod config;

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub use config::{DocTargetConfig, DocTargetType, ProjectDocConfig, ResolvedDocTarget};

/// `docgen_status` -- report how the doc engine would interpret a project's
/// declared (or absent) doc-target config: which targets it declares (or
/// the README-only default), and, when a set of available credential key
/// names is supplied, which targets are currently enabled vs. disabled for
/// missing credentials. Read-only; never mutates anything, never generates
/// or renders content -- purely a config-inspection tool for this scaffold
/// item.
pub struct DocgenStatus;

#[async_trait]
impl RustTool for DocgenStatus {
    fn name(&self) -> &str {
        "docgen_status"
    }

    fn description(&self) -> &str {
        "Report the doc engine's interpretation of a project's per-project doc-target \
config: which targets it declares (readme/wiki/pdf/notion/obsidian/blog), the \
README-only default applied when a project declares none, and -- if a list of \
available credential key names is supplied -- which declared targets are \
enabled vs. disabled for a missing credential. Config-inspection only; this \
scaffold item generates/renders nothing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_config": {
                    "type": "object",
                    "description": "The project's raw doc-target config, e.g. {\"targets\": [{\"type\": \"readme\"}, {\"type\": \"notion\", \"options\": {\"database_id\": \"...\"}}]}. Omit (or pass no `targets` key) to see the README-only default that applies to an unconfigured project."
                },
                "available_credential_keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of runtime secret-store KEY NAMES (never values) currently known to be available, e.g. [\"NOTION_TOKEN\"]. When supplied, the response also reports which declared targets are enabled vs. disabled-for-missing-credential."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_config = args.get("project_config");
        let is_default = project_config
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("targets"))
            .and_then(Value::as_array)
            .map(|a| a.is_empty())
            .unwrap_or(true);

        let cfg = ProjectDocConfig::parse(project_config)?;

        let available: BTreeSet<String> = args
            .get("available_credential_keys")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let resolved = cfg.resolve(&available);
        let targets_json: Vec<Value> = resolved
            .iter()
            .map(|r| {
                json!({
                    "type": r.target_type.as_str(),
                    "enabled": r.enabled,
                    "hint": r.hint,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&json!({
            "is_default_readme_only": is_default,
            "targets": targets_json,
        }))
        .unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Docgen tools into the registry. Called from
/// `crate::tools::register` (`src/tools/mod.rs`), itself called from
/// `register_all()` (`src/registry.rs`) -- the same core registration path
/// `plane`/`gitea`/`github`/`scribe` use. Never called from any
/// personal-only path.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenStatus));
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_TOOL_NAMES: &[&str] = &["docgen_status"];

    #[test]
    fn registers_expected_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), EXPECTED_TOOL_NAMES.len());
        for name in EXPECTED_TOOL_NAMES {
            assert!(reg.contains(name), "missing tool: {name}");
        }
    }

    #[test]
    fn every_tool_has_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for info in reg.list() {
            assert_eq!(
                info.parameters.get("type").and_then(Value::as_str),
                Some("object"),
                "tool {} parameters() must be a JSON Schema object",
                info.name
            );
        }
    }

    #[tokio::test]
    async fn docgen_status_reports_readme_only_default_with_no_args() {
        let tool = DocgenStatus;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["is_default_readme_only"], json!(true));
        assert_eq!(parsed["targets"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["targets"][0]["type"], json!("readme"));
        assert_eq!(parsed["targets"][0]["enabled"], json!(true));
    }

    #[tokio::test]
    async fn docgen_status_reports_declared_targets() {
        let tool = DocgenStatus;
        let out = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "readme"}, {"type": "wiki"}]}
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["is_default_readme_only"], json!(false));
        assert_eq!(parsed["targets"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn docgen_status_reports_disabled_target_for_missing_credential() {
        let tool = DocgenStatus;
        let out = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "notion"}]}
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["targets"][0]["type"], json!("notion"));
        assert_eq!(parsed["targets"][0]["enabled"], json!(false));
        assert!(parsed["targets"][0]["hint"]
            .as_str()
            .unwrap()
            .contains("NOTION_TOKEN"));
    }

    /// Negative test: an unknown target type surfaces as a tool error, not
    /// a panic/crash.
    #[tokio::test]
    async fn docgen_status_returns_clear_error_for_unknown_target_type() {
        let tool = DocgenStatus;
        let result = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "sharepoint"}]}
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }
}
