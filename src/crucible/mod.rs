//! Crucible tools — learning-tracker system, ported from the Python
//! `crucible_tools.py` on the legacy Terminus host (the legacy Python MCP host, streamable-HTTP MCP).
//!
//! ## Verified transport (IMPORTANT — read before touching this module)
//!
//! Before writing this port, every `crucible_*` tool was called live against
//! the legacy Terminus host's MCP endpoint. The fleet host (where the Engram
//! knowledge-store lives per the fleet layout) was down at porting time, so
//! every call failed identically:
//!
//! ```text
//! {"error": "ssh: connect to host <fleet-host> port 22: No route to host"}
//! ```
//!
//! That failure signature is decisive: **Crucible does not talk to Engram over
//! HTTP.** It SSHes into the fleet host and runs a script there, exactly like
//! the already-ported `sentinel` and `vigil` modules in this crate (both also
//! fleet-host-hosted dashboard-style tools, both SSH-exec, neither HTTP). No
//! `EngramClient`/HTTP client exists anywhere in this repo (grepped
//! case-insensitively for "engram" — the only hits are unrelated ASMT/S84
//! corpora and doc files), which is consistent with there being no HTTP
//! Engram API to build a client for.
//!
//! This port therefore mirrors `sentinel`/`vigil`'s SSH-exec pattern, not an
//! HTTP REST client. **This is a deliberate deviation from the assumption
//! that Crucible/Engram is HTTP-based** — that assumption does not survive
//! contact with the live server's actual error behavior.
//!
//! ## What is verified vs. assumed
//!
//! Verified directly against the live server:
//! - All 10 tool names, descriptions, and `inputSchema`s (via `tools/list`).
//! - The transport is SSH to the fleet host on port 22 (via the identical
//!   error signature above, reproduced for every one of the 10 tools).
//! - The general shape of failure responses: a bare `{"error": "<message>"}`
//!   JSON object as the sole tool-call content (not wrapped in extra fields
//!   the way `sentinel_run` wraps its output in `{"status", "operation",
//!   "output", ...}`).
//!
//! NOT verified (the fleet host was unreachable for the entire porting
//! session, and no other route to the legacy host's Python source was available —
//! the legacy host itself has no SSH reachable from this dev box, and Gitea returned no
//! repos to an unauthenticated search):
//! - The exact remote script path/invocation the legacy host uses per tool.
//! - The exact JSON shape of a *successful* response for any of the 10 tools.
//!
//! Given that, this module:
//! 1. Mirrors `sentinel`'s verified SSH-exec mechanics exactly (same ssh2
//!    usage, same generic non-leaking error messages, same env-var naming
//!    convention: `CRUCIBLE_SSH_HOST` / `CRUCIBLE_SSH_USER` /
//!    `CRUCIBLE_SSH_KEY_PATH` / `CRUCIBLE_SCRIPT`).
//! 2. Invokes one assumed-shape remote script (`ops.py <subcommand> '<json>'`,
//!    mirroring the one-script-many-subcommands shape `sentinel`'s
//!    `SENTINEL_SCRIPT` already uses) — **the subcommand name and the exact
//!    JSON payload keys are inferred from the `tools/list` `inputSchema`s
//!    field names, not observed on the wire.** Flagged for human audit.
//! 3. Does NOT parse the remote stdout into hand-picked fields (e.g. we do
//!    not assume a `slug` key exists and rename it). It parses stdout as
//!    JSON and relays it verbatim; if stdout isn't valid JSON it is wrapped
//!    as `{"raw": "<stdout>"}`. This avoids silently fabricating a response
//!    shape that might not match the real backend.
//! 4. `crucible_dashboard` triggers the remote regeneration and relays its
//!    result, mirroring `sentinel_refresh_status` / `vigil`'s dashboard tool
//!    exactly — it does **not** render HTML locally. Every other
//!    dashboard-generating tool already in this crate (`sentinel`, `vigil`)
//!    follows this same SSH-trigger-remote-render shape; none renders HTML
//!    in Rust. Inventing a local HTML renderer here would contradict the
//!    verified transport and the established pattern, so this port does not
//!    do that. (This does mean there is no local HTML fixture to test —
//!    tests below instead cover command construction, response relaying, and
//!    the input-validation/injection-safety surface.)
//!
//! ## Tools (identical names to the Python source)
//!   crucible_track_create  — create a new learning track
//!   crucible_log           — log progress on a track
//!   crucible_status        — status of a track (or all tracks)
//!   crucible_streak        — overall cross-track streak
//!   crucible_tracks        — list tracks
//!   crucible_hobby         — log a hobby activity
//!   crucible_reading_add   — add an item to the reading queue
//!   crucible_reading_list  — list the reading queue
//!   crucible_reading_done  — mark a reading item done
//!   crucible_dashboard     — regenerate the HTML dashboard (remote)
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   CRUCIBLE_SSH_HOST     — SSH host of the fleet box (e.g. "192.168.0.X").
//!   CRUCIBLE_SSH_USER     — SSH user, default "root".
//!   CRUCIBLE_SSH_KEY_PATH — path to the SSH private key file.
//!   CRUCIBLE_SCRIPT       — remote script invocation, default assumed to
//!                           mirror `sentinel`'s convention (see
//!                           `DEFAULT_SCRIPT`) — **unverified, audit before
//!                           relying on it in production.**
//!
//! ## Security model
//! - `track_type` / `type_filter` are validated against the fixed allowlist
//!   the live `tools/list` descriptions document (`book, course, cert, hobby,
//!   skill`).
//! - `priority` is validated against `urgent, normal, low`.
//! - `status_filter` is validated against `unread, read, ""`.
//! - Date-shaped fields (`target_date`, `date`) are validated as `YYYY-MM-DD`
//!   when non-empty.
//! - Free-text fields (`name`, `goal`, `progress`, `notes`, `project`,
//!   `entry_type`, `location`, `title`, `track`, `slug`) are length-capped
//!   (2000 chars) but otherwise unrestricted in *content* — they are never
//!   spliced into the remote shell command as raw text. They are serialized
//!   to a single JSON blob and that whole blob is shell-single-quoted (quotes
//!   inside are escaped `'\''`-style, identical to `dev::escape_single_quotes`)
//!   before being placed in the SSH command string, so no combination of
//!   shell metacharacters in a user-supplied field can break out of the
//!   single-quoted argument.
//! - Because this port does not render HTML (see point 4 above), the
//!   HTML/XSS risk an adversarial reviewer would normally look for in a
//!   dashboard generator does not apply to *this* code — the actual
//!   HTML-writing happens in a remote script this crate never sees. That
//!   remote script is out of scope for this audit but worth flagging
//!   separately to the operator.

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

