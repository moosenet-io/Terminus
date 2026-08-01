//! Media domain recommendations + engagement tools (MEDIA-05) — the
//! **stateless** core of taste-driven suggestions.
//!
//! Three tools:
//! - `media_recommend` — suggests titles from the Radarr/Sonarr library the
//!   user hasn't watched yet, ranked against a taste profile built purely
//!   from recent Plex watch history (genre/director recency-weighted
//!   overlap), with a narration-friendly rationale ("because you watched
//!   Dune and Blade Runner 2049 -- sci-fi").
//! - `media_on_deck` — Plex's own continue-watching / on-deck surface.
//! - `media_recently_added` — recently-added library items (engagement
//!   surface, not personalized).
//!
//! ## STATELESS — this is the whole point of this item
//! This module makes **no call of any kind to a long-term personalization/
//! curation-memory facade**. No such client exists in this crate today (see
//! the BLUEPRINT's SPEC-TO-REALITY correction #3, which reserves that
//! integration for a later, separately-toggled item), and this module reads
//! no personalization-flag env var and imports nothing memory-shaped. It
//! computes its taste profile fresh, in-process, from the Plex watch history
//! returned by *this* call only. It MUST keep working exactly as-is with any
//! future personalization layer turned off. The unit test
//! `stateless_module_makes_no_memory_calls` (below, in `#[cfg(test)]`)
//! asserts this structurally by scanning this file's own non-doc-comment
//! source lines for memory-shaped identifiers, so a future code change that
//! introduces one will fail the test rather than silently compromising the
//! stateless guarantee. (Those identifiers are deliberately spelled out only
//! in this prose, never in code, so the scan stays meaningful.)

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool, ToolOutput};

use super::clients::plex::PlexClient;
use super::clients::radarr::RadarrClient;
use super::clients::sonarr::SonarrClient;

// ── taste profile (pure, testable) ──────────────────────────────────────────

/// Recency-weighted taste signal built from a slice of Plex history items.
/// Pure data, no I/O -- built once per `media_recommend` call from that
/// call's own history fetch, never persisted or read back from anywhere.
#[derive(Debug, Default, Clone, PartialEq)]
struct TasteProfile {
    genre_weight: HashMap<String, f64>,
    director_weight: HashMap<String, f64>,
    watched_titles: HashSet<String>,
    sample_titles_by_genre: HashMap<String, Vec<String>>,
}

fn normalize_title(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Extract a tag-shaped list from a history/library item under `key`,
/// accepting both Plex's tag-object array shape (`[{"tag": "Drama"}, ...]`)
/// and the arr-style plain-string-array shape (`["Drama", ...]`) so the same
/// helper works over both Plex history and Radarr/Sonarr library payloads.
fn extract_tags(item: &Value, key: &str) -> Vec<String> {
    let Some(arr) = item.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("tag").and_then(|t| t.as_str()).map(str::to_string))
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn item_title(item: &Value) -> Option<&str> {
    item.get("title")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("grandparentTitle").and_then(|v| v.as_str()))
        .or_else(|| item.get("name").and_then(|v| v.as_str()))
}

/// Build a taste profile from Plex history items, most-recent-first (Plex's
/// own history endpoint already orders this way; if `viewedAt` is present we
/// re-sort defensively so caller ordering can never silently invert the
/// recency weighting). Pure function, unit-tested directly with no HTTP.
fn build_taste_profile(history_items: &[Value]) -> TasteProfile {
    let mut items: Vec<&Value> = history_items.iter().collect();
    items.sort_by(|a, b| {
        let av = a.get("viewedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let bv = b.get("viewedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        bv.cmp(&av)
    });

    let mut profile = TasteProfile::default();
    for (rank, item) in items.iter().enumerate() {
        // Recency decay: the most recently watched items weigh the most, but
        // older watches still contribute a little -- taste is a trend, not
        // just "the last thing watched".
        let weight = 1.0 / (1.0 + rank as f64 * 0.25);

        if let Some(title) = item_title(item) {
            profile.watched_titles.insert(normalize_title(title));
        }

        for genre in extract_tags(item, "Genre").into_iter().chain(extract_tags(item, "genres")) {
            *profile.genre_weight.entry(genre.clone()).or_insert(0.0) += weight;
            if let Some(title) = item_title(item) {
                let bucket = profile.sample_titles_by_genre.entry(genre).or_default();
                if !bucket.iter().any(|t| t == title) {
                    bucket.push(title.to_string());
                }
            }
        }
        for director in extract_tags(item, "Director").into_iter().chain(extract_tags(item, "directors")) {
            *profile.director_weight.entry(director).or_insert(0.0) += weight;
        }
    }
    profile
}

/// Per Plex's history payload, entries carry either an `accountID` (numeric)
/// or a nested `User.id` -- either identifies which household member watched
/// something. Returns `None` when the item carries no user signal at all
/// (single-user Plex servers commonly omit it).
fn item_account_id(item: &Value) -> Option<String> {
    item.get("accountID")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .or_else(|| item.get("accountID").and_then(|v| v.as_str()).map(str::to_string))
        .or_else(|| item.get("User").and_then(|u| u.get("id")).and_then(|v| v.as_i64()).map(|n| n.to_string()))
        .or_else(|| item.get("User").and_then(|u| u.get("id")).and_then(|v| v.as_str()).map(str::to_string))
}

/// TERM #576: whose watch history, if anyone's, this call may personalise
/// from. Produced ONLY from the gateway-minted [`CallerContext`] plus the
/// caller's own argument -- never from the history payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HistoryScope {
    /// The caller's OWN household account. The only value that unlocks a
    /// taste profile, watched-title exclusion, or a "because you watched X"
    /// rationale.
    Account(String),
    /// The caller's account could not be determined, so nothing derived from
    /// any household member's history may appear in the answer. Not an
    /// error, and not a guess -- the deliberate unpersonalised path.
    Unentitled,
}

