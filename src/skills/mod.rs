//! Skills tools — ported from the Python `skills_tools.py` on the fleet host.
//!
//! Skills are filesystem CRUD over `active/`/`proposed/` skill directories in
//! agentskills.io markdown format (YAML frontmatter + Markdown body), rooted
//! at `<path>/skills/` on the fleet host — confirmed live against
//! the fleet host via `tools/call`:
//!   - `skills_create` (proposed=true, default) returned
//!     `"path": "<path>/skills/proposed/<name>/SKILL.md"`.
//!   - `skills_list` reads the `active/` directory (returns
//!     `{"count": 0, "skills": []}` when empty — confirmed empty on the live
//!     server at port time).
//!   - `skills_read` on a name that only exists in `proposed/` still found it
//!     (checks `active/` then falls back to `proposed/`), returning
//!     `{"name", "status", "meta", "body", "raw"}` — `meta` is the parsed
//!     YAML frontmatter (`agent`, `description`, `license: MIT`, `name`,
//!     `tags: [...]`, `version: '1.0'`), `body` is the Markdown after the
//!     frontmatter, `raw` is the full file text.
//!   - `skills_read` on a missing name returned
//!     `{"error": "Skill '<name>' not found. Use skills_list() to see available skills."}`
//!     (not an MCP-level error — `isError: false` with an error payload).
//!
//! This port:
//! - Uses the `ssh2` crate for typed SSH execution (no `shell=True`, no
//!   subprocess), mirroring `dev/mod.rs`'s read/write file pattern and
//!   `sentinel/mod.rs` / `vigil/mod.rs`'s fleet-host script convention —
//!   Terminus itself does not run on the fleet host, so reaching
//!   `<path>/skills/` means an SSH hop, exactly like every other
//!   module that touches fleet-host paths.
//! - Adds a strict `skill_name` allowlist (kebab-case: lowercase letters,
//!   digits, hyphens only) before it is ever placed into a remote path or
//!   shell command. The Python docstring says "kebab-case" but the live
//!   server did not appear to validate this beyond normal path use; we
//!   enforce it here since `skill_name` becomes a directory name.
//!
//! ## Tools (identical names to the Python source)
//!   skills_list   — list skills in the active/ directory
//!   skills_read   — read a skill's full SKILL.md (active/, then proposed/)
//!   skills_create — create a new skill (proposed/ by default)
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   SKILLS_SSH_HOST      — SSH host of the fleet box (e.g. "192.168.0.X").
//!   SKILLS_SSH_USER      — SSH user, default "root".
//!   SKILLS_SSH_KEY_PATH  — path to the SSH private key file.
//!   SKILLS_ACTIVE_DIR    — default "<path>/skills/active".
//!   SKILLS_PROPOSED_DIR  — default "<path>/skills/proposed".
//!
//! ## Security model
//! - `skill_name` must match `^[a-z0-9][a-z0-9-]*$` — no path separators,
//!   dots, or shell metacharacters can reach a remote command.
//! - All remote paths are built from the fixed `active`/`proposed` roots plus
//!   the validated `skill_name` — no user-controlled path segment is ever
//!   accepted directly.

use std::env;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_ACTIVE_DIR: &str = "<path>/skills/active";
const DEFAULT_PROPOSED_DIR: &str = "<path>/skills/proposed";
const DEFAULT_AGENT: &str = "lumina";
const DEFAULT_LICENSE: &str = "MIT";
const DEFAULT_VERSION: &str = "1.0";

/// Validate a skill name: kebab-case only, must start with a letter or digit.
/// Prevents path traversal (`..`, `/`) and shell metacharacters from ever
/// reaching a remote command, since `skill_name` becomes a directory name.
fn validate_skill_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty() {
        return Err(ToolError::InvalidArgument("'skill_name' must not be empty".into()));
    }
    let ok = name
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "Invalid skill_name '{name}': must be kebab-case (lowercase letters, digits, hyphens only)"
        )))
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct SkillsConfig {
    /// SSH host of the fleet box — from `SKILLS_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `SKILLS_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `SKILLS_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Directory containing approved/live skills — from `SKILLS_ACTIVE_DIR`.
    pub active_dir: String,
    /// Directory containing skills pending review — from `SKILLS_PROPOSED_DIR`.
    pub proposed_dir: String,
}

