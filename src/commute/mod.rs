//! Commute & traffic tools — traffic-aware routing via the TomTom API, with a
//! Bay Area public-transit planner via 511.org.
//!
//! Four tools:
//!   commute_estimate  — typical-day commute (home↔work by default), traffic-aware,
//!                       with "when to leave" when an arrival time is given.
//!   route_traffic     — any two locations, any travel mode, timing + live traffic.
//!   traffic_incidents — accidents / construction / closures near a place.
//!   transit_plan      — public-transit trip planning (511.org, Bay Area).
//!
//! # Named places come from the REGISTRY, never from the process environment
//!
//! "home", "work"/"office", "family" and any name the user chose ("the cabin")
//! resolve through the shared per-caller location registry
//! ([`crate::locations`]) — the same call `weather` makes, on the same
//! entitlement, against the same store. A name the registry has READ and does
//! not hold is then treated as a literal address (or "lat,lon", or an IATA
//! code) and geocoded.
//!
//! When the registry cannot be read at all, only input that could not
//! PLAUSIBLY be a saved name falls through to literal geocoding — see
//! [`is_unambiguously_literal`].
//!
//! ## `COMMUTE_HOME` / `COMMUTE_WORK` / `COMMUTE_FAMILY` are not read (TERM #591)
//!
//! They used to be, right here: `CommuteConfig::from_env` read all three at
//! registration and `resolve` handed them to whoever called the tool. That is
//! one person's home address held process-globally and returned to every
//! entitled caller — the exact disclosure the sibling change deleted from
//! `weather`, left standing in commute. Removing it from one consumer and not
//! the other closes half a hole: the variables are unset on the live host
//! today, and setting them is an ordinary operator action.
//!
//! Narrowing the env read to "the operator's principal" is NOT the fix and must
//! not be reintroduced. Until TERM #577 propagates a human identity, every
//! person in the household authenticates as the SAME service principal, so a
//! principal-keyed gate names the service they share rather than the operator.
//! See [`crate::weather::location::Routine`] for the long form.
//!
//! ## Whose places? Per authenticated PRINCIPAL, not per person (TERM #577)
//!
//! Registry records are keyed on the caller identity the gateway verified. That
//! is a SERVICE principal today: every human talking to Lumina arrives as the
//! same one, so everyone behind it shares a record and sees the same saved
//! home. This module does not claim otherwise anywhere a user can read it, and
//! the fix is TERM #577 (propagating a human identity to authorization), not
//! anything commute can do. What IS true today, and is what the env read never
//! gave: two separately-authenticated principals get two records, and neither
//! can reach the other's.
//!
//! ## Degrade honestly, never invent
//!
//! A named place that is not saved is an ASK, never a substitution — and in
//! particular an omitted `origin` still MEANS "home", but when no home is saved
//! it asks for one instead of quietly routing from somewhere else. "Nothing
//! saved under that name", "you may not use saved locations here" and "I could
//! not read the registry" are three different answers and stay worded
//! differently. See [`Resolution`].
//!
//! Required env:
//!   TOMTOM_API_KEY   — TomTom routing + geocoding (driving tools)
//! Optional env:
//!   SF511_API_TOKEN  — 511.org token for transit_plan (free, https://511.org/open-data/token)

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::locations::{self, store::LocationStore, CallerKey, Lookup};
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool, ToolOutput};

const METERS_PER_MILE: f64 = 1609.34;

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct CommuteConfig {
    api_key: String,
    /// TERM #591: the shared per-caller location registry — the ONLY source of
    /// named places. There are deliberately no `home`/`work`/`family` fields
    /// here any more: a field on a process-wide config is, by construction, one
    /// person's address served to every caller.
    locations: Arc<dyn LocationStore>,
}

