//! BLD-05 — the `compiler_build` Terminus tool: the single build door.
//!
//! `compiler_build(module, ref, host="auto", profile="release", fast=false)`
//! selects a build host, ensures the pinned toolchain, runs an sccache-backed
//! `cargo` build inside a resource-capped systemd scope (`MemorySwapMax=0` — Plex
//! protection), and publishes a SHA-256-checksummed artifact into the shared
//! build dataset. On a local publish it also flips `experimental/current` onto the
//! new sha (BLD-07 store); promotion to `stable` is `compiler_release` (no rebuild).
//!
//! The keystone of the S117 constellation CI/CD. Submodules:
//!   - [`host`]    — primary-vs-heavy selection from RAM/module-size heuristics.
//!   - [`scope`]   — the `systemd-run --scope` cap rendering + the CARGO_TARGET_DIR
//!                   guard (never the file-level NFS dir).
//!   - [`sccache`] — sccache→Redis env wiring (fail-open to a local dir).
//!   - [`publish`] — content-addressed artifact layout + sha256 + sidecar.
//!
//! ## Discipline (S1/S7)
//! Every host, path, cap, threshold, and cache endpoint comes from config env
//! vars — materialized from the vault where sensitive (`SCCACHE_REDIS`), never a
//! literal in source. Nothing token/URL-with-creds shaped is read outside the
//! sccache secret wiring, and the parsed password never logs.

pub mod host;
pub mod publish;
pub mod sccache;
pub mod scope;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use host::HostRequest;

/// Env var naming the shared build dataset root (appdata-backed NFS share).
const BUILD_DATASET_ROOT: &str = "BUILD_DATASET_ROOT";
/// Env var for the LOCAL/tmpfs exec-safe cargo target dir; defaults to a temp
/// dir when unset (NEVER the NFS dataset — enforced by the target-dir guard).
const BUILD_LOCAL_TARGET_DIR: &str = "BUILD_LOCAL_TARGET_DIR";
/// Env var for the build target triple; defaults to the musl static target that
/// `rust-toolchain.toml` pins (a target triple, not an infra literal).
const BUILD_TARGET_TRIPLE: &str = "BUILD_TARGET_TRIPLE";
/// Env var for the pinned rustc channel to ensure-install (BLD-02). Optional —
/// when unset, rustup auto-installs from the source dir's `rust-toolchain.toml`.
const RUST_TOOLCHAIN_PINNED: &str = "RUST_TOOLCHAIN_PINNED";
/// Env var: a relay host (`user@host`) that has the dataset mounted RW, used
/// when this build host lacks the mount (interim publish path, pre-BLD-01).
const BUILD_DATASET_RELAY_HOST: &str = "BUILD_DATASET_RELAY_HOST";
/// Env var: the dataset root PATH on the relay host (defaults to the local
/// `BUILD_DATASET_ROOT` value when unset — same share, same layout).
const BUILD_DATASET_RELAY_ROOT: &str = "BUILD_DATASET_RELAY_ROOT";
/// Env var: the exec-safe LOCAL/tmpfs cargo target dir ON THE HEAVY host (used
/// for the remote build). Required for a heavy build (a target dir on the shared
/// NFS dataset would break exec — the same guard applies remotely).
const BUILD_HEAVY_LOCAL_TARGET_DIR: &str = "BUILD_HEAVY_LOCAL_TARGET_DIR";
/// Env var: the dataset root PATH on the heavy host (where source is staged +
/// where the remote build's env-file lives under the target dir). Defaults to
/// `BUILD_DATASET_RELAY_ROOT`, else the local `BUILD_DATASET_ROOT`.
const BUILD_HEAVY_DATASET_ROOT: &str = "BUILD_HEAVY_DATASET_ROOT";
/// Env var: extra `:`-separated roots a caller-supplied `source_dir` may live
/// under, ON TOP OF the always-allowed `${BUILD_DATASET_ROOT}/src` tree. Lets an
/// operator permit a dedicated staging mount without opening arbitrary paths.
const BUILD_ALLOWED_SOURCE_ROOTS: &str = "BUILD_ALLOWED_SOURCE_ROOTS";
/// Env var (BLD-07): the number of sha dirs the store retains per channel when
/// pruning after a bless/promote. The store floors this at 2 regardless.
const BUILD_RETAIN_PER_CHANNEL: &str = "BUILD_RETAIN_PER_CHANNEL";

const DEFAULT_TARGET_TRIPLE: &str = "x86_64-unknown-linux-musl";

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The configured shared build dataset root. `NotConfigured` when unset — the
/// compiler cannot publish without it.
fn dataset_root() -> Result<PathBuf, ToolError> {
    env_nonempty(BUILD_DATASET_ROOT)
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::NotConfigured(format!("{BUILD_DATASET_ROOT} is not configured")))
}

/// The LOCAL/tmpfs exec-safe cargo target dir. Defaults to a stable temp path so
/// a build never accidentally targets the NFS dataset; the guard re-checks it.
fn local_target_dir() -> PathBuf {
    env_nonempty(BUILD_LOCAL_TARGET_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("terminus-build-target"))
}

fn target_triple() -> String {
    env_nonempty(BUILD_TARGET_TRIPLE).unwrap_or_else(|| DEFAULT_TARGET_TRIPLE.to_string())
}

/// The per-channel retention count for the artifact store's pruning (BLD-07).
/// Config-driven and floored at 2 — the store never keeps fewer than 2 shas nor
/// prunes the current/previous pointer targets.
fn retain_per_channel() -> usize {
    env_nonempty(BUILD_RETAIN_PER_CHANNEL)
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.max(2))
        .unwrap_or(publish::DEFAULT_RETAIN_PER_CHANNEL)
}

/// The exec-safe LOCAL/tmpfs cargo target dir on the HEAVY host. Required for a
/// remote build — there is deliberately NO default (a wrong default could put the
/// live target on the shared NFS dataset; the operator sizes it per host).
fn heavy_local_target_dir() -> Result<PathBuf, ToolError> {
    env_nonempty(BUILD_HEAVY_LOCAL_TARGET_DIR)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "{BUILD_HEAVY_LOCAL_TARGET_DIR} is required for a heavy (remote) build"
            ))
        })
}

/// The dataset root PATH on the heavy host (source-stage + env-file location).
/// Prefers `BUILD_HEAVY_DATASET_ROOT`, then `BUILD_DATASET_RELAY_ROOT`, then the
/// local `BUILD_DATASET_ROOT` value.
fn heavy_dataset_root(local_root: &str) -> String {
    env_nonempty(BUILD_HEAVY_DATASET_ROOT)
        .or_else(|| env_nonempty(BUILD_DATASET_RELAY_ROOT))
        .unwrap_or_else(|| local_root.to_string())
}

/// Single-quote-escape one shell argument so it can be embedded in a remote
/// command string passed to `ssh` (which runs its argument through the remote
/// login shell). `'` → `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join an argv into a single shell command string (each element quoted).
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write `body` to a fresh **0600** file under the system temp dir and return its
/// path. Used to STAGE the remote secret env file before transfer.
///
/// SECURITY (S7, symlink/predictable-tmp attack): the filename carries an
/// unguessable random component (a v4 UUID, OS-CSPRNG-backed) so an attacker on a
/// multi-user build host cannot pre-plant a file or symlink at a predictable path;
/// and the file is opened with **`O_EXCL`** (`create_new` — an existing path is a
/// hard error, never a truncate/overwrite) **+ `O_NOFOLLOW`** (a symlink at the
/// path is not followed). Because `O_EXCL` guarantees a brand-new file, the
/// `mode(0o600)` applies from creation — the "0600-from-creation" guarantee
/// genuinely holds. On write failure the partial file is unlinked. The caller
/// unlinks it after transfer (on both success and error paths).
fn write_local_0600(body: &str, tag: &str) -> Result<PathBuf, ToolError> {
    let path = std::env::temp_dir().join(format!(
        "terminus-build-secret-{tag}-{}.env",
        uuid::Uuid::new_v4()
    ));
    write_secret_0600_at(&path, body)?;
    Ok(path)
}

