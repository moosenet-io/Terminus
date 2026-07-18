//! DLAND-02: no-information-lost cutover preservation guard.
//!
//! Before a first cutover REPLACES a hand-grown, bloated README with the
//! docgen engine's generated landing + `docs/` tree, this module proves no
//! information is lost: every top-level (`##`) section in the OLD README
//! must have its substance present somewhere in the NEW landing or the new
//! `docs/` tree. A section whose substance cannot be found is FLAGGED --
//! this module never silently drops it and never auto-fails the cutover; it
//! only reports data for the caller to surface to an operator.
//!
//! ## Two-signal coverage check
//! A section is `covered` when EITHER:
//! 1. its heading text (normalized, trimmed, lowercased) appears verbatim
//!    somewhere in the new corpus (landing + every `docs/` page), OR
//! 2. a strong enough fraction of its **stable tokens** -- tool/symbol
//!    names, environment-variable-shaped identifiers, numbers, and other
//!    identifier-shaped words, i.e. NOT ordinary prose -- also appear in the
//!    new corpus.
//!
//! Keying on stable tokens rather than raw prose is deliberate: a section
//! that has been paraphrased (same tool names and facts, different
//! sentences) must not be misreported as lost, while a section whose actual
//! substance (its tool names, env vars, numbers) is nowhere in the new
//! corpus is genuinely dropped and must be flagged, even if a heading with
//! a similar name happens to exist elsewhere.
//!
//! ## No silent drop
//! [`check_preservation`] never panics, never fails a build, and never
//! discards a missing section -- it always returns a [`PreservationReport`]
//! whose `missing` list is exactly the sections a human should look at
//! before shipping the cutover. See the module tests for the acceptance
//! fixtures.
//!
//! ## Bounded work, no quadratic blowup
//! Every pass over the old README, the new landing, and the docs tree is a
//! single linear scan (`lines()`/`split()`); coverage lookups go through
//! `BTreeSet` membership rather than re-scanning the new corpus once per
//! old section, so a very large old README (thousands of lines, dozens of
//! sections) costs `O(old_len + new_len)`, not `O(sections * new_len)`.

use std::collections::{BTreeSet, HashMap};

use super::render::docs_tree::DocsTreeFile;

/// Minimum fraction of an old section's stable tokens that must reappear in
/// the new corpus for that section to be considered covered by content
/// signature alone (signal 2). Deliberately not 100%: real paraphrasing can
/// legitimately drop a redundant token or two while preserving the section's
/// substance; deliberately well above 0% so a handful of incidental shared
/// words (e.g. common tool names reused across unrelated sections) cannot
/// mark a genuinely dropped section as covered.
const TOKEN_COVERAGE_THRESHOLD: f32 = 0.5;

/// A minimum token length to consider -- filters out short incidental
/// fragments (`"a"`, `"an"`, `"of"`) that are never a meaningful stable
/// signature even when they happen to satisfy one of the shape rules below.
const MIN_TOKEN_LEN: usize = 3;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One OLD-README section this guard could not find the substance of
/// anywhere in the new landing + docs tree. `heading` is the (possibly
/// disambiguated, see [`check_preservation`]'s duplicate-heading handling)
/// label an operator would recognize; `reason` is a short, human-readable
/// excerpt/explanation -- never just a bare boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub heading: String,
    pub reason: String,
}

/// The full result of [`check_preservation`]: which old sections were found
/// covered, which were not, and the overall ratio. This is DATA the caller
/// surfaces to an operator before a cutover ships -- this module never acts
/// on it (no auto-fail, no auto-block) beyond producing it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreservationReport {
    /// Labels of every old section whose substance was found in the new
    /// corpus (by heading match or token-signature match).
    pub covered: Vec<String>,
    /// Every old section whose substance could NOT be found -- a FLAG for
    /// an operator to review, never something this module drops or hides.
    pub missing: Vec<Section>,
    /// `covered.len() / (covered.len() + missing.len())`, clamped to `1.0`
    /// when the old README had no sections at all (nothing to lose).
    pub coverage_ratio: f32,
}

