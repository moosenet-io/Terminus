//! CXEG-05: Tier-A deterministic house-style lint set.
//!
//! A `syn`-AST checker that walks every `src/**/*.rs` file, parses it on the
//! normal stable toolchain (no `dylint`/rustc-driver — see `docs/house-style.md`
//! for why that path was rejected for this build host), and enforces a small
//! set of mechanical rules. It is wired into the Stage-4 test gate via
//! `tests/house_style.rs`'s `house_style_rules_hold()`, and runnable standalone
//! via `cargo run --bin house_style_check`.
//!
//! ## Rules (full catalog + rationale: `docs/house-style.md`)
//! 1. No raw `std::env::var("SECRET_SHAPED_NAME")` inside a
//!    `RustTool::execute`/`execute_structured` body — a secret-shaped value
//!    must be read through a dedicated accessor (this crate's established
//!    `*::from_env()` / `fn foo_token()` / `crate::config::*` convention),
//!    never inlined at the point of use. See `docs/house-style.md#rule-1` for
//!    why this is scoped to `execute` bodies rather than "anywhere outside
//!    `src/config.rs`" — the wider scope does not match this crate's actual,
//!    already-documented secret-materialization architecture (`crate::cortex`'s
//!    module doc, `crate::secrets_bootstrap`) and would flag ~100 pre-existing,
//!    correctly-routed reads. The rule matches `std::env::var`, `env::var`, and
//!    an imported/aliased `var(...)`/`<alias>(...)` (from `use std::env::var
//!    [as X];`), and fires for a read nested ANYWHERE inside `execute` — e.g.
//!    inside a local helper fn — not just the immediately-enclosing fn.
//! 2. Every `impl RustTool for X`'s `description()` returns a non-empty string
//!    literal (best-effort: only a plain literal tail expression is checked;
//!    a computed description is not statically verifiable and is skipped).
//! 3. NOT IMPLEMENTED in this pass — deferred, see `docs/house-style.md#rule-3`.
//! 4. No `panic!` inside a `RustTool::execute`/`execute_structured` body
//!    (`.unwrap()` was evaluated and intentionally scoped out — see
//!    `docs/house-style.md#rule-4`).
//!
//! ## Deny-by-default
//! A source-tree gate must fail closed: any `src/**/*.rs` file that cannot be
//! walked, read, or parsed is reported as a [`Rule::FileError`] violation (the
//! gate fails), never silently skipped — a file the checker can't inspect
//! could be hiding real violations.
//!
//! ## Test code is exempt
//! Rules 1/4 never fire in test code. Test context is detected by PARSING the
//! `#[cfg(...)]` predicate (via [`cfg_predicate_is_test`]), not substring-
//! matching "test": `#[cfg(test)]` / `#[cfg(all(test, …))]` / `#[cfg(any(test,
//! …))]` are test context, but `#[cfg(not(test))]` (production code) is NOT
//! and is fully checked.
//!
//! ## Known limitation (documented, not a bug)
//! Rule 1 matches a STRING-LITERAL var name only. An indirected name — a
//! `const`/`let`/`format!`-built key passed to `env::var(key)` — is not
//! resolved (that needs real dataflow analysis, out of scope for a mechanical
//! lint). See `docs/house-style.md#known-limitations`.
//!
//! ## Sanctioned files are exempt from RULE 1 ONLY
//! `src/config.rs` / `src/<secret-manager>/mod.rs` / `src/secrets_bootstrap.rs` are
//! the sanctioned secret-materialization layer, so a raw secret read there is
//! not a Rule-1 violation. This exemption is applied at the point a
//! `RawSecretEnvVar` finding would be recorded — every file is still parsed
//! and visited, so Rules 2 (empty description) and 4 (`panic!` in `execute`)
//! apply to them too (`src/<secret-manager>/mod.rs` has real `RustTool` impls).
//!
//! ## Waivers
//! A `// house-style-allow: <reason>` line comment, on the same line as the
//! violation or the line immediately above it, suppresses that one finding.
//! An empty/missing reason after the colon does NOT suppress (the underlying
//! finding stays live) AND is independently reported as a
//! [`Rule::ReasonlessWaiver`] by a standalone line scan
//! ([`Checker::scan_reasonless_waivers`]) — so even a dangling reasonless
//! waiver attached to no finding fails the gate. Prose mentions of the marker
//! (in backticks etc.) are not treated as waivers; the checker's own source
//! is exempt from the standalone scan (see [`is_checker_own_source`]). Mirrors
//! this crate's existing `// pii-test-fixture` line-exact convention
//! (`crate::github::pii`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCall, ExprLit, ExprPath, ImplItem, ItemFn, ItemImpl, ItemMod, ItemUse,
    Lit, Meta, UseTree,
};
use walkdir::WalkDir;

/// Files that are themselves the sanctioned secret-materialization layer —
/// exempt from Rule 1 entirely (they define the accessor / bootstrap, they
/// don't call it). See `docs/house-style.md#allow-list`.
const SANCTIONED_FILES: &[&str] = &[
    "src/config.rs",
    "src/<secret-manager>/mod.rs", // pii-test-fixture
    "src/secrets_bootstrap.rs",
];

