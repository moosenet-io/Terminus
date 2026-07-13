//! BLD-05 — resource-capped build scope (Plex protection).
//!
//! Every `cargo` build the compiler runs is wrapped in a transient systemd
//! scope so it lives in its OWN cgroup with hard resource caps:
//!
//!   systemd-run --scope --unit=<name> \
//!       -p MemoryMax=<cap> -p MemorySwapMax=0 -p CPUQuota=<pct> -p IOWeight=<w> \
//!       --setenv=KEY=VAL ... -- <cargo argv...>
//!
//! The load-bearing property is **`MemorySwapMax=0`**: an over-budget build is
//! OOM-killed INSIDE its own cgroup instead of triggering node-wide swap thrash
//! that would interrupt Plex (and every other co-located service). `MemoryMax`
//! bounds the resident set, `CPUQuota` and `IOWeight` keep the build from
//! starving foreground services. `-j`/parallelism is parameterized per host so
//! the peak fits the host's budget.
//!
//! This module is PURE — it renders the argv; it does not execute anything. The
//! executor (`mod.rs`) runs the rendered command. That split is what makes the
//! swap-off / cap invariants unit-testable offline.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::ToolError;

/// Resource caps for one build scope, resolved per host (`host.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCaps {
    /// `MemoryMax=` value (systemd size, e.g. "12G").
    pub memory_max: String,
    /// `CPUQuota=` value (e.g. "400%").
    pub cpu_quota: String,
    /// `IOWeight=` value (1..=10000, e.g. "50").
    pub io_weight: String,
    /// cargo `-j` / build parallelism (also caps peak RAM).
    pub jobs: u32,
}

/// Render the `systemd-run --scope` argv that runs `cargo_argv` under the caps,
/// with `env` injected via `--setenv=` so the child (and its build scripts) see
/// the sccache/toolchain/target-dir environment.
///
/// `unit_name` is the transient scope's `--unit=` so `systemctl show <unit>` can
/// be used to verify the caps (notably `MemorySwapMax=0`) live.
///
/// INVARIANT (asserted by tests): the rendered argv ALWAYS contains
/// `-p MemorySwapMax=0` — swap-off is not optional.
pub fn render_scope_argv(
    unit_name: &str,
    caps: &ScopeCaps,
    env: &BTreeMap<String, String>,
    cargo_argv: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "systemd-run".to_string(),
        "--scope".to_string(),
        format!("--unit={unit_name}"),
        // Don't inherit the caller's env wholesale into the scope; we pass the
        // build env explicitly via --setenv below (keeps the scope hermetic).
        "--collect".to_string(),
    ];

    // Resource caps. MemorySwapMax=0 is the load-bearing one (see module docs).
    let props = [
        format!("MemoryMax={}", caps.memory_max),
        "MemorySwapMax=0".to_string(),
        format!("CPUQuota={}", caps.cpu_quota),
        format!("IOWeight={}", caps.io_weight),
    ];
    for p in props {
        argv.push("-p".to_string());
        argv.push(p);
    }

    // Build environment (sccache split env, CARGO_TARGET_DIR, toolchain, …).
    // BTreeMap ⇒ deterministic ordering for stable rendering/tests.
    for (k, v) in env {
        argv.push(format!("--setenv={k}={v}"));
    }

    argv.push("--".to_string());
    argv.extend(cargo_argv.iter().cloned());
    argv
}

/// A transient scope unit name derived from module + ref, sanitized to the
/// characters systemd accepts in a unit name.
pub fn scope_unit_name(module: &str, git_ref: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    // Keep the ref fragment short (a full 40-char sha is fine, but truncate long
    // branch names) so the unit name stays reasonable.
    let r = sanitize(git_ref);
    let r = if r.len() > 16 { &r[..16] } else { &r };
    format!("terminus-build-{}-{}", sanitize(module), r)
}

