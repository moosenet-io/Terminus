# Media — sovereign media-stack orchestration

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

**Status: read/search surface live (MEDIA-01+MEDIA-02, S94).** This page documents the domain
through its first two build items: the seven typed service clients + `media_domain_status`
(MEDIA-01), and the read-only search/status surface, `media_search` and `media_status`
(MEDIA-02). Request/download, organize, recommend, and taste-memory tools land with MEDIA-03
through MEDIA-08 as those items ship.

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
| `id_or_title` | string | yes (rejected as `InvalidArgument` if empty/whitespace-only) |

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

## What's not here yet

No request/download, organize, or recommend tools exist yet — see the spec
(`S94-media-domain`, Plane project `TERM`) for MEDIA-03 through MEDIA-08. The tiered
mutation-safety model (read-free / light-execute / confirm-required / hard-confirm-destructive)
and the toggleable taste-memory module are load-bearing design pieces of later items, not this
read/search surface. See [`specs/behavior/media-behavior.md`](../../../specs/behavior/media-behavior.md)
for the states/degradation contract this domain establishes.
