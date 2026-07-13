//! BLD-08 — `compiler_status`: the compiler's read surface (fleet version query).
//!
//! Aggregates three things into one structured payload the fleet GUI (BLD-15) and
//! agents — and the Harmony fleet API (BLD-16, `harmony-server/src/fleet.rs`) —
//! consume:
//!
//!   1. **Store `current` pointers** — per `(module, channel)`, the blessed sha the
//!      artifact store points at (BLD-07 writes `current`; when it is absent we
//!      degrade to the newest published sha), plus every available sha per channel
//!      (the "what versions exist fleet-wide" query).
//!   2. **module × host deployed-sha matrix** — each configured deploy host's
//!      `.deployed_sha` marker (what the constellation-updater wrote), read over the
//!      EXISTING host-reach path (ssh, BatchMode, no new creds). An unreachable host
//!      or missing marker degrades to `unknown` / `undeployed`, never an error.
//!   3. **queue / in-flight** — the compiler job scheduler surface. Until the job
//!      queue (BLD-06) lands these are empty lists (stable shape, not an error).
//!
//! ## Output shape (matches what BLD-16's fleet API reads)
//! `harmony-server/src/fleet.rs::parse_compiler_status` reads, all optional:
//!   - `matrix`: `[{module, host, deployed_sha, current_sha, channel, built_at}]`
//!   - `hosts`:  `[{host, cores?, ram_mb?, …, running_modules?, health}]`
//!   - `current`: `{ "<module>": { "<channel>": "<sha>" } }`
//! plus a raw passthrough of `queue` / `in_flight`. We emit exactly that superset.
//!
//! Every host, path, and template comes from config env — NO infra literals (S1).
//! This is a read-only surface; it holds no secrets and reads none from the env.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::publish::DEFAULT_CHANNEL;

/// Env: `;`-separated deploy targets, each `label|ssh_target` (a bare entry means
/// `label == ssh_target`). Empty/unset → no remote probe (matrix is store-only).
const COMPILER_DEPLOY_HOSTS: &str = "COMPILER_DEPLOY_HOSTS";
/// Env: the deploy-marker path template, `{module}` substituted per module. The
/// updater writes each host's `.deployed_sha` here.
const COMPILER_DEPLOY_MARKER_TEMPLATE: &str = "COMPILER_DEPLOY_MARKER_TEMPLATE";
/// Env: `,`-separated module allow-list. Unset → enumerate the artifact store.
const COMPILER_MODULES: &str = "COMPILER_MODULES";
/// Env: ssh connect/read timeout seconds for the (bounded) marker probe.
const COMPILER_DEPLOY_SSH_TIMEOUT_SECS: &str = "COMPILER_DEPLOY_SSH_TIMEOUT_SECS";

/// The conventional deploy-marker path (FHS `/opt/<module>` convention, matching the
/// constellation-updater and Harmony's `VERSION_MARKER` default). A generic path
/// convention, not an infra identifier; overridable via config.
const DEFAULT_MARKER_TEMPLATE: &str = "/opt/{module}/.deployed_sha";
/// Default bound on each ssh marker read (fail-fast so a dead host can't stall the
/// whole status call).
const DEFAULT_SSH_TIMEOUT_SECS: u64 = 8;
/// The pointer file BLD-07 flips inside each `artifacts/<module>/<channel>/` dir.
const CURRENT_POINTER: &str = "current";
/// Max concurrent ssh marker probes.
const MAX_PROBE_CONCURRENCY: usize = 4;

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ── Deploy-host config ───────────────────────────────────────────────────────

/// One configured deploy target: a display `label` and the `ssh_target` used to
/// read its marker over the existing host-reach path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployHost {
    pub label: String,
    pub ssh_target: String,
}

/// Parse `COMPILER_DEPLOY_HOSTS`: `;`-separated `label|ssh_target` entries. A bare
/// entry (no `|`) uses the same string for both. Blank entries are skipped.
pub fn parse_deploy_hosts(s: &str) -> Vec<DeployHost> {
    s.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('|') {
            Some((label, target)) => DeployHost {
                label: label.trim().to_string(),
                ssh_target: target.trim().to_string(),
            },
            None => DeployHost {
                label: entry.to_string(),
                ssh_target: entry.to_string(),
            },
        })
        .filter(|h| !h.label.is_empty() && !h.ssh_target.is_empty())
        .collect()
}

