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
use std::path::Path;

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
    /// Local path to the repo Scribe inspects by default when a tool call
    /// doesn't specify one explicitly (SCRB-02/03). Not a secret -- a
    /// filesystem path, same convention as `worktree_root`.
    pub repo_path: Option<String>,
    /// Closed allowlist of local filesystem roots a `repo_path` (argument or
    /// default) must resolve under. Default-deny: empty means NO repo_path
    /// is permitted at all, not "anything goes" -- an operator must
    /// explicitly configure this before `scribe_generate_readme` can inspect
    /// anything (see review finding, SCRB-02 cycle 1: an unconfined
    /// `repo_path` would let a caller point Scribe at an arbitrary local
    /// repo and have its contents shipped to an external LLM provider).
    pub allowed_repo_roots: Vec<String>,
    /// Explicit, off-by-default opt-in for subprocess-based worktree
    /// inspection (SCRB-02 cycle 1 review finding: `inspect::checkout`
    /// shells out to `git`, which `src/tool.rs`'s `RustTool` contract bans
    /// from `execute()`). The real fix is swapping `std::process::Command`
    /// for the `git2` crate (a pure-Rust libgit2 binding -- unlike the
    /// review-daemon's LLM-CLI dispatch, which has NO non-subprocess
    /// alternative at all, git has one; this is a library swap, not a
    /// process-isolation problem needing a daemon-wrap) -- not done in this
    /// item because this sandbox has no crates.io/registry access to add
    /// it. Until that swap lands, this flag makes the interim contract
    /// deviation an explicit, reviewable, default-off operator decision
    /// rather than a silent one baked into the code path.
    pub allow_subprocess_inspection: bool,
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
        let repo_path = std::env::var("SCRIBE_REPO_PATH")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let allowed_repo_roots = std::env::var("SCRIBE_ALLOWED_REPO_ROOTS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let allow_subprocess_inspection = std::env::var("SCRIBE_ALLOW_SUBPROCESS_INSPECTION")
            .ok()
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let vault_remote = std::env::var("SCRIBE_VAULT_REMOTE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Self {
            worktree_root,
            repo_path,
            allowed_repo_roots,
            allow_subprocess_inspection,
            vault_remote,
        }
    }
}

/// Closed, ordered fallback chain of review-daemon providers for docs
/// generation. `agy` (Gemini) first -- largest context window, already
/// proven strong at structured writing per this session's live-verified
/// review use (per the spec's own rationale) -- falling back to `codex` then
/// `opus` if the primary is unavailable/rate-limited. Mirrors
/// `review_daemon::provider::Provider`'s closed-enum spirit: this is the only
/// place the fallback order is defined, and it's a fixed constant, not
/// caller-influenced.
const DOCS_PROVIDER_CHAIN: &[&str] = &["agy", "codex", "opus"];

/// Dispatch a documentation-generation prompt through the review-daemon,
/// trying each provider in [`DOCS_PROVIDER_CHAIN`] in order until one
/// succeeds. Reuses `review::ReviewConfig::dispatch_daemon` directly -- the
/// exact same hardened HTTP call the `review_run` tool uses, no new
/// subprocess/HTTP-client code. Returns the aggregated errors from every
/// attempted provider if all fail (so a caller sees why, not just "failed").
/// Takes `cfg` by reference (dependency-injected, not read from the
/// environment internally) so tests can point it at a mock HTTP server via
/// `ReviewConfig { daemon_url: mock.base_url(), .. }` without ever mutating
/// process-wide environment variables (which would race against other tests
/// running concurrently in the same test binary).
async fn dispatch_docs_generation(
    cfg: &crate::review::ReviewConfig,
    prompt: &str,
) -> Result<String, ToolError> {
    let mut errors = Vec::new();
    for provider in DOCS_PROVIDER_CHAIN {
        match cfg.dispatch_daemon(provider, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => errors.push(format!("{provider}: {e}")),
        }
    }
    Err(ToolError::Execution(format!(
        "all docs-generation providers unavailable: {}",
        errors.join("; ")
    )))
}

/// Default-deny confinement check: `repo_path` must canonicalize to a path
/// under one of `allowed_roots` (also canonicalized, so symlinks/relative
/// segments can't be used to appear inside an allowed root without actually
/// being there). An empty `allowed_roots` means nothing is allowed -- this
/// is NOT "unset = anything goes"; an operator must explicitly list the
/// repos Scribe may inspect (SCRB-02 cycle 1 review finding: an unconfined
/// `repo_path` would let a caller point Scribe at an arbitrary local repo
/// and have its contents shipped to an external LLM provider).
fn is_repo_path_allowed(repo_path: &Path, allowed_roots: &[String]) -> bool {
    if allowed_roots.is_empty() {
        return false;
    }
    let Ok(canon_target) = repo_path.canonicalize() else {
        return false;
    };
    allowed_roots.iter().any(|root| {
        Path::new(root)
            .canonicalize()
            .map(|canon_root| canon_target.starts_with(canon_root))
            .unwrap_or(false)
    })
}