/// The refusal message for a caller asking for an account that is not theirs.
///
/// Deliberately names NEITHER the requested account nor the caller's own: a
/// refusal that echoes ids back is a probe oracle ("is 7 a real account?" /
/// "which account am I?"). It says only that the request was refused and what
/// the caller may do instead.
const FOREIGN_ACCOUNT_REFUSAL: &str =
    "account_id may only be your own media account. Omit it and I'll use yours.";

/// TERM #576 — the whole fix, in one pure, unit-testable function.
///
/// Two rules, and the second is the one the original code got wrong:
///
/// 1. **A caller-supplied `account_id` that is not the caller's own is
///    REFUSED, not ignored.** Silently substituting the caller's own account
///    would also be safe, but it hides the bug class: a parameter that looks
///    honoured and is not is exactly how "I asked for their profile and got
///    something back" turns into a leak the next time someone reorders this
///    function. An explicit refusal is auditable and can't rot into a
///    silent grant.
/// 2. **With no `account_id`, the account comes from the AUTHENTICATED
///    CALLER.** The previous fallback -- the account on the most recent
///    history entry -- made the answer depend on who watched last (normally
///    the operator) rather than on who was asking. That, not the argument,
///    was the actual defect: it leaked without the caller doing anything
///    unusual at all.
///
/// Ambiguity fails closed: an unmapped caller is [`HistoryScope::Unentitled`],
/// and an unmapped caller who names ANY account is refused (nothing can be
/// shown to belong to them, so nothing does).
pub(super) fn resolve_history_scope(explicit: Option<&str>, caller: &CallerContext) -> Result<HistoryScope, ToolError> {
    match (caller.media_account(), explicit) {
        (Some(own), None) => Ok(HistoryScope::Account(own.to_string())),
        (Some(own), Some(asked)) if asked == own => Ok(HistoryScope::Account(own.to_string())),
        (_, Some(_)) => Err(ToolError::InvalidArgument(FOREIGN_ACCOUNT_REFUSAL.into())),
        (None, None) => Ok(HistoryScope::Unentitled),
    }
}

/// Filter history items down to exactly one account's watches.
///
/// TERM #576 made this STRICT: an item carrying no user signal is DROPPED,
/// where it previously passed through. An unattributed history entry cannot
/// be shown to belong to the caller, and "cannot be shown to belong to the
/// caller" is precisely the case to fail closed on -- the cost is a thinner
/// (possibly unpersonalised) recommendation, the cost of the alternative is
/// another member's watch leaking into a profile that gets narrated back.
/// Plex emits `accountID`/`User.id` on multi-user servers; a server that
/// omits it everywhere simply lands on the thin-signal library browse.
fn filter_to_account<'a>(history_items: &'a [Value], account_id: &str) -> Vec<&'a Value> {
    history_items.iter().filter(|item| item_account_id(item).as_deref() == Some(account_id)).collect()
}

// ── recommendation scoring (pure, testable) ─────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Recommendation {
    title: String,
    media_type: String,
    score: f64,
    matched_genres: Vec<String>,
    rationale: String,
}

/// One library candidate (already-owned, not-yet-watched Radarr movie or
/// Sonarr series) available to be scored against a [`TasteProfile`].
struct LibraryCandidate {
    title: String,
    media_type: String,
    genres: Vec<String>,
    directors: Vec<String>,
}

fn extract_candidates(library_items: &[Value], media_type: &str) -> Vec<LibraryCandidate> {
    library_items
        .iter()
        .filter_map(|item| {
            let title = item_title(item)?.to_string();
            let genres = extract_tags(item, "genres").into_iter().chain(extract_tags(item, "Genre")).collect();
            let directors = extract_tags(item, "directors").into_iter().chain(extract_tags(item, "Director")).collect();
            Some(LibraryCandidate { title, media_type: media_type.to_string(), genres, directors })
        })
        .collect()
}