fn marker_template() -> String {
    env_nonempty(COMPILER_DEPLOY_MARKER_TEMPLATE)
        .unwrap_or_else(|| DEFAULT_MARKER_TEMPLATE.to_string())
}

fn ssh_timeout() -> Duration {
    let secs = env_nonempty(COMPILER_DEPLOY_SSH_TIMEOUT_SECS)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_SSH_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Resolve the marker path for a module from the template (`{module}` substituted).
pub fn marker_path(template: &str, module: &str) -> String {
    template.replace("{module}", module)
}

// ── Deploy marker parsing (tolerant; mirrors the updater/Harmony formats) ────

/// A parsed `.deployed_sha` marker. Every field optional so a partial/blank marker
/// degrades cleanly instead of erroring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeployMarker {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
}

impl DeployMarker {
    fn is_empty(&self) -> bool {
        self.sha.is_none() && self.channel.is_none() && self.built_at.is_none()
    }
}

/// Parse a `.deployed_sha` marker. Accepts, in order of preference:
///   1. a JSON object `{"sha":…, "channel":…, "built_at":…}`,
///   2. `key=value` lines (`sha=`/`deployed_sha=`/`commit=`, `channel=`,
///      `built_at=`/`deployed_at=`/`timestamp=`),
///   3. a single bare sha token.
/// Unrecognized content yields an empty marker (never an error).
pub fn parse_deploy_marker(contents: &str) -> DeployMarker {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return DeployMarker::default();
    }

    // 1. JSON object.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
            let get = |keys: &[&str]| -> Option<String> {
                keys.iter()
                    .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            let m = DeployMarker {
                sha: get(&["sha", "deployed_sha", "commit"]),
                channel: get(&["channel"]),
                built_at: get(&["built_at", "deployed_at", "timestamp"]),
            };
            if !m.is_empty() {
                return m;
            }
        }
    }

    // 2. key=value lines.
    let mut m = DeployMarker::default();
    let mut saw_kv = false;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
            if v.is_empty() {
                continue;
            }
            match k.as_str() {
                "sha" | "deployed_sha" | "commit" => {
                    m.sha = Some(v);
                    saw_kv = true;
                }
                "channel" => {
                    m.channel = Some(v);
                    saw_kv = true;
                }
                "built_at" | "deployed_at" | "timestamp" => {
                    m.built_at = Some(v);
                    saw_kv = true;
                }
                _ => {}
            }
        }
    }
    if saw_kv {
        return m;
    }

    // 3. A single bare sha token (first whitespace-delimited token of a 1-line file).
    if trimmed.lines().count() == 1 {
        if let Some(tok) = trimmed.split_whitespace().next() {
            return DeployMarker {
                sha: Some(tok.to_string()),
                ..Default::default()
            };
        }
    }
    DeployMarker::default()
}

// ── Status derivation (mirrors Harmony's fleet `derive_status`) ──────────────

/// Compare a host's deployed sha against the store's blessed `current` sha. Short
/// and long shas compare by prefix (either may be abbreviated).
///   - no deployed marker            → `undeployed`
///   - deployed but no `current`     → `unknown`
///   - deployed prefix-matches       → `up_to_date`
///   - otherwise                     → `update_available`
pub fn derive_status(deployed: Option<&str>, current: Option<&str>) -> &'static str {
    match (deployed, current) {
        (None, _) => "undeployed",
        (Some(d), _) if d.trim().is_empty() => "undeployed",
        (Some(_), None) => "unknown",
        (Some(_), Some(c)) if c.trim().is_empty() => "unknown",
        (Some(d), Some(c)) => {
            let (d, c) = (d.trim(), c.trim());
            if d.starts_with(c) || c.starts_with(d) {
                "up_to_date"
            } else {
                "update_available"
            }
        }
    }
}

// ── Artifact store read (current pointers + available shas) ──────────────────

/// The store's view for a set of modules: the blessed `current` sha per
/// `(module, channel)` and every available sha per `(module, channel)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreView {
    /// module → channel → blessed current sha.
    pub current: BTreeMap<String, BTreeMap<String, String>>,
    /// module → channel → all published shas.
    pub available: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Modules discovered/queried.
    pub modules: Vec<String>,
}