/// One rule this checker enforces, numbered to match the CXEG-05 spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Rule 1: raw `std::env::var` for a secret-shaped name inside `execute`.
    RawSecretEnvVar,
    /// Rule 2: `RustTool::description()` returns an empty string literal.
    EmptyToolDescription,
    /// Rule 4: `panic!` inside a `RustTool::execute`/`execute_structured` body.
    PanicInExecute,
    /// Meta-rule: a `// house-style-allow:` waiver with no reason after the colon.
    ReasonlessWaiver,
    /// Infra: a `src/**/*.rs` file could not be read or parsed. A source-tree
    /// gate is deny-by-default — an unreadable/unparseable file could hide any
    /// number of violations, so it FAILS the gate rather than being skipped.
    FileError,
}

impl Rule {
    pub fn id(self) -> &'static str {
        match self {
            Rule::RawSecretEnvVar => "house-style-1-raw-secret-env",
            Rule::EmptyToolDescription => "house-style-2-empty-description",
            Rule::PanicInExecute => "house-style-4-panic-in-execute",
            Rule::ReasonlessWaiver => "house-style-waiver-reason",
            Rule::FileError => "house-style-file-error",
        }
    }
}

/// A single finding: `file:line: message` plus a `help:` fix hint.
#[derive(Debug, Clone)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub rule: Rule,
    pub message: String,
    pub help: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}:{}: [{}] {}", self.file.display(), self.line, self.rule.id(), self.message)?;
        write!(f, "  help: {}", self.help)
    }
}

/// Walk `repo_root/src/**/*.rs`, parse each file with `syn`, and return every
/// house-style violation found (after resolving `// house-style-allow:`
/// waivers). Deterministic: same tree in, same violations out, no I/O beyond
/// reading the `.rs` files themselves.
pub fn check_tree(repo_root: &Path) -> Vec<Violation> {
    let src_dir = repo_root.join("src");
    let mut violations = Vec::new();

    // Deny-by-default: a WalkDir error (e.g. an unreadable directory) is a gate
    // failure, not something to silently drop — it could be hiding files that
    // contain violations.
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(&src_dir) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let p = e.into_path();
                if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                    entries.push(p);
                }
            }
            Ok(_) => {}
            Err(err) => {
                let path = err
                    .path()
                    .map(|p| rel_path(repo_root, p))
                    .unwrap_or_else(|| src_dir.to_string_lossy().replace('\\', "/"));
                violations.push(Violation {
                    file: PathBuf::from(path),
                    line: 0,
                    rule: Rule::FileError,
                    message: format!("failed to walk the source tree: {err}"),
                    help: "the checker is deny-by-default — resolve the I/O error so every `.rs` \
                           file under `src/` can be scanned"
                        .to_string(),
                });
            }
        }
    }
    // Deterministic ordering regardless of filesystem iteration order.
    entries.sort();

    for path in entries {
        let rel = rel_path(repo_root, &path);
        // NOTE: sanctioned files are NOT skipped here — every file is parsed
        // and visited so Rules 2 (empty description) and 4 (panic in execute)
        // apply everywhere, including `src/<secret-manager>/mod.rs` (which has real
        // `RustTool` impls). The sanctioned exemption is Rule-1-only and is
        // applied at the point a `RawSecretEnvVar` would be recorded.
        let source = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                violations.push(Violation {
                    file: PathBuf::from(rel),
                    line: 0,
                    rule: Rule::FileError,
                    message: format!("failed to read file: {e}"),
                    help: "the checker is deny-by-default — an unreadable source file could hide \
                           violations, so it fails the gate rather than being skipped"
                        .to_string(),
                });
                continue;
            }
        };
        // A parse failure means the checker cannot inspect this file's AST. It
        // is NOT silently skipped (that would let a malformed file hide
        // violations): it is reported as a failure. In practice `cargo
        // build`/`cargo test` would also fail on a genuinely un-parseable file,
        // but this gate must stand on its own and fail closed.
        let file = match syn::parse_file(&source) {
            Ok(f) => f,
            Err(e) => {
                violations.push(Violation {
                    file: PathBuf::from(rel),
                    line: line_of(e.span()),
                    rule: Rule::FileError,
                    message: format!("failed to parse file with `syn`: {e}"),
                    help: "the checker is deny-by-default — a file it cannot parse could hide \
                           violations; fix the syntax so it parses (the compiler will need this too)"
                        .to_string(),
                });
                continue;
            }
        };
        let aliases = collect_env_var_aliases(&file);
        let lines: Vec<&str> = source.lines().collect();
        let file_sanctioned = SANCTIONED_FILES.contains(&rel.as_str());
        let mut checker = Checker {
            file_rel: rel,
            lines: &lines,
            env_var_aliases: &aliases,
            file_sanctioned,
            test_depth: 0,
            fn_stack: Vec::new(),
            in_rust_tool_impl: false,
            violations: Vec::new(),
        };
        checker.run(&file);
        violations.extend(checker.violations);
    }

    violations.sort_by(|a, b| (&a.file, a.line, a.rule.id()).cmp(&(&b.file, b.line, b.rule.id())));
    violations
}