/// Score + rank library candidates against a taste profile. Pure function,
/// unit-tested directly. Candidates already in `profile.watched_titles` are
/// dropped -- recommendations are for things not yet watched. When every
/// candidate scores zero (no genre/director overlap at all -- e.g. a
/// brand-new taste profile with no history), returns candidates unscored in
/// their original order so the caller can fall back to a "thin signal"
/// library-browse response instead of an empty one.
fn score_recommendations(profile: &TasteProfile, candidates: &[LibraryCandidate], limit: usize) -> Vec<Recommendation> {
    let unwatched: Vec<&LibraryCandidate> =
        candidates.iter().filter(|c| !profile.watched_titles.contains(&normalize_title(&c.title))).collect();

    let mut scored: Vec<Recommendation> = unwatched
        .iter()
        .map(|c| {
            let mut score = 0.0;
            let mut matched_genres = Vec::new();
            for genre in &c.genres {
                if let Some(w) = profile.genre_weight.get(genre) {
                    score += w;
                    matched_genres.push(genre.clone());
                }
            }
            for director in &c.directors {
                if let Some(w) = profile.director_weight.get(director) {
                    // Directors are a stronger, more specific signal than a
                    // shared genre -- weight them up.
                    score += w * 1.5;
                }
            }

            // TERM #576: neutral phrasing. This is the rationale the
            // UNPERSONALISED path always lands on (empty profile => no genre
            // ever matches), so it must not assert anything about anybody's
            // watch history -- not even "not enough of it yet", which is a
            // claim about the household this caller is not entitled to.
            let rationale = if matched_genres.is_empty() {
                "in the library".to_string()
            } else {
                let examples: Vec<String> = matched_genres
                    .iter()
                    .filter_map(|g| profile.sample_titles_by_genre.get(g).and_then(|v| v.first()))
                    .take(2)
                    .cloned()
                    .collect();
                if examples.is_empty() {
                    format!("because you've been watching {}", matched_genres.join("/"))
                } else {
                    format!("because you watched {} ({})", examples.join(" and "), matched_genres.join("/"))
                }
            };

            Recommendation { title: c.title.clone(), media_type: c.media_type.clone(), score, matched_genres, rationale }
        })
        .collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// Build the narration-shaped `media_recommend` response. Pure function over
/// already-scored recommendations, unit-tested directly.
///
/// TERM #576: `personalized` is false on the unentitled path. Everything
/// history-derived is already absent by then (the profile is empty and the
/// history fetch never happened); this flag makes that visible to a caller and
/// keeps the summary from claiming anything about a household whose data the
/// caller may not see.
fn build_recommend_response(
    recommendations: &[Recommendation],
    thin_signal: bool,
    personalized: bool,
    degraded_note: Option<&str>,
) -> Value {
    let summary = if recommendations.is_empty() {
        "I don't have anything to recommend from the library right now.".to_string()
    } else if !personalized {
        // No claim about watch history in either direction -- "not much
        // history yet" would itself be a statement about the household.
        let titles: Vec<&str> = recommendations.iter().take(3).map(|r| r.title.as_str()).collect();
        format!("Here's what's in the library: {}.", titles.join(", "))
    } else if thin_signal {
        let titles: Vec<&str> = recommendations.iter().take(3).map(|r| r.title.as_str()).collect();
        format!(
            "I don't have much watch history to go on yet, so here's what's in your library: {}.",
            titles.join(", ")
        )
    } else {
        let top = &recommendations[0];
        format!("You might like \"{}\" -- {}.", top.title, top.rationale)
    };

    let summary = match degraded_note {
        Some(note) => format!("{summary} ({note})"),
        None => summary,
    };

    json!({
        "summary": summary,
        "structured": {
            "thin_signal": thin_signal,
            "personalized": personalized,
            "degraded": degraded_note,
            "recommendations": recommendations.iter().map(|r| json!({
                "title": r.title,
                "media_type": r.media_type,
                "score": (r.score * 1000.0).round() / 1000.0,
                "matched_genres": r.matched_genres,
                "rationale": r.rationale,
            })).collect::<Vec<_>>(),
        }
    })
}

pub struct MediaRecommend {
    plex: Option<PlexClient>,
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
}

impl MediaRecommend {
    const DEFAULT_LIMIT: usize = 5;

    /// Build a stateless `MediaRecommend` from env, identically to
    /// [`register`] below. Exposed so a decorator in a sibling module (see
    /// `crate::media::taste_memory`) can wrap this exact stateless tool
    /// rather than reimplementing its client construction -- this is a
    /// plain constructor, not a memory dependency, so it does not affect
    /// the `stateless_module_makes_no_memory_calls` guarantee above.
    pub fn from_env() -> Self {
        Self { plex: PlexClient::from_env().ok(), radarr: RadarrClient::from_env().ok(), sonarr: SonarrClient::from_env().ok() }
    }
}

#[async_trait]
impl RustTool for MediaRecommend {
    fn name(&self) -> &str {
        "media_recommend"
    }

    fn description(&self) -> &str {
        "Suggest movies/shows already in the library that haven't been watched yet, ranked by a taste profile built fresh from recent Plex watch history (genre/director overlap, recency-weighted). Each suggestion carries a narration-friendly rationale, e.g. \"because you watched Dune (sci-fi)\". Stateless -- computed newly each call from this call's own Plex history, no persisted taste memory. Falls back to a plain library browse (noting thin signal) for a new/sparse-history user, and degrades to arr-only library picks (noting Plex is unreachable) rather than failing." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of recommendations to return (default 5)."
                },
                "account_id": {
                    "type": "string",
                    "description": "Optional: your OWN media account id. It may not name another household member's account -- doing so is refused, not silently ignored. Omit it and your own account is used."
                }
            }
        })
    }

    /// TERM #576: `execute` carries NO caller identity, so it is the
    /// fail-closed entry point -- it resolves as [`CallerContext::untrusted`],
    /// i.e. an unmapped caller, and therefore reads NOBODY's watch history.
    /// An authorized dispatch path supplies the real caller via
    /// `execute_with_caller`.
    #[instrument(skip(self, args), fields(tool = "media_recommend"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted()).await
    }

    /// The authorized path: the gateway resolved which household account this
    /// principal IS, from the same server-verified identity it authorized the
    /// call with.
    async fn execute_with_caller(&self, args: Value, caller: CallerContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text_only(self.run(args, caller).await?))
    }
}

impl MediaRecommend {
    pub(super) async fn run(&self, args: Value, caller: CallerContext) -> Result<String, ToolError> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(Self::DEFAULT_LIMIT).max(1);
        let account_id = args.get("account_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());

        // TERM #576: decide WHOSE history (if anyone's) before touching Plex.
        // A refusal short-circuits here, so a request for another member's
        // profile never reaches the history endpoint at all.
        let scope = resolve_history_scope(account_id, &caller)?;

        // History fetch degrades to an empty profile (cold-start path) rather
        // than failing the whole recommendation -- a library-only fallback
        // still beats a hard error.
        //
        // TERM #576: on the unentitled path the fetch is SKIPPED, not merely
        // filtered afterwards. Same reasoning as the weather calendar gate: an
        // unentitled caller must not cause a read of household data at all, so
        // the data never enters the process on their behalf and cannot leak
        // through a later refactor of the filtering.
        let (history_items, plex_note): (Vec<Value>, Option<&str>) = match (&scope, &self.plex) {
            (HistoryScope::Unentitled, _) | (_, None) => (Vec::new(), None),
            (HistoryScope::Account(_), Some(client)) => match client.history().await {
                Ok(v) => {
                    let items = v
                        .get("MediaContainer")
                        .and_then(|m| m.get("Metadata"))
                        .and_then(|m| m.as_array())
                        .cloned()
                        .or_else(|| v.as_array().cloned())
                        .unwrap_or_default();
                    (items, None)
                }
                Err(_) => (Vec::new(), Some("couldn't reach Plex, so this isn't personalized right now")),
            },
        };

        let (scoped, personalized): (Vec<Value>, bool) = match &scope {
            HistoryScope::Account(id) => {
                (filter_to_account(&history_items, id).into_iter().cloned().collect(), true)
            }
            HistoryScope::Unentitled => (Vec::new(), false),
        };

        let profile = build_taste_profile(&scoped);

        let mut candidates = Vec::new();
        let mut library_note = plex_note;
        match &self.radarr {
            Some(c) => match c.library().await {
                Ok(v) => candidates.extend(extract_candidates(&v.as_array().cloned().unwrap_or_default(), "movie")),
                Err(_) if library_note.is_none() => library_note = Some("couldn't reach Radarr for movie picks"),
                Err(_) => {}
            },
            None => {}
        }
        match &self.sonarr {
            Some(c) => match c.library().await {
                Ok(v) => candidates.extend(extract_candidates(&v.as_array().cloned().unwrap_or_default(), "tv")),
                Err(_) if library_note.is_none() => library_note = Some("couldn't reach Sonarr for show picks"),
                Err(_) => {}
            },
            None => {}
        }

        let recommendations = score_recommendations(&profile, &candidates, limit);
        let has_signal = recommendations.iter().any(|r| !r.matched_genres.is_empty());
        let thin_signal = personalized && !recommendations.is_empty() && !has_signal;

        Ok(build_recommend_response(&recommendations, thin_signal, personalized, library_note).to_string())
    }
}