impl StoreView {
    /// The single blessed sha for a module, flattened across channels the same way
    /// the fleet API flattens `current`: prefer `stable`, then `current`, then the
    /// default publish channel, then any.
    pub fn blessed_sha(&self, module: &str) -> Option<String> {
        let chans = self.current.get(module)?;
        chans
            .get("stable")
            .or_else(|| chans.get("current"))
            .or_else(|| chans.get(DEFAULT_CHANNEL))
            .or_else(|| chans.values().next())
            .cloned()
    }
}

/// Read a channel's `current` pointer. BLD-07 writes it as either a small file
/// containing the sha (optionally a path whose last segment is the sha) or a
/// symlink to the sha dir. Absent → `None` (caller degrades to newest sha).
async fn read_current_pointer(channel_dir: &Path) -> Option<String> {
    let pointer = channel_dir.join(CURRENT_POINTER);
    // Symlink form: target's final component is the sha dir name.
    if let Ok(target) = tokio::fs::read_link(&pointer).await {
        if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // File form: trimmed content; tolerate a `.../<sha>` path by taking the tail.
    if let Ok(body) = tokio::fs::read_to_string(&pointer).await {
        let body = body.trim();
        if !body.is_empty() {
            let tail = body.rsplit('/').next().unwrap_or(body).trim();
            if !tail.is_empty() {
                return Some(tail.to_string());
            }
        }
    }
    None
}

/// List a channel dir's published sha subdirectories (every entry that is a dir and
/// is not the `current` pointer). Sorted for stable output.
async fn list_channel_shas(channel_dir: &Path) -> Vec<String> {
    let mut shas = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(channel_dir).await else {
        return shas;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == CURRENT_POINTER {
            continue;
        }
        if entry
            .file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            shas.push(name.to_string());
        }
    }
    shas.sort();
    shas
}

/// Read the artifact store under `${dataset_root}/artifacts`. When `only` is
/// non-empty, restrict to those modules; otherwise enumerate every module dir.
/// Missing store / channel dirs degrade to empty, never an error.
async fn read_store(dataset_root: &Path, only: &[String]) -> StoreView {
    let mut view = StoreView::default();
    let artifacts = dataset_root.join("artifacts");

    // Discover modules: either the allow-list, or the artifact dir's subdirectories.
    let modules: Vec<String> = if !only.is_empty() {
        only.to_vec()
    } else {
        let mut found = Vec::new();
        if let Ok(mut rd) = tokio::fs::read_dir(&artifacts).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        found.push(name.to_string());
                    }
                }
            }
        }
        found.sort();
        found
    };

    for module in &modules {
        let module_dir = artifacts.join(module);
        let Ok(mut rd) = tokio::fs::read_dir(&module_dir).await else {
            continue;
        };
        let mut channel_current: BTreeMap<String, String> = BTreeMap::new();
        let mut channel_avail: BTreeMap<String, Vec<String>> = BTreeMap::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(channel) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let channel_dir = entry.path();
            let shas = list_channel_shas(&channel_dir).await;
            // Prefer BLD-07's `current` pointer; else degrade to the newest sha we
            // can order (lexical max — good enough absent a pointer, and stable).
            let current = match read_current_pointer(&channel_dir).await {
                Some(sha) => Some(sha),
                None => shas.last().cloned(),
            };
            if let Some(sha) = current {
                channel_current.insert(channel.clone(), sha);
            }
            if !shas.is_empty() {
                channel_avail.insert(channel, shas);
            }
        }
        if !channel_current.is_empty() {
            view.current.insert(module.clone(), channel_current);
        }
        if !channel_avail.is_empty() {
            view.available.insert(module.clone(), channel_avail);
        }
    }

    view.modules = modules;
    view
}

// ── Remote deploy-marker probe (bounded, best-effort) ────────────────────────

/// Render the ssh argv that reads one host's marker file over the existing
/// host-reach path. BatchMode + ConnectTimeout so a dead host fails fast and never
/// prompts; `cat --` guards against an option-like path. No new credentials — this
/// reuses whatever ssh access the build path already relies on.
pub fn render_marker_read_argv(ssh_target: &str, marker: &str, timeout_secs: u64) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={timeout_secs}"),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        ssh_target.to_string(),
        "cat".to_string(),
        "--".to_string(),
        marker.to_string(),
    ]
}

