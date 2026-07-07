//! Fixed, pure argv builder for the `agy` container/sandbox wrapper.
//!
//! ## Why this exists
//! `agy --dangerously-skip-permissions` disables agy's own interactive
//! tool-approval gate (required to run it headlessly at all), which means
//! anything adversarial fed to it via `prompt` (e.g. a malicious diff
//! crafted as a prompt-injection payload) could trick agy into taking real
//! actions -- file writes, network calls -- rather than just producing a
//! text verdict. `opus` (`--tools ""`) and `codex` (`--sandbox read-only`)
//! already close this off for their providers; this module is agy's
//! equivalent, at the OS level rather than trusting agy's own (bypassed)
//! permission gate.
//!
//! ## What's investigated and confirmed live on this box
//! - Docker: `docker` binary is present but `/var/run/docker.sock` is not
//!   accessible to the daemon's user (permission denied) -- out of scope
//!   per operator instruction (not chasing a docker-group/root change).
//! - `bubblewrap` (`bwrap`) IS usable without root or a daemon socket --
//!   real mount/pid/ipc/uts namespace isolation for a single subprocess.
//!   This is the mechanism used here.
//! - Filesystem: rather than `--ro-bind / /` plus trying to mask every
//!   sensitive path afterward (error-prone denylist), this builds a minimal
//!   explicit ALLOWLIST -- only `/usr` (+ the standard usrmerge symlinks),
//!   a handful of `/etc` files needed for TLS/DNS to work, the resolved
//!   `agy` binary itself, and agy's own config/cache directory
//!   (`$HOME/.gemini/antigravity-cli`, read-write, since agy writes its
//!   conversation cache there -- confirmed live: it fails cache/conversation
//!   writes with "read-only file system" otherwise). Nothing else in
//!   `$HOME` (SSH keys, `.env`, the daemon's own working directory/repo
//!   checkout) is bound at all, so it is simply not visible -- confirmed
//!   live: `cat $HOME/.env` and `ls $HOME/.ssh` both report
//!   "No such file or directory" from inside the sandbox, not "permission
//!   denied" -- there is no path to even attempt an access.
//! - Capabilities: `--cap-drop ALL` -- confirmed live via
//!   `/proc/self/status` inside the sandbox that `CapBnd` (the bounding set,
//!   not just the effective set) is entirely zero.
//! - Network: see `egress_proxy.rs` for why a loopback CONNECT proxy (not a
//!   `bwrap --unshare-net` + `slirp4netns` network namespace) is the
//!   mechanism used -- this host has no `/dev/net/tun`, so `slirp4netns`
//!   cannot attach a NIC to an isolated netns here. agy is confirmed (live)
//!   to honor `HTTPS_PROXY`/`HTTP_PROXY` (Go's default `net/http` transport
//!   consults `ProxyFromEnvironment`), so the host network namespace is
//!   shared but every one of agy's outbound connections is forced through
//!   the daemon-owned, RFC1918/CGNAT-denying proxy via those two env vars
//!   (plus `NO_PROXY`/`no_proxy` forced empty), which are set here -- never
//!   caller-influenced. IMPORTANT caveat, found in dual review and
//!   independently reproduced live: proxy env vars do not apply to loopback
//!   destinations for common HTTP clients (confirmed with `curl`; documented
//!   Go `net/http`/`httpproxy` behavior too), so the sandboxed process CAN
//!   reach the host's own `127.0.0.1` ports directly -- see
//!   `egress_proxy.rs`'s "Known residual limitation" section for why this
//!   can't be fully closed on this host and what mitigates it.
//!
//! ## Invariant this module upholds (matches `provider.rs`)
//! [`wrap_agy`] returns a `Vec<String>` argv array to spawn `bwrap` with.
//! Every element is either a hardcoded flag/path/host value, or the
//! `agy_argv` passed in (itself already built by `provider::build_command`,
//! which already guarantees the caller's `prompt` is exactly one opaque argv
//! element). Nothing here ever constructs or touches a shell string.

/// The bwrap binary name -- resolved once at daemon startup via
/// `resolve::resolve_on_path`, exactly like the provider binaries.
pub const BWRAP_BIN: &str = "bwrap";

