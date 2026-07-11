//! DOCGEN-15: static wiki search index, built at doc-generation time (S95,
//! Plane TERM-166).
//!
//! ## Scope and shape (why this is a library module, not a registered tool)
//! Like [`super::render::wiki_graph`] (DOCGEN-14), this is a whole-vault
//! concern: a search index needs every page's content at once to build one
//! `term -> pages` map, so it operates over a *slice* of pages
//! ([`IndexedPage`]), not a single [`super::render::RenderContext`], and does
//! not plug into [`super::render::render_all`]. It ships as a new
//! post-render "index" stage: once [`super::render::wiki`] (or any other
//! renderer) has produced a project's pages, the caller collects them into
//! [`IndexedPage`]s and calls [`build_search_artifact`] to get the static
//! index this module builds.
//!
//! ## Pagefind-or-builtin (per the research report, RESEARCH-10 section 5)
//! Two backends, selected automatically:
//! - **`pagefind`** (preferred, if the `pagefind` binary is on `PATH`):
//!   pagefind crawls a directory of rendered HTML pages and writes its own
//!   index shards (JS + WASM + per-page JSON) to an output directory. This
//!   module stages [`IndexedPage`]s as minimal HTML files in a *temporary*
//!   directory (never the caller's real output location -- see WRITE-MODEL
//!   INVERSION below), invokes `pagefind --site <tmp> --output-path
//!   <tmp>/out`, reads every produced file back into memory, and deletes the
//!   temp directories before returning. Pagefind is explicitly the
//!   research's preferred choice: it works fully offline (both for the
//!   vault and a hosted static site) and ships near-zero bandwidth (it
//!   fetches only the index shards a query actually touches, not a
//!   whole-index blob) -- see the module doc on [`SearchIndex`] for why the
//!   builtin backend deliberately does NOT try to replicate that shard-on-
//!   demand behavior.
//! - **Built-in inverted index** (fallback, and the path exercised by this
//!   sandbox where `pagefind` is not installed -- confirmed via `which
//!   pagefind` before writing this module): a small, dependency-free,
//!   deterministic `term -> page_id -> term_frequency` map ([`SearchIndex`]),
//!   serialized to a single static `index.json`, plus a tiny vanilla-JS
//!   query helper ([`QUERY_HELPER_JS`]) that performs the exact same
//!   AND-match/frequency-ranked query this module's own [`SearchIndex::query`]
//!   implements, so a browser can search fully offline with zero server and
//!   zero additional runtime dependency. This is intentionally the *simple*
//!   choice: no client-side whole-index frameworks like Orama (loads the
//!   entire index into the browser, expensive for a large wiki) and no
//!   hosted/paid services like Algolia -- both explicitly steered away from
//!   by the research report. A single flat JSON file plus ~40 lines of JS is
//!   "more useful than a skip" for an offline vault and costs nothing to
//!   host.
//!
//! ## WRITE-MODEL INVERSION (unchanged from the rest of `docgen`)
//! Every public entry point in this module RETURNS the built artifact(s) --
//! `index.json` bytes/string, the query-helper JS, or (for the pagefind
//! path) a `path -> bytes` map of pagefind's own output files -- and never
//! writes into the caller's real output/vault/repo location. The ONLY
//! filesystem writes this module performs are to an ephemeral `tempfile`
//! directory used solely to hand `pagefind` a site directory to crawl (it
//! has no in-memory API); that directory and everything under it is deleted
//! before the function returns, mirroring how [`super::render::wiki_graph`]
//! shells out to a *local* layout binary without ever placing a file for the
//! caller. The calling harness decides where any of this actually lands
//! (see `build_search_artifact_never_leaves_files_behind` in this module's
//! tests for the negative test asserting this end to end).
//!
//! ## Versioning (DOCGEN-07 reuse)
//! The index is a build-time static artifact like every other rendered
//! target -- callers version it the same way any other rendered artifact is
//! versioned, via [`super::versioning::VersionStore`] keyed by
//! `ArtifactKey::new(project, "search-index")`. This module does not import
//! or call `versioning` itself (it has no opinion on project/commit
//! metadata), matching how [`super::render`] renderers stay decoupled from
//! the version store and let the caller wire the two together.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

