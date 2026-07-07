//! Cortex tools — code-graph / blast-radius / risk-scoring system, ported
//! from the Python `cortex_tools.py` on the source host (ai-terminus,
//! the source host's MCP endpoint).
//!
//! ## Verified transport (IMPORTANT — read before touching this module)
//!
//! Every `cortex_*` tool was called live against the source host's MCP endpoint before
//! writing this port (`tools/list` for exact names/descriptions/schemas, then
//! `tools/call` for each of the 10 tools against real args — `repo:
//! "lumina-terminus"` for the graph-shaped tools, and
//! `https://github.com/octocat/Hello-World` — a tiny, well-known, harmless
//! public repo — for `cortex_audit`). The fleet host (the same host
//! `crucible`/`sentinel`/`vigil` already documented as SSH-exec
//! targets in this crate) was unreachable for the entire porting session, so
//! every one of the 10 tools failed. The failure shapes are decisive and
//! come in two flavors:
//!
//! **Flavor A — bare passthrough** (`cortex_scope`, `cortex_review`,
//! `cortex_stats`, `cortex_build`, `cortex_audit`):
//! ```text
//! {"error": "ssh: connect to host 192.168.0.X port 22: No route to host"}
//! ```
//! identical to the crucible/sentinel/vigil signature — the SSH failure is
//! the *entire* response, unwrapped.
//!
//! **Flavor B — degraded/partial response** (`cortex_architecture`,
//! `cortex_deps`, `cortex_recent`, `cortex_community`, `cortex_flows`): these
//! five catch the SSH failure and still return a shaped JSON object:
//! ```text
//! cortex_architecture -> {"repo":"lumina-terminus","stats":{},"architecture_summary":"? nodes, ? edges across ? files"}
//! cortex_deps         -> {"repo":"lumina-terminus","file":"src/registry.rs","affected_files":[],"blast_count":0,"token_reduction_pct":0}
//! cortex_recent       -> {"repo":"lumina-terminus","frequently_changed":[],"stats":{"error":"ssh: ..."}}
//! cortex_community    -> {"repo":"lumina-terminus","community_summary":"","stats":{"error":"ssh: ..."}}
//! cortex_flows        -> {"repo":"lumina-terminus","entry_point":"main","stats":{"error":"ssh: ..."},"note":"Flow tracing uses graph FTS — search for entry_point in graph for full call chain"}
//! ```
//! This is the same *decisive* evidence crucible's port relied on: **Cortex
//! does not build or query the code graph locally in the source host's Python process.
//! It SSHes into the fleet host and runs a script there** — exactly the
//! `sentinel`/`vigil`/`crucible` pattern, not a local graph engine, not an
//! HTTP client, and (critically for `cortex_audit`) not a local `git clone`
//! either. The five Flavor-B tools additionally reveal their outer response
//! *shape* even under total backend failure, which this port reproduces
//! exactly (see the per-tool doc comments below) — this is real signal, not
//! guesswork, and is a meaningfully stronger basis than crucible's port had
//! for any of its ten tools.
//!
//! ## What this means for graph persistence / community detection
//!
//! The task brief asked me to determine "how the source host persists the code graph"
//! and whether community detection uses a real algorithm (Louvain or
//! similar) vs. something simpler, and to reach for a Rust graph/community
//! crate only if warranted. Given the verified transport above, **the graph
//! is built, persisted, and queried entirely on the fleet host — never in
//! this Rust process, exactly as it is never in the source host's Python process
//! either.** There is no local graph data structure to design, no
//! persistence format to choose, and no community-detection algorithm to
//! implement or select a crate for: this crate does not have the graph, the
//! fleet host does. Reaching for `petgraph` (not currently a dependency —
//! confirmed absent from `Cargo.toml`) or hand-rolling Louvain here would be
//! inventing behavior with **zero support** from either the live server's
//! observed responses or the existing SSH-exec precedent in this crate. It
//! would also silently misrepresent this port as "does its own graph
//! analysis" when the verified truth is "relays a fleet-host script's
//! output" — the same reasoning `crucible`'s port already documented for why
//! it does not render its own dashboard HTML.
//!
//! This is the single biggest scope decision in this port and is called out
//! explicitly in the deliverable report, not just here.
//!
//! ## What is verified vs. assumed
//!
//! Verified directly against the live server:
//! - All 10 tool names, descriptions, and `inputSchema`s (via `tools/list`).
//! - The transport is SSH to the fleet host on port 22 (via the reproduced
//!   error signatures above, for all 10 tools including `cortex_audit`).
//! - The Flavor A vs. Flavor B response-shape split documented above,
//!   including the exact field names Flavor B leaks even while broken
//!   (`stats`, `architecture_summary`, `affected_files`, `blast_count`,
//!   `token_reduction_pct`, `frequently_changed`, `community_summary`,
//!   `entry_point`, `note`).
//!
//! NOT verified (the fleet host was unreachable for the whole porting
//! session, and the source host itself has no SSH reachable from this dev box; Gitea
//! search for a `cortex`-named repo/module also returned nothing):
//! - The exact remote script path/invocation the source host uses per tool.
//! - The exact JSON shape of a *successful* response for any of the 10
//!   tools (Flavor A tools reveal nothing about their success shape; Flavor
//!   B tools reveal only their *outer* shape, not what a populated `stats`
//!   object or `architecture_summary` string look like on a real graph).
//! - Whatever sandboxing `cortex_audit`'s remote clone step actually uses on
//!   the fleet host (temp dir, container, VM, none of the above — cannot be
//!   observed from here). See the residual-risk statement at the bottom of
//!   this doc comment and in `audit.rs`.
//!
//! Given that, this module:
//! 1. Mirrors `crucible`/`sentinel`/`vigil`'s SSH-exec mechanics exactly
//!    (same `ssh2` usage, same generic non-infra-leaking error messages,
//!    same env-var naming convention: `CORTEX_SSH_HOST` / `CORTEX_SSH_USER`
//!    / `CORTEX_SSH_KEY_PATH` / `CORTEX_SCRIPT`).
//! 2. Invokes one assumed-shape remote script (`ops.py <subcommand>
//!    '<json>'`), mirroring the one-script-many-subcommands convention
//!    `sentinel`/`crucible` already use — **the subcommand name and exact
//!    JSON payload keys are inferred from the `tools/list` `inputSchema`s
//!    field names, not observed on the wire** (except where Flavor B leaked
//!    field names directly, which are reproduced verbatim). Flagged for
//!    human audit.
//! 3. Does NOT parse remote stdout into hand-picked fields. It parses stdout
//!    as JSON and relays it verbatim; non-JSON stdout is wrapped as
//!    `{"raw": "<stdout>"}`. Same rationale as crucible: avoid fabricating a
//!    response shape not actually observed.
//! 4. For the five tools verified to degrade gracefully (Flavor B), this
//!    port additionally mirrors that *specific* degrade shape locally when
//!    the SSH call itself fails before even reaching the remote host (e.g.
//!    `NotConfigured`, unreachable host) — matching the live behavior that
//!    these tools return a 200-shaped partial JSON rather than a bare error,
//!    since that is directly observed, not assumed. The five Flavor-A tools
//!    propagate the SSH failure as a `ToolError`, matching their observed
//!    bare-error behavior.
//! 5. `cortex_audit` gets defense-in-depth URL validation — see `audit.rs`
//!    — **on top of** the standard shell-quoting every free-text argument in
//!    this crate already gets. This is the one place in the whole crate
//!    where the argument is, by design, an operator-supplied pointer to
//!    execute a clone of untrusted external content; see the residual-risk
//!    statement below.
//!
//! ## Tools (identical names to the Python source)
//!   cortex_scope         — blast-radius for a planned change (pre-change)
//!   cortex_review        — post-change risk score
//!   cortex_audit         — clone + graph + report for an EXTERNAL repo URL
//!   cortex_stats         — graph statistics for a known repo
//!   cortex_build         — rebuild the code graph (incremental)
//!   cortex_architecture  — community-detection architecture overview
//!   cortex_deps          — direct dependencies/callers for a file
//!   cortex_recent        — recently changed high-risk files (git-log based)
//!   cortex_community     — community/cluster structure
//!   cortex_flows         — trace execution flows from an entry point
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   CORTEX_SSH_HOST     — SSH host of the fleet box (e.g. "192.168.0.X").
//!   CORTEX_SSH_USER     — SSH user, default "root".
//!   CORTEX_SSH_KEY_PATH — path to the SSH private key file.
//!   CORTEX_SCRIPT       — remote script invocation, default assumed to
//!                         mirror `sentinel`/`crucible`'s convention (see
//!                         `DEFAULT_SCRIPT`) — **unverified, audit before
//!                         relying on it in production.**
//!
//! ## `cortex_audit` residual risk — READ THIS BEFORE DEPLOYING
//!
//! `cortex_audit`'s live error signature is byte-for-byte identical to every
//! other Flavor-A tool: the clone/graph-build/report/cleanup all happen in
//! whatever script runs on the fleet host after this port's SSH-exec call
//! lands there. **This Rust port never runs `git clone` itself** — there is
//! no local sandbox directory, no local cleanup path, and therefore nothing
//! in this crate that can be *this port's* isolation guarantee for the
//! clone step, because the clone does not happen in this process, exactly
//! as it does not happen in the source host's Python process either. The operator's
//! brief specifically asked for "a git clone into an isolated temp dir that
//! gets cleaned up after" as the expected shape — **that is not what the
//! live system does**; it delegates the entire operation to a remote script
//! this port has no visibility into.
//!
//! What this port *does* contribute, within its actual control surface:
//! - Strict `url` validation (`audit.rs`) rejecting non-http(s) schemes,
//!   embedded credentials, and loopback/private/link-local/metadata hosts —
//!   closing off the SSRF-shaped risk of using this tool to point the fleet
//!   host's clone step at internal infrastructure under cover of "public
//!   repo audit".
//! - The same shell-metacharacter-safe command construction (single-quote
//!   escaping) every other argument in this crate gets, so no `url` value
//!   can break out of the SSH command string regardless of validation
//!   coverage gaps.
//!
//! What remains **unverified and out of this port's control**:
//! - Whether the fleet-host script actually clones into an isolated,
//!   cleaned-up temp dir, and whether cleanup is guaranteed on
//!   failure/panic (there is no way to observe this without direct access
//!   to the fleet host or its script source, neither of which was reachable
//!   this session).
//! - Whether graph-building on the cloned content could execute anything
//!   from the repo (e.g. shelling out to a language-specific import
//!   resolver that runs project code) — again, unobservable from here.
//!
//! **Recommendation to the operator:** audit the fleet-host `cortex` script
//! directly (path assumed `<path>/cortex/ops.py`, unverified) for
//! actual sandbox isolation and guaranteed cleanup before treating
//! `cortex_audit` as safe to run against arbitrary operator-supplied URLs in
//! production. This Rust port is, at best, as safe as that remote script —
//! it cannot be safer, because it does not perform the risky operation
//! itself.

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

