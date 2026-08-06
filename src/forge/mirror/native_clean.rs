//! GHMRFIX-5 — native, in-process deterministic residual cleaner.
//!
//! This is the DEFAULT [`ResidualCleaner`] the mirror engine dispatches when the
//! sweep leaves residual PII: a first-party Rust port of the external
//! `mirror-clean.py` that used to be wired through `TERMINUS_MIRROR_CLEAN_CMD`.
//! Running it in-process removes the external-process dependency, brings the
//! scrub logic under the Rust test suite, and makes the mirror self-contained.
//! ([`super::clean::CommandCleaner`] is kept as an operator OVERRIDE — when
//! `TERMINUS_MIRROR_CLEAN_CMD` is set it takes precedence, see
//! [`super::clean::dispatch_cleaning`].)
//!
//! ## What it scrubs — and the two invariants that keep it safe
//! It walks the whole work-dir tree (the throwaway publish copy — NEVER the
//! source) and rewrites, in place, PII the mechanical sweep left behind. It was
//! hardened against two real failure modes found during the first full public
//! mirror catch-up (2026-07-11):
//!
//! 1. **Whole-tree scrub is limited to unambiguous fleet IDENTIFIERS** — private
//!    IPs, container IDs, internal hostnames, internal domains, infra service
//!    names, the operator's email/handle, internal-cue UUIDs, internal paths.
//!    These are single-line and never alter code STRUCTURE (an IP → `<internal-ip>`
//!    cannot unbalance a quote or collapse a statement). Because the operator
//!    forbids ANY internal IP/hostname in public **even inside a
//!    `pii-test-fixture`-tagged line**, these identifier rules deliberately ignore
//!    the exemption tag and loosen the boundaries so ABUTTED forms scrub too
//!    (`<host>ssh` from a newline-collapsed backlog dump; `\x02<internal-ip>` in a  // pii-test-fixture
//!    binary-blob test fixture; `multimodal_pvf1` as a preset name).  // pii-test-fixture
//!
//!    **These identifier patterns are NOT defined here.** They are built from
//!    the shared constructors in [`crate::github::pii`] — the same ones the gate
//!    detector uses. TERM #661: this file and the gate each used to carry their
//!    own copy, both anchored with a leading `\b`, and `_` is a word character,
//!    so neither matched `multimodal_pvf1` and the internal GPU hostname went  // pii-test-fixture
//!    live on the public mirror. A rule that has to agree in two places will
//!    eventually not; it now has one home. Note the boundary loosening is
//!    asymmetric on purpose: the leading side treats `_` as a separator, the
//!    TRAILING `\b` is kept (it is what stops `<host>` matching inside `pvenv` and  // pii-test-fixture
//!    `<secret-manager>` inside `INFISICAL_CLIENT_ID`).  // pii-test-fixture
//!
//! 2. **Secret-shaped tokens are TOKEN-BOUNDED and SINGLE-LINE.** Every secret
//!    pattern's tail is a bounded token class (`[A-Za-z0-9_\-]{N,}`, never `\S+`)
//!    and secret-field values are `[^"\n]` (never `[^"]`). This is load-bearing:
//!    an unbounded `\S+` api-key pattern once ATE THE CLOSING QUOTE of the fleet's
//!    own redaction regexes (`Regex::new(r"\b(sk-|ghp_|…)")`) and an `[^"]` field
//!    pattern collapsed multi-line strings — corrupting a published mirror into
//!    non-compiling Rust. A bounded pattern leaves a detection regex intact (its
//!    prefix is followed by `|`/`)`/`[`, i.e. no token body → no match) while
//!    still scrubbing a genuine leaked key (which carries a long token body).
//!
//! Anything the gate flags that these rules do NOT cover (a novel secret shape, a
//! config-`generic_secret` the cleaner can't disambiguate) simply fails to drive
//! the gate to 0 and is ESCALATED to the operator by the bounded loop in
//! [`super::clean::run_cleaning_pass`] — the cleaner can never smuggle residual
//! PII into an approved tag, and it never guesses at ambiguous content.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::error::ToolError;
use crate::github::pii::{self, BoundedPattern, TreeViolation};

use super::clean::ResidualCleaner;
use super::sweep::read_text;

/// Directories never descended: VCS metadata, build output, vendored deps, and
/// nested worktrees. Matches the retired python cleaner's `SKIP_DIRS`.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".cargo", "worktrees"];

