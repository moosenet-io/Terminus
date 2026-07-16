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

/// Absolute path to `systemd-run`. Used instead of the bare name `"systemd-run"`
/// because the local build spawn (`tokio::process::Command::new(argv[0])`)
/// resolves a bare program name via the CHILD's `PATH`, and the build env map
/// this argv is spawned with can override `PATH` with one that lacks
/// `/usr/bin` — so a bare `"systemd-run"` failed with "No such file or directory
/// (os error 2)" even though the binary exists and terminus-primary's own PATH
/// has `/usr/bin`. An absolute path bypasses `PATH` resolution entirely. It is
/// also correct for the remote (ssh-rendered) path: `/usr/bin/systemd-run` is
/// the canonical location on every systemd host in the fleet (merged-`/usr`).
pub(crate) const SYSTEMD_RUN_BIN: &str = "/usr/bin/systemd-run";

/// Render the `systemd-run --scope` argv that runs `cargo_argv` under the caps,
/// with the NON-SECRET build env injected via `--setenv=` so the child (and its
/// build scripts) see the sccache endpoint/target-dir/toolchain environment.
///
/// SECURITY (S7): secret-shaped vars (notably `SCCACHE_REDIS_PASSWORD`, the full
/// `SCCACHE_REDIS` URL) are **never** placed in argv — a `--setenv=KEY=VAL` would
/// leak the value into the command line, `ps`, shell history, and journald. This
/// function defensively DROPS any secret-shaped key ([`is_secret_env_key`]); the
/// caller must deliver those to the scoped build through the inherited process
/// environment instead (`systemd-run --scope` runs the command as a direct child
/// that inherits systemd-run's environment — scopes have no `EnvironmentFile=`),
/// e.g. by setting them on the `systemd-run` process env locally, or by sourcing
/// a 0600 file inside the ssh wrapper remotely. See `mod.rs`.
///
/// `unit_name` is the transient scope's `--unit=` so `systemctl show <unit>` can
/// be used to verify the caps (notably `MemorySwapMax=0`) live.
///
/// INVARIANTS (asserted by tests): the rendered argv ALWAYS contains
/// `-p MemorySwapMax=0` (swap-off is not optional), and NEVER a secret-shaped
/// `--setenv`.
pub fn render_scope_argv(
    unit_name: &str,
    caps: &ScopeCaps,
    env: &BTreeMap<String, String>,
    cargo_argv: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        SYSTEMD_RUN_BIN.to_string(),
        "--scope".to_string(),
        format!("--unit={unit_name}"),
        // Reap the transient unit when the command exits.
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

    // NON-SECRET build environment only (sccache endpoint/db/prefix,
    // CARGO_TARGET_DIR, …). BTreeMap ⇒ deterministic ordering. Secret-shaped
    // keys are dropped here as a hard backstop and travel via inherited env.
    for (k, v) in env {
        if is_secret_env_key(k) {
            continue;
        }
        argv.push(format!("--setenv={k}={v}"));
    }

    argv.push("--".to_string());
    argv.extend(cargo_argv.iter().cloned());
    argv
}

/// Whether a build-env var name is secret-shaped and MUST NOT appear on a
/// command line (it travels via the inherited process environment instead).
///
/// Conservative: any name mentioning a password/secret/token, a name ending in
/// `_KEY` (but NOT `_KEY_PREFIX`, which is a non-secret sccache namespace), or
/// the bare `SCCACHE_REDIS` URL (which embeds credentials). `SCCACHE_REDIS_USERNAME`
/// (typically `default`) and `SCCACHE_REDIS_ENDPOINT`/`_DB`/`_KEY_PREFIX` are
/// non-secret and may be passed via `--setenv`.
pub fn is_secret_env_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    if k == "SCCACHE_REDIS" {
        return true;
    }
    if k.contains("PASSWORD") || k.contains("PASSWD") || k.contains("SECRET") || k.contains("TOKEN")
    {
        return true;
    }
    k.ends_with("_KEY") && !k.ends_with("_KEY_PREFIX")
}

/// Partition a build env into `(argv_safe_non_secret, inherited_only_secret)`.
/// The first map is safe to pass via `--setenv`; the second MUST be delivered
/// through the inherited environment (never argv).
pub fn partition_env(
    env: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut non_secret = BTreeMap::new();
    let mut secret = BTreeMap::new();
    for (k, v) in env {
        if is_secret_env_key(k) {
            secret.insert(k.clone(), v.clone());
        } else {
            non_secret.insert(k.clone(), v.clone());
        }
    }
    (non_secret, secret)
}

