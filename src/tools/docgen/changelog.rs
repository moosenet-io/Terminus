//! DOCGEN-17: Changelog / release-notes generation from merged commits
//! (S95, Plane TERM-168).
//!
//! Produces a Keep-a-Changelog-formatted `CHANGELOG.md` section plus a
//! human-readable release-notes artifact from a list of merged commits,
//! parsed as Conventional Commits (`feat(...)/fix(...)/...`) -- the
//! convention this repo's own commit messages already follow (see the
//! skill's Stage 3 commit-message table). This is a NEW artifact type
//! alongside readme/wiki/pdf/notion/obsidian/blog (DOCGEN-01/06), versioned
//! the same way those are (DOCGEN-07's [`super::versioning::VersionStore`],
//! keyed by [`super::versioning::ArtifactKey`]).
//!
//! ## git-cliff vs. built-in (RECONCILIATION)
//! The originating research (`RESEARCH-10-improvements.md` item 7) and the
//! Plane item both point at `git-cliff` (Rust binary, Tera templates,
//! Keep-a-Changelog preset, ~120ms). This sandbox has no `git-cliff` binary
//! available (`which git-cliff` -> not found) and this module deliberately
//! does NOT add it as a new Cargo dependency (RECONCILIATION: "do not add a
//! heavyweight dep") -- git-cliff itself is a standalone CLI binary, not a
//! Rust library crate meant to be vendored in-process. Instead this module
//! is a MINIMAL, dependency-free, in-process Conventional-Commit parser and
//! Keep-a-Changelog / release-notes renderer. It produces the same shape of
//! output (grouped-by-type, dated, Keep-a-Changelog structure) without a
//! subprocess or an external binary dependency; a future item may shell out
//! to a real `git-cliff` binary when one is provisioned on a build host, but
//! that is out of scope here.
//!
//! ## WRITE-MODEL INVERSION (matches `render.rs` and `versioning.rs`)
//! Like [`super::render`], this module RETURNS generated artifacts --
//! strings -- and never places them into a repo or commits/pushes anything.
//! The calling harness decides where `CHANGELOG.md` and the release-notes
//! artifact land. Versioning them (DOCGEN-07) is the caller's
//! responsibility too: call
//! [`super::versioning::VersionStore::store_version`] with
//! `ArtifactKey::new(project, "changelog")` / `ArtifactKey::new(project,
//! "release_notes")` for the two returned strings -- this module has no
//! store of its own and reuses [`super::versioning::VersionStore`] rather
//! than inventing a second one.
//!
//! ## Deterministic, no hidden I/O
//! Like `versioning.rs` and `render.rs`, this module never reads the system
//! clock or the filesystem: the caller supplies both the commit list and
//! the release `version`/`date` strings. The SAME input always produces
//! byte-identical output (see `generation_is_deterministic_same_input_same_output`
//! below) -- required for the diffable-artifact model DOCGEN-07 versioning
//! relies on.
//!
//! ## DOCGEN-08 trigger wiring (NOT built yet -- follow-up)
//! DOCGEN-08 (the post-feat build-skill trigger that automatically invokes
//! docgen after every merged feat) has not shipped yet. This item exposes
//! the API DOCGEN-08 will call once it lands
//! ([`generate_changelog`]/[`docgen_generate_changelog` tool]) rather than
//! wiring an automatic trigger that doesn't exist. Follow-up: when DOCGEN-08
//! ships, its trigger should call `generate_changelog` with the merged
//! commit range and version-bump it into `CHANGELOG.md` via the version
//! store, the same way it will drive readme/wiki generation.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Conventional Commit parsing
// ---------------------------------------------------------------------------

/// One commit as supplied by the caller: its short hash and full commit
/// message (subject line, optionally followed by a blank line and body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInput {
    pub hash: String,
    pub message: String,
}

impl CommitInput {
    pub fn new(hash: impl Into<String>, message: impl Into<String>) -> Self {
        Self { hash: hash.into(), message: message.into() }
    }
}