/// Line-scoped cue words: a UUID is only scrubbed on a line that also carries one
/// of these (mirrors the gate's `uuid_is_sensitive`), so a bare UUID in ordinary
/// test data is left intact.
const UUID_CUES: &[&str] = &["<secret-manager>", "project_id", "workspace_id", "machine_identity"];  // pii-test-fixture

struct CleanPatterns {
    /// Token-bounded secret shapes → `<REDACTED-SECRET>` (single-line).
    secret: Regex,
    /// `field: "value"` where the field name is secret/token/password/key-shaped,
    /// value single-line → `field: "<REDACTED-SECRET>"`. Named group `field`.
    secret_field: Regex,
    /// Ordered fleet-identifier substitutions (pattern, replacement).
    ///
    /// TERM #661: the identifier patterns here are NOT written out locally —
    /// they are built from the shared constructors in [`crate::github::pii`],
    /// the same ones the gate detector uses. Two hand-maintained copies of
    /// these regexes drifted once and published an internal hostname; a rule
    /// that must agree in two places will eventually not.
    literal: Vec<(BoundedPattern, &'static str)>,
    /// A canonical v4-shaped UUID (scrubbed only on cue lines).
    uuid: Regex,
}

fn patterns() -> &'static CleanPatterns {
    static P: OnceLock<CleanPatterns> = OnceLock::new();
    P.get_or_init(|| {
        // Secret token shapes. Each alternative is TOKEN-BOUNDED + single-line —
        // see the module doc for why `\S+` is banned here. Prefix-keys require a
        // real >=10-char body (matching the gate's `api_key` detector), so a bare
        // prefix in detection/doc/test code never matches.
        let secret_alts = [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            // JWT: header + 1-2 dot-separated segments (short example payloads too).
            r"eyJ[A-Za-z0-9_\-]{6,}(?:\.[A-Za-z0-9_\-]+){1,2}",
            // The gate's api_key prefixes, bounded body (>=10). `\b`-anchored so a
            // bare prefix in a detection alternation (`sk-|ghp_`) never matches.
            r"\b(?:sk-|ghp_|gsk_|glpat-|xox[bpasr]-)[A-Za-z0-9_\-]{10,}",
            // GitHub server/OAuth/refresh/user + fine-grained PAT prefixes. `\b`
            // leading and a >=10-char body: `sk-ant-`/`gsk_` are already covered by
            // the alt above, and a generic prefix like `plane_api_` is deliberately
            // NOT listed — it would over-redact identifiers (`plane_api_base_url`).
            // A real GitHub token carries a long base62 body; a short lowercase
            // identifier (`github_pat_lumina`, 6 chars) does not reach {10,}.
            r"\b(?:github_pat_|gho_|ghs_|ghr_|ghu_)[A-Za-z0-9_]{10,}",
            r"AKIA[0-9A-Z]{16}",
            r"AIza[0-9A-Za-z_\-]{30,}",
        ];
        let secret = Regex::new(&secret_alts.map(|a| format!("(?:{a})")).join("|"))
            .expect("mirror clean secret regex");

        // `[ \t]` (NOT `\s`, which includes `\n`) around the assignment keeps the
        // whole match on ONE line, so it can never reach across a newline into the
        // following statement — the value class is `[^"\n]` for the same reason.
        let secret_field = Regex::new(
            r#"(?i)(?P<field>[a-z0-9_]*(?:secret|token|password|api[_-]?key)[a-z0-9_]*[ \t]*[:=][ \t]*)"[^"\n]{8,}""#,
        )
        .expect("mirror clean secret_field regex");

        // A cleaner-only rule: no leading-boundary condition, so it behaves
        // exactly like a plain `Regex::replace_all`.
        let plain = |src: &str| BoundedPattern::unbounded(src);

        let mut literal: Vec<(BoundedPattern, &'static str)> = Vec::new();
        // THE shared fleet-identifier rule set — private IPs, container ids,
        // internal hostnames, infra services, the operator's email/handle. The
        // patterns, their anchoring AND the list membership all live in
        // `crate::github::pii`, so the cleaner and the gate detector cannot
        // disagree about which spans are fleet identifiers (TERM #661).
        literal.extend(
            pii::fleet_identifier_rules()
                .into_iter()
                .map(|r| (r.pattern, r.placeholder)),
        );
        // Cleaner-only rules from here down: these have no gate counterpart to
        // drift against, and carry their own anchors in the pattern source.
        literal.extend([
            // internal_domain
            (plain(r"moosenet\.online|moosenet\.local"), "example.com"),
            // internal_path
            (
                plain(r"<path>/|<path>/|<path>/|/opt/lumina[a-z0-9-]*/"),  // pii-test-fixture
                "<path>/",
            ),
        ]);
        literal.extend([
            // generic email — last
            (plain(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"), "<email>"),
            // Canonical phone numbers (GHMRFIX-6) — the SAME shapes the gate's
            // `phone` detector flags (E.164, or grouped 3-3-4 NANP), so a phone the
            // gate would withhold on gets scrubbed by the mirror instead. Found in
            // history by the GHIST full-history gate: a PII-sanitizer's own fixtures
            // (`"phone": "<phone>"`) tripped it. The strict shapes (canonical  // pii-test-fixture
            // only, matching GHMRFIX-4) keep this from mangling dates/versions/math.
            (
                plain(r"(?:\+\d[\d \-]{5,13}\d)|(?:\b\(?\d{3}\)?[ \-]\d{3}[ \-]\d{4}\b)"),
                "<phone>",
            ),
        ]);

        let uuid =
            Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();

        CleanPatterns { secret, secret_field, literal, uuid }
    })
}

/// The native default residual cleaner (GHMRFIX-5). Stateless: all patterns are
/// process-global. Construct with [`DeterministicCleaner::new`].
#[derive(Default)]
pub struct DeterministicCleaner;

impl DeterministicCleaner {
    pub fn new() -> Self {
        Self
    }

    /// Deterministically scrub one text blob. Every rule is single-line-safe (no
    /// pattern matches across `\n`), so applying them to the whole blob cannot
    /// span a line boundary or collapse code. Returns the possibly-rewritten text.
    pub(crate) fn scrub_text(text: &str) -> String {
        let p = patterns();
        // Order matches the retired python cleaner: secret tokens → secret fields
        // → fleet identifiers → cue-scoped UUIDs.
        let mut out = p.secret.replace_all(text, "<REDACTED-SECRET>").into_owned();
        out = p
            .secret_field
            .replace_all(&out, r#"${field}"<REDACTED-SECRET>""#)
            .into_owned();
        for (re, repl) in &p.literal {
            // `BoundedPattern::replace_all` substitutes the replacement
            // LITERALLY (no `$group` expansion) and applies the shared
            // leading-boundary rule, so the cleaner and the gate agree on
            // exactly which spans are fleet identifiers.
            out = re.replace_all(&out, repl);
        }
        // UUIDs: only on a line that also carries an infra-secret cue. Skip the
        // per-line pass entirely when no UUID is present (the common case).
        // `split('\n')` + `join("\n")` round-trips the newline shape exactly (a
        // trailing `\n` yields a final "" element that `join` restores), so the
        // line count is preserved regardless of trailing newline.
        if p.uuid.is_match(&out) {
            out = out
                .split('\n')
                .map(|line| {
                    let low = line.to_ascii_lowercase();
                    if UUID_CUES.iter().any(|c| low.contains(c)) {
                        p.uuid.replace_all(line, "<uuid>").into_owned()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        out
    }

    /// Scrub a raw blob (for the GHIST full-history replay, which rewrites a
    /// `git fast-export` byte stream). A blob that is a valid-UTF-8 text file and
    /// not oversized and NUL-free is scrubbed via [`Self::scrub_text`]; a binary,
    /// oversized, or non-UTF-8 blob is returned BYTE-IDENTICAL. These skip rules
    /// mirror [`read_text`] exactly, so replaying history never corrupts a binary
    /// asset and never alters a blob's line count (the corruption invariant that
    /// `scrub_text` already guarantees for text).
    pub fn scrub_bytes(bytes: &[u8]) -> Vec<u8> {
        // 5 MiB cap matches `read_text`/the sweep's MAX_FILE_BYTES.
        const MAX_BLOB_BYTES: usize = 5 * 1024 * 1024;
        if bytes.len() > MAX_BLOB_BYTES || bytes.contains(&0) {
            return bytes.to_vec();
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::scrub_text(text).into_bytes(),
            Err(_) => bytes.to_vec(),
        }
    }

    /// Walk `work_dir`, scrubbing every readable text file in place. Returns the
    /// number of files changed. Skips [`SKIP_DIRS`], symlinks (no traversal
    /// outside the tree), and binary/oversized/non-UTF-8 files (via [`read_text`]).
    fn scrub_tree(work_dir: &Path) -> usize {
        fn walk(dir: &Path, changed: &mut usize) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if ft.is_dir() {
                    let skip = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| SKIP_DIRS.contains(&n))
                        .unwrap_or(false);
                    if !skip {
                        walk(&path, changed);
                    }
                } else if ft.is_file() {
                    let Some(text) = read_text(&path) else { continue };
                    let scrubbed = DeterministicCleaner::scrub_text(&text);
                    if scrubbed != text {
                        // Best-effort write; a write failure leaves the original
                        // content, which the gate will re-flag → escalation, never
                        // a silent pass-through.
                        if std::fs::write(&path, scrubbed).is_ok() {
                            *changed += 1;
                        }
                    }
                }
            }
        }
        let mut changed = 0;
        walk(work_dir, &mut changed);
        changed
    }
}

impl ResidualCleaner for DeterministicCleaner {
    fn clean_round(&self, work_dir: &Path, _residuals: &[TreeViolation]) -> Result<(), ToolError> {
        // Whole-tree scrub: the residual list is advisory (it drives the gate's
        // re-check), but a tagged `pii-test-fixture` line the gate EXEMPTS can
        // still hold an internal IP/host that must not ship — so we scrub the
        // whole tree unconditionally, not just the flagged spots.
        let changed = DeterministicCleaner::scrub_tree(work_dir);
        tracing::info!(
            target: "forge.mirror",
            changed,
            "native deterministic cleaner scrubbed {changed} file(s)"
        );
        Ok(())
    }

    fn label(&self) -> &str {
        "native-deterministic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrub(s: &str) -> String {
        DeterministicCleaner::scrub_text(s)
    }

    // ── the corruption regression: detection regexes / code STRUCTURE survive ──
    #[test]
    fn detection_regex_and_code_structure_are_preserved() {
        // The exact line an earlier `\S+` cleaner corrupted.
        let regex_line = r#"api_key: Regex::new(r"\b(sk-[A-Za-z0-9\-_]{10,}|ghp_[A-Za-z0-9]{10,}|glpat-[A-Za-z0-9\-_]{10,})")"#;
        assert_eq!(scrub(regex_line), regex_line, "detection regex must be untouched");

        // A doc comment listing prefixes.
        let doc = "//! - Credential patterns (sk-, ghp_, JWT tokens, API keys)";
        assert_eq!(scrub(doc), doc, "prefix mentions in prose must be untouched");

        // A multi-statement block must not collapse: line count preserved.
        let block = "let secret = \"my-jwt-secret-value\";\nlet args = json!({\"key\": secret});\n";
        let out = scrub(block);
        assert_eq!(out.matches('\n').count(), block.matches('\n').count(), "no line collapse");
        assert!(out.contains(r#"json!({"key": secret})"#), "following statement intact: {out}");
    }

    // ── secret fields + real keys ARE scrubbed (single-line, quote-balanced) ──
    #[test]
    fn secrets_are_scrubbed_cleanly() {
        assert_eq!(
            scrub("jwt_secret: \"test-secret-32-bytes-minimum!!!\".into(),"),
            "jwt_secret: \"<REDACTED-SECRET>\".into(),"
        );
        assert_eq!(
            scrub("let real = \"<REDACTED-SECRET>\";"),  // pii-test-fixture
            "let real = \"<REDACTED-SECRET>\";"
        );
        // A bare short prefix (no real body) is NOT a secret.
        assert_eq!(scrub("prefix is sk- and ghp_"), "prefix is sk- and ghp_");
        // Example JWT with short literal segments.
        assert_eq!(
            scrub("filter(\"<REDACTED-SECRET>\")"),
            "filter(\"<REDACTED-SECRET>\")"
        );
    }

    // ── fleet identifiers, incl. ABUTTED forms the gate's \b misses ──
    #[test]
    fn fleet_identifiers_including_abutted_are_scrubbed() {
        assert_eq!(
            scrub("host <host> at <internal-ip> on <host>"), // pii-test-fixture
            "host <host> at <internal-ip> on <host>"
        );
        // Abutted: <host> inside <host>ssh, IP after a `\x02` escape's digit.  // pii-test-fixture
        assert_eq!(scrub("verified on <host>ssh root@"), "verified on <host>ssh root@");  // pii-test-fixture
        assert_eq!(scrub("start\\0\\x02<internal-ip> end"), "start\\0\\x02<internal-ip> end");  // pii-test-fixture
        // internal domain + infra service + operator.
        assert_eq!(scrub("git.example.com via <secret-manager> for <operator>"),  // pii-test-fixture
                   "git.example.com via <secret-manager> for <operator>");
    }

    // ── TERM #661: the cleaner's half of the underscore hole ─────────────────
    #[test]
    fn underscore_abutted_identifiers_are_scrubbed() {
        // The exact published line. `_` is a word char, so the old leading `\b`
        // never fired and this shipped to the public mirror verbatim.
        assert_eq!(
            scrub(r#"assert_eq!(v["preset_name"], "multimodal_pvf1");"#), // pii-test-fixture
            r#"assert_eq!(v["preset_name"], "multimodal_<host>");"#
        );
        assert_eq!(scrub("host_pvf1 -> status"), "host_<host> -> status"); // pii-test-fixture
        assert_eq!(scrub("pvf1_bar and foo-<host>"), "<host>_bar and foo-<host>"); // pii-test-fixture
        // Back-to-back matches: the boundary check must not consume the
        // separator, or the second occurrence would survive.
        assert_eq!(scrub("pvf1_pvf1_pvf1"), "<host>_<host>_<host>"); // pii-test-fixture
        // Escape-abutted container id, and an underscore-abutted IP.
        assert_eq!(scrub(r#""clean\nCT327\n""#), r#""clean\n<host>\n""#); // pii-test-fixture
        assert_eq!(scrub("svc_<internal-ip>"), "svc_<internal-ip>"); // pii-test-fixture
    }

    #[test]
    fn bare_identifier_scrub_is_unchanged_control() {
        // Control for the mutation runs: the forms that ALREADY scrubbed must
        // keep scrubbing, so green can never mean "the cleaner was weakened".
        assert_eq!(scrub("host <host> here"), "host <host> here"); // pii-test-fixture
        assert_eq!(scrub("on <host> now"), "on <host> now"); // pii-test-fixture
        assert_eq!(scrub("at <internal-ip>"), "at <internal-ip>"); // pii-test-fixture
    }

    #[test]
    fn the_false_positive_guard_still_holds() {
        // A preceding/following LETTER still blocks the match. A cleaner that
        // mangles ordinary words or the fleet's own identifiers gets switched
        // off, which is worse than the hole it closes.
        for s in [
            "pvenv activate",
            "the pvenv dir",
            "pve__get_nodes",
            "pve_status probe",
            "INFISICAL_CLIENT_ID",
            "JELLYSEERR_URL=",
            "trumpeter solo",
        ] {
            assert_eq!(scrub(s), s, "{s:?} must be left alone");
        }
    }

    #[test]
    fn newly_matching_shapes_do_not_over_scrub() {
        // Review-panel round 1 (codex): an unconditional leading boundary would
        // have rewritten `OCT2024` to `O<host>` and `apvf1z` to `a<host>z`.
        // Verified real, so those rules carry `is_token_start` instead.
        for s in ["OCT2024 release", "apvf1z", "xironclawy", "SELECT327"] {
            // pii-test-fixture
            assert_eq!(scrub(s), s, "{s:?} must be left alone");
        }
        // But the escape-abutted form — the one live on the mirror — still goes.
        assert_eq!(scrub(r#""clean\nCT327""#), r#""clean\n<host>""#); // pii-test-fixture
        // `<host>` keeps its numeric suffix (opus, round 1: grouping the  // pii-test-fixture
        // `\d*` across both tokens, so `<host>` scrubs whole).  // pii-test-fixture
        assert_eq!(scrub("on <host>"), "on <host>"); // pii-test-fixture
    }

    #[test]
    fn abutted_scrubs_preserve_line_count() {
        // The mirror's corruption invariant, on the newly-matching shapes.
        let block = "let a = \"multimodal_pvf1\";\nlet b = \"host_pvf1\";\nlet c = 1;\n"; // pii-test-fixture
        let out = scrub(block);
        assert_eq!(out.matches('\n').count(), block.matches('\n').count());
        assert!(!out.contains("<host>"), "no residual hostname: {out}"); // pii-test-fixture
        assert!(out.contains("let c = 1;"), "following statement intact: {out}");
    }

    // ── public tool names & synthetic non-PII are left intact ──
    #[test]
    fn public_and_synthetic_content_is_untouched() {
        let keep = "OLLAMA_URL, LiteLLM, Prometheus, Grafana at 192.0.2.10 (RFC-5737)";
        assert_eq!(scrub(keep), keep, "public tools + doc IP untouched");
        // A bare UUID with no cue on the line is kept; the same UUID on a cue line
        // is scrubbed.
        let bare = "let id = \"4ef3f3ec-e7ef-4af3-b258-881565e629f9\"; // test data";
        assert_eq!(scrub(bare), bare, "bare UUID kept");
        assert_eq!(
            scrub("# PLANE_PROJECT_ID=<uuid>"),  // pii-test-fixture
            "# PLANE_PROJECT_ID=<uuid>"
        );
    }

    // ── GHMRFIX-6: canonical phones scrubbed; non-phone digit shapes untouched ──
    #[test]
    fn canonical_phones_are_scrubbed_but_dates_are_not() {
        // The exact fixtures the GHIST full-history gate flagged in Lumina history.
        assert_eq!(scrub(r#"    "phone": "<phone>","#), r#"    "phone": "<phone>","#); // pii-test-fixture
        assert_eq!(
            scrub("call <phone>. SSN: 123-45-6789."), // pii-test-fixture
            "call <phone>. SSN: 123-45-6789." // 3-3-4 phone scrubbed; 3-2-4 fake SSN left (gate doesn't flag it)
        );
        assert_eq!(scrub("reach <phone> now"), "reach <phone> now"); // pii-test-fixture (e.164)
        // Non-phone digit shapes the strict pattern must NOT touch.
        for keep in [
            "released 2026-01-01 today",
            "version 1.2.3 build 20260514-100000",
            "values 10 20 30 40 middle",
            "range 1000-10000 ms",
        ] {
            assert_eq!(scrub(keep), keep, "non-phone shape untouched: {keep}");
        }
    }

    // ── review hardening: no identifier / venv / multi-line over-redaction ──
    #[test]
    fn hardening_avoids_identifier_and_venv_over_redaction() {
        // A generic `plane_api_`/`github_pat_`-prefixed IDENTIFIER (short body,
        // lowercase) must NOT be redacted into a non-identifier token.
        assert_eq!(scrub("let plane_api_base_url = cfg;"), "let plane_api_base_url = cfg;");
        assert_eq!(scrub("fn github_pat_lumina() {}"), "fn github_pat_lumina() {}");
        // A real GitHub token (long body) still scrubs.
        assert_eq!(
            scrub("<REDACTED-SECRET>"),
            "<REDACTED-SECRET>"
        );
        // Host rule must not swallow an ordinary word starting with a host token.
        assert_eq!(scrub("source pvenv/bin/activate"), "source pvenv/bin/activate");
        assert_eq!(scrub("the pved daemon"), "the pved daemon");
        // …but a real host (optional numeric suffix) still scrubs.
        assert_eq!(scrub("on <host> and <host>"), "on <host> and <host>");  // pii-test-fixture
        // secret_field must not reach across a newline: a field name on line 1 and
        // a quoted value on line 2 must NOT connect (`[ \t]` instead of `\s`). With
        // the old `\s` this over-redacted; the value line is left intact and its
        // line count preserved. (A real key value would still be caught on its own
        // line by the secret-token patterns.)
        let two = "let my_token =\n    \"a-formatted-config-value\";\n";
        assert_eq!(scrub(two), two, "field prefix must not span the newline to the value");
    }

    // ── line count is preserved for a realistic mixed blob (corruption gate) ──
    #[test]
    fn line_count_is_always_preserved() {
        // Single-line literal (not a `\`-continued multi-line string) so the whole
        // statement carries the `pii-test-fixture` tag on the flagged line.
        let blob = "line one <internal-ip>\napi_key: Regex::new(r\"\\b(sk-|ghp_)\")\ntoken = \"<REDACTED-SECRET>\"\nplain line\nhost <host> <host>\n"; // pii-test-fixture
        let out = scrub(blob);
        assert_eq!(
            out.matches('\n').count(),
            blob.matches('\n').count(),
            "line count must be preserved: {out:?}"
        );
        assert!(out.contains(r#"Regex::new(r"\b(sk-|ghp_)")"#), "detection regex intact");
        assert!(out.contains("<internal-ip>") && out.contains("<host>"));
        assert!(out.contains("token = \"<REDACTED-SECRET>\""));
    }

    // ── trailing-newline round trip (split/join) ──
    #[test]
    fn trailing_newline_preserved_through_uuid_pass() {
        // Force the per-line UUID pass by including a UUID; assert newline shape.
        let with_nl = "project_id: <uuid>\n";  // pii-test-fixture
        assert_eq!(scrub(with_nl), "project_id: <uuid>\n");
        let no_nl = "project_id: <uuid>";  // pii-test-fixture
        assert_eq!(scrub(no_nl), "project_id: <uuid>");
    }
}
