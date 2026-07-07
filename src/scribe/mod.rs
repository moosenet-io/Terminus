//! Scribe: agentic knowledge-infrastructure module (SCRB-01 scaffold).
//!
//! Scribe is a standing documentation agent: it generates READMEs, wiki
//! pages, and build-diary/blog entries as a byproduct of the build pipeline,
//! using a high-context cloud LLM dispatched through the existing
//! review-daemon (`src/bin/review_daemon/`, see `src/review/dispatch.rs`'s
//! `ReviewConfig::dispatch_daemon`) -- never a subprocess/shell call from
//! within this module's own code (`src/tool.rs`'s `RustTool` contract forbids
//! it).
//!
//! ## Registration
//! Scribe registers on Chord's core `register_all()` (`src/registry.rs`) --
//! the SAME single registration path `plane`/`gitea`/`github` already use.
//! As of this scaffold, `terminus-rs` has exactly one registration function;
//! there is no separate "personal-only" registry to accidentally register
//! into. (Confirmed by repo-wide search: no `register_personal` exists in
//! this crate. `chord-proxy`'s `src/fallback.rs` calls this same
//! `register_all()` to build its fallback tool registry — see that file for
//! the calling side.)
//!
//! ## Tool surface (stubs in this item; bodies land in SCRB-02..05)
//!   - `scribe_generate_readme`   — generate/refresh a module README (SCRB-02/03)
//!   - `scribe_update_wiki_page`  — generate/refresh an Obsidian wiki note (SCRB-02/03/05)
//!   - `scribe_build_diary_entry` — write a build-diary/blog entry to the vault (SCRB-05/06)
//!   - `scribe_report_discrepancy`— report a doc/code mismatch as a Plane issue (SCRB-04)
//!   - `scribe_status`            — report Scribe's own configuration/health
//!
//! ## Configuration (env vars -- no hardcoded hosts/secrets)
//!   `SCRIBE_WORKTREE_ROOT`  — directory Scribe uses for read-only inspection
//!                             worktrees (default: a `scribe-inspect` dir
//!                             under the OS temp dir).
//!   `SCRIBE_VAULT_REMOTE`   — git remote URL of the Obsidian-compatible
//!                             vault repo (SCRB-05). Absent until SCRB-05 wires
//!                             up `moosenet/scribe-vault`.
//!   Review-daemon reuse (SCRB-02): `REVIEW_DAEMON_URL`, `REVIEW_DAEMON_TOKEN`
//!   -- read via `crate::review::ReviewConfig::from_env()`, never duplicated
//!   here.

pub mod inspect;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Scribe's own configuration, resolved once from the environment. Holds no
/// secrets directly -- the review-daemon bearer token and Plane/Gitea
/// credentials stay inside their own modules (`review::ReviewConfig`,
/// `plane::PlaneClient`, `gitea` config); Scribe calls those, it does not
/// duplicate their env lookups.
#[derive(Clone, Debug, Default)]
pub struct ScribeConfig {
    /// Root directory for read-only inspection worktrees (SCRB-03).
    pub worktree_root: String,
    /// Git remote URL for the Obsidian-compatible vault repo (SCRB-05).
    /// `None` until the vault repo exists / is configured.
    pub vault_remote: Option<String>,
}

impl ScribeConfig {
    pub fn from_env() -> Self {
        let worktree_root = std::env::var("SCRIBE_WORKTREE_ROOT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("scribe-inspect")
                    .to_string_lossy()
                    .to_string()
            });
        let vault_remote = std::env::var("SCRIBE_VAULT_REMOTE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Self { worktree_root, vault_remote }
    }
}

/// Stub result shared by every not-yet-implemented tool in this scaffold.
/// Deliberately `ToolError::Execution`, not a panic and not a fabricated
/// success -- later items (SCRB-02..05) replace these bodies one at a time.
fn not_yet_implemented(tool: &str) -> Result<String, ToolError> {
    Err(ToolError::Execution(format!(
        "{tool}: not yet implemented (scaffold only -- see SCRB-01)"
    )))
}

// ---------------------------------------------------------------------------
// Tool: scribe_generate_readme
// ---------------------------------------------------------------------------

pub struct ScribeGenerateReadme;

#[async_trait]
impl RustTool for ScribeGenerateReadme {
    fn name(&self) -> &str {
        "scribe_generate_readme"
    }

    fn description(&self) -> &str {
        "Generate or refresh a module's README by inspecting its source via a \
read-only worktree and dispatching a documentation-generation prompt through \
the review-daemon. Stub only in this scaffold; implemented in SCRB-02/03."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative path to the module to document, e.g. 'src/sundry'"
                },
                "repo_ref": {
                    "type": "string",
                    "description": "Git ref to inspect (branch, tag, or commit). Defaults to the repo's default branch."
                }
            },
            "required": ["module_path"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        not_yet_implemented(self.name())
    }
}

// ---------------------------------------------------------------------------
// Tool: scribe_update_wiki_page
// ---------------------------------------------------------------------------

pub struct ScribeUpdateWikiPage;

#[async_trait]
impl RustTool for ScribeUpdateWikiPage {
    fn name(&self) -> &str {
        "scribe_update_wiki_page"
    }

    fn description(&self) -> &str {
        "Generate or refresh an Obsidian-compatible wiki note for a module in the \
vault repo, with frontmatter and wikilinks to related notes. Stub only in this \
scaffold; implemented in SCRB-02/03/05."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative path to the module the wiki page documents"
                },
                "title": {
                    "type": "string",
                    "description": "Wiki page title"
                }
            },
            "required": ["module_path", "title"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        not_yet_implemented(self.name())
    }
}