/// Repo-relative, forward-slashed path string for a file (falls back to the
/// full path if it is somehow not under `repo_root`).
fn rel_path(repo_root: &Path, path: &Path) -> String {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── import detection (Rule 1, aliased/imported `std::env::var`) ──────────────

/// Collect the set of bare identifiers in a file that resolve to
/// `std::env::var` via a `use` import — so that a call like `var("TOKEN")`
/// (from `use std::env::var;`) or `evar("TOKEN")` (from
/// `use std::env::var as evar;`) is matched by Rule 1 just like a fully-spelled
/// `std::env::var(...)`. A bare `var(...)` is only treated as `env::var` when
/// such an import is present in the SAME file, keeping false-positive risk low.
///
/// NOTE: `use std::env;` + `env::var(...)` needs no entry here — that call site
/// is already a 2-segment `env::var` path, matched directly in
/// [`Checker::visit_expr_call`].
fn collect_env_var_aliases(file: &syn::File) -> HashSet<String> {
    struct UseCollector {
        aliases: HashSet<String>,
    }
    impl<'ast> Visit<'ast> for UseCollector {
        fn visit_item_use(&mut self, node: &'ast ItemUse) {
            record_use_tree(&node.tree, &mut Vec::new(), &mut self.aliases);
        }
    }
    let mut c = UseCollector { aliases: HashSet::new() };
    // `syn::visit` descends into nested modules and function bodies, so a
    // `use std::env::var;` anywhere in the file (not just at the top) is seen.
    c.visit_file(file);
    c.aliases
}

/// Walk a `use` tree accumulating the path prefix; when the leaf is
/// `std::env::var` (optionally renamed), record the bound name.
fn record_use_tree(tree: &UseTree, prefix: &mut Vec<String>, out: &mut HashSet<String>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            record_use_tree(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            if prefix.len() == 2 && prefix[0] == "std" && prefix[1] == "env" && n.ident == "var" {
                out.insert("var".to_string());
            }
        }
        UseTree::Rename(r) => {
            if prefix.len() == 2 && prefix[0] == "std" && prefix[1] == "env" && r.ident == "var" {
                out.insert(r.rename.to_string());
            }
        }
        UseTree::Group(g) => {
            for item in &g.items {
                record_use_tree(item, prefix, out);
            }
        }
        UseTree::Glob(_) => {}
    }
}

// ── secret-shape classification (Rule 1) ───────────────────────────────────

/// Case-insensitive, segment-aware (`_`-delimited) classification of an env
/// var NAME as secret-shaped. Deliberately NOT a blanket "contains KEY/URL"
/// substring match — see `docs/house-style.md#rule-1-classification` for the
/// false-positive/negative audit this was calibrated against (e.g. `PATH`
/// must not match `PAT`; `GITHUB_API_BASE` must not match on `API`; a bare
/// `*_URL` must not match — this crate has ~30 legitimate non-secret service
/// endpoint URLs — but `*_DATABASE_URL` must, since a DB DSN carries embedded
/// credentials per CLAUDE.md's "DB URLs" enumeration).
fn secret_shaped(name: &str) -> bool {
    let segs: Vec<String> = name.split('_').map(|s| s.to_ascii_uppercase()).collect();
    const EXACT: &[&str] = &["PAT", "CREDS"];
    const SUFFIX: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "JWT"];
    let has_database = segs.iter().any(|s| s == "DATABASE");
    let has_url = segs.iter().any(|s| s == "URL");
    segs.iter().any(|s| EXACT.contains(&s.as_str()) || SUFFIX.iter().any(|suf| s.ends_with(suf)))
        || (has_database && has_url)
}

// ── waiver resolution ───────────────────────────────────────────────────────

const WAIVER_MARKER: &str = "house-style-allow";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiver {
    /// No `house-style-allow` comment marker on the line.
    None,
    /// Marker present as a real comment with a non-empty reason after the colon.
    Reasoned,
    /// Marker present as a real comment but with no (or empty) reason.
    Reasonless,
}

/// Classify a single source line for a `house-style-allow` comment marker.
///
/// To avoid flagging the many *prose* references to the marker (this module's
/// own docs, help strings, and test fixtures all mention it), a line is only
/// treated as carrying a waiver when the marker is a genuine `//` line comment
/// AND the character immediately after the marker is `:`, whitespace, or
/// end-of-line. A marker immediately followed by any other character (a
/// backtick, a quote, an identifier char — i.e. it is being *talked about*,
/// not *used*) yields [`Waiver::None`].
fn classify_waiver_comment(text: &str) -> Waiver {
    let Some(idx) = text.find(WAIVER_MARKER) else {
        return Waiver::None;
    };
    // Must be an actual comment marker: the text just before it, trimmed, ends
    // with `//` (e.g. `// house-style-allow`, `//house-style-allow`).
    if !text[..idx].trim_end().ends_with("//") {
        return Waiver::None;
    }
    let rest = &text[idx + WAIVER_MARKER.len()..];
    if let Some(reason) = rest.strip_prefix(':') {
        if reason.trim().trim_end_matches("*/").trim().is_empty() {
            Waiver::Reasonless
        } else {
            Waiver::Reasoned
        }
    } else if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
        // `// house-style-allow` with no colon at all — reasonless.
        Waiver::Reasonless
    } else {
        // e.g. `` `// house-style-allow` `` inside prose — a mention, not a use.
        Waiver::None
    }
}

