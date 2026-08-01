# Commute — traffic-aware routing & transit planning

[← personal-life index](README.md) · [← tool index](../README.md) · [← docs index](../../README.md)

Commute provides traffic-aware driving directions via the TomTom API, plus a named-location
shortcut system (`home`/`work`/`family`, and any name the user saved) so nobody has to repeat
an address, IATA airport-code expansion for reliable geocoding, and a Bay Area public-transit
planner stub for 511.org. Defined in [`src/commute/mod.rs`](../../../src/commute/mod.rs).

> **TERM #591 — named places come from the registry, not the environment.**
> `COMMUTE_HOME` / `COMMUTE_WORK` / `COMMUTE_FAMILY` are **no longer read by anything**. They
> were process-global — one person's addresses, returned to every entitled caller — which is a
> disclosure that arms itself the moment an operator sets one. Named places now resolve through
> the shared per-caller location registry (`crate::locations`, filled conversationally via
> `location_set`), on the same entitlement and through the same call `weather` makes.
>
> Records are keyed per **authenticated principal, not per person**: until **TERM #577**
> propagates a human identity to authorization, every human reaches the fleet as the same
> service principal and so shares one record. Two separately-authenticated principals do get
> two records and cannot reach each other's — that is what the env var never gave.

<img src="../../../assets/commute-architecture.svg" alt="Three TomTom-backed tools (commute_estimate, route_traffic, traffic_incidents) resolve named locations via CommuteConfig, geocode through TomTom, then call calculateRoute or incidentDetails; transit_plan is gated on SF511_API_TOKEN and not yet wired" width="100%">

## Configuration

| Env var | Required | Notes |
|---|---|---|
| `TOMTOM_API_KEY` | yes, for the 3 driving tools | unset → `NotConfigured` stubs for `commute_estimate`/`route_traffic`/`traffic_incidents`; `transit_plan` is unaffected |
| `SF511_API_TOKEN` | no | 511.org token for `transit_plan`; even when set, the trip planner is **not yet wired** (see below) |
| `TERMINUS_LOCATION_REGISTRY_PATH` | no | where saved locations live (shared with weather; default `~/.terminus/locations.json`) |

## Named-location resolution

`CommuteConfig::resolve` maps a caller's location keyword (case-insensitive) onto a name in
**that caller's** location registry, and looks it up through `locations::lookup`:

| Keywords | Resolves to |
|---|---|
| `home`, `house` | the caller's saved `home` |
| `work`, `office`, `the office` | the caller's saved `work` |
| `current`, `here`, `where i am` | the caller's saved `current` (the travel override) |
| `family`, `family home`, `parents` | the caller's saved `family` |
| any other name | looked up too — a user-chosen name like `the cabin` resolves; if nothing is saved under it, it falls through and is treated as a literal address/`lat,lon`, after IATA expansion (below) |
| a `lat,lon` pair | never looked up — it is already a place |

**Failure is never filled in with a guess**, and the three ways it can fail stay three
different answers:

| Outcome | What the caller gets |
|---|---|
| nothing saved under a well-known name | `NotConfigured` — an ask: *"I don't have a "home" saved for you… tell me the address, or say 'remember this is home'"* |
| caller is unentitled, or arrived with no identity | `NotConfigured` — *"saved locations aren't available on this connection"*. **No read happens at all**, so nothing is in memory to leak; a literal address still routes |
| the registry could not be read | `Execution` — *"I couldn't read your saved locations… that's a problem reading them, not an empty list"*. A **different error type** from an absent value, deliberately |

An omitted `origin` still *means* `home` — the tool's contract is unchanged — but `home` is the
caller's saved entry, and with none saved the tool asks rather than starting from somewhere
else. The display label names a saved place (`Home (…)`) only when one was actually used.

**IATA airport-code expansion** (`expand_iata`, `src/commute/mod.rs:91-134`): a bare 3-letter
alphabetic string geocodes to the wrong place on its own (e.g. TomTom might resolve "SJC" to
something other than San Jose International Airport), so any exact 3-letter code is expanded
to a full "Airport, City, ST" string before geocoding. 34 major US airports are hardcoded
(`ATL`, `LAX`, `ORD`, `DFW`, `DEN`, `JFK`, `LGA`, `EWR`, `SFO`, `SJC`, `OAK`, `SMF`, `SEA`,
`LAS`, `PHX`, `SAN`, `MCO`, `TPA`, `MIA`, `FLL`, `CLT`, `IAH`, `BOS`, `MSP`, `DTW`, `PHL`,
`BWI`, `IAD`, `DCA`, `SLC`, `AUS`, `BNA`, `PDX`, `HNL`); an unrecognized 3-letter string (e.g.
`"ZZZ"`) passes through unchanged.

## Geocoding & routing internals

`geocode` (`src/commute/mod.rs:139-183`) accepts a literal `"lat,lon"` pair as-is
(`is_coord_pair`), otherwise calls `GET https://api.tomtom.com/search/2/geocode/{urlencoded
location}.json?limit=1`, taking the first result's `position.lat`/`position.lon`. Failure to
geocode returns `NotFound`.