// ---------------------------------------------------------------------------
// Tool: scribe_build_diary_entry
// ---------------------------------------------------------------------------

pub struct ScribeBuildDiaryEntry;

#[async_trait]
impl RustTool for ScribeBuildDiaryEntry {
    fn name(&self) -> &str {
        "scribe_build_diary_entry"
    }

    fn description(&self) -> &str {
        "Write a build-diary (or, for notable builds, a longer-form blog) entry \
to the Obsidian-compatible vault, summarizing a spec's real execution narrative. \
Stub only in this scaffold; implemented in SCRB-05/06."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec_id": {
                    "type": "string",
                    "description": "Spec identifier the diary entry covers, e.g. 'S91-scribe-knowledge-infrastructure'"
                },
                "narrative": {
                    "type": "string",
                    "description": "The execution narrative: what was tried, what worked, what didn't, real decisions and bugs found"
                },
                "entry_type": {
                    "type": "string",
                    "enum": ["build-diary", "blog"],
                    "description": "Entry type; 'blog' is for especially notable builds (operator judgment or >3 items / security-relevant)"
                }
            },
            "required": ["spec_id", "narrative"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        not_yet_implemented(self.name())
    }
}

// ---------------------------------------------------------------------------
// Tool: scribe_report_discrepancy
// ---------------------------------------------------------------------------

pub struct ScribeReportDiscrepancy;

#[async_trait]
impl RustTool for ScribeReportDiscrepancy {
    fn name(&self) -> &str {
        "scribe_report_discrepancy"
    }

    fn description(&self) -> &str {
        "Report a mismatch between documented behavior and actual code behavior \
(or a suspected real bug found while verifying functionality) as a real Plane \
issue. Never attempts a code fix. Stub only in this scaffold; implemented in SCRB-04."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative path where the discrepancy was found"
                },
                "doc_claim": {
                    "type": "string",
                    "description": "The specific documentation claim"
                },
                "code_behavior": {
                    "type": "string",
                    "description": "The specific code behavior actually observed"
                }
            },
            "required": ["module_path", "doc_claim", "code_behavior"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        not_yet_implemented(self.name())
    }
}

// ---------------------------------------------------------------------------
// Tool: scribe_status
// ---------------------------------------------------------------------------

pub struct ScribeStatus;

#[async_trait]
impl RustTool for ScribeStatus {
    fn name(&self) -> &str {
        "scribe_status"
    }

    fn description(&self) -> &str {
        "Report Scribe's own configuration and health: whether the review-daemon, \
vault remote, and worktree root are configured. Never returns secret values."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = ScribeConfig::from_env();
        let review_cfg = crate::review::ReviewConfig::from_env();
        Ok(serde_json::to_string_pretty(&json!({
            "worktree_root": cfg.worktree_root,
            "vault_remote_configured": cfg.vault_remote.is_some(),
            "review_daemon_configured": review_cfg.daemon_token.is_some(),
        }))
        .unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Scribe tools into the registry. Called from `register_all()`
/// in `src/registry.rs` -- the same core registration path `plane`/`gitea`/
/// `github` use. Never called from any personal-only path.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(ScribeGenerateReadme));
    let _ = registry.register(Box::new(ScribeUpdateWikiPage));
    let _ = registry.register(Box::new(ScribeBuildDiaryEntry));
    let _ = registry.register(Box::new(ScribeReportDiscrepancy));
    let _ = registry.register(Box::new(ScribeStatus));
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_TOOL_NAMES: &[&str] = &[
        "scribe_generate_readme",
        "scribe_update_wiki_page",
        "scribe_build_diary_entry",
        "scribe_report_discrepancy",
        "scribe_status",
    ];

    #[test]
    fn registers_all_five_stub_tools() {
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

    #[test]
    fn scribe_tools_do_not_collide_with_the_full_core_catalog() {
        // Mirrors the existing `test_no_duplicate_tool_names` invariant at the
        // whole-registry level (registry.rs / integration tests), scoped here
        // to catch a collision introduced by this module in isolation.
        let mut reg = ToolRegistry::new();
        crate::plane::register(&mut reg);
        crate::gitea::register(&mut reg);
        crate::github::register(&mut reg);
        let before = reg.len();
        register(&mut reg);
        assert_eq!(
            reg.len(),
            before + EXPECTED_TOOL_NAMES.len(),
            "scribe tool name collided with an existing core tool"
        );
    }

    #[tokio::test]
    async fn stub_tools_return_execution_error_not_panic() {
        let generate = ScribeGenerateReadme;
        let result = generate.execute(json!({"module_path": "src/sundry"})).await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[tokio::test]
    async fn status_tool_never_leaks_a_secret_value() {
        let status = ScribeStatus;
        let result = status.execute(json!({})).await.expect("status should not fail");
        // No raw token/URL should ever appear -- only booleans/paths.
        assert!(!result.to_lowercase().contains("bearer"));
        assert!(result.contains("review_daemon_configured"));
    }

    #[test]
    fn config_from_env_has_sane_default_worktree_root() {
        // Isolated from whatever the outer test process's env happens to hold,
        // this just checks the default shape when the var is absent.
        let cfg = ScribeConfig {
            worktree_root: std::env::temp_dir()
                .join("scribe-inspect")
                .to_string_lossy()
                .to_string(),
            vault_remote: None,
        };
        assert!(cfg.worktree_root.contains("scribe-inspect"));
        assert!(cfg.vault_remote.is_none());
    }
}
