//! DOCGEN-19: Staleness/drift detection with token-anchored code references
//! (S111, Plane TERM-170). Extends DOCGEN-10's mismatch detector
//! (`super::mismatch`) with a second, independent drift-detection loop:
//! rather than adjudicating a CONTENT disagreement between doc and code,
//! this module anchors a doc snippet to the exact CODE LOCATION it
//! describes (`{file, symbol, line-hash}`) and re-resolves that anchor
//! against a later commit's read-only worktree (`crate::scribe::inspect`,
//! SCRB-03) to catch silent staleness before it ships -- Swimm's anchor
//! model, applied to Terminus's own doc engine.
//!
//! ## Why a separate detector, not a mismatch-detector extension
//! DOCGEN-10 adjudicates a *semantic* contradiction between two texts (a
//! contract vs. observed behavior) via a 5-provider panel -- expensive, and
//! reserved for genuine content disagreement. DOCGEN-19 instead answers a
//! much cheaper, purely structural question: "does the code this snippet
//! pointed at still look like what the snippet described?" -- a line-hash +
//! symbol-declaration re-scan, no model call, no panel. The two are
//! complementary layers (structural drift vs. semantic contradiction), not
//! a replacement for one another, so this stays its own module rather than
//! growing `mismatch.rs`'s scope.
//!
//! ## Reuse (do not reimplement)
//!   - `crate::scribe::inspect::{checkout, cleanup}` -- the SAME read-only,
//!     structurally-write-incapable worktree checkout DOCGEN-10's sibling
//!     module and Scribe itself use. This module never shells out to git on
//!     its own.
//!   - The token-overlap similarity heuristic mirrors `mismatch.rs`'s
//!     `normalize_tokens`/`jaccard_similarity` (same rationale: cheap,
//!     dependency-free, conservative). Reimplemented locally rather than
//!     imported because `mismatch.rs`'s versions are private to that module
//!     and this module's threshold/semantics differ (signature-similarity
//!     for a single anchored line, not whole-text contract-contradiction).
//!
//! ## Pure Rust, no external service
//! Symbol resolution is a line-scan for a declaration keyword
//! (`fn`/`struct`/`enum`/`trait`/`type`/`const`/`static`/`mod`) immediately
//! followed by the symbol name, mirroring `scribe::inspect::extract_excerpt`'s
//! existing "good enough for a documentation-context bundle, not for
//! codegen" line-scan philosophy. `tree-sitter`/`tree-sitter-rust` are real
//! workspace dependencies (KGRAPH-02's Atlas extractor) but are deliberately
//! NOT used here: a full AST parse is unnecessary weight for "did this one
//! anchored line move or change", and adding a second, heavier resolution
//! path for the same file class Atlas already parses would duplicate
//! capability rather than reuse it. A future item needing AST-precision
//! resolution should extend Atlas's extractor, not this module.
//!
//! ## Non-blocking (mirrors DOCGEN-10 / SCRB-04's own contract)
//! A drift-check failure (checkout failure, unreadable file) is always a
//! clean `Err` result for THIS tool call, never a panic -- and the caller
//! wiring this into the merge pipeline (DOCGEN-08+) MUST treat that `Err`
//! as non-fatal to the surrounding feat, exactly like `mismatch.rs`'s own
//! documented contract at its `execute()` boundary. A per-anchor
//! significant-drift finding is NEVER an `Err` on its own -- it is reported
//! as a WARNING entry in the successful JSON result (see
//! [`DriftResolution::SignificantDrift`] and [`DocgenDriftCheck::execute`]),
//! so a stale anchor is always visible, never silently dropped, and never
//! fails the surrounding feat by itself.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::inspect::{checkout, cleanup};
use crate::scribe::{is_repo_path_allowed, ScribeConfig};
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Anchors
// ---------------------------------------------------------------------------

/// A doc snippet's anchor to the exact code location it describes. The
/// three fields the spec names as the persisted contract are `file`,
/// `symbol`, and `line_hash`; `line_number` and `snapshot` are carried
/// alongside so THIS module's own resolution logic (classifying a change as
/// trivial vs. significant) is self-contained without a second worktree
/// read of the anchor-time content. A caller persisting an anchor
/// externally (frontmatter/sidecar, per the spec) may omit `snapshot` --
/// resolution degrades gracefully to "any hash mismatch is significant"
/// without it (see [`resolve_anchor`]'s doc comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeAnchor {
    pub file: String,
    pub symbol: String,
    pub line_number: usize,
    pub line_hash: String,
    #[serde(default)]
    pub snapshot: String,
}

