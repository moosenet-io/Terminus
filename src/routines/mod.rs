//! Scheduler routines tools — ported from the orchestrator host's Python `routines_tools.py`.
//!
//! The external scheduler service (running on the orchestrator host) owns a set of named, cron-like
//! "routines" (scheduled prompts). These tools let an agent list/inspect them
//! and — for anything that mutates a live routine — go through a propose/
//! approve workflow before anything actually changes.
//!
//! ## Tools (identical names + params to the Python source, verified live
//! against the scheduler host's MCP endpoint on 2026-07-06 via `tools/list` + `tools/call`) // pii-test-fixture
//! - `routines_list()` — list all routines (schedule, status, next fire, run count).
//! - `routines_history(name, limit=10)` — recent run history for one routine.
//! - `routines_propose(name, schedule, timezone, description, prompt, action="create")`
//!   — stage a proposed create/update/delete. Does NOT execute anything; just
//!   saves the proposal and (on the Python side) notifies the operator.
//! - `routines_pending()` — read-only: is there a staged proposal, and what is it.
//! - `routines_approve()` — execute the currently staged proposal.
//! - `routines_edit(name, schedule="", prompt="", description="", timezone="", cooldown="")`
//!   — edit an existing routine in place (only provided fields change).
//! - `routines_batch_edit_notify_channel(channel="gateway")` — delete+recreate
//!   EVERY routine to switch its notify-channel. Highest-risk tool in this set:
//!   it wipes run history for every routine.
//!
//! ## What was verified live vs. inferred
//! The exact tool names, descriptions, and JSON Schemas below were pulled
//! directly from the scheduler host's live MCP endpoint (the legacy tool set, v1.26.0) via
//! `initialize` + `tools/list`. `routines_pending`'s exact response shape
//! (`{"pending": bool, "proposal": {...}}`) was also verified live — that host had
//! a stale test proposal sitting in its staging file at the time of the port:
//! `{"action":"create","name":"test","schedule":"test","timezone":"test",
//!   "description":"test","prompt":"test","status":"pending_approval"}`.
//! `routines_list`/`routines_history` were exercised live too; both failed with
//! an SSH "no route to host" error because the scheduler's host is
//! unreachable from this build environment — this at least confirms both are
//! SSH-backed calls to that host, and gave the error shape to mirror (a JSON
//! object with an `"error"` key, plus pass-through identifying fields).
//!
//! This environment had no filesystem access to that host itself (SSH to it timed
//! out), so the exact remote command syntax the scheduler's CLI expects could not be
//! read from source. The remote commands below are built on the configured scheduler
//! CLI binary, documented as living at a fixed path on the scheduler
//! host — a reasonable, clearly-flagged inference, not a verified fact. The
//! operator should confirm exact remote command syntax during the 24h audit
//! this port is subject to; everything else (schemas, staging shape, gating)
//! is faithful to what was actually observed or explicitly specified.
//!
//! ## Security model — TWO deliberately different staging designs stacked
//! 1. **Propose/pending staging** (single-slot, file-backed) is a faithful port
//!    of that other system's own mechanism — its own tool description literally says
//!    "Reads the proposal from the staging file". `routines_propose` writes one
//!    JSON proposal to `ROUTINES_STAGING_PATH` (replacing any prior one);
//!    `routines_pending` reads it back read-only; neither is gated, matching
//!    the Python source (proposing a change and checking on it are not
//!    dangerous — only executing one is).
//! 2. **Execution gating** for `routines_approve`, `routines_edit`, and
//!    `routines_batch_edit_notify_channel` reuses this codebase's existing
//!    human-approval gate (`crate::approval::gate`), the SAME mechanism
//!    `ansible` and `openhands` already use for other guarded tools. This was
//!    judged a better fit than inventing a second bespoke staging/approval
//!    file: it gives routines the same Postgres-backed, single-use,
//!    expiring-code semantics as every other guarded tool in this codebase,
//!    it's the pattern the task explicitly asked to prefer if it fits, and it
//!    means the operator's `approve <CODE>` / `deny <CODE>` Matrix commands
//!    work identically here as everywhere else. It does NOT replace staging
//!    (needed to know *what* to execute) — it replaces "trust that whoever
//!    calls `routines_approve` has genuinely gotten sign-off" with a real,
//!    code-based, single-use grant that the model cannot forge or self-issue.
//!    `routines_batch_edit_notify_channel` — the single most destructive tool
//!    here — goes through the exact same gate, keyed on its own arguments.
//!
//! ### Findings from the required two-reviewer pass, and how they were fixed
//! - **Approval content-binding.** An adversarial review found that
//!   `crate::approval::gate` originally scoped a grant to `(code, tool_name)`
//!   only, not to the actual args — so a code approved for one staged
//!   proposal (or one set of `routines_edit`/`routines_batch_edit_notify_channel`
//!   arguments) could be redeemed against *different* args for the same tool,
//!   e.g. if the single-slot staging file was overwritten by a second
//!   `routines_propose` between approval and redemption. Fixed in
//!   `crate::approval` itself (not just here) by binding the grant to a
//!   content hash: consumption now requires the current args (approval code
//!   stripped) to match the args that were pending when the human approved.
//!   This benefits every guarded tool, not only routines.
//! - **Shell-injection via free-text fields.** A correctness review found
//!   that `schedule`/`timezone`/`description`/`prompt`/`cooldown` were
//!   interpolated into the remote SSH command via Rust's `{:?}` (Debug)
//!   formatting, which only escapes `"`/`\` — not `$(...)`, backticks, or a
//!   bare `$VAR` — while `ssh2::Channel::exec` hands the string straight to a
//!   remote shell. `prompt`/`description` are LLM-authored free text, exactly
//!   where this would bite. Fixed by routing every free-text field through
//!   [`shell_quote`] (POSIX single-quoting), while `name`/`channel` continue
//!   to go through the stricter [`is_safe_identifier`] allowlist.