impl CommuteConfig {
    fn from_env() -> Result<Self, ToolError> {
        let api_key = std::env::var("TOMTOM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("TOMTOM_API_KEY not set".into()))?;
        Ok(Self { api_key, locations: locations::shared_store() })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// CXEG-05 house-style: `SF511_API_TOKEN` is secret-shaped, so it is read
    /// through this one dedicated accessor (mirrors `wizard_db_url`/
    /// `vector_db_url`/this file's own `CommuteConfig::from_env` above)
    /// rather than inline in `TransitPlan::execute` below.
    fn sf511_api_token() -> Option<String> {
        std::env::var("SF511_API_TOKEN").ok().filter(|s| !s.is_empty())
    }

    /// Resolve a user-supplied location for THIS caller.
    ///
    /// One registry read per place, through [`locations::lookup`] — the same
    /// door, gate and store `weather` uses. Nothing here reads the process
    /// environment, and there is no second tier below the registry for a name
    /// the registry does not know.
    fn resolve(
        &self,
        input: &str,
        caller: CallerContext,
        key: Option<&CallerKey>,
    ) -> Resolution {
        let raw = input.trim();
        let alias = registry_name(raw);
        // A coordinate pair is a place the user just gave us; it is never a
        // saved name, and looking it up would be a pointless read.
        let name = if is_coord_pair(raw) { None } else { Some(alias.unwrap_or(raw)) };

        if let Some(name) = name {
            match locations::lookup(self.locations.as_ref(), key, caller, name) {
                Lookup::Found(entry) => {
                    return Resolution::Place { address: entry.value, saved_as: Some(name.to_string()) }
                }
                // Nothing saved under this name. For a WELL-KNOWN alias
                // ("home", "work", …) that is the whole answer and we ask —
                // substituting anything else is the bug this item closes.
                Lookup::NotSet if alias.is_some() => return Resolution::NotSaved(name.to_string()),
                // For anything else the user typed a place, not a nickname:
                // "Reno" is a city whether or not it is also a saved name.
                //
                // This fall-through rests on a fact, not a guess: the registry
                // ANSWERED, and the answer was "no such name". There is nothing
                // left to be uncertain about, so geocoding the string the user
                // typed is the only remaining reading of it. That is why this
                // arm is broad and the `Unavailable` arm below is not.
                Lookup::NotSet => {}
                // Not entitled, or no identity: nothing was read. A literal
                // address still routes — that discloses nothing and refusing it
                // would be a refusal to do arithmetic on the user's own input —
                // but a name we would have to LOOK UP cannot be answered.
                //
                // Deliberately still keyed on `alias`, not on
                // `is_unambiguously_literal`: this arm is about an authorization
                // decision made BEFORE any read, and its current shape is
                // settled behaviour. Narrowing it the same way would refuse
                // "Reno" to an unentitled caller for no privacy gain — nothing
                // was read, so nothing can be disclosed or mis-reported. The
                // absence-vs-failure collapse the arm below fixes does not
                // arise here, because there is no failure to report.
                Lookup::Denied if alias.is_some() => return Resolution::NoAccess,
                Lookup::Denied => {}
                // The registry exists and could not be read. Distinct from
                // "nothing saved", and it must stay distinct in what we say.
                //
                // The fall-through here is NARROW on purpose. We do not know
                // whether the user has a "the cabin" saved — that is precisely
                // what failed — so geocoding the words "the cabin" would convert
                // "I couldn't read your saved locations" into "I routed you
                // somewhere", possibly somewhere real and wrong. A confident
                // wrong route is worse than an error, so anything that could
                // PLAUSIBLY be a saved name surfaces the read failure, and only
                // input that could not plausibly be one (a coordinate pair, a
                // house-numbered street address, a "City, ST"/postal shape, a
                // known airport code) falls through. That keeps the property the
                // broad version was protecting — an ordinary address still
                // routes when the registry is sick — without keeping the
                // collapse. See [`is_unambiguously_literal`] for the predicate
                // and the residual it does not cover (a bare city name).
                //
                // A well-known alias is never unambiguously literal, so the
                // strict answer it already gave is preserved by construction;
                // `well_known_aliases_are_never_unambiguously_literal` pins it.
                Lookup::Unavailable(_) if !is_unambiguously_literal(raw) => {
                    return Resolution::Unreadable(name.to_string())
                }
                Lookup::Unavailable(_) => {}
            }
        }

        // A literal address, "lat,lon", or a bare 3-letter IATA code (e.g.
        // "SJC") which geocodes to the wrong place unless expanded first.
        Resolution::Place {
            address: expand_iata(&raw.to_lowercase()).unwrap_or_else(|| raw.to_string()),
            saved_as: None,
        }
    }
}

/// The well-known aliases, mapped onto the registry names every consumer
/// shares ([`locations::HOME`] / [`locations::WORK`] / [`locations::CURRENT`]).
///
/// `Some` means "the user named a SAVED place" — the strict path, where an
/// absent entry is an ask rather than a string to geocode. `None` means "the
/// user gave us a place", which is still looked up (so "the cabin" works) but
/// falls through to literal geocoding when the registry has nothing.
fn registry_name(input: &str) -> Option<&'static str> {
    match input.trim().to_lowercase().as_str() {
        "home" | "house" => Some(locations::HOME),
        "work" | "office" | "the office" => Some(locations::WORK),
        "current" | "here" | "where i am" => Some(locations::CURRENT),
        // Not a well-known name, but a conventional one worth treating
        // strictly: someone saying "family" means a place they expect us to
        // know, and geocoding the literal word "family" is nonsense.
        "family" | "family home" | "parents" => Some("family"),
        _ => None,
    }
}

/// Could this input NOT plausibly be a name the user saved?
///
/// Used in exactly one place: deciding whether an unreadable registry may be
/// stepped over. It is not "does this look like an address" — it is the much
/// stricter "is there no reading of this string on which it is a nickname",
/// because the two ways of being wrong are not symmetric:
///
/// * Wrongly calling a NAME literal → we geocode the words and confidently
///   route somewhere that is not where the user meant, while the real answer
///   ("I couldn't read your saved locations") is never said. That is the
///   failure this predicate exists to prevent.
/// * Wrongly calling a LITERAL a name → the user gets an accurate error and
///   retries. Recoverable, and honest.
///
/// So every rule below is a shape a person does not give a place they saved.
/// Anything else — including a bare city ("Reno") — is treated as a name.
///
/// The rules:
/// 1. A coordinate pair. (Short-circuited before the lookup as well; kept here
///    so the predicate is true on its own terms.)
/// 2. A leading HOUSE NUMBER followed by at least one word — "1 Placeholder
///    Way". A saved place is nicknamed "the cabin", never "1 Placeholder Way";
///    a leading number is the single strongest signal available and it needs no
///    gazetteer. This is the rule that keeps ordinary street addresses routing
///    through a registry outage.
/// 3. A standalone postal code ("00000", "00000-1234") — pure structure, no
///    natural-language reading at all.
/// 4. A trailing region token: the last comma-separated segment is a two-letter
///    state/province code, optionally with a postal code ("San Jose, CA",
///    "Examplecity, EX 00000"). Nobody saves a place under a string ending in a
///    state abbreviation.
/// 5. A known IATA airport code, from the CLOSED table in [`expand_iata`] —
///    "SJC" is an airport because we can name it, not because it is three
///    letters. "ZZZ" is not, and is treated as a name.
///
/// **What this predicate does NOT have to catch.** A string
/// [`locations::canonical_name`] would reject — one with a comma, or over the
/// name-length limit — could never have been SAVED under (it is the same
/// function that gates writes), so [`locations::lookup`] answers `NotSet`
/// without reading and such input never reaches the `Unavailable` arm at all.
/// Rules 1 and 4 are therefore belt-and-braces in the current call path: a
/// coordinate pair is short-circuited before the lookup, and every "City, ST"
/// shape carries a comma. They are kept because they are true, cheap, and the
/// predicate should not silently depend on an upstream charset to stay correct;
/// `a_name_shaped_input_is_the_only_kind_that_can_reach_the_read_failure` pins
/// the dependency so a change to it is visible.
///
/// **The residual, stated plainly:** a BARE CITY NAME ("Reno", "Boise") is not
/// unambiguously literal and therefore does NOT route while the registry is
/// unreadable — the user is told the registry could not be read and asked for
/// an address. Deciding otherwise needs a gazetteer we do not have, and
/// guessing would reopen exactly the collapse this closes: "Reno" is a real
/// city AND a perfectly ordinary thing to have saved. This is the deliberate
/// cost of erring toward "name"; it applies only while the store is failing.
fn is_unambiguously_literal(input: &str) -> bool {
    let s = input.trim();
    if s.is_empty() {
        return false;
    }
    is_coord_pair(s)
        || starts_with_house_number(s)
        || is_postal_code(s)
        || ends_with_region_token(s)
        || expand_iata(&s.to_lowercase()).is_some()
}

/// Rule 2: a house-number token, then at least one word.
///
/// The number must be a plain street number — digits, optionally with a letter
/// suffix (`12A`) or a hyphen/slash range (`12-14`, `12/3`). A decimal point is
/// excluded so a stray "37.75 something" is never mistaken for one.
fn starts_with_house_number(s: &str) -> bool {
    let mut tokens = s.split_whitespace();
    let Some(first) = tokens.next() else { return false };
    let head = first.trim_end_matches(',');
    let numberish = head.len() <= 10
        && head.starts_with(|c: char| c.is_ascii_digit())
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/');
    // …and something for the number to be ON.
    numberish && tokens.any(|t| t.chars().any(|c| c.is_alphabetic()))
}

/// Rule 3: the whole string is a postal code — `00000` or `00000-1234`.
fn is_postal_code(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('-') {
        Some((head, tail)) => is_digits(head, 5) && is_digits(tail, 4),
        None => is_digits(s, 5),
    }
}

fn is_digits(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| c.is_ascii_digit())
}

/// Rule 4: the last comma-separated segment is a region token — a two-letter
/// state/province code, alone or followed by a postal code, or a postal code
/// on its own.
fn ends_with_region_token(s: &str) -> bool {
    let Some((_, last)) = s.rsplit_once(',') else { return false };
    let last = last.trim();
    if last.is_empty() {
        return false;
    }
    let mut parts = last.split_whitespace();
    let Some(head) = parts.next() else { return false };
    let is_state_code = head.len() == 2 && head.chars().all(|c| c.is_ascii_alphabetic());
    match parts.next() {
        // "CA 00000" — a state code plus a postal code, nothing after it.
        Some(zip) => is_state_code && is_postal_code(zip) && parts.next().is_none(),
        // "CA", or a bare postal code as the final segment.
        None => is_state_code || is_postal_code(head),
    }
}

/// What resolving one place produced.
///
/// Four outcomes rather than `Option<String>`, because the three failures are
/// three different things to say and collapsing them is how "I don't know" gets
/// reported as "there's nothing there" — or, worse, gets filled in with a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// Ready to geocode. `saved_as` is the registry name it came from, used for
    /// the display label ("Home (…)"), or `None` for a literal place.
    Place { address: String, saved_as: Option<String> },
    /// The registry was READ and holds nothing live under this name.
    NotSaved(String),
    /// This caller may not use saved locations (or arrived with no identity).
    /// Nothing was read.
    NoAccess,
    /// The registry could not be read. NOT the same as [`Resolution::NotSaved`].
    Unreadable(String),
}

impl Resolution {
    /// The address, or the honest thing to say instead.
    ///
    /// The error TYPE carries the distinction as well as the wording:
    /// `NotConfigured` for "there is nothing to use", `Execution` for "the
    /// lookup itself failed" — an absent value and a broken read are not the
    /// same class of problem and a caller inspecting the error should not have
    /// to parse prose to tell them apart.
    fn address(self) -> Result<(String, Option<String>), ToolError> {
        match self {
            Resolution::Place { address, saved_as } => Ok((address, saved_as)),
            Resolution::NotSaved(name) => Err(ToolError::NotConfigured(format!(
                "I don't have a \"{name}\" saved for you, so I can't use it here. \
                 Tell me the address and I'll use it — or say \"remember this is {name}\" \
                 and I'll keep it for next time."
            ))),
            Resolution::NoAccess => Err(ToolError::NotConfigured(
                "Saved locations aren't available on this connection, so I can't look up \
                 a named place. Give me the addresses directly, or ask me again from your \
                 own session."
                    .into(),
            )),
            Resolution::Unreadable(name) => Err(ToolError::Execution(format!(
                "I couldn't read your saved locations just now, so I can't tell whether you \
                 have a \"{name}\" saved. That's a problem reading them, not an empty list — \
                 tell me the address and I'll use that."
            ))),
        }
    }
}

