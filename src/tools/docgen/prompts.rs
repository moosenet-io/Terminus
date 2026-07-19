//! DGRICH-02: the three repo-level grounding prompts, their output parsers,
//! and the deterministic validation lints (S119, `TERM` DGRICH,
//! `fable-docgen-redesign.md` §3).
//!
//! ## Why this module exists
//! The old single-shot module-README prompt ([`crate::review::build_docs_prompt`])
//! was hard-coded to a per-module context of `{has_existing_docs,
//! existing_docs, feat_context}` -- the repo's *identity* was never in the
//! prompt, only the last feature's diff. That is precisely why a Terminus
//! docgen run latched its tagline onto "the docgen_backfill tool": it was
//! the only thing the model had ever been shown. This module replaces that
//! single prompt, for repo-level runs, with three narrower ones that are
//! each fed a bounded, KG-grounded slice of [`super::repo_facts::RepoFacts`]
//! (DGRICH-01) and never the triggering feat's diff:
//!
//! 1. [`build_repo_identity_prompt`] -- Pass 1, one call, strict JSON output
//!    ([`RepoIdentity`]): the repo's whole-repo tagline/what-is/subsystems/
//!    features/guide topics.
//! 2. [`build_subsystem_page_prompt`] -- Pass 2, one call per kept
//!    subsystem, markdown reference-page output.
//! 3. [`build_guides_prompt`] -- Pass 3, one call, `=== FILE: <path> ===`
//!    -delimited markdown output for `getting-started.md` + `guides/*.md`.
//!
//! `build_docs_prompt` (`src/review/prompt.rs`) is kept for the legacy
//! per-module path (see its doc note) -- this module does not replace it,
//! it is simply not called by the repo-level orchestration DGRICH-03 adds.
//!
//! ## Decoupling from `RepoFacts` (load-bearing for this item's scope)
//! DGRICH-01 (the real `RepoFacts` builder) lands in a sibling worktree and
//! is not available here. Every lint below therefore takes plain,
//! already-extracted inputs (`&[String]` symbol names, `&[String]` bin/tool
//! names, `&str` feat context) rather than a `&RepoFacts` reference, so this
//! item compiles and tests standalone. DGRICH-03 is expected to wire
//! `RepoFacts`'s real fields (`prose_anchors`, `entry_points`,
//! `config_surface`, subsystem symbol tables, etc.) into these same
//! signatures rather than changing them.
//!
//! ## PII posture
//! This module builds prompt *text* only; it does not itself run the PII
//! sweep. Per DGRICH-01 (§2 Pass 0, item 6) `RepoFacts` content is swept
//! exactly once before serialization into any slice -- the JSON/str values
//! this module's builders accept (`facts_json`, `slice_json`,
//! `entrypoints_json`, `legacy_usage`) are expected to already be
//! post-sweep by the time DGRICH-03 calls these builders. This module adds
//! no second sweep and no bypass.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// RepoIdentity -- the strict-JSON output of Pass 1 ([`build_repo_identity_prompt`]).
// Consumed downstream by DGRICH-03 (orchestration), DGRICH-05 (landing
// assembly), and DGRICH-06 (docs tree render) -- lives here as the shared
// shape all three build on.
// ---------------------------------------------------------------------------

/// One kept subsystem's identity-pass brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubsystemBrief {
    pub name: String,
    pub one_liner: String,
    /// One of "core", "integration", "infra", "ui", "tooling" per the
    /// prompt's instruction -- kept as a plain `String` rather than a
    /// closed enum since the prompt (and thus the model) is the source of
    /// truth for this value and new roles may be added to the prompt text
    /// without a matching Rust enum edit.
    pub role: String,
}

/// One row of the repo-level feature inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureRow {
    pub feature: String,
    pub description: String,
    pub subsystem: String,
}

/// One candidate operator guide topic, naming the entry point it should be
/// grounded in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuideTopic {
    pub title: String,
    pub grounding: String,
}

/// The strict-JSON output of the Pass 1 identity prompt
/// ([`build_repo_identity_prompt`]). Parsed by [`parse_repo_identity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoIdentity {
    pub tagline: String,
    pub what_is: String,
    pub audience: String,
    pub subsystems: Vec<SubsystemBrief>,
    pub feature_rows: Vec<FeatureRow>,
    pub guide_topics: Vec<GuideTopic>,
}

