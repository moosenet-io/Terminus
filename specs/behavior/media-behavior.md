# Media domain — behavior spec (stub)

Status: **STUB** — captures the states/degradation contract established by MEDIA-01
(the domain scaffold + service clients). Full per-tool behavior (request tiering,
destructive-op gating, recommendation/taste-memory states) is documented item-by-item
as MEDIA-02..07 land; this file is the durable anchor those later sections attach to,
not a finished spec.

Spec: `S94-media-domain` (Plane project `TERM`, prefix `MEDIA`).
Source: `src/media/mod.rs`, `src/media/clients/*.rs`.

## Services orchestrated

| Service | Role | Client | Config |
|---|---|---|---|
| Radarr | Movie library + acquisition | `media::clients::radarr::RadarrClient` | `RADARR_URL`, `RADARR_API_KEY` |
| Sonarr | TV library + acquisition | `media::clients::sonarr::SonarrClient` | `SONARR_URL`, `SONARR_API_KEY` |
| Prowlarr | Indexer aggregation | `media::clients::prowlarr::ProwlarrClient` | `PROWLARR_URL`, `PROWLARR_API_KEY` |
| qtor | Download client | `media::clients::qtor::QtorClient` | `QTOR_URL`, `QTOR_CREDS` |
| Plex | Library / consumption / history | `media::clients::plex::PlexClient` | `PLEX_URL`, `PLEX_TOKEN` |
| <media-service> | Request-tracking + discovery | `media::clients::<media-service>::JellyseerrClient` | `JELLYSEERR_URL`, `JELLYSEERR_API_KEY` |
| TMDb | Title resolution (the one external call) | `media::clients::tmdb::TmdbClient` | `TMDB_API_KEY` (+ optional `TMDB_API_URL`) |

## States (MEDIA-01 scope)

Each client has exactly two configuration states, checked independently per service:

- **Configured** — both required env vars are present and non-empty. `from_env()`
  returns `Ok(Client)`. No network call is made at construction time; reachability is
  only proven by the first real operation call.
- **Not configured** — either required env var is missing or empty (whitespace-only
  counts as empty). `from_env()` returns `Err(ToolError::NotConfigured(..))` naming the
  missing variable. Never panics.

`media_domain_status` (the one tool this item registers) reports all seven states in
one call, independently — one service being configured never masks another being
unconfigured, and the tool itself never fails regardless of how many services are
unconfigured (it reports configuration presence only; it does not contact any service).

## Degradation contract

- A service missing its config disables only *that service's* future tools (MEDIA-02
  onward) — the domain always loads, and other configured services are unaffected.
- A service that IS configured but unreachable/erroring at call time (connection
  refused, timeout, 5xx) maps to `ToolError::Http` with a service-named message (e.g.
  "Radarr unavailable: ..."), never a panic or an unwrap.
- A 404 from a configured, reachable service maps to `ToolError::NotFound`.
- A 4xx other than 404 (auth failure, bad request) maps to `ToolError::Http` carrying
  the status code and up to 200 chars of the response body — enough for a human/Lumina
  to diagnose "wrong API key" vs. "malformed request" without ever echoing a credential
  value (only the *response* body is included, never the request's own credential).

## Deferred to later items

- **MEDIA-02** (search/resolve): narration-shaped read responses built on these
  clients' `lookup_movie`/`lookup_series`/`search`/`search_multi` operations.
- **MEDIA-03/04** (request/organize): the tiered mutation-safety states (read / light /
  confirm / hard-confirm) and destructive-op gating — none of that exists yet; these
  clients expose no destructive operations in MEDIA-01.
- **MEDIA-05/06** (recommend + taste-memory toggle): stateless-vs-memory-enriched
  states, gated by `MEDIA_TASTE_MEMORY_ENABLED` (not yet introduced).
- **MEDIA-07** (Lumina surface): conversational intent-routing and confirmation-prompt
  narration states.