/// Expand a 3-letter IATA airport code into a full "Airport, City, ST" string
/// for reliable geocoding. Returns None for non-codes (passed through as-is).
fn expand_iata(input: &str) -> Option<String> {
    let code = input.trim();
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let full = match code.to_ascii_uppercase().as_str() {
        "ATL" => "Hartsfield-Jackson Atlanta International Airport, Atlanta, GA",
        "LAX" => "Los Angeles International Airport, Los Angeles, CA",
        "ORD" => "O'Hare International Airport, Chicago, IL",
        "DFW" => "Dallas/Fort Worth International Airport, Dallas, TX",
        "DEN" => "Denver International Airport, Denver, CO",
        "JFK" => "John F. Kennedy International Airport, New York, NY",
        "LGA" => "LaGuardia Airport, New York, NY",
        "EWR" => "Newark Liberty International Airport, Newark, NJ",
        "SFO" => "San Francisco International Airport, San Francisco, CA",
        "SJC" => "San Jose International Airport, San Jose, CA",
        "OAK" => "Oakland International Airport, Oakland, CA",
        "SMF" => "Sacramento International Airport, Sacramento, CA",
        "SEA" => "Seattle-Tacoma International Airport, Seattle, WA",
        "LAS" => "Harry Reid International Airport, Las Vegas, NV",
        "PHX" => "Phoenix Sky Harbor International Airport, Phoenix, AZ",
        "SAN" => "San Diego International Airport, San Diego, CA",
        "MCO" => "Orlando International Airport, Orlando, FL",
        "TPA" => "Tampa International Airport, Tampa, FL",
        "MIA" => "Miami International Airport, Miami, FL",
        "FLL" => "Fort Lauderdale-Hollywood International Airport, Fort Lauderdale, FL",
        "CLT" => "Charlotte Douglas International Airport, Charlotte, NC",
        "IAH" => "George Bush Intercontinental Airport, Houston, TX",
        "BOS" => "Boston Logan International Airport, Boston, MA",
        "MSP" => "Minneapolis-Saint Paul International Airport, Minneapolis, MN",
        "DTW" => "Detroit Metropolitan Airport, Detroit, MI",
        "PHL" => "Philadelphia International Airport, Philadelphia, PA",
        "BWI" => "Baltimore/Washington International Airport, Baltimore, MD",
        "IAD" => "Washington Dulles International Airport, Dulles, VA",
        "DCA" => "Ronald Reagan Washington National Airport, Arlington, VA",
        "SLC" => "Salt Lake City International Airport, Salt Lake City, UT",
        "AUS" => "Austin-Bergstrom International Airport, Austin, TX",
        "BNA" => "Nashville International Airport, Nashville, TN",
        "PDX" => "Portland International Airport, Portland, OR",
        "HNL" => "Daniel K. Inouye International Airport, Honolulu, HI",
        _ => return None,
    };
    Some(full.to_string())
}

// ── Geocoding ───────────────────────────────────────────────────────────────

/// Return "lat,lon" for an address. Accepts a coordinate pair as-is.
async fn geocode(
    client: &reqwest::Client,
    key: &str,
    location: &str,
) -> Result<String, ToolError> {
    // Already a coordinate pair? ("37.75,-122.41")
    if is_coord_pair(location) {
        return Ok(location.replace(' ', ""));
    }

    let url = format!(
        "https://api.tomtom.com/search/2/geocode/{}.json",
        urlencode(location)
    );
    let resp = client
        .get(&url)
        .query(&[("key", key), ("limit", "1")])
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Geocode HTTP {} for '{location}'",
            resp.status()
        )));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let first = body
        .get("results")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| ToolError::NotFound(format!("Could not geocode '{location}'")))?;
    let pos = first
        .get("position")
        .ok_or_else(|| ToolError::NotFound(format!("No position for '{location}'")))?;
    let lat = pos.get("lat").and_then(Value::as_f64).ok_or_else(|| {
        ToolError::NotFound(format!("No latitude for '{location}'"))
    })?;
    let lon = pos.get("lon").and_then(Value::as_f64).ok_or_else(|| {
        ToolError::NotFound(format!("No longitude for '{location}'"))
    })?;
    Ok(format!("{lat},{lon}"))
}

fn is_coord_pair(s: &str) -> bool {
    let parts: Vec<&str> = s.split(',').collect();
    parts.len() == 2
        && parts.iter().all(|p| p.trim().parse::<f64>().is_ok())
}

/// Minimal percent-encoding for path segments (TomTom geocode path).
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "%20".to_string(),
            other => other
                .to_string()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

// ── Routing ─────────────────────────────────────────────────────────────────

struct RouteResult {
    travel_min: f64,
    no_traffic_min: f64,
    delay_min: f64,
    distance_miles: f64,
    departure: String,
    arrival: String,
}

/// Call the TomTom routing API between two "lat,lon" points.
/// `depart_at` of "now" uses live traffic; an ISO timestamp uses predictive
/// traffic. `arrive_by` (ISO) plans backwards to compute the departure time.
async fn calc_route(
    client: &reqwest::Client,
    key: &str,
    origin: &str,
    dest: &str,
    depart_at: &str,
    arrive_by: Option<&str>,
    mode: &str,
) -> Result<RouteResult, ToolError> {
    let path = format!(
        "https://api.tomtom.com/routing/1/calculateRoute/{origin}:{dest}/json"
    );

    let mut params: Vec<(&str, String)> = vec![
        ("key", key.to_string()),
        ("traffic", "true".to_string()),
        ("travelMode", mode.to_string()),
    ];
    if let Some(arrive) = arrive_by {
        params.push(("arriveAt", arrive.to_string()));
    } else if depart_at != "now" && !depart_at.is_empty() {
        params.push(("departAt", depart_at.to_string()));
    }

    let resp = client
        .get(&path)
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Routing HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let summary = body
        .get("routes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("summary"))
        .ok_or_else(|| ToolError::NotFound("No route found".into()))?;

    let travel_sec = summary.get("travelTimeInSeconds").and_then(Value::as_f64).unwrap_or(0.0);
    let delay_sec = summary.get("trafficDelayInSeconds").and_then(Value::as_f64).unwrap_or(0.0);
    let dist_m = summary.get("lengthInMeters").and_then(Value::as_f64).unwrap_or(0.0);

    Ok(RouteResult {
        travel_min: (travel_sec / 60.0 * 10.0).round() / 10.0,
        no_traffic_min: ((travel_sec - delay_sec) / 60.0 * 10.0).round() / 10.0,
        delay_min: (delay_sec / 60.0 * 10.0).round() / 10.0,
        distance_miles: (dist_m / METERS_PER_MILE * 10.0).round() / 10.0,
        departure: summary.get("departureTime").and_then(Value::as_str).unwrap_or("").to_string(),
        arrival: summary.get("arrivalTime").and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

fn traffic_summary(delay_min: f64, baseline_min: f64) -> String {
    let pct = if baseline_min > 0.0 {
        (delay_min / baseline_min * 100.0).round() as i64
    } else {
        0
    };
    if delay_min < 1.0 {
        "Traffic is clear — normal travel time".to_string()
    } else if delay_min < 5.0 {
        format!("Light traffic — about {} min added", delay_min.round() as i64)
    } else if delay_min < 15.0 {
        format!("Moderate traffic — {} extra min ({pct}% longer)", delay_min.round() as i64)
    } else {
        format!("Heavy traffic — {} extra min ({pct}% longer than normal)", delay_min.round() as i64)
    }
}

/// Build a human-readable report from a route result.
fn format_route(label_from: &str, label_to: &str, r: &RouteResult, arrive_by: Option<&str>) -> String {
    let mut out = format!("**{label_from} → {label_to}**\n");
    out.push_str(&format!(
        "- With traffic: **{:.0} min** ({:.1} mi)\n",
        r.travel_min, r.distance_miles
    ));
    out.push_str(&format!("- Without traffic: {:.0} min\n", r.no_traffic_min));
    out.push_str(&format!("- {}\n", traffic_summary(r.delay_min, r.no_traffic_min)));
    if arrive_by.is_some() && !r.departure.is_empty() {
        out.push_str(&format!("- **Leave by: {}** to arrive at {}\n", r.departure, r.arrival));
    } else {
        if !r.departure.is_empty() {
            out.push_str(&format!("- Depart: {}\n", r.departure));
        }
        if !r.arrival.is_empty() {
            out.push_str(&format!("- Arrive: {}\n", r.arrival));
        }
    }
    out
}

// ── Tools ───────────────────────────────────────────────────────────────────

struct CommuteEstimate { cfg: CommuteConfig }
struct RouteTraffic    { cfg: CommuteConfig }
struct TrafficIncidents { cfg: CommuteConfig }
struct TransitPlan;

#[async_trait]
impl RustTool for CommuteEstimate {
    fn name(&self) -> &str { "commute_estimate" }

    fn description(&self) -> &str {
        "Traffic-aware commute estimate for a typical day. Defaults to home→work, using \
the SAVED locations for this connection (location_set); pass from/to as 'home', \
'work'/'office', 'family', a saved name, or any address. Use arrive_by \
(ISO time) to find when to leave, or depart_at for a future-departure estimate."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from":      { "type": "string", "description": "Origin: home, work, family, or an address. Default: home" },
                "to":        { "type": "string", "description": "Destination: home, work, family, or an address. Default: work" },
                "depart_at": { "type": "string", "description": "'now' (default) or ISO time for a future-departure estimate" },
                "arrive_by": { "type": "string", "description": "ISO time you need to arrive by → returns when to leave" }
            }
        })
    }

    /// The identity-less path: no caller, so no saved locations. A route
    /// between two literal addresses still works; "home" asks. Fail-closed.
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted(), None).await
    }

    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text_only(self.run(args, caller, key.as_ref()).await?))
    }
}