use std::env;
use std::fs;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{error, warn};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// RoutinesConfig
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables — no hardcoded
/// hosts, users, keys, or paths.
#[derive(Debug, Clone)]
pub struct RoutinesConfig {
    /// SSH host of the external scheduler — from `ROUTINES_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `ROUTINES_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `ROUTINES_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Name of the remote scheduler CLI binary — from `ROUTINES_CLI` (defaults to the scheduler's standard binary name).
    pub cli: String,
    /// Path to the local single-slot staging file — from `ROUTINES_STAGING_PATH`,
    /// default "/var/lib/terminus/routines_staging.json".
    pub staging_path: PathBuf,
}

impl RoutinesConfig {
    pub fn from_env() -> Self {
        RoutinesConfig {
            ssh_host: env::var("ROUTINES_SSH_HOST").ok(),
            ssh_user: env::var("ROUTINES_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("ROUTINES_SSH_KEY_PATH").ok(),
            // PII remediation note (2026-07): "<host>" is a real, functional
            // default CLI binary name — left unchanged rather than
            // guess-redacted; flagged for operator review before any public
            // release.
            cli: env::var("ROUTINES_CLI").unwrap_or_else(|_| "<host>".into()),
            staging_path: env::var("ROUTINES_STAGING_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/var/lib/terminus/routines_staging.json")),
        }
    }
}

// ---------------------------------------------------------------------------
// Staging (single-slot, file-backed — mirrors that other system's own "staging file")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutineProposal {
    pub action: String,
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub description: String,
    pub prompt: String,
    pub status: String,
}

fn read_staged(path: &PathBuf) -> Option<RoutineProposal> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_staged(path: &PathBuf, proposal: &RoutineProposal) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let raw = serde_json::to_string_pretty(proposal)
        .map_err(|e| ToolError::Execution(format!("serialize proposal: {e}")))?;
    fs::write(path, raw).map_err(|e| ToolError::Execution(format!("write staging file: {e}")))
}

