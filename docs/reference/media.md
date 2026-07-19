# media

`src/media` — 424 KG symbols.

The media domain gives the fleet sovereign, governed control of the self-hosted
media stack — Radarr, Sonarr, Prowlarr, qtor (torrent client), Plex,
<media-service>, and TMDb — through one thin, typed, mock-friendly client per
service and a tool surface on top (search, request classification/decision,
recommendation, library organization). The design principle is graceful
degradation: every client's `from_env()` returns `ToolError::NotConfigured`
when its env vars are missing rather than panicking, so a single unreachable
service disables only its own tools while the domain keeps loading and its
status tool keeps reporting per-service state.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `media::clients::<media-service>::JellyseerrClient` | struct | `src/media/clients/<media-service>.rs` | Typed <media-service> client; `status()` health probe, `map_response` error normalization. |
| `media::clients::radarr::RadarrClient` | struct | `src/media/clients/radarr.rs` | Typed Radarr (movies) client. |
| `media::clients::sonarr::SonarrClient` | struct | `src/media/clients/sonarr.rs` | Typed Sonarr (TV) client. |
| `media::clients::qtor::QtorClient` | struct | `src/media/clients/qtor.rs` | Typed torrent-client API wrapper. |
| `media::request::classify_request` | fn | `src/media/request.rs` | Classifies an incoming media request before any acquisition decision/grab fires. |
| `media::search` | module | `src/media/search.rs` | Cross-service media search surface. |
| `media::recommend` | module | `src/media/recommend.rs` | Recommendation tools. |
| `media::taste_memory` | module | `src/media/taste_memory.rs` | Persisted taste/preference memory consulted by recommendations. |
| `media::organize` | module | `src/media/organize.rs` | Library organization helpers. |
| `media::surface` | module | `src/media/surface.rs` | The registered media tool surface (10 `media_*` tools). |

## How it connects

Registered on the core registry (`register_all`). Every client is
reqwest-based typed HTTP per the `RustTool` contract; each service's
`map_response` normalizes provider errors into `ToolError`. The acquisition
write path (request → decision → grab) funnels through a single gated
chokepoint so a governance check covers every grab. The separate crate-root
`<media-service>` module is the older standalone <media-service> tool set that predates
this domain and is registered independently.

## Configuration

Per-service URL/credential pairs, read at client construction:
`RADARR_URL`/`RADARR_API_KEY`, `SONARR_URL`/`SONARR_API_KEY`,
`PROWLARR_URL`/`PROWLARR_API_KEY`, `QTOR_URL`, `PLEX_URL`/`PLEX_TOKEN`,
`JELLYSEERR_URL`/`JELLYSEERR_API_KEY`, `TMDB_API_URL`/`TMDB_API_KEY`. Values
are materialized into the process environment at startup by the secrets
bootstrap — no literals in code.

## Notes and gaps

The Muse media-management application (library scan, still-frame matching
verification, HandBrake ingest) builds on this domain from its own repository;
this page covers only what lives in this crate. Metadata-provider bridges
(TVDB/TMDb/IMDb) beyond the TMDb client, and the media web interface, are
likewise out of scope here.