impl CommuteEstimate {
    async fn run(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<&CallerKey>,
    ) -> Result<String, ToolError> {
        // Treat empty strings as "not provided" so the model can omit them and
        // still get the home→work default (it sometimes passes "" explicitly).
        // The default is a NAME, not a value: with nothing saved under it the
        // resolver asks rather than substituting.
        let from_in = args["from"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("home");
        let to_in = args["to"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("work");
        let depart_at = args["depart_at"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("now");
        let arrive_by = args["arrive_by"].as_str().filter(|s| !s.is_empty());

        let (from_addr, from_saved) = self.cfg.resolve(from_in, caller, key).address()?;
        let (to_addr, to_saved) = self.cfg.resolve(to_in, caller, key).address()?;

        let client = CommuteConfig::client()?;
        let o = geocode(&client, &self.cfg.api_key, &from_addr).await?;
        let d = geocode(&client, &self.cfg.api_key, &to_addr).await?;
        let route = calc_route(&client, &self.cfg.api_key, &o, &d, depart_at, arrive_by, "car").await?;

        Ok(format_route(
            &label(from_in, &from_addr, from_saved.as_deref()),
            &label(to_in, &to_addr, to_saved.as_deref()),
            &route,
            arrive_by,
        ))
    }
}

#[async_trait]
impl RustTool for RouteTraffic {
    fn name(&self) -> &str { "route_traffic" }

    fn description(&self) -> &str {
        "Check commute, traffic, drive time, and directions to your office, an \
airport, or any destination. Use when the user asks about traffic, commute, drive \
time, directions, or how long to get somewhere. origin and destination may be \
addresses, 'lat,lon', a 3-letter airport code (e.g. SJC, TPA), or the named places \
home/work/family (or any name saved via location_set). origin is OPTIONAL and defaults to the \
saved home — only destination is required. \
mode: car (default), truck, pedestrian, or bicycle. Supports depart_at / arrive_by."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "origin":      { "type": "string", "description": "Start: address, 'lat,lon', or home/work/family. Optional — defaults to home." },
                "destination": { "type": "string", "description": "End: address, 'lat,lon', or home/work/family" },
                "mode":        { "type": "string", "description": "car (default), truck, pedestrian, bicycle" },
                "depart_at":   { "type": "string", "description": "'now' (default) or ISO time" },
                "arrive_by":   { "type": "string", "description": "ISO time to arrive by → returns when to leave" }
            },
            "required": ["destination"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted(), None).await
    }

    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text_only(self.run(args, caller, key.as_ref()).await?))
    }
}

impl RouteTraffic {
    async fn run(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<&CallerKey>,
    ) -> Result<String, ToolError> {
        // An omitted origin still MEANS "home" — that is the tool's contract
        // and the user's own saved place. What changed (TERM #591) is where
        // "home" comes from and what happens when there isn't one: the caller's
        // registry entry, and an ASK when it is absent. Never a substitution.
        let origin_in = args["origin"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("home");
        let dest_in = args["destination"].as_str().filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'destination' is required (address, 'lat,lon', or home/work/family)".into()))?;
        let depart_at = args["depart_at"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("now");
        let arrive_by = args["arrive_by"].as_str().filter(|s| !s.is_empty());
        let mode = match args["mode"].as_str().unwrap_or("car") {
            m @ ("car" | "truck" | "pedestrian" | "bicycle") => m,
            _ => "car",
        };

        let (origin_addr, origin_saved) = self.cfg.resolve(origin_in, caller, key).address()?;
        let (dest_addr, dest_saved) = self.cfg.resolve(dest_in, caller, key).address()?;

        let client = CommuteConfig::client()?;
        let o = geocode(&client, &self.cfg.api_key, &origin_addr).await?;
        let d = geocode(&client, &self.cfg.api_key, &dest_addr).await?;
        let route = calc_route(&client, &self.cfg.api_key, &o, &d, depart_at, arrive_by, mode).await?;

        let mut out = format_route(
            &label(origin_in, &origin_addr, origin_saved.as_deref()),
            &label(dest_in, &dest_addr, dest_saved.as_deref()),
            &route,
            arrive_by,
        );
        if mode != "car" {
            out.push_str(&format!("- Mode: {mode}\n"));
        }
        Ok(out)
    }
}

#[async_trait]
impl RustTool for TrafficIncidents {
    fn name(&self) -> &str { "traffic_incidents" }

    fn description(&self) -> &str {
        "List current traffic incidents (accidents, construction, closures) near a \
location. Pass an address, 'lat,lon', or home/work/family, and an optional radius."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location":     { "type": "string", "description": "Center: address, 'lat,lon', or home/work/family" },
                "radius_miles": { "type": "number", "description": "Search radius in miles (default 10)" }
            },
            "required": ["location"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted(), None).await
    }

    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text_only(self.run(args, caller, key.as_ref()).await?))
    }
}