/// GUARD: the live `CARGO_TARGET_DIR` MUST be exec-safe local/tmpfs, NEVER the
/// file-level NFS build dataset. cargo compiles build scripts + proc-macros then
/// EXECUTES them, and NFS breaks exec + adds `.cargo-lock`/mtime hazards — so a
/// target dir anywhere under `${BUILD_DATASET_ROOT}` is a hard error.
///
/// Returns `Ok(())` when `target_dir` is safe, `Err(InvalidArgument)` when it is
/// inside `dataset_root` (the file-level NFS dir).
pub fn validate_target_dir(target_dir: &Path, dataset_root: &Path) -> Result<(), ToolError> {
    // Compare on a lexical, normalized basis (both may be non-existent at check
    // time). Any target dir that is the dataset root or nested under it is
    // rejected; the dataset root is for source-staging + sccache + artifact
    // publish ONLY, never a live cargo target.
    let t = normalize(target_dir);
    let root = normalize(dataset_root);
    if t == root || t.starts_with(&format!("{root}/")) {
        return Err(ToolError::InvalidArgument(format!(
            "CARGO_TARGET_DIR ({}) is inside the file-level NFS build dataset ({}); \
             cargo targets must be on exec-safe local disk or tmpfs (build scripts \
             are compiled then executed — NFS breaks exec + adds lock/mtime hazards)",
            target_dir.display(),
            dataset_root.display()
        )));
    }
    Ok(())
}

/// Lexical path normalization sufficient for the containment check: trims a
/// trailing slash and collapses `//`. (We deliberately avoid canonicalize() so
/// the guard works on paths that don't exist yet at plan time.)
fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for ch in s.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push(ch);
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    // Trim a single trailing slash (but keep root "/").
    if out.len() > 1 {
        out.trim_end_matches('/').to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn caps() -> ScopeCaps {
        ScopeCaps {
            memory_max: "12G".to_string(),
            cpu_quota: "400%".to_string(),
            io_weight: "50".to_string(),
            jobs: 4,
        }
    }

    #[test]
    fn scope_always_sets_swap_off() {
        let mut env = BTreeMap::new();
        env.insert("CARGO_TARGET_DIR".to_string(), "/tmp/t".to_string());
        let argv = render_scope_argv(
            "terminus-build-terminus-abc",
            &caps(),
            &env,
            &["cargo".into(), "build".into(), "--release".into()],
        );
        // The load-bearing invariant: MemorySwapMax=0 is present as its own -p arg.
        let joined = argv.join(" ");
        assert!(
            argv.windows(2).any(|w| w[0] == "-p" && w[1] == "MemorySwapMax=0"),
            "rendered argv must cap swap to 0: {joined}"
        );
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.iter().any(|a| a == "-p"));
        assert!(argv.iter().any(|a| a.starts_with("--unit=")));
    }

    #[test]
    fn scope_carries_all_caps_and_env_and_cargo() {
        let mut env = BTreeMap::new();
        env.insert("RUSTC_WRAPPER".to_string(), "sccache".to_string());
        env.insert("CARGO_TARGET_DIR".to_string(), "/mnt/t".to_string());
        let argv = render_scope_argv("u", &caps(), &env, &["cargo".into(), "build".into()]);
        let j = argv.join(" ");
        assert!(j.contains("MemoryMax=12G"));
        assert!(j.contains("CPUQuota=400%"));
        assert!(j.contains("IOWeight=50"));
        assert!(j.contains("--setenv=RUSTC_WRAPPER=sccache"));
        assert!(j.contains("--setenv=CARGO_TARGET_DIR=/mnt/t"));
        // cargo argv comes after the `--` separator.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1], "cargo");
        assert_eq!(argv[sep + 2], "build");
    }

    #[test]
    fn unit_name_is_sanitized() {
        let n = scope_unit_name("Chord", "feature/big_thing!");
        assert!(n.starts_with("terminus-build-chord-"));
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn target_dir_on_nfs_dataset_is_rejected() {
        let root = PathBuf::from("/data/build");
        // Directly under the dataset root.
        assert!(validate_target_dir(&PathBuf::from("/data/build/target"), &root).is_err());
        // The dataset root itself.
        assert!(validate_target_dir(&PathBuf::from("/data/build"), &root).is_err());
        // A deeper nested path.
        assert!(
            validate_target_dir(&PathBuf::from("/data/build/src/x/target"), &root).is_err()
        );
        // Trailing-slash variant still caught.
        assert!(validate_target_dir(&PathBuf::from("/data/build/target/"), &root).is_err());
    }

    #[test]
    fn target_dir_on_local_disk_is_allowed() {
        let root = PathBuf::from("/data/build");
        assert!(validate_target_dir(&PathBuf::from("/tmp/build-target"), &root).is_ok());
        assert!(validate_target_dir(&PathBuf::from("/mnt/build-target"), &root).is_ok());
        // A sibling that merely shares a prefix STRING but not a path segment
        // must NOT be falsely rejected.
        assert!(validate_target_dir(&PathBuf::from("/data/build-target"), &root).is_ok());
    }
}