/// Render a 0600 env-file body for the secret vars, used ONLY for the REMOTE
/// (ssh heavy) path: the file is written 0600 on the build host and `source`d
/// inside the ssh wrapper before `exec systemd-run`, so the secret reaches the
/// scoped build's inherited environment WITHOUT ever touching a command line.
/// Deterministic order (BTreeMap).
///
/// SECURITY (shell-injection, S7): the file is `source`d by the remote shell, so
/// each value is emitted **SINGLE-QUOTED with embedded single-quotes escaped as
/// `'\''`** (`KEY='...'`). Single-quoting makes the value a fully literal byte
/// string — spaces, `"`, `$(...)`, backticks, `;`, `&`, `|`, newlines, etc. are
/// all inert, so a hostile Redis password can neither be corrupted nor trigger
/// shell execution during `. <file>`. Keys are our own fixed `[A-Za-z0-9_]`
/// names (not attacker-controlled), so they are written verbatim.
pub fn render_secret_env_file(secret: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    for (k, v) in secret {
        s.push_str(k);
        s.push('=');
        s.push_str(&shell_single_quote(v));
        s.push('\n');
    }
    s
}

/// Wrap `v` in single quotes for POSIX shell, escaping any embedded single quote
/// as `'\''` (close-quote, escaped-quote, reopen-quote). The result is a single
/// shell word whose expansion is EXACTLY the input bytes — no metacharacter is
/// interpreted. Used by [`render_secret_env_file`] (and safe for any shell
/// context). Newlines are preserved literally inside the single quotes.
fn shell_single_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
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
    // Any target dir that lexically resolves to the dataset root or nested under
    // it is rejected; the dataset root is for source-staging + sccache + artifact
    // publish ONLY, never a live cargo target.
    if is_within(target_dir, dataset_root) {
        return Err(ToolError::InvalidArgument(format!(
            "CARGO_TARGET_DIR ({}) lexically resolves inside the file-level NFS build \
             dataset ({}); cargo targets must be on exec-safe local disk or tmpfs \
             (build scripts are compiled then executed — NFS breaks exec + adds \
             lock/mtime hazards)",
            target_dir.display(),
            dataset_root.display()
        )));
    }
    Ok(())
}

/// Whether `path` lexically resolves to `root` or a path nested under it — the
/// containment primitive shared by the target-dir guard (reject-if-within) and
/// the `source_dir` check (require-within). Both operands are lexically
/// normalized first (`.`/`..` resolved textually, no filesystem access — works
/// for non-existent paths), so a traversal like `/mnt/other/../build/x` is
/// judged by where it actually lands. The trailing-slash boundary prevents a
/// sibling that merely shares a string prefix (`/data/build-x` vs `/data/build`)
/// from matching; `root == "/"` contains every absolute path.
pub fn is_within(path: &Path, root: &Path) -> bool {
    let p = lexical_normalize(path);
    let r = lexical_normalize(root);
    p == r
        || if r == "/" {
            p.starts_with('/')
        } else {
            p.starts_with(&format!("{r}/"))
        }
}