impl TrafficIncidents {
    async fn run(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<&CallerKey>,
    ) -> Result<String, ToolError> {
        let loc_in = args["location"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'location' is required".into()))?;
        let radius = args["radius_miles"].as_f64().unwrap_or(10.0).clamp(1.0, 50.0);

        let (loc_addr, _) = self.cfg.resolve(loc_in, caller, key).address()?;
        let client = CommuteConfig::client()?;
        let center = geocode(&client, &self.cfg.api_key, &loc_addr).await?;
        let parts: Vec<f64> = center.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() != 2 {
            return Err(ToolError::NotFound(format!("Could not resolve '{loc_in}'")));
        }
        let (lat, lon) = (parts[0], parts[1]);
        let dlat = radius / 69.0;
        let dlon = radius / 54.6;
        let bbox = format!("{},{},{},{}", lon - dlon, lat - dlat, lon + dlon, lat + dlat);

        let fields = "{incidents{type,properties{iconCategory,magnitudeOfDelay,events{description,code},from,to}}}";
        let resp = client
            .get("https://api.tomtom.com/traffic/services/5/incidentDetails")
            .query(&[("key", self.cfg.api_key.as_str()), ("bbox", bbox.as_str()), ("fields", fields)])
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ToolError::Http(format!("Incidents HTTP {}", resp.status())));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let incidents = body.get("incidents").and_then(Value::as_array).cloned().unwrap_or_default();

        if incidents.is_empty() {
            return Ok(format!("No traffic incidents within {radius:.0} miles of {loc_in}."));
        }

        let mut out = format!("{} incident(s) within {radius:.0} mi of {loc_in}:\n", incidents.len());
        for inc in incidents.iter().take(10) {
            let props = inc.get("properties").cloned().unwrap_or(json!({}));
            let desc = props.get("events").and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|e| e.get("description")).and_then(Value::as_str)
                .unwrap_or("Incident");
            let from = props.get("from").and_then(Value::as_str).unwrap_or("");
            let to = props.get("to").and_then(Value::as_str).unwrap_or("");
            let where_str = if !from.is_empty() || !to.is_empty() {
                format!(" ({from} → {to})")
            } else {
                String::new()
            };
            out.push_str(&format!("  • {desc}{where_str}\n"));
        }
        Ok(out)
    }
}

#[async_trait]
impl RustTool for TransitPlan {
    fn name(&self) -> &str { "transit_plan" }

    fn description(&self) -> &str {
        "Public-transit trip planning for the San Francisco Bay Area (BART, Caltrain, \
Muni, SamTrans, VTA) via 511.org. Pass origin and destination addresses or 'lat,lon'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "origin":      { "type": "string", "description": "Start address or 'lat,lon'" },
                "destination": { "type": "string", "description": "End address or 'lat,lon'" },
                "depart_at":   { "type": "string", "description": "'now' (default) or ISO time" }
            },
            "required": ["origin", "destination"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        // 511.org requires a free API token. Until SF511_API_TOKEN is set, return a
        // clear, actionable message rather than fabricating transit data.
        let _token = CommuteConfig::sf511_api_token().ok_or_else(|| ToolError::NotConfigured(
            "Public transit needs a free 511.org token. Get one at \
             https://511.org/open-data/token and set SF511_API_TOKEN.".into()
        ))?;
        // NOTE: 511.org trip-planning wiring lands once a token is configured.
        Err(ToolError::NotConfigured(
            "SF511_API_TOKEN is set but the 511 trip-planner is not yet wired. \
             Driving tools (commute_estimate / route_traffic) are fully available.".into(),
        ))
    }
}