/// Exclusively create `path` with mode 0600, refusing to follow a symlink or
/// touch an existing path, and write `body`. The load-bearing security core of
/// [`write_local_0600`], split out so the O_EXCL/O_NOFOLLOW semantics are
/// directly testable at a known path.
fn write_secret_0600_at(path: &std::path::Path, body: &str) -> Result<(), ToolError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL — never open/truncate an existing path
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC) // don't follow a symlink
        .mode(0o600) // applies because O_EXCL guarantees a brand-new file
        .open(path)
        .map_err(|e| {
            ToolError::Execution(format!("create exclusive 0600 secret staging file: {e}"))
        })?;
    if let Err(e) = f.write_all(body.as_bytes()) {
        // Never leave a partial secret file behind on a write error.
        let _ = std::fs::remove_file(path);
        return Err(ToolError::Execution(format!(
            "write secret staging file: {e}"
        )));
    }
    Ok(())
}

/// Map a profile name to (the cargo flag(s) that select it, the target subdir it
/// lands in). `debug` ⇒ no flag / `debug`; `release` ⇒ `--release` / `release`;
/// any other name ⇒ `--profile <name>` / `<name>`.
fn profile_flags_and_subdir(profile: &str) -> (Vec<String>, String) {
    match profile {
        "debug" => (vec![], "debug".to_string()),
        "release" => (vec!["--release".to_string()], "release".to_string()),
        other => (
            vec!["--profile".to_string(), other.to_string()],
            other.to_string(),
        ),
    }
}

/// Build the `cargo build` argv (pure — testable). `bin` selects a single
/// binary target (defaults to the module name); `--locked` keeps the build
/// reproducible against the committed lockfile. `manifest_path` points cargo at
/// the source tree's `Cargo.toml` so the build is independent of the process
/// CWD — which is what makes the REMOTE (ssh) heavy path correct (the scoped
/// cargo need not rely on an ssh working directory).
fn cargo_build_argv(
    profile: &str,
    triple: &str,
    jobs: u32,
    bin: &str,
    manifest_path: &str,
) -> Vec<String> {
    let (profile_flags, _subdir) = profile_flags_and_subdir(profile);
    let mut argv = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--locked".to_string(),
    ];
    argv.extend(profile_flags);
    argv.push("--manifest-path".to_string());
    argv.push(manifest_path.to_string());
    argv.push("--target".to_string());
    argv.push(triple.to_string());
    argv.push("-j".to_string());
    argv.push(jobs.to_string());
    argv.push("--bin".to_string());
    argv.push(bin.to_string());
    argv
}

/// The path (relative to CARGO_TARGET_DIR) where the built binary lands:
/// `<triple>/<profile-subdir>/<bin>`.
fn built_bin_rel(triple: &str, profile: &str, bin: &str) -> PathBuf {
    let (_flags, subdir) = profile_flags_and_subdir(profile);
    PathBuf::from(triple).join(subdir).join(bin)
}

/// Replace every non-empty secret value in `text` with a fixed placeholder, so a
/// secret that a build script / proc-macro / wrapper / sub-tool echoed to
/// stdout/stderr never reaches a `ToolError`, a log line, or a returned string
/// (S7). Plain substring replace of each raw value; empty values are skipped;
/// an empty `secrets` set is a no-op. This helper never logs the secret itself.
fn redact_secrets(text: &str, secrets: &[String]) -> String {
    // Replace LONGEST values first: if one secret is a substring of another (the
    // `SCCACHE_REDIS_PASSWORD` value is embedded in the full `SCCACHE_REDIS` URL),
    // redacting the short one first would break the longer match and leak the
    // URL's non-password parts. Longest-first guarantees the full value is
    // scrubbed before any of its substrings.
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for s in ordered {
        if out.contains(s) {
            out = out.replace(s, "<redacted>");
        }
    }
    out
}

/// On a REMOTE (ssh heavy) build, killing the LOCAL `ssh` process group does not
/// reach the remote `systemd-run --scope` / `cargo` / `rustc` tree. This carries
/// the info needed to tear that remote tree down by name on timeout: the ssh
/// host and the transient scope's unit name (so `systemctl kill <unit>.scope`
/// terminates the scope + all its descendants remotely).
struct RemoteScopeKill {
    host: String,
    unit: String,
}

/// Render the argv that kills a remote transient scope by unit name over ssh:
/// `systemctl kill --signal=SIGKILL <unit>.scope`, falling back to
/// `systemctl stop <unit>.scope`. Pure (returns the argv) so it is testable
/// offline; the unit is shell-quoted for the remote shell.
fn render_remote_scope_kill_argv(host: &str, unit: &str) -> Vec<String> {
    let scope = shell_quote(&format!("{unit}.scope"));
    vec![
        "ssh".to_string(),
        host.to_string(),
        format!("systemctl kill --signal=SIGKILL {scope} || systemctl stop {scope}"),
    ]
}

/// Best-effort remote scope kill (own short timeout, non-fatal). Spawned when a
/// remote heavy build times out, so the remote build tree does not keep running
/// (and keep the secret-bearing inherited env alive) after the tool returns.
///
/// SECURITY (S7): the SAME `redact` set as the build is threaded into the cleanup
/// `run()` — this ssh/systemctl child inherits the parent process env (including
/// ambient `SCCACHE_REDIS`), so a failing cleanup command could otherwise surface
/// an unredacted secret in the returned error we log at `warn!` below.
async fn remote_scope_kill(rk: &RemoteScopeKill, redact: &[String]) {
    let argv = render_remote_scope_kill_argv(&rk.host, &rk.unit);
    // Reuse `run` with no further remote-kill (None) and a small timeout; ignore
    // the outcome — this is cleanup, the caller already returns the timeout error.
    // `Box::pin` breaks the `run`↔`remote_scope_kill` async recursion cycle (the
    // `None` remote_kill above means this never actually recurses at runtime).
    if let Err(e) = Box::pin(run(
        &argv,
        None,
        &BTreeMap::new(),
        Duration::from_secs(30),
        redact,
        None,
    ))
    .await
    {
        tracing::warn!(
            "compiler: best-effort remote scope kill of {}.scope failed: {e}",
            rk.unit
        );
    }
}

/// Render the argv that removes the remote 0600 secret env file over ssh:
/// `ssh -o BatchMode=yes -o ConnectTimeout=10 <host> rm -f <quoted-path>`. Pure
/// (returns the argv) so it is testable offline; the path is shell-quoted for the
/// remote shell, and the ssh options bound a hung connect (so a synchronous Drop
/// cleanup can never block indefinitely).
fn render_remote_secret_rm_argv(host: &str, remote_path: &str) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        host.to_string(),
        format!("rm -f {}", shell_quote(remote_path)),
    ]
}