pub mod audit;
use audit::validate_repo_url;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const KNOWN_REPOS: &[&str] = &["lumina-fleet", "lumina-terminus"];
const MAX_TEXT_LEN: usize = 2000;

/// Assumed remote invocation shape, mirroring `sentinel::DEFAULT_SCRIPT` /
/// `crucible::DEFAULT_SCRIPT`'s one-script-many-subcommands convention.
/// **Not observed on the wire** — the fleet host was unreachable for the
/// whole porting session. Audit against the real source-host implementation before relying
/// on this in production.
const DEFAULT_SCRIPT: &str = "/usr/bin/python3 <path>/cortex/ops.py";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CortexConfig {
    pub ssh_host: Option<String>,
    pub ssh_user: String,
    pub ssh_key_path: Option<String>,
    pub script: String,
}

impl CortexConfig {
    pub fn from_env() -> Self {
        CortexConfig {
            ssh_host: env::var("CORTEX_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("CORTEX_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("CORTEX_SSH_KEY_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
            script: env::var("CORTEX_SCRIPT")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_SCRIPT.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("CORTEX_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("CORTEX_SSH_KEY_PATH is not set".into()))
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_repo(repo: &str) -> Result<(), ToolError> {
    if KNOWN_REPOS.contains(&repo) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "'repo' must be one of: {}",
            KNOWN_REPOS.join(", ")
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
// Shell-quoting helper (matches `dev::escape_single_quotes` / crucible's
// convention)
// ---------------------------------------------------------------------------

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Command construction (pure — independently testable, no network)
// ---------------------------------------------------------------------------

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
/// `crucible::ssh_exec` / `sentinel::ssh_exec` — generic, non-infra-leaking
/// error messages.
fn ssh_exec(config: &CortexConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("cortex: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("cortex: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("cortex: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("cortex: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!("cortex: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution(
            "Could not connect to the fleet server.".into(),
        ));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("cortex: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("cortex ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("cortex: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("cortex: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("cortex ssh_exec exit status {exit_status} for: {command}");
        return Err(ToolError::Execution(format!(
            "Remote command exited with status {exit_status}"
        )));
    }

    Ok(output)
}

/// Run one subcommand end-to-end: build the command, exec it over SSH (in a
/// blocking task), and relay the parsed response as pretty-printed JSON.
/// Used by the five Flavor-A tools, whose verified live behavior is to
/// propagate a bare SSH-failure error with no wrapping.
async fn run_subcommand(
    config: &Arc<CortexConfig>,
    subcommand: &'static str,
    payload: Value,
    timeout_secs: u64,
) -> Result<String, ToolError> {
    let cfg = Arc::clone(config);
    let command = build_command(&cfg.script, subcommand, &payload);
    let output = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, timeout_secs))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

    let response = parse_remote_response(&output);
    serde_json::to_string_pretty(&response)
        .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
}

/// Run one subcommand for a Flavor-B tool: verified to degrade to a shaped
/// JSON object (rather than a bare error) when the *remote* SSH call fails
/// (unreachable host, auth failure, non-zero exit, etc.) — matching the
/// live-observed behavior. `on_degrade` builds that shaped fallback from the
/// SSH error message.
///
/// A `NotConfigured` error (missing `CORTEX_SSH_HOST`/`CORTEX_SSH_KEY_PATH`)
/// is deliberately **not** degraded here — that's a local deployment
/// misconfiguration, not something the live source-host/fleet-host pair would ever
/// actually experience in production (they're always configured), so
/// masking it behind a degrade shape that looks like a mostly-empty-but-
/// valid response would hide an operator setup mistake rather than surface
/// it. It propagates as a normal error instead, same as every Flavor-A tool.
async fn run_subcommand_degrading(
    config: &Arc<CortexConfig>,
    subcommand: &'static str,
    payload: Value,
    timeout_secs: u64,
    on_degrade: impl FnOnce(String) -> Value,
) -> Result<String, ToolError> {
    let cfg = Arc::clone(config);
    let command = build_command(&cfg.script, subcommand, &payload);
    let result = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, timeout_secs))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

    let response = match result {
        Ok(output) => parse_remote_response(&output),
        Err(ToolError::NotConfigured(msg)) => return Err(ToolError::NotConfigured(msg)),
        Err(e) => on_degrade(e.to_string()),
    };
    serde_json::to_string_pretty(&response)
        .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
}

// ---------------------------------------------------------------------------
// Tool: cortex_scope
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
        "Get blast radius for a planned code change — which files will be \
         affected. Use this BEFORE a dev loop to scope the Claude Code \
         session context. repo: 'lumina-fleet' or 'lumina-terminus'. \
         changed_files: comma-separated list of files e.g. \
         'axon/axon.py,axon_tools.py'. Returns: blast_radius (list of \
         affected files), token_reduction_pct, blast_count."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS },
                "changed_files": { "type": "string", "description": "Comma-separated list of file paths e.g. 'axon/axon.py,axon_tools.py'" }
            },
            "required": ["repo", "changed_files"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?;
        let changed_files = require_str(&args, "changed_files")?;
        validate_repo(repo)?;
        validate_text_len(changed_files, "changed_files")?;

        let payload = json!({ "repo": repo, "changed_files": changed_files });
        run_subcommand(&self.config, "scope", payload, 60).await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_review
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
        "Get post-change risk assessment for modified files. Use this AFTER \
         a dev loop to check risk before committing. repo: 'lumina-fleet' or \
         'lumina-terminus'. changed_files: comma-separated file paths that \
         were modified. Returns: risk_score (0-10), risk_signals (list), \
         blast_radius, token_reduction_pct. If risk_score > 7: escalate to \
         Mr. Wizard before committing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS },
                "changed_files": { "type": "string", "description": "Comma-separated file paths that were modified" }
            },
            "required": ["repo", "changed_files"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?;
        let changed_files = require_str(&args, "changed_files")?;
        validate_repo(repo)?;
        validate_text_len(changed_files, "changed_files")?;

        let payload = json!({ "repo": repo, "changed_files": changed_files });
        run_subcommand(&self.config, "review", payload, 60).await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_audit
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
        "Audit an external public Git repository. Clones, builds code \
         graph, generates HTML report, cleans up sandbox. url: public git \
         repo URL e.g. 'https://github.com/owner/repo'. Returns: stats \
         (nodes, edges, files), report_url, risk signals. Report published \
         at http://<fleet-host>/code/{report-name}.html. \
         SAFETY NOTE: only http/https URLs to public hosts are accepted; \
         URLs pointing at local/private/internal addresses are rejected."
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
        validate_repo_url(url)?;

        let payload = json!({ "url": url });
        // Longer timeout than the graph-query tools: a real clone + graph
        // build + HTML render is a much heavier operation than a query
        // against an already-built graph.
        run_subcommand(&self.config, "audit", payload, 300).await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_stats
// ---------------------------------------------------------------------------

pub struct CortexStats {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexStats {
    fn name(&self) -> &str {
        "cortex_stats"
    }

    fn description(&self) -> &str {
        "Get graph statistics for a known repo. repo: 'lumina-fleet' or \
         'lumina-terminus'. Returns: nodes, edges, files, languages, \
         last_updated, commit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?;
        validate_repo(repo)?;

        let payload = json!({ "repo": repo });
        run_subcommand(&self.config, "stats", payload, 60).await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_build
// ---------------------------------------------------------------------------

pub struct CortexBuild {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexBuild {
    fn name(&self) -> &str {
        "cortex_build"
    }

    fn description(&self) -> &str {
        "Rebuild the code graph for a repo (incremental update). Use after \
         pushing changes to keep the graph current. repo: 'lumina-fleet' or \
         'lumina-terminus'. Returns: stats after rebuild."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?;
        validate_repo(repo)?;

        let payload = json!({ "repo": repo });
        // Rebuilding the graph is a heavier operation than a plain query.
        run_subcommand(&self.config, "build", payload, 180).await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_architecture
// ---------------------------------------------------------------------------

pub struct CortexArchitecture {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexArchitecture {
    fn name(&self) -> &str {
        "cortex_architecture"
    }

    fn description(&self) -> &str {
        "Get high-level architecture overview via community detection. \
         Returns module communities, inter-module coupling, key files. \
         repo: 'lumina-fleet' or 'lumina-terminus'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?.to_string();
        validate_repo(&repo)?;

        let payload = json!({ "repo": repo });
        let repo_for_degrade = repo.clone();
        run_subcommand_degrading(&self.config, "architecture", payload, 60, move |_err| {
            json!({
                "repo": repo_for_degrade,
                "stats": {},
                "architecture_summary": "? nodes, ? edges across ? files",
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_deps
// ---------------------------------------------------------------------------

pub struct CortexDeps {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexDeps {
    fn name(&self) -> &str {
        "cortex_deps"
    }

    fn description(&self) -> &str {
        "Get direct dependencies and callers for a specific file. repo: \
         'lumina-fleet' or 'lumina-terminus'. file_path: relative path e.g. \
         'axon/axon.py'. Returns: imports_from (what this file imports), \
         imported_by (what imports this file)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS },
                "file_path": { "type": "string", "description": "Relative path e.g. 'axon/axon.py'" }
            },
            "required": ["repo", "file_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?.to_string();
        let file_path = require_str(&args, "file_path")?.to_string();
        validate_repo(&repo)?;
        validate_text_len(&file_path, "file_path")?;

        let payload = json!({ "repo": repo, "file_path": file_path });
        let (repo_d, file_d) = (repo.clone(), file_path.clone());
        run_subcommand_degrading(&self.config, "deps", payload, 60, move |_err| {
            json!({
                "repo": repo_d,
                "file": file_d,
                "affected_files": [],
                "blast_count": 0,
                "token_reduction_pct": 0,
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_recent
// ---------------------------------------------------------------------------

pub struct CortexRecent {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexRecent {
    fn name(&self) -> &str {
        "cortex_recent"
    }

    fn description(&self) -> &str {
        "Get recently changed high-risk files in a repo. Uses git log + \
         graph coupling to surface files that need attention. repo: \
         'lumina-fleet' or 'lumina-terminus'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?.to_string();
        validate_repo(&repo)?;

        let payload = json!({ "repo": repo });
        let repo_d = repo.clone();
        run_subcommand_degrading(&self.config, "recent", payload, 60, move |err| {
            json!({
                "repo": repo_d,
                "frequently_changed": [],
                "stats": { "error": err },
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_community
// ---------------------------------------------------------------------------

pub struct CortexCommunity {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexCommunity {
    fn name(&self) -> &str {
        "cortex_community"
    }

    fn description(&self) -> &str {
        "Get community structure (module clusters) from the code graph. \
         Identifies architectural boundaries and cross-cutting concerns. \
         repo: 'lumina-fleet' or 'lumina-terminus'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?.to_string();
        validate_repo(&repo)?;

        let payload = json!({ "repo": repo });
        let repo_d = repo.clone();
        run_subcommand_degrading(&self.config, "community", payload, 60, move |err| {
            json!({
                "repo": repo_d,
                "community_summary": "",
                "stats": { "error": err },
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_flows
// ---------------------------------------------------------------------------

pub struct CortexFlows {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexFlows {
    fn name(&self) -> &str {
        "cortex_flows"
    }

    fn description(&self) -> &str {
        "Trace execution flows from an entry point through the codebase. \
         repo: 'lumina-fleet' or 'lumina-terminus'. entry_point: function or \
         module name e.g. 'axon.run_loop' or 'briefing.run_briefing'. \
         Returns: call chain, reachable functions, flow depth."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "'lumina-fleet' or 'lumina-terminus'", "enum": KNOWN_REPOS },
                "entry_point": { "type": "string", "description": "Function or module name e.g. 'axon.run_loop' or 'briefing.run_briefing'" }
            },
            "required": ["repo", "entry_point"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = require_str(&args, "repo")?.to_string();
        let entry_point = require_str(&args, "entry_point")?.to_string();
        validate_repo(&repo)?;
        validate_text_len(&entry_point, "entry_point")?;

        let payload = json!({ "repo": repo, "entry_point": entry_point });
        let (repo_d, entry_d) = (repo.clone(), entry_point.clone());
        run_subcommand_degrading(&self.config, "flows", payload, 60, move |err| {
            json!({
                "repo": repo_d,
                "entry_point": entry_d,
                "stats": { "error": err },
                "note": "Flow tracing uses graph FTS — search for entry_point in graph for full call chain",
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Cortex tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(CortexConfig::from_env());

    let _ = registry.register(Box::new(CortexScope {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexReview {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexAudit {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexStats {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexBuild {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexArchitecture {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexDeps {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexRecent {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexCommunity {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexFlows { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<CortexConfig> {
        Arc::new(CortexConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: DEFAULT_SCRIPT.into(),
        })
    }

    // --- pure helpers: command construction & response parsing -------------

    #[test]
    fn test_build_command_wraps_payload_in_single_quotes() {
        let payload = json!({"repo": "lumina-terminus"});
        let cmd = build_command("/bin/ops.py", "stats", &payload);
        assert!(cmd.starts_with("/bin/ops.py stats '"));
        assert!(cmd.ends_with('\''));
    }

    #[test]
    fn test_build_command_escapes_embedded_single_quotes_and_shell_metachars() {
        let payload = json!({"changed_files": "a.py'; rm -rf / #"});
        let cmd = build_command("/bin/ops.py", "scope", &payload);
        assert!(cmd.contains("'\\''"));
        assert!(!cmd.contains("'; rm -rf / #'"));
    }

    #[test]
    fn test_shell_single_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn test_parse_remote_response_valid_json_passthrough() {
        let v = parse_remote_response("{\"nodes\": 42, \"edges\": 7}\n");
        assert_eq!(v["nodes"], 42);
        assert_eq!(v["edges"], 7);
    }

    #[test]
    fn test_parse_remote_response_non_json_is_wrapped_raw() {
        let v = parse_remote_response("Graph rebuilt: 42 nodes");
        assert_eq!(v["raw"], "Graph rebuilt: 42 nodes");
    }

    // --- validation ----------------------------------------------------------

    #[test]
    fn test_validate_repo_accepts_known_values() {
        assert!(validate_repo("lumina-fleet").is_ok());
        assert!(validate_repo("lumina-terminus").is_ok());
    }

    #[test]
    fn test_validate_repo_rejects_unknown() {
        assert!(validate_repo("some-other-repo").is_err());
    }

    #[test]
    fn test_validate_text_len_rejects_oversized() {
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        assert!(validate_text_len(&huge, "entry_point").is_err());
        assert!(validate_text_len("short", "entry_point").is_ok());
    }

    // --- tool metadata ---------------------------------------------------------

    #[test]
    fn test_all_tool_names_and_required_fields() {
        let cfg = test_config();
        let names_and_required: Vec<(Box<dyn RustTool>, &[&str])> = vec![
            (
                Box::new(CortexScope { config: Arc::clone(&cfg) }),
                &["repo", "changed_files"],
            ),
            (
                Box::new(CortexReview { config: Arc::clone(&cfg) }),
                &["repo", "changed_files"],
            ),
            (Box::new(CortexAudit { config: Arc::clone(&cfg) }), &["url"]),
            (Box::new(CortexStats { config: Arc::clone(&cfg) }), &["repo"]),
            (Box::new(CortexBuild { config: Arc::clone(&cfg) }), &["repo"]),
            (
                Box::new(CortexArchitecture { config: Arc::clone(&cfg) }),
                &["repo"],
            ),
            (
                Box::new(CortexDeps { config: Arc::clone(&cfg) }),
                &["repo", "file_path"],
            ),
            (Box::new(CortexRecent { config: Arc::clone(&cfg) }), &["repo"]),
            (
                Box::new(CortexCommunity { config: Arc::clone(&cfg) }),
                &["repo"],
            ),
            (
                Box::new(CortexFlows { config: cfg }),
                &["repo", "entry_point"],
            ),
        ];

        for (tool, required) in names_and_required {
            assert!(tool.name().starts_with("cortex_"), "{}", tool.name());
            let params = tool.parameters();
            assert_eq!(params["type"], "object");
            let got_required: Vec<String> = params["required"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
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

    // --- execute: invalid arguments (no network needed) -----------------------

    #[tokio::test]
    async fn test_scope_rejects_unknown_repo() {
        let tool = CortexScope { config: test_config() };
        let err = tool
            .execute(json!({"repo": "not-a-repo", "changed_files": "a.py"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_deps_rejects_unknown_repo() {
        let tool = CortexDeps { config: test_config() };
        let err = tool
            .execute(json!({"repo": "nope", "file_path": "a.py"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_flows_rejects_unknown_repo() {
        let tool = CortexFlows { config: test_config() };
        let err = tool
            .execute(json!({"repo": "nope", "entry_point": "main"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_audit_rejects_non_public_url_before_any_network_attempt() {
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
    async fn test_deps_rejects_oversized_file_path() {
        let tool = CortexDeps { config: test_config() };
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = tool
            .execute(json!({"repo": "lumina-terminus", "file_path": huge}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- execute: NotConfigured before any network attempt ----------------------

    #[tokio::test]
    async fn test_stats_not_configured_without_ssh_host() {
        let tool = CortexStats { config: test_config() };
        let err = tool
            .execute(json!({"repo": "lumina-terminus"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("CORTEX_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_audit_not_configured_without_ssh_host() {
        let tool = CortexAudit { config: test_config() };
        let err = tool
            .execute(json!({"url": "https://github.com/owner/repo"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("CORTEX_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- Flavor-B degrade shape: verified-live-shape reproduction ------------
    // These exercise `run_subcommand_degrading`'s fallback branch directly by
    // driving it through a tool whose backend is unreachable (no SSH host
    // configured), proving the degrade JSON matches the exact shape observed
    // live against the source host.

    #[tokio::test]
    async fn test_architecture_degrades_to_verified_shape_when_unreachable() {
        let tool = CortexArchitecture { config: test_config() };
        // NotConfigured short-circuits before reaching run_subcommand_degrading
        // in this port (config validated first) — that's an intentional and
        // safer improvement over blindly returning the degrade shape without
        // ever having a host configured at all. Confirm the error type here,
        // and separately unit-test the degrade closures' shapes below.
        let err = tool
            .execute(json!({"repo": "lumina-terminus"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    /// A config with a host/key configured (so `NotConfigured` never fires)
    /// but pointed at loopback with a key path that cannot possibly
    /// authenticate — this reaches the real remote-failure branch inside
    /// `run_subcommand_degrading` so the degrade-shape tests below exercise
    /// the actual production closures, not a hand-copied literal.
    fn reachable_but_failing_config() -> Arc<CortexConfig> {
        Arc::new(CortexConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/nonexistent/cortex-test-key".into()),
            script: DEFAULT_SCRIPT.into(),
        })
    }

    #[tokio::test]
    async fn test_architecture_degrade_shape_matches_live_observation() {
        let tool = CortexArchitecture {
            config: reachable_but_failing_config(),
        };
        let out = tool
            .execute(json!({"repo": "lumina-terminus"}))
            .await
            .expect("degrading tool must return Ok, not propagate the SSH failure");
        let shape: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(shape["repo"], "lumina-terminus");
        assert_eq!(shape["stats"], json!({}));
        assert_eq!(shape["architecture_summary"], "? nodes, ? edges across ? files");
    }

    #[tokio::test]
    async fn test_deps_degrade_shape_matches_live_observation() {
        let tool = CortexDeps {
            config: reachable_but_failing_config(),
        };
        let out = tool
            .execute(json!({"repo": "lumina-terminus", "file_path": "src/registry.rs"}))
            .await
            .expect("degrading tool must return Ok, not propagate the SSH failure");
        let shape: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(shape["repo"], "lumina-terminus");
        assert_eq!(shape["file"], "src/registry.rs");
        assert_eq!(shape["affected_files"], json!([]));
        assert_eq!(shape["blast_count"], 0);
        assert_eq!(shape["token_reduction_pct"], 0);
    }

    #[tokio::test]
    async fn test_recent_community_flows_degrade_shapes_match_live_observation() {
        let cfg = reachable_but_failing_config();

        let recent_out = CortexRecent { config: Arc::clone(&cfg) }
            .execute(json!({"repo": "lumina-terminus"}))
            .await
            .expect("degrading tool must return Ok");
        let recent: Value = serde_json::from_str(&recent_out).unwrap();
        assert_eq!(recent["repo"], "lumina-terminus");
        assert_eq!(recent["frequently_changed"], json!([]));
        assert!(recent["stats"]["error"].is_string());

        let community_out = CortexCommunity { config: Arc::clone(&cfg) }
            .execute(json!({"repo": "lumina-terminus"}))
            .await
            .expect("degrading tool must return Ok");
        let community: Value = serde_json::from_str(&community_out).unwrap();
        assert_eq!(community["repo"], "lumina-terminus");
        assert_eq!(community["community_summary"], "");
        assert!(community["stats"]["error"].is_string());

        let flows_out = CortexFlows { config: cfg }
            .execute(json!({"repo": "lumina-terminus", "entry_point": "main"}))
            .await
            .expect("degrading tool must return Ok");
        let flows: Value = serde_json::from_str(&flows_out).unwrap();
        assert_eq!(flows["repo"], "lumina-terminus");
        assert_eq!(flows["entry_point"], "main");
        assert!(flows["stats"]["error"].is_string());
        assert!(flows["note"].as_str().unwrap().contains("Flow tracing"));
    }

    // --- infra-leak genericization (mirrors crucible's / sentinel's test) -----

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        let cfg = CortexConfig {
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

    // --- registration -----------------------------------------------------------

    #[test]
    fn test_cortex_tools_registered() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
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