impl SkillsConfig {
    pub fn from_env() -> Self {
        SkillsConfig {
            ssh_host: env::var("SKILLS_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("SKILLS_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("SKILLS_SSH_KEY_PATH").ok().filter(|s| !s.is_empty()),
            active_dir: env::var("SKILLS_ACTIVE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_ACTIVE_DIR.into()),
            proposed_dir: env::var("SKILLS_PROPOSED_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_PROPOSED_DIR.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SKILLS_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SKILLS_SSH_KEY_PATH is not set".into()))
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return stdout + exit
/// status. Mirrors `dev::ssh_cmd` / `sentinel::ssh_exec` — generic,
/// non-infra-leaking error messages.
fn ssh_exec(config: &SkillsConfig, command: &str, timeout_secs: u64) -> Result<(String, i32), ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("skills: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("skills: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("skills: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("skills: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!("skills: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution("Could not connect to the fleet server.".into()));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("skills: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("skills ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("skills: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("skills: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);

    Ok((output, exit_status))
}

/// Run a command over SSH on a blocking thread (ssh2 is synchronous).
async fn run_ssh(config: Arc<SkillsConfig>, command: String, timeout_secs: u64) -> Result<(String, i32), ToolError> {
    tokio::task::spawn_blocking(move || ssh_exec(&config, &command, timeout_secs))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
}

/// Escape embedded single quotes for safe single-quoting in a remote command.
fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

// ---------------------------------------------------------------------------
// SKILL.md frontmatter (agentskills.io format)
// ---------------------------------------------------------------------------

/// Parsed skill metadata + body, mirroring the Python `skills_read` response.
struct ParsedSkill {
    meta: Value,
    body: String,
}

/// Split a SKILL.md file into YAML frontmatter (`---\n...\n---`) and the
/// Markdown body that follows, then parse the frontmatter as YAML -> JSON.
fn parse_skill_md(raw: &str) -> Result<ParsedSkill, ToolError> {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let frontmatter = &rest[..end];
            // Body starts after the closing `---` and its trailing newline(s).
            let after = &rest[end + 4..];
            let body = after.trim_start_matches('\n').to_string();

            let meta: Value = serde_yaml::from_str(frontmatter).map_err(|e| {
                ToolError::Execution(format!("Failed to parse skill frontmatter: {e}"))
            })?;
            return Ok(ParsedSkill { meta, body });
        }
    }
    Err(ToolError::Execution(
        "Skill file is missing valid YAML frontmatter".into(),
    ))
}

/// Build a SKILL.md file (frontmatter + body) in the same shape observed
/// live from `skills_create` (`agent`, `description`, `license`, `name`,
/// `tags`, `version` keys, alphabetically ordered by serde_yaml's default
/// map serialization, then a `# {name}` heading and the procedure body).
fn build_skill_md(name: &str, description: &str, procedure: &str, agent: &str, tags: &str) -> String {
    let tag_list: Vec<&str> = tags
        .split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();

    let meta = json!({
        "agent": agent,
        "description": description,
        "license": DEFAULT_LICENSE,
        "name": name,
        "tags": tag_list,
        "version": DEFAULT_VERSION,
    });
    let frontmatter = serde_yaml::to_string(&meta).unwrap_or_default();

    format!("---\n{frontmatter}---\n\n# {name}\n\n{procedure}")
}

// ---------------------------------------------------------------------------
// Tool: skills_list
// ---------------------------------------------------------------------------

pub struct SkillsList {
    config: Arc<SkillsConfig>,
}

#[async_trait]
impl RustTool for SkillsList {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List all available agent skills with names and descriptions. Returns skills \
         from the active skills directory in agentskills.io format."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let active_dir = self.config.active_dir.clone();
        let safe_dir = escape_single_quotes(&active_dir);
        // List immediate subdirectories, then cat each SKILL.md preceded by a
        // unique marker so we can split the combined stdout back into
        // per-skill chunks without a second round trip per skill.
        let command = format!(
            "for d in '{safe_dir}'/*/; do \
                [ -f \"$d/SKILL.md\" ] && echo \"===SKILL:$(basename \"$d\")===\" && cat \"$d/SKILL.md\"; \
             done"
        );

        let (output, _status) = run_ssh(Arc::clone(&self.config), command, 30).await?;

        let mut skills = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_body = String::new();

        for line in output.split_inclusive('\n').chain(std::iter::once("")) {
            if let Some(name) = line.strip_prefix("===SKILL:").and_then(|s| s.strip_suffix("===\n")) {
                if let Some(prev_name) = current_name.take() {
                    push_skill_summary(&mut skills, &prev_name, &current_body);
                }
                current_name = Some(name.to_string());
                current_body.clear();
            } else if current_name.is_some() {
                current_body.push_str(line);
            }
        }
        if let Some(prev_name) = current_name.take() {
            push_skill_summary(&mut skills, &prev_name, &current_body);
        }

        let response = json!({
            "count": skills.len(),
            "skills": skills,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

/// Parse one skill's raw SKILL.md text and push a `{name, description}`
/// summary entry (skipping unparseable entries rather than failing the
/// whole listing).
fn push_skill_summary(skills: &mut Vec<Value>, dir_name: &str, raw: &str) {
    match parse_skill_md(raw) {
        Ok(parsed) => {
            let description = parsed
                .meta
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = parsed
                .meta
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(dir_name);
            skills.push(json!({ "name": name, "description": description }));
        }
        Err(e) => {
            warn!("skills_list: skipping unparseable skill '{dir_name}': {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: skills_read
// ---------------------------------------------------------------------------

pub struct SkillsRead {
    config: Arc<SkillsConfig>,
}

#[async_trait]
impl RustTool for SkillsRead {
    fn name(&self) -> &str {
        "skills_read"
    }

    fn description(&self) -> &str {
        "Read the full SKILL.md content for a named skill. skill_name: exact name of \
         the skill (e.g. 'morning-briefing', 'health-check', 'code-review')."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Exact name of the skill"
                }
            },
            "required": ["skill_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let skill_name = args["skill_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'skill_name' must be a string".into()))?;
        validate_skill_name(skill_name)?;

        // Check active/ first, then fall back to proposed/ — matches the
        // live server, which found a proposed-only skill via skills_read.
        for (dir, status) in [
            (self.config.active_dir.clone(), "active"),
            (self.config.proposed_dir.clone(), "proposed"),
        ] {
            let path = format!("{dir}/{skill_name}/SKILL.md");
            let safe_path = escape_single_quotes(&path);
            let (output, exit_status) = run_ssh(
                Arc::clone(&self.config),
                format!("cat '{safe_path}' 2>/dev/null"),
                30,
            )
            .await?;

            if exit_status == 0 && !output.is_empty() {
                let parsed = parse_skill_md(&output)?;
                let response = json!({
                    "name": skill_name,
                    "status": status,
                    "meta": parsed.meta,
                    "body": parsed.body.trim_end(),
                    "raw": output.trim_end(),
                });
                return serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")));
            }
        }

        let response = json!({
            "error": format!(
                "Skill '{skill_name}' not found. Use skills_list() to see available skills."
            )
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: skills_create
// ---------------------------------------------------------------------------

pub struct SkillsCreate {
    config: Arc<SkillsConfig>,
}

#[async_trait]
impl RustTool for SkillsCreate {
    fn name(&self) -> &str {
        "skills_create"
    }

    fn description(&self) -> &str {
        "Create a new skill in agentskills.io format. skill_name: directory name \
         (kebab-case, e.g. 'my-skill'). description: one-line description. procedure: \
         markdown body describing the procedure. agent: which agent owns this skill. \
         tags: comma-separated tags. proposed: if True (default), creates in proposed/ \
         for review. False creates directly in active/."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Directory name (kebab-case, e.g. 'my-skill')"
                },
                "description": {
                    "type": "string",
                    "description": "One-line description"
                },
                "procedure": {
                    "type": "string",
                    "description": "Markdown body describing the procedure"
                },
                "agent": {
                    "type": "string",
                    "description": "Which agent owns this skill",
                    "default": DEFAULT_AGENT
                },
                "tags": {
                    "type": "string",
                    "description": "Comma-separated tags",
                    "default": ""
                },
                "proposed": {
                    "type": "boolean",
                    "description": "If true (default), creates in proposed/ for review. False creates directly in active/.",
                    "default": true
                }
            },
            "required": ["skill_name", "description", "procedure"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let skill_name = args["skill_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'skill_name' must be a string".into()))?;
        let description = args["description"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'description' must be a string".into()))?;
        let procedure = args["procedure"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'procedure' must be a string".into()))?;
        let agent = args["agent"].as_str().unwrap_or(DEFAULT_AGENT);
        let tags = args["tags"].as_str().unwrap_or("");
        let proposed = args["proposed"].as_bool().unwrap_or(true);

        validate_skill_name(skill_name)?;

        let (dir, location) = if proposed {
            (self.config.proposed_dir.clone(), "proposed")
        } else {
            (self.config.active_dir.clone(), "active")
        };

        let skill_dir = format!("{dir}/{skill_name}");
        let path = format!("{skill_dir}/SKILL.md");
        let content = build_skill_md(skill_name, description, procedure, agent, tags);

        let safe_dir = escape_single_quotes(&skill_dir);
        let safe_path = escape_single_quotes(&path);

        let (_out, mkdir_status) =
            run_ssh(Arc::clone(&self.config), format!("mkdir -p '{safe_dir}'"), 30).await?;
        if mkdir_status != 0 {
            return Err(ToolError::Execution(format!(
                "Failed to create skill directory: {skill_dir}"
            )));
        }

        let cfg = Arc::clone(&self.config);
        let write_command = format!("cat > '{safe_path}'");
        let (_out, write_status) = tokio::task::spawn_blocking(move || {
            ssh_write_stdin(&cfg, &write_command, &content, 30)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        if write_status != 0 {
            return Err(ToolError::Execution(format!("Failed to write skill file: {path}")));
        }

        let response = json!({
            "status": "created",
            "skill": skill_name,
            "location": location,
            "path": path,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

/// Run a command over SSH while writing `input` to the remote process's
/// stdin, then signal EOF. Mirrors `dev::ssh_cmd_with_input`.
fn ssh_write_stdin(
    config: &SkillsConfig,
    command: &str,
    input: &str,
    timeout_secs: u64,
) -> Result<(String, i32), ToolError> {
    use std::io::Write as IoWrite;

    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr)
        .map_err(|_| ToolError::Execution("The fleet server is unreachable.".into()))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new()
        .map_err(|_| ToolError::Execution("Could not complete the operation on the fleet server.".into()))?;
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|_| ToolError::Execution("The fleet server is unreachable.".into()))?;
    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|_| ToolError::Execution("Could not connect to the fleet server.".into()))?;
    if !sess.authenticated() {
        return Err(ToolError::Execution("Could not connect to the fleet server.".into()));
    }

    let mut channel = sess
        .channel_session()
        .map_err(|_| ToolError::Execution("Could not complete the operation on the fleet server.".into()))?;

    debug!("skills ssh_write_stdin: {command}");
    channel
        .exec(command)
        .map_err(|_| ToolError::Execution("Could not complete the operation on the fleet server.".into()))?;
    channel
        .write_all(input.as_bytes())
        .map_err(|_| ToolError::Execution("Could not complete the operation on the fleet server.".into()))?;
    channel
        .send_eof()
        .map_err(|_| ToolError::Execution("Could not complete the operation on the fleet server.".into()))?;

    let mut output = String::new();
    let _ = channel.read_to_string(&mut output);
    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);

    Ok((output, exit_status))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Skills tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(SkillsConfig::from_env());

    let _ = registry.register(Box::new(SkillsList { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SkillsRead { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SkillsCreate { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<SkillsConfig> {
        Arc::new(SkillsConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            active_dir: DEFAULT_ACTIVE_DIR.into(),
            proposed_dir: DEFAULT_PROPOSED_DIR.into(),
        })
    }

    // --- validate_skill_name ---------------------------------------------

    #[test]
    fn test_validate_skill_name_accepts_kebab_case() {
        assert!(validate_skill_name("my-skill").is_ok());
        assert!(validate_skill_name("morning-briefing").is_ok());
        assert!(validate_skill_name("health-check2").is_ok());
        assert!(validate_skill_name("a").is_ok());
    }

    #[test]
    fn test_validate_skill_name_rejects_empty() {
        assert!(validate_skill_name("").is_err());
    }

    #[test]
    fn test_validate_skill_name_rejects_path_traversal() {
        assert!(validate_skill_name("../etc/passwd").is_err());
        assert!(validate_skill_name("foo/bar").is_err());
        assert!(validate_skill_name("..").is_err());
    }

    #[test]
    fn test_validate_skill_name_rejects_shell_metacharacters() {
        assert!(validate_skill_name("skill; rm -rf /").is_err());
        assert!(validate_skill_name("$(whoami)").is_err());
        assert!(validate_skill_name("skill && evil").is_err());
        assert!(validate_skill_name("skill'name").is_err());
    }

    #[test]
    fn test_validate_skill_name_rejects_uppercase_and_leading_hyphen() {
        assert!(validate_skill_name("MySkill").is_err());
        assert!(validate_skill_name("-leading-hyphen").is_err());
    }

    // --- parse_skill_md ----------------------------------------------------

    #[test]
    fn test_parse_skill_md_roundtrip() {
        let raw = "---\nagent: lumina\ndescription: A test skill\nlicense: MIT\nname: my-skill\ntags:\n- test\nversion: '1.0'\n---\n\n# my-skill\n\nDo the thing.";
        let parsed = parse_skill_md(raw).unwrap();
        assert_eq!(parsed.meta["agent"], "lumina");
        assert_eq!(parsed.meta["description"], "A test skill");
        assert!(parsed.body.contains("Do the thing."));
    }

    #[test]
    fn test_parse_skill_md_rejects_missing_frontmatter() {
        let raw = "# no frontmatter here\n\njust a body";
        assert!(parse_skill_md(raw).is_err());
    }

    // --- build_skill_md -----------------------------------------------------

    #[test]
    fn test_build_skill_md_contains_expected_fields() {
        let content = build_skill_md("my-skill", "desc here", "1. step one", "lumina", "a,b, c");
        assert!(content.starts_with("---\n"));
        assert!(content.contains("agent: lumina"));
        assert!(content.contains("description: desc here"));
        assert!(content.contains("license: MIT"));
        assert!(content.contains("name: my-skill"));
        assert!(content.contains("# my-skill"));
        assert!(content.contains("1. step one"));
        // Round-trips through our own parser.
        let parsed = parse_skill_md(&content).unwrap();
        let tags: Vec<String> = parsed.meta["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_build_skill_md_empty_tags_produces_empty_list() {
        let content = build_skill_md("x", "d", "p", "lumina", "");
        let parsed = parse_skill_md(&content).unwrap();
        assert!(parsed.meta["tags"].as_array().unwrap().is_empty());
    }

    // --- tool metadata -------------------------------------------------------

    #[test]
    fn test_skills_list_metadata() {
        let tool = SkillsList { config: test_config() };
        assert_eq!(tool.name(), "skills_list");
    }

    #[test]
    fn test_skills_read_metadata() {
        let tool = SkillsRead { config: test_config() };
        assert_eq!(tool.name(), "skills_read");
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "skill_name"));
    }

    #[test]
    fn test_skills_create_metadata() {
        let tool = SkillsCreate { config: test_config() };
        assert_eq!(tool.name(), "skills_create");
        let params = tool.parameters();
        let required: Vec<&str> = params["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["skill_name", "description", "procedure"]);
    }

    // --- execute: invalid arguments (no network needed) ----------------------

    #[tokio::test]
    async fn test_skills_read_missing_name_rejected() {
        let tool = SkillsRead { config: test_config() };
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_skills_read_rejects_bad_name() {
        let tool = SkillsRead { config: test_config() };
        let err = tool
            .execute(json!({"skill_name": "../../etc/passwd"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_skills_create_missing_fields_rejected() {
        let tool = SkillsCreate { config: test_config() };
        let err = tool
            .execute(json!({"skill_name": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_skills_create_rejects_bad_name() {
        let tool = SkillsCreate { config: test_config() };
        let err = tool
            .execute(json!({
                "skill_name": "bad/name",
                "description": "d",
                "procedure": "p"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_skills_list_not_configured_without_host() {
        let tool = SkillsList { config: test_config() };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SKILLS_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_skills_create_not_configured_without_host() {
        let tool = SkillsCreate { config: test_config() };
        let err = tool
            .execute(json!({
                "skill_name": "my-skill",
                "description": "d",
                "procedure": "p"
            }))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SKILLS_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- registration ----------------------------------------------------

    #[test]
    fn test_register_adds_three_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
        assert!(registry.contains("skills_list"));
        assert!(registry.contains("skills_read"));
        assert!(registry.contains("skills_create"));
    }
}