/// Pretty label: name the saved place AND show where it resolved to.
///
/// Driven by whether the registry actually answered (`saved_as`), not by a
/// keyword list — so the label says "Home (…)" exactly when a stored `home` was
/// used, and can never announce a saved place for a value that came from
/// somewhere else. Showing the resolved address is deliberate: the user should
/// always be able to see WHICH place we used, which is also how a wrong saved
/// entry becomes visible instead of silently shaping every answer.
fn label(input: &str, resolved: &str, saved_as: Option<&str>) -> String {
    match saved_as {
        Some(name) => format!("{} ({})", titlecase(name), short_addr(resolved)),
        None => input.trim().to_string(),
    }
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// First line / first comma-segment of an address for compact display.
fn short_addr(addr: &str) -> String {
    addr.split(',').next().unwrap_or(addr).trim().to_string()
}

// ── Registration ────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match CommuteConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(CommuteEstimate { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(RouteTraffic { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(TrafficIncidents { cfg }));
            registry.register_or_replace(Box::new(TransitPlan));
        }
        Err(e) => {
            tracing::warn!("Commute tools not configured: {e}. Registering stubs.");
            registry.register_or_replace(Box::new(NotConfiguredStub("commute_estimate")));
            registry.register_or_replace(Box::new(NotConfiguredStub("route_traffic")));
            registry.register_or_replace(Box::new(NotConfiguredStub("traffic_incidents")));
            registry.register_or_replace(Box::new(TransitPlan));
        }
    }
}

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "Commute tool (TOMTOM_API_KEY not configured)" }
    fn parameters(&self) -> Value { json!({"type": "object", "properties": {}}) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured("TOMTOM_API_KEY not set".into()))
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    use crate::locations::store::fake::{BrokenStore, CountingBrokenStore, CountingStore};
    use crate::locations::{CallerKey, HOME, WORK};

    // ── Fixtures ────────────────────────────────────────────────────────────
    //
    // Every address below is an obvious placeholder. This repo publishes a
    // PII-scrubbed public mirror, and a "realistic" fixture address is
    // indistinguishable from a real leak to whoever reads it later.

    const SAVED_HOME: &str = "1 Placeholder Way, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a saved home address
    const SAVED_WORK: &str = "2 Placeholder Row, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a saved work address
    const SAVED_FAMILY: &str = "3 Placeholder Close, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a saved family address
    const LITERAL: &str = "4 Literal Street, Examplecity"; // pii-test-fixture: obvious placeholder standing in for an address the user typed
    /// What a COMMUTE_* variable would hold if an operator set one. No commute
    /// answer may ever contain it — see `no_caller_can_obtain_a_commute_env_value`.
    const ENV_PLACEHOLDER: &str = "9 Legacy Lane, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a COMMUTE_HOME/COMMUTE_WORK value that must never resolve

    /// A caller entitled to stored-location context — what the gateway derives
    /// for a principal holding the `commute_estimate` grant.
    fn entitled() -> CallerContext {
        CallerContext::entitled_for_test_only(false, true)
    }

    /// A household guest: may call the tool, entitled to nothing.
    fn guest() -> CallerContext {
        CallerContext::untrusted()
    }

    fn key(name: &str) -> CallerKey {
        CallerKey::for_principal_name(name).unwrap()
    }

    /// A config over a store the test controls.
    fn cfg_with(store: Arc<dyn LocationStore>) -> CommuteConfig {
        CommuteConfig { api_key: "testkey".into(), locations: store }
    }

    /// The default fixture: an entitled caller with home, work and family saved.
    fn cfg() -> (CommuteConfig, CallerKey) {
        let store = Arc::new(CountingStore::new());
        let k = key("alpha");
        for (name, value) in
            [(HOME, SAVED_HOME), (WORK, SAVED_WORK), ("family", SAVED_FAMILY)]
        {
            match locations::set(store.as_ref(), Some(&k), entitled(), name, value, None, true) {
                locations::WriteOutcome::Stored { .. } => {}
                other => panic!("seed failed: {other:?}"),
            }
        }
        (cfg_with(store), k)
    }

    fn address(r: Resolution) -> String {
        r.address().expect("expected a resolved place").0
    }

    /// POSITIVE CONTROL. A caller with saved places gets them back — an
    /// implementation that merely DELETED the env read (and always answered
    /// "nothing saved") passes every negative test below and fails this one.
    #[test]
    fn saved_names_resolve_from_the_registry() {
        let (c, k) = cfg();
        assert_eq!(address(c.resolve("home", entitled(), Some(&k))), SAVED_HOME);
        assert_eq!(address(c.resolve("Work", entitled(), Some(&k))), SAVED_WORK);
        assert_eq!(address(c.resolve("the office", entitled(), Some(&k))), SAVED_WORK);
        assert_eq!(address(c.resolve("family home", entitled(), Some(&k))), SAVED_FAMILY);
    }

    /// A user-chosen name ("the cabin") is registry data too — the registry was
    /// never a home/work pair, and commute needed no registry change to use it.
    #[test]
    fn a_user_chosen_name_resolves_from_the_registry() {
        let (c, k) = cfg();
        match locations::set(
            c.locations.as_ref(),
            Some(&k),
            entitled(),
            "the cabin",
            LITERAL,
            None,
            true,
        ) {
            locations::WriteOutcome::Stored { .. } => {}
            other => panic!("seed failed: {other:?}"),
        }
        assert_eq!(address(c.resolve("the cabin", entitled(), Some(&k))), LITERAL);
    }

    /// One caller's saved home is not another's. The registry is keyed per
    /// caller; this is the property the process-global env var never had.
    #[test]
    fn another_callers_record_is_not_visible() {
        let (c, _) = cfg();
        let other = key("beta");
        assert!(matches!(
            c.resolve("home", entitled(), Some(&other)),
            Resolution::NotSaved(_)
        ));
    }

    #[test]
    fn resolve_literal_address_passthrough() {
        let (c, k) = cfg();
        assert_eq!(address(c.resolve(LITERAL, entitled(), Some(&k))), LITERAL);
        assert_eq!(address(c.resolve("37.75,-122.41", entitled(), Some(&k))), "37.75,-122.41");
    }

    /// **The ask.** Nothing saved under a well-known name is a QUESTION, and
    /// the message says what is missing and how to fix it. It is emphatically
    /// not a substitution and not a mention of any env var.
    #[test]
    fn an_unsaved_name_asks_and_never_substitutes() {
        let c = cfg_with(Arc::new(CountingStore::new()));
        let k = key("alpha");
        assert_eq!(c.resolve("home", entitled(), Some(&k)), Resolution::NotSaved("home".into()));
        let err = c.resolve("home", entitled(), Some(&k)).address().unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("don't have a \"home\" saved"), "{msg}");
        assert!(!msg.contains("COMMUTE"), "the user must never be pointed at an env var: {msg}");
    }

    /// **Absence and failure are different answers.** A registry that cannot be
    /// read must never be reported as an empty one — that would teach the user
    /// nothing is saved and invite exactly the confident guess this closes.
    #[test]
    fn an_unreadable_registry_is_distinct_from_nothing_saved() {
        let c = cfg_with(Arc::new(BrokenStore));
        let k = key("alpha");
        assert_eq!(c.resolve("home", entitled(), Some(&k)), Resolution::Unreadable("home".into()));

        let err = c.resolve("home", entitled(), Some(&k)).address().unwrap_err();
        // A different ERROR TYPE, not merely different prose: an absent value
        // and a broken read are not the same class of problem.
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("couldn't read"), "{msg}");
        assert!(msg.contains("not an empty list"), "{msg}");
    }

    // ── The unreadable-registry fall-through, narrowed ──────────────────────
    //
    // `geocode()` is reached from exactly three places (`CommuteEstimate`,
    // `RouteTraffic`, `TrafficIncidents`), each of them as
    // `self.cfg.resolve(..).address()?` — so a resolution that is not
    // `Place` cannot reach the geocoder at all. "Asserts no geocode call" and
    // "asserts `address()` is `Err`" are therefore the same assertion, and the
    // latter is checkable offline. Each test below states which it needs.

    /// **The finding.** A free-text name the user might well have saved must
    /// surface the READ FAILURE, not be geocoded as if it were a place.
    ///
    /// Geocoding "the cabin" while the registry is down is the worst available
    /// answer: it can SUCCEED, routing the user somewhere real and wrong, and
    /// the true answer — "I could not read your saved locations" — is never
    /// said. That is the absence-vs-failure collapse this branch exists to
    /// prevent, arrived at from the other side.
    #[test]
    fn a_free_text_name_surfaces_an_unreadable_registry_instead_of_geocoding() {
        let c = cfg_with(Arc::new(BrokenStore));
        let k = key("alpha");

        assert_eq!(
            c.resolve("the cabin", entitled(), Some(&k)),
            Resolution::Unreadable("the cabin".into())
        );

        // No address is produced, so no geocode call is possible.
        let err = c.resolve("the cabin", entitled(), Some(&k)).address().unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("couldn't read"), "{msg}");
        assert!(msg.contains("the cabin"), "the answer must name what was asked for: {msg}");

        // Other things a person plausibly saves, all of them names.
        for name in ["the cabin", "mom's", "the lake house", "storage unit", "Reno"] {
            assert!(
                matches!(c.resolve(name, entitled(), Some(&k)), Resolution::Unreadable(_)),
                "{name:?} could plausibly be a saved name and must surface the read failure"
            );
        }
    }

    /// **POSITIVE CONTROL — the property the broad fall-through was protecting.**
    /// An ordinary street address still routes while the registry is unreadable.
    /// An implementation that fixed the test above by failing EVERYTHING passes
    /// it and fails this one.
    #[test]
    fn a_literal_street_address_still_routes_when_the_registry_is_unreadable() {
        let c = cfg_with(Arc::new(BrokenStore));
        let k = key("alpha");
        assert_eq!(address(c.resolve(LITERAL, entitled(), Some(&k))), LITERAL);
        // The comma-free forms, which reach the predicate rather than being
        // short-circuited upstream (see
        // `a_name_shaped_input_is_the_only_kind_that_can_reach_the_read_failure`).
        // pii-test-fixture: obvious placeholder addresses in the shapes the predicate must clear
        const NO_COMMA_STREET: &str = "1600 Placeholder Ave Examplecity"; // pii-test-fixture: obvious placeholder street address without a comma
        assert_eq!(address(c.resolve(NO_COMMA_STREET, entitled(), Some(&k))), NO_COMMA_STREET);
        assert_eq!(address(c.resolve("00000", entitled(), Some(&k))), "00000");
        // A known airport code comes from a closed table, so it is a place, not
        // a nickname — and it still expands.
        assert_eq!(
            address(c.resolve("SJC", entitled(), Some(&k))),
            "San Jose International Airport, San Jose, CA"
        );
        // And the comma-bearing forms, whatever route they take to get there.
        for literal in [
            "1600 Placeholder Ave, Examplecity, EX 00000", // pii-test-fixture: obvious placeholder full address
            "Examplecity, EX",                             // pii-test-fixture: obvious placeholder city/state
        ] {
            assert_eq!(address(c.resolve(literal, entitled(), Some(&k))), literal);
        }
    }

    /// **Why the predicate only has to judge comma-free input.**
    ///
    /// [`locations::canonical_name`] is the SAME function that gates writes, so
    /// a string it rejects could never have been saved under — which makes it an
    /// authoritative "this is not a name" test applied upstream of the store.
    /// `lookup` returns `NotSet` for such a string WITHOUT reading, so it can
    /// never reach the `Unavailable` arm however sick the registry is.
    ///
    /// That is sound rather than lucky, and this test pins it: if the name
    /// charset is ever widened, the assertions below change and the predicate
    /// becomes load-bearing for these shapes too (which it already handles —
    /// see `unambiguously_literal_predicate_boundaries`).
    #[test]
    fn a_name_shaped_input_is_the_only_kind_that_can_reach_the_read_failure() {
        let store = Arc::new(CountingBrokenStore::new());
        let c = cfg_with(store.clone());
        let k = key("alpha");

        // A comma is not a legal character in a location NAME, so this string
        // cannot name a saved place and is not looked up at all.
        assert!(locations::canonical_name("Examplecity, EX").is_err());
        assert_eq!(address(c.resolve("Examplecity, EX", entitled(), Some(&k))), "Examplecity, EX");
        assert_eq!(store.reads(), 0, "an unstorable name must not cause a read");

        // A name-shaped string IS looked up, and the failure surfaces.
        assert!(locations::canonical_name("the cabin").is_ok());
        assert!(matches!(
            c.resolve("the cabin", entitled(), Some(&k)),
            Resolution::Unreadable(_)
        ));
        assert_eq!(store.reads(), 1);
    }

    /// A coordinate pair routes AND is never looked up — asserted against the
    /// store, not the answer, and specifically while the store is failing.
    #[test]
    fn a_coordinate_pair_routes_and_causes_no_read_when_the_registry_is_unreadable() {
        let store = Arc::new(CountingBrokenStore::new());
        let c = cfg_with(store.clone());
        let k = key("alpha");
        assert_eq!(address(c.resolve("37.75,-122.41", entitled(), Some(&k))), "37.75,-122.41");
        assert_eq!(store.reads(), 0, "a coordinate pair must never be looked up");

        // And the store really is failing — otherwise the assertion above would
        // pass for the wrong reason.
        assert!(matches!(
            c.resolve("the cabin", entitled(), Some(&k)),
            Resolution::Unreadable(_)
        ));
        assert!(store.reads() > 0);
    }

    /// The well-known aliases are UNCHANGED by the narrowing: they were already
    /// strict, and the predicate must never accidentally make one literal.
    #[test]
    fn well_known_aliases_are_never_unambiguously_literal() {
        let c = cfg_with(Arc::new(BrokenStore));
        let k = key("alpha");
        for name in [
            "home", "house", "work", "office", "the office", "current", "here",
            "where i am", "family", "family home", "parents",
        ] {
            assert!(
                !is_unambiguously_literal(name),
                "{name:?} is a well-known alias and must never be treated as literal"
            );
            assert!(
                matches!(c.resolve(name, entitled(), Some(&k)), Resolution::Unreadable(_)),
                "{name:?} must still surface the read failure"
            );
        }
    }

    /// **The distinction, from the other direction.** A registry that WAS read
    /// and holds nothing is not the failure above — same input, different
    /// outcome, and the user is never told the registry broke when it did not.
    ///
    /// The `NotSet` fall-through is deliberately left broad: the registry
    /// answered, so "no such name" is a fact and the string can only be a place.
    /// The uncertainty that justifies the narrow `Unavailable` arm is absent.
    #[test]
    fn nothing_saved_is_not_reported_as_an_unreadable_registry() {
        let empty = cfg_with(Arc::new(CountingStore::new()));
        let k = key("alpha");

        let r = empty.resolve("the cabin", entitled(), Some(&k));
        assert!(!matches!(r, Resolution::Unreadable(_)), "got {r:?}");
        // Whatever happens next, it must not claim a read failure.
        let rendered = match empty.resolve("the cabin", entitled(), Some(&k)) {
            Resolution::Place { address, .. } => address,
            other => other.address().unwrap_err().to_string(),
        };
        assert!(!rendered.contains("couldn't read"), "{rendered}");

        // The same name against a BROKEN store is the other answer entirely.
        let broken = cfg_with(Arc::new(BrokenStore));
        assert_ne!(
            std::mem::discriminant(&empty.resolve("the cabin", entitled(), Some(&k))),
            std::mem::discriminant(&broken.resolve("the cabin", entitled(), Some(&k))),
            "an empty registry and an unreadable one must not produce the same outcome"
        );

        // And a well-known name against a read registry is still the ask.
        assert_eq!(
            empty.resolve("home", entitled(), Some(&k)),
            Resolution::NotSaved("home".into())
        );
    }

    /// The predicate itself, at the boundary. Written as a table because the
    /// interesting content is WHICH shapes are on which side.
    #[test]
    fn unambiguously_literal_predicate_boundaries() {
        for literal in [
            "37.75,-122.41", // pii-test-fixture: obvious placeholder coordinates
            " 37.75 , -122.41 ", // pii-test-fixture: obvious placeholder coordinates, spaced
            "4 Literal Street, Examplecity", // pii-test-fixture: obvious placeholder address
            "12A Placeholder Way", // pii-test-fixture: obvious placeholder address, lettered number
            "12-14 Placeholder Way", // pii-test-fixture: obvious placeholder address, number range
            "00000",         // pii-test-fixture: obvious placeholder postal code
            "00000-1234",    // pii-test-fixture: obvious placeholder postal code, extended
            "Examplecity, EX", // pii-test-fixture: obvious placeholder city and region
            "Examplecity, EX 00000", // pii-test-fixture: obvious placeholder city, region and postal code
            "SJC",           // a known airport, from the closed table above
            "sfo",           // …case-insensitively
        ] {
            assert!(is_unambiguously_literal(literal), "{literal:?} should be literal");
        }
        for name in [
            "",
            "   ",
            "the cabin",
            "Reno",           // a real city AND an ordinary thing to save
            "ZZZ",            // three letters, not a known airport
            "mom's",
            "the lake house",
            "Examplecity",    // pii-test-fixture: a bare placeholder place word, no region token
            "37.75",          // half a coordinate pair
        ] {
            assert!(!is_unambiguously_literal(name), "{name:?} should be treated as a name");
        }
    }

    /// An unentitled caller discloses nothing AND causes no read — the stronger
    /// property, asserted against the store rather than against the answer.
    #[test]
    fn an_unentitled_caller_causes_no_read_and_learns_nothing() {
        let store = Arc::new(CountingStore::new());
        let k = key("alpha");
        match locations::set(store.as_ref(), Some(&k), entitled(), HOME, SAVED_HOME, None, true) {
            locations::WriteOutcome::Stored { .. } => {}
            other => panic!("seed failed: {other:?}"),
        }
        let before = store.reads();
        let c = cfg_with(store.clone());

        assert_eq!(c.resolve("home", guest(), Some(&k)), Resolution::NoAccess);
        // And with no identity at all, entitlement or not.
        assert_eq!(c.resolve("home", entitled(), None), Resolution::NoAccess);
        assert_eq!(store.reads(), before, "an unentitled caller must cause zero reads");

        let msg = c.resolve("home", guest(), Some(&k)).address().unwrap_err().to_string();
        assert!(!msg.contains(SAVED_HOME), "the answer must not disclose the address: {msg}");
        assert!(msg.contains("aren't available on this connection"), "{msg}");

        // A literal address still routes: it discloses nothing and refusing it
        // would be refusing to work on the caller's own input.
        assert_eq!(address(c.resolve(LITERAL, guest(), Some(&k))), LITERAL);
    }

    /// **The env-disclosure guard.** With `COMMUTE_HOME`/`COMMUTE_WORK`/
    /// `COMMUTE_FAMILY` genuinely SET in this process, no caller — entitled,
    /// unentitled, identity-less — obtains their values from any commute path.
    ///
    /// This is the test the sibling weather change earned and commute did not
    /// have: the previous code read all three at registration and handed them
    /// to every entitled caller.
    #[test]
    #[serial]
    fn no_caller_can_obtain_a_commute_env_value() {
        struct Restore(Vec<(&'static str, Option<String>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (k, v) in &self.0 {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let vars = ["COMMUTE_HOME", "COMMUTE_WORK", "COMMUTE_FAMILY"];
        let _restore = Restore(vars.iter().map(|k| (*k, std::env::var(k).ok())).collect());
        for k in vars {
            std::env::set_var(k, ENV_PLACEHOLDER);
        }

        // An EMPTY registry, so anything that appears could only have come from
        // the environment.
        let c = cfg_with(Arc::new(CountingStore::new()));
        let k = key("alpha");
        for (caller, key_opt) in [
            (entitled(), Some(&k)),
            (guest(), Some(&k)),
            (entitled(), None),
            (guest(), None),
        ] {
            for name in ["home", "work", "family", "house", "the office", "parents"] {
                let rendered = match c.resolve(name, caller, key_opt) {
                    Resolution::Place { address, .. } => address,
                    other => other.address().unwrap_err().to_string(),
                };
                assert!(
                    !rendered.contains(ENV_PLACEHOLDER),
                    "a COMMUTE_* value reached the caller for {name:?}: {rendered}"
                );
            }
        }
    }

    /// The registry is not consulted for a coordinate pair — it is a place the
    /// user just gave us, and a lookup would be a read with no possible answer.
    #[test]
    fn a_coordinate_pair_causes_no_registry_read() {
        let store = Arc::new(CountingStore::new());
        let c = cfg_with(store.clone());
        let k = key("alpha");
        let before = store.reads();
        assert_eq!(address(c.resolve("37.75,-122.41", entitled(), Some(&k))), "37.75,-122.41");
        assert_eq!(store.reads(), before);
    }

    #[test]
    fn resolve_iata_airport_codes() {
        let (c, k) = cfg();
        // Bare IATA codes expand to a full airport address for geocoding.
        assert_eq!(
            address(c.resolve("SJC", entitled(), Some(&k))),
            "San Jose International Airport, San Jose, CA"
        );
        assert_eq!(
            address(c.resolve("tpa", entitled(), Some(&k))),
            "Tampa International Airport, Tampa, FL"
        );
        // Unknown 3-letter strings and non-codes pass through unchanged.
        assert_eq!(address(c.resolve("ZZZ", entitled(), Some(&k))), "ZZZ");
        assert_eq!(address(c.resolve("Reno", entitled(), Some(&k))), "Reno");
    }

    #[test]
    fn expand_iata_only_matches_three_letter_codes() {
        assert!(expand_iata("SF").is_none());
        assert!(expand_iata("SJCX").is_none());
        assert!(expand_iata("S1C").is_none());
        assert_eq!(expand_iata("sfo"), Some("San Francisco International Airport, San Francisco, CA".to_string()));
    }

    #[test]
    fn coord_pair_detection() {
        assert!(is_coord_pair("37.75,-122.41"));
        assert!(is_coord_pair(" 37.75 , -122.41 "));
        assert!(!is_coord_pair("San Jose, CA"));
        assert!(!is_coord_pair("37.75"));
    }

    #[test]
    fn traffic_summary_tiers() {
        assert!(traffic_summary(0.5, 30.0).contains("clear"));
        assert!(traffic_summary(3.0, 30.0).contains("Light"));
        assert!(traffic_summary(10.0, 30.0).contains("Moderate"));
        assert!(traffic_summary(20.0, 30.0).contains("Heavy"));
        // percentage shown for moderate/heavy
        assert!(traffic_summary(15.0, 30.0).contains("50%"));
    }

    #[test]
    fn urlencode_spaces_and_specials() {
        assert_eq!(urlencode("San Jose, CA"), "San%20Jose%2C%20CA");
        assert_eq!(urlencode("4 Literal Street"), "4%20Literal%20Street"); // pii-test-fixture: obvious placeholder
    }

    #[test]
    fn format_route_shows_leave_by_when_arrive_set() {
        let r = RouteResult {
            travel_min: 35.0, no_traffic_min: 33.0, delay_min: 2.0,
            distance_miles: 22.0,
            departure: "2026-06-09T08:24:00-07:00".into(), // pii-test-fixture
            arrival: "2026-06-09T09:00:00-07:00".into(), // pii-test-fixture
        };
        let out = format_route("Home", "Work", &r, Some("2026-06-09T09:00:00-07:00")); // pii-test-fixture
        assert!(out.contains("Leave by"));
        assert!(out.contains("35 min"));
    }

    /// The label names a saved place only when one was actually used. Driving
    /// it off `saved_as` rather than a keyword list is what stops it announcing
    /// "Home (…)" over a value that came from anywhere else.
    #[test]
    fn label_names_a_saved_place_only_when_one_was_used() {
        let l = label("home", SAVED_HOME, Some(HOME));
        assert!(l.contains("Home"));
        assert!(l.contains("1 Placeholder Way")); // pii-test-fixture: obvious placeholder
        // A literal place passes through unchanged and is never dressed up as
        // a saved one, even when the user typed the word "home".
        assert_eq!(label("Reno NV", "Reno NV", None), "Reno NV");
        assert_eq!(label("home", LITERAL, None), "home");
    }

    #[tokio::test]
    #[serial]
    async fn transit_plan_needs_token() {
        std::env::remove_var("SF511_API_TOKEN");
        let r = TransitPlan.execute(json!({"origin":"a","destination":"b"})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn route_traffic_requires_destination() {
        // destination is still required; origin alone is not enough.
        let (c, _) = cfg();
        let t = RouteTraffic { cfg: c };
        assert!(matches!(
            t.execute(json!({"origin":"home"})).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    /// Replaces `route_traffic_omitted_origin_defaults_to_home`, which asserted
    /// that an omitted origin resolved through `COMMUTE_HOME`.
    ///
    /// What it asserts NOW: an omitted origin still MEANS "home" — it is not
    /// suddenly a required argument — and "home" is the caller's own SAVED
    /// entry. The proof is deterministic and needs no network: with a home
    /// saved and no `work`, the call gets past origin resolution and fails on
    /// the DESTINATION. An implementation that had made origin required would
    /// return `InvalidArgument`; one that still read the environment would be
    /// caught by `no_caller_can_obtain_a_commute_env_value` above.
    #[tokio::test]
    async fn route_traffic_omitted_origin_resolves_the_callers_saved_home() {
        let store = Arc::new(CountingStore::new());
        let k = key("alpha");
        match locations::set(store.as_ref(), Some(&k), entitled(), HOME, SAVED_HOME, None, true) {
            locations::WriteOutcome::Stored { .. } => {}
            other => panic!("seed failed: {other:?}"),
        }
        let t = RouteTraffic { cfg: cfg_with(store) };

        let err = t
            .execute_with_caller_key(json!({"destination": "work"}), entitled(), Some(k))
            .await
            .expect_err("no `work` is saved, so this cannot succeed");
        assert!(!matches!(err, ToolError::InvalidArgument(_)), "origin must still default: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("\"work\""), "the failure must be about the destination: {msg}");
        assert!(!msg.contains("\"home\""), "origin resolved from the registry: {msg}");
    }

    /// The other half: with NO home saved, an omitted origin is an honest ask —
    /// never a silent substitution, and never a claim that the argument was
    /// missing.
    #[tokio::test]
    async fn route_traffic_omitted_origin_asks_when_no_home_is_saved() {
        let t = RouteTraffic { cfg: cfg_with(Arc::new(CountingStore::new())) };
        let err = t
            .execute_with_caller_key(json!({"destination": LITERAL}), entitled(), Some(key("alpha")))
            .await
            .expect_err("with no saved home there is nothing to start from");
        assert!(matches!(err, ToolError::NotConfigured(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("don't have a \"home\" saved"), "{msg}");
    }

    /// The identity-less entry point cannot reach a saved place at all — a path
    /// that forgets to thread a caller gets a refusal, never someone's record.
    #[tokio::test]
    async fn the_identity_less_path_cannot_reach_a_saved_place() {
        let store = Arc::new(CountingStore::new());
        let k = key("alpha");
        match locations::set(store.as_ref(), Some(&k), entitled(), HOME, SAVED_HOME, None, true) {
            locations::WriteOutcome::Stored { .. } => {}
            other => panic!("seed failed: {other:?}"),
        }
        let reads = store.reads();
        let t = RouteTraffic { cfg: cfg_with(store.clone()) };

        let err = t.execute(json!({"destination": LITERAL})).await.expect_err("no identity");
        assert!(!err.to_string().contains(SAVED_HOME), "must not disclose: {err}");
        assert_eq!(store.reads(), reads, "an identity-less call must cause zero reads");
    }

    #[test]
    #[serial]
    fn register_adds_four_tools() {
        let mut reg = ToolRegistry::new();
        let key = std::env::var("TOMTOM_API_KEY").ok();
        std::env::set_var("TOMTOM_API_KEY", "testkey");
        register(&mut reg);
        if let Some(k) = key { std::env::set_var("TOMTOM_API_KEY", k); } else { std::env::remove_var("TOMTOM_API_KEY"); }
        assert!(reg.contains("commute_estimate"));
        assert!(reg.contains("route_traffic"));
        assert!(reg.contains("traffic_incidents"));
        assert!(reg.contains("transit_plan"));
    }
}
