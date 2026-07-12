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
//!    correctly-routed reads.
//! 2. Every `impl RustTool for X`'s `description()` returns a non-empty string
//!    literal (best-effort: only a plain literal tail expression is checked;
//!    a computed description is not statically verifiable and is skipped).
//! 3. NOT IMPLEMENTED in this pass — deferred, see `docs/house-style.md#rule-3`.
//! 4. No `panic!` inside a `RustTool::execute`/`execute_structured` body
//!    (`.unwrap()` was evaluated and intentionally scoped out — see
//!    `docs/house-style.md#rule-4`).
//!
//! ## Waivers
//! A `// house-style-allow: <reason>` line comment, on the same line as the
//! violation or the line immediately above it, suppresses that one finding.
//! An empty or missing reason after the colon does NOT suppress — it is
//! itself reported as a [`Rule::ReasonlessWaiver`] violation, so a waiver can
//! never silently swallow a finding. Mirrors this crate's existing
//! `// pii-test-fixture` line-exact convention (`crate::github::pii`).

use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprCall, ExprLit, ExprPath, ImplItem, ItemImpl, ItemFn, ItemMod, Lit};
use walkdir::WalkDir;

/// Files that are themselves the sanctioned secret-materialization layer —
/// exempt from Rule 1 entirely (they define the accessor / bootstrap, they
/// don't call it). See `docs/house-style.md#allow-list`.
const SANCTIONED_FILES: &[&str] = &[
    "src/config.rs",
    "src/<secret-manager>/mod.rs",
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
}

impl Rule {
    pub fn id(self) -> &'static str {
        match self {
            Rule::RawSecretEnvVar => "house-style-1-raw-secret-env",
            Rule::EmptyToolDescription => "house-style-2-empty-description",
            Rule::PanicInExecute => "house-style-4-panic-in-execute",
            Rule::ReasonlessWaiver => "house-style-waiver-reason",
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

    let mut entries: Vec<PathBuf> = WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    // Deterministic ordering regardless of filesystem iteration order.
    entries.sort();

    for path in entries {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if SANCTIONED_FILES.contains(&rel.as_str()) {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        // A parse failure here means the tree doesn't compile at all — `cargo
        // build`/`cargo test` itself will already fail loudly on that, so the
        // checker skips rather than duplicating that report.
        let Ok(file) = syn::parse_file(&source) else {
            continue;
        };
        let lines: Vec<&str> = source.lines().collect();
        let mut checker = Checker {
            file_rel: rel,
            lines: &lines,
            test_depth: 0,
            fn_stack: Vec::new(),
            in_rust_tool_impl: false,
            violations: Vec::new(),
        };
        checker.visit_file(&file);
        violations.extend(checker.violations);
    }

    violations.sort_by(|a, b| (&a.file, a.line, a.rule.id()).cmp(&(&b.file, b.line, b.rule.id())));
    violations
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

enum Waiver {
    /// No `house-style-allow` marker on this or the previous line.
    None,
    /// Marker present with a non-empty reason after the colon — suppressed.
    Reasoned,
    /// Marker present but no (or empty) reason after the colon.
    Reasonless,
}

/// Looks for `house-style-allow` on the violation's own source line, then the
/// line above it (1-indexed `line`, matching `Violation::line` / span line
/// numbers). Mirrors `crate::github::pii`'s existing `pii-test-fixture`
/// line-exact convention, but additionally requires a non-empty reason.
fn waiver_status(lines: &[&str], line: usize) -> Waiver {
    for candidate in [line, line.saturating_sub(1)] {
        if candidate == 0 {
            continue;
        }
        let Some(text) = lines.get(candidate - 1) else { continue };
        let Some(idx) = text.find("house-style-allow") else { continue };
        let rest = text[idx + "house-style-allow".len()..].trim_start();
        return match rest.strip_prefix(':') {
            Some(reason) if !reason.trim().trim_end_matches("*/").trim().is_empty() => Waiver::Reasoned,
            _ => Waiver::Reasonless,
        };
    }
    Waiver::None
}

// ── test-context detection ──────────────────────────────────────────────────

/// True if `attrs` marks this item as test-only: `#[test]`, `#[tokio::test]`,
/// `#[async_std::test]` (matched by trailing path segment `test`), or
/// `#[cfg(test)]` / `#[cfg(all(test, ...))]` / `#[cfg(any(test, ...))]`.
fn has_test_context(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        if a.path().segments.last().map(|s| s.ident == "test").unwrap_or(false) {
            return true;
        }
        if a.path().is_ident("cfg") {
            if let Ok(meta) = a.parse_args::<syn::Meta>() {
                return meta_mentions_test(&meta);
            }
        }
        false
    })
}

fn meta_mentions_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::List(l) => {
            if let Ok(nested) =
                l.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            {
                nested.iter().any(meta_mentions_test)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn line_of(span: Span) -> usize {
    span.start().line
}

// ── the visitor ──────────────────────────────────────────────────────────────

struct Checker<'s> {
    file_rel: String,
    lines: &'s [&'s str],
    /// >0 while inside any `#[cfg(test)]`/`#[test]`-marked item. Rules 1/4
    /// never fire in test code (fixtures, mocks, and assertion helpers are
    /// not production secret handling or user-facing tool execution).
    test_depth: u32,
    /// Innermost enclosing named-function stack (free fns and impl methods).
    fn_stack: Vec<String>,
    /// True while visiting the body of an `impl RustTool for X` block.
    in_rust_tool_impl: bool,
    violations: Vec<Violation>,
}

impl<'s> Checker<'s> {
    fn in_execute(&self) -> bool {
        matches!(self.fn_stack.last().map(String::as_str), Some("execute") | Some("execute_structured"))
    }

    fn record(&mut self, rule: Rule, line: usize, message: String, help: String) {
        match waiver_status(self.lines, line) {
            Waiver::Reasoned => {}
            Waiver::Reasonless => {
                self.violations.push(Violation {
                    file: PathBuf::from(self.file_rel.as_str()),
                    line,
                    rule: Rule::ReasonlessWaiver,
                    message: format!(
                        "`// house-style-allow` waiver near this line has no reason after the colon \
                         (original finding, still live: {message})"
                    ),
                    help: "give the waiver a reason: `// house-style-allow: <why this is OK>` — a bare \
                           `// house-style-allow` (no colon, or an empty reason) is itself a violation"
                        .to_string(),
                });
            }
            Waiver::None => {
                self.violations.push(Violation { file: PathBuf::from(self.file_rel.as_str()), line, rule, message, help });
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
                let is_env_var = path.segments.len() >= 2
                    && path.segments[path.segments.len() - 1].ident == "var"
                    && path.segments[path.segments.len() - 2].ident == "env";
                if is_env_var {
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
}