/// The Keep-a-Changelog section a parsed commit is grouped into. Fixed,
/// deterministic ordering (`ALL` below) -- never derived from insertion
/// order or hash-map iteration, so output is stable across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangelogSection {
    Breaking,
    Added,
    Changed,
    Fixed,
    Documentation,
    Performance,
    Tests,
    BuildAndCi,
    Reverted,
    Chore,
    Other,
}

impl ChangelogSection {
    /// Fixed rendering order -- Keep-a-Changelog convention (breaking
    /// changes first so they're impossible to miss), then the standard
    /// Added/Changed/Fixed categories, then the less release-notable ones.
    pub const ALL: &'static [ChangelogSection] = &[
        ChangelogSection::Breaking,
        ChangelogSection::Added,
        ChangelogSection::Changed,
        ChangelogSection::Fixed,
        ChangelogSection::Documentation,
        ChangelogSection::Performance,
        ChangelogSection::Tests,
        ChangelogSection::BuildAndCi,
        ChangelogSection::Reverted,
        ChangelogSection::Chore,
        ChangelogSection::Other,
    ];

    pub fn heading(self) -> &'static str {
        match self {
            Self::Breaking => "BREAKING CHANGES",
            Self::Added => "Added",
            Self::Changed => "Changed",
            Self::Fixed => "Fixed",
            Self::Documentation => "Documentation",
            Self::Performance => "Performance",
            Self::Tests => "Tests",
            Self::BuildAndCi => "Build & CI",
            Self::Reverted => "Reverted",
            Self::Chore => "Chore",
            Self::Other => "Other",
        }
    }

    /// Map a Conventional Commit `type` token (already lowercased) to its
    /// section. Unknown/unrecognized types fall into [`Self::Other`] --
    /// never dropped (spec EDGE CASE: a commit that doesn't follow the
    /// convention is still represented, not silently discarded).
    fn from_commit_type(commit_type: &str) -> Self {
        match commit_type {
            "feat" | "feature" => Self::Added,
            "fix" | "bugfix" => Self::Fixed,
            "docs" | "doc" => Self::Documentation,
            "perf" => Self::Performance,
            "test" | "tests" => Self::Tests,
            "build" | "ci" => Self::BuildAndCi,
            "revert" => Self::Reverted,
            "chore" => Self::Chore,
            "refactor" | "style" => Self::Changed,
            _ => Self::Other,
        }
    }
}

/// A commit successfully parsed as (or gracefully degraded from)
/// Conventional Commit format, ready for grouping/rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommit {
    pub hash: String,
    /// The commit-type token as written (e.g. `"feat"`, `"fix"`), or
    /// `"other"` when the message didn't match the `type(scope): desc`
    /// shape at all.
    pub commit_type: String,
    pub scope: Option<String>,
    pub description: String,
    /// `true` when the commit is a breaking change: either a `!` right
    /// before the `:` (`feat(api)!: ...`) or a `BREAKING CHANGE:` /
    /// `BREAKING-CHANGE:` footer anywhere in the message body.
    pub breaking: bool,
    pub section: ChangelogSection,
}

/// Parse one commit message into a [`ParsedCommit`]. Never fails and never
/// drops a commit: a message that doesn't match Conventional Commit shape
/// (`type(scope): description` / `type: description`) is represented with
/// `commit_type = "other"` and the FULL original subject line as its
/// description, landing in [`ChangelogSection::Other`] -- so every supplied
/// commit is always accounted for in the output (spec EDGE CASES).
pub fn parse_conventional_commit(message: &str) -> ParsedCommit {
    parse_conventional_commit_for(String::new(), message)
}