/// Run an argv, returning its stdout on exit 0, else `None` (any spawn/timeout/
/// non-zero exit → best-effort miss, the caller renders the cell `unknown`). The
/// marker holds no secrets, so no redaction machinery is needed here.
async fn capture_stdout(argv: &[String], timeout: Duration) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().ok()?;
    let mut pipe = child.stdout.take();
    let read = async move {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    };
    let out = tokio::spawn(read);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        _ => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            out.abort();
            return None;
        }
    };
    let bytes = out.await.unwrap_or_default();
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// Probe every `(host, module)` marker with bounded concurrency. Returns a map keyed
/// by `(host_label, module)` → parsed marker for the cells that answered; a missing
/// key means unreachable/absent (rendered `unknown`/`undeployed` downstream).
async fn probe_markers(
    hosts: &[DeployHost],
    modules: &[String],
    template: &str,
    timeout: Duration,
) -> BTreeMap<(String, String), DeployMarker> {
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let mut pending = FuturesUnordered::new();
    let mut out = BTreeMap::new();
    let mut queue = Vec::new();
    for host in hosts {
        for module in modules {
            queue.push((host.clone(), module.clone()));
        }
    }
    let mut iter = queue.into_iter();

    // Prime up to the concurrency cap.
    for _ in 0..MAX_PROBE_CONCURRENCY {
        if let Some((host, module)) = iter.next() {
            pending.push(probe_one(host, module, template.to_string(), timeout));
        }
    }
    while let Some(result) = pending.next().await {
        if let Some((host, module, marker)) = result {
            out.insert((host, module), marker);
        }
        if let Some((host, module)) = iter.next() {
            pending.push(probe_one(host, module, template.to_string(), timeout));
        }
    }
    out
}

/// One marker probe. `None` when the host was unreachable or the marker was
/// absent/blank — the cell degrades to `unknown`/`undeployed`, not an error.
async fn probe_one(
    host: DeployHost,
    module: String,
    template: String,
    timeout: Duration,
) -> Option<(String, String, DeployMarker)> {
    let path = marker_path(&template, &module);
    let argv = render_marker_read_argv(&host.ssh_target, &path, timeout.as_secs());
    let body = capture_stdout(&argv, timeout).await?;
    let marker = parse_deploy_marker(&body);
    if marker.is_empty() {
        return None;
    }
    Some((host.label, module, marker))
}

// ── Matrix + payload assembly (pure, offline-testable) ───────────────────────

/// A module × host deployment cell (the exact fields Harmony's fleet API reads,
/// plus a derived `status` for direct consumers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleDeployment {
    pub module: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
    pub status: String,
}

/// Build the module × host matrix from the store view + probed markers. Emits a cell
/// for every `(host, module)` pair; a host/module with no marker degrades to a
/// `deployed_sha`-less `undeployed`/`unknown` cell rather than being dropped.
pub fn assemble_matrix(
    hosts: &[DeployHost],
    modules: &[String],
    markers: &BTreeMap<(String, String), DeployMarker>,
    store: &StoreView,
) -> Vec<ModuleDeployment> {
    let mut rows = Vec::new();
    for host in hosts {
        for module in modules {
            let marker = markers
                .get(&(host.label.clone(), module.clone()))
                .cloned()
                .unwrap_or_default();
            let current_sha = store.blessed_sha(module);
            let status =
                derive_status(marker.sha.as_deref(), current_sha.as_deref()).to_string();
            rows.push(ModuleDeployment {
                module: module.clone(),
                host: host.label.clone(),
                deployed_sha: marker.sha,
                current_sha,
                channel: marker.channel,
                built_at: marker.built_at,
                status,
            });
        }
    }
    rows
}

/// Assemble the full `compiler_status` payload (pure given its inputs, so the exact
/// serialized shape — the contract BLD-16's fleet API parses — is unit-testable).
#[allow(clippy::too_many_arguments)]
pub fn build_payload(
    generated_at: &str,
    store: &StoreView,
    matrix: &[ModuleDeployment],
    host_rows: &[Value],
    queue: Vec<Value>,
    in_flight: Vec<Value>,
    degraded: bool,
    notes: &[String],
) -> Value {
    // `current`: module → channel → sha (the nested form the fleet API flattens).
    let current: BTreeMap<&String, &BTreeMap<String, String>> = store.current.iter().collect();
    json!({
        "generated_at": generated_at,
        "modules": store.modules,
        "current": current,
        "available": store.available,
        "matrix": matrix,
        "hosts": host_rows,
        "queue": queue,
        "in_flight": in_flight,
        "degraded": degraded,
        "notes": notes,
    })
}