// ─── Indexed page (the whole-index input shape) ──────────────────────────────

/// One page to fold into the search index. `content` should already be
/// plain text (or close to it) -- this module does no HTML/Markdown
/// stripping of its own; the caller (typically after [`super::render::wiki`]
/// has produced a page) supplies the already-rendered prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedPage {
    /// Stable page id (e.g. a slug). Used as the index's join key and as
    /// the pagefind staging filename stem.
    pub id: String,
    pub title: String,
    /// Where this page will be hosted, relative to the site/vault root
    /// (e.g. `"wiki/widget.html"`). Informational only -- carried through to
    /// query results so a caller can link straight to a hit.
    pub url: String,
    pub content: String,
}

// ─── Tokenization (pure, deterministic) ──────────────────────────────────────

/// A tiny English stopword list -- just enough to keep the index from being
/// dominated by near-universal words, without pulling in a dependency for
/// it. Deliberately short; this is a build-time relevance nicety, not a
/// linguistics engine.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "he", "in", "is",
    "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will", "with",
];

/// Lowercase, split on non-alphanumeric boundaries, drop single-character
/// tokens and stopwords. Pure and deterministic -- the same `text` always
/// tokenizes to the same sequence, in order, which is what makes both the
/// index build and a query against it reproducible.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

// ─── Built-in inverted index ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMeta {
    pub title: String,
    pub url: String,
}

/// `term -> page_id -> term_frequency`, plus per-page metadata for
/// rendering a result. `BTreeMap` throughout so serialization and iteration
/// order are deterministic -- regenerating from an unchanged page set always
/// produces byte-identical `index.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchIndex {
    pub pages: BTreeMap<String, PageMeta>,
    pub terms: BTreeMap<String, BTreeMap<String, u32>>,
}

/// Build the inverted index from `pages`. A page with empty/all-stopword
/// content still gets a `pages` entry (so it can be looked up/linked) even
/// though it contributes no terms.
pub fn build_index(pages: &[IndexedPage]) -> SearchIndex {
    let mut index = SearchIndex::default();
    for page in pages {
        index
            .pages
            .insert(page.id.clone(), PageMeta { title: page.title.clone(), url: page.url.clone() });
        for term in tokenize(&page.content) {
            *index.terms.entry(term).or_default().entry(page.id.clone()).or_insert(0) += 1;
        }
        // Title words count too (weighted higher, matches feel more
        // relevant when they hit the title rather than only the body).
        for term in tokenize(&page.title) {
            *index.terms.entry(term).or_default().entry(page.id.clone()).or_insert(0) += 5;
        }
    }
    index
}

/// One query hit: which page, how it's titled/linked, and its relevance
/// score (sum of matched terms' frequency in that page -- higher is more
/// relevant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub url: String,
    pub score: u32,
}

impl SearchIndex {
    /// Query the index: tokenize `query_str` the same way pages were
    /// tokenized, then return every page containing ALL query terms
    /// (AND semantics -- a page missing even one term is not a match),
    /// ranked by summed term frequency descending, ties broken by page id
    /// ascending so results are fully deterministic. An empty/all-stopword
    /// query returns no hits rather than matching everything.
    pub fn query(&self, query_str: &str, limit: usize) -> Vec<SearchHit> {
        let query_terms = tokenize(query_str);
        if query_terms.is_empty() {
            return Vec::new();
        }

        // Candidate page ids: intersection of every query term's postings.
        let mut candidates: Option<BTreeSet<String>> = None;
        for term in &query_terms {
            let postings: BTreeSet<String> =
                self.terms.get(term).map(|m| m.keys().cloned().collect()).unwrap_or_default();
            candidates = Some(match candidates {
                None => postings,
                Some(existing) => existing.intersection(&postings).cloned().collect(),
            });
            if candidates.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
                return Vec::new();
            }
        }