/// Synchronous, bounded, best-effort remote `rm -f` of the secret env file — used
/// by [`RemoteSecretGuard`]'s `Drop` (which cannot run async). `ssh -o
/// ConnectTimeout` bounds a hung connect; the `rm` itself is instant. Any failure
/// output is redacted (S7) before it is logged.
fn blocking_ssh_rm(argv: &[String], redact: &[String]) {
    use std::process::{Command, Stdio};
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match child {
        Ok(c) => match c.wait_with_output() {
            Ok(out) if !out.status.success() => {
                let tail = redact_secrets(&String::from_utf8_lossy(&out.stderr), redact);
                tracing::warn!("compiler: remote secret-file cleanup rm failed: {tail}");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("compiler: remote secret-file cleanup wait failed: {e}"),
        },
        Err(e) => tracing::warn!("compiler: remote secret-file cleanup spawn failed: {e}"),
    }
}

/// RAII guard that GUARANTEES the secret env file is removed on EVERY post-transfer
/// exit path — success, any `?` error, a timeout, or a panic — closing the whole
/// leak class (not just one code path). Armed right after the secret file is
/// transferred to the remote; its `Drop` issues a bounded best-effort remote
/// `rm -f` (and, as a backstop, unlinks the local staging file if it wasn't
/// already). On the happy path the remote build's own wrapper `rm`s the file, so
/// the guard is [`disarm`](Self::disarm)ed after a successful build to avoid a
/// redundant ssh; on any earlier exit it stays armed and fires.
struct RemoteSecretGuard {
    host: String,
    remote_path: String,
    redact: Vec<String>,
    /// Local staging file to unlink as a backstop (cleared once removed inline).
    local_path: Option<PathBuf>,
    /// When false, `Drop` performs no remote cleanup (the file is already gone).
    armed: bool,
    /// Test-only sink: when set, `Drop` RECORDS the rendered rm argv here instead
    /// of spawning a real ssh — so the "cleanup fires on the error path" property
    /// is unit-testable offline. `None` in production.
    recorder: Option<std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>>,
}

impl RemoteSecretGuard {
    fn new(
        host: String,
        remote_path: String,
        local_path: Option<PathBuf>,
        redact: Vec<String>,
    ) -> Self {
        Self {
            host,
            remote_path,
            redact,
            local_path,
            armed: true,
            recorder: None,
        }
    }

    /// Clear the local-staging backstop after it has been unlinked inline (so
    /// `Drop` doesn't try again — harmless either way).
    fn clear_local(&mut self) {
        self.local_path = None;
    }

    /// Disarm the REMOTE cleanup (call after a successful build, whose own wrapper
    /// already removed the remote file). The local backstop is still honored.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoteSecretGuard {
    fn drop(&mut self) {
        // Local staging backstop (instant, sync) — always, even when disarmed.
        if let Some(p) = self.local_path.take() {
            let _ = std::fs::remove_file(&p);
        }
        if !self.armed {
            return;
        }
        let argv = render_remote_secret_rm_argv(&self.host, &self.remote_path);
        if let Some(rec) = &self.recorder {
            if let Ok(mut g) = rec.lock() {
                g.push(argv);
            }
            return;
        }
        blocking_ssh_rm(&argv, &self.redact);
    }
}

/// Run a subprocess argv with an optional cwd + extra env, bounded by `timeout`.
/// Returns `Ok(stdout)` on success (exit 0), else an `Execution` error with a
/// trimmed stderr tail. The env is applied on top of the inherited environment.
///
/// SECURITY (S7): ALL captured child output (the success stdout AND the failure
/// stderr tail) is passed through [`redact_secrets`] with `redact` — the set of
/// secret env VALUES in play for this build — BEFORE it is returned or surfaced,
/// so a build script that prints its environment can never leak
/// `SCCACHE_REDIS_PASSWORD` / the `SCCACHE_REDIS` URL into an error or log. This
/// is the single choke point covering both the local and remote (ssh) paths.
///
/// PROCESS LIFECYCLE: the child is spawned in its OWN process group
/// (`process_group(0)` ⇒ pgid == child pid) with `kill_on_drop(true)`. On timeout
/// the WHOLE LOCAL group is `killpg(SIGKILL)`-ed (so a local build tree —
/// systemd-run and its `cargo`/`rustc` descendants — dies, not just the direct
/// child), then the direct child is `start_kill`-ed and `wait`-ed to REAP it (no
/// zombie). `kill_on_drop` guarantees any early return / panic also tears the
/// child down.
///
/// REMOTE builds: killing the local `ssh` process group does NOT reach the remote
/// scope. When `remote_kill` is `Some`, a timeout ALSO issues a best-effort
/// `systemctl kill <unit>.scope` over ssh to tear down the remote build tree — so
/// a timed-out heavy build cannot keep running remotely (holding the inherited
/// secret env + capped host resources) after the tool returns.
async fn run(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    redact: &[String],
    remote_kill: Option<&RemoteScopeKill>,
) -> Result<String, ToolError> {
    use tokio::io::AsyncReadExt;
    if argv.is_empty() {
        return Err(ToolError::Execution("empty command".into()));
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Own process group (pgid == child pid) so a timeout can signal the whole
    // build tree; kill_on_drop so an early return also cleans up the child.
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn {}: {e}", argv[0])))?;
    // Capture the pgid up front (== the child pid, from process_group(0)); it is
    // available now because the child has not yet exited.
    let pgid = child.id().map(|p| p as libc::pid_t);

    // Drain stdout/stderr concurrently while we wait, so a chatty child can't
    // deadlock on a full pipe and we still have the output after `wait()`.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(ToolError::Execution(format!("{}: {e}", argv[0]))),
        Err(_) => {
            // TIMEOUT: kill the whole LOCAL process group (the build tree), then
            // reap the direct child so it can never become a zombie or leak.
            if let Some(pgid) = pgid {
                // Safe: killpg takes plain integers and has no memory effects; an
                // ESRCH (already-empty group) is a harmless no-op.
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // REMOTE builds: the local kill only reached `ssh`; tear down the
            // remote scope by name too (best-effort, non-fatal). Thread the same
            // redaction set so a failing cleanup command can't leak a secret.
            if let Some(rk) = remote_kill {
                remote_scope_kill(rk, redact).await;
            }
            return Err(ToolError::Execution(format!(
                "{} timed out after {}s (child process group killed)",
                argv[0],
                timeout.as_secs()
            )));
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    if status.success() {
        // Redact even the success stdout — it is returned to callers and may be
        // logged; a sub-tool could have echoed a secret onto it too.
        Ok(redact_secrets(&String::from_utf8_lossy(&stdout), redact))
    } else {
        let stderr = String::from_utf8_lossy(&stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let tail = redact_secrets(&tail, redact);
        Err(ToolError::Execution(format!(
            "{} exited {}: {tail}",
            argv[0],
            status.code().unwrap_or(-1)
        )))
    }
}

/// The `compiler_build` tool.
struct CompilerBuild;

#[async_trait]
impl RustTool for CompilerBuild {
    fn name(&self) -> &str {
        "compiler_build"
    }

    fn description(&self) -> &str {
        "Build a constellation module at a git ref on a selected build host: pinned \
         toolchain, sccache→Redis (fail-open), inside a resource-capped systemd scope \
         (MemorySwapMax=0, Plex-safe), then publish a sha256-checksummed artifact to the \
         shared build dataset and flip `experimental/current` onto it. Promotion to the \
         `stable` channel is a separate pointer-flip (compiler_release), never a rebuild."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module": {
                    "type": "string",
                    "description": "Module/repo to build (e.g. terminus, chord, harmony, lumina-core)."
                },
                "ref": {
                    "type": "string",
                    "description": "Git ref (sha or branch) being built; used for the source-stage path + scope unit name."
                },
                "host": {
                    "type": "string",
                    "enum": ["auto", "primary", "heavy"],
                    "default": "auto",
                    "description": "Build host role. auto → primary unless the module's known peak or `fast` needs the heavy host."
                },
                "profile": {
                    "type": "string",
                    "default": "release",
                    "description": "Cargo profile: debug | release | <named cargo profile>."
                },
                "fast": {
                    "type": "boolean",
                    "default": false,
                    "description": "Force the heavy host for a full-parallelism build."
                },
                "bin": {
                    "type": "string",
                    "description": "Binary target to build (defaults to the module name)."
                },
                "source_dir": {
                    "type": "string",
                    "description": "Override the source tree location (defaults to ${BUILD_DATASET_ROOT}/src/<module>/<ref>)."
                }
            },
            "required": ["module", "ref"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let module = str_arg(&args, "module")?;
        let git_ref = str_arg(&args, "ref")?;
        let host_req =
            HostRequest::parse(args.get("host").and_then(Value::as_str).unwrap_or("auto"))?;
        let profile = args
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("release")
            .to_string();
        let fast = args.get("fast").and_then(Value::as_bool).unwrap_or(false);
        let bin = args
            .get("bin")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module.clone());

        // ── Validate user-controlled path inputs BEFORE any path join / rsync /
        // ssh (no traversal, no separators, no injection). After this, joining
        // and interpolation are safe. ───────────────────────────────────────
        validate_segment("module", &module)?;
        validate_segment("bin", &bin)?;
        validate_segment("profile", &profile)?;
        validate_git_ref(&git_ref)?;

        // ── Resolve config (fail fast, no side effects) ──────────────────────
        let root = dataset_root()?;
        let root_str = root.to_string_lossy().to_string();
        let resolved = host::resolve(host_req, &module, fast)?;
        let triple = target_triple();
        // `target` (the triple) comes from config but is used as a path segment.
        validate_segment("target", &triple)?;
        // A DETERMINISTIC, UNIQUE transient-scope unit name: `<module>-<ref>` plus
        // a per-invocation uuid so it can never collide with a concurrent build of
        // the same module@ref and is unambiguously addressable for `systemctl kill
        // <unit>.scope` if a (remote) build times out.
        let unit = format!(
            "{}-{}",
            scope::scope_unit_name(&module, &git_ref),
            uuid::Uuid::new_v4().simple()
        );

        // sccache env (fail-open to a local dir if Redis is unconfigured).
        let sccache_env = sccache::resolve(&root_str);

        // Redaction set (S7): the secret VALUES that could be echoed by a child
        // build (a build script printing its env, etc.) and must be scrubbed from
        // ANY captured stdout/stderr before it reaches an error/log. That is every
        // secret-shaped value in the sccache env (SCCACHE_REDIS_PASSWORD, …) PLUS
        // the ambient full `SCCACHE_REDIS` URL (which the child inherits). Passed
        // to `run()` for both the local and remote build paths.
        let mut redact: Vec<String> = sccache_env
            .vars
            .iter()
            .filter(|(k, _)| scope::is_secret_env_key(k))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .collect();
        if let Some(url) = sccache::ambient_secret_url() {
            if !url.is_empty() {
                redact.push(url);
            }
        }
        redact.sort();
        redact.dedup();

        // The local source stage (staged on the shared NFS share is fine — it's a
        // source stage, not the live target). Also the rsync source for a remote
        // build. A caller-supplied `source_dir` is a FULL PATH (not a segment), so
        // it is validated by CONTAINMENT — it must lexically resolve inside an
        // allowed root (the dataset `src` tree, plus any `BUILD_ALLOWED_SOURCE_ROOTS`)
        // BEFORE it is used for current_dir / --manifest-path / rsync, so an
        // absolute-elsewhere or `../`-escaping override can't build/sync source
        // outside the dataset. The default staged path is already safe.
        let local_source_dir = match args.get("source_dir").and_then(Value::as_str) {
            Some(s) => {
                let sd = PathBuf::from(s);
                validate_source_dir(&sd, &root)?;
                sd
            }
            None => root.join("src").join(&module).join(&git_ref),
        };

        // Pinned toolchain channel to ensure (idempotent; never `rustup update`).
        let pinned = env_nonempty(RUST_TOOLCHAIN_PINNED);

        // The build produces a LOCALLY-readable binary at `built_bin` in BOTH the
        // local and remote paths, so the publish step below is host-agnostic.
        let built_bin: PathBuf;

        if resolved.is_local() {
            // ── LOCAL build (primary, in place) ──────────────────────────────
            let target_dir = local_target_dir();
            // GUARD: exec-safe local/tmpfs target, never the file-level NFS dataset.
            scope::validate_target_dir(&target_dir, &root)?;

            let mut build_env = sccache_env.vars.clone();
            build_env.insert(
                "CARGO_TARGET_DIR".to_string(),
                target_dir.to_string_lossy().to_string(),
            );
            // S7: non-secret vars → `--setenv` (argv); secret vars → the INHERITED
            // process environment of systemd-run (which `--scope` passes to the
            // cargo child) — never argv.
            let (setenv, secret_env) = scope::partition_env(&build_env);

            if let Some(channel) = &pinned {
                run(
                    &[
                        "rustup".into(),
                        "toolchain".into(),
                        "install".into(),
                        channel.clone(),
                    ],
                    Some(&local_source_dir),
                    &BTreeMap::new(),
                    Duration::from_secs(600),
                    &redact,
                    None,
                )
                .await?;
            }

            let manifest = local_source_dir.join("Cargo.toml");
            let cargo_argv = cargo_build_argv(
                &profile,
                &triple,
                resolved.caps.jobs,
                &bin,
                &manifest.to_string_lossy(),
            );
            let scope_argv = scope::render_scope_argv(&unit, &resolved.caps, &setenv, &cargo_argv);
            // Secret env is delivered via the inherited environment (last arg),
            // NOT argv.
            run(
                &scope_argv,
                Some(&local_source_dir),
                &secret_env,
                Duration::from_secs(3600),
                &redact,
                None,
            )
            .await?;

            built_bin = target_dir.join(built_bin_rel(&triple, &profile, &bin));
        } else {
            // ── REMOTE build (heavy host, over ssh) ──────────────────────────
            let host_addr = resolved
                .address
                .clone()
                .expect("a non-local resolved host always has an ssh address");
            let remote_root = heavy_dataset_root(&root_str);
            let remote_target = heavy_local_target_dir()?;
            // GUARD applies remotely too: the remote cargo target must be exec-safe,
            // never under the remote NFS dataset.
            scope::validate_target_dir(&remote_target, std::path::Path::new(&remote_root))?;
            let remote_target_str = remote_target.to_string_lossy().to_string();
            let remote_source = format!(
                "{}/src/{}/{}",
                remote_root.trim_end_matches('/'),
                module,
                git_ref
            );

            // Stage source to the remote + ensure the remote dirs exist. Every
            // interpolated remote path is shell-quoted (defense-in-depth on top of
            // the segment validation above), and rsync uses `-s`/--protect-args so
            // the remote path is never re-split by the remote shell.
            run(
                &[
                    "ssh".into(),
                    host_addr.clone(),
                    format!(
                        "mkdir -p {} {}",
                        shell_quote(&remote_source),
                        shell_quote(&remote_target_str)
                    ),
                ],
                None,
                &BTreeMap::new(),
                Duration::from_secs(120),
                &redact,
                None,
            )
            .await?;
            run(
                &[
                    "rsync".into(),
                    "-a".into(),
                    "--delete".into(),
                    "-s".into(),
                    format!("{}/", local_source_dir.to_string_lossy()),
                    format!("{host_addr}:{remote_source}/"),
                ],
                None,
                &BTreeMap::new(),
                Duration::from_secs(1800),
                &redact,
                None,
            )
            .await?;

            let mut build_env = sccache_env.vars.clone();
            build_env.insert("CARGO_TARGET_DIR".to_string(), remote_target_str.clone());
            let (setenv, secret_env) = scope::partition_env(&build_env);

            // Secret env (if any) → a 0600 file ON THE REMOTE, `source`d inside the
            // ssh wrapper before `exec systemd-run` so it reaches the scoped build's
            // inherited env WITHOUT ever touching a command line (S7). The remote
            // filename carries an unguessable random component (defense-in-depth vs
            // a pre-planted file/symlink), matching the local staging file below.
            let remote_env_path = format!(
                "{remote_target_str}/.terminus-build-{unit}-{}.env",
                uuid::Uuid::new_v4()
            );
            let have_secret = !secret_env.is_empty();
            // RAII guard: once the secret file is (about to be) on the remote, its
            // removal is GUARANTEED on every subsequent exit — the happy path, any
            // `?` (e.g. a failing pinned-toolchain install), a timeout, or a panic —
            // via `Drop`. It stays in scope for the whole remote build below (it is
            // disarmed after a successful build, whose own wrapper already `rm`s the
            // file, to avoid a redundant ssh).
            let mut secret_guard: Option<RemoteSecretGuard> = None;
            if have_secret {
                let body = scope::render_secret_env_file(&secret_env);
                let local_secret = write_local_0600(&body, &unit)?;
                // Arm the guard BEFORE the transfer (covers a partial/failed rsync
                // that may still have created the remote file); the local staging
                // file is a Drop backstop until we unlink it inline just below.
                secret_guard = Some(RemoteSecretGuard::new(
                    host_addr.clone(),
                    remote_env_path.clone(),
                    Some(local_secret.clone()),
                    redact.clone(),
                ));
                // `rsync -a` preserves the local 0600 mode on the remote (so the
                // remote secret file is 0600 without a separate chmod), and `-s`
                // protects the remote path from remote-shell re-splitting.
                let xfer_res = run(
                    &[
                        "rsync".into(),
                        "-a".into(),
                        "-s".into(),
                        local_secret.to_string_lossy().to_string(),
                        format!("{host_addr}:{remote_env_path}"),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(120),
                    &redact,
                    None,
                )
                .await;
                // Delete the local staging copy immediately (minimize its on-disk
                // lifetime), whether the transfer succeeded or not, then clear the
                // guard's local backstop. If `xfer_res` is an error, `secret_guard`
                // drops on the `?` below → the remote file is cleaned up.
                let _ = tokio::fs::remove_file(&local_secret).await;
                if let Some(g) = secret_guard.as_mut() {
                    g.clear_local();
                }
                xfer_res?;
            }

            if let Some(channel) = &pinned {
                // `rustup toolchain install <channel>` is cwd-independent; the
                // channel is shell-quoted for the remote shell.
                run(
                    &[
                        "ssh".into(),
                        host_addr.clone(),
                        format!("rustup toolchain install {}", shell_quote(channel)),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(600),
                    &redact,
                    None,
                )
                .await?;
            }

            let manifest = format!("{remote_source}/Cargo.toml");
            let cargo_argv =
                cargo_build_argv(&profile, &triple, resolved.caps.jobs, &bin, &manifest);
            let scope_argv = scope::render_scope_argv(&unit, &resolved.caps, &setenv, &cargo_argv);
            // Remote wrapper: source the secret env file (if any), delete it, then
            // exec the scoped build. The secret lives only in the 0600 file, never argv.
            let scope_cmd = shell_join(&scope_argv);
            let remote_cmd = if have_secret {
                format!(
                    "set -a; . {f}; rm -f {f}; set +a; exec {scope_cmd}",
                    f = shell_quote(&remote_env_path)
                )
            } else {
                format!("exec {scope_cmd}")
            };
            // On timeout, tear down the REMOTE scope by its unit name too — the
            // local ssh process-group kill can't reach the remote build tree.
            let remote_kill = RemoteScopeKill {
                host: host_addr.clone(),
                unit: unit.clone(),
            };
            let build_res = run(
                &["ssh".into(), host_addr.clone(), remote_cmd],
                None,
                &BTreeMap::new(),
                Duration::from_secs(3600),
                &redact,
                Some(&remote_kill),
            )
            .await;
            // If the build FAILED/timed out, propagate now — `secret_guard` drops on
            // this `?` and cleans up the remote file (it may still exist if the build
            // never reached the wrapper's own `rm`). On SUCCESS the wrapper already
            // removed the file, so disarm the guard's remote cleanup (avoids a
            // redundant ssh); the guard object stays alive but Drop becomes a no-op.
            build_res?;
            if let Some(g) = secret_guard.as_mut() {
                g.disarm();
            }

            // Retrieve the built binary back to a local temp path so publish is
            // host-agnostic (the build ran remotely; publish reads it locally).
            let remote_bin = format!(
                "{}/{}",
                remote_target_str.trim_end_matches('/'),
                built_bin_rel(&triple, &profile, &bin).to_string_lossy()
            );
            let local_tmp_dir = std::env::temp_dir().join(format!("terminus-artifact-{unit}"));
            tokio::fs::create_dir_all(&local_tmp_dir)
                .await
                .map_err(|e| ToolError::Execution(format!("mk artifact tmp dir: {e}")))?;
            let local_bin = local_tmp_dir.join(&bin);
            run(
                &[
                    "rsync".into(),
                    "-a".into(),
                    "-s".into(),
                    format!("{host_addr}:{remote_bin}"),
                    local_bin.to_string_lossy().to_string(),
                ],
                None,
                &BTreeMap::new(),
                Duration::from_secs(600),
                &redact,
                None,
            )
            .await?;
            built_bin = local_bin;
        }

        // ── Publish the artifact (checksummed; no `current` flip) ────────────
        // `built_bin` is a locally-readable path (built in place locally, or
        // retrieved from the heavy host above), so publish is host-agnostic.
        let channel = publish::DEFAULT_CHANNEL;
        validate_segment("channel", channel)?;
        let published = if let Some(relay_host) = env_nonempty(BUILD_DATASET_RELAY_HOST) {
            // Interim: relay-publish over a single hop to a host with the dataset RW.
            // The plan bundles BOTH the binary and its `.sha256` sidecar so the
            // relayed artifact is verifiable by the updater (never binary-only).
            let remote_root =
                env_nonempty(BUILD_DATASET_RELAY_ROOT).unwrap_or_else(|| root_str.clone());
            let sha = publish::sha256_file(&built_bin).await?;
            let sidecar_tmp = built_bin.with_file_name(format!("{bin}.sha256"));
            let plan = publish::render_relay_plan(
                &relay_host,
                &remote_root,
                &module,
                channel,
                &sha,
                &triple,
                &bin,
                &built_bin,
                &sidecar_tmp,
            );
            // Stage the sidecar locally, then relay the binary + sidecar.
            tokio::fs::write(&sidecar_tmp, &plan.sidecar_body)
                .await
                .map_err(|e| ToolError::Execution(format!("write sidecar: {e}")))?;
            let bin_res = run(
                &plan.binary_argv,
                None,
                &BTreeMap::new(),
                Duration::from_secs(600),
                &redact,
                None,
            )
            .await;
            let sc_res = if bin_res.is_ok() {
                run(
                    &plan.sidecar_argv,
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(120),
                    &redact,
                    None,
                )
                .await
            } else {
                Ok(String::new())
            };
            // Clean up the local staging sidecar regardless of outcome.
            let _ = tokio::fs::remove_file(&sidecar_tmp).await;
            bin_res?;
            sc_res?;
            publish::Published {
                sha256: sha.clone(),
                artifact_path: plan.remote_binary,
                sha256_path: plan.remote_sidecar,
                relayed: true,
            }
        } else {
            publish::publish_local(&root, &module, channel, &triple, &bin, &built_bin).await?
        };

        // ── BLD-07 store: on a LOCAL publish (dataset mounted RW on this host),
        // write the per-sha manifest and flip `experimental/current` onto the new
        // sha (atomic temp+rename), then prune the channel to the retention policy.
        // Skipped on the INTERIM relay path — the build host lacks the dataset
        // mount, so it cannot (and must not) write a local pointer; the relay
        // target host owns that flip. `compiler_release` promotes to `stable`.
        let mut blessed_current = false;
        let mut pruned: Vec<String> = Vec::new();
        if !published.relayed {
            publish::write_manifest(&root, &module, channel, &published.sha256, &triple, &bin)
                .await?;
            let set =
                publish::set_current(&root, &module, channel, &published.sha256, "bless", None)
                    .await?;
            blessed_current = set.changed;
            let mut keep = vec![published.sha256.clone()];
            if let Some(prev) = &set.previous {
                keep.push(prev.clone());
            }
            pruned =
                publish::prune_channel(&root, &module, channel, retain_per_channel(), &keep).await?;
        }

        let text = format!(
            "Built {module}@{git_ref} on {host} ({sccache}); artifact {sha} → {path}{relayed}",
            host = resolved.role.as_str(),
            sccache = sccache_env.describe(),
            sha = &published.sha256,
            path = published.artifact_path.display(),
            relayed = if published.relayed { " (relayed)" } else { "" },
        );
        let structured = json!({
            "module": module,
            "ref": git_ref,
            "host": resolved.role.as_str(),
            "remote": !resolved.is_local(),
            "profile": profile,
            "target": triple,
            "channel": channel,
            "bin": bin,
            "sha256": published.sha256,
            "artifact_path": published.artifact_path.to_string_lossy(),
            "sha256_path": published.sha256_path.to_string_lossy(),
            "relayed": published.relayed,
            "current_channel": channel,
            "blessed_current": blessed_current,
            "pruned": pruned,
            "sccache_mode": sccache_env.mode.as_str(),
            "caps": {
                "memory_max": resolved.caps.memory_max,
                "memory_swap_max": "0",
                "cpu_quota": resolved.caps.cpu_quota,
                "io_weight": resolved.caps.io_weight,
                "jobs": resolved.caps.jobs,
            },
        });
        Ok(ToolOutput::with_structured(text, structured))
    }
}

/// BLD-07 — the `compiler_release` tool: the channel-pointer surface over the
/// artifact store. It NEVER rebuilds — it promotes an already-built sha into a
/// channel by an atomic `current` pointer flip (Rust-train model), rolls a
/// channel back to its previous blessed sha, or queries the current blessed sha.
struct CompilerRelease;

#[async_trait]
impl RustTool for CompilerRelease {
    fn name(&self) -> &str {
        "compiler_release"
    }

    fn description(&self) -> &str {
        "Manage the artifact-store channel pointers (no rebuild). op=promote blesses an \
         already-built sha into a channel by an atomic `current` pointer flip after verifying \
         the artifact + its .sha256 (fail-closed on an unbuilt/corrupt sha), giving the target \
         channel its own copy (Rust-train) and pruning to the retention floor; op=rollback \
         reverts a channel to its previous blessed sha; op=current returns the blessed sha for \
         a (module, channel). This is the `current` the constellation-updater fetches."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["promote", "rollback", "current"],
                    "default": "promote",
                    "description": "promote an already-built sha (default) | rollback to the previous blessed sha | query the current blessed sha."
                },
                "module": {
                    "type": "string",
                    "description": "Module/repo whose channel pointer is being managed."
                },
                "sha": {
                    "type": "string",
                    "description": "The already-built content-address sha to promote (required for op=promote)."
                },
                "from_channel": {
                    "type": "string",
                    "default": "experimental",
                    "description": "Source channel the sha was built/published into (op=promote)."
                },
                "to_channel": {
                    "type": "string",
                    "default": "stable",
                    "description": "Target channel: the one promoted into, rolled back, or queried."
                },
                "bin": {
                    "type": "string",
                    "description": "Binary name to verify (defaults to the module name)."
                },
                "target": {
                    "type": "string",
                    "description": "Target triple to verify (defaults to the configured build target)."
                }
            },
            "required": ["module"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("promote")
            .to_string();
        let module = str_arg(&args, "module")?;
        validate_segment("module", &module)?;
        let to_channel = args
            .get("to_channel")
            .and_then(Value::as_str)
            .unwrap_or("stable")
            .to_string();
        validate_segment("channel", &to_channel)?;

        let root = dataset_root()?;

        match op.as_str() {
            "current" => {
                let current = publish::read_current(&root, &module, &to_channel).await?;
                let previous = publish::read_previous(&root, &module, &to_channel).await?;
                let text = match &current {
                    Some(sha) => format!("{module}/{to_channel} current = {sha}"),
                    None => format!("{module}/{to_channel} has no blessed sha yet"),
                };
                let structured = json!({
                    "op": "current",
                    "module": module,
                    "channel": to_channel,
                    "current": current,
                    "previous": previous,
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
            "rollback" => {
                let out = publish::rollback_current(&root, &module, &to_channel).await?;
                let text = format!(
                    "Rolled {module}/{to_channel} back to {sha} (was {was})",
                    sha = out.sha,
                    was = out.previous.as_deref().unwrap_or("<none>"),
                );
                let structured = json!({
                    "op": "rollback",
                    "module": module,
                    "channel": to_channel,
                    "current": out.sha,
                    "previous": out.previous,
                    "changed": out.changed,
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
            "promote" => {
                let sha = str_arg(&args, "sha")?;
                validate_segment("sha", &sha)?;
                let from_channel = args
                    .get("from_channel")
                    .and_then(Value::as_str)
                    .unwrap_or(publish::DEFAULT_CHANNEL)
                    .to_string();
                validate_segment("channel", &from_channel)?;
                let bin = args
                    .get("bin")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| module.clone());
                validate_segment("bin", &bin)?;
                let target = args
                    .get("target")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(target_triple);
                validate_segment("target", &target)?;

                let out = publish::promote(
                    &root,
                    &module,
                    &from_channel,
                    &to_channel,
                    &sha,
                    &target,
                    &bin,
                    retain_per_channel(),
                )
                .await?;

                let text = if out.already_current {
                    format!("{module}@{sha} already current on {to_channel} (no-op)")
                } else {
                    format!(
                        "Promoted {module}@{sha} {from_channel} → {to_channel} (no rebuild{copied}); \
                         current flipped{pruned}",
                        copied = if out.copied { ", copied" } else { "" },
                        pruned = if out.pruned.is_empty() {
                            String::new()
                        } else {
                            format!("; pruned {}", out.pruned.len())
                        },
                    )
                };
                let structured = json!({
                    "op": "promote",
                    "module": out.module,
                    "sha256": out.sha256,
                    "from_channel": out.from_channel,
                    "to_channel": out.to_channel,
                    "previous_current": out.previous_current,
                    "copied": out.copied,
                    "already_current": out.already_current,
                    "pruned": out.pruned,
                    "current_path": out.current_path.to_string_lossy(),
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
            other => Err(ToolError::InvalidArgument(format!(
                "unknown op {other:?} (expected promote | rollback | current)"
            ))),
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("`{key}` is required")))
}

/// The conservative allowlist for one path segment: ASCII alphanumerics plus
/// `.`, `_`, `-`. No `/`, `\`, whitespace, control chars, NUL, or any shell/path
/// metacharacter.
fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Validate a user-controlled value as a SAFE single path segment — no
/// traversal, no path separator, no injection — BEFORE it is ever joined into a
/// path or interpolated into an rsync/ssh command. Rejects empty, `.`/`..`, and
/// anything containing a byte outside `[A-Za-z0-9._-]` (which also excludes `/`,
/// `\`, whitespace, control chars, and shell metacharacters). Used for
/// module/bin/profile/target/channel.
fn validate_segment(kind: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} must not be empty"
        )));
    }
    if value == "." || value == ".." {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} must not be '.' or '..'"
        )));
    }
    if !value.chars().all(is_segment_char) {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} {value:?} contains characters outside [A-Za-z0-9._-] \
             (no path separators, whitespace, control chars, or shell metacharacters)"
        )));
    }
    Ok(())
}