/// A short human summary for the tool's `text` channel.
fn summarize(store: &StoreView, matrix: &[ModuleDeployment], degraded: bool) -> String {
    let deployed = matrix
        .iter()
        .filter(|m| m.deployed_sha.is_some())
        .count();
    format!(
        "compiler_status: {} module(s), {} store pointer(s), {} matrix cell(s) ({} deployed){}",
        store.modules.len(),
        store.current.values().map(|c| c.len()).sum::<usize>(),
        matrix.len(),
        deployed,
        if degraded { " [degraded]" } else { "" }
    )
}

// ── The tool ─────────────────────────────────────────────────────────────────

/// The `compiler_status` tool.
struct CompilerStatus;

#[async_trait]
impl RustTool for CompilerStatus {
    fn name(&self) -> &str {
        "compiler_status"
    }

    fn description(&self) -> &str {
        "Read the compiler's fleet version state: the artifact store's `current` sha \
         per (module, channel), every available published sha, and a module×host \
         deployed-sha matrix read from each deploy host's `.deployed_sha` marker over \
         the existing host-reach path. Plus queue/in-flight builds. An unreachable \
         host or missing marker degrades to unknown/undeployed, never an error."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module": {
                    "type": "string",
                    "description": "Restrict to a single module (default: all modules in the store / config)."
                },
                "probe_hosts": {
                    "type": "boolean",
                    "default": true,
                    "description": "Probe each configured deploy host's marker over ssh. false → store pointers only (no remote reads)."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let mut notes: Vec<String> = Vec::new();
        let mut degraded = false;

        // Module filter: an explicit arg, else the `COMPILER_MODULES` allow-list,
        // else empty (→ enumerate the store).
        let only: Vec<String> = match args.get("module").and_then(Value::as_str) {
            Some(m) if !m.trim().is_empty() => vec![m.trim().to_string()],
            _ => env_nonempty(COMPILER_MODULES)
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        };
        let probe = args
            .get("probe_hosts")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        // Artifact store (degrade gracefully when unconfigured).
        let store = match super::dataset_root() {
            Ok(root) => read_store(&root, &only).await,
            Err(_) => {
                degraded = true;
                notes.push(
                    "artifact store unavailable (BUILD_DATASET_ROOT unset) — no store pointers"
                        .to_string(),
                );
                StoreView {
                    modules: only.clone(),
                    ..Default::default()
                }
            }
        };

        // Deploy hosts.
        let hosts = env_nonempty(COMPILER_DEPLOY_HOSTS)
            .map(|s| parse_deploy_hosts(&s))
            .unwrap_or_default();

        // The module set the matrix spans: the store's modules, or the filter.
        let matrix_modules: Vec<String> = if !store.modules.is_empty() {
            store.modules.clone()
        } else {
            only.clone()
        };

        // Remote marker probe (bounded, best-effort).
        let markers = if probe && !hosts.is_empty() && !matrix_modules.is_empty() {
            let template = marker_template();
            probe_markers(&hosts, &matrix_modules, &template, ssh_timeout()).await
        } else {
            if hosts.is_empty() {
                notes.push(format!(
                    "{COMPILER_DEPLOY_HOSTS} unset — no deploy-host matrix (store pointers only)"
                ));
            } else if !probe {
                notes.push("host probe disabled (probe_hosts=false)".to_string());
            }
            BTreeMap::new()
        };

        let matrix = assemble_matrix(&hosts, &matrix_modules, &markers, &store);
        // A cell we couldn't read (host configured but no marker answered) means a
        // partial matrix → degraded.
        if probe && !hosts.is_empty() {
            let answered = markers.len();
            let expected = hosts.len() * matrix_modules.len();
            if answered < expected {
                degraded = true;
                notes.push(format!(
                    "deploy matrix partial: {answered}/{expected} host×module marker(s) answered"
                ));
            }
        }

        // Host rows (the fleet `hosts` shape). Reachability from whether any of the
        // host's markers answered.
        let host_rows: Vec<Value> = hosts
            .iter()
            .map(|h| {
                let reachable = matrix_modules
                    .iter()
                    .any(|m| markers.contains_key(&(h.label.clone(), m.clone())));
                let health = if !probe {
                    "unknown"
                } else if reachable {
                    "ok"
                } else {
                    "unknown"
                };
                json!({
                    "host": h.label,
                    "health": health,
                    "source": "compiler",
                })
            })
            .collect();

        // Queue / in-flight: the job scheduler (BLD-06) is not wired yet — a stable
        // empty shape, explicitly noted, not an error.
        let queue: Vec<Value> = Vec::new();
        let in_flight: Vec<Value> = Vec::new();
        notes.push("build queue/in-flight surface pending the job scheduler (BLD-06)".to_string());

        let generated_at = chrono::Utc::now().to_rfc3339();
        let payload = build_payload(
            &generated_at,
            &store,
            &matrix,
            &host_rows,
            queue,
            in_flight,
            degraded,
            &notes,
        );
        let text = summarize(&store, &matrix, degraded);
        Ok(ToolOutput::with_structured(text, payload))
    }
}

