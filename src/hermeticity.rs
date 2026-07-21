//! PCON-08: test-hermeticity source-scan guard.
//!
//! A structural, source-scanning regression guard — modeled on the existing
//! `no_pii_in_own_source_tree` self-check (`src/github/pii.rs`) and the
//! `no_direct_http_client` token scan (`src/bin/cortex_calibrate.rs`) — that
//! FAILS when a `#[test]` depends on ambient shared state WITHOUT forcing it.
//! It encodes the concrete flakes fixed this session:
//!
//!   1. **env-mutation-no-serial** — a test mutating process-global environment
//!      (`std::env::set_var` / `remove_var`) on a shared, plain-uppercase-literal
//!      key (e.g. `SCCACHE_BIN`, `PLANE_PAT_*`) without a `#[serial]` attribute,
//!      so a parallel test observing that key flakes. Dynamic / per-test-unique
//!      keys (a `format!`/variable first arg) are NOT flagged — they can't
//!      collide across tests.
//!   2. **secret-env-read-no-serial** — a test READING a secret-shaped env key
//!      (per [`crate::compiler::scope::is_secret_env_key`]) without `#[serial]`;
//!      its result depends on an ambient secret being present/absent.
//!   3. **git-unforced-config** — a test spawning `git` without forcing the
//!      config that makes a git op hermetic (`protocol.file.allow=always`, a
//!      test identity, `commit.gpgsign=false`) anywhere in its body.
//!   4. **hardcoded-tmp** — a write to a hardcoded `/tmp/...` path instead of a
//!      unique per-invocation dir (`TempDir` / `std::env::temp_dir().join(...)`),
//!      so two invocations clobber each other.
//!
//! A `// hermeticity-allow: <reason>` line marker suppresses a finding on that
//! line (mirrors the `// pii-test-fixture` convention).
//!
//! ## Green-on-today's-tree + regression ratchet
//!
//! The tree predates this guard and carries a known set of unforced patterns
//! (159 env mutations, 56 `/tmp` writes, etc.). Rather than churn every call
//! site in one commit, the tree self-check enforces a **per-file baseline
//! ratchet** ([`BASELINE`]): a file may carry up to its baselined count, and a
//! file with NO baseline entry must be clean. So the guard is GREEN today, yet
//! any NEW violation — a new file, or a count increase in an existing file —
//! fails loudly. The negative-fixture tests prove the detector itself flags each
//! rule independently of the baseline (they scan synthetic temp trees).
//!
//! This whole module is `#[cfg(test)]` — pure test infrastructure, never shipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::compiler::scope::is_secret_env_key;

/// One unforced-ambient-state finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HermeticityFinding {
    /// Path relative to the scanned root (forward-slash), e.g. `compiler/mod.rs`.
    pub file: String,
    /// 1-based line number of the offending line.
    pub line: usize,
    /// The rule that fired.
    pub rule: &'static str,
    /// The trimmed offending source line (truncated).
    pub snippet: String,
}