/// Suppression lookup for a specific finding: is there a REASONED waiver on the
/// finding's own source line, or the line above it (1-indexed `line`, matching
/// `Violation::line` / span line numbers)? A reasonless waiver does NOT
/// suppress — an invalid waiver leaves the underlying finding live (and is
/// separately reported by [`Checker::scan_reasonless_waivers`]). Mirrors
/// `crate::github::pii`'s `pii-test-fixture` line-exact convention, plus the
/// mandatory-reason requirement.
fn waiver_status(lines: &[&str], line: usize) -> Waiver {
    for candidate in [line, line.saturating_sub(1)] {
        if candidate == 0 {
            continue;
        }
        let Some(text) = lines.get(candidate - 1) else { continue };
        match classify_waiver_comment(text) {
            Waiver::None => continue,
            w => return w,
        }
    }
    Waiver::None
}

/// The checker's OWN source files necessarily embed the `house-style-allow`
/// marker in module docs, help strings, and test fixtures — scanning them for
/// standalone reasonless waivers would flag those examples. They are exempt
/// from the standalone-waiver scan only (Rule 1's sanctioned-file list is a
/// separate concern). Mirrors how `crate::github::pii` self-exempts its own
/// pattern-bearing source.
fn is_checker_own_source(rel: &str) -> bool {
    rel.starts_with("src/house_style/") || rel == "src/bin/house_style_check.rs"
}

// ── test-context detection ──────────────────────────────────────────────────

/// True if `attrs` marks this item as test-only: a `test` attribute
/// (`#[test]`, `#[tokio::test]`, `#[async_std::test]` — matched by trailing
/// path segment `test`), or a `#[cfg(...)]` whose predicate contains `test` as
/// a POSITIVE predicate.
///
/// Crucially, this PARSES the `cfg` predicate rather than substring-matching
/// "test": `#[cfg(not(test))]` (and any `not(...)` wrapping `test`) is
/// PRODUCTION code and must NOT be treated as test context. `#[cfg(test)]`,
/// `#[cfg(all(test, ...))]`, and `#[cfg(any(test, ...))]` are test context.
fn has_test_context(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        // `#[test]` / `#[tokio::test]` — but NOT `#[cfg(...)]` (its last
        // segment is `cfg`, handled below).
        if a.path().is_ident("cfg") {
            return a
                .parse_args::<Meta>()
                .map(|meta| cfg_predicate_is_test(&meta, false))
                .unwrap_or(false);
        }
        a.path().segments.last().map(|s| s.ident == "test").unwrap_or(false)
    })
}

/// Evaluate whether a `cfg` predicate makes the annotated item test-only —
/// i.e. whether `test` appears as a POSITIVE predicate (nested under an even
/// number of `not(...)`). `negated` tracks the parity of enclosing `not`s.
///
/// - `test` → positive when not negated.
/// - `not(P)` → recurse into `P` with the negation flipped.
/// - `all(P, ...)` / `any(P, ...)` → positive if ANY child is positive under
///   the current negation (a build with `--test` compiles `any(test, ...)`,
///   and `all(test, ...)` is by definition test-gated).
/// - anything else (`feature = "x"`, `unix`, ...) → not test.
fn cfg_predicate_is_test(meta: &Meta, negated: bool) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test") && !negated,
        Meta::List(l) => {
            let op = l.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
            let Ok(nested) = l.parse_args_with(
                syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            match op.as_str() {
                "not" => nested.iter().any(|m| cfg_predicate_is_test(m, !negated)),
                "all" | "any" => nested.iter().any(|m| cfg_predicate_is_test(m, negated)),
                _ => false,
            }
        }
        Meta::NameValue(_) => false,
    }
}

fn line_of(span: Span) -> usize {
    span.start().line
}

// ── the visitor ──────────────────────────────────────────────────────────────

struct Checker<'s> {
    file_rel: String,
    lines: &'s [&'s str],
    /// Bare identifiers that resolve to `std::env::var` in THIS file via a
    /// `use` import (e.g. `var` from `use std::env::var;`, or an alias).
    env_var_aliases: &'s HashSet<String>,
    /// >0 while inside any `#[cfg(test)]`/`#[test]`-marked item. Rules 1/4
    /// never fire in test code (fixtures, mocks, and assertion helpers are
    /// not production secret handling or user-facing tool execution).
    test_depth: u32,
    /// Enclosing named-function stack (free fns and impl methods), outermost
    /// first. Used to tell whether the current node is lexically nested
    /// anywhere inside an `execute`/`execute_structured` body.
    fn_stack: Vec<String>,
    /// True while visiting the body of an `impl RustTool for X` block.
    in_rust_tool_impl: bool,
    /// True for `src/config.rs` / `src/<secret-manager>/mod.rs` /
    /// `src/secrets_bootstrap.rs` — the sanctioned secret-materialization
    /// layer. Suppresses RULE 1 (raw secret env read) ONLY; Rules 2/4 still
    /// apply to these files.
    file_sanctioned: bool,
    violations: Vec<Violation>,
}