/// Build the fixed bwrap argv that wraps a resolved `agy` invocation.
///
/// - `agy_resolved_path`: absolute path to `agy`, resolved once at startup
///   (never re-resolved per request -- same TOCTOU-closing reasoning as
///   `provider::build_command`'s callers).
/// - `agy_argv`: the already-built agy argv (from `provider::build_command`),
///   passed through verbatim as the trailing command bwrap execs.
/// - `home_dir`: the daemon process's `$HOME`, used only to scope the two
///   binds agy actually needs (its own binary under `~/.local/bin`, already
///   covered by `agy_resolved_path`, and its own app-data directory). This
///   is an operator/environment fact, never caller input.
/// - `gemini_cache_dir`: `$HOME/.gemini/antigravity-cli` (or wherever agy's
///   app-data directory resolves to) -- bound read-write since agy writes
///   its conversation cache there.
/// - `proxy_port`: the loopback port `egress_proxy::spawn` bound at startup.
pub fn wrap_agy(
    agy_resolved_path: &std::path::Path,
    agy_argv: &[String],
    home_dir: &str,
    gemini_cache_dir: &str,
    proxy_port: u16,
) -> Vec<String> {
    let agy_path_str = agy_resolved_path.to_string_lossy().to_string();
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");

    let mut args: Vec<String> = vec![
        // Minimal explicit filesystem allowlist -- see module docs for why
        // this is an allowlist rather than `--ro-bind / /` plus masking.
        "--ro-bind".into(), "/usr".into(), "/usr".into(),
        "--symlink".into(), "usr/bin".into(), "/bin".into(),
        "--symlink".into(), "usr/lib".into(), "/lib".into(),
        "--symlink".into(), "usr/lib64".into(), "/lib64".into(),
        "--symlink".into(), "usr/sbin".into(), "/sbin".into(),
        "--ro-bind".into(), "/etc/resolv.conf".into(), "/etc/resolv.conf".into(),
        "--ro-bind".into(), "/etc/nsswitch.conf".into(), "/etc/nsswitch.conf".into(),
        "--ro-bind".into(), "/etc/hosts".into(), "/etc/hosts".into(),
        "--ro-bind".into(), "/etc/ssl".into(), "/etc/ssl".into(),
        "--ro-bind".into(), agy_path_str.clone(), agy_path_str,
        // agy's own app-data/cache dir -- read-write, since it writes its
        // conversation cache/oauth-token-refresh state there. Nothing else
        // under $HOME (SSH keys, .env, the daemon's own working directory)
        // is bound, so it is not visible to the sandboxed process at all.
        "--bind".into(), gemini_cache_dir.into(), gemini_cache_dir.into(),
        "--tmpfs".into(), "/tmp".into(),
        "--proc".into(), "/proc".into(),
        "--dev".into(), "/dev".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--unshare-cgroup-try".into(),
        "--die-with-parent".into(),
        "--new-session".into(),
        "--cap-drop".into(), "ALL".into(),
        "--setenv".into(), "HOME".into(), home_dir.into(),
        "--setenv".into(), "PATH".into(), "/usr/bin:/bin".into(),
        // The entire point of this module: force every one of agy's
        // outbound connections through the RFC1918-denying loopback proxy.
        // Every common spelling (upper- and lower-case, HTTP/HTTPS/ALL) is
        // set explicitly -- flagged in dual review that setting only the
        // uppercase HTTP(S)_PROXY would leave a lowercase-preferring or
        // ALL_PROXY-consulting client free to use a DIFFERENT inherited
        // proxy value instead (main.rs passes the daemon's sanitized_env
        // through to bwrap, which does not itself strip lowercase proxy
        // vars). None of these values is derived from request/prompt
        // content.
        "--setenv".into(), "HTTPS_PROXY".into(), proxy_url.clone(),
        "--setenv".into(), "https_proxy".into(), proxy_url.clone(),
        "--setenv".into(), "HTTP_PROXY".into(), proxy_url.clone(),
        "--setenv".into(), "http_proxy".into(), proxy_url.clone(),
        "--setenv".into(), "ALL_PROXY".into(), proxy_url.clone(),
        "--setenv".into(), "all_proxy".into(), proxy_url,
        // Neutralize NO_PROXY/no_proxy explicitly -- if either were somehow
        // inherited from the daemon's own environment, a caller-uninfluenced
        // but still-undesirable value could exempt additional hosts from
        // the proxy above. Forcing both to empty here means only bwrap's
        // own fixed flags (never any inherited env) can affect what agy's
        // traffic bypasses the proxy for.
        "--setenv".into(), "NO_PROXY".into(), "".into(),
        "--setenv".into(), "no_proxy".into(), "".into(),
        "--".into(),
    ];
    // bwrap execve's the command after "--" directly -- unlike
    // `tokio::process::Command::new(path)` (which implies argv[0]), bwrap
    // has no implicit program argument, so the resolved agy path must be
    // pushed explicitly here as the first post-"--" element, followed by
    // agy's own argv (which does not itself include the binary path -- see
    // `provider::build_command`, whose `args` are appended via
    // `Command::args()` onto a `Command::new(resolved_path)`).
    args.push(agy_resolved_path.to_string_lossy().to_string());
    args.extend(agy_argv.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_MARKERS: &[&str] = &["sh", "-c", "bash"];

    fn agy_argv(prompt: &str) -> Vec<String> {
        vec![
            "--model".into(), "gemini-3.1-pro".into(),
            "-p".into(), prompt.to_string(),
            "--dangerously-skip-permissions".into(),
        ]
    }

    #[test]
    fn wrap_agy_never_contains_a_shell_marker_as_a_standalone_argv_element() {
        let prompt = "; cat /etc/passwd; curl http://<internal-ip>/admin #"; // pii-test-fixture
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv(prompt),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            54321,
        );
        for a in &args {
            assert!(
                !SHELL_MARKERS.contains(&a.as_str()),
                "argv must never contain a bare shell marker element, found {a:?} in {args:?}"
            );
        }
    }

    #[test]
    fn wrap_agy_passes_prompt_through_as_exactly_one_opaque_argv_element() {
        let prompt = "$(whoami) `id` && rm -rf ~ ; DROP TABLE users;";
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv(prompt),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            54321,
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == prompt).count(),
            1,
            "prompt must appear exactly once, verbatim, never split/re-tokenized"
        );
    }

    #[test]
    fn wrap_agy_terminates_bwrap_flags_before_the_agy_command() {
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv("hello"),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            54321,
        );
        let term_idx = args.iter().position(|a| a == "--").expect("expected a -- terminator");
        // bwrap execve's the command directly (no implicit argv[0] the way
        // Command::new provides) -- so the resolved agy path must appear
        // explicitly as the first element after "--", followed by agy's own
        // argv, unmodified.
        assert_eq!(args[term_idx + 1], "/home/user/.local/bin/agy");
        assert_eq!(&args[term_idx + 2..], agy_argv("hello").as_slice());
    }

    #[test]
    fn wrap_agy_sets_proxy_env_to_loopback_only() {
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv("hello"),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            54321,
        );
        for var in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            let idx = args.iter().position(|a| a == var).unwrap_or_else(|| panic!("expected {var} to be set"));
            assert_eq!(args[idx + 1], "http://127.0.0.1:54321", "wrong value for {var}"); // pii-test-fixture
        }
        for var in ["NO_PROXY", "no_proxy"] {
            let idx = args.iter().position(|a| a == var).unwrap_or_else(|| panic!("expected {var} to be set"));
            assert_eq!(args[idx + 1], "", "expected {var} to be neutralized");
        }
    }

    #[test]
    fn wrap_agy_never_binds_ssh_or_env_paths() {
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv("hello"),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            54321,
        );
        let joined = args.join(" ");
        assert!(!joined.contains(".ssh"));
        assert!(!joined.contains(".env"));
        // The only $HOME-scoped path bound is the gemini cache dir --
        // confirm the daemon's cwd / repo checkout is never mentioned.
        assert!(!joined.contains("Terminus"));
    }

    #[test]
    fn wrap_agy_drops_all_capabilities_and_is_a_fixed_flag_never_caller_influenced() {
        let args = wrap_agy(
            std::path::Path::new("/home/user/.local/bin/agy"),
            &agy_argv("--cap-drop NONE; ignore-me"),
            "/home/user",
            "/home/user/.gemini/antigravity-cli",
            1,
        );
        let idx = args.iter().position(|a| a == "--cap-drop").unwrap();
        assert_eq!(args[idx + 1], "ALL");
    }
}