fn parse_conventional_commit_for(hash: String, message: &str) -> ParsedCommit {
    let subject = message.lines().next().unwrap_or("").trim();
    let breaking_footer = message
        .lines()
        .any(|l| l.trim_start().starts_with("BREAKING CHANGE:") || l.trim_start().starts_with("BREAKING-CHANGE:"));

    if let Some(colon_idx) = subject.find(':') {
        let (head, rest) = subject.split_at(colon_idx);
        let description = rest[1..].trim().to_string();
        if !description.is_empty() {
            let (head, bang) = if let Some(stripped) = head.strip_suffix('!') {
                (stripped, true)
            } else {
                (head, false)
            };

            let (commit_type, scope) = if let Some(open) = head.find('(') {
                if let Some(close) = head.rfind(')') {
                    if close > open {
                        let ty = head[..open].trim();
                        let sc = head[open + 1..close].trim();
                        (ty, if sc.is_empty() { None } else { Some(sc.to_string()) })
                    } else {
                        (head.trim(), None)
                    }
                } else {
                    (head.trim(), None)
                }
            } else {
                (head.trim(), None)
            };

            let type_is_valid_token = !commit_type.is_empty()
                && commit_type.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');

            if type_is_valid_token {
                let commit_type_lower = commit_type.to_ascii_lowercase();
                let section = ChangelogSection::from_commit_type(&commit_type_lower);
                let breaking = bang || breaking_footer;
                return ParsedCommit {
                    hash,
                    commit_type: commit_type_lower,
                    scope,
                    description,
                    breaking,
                    section: if breaking { ChangelogSection::Breaking } else { section },
                };
            }
        }
    }

    // Didn't match Conventional Commit shape at all -- degrade gracefully
    // into "other" rather than dropping the commit.
    ParsedCommit {
        hash,
        commit_type: "other".to_string(),
        scope: None,
        description: subject.to_string(),
        breaking: breaking_footer,
        section: if breaking_footer { ChangelogSection::Breaking } else { ChangelogSection::Other },
    }
}

/// `true` for a merge-commit subject line (`Merge pull request ...` /
/// `Merge branch ...` / `Merge {PREFIX}-NN: ...` per this repo's own merge
/// message convention -- see the skill's Stage 6). Merge commits are noise
/// for a changelog -- they restate work already represented by the commits
/// they merge -- so they're excluded from the grouped sections (spec EDGE
/// CASE: don't double-count merge noise).
fn is_merge_commit(message: &str) -> bool {
    let subject = message.lines().next().unwrap_or("").trim();
    subject.starts_with("Merge pull request")
        || subject.starts_with("Merge branch")
        || subject.starts_with("Merge remote-tracking branch")
        || (subject.starts_with("Merge ") && subject.contains(": "))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The result of generating changelog artifacts for one release: a
/// Keep-a-Changelog-formatted section and a human-readable release-notes
/// document, both plain strings ready to be versioned
/// ([`super::versioning::VersionStore`]) and placed by the caller. Neither
/// is written to disk by this module (WRITE-MODEL INVERSION, see module doc
/// comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogArtifacts {
    pub version: String,
    pub date: String,
    /// Keep-a-Changelog formatted markdown for this one release (a
    /// `## [version] - date` section, ready to prepend above prior
    /// releases in a project's `CHANGELOG.md`).
    pub changelog_md: String,
    /// A longer-form, human-readable release-notes document for the same
    /// release (a marketing-friendlier read than the terse changelog
    /// bullets, per the Plane item's "marketing re-engagement surface"
    /// framing) -- a SEPARATE, differently-shaped artifact, not a
    /// duplicate of `changelog_md`.
    pub release_notes_md: String,
    /// How many of the input commits were merge commits and excluded.
    pub merge_commits_excluded: usize,
    /// How many input commits are represented in the output (== total
    /// input commits minus `merge_commits_excluded`; every non-merge
    /// commit is always represented, even if only in the `Other` section).
    pub commits_included: usize,
}

impl fmt::Display for ChangelogArtifacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.changelog_md)
    }
}