/// Parse failure for either of this module's parsers.
#[derive(Debug, thiserror::Error)]
pub enum PromptParseError {
    #[error("repo identity JSON did not parse: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Prompt builders (§3.1-3.3, verbatim content)
// ---------------------------------------------------------------------------

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Pass 1: the repo identity prompt (§3.1). `facts_json` is the
/// [`super::repo_facts::RepoFacts`] identity slice (subsystem inventory,
/// entry points, prose anchors, legacy README headings) -- NEVER the
/// triggering feat's diff; the identity pass must never see it (that is
/// what the anti-latch lint, [`anti_latch_lint`], guards against a wiring
/// mistake reintroducing).
pub fn build_repo_identity_prompt(repo_name: &str, git_ref: &str, facts_json: &Value) -> String {
    let facts_str = pretty(facts_json);
    format!(
        "You are a senior technical writer producing the identity brief for the \
repository `{repo_name}` (analyzed at {git_ref}). You will be shown \
REPO FACTS extracted from the repository's code knowledge graph and \
source tree: subsystem inventory with symbol counts and top-ranked \
symbols, entry points (binaries, registered tools), crate/module doc \
comments, and headings from the previous README. REPO FACTS may also \
include an `existing_landing` field: the CURRENT README content already \
in place for this repository, when one exists.\n\n\
REPO FACTS:\n{facts_str}\n\n\
Write a JSON object with EXACTLY these keys:\n\
- \"tagline\": one sentence, <= 120 chars, stating what the WHOLE \
repository is and does.\n\
- \"what_is\": 2-3 paragraphs (markdown) explaining what the repository \
is, who runs it, and how its major subsystems fit together.\n\
- \"audience\": one sentence naming who this documentation serves.\n\
- \"subsystems\": one entry per subsystem in REPO FACTS, each \
{{\"name\", \"one_liner\" (<= 140 chars), \"role\" (one of: \"core\", \
\"integration\", \"infra\", \"ui\", \"tooling\")}}.\n\
- \"feature_rows\": 5-12 entries {{\"feature\", \"description\", \"subsystem\"}} -- \
the repository's genuinely distinctive capabilities.\n\
- \"guide_topics\": 2-6 entries {{\"title\", \"grounding\"}} where \"grounding\" \
names the entry point (binary, tool, or function) from REPO FACTS \
that the guide would document.\n\n\
HARD RULES:\n\
1. The identity is the SUM of the subsystems. Never describe the whole \
repository as being any single subsystem or any single feature -- \
especially not the most recently changed one. If REPO FACTS shows 15 \
subsystems, a tagline about one of them is WRONG.\n\
2. Every claim must be evidenced by REPO FACTS. Doc comments and code \
structure outrank the previous README's headings; where they \
conflict, follow the code and note nothing.\n\
3. Never invent symbol names, binary names, counts, or capabilities not \
present in REPO FACTS. If you are unsure a capability exists, omit it.\n\
4. Concrete beats generic: \"MCP tool hub exposing N tools over mTLS\" \
is right; \"a powerful, flexible platform\" is wrong. Never use the \
words \"powerful\", \"seamless\", \"comprehensive\", or \"cutting-edge\".\n\
5. DEEPEN, DON'T REGENERATE: when REPO FACTS' `existing_landing` is \
present and non-empty, treat its current tagline/what-is prose as your \
baseline. Preserve what is still accurate, refine what has drifted, and \
correct only what the code now contradicts -- do not discard good \
existing writing just to sound different. When `existing_landing` is \
absent or empty, there is no baseline: write it fresh from REPO FACTS \
alone, exactly as you would for a project's first-ever identity brief.\n\
Respond with ONLY the JSON object. No preamble, no code fence.\n"
    )
}

/// Pass 2: the per-subsystem reference-page prompt (§3.2). `identity_json`
/// is the already-parsed [`RepoIdentity`] (re-serialized so every page
/// shares one true story); `slice_json` is that subsystem's
/// [`super::repo_facts::RepoFacts`] slice.
pub fn build_subsystem_page_prompt(
    repo_name: &str,
    subsystem: &str,
    identity_json: &Value,
    slice_json: &Value,
) -> String {
    let identity_str = pretty(identity_json);
    let slice_str = pretty(slice_json);
    format!(
        "You are writing the reference page for the `{subsystem}` subsystem of \
`{repo_name}`. REPO IDENTITY (already established -- stay consistent \
with it, do not restate it):\n{identity_str}\n\n\
SUBSYSTEM FACTS (top-ranked symbols with kinds and file paths, real \
source signatures and doc comments for the key files, caller/callee \
relationships into other subsystems, env/config keys it reads, -- \
clearly labeled -- any section of the OLD README that described it, and \
-- when this subsystem already has a generated reference page -- its \
CURRENT content under `existing_page`):\n{slice_str}\n\n\
Write `docs/reference/{subsystem}.md` in markdown, 60-200 lines:\n\
1. `# {subsystem}` + one-paragraph purpose (what it does FOR the \
repository -- consistent with REPO IDENTITY).\n\
2. `## Key types and functions`: a table of the genuinely important \
symbols from SUBSYSTEM FACTS -- name (backticked full path), kind, \
file, one-line description grounded in its doc comment/signature. \
6-15 rows. Never pad with trivial accessors.\n\
3. `## How it connects`: which subsystems call into this one and which \
it calls, from the relationship data -- as prose, not a list dump.\n\
4. `## Configuration` (only if it reads config keys): each key and what \
it controls, from the code. Key NAMES only -- never values.\n\
5. `## Notes and gaps`: anything important the facts show that the \
sections above didn't cover; explicitly say what this page does NOT \
cover. Honest and short.\n\n\
HARD RULES: every symbol, path, and config key must appear in SUBSYSTEM \
FACTS -- never invent or \"round up\". Where the OLD README section \
conflicts with the code facts, follow the code and add one line noting \
the discrepancy. When SUBSYSTEM FACTS includes `existing_page`, treat it \
as your baseline: DEEPEN AND REFINE it -- keep sections that are still \
accurate, correct anything the code now contradicts, and extend real \
gaps in coverage; do not discard accurate existing content just to \
produce something different. When `existing_page` is absent, write this \
page fresh, exactly as you would for a subsystem's first-ever reference \
page. Plain markdown only, no wrapping code fence, no preamble.\n"
    )
}

/// Pass 3: the guides + getting-started prompt (§3.3). `entrypoints_json`
/// is the [`super::repo_facts::RepoFacts`] entry-point/config-surface
/// slice; `legacy_usage` is the (already labeled) old README install/usage
/// text, or an empty string if there is none.
pub fn build_guides_prompt(
    repo_name: &str,
    identity_json: &Value,
    entrypoints_json: &Value,
    legacy_usage: &str,
) -> String {
    let identity_str = pretty(identity_json);
    let entrypoints_str = pretty(entrypoints_json);
    format!(
        "You are writing the operator guides for `{repo_name}`. REPO IDENTITY:\n{identity_str}\n\n\
ENTRY POINTS AND CONFIGURATION (real binaries, registered tools, env \
keys, service endpoints -- extracted from the code) plus, clearly \
labeled, the OLD README's install/usage material, and -- when this \
repository already has generated guides -- their CURRENT content under \
`existing_getting_started`/`existing_guides`:\n{entrypoints_str}\n{legacy_usage}\n\n\
Produce, separated by lines reading exactly `=== FILE: <path> ===`:\n\
1. `docs/getting-started.md`: a tutorial from clone to first success -- \
prerequisites, build, minimal configuration (key names only), \
verification step. Every command must use a binary/tool name from \
ENTRY POINTS.\n\
2. One `docs/guides/<slug>.md` per guide topic in REPO IDENTITY: a \
task-oriented how-to (numbered steps, expected outcome, one \
troubleshooting note).\n\n\
HARD RULES: no invented commands, flags you cannot evidence, or \
placeholder URLs. If the facts don't show how to do a step, write \
\"(operator-specific: <what's needed>)\" rather than guessing. Secrets \
are never inlined: reference key names and state that values are \
provided by the repo's configured secret source at runtime (do not name \
a specific secret backend unless ENTRY POINTS establishes one). When \
`existing_getting_started`/`existing_guides` are present, DEEPEN AND \
REFINE them -- preserve steps that are still accurate, correct anything \
the code now contradicts, and extend real gaps; do not regenerate from \
a blank page. When absent, write these guides fresh, exactly as you \
would for a repository's first-ever guides.\n"
    )
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Strip a stray ```` ```json ```` / ```` ``` ```` code fence a model
/// wrapped its JSON response in, despite the prompt asking for none.
/// Tolerant of a missing closing fence (returns everything after the
/// opening fence's language-tag line in that case).
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = match after_open.find('\n') {
        Some(idx) => &after_open[idx + 1..],
        None => after_open,
    };
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// Parse Pass 1's output into a [`RepoIdentity`]. Strict serde -- a
/// subsystem entry missing its required `one_liner` (or any other required
/// key) fails to parse, per the TEST PLAN. Tolerant of a stray ```` ```json
/// ```` fence around an otherwise-conformant response.
pub fn parse_repo_identity(raw: &str) -> Result<RepoIdentity, PromptParseError> {
    let cleaned = strip_code_fence(raw);
    let identity: RepoIdentity = serde_json::from_str(cleaned)?;
    Ok(identity)
}

const FILE_MARKER_PREFIX: &str = "=== FILE: ";
const FILE_MARKER_SUFFIX: &str = " ===";

/// Split Pass 3's output on the exact literal marker line
/// `=== FILE: <path> ===` into `(path, body)` pairs, in order of
/// appearance. Content before the first marker (if any) is discarded.
/// Returns an empty `Vec` if no marker line is present at all -- per the
/// spec's EDGE CASE, the caller decides whether to retry/flag, this parser
/// never errors or panics.
pub fn parse_file_blocks(raw: &str) -> Vec<(PathBuf, String)> {
    let mut blocks = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_body = String::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(path_str) = trimmed
            .strip_prefix(FILE_MARKER_PREFIX)
            .and_then(|rest| rest.strip_suffix(FILE_MARKER_SUFFIX))
        {
            if let Some(path) = current_path.take() {
                blocks.push((path, current_body.trim().to_string()));
            }
            // The path comes straight from model output and is handed to a
            // downstream writer, so a `=== FILE: ../../x ===` or absolute path
            // would be a write-outside-the-docs-tree primitive. Only accept a
            // safe, relative, in-`docs/` path; drop the block otherwise (its body
            // is still consumed so it never bleeds into the next accepted file).
            let candidate = PathBuf::from(path_str.trim());
            current_path = is_safe_docs_path(&candidate).then_some(candidate);
            current_body.clear();
        } else if current_path.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some(path) = current_path.take() {
        blocks.push((path, current_body.trim().to_string()));
    }
    blocks
}

/// A model-supplied `=== FILE: … ===` path is only accepted if it is a
/// relative path, under `docs/`, with no `..`/root/prefix component. This is the
/// parser-level guard against a generated file block becoming a write primitive
/// outside the docs tree (defense in depth — `place_docs` is the sole writer and
/// enforces its own placement-area fence too).
fn is_safe_docs_path(path: &std::path::Path) -> bool {
    use std::path::Component;
    if path.is_absolute() {
        return false;
    }
    if path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return false;
    }
    path.starts_with("docs")
}

// ---------------------------------------------------------------------------
// Lints -- deterministic, each returns `None` (pass) or
// `Some(violation reason)` (fail). The caller (DGRICH-03) is expected to
// retry once with the violation quoted, then flag on a second failure.
// ---------------------------------------------------------------------------

/// Belt-and-suspenders anti-latch check for the identity pass's `tagline` +
/// `what_is`. The identity pass ([`build_repo_identity_prompt`]) never sees
/// the triggering feat's diff at all -- this lint exists purely to catch a
/// caller wiring mistake that reintroduces it (the exact TERM
/// "docgen_backfill tool" tagline-latch failure this whole item exists to
/// prevent), plus the softer failure mode of a tagline that only describes
/// one subsystem.
///
/// Fails when either:
/// 1. exactly one of `subsystem_names` is mentioned in the tagline/what_is
///    text while the repo has more than one kept subsystem (vocabulary
///    dominated by a single subsystem), or
/// 2. the tagline/what_is shares a distinctive (>=4-letter-word) 3-word
///    shingle with `feat_context`.
///
/// `feat_context` may legitimately be empty (this pass is never supposed
/// to receive one) -- rule 2 is then a no-op, not an error.
pub fn anti_latch_lint(
    tagline: &str,
    what_is: &str,
    subsystem_names: &[String],
    feat_context: &str,
) -> Option<String> {
    let text = format!("{tagline} {what_is}").to_lowercase();

    if subsystem_names.len() > 1 {
        let mentioned: Vec<&String> = subsystem_names
            .iter()
            .filter(|name| !name.is_empty() && text.contains(&name.to_lowercase()))
            .collect();
        if mentioned.len() == 1 {
            return Some(format!(
                "tagline/what_is mentions only the '{}' subsystem out of {} kept subsystems -- \
looks latched onto a single subsystem rather than describing the whole repository",
                mentioned[0],
                subsystem_names.len()
            ));
        }
    }

    if !feat_context.trim().is_empty() {
        let feat_shingles = word_shingles(feat_context, 3);
        let text_shingles = word_shingles(&text, 3);
        let shared: Vec<String> = text_shingles
            .intersection(&feat_shingles)
            .cloned()
            .collect();
        if !shared.is_empty() {
            let mut shown = shared.clone();
            shown.sort();
            shown.truncate(3);
            return Some(format!(
                "tagline/what_is shares distinctive phrasing with the triggering feat context \
({}) -- the identity pass must never echo the last feature it was (mistakenly) shown",
                shown.join("; ")
            ));
        }
    }

    None
}

/// Lowercased, punctuation-stripped word shingles (n consecutive words,
/// each >=4 letters, joined by a space) -- a cheap, dependency-free
/// distinctive-phrase fingerprint. Words shorter than 4 letters are
/// dropped so common short connective words don't manufacture spurious
/// overlap.
fn word_shingles(text: &str, n: usize) -> HashSet<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .collect();
    let mut shingles = HashSet::new();
    if words.len() >= n {
        for window in words.windows(n) {
            shingles.insert(window.join(" "));
        }
    }
    shingles
}