impl<'s> Checker<'s> {
    /// Run every check over a parsed file: the AST visitor (Rules 1/2/4) plus
    /// the standalone reasonless-waiver line scan.
    fn run(&mut self, file: &syn::File) {
        self.visit_file(file);
        self.scan_reasonless_waivers();
    }

    /// True if the current node is lexically nested ANYWHERE inside a
    /// `RustTool::execute`/`execute_structured` body — not merely if the
    /// immediately-enclosing fn is one. This closes the "wrap the raw read in
    /// a local helper fn defined inside `execute`" bypass: such a helper's
    /// body still has an `execute` frame below it on `fn_stack`.
    fn in_execute(&self) -> bool {
        self.fn_stack
            .iter()
            .any(|f| f == "execute" || f == "execute_structured")
    }

    /// Record a finding unless a REASONED waiver on the same/previous line
    /// suppresses it. A reasonless waiver does NOT suppress (the finding stays
    /// live); the reasonless waiver itself is reported separately by
    /// [`Self::scan_reasonless_waivers`].
    fn record(&mut self, rule: Rule, line: usize, message: String, help: String) {
        if waiver_status(self.lines, line) == Waiver::Reasoned {
            return;
        }
        self.violations.push(Violation {
            file: PathBuf::from(self.file_rel.as_str()),
            line,
            rule,
            message,
            help,
        });
    }

    /// Standalone scan (independent of any other finding): every reasonless
    /// `// house-style-allow` comment is itself a violation, whether or not it
    /// is attached to a suppressed finding. Skips the checker's own source
    /// files (see [`is_checker_own_source`]).
    fn scan_reasonless_waivers(&mut self) {
        if is_checker_own_source(&self.file_rel) {
            return;
        }
        for (i, text) in self.lines.iter().enumerate() {
            if classify_waiver_comment(text) == Waiver::Reasonless {
                self.violations.push(Violation {
                    file: PathBuf::from(self.file_rel.as_str()),
                    line: i + 1,
                    rule: Rule::ReasonlessWaiver,
                    message: "`// house-style-allow` waiver has no reason after the colon".to_string(),
                    help: "give the waiver a reason: `// house-style-allow: <why this is OK>` — a bare \
                           `// house-style-allow` (no colon, or an empty reason) is itself a violation"
                        .to_string(),
                });
            }
        }
    }

    /// Rule 2: `impl RustTool for X`'s `description()` must not be a plain
    /// empty string literal. Best-effort — only the common `{ "..." }`
    /// tail-expression shape is checked; a computed/formatted description is
    /// skipped (cannot be verified non-empty statically).
    fn check_description(&mut self, node: &ItemImpl) {
        for item in &node.items {
            let ImplItem::Fn(f) = item else { continue };
            if f.sig.ident != "description" {
                continue;
            }
            if let Some(syn::Stmt::Expr(Expr::Lit(ExprLit { lit: Lit::Str(s), .. }), None)) = f.block.stmts.last() {
                if s.value().is_empty() {
                    self.record(
                        Rule::EmptyToolDescription,
                        line_of(s.span()),
                        "RustTool::description() returns an empty string literal".to_string(),
                        "give the tool a real, non-empty description -- it is shown in the MCP tool catalog"
                            .to_string(),
                    );
                }
            }
        }
    }
}