const TRACK_TYPES: &[&str] = &["book", "course", "cert", "hobby", "skill"];
const PRIORITIES: &[&str] = &["urgent", "normal", "low"];
const READING_STATUS_FILTERS: &[&str] = &["unread", "read", ""];

const MAX_TEXT_LEN: usize = 2000;

/// Assumed remote invocation shape, mirroring `sentinel::DEFAULT_SCRIPT`'s
/// one-script-many-subcommands convention. **Not observed on the wire** — the
/// fleet host was unreachable for the whole porting session. Audit against
/// the real legacy-host source before relying on this in production.
///
/// PII remediation note (2026-07): real functional default (remote script
/// path on the fleet host) — left unchanged, not guess-redacted; flagged for
/// operator review before any public release.
const DEFAULT_SCRIPT: &str = "/usr/bin/python3 <path>/crucible/ops.py";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct CrucibleConfig {
    /// SSH host of the fleet box — from `CRUCIBLE_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `CRUCIBLE_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `CRUCIBLE_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Remote script invocation — from `CRUCIBLE_SCRIPT`.
    pub script: String,
}

impl CrucibleConfig {
    pub fn from_env() -> Self {
        CrucibleConfig {
            ssh_host: env::var("CRUCIBLE_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("CRUCIBLE_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("CRUCIBLE_SSH_KEY_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
            script: env::var("CRUCIBLE_SCRIPT")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_SCRIPT.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("CRUCIBLE_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("CRUCIBLE_SSH_KEY_PATH is not set".into()))
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_enum(value: &str, allowed: &[&str], field: &str) -> Result<(), ToolError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "'{field}' must be one of: {}",
            allowed
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

/// Validate a `YYYY-MM-DD` date string. Empty is always allowed (optional
/// field).
fn validate_date(s: &str) -> Result<(), ToolError> {
    if s.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = s.split('-').collect();
    let ok = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(
            "Date fields must use YYYY-MM-DD format".into(),
        ))
    }
}

/// Cap free-text field length. Content itself is not restricted — it never
/// touches the shell directly (see `shell_single_quote`).
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

fn opt_str<'a>(args: &'a Value, field: &str) -> &'a str {
    args[field].as_str().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Shell-quoting helper (matches `dev::escape_single_quotes`'s convention)
// ---------------------------------------------------------------------------

/// Wrap `s` in single quotes, escaping any embedded single quotes as
/// `'\''`. The result is safe to splice into a remote shell command as one
/// argument regardless of its content.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Command construction (pure — independently testable, no network)
// ---------------------------------------------------------------------------

/// Build the remote command for a given subcommand + JSON payload. Assumed
/// shape: `<script> <subcommand> '<json>'` — see module doc for caveats.
fn build_command(script: &str, subcommand: &str, payload: &Value) -> String {
    format!(
        "{script} {subcommand} {}",
        shell_single_quote(&payload.to_string())
    )
}

/// Parse remote stdout as JSON; if it isn't valid JSON, wrap it as
/// `{"raw": "<trimmed stdout>"}` rather than fail. Relays the backend's
/// response verbatim instead of renaming/reshaping fields we have not
/// verified.
fn parse_remote_response(stdout: &str) -> Value {
    let trimmed = stdout.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => v,
        Err(_) => json!({ "raw": trimmed }),
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking by async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return stdout. Mirrors
/// `sentinel::ssh_exec` / `gateway::ssh_exec` — generic, non-infra-leaking
/// error messages.
fn ssh_exec(
    config: &CrucibleConfig,
    command: &str,
    timeout_secs: u64,
) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("crucible: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("crucible: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("crucible: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("crucible: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!(
            "crucible: authentication failed for {}@{host}",
            config.ssh_user
        );
        return Err(ToolError::Execution(
            "Could not connect to the fleet server.".into(),
        ));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("crucible: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("crucible ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("crucible: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("crucible: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("crucible ssh_exec exit status {exit_status} for: {command}");
        return Err(ToolError::Execution(format!(
            "Remote command exited with status {exit_status}"
        )));
    }

    Ok(output)
}

/// Run one subcommand end-to-end: build the command, exec it over SSH (in a
/// blocking task), and relay the parsed response as pretty-printed JSON.
async fn run_subcommand(
    config: &Arc<CrucibleConfig>,
    subcommand: &'static str,
    payload: Value,
) -> Result<String, ToolError> {
    let cfg = Arc::clone(config);
    let command = build_command(&cfg.script, subcommand, &payload);
    let output = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, 60))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

    let response = parse_remote_response(&output);
    serde_json::to_string_pretty(&response)
        .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
}

// ---------------------------------------------------------------------------
// Tool: crucible_track_create
// ---------------------------------------------------------------------------

pub struct CrucibleTrackCreate {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleTrackCreate {
    fn name(&self) -> &str {
        "crucible_track_create"
    }

    fn description(&self) -> &str {
        "Create a new learning track in Crucible. track_type is one of: book, \
         course, cert, hobby, skill. goal describes what completion looks like. \
         target_date is an optional ISO date (YYYY-MM-DD). Returns the created \
         track object with its slug for future log calls."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Human-readable name" },
                "track_type": { "type": "string", "description": "One of: book, course, cert, hobby, skill", "enum": TRACK_TYPES },
                "goal": { "type": "string", "description": "What completion looks like" },
                "target_date": { "type": "string", "description": "Optional ISO date (YYYY-MM-DD)", "default": "" }
            },
            "required": ["name", "track_type", "goal"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = require_str(&args, "name")?;
        let track_type = require_str(&args, "track_type")?;
        let goal = require_str(&args, "goal")?;
        let target_date = opt_str(&args, "target_date");

        validate_text_len(name, "name")?;
        validate_enum(track_type, TRACK_TYPES, "track_type")?;
        validate_text_len(goal, "goal")?;
        validate_date(target_date)?;

        let payload = json!({
            "name": name,
            "track_type": track_type,
            "goal": goal,
            "target_date": target_date,
        });
        run_subcommand(&self.config, "track_create", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_log
// ---------------------------------------------------------------------------

pub struct CrucibleLog {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleLog {
    fn name(&self) -> &str {
        "crucible_log"
    }

    fn description(&self) -> &str {
        "Log a learning session for an existing track (by slug). Updates streak, \
         stores the session in Engram. Returns streak count and milestone if hit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "The track slug" },
                "progress": { "type": "string", "description": "What was accomplished" },
                "notes": { "type": "string", "description": "Optional extra notes", "default": "" },
                "duration_min": { "type": "integer", "description": "Time spent in minutes (0 if unknown)", "default": 0 }
            },
            "required": ["track", "progress"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let track = require_str(&args, "track")?;
        let progress = require_str(&args, "progress")?;
        let notes = opt_str(&args, "notes");
        let duration_min = args["duration_min"].as_i64().unwrap_or(0);

        validate_text_len(track, "track")?;
        validate_text_len(progress, "progress")?;
        validate_text_len(notes, "notes")?;
        if !(0..=1440).contains(&duration_min) {
            return Err(ToolError::InvalidArgument(
                "'duration_min' must be between 0 and 1440 (minutes in a day)".into(),
            ));
        }

        let payload = json!({
            "track": track,
            "progress": progress,
            "notes": notes,
            "duration_min": duration_min,
        });
        run_subcommand(&self.config, "log", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_status
// ---------------------------------------------------------------------------

pub struct CrucibleStatus {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleStatus {
    fn name(&self) -> &str {
        "crucible_status"
    }

    fn description(&self) -> &str {
        "Get status of one or all active learning tracks. Returns track data \
         including sessions count, last session date, streak, and goal."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Slug of a specific track, or empty for all active tracks", "default": "" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let track = opt_str(&args, "track");
        validate_text_len(track, "track")?;

        let payload = json!({ "track": track });
        run_subcommand(&self.config, "status", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_streak
// ---------------------------------------------------------------------------

pub struct CrucibleStreak {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleStreak {
    fn name(&self) -> &str {
        "crucible_streak"
    }

    fn description(&self) -> &str {
        "Get the overall learning streak across all tracks. Returns \
         current_streak (consecutive days with any session), recent_active_days \
         (last 30 days), and sessions_total."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        run_subcommand(&self.config, "streak", json!({})).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_tracks
// ---------------------------------------------------------------------------

pub struct CrucibleTracks {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleTracks {
    fn name(&self) -> &str {
        "crucible_tracks"
    }

    fn description(&self) -> &str {
        "List learning tracks, optionally filtered by type. type_filter is one \
         of book, course, cert, hobby, skill — or empty for all types. Returns \
         list of track objects."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type_filter": { "type": "string", "description": "One of book, course, cert, hobby, skill — or empty for all types", "default": "" },
                "active_only": { "type": "boolean", "description": "If true (default), only returns active tracks", "default": true }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let type_filter = opt_str(&args, "type_filter");
        let active_only = args["active_only"].as_bool().unwrap_or(true);

        if !type_filter.is_empty() {
            validate_enum(type_filter, TRACK_TYPES, "type_filter")?;
        }

        let payload = json!({
            "type_filter": type_filter,
            "active_only": active_only,
        });
        run_subcommand(&self.config, "tracks", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_hobby
// ---------------------------------------------------------------------------

pub struct CrucibleHobby {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleHobby {
    fn name(&self) -> &str {
        "crucible_hobby"
    }

    fn description(&self) -> &str {
        "Log a hobby activity (FPV drone, photography, woodworking, etc.). date \
         defaults to today if empty. Stores in Engram under crucible/hobbies/."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "Project name" },
                "entry_type": { "type": "string", "description": "Type of activity (e.g. build, flight, test, repair, planning)" },
                "date": { "type": "string", "description": "ISO date (YYYY-MM-DD), defaults to today", "default": "" },
                "location": { "type": "string", "description": "Where it happened", "default": "" },
                "notes": { "type": "string", "description": "What was done, results, observations", "default": "" }
            },
            "required": ["project", "entry_type"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = require_str(&args, "project")?;
        let entry_type = require_str(&args, "entry_type")?;
        let date = opt_str(&args, "date");
        let location = opt_str(&args, "location");
        let notes = opt_str(&args, "notes");

        validate_text_len(project, "project")?;
        validate_text_len(entry_type, "entry_type")?;
        validate_date(date)?;
        validate_text_len(location, "location")?;
        validate_text_len(notes, "notes")?;

        let payload = json!({
            "project": project,
            "entry_type": entry_type,
            "date": date,
            "location": location,
            "notes": notes,
        });
        run_subcommand(&self.config, "hobby", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_reading_add
// ---------------------------------------------------------------------------

pub struct CrucibleReadingAdd {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleReadingAdd {
    fn name(&self) -> &str {
        "crucible_reading_add"
    }

    fn description(&self) -> &str {
        "Add an article, post, doc, or book to the reading queue. priority is \
         'urgent', 'normal', or 'low'. Returns the slug for use with \
         crucible_reading_done."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Title or URL of the item to read" },
                "priority": { "type": "string", "description": "'urgent', 'normal', or 'low'", "enum": PRIORITIES, "default": "normal" },
                "notes": { "type": "string", "description": "Why it's relevant or what to look for", "default": "" }
            },
            "required": ["title"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let title = require_str(&args, "title")?;
        let priority_raw = args["priority"].as_str();
        let priority = priority_raw.unwrap_or("normal");
        let notes = opt_str(&args, "notes");

        validate_text_len(title, "title")?;
        validate_enum(priority, PRIORITIES, "priority")?;
        validate_text_len(notes, "notes")?;

        let payload = json!({
            "title": title,
            "priority": priority,
            "notes": notes,
        });
        run_subcommand(&self.config, "reading_add", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_reading_list
// ---------------------------------------------------------------------------

pub struct CrucibleReadingList {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleReadingList {
    fn name(&self) -> &str {
        "crucible_reading_list"
    }

    fn description(&self) -> &str {
        "Get the reading queue. status_filter is 'unread' (default), 'read', or \
         empty for all. Returns list of reading items sorted by priority then \
         date added."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status_filter": { "type": "string", "description": "'unread' (default), 'read', or empty for all", "default": "unread" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let status_filter = args["status_filter"].as_str().unwrap_or("unread");
        validate_enum(status_filter, READING_STATUS_FILTERS, "status_filter")?;

        let payload = json!({ "status_filter": status_filter });
        run_subcommand(&self.config, "reading_list", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_reading_done
// ---------------------------------------------------------------------------

pub struct CrucibleReadingDone {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleReadingDone {
    fn name(&self) -> &str {
        "crucible_reading_done"
    }

    fn description(&self) -> &str {
        "Mark a reading queue item as done, by the slug returned when it was \
         added. Journals the completion in Engram."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "The item slug returned when it was added" },
                "notes": { "type": "string", "description": "Optional completion notes", "default": "" }
            },
            "required": ["slug"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let slug = require_str(&args, "slug")?;
        let notes = opt_str(&args, "notes");

        validate_text_len(slug, "slug")?;
        validate_text_len(notes, "notes")?;

        let payload = json!({
            "slug": slug,
            "notes": notes,
        });
        run_subcommand(&self.config, "reading_done", payload).await
    }
}

// ---------------------------------------------------------------------------
// Tool: crucible_dashboard
// ---------------------------------------------------------------------------

pub struct CrucibleDashboard {
    config: Arc<CrucibleConfig>,
}

#[async_trait]
impl RustTool for CrucibleDashboard {
    fn name(&self) -> &str {
        "crucible_dashboard"
    }

    fn description(&self) -> &str {
        "Regenerate the Crucible learning dashboard on the fleet host's \
         /learning/ page. Pulls current track data, streak, and reading queue \
         from Engram on the fleet host and rewrites the HTML file there. \
         Returns the path to the written HTML file. Call after logging \
         sessions or adding tracks to refresh the page."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        run_subcommand(&self.config, "dashboard", json!({})).await
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Crucible tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(CrucibleConfig::from_env());

    let _ = registry.register(Box::new(CrucibleTrackCreate {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleLog {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleStatus {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleStreak {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleTracks {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleHobby {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleReadingAdd {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleReadingList {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleReadingDone {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CrucibleDashboard { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<CrucibleConfig> {
        Arc::new(CrucibleConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: DEFAULT_SCRIPT.into(),
        })
    }

    // --- pure helpers: command construction & response parsing -----------
    // (This is the "mock the Engram layer" surface for this module: since
    // the real transport is SSH not HTTP, there is no HTTP client to mock —
    // instead these pure functions are exercised directly with fixture data,
    // with zero network involved.)

    #[test]
    fn test_build_command_wraps_payload_in_single_quotes() {
        let payload = json!({"name": "Rust"});
        let cmd = build_command("/bin/ops.py", "track_create", &payload);
        assert!(cmd.starts_with("/bin/ops.py track_create '"));
        assert!(cmd.ends_with('\''));
    }

    #[test]
    fn test_build_command_escapes_embedded_single_quotes() {
        // A user-supplied field containing a single quote and shell
        // metacharacters must not be able to break out of the quoted arg.
        let payload = json!({"goal": "finish '; rm -rf / #"});
        let cmd = build_command("/bin/ops.py", "track_create", &payload);
        // No unescaped single quote sequence that would close the argument
        // early followed by shell metacharacters outside quotes.
        assert!(cmd.contains("'\\''"));
        // The dangerous substring must only appear inside the quoted+escaped
        // payload, never as a bare, shell-interpretable fragment.
        assert!(!cmd.contains("'; rm -rf / #'"));
    }

    #[test]
    fn test_shell_single_quote_escapes_single_quotes() {
        let out = shell_single_quote("a'b");
        assert_eq!(out, "'a'\\''b'");
    }

    #[test]
    fn test_parse_remote_response_valid_json_passthrough() {
        let v = parse_remote_response("{\"slug\": \"rust-programming\", \"streak\": 1}\n");
        assert_eq!(v["slug"], "rust-programming");
        assert_eq!(v["streak"], 1);
    }

    #[test]
    fn test_parse_remote_response_non_json_is_wrapped_raw() {
        let v = parse_remote_response("Track created: rust-programming");
        assert_eq!(v["raw"], "Track created: rust-programming");
    }

    // --- validation --------------------------------------------------------

    #[test]
    fn test_validate_enum_rejects_unknown() {
        assert!(validate_enum("nonsense", TRACK_TYPES, "track_type").is_err());
        assert!(validate_enum("book", TRACK_TYPES, "track_type").is_ok());
    }

    #[test]
    fn test_validate_date_allows_empty_and_rejects_malformed() {
        assert!(validate_date("").is_ok());
        assert!(validate_date("2026-07-06").is_ok()); // pii-test-fixture
        assert!(validate_date("07/06/2026").is_err());
        assert!(validate_date("not-a-date").is_err());
    }

    #[test]
    fn test_validate_text_len_rejects_oversized() {
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        assert!(validate_text_len(&huge, "notes").is_err());
        assert!(validate_text_len("short", "notes").is_ok());
    }

    // --- tool metadata -------------------------------------------------------

    #[test]
    fn test_all_tool_names_and_required_fields() {
        let cfg = test_config();
        let names_and_required: Vec<(Box<dyn RustTool>, &[&str])> = vec![
            (
                Box::new(CrucibleTrackCreate {
                    config: Arc::clone(&cfg),
                }),
                &["name", "track_type", "goal"],
            ),
            (
                Box::new(CrucibleLog {
                    config: Arc::clone(&cfg),
                }),
                &["track", "progress"],
            ),
            (
                Box::new(CrucibleStatus {
                    config: Arc::clone(&cfg),
                }),
                &[],
            ),
            (
                Box::new(CrucibleStreak {
                    config: Arc::clone(&cfg),
                }),
                &[],
            ),
            (
                Box::new(CrucibleTracks {
                    config: Arc::clone(&cfg),
                }),
                &[],
            ),
            (
                Box::new(CrucibleHobby {
                    config: Arc::clone(&cfg),
                }),
                &["project", "entry_type"],
            ),
            (
                Box::new(CrucibleReadingAdd {
                    config: Arc::clone(&cfg),
                }),
                &["title"],
            ),
            (
                Box::new(CrucibleReadingList {
                    config: Arc::clone(&cfg),
                }),
                &[],
            ),
            (
                Box::new(CrucibleReadingDone {
                    config: Arc::clone(&cfg),
                }),
                &["slug"],
            ),
            (Box::new(CrucibleDashboard { config: cfg }), &[]),
        ];

        for (tool, required) in names_and_required {
            assert!(tool.name().starts_with("crucible_"), "{}", tool.name());
            let params = tool.parameters();
            assert_eq!(params["type"], "object");
            let got_required: Vec<String> = params["required"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(got_required.len(), required.len(), "tool {}", tool.name());
            for r in required {
                assert!(
                    got_required.iter().any(|g| g == r),
                    "tool {} missing required {}",
                    tool.name(),
                    r
                );
            }
        }
    }

    // --- execute: invalid arguments (no network needed) ---------------------

    #[tokio::test]
    async fn test_track_create_rejects_unknown_track_type() {
        let tool = CrucibleTrackCreate {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"name": "x", "track_type": "not-a-type", "goal": "y"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_track_create_rejects_bad_target_date() {
        let tool = CrucibleTrackCreate {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"name": "x", "track_type": "book", "goal": "y", "target_date": "not-a-date"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_log_rejects_negative_duration() {
        let tool = CrucibleLog {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"track": "rust", "progress": "did stuff", "duration_min": -5}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_log_rejects_absurdly_large_duration() {
        // Defense-in-depth: an unbounded duration_min would reach the remote
        // script unchecked (flagged by adversarial review). Cap at 1440
        // (minutes in a day).
        let tool = CrucibleLog {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"track": "rust", "progress": "did stuff", "duration_min": i64::MAX}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_reading_add_rejects_bad_priority() {
        let tool = CrucibleReadingAdd {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"title": "some post", "priority": "asap"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_reading_list_rejects_bad_status_filter() {
        let tool = CrucibleReadingList {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"status_filter": "archived"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_hobby_rejects_bad_date() {
        let tool = CrucibleHobby {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"project": "FPV", "entry_type": "build", "date": "yesterday"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_tracks_rejects_bad_type_filter() {
        let tool = CrucibleTracks {
            config: test_config(),
        };
        let err = tool
            .execute(json!({"type_filter": "not-a-type"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- execute: NotConfigured before any network attempt -------------------

    #[tokio::test]
    async fn test_streak_not_configured_without_ssh_host() {
        let tool = CrucibleStreak {
            config: test_config(),
        };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("CRUCIBLE_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_dashboard_not_configured_without_ssh_host() {
        let tool = CrucibleDashboard {
            config: test_config(),
        };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("CRUCIBLE_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- infra-leak genericization (mirrors sentinel's / gateway's test) -----

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        let cfg = CrucibleConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: DEFAULT_SCRIPT.into(),
        };
        let msg = match ssh_exec(&cfg, "true", 2).expect_err("unroutable host must error") {
            ToolError::Execution(m) => m,
            other => panic!("expected Execution error, got {other:?}"),
        };
        let lower = msg.to_lowercase();
        assert!(!lower.contains("ssh"), "leaked 'ssh': {msg}");
        assert!(!msg.contains(":22"), "leaked port 22: {msg}");
        assert!(!msg.contains("127.0.0.1"), "leaked target IP: {msg}");
    }

    // --- registration ---------------------------------------------------------

    #[test]
    fn test_crucible_tools_registered() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 10);
        for name in [
            "crucible_track_create",
            "crucible_log",
            "crucible_status",
            "crucible_streak",
            "crucible_tracks",
            "crucible_hobby",
            "crucible_reading_add",
            "crucible_reading_list",
            "crucible_reading_done",
            "crucible_dashboard",
        ] {
            assert!(registry.contains(name), "missing tool {name}");
        }
    }
}