/// Extract the contents of every backtick-delimited span in `text`, in
/// order of appearance. Unterminated trailing backtick is ignored.
fn backticked_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                spans.push(&after[..end]);
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    spans
}

/// A backticked span is treated as a candidate *symbol* (rather than a
/// file path, project name, or other incidental backticked text) only if
/// it looks like a Rust-style qualified path: `crate::foo::Bar::baz`-
/// shaped, i.e. contains `::` and is otherwise alphanumeric/underscore.
/// This keeps the lint from flagging things like `` `docs/index.md` `` or
/// `` `TERM` `` as invented symbols.
fn looks_like_symbol_path(s: &str) -> bool {
    s.contains("::")
        && !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
}

/// Every backticked, `::`-qualified symbol path named in `text` must exist
/// in `real_symbols` (the flattened set of real symbol names from
/// `RepoFacts` -- see the module doc's decoupling note: DGRICH-03 supplies
/// this from the real `RepoFacts`, this lint takes it as plain data).
/// Fails listing every invented name found; passes (returns `None`) if
/// every backticked symbol-shaped span matches.
pub fn symbol_existence_lint(text: &str, real_symbols: &[String]) -> Option<String> {
    let real: HashSet<&str> = real_symbols.iter().map(String::as_str).collect();
    let mut invented: Vec<String> = backticked_spans(text)
        .into_iter()
        .filter(|span| looks_like_symbol_path(span) && !real.contains(span))
        .map(str::to_string)
        .collect();
    if invented.is_empty() {
        return None;
    }
    invented.sort();
    invented.dedup();
    Some(format!(
        "named symbol(s) not present in RepoFacts (invented API): {}",
        invented.join(", ")
    ))
}