// ── media_on_deck / media_recently_added (engagement, thin passthrough) ────

fn extract_engagement_items(raw: &Value) -> Vec<Value> {
    raw.get("MediaContainer")
        .and_then(|m| m.get("Metadata"))
        .and_then(|m| m.as_array())
        .cloned()
        .or_else(|| raw.as_array().cloned())
        .unwrap_or_default()
}

/// Build the narration-shaped response shared by `media_on_deck` and
/// `media_recently_added`. Pure function, unit-tested directly.
fn build_engagement_response(kind: &str, items: &[Value]) -> Value {
    let titles: Vec<String> = items.iter().filter_map(|i| item_title(i).map(str::to_string)).collect();
    let summary = if titles.is_empty() {
        match kind {
            "on_deck" => "Nothing on deck right now.".to_string(),
            _ => "Nothing recently added.".to_string(),
        }
    } else {
        let shown: Vec<&str> = titles.iter().take(5).map(String::as_str).collect();
        match kind {
            "on_deck" => format!("Up next: {}.", shown.join(", ")),
            _ => format!("Recently added: {}.", shown.join(", ")),
        }
    };
    json!({
        "summary": summary,
        "structured": {
            "count": titles.len(),
            "titles": titles,
        }
    })
}

/// TERM #576: what `media_on_deck` says to a caller it may not show the
/// continue-watching row to.
///
/// Constant, and constant is the point -- it is byte-identical whether the
/// household has forty in-progress episodes or none, so it fingerprints
/// nothing. Returned as `Ok`, not `Err`, following the `weather` ASK
/// precedent: a plain-language "not something I can show you" is a valid final
/// answer, whereas an error invites the model to retry with arguments.
const ON_DECK_WITHHELD: &str = "Continue-watching is personal to each household member, and I can't tell which one you are, so I'm not showing it.";

pub struct MediaOnDeck {
    plex: Option<PlexClient>,
}

impl MediaOnDeck {
    fn require_client(&self) -> Result<&PlexClient, ToolError> {
        self.plex.as_ref().ok_or_else(|| ToolError::NotConfigured("PLEX_URL/PLEX_TOKEN not set".into()))
    }

    /// TERM #576: may this caller see the on-deck row?
    ///
    /// `GET /library/onDeck` takes no account parameter (see
    /// [`PlexClient::on_deck`]) -- Plex answers it for whichever account the
    /// configured `PLEX_TOKEN` belongs to, and there is no per-account variant
    /// to call instead. So the row cannot be SCOPED to an arbitrary caller;
    /// the only honest rule is to disclose it to the one account it actually
    /// is, named by `PLEX_ACCOUNT_ID`.
    ///
    /// Fail-closed twice over: an unmapped caller is nobody, and an unset
    /// `PLEX_ACCOUNT_ID` means the row belongs to nobody we can name, so it
    /// goes to nobody. (Operator note: this means `PLEX_ACCOUNT_ID` must be
    /// set for the operator's own on-deck surface to keep working. Withholding
    /// household state until someone says whose it is is the correct default
    /// for a tool that guests can reach.)
    fn caller_may_see(caller: &CallerContext) -> bool {
        match (caller.media_account(), super::account_map::plex_token_account()) {
            (Some(caller_account), Some(token_account)) => caller_account == token_account,
            _ => false,
        }
    }
}

#[async_trait]
impl RustTool for MediaOnDeck {
    fn name(&self) -> &str {
        "media_on_deck"
    }

    fn description(&self) -> &str {
        "List what's currently on deck / continue-watching in Plex -- in-progress episodes and next-up in a show. Read-only." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    /// TERM #576: no caller identity => nobody => withheld, and Plex is never
    /// contacted on an unidentified caller's behalf.
    #[instrument(skip(self, args), fields(tool = "media_on_deck"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted()).await
    }

    async fn execute_with_caller(&self, args: Value, caller: CallerContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text_only(self.run(args, caller).await?))
    }
}

impl MediaOnDeck {
    async fn run(&self, _args: Value, caller: CallerContext) -> Result<String, ToolError> {
        if !Self::caller_may_see(&caller) {
            // Withheld BEFORE `require_client`, so an unentitled caller cannot
            // even distinguish "Plex isn't configured" from "not for you", and
            // before any network call, so no household read happens for them.
            return Ok(json!({
                "summary": ON_DECK_WITHHELD,
                "structured": { "personalized": false, "count": 0, "titles": [] }
            })
            .to_string());
        }
        let client = self.require_client()?;
        let raw = client.on_deck().await?;
        let items = extract_engagement_items(&raw);
        let mut out = build_engagement_response("on_deck", &items);
        out["structured"]["personalized"] = json!(true);
        Ok(out.to_string())
    }
}

