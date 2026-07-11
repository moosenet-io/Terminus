# Media — sovereign media-stack orchestration

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

**Status: complete (MEDIA-01 through MEDIA-08, S94).** The media domain
(`src/media/mod.rs`) registers ten tools, always-on: `media_domain_status` (MEDIA-01),
`media_search`/`media_status` (MEDIA-02, read), `media_request` (MEDIA-03, tiered
mutation), `media_organize`/`media_delete`/`media_cleanup` (MEDIA-04, tiered + hard-gated
destructive), and `media_recommend`/`media_on_deck`/`media_recently_added` (MEDIA-05, read,
stateless). An eleventh tool, `media_taste_feedback`, registers only when the MEDIA-06 taste-
memory toggle is on. MEDIA-07 (`src/media/surface.rs`) adds no new tool — it's the intent-
routing + confirmation-narration layer that makes Lumina the conversational surface for
everything above. This page (MEDIA-08) is the consolidated reference for the whole domain.

## Overview

The media domain orchestrates the self-hosted media stack directly — Radarr, Sonarr,
Prowlarr, qtor (download client), Plex, <media-service>, and TMDb — rather than wrapping a single
thin API. It is a **sovereign** build: every credential comes from the vault/env at runtime
(`vault::manager().get()` / documented env-var pairs, never a literal), there is no
third-party MCP server in the loop, and everything routes through this one hardened
terminus-rs hub, the same as every other domain in this crate. The one external network call
in the whole domain is TMDb title resolution (fuzzy title → real ID); no PII crosses that
boundary.