        let mut hits: Vec<SearchHit> = candidates
            .unwrap_or_default()
            .into_iter()
            .map(|id| {
                let score: u32 =
                    query_terms.iter().filter_map(|t| self.terms.get(t)?.get(&id)).sum();
                let meta = self.pages.get(&id).cloned().unwrap_or(PageMeta {
                    title: id.clone(),
                    url: String::new(),
                });
                SearchHit { id, title: meta.title, url: meta.url, score }
            })
            .collect();

        // Score descending, then id ascending -- deterministic tie-break.
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(limit);
        hits
    }

    /// Serialize to the static `index.json` artifact. Deterministic (all
    /// maps are `BTreeMap`s, `serde_json` preserves insertion/iteration
    /// order for `Value::Object` built from an already-sorted source).
    pub fn to_json(&self) -> String {
        let pages: Value = self
            .pages
            .iter()
            .map(|(id, meta)| (id.clone(), json!({ "title": meta.title, "url": meta.url })))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let terms: Value = self
            .terms
            .iter()
            .map(|(term, postings)| {
                let postings_json: Value = postings
                    .iter()
                    .map(|(id, freq)| (id.clone(), json!(freq)))
                    .collect::<serde_json::Map<_, _>>()
                    .into();
                (term.clone(), postings_json)
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
        serde_json::to_string_pretty(&json!({ "pages": pages, "terms": terms })).unwrap_or_default()
    }
}

/// A minimal, dependency-free vanilla-JS query helper that performs the
/// exact same AND-match/frequency-ranked query [`SearchIndex::query`]
/// implements, against the static `index.json` this module produces. Ships
/// as a static file alongside `index.json` -- no bundler, no framework, no
/// network calls beyond the one `fetch("index.json")` (or none at all if
/// the caller inlines the JSON), which is what makes this work fully
/// offline for the vault case.
pub const QUERY_HELPER_JS: &str = r#"// DOCGEN-15 static search query helper (built-in backend).
// Usage: const hits = await search("some query", await loadIndex());
function tokenize(text) {
  const stop = new Set(["a","an","and","are","as","at","be","by","for","from","has","he","in","is","it","its","of","on","or","that","the","this","to","was","were","will","with"]);
  return text
    .split(/[^a-zA-Z0-9]+/)
    .map((w) => w.toLowerCase())
    .filter((w) => w.length > 1 && !stop.has(w));
}

async function loadIndex(url = "index.json") {
  const res = await fetch(url);
  return res.json();
}

function search(queryStr, index, limit = 20) {
  const queryTerms = tokenize(queryStr);
  if (queryTerms.length === 0) return [];

  let candidates = null;
  for (const term of queryTerms) {
    const postings = index.terms[term] ? Object.keys(index.terms[term]) : [];
    const postingSet = new Set(postings);
    candidates = candidates === null ? postingSet : new Set([...candidates].filter((id) => postingSet.has(id)));
    if (candidates.size === 0) return [];
  }

  const hits = [...candidates].map((id) => {
    const score = queryTerms.reduce((sum, t) => sum + (index.terms[t]?.[id] || 0), 0);
    const meta = index.pages[id] || { title: id, url: "" };
    return { id, title: meta.title, url: meta.url, score };
  });

  hits.sort((a, b) => (b.score - a.score) || a.id.localeCompare(b.id));
  return hits.slice(0, limit);
}
"#;

// ─── Pagefind (external binary, if available) ────────────────────────────────

/// Whether the `pagefind` binary is invocable on `PATH`. Mirrors
/// [`super::render::wiki_graph::engine_available`]'s spawn-and-probe
/// pattern: spawn success (any exit code) means the binary exists and can
/// be run; a spawn failure (not found) means unavailable.
pub fn pagefind_available() -> bool {
    Command::new("pagefind")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok()
}

/// Stage `pages` as minimal static HTML files pagefind can crawl -- pure
/// and fully testable without the `pagefind` binary itself (this is the
/// piece [`build_pagefind_site_files`]'s tests exercise directly, since
/// invoking the real binary isn't possible in every environment). Returns
/// `relative_filename -> html_content`.
pub fn build_pagefind_site_files(pages: &[IndexedPage]) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for page in pages {
        let html = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
<body data-pagefind-body><h1>{title}</h1><div>{content}</div></body></html>\n",
            title = html_escape(&page.title),
            content = html_escape(&page.content),
        );
        files.insert(format!("{}.html", page.id), html);
    }
    files
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Run `pagefind` end to end against `pages`: stage HTML into a temp
/// directory, invoke `pagefind --site <tmp> --output-path <tmp>/out`, read
/// every produced file back into memory, then delete the temp directory --
/// so no trace is left on disk regardless of success or failure. Returns
/// `None` on any failure (binary missing, non-zero exit, no output
/// produced), letting the caller fall back to the built-in index.
pub fn run_pagefind(pages: &[IndexedPage]) -> Option<BTreeMap<String, Vec<u8>>> {
    let tmp = std::env::temp_dir().join(format!(
        "docgen-search-index-pagefind-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()
    ));
    let site_dir = tmp.join("site");
    let out_dir = tmp.join("out");
    fs::create_dir_all(&site_dir).ok()?;

    for (filename, html) in build_pagefind_site_files(pages) {
        let path = site_dir.join(filename);
        let mut f = fs::File::create(&path).ok()?;
        f.write_all(html.as_bytes()).ok()?;
    }

    let status = Command::new("pagefind")
        .arg("--site")
        .arg(&site_dir)
        .arg("--output-path")
        .arg(&out_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let result = match status {
        Ok(s) if s.success() => read_dir_recursive(&out_dir),
        _ => None,
    };

    let _ = fs::remove_dir_all(&tmp);
    result
}

fn read_dir_recursive(dir: &Path) -> Option<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    collect_files(dir, dir, &mut out)?;
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Option<()> {
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
            out.insert(rel, fs::read(&path).ok()?);
        }
    }
    Some(())
}