pub struct MediaRecentlyAdded {
    plex: Option<PlexClient>,
}

impl MediaRecentlyAdded {
    fn require_client(&self) -> Result<&PlexClient, ToolError> {
        self.plex.as_ref().ok_or_else(|| ToolError::NotConfigured("PLEX_URL/PLEX_TOKEN not set".into()))
    }
}

#[async_trait]
impl RustTool for MediaRecentlyAdded {
    fn name(&self) -> &str {
        "media_recently_added"
    }

    fn description(&self) -> &str {
        "List titles recently added to the Plex library. Read-only, not personalized -- pairs with media_recommend for a fuller \"what's new / what might I like\" surface." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    #[instrument(skip(self, _args), fields(tool = "media_recently_added"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;
        let raw = client.recently_added().await?;
        let items = extract_engagement_items(&raw);
        Ok(build_engagement_response("recently_added", &items).to_string())
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Register the MEDIA-05 recommend/engagement tools. Each client is built
/// independently from its own env config, matching MEDIA-02's degradation
/// pattern -- an unconfigured/unreachable service disables/degrades only its
/// own contribution, never the whole domain.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaRecommend::from_env()));
    registry.register_or_replace(Box::new(MediaOnDeck { plex: PlexClient::from_env().ok() }));
    registry.register_or_replace(Box::new(MediaRecentlyAdded { plex: PlexClient::from_env().ok() }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    // ── TERM #576 caller fixtures ───────────────────────────────────────────
    //
    // Placeholder account ids only -- these are not real household accounts.

    /// The household member doing the asking in the positive-control tests.
    const OPERATOR_ACCOUNT: &str = "acct-operator";
    /// A DIFFERENT household member, whose data must never reach a guest.
    const OTHER_MEMBER_ACCOUNT: &str = "acct-other";

    /// A caller the gateway resolved to their own media account.
    fn caller_for(account: &str) -> CallerContext {
        CallerContext::with_media_account_for_test_only(account)
    }

    /// A guest: authenticated, but bound to no media account. Identical to the
    /// "caller account undeterminable" case, which is the point -- an unmapped
    /// caller and an unidentifiable one are the SAME unentitled path.
    fn guest() -> CallerContext {
        CallerContext::untrusted()
    }

    fn plex_client(base_url: &str) -> PlexClient {
        PlexClient::new(base_url, "t", reqwest::Client::new())
    }
    fn radarr_client(base_url: &str) -> RadarrClient {
        RadarrClient::new(base_url, "k", reqwest::Client::new())
    }
    fn sonarr_client(base_url: &str) -> SonarrClient {
        SonarrClient::new(base_url, "k", reqwest::Client::new())
    }

    // ── STATELESS structural assertion ──────────────────────────────────────

    /// This is the load-bearing MEDIA-05 negative test: scans THIS file's own
    /// source for any identifier that would indicate a memory/taste-
    /// persistence dependency. MEDIA-06 (not built yet) is the only place a
    /// memory facade is allowed to appear; if a future edit to this file
    /// introduces one, this test fails loudly instead of silently breaking
    /// the "works with memory OFF" guarantee.
    #[test]
    fn stateless_module_makes_no_memory_calls() {
        let source = include_str!("recommend.rs");
        // Scan only the PRODUCTION code above `#[cfg(test)]`: doc-comment
        // prose (`//!`/`///`) is allowed to explain the stateless guarantee
        // in words (as the module doc above does), and this very test
        // function necessarily names the forbidden identifiers as string
        // literals to check for them -- neither of those is a memory
        // dependency. It is production code -- an import, a call, an env
        // var read -- above the test module that would compromise it.
        let production_source = source.split("#[cfg(test)]").next().unwrap_or(source);
        let code_lines: String = production_source
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//!") && !trimmed.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for forbidden in ["Engram", "engram", "MEDIA_TASTE", "taste_memory", "TasteMemory"] {
            assert!(
                !code_lines.contains(forbidden),
                "media/recommend.rs (MEDIA-05, stateless) must not reference '{forbidden}' \
                 in production code -- that belongs in MEDIA-06's taste-memory toggle, not here"
            );
        }
    }

    /// Functional companion to the structural test above: build a full
    /// recommendation end-to-end (mocked Plex + Radarr + Sonarr only, no
    /// other service) and confirm it produces a real, non-empty, rationale-
    /// bearing result -- i.e. the stateless path is not just absent of
    /// memory calls, it is actually sufficient on its own.
    #[tokio::test]
    async fn media_recommend_works_fully_with_only_plex_and_arr_clients() {
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [
                    { "title": "Dune", "Genre": [{"tag": "Science Fiction"}], "viewedAt": 2000, "accountID": OPERATOR_ACCOUNT },
                    { "title": "Blade Runner 2049", "Genre": [{"tag": "Science Fiction"}], "viewedAt": 1000, "accountID": OPERATOR_ACCOUNT }
                ] }
            }));
        });
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([
                { "title": "Arrival", "genres": ["Science Fiction"] },
                { "title": "The Great British Bake Off Movie", "genres": ["Comedy"] }
            ]));
        });

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["thin_signal"], false);
        let recs = parsed["structured"]["recommendations"].as_array().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0]["title"], "Arrival");
        assert!(recs[0]["rationale"].as_str().unwrap().contains("Dune"));
    }

    // ── taste profile / scoring pure-logic tests ────────────────────────────

    #[test]
    fn build_taste_profile_weights_recent_watches_more() {
        let history = json!([
            { "title": "Old Movie", "Genre": [{"tag": "Drama"}], "viewedAt": 100 },
            { "title": "New Movie", "Genre": [{"tag": "Comedy"}], "viewedAt": 999999 }
        ]);
        let profile = build_taste_profile(history.as_array().unwrap());
        assert!(profile.genre_weight["Comedy"] > profile.genre_weight["Drama"]);
    }

    #[test]
    fn build_taste_profile_tracks_watched_titles() {
        let history = json!([{ "title": "Dune", "Genre": [] }]);
        let profile = build_taste_profile(history.as_array().unwrap());
        assert!(profile.watched_titles.contains("dune"));
    }

    #[test]
    fn score_recommendations_excludes_already_watched() {
        let mut profile = TasteProfile::default();
        profile.watched_titles.insert("arrival".to_string());
        profile.genre_weight.insert("Science Fiction".to_string(), 1.0);
        let candidates = vec![
            LibraryCandidate { title: "Arrival".into(), media_type: "movie".into(), genres: vec!["Science Fiction".into()], directors: vec![] },
            LibraryCandidate { title: "Dune".into(), media_type: "movie".into(), genres: vec!["Science Fiction".into()], directors: vec![] },
        ];
        let recs = score_recommendations(&profile, &candidates, 10);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].title, "Dune");
    }

    #[test]
    fn score_recommendations_ranks_genre_overlap_above_no_overlap() {
        let mut profile = TasteProfile::default();
        profile.genre_weight.insert("Science Fiction".to_string(), 2.0);
        let candidates = vec![
            LibraryCandidate { title: "Cooking Show".into(), media_type: "tv".into(), genres: vec!["Food".into()], directors: vec![] },
            LibraryCandidate { title: "Dune".into(), media_type: "movie".into(), genres: vec!["Science Fiction".into()], directors: vec![] },
        ];
        let recs = score_recommendations(&profile, &candidates, 10);
        assert_eq!(recs[0].title, "Dune");
        assert!(recs[0].score > recs[1].score);
    }

    #[test]
    fn score_recommendations_director_match_outweighs_genre_only() {
        let mut profile = TasteProfile::default();
        profile.genre_weight.insert("Science Fiction".to_string(), 1.0);
        profile.director_weight.insert("Denis Villeneuve".to_string(), 1.0);
        let candidates = vec![
            LibraryCandidate { title: "Generic Sci-Fi".into(), media_type: "movie".into(), genres: vec!["Science Fiction".into()], directors: vec![] },
            LibraryCandidate {
                title: "Dune".into(),
                media_type: "movie".into(),
                genres: vec!["Science Fiction".into()],
                directors: vec!["Denis Villeneuve".into()],
            },
        ];
        let recs = score_recommendations(&profile, &candidates, 10);
        assert_eq!(recs[0].title, "Dune");
    }

    // ── edge cases ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn media_recommend_sparse_history_falls_back_to_library_with_thin_signal_note() {
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [] } }));
        });
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Arrival", "genres": ["Science Fiction"] }]));
        });

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["thin_signal"], true);
        assert_eq!(parsed["structured"]["personalized"], true);
        assert!(parsed["summary"].as_str().unwrap().to_lowercase().contains("library"));
    }

    #[tokio::test]
    async fn media_recommend_plex_unreachable_degrades_to_arr_trending_not_panic() {
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(500);
        });
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Arrival", "genres": ["Science Fiction"] }]));
        });

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await;
        assert!(result.is_ok(), "Plex being unreachable must degrade, not fail the tool");
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(parsed["structured"]["degraded"].as_str().unwrap().to_lowercase().contains("plex"));
        // Still returns library picks despite Plex being down.
        assert!(!parsed["structured"]["recommendations"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn media_recommend_no_services_configured_returns_friendly_empty_not_error() {
        let tool = MediaRecommend { plex: None, radarr: None, sonarr: None };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await;
        assert!(result.is_ok());
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["recommendations"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn multi_user_history_is_scoped_to_one_account_not_blended() {
        let history = json!([
            { "title": "Kids Show", "Genre": [{"tag": "Animation"}], "accountID": OTHER_MEMBER_ACCOUNT, "viewedAt": 500 },
            { "title": "Dune", "Genre": [{"tag": "Science Fiction"}], "accountID": OPERATOR_ACCOUNT, "viewedAt": 1000 }
        ]);
        let items: Vec<Value> = history.as_array().unwrap().to_vec();
        let scoped = filter_to_account(&items, OPERATOR_ACCOUNT);
        assert_eq!(scoped.len(), 1);
        let profile = build_taste_profile(&scoped.into_iter().cloned().collect::<Vec<_>>());
        assert!(profile.genre_weight.contains_key("Science Fiction"));
        assert!(!profile.genre_weight.contains_key("Animation"), "the other account's genre must not leak in");
    }

    /// TERM #576: an unattributable history entry is DROPPED when scoping,
    /// where it used to pass through. It cannot be shown to belong to the
    /// caller, and this tool's whole job is to only use what does.
    #[test]
    fn history_without_an_account_signal_is_dropped_when_scoping() {
        let history = json!([
            { "title": "Dune", "Genre": [{"tag": "Science Fiction"}], "accountID": OPERATOR_ACCOUNT },
            { "title": "Unattributed", "Genre": [{"tag": "Animation"}] }
        ]);
        let items: Vec<Value> = history.as_array().unwrap().to_vec();
        let scoped = filter_to_account(&items, OPERATOR_ACCOUNT);
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0]["title"], "Dune");
    }

    // ── TERM #576: scope resolution (pure) ──────────────────────────────────

    #[test]
    fn scope_with_no_argument_comes_from_the_caller_not_the_history() {
        // The defect: this used to be "whoever watched last".
        assert_eq!(
            resolve_history_scope(None, &caller_for(OPERATOR_ACCOUNT)).unwrap(),
            HistoryScope::Account(OPERATOR_ACCOUNT.to_string())
        );
    }

    #[test]
    fn scope_naming_your_own_account_is_allowed() {
        assert_eq!(
            resolve_history_scope(Some(OPERATOR_ACCOUNT), &caller_for(OPERATOR_ACCOUNT)).unwrap(),
            HistoryScope::Account(OPERATOR_ACCOUNT.to_string())
        );
    }

    #[test]
    fn scope_naming_another_members_account_is_refused_not_ignored() {
        let err = resolve_history_scope(Some(OTHER_MEMBER_ACCOUNT), &caller_for(OPERATOR_ACCOUNT)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        // The refusal must not echo either account id back -- no probe oracle.
        let msg = err.to_string();
        assert!(!msg.contains(OTHER_MEMBER_ACCOUNT) && !msg.contains(OPERATOR_ACCOUNT));
    }

    #[test]
    fn scope_for_an_unmapped_caller_is_unentitled_and_any_account_id_is_refused() {
        assert_eq!(resolve_history_scope(None, &guest()).unwrap(), HistoryScope::Unentitled);
        assert!(matches!(
            resolve_history_scope(Some(OTHER_MEMBER_ACCOUNT), &guest()),
            Err(ToolError::InvalidArgument(_))
        ));
        // ...including an id that happens to be nobody's: an unmapped caller
        // owns no account, so no account_id can be theirs.
        assert!(matches!(resolve_history_scope(Some("acct-anything"), &guest()), Err(ToolError::InvalidArgument(_))));
    }

    // ── TERM #576: end-to-end content assertions ────────────────────────────

    /// The household fixture both guest tests share: two members, and a
    /// distinctive title watched only by the OTHER member.
    fn household_plex_server() -> MockServer {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [
                    { "title": "Placeholder Other-Member Title", "Genre": [{"tag": "Animation"}],
                      "accountID": OTHER_MEMBER_ACCOUNT, "viewedAt": 9000 },
                    { "title": "Placeholder Operator Title", "Genre": [{"tag": "Science Fiction"}],
                      "accountID": OPERATOR_ACCOUNT, "viewedAt": 100 }
                ] }
            }));
        });
        server
    }

    fn household_radarr_server() -> MockServer {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([
                { "title": "Placeholder Library Movie", "genres": ["Animation"] }
            ]));
        });
        server
    }

    /// GUEST, no `account_id`: no household-derived personalisation, and ZERO
    /// reads of the household watch history (asserted by hit count on the
    /// mock, which is the counting source).
    #[tokio::test]
    async fn guest_with_no_account_id_gets_no_personalisation_and_reads_no_history() {
        let plex_server = MockServer::start();
        let history_mock = plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [
                    { "title": "Placeholder Other-Member Title", "Genre": [{"tag": "Animation"}],
                      "accountID": OTHER_MEMBER_ACCOUNT, "viewedAt": 9000 }
                ] }
            }));
        });
        let radarr_server = household_radarr_server();

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({}), guest()).await.unwrap();

        assert_eq!(history_mock.hits(), 0, "an unentitled caller must not cause ANY read of household watch history");

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["personalized"], false);
        // Content assertion: nothing drawn from another account survives.
        assert!(!result.contains("Placeholder Other-Member Title"));
        assert!(!result.contains(OTHER_MEMBER_ACCOUNT));
        assert!(!result.to_lowercase().contains("because you watched"));
        for rec in parsed["structured"]["recommendations"].as_array().unwrap() {
            assert!(rec["matched_genres"].as_array().unwrap().is_empty());
            assert_eq!(rec["score"], 0.0);
        }
    }

    /// GUEST naming ANOTHER member's account: refused, and that member's
    /// titles never appear -- because the refusal happens before Plex is
    /// contacted at all.
    #[tokio::test]
    async fn guest_requesting_another_members_account_is_refused_and_leaks_nothing() {
        let plex_server = household_plex_server();
        let history_mock = plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [] } }));
        });
        let radarr_server = household_radarr_server();

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({ "account_id": OTHER_MEMBER_ACCOUNT }), guest()).await;

        let err = result.expect_err("naming another account must be refused, not silently honoured");
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        let msg = err.to_string();
        assert!(!msg.contains("Placeholder Other-Member Title"));
        assert!(!msg.contains(OTHER_MEMBER_ACCOUNT));
        assert_eq!(history_mock.hits(), 0, "a refused request must not read history either");
    }

    /// An ENTITLED member naming SOMEONE ELSE's account is refused too --
    /// the guard is "your own account", not "any authenticated caller".
    #[tokio::test]
    async fn entitled_member_requesting_another_members_account_is_refused() {
        let tool = MediaRecommend { plex: None, radarr: None, sonarr: None };
        let result = tool.run(json!({ "account_id": OTHER_MEMBER_ACCOUNT }), caller_for(OPERATOR_ACCOUNT)).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    /// POSITIVE CONTROL: the operator with no `account_id` still gets THEIR
    /// OWN profile, attributed. Proves the fix scoped personalisation rather
    /// than disabling it for everyone -- and that the other member's watch
    /// still does not bleed in even though it is more recent (which is
    /// exactly what the old most-recent-entry fallback would have picked).
    #[tokio::test]
    async fn operator_with_no_account_id_still_gets_their_own_profile_attributed() {
        let plex_server = household_plex_server();
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([
                { "title": "Placeholder Sci-Fi Pick", "genres": ["Science Fiction"] },
                { "title": "Placeholder Animated Pick", "genres": ["Animation"] }
            ]));
        });

        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["structured"]["personalized"], true);
        let recs = parsed["structured"]["recommendations"].as_array().unwrap();
        assert_eq!(recs[0]["title"], "Placeholder Sci-Fi Pick", "the operator's OWN genre must rank first");
        assert!(recs[0]["rationale"].as_str().unwrap().contains("Placeholder Operator Title"));
        // The other member's more-recent Animation watch must not have been
        // picked up -- that is the old most-recent-entry defect.
        assert!(!result.contains("Placeholder Other-Member Title"));
    }

    /// The undeterminable-caller case, spelled out: `execute` (no caller at
    /// all) is the SAME unentitled path, not a privileged internal one.
    #[tokio::test]
    async fn execute_without_a_caller_is_the_unentitled_path() {
        let plex_server = household_plex_server();
        let radarr_server = household_radarr_server();
        let tool = MediaRecommend {
            plex: Some(plex_client(&plex_server.base_url())),
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
        };
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["personalized"], false);
        assert!(!result.contains("Placeholder Other-Member Title"));
    }

    // ── media_on_deck / media_recently_added ────────────────────────────────

    /// TERM #576 on-deck fixtures. `PLEX_ACCOUNT_ID` names the account the
    /// configured token speaks for; `media_on_deck` discloses the row to that
    /// account and nobody else.
    fn set_token_account(account: Option<&str>) {
        match account {
            Some(a) => std::env::set_var(super::super::account_map::TOKEN_ACCOUNT_ENV, a),
            None => std::env::remove_var(super::super::account_map::TOKEN_ACCOUNT_ENV),
        }
    }

    #[tokio::test]
    #[serial]
    async fn media_on_deck_returns_current_items_for_the_account_it_belongs_to() {
        set_token_account(Some(OPERATOR_ACCOUNT));
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [{ "title": "Placeholder On Deck Title" }] } }));
        });
        let tool = MediaOnDeck { plex: Some(plex_client(&server.base_url())) };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["titles"][0], "Placeholder On Deck Title");
        assert_eq!(parsed["structured"]["personalized"], true);
        set_token_account(None);
    }

    /// TERM #576: a guest gets NO continue-watching, and Plex is never asked.
    #[tokio::test]
    #[serial]
    async fn media_on_deck_withholds_household_state_from_a_guest() {
        set_token_account(Some(OPERATOR_ACCOUNT));
        let server = MockServer::start();
        let on_deck_mock = server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [{ "title": "Placeholder On Deck Title" }] } }));
        });
        let tool = MediaOnDeck { plex: Some(plex_client(&server.base_url())) };
        let result = tool.run(json!({}), guest()).await.unwrap();

        assert_eq!(on_deck_mock.hits(), 0, "an unentitled caller must not cause an on-deck read");
        assert!(!result.contains("Placeholder On Deck Title"));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["personalized"], false);
        assert_eq!(parsed["structured"]["count"], 0);
        assert!(parsed["structured"]["titles"].as_array().unwrap().is_empty());
        set_token_account(None);
    }

    /// A household member who is NOT the token's account gets nothing either:
    /// the row is the token owner's, and there is no per-account variant of
    /// the Plex endpoint to answer them with.
    #[tokio::test]
    #[serial]
    async fn media_on_deck_withholds_from_a_different_household_member() {
        set_token_account(Some(OPERATOR_ACCOUNT));
        let server = MockServer::start();
        let on_deck_mock = server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [{ "title": "Placeholder On Deck Title" }] } }));
        });
        let tool = MediaOnDeck { plex: Some(plex_client(&server.base_url())) };
        let result = tool.run(json!({}), caller_for(OTHER_MEMBER_ACCOUNT)).await.unwrap();
        assert_eq!(on_deck_mock.hits(), 0);
        assert!(!result.contains("Placeholder On Deck Title"));
        set_token_account(None);
    }

    /// Fail-closed on config: `PLEX_ACCOUNT_ID` unset means the row belongs to
    /// nobody we can name, so it goes to nobody -- not to everybody.
    #[tokio::test]
    #[serial]
    async fn media_on_deck_withholds_when_the_token_account_is_unconfigured() {
        set_token_account(None);
        let server = MockServer::start();
        let on_deck_mock = server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [{ "title": "Placeholder On Deck Title" }] } }));
        });
        let tool = MediaOnDeck { plex: Some(plex_client(&server.base_url())) };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await.unwrap();
        assert_eq!(on_deck_mock.hits(), 0);
        assert!(!result.contains("Placeholder On Deck Title"));
    }

    #[tokio::test]
    #[serial]
    async fn media_on_deck_not_configured_for_an_entitled_caller_without_client() {
        set_token_account(Some(OPERATOR_ACCOUNT));
        let tool = MediaOnDeck { plex: None };
        let result = tool.run(json!({}), caller_for(OPERATOR_ACCOUNT)).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
        set_token_account(None);
    }

    /// ...but an unentitled caller can't even tell that apart: the withheld
    /// answer comes back before `require_client` runs.
    #[tokio::test]
    #[serial]
    async fn media_on_deck_withheld_answer_does_not_reveal_configuration_state() {
        set_token_account(Some(OPERATOR_ACCOUNT));
        let configured = MockServer::start();
        configured.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [] } }));
        });
        let with_client = MediaOnDeck { plex: Some(plex_client(&configured.base_url())) };
        let without_client = MediaOnDeck { plex: None };
        assert_eq!(
            with_client.run(json!({}), guest()).await.unwrap(),
            without_client.run(json!({}), guest()).await.unwrap()
        );
        set_token_account(None);
    }

    #[tokio::test]
    async fn media_recently_added_returns_new_items() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/recentlyAdded");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [{ "title": "New Arrival" }] } }));
        });
        let tool = MediaRecentlyAdded { plex: Some(plex_client(&server.base_url())) };
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["titles"][0], "New Arrival");
    }

    #[tokio::test]
    async fn media_recently_added_empty_is_friendly_not_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/recentlyAdded");
            then.status(200).json_body(json!({ "MediaContainer": { "Metadata": [] } }));
        });
        let tool = MediaRecentlyAdded { plex: Some(plex_client(&server.base_url())) };
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["count"], 0);
        assert!(parsed["summary"].as_str().unwrap().to_lowercase().contains("nothing"));
    }

    #[test]
    fn tool_metadata_is_valid() {
        let recommend = MediaRecommend { plex: None, radarr: None, sonarr: None };
        assert_eq!(recommend.name(), "media_recommend");
        assert!(!recommend.description().is_empty());
        assert_eq!(recommend.parameters()["type"], "object");

        let on_deck = MediaOnDeck { plex: None };
        assert_eq!(on_deck.name(), "media_on_deck");
        assert!(!on_deck.description().is_empty());

        let recently_added = MediaRecentlyAdded { plex: None };
        assert_eq!(recently_added.name(), "media_recently_added");
        assert!(!recently_added.description().is_empty());
    }
}