impl<'s, 'ast> Visit<'ast> for Checker<'s> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let bumped = has_test_context(&node.attrs);
        if bumped {
            self.test_depth += 1;
        }
        visit::visit_item_mod(self, node);
        if bumped {
            self.test_depth -= 1;
        }
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let bumped = has_test_context(&node.attrs);
        if bumped {
            self.test_depth += 1;
        }
        self.fn_stack.push(node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.fn_stack.pop();
        if bumped {
            self.test_depth -= 1;
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let bumped = has_test_context(&node.attrs);
        if bumped {
            self.test_depth += 1;
        }
        self.fn_stack.push(node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.fn_stack.pop();
        if bumped {
            self.test_depth -= 1;
        }
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        // Defensive: an `impl` block can itself carry `#[cfg(test)]` directly
        // (not just be nested inside a `#[cfg(test)] mod tests { .. }`,
        // which `visit_item_mod` above already handles).
        let bumped = has_test_context(&node.attrs);
        if bumped {
            self.test_depth += 1;
        }

        let is_rust_tool = node
            .trait_
            .as_ref()
            .map(|(_, path, _)| path.segments.last().map(|s| s.ident == "RustTool").unwrap_or(false))
            .unwrap_or(false);
        let prev = self.in_rust_tool_impl;
        self.in_rust_tool_impl = is_rust_tool;

        if is_rust_tool && self.test_depth == 0 {
            self.check_description(node);
        }

        visit::visit_item_impl(self, node);
        self.in_rust_tool_impl = prev;
        if bumped {
            self.test_depth -= 1;
        }
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if self.test_depth == 0 && self.in_rust_tool_impl && self.in_execute() {
            if let Expr::Path(ExprPath { path, .. }) = node.func.as_ref() {
                let n = path.segments.len();
                // Fully-qualified `std::env::var(...)` (3-seg) or `env::var(...)`
                // (2-seg, e.g. via `use std::env;`) — matched by the trailing
                // `env::var` regardless of the leading segments.
                let qualified_env_var = n >= 2
                    && path.segments[n - 1].ident == "var"
                    && path.segments[n - 2].ident == "env";
                // Bare `var(...)` / `alias(...)` — only when the file imported
                // `std::env::var` (possibly renamed); see `collect_env_var_aliases`.
                let imported_env_var = n == 1
                    && self.env_var_aliases.contains(&path.segments[0].ident.to_string());
                // RULE 1 EXEMPTION (this rule only): the sanctioned
                // secret-materialization files ARE the accessor layer, so a raw
                // read there is not a violation. Rules 2/4 still ran on this
                // file (they don't consult `file_sanctioned`).
                if (qualified_env_var || imported_env_var) && !self.file_sanctioned {
                    if let Some(Expr::Lit(ExprLit { lit: Lit::Str(s), .. })) = node.args.first() {
                        let name = s.value();
                        if secret_shaped(&name) {
                            self.record(
                                Rule::RawSecretEnvVar,
                                line_of(node.span()),
                                format!(
                                    "raw `std::env::var(\"{name}\")` for a secret-shaped name inline in \
                                     `RustTool::execute`"
                                ),
                                format!(
                                    "read `{name}` through a dedicated accessor OUTSIDE `execute` -- this \
                                     crate's convention: `*::from_env()`, a small `fn foo_token()`-style \
                                     helper, or `crate::config::*` -- not inline in the tool body (see \
                                     `docs/house-style.md#rule-1`)"
                                ),
                            );
                        }
                    }
                }
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if self.test_depth == 0 && self.in_rust_tool_impl && self.in_execute() {
            if node.path.segments.last().map(|s| s.ident == "panic").unwrap_or(false) {
                self.record(
                    Rule::PanicInExecute,
                    line_of(node.span()),
                    "`panic!` inside a `RustTool::execute`/`execute_structured` body".to_string(),
                    "return a `ToolError` (e.g. `ToolError::Execution`/`ToolError::InvalidArgument`) instead \
                     of aborting the process on unexpected/external input"
                        .to_string(),
                );
            }
        }
        visit::visit_macro(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_shaped_classifies_known_credentials() {
        for name in [
            "GITHUB_TOKEN",
            "GITEA_PAT_MOOSE",
            "PLANE_API_KEY",
            "GOOGLE_APP_PASSWORD",
            "QTOR_CREDS",
            "ATLAS_DATABASE_URL",
            "DATABASE_URL",
            "TERMINUS_TSNET_AUTHKEY",
            "CHORD_JWT",
        ] {
            assert!(secret_shaped(name), "expected {name} to be secret-shaped");
        }
    }

    #[test]
    fn secret_shaped_does_not_false_positive_on_non_secrets() {
        for name in [
            "PATH",
            "HOME",
            "SCRIBE_REPO_PATH",
            "MODEL_REGISTRY_PATH",
            "GITHUB_API_BASE",
            "GITEA_URL",
            "PLANE_API_URL",
            "COUNCIL_CONSTELLATION_YAML_PATH",
            "PLANE_REDIS_URL",
            "GITHUB_IDENTITY_NAME",
        ] {
            assert!(!secret_shaped(name), "expected {name} to NOT be secret-shaped");
        }
    }

    #[test]
    fn waiver_requires_a_reason() {
        let lines = ["let x = 1; // house-style-allow: known-safe fixture", "let y = 2; // house-style-allow"];
        assert!(matches!(waiver_status(&lines, 1), Waiver::Reasoned));
        assert!(matches!(waiver_status(&lines, 2), Waiver::Reasonless));
        assert!(matches!(waiver_status(&lines, 100), Waiver::None));
    }

    #[test]
    fn waiver_honored_on_previous_line() {
        let lines = ["// house-style-allow: see ADR-1", "std::env::var(\"X_TOKEN\")"];
        assert!(matches!(waiver_status(&lines, 2), Waiver::Reasoned));
    }

    #[test]
    fn check_tree_is_green_on_this_crate() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let violations = check_tree(manifest_dir);
        assert!(
            violations.is_empty(),
            "house-style violations on the current tree:\n{}",
            violations.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n\n")
        );
    }

    // ── helper: run the whole per-file pipeline over an in-memory source ─────

    fn check_source(src: &str) -> Vec<Violation> {
        check_source_as("src/x.rs", src)
    }

    /// Like [`check_source`] but with an explicit relative path, so a test can
    /// simulate a sanctioned file (Rule 1 exempt) or the checker's own source
    /// (reasonless-waiver-scan exempt).
    fn check_source_as(rel: &str, src: &str) -> Vec<Violation> {
        let file = syn::parse_file(src).expect("test source must parse");
        let aliases = collect_env_var_aliases(&file);
        let lines: Vec<&str> = src.lines().collect();
        let mut checker = Checker {
            file_rel: rel.to_string(),
            lines: &lines,
            env_var_aliases: &aliases,
            file_sanctioned: SANCTIONED_FILES.contains(&rel),
            test_depth: 0,
            fn_stack: Vec::new(),
            in_rust_tool_impl: false,
            violations: Vec::new(),
        };
        checker.run(&file);
        checker.violations
    }

    fn rules(vs: &[Violation]) -> Vec<Rule> {
        vs.iter().map(|v| v.rule).collect()
    }

    // ── Fix #1: cfg(not(test)) is PRODUCTION, not test context ──────────────

    fn cfg_meta(src: &str) -> Meta {
        // Parse `cfg(<pred>)` out of a `#[cfg(<pred>)]` attribute.
        let item: syn::ItemFn = syn::parse_str(&format!("{src}\nfn f() {{}}")).unwrap();
        item.attrs[0].parse_args::<Meta>().unwrap()
    }

    #[test]
    fn cfg_test_positive_forms_are_test_context() {
        assert!(cfg_predicate_is_test(&cfg_meta("#[cfg(test)]"), false));
        assert!(cfg_predicate_is_test(&cfg_meta("#[cfg(all(test, unix))]"), false));
        assert!(cfg_predicate_is_test(&cfg_meta("#[cfg(any(test, feature = \"x\"))]"), false));
    }

    #[test]
    fn cfg_not_test_is_not_test_context() {
        // The critical regression: production code guarded by `not(test)` must
        // NOT be treated as test context (else it is wrongly skipped).
        assert!(!cfg_predicate_is_test(&cfg_meta("#[cfg(not(test))]"), false));
        assert!(!cfg_predicate_is_test(&cfg_meta("#[cfg(all(not(test), unix))]"), false));
        assert!(!cfg_predicate_is_test(&cfg_meta("#[cfg(feature = \"x\")]"), false));
        // Double negation is positive again.
        assert!(cfg_predicate_is_test(&cfg_meta("#[cfg(not(not(test)))]"), false));
    }

    #[test]
    fn secret_read_in_cfg_not_test_fn_is_flagged() {
        // A `#[cfg(not(test))]` execute body is PRODUCTION — Rule 1 must fire.
        let src = r#"
            struct T;
            impl RustTool for T {
                #[cfg(not(test))]
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = std::env::var("GITHUB_TOKEN");
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::RawSecretEnvVar]);
    }

    // ── Fix #2: imported / aliased std::env::var ────────────────────────────

    #[test]
    fn imported_bare_var_is_flagged() {
        let src = r#"
            use std::env::var;
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = var("PLANE_API_KEY");
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::RawSecretEnvVar]);
    }

    #[test]
    fn aliased_var_is_flagged() {
        let src = r#"
            use std::env::var as getenv;
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = getenv("GITEA_PAT_MOOSE");
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::RawSecretEnvVar]);
    }

    #[test]
    fn two_segment_env_var_is_flagged() {
        let src = r#"
            use std::env;
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = env::var("OPENROUTER_API_KEY");
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::RawSecretEnvVar]);
    }

    #[test]
    fn bare_var_without_import_is_not_flagged() {
        // No `use std::env::var;` in the file, so a bare `var(...)` is some
        // other function — must NOT be treated as env::var.
        let src = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = var("GITHUB_TOKEN");
                    Ok(String::new())
                }
            }
            fn var(_k: &str) -> String { String::new() }
        "#;
        assert!(check_source(src).is_empty());
    }

    // ── Fix #3: read nested in a local helper fn inside execute ─────────────

    #[test]
    fn secret_read_in_nested_helper_inside_execute_is_flagged() {
        let src = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    fn inner() -> Result<String, ()> {
                        std::env::var("GITHUB_TOKEN").map_err(|_| ())
                    }
                    let _ = inner();
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::RawSecretEnvVar]);
    }

    #[test]
    fn secret_read_in_non_execute_method_is_not_flagged() {
        // A sibling impl method (not execute, not nested in it) is the crate's
        // sanctioned accessor pattern — must NOT be flagged.
        let src = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> { Ok(String::new()) }
            }
            impl T {
                fn from_env() -> Option<String> { std::env::var("GITHUB_TOKEN").ok() }
            }
        "#;
        assert!(check_source(src).is_empty());
    }

    // ── Rule 2 / Rule 4 quick coverage ──────────────────────────────────────

    #[test]
    fn empty_description_is_flagged_and_panic_in_execute_is_flagged() {
        let src = r#"
            struct T;
            impl RustTool for T {
                fn description(&self) -> &str { "" }
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    panic!("boom");
                }
            }
        "#;
        let mut got = rules(&check_source(src));
        got.sort_by_key(|r| r.id());
        assert_eq!(got, vec![Rule::EmptyToolDescription, Rule::PanicInExecute]);
    }

    #[test]
    fn test_only_code_is_skipped() {
        // A full RustTool impl (with a secret read + panic! in execute AND an
        // empty description) nested in a `#[cfg(test)]` mod must be entirely
        // exempt — it's a mock, not production.
        let src = r#"
            #[cfg(test)]
            mod tests {
                struct MockTool;
                impl RustTool for MockTool {
                    fn description(&self) -> &str { "" }
                    async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                        let _ = std::env::var("GITHUB_TOKEN");
                        panic!("mock");
                    }
                }
            }
        "#;
        assert!(check_source(src).is_empty());
    }

    // ── Fix #4: deny-by-default on an unparseable file ──────────────────────

    #[test]
    fn unparseable_file_fails_the_gate() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        // Not valid Rust — `syn::parse_file` must reject it.
        let mut f = std::fs::File::create(dir.path().join("src/bad.rs")).unwrap();
        write!(f, "fn broken( {{ this is not rust ").unwrap();
        let violations = check_tree(dir.path());
        assert!(
            violations.iter().any(|v| v.rule == Rule::FileError),
            "an unparseable file must produce a FileError, got: {violations:?}"
        );
    }

    #[test]
    fn waiver_suppresses_and_reasonless_waiver_is_itself_a_violation() {
        let reasoned = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    // house-style-allow: legacy one-off, tracked in TERM-999
                    let _ = std::env::var("GITHUB_TOKEN");
                    Ok(String::new())
                }
            }
        "#;
        assert!(check_source(reasoned).is_empty());

        // A reasonless waiver attached to a real finding does NOT suppress it:
        // the underlying finding stays live AND the reasonless waiver is its
        // own violation.
        let reasonless = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = std::env::var("GITHUB_TOKEN"); // house-style-allow
                    Ok(String::new())
                }
            }
        "#;
        let mut got = rules(&check_source(reasonless));
        got.sort_by_key(|r| r.id());
        assert_eq!(got, vec![Rule::RawSecretEnvVar, Rule::ReasonlessWaiver]);
    }

    // ── Fix #2 (cycle 2): a STANDALONE reasonless waiver is a violation ─────

    #[test]
    fn standalone_reasonless_waiver_is_flagged_even_without_a_finding() {
        // No underlying finding anywhere — just a bare reasonless waiver line.
        let src = r#"
            struct T;
            impl RustTool for T {
                fn description(&self) -> &str { "ok" }
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    // house-style-allow
                    Ok(String::new())
                }
            }
        "#;
        assert_eq!(rules(&check_source(src)), vec![Rule::ReasonlessWaiver]);
    }

    #[test]
    fn standalone_reasonless_waiver_empty_colon_is_flagged() {
        let src = "// house-style-allow:   \nstruct Z;\n";
        assert_eq!(rules(&check_source(src)), vec![Rule::ReasonlessWaiver]);
    }

    #[test]
    fn well_formed_standalone_waiver_is_allowed() {
        let src = "// house-style-allow: documented reason\nstruct Z;\n";
        assert!(check_source(src).is_empty());
    }

    #[test]
    fn prose_mention_of_the_marker_is_not_a_waiver() {
        // The marker appears inside backticks / as a bare word — a mention,
        // not a use. Must not be flagged.
        let src = r#"
            /// See the `// house-style-allow` convention.
            /// The house-style-allow marker lives in comments.
            struct Z;
        "#;
        assert!(check_source(src).is_empty());
    }

    // ── Fix #1 (cycle 2): sanctioned files — Rule 1 exempt, Rules 2/4 apply ─

    #[test]
    fn sanctioned_file_rule1_exempt_but_rules_2_and_4_still_apply() {
        // Same source, checked once as a sanctioned file and once as an
        // ordinary file.
        let src = r#"
            struct T;
            impl RustTool for T {
                fn description(&self) -> &str { "" }
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = std::env::var("GITHUB_TOKEN");
                    panic!("boom");
                }
            }
        "#;
        // Sanctioned (e.g. src/<secret-manager>/mod.rs): the raw secret read (Rule 1)
        // is exempt, but the empty description (Rule 2) and panic! (Rule 4)
        // are STILL flagged.
        let mut sanctioned = rules(&check_source_as("src/<secret-manager>/mod.rs", src));
        sanctioned.sort_by_key(|r| r.id());
        assert_eq!(sanctioned, vec![Rule::EmptyToolDescription, Rule::PanicInExecute]);

        // Ordinary file: all three fire.
        let mut ordinary = rules(&check_source_as("src/foo/mod.rs", src));
        ordinary.sort_by_key(|r| r.id());
        assert_eq!(
            ordinary,
            vec![Rule::RawSecretEnvVar, Rule::EmptyToolDescription, Rule::PanicInExecute]
        );
    }

    #[test]
    fn all_three_sanctioned_files_are_rule1_exempt() {
        let src = r#"
            struct T;
            impl RustTool for T {
                async fn execute(&self, _a: Value) -> Result<String, ToolError> {
                    let _ = std::env::var("PLANE_API_KEY");
                    Ok(String::new())
                }
            }
        "#;
        for rel in ["src/config.rs", "src/<secret-manager>/mod.rs", "src/secrets_bootstrap.rs"] {
            assert!(
                check_source_as(rel, src).is_empty(),
                "{rel} should be Rule-1 exempt"
            );
        }
    }
}