/// Build the JSON context `build_docs_prompt` embeds, from a real
/// [`inspect::ModuleBundle`]. Kept as its own function so prompt-context
/// shaping is unit-testable independent of a real worktree checkout.
fn docs_prompt_context(bundle: &inspect::ModuleBundle) -> Value {
    json!({
        "files": bundle.files.iter().map(|f| json!({
            "path": f.path,
            "doc_comments": f.doc_comments,
            "public_signatures": f.public_signatures,
        })).collect::<Vec<_>>(),
        "existing_readme": bundle.existing_readme,
    })
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

/// ## A note on the `RustTool` no-subprocess contract (resolved here, SCRB-02)
/// `execute()` below calls `inspect::checkout`, which shells out to `git` via
/// `std::process::Command` (see `src/scribe/inspect.rs`). `src/tool.rs`'s
/// `RustTool` contract states `execute()` must never use shell commands or
/// subprocess calls -- SCRB-03 flagged this tension and deferred its
/// resolution to whichever item first wires `inspect.rs` into a live
/// `execute()` body. That's this item. Decision, made explicitly rather than
/// silently: `inspect.rs`'s git invocations are NOT an arbitrary/unsanitized
/// shell surface -- they're built exclusively from a closed
/// `ReadOnlyGitOp` enum, argv-array (never shell-string) construction, an
/// explicit `--` end-of-options separator, and a runtime
/// `assert_read_only_argv` check on every invocation (all peer-reviewed
/// twice in SCRB-03). The contract's underlying security concern --
/// caller-controlled/injectable shell execution -- is what SCRB-03 already
/// closed. Building a full daemon-wrap (matching `review_daemon`/`dgem`'s
/// precedent for LLM-CLI dispatch) to satisfy the contract's letter as well
/// as its spirit is real follow-up work, out of proportion for this item's
/// scope and blocked in this environment by having no crates.io/registry
/// access to add the tooling such a daemon would need; tracked as residual
/// architecture debt rather than silently accepted.
pub struct ScribeGenerateReadme;

#[async_trait]
impl RustTool for ScribeGenerateReadme {
    fn name(&self) -> &str {
        "scribe_generate_readme"
    }

    fn description(&self) -> &str {
        "Generate a module's README content by inspecting its real source (via a \
read-only worktree checkout) and dispatching a documentation-generation prompt \
through the review-daemon (agy, falling back to codex then opus). Returns the \
generated Markdown as this tool's result; does not write or commit anything -- \
persisting it into a vault/repo is a separate concern (SCRB-05)."
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
                    "description": "Git ref to inspect (branch, tag, or commit). Defaults to 'HEAD'."
                },
                "repo_path": {
                    "type": "string",
                    "description": "Local filesystem path to the repo to inspect. Defaults to SCRIBE_REPO_PATH if unset."
                }
            },
            "required": ["module_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let module_path = args
            .get("module_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("module_path is required and must not be empty".into()))?;
        let repo_ref = args.get("repo_ref").and_then(Value::as_str).unwrap_or("HEAD");

        let cfg = ScribeConfig::from_env();

        if !cfg.allow_subprocess_inspection {
            return Err(ToolError::NotConfigured(
                "subprocess-based worktree inspection is disabled by default (see \
ScribeConfig::allow_subprocess_inspection's doc comment for why); set \
SCRIBE_ALLOW_SUBPROCESS_INSPECTION=true to enable it explicitly"
                    .into(),
            ));
        }

        let repo_path_str = args
            .get("repo_path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .or(cfg.repo_path.clone())
            .ok_or_else(|| {
                ToolError::NotConfigured(
                    "no repo_path argument given and SCRIBE_REPO_PATH is not set".into(),
                )
            })?;

        let repo_path = std::path::Path::new(&repo_path_str);
        if !is_repo_path_allowed(repo_path, &cfg.allowed_repo_roots) {
            return Err(ToolError::InvalidArgument(format!(
                "repo_path '{}' is not under any root in SCRIBE_ALLOWED_REPO_ROOTS \
(default-deny: this env var must list the specific repos Scribe may inspect)",
                repo_path.display()
            )));
        }

        let worktree_root = std::path::Path::new(&cfg.worktree_root);

        let wt = inspect::checkout(repo_path, repo_ref, worktree_root)?;
        let bundle = inspect::inspect_module(&wt, module_path)?;
        let context = docs_prompt_context(&bundle);
        let prompt = crate::review::build_docs_prompt(module_path, repo_ref, &context);

        let review_cfg = crate::review::ReviewConfig::from_env();
        dispatch_docs_generation(&review_cfg, &prompt).await
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
        // scribe_generate_readme is real as of SCRB-02 (see the dedicated
        // tests below); the remaining three are still scaffold stubs.
        let wiki = ScribeUpdateWikiPage;
        let result = wiki
            .execute(json!({"module_path": "src/sundry", "title": "Sundry"}))
            .await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[tokio::test]
    async fn generate_readme_without_repo_path_configured_is_a_clean_error_not_panic() {
        // No `repo_path` argument and (within this test process) no
        // SCRIBE_REPO_PATH env var -- must fail with NotConfigured, not panic
        // and not silently fabricate content.
        let generate = ScribeGenerateReadme;
        let result = generate.execute(json!({"module_path": "src/sundry"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn generate_readme_missing_module_path_is_invalid_argument() {
        let generate = ScribeGenerateReadme;
        let result = generate
            .execute(json!({"repo_path": env!("CARGO_MANIFEST_DIR")}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn generate_readme_empty_module_path_is_invalid_argument_not_a_whole_repo_walk() {
        // Cycle 1 review finding: an empty (not missing) module_path used to
        // pass the presence check and silently expand to a full-repo walk
        // via `wt.path.join("")` == `wt.path`. Must now be rejected up front.
        let generate = ScribeGenerateReadme;
        let result = generate
            .execute(json!({"module_path": "", "repo_path": env!("CARGO_MANIFEST_DIR")}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn generate_readme_execute_is_disabled_by_default_pending_operator_optin() {
        // Cycle 1 review finding: execute() called inspect::checkout
        // unconditionally, keeping a live RustTool no-subprocess-contract
        // deviation with no operator opt-in. Default (SCRIBE_ALLOW_SUBPROCESS_
        // INSPECTION unset in this test process) must be a clean NotConfigured,
        // not a checkout attempt.
        let generate = ScribeGenerateReadme;
        let result = generate
            .execute(json!({"module_path": "src/sundry", "repo_path": env!("CARGO_MANIFEST_DIR")}))
            .await;
        match result {
            Err(ToolError::NotConfigured(msg)) => {
                assert!(msg.contains("SCRIBE_ALLOW_SUBPROCESS_INSPECTION"));
            }
            other => panic!("expected NotConfigured pending opt-in, got: {other:?}"),
        }
    }

    #[test]
    fn is_repo_path_allowed_denies_by_default_with_no_roots_configured() {
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(!is_repo_path_allowed(&repo, &[]));
    }

    #[test]
    fn is_repo_path_allowed_allows_a_path_under_a_configured_root() {
        let repo = env!("CARGO_MANIFEST_DIR").to_string();
        let allowed = vec![repo.clone()];
        assert!(is_repo_path_allowed(std::path::Path::new(&repo), &allowed));
    }

    #[test]
    fn is_repo_path_allowed_denies_a_path_outside_every_configured_root() {
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let unrelated_root = std::env::temp_dir();
        // temp_dir is very unlikely to be an ancestor of CARGO_MANIFEST_DIR.
        assert!(!is_repo_path_allowed(&repo, &[unrelated_root.to_string_lossy().into_owned()]));
    }

    #[test]
    fn is_repo_path_allowed_denies_a_nonexistent_path() {
        // canonicalize() fails for a path that doesn't exist -- must deny,
        // not panic or silently pass.
        let bogus = std::path::PathBuf::from("/tmp/scribe-test-does-not-exist-xyz-123");
        let allowed = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        assert!(!is_repo_path_allowed(&bogus, &allowed));
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
            repo_path: None,
            allowed_repo_roots: Vec::new(),
            allow_subprocess_inspection: false,
            vault_remote: None,
        };
        assert!(cfg.worktree_root.contains("scribe-inspect"));
        assert!(cfg.repo_path.is_none());
        assert!(cfg.vault_remote.is_none());
    }

    // ─── SCRB-02: LLM-backend dispatch tests ────────────────────────────────

    #[test]
    fn docs_prompt_context_serializes_files_and_readme() {
        let bundle = inspect::ModuleBundle {
            module_path: "src/sundry".to_string(),
            git_ref: "HEAD".to_string(),
            files: vec![inspect::FileExcerpt {
                path: "src/sundry/mod.rs".to_string(),
                doc_comments: vec!["//! Sundry tools".to_string()],
                public_signatures: vec!["pub struct Health;".to_string()],
            }],
            existing_readme: Some("# Old README".to_string()),
        };
        let ctx = docs_prompt_context(&bundle);
        assert_eq!(ctx["files"][0]["path"], "src/sundry/mod.rs");
        assert_eq!(ctx["existing_readme"], "# Old README");
    }

    #[tokio::test]
    async fn dispatch_docs_generation_tries_agy_first_and_returns_on_success() {
        let server = httpmock::MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/dispatch")
                .header("authorization", "Bearer testtoken")
                .json_body(json!({"provider": "agy", "prompt": "doc this", "timeout_secs": 120}));
            then.status(200).json_body(json!({"text": "# Generated README"}));
        });
        let cfg = crate::review::ReviewConfig {
            daemon_url: server.base_url(),
            daemon_token: Some("testtoken".to_string()),
            openrouter_key: None,
        };
        let result = dispatch_docs_generation(&cfg, "doc this").await.unwrap();
        assert_eq!(result, "# Generated README");
        mock.assert();
    }

    #[tokio::test]
    async fn dispatch_docs_generation_falls_back_through_the_provider_chain() {
        let server = httpmock::MockServer::start();
        // agy and codex both report unavailable; opus (last in the chain)
        // succeeds -- proves the fallback chain, not just the happy path.
        let agy_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/dispatch")
                .json_body(json!({"provider": "agy", "prompt": "doc this", "timeout_secs": 120}));
            then.status(502).json_body(json!({"error": "binary_not_found", "detail": "agy not found"}));
        });
        let codex_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/dispatch")
                .json_body(json!({"provider": "codex", "prompt": "doc this", "timeout_secs": 120}));
            then.status(502).json_body(json!({"error": "timeout", "detail": "codex timed out"}));
        });
        let opus_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/dispatch")
                .json_body(json!({"provider": "opus", "prompt": "doc this", "timeout_secs": 120}));
            then.status(200).json_body(json!({"text": "# Fallback README"}));
        });
        let cfg = crate::review::ReviewConfig {
            daemon_url: server.base_url(),
            daemon_token: Some("testtoken".to_string()),
            openrouter_key: None,
        };
        let result = dispatch_docs_generation(&cfg, "doc this").await.unwrap();
        assert_eq!(result, "# Fallback README");
        agy_mock.assert();
        codex_mock.assert();
        opus_mock.assert();
    }

    #[tokio::test]
    async fn dispatch_docs_generation_aggregates_errors_when_every_provider_fails() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/dispatch");
            then.status(502).json_body(json!({"error": "binary_not_found", "detail": "not found"}));
        });
        let cfg = crate::review::ReviewConfig {
            daemon_url: server.base_url(),
            daemon_token: Some("testtoken".to_string()),
            openrouter_key: None,
        };
        let err = dispatch_docs_generation(&cfg, "doc this").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("agy"));
        assert!(msg.contains("codex"));
        assert!(msg.contains("opus"));
    }

    /// Full, real-checkout-plus-mocked-dispatch flow: real worktree checkout
    /// and real source inspection against this crate's own repo
    /// (src/sundry), with only the review-daemon HTTP call mocked (there is
    /// no running review-daemon/agy in this environment -- see the module
    /// doc comment's note on the live test being environment-blocked).
    #[tokio::test]
    async fn generate_readme_full_flow_with_mocked_daemon() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/dispatch")
                .json_body_partial(json!({"provider": "agy"}).to_string());
            then.status(200).json_body(json!({"text": "# Sundry\n\nUtility tools."}));
        });

        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let worktree_root = std::env::temp_dir().join(format!(
            "scribe-scrb02-fullflow-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&worktree_root);

        let wt = inspect::checkout(&repo, "HEAD", &worktree_root).expect("checkout should succeed");
        let bundle = inspect::inspect_module(&wt, "src/sundry").expect("inspect should succeed");
        let context = docs_prompt_context(&bundle);
        let prompt = crate::review::build_docs_prompt("src/sundry", "HEAD", &context);

        let cfg = crate::review::ReviewConfig {
            daemon_url: server.base_url(),
            daemon_token: Some("testtoken".to_string()),
            openrouter_key: None,
        };
        let result = dispatch_docs_generation(&cfg, &prompt).await.unwrap();
        assert_eq!(result, "# Sundry\n\nUtility tools.");

        inspect::cleanup(&wt).ok();
        let _ = std::fs::remove_dir_all(&worktree_root);
    }
}