fn format_commit_line(c: &ParsedCommit) -> String {
    let scope_part = c.scope.as_deref().map(|s| format!("**{s}**: ")).unwrap_or_default();
    let hash_part = if c.hash.is_empty() { String::new() } else { format!(" ({})", short_hash(&c.hash)) };
    format!("- {scope_part}{}{hash_part}", capitalize_first(&c.description))
}

fn short_hash(hash: &str) -> String {
    if hash.len() > 7 {
        hash[..7].to_string()
    } else {
        hash.to_string()
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Generate Keep-a-Changelog + release-notes artifacts for one release from
/// a list of merged commits, parsed as Conventional Commits.
///
/// - `project`: project/repo name, used only in the release-notes header
///   (the changelog section itself is project-agnostic markdown meant to be
///   appended to that project's own `CHANGELOG.md`).
/// - `version`: the release version string (DOCGEN-07 versioning is
///   independent of this -- this is the human-facing SemVer-ish release
///   tag, e.g. `"1.4.0"`, not an [`super::versioning::ArtifactVersion`]
///   number).
/// - `date`: caller-supplied date string (e.g. `"2026-07-11"`) -- this
///   module never reads the system clock (see module doc comment).
/// - `commits`: the merged commits for this release, in any order; this
///   function does not require or assume chronological order and groups by
///   section, preserving each section's relative input order (a stable
///   sort) so output is deterministic given the same input.
///
/// Never fails: an empty `commits` list produces a valid, well-formed
/// "no notable changes" release entry rather than an error (spec EDGE
/// CASE).
pub fn generate_changelog(
    project: &str,
    version: &str,
    date: &str,
    commits: &[CommitInput],
) -> ChangelogArtifacts {
    let mut merge_excluded = 0usize;
    let mut by_section: BTreeMap<ChangelogSection, Vec<ParsedCommit>> = BTreeMap::new();

    for c in commits {
        if is_merge_commit(&c.message) {
            merge_excluded += 1;
            continue;
        }
        let parsed = parse_conventional_commit_for(c.hash.clone(), &c.message);
        by_section.entry(parsed.section).or_default().push(parsed);
    }

    let commits_included: usize = by_section.values().map(Vec::len).sum();

    // ---- Keep-a-Changelog section ----
    let mut changelog_md = String::new();
    changelog_md.push_str(&format!("## [{version}] - {date}\n"));

    if commits_included == 0 {
        changelog_md.push_str("\n_No notable changes._\n");
    } else {
        for section in ChangelogSection::ALL {
            if let Some(entries) = by_section.get(section) {
                if entries.is_empty() {
                    continue;
                }
                changelog_md.push_str(&format!("\n### {}\n", section.heading()));
                for c in entries {
                    changelog_md.push_str(&format_commit_line(c));
                    changelog_md.push('\n');
                }
            }
        }
    }

    // ---- Human-readable release notes ----
    let mut release_notes_md = String::new();
    release_notes_md.push_str(&format!("# {project} {version}\n\n_Released {date}._\n"));

    if commits_included == 0 {
        release_notes_md.push_str("\nThis release contains no notable changes.\n");
    } else {
        if let Some(breaking) = by_section.get(&ChangelogSection::Breaking) {
            if !breaking.is_empty() {
                release_notes_md.push_str("\n## ⚠ Breaking changes\n\n");
                release_notes_md.push_str(
                    "This release contains changes that may require action when upgrading:\n\n",
                );
                for c in breaking {
                    release_notes_md.push_str(&format_commit_line(c));
                    release_notes_md.push('\n');
                }
            }
        }

        let highlight_sections = [ChangelogSection::Added, ChangelogSection::Fixed, ChangelogSection::Changed];
        for section in highlight_sections {
            if let Some(entries) = by_section.get(&section) {
                if entries.is_empty() {
                    continue;
                }
                let intro = match section {
                    ChangelogSection::Added => "## What's new",
                    ChangelogSection::Fixed => "## Fixes",
                    ChangelogSection::Changed => "## Improvements",
                    _ => unreachable!(),
                };
                release_notes_md.push_str(&format!("\n{intro}\n\n"));
                for c in entries {
                    release_notes_md.push_str(&format_commit_line(c));
                    release_notes_md.push('\n');
                }
            }
        }

        let other_count: usize = by_section
            .iter()
            .filter(|(s, _)| {
                !matches!(
                    s,
                    ChangelogSection::Breaking | ChangelogSection::Added | ChangelogSection::Fixed | ChangelogSection::Changed
                )
            })
            .map(|(_, v)| v.len())
            .sum();
        if other_count > 0 {
            release_notes_md.push_str(&format!(
                "\n_Plus {other_count} additional maintenance/internal change(s) in this release._\n"
            ));
        }
    }

    ChangelogArtifacts {
        version: version.to_string(),
        date: date.to_string(),
        changelog_md,
        release_notes_md,
        merge_commits_excluded: merge_excluded,
        commits_included,
    }
}

// ---------------------------------------------------------------------------
// Tool: docgen_generate_changelog
// ---------------------------------------------------------------------------

/// `docgen_generate_changelog` -- generate Keep-a-Changelog + release-notes
/// artifacts for a set of merged commits. Pure/deterministic: RETURNS the
/// two artifacts (see module doc comment's WRITE-MODEL INVERSION); it never
/// writes to a repo, never calls Chord, never touches the version store
/// itself (the caller versions the returned strings via
/// [`super::versioning::VersionStore`] using
/// `ArtifactKey::new(project, "changelog")` /
/// `ArtifactKey::new(project, "release_notes")`).
pub struct DocgenGenerateChangelog;

#[async_trait]
impl RustTool for DocgenGenerateChangelog {
    fn name(&self) -> &str {
        "docgen_generate_changelog"
    }

    fn description(&self) -> &str {
        "Generate a Keep-a-Changelog CHANGELOG.md section and a human-readable release-notes \
document from a list of merged commits, parsed as Conventional Commits (feat/fix/docs/etc). \
Deterministic and pure -- returns both artifacts as strings; never writes to a repo or the \
version store itself (the caller versions them via the docgen version store). Commits that \
don't follow Conventional Commit format are still included, grouped under 'Other', never \
dropped. Merge commits are excluded as noise."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project/repo name, used in the release-notes header."
                },
                "version": {
                    "type": "string",
                    "description": "The release version string, e.g. \"1.4.0\"."
                },
                "date": {
                    "type": "string",
                    "description": "Caller-supplied release date (e.g. \"2026-07-11\"). This tool never reads the system clock."
                },
                "commits": {
                    "type": "array",
                    "description": "Merged commits for this release, in any order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "hash": {"type": "string", "description": "Commit hash (short or full)."},
                            "message": {"type": "string", "description": "Full commit message (subject line, optionally a body)."}
                        },
                        "required": ["hash", "message"]
                    }
                }
            },
            "required": ["project", "version", "date", "commits"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args
            .get("project")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("project is required".to_string()))?;
        let version = args
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("version is required".to_string()))?;
        let date = args
            .get("date")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("date is required".to_string()))?;
        let commits_raw = args
            .get("commits")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidArgument("commits must be an array".to_string()))?;

        let mut commits = Vec::with_capacity(commits_raw.len());
        for (i, c) in commits_raw.iter().enumerate() {
            let hash = c
                .get("hash")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgument(format!("commits[{i}].hash is required")))?;
            let message = c
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgument(format!("commits[{i}].message is required")))?;
            commits.push(CommitInput::new(hash, message));
        }

        let artifacts = generate_changelog(project, version, date, &commits);

        Ok(serde_json::to_string_pretty(&json!({
            "version": artifacts.version,
            "date": artifacts.date,
            "changelog_md": artifacts.changelog_md,
            "release_notes_md": artifacts.release_notes_md,
            "merge_commits_excluded": artifacts.merge_commits_excluded,
            "commits_included": artifacts.commits_included,
        }))
        .unwrap_or_else(|_| "{}".to_string()))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenGenerateChangelog));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Conventional Commit parsing ──────────────────────────────────

    #[test]
    fn parses_feat_with_scope() {
        let p = parse_conventional_commit("feat(docgen): DOCGEN-17 -- changelog generation");
        assert_eq!(p.commit_type, "feat");
        assert_eq!(p.scope, Some("docgen".to_string()));
        assert_eq!(p.description, "DOCGEN-17 -- changelog generation");
        assert!(!p.breaking);
        assert_eq!(p.section, ChangelogSection::Added);
    }

    #[test]
    fn parses_fix_without_scope() {
        let p = parse_conventional_commit("fix: correct off-by-one in pagination");
        assert_eq!(p.commit_type, "fix");
        assert_eq!(p.scope, None);
        assert_eq!(p.section, ChangelogSection::Fixed);
    }

    #[test]
    fn parses_docs_type() {
        let p = parse_conventional_commit("docs(readme): document changelog tool");
        assert_eq!(p.section, ChangelogSection::Documentation);
    }

    #[test]
    fn breaking_change_via_bang_is_flagged_and_sectioned() {
        let p = parse_conventional_commit("feat(api)!: remove deprecated v1 endpoint");
        assert!(p.breaking);
        assert_eq!(p.section, ChangelogSection::Breaking);
        assert_eq!(p.description, "remove deprecated v1 endpoint");
    }

    #[test]
    fn breaking_change_via_footer_is_flagged_and_sectioned() {
        let msg = "feat(auth): rotate token format\n\nBREAKING CHANGE: old tokens are rejected";
        let p = parse_conventional_commit(msg);
        assert!(p.breaking);
        assert_eq!(p.section, ChangelogSection::Breaking);
    }

    /// Negative test: a message that doesn't follow Conventional Commit
    /// shape at all is NOT dropped -- it degrades to "other" with the full
    /// subject line preserved as its description.
    #[test]
    fn non_conventional_message_degrades_to_other_not_dropped() {
        let p = parse_conventional_commit("quick fix for the build");
        assert_eq!(p.commit_type, "other");
        assert_eq!(p.description, "quick fix for the build");
        assert_eq!(p.section, ChangelogSection::Other);
    }

    /// Negative test: a colon with nothing meaningful after it (empty
    /// description) also degrades gracefully rather than producing a junk
    /// entry.
    #[test]
    fn empty_description_after_colon_degrades_to_other() {
        let p = parse_conventional_commit("feat: ");
        assert_eq!(p.commit_type, "other");
    }

    #[test]
    fn unknown_type_token_falls_into_other_section() {
        let p = parse_conventional_commit("wip(x): still exploring");
        assert_eq!(p.commit_type, "wip");
        assert_eq!(p.section, ChangelogSection::Other);
    }

    // ── generate_changelog: grouping, headings, ordering ─────────────

    #[test]
    fn groups_commits_by_type_into_keep_a_changelog_sections() {
        let commits = vec![
            CommitInput::new("aaa1111", "feat(docgen): add changelog tool"),
            CommitInput::new("bbb2222", "fix(docgen): correct grouping bug"),
            CommitInput::new("ccc3333", "docs(readme): document tool"),
        ];
        let out = generate_changelog("Terminus", "1.5.0", "2026-07-11", &commits);

        assert!(out.changelog_md.contains("## [1.5.0] - 2026-07-11"));
        assert!(out.changelog_md.contains("### Added"));
        assert!(out.changelog_md.contains("### Fixed"));
        assert!(out.changelog_md.contains("### Documentation"));
        assert!(out.changelog_md.contains("Add changelog tool"));
        assert_eq!(out.commits_included, 3);
        assert_eq!(out.merge_commits_excluded, 0);

        // Section order: Added must appear before Fixed, before Documentation.
        let added_pos = out.changelog_md.find("### Added").unwrap();
        let fixed_pos = out.changelog_md.find("### Fixed").unwrap();
        let docs_pos = out.changelog_md.find("### Documentation").unwrap();
        assert!(added_pos < fixed_pos && fixed_pos < docs_pos);
    }

    #[test]
    fn breaking_changes_render_first_and_are_visually_flagged() {
        let commits = vec![
            CommitInput::new("aaa", "feat(api): add v2 endpoint"),
            CommitInput::new("bbb", "feat(api)!: remove v1 endpoint"),
        ];
        let out = generate_changelog("Chord", "2.0.0", "2026-07-11", &commits);
        let breaking_pos = out.changelog_md.find("### BREAKING CHANGES").unwrap();
        let added_pos = out.changelog_md.find("### Added").unwrap();
        assert!(breaking_pos < added_pos, "breaking changes must render first");
        assert!(out.release_notes_md.contains("⚠ Breaking changes"));
    }

    /// Merge commits are noise, not release content -- excluded from
    /// grouped sections but counted, never silently vanished from the
    /// summary metadata.
    #[test]
    fn merge_commits_are_excluded_but_counted() {
        let commits = vec![
            CommitInput::new("aaa", "feat(x): real change"),
            CommitInput::new("bbb", "Merge pull request #42 from moosenet/DOCGEN-17-changelog"),
            CommitInput::new("ccc", "Merge branch 'main' into feature"),
        ];
        let out = generate_changelog("Terminus", "1.0.0", "2026-07-11", &commits);
        assert_eq!(out.commits_included, 1);
        assert_eq!(out.merge_commits_excluded, 2);
        assert!(!out.changelog_md.contains("Merge pull request"));
        assert!(!out.changelog_md.contains("Merge branch"));
    }

    /// Spec EDGE CASE: an empty commit list still produces a valid,
    /// well-formed release entry, not an error or a blank/broken document.
    #[test]
    fn empty_commit_list_produces_valid_no_notable_changes_entry() {
        let out = generate_changelog("Terminus", "0.0.1", "2026-07-11", &[]);
        assert!(out.changelog_md.contains("## [0.0.1] - 2026-07-11"));
        assert!(out.changelog_md.contains("No notable changes"));
        assert!(out.release_notes_md.contains("no notable changes"));
        assert_eq!(out.commits_included, 0);
    }

    /// A non-conventional commit message must still show up SOMEWHERE in
    /// the output (under Other), not be silently dropped from the release.
    #[test]
    fn non_conventional_commits_still_appear_under_other() {
        let commits = vec![CommitInput::new("aaa", "tweak some stuff")];
        let out = generate_changelog("Terminus", "1.0.1", "2026-07-11", &commits);
        assert!(out.changelog_md.contains("### Other"));
        assert!(out.changelog_md.contains("Tweak some stuff"));
        assert_eq!(out.commits_included, 1);
    }

    /// Deterministic output: the exact same input, called twice
    /// independently, must produce byte-identical artifacts -- required for
    /// the diffable/versioned artifact model (DOCGEN-07).
    #[test]
    fn generation_is_deterministic_same_input_same_output() {
        let commits = vec![
            CommitInput::new("aaa", "feat(a): one"),
            CommitInput::new("bbb", "fix(b): two"),
            CommitInput::new("ccc", "chore(c): three"),
        ];
        let out1 = generate_changelog("Terminus", "1.2.0", "2026-07-11", &commits);
        let out2 = generate_changelog("Terminus", "1.2.0", "2026-07-11", &commits);
        assert_eq!(out1.changelog_md, out2.changelog_md);
        assert_eq!(out1.release_notes_md, out2.release_notes_md);
    }

    /// Within a section, commits preserve their input (relative) order --
    /// a stable grouping, not a re-sort by hash or description.
    #[test]
    fn commits_within_a_section_preserve_input_order() {
        let commits = vec![
            CommitInput::new("aaa", "feat(x): first added"),
            CommitInput::new("bbb", "feat(y): second added"),
        ];
        let out = generate_changelog("Terminus", "1.0.0", "2026-07-11", &commits);
        let first_pos = out.changelog_md.find("First added").unwrap();
        let second_pos = out.changelog_md.find("Second added").unwrap();
        assert!(first_pos < second_pos);
    }

    #[test]
    fn release_notes_is_a_distinct_human_readable_document() {
        let commits = vec![CommitInput::new("aaa", "feat(x): add widget export")];
        let out = generate_changelog("Terminus", "1.3.0", "2026-07-11", &commits);
        assert!(out.release_notes_md.contains("# Terminus 1.3.0"));
        assert!(out.release_notes_md.contains("Released 2026-07-11"));
        assert!(out.release_notes_md.contains("What's new"));
        assert_ne!(out.release_notes_md, out.changelog_md);
    }

    #[test]
    fn scoped_commit_line_includes_bold_scope_prefix() {
        let commits = vec![CommitInput::new("1234567", "fix(auth): reject expired tokens")];
        let out = generate_changelog("Terminus", "1.0.2", "2026-07-11", &commits);
        assert!(out.changelog_md.contains("**auth**: Reject expired tokens"));
        assert!(out.changelog_md.contains("(1234567)"));
    }

    // ── Versioning integration (DOCGEN-07 reuse, not reimplementation) ──

    #[test]
    fn changelog_artifacts_are_versionable_via_existing_version_store() {
        use super::super::versioning::{ArtifactKey, VersionStore};

        let commits = vec![CommitInput::new("aaa", "feat(x): versioned changelog")];
        let out = generate_changelog("Terminus", "1.0.0", "2026-07-11", &commits);

        let store = VersionStore::new();
        let key = ArtifactKey::new("Terminus", "changelog");
        let v1 = store.store_version(key.clone(), out.changelog_md.clone(), "abc123", "2026-07-11T00:00:00Z");
        assert_eq!(v1.version, 1);
        assert_eq!(store.current(&key).unwrap().content, out.changelog_md);

        let notes_key = ArtifactKey::new("Terminus", "release_notes");
        store.store_version(notes_key.clone(), out.release_notes_md.clone(), "abc123", "2026-07-11T00:00:00Z");
        assert_eq!(store.current(&notes_key).unwrap().content, out.release_notes_md);
    }

    // ── Tool ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tool_generates_artifacts_from_commit_list() {
        let tool = DocgenGenerateChangelog;
        let out = tool
            .execute(json!({
                "project": "Terminus",
                "version": "1.6.0",
                "date": "2026-07-11",
                "commits": [
                    {"hash": "aaa1111", "message": "feat(docgen): DOCGEN-17"},
                    {"hash": "bbb2222", "message": "fix(docgen): fix bug"}
                ]
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["version"], json!("1.6.0"));
        assert_eq!(parsed["commits_included"], json!(2));
        assert!(parsed["changelog_md"].as_str().unwrap().contains("### Added"));
        assert!(parsed["release_notes_md"].as_str().unwrap().contains("# Terminus 1.6.0"));
    }

    /// Negative test: missing required fields return a clear
    /// `InvalidArgument`, never a panic.
    #[tokio::test]
    async fn tool_missing_required_field_is_invalid_argument() {
        let tool = DocgenGenerateChangelog;
        let result = tool.execute(json!({"project": "Terminus"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn tool_malformed_commit_entry_is_invalid_argument() {
        let tool = DocgenGenerateChangelog;
        let result = tool
            .execute(json!({
                "project": "Terminus",
                "version": "1.0.0",
                "date": "2026-07-11",
                "commits": [{"hash": "aaa"}]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn registers_expected_tool() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("docgen_generate_changelog"));
    }

    #[test]
    fn tool_has_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for info in reg.list() {
            assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
        }
    }
}
