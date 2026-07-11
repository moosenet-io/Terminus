# Media — sovereign media-stack orchestration

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

**Status: read/search + tiered request + organize/destructive-gating surface live (MEDIA-01
through MEDIA-04, S94).** This page documents the domain through its first four build items: the
seven typed service clients + `media_domain_status` (MEDIA-01), the read-only search/status
surface (`media_search` and `media_status`, MEDIA-02), the tiered-mutation-safety request/
download tool (`media_request`, MEDIA-03), and non-destructive organize plus hard-typed-
confirmation destructive delete/bulk-cleanup (`media_organize`/`media_delete`/`media_cleanup`,
MEDIA-04). Recommend and taste-memory tools land with MEDIA-05 through MEDIA-08 as those items
ship.

The media domain (`src/media/mod.rs`) orchestrates the self-hosted media stack directly —
Radarr, Sonarr, Prowlarr, qtor (download client), Plex, <media-service>, and TMDb — rather than
wrapping a single thin API. It is a **sovereign** build: vault(env)-backed secrets, no
third-party MCP server, everything through this one hardened hub. Lumina (the personality
agent) is intended as the eventual conversational surface (MEDIA-07); this domain is the muscle
behind it.

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

## What's not here yet

No recommend or taste-memory tools exist yet — see the spec (`S94-media-domain`, Plane project
`TERM`) for MEDIA-05 through MEDIA-08. See
[`specs/behavior/media-behavior.md`](../../../specs/behavior/media-behavior.md) for the
states/degradation contract this domain establishes.