`calc_route` (`src/commute/mod.rs:220-280`) calls `GET
https://api.tomtom.com/routing/1/calculateRoute/{origin}:{dest}/json` with `traffic=true` and
`travelMode={mode}`. Timing precedence: `arrive_by` (if given) sets `arriveAt`, and TomTom
plans backwards; otherwise `depart_at` sets `departAt` unless it is `"now"` or empty, in which
case the parameter is omitted entirely (TomTom's own default = live traffic now). The response
summary yields travel time, no-traffic time, delay, and distance, each rounded to one decimal
place; distance is converted meters → miles via `METERS_PER_MILE = 1609.34`.

`traffic_summary` (`src/commute/mod.rs:282-297`) buckets the delay into four human-readable
tiers: **clear** (<1 min), **light** (<5 min), **moderate** (<15 min, shows a percentage over
baseline), **heavy** (≥15 min, shows percentage). `format_route` renders the full report,
including a `"Leave by: {departure} to arrive at {arrival}"` line only when `arrive_by` was
supplied.

## commute_estimate

Traffic-aware commute for a typical day, defaulting to home→work
(`src/commute/mod.rs:323-368`).

**Input schema**

| Field | Type | Required | Default |
|---|---|---|---|
| `from` | string: a saved name (`home`/`work`/`family`/…) or an address | no | `home` (the caller's saved one; asks if absent) |
| `to` | string: same | no | `work` |
| `depart_at` | string: `"now"` or ISO time | no | `now` |
| `arrive_by` | string: ISO time | no | — |

Empty strings for `from`/`to`/`depart_at` are treated as "not provided" (the model sometimes
passes `""` explicitly rather than omitting the key), so the defaults still apply.

**Errors:** `NotConfigured` if `TOMTOM_API_KEY` is unset, or if a referenced named place is
not saved / not available to this caller; `Execution` if the registry could not be read;
`Http`/`NotFound` from geocoding/routing failures.

## route_traffic

The general-purpose version: any two locations, any travel mode
(`src/commute/mod.rs:370-424`).

**Input schema**

| Field | Type | Required | Default |
|---|---|---|---|
| `origin` | string: address, `lat,lon`, IATA code, or a saved name (`home`/`work`/`family`/…) | no | `home` (the caller's saved one; asks if absent) |
| `destination` | string: same | **yes** | — |
| `mode` | string: `car`\|`truck`\|`pedestrian`\|`bicycle` | no | `car`; any other value silently falls back to `car` |
| `depart_at` | string | no | `now` |
| `arrive_by` | string: ISO time | no | — |

Unlike `commute_estimate`, `origin` is optional (defaults to the caller's saved `home`) but `destination` is
required and validated explicitly — a missing/empty `destination` raises `InvalidArgument`
before any resolution or network call. When `mode != "car"`, the mode is appended to the
formatted output as an extra line.

## traffic_incidents

Lists current accidents, construction, and closures near a location
(`src/commute/mod.rs:426-499`).

**Input schema**

| Field | Type | Required | Default |
|---|---|---|---|
| `location` | string: address, `lat,lon`, or a saved name (`home`/`work`/`family`/…) | yes | — |
| `radius_miles` | number | no | `10`, clamped to `1..=50` |

**Behavior.** After geocoding the center point, a bounding box is computed with `dlat =
radius/69.0` and `dlon = radius/54.6` (approximate miles-per-degree at mid-latitudes), then
`GET https://api.tomtom.com/traffic/services/5/incidentDetails` with that `bbox` and a fixed
`fields` selector requesting `type`, `iconCategory`, `magnitudeOfDelay`, `events{description,
code}`, `from`, `to`. Up to 10 incidents are rendered as bullet lines with an optional `(from →
to)` location suffix. Zero incidents returns a plain "No traffic incidents within {radius}
miles" message rather than an empty list.

## transit_plan

Public-transit planning for the Bay Area via 511.org — **stub, not yet wired**
(`src/commute/mod.rs:501-538`).

**Input schema**

| Field | Type | Required |
|---|---|---|
| `origin` | string | yes |
| `destination` | string | yes |
| `depart_at` | string | no |

**Behavior.** This tool always errors today, in one of two ways, regardless of the
`TOMTOM_API_KEY` configuration state (it is registered independently of the other three
tools' config gate):

- `SF511_API_TOKEN` unset → `NotConfigured`: "Public transit needs a free 511.org token. Get
  one at https://511.org/open-data/token and set SF511_API_TOKEN."
- `SF511_API_TOKEN` set → `NotConfigured`: "SF511_API_TOKEN is set but the 511 trip-planner is
  not yet wired. Driving tools (commute_estimate / route_traffic) are fully available."

The arguments are never read in the second branch — this is an honest placeholder rather than
a fabricated response, matching this codebase's convention of returning `NotConfigured` for
genuinely unimplemented backends (see also `council_convene` in the [council](council.md)
module).

## Registration

`register()` (`src/commute/mod.rs:565-581`) is asymmetric: when `TOMTOM_API_KEY` is set, all
four tools register live (three real + `TransitPlan`, which is always the same struct
regardless of key presence); when unset, `commute_estimate`/`route_traffic`/
`traffic_incidents` become `NotConfiguredStub`s naming `TOMTOM_API_KEY`, but `TransitPlan`
still registers normally (its own `SF511_API_TOKEN` gate is independent, checked inside
`execute`, not at registration time).