/// Recursively collect `.rs` file paths under `dir`, skipping any `target/`
/// build-output directory. (Same walker shape as the PII self-check.)
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Whether an attribute-ish line declares a test (`#[test]`, `#[tokio::test]`,
/// `#[actix_web::test]`, `#[serial]`+`#[test]`, …). Deliberately does NOT match
/// `#[cfg(test)]` (which compacts to `...test)]`, not `...test]`).
fn is_test_attr(trimmed: &str) -> bool {
    if !trimmed.starts_with("#[") {
        return false;
    }
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("test]")
}

/// Extract the first string-literal argument of a `name(` call starting at
/// `after` (byte offset just past the `(`), if the first non-space token is a
/// `"..."` literal. Returns the literal's inner content. Only simple literals
/// (no escapes needed for our env-key case) are recognized; anything else
/// (a variable, a `format!`, a method call) yields `None` — which is exactly
/// how we treat per-test-unique / dynamic keys as safe.
fn first_str_literal_arg(line: &str, after: usize) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = after;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < bytes.len() && bytes[i] != b'"' {
        // A backslash means this isn't the plain-identifier literal we key on.
        if bytes[i] == b'\\' {
            return None;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    Some(line[start..i].to_string())
}

/// Whether `s` is a plain env-var-name-shaped literal (`[A-Za-z0-9_]+`), i.e. a
/// SHARED key rather than a dynamic per-test key.
fn is_plain_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Find the argument offset just past the first occurrence of `needle` (a
/// `foo::bar(` call opener) in `line`.
fn call_arg_offset(line: &str, needle: &str) -> Option<usize> {
    line.find(needle).map(|p| p + needle.len())
}

/// Scan a single already-read file's lines for unforced-ambient-state findings.
/// `rel` is the path reported in findings.
fn scan_lines(rel: &str, lines: &[&str]) -> Vec<HermeticityFinding> {
    let n = lines.len();
    let mut findings = Vec::new();
    let mut i = 0usize;
    while i < n {
        let trimmed = lines[i].trim_start();
        if is_test_attr(trimmed) {
            // Determine `#[serial]` by scanning the contiguous attribute cluster
            // above (attrs/comments/blanks) and from here down to the `fn` line.
            let mut serial = false;
            let mut up = i as isize - 1;
            while up >= 0 {
                let t = lines[up as usize].trim_start();
                if t.starts_with("#[") || t.starts_with("//") || t.is_empty() {
                    if t.contains("serial") {
                        serial = true;
                    }
                    up -= 1;
                } else {
                    break;
                }
            }
            let mut k = i;
            while k < n && !lines[k].contains("fn ") {
                if lines[k].contains("serial") {
                    serial = true;
                }
                k += 1;
            }
            if k >= n {
                i += 1;
                continue;
            }
            let fn_line = k;
            // Brace-match the body from the fn line.
            let mut depth: i32 = 0;
            let mut started = false;
            let mut end = fn_line;
            for m in fn_line..n {
                depth += lines[m].matches('{').count() as i32;
                depth -= lines[m].matches('}').count() as i32;
                if lines[m].contains('{') {
                    started = true;
                }
                if started && depth <= 0 {
                    end = m;
                    break;
                }
            }
            if !started {
                i = fn_line + 1;
                continue;
            }

            // Body-wide "forced git config" check.
            let body_forces_git = (fn_line..=end).any(|m| {
                let l = lines[m];
                l.contains("protocol.file.allow")
                    || l.contains("commit.gpgsign")
                    || l.contains("GIT_AUTHOR_NAME")
                    || l.contains("GIT_COMMITTER_NAME")
                    || l.contains("user.name")
            });

            for (off, l) in lines[fn_line..=end].iter().enumerate() {
                let lineno = fn_line + off + 1;
                if l.contains("hermeticity-allow") {
                    continue;
                }
                let push = |rule: &'static str, findings: &mut Vec<HermeticityFinding>| {
                    findings.push(HermeticityFinding {
                        file: rel.to_string(),
                        line: lineno,
                        rule,
                        snippet: l.trim().chars().take(100).collect(),
                    });
                };

                // Rule 1: env mutation of a shared plain-literal key, no serial.
                if !serial {
                    for opener in ["env::set_var(", "env::remove_var("] {
                        if let Some(o) = call_arg_offset(l, opener) {
                            if let Some(key) = first_str_literal_arg(l, o) {
                                if is_plain_key(&key) {
                                    push("env-mutation-no-serial", &mut findings);
                                }
                            }
                        }
                    }
                    // Rule 2: read of a secret-shaped key, no serial.
                    for opener in ["env::var(", "env::var_os("] {
                        if let Some(o) = call_arg_offset(l, opener) {
                            if let Some(key) = first_str_literal_arg(l, o) {
                                if is_plain_key(&key) && is_secret_env_key(&key) {
                                    push("secret-env-read-no-serial", &mut findings);
                                }
                            }
                        }
                    }
                }

                // Rule 3: git op without forced config anywhere in the body.
                if l.contains("Command::new(\"git\")") && !body_forces_git {
                    push("git-unforced-config", &mut findings);
                }

                // Rule 4: hardcoded /tmp path.
                if l.contains("\"/tmp/") {
                    push("hardcoded-tmp", &mut findings);
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    findings
}

/// Walk `root` (a crate `src/` dir) and return every unforced-ambient-state
/// finding, with `file` paths relative to `root`.
pub fn scan_hermeticity(root: &Path) -> Vec<HermeticityFinding> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut all = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();
        all.extend(scan_lines(&rel, &lines));
    }
    all.sort_by(|a, b| (a.file.as_str(), a.line).cmp(&(b.file.as_str(), b.line)));
    all
}

/// Per-file baseline of KNOWN pre-existing findings (path relative to `src/`,
/// count). A file may carry up to this many findings; a file absent here must
/// be clean. Any increase, or a violation in a new file, fails the ratchet.
///
/// This is grandfathered technical debt from before PCON-08 — the guard's job
/// is to stop it GROWING and force new code to be hermetic. Generated from the
/// live tree (run the self-check with `HERMETICITY_DUMP=1` to regenerate).
#[cfg(test)]
const BASELINE: &[(&str, usize)] = &[
    // == GENERATED BASELINE (regenerate with HERMETICITY_DUMP=1) — 229 findings ==
    ("bin/mint.rs", 18),
    ("broker/control.rs", 3),
    ("compiler/deploy.rs", 4),
    ("compiler/mod.rs", 3),
    ("compiler/publish.rs", 6),
    ("compiler/scheduler.rs", 27),
    ("compiler/scope.rs", 3),
    ("config.rs", 10),
    ("crucible/mod.rs", 2),
    ("dev/mod.rs", 1),
    ("dura/mod.rs", 1),
    ("forge/gitea_family.rs", 2),
    ("forge/mirror/history.rs", 7),
    ("forge/mirror/tools.rs", 1),
    ("gateway/mod.rs", 3),
    ("gateway_framework/mod.rs", 14),
    ("gitea/merge_queue.rs", 2),
    ("gitea/mod.rs", 2),
    ("house_style/mod.rs", 9),
    ("intake/assistant/acquire.rs", 2),
    ("intake/code.rs", 4),
    ("intake/code_v2.rs", 8),
    ("intake/coder_sweep.rs", 7),
    ("intake/context.rs", 2),
    ("intake/discovery/hf_client.rs", 4),
    ("intake/gpu_authority.rs", 10),
    ("intake/infer.rs", 2),
    ("intake/timeouts.rs", 9),
    ("mesh/client.rs", 2),
    ("mesh/principal.rs", 1),
    ("mint/idle.rs", 14),
    ("network/mod.rs", 1),
    ("odyssey/mod.rs", 1),
    ("plane/prefix.rs", 3),
    ("reminder/mod.rs", 3),
    ("scribe/inspect.rs", 6),
    ("scribe/mod.rs", 2),
    ("scribe/vault.rs", 1),
    ("sentinel/mod.rs", 3),
    ("skills/mod.rs", 1),
    ("sysversion/mod.rs", 16),
    ("tools/docgen/drift.rs", 3),
    ("tools/docgen/generate.rs", 4),
    ("vigil/mod.rs", 2),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fresh unique temp dir for a synthetic fixture tree.
    fn temp_tree(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "pcon08-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ── Fixture builders ─────────────────────────────────────────────────────
    // These assemble Rust-source fixture CONTENT from parts so that the trigger
    // tokens (`env::set_var(`, `Command::new("git")`, `"/tmp/`) never appear
    // contiguously in THIS module's own source — otherwise the real-tree
    // self-check would flag the fixtures. The written temp files (under
    // temp_dir, not src/) do contain the real tokens.  // hermeticity-allow: guard fixtures

    fn env_mutate(fname: &str, key: &str) -> String {
        // `env::{}(` in source ≠ `env::set_var(`; the built string has the token.
        format!(
            "#[test]\nfn {fname}() {{\n    std::env::{}({key:?}, \"1\");\n}}\n",
            "set_var"
        )
    }
    fn env_mutate_serial(fname: &str, key: &str) -> String {
        format!(
            "#[test]\n#[serial]\nfn {fname}() {{\n    std::env::{}({key:?}, \"1\");\n}}\n",
            "set_var"
        )
    }
    fn env_read(fname: &str, key: &str) -> String {
        format!(
            "#[test]\nfn {fname}() {{\n    let _ = std::env::{}({key:?});\n}}\n",
            "var"
        )
    }
    fn git_call(fname: &str, forced: bool) -> String {
        let cfg = if forced {
            "    let _c = format!(\"protocol.file.allow=always\");\n"
        } else {
            ""
        };
        // `Command::new({:?})` in source ≠ `Command::new("git")`.
        format!(
            "#[test]\nfn {fname}() {{\n{cfg}    let _ = std::process::Command::new({:?});\n}}\n",
            "git"
        )
    }
    fn tmp_write(fname: &str) -> String {
        // Build the `/tmp/...` literal so `"/tmp/` is not contiguous in source.
        let path = format!("\"/{}\"", "tmp/pcon08-fixture");
        format!("#[test]\nfn {fname}() {{\n    let _p = {path};\n}}\n")
    }
    fn tempdir_write(fname: &str) -> String {
        format!(
            "#[test]\nfn {fname}() {{\n    let _p = std::env::temp_dir().join(\"x\");\n}}\n"
        )
    }

    #[test]
    fn flags_env_mutation_without_serial() {
        let dir = temp_tree("env-mut");
        write(&dir, "a.rs", &env_mutate("t", "SCCACHE_BIN"));
        let f = scan_hermeticity(&dir);
        assert!(
            f.iter().any(|x| x.rule == "env-mutation-no-serial"),
            "{f:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn serial_env_mutation_is_clean() {
        let dir = temp_tree("env-mut-serial");
        write(&dir, "a.rs", &env_mutate_serial("t", "SCCACHE_BIN"));
        let f = scan_hermeticity(&dir);
        assert!(f.is_empty(), "serialized mutation should be clean: {f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dynamic_key_mutation_is_not_flagged() {
        // A per-test-unique key (built via a variable / format!) is safe.
        let dir = temp_tree("env-mut-dyn");
        let body = format!(
            "#[test]\nfn t() {{\n    let k = format!(\"K_{{}}\", 1);\n    std::env::{}(&k, \"1\");\n}}\n",
            "set_var"
        );
        write(&dir, "a.rs", &body);
        let f = scan_hermeticity(&dir);
        assert!(
            !f.iter().any(|x| x.rule == "env-mutation-no-serial"),
            "dynamic key must not be flagged: {f:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_secret_env_read_without_serial() {
        let dir = temp_tree("secret-read");
        write(&dir, "a.rs", &env_read("t", "GITEA_TOKEN"));
        let f = scan_hermeticity(&dir);
        assert!(
            f.iter().any(|x| x.rule == "secret-env-read-no-serial"),
            "{f:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nonsecret_env_read_is_not_flagged() {
        let dir = temp_tree("nonsecret-read");
        write(&dir, "a.rs", &env_read("t", "BUILD_FLEET_QUIET"));
        let f = scan_hermeticity(&dir);
        assert!(f.is_empty(), "non-secret read should be clean: {f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_git_without_forced_config() {
        let dir = temp_tree("git-unforced");
        write(&dir, "a.rs", &git_call("t", false));
        let f = scan_hermeticity(&dir);
        assert!(f.iter().any(|x| x.rule == "git-unforced-config"), "{f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_with_forced_config_is_clean() {
        let dir = temp_tree("git-forced");
        write(&dir, "a.rs", &git_call("t", true));
        let f = scan_hermeticity(&dir);
        assert!(
            !f.iter().any(|x| x.rule == "git-unforced-config"),
            "forced-config git should be clean: {f:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_hardcoded_tmp() {
        let dir = temp_tree("tmp");
        write(&dir, "a.rs", &tmp_write("t"));
        let f = scan_hermeticity(&dir);
        assert!(f.iter().any(|x| x.rule == "hardcoded-tmp"), "{f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tempdir_write_is_not_flagged() {
        let dir = temp_tree("tempdir");
        write(&dir, "a.rs", &tempdir_write("t"));
        let f = scan_hermeticity(&dir);
        assert!(
            !f.iter().any(|x| x.rule == "hardcoded-tmp"),
            "temp_dir().join should be clean: {f:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allow_marker_suppresses_finding() {
        let dir = temp_tree("allow");
        // Same violation, but the offending line carries the marker.
        let body = format!(
            "#[test]\nfn t() {{\n    std::env::{}(\"SCCACHE_BIN\", \"1\"); // hermeticity-allow: deliberate\n}}\n",
            "set_var"
        );
        write(&dir, "a.rs", &body);
        let f = scan_hermeticity(&dir);
        assert!(f.is_empty(), "marker should suppress: {f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_test_code_is_ignored() {
        // A production fn (not `#[test]`) mutating env is out of scope.
        let dir = temp_tree("nontest");
        let body = format!(
            "pub fn setup() {{\n    std::env::{}(\"SCCACHE_BIN\", \"1\");\n}}\n",
            "set_var"
        );
        write(&dir, "a.rs", &body);
        let f = scan_hermeticity(&dir);
        assert!(f.is_empty(), "non-test code must be ignored: {f:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Root-cause regression ratchet: scan this crate's own `src/` and require
    /// no file EXCEEDS its [`BASELINE`] count, and no un-baselined file has any
    /// finding. Green today; any new unforced ambient-state dependency fails.
    ///
    /// Run with `HERMETICITY_DUMP=1` to print the current per-file counts (to
    /// regenerate `BASELINE`) instead of asserting.
    #[test]
    fn no_new_unforced_ambient_state_in_own_source_tree() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(src.is_dir(), "expected {src:?} to exist");

        let findings = scan_hermeticity(&src);

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.file.clone()).or_default() += 1;
        }

        if std::env::var("HERMETICITY_DUMP").is_ok() {
            // Regeneration mode: print a paste-ready BASELINE table.
            println!("// == GENERATED BASELINE ==");
            for (file, n) in &counts {
                println!("    (\"{file}\", {n}),");
            }
            println!("// total findings: {}", findings.len());
            return;
        }

        let baseline: BTreeMap<&str, usize> = BASELINE.iter().copied().collect();

        let mut regressions: Vec<String> = Vec::new();
        for (file, n) in &counts {
            let allowed = baseline.get(file.as_str()).copied().unwrap_or(0);
            if *n > allowed {
                let detail: Vec<String> = findings
                    .iter()
                    .filter(|f| &f.file == file)
                    .map(|f| format!("      {}:{}: {} — {}", f.file, f.line, f.rule, f.snippet))
                    .collect();
                regressions.push(format!(
                    "  {file}: {n} finding(s), baseline allows {allowed}:\n{}",
                    detail.join("\n")
                ));
            }
        }

        assert!(
            regressions.is_empty(),
            "PCON-08 hermeticity guard: {} file(s) exceed their baseline — a test now \
             depends on ambient shared state without forcing it. Make the test hermetic \
             (add `#[serial]` for shared-env mutation, force git config, use a unique \
             temp dir) or tag the line `// hermeticity-allow: <reason>`:\n{}",
            regressions.len(),
            regressions.join("\n")
        );
    }
}
