//! BLD-05 — the `compiler_build` Terminus tool: the single build door.
//!
//! `compiler_build(module, ref, host="auto", profile="release", fast=false)`
//! selects a build host, ensures the pinned toolchain, runs an sccache-backed
//! `cargo` build inside a resource-capped systemd scope (`MemorySwapMax=0` — Plex
//! protection), and publishes a SHA-256-checksummed artifact into the shared
//! build dataset. It does NOT flip a `current` pointer (that is BLD-07).
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
pub mod scope;
pub mod sccache;

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
        .ok_or_else(|| {
            ToolError::NotConfigured(format!("{BUILD_DATASET_ROOT} is not configured"))
        })
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
/// reproducible against the committed lockfile.
fn cargo_build_argv(profile: &str, triple: &str, jobs: u32, bin: &str) -> Vec<String> {
    let (profile_flags, _subdir) = profile_flags_and_subdir(profile);
    let mut argv = vec!["cargo".to_string(), "build".to_string(), "--locked".to_string()];
    argv.extend(profile_flags);
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

/// Run a subprocess argv with an optional cwd + extra env, bounded by `timeout`.
/// Returns `Ok(stdout)` on success (exit 0), else an `Execution` error with a
/// trimmed stderr tail. The env is applied on top of the inherited environment.
async fn run(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<String, ToolError> {
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

    let child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn {}: {e}", argv[0])))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(ToolError::Execution(format!("{}: {e}", argv[0]))),
        Err(_) => {
            return Err(ToolError::Execution(format!(
                "{} timed out after {}s",
                argv[0],
                timeout.as_secs()
            )))
        }
    };
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        Err(ToolError::Execution(format!(
            "{} exited {}: {tail}",
            argv[0],
            out.status.code().unwrap_or(-1)
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
         shared build dataset. Does not flip the `current` channel pointer (that is \
         compiler_release)."
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
        let host_req = HostRequest::parse(
            args.get("host").and_then(Value::as_str).unwrap_or("auto"),
        )?;
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

        // ── Resolve config (fail fast, no side effects) ──────────────────────
        let root = dataset_root()?;
        let root_str = root.to_string_lossy().to_string();
        let resolved = host::resolve(host_req, &module, fast)?;
        let triple = target_triple();
        let target_dir = local_target_dir();

        // GUARD: the live cargo target dir must be exec-safe local/tmpfs, never
        // the file-level NFS dataset.
        scope::validate_target_dir(&target_dir, &root)?;

        // sccache env (fail-open to a local dir if Redis is unconfigured).
        let sccache_env = sccache::resolve(&root_str);

        // Source tree (staged on the file-level NFS share is fine — it's only a
        // source stage, not the live target).
        let source_dir = match args.get("source_dir").and_then(Value::as_str) {
            Some(s) => PathBuf::from(s),
            None => root.join("src").join(&module).join(&git_ref),
        };

        // Build env: sccache split env + the exec-safe target dir.
        let mut build_env = sccache_env.vars.clone();
        build_env.insert(
            "CARGO_TARGET_DIR".to_string(),
            target_dir.to_string_lossy().to_string(),
        );

        // ── Ensure the pinned toolchain (idempotent; never `rustup update`) ──
        if let Some(channel) = env_nonempty(RUST_TOOLCHAIN_PINNED) {
            let install = vec![
                "rustup".to_string(),
                "toolchain".to_string(),
                "install".to_string(),
                channel,
            ];
            // Best-effort: run from the source dir so rustup honors rust-toolchain.toml.
            run(&install, Some(&source_dir), &BTreeMap::new(), Duration::from_secs(600)).await?;
        }

        // ── Build inside the capped scope ────────────────────────────────────
        let cargo_argv = cargo_build_argv(&profile, &triple, resolved.caps.jobs, &bin);
        let unit = scope::scope_unit_name(&module, &git_ref);
        let scope_argv = scope::render_scope_argv(&unit, &resolved.caps, &build_env, &cargo_argv);

        // On a remote heavy host, wrap the scope command in ssh; on a local
        // (primary in-place) host, run it directly. The env is carried into the
        // scope via `--setenv=` (rendered above), so ssh needs no env passthrough.
        let exec_argv = match (&resolved.address, resolved.role) {
            (Some(addr), host::HostRole::Heavy) => {
                let mut a = vec!["ssh".to_string(), addr.clone()];
                a.extend(scope_argv.clone());
                a
            }
            _ => scope_argv.clone(),
        };
        let build_cwd = if resolved.is_local() { Some(source_dir.as_path()) } else { None };
        // Build timeout is generous — a cold full build can take minutes.
        run(&exec_argv, build_cwd, &BTreeMap::new(), Duration::from_secs(3600)).await?;

        // ── Publish the artifact (checksummed; no `current` flip) ────────────
        let built_bin = target_dir.join(built_bin_rel(&triple, &profile, &bin));
        let channel = publish::DEFAULT_CHANNEL;
        let published = if let Some(relay_host) = env_nonempty(BUILD_DATASET_RELAY_HOST) {
            // Interim: relay-publish over a single hop to a host with the dataset RW.
            let remote_root =
                env_nonempty(BUILD_DATASET_RELAY_ROOT).unwrap_or_else(|| root_str.clone());
            let sha = publish::sha256_file(&built_bin).await?;
            let relay_argv = publish::render_relay_argv(
                &relay_host, &remote_root, &module, channel, &sha, &triple, &bin, &built_bin,
            );
            run(&relay_argv, None, &BTreeMap::new(), Duration::from_secs(600)).await?;
            // Relay the sidecar too.
            let sidecar_tmp = built_bin.with_file_name(format!("{bin}.sha256"));
            tokio::fs::write(&sidecar_tmp, publish::sidecar_contents(&sha, &bin))
                .await
                .map_err(|e| ToolError::Execution(format!("write sidecar: {e}")))?;
            let sidecar_relay = publish::render_relay_argv(
                &relay_host,
                &remote_root,
                &module,
                channel,
                &sha,
                &triple,
                &format!("{bin}.sha256"),
                &sidecar_tmp,
            );
            run(&sidecar_relay, None, &BTreeMap::new(), Duration::from_secs(120)).await?;
            publish::Published {
                sha256: sha.clone(),
                artifact_path: PathBuf::from(&remote_root)
                    .join(publish::artifact_rel_path(&module, channel, &sha, &triple, &bin)),
                sha256_path: PathBuf::from(&remote_root).join(publish::artifact_rel_path(
                    &module,
                    channel,
                    &sha,
                    &triple,
                    &format!("{bin}.sha256"),
                )),
                relayed: true,
            }
        } else {
            publish::publish_local(&root, &module, channel, &triple, &bin, &built_bin).await?
        };

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
            "profile": profile,
            "target": triple,
            "channel": channel,
            "bin": bin,
            "sha256": published.sha256,
            "artifact_path": published.artifact_path.to_string_lossy(),
            "sha256_path": published.sha256_path.to_string_lossy(),
            "relayed": published.relayed,
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

fn str_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("`{key}` is required")))
}

/// Register the `compiler_*` tool surface on the registry.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(CompilerBuild)) {
        tracing::error!("compiler: failed to register compiler_build: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_argv_release_musl() {
        let argv = cargo_build_argv("release", "x86_64-unknown-linux-musl", 4, "chord");
        let j = argv.join(" ");
        assert!(j.starts_with("cargo build --locked --release"));
        assert!(j.contains("--target x86_64-unknown-linux-musl"));
        assert!(j.contains("-j 4"));
        assert!(j.contains("--bin chord"));
    }

    #[test]
    fn cargo_argv_debug_has_no_release_flag() {
        let argv = cargo_build_argv("debug", "t", 8, "m");
        assert!(!argv.iter().any(|a| a == "--release"));
        assert!(argv.contains(&"-j".to_string()));
        assert!(argv.windows(2).any(|w| w[0] == "-j" && w[1] == "8"));
    }

    #[test]
    fn cargo_argv_named_profile() {
        let argv = cargo_build_argv("release-dist", "t", 2, "m");
        assert!(argv.windows(2).any(|w| w[0] == "--profile" && w[1] == "release-dist"));
    }

    #[test]
    fn built_bin_path_matches_profile_subdir() {
        assert_eq!(
            built_bin_rel("x86_64-unknown-linux-musl", "release", "chord"),
            PathBuf::from("x86_64-unknown-linux-musl/release/chord")
        );
        assert_eq!(
            built_bin_rel("t", "debug", "m"),
            PathBuf::from("t/debug/m")
        );
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
    fn tool_metadata_is_stable() {
        let t = CompilerBuild;
        assert_eq!(t.name(), "compiler_build");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "module");
        assert_eq!(p["required"][1], "ref");
    }
}
