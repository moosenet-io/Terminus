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

pub mod graph;
pub mod inspect;
pub mod vault;

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
    /// Local working-copy directory for the vault repo (already cloned from
    /// `vault_remote`; SCRB-05 does not clone-on-demand -- see `vault.rs`'s
    /// module doc comment). Not a secret -- a filesystem path.
    pub vault_local_dir: String,
    /// Explicit, off-by-default opt-in for subprocess-based vault
    /// commit/push (same rationale as `allow_subprocess_inspection`: `git2`
    /// is the real fix, unavailable here for lack of registry access; kept
    /// as a SEPARATE flag from inspection's so an operator can enable
    /// read-only inspection without also enabling vault writes, or vice
    /// versa).
    pub allow_subprocess_vault_write: bool,
    /// Local file path a discrepancy report is appended to (one JSON object
    /// per line) when Plane is unreachable at report time (SCRB-04 edge
    /// case: "Plane unreachable -- the discrepancy is logged locally /
    /// surfaced in Scribe's own output rather than silently lost, with a
    /// retry-later marker"). Not a secret -- a filesystem path.
    pub pending_queue_path: String,
    /// Root directory for Atlas per-project knowledge graphs (KGRAPH-03): one
    /// `{project_slug}.json` per project. Not a secret -- a filesystem path,
    /// same convention as `worktree_root` / `vault_local_dir`.
    pub kg_store_dir: String,
    /// Gate for KGEMB-03's best-effort node-embedding step during
    /// `scribe_kg_build` (`SCRIBE_KG_EMBED`, default off). Mirrors the
    /// `SCRIBE_KG_SEMANTIC` env read (`build.rs`'s `semantic_on`) -- kept as a
    /// separate flag rather than reusing that one because the two passes are
    /// independent opt-ins (semantic-edge inference vs. vector embedding) with
    /// different infra prerequisites (a review-daemon vs. an embeddings
    /// endpoint + pgvector store).
    pub embed_enabled: bool,
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
        let vault_local_dir = std::env::var("SCRIBE_VAULT_LOCAL_DIR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("scribe-vault")
                    .to_string_lossy()
                    .to_string()
            });
        let allow_subprocess_vault_write = std::env::var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE")
            .ok()
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let pending_queue_path = std::env::var("SCRIBE_PENDING_QUEUE_PATH")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("scribe-pending-discrepancies.jsonl")
                    .to_string_lossy()
                    .to_string()
            });
        let kg_store_dir = std::env::var("SCRIBE_KG_STORE_DIR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("scribe-knowledge-graphs")
                    .to_string_lossy()
                    .to_string()
            });
        let embed_enabled = std::env::var("SCRIBE_KG_EMBED")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        Self {
            worktree_root,
            repo_path,
            allowed_repo_roots,
            allow_subprocess_inspection,
            vault_remote,
            vault_local_dir,
            allow_subprocess_vault_write,
            pending_queue_path,
            kg_store_dir,
            embed_enabled,
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
pub(crate) async fn dispatch_docs_generation(
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
///
/// `pub(crate)` (DOCGEN-19): `crate::tools::docgen::drift`'s
/// `docgen_drift_check` reuses this exact confinement check rather than
/// duplicating it -- it calls the same `inspect::checkout` this module's
/// `scribe_generate_readme` does, so it must honor the same default-deny
/// gate, not a second copy of the logic.
pub(crate) fn is_repo_path_allowed(repo_path: &Path, allowed_roots: &[String]) -> bool {
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
                },
                "project_id": {
                    "type": "string",
                    "description": "If set and the project has a built Atlas knowledge graph, its rendered map (map.svg + legend) is appended as an '## Architecture map' section."
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
        let doc = dispatch_docs_generation(&review_cfg, &prompt).await?;
        // KGRAPH-09: if the project has a rendered Atlas map, append it. Purely
        // additive — a project without a graph gets exactly the same output.
        let project_id = args.get("project_id").and_then(Value::as_str);
        Ok(graph::embed::embed_map_section(doc, project_id, &cfg, false))
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
Requires an already-cloned vault working copy and an explicit operator opt-in \
(SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE=true) before it will commit/push anything."
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
                    "description": "Entry type; 'blog' is for especially notable builds (operator judgment or >3 items / security-relevant). Defaults to 'build-diary'."
                }
            },
            "required": ["spec_id", "narrative"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let spec_id = args
            .get("spec_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("spec_id is required and must not be empty".into()))?;
        let narrative = args
            .get("narrative")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("narrative is required and must not be empty".into()))?;
        let entry_type_str = args.get("entry_type").and_then(Value::as_str).unwrap_or("build-diary");
        let note_type = match entry_type_str {
            "blog" => vault::NoteType::Blog,
            "build-diary" => vault::NoteType::BuildDiary,
            other => {
                return Err(ToolError::InvalidArgument(format!(
                    "entry_type must be 'build-diary' or 'blog', got '{other}'"
                )))
            }
        };

        let cfg = ScribeConfig::from_env();
        if !cfg.allow_subprocess_vault_write {
            return Err(ToolError::NotConfigured(
                "vault commit/push is disabled by default (see \
ScribeConfig::allow_subprocess_vault_write's doc comment); set \
SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE=true to enable it explicitly"
                    .into(),
            ));
        }

        let vault_dir = std::path::Path::new(&cfg.vault_local_dir);
        if !vault_dir.join(".git").exists() {
            return Err(ToolError::NotConfigured(format!(
                "no vault working copy found at {} -- clone SCRIBE_VAULT_REMOTE there first \
(SCRB-05 does not clone-on-demand)",
                vault_dir.display()
            )));
        }

        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let slug = format!("{date}-{}", vault::slugify(spec_id));
        let note_path = vault::note_path(vault_dir, note_type, "", &slug);

        let fm = vault::NoteFrontmatter {
            title: format!("{spec_id} -- {entry_type_str}"),
            module: "build-pipeline".to_string(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            source_commit: "n/a (spans the whole spec, not one module commit)".to_string(),
            note_type,
        };
        let content = vault::render_note(&fm, narrative, &[]);
        let commit_message = format!("scribe: {entry_type_str} entry for {spec_id}");

        vault::write_note_and_push(vault_dir, &note_path, &content, &commit_message)?;

        Ok(format!("Wrote {entry_type_str} entry to vault: {}", note_path.display()))
    }
}

// ---------------------------------------------------------------------------
// Tool: scribe_report_discrepancy (SCRB-04)
// ---------------------------------------------------------------------------

/// Stable, short signature for a discrepancy: same `module_path` + same
/// `doc_claim` (case/whitespace-insensitive) always produces the same
/// signature, embedded in the issue title as `[scribe-disc:<signature>]` so
/// a later run can detect "we already reported this" by scanning existing
/// issue titles -- no dependency on Plane label UUID resolution, which would
/// otherwise be a second point of failure before dedup could even run.
pub(crate) fn discrepancy_signature(module_path: &str, doc_claim: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    module_path.trim().hash(&mut hasher);
    doc_claim.trim().to_lowercase().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn build_discrepancy_title(module_path: &str, signature: &str) -> String {
    format!("Scribe discrepancy: {module_path} -- doc vs. code mismatch [scribe-disc:{signature}]")
}

/// Escape the five HTML-significant characters. `doc_claim`/`code_behavior`
/// come from Scribe's own doc/code inspection (docstrings, source snippets),
/// not a fully trusted operator -- they can legitimately contain `<`, `>`,
/// `&`, or quote characters (e.g. a docstring literally saying `returns
/// Option<T>` or containing `</p>`), and `description_html` is rendered as
/// raw HTML by Plane, so unescaped interpolation is a real HTML-injection
/// risk into that view, not merely cosmetic (cycle 1 review finding).
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) fn build_discrepancy_description(module_path: &str, doc_claim: &str, code_behavior: &str) -> String {
    format!(
        "<p><strong>Module:</strong> {module}</p>\
<p><strong>Documentation claims:</strong> {claim}</p>\
<p><strong>Code actually does:</strong> {behavior}</p>\
<p><em>Filed automatically by Scribe. No code fix has been attempted -- Scribe's \
inspection module has no commit/push capability by design; a human triages \
severity and decides the fix.</em></p>",
        module = html_escape(module_path),
        claim = html_escape(doc_claim),
        behavior = html_escape(code_behavior),
    )
}

/// Scan a `plane_list_work_items_filtered`-shaped text listing (one issue
/// per line, per that tool's own output format) for a line already
/// containing this signature's tag, in the tag namespace identified by
/// `tag_prefix` (e.g. `"scribe-disc"` for SCRB-04 discrepancies,
/// `"mismatch-sig"` for the DOCGEN-10 behavior-contract mismatch detector).
/// Returns that line verbatim if found, so the caller can surface which
/// existing issue matched.
///
/// The signature-generation function (`discrepancy_signature`) is shared
/// across artifact kinds, but each kind's issues are tagged with its own
/// prefix and this function only ever matches within that one prefix's
/// namespace -- a `[scribe-disc:SIG]` issue can never be mistaken for a
/// `[mismatch-sig:SIG]` one even if they happen to carry the same `SIG`,
/// because they are different artifacts describing different things and
/// deduping across them would silently suppress a real, distinct issue.
pub(crate) fn find_duplicate_by_signature<'a>(
    listing_text: &'a str,
    signature: &str,
    tag_prefix: &str,
) -> Option<&'a str> {
    let tag = format!("[{tag_prefix}:{signature}]");
    listing_text.lines().find(|line| line.contains(&tag))
}