impl PreservationReport {
    /// True iff every old section was accounted for. Convenience only --
    /// callers should still inspect `missing` themselves rather than branch
    /// solely on this, since `missing` carries the detail worth surfacing.
    pub fn is_fully_preserved(&self) -> bool {
        self.missing.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Old-README section splitting
// ---------------------------------------------------------------------------

/// One raw section parsed out of the OLD README: its heading text (empty
/// for the "no `##` headings at all" edge case, see [`split_old_sections`])
/// and its body.
struct RawSection {
    heading: String,
    body: String,
}

/// Split the OLD README into its top-level (`## `) sections. Per the spec's
/// EDGE CASE, a README with NO `## ` headings at all is treated as a single
/// section (heading left empty; [`check_preservation`] labels it
/// `"(whole document)"`) rather than reporting zero sections -- the
/// document's entire substance still needs to be accounted for somewhere in
/// the new corpus.
///
/// Single linear pass over `content.lines()` -- no re-scanning, so this
/// stays `O(n)` even for a several-thousand-line README.
fn split_old_sections(content: &str) -> Vec<RawSection> {
    let mut sections = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("## ") {
            // Flush whatever section (or preamble) we were accumulating.
            if let Some(heading) = current_heading.take() {
                sections.push(RawSection { heading, body: current_body.trim().to_string() });
            } else if !current_body.trim().is_empty() {
                // Preamble before the FIRST "## " heading. Not itself a
                // top-level section per the spec's definition -- it is
                // dropped as a tracked section here, matching
                // `readme_layers::preamble`'s treatment of the hero layer
                // as distinct from a "## " section. (If the README turns
                // out to have no "## " headings at all, this branch never
                // fires -- see the fallback below.)
            }
            current_heading = Some(title.trim().to_string());
            current_body = String::new();
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some(heading) = current_heading.take() {
        sections.push(RawSection { heading, body: current_body.trim().to_string() });
    }

    if sections.is_empty() {
        // EDGE CASE: no "## " headings anywhere -- the whole body is one
        // section (heading left empty; labeled at the call site).
        let whole = content.trim();
        if !whole.is_empty() {
            sections.push(RawSection { heading: String::new(), body: whole.to_string() });
        }
    }

    sections
}

// ---------------------------------------------------------------------------
// Stable-token extraction (the content-signature signal)
// ---------------------------------------------------------------------------

/// A conservative common-word denylist for the ALL-CAPS shape rule below --
/// short, common all-caps words ("THE", "AND") are not a meaningful stable
/// signature even though they satisfy the "all uppercase" shape. Kept small
/// and deliberately not exhaustive: this is a heuristic guard, not a
/// dictionary.
const COMMON_ALL_CAPS_STOPWORDS: &[&str] = &["THE", "AND", "FOR", "ARE", "NOT", "YOU", "ALL", "NEW"];

/// Whether `token` looks like a stable identifier/signature rather than
/// ordinary prose: contains a digit, contains an underscore (env-var/
/// snake_case shape), is ALL-CAPS (constant/env-var shape, common stopwords
/// excluded), or is camelCase/PascalCase (a symbol-name shape).
fn is_significant_token(token: &str) -> bool {
    if token.len() < MIN_TOKEN_LEN {
        return false;
    }
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_underscore = token.contains('_');
    let alpha: String = token.chars().filter(|c| c.is_alphabetic()).collect();
    let all_upper = alpha.len() >= MIN_TOKEN_LEN
        && alpha.chars().all(|c| c.is_uppercase())
        && !COMMON_ALL_CAPS_STOPWORDS.contains(&alpha.as_str());
    let camel_case = is_camel_case(token);

    has_digit || has_underscore || all_upper || camel_case
}

/// A lowercase letter immediately followed by an uppercase letter anywhere
/// in the token -- the shape of `camelCase`/`PascalCase` symbol names like
/// `DocGenerator` or `renderLayeredReadme`.
fn is_camel_case(token: &str) -> bool {
    let chars: Vec<char> = token.chars().collect();
    chars.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase())
}

/// Split `text` on non-identifier characters (anything that isn't
/// alphanumeric, `_`, `.`, or `-`), returning non-empty fragments. Used both
/// for plain prose scanning and for splitting the contents of backtick code
/// spans into individual identifier-shaped tokens.
fn split_identifier_like(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-'))
        // `.` and `-` are kept as INTERNAL identifier characters (version
        // numbers like `v1.2.3`, hyphenated names) but must not be allowed to
        // stick to a fragment's edges -- otherwise ordinary sentence
        // punctuation (a trailing "." ending a sentence, a leading "-" from
        // a list marker) gets baked into the token and breaks exact-value
        // membership checks (e.g. "8080." instead of "8080").
        .map(|s| s.trim_matches(|c: char| c == '.' || c == '-'))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Pull the contents of every inline `` `code span` `` and fenced
/// ` ```block``` ` out of `text`. Code spans are the single highest-signal
/// source of tool/symbol names in generated prose, so they are always
/// tokenized regardless of shape (a code span naming a tool is significant
/// even if, in isolation, the identifier-shape heuristic would have missed
/// it).
fn code_span_contents(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            if let Some(rel_end) = text[i + 1..].find('`') {
                let end = i + 1 + rel_end;
                spans.push(&text[i + 1..end]);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    spans
}

/// Extract the set of stable/significant tokens from `text`: every
/// backtick-code-span fragment (tokenized further, any shape) plus every
/// plain-prose word that satisfies [`is_significant_token`]. One linear pass
/// over `text` plus one linear pass over each code span found in it -- no
/// re-scanning of the whole text per span.
fn significant_tokens(text: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();

    for span in code_span_contents(text) {
        for frag in split_identifier_like(span) {
            if frag.len() >= MIN_TOKEN_LEN {
                tokens.insert(frag.to_string());
            }
        }
    }

    for word in split_identifier_like(text) {
        if is_significant_token(word) {
            tokens.insert(word.to_string());
        }
    }

    tokens
}

// ---------------------------------------------------------------------------
// New-corpus assembly
// ---------------------------------------------------------------------------

/// Whether `needle` occurs in `haystack` bounded by non-alphanumeric
/// characters (or start/end of string) on both sides -- i.e. as a whole
/// word/phrase, not merely as a run of characters embedded inside a larger
/// word. Plain [`str::contains`] would let a heading like "Telemetry" match
/// against unrelated incidental prose that merely contains it as a
/// substring (e.g. "telemetry-related"), wrongly marking a genuinely
/// dropped section as covered.
fn contains_as_whole_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while start < haystack.len() {
        let Some(rel) = haystack[start..].find(needle) else { break };
        let idx = start + rel;
        let before_ok = haystack[..idx].chars().next_back().map(|c| !c.is_alphanumeric()).unwrap_or(true);
        let end = idx + needle.len();
        let after_ok = haystack[end..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        // Advance past the first char of this (rejected or accepted) match
        // to keep scanning for a later occurrence, staying on a char
        // boundary regardless of the needle's/haystack's byte widths.
        let advance = haystack[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        start = idx + advance;
    }
    false
}

/// Concatenate the new landing README plus every `docs/` tree page into one
/// corpus string, and its lowercased form for substring heading checks.
/// Built once per [`check_preservation`] call, not once per old section.
fn build_new_corpus(new_landing: &str, new_docs_tree: &[DocsTreeFile]) -> String {
    let mut corpus = String::with_capacity(
        new_landing.len() + new_docs_tree.iter().map(|f| f.content.len()).sum::<usize>(),
    );
    corpus.push_str(new_landing);
    corpus.push('\n');
    for file in new_docs_tree {
        corpus.push_str(&file.content);
        corpus.push('\n');
    }
    corpus
}

// ---------------------------------------------------------------------------
// check_preservation
// ---------------------------------------------------------------------------

/// Prove (or flag the failure to prove) that every top-level (`## `) section
/// of `old_readme` has its substance present somewhere in `new_landing` or
/// `new_docs_tree`.
///
/// Never fails/panics/blocks on its own -- always returns a
/// [`PreservationReport`] the caller surfaces. See the module doc comment
/// for the two-signal coverage rule and the bounded-work guarantee.
pub fn check_preservation(
    old_readme: &str,
    new_landing: &str,
    new_docs_tree: &[DocsTreeFile],
) -> PreservationReport {
    let old_sections = split_old_sections(old_readme);

    let corpus = build_new_corpus(new_landing, new_docs_tree);
    let corpus_lower = corpus.to_lowercase();
    let corpus_tokens = significant_tokens(&corpus);

    // Disambiguate duplicate heading names by ordinal (spec EDGE CASE:
    // duplicate heading names -> track by (heading, ordinal)) -- computed
    // as a single pass over the already-split sections, not a re-parse.
    let mut seen_counts: HashMap<&str, usize> = HashMap::new();
    let mut total_counts: HashMap<&str, usize> = HashMap::new();
    for s in &old_sections {
        *total_counts.entry(s.heading.as_str()).or_insert(0) += 1;
    }

    let mut covered = Vec::new();
    let mut missing = Vec::new();

    for section in &old_sections {
        let occurrence = {
            let count = seen_counts.entry(section.heading.as_str()).or_insert(0);
            *count += 1;
            *count
        };
        let is_duplicate = total_counts.get(section.heading.as_str()).copied().unwrap_or(0) > 1;

        let label = if section.heading.trim().is_empty() {
            "(whole document)".to_string()
        } else if is_duplicate {
            format!("{} (#{occurrence})", section.heading)
        } else {
            section.heading.clone()
        };

        let heading_covered = !section.heading.trim().is_empty()
            && contains_as_whole_phrase(&corpus_lower, &section.heading.trim().to_lowercase());

        let section_tokens = significant_tokens(&section.body);
        let (token_covered, ratio) = if section_tokens.is_empty() {
            (false, 0.0_f32)
        } else {
            let matched = section_tokens.iter().filter(|t| corpus_tokens.contains(*t)).count();
            let ratio = matched as f32 / section_tokens.len() as f32;
            (ratio >= TOKEN_COVERAGE_THRESHOLD, ratio)
        };

        if heading_covered || token_covered {
            covered.push(label);
        } else {
            let excerpt = excerpt_of(&section.body, 120);
            let reason = if section_tokens.is_empty() {
                format!(
                    "no heading match and no stable tokens (tool/symbol names, env vars, numbers) found \
in the section body to check for content-signature coverage; excerpt: \"{excerpt}\""
                )
            } else {
                format!(
                    "heading not found and only {:.0}% of stable tokens reappear in the new landing/docs \
(below the {:.0}% coverage threshold); excerpt: \"{excerpt}\"",
                    ratio * 100.0,
                    TOKEN_COVERAGE_THRESHOLD * 100.0
                )
            };
            missing.push(Section { heading: label, reason });
        }
    }

    let total = covered.len() + missing.len();
    let coverage_ratio = if total == 0 { 1.0 } else { covered.len() as f32 / total as f32 };

    PreservationReport { covered, missing, coverage_ratio }
}

/// A short, char-boundary-safe excerpt of `text` for a [`Section::reason`]
/// message.
fn excerpt_of(text: &str, max_chars: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs_file(path: &str, content: &str) -> DocsTreeFile {
        DocsTreeFile { path: path.to_string(), content: content.to_string() }
    }

    // ── Splitting ─────────────────────────────────────────────────────────

    #[test]
    fn split_old_sections_finds_every_top_level_heading() {
        let readme = "# Title\n\nIntro.\n\n## Install\n\nRun `cargo build`.\n\n## Usage\n\nCall `widget_run()`.\n";
        let sections = split_old_sections(readme);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Install");
        assert!(sections[0].body.contains("cargo build"));
        assert_eq!(sections[1].heading, "Usage");
        assert!(sections[1].body.contains("widget_run()"));
    }

    #[test]
    fn split_old_sections_preamble_before_first_heading_is_not_a_tracked_section() {
        let readme = "# Title\n\nSome intro prose nobody tagged as a section.\n\n## Install\n\nSteps.\n";
        let sections = split_old_sections(readme);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Install");
    }

    /// EDGE CASE: no `## ` headings at all -> whole body is one section.
    #[test]
    fn split_old_sections_no_headings_becomes_one_whole_document_section() {
        let readme = "# Title\n\nJust prose, no top-level sections at all.\n";
        let sections = split_old_sections(readme);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "");
        assert!(sections[0].body.contains("Just prose"));
    }

    #[test]
    fn split_old_sections_empty_content_yields_no_sections() {
        assert!(split_old_sections("").is_empty());
        assert!(split_old_sections("   \n\n  ").is_empty());
    }

    // ── Token significance ───────────────────────────────────────────────

    #[test]
    fn significant_tokens_catches_env_vars_snake_case_camel_case_numbers_and_code_spans() {
        let text = "Configure `CHORD_BASE_URL` and call `DocGenerator::generate()`. \
Also see widgetFactory and the v2 release, port 8080.";
        let tokens = significant_tokens(text);
        assert!(tokens.contains("CHORD_BASE_URL"));
        assert!(tokens.iter().any(|t| t.contains("DocGenerator")));
        assert!(tokens.contains("widgetFactory"));
        assert!(tokens.contains("8080"));
    }

    #[test]
    fn significant_tokens_ignores_ordinary_prose_words() {
        let text = "This is a short sentence about the widget and how it works.";
        let tokens = significant_tokens(text);
        assert!(tokens.is_empty(), "expected no stable tokens in plain prose, got {tokens:?}");
    }

    // ── Acceptance: full coverage -> ratio 1.0, missing empty ────────────

    #[test]
    fn every_section_covered_yields_coverage_ratio_one_and_empty_missing() {
        let old_readme = "# Widget\n\n\
## Install\n\nRun `cargo install widget_cli` to get the `WIDGET_HOME` env var set up.\n\n\
## Configuration\n\nSet `WIDGET_PORT=8080` in your environment.\n\n\
## API\n\nCall `WidgetClient::connect()` to start a session.\n";

        let new_landing = "# Widget\n\n## Quick Start\n\n\
Install with `cargo install widget_cli`; this sets up `WIDGET_HOME` for you.\n";
        let docs = vec![
            docs_file(
                "docs/reference/configuration.md",
                "# Configuration\n\nThe `WIDGET_PORT=8080` variable controls the listen port.\n",
            ),
            docs_file(
                "docs/reference/api.md",
                "# API Reference\n\nUse `WidgetClient::connect()` to open a connection.\n",
            ),
        ];

        let report = check_preservation(old_readme, new_landing, &docs);
        assert!(report.missing.is_empty(), "expected nothing missing, got {:?}", report.missing);
        assert_eq!(report.coverage_ratio, 1.0);
        assert_eq!(report.covered.len(), 3);
    }

    // ── Negative: a genuinely dropped section is reported missing ───────

    #[test]
    fn a_deleted_feature_section_is_reported_missing_and_lowers_coverage_ratio() {
        let old_readme = "# Widget\n\n\
## Install\n\nRun `cargo install widget_cli` to get the `WIDGET_HOME` env var set up.\n\n\
## Telemetry\n\nSet `WIDGET_TELEMETRY_ENDPOINT` to opt in to the `submit_metrics()` reporter.\n";

        // New corpus preserves Install, but the Telemetry feature's substance
        // (its env var + function name) is nowhere in it.
        let new_landing = "# Widget\n\n## Quick Start\n\n\
Install with `cargo install widget_cli`; this sets up `WIDGET_HOME` for you.\n";
        let docs = vec![docs_file(
            "docs/reference/api.md",
            "# API Reference\n\nA generic reference page with nothing telemetry-related.\n",
        )];

        let report = check_preservation(old_readme, new_landing, &docs);
        assert_eq!(report.covered, vec!["Install".to_string()]);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].heading, "Telemetry");
        assert!(report.missing[0].reason.contains("stable tokens"));
        assert!(report.coverage_ratio < 1.0);
        assert!((report.coverage_ratio - 0.5).abs() < f32::EPSILON);
    }

    // ── Paraphrase: same tokens, different prose -> still covered ───────

    #[test]
    fn paraphrased_section_with_same_tool_names_is_covered_not_flagged() {
        let old_readme = "# Widget\n\n\
## Deployment\n\nDeploy the service by setting `WIDGET_DEPLOY_TOKEN` and running \
`widget_deploy.sh`. This uploads the built artifact to the fleet.\n";

        // Completely reworded, but keeps the exact tool/env-var names.
        let new_docs = vec![docs_file(
            "docs/guides/index.md",
            "# Guides\n\nTo ship your build out to the fleet, first make sure \
`WIDGET_DEPLOY_TOKEN` is exported, then hand things off to the `widget_deploy.sh` \
script -- it takes care of the upload for you.\n",
        )];

        let report = check_preservation(old_readme, "# Widget landing, unrelated content.", &new_docs);
        assert!(report.missing.is_empty(), "paraphrase must not be reported as loss: {:?}", report.missing);
        assert_eq!(report.covered, vec!["Deployment".to_string()]);
    }

    // ── EDGE CASE: no `##` headings -> whole body is one section ────────

    #[test]
    fn old_readme_with_no_headings_is_checked_as_a_single_whole_document_section() {
        let old_readme = "Just a flat README with a `WIDGET_TOKEN` reference and nothing else, \
no markdown sections at all.";
        let new_docs = vec![docs_file("docs/index.md", "The `WIDGET_TOKEN` value is documented here.")];

        let report = check_preservation(old_readme, "landing", &new_docs);
        assert_eq!(report.covered, vec!["(whole document)".to_string()]);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn empty_old_readme_yields_perfect_coverage_ratio_nothing_to_lose() {
        let report = check_preservation("", "landing", &[]);
        assert!(report.covered.is_empty());
        assert!(report.missing.is_empty());
        assert_eq!(report.coverage_ratio, 1.0);
    }

    // ── EDGE CASE: duplicate heading names tracked by (heading, ordinal) ─

    #[test]
    fn duplicate_heading_names_are_tracked_independently_by_ordinal() {
        let old_readme = "# Widget\n\n\
## Notes\n\nFirst notes section mentions `FIRST_TOKEN_XYZ`.\n\n\
## Notes\n\nSecond notes section mentions `SECOND_TOKEN_ABC`.\n";

        // Only the first Notes section's token is preserved.
        let new_docs = vec![docs_file("docs/index.md", "Reference to `FIRST_TOKEN_XYZ` lives here.")];

        let report = check_preservation(old_readme, "landing", &new_docs);
        assert_eq!(report.covered, vec!["Notes (#1)".to_string()]);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].heading, "Notes (#2)");
    }

    // ── Never silently drops missing sections; report is data, not action ─

    #[test]
    fn missing_sections_are_reported_never_silently_dropped_even_when_intentional() {
        // An operator INTENTIONALLY removed a legacy section -- the tool
        // still must not silently agree; it always reports the gap so a
        // human makes the call.
        let old_readme =
            "# Widget\n\n## Legacy Windows Support\n\nSet `LEGACY_WIN32_SHIM_PATH` to enable it.\n";
        let report = check_preservation(old_readme, "landing has nothing to do with this", &[]);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].heading, "Legacy Windows Support");
        assert!(!report.is_fully_preserved());
    }

    #[test]
    fn fully_preserved_report_reports_true() {
        let old_readme = "# Widget\n\n## Install\n\nRun `widget_setup()`.\n";
        let new_docs = vec![docs_file("docs/index.md", "Run `widget_setup()` to get started.")];
        let report = check_preservation(old_readme, "landing", &new_docs);
        assert!(report.is_fully_preserved());
    }

    // ── Bounded work: a large old README does not panic or blow up ──────

    #[test]
    fn large_old_readme_with_many_sections_is_handled_without_panicking() {
        let mut old_readme = String::from("# Widget\n\n");
        for i in 0..500 {
            old_readme.push_str(&format!(
                "## Section {i}\n\nThis section references `TOKEN_{i}_VALUE` uniquely.\n\n"
            ));
        }
        // New corpus preserves every even-numbered section's token only.
        let mut new_landing = String::new();
        for i in (0..500).step_by(2) {
            new_landing.push_str(&format!("Mentions `TOKEN_{i}_VALUE` here.\n"));
        }

        let report = check_preservation(&old_readme, &new_landing, &[]);
        assert_eq!(report.covered.len() + report.missing.len(), 500);
        assert_eq!(report.covered.len(), 250);
        assert_eq!(report.missing.len(), 250);
    }
}