/// Deterministic, dependency-free content hash for one line of code. Not
/// cryptographic -- this is a staleness fingerprint, not a security
/// boundary, so `DefaultHasher` (SipHash) is sufficient and avoids pulling
/// in a checksum crate for a single-line hash.
pub fn hash_line(line: &str) -> String {
    let mut hasher = DefaultHasher::new();
    line.trim().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// True when `line` contains a declaration of `symbol` -- a keyword
/// (`fn`/`struct`/`enum`/`trait`/`type`/`const`/`static`/`mod`) immediately
/// followed by whitespace and the symbol name, with a non-identifier
/// character (or end of line) immediately after the symbol. This
/// deliberately matches "fn foo(" / "struct Foo" / "pub async fn foo("
/// (any visibility/async/etc. modifier before the keyword) while rejecting
/// a mere call site like `foo()` or `return foo();` (no declaration keyword
/// immediately precedes it) and a symbol that is merely a prefix of a
/// longer identifier (`foo_helper`).
fn line_declares_symbol(line: &str, symbol: &str) -> bool {
    if symbol.is_empty() {
        return false;
    }
    const KEYWORDS: &[&str] = &["fn", "struct", "enum", "trait", "type", "const", "static", "mod"];
    let bytes = line.as_bytes();
    for kw in KEYWORDS {
        let pattern = format!("{kw} {symbol}");
        let mut search_from = 0usize;
        while let Some(rel) = line[search_from..].find(pattern.as_str()) {
            let idx = search_from + rel;
            let after = idx + pattern.len();
            let boundary_ok = match bytes.get(after) {
                None => true,
                Some(&b) => !(b as char).is_alphanumeric() && b != b'_',
            };
            if boundary_ok {
                return true;
            }
            search_from = idx + 1;
        }
    }
    false
}

/// Scan `content` for the first line declaring `symbol`, returning its
/// 1-based line number and trimmed text.
fn find_symbol_line(content: &str, symbol: &str) -> Option<(usize, String)> {
    for (i, line) in content.lines().enumerate() {
        if line_declares_symbol(line, symbol) {
            return Some((i + 1, line.trim().to_string()));
        }
    }
    None
}

/// Record a fresh anchor for `symbol` inside `file_rel` (repo-relative path)
/// as it exists in `worktree_root` at `content` -- the doc-generation-time
/// snapshot. Returns [`ToolError::NotFound`] if the file can't be read or
/// the symbol has no declaration line in it (a doc snippet must not anchor
/// to a symbol that doesn't exist yet).
pub fn record_anchor(file_rel: &str, content: &str, symbol: &str) -> Result<CodeAnchor, ToolError> {
    let (line_number, snapshot) = find_symbol_line(content, symbol).ok_or_else(|| {
        ToolError::NotFound(format!(
            "symbol '{symbol}' has no declaration line in '{file_rel}'"
        ))
    })?;
    Ok(CodeAnchor {
        file: file_rel.to_string(),
        symbol: symbol.to_string(),
        line_number,
        line_hash: hash_line(&snapshot),
        snapshot,
    })
}

// ---------------------------------------------------------------------------
// Resolution: trivial auto-patch vs. significant drift
// ---------------------------------------------------------------------------

/// Above this token-overlap ratio between the anchor's original snapshot
/// line and the newly re-scanned declaration line, a changed line is
/// considered a TRIVIAL edit (rename of a parameter, a shifted line number,
/// a reordered modifier) worth auto-patching rather than flagging. Chosen
/// deliberately lower than `mismatch.rs`'s `PHRASING_SIMILARITY_THRESHOLD`
/// (0.7, for whole-paragraph contradiction detection): a single code
/// declaration line is short, so even a genuinely meaningful signature
/// change (an added/removed parameter, a changed return type) still shares
/// most of its tokens (`pub`, `fn`, the symbol name, punctuation) with the
/// original -- 0.5 is the conservative cut observed to still separate
/// "same shape, minor edit" from "materially different signature" on
/// realistic Rust declaration lines (see this module's tests).
const SIGNATURE_SIMILARITY_THRESHOLD: f64 = 0.5;

fn normalize_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

fn jaccard_similarity(a: &[String], b: &[String]) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let intersection = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// The outcome of re-resolving one [`CodeAnchor`] against a later worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftResolution {
    /// The anchored line is byte-for-byte (modulo surrounding whitespace)
    /// identical, at the same line number. No action needed.
    Unchanged,
    /// The symbol still resolves and the change (a pure line-shift, or a
    /// line edit whose tokens still overlap heavily with the original) is
    /// classified trivial -- `patched` is the updated anchor, ready to
    /// silently replace the stale one.
    TrivialPatch { patched: CodeAnchor },
    /// Either the symbol no longer has a declaration line anywhere in the
    /// file (symbol gone / file removed), or it does but the declaration
    /// line changed enough that this module will not auto-patch it. This
    /// is always surfaced as a WARNING to the caller -- never silent, never
    /// auto-corrected.
    SignificantDrift { reason: String },
}