fn clear_staged(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// Allowed characters for identifiers we interpolate into a remote command
/// string (routine names, notify channels). Deliberately conservative —
/// alnum, dash, underscore, dot only. No shell metacharacters can appear.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// POSIX single-quote a string for safe interpolation into a remote shell
/// command line. `ssh2::Channel::exec` hands the command string straight to
/// the remote shell, so free-text fields (routine `prompt`, `description`,
/// `schedule`, `timezone`, `cooldown` — LLM-authored text, not restricted to
/// an identifier charset) must NEVER be interpolated via `Debug` (`{:?}`)
/// formatting: Rust's debug-quoting only escapes `"`/`\`, it does not
/// neutralize `$(...)`, backticks, or a bare `$VAR`, all of which a POSIX
/// shell still expands even inside double quotes.
///
/// Single-quoting is unconditionally safe in POSIX shells — nothing inside
/// `'...'` is expanded — with one exception (an embedded `'`), handled here
/// by closing the quote, emitting an escaped literal quote, and reopening:
/// `it's` -> `'it'\''s'`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

struct SshResult {
    returncode: i32,
    stdout: String,
    stderr: String,
}

/// Open an SSH session, run a single command, and return its stdout/stderr/exit.
///
/// `command` must be built by the caller from validated/fixed inputs only —
/// no raw, unvalidated user-supplied text reaches this function.
fn ssh_exec(config: &RoutinesConfig, command: &str, timeout_secs: u64) -> Result<SshResult, ToolError> {
    let host = config
        .ssh_host
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("ROUTINES_SSH_HOST is not set".into()))?;
    let key_path = config
        .ssh_key_path
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("ROUTINES_SSH_KEY_PATH is not set".into()))?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr)
        .map_err(|e| ToolError::Execution(format!("Cannot reach scheduler host {host}: {e}")))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| ToolError::Execution(e.to_string()))?;
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|e| ToolError::Execution(format!("SSH handshake failed with {host}: {e}")))?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| ToolError::Execution(format!("SSH auth failed: {e}")))?;
    if !sess.authenticated() {
        return Err(ToolError::Execution(format!(
            "SSH authentication failed for {}@{host}",
            config.ssh_user
        )));
    }

    let mut channel = sess.channel_session().map_err(|e| ToolError::Execution(e.to_string()))?;
    channel
        .exec(command)
        .map_err(|e| ToolError::Execution(format!("SSH exec failed: {e}")))?;

    let mut stdout = String::new();
    channel
        .read_to_string(&mut stdout)
        .map_err(|e| ToolError::Execution(format!("SSH read failed: {e}")))?;
    let mut stderr = String::new();
    channel
        .stderr()
        .read_to_string(&mut stderr)
        .map_err(|e| ToolError::Execution(format!("SSH stderr read failed: {e}")))?;
    channel.wait_close().ok();
    let returncode = channel.exit_status().unwrap_or(-1);
    if returncode != 0 {
        warn!("routines ssh_exec exit status {returncode} for: {command}");
    }

    Ok(SshResult {
        returncode,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
    })
}