Lumina (the personality agent) is the conversational surface, not a wrapper the operator
talks to directly. `src/media/surface.rs` (MEDIA-07) routes fuzzy intent ("put something on")
to the right tool or tool chain, and turns a tool's raw JSON into an in-voice line Lumina
actually says — including the confirmation prompts that gate every mutation. The domain's
tools are the muscle; Lumina is the mouth. See [Lumina surface wiring](#lumina-surface-wiring-media-07--conversational-contract-no-new-mutation-logic) below.

## The stack it orchestrates

Seven services, each with one job:

| Service | Role |
|---|---|
| **Radarr** | Movie library manager — add/track movies, kick off indexer search + grab, hold quality profile + root folder. |
| **Sonarr** | The Radarr equivalent for TV — series/season/episode adds, monitoring, quality profile + root folder. |
| **Prowlarr** | Indexer aggregation feeding Radarr/Sonarr's searches (client exists from MEDIA-01; no dedicated tool calls it directly yet — Radarr/Sonarr call it internally). |
| **qtor** | The download client Radarr/Sonarr hand a grab to once an indexer hit is found; this domain never talks to it for acquisition directly, only status/queue. |
| **Plex** | The library/consumption layer — what's actually playable, watch history, on-deck, recently-added. The source of truth for "have I seen this" and for `media_recommend`'s taste signal. |
| **<media-service>** | Request-tracking + discovery, **secondary** — `media_request` best-effort mirrors a grab into <media-service> for visibility, but Radarr/Sonarr/qtor do the real acquisition work. Also the pre-existing read-only [`<media-service>`](<media-service>.md) tool module. |
| **TMDb** | Title resolution — the one external (non-LAN) call, turning a fuzzy natural-language title into real movie/TV IDs that Radarr/Sonarr/Plex can be queried or driven with. |

Each service is configured independently (own env-var pair, own client) — one service being
absent or unreachable degrades only the tools that need it; the rest of the domain keeps
working. See [Client shape](#client-shape-srcmediaclients) below.

## Mutation-safety tiers

Every mutation in the domain is classified into one of four tiers before any write happens.
The tier decision is a pure, unit-tested function of the request shape — never a judgment
call made mid-flight:

| Tier | Fires on | Example |
|---|---|---|
| **Read** | No tier — nothing ever mutates. | `media_search "dune"`, `media_status "Dune"`, `media_recommend`, `media_on_deck` — pure lookups, always safe to call. |
| **Light** | A specific, unambiguous, single item under the size threshold — executes immediately, no blocking. | `media_request` for one named movie under 20 GiB, e.g. `media_request { title: "Arrival", media_type: "movie", tmdb_id: 210577 }` → grabbed immediately, response says so. |
| **Confirm** | Ambiguous, bulk (`item_count > 1`), a whole series/season-less request, or oversized (>20 GiB, e.g. a 4K remux) — returns a confirmation payload (title/year/size/quality) and does nothing until re-called with `confirm: true`. | `media_request` for "Dune (2021)" at `2160p remux` (~40 GB) stops and asks; a whole-series `media_request`/`media_organize` with no `season` stops and asks even though it's nominally "one" request. |
| **Hard-confirm** | Destructive — permanent delete or bulk cleanup. Rejects a plain `confirm: true`; requires the caller to echo back the **exact target** (a verbatim title, or the verbatim set of eligible titles for a bulk op). | `media_delete` requires `confirm_delete` to equal the movie's exact title; `media_cleanup` enumerates eligible titles first and only executes when `confirm_delete` is exactly that title set. |

Confirmation weight scales with irreversibility + ambiguity, not a blanket rule — see
[`media_request`](#media_request-media-03), [`media_organize`](#media_organize-media-04),
[`media_delete`](#media_delete-media-04--destructive-hard-gated), and
[`media_cleanup`](#media_cleanup-media-04--destructive-bulk-op-enumerate-then-confirm) below
for the exact per-tool rules. The gate cannot be bypassed by chaining tools together either —
see [Chain composition](#lumina-surface-wiring-media-07--conversational-contract-no-new-mutation-logic).

## Configuration

| Env var | Service | Notes |
|---|---|---|
| `RADARR_URL` / `RADARR_API_KEY` | Radarr (movies) | sent as `X-Api-Key` header |
| `SONARR_URL` / `SONARR_API_KEY` | Sonarr (TV) | sent as `X-Api-Key` header |
| `PROWLARR_URL` / `PROWLARR_API_KEY` | Prowlarr (indexers) | sent as `X-Api-Key` header |
| `QTOR_URL` / `QTOR_CREDS` | qtor (download client) | `QTOR_CREDS` sent as `Authorization` header |
| `PLEX_URL` / `PLEX_TOKEN` | Plex (library/history) | sent as `X-Plex-Token` header |
| `JELLYSEERR_URL` / `JELLYSEERR_API_KEY` | <media-service> (request-tracking) | shared with the pre-existing [`<media-service>`](<media-service>.md) tool module; same two env vars |
| `TMDB_API_KEY` | TMDb (title resolution) | sent as the `api_key` query param — the one external call in this domain |
| `TMDB_API_URL` | TMDb | optional, non-secret base-URL override; defaults to `https://api.themoviedb.org/3` |
| `RADARR_QUALITY_PROFILE_ID` / `RADARR_ROOT_FOLDER_PATH` | Radarr | **MEDIA-03**, non-secret behavioral config read at `media_request` execute time (not client-construction time) -- which quality profile (integer id) and library root folder Radarr should use for a new movie. Missing/invalid -> `NotConfigured` when a movie add is attempted. |
| `SONARR_QUALITY_PROFILE_ID` / `SONARR_ROOT_FOLDER_PATH` | Sonarr | **MEDIA-03**, same idea as the Radarr pair, for Sonarr series/season adds. |

Each service is configured independently — a missing pair for one service never affects
another. All names are documented (no values) in `.env.example` and materialized into the
process environment at deploy time via the runtime secret store (see
`secrets_bootstrap::GITEA_PLANE_GITHUB_SECRET_KEYS`, which this item extends with these keys).

## Client shape (`src/media/clients/`)

Each of the seven services gets one thin, typed client, following the same hearth/<media-service>
pattern used elsewhere in this crate:

- `from_env()` reads the service's env vars, trims/strips a trailing slash from the URL, and
  returns `Err(ToolError::NotConfigured(..))` naming the missing variable if either is absent
  or empty — never a panic.
- A small `reqwest::Client` (15s timeout) is built once per client.
- One or two representative operations per client make the real HTTP call shape (e.g. Radarr's
  `lookup_movie`/`library`, TMDb's `search_multi`) so later items have a mock-tested foundation;
  these are thin passthroughs of the parsed JSON, not yet shaped for narration (MEDIA-02's job).
- Response mapping is shared: HTTP 404 → `ToolError::NotFound`; other 4xx → `ToolError::Http`
  carrying the status and up to 200 chars of the response body; 5xx or a transport-level error →
  `ToolError::Http` with a service-named "unavailable" message. Nothing on the network path
  panics or unwraps.

| Client | Module | Representative operations |
|---|---|---|
| Radarr | `media::clients::radarr::RadarrClient` | `lookup_movie`, `library` |
| Sonarr | `media::clients::sonarr::SonarrClient` | `lookup_series`, `library` |
| Prowlarr | `media::clients::prowlarr::ProwlarrClient` | `search`, `indexers` |
| qtor | `media::clients::qtor::QtorClient` | `status`, `queue` |
| Plex | `media::clients::plex::PlexClient` | `library_sections`, `history` |
| <media-service> | `media::clients::<media-service>::JellyseerrClient` | `status`, `create_request` |
| TMDb | `media::clients::tmdb::TmdbClient` | `search_multi` |

## media_domain_status

The one tool this item registers. No arguments; reports which of the seven services are
configured (env vars present and non-empty) — **configuration presence only**, it does not
contact any service (the S94 build constraints explicitly prohibit touching the live ARR
stack during this build).

**Output** (JSON string):

```json
{
  "radarr": true,
  "sonarr": false,
  "prowlarr": false,
  "qtor": false,
  "plex": true,
  "<media-service>": false,
  "tmdb": true,
  "configured_count": 3,
  "total_services": 7
}
```

## media_search (MEDIA-02)

Resolves a fuzzy natural-language or partial title to real TMDb candidates
(`src/media/search.rs`). Read-only — never requests or downloads anything.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `query` | string | yes (rejected as `InvalidArgument` if empty/whitespace-only) |

**Behavior.** Calls `TmdbClient::search_multi`, then ranks the `movie`/`tv` results (persons are
dropped) against `query` with a dependency-free title-similarity heuristic: exact
case-insensitive match scores 1.0, a substring match scores 0.6-0.9 by coverage, otherwise a
capped Jaccard word-overlap score. Popularity only breaks ties between equally-relevant titles —
it never lets an irrelevant-but-popular result outrank a genuine title match. Candidates scoring
below a relevance floor are dropped entirely.

Two or more candidates that are both strong matches (score ≥ 0.75) within 0.15 of each other are
**ambiguous** — the response lists up to three of them with disambiguating year/type detail and
asks which one was meant, rather than guessing. No qualifying candidates returns a friendly
"couldn't find that" summary with a refinement suggestion, not an error.

**Output** (JSON string, narration-shaped — always `summary` + `structured`):

```json
{
  "summary": "I found a few close matches for \"dune\": Dune (2021, movie); Dune (1984, movie). Which one did you mean?",
  "structured": {
    "query": "dune",
    "ambiguous": true,
    "candidates": [
      { "title": "Dune", "year": "2021", "tmdb_id": 438631, "media_type": "movie", "score": 1.0 },
      { "title": "Dune", "year": "1984", "tmdb_id": 890, "media_type": "movie", "score": 1.0 }
    ]
  }
}
```

## media_status (MEDIA-02)

Checks whether a title (or the title returned by `media_search`) is already in the library,
aggregating across Radarr, Sonarr, and Plex (`src/media/search.rs`). Read-only.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `title` | string | yes (rejected as `InvalidArgument` if empty/whitespace-only) |

> **Note:** MEDIA-02 matches **by title only** (e.g. a title returned by `media_search`). Direct lookup by numeric TMDb id is deferred to a later item; a bare id string is treated as a title and simply resolves to "not present" rather than erroring.

**Behavior.** Independently checks each service's client:
- Radarr/Sonarr: fetches the full library (`RadarrClient::library` / `SonarrClient::library`)
  and matches by title with the same similarity heuristic as `media_search`.
- Plex: matches against recent watch history (`PlexClient::history`) as an "available to
  watch"/"already seen" proxy — the MEDIA-01 client has no dedicated library-content lookup yet;
  a later item can swap this in without changing `media_status`'s response shape.

Each service reports independently as one of: not configured, configured-but-unreachable
(the HTTP/parse error, never a panic), configured-and-present, or configured-and-absent — one
service's trouble never masks or blocks another's answer, and an id/title matching nothing
anywhere still returns a normal (not error) response.

**Output** (JSON string, narration-shaped):

```json
{
  "summary": "\"Dune\": in Radarr (downloaded); not in Sonarr; in Plex (present).",
  "structured": {
    "query": "Dune",
    "radarr": { "configured": true, "reachable": true, "present": true, "detail": { "title": "Dune", "hasFile": true } },
    "sonarr": { "configured": true, "reachable": true, "present": false },
    "plex": { "configured": true, "reachable": false, "error": "Plex unavailable (HTTP 500 Internal Server Error)" }
  }
}
```

## media_request (MEDIA-03)

Requests/downloads a movie (Radarr) or TV season/series (Sonarr) -- the acquisition surface,
guarded by a **tiered mutation safety** model (`src/media/request.rs`). Optionally registers a
<media-service> request alongside the real grab, best-effort, for tracking.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `title` | string | yes (rejected as `InvalidArgument` if empty/whitespace-only) |
| `media_type` | `"movie"` \| `"series"` | yes |
| `year` | string | no |
| `tmdb_id` | integer | conditionally -- required to execute a movie add (from `media_search`) |
| `tvdb_id` | integer | conditionally -- required to execute a series add (Sonarr's id space, distinct from TMDb) |
| `season` | integer | no -- a specific season number; omitting it for a series means "the whole series" |
| `quality_hint` | string | no -- e.g. `"2160p remux"`, `"1080p"`; used to estimate size when `size_estimate_bytes` isn't given |
| `size_estimate_bytes` | integer | no -- a known/estimated download size, if available |
| `item_count` | integer | no, default 1 -- how many discrete items this single call would grab (e.g. requesting 3 seasons at once); `>1` is always Confirm-tier |
| `is_ambiguous` | boolean | no, default false -- true when the title/candidate itself isn't definitively resolved |
| `confirm` | boolean | no, default false -- must be `true` to execute a Confirm-tier request |

**The tiering model** (`classify_request(kind, is_ambiguous, item_count, est_size_bytes) -> MutationTier`,
pure and unit-tested, `src/media/request.rs`):

- **Light** -- a specific, unambiguous, single item (`item_count == 1`, not a whole series) under
  `OVERSIZED_THRESHOLD_BYTES` (20 GiB). Executed immediately.
- **Confirm** -- `is_ambiguous`, `item_count > 1`, a whole series with no `season` given (always
  high-impact, even as "one" request), or an oversized single item (e.g. a 4K remux, `est_size_bytes >
  20 GiB`). **Never auto-executed** -- the response carries the confirmation payload (title, year,
  size, quality) and the caller must re-call with `confirm: true`.

Size, when not given explicitly, is estimated from `quality_hint` (`remux`/`2160p`/`4k` -> 25 GB,
`1080p` -> 4 GB, `720p` -> 2 GB, unknown -> 4 GB) purely so an oversized hint alone is enough to
force Confirm-tier even without a byte-accurate estimate.

**Behavior.** Classifies first, before any I/O. Confirm-tier without `confirm: true` returns the
confirmation payload and stops -- no library check, no HTTP call to Radarr/Sonarr. Otherwise:
checks the target service's library for a title match (`RadarrClient::library` /
`SonarrClient::library`) and, if already present, reports that without duplicating; otherwise
calls `RadarrClient::add_movie` / `SonarrClient::add_series` (quality profile + root folder from
`RADARR_QUALITY_PROFILE_ID`/`RADARR_ROOT_FOLDER_PATH` or the Sonarr equivalents, `addOptions`
set to trigger Radarr/Sonarr's own indexer search+grab -- the mechanism that hands a completed
download to qtor) and, best-effort, a <media-service> tracking request. An arr rejection or
unreachable-service error propagates as a real `ToolError`, never a false "executed" success.
Every *executed* mutation is recorded via `crate::gateway_framework::audit::AuditEntry`
(S6-sanitized) in addition to the `#[instrument]` span.

**Output** (JSON string, narration-shaped):

```json
// Confirm-tier, not yet confirmed
{
  "summary": "\"Dune (2021)\" is ~40.0 GB at 2160p remux -- this needs confirmation before I grab it. Reply with confirm: true to proceed.",
  "structured": {
    "title": "Dune", "year": "2021", "media_type": "movie", "season": null,
    "quality_hint": "2160p remux", "estimated_size_bytes": 42949672960,
    "tier": "confirm", "executed": false, "already_present": false, "note": null
  }
}
```

```json
// Light-tier, executed
{
  "summary": "Grabbed \"Arrival\" (2016) (~4.0 GB) -- Radarr/Sonarr is searching indexers now.",
  "structured": {
    "title": "Arrival", "year": "2016", "media_type": "movie", "season": null,
    "quality_hint": "1080p", "estimated_size_bytes": 4294967296,
    "tier": "light", "executed": true, "already_present": false, "note": null
  }
}
```

## media_organize (MEDIA-04)

Non-destructive library organization — set the `monitored` flag, replace a movie/series' tags,
or set a movie's TMDb collection (`src/media/organize.rs`). Reuses `media_request`'s exact
`classify_request`/`MutationTier` pure tiering model verbatim: a specific, unambiguous,
single-item or single-season change (`item_count == 1`, not a whole-series change) executes
immediately (Light); anything ambiguous, bulk (`item_count > 1`), or whole-series-scoped (no
`season` given) returns a confirmation payload and requires a follow-up `confirm: true`
(Confirm) — identical semantics to `media_request`, just with `est_size_bytes` fixed at 0 since
metadata changes have no download size.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `id` | integer | yes — Radarr/Sonarr resource id |
| `media_type` | `"movie"` \| `"series"` | yes |
| `action` | `"monitor"` \| `"tag"` \| `"add_to_collection"` | yes |
| `season` | integer | no — season-scoped change; omitting it for a series means "whole series" (always Confirm-tier) |
| `monitored` | boolean | required for `action=monitor` |
| `tag_ids` | integer[] | required for `action=tag` — already-resolved Radarr/Sonarr tag ids |
| `collection_tmdb_id` | integer | required for `action=add_to_collection` (movies only) |
| `item_count` | integer | no, default 1 |
| `is_ambiguous` | boolean | no, default false |
| `confirm` | boolean | no, default false — must be `true` to execute a Confirm-tier change |

**Behavior.** Confirm-tier without `confirm: true` returns the confirmation payload and stops —
no library fetch, no PUT. Otherwise fetches the current resource (`RadarrClient::library` /
`SonarrClient::library`, matched by `id`), merges the requested field into the full resource
body (Radarr/Sonarr's PUT expects the complete resource, not a partial patch), and calls
`RadarrClient::update_movie` / `SonarrClient::update_series`. A missing `id` is `NotFound`, never
a silent no-op (organize targets a specific known resource, unlike delete's "already absent" —
see below). Every executed change is audit-logged with the id/title/action.

## media_delete (MEDIA-04) — DESTRUCTIVE, hard-gated

Permanently deletes a single movie or series and its files. This is **structurally stronger**
than `media_request`/`media_organize`'s boolean `confirm: true`: the schema doesn't even accept
one. The caller must set `confirm_delete` to the **exact, verbatim title** of the thing being
deleted (as returned by the first, unconfirmed call) — a light ack, a stale/replayed
`confirm: true`, or any string that isn't an exact (trimmed, case-sensitive) match to the real
title never triggers a delete.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `id` | integer | yes |
| `media_type` | `"movie"` \| `"series"` | yes |
| `confirm_delete` | string | no — must equal the target's exact title to execute; omit to get the confirmation payload |

**Behavior.** Looks the item up by `id` first. Not present in the library at all → a clean
no-op response (`already_absent: true`), never an error and never a confirmation prompt (EDGE
CASE: deleting something already gone). Present but `confirm_delete` missing/mismatched → a
confirmation payload naming the exact target title, **nothing deleted**. Present and
`confirm_delete` matches exactly → `RadarrClient::delete_movie` / `SonarrClient::delete_series`
(which itself deletes files), audit-logged with the id/title.

## media_cleanup (MEDIA-04) — DESTRUCTIVE bulk op, enumerate-then-confirm

Bulk removal (e.g. "clean up what I've watched"). The caller supplies pre-resolved `candidates`
(typically Plex watch history cross-referenced against the Radarr/Sonarr library upstream of
this tool — this domain's `PlexClient` exposes account-level history only, not a per-user
breakdown, so per-user watched-aggregation happens above this tool; the wire shape for that
aggregation is not verified against a live multi-user Plex deployment).

**Input schema**

| Field | Type | Required |
|---|---|---|
| `media_type` | `"movie"` \| `"series"` | yes |
| `candidates` | array of `{ id, title, watched_by_all_users? }` | yes |
| `confirm_delete` | string[] | no — must exactly equal (order-independent) the enumerated eligible-title set from the first call |

**Behavior — multi-user Plex EDGE CASE.** Candidates split into *eligible* (`watched_by_all_users
== true`) and *flagged* (`false`, **or omitted** — the safe default for a destructive op is to
flag, not assume consensus). Flagged items are surfaced in every response but are **never
deleted, even when confirmed** — the confirm-set comparison is against the eligible set only, so
a caller that tries to confirm a superset including a flagged item falls back to
re-enumeration rather than partially executing.

The first call (no `confirm_delete`, or one that doesn't exactly match the eligible set)
**enumerates** the exact eligible titles in the response and deletes nothing — no blind purge.
Only a follow-up call whose `confirm_delete` is exactly the eligible-title set (as a set; order
doesn't matter, a partial or superset confirm doesn't match) executes: each eligible candidate is
deleted via the same Radarr/Sonarr client methods `media_delete` uses, individually audit-logged,
with per-item `deleted`/`already_absent`/`failed` outcomes in the response (one candidate's
failure never blocks the others).

## media_recommend (MEDIA-05) — stateless

Suggests movies/shows already in the Radarr/Sonarr library that haven't been watched yet, ranked
against a taste profile built **fresh, in-process, on every call** from recent Plex watch history
(`src/media/recommend.rs`). Read-only.

**STATELESS by design.** This tool makes no call to any personalization/curation-memory facade —
no such client exists in this crate yet (a later, separately-toggled item may add one). The taste
profile is computed newly each call from that call's own Plex history and is never persisted or
read back; the tool works fully with any future personalization layer off. A unit test
(`stateless_module_makes_no_memory_calls`) scans the module's own production source for
memory-shaped identifiers so a future change that introduces one fails the test.

**Input schema**

| Field | Type | Required |
|---|---|---|
| `limit` | integer | no, default 5 |
| `account_id` | string | no — scopes watch history to one Plex account/user id. On a multi-user server, watch history is **never blended** across accounts: an explicit `account_id` is honored, otherwise the most-recently-active account (by `viewedAt`) is used alone; history with no per-user signal at all (single-user servers) is used unfiltered. |

**Behavior.** Builds a recency-weighted taste profile from Plex history (`PlexClient::history`):
genre and director overlap accumulate weight, most-recent watches weighted most. Candidates come
from the current Radarr/Sonarr library (`RadarrClient::library`/`SonarrClient::library`) minus
anything already watched, scored by genre/director overlap against the profile (a director match
weighs more than a bare genre match) and ranked descending. Each recommendation's `rationale`
names the watched title(s) that drove the match, e.g. "because you watched Dune (Science
Fiction)".

**Edge cases.** Sparse/empty watch history (new user) → falls back to plain library candidates,
`thin_signal: true`, and a summary that says so instead of fabricating a rationale. Plex
unreachable → degrades to Radarr/Sonarr-only library picks with `structured.degraded` naming
what couldn't be reached, never a hard failure. No services configured at all → a friendly empty
response, not an error.

**Output** (JSON string, narration-shaped):

```json
{
  "summary": "You might like \"Arrival\" -- because you watched Dune (Science Fiction).",
  "structured": {
    "thin_signal": false,
    "degraded": null,
    "recommendations": [
      { "title": "Arrival", "media_type": "movie", "score": 1.0, "matched_genres": ["Science Fiction"], "rationale": "because you watched Dune (Science Fiction)" }
    ]
  }
}
```

## media_on_deck / media_recently_added (MEDIA-05) — engagement

Thin, read-only Plex engagement surface, not personalized:

- `media_on_deck` — `PlexClient::on_deck` (`GET /library/onDeck`), Plex's own continue-watching /
  next-up surface.
- `media_recently_added` — `PlexClient::recently_added` (`GET /library/recentlyAdded`), what's
  new in the library.

Both take no arguments, require `PLEX_URL`/`PLEX_TOKEN` (else `NotConfigured`), and return the
same narration-shaped `{ summary, structured: { count, titles } }` shape; an empty result is a
friendly "nothing on deck"/"nothing recently added" summary, not an error.

## Taste-memory personalization (MEDIA-06) — toggleable, default OFF

A hard on/off personalization layer that enriches `media_recommend` with the operator's
taste/curation memory. **The media domain works fully without it** — flipping it off
de-personalizes suggestions but never breaks recommendations.

**Toggle.** `MEDIA_TASTE_MEMORY_ENABLED` (three-state env: `1/true/on/yes` = on; anything else,
including unset, = off — **default OFF**). The flag gates the *entire* module:

- **OFF** → `crate::media::register` registers MEDIA-05's stateless `media_recommend` unchanged and
  makes **zero** taste-facade calls. `taste_memory::register` is a no-op. (The MEDIA-05 statelessness
  guarantee — and its `stateless_module_makes_no_memory_calls` test — are unaffected: the taste code
  lives entirely in `src/media/taste_memory.rs`, and `recommend.rs` gains only a plain `from_env()`
  constructor, no memory dependency.)
- **ON** → `taste_memory::register` `register_or_replace`s `media_recommend` with a taste-aware
  decorator that first computes the stateless MEDIA-05 result, then blends in taste signals read from
  the facade (adjusting ranking + rationale, e.g. *"you told me you're into slow-burn sci-fi"*), and
  registers the optional **`media_taste_feedback`** write-back tool.

**Facade.** `MEDIA_TASTE_API_URL` — an *assumed* REST facade for the taste/curation store (liked/
disliked, curation notes, stated preferences), following the `vitals`/`odyssey` convention. Non-secret
base URL. **The endpoint/wire shape is not verified against a live service** and may need adjustment
when a real taste store is wired.

**Never a hard dependency.** Flag on but `MEDIA_TASTE_API_URL` unset, or the facade unreachable/erroring
→ recommendations **degrade to the stateless MEDIA-05 result**, stamped `structured.taste_memory: { applied: false, note: "…" }`;
they still return `Ok`. Write-back failures are logged, not surfaced as errors. No PII is written into stored signals.

### media_taste_feedback (MEDIA-06, flag-gated) — write-back
Registered only when the flag is on. Records engagement signals (requested/watched/dismissed) to the
taste facade (POST) so curation improves over time. Its mere presence in the tool catalog reflects the
flag state.

## Lumina surface wiring (MEDIA-07) — conversational contract, no new mutation logic

`src/media/surface.rs` makes Lumina the interaction surface for the whole domain. It adds **no**
new tool and **no** new mutation logic — the tiering/confirm gates stay exactly where MEDIA-03/04
put them. It is pure metadata + pure helper functions, fully unit-tested without HTTP.

**Intent routing.** `resolve_intent(phrase: &str) -> MediaIntent` is a small, deterministic,
keyword-matched routing table from representative conversational phrases to the tool (or ordered
tool chain) they imply, e.g.:

| Phrase (representative) | Resolves to |
|---|---|
| "put something on" / "what should I watch" | `media_recommend` |
| "grab that show I was watching" | chain: `media_search` → `media_status` → `media_request` |
| "is Dune on Plex?" / "do I already have it" | `media_status` |
| "clean up what I've watched" | `media_cleanup` |
| "delete that movie" | `media_delete` |
| "what's on deck" / "what's new" | `media_on_deck` / `media_recently_added` |

Note this repo's `ToolRegistry`/`RustTool` trait has no separate keywords/intent-hints field, and
the live subagent-side tool matcher (`discover(query, max)`, tokenized name+description matching)
lives in Chord, not here — `resolve_intent` is a standalone, directly-testable reference table a
subagent-side matcher can consult, in the same spirit as this domain's keyword-rich
`description()` strings. An under-specified phrase (media vocabulary present but no clear action,
or no media vocabulary at all) resolves to `MediaIntent::Clarify(question)` — a clarifying
question Lumina asks, never a guessed action.

**Confirmation narration.** Three pure helpers turn a tool's raw `structured` JSON into a short,
in-voice prompt carrying the concrete specifics, instead of Lumina reading raw JSON aloud:

- `narrate_request_confirmation(&structured)` — MEDIA-03's `media_request`/`media_organize`
  Confirm-tier payload → e.g. *"'Dune' (2021) -- that's a ~60GB 2160p remux grab -- want me to go
  ahead?"*. Returns `None` when the call already executed or was never Confirm-tier (nothing to
  narrate, not an error).
- `narrate_delete_confirmation(&structured)` — MEDIA-04's `media_delete` pending-delete payload →
  names the exact target and reminds Lumina the gate is a **typed** exact-title match, not a
  yes/no `confirm: true`.
- `narrate_cleanup_confirmation(&structured)` — MEDIA-04's `media_cleanup` pending-bulk-delete
  payload → enumerates every eligible title plus how many were flagged (not watched by every
  user) and left alone.

**Chain composition + the gate holds mid-chain.** `MediaChain`/`SEARCH_STATUS_REQUEST_CHAIN`
documents the canonical `media_search` → `media_status` → `media_request` acquisition chain.
Each step is an independent tool call — there is no "this call came from a chain" flag anywhere
in this domain for a chain to exploit. `surface.rs`'s tests include a negative case that drives a
whole-series `media_request` call (always Confirm-tier per MEDIA-03) through the public
`ToolRegistry`/`call()` path exactly as the last step of a resolved chain, and asserts it still
returns the confirmation payload (`executed: false`) rather than a false "done" — chaining cannot
bypass the confirm gate.

## Example conversations

Three representative exchanges, tying `resolve_intent` and the confirmation narration back to
the actual tool calls and gates above.

**"Put something on."** Resolves to `media_recommend` (a Read-tier, stateless call — see
[`media_recommend`](#media_recommend-media-05--stateless)). Lumina narrates the top result's
`rationale` directly, no gate involved: *"You might like Arrival — because you watched Dune."*

**"Grab that show I was watching."** Resolves to the `media_search` → `media_status` →
`media_request` chain. `media_search` resolves the fuzzy title to a TMDb candidate;
`media_status` checks it isn't already in Sonarr/Plex; `media_request` classifies the actual
grab. If it's a single named season under the size threshold, that's Light-tier and it just
happens — *"Grabbed Arrival (2016) — Sonarr/Radarr is searching indexers now."* If it resolves
ambiguously (two candidate titles) or the user asked for "the whole series" with no season
given, `media_request` returns Confirm-tier and `narrate_request_confirmation` turns that into
an in-voice ask: *"That's the whole series, no season specified — about 60GB at 2160p remux —
want me to go ahead?"* Nothing is grabbed until the reply carries `confirm: true`; the chain
does not bypass this (see [Chain composition](#lumina-surface-wiring-media-07--conversational-contract-no-new-mutation-logic)).

**"Clean up what I've watched."** Resolves to `media_cleanup`, a destructive bulk op. The
first call enumerates every eligible title (watched by all users on a multi-user Plex — see
the [`media_cleanup`](#media_cleanup-media-04--destructive-bulk-op-enumerate-then-confirm)
edge case) and deletes nothing: *"I'd remove these 4 you've all finished: Arrival, Dune,
Sicario, Prisoners — anything half-watched by someone else stays. Say the word and I'll clear
exactly those four."* Only a follow-up call whose `confirm_delete` is exactly that title set
executes — a vague "yeah go ahead" from the user is not itself enough; Lumina must echo back
the enumerated set as the typed confirmation, per `narrate_cleanup_confirmation`.