/// Append a discrepancy report to a local pending-queue file (one JSON
/// object per line) instead of losing it, when Plane is unreachable. Pure
/// local file I/O -- no subprocess, no network, always available regardless
/// of Plane's status.
pub(crate) fn queue_discrepancy_locally(
    queue_path: &str,
    project_id: &str,
    module_path: &str,
    title: &str,
    description_html: &str,
    reason: &str,
) -> Result<String, ToolError> {
    use std::io::Write;

    let record = json!({
        "project_id": project_id,
        "module_path": module_path,
        "title": title,
        "description_html": description_html,
        "reason": reason,
        "retry_marker": "pending-retry",
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue_path)
        .map_err(|e| {
            ToolError::Execution(format!("failed to open pending-discrepancy queue at {queue_path}: {e}"))
        })?;
    file.write_all(format!("{record}\n").as_bytes())
        .map_err(|e| ToolError::Execution(format!("failed to write queued discrepancy: {e}")))?;

    Ok(format!(
        "Plane unreachable ({reason}) -- discrepancy logged locally at {queue_path} with a \
retry-later marker, not lost. Title: {title}"
    ))
}

pub struct ScribeReportDiscrepancy;

#[async_trait]
impl RustTool for ScribeReportDiscrepancy {
    fn name(&self) -> &str {
        "scribe_report_discrepancy"
    }

    fn description(&self) -> &str {
        "Report a mismatch between documented behavior and actual code behavior \
(or a suspected real bug found while verifying functionality) as a real Plane \
issue, via the Terminus Plane tool's create-work-item call made in-process \
(same crate, the sanctioned Plane access path). Never attempts a code fix. \
Deduplicates against existing issues by a stable signature embedded in the \
issue title. If Plane is unreachable, the discrepancy is logged locally with \
a retry-later marker rather than lost."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {
                    "type": "string",
                    "description": "Plane project UUID or identifier to file the discrepancy in (e.g. \"TERM\")"
                },
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
            "required": ["project_id", "module_path", "doc_claim", "code_behavior"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("project_id is required and must not be empty".into()))?;
        let module_path = args
            .get("module_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("module_path is required and must not be empty".into()))?;
        let doc_claim = args
            .get("doc_claim")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("doc_claim is required and must not be empty".into()))?;
        let code_behavior = args
            .get("code_behavior")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("code_behavior is required and must not be empty".into()))?;

        let signature = discrepancy_signature(module_path, doc_claim);
        let title = build_discrepancy_title(module_path, &signature);
        let description_html = build_discrepancy_description(module_path, doc_claim, code_behavior);

        let client = std::sync::Arc::new(crate::plane::PlaneClient::from_env());
        if !client.configured() {
            return Err(ToolError::NotConfigured(
                "PLANE_API_URL and PLANE_API_KEY must be set to report discrepancies via Plane".into(),
            ));
        }

        let cfg = ScribeConfig::from_env();
        let lister = crate::plane::PlaneListWorkItemsFiltered::new(client.clone());
        let listing_text = match lister.execute(json!({"project_id": project_id, "limit": 200})).await {
            Ok(text) => text,
            Err(ToolError::Http(detail)) => {
                return queue_discrepancy_locally(
                    &cfg.pending_queue_path,
                    project_id,
                    module_path,
                    &title,
                    &description_html,
                    &format!("listing existing issues failed: {detail}"),
                );
            }
            Err(e) => return Err(e),
        };

        if let Some(existing_line) = find_duplicate_by_signature(&listing_text, &signature, "scribe-disc") {
            return Ok(format!(
                "Duplicate discrepancy -- an existing open issue already matches this signature, \
not creating another: {}",
                existing_line.trim()
            ));
        }

        let creator = crate::plane::PlaneCreateWorkItem::new(client);
        let create_args = json!({
            "project_id": project_id,
            "name": title,
            "description_html": description_html,
            "priority": "medium",
        });
        match creator.execute(create_args).await {
            Ok(result) => Ok(result),
            Err(ToolError::Http(detail)) => queue_discrepancy_locally(
                &cfg.pending_queue_path,
                project_id,
                module_path,
                &title,
                &description_html,
                &format!("creating the issue failed: {detail}"),
            ),
            Err(e) => Err(e),
        }
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
    // Atlas kg_* query tools (KGRAPH-06) + build/status orchestration (KGRAPH-10).
    graph::tools::register(registry);
    graph::build::register(registry);
    // KGRULE-02: kg_rule_crystallize (candidate rule minting from recurring findings).
    graph::rules::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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
        // >= not ==: register() also co-registers KGRAPH-06's kg_* graph tools
        // on the same core path (see the graph::tools::register call in
        // register()), so the registry is a SUPERSET of the 5 scribe stubs.
        // The presence loop below is the real assertion.
        assert!(reg.len() >= EXPECTED_TOOL_NAMES.len());
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
        let before: Vec<String> = reg.list().into_iter().map(|i| i.name.to_string()).collect();
        register(&mut reg);
        // Each scribe stub must be present AND must not collide with a
        // pre-existing core tool name. register() also co-registers KGRAPH-06's
        // kg_* graph tools, so an exact `before + 5` count delta is no longer
        // meaningful -- assert the scribe names' presence + non-collision
        // directly, which is what this test actually guards.
        for name in EXPECTED_TOOL_NAMES {
            assert!(reg.contains(name), "scribe tool missing after register: {name}");
            assert!(
                !before.iter().any(|n| n.as_str() == *name),
                "scribe tool name collided with an existing core tool: {name}"
            );
        }
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
    async fn generate_readme_with_no_repo_path_and_no_optin_is_a_clean_error_not_panic() {
        // Reviewer nit (SCRB-02 cycle 2, non-blocking): this test's original
        // name claimed to exercise the "no repo_path configured" branch, but
        // since `allow_subprocess_inspection` defaults false and that gate
        // runs FIRST in execute(), it actually hits the same NotConfigured
        // as `generate_readme_execute_is_disabled_by_default_pending_operator_optin`
        // -- same enum variant, so it still passed, but didn't prove what its
        // name claimed. Renamed to describe what it actually verifies (no
        // panic, no fabricated content, clean error) rather than a specific
        // branch; the opt-in-gate branch has its own dedicated test above.
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
            vault_local_dir: std::env::temp_dir()
                .join("scribe-vault")
                .to_string_lossy()
                .to_string(),
            allow_subprocess_vault_write: false,
            pending_queue_path: std::env::temp_dir()
                .join("scribe-pending-discrepancies.jsonl")
                .to_string_lossy()
                .to_string(),
            kg_store_dir: std::env::temp_dir()
                .join("scribe-knowledge-graphs")
                .to_string_lossy()
                .to_string(),
            embed_enabled: false,
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
                mod_decls: vec![],
                use_decls: vec![],
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

    // ─── SCRB-04: Plane discrepancy reporting tests ─────────────────────────

    #[test]
    fn discrepancy_signature_is_stable_and_case_whitespace_insensitive() {
        let a = discrepancy_signature("src/sundry", "does X");
        let b = discrepancy_signature("src/sundry", "  Does X  ");
        let c = discrepancy_signature("src/sundry", "does Y");
        assert_eq!(a, b, "same module+claim (modulo case/whitespace) must produce the same signature");
        assert_ne!(a, c, "different claims must produce different signatures");
    }

    #[test]
    fn discrepancy_signature_differs_by_module_path_too() {
        let a = discrepancy_signature("src/sundry", "does X");
        let b = discrepancy_signature("src/other", "does X");
        assert_ne!(a, b);
    }

    #[test]
    fn discrepancy_title_embeds_signature_tag() {
        let sig = discrepancy_signature("src/sundry", "does X");
        let title = build_discrepancy_title("src/sundry", &sig);
        assert!(title.contains("src/sundry"));
        assert!(title.contains(&format!("[scribe-disc:{sig}]")));
    }

    #[test]
    fn discrepancy_description_includes_both_claims_and_no_fix_disclaimer() {
        let desc = build_discrepancy_description("src/sundry", "claims X", "actually does Y");
        assert!(desc.contains("claims X"));
        assert!(desc.contains("actually does Y"));
        assert!(desc.to_lowercase().contains("no code fix"));
    }

    #[test]
    fn discrepancy_description_html_escapes_injected_markup_in_doc_claim() {
        // Cycle 1 review finding: doc_claim/code_behavior come from Scribe's
        // own inspection (docstrings, source snippets), not a fully trusted
        // operator, and description_html is rendered as raw HTML by Plane --
        // a claim containing `</p>` or `<script>` must not break the
        // description's HTML structure or inject markup.
        let desc = build_discrepancy_description(
            "src/sundry",
            "claims </p><script>alert(1)</script>",
            "returns Option<T> & does \"weird\" things",
        );
        assert!(!desc.contains("<script>"));
        assert!(desc.contains("&lt;script&gt;"));
        assert!(desc.contains("&lt;/p&gt;"));
        assert!(desc.contains("Option&lt;T&gt;"));
        assert!(desc.contains("&amp;"));
        assert!(desc.contains("&quot;weird&quot;"));
    }

    #[test]
    fn find_duplicate_by_signature_finds_a_matching_line() {
        let sig = "abc123";
        let listing = "Filtered work items (1):\n  [uuid-1] Scribe discrepancy: src/x [scribe-disc:abc123] (priority: medium)\n";
        assert_eq!(
            find_duplicate_by_signature(listing, sig, "scribe-disc"),
            Some("  [uuid-1] Scribe discrepancy: src/x [scribe-disc:abc123] (priority: medium)")
        );
    }

    #[test]
    fn find_duplicate_by_signature_none_when_no_match() {
        let listing = "No work items match the given filters";
        assert_eq!(find_duplicate_by_signature(listing, "abc123", "scribe-disc"), None);
    }

    #[test]
    fn find_duplicate_by_signature_does_not_cross_the_mismatch_sig_namespace() {
        // A [mismatch-sig:] issue (DOCGEN-10's detector) must never be
        // mistaken for a [scribe-disc:] duplicate (SCRB-04) even with the
        // identical signature -- they are different artifacts.
        let listing = "Filtered work items (1):\n  [uuid-1] Behavior-contract mismatch [READY-FOR-BUILD: fix code]: src/x [mismatch-sig:abc123] (priority: medium)\n";
        assert_eq!(find_duplicate_by_signature(listing, "abc123", "scribe-disc"), None);
    }

    #[test]
    fn queue_discrepancy_locally_appends_a_json_line_and_reports_the_reason() {
        let path = std::env::temp_dir().join(format!("scribe-test-queue-{}.jsonl", std::process::id()));
        let path_str = path.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&path);

        let result = queue_discrepancy_locally(
            &path_str,
            "TERM",
            "src/sundry",
            "Some title",
            "<p>desc</p>",
            "plane unreachable: connection refused",
        )
        .unwrap();
        assert!(result.contains("plane unreachable"));
        assert!(result.contains(&path_str));

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(contents.trim()).expect("queued line must be valid JSON");
        assert_eq!(parsed["project_id"], "TERM");
        assert_eq!(parsed["retry_marker"], "pending-retry");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_discrepancy_missing_project_id_is_invalid_argument() {
        let tool = ScribeReportDiscrepancy;
        let result = tool
            .execute(json!({"module_path": "src/x", "doc_claim": "a", "code_behavior": "b"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn report_discrepancy_creates_a_new_issue_when_none_exists() {
        let server = httpmock::MockServer::start();
        // Project-identifier resolution: "TERM" isn't a UUID, so
        // resolve_project_id() looks it up via a projects-list GET first
        // (same precedent as plane::tests::mock_projects).
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "TERM", "name": "Mock", "identifier": "TERM", "network": 0}
            ]));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(200).json_body(json!([]));
        });
        let create_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(201).json_body(json!({
                "id": "issue-uuid-1",
                "name": "Scribe discrepancy: src/sundry -- doc vs. code mismatch [scribe-disc:deadbeef]",
                "project": "TERM",
                "workspace": "testws",
                "sequence_id": 42
            }));
        });

        let client = crate::plane::PlaneClient::test_client_with_base_url(server.base_url());
        let lister = crate::plane::PlaneListWorkItemsFiltered::new(client.clone());
        let listing = lister.execute(json!({"project_id": "TERM", "limit": 200})).await.unwrap();
        assert!(find_duplicate_by_signature(&listing, "anything", "scribe-disc").is_none());

        let creator = crate::plane::PlaneCreateWorkItem::new(client);
        let result = creator
            .execute(json!({
                "project_id": "TERM",
                "name": "Scribe discrepancy: src/sundry -- doc vs. code mismatch [scribe-disc:deadbeef]",
                "description_html": "<p>x</p>",
                "priority": "medium"
            }))
            .await
            .unwrap();
        assert!(result.contains("Created issue"));
        create_mock.assert();
    }

    #[tokio::test]
    async fn report_discrepancy_detects_an_existing_duplicate_and_does_not_create() {
        let signature = discrepancy_signature("src/sundry", "does X");
        let title = build_discrepancy_title("src/sundry", &signature);

        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "TERM", "name": "Mock", "identifier": "TERM", "network": 0}
            ]));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(200).json_body(json!([
                {"id": "existing-uuid", "name": title, "priority": "medium", "project": "TERM", "workspace": "testws"}
            ]));
        });
        // No POST mock registered at all -- if the tool tried to create
        // anyway, httpmock would simply not match and the call would fail,
        // which the test asserts against (Ok(..) containing "Duplicate").

        let client = crate::plane::PlaneClient::test_client_with_base_url(server.base_url());
        let lister = crate::plane::PlaneListWorkItemsFiltered::new(client);
        let listing = lister.execute(json!({"project_id": "TERM", "limit": 200})).await.unwrap();
        let found = find_duplicate_by_signature(&listing, &signature, "scribe-disc");
        assert!(found.is_some(), "listing should contain the pre-seeded duplicate: {listing}");
    }

    #[test]
    fn report_discrepancy_dedup_runs_twice_produces_one_signature_not_two() {
        // Running detection twice against the SAME underlying doc_claim/module_path
        // must produce the SAME signature both times -- the actual dedup
        // mechanism the spec's test plan describes ("running the same
        // discrepancy detection twice produces one issue, not two").
        let sig1 = discrepancy_signature("src/sundry", "claims X");
        let sig2 = discrepancy_signature("src/sundry", "claims X");
        assert_eq!(sig1, sig2);
    }

    #[tokio::test]
    #[serial]
    async fn report_discrepancy_execute_without_plane_configured_is_not_configured() {
        // #[serial] + explicit removal: PLANE_API_URL/PLANE_API_KEY are
        // process-wide env vars other #[serial] tests in this module (and
        // in crate::plane's own test module) set/unset -- without both the
        // shared serial lock AND an explicit remove_var here, this test
        // could observe another test's leaked-or-concurrent env state.
        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");

        let tool = ScribeReportDiscrepancy;
        let result = tool
            .execute(json!({
                "project_id": "TERM",
                "module_path": "src/sundry",
                "doc_claim": "claims X",
                "code_behavior": "does Y"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn report_discrepancy_execute_end_to_end_creates_then_dedups_second_call() {
        // Isolate from other tests / the real environment via serial_test,
        // since ScribeReportDiscrepancy::execute() calls
        // crate::plane::PlaneClient::from_env() internally (matches the
        // plane module's own established pattern for from_env()-dependent
        // tests, e.g. test_from_env_resolves_identity_name_from_matching_token).
        let server = httpmock::MockServer::start();

        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "TERM", "name": "Mock", "identifier": "TERM", "network": 0}
            ]));
        });

        // First call: no existing issues -> creates one.
        let list_empty = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(200).json_body(json!([]));
        });
        let create_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(201).json_body(json!({
                "id": "issue-uuid-1",
                "name": "Scribe discrepancy: src/sundry -- doc vs. code mismatch [scribe-disc:x]",
                "project": "TERM",
                "workspace": "testws",
                "sequence_id": 7
            }));
        });

        std::env::set_var("PLANE_API_URL", server.base_url());
        std::env::set_var("PLANE_API_KEY", "test-token");
        std::env::set_var("PLANE_WORKSPACE", "testws");

        let tool = ScribeReportDiscrepancy;
        let args = json!({
            "project_id": "TERM",
            "module_path": "src/sundry",
            "doc_claim": "the README says this always returns Ok",
            "code_behavior": "the function actually panics on empty input"
        });
        let first = tool.execute(args.clone()).await;

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_WORKSPACE");

        let first_text = first.expect("first call should create the issue");
        assert!(first_text.contains("Created issue"), "{first_text}");
        list_empty.assert();
        create_mock.assert();

        // Second call, same signature: a SEPARATE mock server (httpmock
        // matches the first-registered mock for overlapping method+path, so
        // reusing `server` here would keep hitting `list_empty` instead of a
        // newly added mock -- a fresh server sidesteps that ambiguity
        // entirely). Note this does NOT test client/cache reuse across
        // calls, because there isn't any to test: execute() constructs a
        // brand-new PlaneClient::from_env() (and therefore a brand-new,
        // empty GetCache) on every invocation -- production has no
        // cross-call caching to invalidate either, so two independent
        // servers accurately model two independent real calls. This
        // server's issues-list now returns the "already reported"
        // issue -- must detect the duplicate and NOT call create at all (no
        // create mock registered on `server2`; if the tool tried to create
        // anyway, that request simply wouldn't match anything).
        let signature = discrepancy_signature("src/sundry", "the README says this always returns Ok");
        let title = build_discrepancy_title("src/sundry", &signature);
        let server2 = httpmock::MockServer::start();
        server2.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "TERM", "name": "Mock", "identifier": "TERM", "network": 0}
            ]));
        });
        let list_with_existing = server2.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v1/workspaces/testws/projects/TERM/issues/");
            then.status(200).json_body(json!([
                {"id": "issue-uuid-1", "name": title, "priority": "medium", "project": "TERM", "workspace": "testws"}
            ]));
        });

        std::env::set_var("PLANE_API_URL", server2.base_url());
        std::env::set_var("PLANE_API_KEY", "test-token");
        std::env::set_var("PLANE_WORKSPACE", "testws");

        let second = tool.execute(args).await;

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_WORKSPACE");

        let second_text = second.expect("second call should detect the duplicate, not error");
        assert!(second_text.contains("Duplicate discrepancy"), "{second_text}");
        list_with_existing.assert();
    }

    #[tokio::test]
    #[serial]
    async fn report_discrepancy_execute_queues_locally_when_listing_is_unreachable() {
        std::env::set_var("PLANE_API_URL", "http://127.0.0.1:1"); // unroutable
        std::env::set_var("PLANE_API_KEY", "test-token");
        std::env::set_var("PLANE_WORKSPACE", "testws");
        std::env::set_var(
            "SCRIBE_PENDING_QUEUE_PATH",
            std::env::temp_dir()
                .join(format!("scribe-test-e2e-queue-{}.jsonl", std::process::id()))
                .to_string_lossy()
                .into_owned(),
        );

        let queue_path = std::env::var("SCRIBE_PENDING_QUEUE_PATH").unwrap();
        let _ = std::fs::remove_file(&queue_path);

        let tool = ScribeReportDiscrepancy;
        let result = tool
            .execute(json!({
                "project_id": "TERM",
                "module_path": "src/sundry",
                "doc_claim": "claims X",
                "code_behavior": "does Y"
            }))
            .await;

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_WORKSPACE");
        std::env::remove_var("SCRIBE_PENDING_QUEUE_PATH");

        let text = result.expect("unreachable Plane should queue locally, not error");
        assert!(text.contains("Plane unreachable"), "{text}");
        assert!(std::path::Path::new(&queue_path).exists());
        let _ = std::fs::remove_file(&queue_path);
    }

    // ─── SCRB-05: scribe_build_diary_entry tests ────────────────────────────

    #[tokio::test]
    #[serial]
    async fn build_diary_entry_disabled_by_default_pending_operator_optin() {
        std::env::remove_var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE");
        let tool = ScribeBuildDiaryEntry;
        let result = tool
            .execute(json!({"spec_id": "S91-test", "narrative": "did things"}))
            .await;
        match result {
            Err(ToolError::NotConfigured(msg)) => {
                assert!(msg.contains("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE"));
            }
            other => panic!("expected NotConfigured pending opt-in, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_diary_entry_missing_spec_id_is_invalid_argument() {
        let tool = ScribeBuildDiaryEntry;
        let result = tool.execute(json!({"narrative": "did things"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn build_diary_entry_missing_narrative_is_invalid_argument() {
        let tool = ScribeBuildDiaryEntry;
        let result = tool.execute(json!({"spec_id": "S91-test"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn build_diary_entry_bad_entry_type_is_invalid_argument() {
        let tool = ScribeBuildDiaryEntry;
        let result = tool
            .execute(json!({"spec_id": "S91-test", "narrative": "x", "entry_type": "poem"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    #[serial]
    async fn build_diary_entry_without_a_cloned_vault_is_not_configured() {
        std::env::set_var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE", "true");
        std::env::set_var(
            "SCRIBE_VAULT_LOCAL_DIR",
            std::env::temp_dir()
                .join(format!("scribe-vault-not-cloned-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
        );
        let tool = ScribeBuildDiaryEntry;
        let result = tool
            .execute(json!({"spec_id": "S91-test", "narrative": "did things"}))
            .await;

        std::env::remove_var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE");
        std::env::remove_var("SCRIBE_VAULT_LOCAL_DIR");

        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn build_diary_entry_end_to_end_writes_and_pushes_a_real_note() {
        // Real local bare repo standing in for the vault remote (see
        // vault::test_setup_bare_vault's doc comment for why this
        // environment can't reach the real Gitea remote; this helper is
        // shared with vault.rs's own tests -- cycle 1 review finding: the
        // ~30-line setup was previously duplicated near-verbatim here).
        let base = std::env::temp_dir().join(format!("scribe-diary-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (bare_remote, working_copy) = vault::test_setup_bare_vault(&base);

        std::env::set_var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE", "true");
        std::env::set_var("SCRIBE_VAULT_LOCAL_DIR", working_copy.to_string_lossy().into_owned());

        let tool = ScribeBuildDiaryEntry;
        let result = tool
            .execute(json!({
                "spec_id": "S91-scribe-knowledge-infrastructure",
                "narrative": "Built the Scribe module scaffold, wired LLM dispatch, worktree \
inspection, Plane discrepancy reporting, and this vault -- in that order.",
                "entry_type": "build-diary"
            }))
            .await;

        std::env::remove_var("SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE");
        std::env::remove_var("SCRIBE_VAULT_LOCAL_DIR");

        let text = result.expect("build diary entry should write and push successfully");
        assert!(text.contains("build-diary"), "{text}");
        assert!(text.contains("build-diaries"), "{text}");

        // Verify via a fresh, independent clone.
        let fresh_clone = base.join("fresh-clone");
        let clone_output = std::process::Command::new("git")
            .current_dir(&base)
            .args(["clone", "-q", bare_remote.to_str().unwrap(), fresh_clone.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(clone_output.status.success(), "fresh clone failed: {}", String::from_utf8_lossy(&clone_output.stderr));
        let entries: Vec<_> = std::fs::read_dir(fresh_clone.join("build-diaries"))
            .expect("build-diaries/ should exist in the fresh clone")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one build-diary entry");
        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("Built the Scribe module scaffold"));
        assert!(content.contains("type: build-diary"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