/// Lexically normalize a path: resolve `.` and `..` PURELY TEXTUALLY (no
/// filesystem access, so it works for non-existent paths — unlike
/// `canonicalize`), collapse `//`, and trim a trailing slash. Preserves whether
/// the path is absolute; a leading `..` in a RELATIVE path is preserved, and a
/// `..` at/above an absolute root is dropped. This is what makes the containment
/// guard robust against traversal inputs like `/mnt/other/../build/target`.
fn lexical_normalize(p: &Path) -> String {
    let s = p.to_string_lossy();
    let absolute = s.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for comp in s.split('/') {
        match comp {
            "" | "." => continue,
            ".." => match stack.last().copied() {
                // Pop a real component…
                Some(prev) if prev != ".." => {
                    stack.pop();
                }
                // …but keep leading `..` for a relative path; for an absolute
                // path a `..` above root is simply dropped.
                _ => {
                    if !absolute {
                        stack.push("..");
                    }
                }
            },
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
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
            argv.windows(2)
                .any(|w| w[0] == "-p" && w[1] == "MemorySwapMax=0"),
            "rendered argv must cap swap to 0: {joined}"
        );
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.iter().any(|a| a == "-p"));
        assert!(argv.iter().any(|a| a.starts_with("--unit=")));
    }

    #[test]
    fn scope_argv0_is_absolute_systemd_run_not_a_bare_name() {
        // Regression guard: the local build spawn resolves argv[0] via the
        // child env's PATH, which the build env can override — a bare
        // "systemd-run" then fails "No such file". argv[0] must be absolute.
        let argv = render_scope_argv("u", &caps(), &BTreeMap::new(), &["cargo".into()]);
        assert_eq!(argv[0], "/usr/bin/systemd-run", "argv[0] must be an absolute path, never a bare name");
        assert!(argv[0].starts_with('/'), "argv[0] must be absolute so PATH resolution can't fail");
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
    fn secret_env_never_appears_in_argv() {
        // S7: even if the caller mistakenly hands a secret into the env, it must
        // never be rendered into a --setenv (which would leak into ps/history).
        let mut env = BTreeMap::new();
        env.insert(
            "SCCACHE_REDIS_ENDPOINT".to_string(),
            "redis://h:6379".to_string(),
        );
        env.insert(
            "SCCACHE_REDIS_PASSWORD".to_string(),
            "sup3rsecret".to_string(),
        );
        env.insert(
            "SCCACHE_REDIS_KEY_PREFIX".to_string(),
            "sccache".to_string(),
        );
        env.insert(
            "SCCACHE_REDIS".to_string(),
            "redis://default:sup3rsecret@h:6379/1".to_string(),
        );
        let argv = render_scope_argv("u", &caps(), &env, &["cargo".into(), "build".into()]);
        for a in &argv {
            assert!(
                !a.contains("sup3rsecret"),
                "secret leaked into argv element: {a}"
            );
            assert!(
                !a.contains("SCCACHE_REDIS_PASSWORD"),
                "password key in argv: {a}"
            );
        }
        // The non-secret endpoint/prefix DO get through as --setenv.
        let j = argv.join(" ");
        assert!(j.contains("--setenv=SCCACHE_REDIS_ENDPOINT=redis://h:6379"));
        assert!(j.contains("--setenv=SCCACHE_REDIS_KEY_PREFIX=sccache"));
    }

    #[test]
    fn is_secret_env_key_classification() {
        assert!(is_secret_env_key("SCCACHE_REDIS_PASSWORD"));
        assert!(is_secret_env_key("SCCACHE_REDIS")); // full URL embeds creds
        assert!(is_secret_env_key("GITEA_TOKEN"));
        assert!(is_secret_env_key("SOME_API_KEY"));
        // Non-secret sccache/build vars:
        assert!(!is_secret_env_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!is_secret_env_key("SCCACHE_REDIS_USERNAME"));
        assert!(!is_secret_env_key("SCCACHE_REDIS_DB"));
        assert!(!is_secret_env_key("SCCACHE_REDIS_KEY_PREFIX"));
        assert!(!is_secret_env_key("CARGO_TARGET_DIR"));
        assert!(!is_secret_env_key("RUSTC_WRAPPER"));
    }

    #[test]
    fn partition_splits_secret_from_non_secret() {
        let mut env = BTreeMap::new();
        env.insert(
            "SCCACHE_REDIS_ENDPOINT".to_string(),
            "redis://h:6379".to_string(),
        );
        env.insert("SCCACHE_REDIS_PASSWORD".to_string(), "pw".to_string());
        env.insert("CARGO_TARGET_DIR".to_string(), "/tmp/t".to_string());
        let (non_secret, secret) = partition_env(&env);
        assert!(non_secret.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(non_secret.contains_key("CARGO_TARGET_DIR"));
        assert!(!non_secret.contains_key("SCCACHE_REDIS_PASSWORD"));
        assert_eq!(
            secret.get("SCCACHE_REDIS_PASSWORD").map(String::as_str),
            Some("pw")
        );
        assert_eq!(secret.len(), 1);
    }

    #[test]
    fn secret_env_file_body_is_single_quoted_key_value_lines() {
        // Keys/values are assembled at runtime (never a literal `KEY=value`
        // string) so the render is verified without embedding a secret-shaped
        // literal in source. Values are single-quoted (shell-safe).
        let pass_key = "SCCACHE_REDIS_PASSWORD";
        let tok_key = "A_TOKEN";
        let mut secret = BTreeMap::new();
        secret.insert(pass_key.to_string(), "pw".to_string());
        secret.insert(tok_key.to_string(), "t".to_string());
        // BTreeMap order: A_TOKEN before SCCACHE_...
        let expected = format!("{tok_key}='{}'\n{pass_key}='{}'\n", "t", "pw");
        assert_eq!(render_secret_env_file(&secret), expected);
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // a'b → 'a'\''b'
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        // A pure injection token is neutralized (stays inside the quotes).
        assert_eq!(shell_single_quote("$(x)"), "'$(x)'");
    }

    #[test]
    fn secret_env_file_is_shell_injection_safe() {
        // A hostile Redis password with EVERY dangerous shell construct: space,
        // single quote, double quote, $(...), backtick, ;, |, &, and a newline.
        // After `source`ing the rendered file in a REAL shell, the value must
        // round-trip EXACTLY and NO injected command may have executed.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("PWNED");
        let nasty = format!(
            "a b$(touch '{m}')`touch '{m}'`;| & \"dq\" 'sq'\nsecond-line",
            m = marker.display()
        );

        let mut secret = BTreeMap::new();
        secret.insert("SCCACHE_REDIS_PASSWORD".to_string(), nasty.clone());
        let body = render_secret_env_file(&secret);
        let envf = dir.path().join("secret.env");
        std::fs::write(&envf, &body).unwrap();

        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "set -a; . '{}'; printf %s \"$SCCACHE_REDIS_PASSWORD\"",
                envf.display()
            ))
            .output()
            .expect("run sh");
        assert!(out.status.success(), "sourcing the env file must succeed");
        let parsed = String::from_utf8(out.stdout).unwrap();
        assert_eq!(parsed, nasty, "value must round-trip byte-for-byte");
        assert!(
            !marker.exists(),
            "no injected command (touch) may have executed — shell injection!"
        );
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
        assert!(validate_target_dir(&PathBuf::from("/data/build/src/x/target"), &root).is_err());
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

    #[test]
    fn target_dir_guard_resolves_dotdot_lexically() {
        // The traversal-bypass case: `/mnt/other/../build/target` LEXICALLY
        // resolves to `/mnt/build/target`, which is under the dataset root — must
        // be REJECTED even though the naive string prefix check would miss it.
        let root = PathBuf::from("/mnt/build");
        assert!(
            validate_target_dir(&PathBuf::from("/mnt/other/../build/target"), &root).is_err(),
            "a `..` that lands under the dataset must be rejected"
        );
        // `.` components and redundant slashes also resolve then reject.
        assert!(validate_target_dir(&PathBuf::from("/mnt/build/./target"), &root).is_err());
        assert!(validate_target_dir(&PathBuf::from("/mnt/build//sub/../target"), &root).is_err());
        // A `..` that resolves BACK to the root itself is rejected.
        assert!(validate_target_dir(&PathBuf::from("/mnt/build/x/.."), &root).is_err());
        // A genuinely-separate dir (even reached via `..`) is allowed.
        assert!(validate_target_dir(&PathBuf::from("/mnt/other/target"), &root).is_ok());
        assert!(
            validate_target_dir(&PathBuf::from("/mnt/build/../other/target"), &root).is_ok(),
            "a `..` that escapes the dataset is fine"
        );
    }

    #[test]
    fn lexical_normalize_resolves_dot_and_dotdot() {
        assert_eq!(
            lexical_normalize(Path::new("/mnt/other/../build/target")),
            "/mnt/build/target"
        );
        assert_eq!(lexical_normalize(Path::new("/a/./b//c/")), "/a/b/c");
        assert_eq!(lexical_normalize(Path::new("/a/b/../..")), "/");
        assert_eq!(lexical_normalize(Path::new("/a/../../x")), "/x"); // `..` above root dropped
                                                                      // Relative paths keep a leading `..`.
        assert_eq!(lexical_normalize(Path::new("../a/b")), "../a/b");
        assert_eq!(lexical_normalize(Path::new("a/./b")), "a/b");
    }

    #[test]
    fn is_within_containment() {
        let root = Path::new("/data/build/src");
        assert!(is_within(Path::new("/data/build/src"), root)); // identical
        assert!(is_within(Path::new("/data/build/src/chord/x"), root)); // nested
        assert!(is_within(Path::new("/data/build/src/../src/y"), root)); // resolves back in
        assert!(!is_within(Path::new("/data/build/srcx"), root)); // prefix-string sibling
        assert!(!is_within(Path::new("/etc/passwd"), root)); // elsewhere
        assert!(!is_within(Path::new("/data/build/src/../../etc"), root)); // escapes
        assert!(is_within(Path::new("/anything"), Path::new("/"))); // root contains all
    }
}