/// Run a fixed-shape remote command, returning either its parsed JSON stdout
/// or a `{"error": ...}` object on any failure — mirroring the shape observed
/// live from that other host (`{"error": "ssh: connect to host ... No route to host"}`).
async fn run_remote(config: &RoutinesConfig, command: String, timeout_secs: u64) -> Value {
    let cfg = config.clone();
    let cmd = command.clone();
    let joined = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &cmd, timeout_secs)).await;

    let result = match joined {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return json!({ "error": e.to_string() }),
        Err(e) => return json!({ "error": format!("Task join error: {e}") }),
    };

    if result.returncode != 0 {
        return json!({ "error": if result.stderr.is_empty() { result.stdout } else { result.stderr } });
    }

    match serde_json::from_str::<Value>(&result.stdout) {
        Ok(v) => v,
        Err(_) => json!({ "raw": result.stdout }),
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_list
// ---------------------------------------------------------------------------

pub struct RoutinesList {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesList {
    fn name(&self) -> &str {
        "routines_list"
    }

    fn description(&self) -> &str {
        "List all scheduler routines with their schedule, status, next fire time, and run count."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let command = format!("{} routines list --json", self.config.cli);
        let result = run_remote(&self.config, command, 30).await;
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_history
// ---------------------------------------------------------------------------

pub struct RoutinesHistory {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesHistory {
    fn name(&self) -> &str {
        "routines_history"
    }

    fn description(&self) -> &str {
        "Show recent run history for a specific routine by name."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "limit": { "type": "integer", "default": 10 }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'name' must be a string".into()))?
            .to_string();
        let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 1000);

        if !is_safe_identifier(&name) {
            return Ok(json!({
                "error": "Invalid routine name: only letters, digits, '-', '_', '.' are allowed",
                "name": name,
            })
            .to_string());
        }

        let command = format!(
            "{} routines history {} --limit {} --json",
            self.config.cli, name, limit
        );
        let mut result = run_remote(&self.config, command, 30).await;
        // Match the Python source's observed behaviour: error responses carry
        // the requested name back alongside the error.
        if let Some(obj) = result.as_object_mut() {
            if obj.contains_key("error") {
                obj.insert("name".into(), json!(name));
            }
        }
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_propose
// ---------------------------------------------------------------------------

pub struct RoutinesPropose {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesPropose {
    fn name(&self) -> &str {
        "routines_propose"
    }

    fn description(&self) -> &str {
        "Propose a routine change for <operator>'s approval. Action: create, update, delete.\nThis does NOT execute the change — it saves a proposal and notifies <operator>.\nPeter must then approve before routines_approve is called."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "schedule": { "type": "string" },
                "timezone": { "type": "string" },
                "description": { "type": "string" },
                "prompt": { "type": "string" },
                "action": { "type": "string", "default": "create" }
            },
            "required": ["name", "schedule", "timezone", "description", "prompt"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let get = |k: &str| -> Result<String, ToolError> {
            args[k]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidArgument(format!("'{k}' must be a string")))
        };
        let proposal = RoutineProposal {
            action: args["action"].as_str().unwrap_or("create").to_string(),
            name: get("name")?,
            schedule: get("schedule")?,
            timezone: get("timezone")?,
            description: get("description")?,
            prompt: get("prompt")?,
            status: "pending_approval".to_string(),
        };

        write_staged(&self.config.staging_path, &proposal)?;

        Ok(json!({
            "saved": true,
            "message": "Proposal saved. <operator> must approve before routines_approve is called.",
            "proposal": proposal,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_pending
// ---------------------------------------------------------------------------

pub struct RoutinesPending {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesPending {
    fn name(&self) -> &str {
        "routines_pending"
    }

    fn description(&self) -> &str {
        "Check if there is a pending routine proposal awaiting approval."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        match read_staged(&self.config.staging_path) {
            Some(p) => Ok(json!({ "pending": true, "proposal": p }).to_string()),
            None => Ok(json!({ "pending": false }).to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_approve
// ---------------------------------------------------------------------------

pub struct RoutinesApprove {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesApprove {
    fn name(&self) -> &str {
        "routines_approve"
    }

    fn description(&self) -> &str {
        "Execute the pending routine proposal. Only call this after <operator> explicitly approves.\nReads the proposal from the staging file and executes via a script on the scheduler host.\nGUARDED: requires a genuine prior operator approval grant (crate::approval)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let proposal = match read_staged(&self.config.staging_path) {
            Some(p) => p,
            None => {
                return Ok(json!({
                    "executed": false,
                    "error": "No pending routine proposal to approve.",
                })
                .to_string())
            }
        };

        // --- APPROVAL GATE (must run before any real work) — keyed on the
        // staged proposal itself, so the granted code is specific to *this*
        // proposal, not a blank check for "any approve call".
        let summary = format!(
            "{} routine '{}' (schedule='{}', tz='{}') via the scheduler",
            proposal.action, proposal.name, proposal.schedule, proposal.timezone
        );
        let gate_args = json!({ "_approval_code": args.get("_approval_code"), "proposal": &proposal });
        match gate(self.name(), &gate_args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if !is_safe_identifier(&proposal.name) {
            return Ok(json!({
                "executed": false,
                "error": "Staged proposal has an invalid routine name; refusing to execute.",
            })
            .to_string());
        }

        let command = match proposal.action.as_str() {
            "create" => format!(
                "{} routines create {} --schedule {} --timezone {} --description {} --prompt {} --json",
                self.config.cli,
                proposal.name,
                shell_quote(&proposal.schedule),
                shell_quote(&proposal.timezone),
                shell_quote(&proposal.description),
                shell_quote(&proposal.prompt),
            ),
            "update" => format!(
                "{} routines update {} --schedule {} --timezone {} --description {} --prompt {} --json",
                self.config.cli,
                proposal.name,
                shell_quote(&proposal.schedule),
                shell_quote(&proposal.timezone),
                shell_quote(&proposal.description),
                shell_quote(&proposal.prompt),
            ),
            "delete" => format!("{} routines delete {} --json", self.config.cli, proposal.name),
            other => {
                return Ok(json!({
                    "executed": false,
                    "error": format!("Unknown proposal action '{other}'"),
                })
                .to_string())
            }
        };

        let result = run_remote(&self.config, command, 60).await;
        let ok = result.get("error").is_none();
        if ok {
            clear_staged(&self.config.staging_path);
        }
        Ok(json!({ "executed": ok, "result": result }).to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_edit
// ---------------------------------------------------------------------------

pub struct RoutinesEdit {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesEdit {
    fn name(&self) -> &str {
        "routines_edit"
    }

    fn description(&self) -> &str {
        "Edit an existing routine's properties without deleting it.\nOnly provided fields are updated. Preserves run history.\nThis tool is gated — only call after <operator> explicitly approves the change."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "schedule": { "type": "string", "default": "" },
                "prompt": { "type": "string", "default": "" },
                "description": { "type": "string", "default": "" },
                "timezone": { "type": "string", "default": "" },
                "cooldown": { "type": "string", "default": "" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'name' must be a string".into()))?
            .to_string();

        // --- APPROVAL GATE ---
        let summary = format!("Edit routine '{name}' on the scheduler");
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if !is_safe_identifier(&name) {
            return Ok(json!({
                "error": "Invalid routine name: only letters, digits, '-', '_', '.' are allowed",
            })
            .to_string());
        }

        let mut cmd = format!("{} routines edit {}", self.config.cli, name);
        for field in ["schedule", "prompt", "description", "timezone", "cooldown"] {
            if let Some(v) = args[field].as_str() {
                if !v.is_empty() {
                    cmd.push_str(&format!(" --{field} {}", shell_quote(v)));
                }
            }
        }
        cmd.push_str(" --json");

        let result = run_remote(&self.config, cmd, 30).await;
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: routines_batch_edit_notify_channel
// ---------------------------------------------------------------------------

pub struct RoutinesBatchEditNotifyChannel {
    config: Arc<RoutinesConfig>,
}

#[async_trait]
impl RustTool for RoutinesBatchEditNotifyChannel {
    fn name(&self) -> &str {
        "routines_batch_edit_notify_channel"
    }

    fn description(&self) -> &str {
        "Batch update the notify-channel on ALL routines by deleting and recreating each one.\nWARNING: This wipes run history. Use only when switching all routines to a new channel.\nThe scheduler's edit command does not support changing notify-channel, so delete/recreate is required.\nThis tool is gated — only call after <operator> explicitly approves."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "channel": { "type": "string", "default": "gateway" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let channel = args["channel"].as_str().unwrap_or("gateway").to_string();

        // --- APPROVAL GATE — the single highest-risk tool in this module.
        // No SSH call, no destructive action of any kind happens above this
        // line; `Gate::Granted` is the only path that reaches the code below.
        let summary = format!(
            "DESTRUCTIVE: delete+recreate EVERY routine on the scheduler to switch notify-channel to '{channel}' (wipes all run history)"
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if !is_safe_identifier(&channel) {
            return Ok(json!({
                "error": "Invalid channel: only letters, digits, '-', '_', '.' are allowed",
            })
            .to_string());
        }

        let command = format!(
            "{} routines batch-edit-notify-channel --channel {} --json",
            self.config.cli, channel
        );
        let result = run_remote(&self.config, command, 120).await;
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(RoutinesConfig::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(RoutinesList { config: Arc::clone(&config) }),
        Box::new(RoutinesHistory { config: Arc::clone(&config) }),
        Box::new(RoutinesPropose { config: Arc::clone(&config) }),
        Box::new(RoutinesPending { config: Arc::clone(&config) }),
        Box::new(RoutinesApprove { config: Arc::clone(&config) }),
        Box::new(RoutinesEdit { config: Arc::clone(&config) }),
        Box::new(RoutinesBatchEditNotifyChannel { config: Arc::clone(&config) }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            error!("routines: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no network / no SSH — arg validation, staging, gate, registration)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn test_config(staging_path: PathBuf) -> Arc<RoutinesConfig> {
        Arc::new(RoutinesConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            cli: "<host>".into(), // pii-test-fixture
            staging_path,
        })
    }

    fn clear_db_url() {
        std::env::remove_var("DATABASE_URL");
    }

    fn sample_proposal() -> RoutineProposal {
        RoutineProposal {
            action: "create".into(),
            name: "morning-briefing".into(),
            schedule: "0 7 * * *".into(),
            timezone: "America/New_York".into(),
            description: "test".into(),
            prompt: "test".into(),
            status: "pending_approval".into(),
        }
    }

    // ------------------------------------------------------------------
    // register() adds exactly 7 tools, all expected names present
    // ------------------------------------------------------------------
    #[test]
    fn test_register_adds_seven_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 7, "routines must register exactly 7 tools");
        for name in &[
            "routines_list",
            "routines_history",
            "routines_propose",
            "routines_pending",
            "routines_approve",
            "routines_edit",
            "routines_batch_edit_notify_channel",
        ] {
            assert!(registry.contains(name), "registry should contain '{name}'");
        }
    }

    // ------------------------------------------------------------------
    // is_safe_identifier
    // ------------------------------------------------------------------
    #[test]
    fn test_is_safe_identifier() {
        assert!(is_safe_identifier("morning-briefing"));
        assert!(is_safe_identifier("routine_1.v2"));
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("foo; rm -rf /"));
        assert!(!is_safe_identifier("foo bar"));
        assert!(!is_safe_identifier("$(whoami)"));
        assert!(!is_safe_identifier(&"a".repeat(201)));
    }

    // ------------------------------------------------------------------
    // shell_quote — free-text fields must never let a remote shell expand
    // command substitution / variables, unlike `{:?}` (Debug) formatting.
    // ------------------------------------------------------------------
    #[test]
    fn test_shell_quote_wraps_in_single_quotes() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_neutralizes_command_substitution() {
        // A POSIX shell never expands anything inside single quotes.
        let quoted = shell_quote("$(rm -rf /) `whoami` $HOME");
        assert_eq!(quoted, "'$(rm -rf /) `whoami` $HOME'");
        // Sanity: Debug formatting (the old, unsafe approach) would NOT have
        // neutralized this — it leaves `$(...)` untouched.
        let debug_quoted = format!("{:?}", "$(rm -rf /) `whoami` $HOME");
        assert!(debug_quoted.contains("$(rm -rf /)"), "debug-format leaves shell metachars live");
    }

    #[test]
    fn test_shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_quote("it's a test"), "'it'\\''s a test'");
    }

    #[test]
    fn test_shell_quote_empty_string() {
        assert_eq!(shell_quote(""), "''");
    }

    // ------------------------------------------------------------------
    // routines_history rejects unsafe names before any SSH attempt
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_history_rejects_unsafe_name() {
        let dir = tempdir().unwrap();
        let tool = RoutinesHistory { config: test_config(dir.path().join("staging.json")) };
        let result = tool
            .execute(json!({ "name": "foo; rm -rf /" }))
            .await
            .unwrap();
        assert!(result.contains("Invalid routine name"));
    }

    #[tokio::test]
    async fn test_history_missing_name_errors() {
        let dir = tempdir().unwrap();
        let tool = RoutinesHistory { config: test_config(dir.path().join("staging.json")) };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // ------------------------------------------------------------------
    // propose/pending round-trip via the staging file (not gated)
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_propose_then_pending_round_trip() {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path().join("staging.json"));

        let pending_before = RoutinesPending { config: Arc::clone(&cfg) };
        let before = pending_before.execute(json!({})).await.unwrap();
        assert_eq!(before, json!({"pending": false}).to_string());

        let propose = RoutinesPropose { config: Arc::clone(&cfg) };
        let propose_result = propose
            .execute(json!({
                "name": "morning-briefing",
                "schedule": "0 7 * * *",
                "timezone": "America/New_York",
                "description": "test",
                "prompt": "test",
            }))
            .await
            .unwrap();
        assert!(propose_result.contains("\"saved\":true"));

        let pending_after = RoutinesPending { config: Arc::clone(&cfg) };
        let after: Value = serde_json::from_str(&pending_after.execute(json!({})).await.unwrap()).unwrap();
        assert_eq!(after["pending"], true);
        assert_eq!(after["proposal"]["name"], "morning-briefing");
        assert_eq!(after["proposal"]["status"], "pending_approval");
    }

    #[tokio::test]
    async fn test_propose_missing_field_errors() {
        let dir = tempdir().unwrap();
        let tool = RoutinesPropose { config: test_config(dir.path().join("staging.json")) };
        let result = tool.execute(json!({ "name": "x" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // ------------------------------------------------------------------
    // routines_approve: no pending proposal — refuses without even
    // reaching the gate (nothing to approve, nothing to gate).
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_approve_no_pending_proposal() {
        let dir = tempdir().unwrap();
        let tool = RoutinesApprove { config: test_config(dir.path().join("staging.json")) };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("No pending routine proposal"));
    }

    // ------------------------------------------------------------------
    // GATE — the three mutating tools all refuse to execute without a
    // valid approval grant, and this is enforced BEFORE any staging file
    // mutation / SSH attempt. This is the load-bearing test set: it proves
    // routines_approve / routines_edit / routines_batch_edit_notify_channel
    // cannot be triggered by a bare call with no genuine prior approval.
    // ------------------------------------------------------------------
    #[tokio::test]
    #[serial]
    async fn test_approve_gate_denies_without_db_and_does_not_clear_staging() {
        clear_db_url();
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path().join("staging.json"));
        write_staged(&cfg.staging_path, &sample_proposal()).unwrap();

        let tool = RoutinesApprove { config: Arc::clone(&cfg) };
        let msg = tool.execute(json!({})).await.unwrap();
        assert!(
            msg.contains("unavailable") || msg.contains("DATABASE_URL") || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
        // The proposal must still be staged: no execution happened, and the
        // gate must have run before any staging-file mutation.
        assert!(read_staged(&cfg.staging_path).is_some(), "staging file was cleared without a granted approval");
    }

    #[tokio::test]
    #[serial]
    async fn test_edit_gate_denies_without_db() {
        clear_db_url();
        let dir = tempdir().unwrap();
        let tool = RoutinesEdit { config: test_config(dir.path().join("staging.json")) };
        let msg = tool
            .execute(json!({ "name": "morning-briefing", "schedule": "0 8 * * *" }))
            .await
            .unwrap();
        assert!(
            msg.contains("unavailable") || msg.contains("DATABASE_URL") || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_batch_edit_notify_channel_gate_denies_without_db() {
        clear_db_url();
        let dir = tempdir().unwrap();
        let tool = RoutinesBatchEditNotifyChannel { config: test_config(dir.path().join("staging.json")) };
        let msg = tool.execute(json!({ "channel": "matrix" })).await.unwrap();
        assert!(
            msg.contains("unavailable") || msg.contains("DATABASE_URL") || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
        // Must not leak into a "destructive action succeeded" shape.
        assert!(!msg.contains("\"executed\":true"));
    }

    // ------------------------------------------------------------------
    // routines_batch_edit_notify_channel rejects unsafe channel names
    // (defense in depth — checked AFTER the gate, but before any SSH call,
    // and the gate test above already proves it can't be reached without
    // DATABASE_URL/approval in the first place).
    // ------------------------------------------------------------------
    #[test]
    fn test_batch_edit_channel_validation() {
        assert!(is_safe_identifier("matrix"));
        assert!(is_safe_identifier("gateway"));
        assert!(!is_safe_identifier("matrix; rm -rf /"));
    }

    // ------------------------------------------------------------------
    // Parameter schemas are well-formed and match the live schemas observed from that other system
    // ------------------------------------------------------------------
    #[test]
    fn test_propose_schema_matches_live_source() {
        let dir = tempdir().unwrap();
        let tool = RoutinesPropose { config: test_config(dir.path().join("staging.json")) };
        let params = tool.parameters();
        let required: Vec<&str> = params["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["name", "schedule", "timezone", "description", "prompt"]);
    }

    #[test]
    fn test_edit_schema_only_name_required() {
        let dir = tempdir().unwrap();
        let tool = RoutinesEdit { config: test_config(dir.path().join("staging.json")) };
        let params = tool.parameters();
        assert_eq!(params["required"], json!(["name"]));
    }

    #[test]
    fn test_history_schema() {
        let dir = tempdir().unwrap();
        let tool = RoutinesHistory { config: test_config(dir.path().join("staging.json")) };
        let params = tool.parameters();
        assert_eq!(params["required"], json!(["name"]));
        assert_eq!(params["properties"]["limit"]["default"], 10);
    }

    #[test]
    fn test_batch_edit_channel_default() {
        let dir = tempdir().unwrap();
        let tool = RoutinesBatchEditNotifyChannel { config: test_config(dir.path().join("staging.json")) };
        let params = tool.parameters();
        assert_eq!(params["properties"]["channel"]["default"], "gateway");
    }

    // ------------------------------------------------------------------
    // list/history without ROUTINES_SSH_HOST returns a graceful error
    // object (not a panic / not an Err) — matches the JSON-error shape
    // observed live from that other system when its own SSH target was unreachable.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_list_without_ssh_host_returns_error_object() {
        let dir = tempdir().unwrap();
        let tool = RoutinesList { config: test_config(dir.path().join("staging.json")) };
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("error").is_some());
    }
}
