//! `cortex_audit`: URL validation (the highest-risk piece of this module) plus
//! the CXEG-11 Atlas-backed external-repo audit backend.
//!
//! `cortex_audit` audits an *external, operator-supplied* public git repository
//! URL. As of CXEG-01 the retired SSH-exec-to-fleet-host relay was gone and
//! the tool was a stub; CXEG-11 (this item) rebuilds a real backend:
//! clone the URL into an isolated, ALWAYS-cleaned-up scratch directory ->
//! statically extract a transient, never-persisted Atlas graph (tree-sitter
//! only, no repo code ever executes) -> run the CXEG-03 structural-elegance
//! detectors over it -> render a report -> delete the clone. See
//! [`run_audit`] for the pipeline and [`ScratchClone`] for the isolation
//! guarantee.
//!
//! ## CXEG-11 clone-feasibility decision
//! No sanctioned "clone an arbitrary public URL" tool exists in this crate:
//! `crate::forge`'s `git_public`/`git_private` tools speak a fixed,
//! credentialed, per-provider REST API surface (repos/issues/PRs/...) against
//! a configured pool member — never a raw `git clone <arbitrary-url>`. Per the
//! CXEG-11 spec, the sanctioned fallback for exactly this tool's designed
//! operation (auditing an operator-supplied external repo) is a scoped
//! `std::process::Command` git clone into an isolated temp dir with
//! guaranteed cleanup — that is what [`ScratchClone`] implements. This is a
//! narrower, more contained blast radius than the retired SSH-relay era ever
//! had (that implementation didn't even clone locally — it shipped the URL to
//! a remote fleet-host script and trusted whatever came back).
//!
//! ## `validate_repo_url`
//! The front-gate every `cortex_audit` call passes before the URL reaches any
//! backend. It rejects URL shapes that have no legitimate reason to be passed
//! to "clone a public git repo" — non-http(s) schemes, embedded credentials,
//! shell metacharacters, and (crucially) loopback / private / link-local /
//! metadata hosts in any of their common obfuscated encodings. That closes
//! off SSRF-style redirection of the clone step at internal/private network
//! targets under the guise of a "public repo audit". **Kept byte-for-byte
//! unchanged by CXEG-11** — it was already the strongest piece of this
//! module and has no dependency on the backend behind it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::cortex::{metrics, CortexConfig};
use crate::error::ToolError;
use crate::scribe::graph::{build_rust_graph, cluster, pagerank};

const MAX_URL_LEN: usize = 500;

/// Validate that `url` is a plausible **public** git repository URL:
/// - `http` or `https` scheme only (no `ssh://`, `git://`, `file://`,
///   `ftp://`, `data:`, etc. — those either escape the "public git host over
///   HTTPS" assumption the tool's own description makes, or have no business
///   being clone targets for an "external public Git repository" audit tool).
/// - No embedded userinfo (`user:pass@host`) — credential smuggling into a
///   downstream clone command has no legitimate use here.
/// - No control characters, whitespace, or shell metacharacters — a URL has
///   no legitimate reason to contain any of these, so rejecting them outright
///   is strictly safer than relying on any downstream quoting alone.
/// - Host must not resolve, textually, to a loopback/private/link-local
///   address or `localhost` — a clone/audit backend has no business being
///   pointed at internal addresses under the guise of "public repo audit",
///   which would be an SSRF-shaped hole.
/// - Length-capped like every other free-text field in this crate.
pub fn validate_repo_url(url: &str) -> Result<(), ToolError> {
    if url.is_empty() {
        return Err(ToolError::InvalidArgument("'url' must not be empty".into()));
    }
    if url.chars().count() > MAX_URL_LEN {
        return Err(ToolError::InvalidArgument(format!(
            "'url' exceeds {MAX_URL_LEN} character limit"
        )));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ToolError::InvalidArgument(
            "'url' must not contain whitespace or control characters".into(),
        ));
    }
    const FORBIDDEN_SHELL_CHARS: &[char] =
        &[';', '&', '|', '`', '$', '(', ')', '<', '>', '\\', '"', '\'', '\n', '\r'];
    if url.chars().any(|c| FORBIDDEN_SHELL_CHARS.contains(&c)) {
        return Err(ToolError::InvalidArgument(
            "'url' contains characters that are never valid in a git repo URL".into(),
        ));
    }

    let scheme_end = url.find("://").ok_or_else(|| {
        ToolError::InvalidArgument("'url' must be an http:// or https:// URL".into())
    })?;
    let scheme = &url[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return Err(ToolError::InvalidArgument(format!(
            "'url' scheme '{scheme}' is not allowed — only http/https public git URLs are accepted"
        )));
    }

    let rest = &url[scheme_end + 3..];
    if rest.is_empty() {
        return Err(ToolError::InvalidArgument("'url' has no host".into()));
    }
    // authority ends at the first '/', '?', or '#'
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    if authority.contains('@') {
        return Err(ToolError::InvalidArgument(
            "'url' must not contain embedded credentials (user@host)".into(),
        ));
    }

    // Strip a port suffix, if any (but keep IPv6 bracket literals intact for
    // the loopback/private checks below).
    let host = if let Some(bracket_end) = authority.find(']') {
        &authority[..=bracket_end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };

    if host.is_empty() {
        return Err(ToolError::InvalidArgument("'url' has no host".into()));
    }

    let host_lower = host.to_ascii_lowercase();
    if is_disallowed_host(&host_lower) {
        return Err(ToolError::InvalidArgument(
            "'url' must point to a public host, not a local/private/internal address".into(),
        ));
    }

    Ok(())
}