/// Register the `compiler_status` tool on the registry.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(CompilerStatus)) {
        tracing::error!("compiler: failed to register compiler_status: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_deploy_hosts_label_and_bare() {
        let hosts = parse_deploy_hosts("host-a|deploy@host-a; host-b|deploy@host-b ; solo");
        assert_eq!(
            hosts,
            vec![
                DeployHost {
                    label: "host-a".into(),
                    ssh_target: "deploy@host-a".into()
                },
                DeployHost {
                    label: "host-b".into(),
                    ssh_target: "deploy@host-b".into()
                },
                DeployHost {
                    label: "solo".into(),
                    ssh_target: "solo".into()
                },
            ]
        );
    }

    #[test]
    fn parse_deploy_hosts_empty_and_blank() {
        assert!(parse_deploy_hosts("").is_empty());
        assert!(parse_deploy_hosts("  ;  ; ").is_empty());
    }

    #[test]
    fn marker_path_substitutes_module() {
        assert_eq!(
            marker_path("/opt/{module}/.deployed_sha", "chord"),
            "<path>/.deployed_sha"
        );
    }

    #[test]
    fn parse_marker_bare_sha() {
        let m = parse_deploy_marker("deadbeefcafe\n");
        assert_eq!(m.sha.as_deref(), Some("deadbeefcafe"));
        assert!(m.channel.is_none() && m.built_at.is_none());
    }

    #[test]
    fn parse_marker_key_value() {
        let m = parse_deploy_marker(
            "sha=abc123\nchannel=stable\nbuilt_at=2026-07-12T00:00:00Z\n# comment\n",
        );
        assert_eq!(m.sha.as_deref(), Some("abc123"));
        assert_eq!(m.channel.as_deref(), Some("stable"));
        assert_eq!(m.built_at.as_deref(), Some("2026-07-12T00:00:00Z"));
    }

    #[test]
    fn parse_marker_json_with_aliases() {
        let m = parse_deploy_marker(
            r#"{"deployed_sha":"ff00","channel":"experimental","deployed_at":"t"}"#,
        );
        assert_eq!(m.sha.as_deref(), Some("ff00"));
        assert_eq!(m.channel.as_deref(), Some("experimental"));
        assert_eq!(m.built_at.as_deref(), Some("t"));
    }

    #[test]
    fn parse_marker_empty_is_empty() {
        assert!(parse_deploy_marker("   \n  ").is_empty());
        assert!(parse_deploy_marker("").is_empty());
    }

    #[test]
    fn derive_status_all_cases() {
        assert_eq!(derive_status(None, Some("abc")), "undeployed");
        assert_eq!(derive_status(Some(""), Some("abc")), "undeployed");
        assert_eq!(derive_status(Some("abc"), None), "unknown");
        assert_eq!(derive_status(Some("abc"), Some("")), "unknown");
        assert_eq!(derive_status(Some("abcdef"), Some("abc")), "up_to_date");
        assert_eq!(derive_status(Some("abc"), Some("abcdef")), "up_to_date");
        assert_eq!(derive_status(Some("abc"), Some("xyz")), "update_available");
    }

    #[test]
    fn render_marker_read_argv_is_batchmode_and_guarded() {
        let argv = render_marker_read_argv("deploy@host", "<path>/.deployed_sha", 8);
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "ConnectTimeout=8"));
        // `cat --` guards against an option-like path; the path is the final arg.
        let cat = argv.iter().position(|a| a == "cat").unwrap();
        assert_eq!(argv[cat + 1], "--");
        assert_eq!(argv.last().unwrap(), "<path>/.deployed_sha");
        assert!(argv.iter().any(|a| a == "deploy@host"));
    }

    fn hosts() -> Vec<DeployHost> {
        vec![
            DeployHost {
                label: "host-a".into(),
                ssh_target: "u@host-a".into(),
            },
            DeployHost {
                label: "host-b".into(),
                ssh_target: "u@host-b".into(),
            },
        ]
    }

    fn store_with(module: &str, channel: &str, current: &str) -> StoreView {
        let mut v = StoreView::default();
        v.modules = vec![module.to_string()];
        v.current.insert(
            module.to_string(),
            BTreeMap::from([(channel.to_string(), current.to_string())]),
        );
        v
    }

    #[test]
    fn blessed_sha_prefers_stable() {
        let mut v = StoreView::default();
        v.current.insert(
            "chord".into(),
            BTreeMap::from([
                ("experimental".into(), "exp".into()),
                ("stable".into(), "stab".into()),
            ]),
        );
        assert_eq!(v.blessed_sha("chord").as_deref(), Some("stab"));
        assert_eq!(v.blessed_sha("absent"), None);
    }

    #[test]
    fn assemble_matrix_covers_every_host_module_and_derives_status() {
        let hosts = hosts();
        let modules = vec!["chord".to_string()];
        let store = store_with("chord", "stable", "abcdef");
        // <host> up-to-date, ct327 has an old sha, no marker for a would-be third host.
        let mut markers = BTreeMap::new();
        markers.insert(
            ("host-a".to_string(), "chord".to_string()),
            DeployMarker {
                sha: Some("abcdef123".into()),
                channel: Some("stable".into()),
                built_at: Some("t1".into()),
            },
        );
        markers.insert(
            ("host-b".to_string(), "chord".to_string()),
            DeployMarker {
                sha: Some("999999".into()),
                channel: Some("stable".into()),
                built_at: None,
            },
        );
        let matrix = assemble_matrix(&hosts, &modules, &markers, &store);
        assert_eq!(matrix.len(), 2);
        let <host> = matrix.iter().find(|m| m.host == "host-a").unwrap();
        assert_eq!(<host>.status, "up_to_date");
        assert_eq!(<host>.current_sha.as_deref(), Some("abcdef"));
        let ct327 = matrix.iter().find(|m| m.host == "host-b").unwrap();
        assert_eq!(ct327.status, "update_available");
    }

    #[test]
    fn assemble_matrix_missing_marker_is_undeployed_not_dropped() {
        let hosts = hosts();
        let modules = vec!["chord".to_string()];
        let store = store_with("chord", "stable", "abcdef");
        let markers = BTreeMap::new(); // nothing answered
        let matrix = assemble_matrix(&hosts, &modules, &markers, &store);
        assert_eq!(matrix.len(), 2, "both cells present even with no markers");
        assert!(matrix.iter().all(|m| m.status == "undeployed"));
        assert!(matrix.iter().all(|m| m.deployed_sha.is_none()));
    }

    #[test]
    fn payload_shape_matches_fleet_api_contract() {
        // The exact keys BLD-16's `parse_compiler_status` reads.
        let store = store_with("chord", "stable", "abcdef");
        let markers = BTreeMap::from([(
            ("host-a".to_string(), "chord".to_string()),
            DeployMarker {
                sha: Some("abcdef123".into()),
                channel: Some("stable".into()),
                built_at: Some("t1".into()),
            },
        )]);
        let hosts = vec![DeployHost {
            label: "host-a".into(),
            ssh_target: "u@host-a".into(),
        }];
        let modules = vec!["chord".to_string()];
        let matrix = assemble_matrix(&hosts, &modules, &markers, &store);
        let host_rows = vec![json!({"host": "host-a", "health": "ok", "source": "compiler"})];
        let payload = build_payload(
            "2026-07-12T00:00:00Z",
            &store,
            &matrix,
            &host_rows,
            Vec::new(),
            Vec::new(),
            false,
            &["ok".to_string()],
        );

        // `current`: module → channel → sha (fleet flattens this).
        assert_eq!(payload["current"]["chord"]["stable"], json!("abcdef"));
        // `matrix`: rows with the fleet's exact field names.
        let row = &payload["matrix"][0];
        assert_eq!(row["module"], json!("chord"));
        assert_eq!(row["host"], json!("host-a"));
        assert_eq!(row["deployed_sha"], json!("abcdef123"));
        assert_eq!(row["current_sha"], json!("abcdef"));
        assert_eq!(row["channel"], json!("stable"));
        assert_eq!(row["built_at"], json!("t1"));
        // `hosts` + passthrough keys present.
        assert_eq!(payload["hosts"][0]["host"], json!("host-a"));
        assert!(payload["queue"].is_array());
        assert!(payload["in_flight"].is_array());
        assert_eq!(payload["degraded"], json!(false));
    }

    #[tokio::test]
    async fn read_store_reads_current_pointer_and_lists_shas() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chan = root.join("artifacts/chord/experimental");
        tokio::fs::create_dir_all(chan.join("aaa/t/chord"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(chan.join("bbb/t/chord"))
            .await
            .unwrap();
        // BLD-07 `current` pointer file → sha `bbb`.
        tokio::fs::write(chan.join("current"), "bbb\n").await.unwrap();

        let view = read_store(root, &["chord".to_string()]).await;
        assert_eq!(
            view.current["chord"]["experimental"], "bbb",
            "current pointer wins"
        );
        assert_eq!(
            view.available["chord"]["experimental"],
            vec!["aaa".to_string(), "bbb".to_string()]
        );
        assert_eq!(view.blessed_sha("chord").as_deref(), Some("bbb"));
    }

    #[tokio::test]
    async fn read_store_degrades_to_newest_sha_when_no_current_pointer() {
        // BLD-07 not landed: no `current` file → newest (lexical-max) sha is used.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chan = root.join("artifacts/harmony/stable");
        tokio::fs::create_dir_all(chan.join("v1/t/harmony"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(chan.join("v2/t/harmony"))
            .await
            .unwrap();

        let view = read_store(root, &["harmony".to_string()]).await;
        assert_eq!(
            view.current["harmony"]["stable"], "v2",
            "degrades to newest sha with no pointer"
        );
    }

    #[tokio::test]
    async fn read_store_absent_store_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        // No `artifacts/` dir at all.
        let view = read_store(dir.path(), &["chord".to_string()]).await;
        assert!(view.current.is_empty());
        assert!(view.available.is_empty());
        assert_eq!(view.modules, vec!["chord".to_string()]);
        assert_eq!(view.blessed_sha("chord"), None);
    }

    #[tokio::test]
    async fn read_store_enumerates_modules_when_no_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::create_dir_all(root.join("artifacts/chord/stable/s/t/chord"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join("artifacts/harmony/stable/s/t/harmony"))
            .await
            .unwrap();
        let view = read_store(root, &[]).await;
        assert_eq!(
            view.modules,
            vec!["chord".to_string(), "harmony".to_string()]
        );
    }

    #[tokio::test]
    async fn read_store_reads_symlink_current_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chan = root.join("artifacts/chord/experimental");
        tokio::fs::create_dir_all(chan.join("realsha/t/chord"))
            .await
            .unwrap();
        // Symlink `current` → the sha dir (the alternative BLD-07 form).
        tokio::fs::symlink(chan.join("realsha"), chan.join("current"))
            .await
            .unwrap();
        let view = read_store(root, &["chord".to_string()]).await;
        assert_eq!(view.current["chord"]["experimental"], "realsha");
    }

    #[tokio::test]
    async fn capture_stdout_unreachable_returns_none() {
        // A command that exits non-zero → None (best-effort miss).
        let argv = vec!["sh".to_string(), "-c".to_string(), "exit 3".to_string()];
        assert!(capture_stdout(&argv, Duration::from_secs(2)).await.is_none());
    }

    #[tokio::test]
    async fn capture_stdout_success_returns_stdout() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'sha=abc\\n'".to_string(),
        ];
        let out = capture_stdout(&argv, Duration::from_secs(2)).await.unwrap();
        assert_eq!(parse_deploy_marker(&out).sha.as_deref(), Some("abc"));
    }
}