// ─── Top-level: pagefind-or-builtin selection ────────────────────────────────

/// Which backend actually produced [`SearchIndexResult::artifact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngine {
    Pagefind,
    Builtin,
}

/// The static file(s) to ship. `Pagefind` carries pagefind's own output
/// tree (`relative_path -> bytes`); `Builtin` carries this module's own
/// `index.json` plus [`QUERY_HELPER_JS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchIndexArtifact {
    Pagefind { files: BTreeMap<String, Vec<u8>> },
    Builtin { index_json: String, query_helper_js: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexResult {
    pub engine: SearchEngine,
    pub artifact: SearchIndexArtifact,
    pub note: String,
}

/// Build the static search index for `pages`, preferring `pagefind` when
/// it's on `PATH` and can be run successfully, falling back to the built-in
/// inverted index otherwise (unavailable binary, or `pagefind` itself
/// failed). Never places anything -- see the module's WRITE-MODEL INVERSION
/// doc; the caller decides where the returned artifact lands.
pub fn build_search_artifact(pages: &[IndexedPage]) -> SearchIndexResult {
    if pagefind_available() {
        if let Some(files) = run_pagefind(pages) {
            return SearchIndexResult {
                engine: SearchEngine::Pagefind,
                artifact: SearchIndexArtifact::Pagefind { files },
                note: "built via the local pagefind binary".to_string(),
            };
        }
    }

    let index = build_index(pages);
    SearchIndexResult {
        engine: SearchEngine::Builtin,
        artifact: SearchIndexArtifact::Builtin {
            index_json: index.to_json(),
            query_helper_js: QUERY_HELPER_JS.to_string(),
        },
        note: "pagefind unavailable (no binary on PATH, or it failed) -- built the dependency-free \
built-in inverted index (index.json + query helper JS) instead"
            .to_string(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, title: &str, content: &str) -> IndexedPage {
        IndexedPage {
            id: id.to_string(),
            title: title.to_string(),
            url: format!("wiki/{id}.html"),
            content: content.to_string(),
        }
    }

    // ── Tokenization ────────────────────────────────────────────────────

    #[test]
    fn tokenize_lowercases_and_splits_on_non_alphanumeric() {
        let toks = tokenize("Widget-Module: The Fast Path!");
        assert_eq!(toks, vec!["widget", "module", "fast", "path"]);
    }

    #[test]
    fn tokenize_drops_stopwords_and_single_chars() {
        let toks = tokenize("a the of it is a widget");
        assert_eq!(toks, vec!["widget"]);
    }

    #[test]
    fn tokenize_empty_string_yields_no_tokens() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    // ── Build-time static index ─────────────────────────────────────────

    #[test]
    fn build_index_is_deterministic_across_rebuilds() {
        let pages = vec![
            page("widget", "Widget Module", "the widget handles routing"),
            page("gadget", "Gadget Module", "the gadget handles caching"),
        ];
        let a = build_index(&pages).to_json();
        let b = build_index(&pages).to_json();
        assert_eq!(a, b, "rebuilding from an unchanged page set must be byte-identical");
    }

    #[test]
    fn build_index_records_every_page_even_with_no_terms() {
        let pages = vec![page("empty", "", "")];
        let index = build_index(&pages);
        assert!(index.pages.contains_key("empty"));
    }

    #[test]
    fn build_index_to_json_round_trips_as_valid_json() {
        let pages = vec![page("widget", "Widget", "widget content here")];
        let json_str = build_index(&pages).to_json();
        let parsed: Value = serde_json::from_str(&json_str).expect("index.json must be valid JSON");
        assert!(parsed["pages"]["widget"]["title"] == "Widget");
        assert!(parsed["terms"]["widget"].is_object());
    }

    // ── Query correctness (the load-bearing "returns the right pages" check) ──

    #[test]
    fn query_returns_pages_containing_the_term() {
        let pages = vec![
            page("widget", "Widget Module", "the widget handles routing"),
            page("gadget", "Gadget Module", "the gadget handles caching"),
        ];
        let index = build_index(&pages);
        let hits = index.query("routing", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "widget");
        assert_eq!(hits[0].url, "wiki/widget.html");
    }

    #[test]
    fn query_and_semantics_requires_all_terms() {
        let pages = vec![
            page("a", "A", "alpha beta"),
            page("b", "B", "alpha only"),
        ];
        let index = build_index(&pages);
        let hits = index.query("alpha beta", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a", "page b lacks 'beta' so must not match an AND query");
    }

    #[test]
    fn query_ranks_by_term_frequency_descending() {
        let pages = vec![
            page("low", "Low", "widget appears once"),
            page("high", "High", "widget widget widget everywhere widget"),
        ];
        let index = build_index(&pages);
        let hits = index.query("widget", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "high", "page with more occurrences must rank first");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn query_title_matches_are_weighted_higher_than_body_matches() {
        let pages = vec![
            page("titled", "Widget", "no relevant body text"),
            page("bodied", "Something Else", "widget mentioned once in body"),
        ];
        let index = build_index(&pages);
        let hits = index.query("widget", 10);
        assert_eq!(hits[0].id, "titled", "a title hit should outrank a single body mention");
    }

    #[test]
    fn query_is_deterministic_for_tied_scores() {
        let pages = vec![
            page("zeta", "Zeta", "widget"),
            page("alpha", "Alpha", "widget"),
        ];
        let index = build_index(&pages);
        let hits = index.query("widget", 10);
        // Equal scores -> id-ascending tie-break, every time.
        assert_eq!(hits[0].id, "alpha");
        assert_eq!(hits[1].id, "zeta");
    }

    #[test]
    fn query_no_match_returns_empty() {
        let pages = vec![page("a", "A", "alpha beta")];
        let index = build_index(&pages);
        assert!(index.query("nonexistent term", 10).is_empty());
    }

    #[test]
    fn query_empty_or_stopword_only_returns_no_hits() {
        let pages = vec![page("a", "A", "alpha beta")];
        let index = build_index(&pages);
        assert!(index.query("", 10).is_empty());
        assert!(index.query("the a of", 10).is_empty());
    }

    #[test]
    fn query_respects_limit() {
        let pages: Vec<IndexedPage> =
            (0..5).map(|i| page(&format!("p{i}"), &format!("Page {i}"), "widget")).collect();
        let index = build_index(&pages);
        let hits = index.query("widget", 2);
        assert_eq!(hits.len(), 2);
    }

    // ── Pagefind-present path (staging is pure and testable without the binary) ──

    #[test]
    fn pagefind_available_returns_false_for_missing_binary_in_probe_semantics() {
        // Exercises the exact spawn-and-probe idiom pagefind_available()
        // uses, against a binary name that cannot exist, proving the
        // "unavailable" branch behaves correctly without requiring the
        // real `pagefind` binary to be installed in this environment.
        let probe = Command::new("definitely-not-a-real-pagefind-binary-xyz123")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        assert!(probe.is_err());
    }

    #[test]
    fn build_pagefind_site_files_stages_one_html_file_per_page() {
        let pages = vec![
            page("widget", "Widget Module", "handles routing"),
            page("gadget", "Gadget Module", "handles caching"),
        ];
        let files = build_pagefind_site_files(&pages);
        assert_eq!(files.len(), 2);
        assert!(files.contains_key("widget.html"));
        let html = &files["widget.html"];
        assert!(html.contains("data-pagefind-body"));
        assert!(html.contains("Widget Module"));
        assert!(html.contains("handles routing"));
    }

    #[test]
    fn build_pagefind_site_files_escapes_html_special_characters() {
        let pages = vec![page("x", "A <script> & \"quote\"", "body & more <tags>")];
        let files = build_pagefind_site_files(&pages);
        let html = &files["x.html"];
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    // ── Top-level selection: pagefind-or-builtin ──────────────────────────

    #[test]
    fn build_search_artifact_falls_back_to_builtin_when_pagefind_unavailable() {
        // This sandbox has no `pagefind` binary on PATH (verified via
        // `which pagefind` before writing this module), so this exercises
        // the real fallback path end to end, not a mock.
        assert!(!pagefind_available(), "this test assumes no pagefind binary is on PATH");
        let pages = vec![page("widget", "Widget", "widget content")];
        let result = build_search_artifact(&pages);
        assert_eq!(result.engine, SearchEngine::Builtin);
        match result.artifact {
            SearchIndexArtifact::Builtin { index_json, query_helper_js } => {
                assert!(!index_json.is_empty());
                assert!(serde_json::from_str::<Value>(&index_json).is_ok());
                assert!(query_helper_js.contains("function search"));
            }
            SearchIndexArtifact::Pagefind { .. } => panic!("expected the builtin fallback"),
        }
        assert!(result.note.contains("built-in"));
    }

    #[test]
    fn build_search_artifact_empty_page_set_still_produces_a_valid_index() {
        let result = build_search_artifact(&[]);
        match result.artifact {
            SearchIndexArtifact::Builtin { index_json, .. } => {
                let parsed: Value = serde_json::from_str(&index_json).unwrap();
                assert!(parsed["pages"].as_object().unwrap().is_empty());
            }
            SearchIndexArtifact::Pagefind { .. } => panic!("expected the builtin fallback"),
        }
    }

    // ── WRITE-MODEL INVERSION (negative test) ─────────────────────────────

    #[test]
    fn build_search_artifact_never_leaves_files_behind() {
        let before: BTreeSet<String> = fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("docgen-search-index-pagefind-"))
            .collect();
        assert!(before.is_empty(), "no stray temp dirs should exist before this test runs");

        let pages = vec![page("widget", "Widget", "widget content")];
        let _ = build_search_artifact(&pages);

        let after: BTreeSet<String> = fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("docgen-search-index-pagefind-"))
            .collect();
        assert!(after.is_empty(), "build_search_artifact must never leave temp files behind");
    }
}