/// Textual check for loopback / private / link-local / metadata hosts. This
/// is a defense-in-depth string check, not a DNS-resolving check (validation
/// runs on the textual URL before any backend resolves or clones it). It
/// catches the obvious, common ways an operator or an attacker-controlled
/// input could try to point the tool at internal infrastructure.
///
/// ## Fail-closed on ambiguous numeric hosts
///
/// A real HTTP/git client resolves far more host shapes to an IPv4 address
/// than a plain 4-octet decimal dotted-quad: bare decimal integers
/// (`2130706433` == `127.0.0.1`), hex (`0x7f000001`), octal-by-leading-zero — pii-test-fixture
/// (`0177.0.0.1` — glibc's `inet_aton` reads the leading zero as octal `127`,
/// i.e. loopback, even though it parses as decimal `177` under a naive
/// string-to-u8 parse), 2/3-part shorthand (`127.1`), and IPv4-mapped IPv6
/// (`::ffff:127.0.0.1`). An earlier version of this function only recognized
/// the plain 4-octet decimal form and let every other encoding through
/// unchecked — a classic SSRF-filter "IP obfuscation" gap. The fix: treat
/// *any* numeric-looking host as a candidate IP address that must parse
/// strictly (via [`parse_strict_ipv4`]) to be judged safe; if it looks
/// numeric/IP-shaped but doesn't parse strictly, it is rejected outright
/// (fail closed) rather than allowed through by omission (fail open).
fn is_disallowed_host(host_lower: &str) -> bool {
    if host_lower == "localhost" || host_lower.ends_with(".localhost") {
        return true;
    }
    if host_lower == "0" {
        return true;
    }

    // Bracketed IPv6 literal, e.g. "[::1]" or "[::ffff:127.0.0.1]".
    if host_lower.starts_with('[') {
        let inner = host_lower.trim_start_matches('[').trim_end_matches(']');
        // IPv4-mapped IPv6 — validate the embedded IPv4 address itself
        // rather than only pattern-matching the "::ffff:" prefix.
        if let Some(mapped) = inner.strip_prefix("::ffff:") {
            return match parse_strict_ipv4(mapped) {
                Some(v4) => is_disallowed_ipv4(v4),
                None => true, // unparseable/ambiguous embedded address -> fail closed
            };
        }
        return match inner.parse::<std::net::Ipv6Addr>() {
            Ok(v6) => is_disallowed_ipv6(v6),
            Err(_) => true, // unparseable bracketed literal -> fail closed
        };
    }

    // A bare "0x..." hex literal, or a purely digit/dot string, is always
    // treated as an IP-address candidate (no legitimate public DNS hostname
    // is shaped like either) and must parse as a strict dotted-quad to pass.
    let looks_like_ip_candidate = host_lower.starts_with("0x")
        || host_lower.chars().all(|c| c.is_ascii_digit() || c == '.');
    if looks_like_ip_candidate {
        return match parse_strict_ipv4(host_lower) {
            Some(v4) => is_disallowed_ipv4(v4),
            None => true, // decimal-integer/hex/octal/shorthand encoding -> fail closed
        };
    }

    false
}