/// Shell builtins / universally-available tools that are honest commands
/// in ANY repo's guide even though they are never one of the repo's own
/// `[[bin]]`/registered-tool entry points.
const SHELL_BUILTINS: &[&str] = &[
    "cd", "ls", "cat", "export", "mkdir", "echo", "cp", "mv", "rm", "curl", "git", "cargo",
    "sudo", "source", "chmod", "pwd", "grep", "tar", "make", "which",
];

fn is_command_shaped(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false)
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every backticked command's first word must name either a real
/// `[[bin]]`/registered-tool from `real_bin_names` (the flattened set from
/// `RepoFacts::entry_points` -- see the module doc's decoupling note) or a
/// [`SHELL_BUILTINS`] entry. Fails listing every unrecognized command
/// name; passes if every command is honest.
pub fn honest_command_lint(guide_text: &str, real_bin_names: &[String]) -> Option<String> {
    let real: HashSet<&str> = real_bin_names.iter().map(String::as_str).collect();
    let mut unknown: Vec<String> = backticked_spans(guide_text)
        .into_iter()
        .filter_map(|span| span.split_whitespace().next())
        .filter(|first_word| is_command_shaped(first_word) && !first_word.contains("::"))
        .filter(|first_word| !real.contains(first_word) && !SHELL_BUILTINS.contains(first_word))
        .map(str::to_string)
        .collect();
    if unknown.is_empty() {
        return None;
    }
    unknown.sort();
    unknown.dedup();
    Some(format!(
        "command(s) name a binary/tool not present in RepoFacts entry points: {}",
        unknown.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_identity_json() -> Value {
        json!({
            "tagline": "The Lumina Constellation's MCP tool hub, model intake, and code intelligence engines behind one mTLS mesh.",
            "what_is": "Terminus is the tool plane of the Lumina Constellation fleet.\n\nIt runs intake, scribe, and cortex behind a gateway.",
            "audience": "Fleet operators and agents that need tool access.",
            "subsystems": [
                {"name": "intake", "one_liner": "Model discovery and profiling.", "role": "core"},
                {"name": "forge", "one_liner": "Gitea/GitHub mirror integration.", "role": "integration"},
                {"name": "mesh", "one_liner": "mTLS tailnet gateway.", "role": "infra"}
            ],
            "feature_rows": [
                {"feature": "Tool registry", "description": "Dispatches MCP tool calls.", "subsystem": "tools"},
                {"feature": "KG build", "description": "Builds the Atlas code graph.", "subsystem": "scribe"},
                {"feature": "Model discovery", "description": "Finds and profiles models.", "subsystem": "intake"},
                {"feature": "PR replay", "description": "Replays merged PRs to mirrors.", "subsystem": "forge"},
                {"feature": "mTLS auth", "description": "Authenticates callers.", "subsystem": "mesh"}
            ],
            "guide_topics": [
                {"title": "Run a fleet assessment", "grounding": "intake::assessment::run"},
                {"title": "Connect an MCP client", "grounding": "bin::terminus_primary"}
            ]
        })
    }

    // --- prompt builders -----------------------------------------------

    #[test]
    fn identity_prompt_contains_hard_rules_and_banned_adjectives() {
        let facts = json!({"subsystems": ["intake", "forge"]});
        let prompt = build_repo_identity_prompt("Terminus", "abc123", &facts);
        assert!(prompt.contains("Terminus"));
        assert!(prompt.contains("abc123"));
        assert!(prompt.contains("HARD RULES"));
        assert!(prompt.contains("\"powerful\""));
        assert!(prompt.contains("\"seamless\""));
        assert!(prompt.contains("\"comprehensive\""));
        assert!(prompt.contains("cutting-edge"));
        assert!(prompt.contains("tagline"));
        assert!(prompt.contains("guide_topics"));
        assert!(prompt.contains("No preamble, no code fence"));
    }

    #[test]
    fn subsystem_page_prompt_contains_sections_and_subsystem_name() {
        let identity = sample_identity_json();
        let slice = json!({"top_symbols": ["mesh::tailnet::TailnetServer::start"]});
        let prompt = build_subsystem_page_prompt("Terminus", "mesh", &identity, &slice);
        assert!(prompt.contains("`mesh`"));
        assert!(prompt.contains("Key types and functions"));
        assert!(prompt.contains("How it connects"));
        assert!(prompt.contains("Notes and gaps"));
        assert!(prompt.contains("never invent"));
    }

    #[test]
    fn guides_prompt_contains_file_marker_instruction() {
        let identity = sample_identity_json();
        let entrypoints = json!({"bins": ["terminus_primary"]});
        let prompt = build_guides_prompt("Terminus", &identity, &entrypoints, "");
        assert!(prompt.contains("=== FILE: <path> ==="));
        assert!(prompt.contains("getting-started.md"));
        assert!(prompt.contains("operator-specific"));
    }

    // --- DGDG-02: deepen-from-baseline wording ---------------------------

    #[test]
    fn identity_prompt_instructs_deepening_the_existing_landing_baseline() {
        let facts = json!({"subsystems": ["intake", "forge"]});
        let prompt = build_repo_identity_prompt("Terminus", "abc123", &facts);
        assert!(prompt.contains("existing_landing"));
        assert!(prompt.contains("DEEPEN"));
        assert!(prompt.to_lowercase().contains("no baseline"));
    }

    #[test]
    fn subsystem_page_prompt_instructs_deepening_the_existing_page_baseline() {
        let identity = sample_identity_json();
        let slice = json!({"top_symbols": []});
        let prompt = build_subsystem_page_prompt("Terminus", "mesh", &identity, &slice);
        assert!(prompt.contains("existing_page"));
        assert!(prompt.contains("DEEPEN AND REFINE"));
    }

    #[test]
    fn guides_prompt_instructs_deepening_existing_guides_baseline() {
        let identity = sample_identity_json();
        let entrypoints = json!({"bins": ["terminus_primary"]});
        let prompt = build_guides_prompt("Terminus", &identity, &entrypoints, "");
        assert!(prompt.contains("existing_getting_started"));
        assert!(prompt.contains("existing_guides"));
        assert!(prompt.contains("DEEPEN AND REFINE"));
    }

    // --- parse_repo_identity ---------------------------------------------

    #[test]
    fn parses_well_formed_identity_json() {
        let raw = sample_identity_json().to_string();
        let identity = parse_repo_identity(&raw).expect("should parse");
        assert_eq!(identity.tagline.contains("mTLS"), true);
        assert_eq!(identity.subsystems.len(), 3);
        assert_eq!(identity.feature_rows.len(), 5);
        assert_eq!(identity.guide_topics.len(), 2);
    }

    #[test]
    fn parses_identity_json_wrapped_in_a_code_fence() {
        let raw = format!("```json\n{}\n```", sample_identity_json());
        let identity = parse_repo_identity(&raw).expect("fenced JSON should still parse");
        assert_eq!(identity.subsystems.len(), 3);
    }

    #[test]
    fn parses_identity_json_wrapped_in_a_bare_fence() {
        let raw = format!("```\n{}\n```", sample_identity_json());
        let identity = parse_repo_identity(&raw).expect("bare-fenced JSON should still parse");
        assert_eq!(identity.audience.is_empty(), false);
    }

    #[test]
    fn rejects_identity_json_missing_a_subsystem_one_liner() {
        let mut broken = sample_identity_json();
        broken["subsystems"][0]
            .as_object_mut()
            .unwrap()
            .remove("one_liner");
        let raw = broken.to_string();
        let result = parse_repo_identity(&raw);
        assert!(result.is_err(), "missing required one_liner must fail to parse");
    }

    #[test]
    fn rejects_non_json_garbage() {
        let result = parse_repo_identity("not json at all");
        assert!(result.is_err());
    }

    // --- parse_file_blocks ------------------------------------------------

    #[test]
    fn splits_two_file_guide_output() {
        let raw = "\
=== FILE: docs/getting-started.md ===
# Getting started
Clone the repo and build it.

=== FILE: docs/guides/run-assessment.md ===
# Run a fleet assessment
1. Do the thing.
2. Verify it worked.
";
        let blocks = parse_file_blocks(raw);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, PathBuf::from("docs/getting-started.md"));
        assert!(blocks[0].1.contains("Clone the repo"));
        assert_eq!(blocks[1].0, PathBuf::from("docs/guides/run-assessment.md"));
        assert!(blocks[1].1.contains("Verify it worked"));
    }

    #[test]
    fn missing_file_marker_yields_empty_vec() {
        let raw = "Just some prose with no marker line at all.";
        let blocks = parse_file_blocks(raw);
        assert!(blocks.is_empty());
    }

    #[test]
    fn content_before_first_marker_is_discarded() {
        let raw = "preamble the model was told not to write\n\
=== FILE: docs/getting-started.md ===\nbody\n";
        let blocks = parse_file_blocks(raw);
        assert_eq!(blocks.len(), 1);
        assert!(!blocks[0].1.contains("preamble"));
    }

    #[test]
    fn unsafe_or_out_of_tree_file_paths_are_dropped() {
        let raw = "\
=== FILE: ../../etc/cron.d/evil ===\nrm -rf /\n\
=== FILE: /etc/passwd ===\nroot:x:0:0\n\
=== FILE: src/main.rs ===\nfn main(){}\n\
=== FILE: docs/guides/ok.md ===\nreal guide\n";
        let blocks = parse_file_blocks(raw);
        // only the docs/ path survives; the traversal/absolute/out-of-docs ones are dropped
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, PathBuf::from("docs/guides/ok.md"));
        assert_eq!(blocks[0].1, "real guide");
    }

    // --- anti_latch_lint ----------------------------------------------------

    #[test]
    fn anti_latch_fails_a_single_subsystem_dominated_tagline() {
        let subsystems = vec!["intake".to_string(), "forge".to_string(), "mesh".to_string()];
        let tagline = "Intake is the model discovery and profiling engine for the fleet.";
        let what_is = "Intake handles model discovery. Intake also profiles models.";
        let violation = anti_latch_lint(tagline, what_is, &subsystems, "");
        assert!(violation.is_some());
        assert!(violation.unwrap().contains("intake"));
    }

    #[test]
    fn anti_latch_passes_a_balanced_hub_tagline() {
        let subsystems = vec!["intake".to_string(), "forge".to_string(), "mesh".to_string()];
        let tagline = "The Lumina Constellation's tool hub, combining intake, forge, and mesh.";
        let what_is = "Terminus brings intake, forge, and mesh together behind one gateway.";
        let violation = anti_latch_lint(tagline, what_is, &subsystems, "");
        assert!(violation.is_none());
    }

    #[test]
    fn anti_latch_empty_feat_context_is_a_noop_not_a_crash() {
        let subsystems = vec!["intake".to_string(), "forge".to_string()];
        let tagline = "A hub combining intake and forge behind one gateway.";
        let what_is = "It brings intake and forge together for the fleet.";
        // feat_context empty -- rule 2 must not run/crash regardless of
        // how much vocabulary overlap there happens to be.
        let violation = anti_latch_lint(tagline, what_is, &subsystems, "");
        assert!(violation.is_none());
    }

    #[test]
    fn anti_latch_fails_when_tagline_echoes_the_feat_context() {
        let subsystems = vec!["intake".to_string(), "forge".to_string(), "mesh".to_string()];
        let tagline = "A hub combining intake, forge, and mesh for the migration project.";
        let what_is = "The docgen backfill tool migrates a repo's bloated README into layers.";
        let feat_context = "the docgen backfill tool migrates a repo bloated readme into layers";
        let violation = anti_latch_lint(tagline, what_is, &subsystems, feat_context);
        assert!(violation.is_some());
    }

    // --- symbol_existence_lint ----------------------------------------------

    #[test]
    fn symbol_existence_fails_an_invented_api_name() {
        let real_symbols = vec![
            "crate::mesh::tailnet::TailnetServer::start".to_string(),
            "crate::registry::ToolRegistry::contains".to_string(),
        ];
        let text = "The gateway calls `crate::mesh::tailnet::TailnetServer::start` and then \
the invented `crate::mesh::phantom::GhostRouter::teleport` before dispatch.";
        let violation = symbol_existence_lint(text, &real_symbols);
        assert!(violation.is_some());
        let msg = violation.unwrap();
        assert!(msg.contains("GhostRouter"));
        assert!(!msg.contains("TailnetServer"));
    }

    #[test]
    fn symbol_existence_passes_when_every_symbol_is_real() {
        let real_symbols = vec!["crate::registry::ToolRegistry::contains".to_string()];
        let text = "Dispatch checks `crate::registry::ToolRegistry::contains` and the docs path \
`docs/reference/mesh.md` before continuing.";
        let violation = symbol_existence_lint(text, &real_symbols);
        assert!(violation.is_none());
    }

    // --- honest_command_lint -------------------------------------------------

    #[test]
    fn honest_command_fails_an_unknown_binary() {
        let real_bins = vec!["terminus_primary".to_string()];
        let text = "Start the server with `terminus_primary --config ./config.toml` then run \
`ghostbinary --serve` to enable the phantom feature.";
        let violation = honest_command_lint(text, &real_bins);
        assert!(violation.is_some());
        assert!(violation.unwrap().contains("ghostbinary"));
    }

    #[test]
    fn honest_command_passes_real_bins_and_shell_builtins() {
        let real_bins = vec!["terminus_primary".to_string()];
        let text = "Run `git clone <repo>`, `cd terminus`, then `terminus_primary --config ./config.toml`.";
        let violation = honest_command_lint(text, &real_bins);
        assert!(violation.is_none());
    }
}
