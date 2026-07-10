# <media-service> — media request queries

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

<media-service> manages Plex/Jellyfin media requests: users request movies and shows, and it routes
them to Radarr/Sonarr for download. This module (`src/<media-service>/mod.rs`) provides four
**read-only** tools that mirror the Python `jellyseerr_tools.py` exactly — same names, same
parameters (`src/<media-service>/mod.rs:1-17`). No tool in this module writes to <media-service>; there
is no approve/decline/create-request tool.

<img src="../../../assets/<media-service>-architecture.svg" alt="Four read-only <media-service> tools call JellyseerrConfig::api_get, which sends a GET with header X-Api-Key to JELLYSEERR_URL/api/v1/..., and parse_requests/parse_search normalize the response shape" width="100%">

## Configuration

| Env var | Required | Notes |
|---|---|---|
| `JELLYSEERR_URL` | yes | base URL, e.g. `http://<<media-service>-host>:5055`; trailing slash stripped |
| `JELLYSEERR_API_KEY` | yes | sent as the `X-Api-Key` header |

If either is unset or empty, `register()` installs a `NotConfiguredStub` for all four tool
names instead of the real implementations (`src/<media-service>/mod.rs:388-404`) — each stub keeps
the tool visible in the catalog but always returns `NotConfigured` naming both env vars.

## JellyseerrConfig::api_get — shared request path

Every tool goes through `api_get` (`src/<media-service>/mod.rs:56-87`): `GET
{base}/api/v1{path}` with `Accept: application/json` and `X-Api-Key: {key}` headers, plus any
caller-supplied query params. Non-2xx responses are turned into `Http` errors carrying the
status and up to 200 chars of the response body. An empty successful body is treated as `{}`
rather than a parse failure — <media-service>'s `/status` endpoint can return an empty body on some
versions.

## jellyseerr_status

`GET /status` — server health, version, and update status (`src/<media-service>/mod.rs:235-258`). No
arguments.

**Output** (JSON string):

```json
{
  "version": "1.9.2",
  "commit": "abc1234",
  "update_available": false,
  "restart_required": false,
  "healthy": true
}
```

`healthy` is always `true` when this tool returns at all — a non-2xx status is instead surfaced
as an `Http` error before any JSON is constructed, so `healthy: false` never actually appears
in output.

## jellyseerr_requests

`GET /request` — list recent media requests, paginated and filterable
(`src/<media-service>/mod.rs:260-306`). The tool description is written specifically to catch
loosely-phrased queries: "what's queued, pending, or requested on Plex/Jellyfin... their
watchlist."

**Input schema**

| Field | Type | Required | Default |
|---|---|---|---|
| `take` | integer | no | `20`, capped at `100` |
| `skip` | integer | no | `0` |
| `status` | string: `pending`\|`approved`\|`available`\|`declined`\|`""` | no | `""` (all) |

**Behavior.** `status` is case-insensitively mapped to <media-service>'s numeric `filter` value via
`status_filter_code` (`src/<media-service>/mod.rs:105-113`): `pending→1, approved→2, declined→3,
available→5`. Note `processing` is **not** a valid `filter` value on the live API (it maps to
`None` and is silently dropped from the query rather than erroring) — this mirrors the Python
source's own behavior. Requests are always fetched sorted by `added`. The response is passed
through `parse_requests` (below).

**`parse_requests`** (`src/<media-service>/mod.rs:116-170`) normalizes each raw request into:

```json
{"id": 7, "title": "Dune", "type": "movie", "status": "approved",
 "media_status": "available", "requested_by": "Moose", "created": "2026-06-01T10:00:00Z"}
```

Field derivation: `title` prefers `media.name`, falling back to `TMDB:{tmdbId}` if the name is
absent; `requested_by` prefers `requestedBy.displayName`, falling back to
`requestedBy.email`, then `"unknown"`; `status`/`media_status` are both run through the same
`status_name` numeric-code map (`1→pending, 2→approved, 3→declined, 4→processing, 5→available,
else→unknown`); `total` prefers `pageInfo.results`, falling back to the count actually
returned in this page.

## jellyseerr_request_count

`GET /request/count` — summary counts by status, no arguments
(`src/<media-service>/mod.rs:309-334`). Returns `{"total", "pending", "approved", "available",
"declined", "processing"}`, each defaulting to `0` if the field is absent from the upstream
response.

## jellyseerr_search

`GET /search` — search movies and TV shows (`src/<media-service>/mod.rs:337-370`).

**Input schema**

| Field | Type | Required | Default |
|---|---|---|---|
| `query` | string | yes | — |

**Behavior.** `query` is trimmed and rejected as `InvalidArgument` if empty (including
whitespace-only). Always requests `page=1`. Response goes through `parse_search`
(`src/<media-service>/mod.rs:172-225`), which caps results at **15 entries**, truncates `overview`
to 150 chars + `"..."` when longer, resolves `title` from `title` (movie) or `name` (TV) with
year extracted from `releaseDate`/`firstAirDate`'s first 4 characters, and reports
`media_status` as `"not requested"` when no `mediaInfo.status` is present (as opposed to
`"unknown"`, which is reserved for an unrecognized numeric code).

**Example**

```json
// request
{"query": "blade runner"}
```
```json
{"query": "blade runner", "count": 1,
 "results": [{"title": "Blade Runner 2049", "type": "movie", "year": "2017",
              "overview": "A new blade runner unearths a secret...",
              "media_status": "available", "tmdb_id": 100}]}
```

## Registration

`register()` (`src/<media-service>/mod.rs:388-404`) either registers all four live tools sharing one
cloned `JellyseerrConfig`, or all four `NotConfiguredStub`s — there is no partial
configuration; both `JELLYSEERR_URL` and `JELLYSEERR_API_KEY` are required together.