/// Validate a git ref: like [`validate_segment`] but MAY contain `/` between
/// otherwise-valid segments (a branch such as `feature/foo`), and never a
/// traversal. Rejects an absolute ref (`/`-leading), a trailing `/`, `\`, any
/// empty/`.`/`..` component, and any disallowed byte. This keeps a ref usable as
/// a nested-but-contained path fragment under the dataset root.
fn validate_git_ref(value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument("ref must not be empty".into()));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(ToolError::InvalidArgument(format!(
            "ref {value:?} must not start or end with '/'"
        )));
    }
    if value.contains('\\') {
        return Err(ToolError::InvalidArgument(format!(
            "ref {value:?} must not contain '\\'"
        )));
    }
    for comp in value.split('/') {
        validate_segment("ref component", comp)?;
    }
    Ok(())
}

/// The allowed roots a caller-supplied `source_dir` may resolve under: always
/// `${BUILD_DATASET_ROOT}/src`, plus any `:`-separated `BUILD_ALLOWED_SOURCE_ROOTS`.
fn allowed_source_roots(dataset_root: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = vec![dataset_root.join("src")];
    if let Some(extra) = env_nonempty(BUILD_ALLOWED_SOURCE_ROOTS) {
        for r in extra.split(':') {
            let r = r.trim();
            if !r.is_empty() {
                roots.push(PathBuf::from(r));
            }
        }
    }
    roots
}