/// Parse `s` as a strict, unambiguous IPv4 dotted-quad: exactly four
/// dot-separated decimal octets, each 1-3 ASCII digits, none with a leading
/// zero (a leading-zero octet like "0177" is ambiguous — some C library
/// resolvers, e.g. glibc's `inet_aton`, read it as octal, which would
/// silently disagree with a plain decimal reading of the same text).
/// Anything else (bare integers, hex, 2/3-part shorthand, leading zeros)
/// returns `None` so the caller fails closed.
fn parse_strict_ipv4(s: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 3 || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        octets[i] = part.parse::<u8>().ok()?;
    }
    Some(std::net::Ipv4Addr::from(octets))
}

/// Range checks using the standard library's own classification methods
/// (`is_loopback`/`is_private`/`is_link_local`/`is_unspecified`) rather than
/// hand-rolled octet comparisons, plus the well-known cloud metadata address.
fn is_disallowed_ipv4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.octets()[0] == 0
        || v4 == std::net::Ipv4Addr::new(169, 254, 169, 254)
}

fn is_disallowed_ipv6(v6: std::net::Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    let seg0 = v6.segments()[0];
    (0xfe80..=0xfebf).contains(&seg0) // link-local
        || (0xfc00..=0xfdff).contains(&seg0) // unique local
}

// ---------------------------------------------------------------------------
// CXEG-11: isolated scratch clone
// ---------------------------------------------------------------------------

/// An isolated scratch directory holding a shallow clone of one external
/// repo, deleted on EVERY exit path: normal drop, an early `?`-propagated
/// error, or an unwinding panic (`Drop::drop` still runs during unwind,
/// which is the only case that matters here — this process never compiles
/// with `panic = "abort"`, and if it ever did, the OS reclaims `temp_dir()`
/// on the next boot rather than leaking silently forever).
#[derive(Debug)]
struct ScratchClone {
    /// The scratch dir itself (parent of `repo/` and the isolated `home/`
    /// handed to the `git` subprocess as `$HOME`) — removed wholesale on
    /// drop, so cleaning up `dir` also cleans up the isolated home.
    dir: PathBuf,
    /// Opaque label for this run, used only as an in-memory, never-persisted
    /// graph project id — carries no operator/host data.
    slug: String,
}

impl ScratchClone {
    fn repo_path(&self) -> PathBuf {
        self.dir.join("repo")
    }

    /// Clone `url` into a fresh, isolated scratch dir with:
    /// - `--depth 1 --single-branch --no-tags --no-recurse-submodules`: the
    ///   smallest possible checkout (no history, no other branches, no tags,
    ///   no submodule fetches — a malicious submodule URL never gets a
    ///   second, unvalidated clone target).
    /// - `core.hooksPath=/dev/null`: a cloned repo's hooks (if any shipped in
    ///   its working tree) are never installed as executable hooks.
    /// - an isolated `HOME` + `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL=/dev/null`:
    ///   no operator credential helper, global gitconfig, or stored token is
    ///   ever reachable from this subprocess.
    /// - `GIT_TERMINAL_PROMPT=0` + `GIT_ASKPASS` pointed at a no-op: a private
    ///   or auth-walled URL fails fast instead of hanging on a prompt.
    /// - a wall-clock timeout (`timeout_secs`, [`CortexConfig::audit_clone_timeout_secs`]):
    ///   the subprocess is killed and the scratch dir is still cleaned up
    ///   (via `Drop`) if the clone runs long — bounds clone TIME.
    ///
    /// Bounding clone SIZE is a separate step ([`dir_size`]) run by the
    /// caller once the clone succeeds, since `git clone` itself has no
    /// portable "abort past N bytes" flag.
    async fn create(url: &str, timeout_secs: u64) -> Result<Self, ToolError> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let slug = format!("{}-{nanos}", std::process::id());
        let dir = std::env::temp_dir().join(format!("terminus-cortex-audit-{slug}"));
        let fake_home = dir.join("home");
        fs::create_dir_all(&fake_home)
            .map_err(|e| ToolError::Execution(format!("create audit scratch dir: {e}")))?;

        // From this point on `scratch` owns `dir` and Drop::drop removes it
        // on every remaining exit path in this function (`?` included).
        let scratch = ScratchClone { dir: dir.clone(), slug };

