# Media — sovereign media-stack orchestration (scaffold)

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

**Status: scaffold (MEDIA-01, S94).** This page documents the domain as it exists after the
first build item only — one registered tool (`media_domain_status`) plus the seven typed
service clients later items build on. Full per-tool documentation (search, request/download,
organize, recommend, taste-memory) lands with MEDIA-02 through MEDIA-08 as those items ship.

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

## What's not here yet

No search, request/download, organize, or recommend tools exist in this item — see the spec
(`S94-media-domain`, Plane project `TERM`) for MEDIA-02 through MEDIA-08. The tiered
mutation-safety model (read-free / light-execute / confirm-required / hard-confirm-destructive)
and the toggleable taste-memory module are load-bearing design pieces of later items, not this
scaffold. See [`specs/behavior/media-behavior.md`](../../../specs/behavior/media-behavior.md)
for the states/degradation contract this item establishes.