/// Validate a caller-supplied `source_dir` (a FULL PATH, not a segment) by
/// CONTAINMENT: it must lexically resolve (no filesystem access) to a path inside
/// one of the [`allowed_source_roots`]. Rejects an absolute path elsewhere or a
/// `../`-escaping override, so the build/relay never touches source outside the
/// dataset. Checked before `source_dir` is used for current_dir / --manifest-path
/// / rsync.
fn validate_source_dir(
    source_dir: &std::path::Path,
    dataset_root: &std::path::Path,
) -> Result<(), ToolError> {
    let roots = allowed_source_roots(dataset_root);
    if roots.iter().any(|root| scope::is_within(source_dir, root)) {
        return Ok(());
    }
    Err(ToolError::InvalidArgument(format!(
        "source_dir ({}) resolves outside the allowed source roots ({}); a \
         caller-supplied source path must stay within the dataset src tree \
         (set BUILD_ALLOWED_SOURCE_ROOTS to permit an additional staging root)",
        source_dir.display(),
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Register the `compiler_*` tool surface on the registry.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(CompilerBuild)) {
        tracing::error!("compiler: failed to register compiler_build: {e}");
    }
    if let Err(e) = registry.register(Box::new(CompilerRelease)) {
        tracing::error!("compiler: failed to register compiler_release: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_argv_release_musl() {
        let argv = cargo_build_argv(
            "release",
            "x86_64-unknown-linux-musl",
            4,
            "chord",
            "/src/chord/Cargo.toml",
        );
        let j = argv.join(" ");
        assert!(j.starts_with("cargo build --locked --release"));
        assert!(j.contains("--manifest-path /src/chord/Cargo.toml"));
        assert!(j.contains("--target x86_64-unknown-linux-musl"));
        assert!(j.contains("-j 4"));
        assert!(j.contains("--bin chord"));
    }

    #[test]
    fn cargo_argv_debug_has_no_release_flag() {
        let argv = cargo_build_argv("debug", "t", 8, "m", "/s/Cargo.toml");
        assert!(!argv.iter().any(|a| a == "--release"));
        assert!(argv.contains(&"-j".to_string()));
        assert!(argv.windows(2).any(|w| w[0] == "-j" && w[1] == "8"));
        // Manifest-path makes the build CWD-independent (correct for remote ssh).
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--manifest-path" && w[1] == "/s/Cargo.toml"));
    }

    #[test]
    fn cargo_argv_named_profile() {
        let argv = cargo_build_argv("release-dist", "t", 2, "m", "/s/Cargo.toml");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--profile" && w[1] == "release-dist"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        // An embedded single quote is closed, escaped, reopened.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_join_quotes_every_arg() {
        let argv = vec![
            "systemd-run".to_string(),
            "--setenv=SCCACHE_REDIS_ENDPOINT=redis://h:6379".to_string(),
            "cargo".to_string(),
        ];
        let s = shell_join(&argv);
        assert_eq!(
            s,
            "'systemd-run' '--setenv=SCCACHE_REDIS_ENDPOINT=redis://h:6379' 'cargo'"
        );
    }

    #[test]
    fn built_bin_path_matches_profile_subdir() {
        assert_eq!(
            built_bin_rel("x86_64-unknown-linux-musl", "release", "chord"),
            PathBuf::from("x86_64-unknown-linux-musl/release/chord")
        );
        assert_eq!(built_bin_rel("t", "debug", "m"), PathBuf::from("t/debug/m"));
        assert_eq!(
            built_bin_rel("t", "release-dist", "m"),
            PathBuf::from("t/release-dist/m")
        );
    }

    #[test]
    fn default_target_dir_is_never_the_nfs_dataset() {
        // Whatever the default local target dir is, it must pass the guard
        // against a dataset root — i.e. it is not under it. (Uses a sample root;
        // the default target lives under the temp dir, not the dataset.)
        let target = local_target_dir();
        let root = PathBuf::from("/data/build");
        assert!(scope::validate_target_dir(&target, &root).is_ok());
    }

    #[test]
    fn str_arg_rejects_missing_and_blank() {
        let v = json!({"module": "  ", "ref": "abc"});
        assert!(str_arg(&v, "module").is_err());
        assert_eq!(str_arg(&v, "ref").unwrap(), "abc");
        assert!(str_arg(&v, "missing").is_err());
    }

    #[test]
    fn segment_validation_accepts_normal_and_rejects_traversal() {
        // Normal segments accepted.
        for ok in [
            "chord",
            "lumina-core",
            "terminus_rs",
            "release-dist",
            "v1.2.3",
            "abc123",
        ] {
            assert!(
                validate_segment("module", ok).is_ok(),
                "should accept {ok:?}"
            );
        }
        // Traversal / separators / injection / control chars all rejected.
        for bad in [
            "",            // empty
            ".",           // dot
            "..",          // parent
            "../..",       // traversal (contains '/')
            "a/b",         // separator
            "a/../b",      // embedded traversal
            "/etc/passwd", // absolute (leading '/')
            "a\\b",        // backslash
            "a b",         // whitespace
            "a;rm -rf /",  // shell metachars + space
            "$(touch x)",  // command substitution
            "a`b`",        // backticks
            "a\0b",        // NUL
            "a\nb",        // newline / control
        ] {
            assert!(
                validate_segment("module", bad).is_err(),
                "should REJECT {bad:?}"
            );
        }
    }

    #[test]
    fn git_ref_validation_allows_branch_slashes_but_not_traversal() {
        // A real branch/sha is accepted, including a single '/'.
        for ok in [
            "main",
            "feature/foo",
            "release/2026-07",
            "0a1b2c3d",
            "v1.0.0",
        ] {
            assert!(validate_git_ref(ok).is_ok(), "should accept ref {ok:?}");
        }
        // Traversal and injection are rejected even with the looser ref charset.
        for bad in [
            "",         // empty
            "/etc",     // absolute
            "feature/", // trailing slash
            "../..",    // traversal
            "a/../b",   // embedded '..'
            "a//b",     // empty component
            "a\\b",     // backslash
            "a b",      // whitespace
            "$(x)",     // injection
            "a;b",      // shell metachar
        ] {
            assert!(validate_git_ref(bad).is_err(), "should REJECT ref {bad:?}");
        }
    }

    #[test]
    fn shell_metachar_segment_is_rejected_and_quoting_is_injection_safe() {
        // Finding #1 already rejects a metachar-laden segment outright…
        let nasty = "m;$(touch PWNED)`id`";
        assert!(validate_segment("module", nasty).is_err());
        // …and even if some interpolated value reached the ssh layer, shell_quote
        // renders it a single inert word (round-trips through a real shell with no
        // command execution).
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("PWNED");
        let payload = format!("x $(touch '{m}') `touch '{m}'` ; y", m = marker.display());
        let script = format!("printf %s {}", shell_quote(&payload));
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(script.as_bytes()).unwrap();
        let out = std::process::Command::new("sh")
            .arg(f.path())
            .output()
            .expect("run sh");
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap(), payload);
        assert!(
            !marker.exists(),
            "shell_quote must prevent command execution"
        );
    }

    #[test]
    fn secret_file_is_exclusive_0600_no_symlink_follow() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        // The body content is arbitrary for this test (we're exercising the
        // creation semantics, not the payload) — a non-secret-shaped literal.
        let body = "payload-line-one\n";

        // (a) Fresh path → succeeds, mode exactly 0600, contents match.
        let fresh = dir.path().join("fresh.env");
        write_secret_0600_at(&fresh, body).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), body);
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "must be 0600 from creation, got {mode:o}");

        // (b) Pre-existing path → O_EXCL makes it a hard error, and the existing
        // file is NOT truncated/overwritten.
        let existing = dir.path().join("existing.env");
        std::fs::write(&existing, "PREEXISTING").unwrap();
        assert!(write_secret_0600_at(&existing, body).is_err());
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "PREEXISTING",
            "an existing file must never be truncated/overwritten"
        );

        // (c) Symlink at the path → O_NOFOLLOW refuses to follow it; the symlink
        // target is NOT created or written.
        let target = dir.path().join("target-should-not-be-written");
        let link = dir.path().join("link.env");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_secret_0600_at(&link, body).is_err());
        assert!(
            !target.exists(),
            "a symlink must not be followed to create/write its target"
        );
    }

    #[test]
    fn redact_secrets_replaces_values_and_is_a_noop_when_empty() {
        let secret = "<REDACTED-SECRET>".to_string();
        let url = "redis://default:topsecretvalue123@h:6379/1".to_string();
        let secrets = vec![secret.clone(), url.clone(), String::new()];

        // A line echoing the secret is scrubbed; the raw value is absent.
        let leaked = format!("error: a build script printed the secret {secret} to stderr");
        let red = redact_secrets(&leaked, &secrets);
        assert!(
            !red.contains("topsecretvalue123"),
            "raw secret must be gone: {red}"
        );
        assert!(red.contains("<redacted>"));

        // The full URL value is scrubbed too.
        let leaked_url = format!("connecting to {url} ...");
        assert!(!redact_secrets(&leaked_url, &secrets).contains("topsecretvalue123"));

        // A non-secret line passes through unchanged.
        let benign = "warning: unused variable `x`";
        assert_eq!(redact_secrets(benign, &secrets), benign);

        // Empty secret set / empty values are a no-op.
        assert_eq!(redact_secrets(&leaked, &[]), leaked);
        assert_eq!(redact_secrets("plain", &[String::new()]), "plain");
    }

    #[test]
    fn redact_secrets_handles_overlapping_values_longest_first() {
        // The exact overlap case: the password is a SUBSTRING of the full URL.
        // Order the input worst-case (password first) — longest-first ordering
        // inside the helper must still fully scrub the URL, leaving no partial
        // `redis://...@host` fragment.
        let password = "abc".to_string();
        let url = "redis://u:abc@host:6379/1".to_string();
        let secrets = vec![password.clone(), url.clone()];

        let text = format!("dump: url={url} pw={password}");
        let red = redact_secrets(&text, &secrets);
        assert!(
            !red.contains("abc"),
            "no secret substring may survive: {red}"
        );
        assert!(!red.contains("redis://"), "no partial URL may leak: {red}");
        assert!(!red.contains("@host"), "URL host/port must not leak: {red}");
        // Both occurrences became the placeholder.
        assert_eq!(red, "dump: url=<redacted> pw=<redacted>");
    }

    #[test]
    fn source_dir_containment() {
        let root = std::path::Path::new("/data/build");
        // Under the dataset src tree → accepted.
        assert!(
            validate_source_dir(std::path::Path::new("/data/build/src/chord/abc"), root).is_ok()
        );
        assert!(validate_source_dir(std::path::Path::new("/data/build/src"), root).is_ok());
        // Absolute elsewhere → rejected.
        assert!(validate_source_dir(std::path::Path::new("/etc"), root).is_err());
        assert!(validate_source_dir(std::path::Path::new("/data/build/cache/x"), root).is_err());
        // `..`-escape that lexically leaves the src tree → rejected.
        assert!(
            validate_source_dir(std::path::Path::new("/data/build/src/../../etc"), root).is_err()
        );
        // A sibling sharing a string prefix but not the path → rejected.
        assert!(validate_source_dir(std::path::Path::new("/data/build/src-evil/x"), root).is_err());
    }

    #[tokio::test]
    async fn run_redacts_secret_from_stderr_tail_and_stdout() {
        let secret = "<REDACTED-SECRET>".to_string();
        let redact = vec![secret.clone()];

        // Failing child that echoes the secret to stderr → the error tail must be
        // redacted (this is the exact leak path: a build.rs printing its env).
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo leak={secret} 1>&2; exit 1"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("topsecretvalue123"),
            "secret leaked into error: {msg}"
        );
        assert!(msg.contains("<redacted>"));

        // Successful child that echoes the secret to stdout → returned stdout redacted.
        let out = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo out={secret}; exit 0"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
        )
        .await
        .unwrap();
        assert!(
            !out.contains("topsecretvalue123"),
            "secret leaked into stdout: {out}"
        );
        assert!(out.contains("<redacted>"));
    }

    #[tokio::test]
    async fn run_timeout_kills_the_child_process_tree() {
        // A child that would create a marker AFTER a sleep longer than the timeout.
        // If the timeout path merely dropped the future without killing the process
        // group, the sleep would finish and the marker would appear. The kill must
        // prevent that. `sh -c 'sleep …; touch marker'` — sh is the group leader and
        // sleep is in its group, so killpg(SIGKILL) tears down the whole tree.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("SHOULD_NOT_EXIST");
        let start = std::time::Instant::now();
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("sleep 3; : > {}", marker.display()),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_millis(300),
            &[],
            None,
        )
        .await
        .unwrap_err();
        // Timed out promptly (did not block for the full sleep).
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "run should return at the timeout"
        );
        assert!(format!("{err:?}").contains("timed out"));

        // Wait past when the marker WOULD have been created had the child survived;
        // it must never appear, proving the process was killed.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "the timed-out child was not killed — its process tree leaked"
        );
    }

    #[test]
    fn remote_secret_rm_argv_is_bounded_and_quoted() {
        let argv = render_remote_secret_rm_argv("builduser@heavy", "/mnt/x/.terminus-build-y.env");
        assert_eq!(argv[0], "ssh");
        let j = argv.join(" ");
        // Bounded connect so a synchronous Drop cleanup can't hang; batch mode so
        // it never prompts; path shell-quoted.
        assert!(j.contains("-o BatchMode=yes"), "{j}");
        assert!(j.contains("-o ConnectTimeout=10"), "{j}");
        assert!(j.contains("builduser@heavy"));
        assert_eq!(argv.last().unwrap(), "rm -f '/mnt/x/.terminus-build-y.env'");
    }

    #[test]
    fn secret_guard_cleans_remote_and_local_on_drop_error_path() {
        use std::sync::{Arc, Mutex};
        // A local staging file that must be unlinked when the guard drops.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("staging.env");
        std::fs::write(&local, "secret-bytes").unwrap();

        let rec: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            // Guard armed after transfer; NO disarm ⇒ models ANY post-transfer
            // early return (a failing pinned-toolchain install, a build error, a
            // timeout, or a panic) — Drop must clean up.
            let mut g = RemoteSecretGuard::new(
                "builduser@heavy".to_string(),
                "/mnt/build-target/.terminus-build-chord-deadbeef.env".to_string(),
                Some(local.clone()),
                vec![],
            );
            g.recorder = Some(rec.clone());
        } // <- early-return / scope-exit: Drop fires here

        // Remote rm was issued (exactly once) with the expected bounded, quoted argv.
        let calls = rec.lock().unwrap();
        assert_eq!(calls.len(), 1, "remote rm must fire on the error path");
        assert_eq!(calls[0][0], "ssh");
        assert_eq!(
            calls[0].last().unwrap(),
            "rm -f '/mnt/build-target/.terminus-build-chord-deadbeef.env'"
        );
        // Local staging file was unlinked too.
        assert!(
            !local.exists(),
            "local staging secret must be removed on drop"
        );
    }

    #[test]
    fn secret_guard_disarmed_skips_remote_cleanup() {
        use std::sync::{Arc, Mutex};
        let rec: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            // Happy path: the build's own wrapper already removed the remote file,
            // so the guard is disarmed — Drop must NOT issue a redundant remote rm.
            let mut g =
                RemoteSecretGuard::new("h".to_string(), "/p/.env".to_string(), None, vec![]);
            g.recorder = Some(rec.clone());
            g.disarm();
        }
        assert!(
            rec.lock().unwrap().is_empty(),
            "a disarmed guard must not issue a remote rm"
        );
    }

    #[test]
    fn remote_scope_kill_argv_targets_the_named_scope() {
        let unit = "terminus-build-chord-abc-deadbeefcafe";
        let argv = render_remote_scope_kill_argv("builduser@heavy", unit);
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "builduser@heavy");
        let cmd = &argv[2];
        // SIGKILL the scope, falling back to stop — both target the exact unit's
        // `.scope`, shell-quoted.
        assert!(
            cmd.contains(&format!("systemctl kill --signal=SIGKILL '{unit}.scope'")),
            "kill cmd: {cmd}"
        );
        assert!(
            cmd.contains(&format!("systemctl stop '{unit}.scope'")),
            "stop fallback: {cmd}"
        );
    }

    #[tokio::test]
    async fn cleanup_run_redacts_secret_like_the_build() {
        // The remote-scope-kill cleanup goes through `run(argv, .., redact, None)`
        // — the SAME redaction path as the build. This guards the property that a
        // secret emitted by a FAILING cleanup command is redacted before it lands
        // in the error `remote_scope_kill` logs at `warn!`. (The cleanup child
        // inherits the parent env incl. ambient SCCACHE_REDIS, so this matters.)
        let secret = "<REDACTED-SECRET>".to_string();
        let redact = vec![secret.clone()];
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo leak={secret} 1>&2; exit 1"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("topsecretvalue123"),
            "cleanup output must be redacted: {msg}"
        );
        assert!(msg.contains("<redacted>"));
    }

    #[test]
    fn remote_scope_is_addressable_by_the_same_unit_the_kill_targets() {
        // The remote build's scope argv carries `--unit=<unit>`, and the timeout
        // kill targets exactly `<unit>.scope` — so a timed-out remote build IS
        // reachable by name (the fix's core invariant).
        let unit = "terminus-build-chord-abc-deadbeefcafe";
        let caps = scope::ScopeCaps {
            memory_max: "12G".to_string(),
            cpu_quota: "400%".to_string(),
            io_weight: "50".to_string(),
            jobs: 4,
        };
        let scope_argv = scope::render_scope_argv(
            unit,
            &caps,
            &BTreeMap::new(),
            &["cargo".into(), "build".into()],
        );
        assert!(
            scope_argv.iter().any(|a| a == &format!("--unit={unit}")),
            "remote scope must be named --unit={unit}: {scope_argv:?}"
        );
        let kill = render_remote_scope_kill_argv("h", unit);
        assert!(kill[2].contains(&format!("{unit}.scope")));
    }

    #[test]
    fn tool_metadata_is_stable() {
        let t = CompilerBuild;
        assert_eq!(t.name(), "compiler_build");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "module");
        assert_eq!(p["required"][1], "ref");
    }

    #[test]
    fn release_tool_metadata_is_stable() {
        let t = CompilerRelease;
        assert_eq!(t.name(), "compiler_release");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "module");
        // The op enum offers promote (default) | rollback | current.
        let ops = p["properties"]["op"]["enum"].as_array().unwrap();
        assert!(ops.iter().any(|v| v == "promote"));
        assert!(ops.iter().any(|v| v == "rollback"));
        assert!(ops.iter().any(|v| v == "current"));
        assert_eq!(p["properties"]["op"]["default"], "promote");
        assert_eq!(p["properties"]["from_channel"]["default"], "experimental");
        assert_eq!(p["properties"]["to_channel"]["default"], "stable");
    }

    #[test]
    fn retain_per_channel_is_floored_at_two() {
        // Default when unset is the store's ≥2 default.
        assert!(retain_per_channel() >= 2);
    }
}