        let repo_dir = scratch.repo_path();
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("clone")
            .arg("--depth")
            .arg("1")
            .arg("--single-branch")
            .arg("--no-tags")
            .arg("--no-recurse-submodules")
            .arg("--config")
            .arg("core.hooksPath=/dev/null")
            .arg(url)
            .arg(&repo_dir)
            .env("HOME", &fake_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("GIT_SSH_COMMAND", "false") // no ssh:// transport ever, belt-and-suspenders w/ validate_repo_url
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn 'git clone': {e}")))?;

        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(scratch),
            Ok(Ok(status)) => Err(ToolError::Execution(format!("'git clone' exited with {status}"))),
            Ok(Err(e)) => Err(ToolError::Execution(format!("'git clone' failed to run: {e}"))),
            Err(_) => {
                // Timed out: kill the still-running subprocess before this
                // function returns (and `scratch` drops, removing the dir).
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(ToolError::Execution(format!(
                    "'git clone' exceeded the {timeout_secs}s audit timeout"
                )))
            }
        }
    }
}

impl Drop for ScratchClone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Sum of file sizes under `root`, walked without following symlinks (a
/// malicious repo's symlink could otherwise point outside the clone and
/// report/consume something never actually cloned). Best-effort: an
/// unreadable entry is skipped rather than failing the whole walk, since this
/// is a size ESTIMATE used only to enforce [`CortexConfig::audit_max_clone_bytes`],
/// not a security boundary itself (`walk_rs`'s own symlink guard is that).
fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// The CXEG-11 pipeline: clone `url` into an isolated scratch dir, build a
/// transient (never persisted to `GraphStore`, never given a real
/// `project_id`) Atlas graph via pure static/tree-sitter extraction — the
/// SAME `build_rust_graph`/`walk_rs` path `scribe_kg_build` uses, so no
/// second extractor exists — run the CXEG-03 structural detectors over it,
/// and render a report. The scratch dir (and everything under it) is removed
/// before this function returns, success or failure, via [`ScratchClone`]'s
/// `Drop`.
///
/// Deliberately calls `build_rust_graph`/`cluster`/`pagerank` directly rather
/// than going through the `scribe_kg_build` TOOL: that tool's `repo_path`
/// confinement (`SCRIBE_ALLOWED_REPO_ROOTS`) exists to stop an operator- or
/// caller-supplied path from reading arbitrary host filesystem locations —
/// a concern that doesn't apply here, since `repo_dir` was never
/// caller-supplied; it's a scratch dir this function created and clone-wrote
/// itself. It also deliberately uses [`metrics::compute_structural_signals`]
/// (the sync, no-vector-store subset), not the full async
/// [`metrics::compute_signals`]: semantic-duplication detection compares a
/// touched node's embedding against the PROJECT's own persisted vector-store
/// rows, and this graph is intentionally never persisted or embedded — there
/// is no stored project to compare against, and embedding + storing vectors
/// for an arbitrary external repo would leak its content into local
/// infrastructure state, exactly what "transient" is meant to avoid.
///
/// No network beyond the clone itself, and — the untrusted-clone safety
/// property this whole pipeline hinges on — **no code from the cloned repo is
/// ever executed**: `walk_rs` only ever calls `fs::read_to_string` on
/// allowlisted-extension files, and `build_rust_graph` is a pure tree-sitter
/// PARSE (no build scripts, no `cargo`/`npm`/interpreter invocation, no
/// import resolution that would need to load foreign code).
pub async fn run_audit(url: &str, config: &CortexConfig) -> Result<Value, ToolError> {
    let scratch = ScratchClone::create(url, config.audit_clone_timeout_secs).await?;
    let repo_path = scratch.repo_path();

    let clone_bytes = dir_size(&repo_path);
    if clone_bytes > config.audit_max_clone_bytes {
        return Err(ToolError::InvalidArgument(format!(
            "cloned repository is {clone_bytes} bytes, exceeding the {}-byte audit ceiling \
             (CORTEX_AUDIT_MAX_CLONE_BYTES) — refusing to build a graph over it",
            config.audit_max_clone_bytes
        )));
    }

    let (files, file_scan_capped) = crate::scribe::graph::build::walk_rs(&repo_path)?;
    if files.is_empty() {
        return Err(ToolError::InvalidArgument(
            "no supported source files found in the cloned repository".to_string(),
        ));
    }

    // Opaque, never-persisted graph id -- just needs to be stable within this
    // one call, not globally meaningful (this graph is never saved to
    // GraphStore and never reused across calls).
    let project_id = format!("cortex-audit-transient-{}", scratch.slug);
    let mut graph = build_rust_graph(&project_id, &files)?;
    cluster(&mut graph);
    pagerank(&mut graph);

    let all_node_ids: Vec<String> = graph.nodes().map(|n| n.id.clone()).collect();
    let signal_scope_capped = all_node_ids.len() > config.max_blast_nodes;
    let touched: Vec<String> = all_node_ids.into_iter().take(config.max_blast_nodes).collect();

    // Whole-repo audit -> every (capped) current node is "touched": there is
    // no diff to scope to, the point of this tool is auditing the repo's
    // overall structure, not one change against it.
    let signals = metrics::compute_structural_signals(&touched, &graph, config);
    let clusters: HashSet<u32> = graph.nodes().filter_map(|n| n.cluster).collect();

    Ok(json!({
        "status": "complete",
        "tool": "cortex_audit",
        "url": url,
        "stats": {
            "nodes": graph.node_count(),
            "edges": graph.edge_count(),
            "clusters": clusters.len(),
            "files_scanned": files.len(),
            "file_scan_cap_hit": file_scan_capped,
            "signal_scope_cap_hit": signal_scope_capped,
            "clone_bytes": clone_bytes,
        },
        "signals": signals,
        "signal_count": signals.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accepts_plausible_public_https_url() {
        assert!(validate_repo_url("https://github.com/owner/repo").is_ok());
    }

    #[test]
    fn test_accepts_http_scheme() {
        assert!(validate_repo_url("http://gitlab.example.com/owner/repo").is_ok());
    }

    #[test]
    fn test_rejects_empty() {
        assert!(validate_repo_url("").is_err());
    }

    #[test]
    fn test_rejects_ssh_scheme() {
        let err = validate_repo_url("ssh://<email>/owner/repo.git").unwrap_err(); // pii-test-fixture
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_rejects_file_scheme() {
        assert!(validate_repo_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn test_rejects_git_scheme() {
        assert!(validate_repo_url("git://github.com/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_data_scheme() {
        assert!(validate_repo_url("data:text/plain;base64,aGk=").is_err());
    }

    #[test]
    fn test_rejects_embedded_credentials() {
        assert!(validate_repo_url("https://user:<email>/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_localhost() {
        assert!(validate_repo_url("https://localhost/owner/repo").is_err());
        assert!(validate_repo_url("https://LOCALHOST/owner/repo").is_err());
    }

    // NOTE: the addresses in the tests below are RFC 1918 private-range /
    // loopback / link-local test fixtures for the SSRF guard, not real
    // infrastructure — they exist to prove `validate_repo_url` actually
    // rejects each reserved range it claims to.

    #[test]
    fn test_rejects_loopback_ipv4() {
        // test fixture: loopback address
        assert!(validate_repo_url("https://127.0.0.1/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_private_ipv4_ranges() {
        // test fixtures: RFC 1918 class A, B, and C private ranges
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_link_local_and_metadata() {
        // test fixture: the well-known cloud metadata endpoint (AWS/GCP/Azure)
        assert!(validate_repo_url("https://169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn test_rejects_ipv6_loopback() {
        assert!(validate_repo_url("https://[::1]/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_shell_metacharacters() {
        assert!(validate_repo_url("https://github.com/owner/repo; rm -rf /").is_err());
        assert!(validate_repo_url("https://github.com/owner/$(whoami)").is_err());
        assert!(validate_repo_url("https://github.com/owner/`whoami`").is_err());
        assert!(validate_repo_url("https://github.com/owner/repo|cat").is_err());
        assert!(validate_repo_url("https://github.com/owner/repo\nrm -rf /").is_err());
    }

    #[test]
    fn test_rejects_whitespace() {
        assert!(validate_repo_url("https://github.com/owner/repo with spaces").is_err());
    }

    #[test]
    fn test_rejects_oversized_url() {
        let huge = format!("https://github.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_repo_url(&huge).is_err());
    }

    #[test]
    fn test_accepts_url_with_port() {
        assert!(validate_repo_url("https://git.example.com:8443/owner/repo").is_ok());
    }

    // --- SSRF bypass regression tests --------------------------------------
    // Each of these encodes a loopback/private address in a form real HTTP/
    // git clients still resolve, but that a naive "4 decimal octets" check
    // would miss. All must be rejected.

    #[test]
    fn test_rejects_decimal_integer_ipv4() {
        // 2130706433 == 127.0.0.1 as a big-endian u32. — pii-test-fixture
        assert!(validate_repo_url("http://2130706433/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_hex_ipv4() {
        // 0x7f000001 == 127.0.0.1.
        assert!(validate_repo_url("http://0x7f000001/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_octal_leading_zero_ipv4() {
        // "0177" is ambiguous: some resolvers (e.g. glibc inet_aton) read a
        // leading-zero octet as octal, so "0177.0.0.1" can resolve to
        // 127.0.0.1 even though a naive decimal parse reads it as 177.
        assert!(validate_repo_url("http://0177.0.0.1/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_shorthand_dotted_quad() {
        // "127.1" is shorthand for 127.0.0.1 in many resolvers.
        assert!(validate_repo_url("http://127.1/owner/repo").is_err());
        assert!(validate_repo_url("http://192.168.1/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_ipv4_mapped_ipv6() {
        assert!(validate_repo_url("https://[::ffff:127.0.0.1]/owner/repo").is_err());
        assert!(validate_repo_url("https://[::ffff:<internal-ip>]/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_bare_zero_host() {
        // "0" resolves to 0.0.0.0 on most stacks.
        assert!(validate_repo_url("http://0/owner/repo").is_err());
    }

    #[test]
    fn test_still_accepts_ordinary_hostnames_after_numeric_hardening() {
        // Regression guard: the stricter numeric-host handling must not
        // start rejecting normal-looking public hostnames.
        assert!(validate_repo_url("https://github.com/owner/repo").is_ok());
        assert!(validate_repo_url("https://git.example.com:8443/owner/repo").is_ok());
        assert!(validate_repo_url("https://1a2b.example.com/owner/repo").is_ok());
    }

    // ── CXEG-11: clone -> transient-graph -> report pipeline ───────────────
    //
    // These tests clone LOCAL git fixture repos (created with `git init` in a
    // temp dir, never a network URL) so they're hermetic and don't depend on
    // external network access or `validate_repo_url`'s http(s)-only rule
    // (that gate is enforced one layer up, in `CortexAudit::execute` /
    // `mod.rs`'s tests -- `ScratchClone`/`run_audit` here take a raw clone
    // source, matching `cortex_scope`'s split between `mod.rs` validation and
    // `scope.rs`'s pure-ish derivation).

    fn cfg() -> CortexConfig {
        CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: false,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: crate::cortex::house_style::DEFAULT_EXEMPLARS_K,
            risk_weight_centrality_spike: 2.0,
            risk_weight_complexity_spike: 1.5,
            risk_weight_fan_out_explosion: 1.5,
            risk_weight_community_boundary_crossing: 2.5,
            risk_weight_semantic_duplication: 10.0,
            risk_weight_recurrence: 1.0,
            risk_band_elevated_cut: 4.0,
            audit_clone_timeout_secs: 30,
            audit_max_clone_bytes: 200_000_000,
            crystallize_min_recurrence: crate::cortex::crystallize::DEFAULT_MIN_RECURRENCE,
            escalation_enabled: true,
            escalation_add_provider: "agy".to_string(),
        }
    }

    /// Build a tiny local git repo fixture (no network) with the given
    /// `(repo_relative_path, content)` files, `git init`+commit it, and
    /// return its path -- usable directly as a `git clone` source.
    fn make_local_git_repo(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cortex-audit-fixture-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, content).unwrap();
        }
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .expect("git must be available to run CXEG-11's fixture-backed tests");
            assert!(status.success(), "git {args:?} failed setting up the fixture repo");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "<email>"]); // pii-test-fixture
        git(&["config", "user.name", "cortex-audit-test"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "fixture"]);
        dir
    }

    /// Count this PROCESS's own leaked `ScratchClone` dirs under the shared
    /// temp dir. Scoped to `std::process::id()` (the `slug` in
    /// [`ScratchClone::create`] is `"{pid}-{nanos}"`) rather than matching the
    /// bare `"terminus-cortex-audit-"` prefix — the compiler test-gate can run
    /// this same crate's tests from multiple concurrent worktrees/builds
    /// against a SHARED `/tmp`, and an unscoped prefix match would count a
    /// concurrent, unrelated process's in-flight scratch dirs as if they were
    /// this test's own leak, making the before/after equality assertion below
    /// flaky under exactly that (real, observed) shared-build-host condition.
    fn count_scratch_dirs() -> usize {
        let prefix = format!("terminus-cortex-audit-{}-", std::process::id());
        fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .count()
            })
            .unwrap_or(0)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scratch_clone_cleans_up_on_success() {
        let fixture = make_local_git_repo(
            "success",
            &[("src/lib.rs", "pub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n")],
        );
        let scratch_dir;
        {
            let scratch = ScratchClone::create(fixture.to_str().unwrap(), 30)
                .await
                .expect("cloning a local fixture repo should succeed");
            scratch_dir = scratch.dir.clone();
            assert!(scratch.repo_path().join("src/lib.rs").exists(), "cloned file present while scratch alive");
        }
        assert!(!scratch_dir.exists(), "scratch dir removed once ScratchClone drops");
        let _ = fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scratch_clone_cleans_up_on_clone_failure() {
        let before = count_scratch_dirs();
        let bogus = std::env::temp_dir().join(format!("cortex-audit-bogus-source-{}", std::process::id()));
        let err = ScratchClone::create(bogus.to_str().unwrap(), 30).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert_eq!(count_scratch_dirs(), before, "a failed clone must not leak its scratch dir");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scratch_clone_cleans_up_on_panic() {
        let fixture = make_local_git_repo("panic", &[("src/lib.rs", "pub fn f() {}\n")]);
        let before = count_scratch_dirs();
        let fixture_str = fixture.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let _scratch = ScratchClone::create(&fixture_str, 30)
                .await
                .expect("cloning a local fixture repo should succeed");
            panic!("simulated failure after a successful clone");
        });
        let joined = handle.await;
        assert!(joined.is_err(), "the spawned task should have panicked");
        assert_eq!(
            count_scratch_dirs(),
            before,
            "Drop still runs during unwind -- a panic mid-audit must not leak the scratch dir"
        );
        let _ = fs::remove_dir_all(&fixture);
    }

    #[test]
    fn test_dir_size_sums_bytes_and_does_not_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("cortex-audit-dirsize-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "12345").unwrap(); // 5 bytes
        fs::write(dir.join("sub/b.txt"), "1234567890").unwrap(); // 10 bytes

        #[cfg(unix)]
        {
            let outside = std::env::temp_dir().join(format!("cortex-audit-dirsize-outside-{}", std::process::id()));
            fs::write(&outside, "this content must never be counted via a symlink").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link.txt")).unwrap();
            assert_eq!(dir_size(&dir), 15, "symlinked file must not be followed/counted");
            let _ = fs::remove_file(&outside);
        }
        #[cfg(not(unix))]
        {
            assert_eq!(dir_size(&dir), 15);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_run_audit_builds_report_from_local_clone() {
        let fixture = make_local_git_repo(
            "runaudit",
            &[("src/lib.rs", "pub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n")],
        );
        let config = cfg();
        let report = run_audit(fixture.to_str().unwrap(), &config)
            .await
            .expect("audit of a small local repo should succeed");
        assert_eq!(report["status"], "complete");
        assert_eq!(report["url"], fixture.to_str().unwrap());
        assert!(report["stats"]["nodes"].as_u64().unwrap() >= 2, "helper + caller functions at least: {report}");
        assert!(report["stats"]["files_scanned"].as_u64().unwrap() >= 1);
        assert!(report["signals"].is_array());
        assert!(fixture.exists(), "the ORIGINAL fixture repo is untouched by the audit (only the clone is scratch)");
        let _ = fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    async fn test_run_audit_rejects_clone_over_size_ceiling() {
        let fixture = make_local_git_repo("sizecap", &[("src/lib.rs", "pub fn f() {}\n")]);
        let mut config = cfg();
        config.audit_max_clone_bytes = 1; // any real clone (even just `.git/`) exceeds this
        let err = run_audit(fixture.to_str().unwrap(), &config).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        let _ = fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    async fn test_run_audit_rejects_repo_with_no_supported_source_files() {
        let fixture = make_local_git_repo("nosource", &[("README.md", "just docs, no source files")]);
        let config = cfg();
        let err = run_audit(fixture.to_str().unwrap(), &config).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        let _ = fs::remove_dir_all(&fixture);
    }
}