/// Re-resolve `anchor` against `content` (the current text of
/// `anchor.file`, already read from a read-only inspection worktree by the
/// caller -- see [`DocgenDriftCheck`]). Pure and side-effect-free so it is
/// exhaustively unit-testable without a real git checkout.
///
/// Classification:
///   - Symbol not found at all -> [`DriftResolution::SignificantDrift`]
///     ("symbol gone").
///   - Same line number AND same `line_hash` -> [`DriftResolution::Unchanged`].
///   - `line_hash` differs (line moved and/or its text changed): if
///     `anchor.snapshot` is available, compare token overlap against the
///     freshly re-scanned line via [`jaccard_similarity`] --
///     >= [`SIGNATURE_SIMILARITY_THRESHOLD`] auto-patches
///     ([`DriftResolution::TrivialPatch`]), below it is
///     [`DriftResolution::SignificantDrift`] ("body/signature changed
///     materially"). If `anchor.snapshot` is empty (a caller that dropped
///     it after persisting only the `{file,symbol,line-hash}` triple), any
///     hash mismatch is conservatively significant -- there is no prior
///     text left to compare against.
pub fn resolve_anchor(anchor: &CodeAnchor, content: &str) -> DriftResolution {
    let Some((new_line_number, new_snapshot)) = find_symbol_line(content, &anchor.symbol) else {
        return DriftResolution::SignificantDrift {
            reason: format!(
                "symbol '{}' no longer has a declaration line in '{}'",
                anchor.symbol, anchor.file
            ),
        };
    };

    let new_hash = hash_line(&new_snapshot);

    if new_hash == anchor.line_hash && new_line_number == anchor.line_number {
        return DriftResolution::Unchanged;
    }

    if new_hash == anchor.line_hash {
        // Pure line-shift: identical content, different position. Always
        // trivial -- nothing about the declaration itself changed.
        return DriftResolution::TrivialPatch {
            patched: CodeAnchor {
                file: anchor.file.clone(),
                symbol: anchor.symbol.clone(),
                line_number: new_line_number,
                line_hash: new_hash,
                snapshot: new_snapshot,
            },
        };
    }

    if anchor.snapshot.is_empty() {
        return DriftResolution::SignificantDrift {
            reason: format!(
                "line-hash changed for symbol '{}' in '{}' and no prior snapshot was available to \
classify the change as trivial",
                anchor.symbol, anchor.file
            ),
        };
    }

    let similarity = jaccard_similarity(
        &normalize_tokens(&anchor.snapshot),
        &normalize_tokens(&new_snapshot),
    );

    if similarity >= SIGNATURE_SIMILARITY_THRESHOLD {
        DriftResolution::TrivialPatch {
            patched: CodeAnchor {
                file: anchor.file.clone(),
                symbol: anchor.symbol.clone(),
                line_number: new_line_number,
                line_hash: new_hash,
                snapshot: new_snapshot,
            },
        }
    } else {
        DriftResolution::SignificantDrift {
            reason: format!(
                "declaration line for symbol '{}' in '{}' changed materially (token overlap {:.2} < {SIGNATURE_SIMILARITY_THRESHOLD:.2})",
                anchor.symbol, anchor.file, similarity
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: docgen_drift_check
// ---------------------------------------------------------------------------

/// One anchor's resolution outcome, serialized for the tool's JSON result.
#[derive(Debug, Clone, Serialize)]
struct AnchorReport {
    file: String,
    symbol: String,
    status: &'static str,
    detail: Option<String>,
    patched_anchor: Option<CodeAnchor>,
}

pub struct DocgenDriftCheck;

#[async_trait]
impl RustTool for DocgenDriftCheck {
    fn name(&self) -> &str {
        "docgen_drift_check"
    }

    fn description(&self) -> &str {
        "Staleness/drift detector for token-anchored doc snippets (extends DOCGEN-10). \
Re-resolves a set of {file, symbol, line_hash} anchors recorded at doc-generation time \
against a read-only worktree checkout of a later git ref. Trivial changes (a pure \
line-shift, or an edit whose tokens still overlap heavily with the original -- a \
rename/param edit) are auto-patched and returned as updated anchors. Significant \
changes (the symbol's declaration is gone, or the line changed enough that this \
module will not guess) are reported as WARNINGS, never silently dropped and never \
auto-corrected. A per-anchor warning never fails the call; only an infrastructure \
failure (bad repo path/ref, unreadable file) does, and that failure is documented as \
non-fatal to the caller's surrounding feat/doc-gen, mirroring docgen_mismatch_detect."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo_path": {
                    "type": "string",
                    "description": "Filesystem path to the git repository to check anchors against. Defaults to SCRIBE_REPO_PATH if unset; must resolve under SCRIBE_ALLOWED_REPO_ROOTS."
                },
                "git_ref": {
                    "type": "string",
                    "description": "Git ref (branch or SHA) to re-resolve anchors against, e.g. \"main\""
                },
                "anchors": {
                    "type": "array",
                    "description": "Anchors recorded at doc-generation time",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {"type": "string"},
                            "symbol": {"type": "string"},
                            "line_number": {"type": "integer"},
                            "line_hash": {"type": "string"},
                            "snapshot": {"type": "string"}
                        },
                        "required": ["file", "symbol", "line_number", "line_hash"]
                    }
                }
            },
            "required": ["git_ref", "anchors"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let git_ref = args
            .get("git_ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("git_ref is required and must not be empty".into()))?;
        let anchors_json = args
            .get("anchors")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidArgument("anchors must be a non-empty array".into()))?;
        if anchors_json.is_empty() {
            return Err(ToolError::InvalidArgument("anchors must not be empty".into()));
        }

        let mut anchors = Vec::with_capacity(anchors_json.len());
        for a in anchors_json {
            let anchor: CodeAnchor = serde_json::from_value(a.clone())
                .map_err(|e| ToolError::InvalidArgument(format!("invalid anchor entry: {e}")))?;
            anchors.push(anchor);
        }

        // Same subprocess-inspection gate + repo-path allowlist confinement
        // as `scribe_generate_readme` (SCRB-02/03) -- this tool reuses the
        // exact same `inspect::checkout` call, so it inherits the exact
        // same interim-contract-deviation gate (see `ScribeConfig::
        // allow_subprocess_inspection`'s doc comment) and the exact same
        // default-deny confinement (`is_repo_path_allowed`), rather than
        // opening a second, ungated path to the same subprocess call.
        let cfg = ScribeConfig::from_env();

        if !cfg.allow_subprocess_inspection {
            return Err(ToolError::NotConfigured(
                "subprocess-based worktree inspection is disabled by default (see \
ScribeConfig::allow_subprocess_inspection's doc comment for why); set \
SCRIBE_ALLOW_SUBPROCESS_INSPECTION=true to enable it explicitly"
                    .into(),
            ));
        }

        let repo_path_str = args
            .get("repo_path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .or(cfg.repo_path.clone())
            .ok_or_else(|| {
                ToolError::NotConfigured(
                    "no repo_path argument given and SCRIBE_REPO_PATH is not set".into(),
                )
            })?;

        let repo_path = Path::new(&repo_path_str);
        if !is_repo_path_allowed(repo_path, &cfg.allowed_repo_roots) {
            return Err(ToolError::InvalidArgument(format!(
                "repo_path '{}' is not under any root in SCRIBE_ALLOWED_REPO_ROOTS \
(default-deny: this env var must list the specific repos this tool may inspect)",
                repo_path.display()
            )));
        }

        let worktree_root = Path::new(&cfg.worktree_root);
        let wt = checkout(repo_path, git_ref, worktree_root)?;

        let mut reports = Vec::with_capacity(anchors.len());
        let mut warning_count = 0usize;

        for anchor in &anchors {
            let full_path = wt.path.join(&anchor.file);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(_) => {
                    warning_count += 1;
                    reports.push(AnchorReport {
                        file: anchor.file.clone(),
                        symbol: anchor.symbol.clone(),
                        status: "significant_drift",
                        detail: Some(format!("file '{}' no longer exists at ref '{git_ref}'", anchor.file)),
                        patched_anchor: None,
                    });
                    continue;
                }
            };

            match resolve_anchor(anchor, &content) {
                DriftResolution::Unchanged => reports.push(AnchorReport {
                    file: anchor.file.clone(),
                    symbol: anchor.symbol.clone(),
                    status: "unchanged",
                    detail: None,
                    patched_anchor: None,
                }),
                DriftResolution::TrivialPatch { patched } => reports.push(AnchorReport {
                    file: anchor.file.clone(),
                    symbol: anchor.symbol.clone(),
                    status: "trivial_patch",
                    detail: None,
                    patched_anchor: Some(patched),
                }),
                DriftResolution::SignificantDrift { reason } => {
                    warning_count += 1;
                    reports.push(AnchorReport {
                        file: anchor.file.clone(),
                        symbol: anchor.symbol.clone(),
                        status: "significant_drift",
                        detail: Some(reason),
                        patched_anchor: None,
                    });
                }
            }
        }

        let _ = cleanup(&wt);

        Ok(serde_json::to_string_pretty(&json!({
            "git_ref": git_ref,
            "anchor_count": anchors.len(),
            "warning_count": warning_count,
            "reports": reports,
        }))
        .unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenDriftCheck));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── hash_line ───────────────────────────────────────────────────────

    #[test]
    fn hash_line_is_deterministic_and_whitespace_insensitive() {
        assert_eq!(hash_line("pub fn foo(x: i32) {"), hash_line("  pub fn foo(x: i32) {  "));
    }

    #[test]
    fn hash_line_differs_for_different_content() {
        assert_ne!(hash_line("pub fn foo() {"), hash_line("pub fn bar() {"));
    }

    // ─── line_declares_symbol / find_symbol_line ────────────────────────

    #[test]
    fn declares_matches_plain_and_modified_fn() {
        assert!(line_declares_symbol("fn foo() {", "foo"));
        assert!(line_declares_symbol("pub fn foo(x: i32) -> bool {", "foo"));
        assert!(line_declares_symbol("pub async fn foo() {", "foo"));
    }

    #[test]
    fn declares_matches_struct_enum_trait() {
        assert!(line_declares_symbol("pub struct Foo {", "Foo"));
        assert!(line_declares_symbol("pub enum Bar {", "Bar"));
        assert!(line_declares_symbol("pub trait Baz {", "Baz"));
    }

    #[test]
    fn declares_rejects_mere_call_site() {
        // Negative test: a call site is not a declaration.
        assert!(!line_declares_symbol("    let x = foo();", "foo"));
        assert!(!line_declares_symbol("    return foo(a, b);", "foo"));
    }

    #[test]
    fn declares_rejects_prefix_collision() {
        // Negative test: "foo_helper" must not match a search for "foo".
        assert!(!line_declares_symbol("fn foo_helper() {", "foo"));
        assert!(!line_declares_symbol("struct FooBar {", "Foo"));
    }

    #[test]
    fn find_symbol_line_returns_first_match_with_1_based_line_number() {
        let content = "// header\nfn other() {}\npub fn target() {\n    1\n}\n";
        let (line_no, text) = find_symbol_line(content, "target").unwrap();
        assert_eq!(line_no, 3);
        assert_eq!(text, "pub fn target() {");
    }

    #[test]
    fn find_symbol_line_none_when_absent() {
        assert!(find_symbol_line("fn other() {}\n", "target").is_none());
    }

    // ─── record_anchor ───────────────────────────────────────────────────

    #[test]
    fn record_anchor_captures_file_symbol_and_line_hash() {
        let content = "pub struct Widget {\n    pub id: u64,\n}\n";
        let anchor = record_anchor("src/widget.rs", content, "Widget").unwrap();
        assert_eq!(anchor.file, "src/widget.rs");
        assert_eq!(anchor.symbol, "Widget");
        assert_eq!(anchor.line_number, 1);
        assert_eq!(anchor.line_hash, hash_line("pub struct Widget {"));
        assert_eq!(anchor.snapshot, "pub struct Widget {");
    }

    #[test]
    fn record_anchor_missing_symbol_is_not_found_error() {
        let result = record_anchor("src/widget.rs", "fn other() {}\n", "Widget");
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    // ─── resolve_anchor: unchanged ───────────────────────────────────────

    #[test]
    fn resolve_unchanged_when_line_and_number_identical() {
        let content = "pub fn foo() {\n    1\n}\n";
        let anchor = record_anchor("src/x.rs", content, "foo").unwrap();
        assert_eq!(resolve_anchor(&anchor, content), DriftResolution::Unchanged);
    }

    // ─── resolve_anchor: trivial -- pure line shift ─────────────────────

    #[test]
    fn resolve_trivial_patch_on_pure_line_shift() {
        let original = "pub fn foo() {\n    1\n}\n";
        let anchor = record_anchor("src/x.rs", original, "foo").unwrap();
        assert_eq!(anchor.line_number, 1);

        // Same declaration line, but pushed down by two lines of new
        // content above it -- content identical, only position changed.
        let shifted = "// new comment\n// another comment\npub fn foo() {\n    1\n}\n";
        match resolve_anchor(&anchor, shifted) {
            DriftResolution::TrivialPatch { patched } => {
                assert_eq!(patched.line_number, 3);
                assert_eq!(patched.line_hash, anchor.line_hash);
            }
            other => panic!("expected TrivialPatch, got {other:?}"),
        }
    }

    // ─── resolve_anchor: trivial -- param rename ────────────────────────

    #[test]
    fn resolve_trivial_patch_on_param_rename() {
        let original = "pub fn foo(old_name: i32) -> bool {\n    true\n}\n";
        let anchor = record_anchor("src/x.rs", original, "foo").unwrap();

        let renamed = "pub fn foo(new_name: i32) -> bool {\n    true\n}\n";
        match resolve_anchor(&anchor, renamed) {
            DriftResolution::TrivialPatch { patched } => {
                assert!(patched.snapshot.contains("new_name"));
            }
            other => panic!("expected TrivialPatch, got {other:?}"),
        }
    }

    // ─── resolve_anchor: significant -- symbol gone (negative test) ────

    #[test]
    fn resolve_significant_drift_when_symbol_removed() {
        let original = "pub fn foo() {\n    1\n}\n";
        let anchor = record_anchor("src/x.rs", original, "foo").unwrap();

        let removed = "// foo was deleted entirely\nfn unrelated() {}\n";
        match resolve_anchor(&anchor, removed) {
            DriftResolution::SignificantDrift { reason } => {
                assert!(reason.contains("no longer has a declaration"));
            }
            other => panic!("expected SignificantDrift, got {other:?}"),
        }
    }

    // ─── resolve_anchor: significant -- body/signature changed materially
    //     (negative test: NOT silently auto-patched) ──────────────────────

    #[test]
    fn resolve_significant_drift_on_materially_changed_signature() {
        let original = "pub fn foo(a: i32) -> bool {\n    true\n}\n";
        let anchor = record_anchor("src/x.rs", original, "foo").unwrap();

        // Same symbol name, but the rest of the declaration line is
        // unrecognizable relative to the original -- must be flagged, not
        // silently patched.
        let changed = "pub async unsafe fn foo<T: Clone + Send + 'static>(x: T, y: T, z: T) -> Result<Option<Vec<T>>, std::io::Error> {\n    unimplemented!()\n}\n";
        match resolve_anchor(&anchor, changed) {
            DriftResolution::SignificantDrift { reason } => {
                assert!(reason.contains("changed materially"));
            }
            other => panic!("expected SignificantDrift, got {other:?}"),
        }
    }

    #[test]
    fn resolve_without_snapshot_any_change_is_conservatively_significant() {
        let anchor = CodeAnchor {
            file: "src/x.rs".to_string(),
            symbol: "foo".to_string(),
            line_number: 1,
            line_hash: hash_line("pub fn foo(a: i32) {"),
            snapshot: String::new(),
        };
        let changed = "pub fn foo(a: i32, b: i32) {\n    1\n}\n";
        match resolve_anchor(&anchor, changed) {
            DriftResolution::SignificantDrift { reason } => {
                assert!(reason.contains("no prior snapshot"));
            }
            other => panic!("expected SignificantDrift, got {other:?}"),
        }
    }

    // ─── resolve_anchor: file removed entirely (covered at the tool layer,
    //     but the pure resolve_anchor fn only ever sees content it was
    //     given -- this documents that boundary explicitly). ─────────────

    #[test]
    fn resolve_treats_empty_content_as_symbol_gone() {
        let original = "pub fn foo() {}\n";
        let anchor = record_anchor("src/x.rs", original, "foo").unwrap();
        match resolve_anchor(&anchor, "") {
            DriftResolution::SignificantDrift { .. } => {}
            other => panic!("expected SignificantDrift, got {other:?}"),
        }
    }

    // ─── Tool: input validation ──────────────────────────────────────────

    #[tokio::test]
    async fn tool_missing_required_field_is_invalid_argument() {
        let tool = DocgenDriftCheck;
        let result = tool.execute(json!({"repo_path": "/tmp/x", "git_ref": "main"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn tool_empty_anchors_is_invalid_argument() {
        let tool = DocgenDriftCheck;
        let result = tool
            .execute(json!({"repo_path": "/tmp/x", "git_ref": "main", "anchors": []}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn tool_malformed_anchor_is_invalid_argument() {
        let tool = DocgenDriftCheck;
        let result = tool
            .execute(json!({
                "repo_path": "/tmp/x",
                "git_ref": "main",
                "anchors": [{"file": "src/x.rs"}]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    /// Non-blocking contract, negative-tested at the tool boundary: with
    /// `SCRIBE_ALLOW_SUBPROCESS_INSPECTION` unset (the default in this test
    /// process, matching every other test in this suite -- see
    /// `scribe::mod`'s own
    /// `generate_readme_execute_is_disabled_by_default_pending_operator_optin`
    /// for the identical pattern this tool intentionally reuses), the tool
    /// returns a clean `NotConfigured`, never a panic, and never actually
    /// attempts the subprocess checkout. This is the infra-gate path; a
    /// live checkout-failure (bad repo path with the opt-in enabled) would
    /// need a real opt-in + repo on disk to test meaningfully and is
    /// exercised indirectly via `scribe::inspect`'s own
    /// `checkout_of_nonexistent_repo_is_a_clean_error_not_a_panic` (the
    /// exact same `inspect::checkout` call this tool makes). Per-anchor
    /// drift findings (tested above via `resolve_anchor` directly) NEVER
    /// take the `Err` path at all -- they are always a successful result
    /// with warnings.
    #[tokio::test]
    #[serial_test::serial]
    async fn tool_disabled_by_default_pending_operator_optin_is_a_clean_error_not_panic() {
        std::env::remove_var("SCRIBE_ALLOW_SUBPROCESS_INSPECTION");
        let tool = DocgenDriftCheck;
        let result = tool
            .execute(json!({
                "repo_path": "/tmp/docgen-drift-test-nonexistent-repo-xyz",
                "git_ref": "main",
                "anchors": [{
                    "file": "src/x.rs",
                    "symbol": "foo",
                    "line_number": 1,
                    "line_hash": "deadbeef",
                    "snapshot": "pub fn foo() {"
                }]
            }))
            .await;
        match result {
            Err(ToolError::NotConfigured(msg)) => {
                assert!(msg.contains("SCRIBE_ALLOW_SUBPROCESS_INSPECTION"));
            }
            other => panic!("expected NotConfigured pending opt-in, got: {other:?}"),
        }
    }

    #[test]
    fn registers_expected_tool() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("docgen_drift_check"));
    }

    #[test]
    fn tool_has_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for info in reg.list() {
            assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
        }
    }

    // ─── CodeAnchor (de)serialization round-trip: the persisted contract ──

    #[test]
    fn code_anchor_serializes_with_the_spec_named_fields() {
        let anchor = CodeAnchor {
            file: "src/x.rs".to_string(),
            symbol: "foo".to_string(),
            line_number: 5,
            line_hash: "abc123".to_string(),
            snapshot: "pub fn foo() {".to_string(),
        };
        let v = serde_json::to_value(&anchor).unwrap();
        assert_eq!(v["file"], json!("src/x.rs"));
        assert_eq!(v["symbol"], json!("foo"));
        assert_eq!(v["line_hash"], json!("abc123"));
    }

    #[test]
    fn code_anchor_deserializes_without_snapshot_field() {
        // A caller that persisted only the {file,symbol,line-hash} triple
        // (per the spec's minimal contract) must still deserialize -- the
        // fourth field degrades to the empty-snapshot conservative path.
        let v = json!({
            "file": "src/x.rs",
            "symbol": "foo",
            "line_number": 1,
            "line_hash": "abc123"
        });
        let anchor: CodeAnchor = serde_json::from_value(v).unwrap();
        assert_eq!(anchor.snapshot, "");
    }
}
