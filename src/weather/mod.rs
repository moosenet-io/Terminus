//! Weather tool — current conditions and forecasts via OpenWeatherMap.
//!
//! One LLM-callable tool:
//!   weather  — current / tomorrow / this-week weather for a location.
//!
//! Location resolution: when `location` is omitted or empty the tool resolves it
//! through the chain in [`location`] — **calendar → home/work routine → ASK** —
//! and NEVER invents a place.
//!
//! This module previously resolved a missing location from `COMMUTE_HOME` alone.
//! With that env var unset the tool failed instantly, the model retried, and it
//! answered for **Tampa** — the first example in this tool's own JSON schema. An
//! example in a schema becomes a default in practice, and a silently-substituted
//! location is indistinguishable from a wrong one. Hence three changes here:
//!   1. the chain above is WIRED into `resolve_location` (it was previously
//!      implemented in `location.rs` and never called — dead code);
//!   2. an unresolvable location returns [`location::ASK_MESSAGE`] as a normal
//!      successful answer, not an error — an error is what made the model retry
//!      and invent in the first place;
//!   3. the schema and description name NO city, so there is nothing to copy.
//!
//! Calendar access goes through [`crate::google::caldav::GoogleCalendarSource`],
//! the module that already owns Google credentials — this tool holds only a
//! `dyn CalendarSource` and never reads a secret. When Google is unconfigured the
//! source is `NoCalendar` and the chain degrades to routine→ask.
//!
//! Forecast extraction (BUG 2): the OpenWeatherMap free tier exposes current
//! conditions at /data/2.5/weather and a 5-day / 3-hour forecast at
//! /data/2.5/forecast. The forecast endpoint returns a `list` of 3-hour data
//! points each stamped with `dt` (unix UTC) and `dt_txt` ("YYYY-MM-DD HH:MM:SS").
//!   - `tomorrow` filters the list to the points whose date == today+1 (UTC),
//!     then reduces them to a min/max temp and the most common condition.
//!   - `week` groups every point by its date and summarises each day the same
//!     way, giving the full ~5-day outlook.
//! All parsing is done in Rust with serde — no LLM.
//!
//! Dual units (operator travels internationally): the tool ALWAYS fetches in
//! metric (canonical Celsius) and renders BOTH °F and °C for every temperature,
//! regardless of country. Conversion is pure Rust (f = c*9/5 + 32). It also
//! reports humidity, precipitation (forecast probability `pop` and/or rain/snow
//! volume in mm), and a rule-based "What to wear" suggestion derived from the
//! temperature and conditions — no LLM is involved.
//!
//! Required env:
//!   OPENWEATHER_API_KEY  — OpenWeatherMap API key (free tier works)
//! Optional env:
//!   OPENWEATHER_API_URL  — base URL (default https://api.openweathermap.org)
//!   COMMUTE_HOME         — home address for routine inference (shared with the
//!                          commute tools; a plain address, not a secret)
//!   COMMUTE_WORK         — work address for routine inference
//!
//! NOTE: temperatures are always fetched and displayed in metric+imperial, so
//! OPENWEATHER_UNITS is no longer consulted (canonical fetch is always metric).

pub mod location;

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool};
use location::{CalendarSource, NoCalendar, Resolved, Routine};

const DEFAULT_BASE_URL: &str = "https://api.openweathermap.org";
/// Canonical fetch unit. Temperatures are always retrieved in metric (Celsius)
/// and converted in Rust so output can show both °C and °F regardless of
/// locale. Wind in metric is m/s.
const CANONICAL_UNITS: &str = "metric";

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct WeatherConfig {
    api_key: String,
    base_url: String,
    units: String,
    /// Home/work addresses (COMMUTE_HOME / COMMUTE_WORK, shared with the commute
    /// tools) — the THIRD step of the resolution chain, below the calendar.
    routine: Routine,
    /// Today's calendar, behind a trait object so this tool never reaches Google
    /// directly. `NoCalendar` when Google is unconfigured (degrade to routine→ask).
    calendar: Arc<dyn CalendarSource>,
}

impl WeatherConfig {
    fn from_env() -> Result<Self, ToolError> {
        let api_key = std::env::var("OPENWEATHER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("OPENWEATHER_API_KEY not set".into()))?;
        let base_url = std::env::var("OPENWEATHER_API_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        // The calendar comes from the module that owns Google access. When Google
        // is unconfigured we take `NoCalendar` — an explicit, typed statement that
        // the chain is routine→ask, rather than a silent hole.
        let calendar: Arc<dyn CalendarSource> = match crate::google::GoogleConfig::from_env() {
            Ok(g) => Arc::new(crate::google::caldav::GoogleCalendarSource::new(g)),
            Err(e) => {
                tracing::info!(
                    "weather: Google calendar unavailable for location resolution ({e}); \
                     falling back to home/work routine, then asking"
                );
                Arc::new(NoCalendar)
            }
        };
        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            units: CANONICAL_UNITS.to_string(),
            routine: Routine::from_env(),
            calendar,
        })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Resolve the caller-supplied location through the full chain:
    /// explicit → calendar → home/work routine → ASK.
    ///
    /// Returns a [`Resolved`], NOT a bare string, so the caller can (a) attribute
    /// the location in the answer and (b) ASK when nothing resolves. This is the
    /// ONE resolution path — `location::resolve*` is not called anywhere else, and
    /// there is no longer a COMMUTE_HOME-only branch to fall back into.
    ///
    /// TRTR-05: `caller` decides whether the calendar/routine steps run at all.
    /// The calendar and the home/work routine are the OPERATOR's, and this tool
    /// is reachable by household guests — so for anyone not entitled to those
    /// sources the chain is explicit→ASK, and the operator's calendar is not
    /// even read. See `location`'s privacy note.
    async fn resolve_location(&self, input: Option<&str>, caller: CallerContext) -> Resolved {
        let (hour, weekday) = location::local_hour_and_weekday();
        location::resolve_with_calendar(
            input,
            self.calendar.as_ref(),
            &self.routine,
            hour,
            weekday,
            caller,
        )
        .await
    }
}

// ── Temperature / wind helpers (pure Rust, no LLM) ───────────────────────────

/// Convert Celsius → Fahrenheit.
fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

/// Render a canonical Celsius value as "72°F / 22°C" (both rounded to whole
/// degrees). The operator travels internationally and wants both, always.
fn dual_temp(c: f64) -> String {
    format!("{:.0}°F / {:.0}°C", c_to_f(c), c)
}

/// Render a canonical Celsius low/high range as "54–68°F / 12–20°C".
fn dual_range(min_c: f64, max_c: f64) -> String {
    format!(
        "{:.0}–{:.0}°F / {:.0}–{:.0}°C",
        c_to_f(min_c),
        c_to_f(max_c),
        min_c,
        max_c
    )
}

/// Render wind (canonical m/s from the metric API) as "11 km/h / 7 mph".
fn dual_wind(ms: f64) -> String {
    format!("{:.0} km/h / {:.0} mph", ms * 3.6, ms * 2.237)
}

/// Rule-based clothing suggestion from temperature (°C) and conditions.
/// Pure logic — never an LLM call. Layers a base "what to wear" recommendation
/// keyed on temperature with modifiers for rain, snow, and strong wind.
///
/// `feels_c` is preferred when available (what the body actually experiences);
/// `desc` is the lower-cased weather description; `wind_ms` is wind speed in m/s.
fn what_to_wear(feels_c: f64, desc: &str, wind_ms: Option<f64>) -> String {
    let base = if feels_c <= -10.0 {
        "Bitter cold: heavy insulated coat, hat, gloves, scarf, and thermal layers"
    } else if feels_c <= 0.0 {
        "Freezing: heavy coat, hat, gloves, and warm layers"
    } else if feels_c <= 8.0 {
        "Cold: warm coat and a sweater"
    } else if feels_c <= 15.0 {
        "Cool: a jacket or hoodie"
    } else if feels_c <= 22.0 {
        "Mild: a light jacket or long sleeves"
    } else if feels_c <= 28.0 {
        "Warm: t-shirt and shorts"
    } else {
        "Hot: light, breathable clothing; stay hydrated and use sun protection"
    };

    let d = desc.to_lowercase();
    let mut extras: Vec<&str> = Vec::new();
    if d.contains("snow") || d.contains("sleet") {
        extras.push("waterproof boots for snow");
    } else if d.contains("rain") || d.contains("drizzle") || d.contains("thunderstorm") {
        extras.push("bring an umbrella or a waterproof layer");
    }
    if wind_ms.map(|w| w >= 8.0).unwrap_or(false) {
        extras.push("windproof outer layer (it's gusty)");
    }

    if extras.is_empty() {
        format!("What to wear: {base}.")
    } else {
        format!("What to wear: {base}; {}.", extras.join("; "))
    }
}

// ── Geocoding ───────────────────────────────────────────────────────────────

/// Resolve a location string to (lat, lon). Accepts a literal "lat,lon" pair
/// as-is; otherwise queries the OWM geocoding API.
///
/// OWM's `/geo/1.0/direct` resolves CITY-level names, not full street
/// addresses, and answers HTTP 200 with an empty array for an address it can't
/// place. The default location is COMMUTE_HOME — a full street address shared
/// with the commute tools — so we try the string as given, then retry with
/// progressively coarser variants (dropping leading street components). e.g.
/// "123 Main St, San Jose, CA 95123" falls back to "San Jose, CA 95123" →
/// "CA 95123"; the first variant that resolves wins.
async fn geocode(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
    location: &str,
) -> Result<(f64, f64), ToolError> {
    if let Some(pair) = parse_coord_pair(location) {
        return Ok(pair);
    }

    for query in geocode_candidates(location) {
        if let Some(pair) = geocode_once(client, cfg, &query).await? {
            return Ok(pair);
        }
    }

    // No example city here either: this string is returned to the model, and a
    // model with no location will happily adopt whatever place it is shown.
    Err(ToolError::NotFound(format!(
        "Could not geocode '{location}'. Ask the user for a city (and state/country \
         if ambiguous) — do not substitute one."
    )))
}

/// Candidate geocoding queries for a location, finest-first.
///
/// Two coarsening strategies, applied in order, deduped:
///   1. Comma-coarsening (addresses): the full string, then the string with
///      leading (street-level) comma components removed one at a time. e.g.
///      "123 Main St, San Jose, CA 95123" → "San Jose, CA 95123" → "CA 95123".
///   2. Space-coarsening (no-comma multi-word names): OWM's geocoder answers
///      200 + [] for a bare space-separated "City State" like "Tampa Florida".
///      So when the working string has NO comma but multiple whitespace words,
///      also try (a) a comma inserted before the LAST word ("Tampa Florida" →
///      "Tampa, Florida", "San Jose California" → "San Jose, California") and
///      (b) the string with the trailing word dropped ("Tampa Florida" →
///      "Tampa", "San Jose California" → "San Jose"). Multi-word cities are
///      preserved (we never collapse to just the first token).
///
/// Trimmed, de-duplicated, empties dropped; the first that geocodes wins.
fn geocode_candidates(location: &str) -> Vec<String> {
    let parts: Vec<&str> = location
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    fn push(out: &mut Vec<String>, s: String) {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }

    let mut out: Vec<String> = Vec::new();
    push(&mut out, parts.join(", "));
    for i in 1..parts.len() {
        push(&mut out, parts[i..].join(", "));
    }

    // Space-coarsening: only for a no-comma string with multiple words.
    if parts.len() <= 1 {
        let words: Vec<&str> = location.split_whitespace().collect();
        if words.len() >= 2 {
            // (a) comma before the last word: "City Name State" → "City Name, State"
            let (head, last) = words.split_at(words.len() - 1);
            push(&mut out, format!("{}, {}", head.join(" "), last[0]));
            // (b) drop the trailing word: "City Name State" → "City Name"
            push(&mut out, head.join(" "));
        }
    }

    if out.is_empty() {
        push(&mut out, location.trim().to_string());
    }
    out
}

/// Run one OWM geocoding query. `Ok(Some((lat, lon)))` on a hit; `Ok(None)`
/// when OWM answers 200 with no matches (so the caller tries a coarser
/// variant); `Err` only on a real HTTP/transport failure.
async fn geocode_once(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
    query: &str,
) -> Result<Option<(f64, f64)>, ToolError> {
    let url = format!("{}/geo/1.0/direct", cfg.base_url);
    let resp = client
        .get(&url)
        .query(&[("q", query), ("limit", "1"), ("appid", cfg.api_key.as_str())])
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Geocode HTTP {} for '{query}'",
            resp.status()
        )));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let Some(first) = body.as_array().and_then(|a| a.first()) else {
        return Ok(None); // 200 + empty array → no match; try a coarser variant
    };
    match (
        first.get("lat").and_then(Value::as_f64),
        first.get("lon").and_then(Value::as_f64),
    ) {
        (Some(lat), Some(lon)) => Ok(Some((lat, lon))),
        _ => Ok(None),
    }
}

/// Parse "lat,lon" → (f64, f64). Returns None if not a coordinate pair.
fn parse_coord_pair(s: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        return None;
    }
    let lat = parts[0].trim().parse::<f64>().ok()?;
    let lon = parts[1].trim().parse::<f64>().ok()?;
    Some((lat, lon))
}

// ── API calls ───────────────────────────────────────────────────────────────

async fn fetch_current(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
    lat: f64,
    lon: f64,
) -> Result<Value, ToolError> {
    let url = format!("{}/data/2.5/weather", cfg.base_url);
    let resp = client
        .get(&url)
        .query(&[
            ("lat", lat.to_string()),
            ("lon", lon.to_string()),
            ("units", cfg.units.clone()),
            ("appid", cfg.api_key.clone()),
        ])
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Weather HTTP {} (current)",
            resp.status()
        )));
    }
    resp.json().await.map_err(|e| ToolError::Http(e.to_string()))
}

async fn fetch_forecast(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
    lat: f64,
    lon: f64,
) -> Result<Value, ToolError> {
    let url = format!("{}/data/2.5/forecast", cfg.base_url);
    let resp = client
        .get(&url)
        .query(&[
            ("lat", lat.to_string()),
            ("lon", lon.to_string()),
            ("units", cfg.units.clone()),
            ("appid", cfg.api_key.clone()),
        ])
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Weather HTTP {} (forecast)",
            resp.status()
        )));
    }
    resp.json().await.map_err(|e| ToolError::Http(e.to_string()))
}

// ── Parsing / summarising ───────────────────────────────────────────────────

/// A reduced per-day summary of forecast data points.
struct DaySummary {
    date: String,
    temp_min: f64,
    temp_max: f64,
    condition: String,
    /// Max probability of precipitation across the day's points (0..1), if any
    /// point carried a `pop` field.
    pop: Option<f64>,
    /// Total rain volume (mm) summed across the day's points, if any.
    rain_mm: Option<f64>,
    /// Total snow volume (mm) summed across the day's points, if any.
    snow_mm: Option<f64>,
}

/// Reduce a slice of OWM forecast `list` entries (all for one day) into a
/// min/max temperature and the most frequent textual condition.
fn summarise_points(date: &str, points: &[&Value]) -> Option<DaySummary> {
    if points.is_empty() {
        return None;
    }
    let mut temp_min = f64::INFINITY;
    let mut temp_max = f64::NEG_INFINITY;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pop_max: Option<f64> = None;
    let mut rain_mm: Option<f64> = None;
    let mut snow_mm: Option<f64> = None;

    for p in points {
        if let Some(main) = p.get("main") {
            if let Some(t) = main.get("temp_min").and_then(Value::as_f64) {
                temp_min = temp_min.min(t);
            }
            if let Some(t) = main.get("temp_max").and_then(Value::as_f64) {
                temp_max = temp_max.max(t);
            }
            // Fall back to the instantaneous temp if min/max are absent.
            if let Some(t) = main.get("temp").and_then(Value::as_f64) {
                temp_min = temp_min.min(t);
                temp_max = temp_max.max(t);
            }
        }
        if let Some(desc) = p
            .get("weather")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|w| w.get("description"))
            .and_then(Value::as_str)
        {
            *counts.entry(desc.to_string()).or_insert(0) += 1;
        }
        // Precipitation: track the day's peak probability and total volume.
        if let Some(pop) = p.get("pop").and_then(Value::as_f64) {
            pop_max = Some(pop_max.map_or(pop, |m: f64| m.max(pop)));
        }
        if let Some(v) = volume_mm(p.get("rain")) {
            rain_mm = Some(rain_mm.unwrap_or(0.0) + v);
        }
        if let Some(v) = volume_mm(p.get("snow")) {
            snow_mm = Some(snow_mm.unwrap_or(0.0) + v);
        }
    }

    if !temp_min.is_finite() || !temp_max.is_finite() {
        return None;
    }

    let condition = counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(d, _)| d)
        .unwrap_or_else(|| "unknown".to_string());

    Some(DaySummary {
        date: date.to_string(),
        temp_min,
        temp_max,
        condition,
        pop: pop_max,
        rain_mm,
        snow_mm,
    })
}

/// Extract a precipitation volume (mm) from an OWM `rain`/`snow` object, which
/// keys volume by accumulation window ("1h" current, "3h" forecast). Returns
/// the first present window's value.
fn volume_mm(obj: Option<&Value>) -> Option<f64> {
    let o = obj?;
    o.get("3h")
        .and_then(Value::as_f64)
        .or_else(|| o.get("1h").and_then(Value::as_f64))
}

/// Build a clearly-labelled precipitation phrase from a probability (0..1) and
/// optional rain/snow volumes (mm). Returns None when there is nothing to say.
fn precip_phrase(pop: Option<f64>, rain_mm: Option<f64>, snow_mm: Option<f64>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = pop {
        parts.push(format!("{:.0}% chance", (p * 100.0).round()));
    }
    if let Some(r) = rain_mm.filter(|v| *v > 0.0) {
        parts.push(format!("{r:.1} mm rain"));
    }
    if let Some(s) = snow_mm.filter(|v| *v > 0.0) {
        parts.push(format!("{s:.1} mm snow"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("precipitation {}", parts.join(", ")))
    }
}

/// The `YYYY-MM-DD` date portion of an OWM `dt_txt` field.
fn date_of(point: &Value) -> Option<String> {
    point
        .get("dt_txt")
        .and_then(Value::as_str)
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_string)
}

/// Group a forecast `list` by calendar date (preserving chronological order).
fn group_by_date(list: &[Value]) -> Vec<(String, Vec<&Value>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for p in list {
        if let Some(d) = date_of(p) {
            if !groups.contains_key(&d) {
                order.push(d.clone());
            }
            groups.entry(d).or_default().push(p);
        }
    }
    order
        .into_iter()
        .map(|d| {
            let pts = groups.remove(&d).unwrap_or_default();
            (d, pts)
        })
        .collect()
}

/// Format the current-conditions response. Temperatures are canonical Celsius
/// (the API is always queried in metric) and rendered dual (°F / °C). Adds
/// humidity, precipitation (volume — the current endpoint has no `pop`), and a
/// rule-based "What to wear" line.
fn format_current(_cfg: &WeatherConfig, label: &str, body: &Value) -> String {
    let temp = body
        .get("main")
        .and_then(|m| m.get("temp"))
        .and_then(Value::as_f64);
    let feels = body
        .get("main")
        .and_then(|m| m.get("feels_like"))
        .and_then(Value::as_f64);
    let desc = body
        .get("weather")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|w| w.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("unknown conditions");
    let humidity = body
        .get("main")
        .and_then(|m| m.get("humidity"))
        .and_then(Value::as_f64);
    let wind = body
        .get("wind")
        .and_then(|w| w.get("speed"))
        .and_then(Value::as_f64);
    let rain_mm = volume_mm(body.get("rain"));
    let snow_mm = volume_mm(body.get("snow"));

    let mut out = format!("Current weather for {label}: {desc}");
    if let Some(t) = temp {
        out.push_str(&format!(", {}", dual_temp(t)));
    }
    if let Some(f) = feels {
        out.push_str(&format!(" (feels like {})", dual_temp(f)));
    }
    if let Some(h) = humidity {
        out.push_str(&format!(", humidity {h:.0}%"));
    }
    if let Some(p) = precip_phrase(None, rain_mm, snow_mm) {
        out.push_str(&format!(", {p}"));
    }
    if let Some(w) = wind {
        out.push_str(&format!(", wind {}", dual_wind(w)));
    }
    out.push('.');

    // Rule-based clothing suggestion (prefer feels-like, fall back to temp).
    if let Some(c) = feels.or(temp) {
        out.push(' ');
        out.push_str(&what_to_wear(c, desc, wind));
    }
    out
}

/// Format one forecast day. Temperatures are dual (°F / °C); precipitation
/// (probability and/or volume) and a "What to wear" line are appended when
/// data is present.
fn format_day(_cfg: &WeatherConfig, d: &DaySummary) -> String {
    let mut out = format!(
        "{}: {}, {}",
        d.date,
        d.condition,
        dual_range(d.temp_min, d.temp_max)
    );
    if let Some(p) = precip_phrase(d.pop, d.rain_mm, d.snow_mm) {
        out.push_str(&format!(", {p}"));
    }
    // Suggest clothing from the day's high (what you'd dress for out and about).
    out.push_str(&format!(
        " — {}",
        what_to_wear(d.temp_max, &d.condition, None)
    ));
    out
}

// ── Tool ────────────────────────────────────────────────────────────────────

struct Weather {
    cfg: WeatherConfig,
}

#[async_trait]
impl RustTool for Weather {
    fn name(&self) -> &str {
        "weather"
    }

    fn description(&self) -> &str {
        // NO CITY NAMES ANYWHERE IN THIS TEXT — deliberate, load-bearing, and
        // tested (`description_and_schema_name_no_city`). The previous wording
        // listed example cities here; with the location unresolvable the model
        // copied the first one and answered for a city the user has no connection
        // to. Describe the SHAPE of the argument, never an instance of it.
        "Get the weather for ANY place — ALWAYS use this tool for weather questions \
instead of a web search. It works for any city, town, address, landmark, or \
'lat,lon' anywhere in the world, not just where the user lives. It returns BOTH \
current conditions AND multi-day forecasts (up to ~5–6 days ahead) directly from \
live weather data. \
Pass 'location' as the user named it (a city, a town, a full street address, a \
landmark, or a 'lat,lon' pair). It is OPTIONAL: when omitted, the tool resolves the \
place itself from the user's calendar, then their home/work routine, and if neither \
is known it ASKS them. NEVER invent, assume, or fill in a location the user did not \
give — leave it out and let the tool ask. \
Pass 'days' (1–7) for a forecast: days=1 (or omit) gives current conditions; \
days=3 gives a 3-day forecast with each day's high/low and conditions; days=5 gives \
a 5-day forecast, etc. (clamped to what the data provides). \
The legacy 'when' field still works ('current', 'tomorrow', 'week') but prefer \
'days'. Returns a short human-readable summary; when the location was inferred the \
answer says which place it used and why."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "Any city, town, address, landmark, or 'lat,lon'. Optional — when omitted the location is resolved from your calendar, then your home/work routine, and if none of those are known you will be ASKED rather than a place being guessed."
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 7,
                    "description": "Forecast length. 1 (or omitted) = current conditions; 2–7 = that many days of forecast (high/low + conditions per day), clamped to ~5–6 days available. Takes precedence over 'when'."
                },
                "when": {
                    "type": "string",
                    "enum": ["current", "tomorrow", "week"],
                    "description": "Legacy timeframe selector: current (default), tomorrow, or week (~5-6 day outlook). Ignored if 'days' is given."
                }
            }
        })
    }

    /// TRTR-05: `execute` carries NO caller identity, so it is the fail-closed
    /// entry point — it resolves as [`CallerContext::untrusted`], i.e. an
    /// omitted location is ASKED about rather than inferred from the operator's
    /// calendar or routine. An authorized dispatch path supplies the real caller
    /// via `execute_with_caller` below; anything else (a self-test, an internal
    /// helper, a future path that forgets to thread one) gets the safe answer
    /// rather than the operator's whereabouts.
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted()).await
    }

    /// The authorized path: the gateway derived what operator context this
    /// caller is entitled to (`GatewayFramework::caller_context`) from the same
    /// server-verified principal it authorized the call with.
    async fn execute_with_caller(
        &self,
        args: Value,
        caller: CallerContext,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text_only(self.run(args, caller).await?))
    }
}

impl Weather {
    async fn run(&self, args: Value, caller: CallerContext) -> Result<String, ToolError> {
        let resolved = self.cfg.resolve_location(args["location"].as_str(), caller).await;
        let (location, attribution) = match &resolved {
            Resolved::Found { location, .. } => (location.clone(), resolved.attribution()),
            // ASK. Deliberately `Ok`, not `Err`: an error is what made the model
            // retry and answer for a city out of the schema. A plain-language
            // question is a valid, final answer to "what's the weather?" when we
            // genuinely do not know where the user means.
            Resolved::AskUser => return Ok(location::ASK_MESSAGE.to_string()),
        };
        let mode = Mode::resolve(&args)?;

        let client = WeatherConfig::client()?;
        let (lat, lon) = geocode(&client, &self.cfg, &location).await?;

        let report = match mode {
            Mode::Current => {
                let body = fetch_current(&client, &self.cfg, lat, lon).await?;
                format_current(&self.cfg, &location, &body)
            }
            Mode::Tomorrow => {
                let body = fetch_forecast(&client, &self.cfg, lat, lon).await?;
                let list = forecast_list(&body)?;
                let grouped = group_by_date(list);
                // Tomorrow is the second distinct date in the forecast (the
                // first is today). If only one day is present, there is no
                // tomorrow to report.
                let day = grouped
                    .get(1)
                    .and_then(|(date, pts)| summarise_points(date, pts))
                    .ok_or_else(|| {
                        ToolError::NotFound("No forecast available for tomorrow".into())
                    })?;
                format!(
                    "Tomorrow's weather for {location} — {}",
                    format_day(&self.cfg, &day)
                )
            }
            // Multi-day forecast: up to `n` distinct days, clamped to what the
            // API returns (~6). `Mode::Week` is `n == FORECAST_MAX_DAYS`.
            Mode::Days(n) => {
                let body = fetch_forecast(&client, &self.cfg, lat, lon).await?;
                let list = forecast_list(&body)?;
                let grouped = group_by_date(list);
                let days: Vec<DaySummary> = grouped
                    .iter()
                    .filter_map(|(date, pts)| summarise_points(date, pts))
                    .take(n)
                    .collect();
                if days.is_empty() {
                    return Err(ToolError::NotFound("No forecast data available".into()));
                }
                let mut out = format!("{}-day forecast for {location}:\n", days.len());
                for d in &days {
                    out.push_str(&format!("- {}\n", format_day(&self.cfg, d)));
                }
                out
            }
        };

        // ATTRIBUTION. When the location was INFERRED (calendar or routine) the
        // answer says so, up front. Without this, a wrong inference is silent and
        // the user has no way to tell a right answer from a confidently wrong one
        // — which is exactly how "the weather in Tampa" went unnoticed. An
        // explicit location gets no prefix: they already know what they asked for.
        Ok(match attribution {
            Some(a) => format!("{a}.\n{report}"),
            None => report,
        })
    }
}

/// Largest forecast horizon we will ask for; the free /data/2.5/forecast tier
/// covers roughly 6 distinct calendar days (today + 5).
const FORECAST_MAX_DAYS: usize = 7;

/// What the caller asked for, after reconciling `days` and `when`.
enum Mode {
    Current,
    Tomorrow,
    /// Multi-day forecast of up to N distinct days.
    Days(usize),
}

impl Mode {
    /// Reconcile the `days` integer and the legacy `when` enum.
    ///
    /// Precedence: if `days` is explicitly provided it WINS — `days <= 1` →
    /// current, `days >= 2` → an N-day forecast (clamped to 1..=FORECAST_MAX_DAYS).
    /// Otherwise fall back to `when`: tomorrow → the 2nd day only, week → up to
    /// FORECAST_MAX_DAYS, current/absent → current.
    fn resolve(args: &Value) -> Result<Mode, ToolError> {
        if let Some(days) = args.get("days").filter(|v| !v.is_null()) {
            let n = days
                .as_i64()
                .ok_or_else(|| ToolError::InvalidArgument("'days' must be an integer".into()))?;
            return Ok(if n <= 1 {
                Mode::Current
            } else {
                Mode::Days((n as usize).min(FORECAST_MAX_DAYS))
            });
        }

        match args["when"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("current")
        {
            "current" => Ok(Mode::Current),
            "tomorrow" => Ok(Mode::Tomorrow),
            "week" => Ok(Mode::Days(FORECAST_MAX_DAYS)),
            other => Err(ToolError::InvalidArgument(format!(
                "'when' must be current, tomorrow, or week (got '{other}')"
            ))),
        }
    }
}

/// Extract the `list` array from a /data/2.5/forecast response body.
fn forecast_list(body: &Value) -> Result<&Vec<Value>, ToolError> {
    body.get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::NotFound("No forecast data returned".into()))
}

// ── Registration ────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match WeatherConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(Weather { cfg }));
        }
        Err(e) => {
            tracing::warn!("Weather tool not configured: {e}. Registering stub.");
            registry.register_or_replace(Box::new(NotConfiguredStub));
        }
    }
}

struct NotConfiguredStub;

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str {
        "weather"
    }
    fn description(&self) -> &str {
        "Weather tool (OPENWEATHER_API_KEY not configured)"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured("OPENWEATHER_API_KEY not set".into()))
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use httpmock::prelude::*;
    use location::{LocationSource, ASK_MESSAGE};

    /// A calendar the test controls. This is the mock half of the seam — see
    /// `wiring_is_not_stubbed_*` below for the positive controls that make a
    /// hardwired `resolve_location` fail.
    struct FakeCalendar(Vec<Value>);

    #[async_trait]
    impl CalendarSource for FakeCalendar {
        async fn events_now(&self) -> Vec<Value> {
            self.0.clone()
        }
    }

    fn routine_of(home: Option<&str>, work: Option<&str>) -> Routine {
        Routine { home: home.map(str::to_string), work: work.map(str::to_string) }
    }

    /// The caller context the gateway derives for the OPERATOR's own identity:
    /// entitled to both sources of operator context, because it is already
    /// allowed to call `google_calendar_today` and `commute_estimate` directly.
    fn operator() -> CallerContext {
        CallerContext::new(true, true)
    }

    /// A household GUEST: allowed to call `weather` (it is in
    /// `GUEST_BASELINE_ALLOW`) and entitled to neither source of operator
    /// context — identical to the untrusted default.
    fn guest() -> CallerContext {
        CallerContext::untrusted()
    }

    impl Weather {
        /// Test-only: drive the tool as the OPERATOR, which is what every
        /// pre-TRTR-05 test meant by `execute`. Kept explicit so the guest tests
        /// below read as the deliberate contrast they are, rather than as the
        /// odd one out.
        async fn execute_as_operator(&self, args: Value) -> Result<String, ToolError> {
            self.execute_with_caller(args, operator()).await.map(|o| o.text)
        }
    }

    /// A config with no calendar and an optional home — the pre-existing helper's
    /// semantics, preserved so the older tests keep testing what they tested.
    fn cfg_for(server: &MockServer, home: Option<&str>) -> WeatherConfig {
        cfg_full(server, routine_of(home, None), Arc::new(NoCalendar))
    }

    fn cfg_full(
        server: &MockServer,
        routine: Routine,
        calendar: Arc<dyn CalendarSource>,
    ) -> WeatherConfig {
        WeatherConfig {
            api_key: "testkey".into(),
            base_url: server.base_url(),
            units: "metric".into(),
            routine,
            calendar,
        }
    }

    /// A config for the pure-resolution tests (no HTTP involved).
    fn offline_cfg(routine: Routine, calendar: Arc<dyn CalendarSource>) -> WeatherConfig {
        WeatherConfig {
            api_key: "k".into(),
            base_url: "http://x".into(),
            units: "metric".into(),
            routine,
            calendar,
        }
    }

    fn events(v: Value) -> Arc<dyn CalendarSource> {
        Arc::new(FakeCalendar(v.as_array().cloned().unwrap_or_default()))
    }

    /// A raw iCal VEVENT the user DECLINED whose organiser copy is still
    /// `STATUS:CONFIRMED` — the normal shape of a declined invite. Built as raw iCal
    /// (not a `"status": "declined"` JSON literal) so the tests below span the real
    /// parse path. The attendee address is an RFC 2606 placeholder.
    fn confirmed_but_declined_ical() -> String {
        let attendee = "ATTENDEE;CN=Me;PARTSTAT=DECLINED:mailto:<email>"; // pii-test-fixture
        format!(
            "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Offsite in Denver\r\n\
             DTSTART:20260601T090000Z\r\nLOCATION:Denver, CO\r\n\
             STATUS:CONFIRMED\r\n{attendee}\r\nEND:VEVENT\r\n"
        )
    }

    fn geo_body() -> Value {
        json!([{ "name": "San Francisco", "lat": 37.7749, "lon": -122.4194, "country": "US" }])
    }

    fn current_body() -> Value {
        json!({
            "weather": [{ "description": "clear sky" }],
            "main": { "temp": 18.0, "feels_like": 17.0, "humidity": 60, "temp_min": 15.0, "temp_max": 20.0 },
            "wind": { "speed": 3.0 }
        })
    }

    /// A wet current-conditions body (rain volume in the "1h" window).
    fn rainy_current_body() -> Value {
        json!({
            "weather": [{ "description": "light rain" }],
            "main": { "temp": 9.0, "feels_like": 7.0, "humidity": 88 },
            "wind": { "speed": 9.0 },
            "rain": { "1h": 1.2 }
        })
    }

    /// Forecast spanning today (2 points) + tomorrow (2) + day-after (1).
    fn forecast_body() -> Value {
        json!({
            "list": [
                { "dt_txt": "2026-06-09 12:00:00", "main": { "temp": 19.0, "temp_min": 17.0, "temp_max": 21.0 }, "weather": [{ "description": "clear sky" }] },  // pii-test-fixture
                { "dt_txt": "2026-06-09 15:00:00", "main": { "temp": 20.0, "temp_min": 18.0, "temp_max": 22.0 }, "weather": [{ "description": "clear sky" }] },  // pii-test-fixture
                { "dt_txt": "2026-06-10 09:00:00", "main": { "temp": 14.0, "temp_min": 12.0, "temp_max": 16.0 }, "weather": [{ "description": "light rain" }], "pop": 0.4, "rain": { "3h": 1.0 } },  // pii-test-fixture
                { "dt_txt": "2026-06-10 18:00:00", "main": { "temp": 16.0, "temp_min": 13.0, "temp_max": 19.0 }, "weather": [{ "description": "light rain" }], "pop": 0.8, "rain": { "3h": 1.5 } },  // pii-test-fixture
                { "dt_txt": "2026-06-11 12:00:00", "main": { "temp": 22.0, "temp_min": 19.0, "temp_max": 25.0 }, "weather": [{ "description": "few clouds" }] }  // pii-test-fixture
            ]
        })
    }

    // ── location resolution, AT THE CONFIG LEVEL ─────────────────────────────
    //
    // These exercise `WeatherConfig::resolve_location` — the PRODUCTION entry
    // point — not `location::resolve` standalone. The bug being fixed here was
    // precisely that `location::resolve` was fully implemented, fully tested, and
    // never called; a test that only calls the module proves nothing about the
    // tool. The end-to-end `execute`-level tests further down close the loop.

    #[tokio::test]
    async fn resolve_explicit_location_passthrough() {
        let c = offline_cfg(
            routine_of(Some("Reno NV"), None),
            events(json!([{"summary": "Trip", "location": "Denver"}])),
        );
        match c.resolve_location(Some("Paris"), operator()).await {
            Resolved::Found { location, source: LocationSource::Explicit } => {
                assert_eq!(location, "Paris")
            }
            other => panic!("explicit must win, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_omitted_location_prefers_the_calendar_over_the_routine() {
        let c = offline_cfg(
            routine_of(Some("123 Home St"), None),
            events(json!([{"summary": "Client onsite", "location": "Denver, CO"}])),
        );
        match c.resolve_location(None, operator()).await {
            Resolved::Found { location, source: LocationSource::Calendar(s) } => {
                assert_eq!(location, "Denver, CO");
                assert_eq!(s, "Client onsite");
            }
            other => panic!("expected the calendar to win, got {other:?}"),
        }
        // whitespace is treated as omitted, and still consults the calendar
        match c.resolve_location(Some("  "), operator()).await {
            Resolved::Found { source: LocationSource::Calendar(_), .. } => {}
            other => panic!("blank location must fall through, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_falls_back_to_the_routine_when_the_calendar_is_empty() {
        let c = offline_cfg(routine_of(Some("123 Home St"), None), Arc::new(NoCalendar));
        match c.resolve_location(None, operator()).await {
            Resolved::Found { location, source: LocationSource::Routine(_) } => {
                assert_eq!(location, "123 Home St")
            }
            other => panic!("expected the routine, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_asks_when_nothing_is_known() {
        // THE live bug. Previously this returned NotConfigured, the model retried,
        // and it answered for a city lifted out of this tool's own schema.
        let c = offline_cfg(Routine::default(), Arc::new(NoCalendar));
        assert_eq!(c.resolve_location(None, operator()).await, Resolved::AskUser);
    }

    #[test]
    fn parse_coord_pair_works() {
        assert_eq!(parse_coord_pair("37.75,-122.41"), Some((37.75, -122.41)));
        assert_eq!(parse_coord_pair(" 1.0 , 2.0 "), Some((1.0, 2.0)));
        assert_eq!(parse_coord_pair("San Jose, CA"), None);
        assert_eq!(parse_coord_pair("37.75"), None);
    }

    // ── geocoding fallback (street address → city) ───────────────────────────

    #[test]
    fn geocode_candidates_coarsen_address() {
        assert_eq!(
            geocode_candidates("123 Main St, San Jose, CA 95123"),
            vec![
                "123 Main St, San Jose, CA 95123".to_string(),
                "San Jose, CA 95123".to_string(),
                "CA 95123".to_string(),
            ]
        );
        // A bare city yields just itself.
        assert_eq!(geocode_candidates("Paris"), vec!["Paris".to_string()]);
        // Whitespace around components is normalised.
        assert_eq!(
            geocode_candidates(" 1 A Rd , Reno , NV "),
            vec!["1 A Rd, Reno, NV".to_string(), "Reno, NV".to_string(), "NV".to_string()]
        );
    }

    #[test]
    fn geocode_candidates_space_separated_no_comma() {
        // The live bug: "Tampa Florida" (space, no comma) → 0 results from OWM.
        // We must offer the comma'd and trailing-dropped variants too.
        let cands = geocode_candidates("Tampa Florida");
        assert!(cands.contains(&"Tampa Florida".to_string()), "{cands:?}");
        assert!(cands.contains(&"Tampa, Florida".to_string()), "{cands:?}");
        assert!(cands.contains(&"Tampa".to_string()), "{cands:?}");
    }

    #[test]
    fn geocode_candidates_multiword_city_preserved() {
        // "San Jose California" must yield "San Jose, California" and "San Jose"
        // — NOT just the first token "San".
        let cands = geocode_candidates("San Jose California");
        assert!(cands.contains(&"San Jose, California".to_string()), "{cands:?}");
        assert!(cands.contains(&"San Jose".to_string()), "{cands:?}");
        assert!(!cands.contains(&"San".to_string()), "{cands:?}");
    }

    /// The live geocoding bug end-to-end: "Tampa Florida" (no comma) returns
    /// 200 + [] from OWM; the tool must retry a coarser variant and succeed.
    #[tokio::test]
    async fn space_separated_location_falls_back_to_comma_variant() {
        let server = MockServer::start();
        let geo_full = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct")
                .query_param("q", "Tampa Florida");
            then.status(200).json_body(json!([])); // the live bug: 0 results
        });
        let geo_comma = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct")
                .query_param("q", "Tampa, Florida");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "Tampa Florida"})).await.unwrap();
        geo_full.assert();  // no-comma string tried first
        geo_comma.assert(); // comma variant resolved it
        wx.assert();
        assert!(out.contains("clear sky"));
    }

    // ── days param (multi-day forecast) ──────────────────────────────────────

    #[tokio::test]
    async fn days_three_hits_forecast_and_returns_three_days() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let fc = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "days": 3})).await.unwrap();
        fc.assert();
        assert_eq!(wx.hits(), 0, "days>=2 must not hit the current endpoint");
        assert!(out.contains("3-day forecast"));
        // every distinct day present
        assert!(out.contains("2026-06-09"));  // pii-test-fixture
        assert!(out.contains("2026-06-10"));  // pii-test-fixture
        assert!(out.contains("2026-06-11"));  // pii-test-fixture
    }

    #[tokio::test]
    async fn days_clamps_to_available_days() {
        // Ask for 7 but the mock only has 3 distinct days.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "days": 7})).await.unwrap();
        assert!(out.contains("3-day forecast"), "{out}");
    }

    #[tokio::test]
    async fn days_one_is_current() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let fc = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "days": 1})).await.unwrap();
        wx.assert();
        assert_eq!(fc.hits(), 0, "days=1 must use the current endpoint");
        assert!(out.starts_with("Current weather"));
    }

    #[tokio::test]
    async fn days_takes_precedence_over_when() {
        // days=3 wins even though when=current is also present.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let fc = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool
            .execute_as_operator(json!({"location": "SF", "days": 3, "when": "current"}))
            .await
            .unwrap();
        fc.assert();
        assert!(out.contains("3-day forecast"));
    }

    /// The actual bug: COMMUTE_HOME is a full street address that OWM's
    /// geocoder returns 200 + [] for. The tool must retry with the coarser
    /// "city, state" variant and still succeed.
    #[tokio::test]
    async fn full_address_falls_back_to_city_geocode() {
        let server = MockServer::start();
        let geo_full = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct")
                .query_param("q", "123 Main St, San Jose, CA 95123");
            then.status(200).json_body(json!([])); // OWM can't place a street address
        });
        let geo_city = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct")
                .query_param("q", "San Jose, CA 95123");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        // Bare call → defaults to COMMUTE_HOME (the full address) → must succeed.
        let tool = Weather { cfg: cfg_for(&server, Some("123 Main St, San Jose, CA 95123")) };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        geo_full.assert(); // full address was tried first
        geo_city.assert(); // coarser variant resolved it
        wx.assert();
        assert!(out.contains("clear sky"));
    }

    #[tokio::test]
    async fn all_geocode_candidates_empty_errors_clearly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(json!([]));
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        match tool.execute_as_operator(json!({"location": "Nowhere, ZZ", "when": "current"})).await {
            Err(ToolError::NotFound(m)) => assert!(m.contains("Could not geocode")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // ── missing key → NotConfigured ──────────────────────────────────────────

    #[tokio::test]
    async fn stub_returns_not_configured() {
        let r = NotConfiguredStub.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    // ── current → /data/2.5/weather ──────────────────────────────────────────

    #[tokio::test]
    async fn current_hits_weather_endpoint() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "San Francisco");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });

        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "San Francisco", "when": "current"})).await.unwrap();
        geo.assert();
        wx.assert();
        assert!(out.contains("clear sky"));
        // Dual units, always: 18°C → 64°F.
        assert!(out.contains("18°C"), "{out}");
        assert!(out.contains("64°F"), "{out}");
    }

    /// Current conditions render the full enriched report: dual temps, humidity,
    /// dual wind, and a "What to wear" line.
    #[tokio::test]
    async fn current_output_is_enriched_dual_units() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF"})).await.unwrap();
        // both unit systems for temp and feels-like
        assert!(out.contains("°F") && out.contains("°C"), "{out}");
        assert!(out.contains("feels like"), "{out}");
        // humidity
        assert!(out.contains("humidity 60%"), "{out}");
        // dual wind
        assert!(out.contains("km/h") && out.contains("mph"), "{out}");
        // clothing suggestion present
        assert!(out.contains("What to wear:"), "{out}");
    }

    /// A wet current report surfaces precipitation volume and an umbrella/
    /// waterproof clothing modifier.
    #[tokio::test]
    async fn current_output_reports_precipitation() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(rainy_current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF"})).await.unwrap();
        assert!(out.contains("precipitation"), "{out}");
        assert!(out.contains("1.2 mm rain"), "{out}");
        let low = out.to_lowercase();
        assert!(low.contains("umbrella") || low.contains("waterproof"), "{out}");
    }

    /// A multi-day forecast renders dual temp ranges, precipitation probability,
    /// and a "What to wear" line per day.
    #[tokio::test]
    async fn forecast_output_is_enriched() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "days": 3})).await.unwrap();
        // dual range, e.g. 2026-06-10 12–16C → 54–61F  // pii-test-fixture
        assert!(out.contains("°F") && out.contains("°C"), "{out}");
        // precipitation probability surfaced for the rainy day (max pop 0.8)
        assert!(out.contains("80% chance"), "{out}");
        assert!(out.contains("mm rain"), "{out}");
        // per-day clothing suggestion
        assert!(out.contains("What to wear:"), "{out}");
    }

    #[tokio::test]
    async fn current_is_default_when_when_omitted() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "San Francisco"})).await.unwrap();
        wx.assert();
        assert!(out.starts_with("Current weather"));
    }

    // ── omitted location uses COMMUTE_HOME (BUG 1, end-to-end) ────────────────

    #[tokio::test]
    async fn omitted_location_geocodes_home() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "1 Home Rd");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, Some("1 Home Rd")) };
        // No "location" key at all.
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        geo.assert();
        assert!(out.contains("1 Home Rd"));
    }

    // ── THE LIVE BUG, END TO END AT THE TOOL BOUNDARY ────────────────────────
    //
    // Everything below drives `Weather::execute` — the path the model actually
    // reaches. The previous round of tests passed against `location.rs` while the
    // tool still ran the old COMMUTE_HOME-only branch, so "tests are green" said
    // nothing about the live behaviour. These would all fail if the wiring were
    // removed.

    /// Nothing resolvable → the tool ASKS, in plain language, naming NO city, and
    /// makes no network call at all (nothing to geocode).
    #[tokio::test]
    async fn execute_asks_when_nothing_resolves_and_names_no_city() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(&server, Routine::default(), Arc::new(NoCalendar)),
        };
        // An Ok answer, NOT an error: an error is what made the model retry and
        // invent a city.
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        assert_eq!(out, ASK_MESSAGE);
        assert_eq!(geo.hits(), 0, "nothing to geocode — must not call out");
        assert_eq!(wx.hits(), 0);
        // The regression guard: the ask must never seed a place.
        let low = out.to_lowercase();
        for city in [
            "tampa", "florida", "paris", "omaha", "san jose", "foster city",
            "new york", "london",
        ] {
            assert!(!low.contains(city), "the ask must not name {city:?}: {out}");
        }
    }

    /// A calendar event supplies the location, and the answer SAYS so.
    #[tokio::test]
    async fn execute_uses_a_calendar_event_and_attributes_it() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "Denver, CO");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(
                &server,
                // A home is configured — the calendar must still win, because the
                // weather you care about is where you will BE.
                routine_of(Some("1 Home Rd"), None),
                events(json!([{"summary": "Client onsite", "location": "Denver, CO"}])),
            ),
        };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        geo.assert();
        assert!(out.contains("Denver, CO"), "{out}");
        assert!(out.to_lowercase().contains("calendar"), "must attribute: {out}");
        assert!(out.contains("Client onsite"), "must name the event: {out}");
        assert!(!out.contains("1 Home Rd"), "the routine must not win: {out}");
    }

    /// A video-call "location" is NOT a place — the tool falls through to the
    /// routine rather than geocoding a meeting link.
    #[tokio::test]
    async fn execute_ignores_a_virtual_event_location() {
        for virt in ["https://zoom.us/j/123", "Microsoft Teams Meeting", "TBD"] {
            let server = MockServer::start();
            let geo_home = server.mock(|when, then| {
                when.method(GET).path("/geo/1.0/direct").query_param("q", "1 Home Rd");
                then.status(200).json_body(geo_body());
            });
            server.mock(|when, then| {
                when.method(GET).path("/data/2.5/weather");
                then.status(200).json_body(current_body());
            });
            let tool = Weather {
                cfg: cfg_full(
                    &server,
                    routine_of(Some("1 Home Rd"), None),
                    events(json!([{"summary": "Sync", "location": virt}])),
                ),
            };
            let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
            geo_home.assert();
            assert!(out.contains("1 Home Rd"), "{virt} → {out}");
            assert!(
                out.to_lowercase().contains("home"),
                "must attribute the routine: {out}"
            );
            assert!(!out.contains(virt), "{virt} must never be used as a place: {out}");
        }
    }

    /// An explicit location beats a calendar event AND is not attributed (the
    /// user already knows what they asked for).
    #[tokio::test]
    async fn execute_explicit_location_wins_and_is_not_attributed() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "Reykjavik");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(
                &server,
                routine_of(Some("1 Home Rd"), None),
                events(json!([{"summary": "Client onsite", "location": "Denver, CO"}])),
            ),
        };
        let out = tool.execute_as_operator(json!({"location": "Reykjavik"})).await.unwrap();
        geo.assert();
        assert!(out.starts_with("Current weather for Reykjavik"), "{out}");
        assert!(!out.to_lowercase().contains("calendar"), "{out}");
    }

    /// A cancelled event is not where the user will be — the next real event wins.
    #[tokio::test]
    async fn execute_skips_a_cancelled_event() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "Austin");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(
                &server,
                Routine::default(),
                events(json!([
                    {"summary": "Cancelled trip", "location": "Denver", "status": "cancelled"},
                    {"summary": "Real trip", "location": "Austin"}
                ])),
            ),
        };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        geo.assert();
        assert!(out.contains("Austin"), "{out}");
        assert!(!out.contains("Denver"), "{out}");
    }

    /// END-TO-END, from RAW iCAL: an event the user DECLINED whose `STATUS` is still
    /// `CONFIRMED` — the normal shape of a declined invite, since declining changes
    /// the attendee's PARTSTAT and not the organiser's STATUS — must not supply its
    /// LOCATION. The tool falls through to the routine.
    ///
    /// This goes through `parse_ical`/`event_status` rather than a hand-written
    /// `"status": "declined"` JSON literal ON PURPOSE: the bug was in that computation,
    /// and the resolver's own tests (which take `status` as given) could never see it.
    /// Only a test that spans the wiring catches a `status` field that is correct at
    /// one end and wrong at the other.
    #[tokio::test]
    async fn execute_skips_a_confirmed_but_declined_event_end_to_end() {
        let cal = crate::google::caldav::location_events_from_ical(
            &confirmed_but_declined_ical(),
            "primary",
        );
        assert_eq!(cal.len(), 1, "fixture must yield exactly one event");

        let server = MockServer::start();
        let geo_home = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "1 Home Rd");
            then.status(200).json_body(geo_body());
        });
        let geo_denver = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "Denver, CO");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(
                &server,
                routine_of(Some("1 Home Rd"), None),
                events(Value::Array(cal)),
            ),
        };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        assert_eq!(geo_denver.hits(), 0, "a declined event must never be geocoded: {out}");
        geo_home.assert();
        assert!(out.contains("1 Home Rd"), "must fall through to the routine: {out}");
        assert!(!out.contains("Denver"), "declined location must not surface: {out}");
    }

    /// Same event, but with NO routine configured: the tool must ASK rather than fall
    /// back to the declined event's location. "Skip it" must not degrade to "use it
    /// anyway when there is nothing else".
    #[tokio::test]
    async fn execute_asks_rather_than_using_a_declined_events_location() {
        let cal = crate::google::caldav::location_events_from_ical(
            &confirmed_but_declined_ical(),
            "primary",
        );

        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let tool = Weather {
            cfg: cfg_full(&server, Routine::default(), events(Value::Array(cal))),
        };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        assert_eq!(out, ASK_MESSAGE);
        assert_eq!(geo.hits(), 0, "nothing resolvable — must not geocode: {out}");
    }

    // ── POSITIVE CONTROLS ────────────────────────────────────────────────────
    //
    // The trap this change exists to avoid: a suite that passes against code the
    // tool never calls. A mock seam can be satisfied by a stub, so these two
    // tests pin behaviour that NO constant and NO ignored-calendar implementation
    // can produce. If `resolve_location` were hardwired to any fixed answer — a
    // city, the home address, always-ASK, or "ignore the calendar" — at least one
    // of them fails.

    /// Two DIFFERENT calendars must produce two DIFFERENT locations, from the
    /// same config and the same arguments. A hardwired return cannot do this.
    #[tokio::test]
    async fn wiring_is_not_stubbed_calendar_content_changes_the_answer() {
        let mut seen: Vec<String> = Vec::new();
        for (place, summary) in [("Austin", "Trip A"), ("Reykjavik", "Trip B")] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/geo/1.0/direct").query_param("q", place);
                then.status(200).json_body(geo_body());
            });
            server.mock(|when, then| {
                when.method(GET).path("/data/2.5/weather");
                then.status(200).json_body(current_body());
            });
            let tool = Weather {
                cfg: cfg_full(
                    &server,
                    routine_of(Some("1 Home Rd"), None),
                    events(json!([{"summary": summary, "location": place}])),
                ),
            };
            let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
            assert!(out.contains(place), "calendar location must reach the answer: {out}");
            assert!(out.contains(summary), "event summary must reach the answer: {out}");
            seen.push(out);
        }
        assert_ne!(seen[0], seen[1], "the answer must depend on the calendar");
    }

    /// The calendar is actually CONSULTED — the seam is awaited, not skipped. A
    /// wiring that ignored the source (or short-circuited to the routine) would
    /// leave this counter at zero.
    #[tokio::test]
    async fn wiring_is_not_stubbed_the_calendar_is_actually_consulted() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingCalendar(Arc<AtomicUsize>);
        #[async_trait]
        impl CalendarSource for CountingCalendar {
            async fn events_now(&self) -> Vec<Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let server = MockServer::start();
        let cfg = cfg_full(
            &server,
            Routine::default(),
            Arc::new(CountingCalendar(hits.clone())),
        );
        assert_eq!(
            Weather { cfg: cfg.clone() }.execute_as_operator(json!({"when": "current"})).await.unwrap(),
            ASK_MESSAGE
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the calendar must be consulted");

        // ...and NOT consulted when the caller named a place: an explicit
        // location short-circuits the chain before any calendar round-trip.
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        Weather { cfg }.execute_as_operator(json!({"location": "Reykjavik"})).await.unwrap();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an explicit location must not cost a calendar fetch"
        );
    }

    // ── TRTR-05: THE GUEST PRIVACY GATE, END TO END AT THE TOOL BOUNDARY ────
    //
    // `weather` is granted to household guests (`GUEST_BASELINE_ALLOW`). Before
    // this gate, a guest asking "what's the weather?" with no location got the
    // OPERATOR's whereabouts, attributed out loud with the calendar event's
    // SUMMARY — "using <address> — from your calendar (Dentist appointment …)".
    // These drive `Weather` at the tool boundary, the path a guest's turn
    // actually reaches.

    /// The operator's day, as the fixtures model it. Placeholders only — never a
    /// real name or address. The SUMMARY is the most sensitive field (it says
    /// what the appointment IS), so it is what the guest assertions pin.
    const OPERATOR_EVENT_SUMMARY: &str = "Dentist appointment"; // pii-test-fixture: obvious placeholder for a real calendar entry
    const OPERATOR_EVENT_PLACE: &str = "000 Placeholder St, Examplecity"; // pii-test-fixture: obvious placeholder for a real appointment address
    const OPERATOR_HOME: &str = "111 Placeholder Ave, Examplecity"; // pii-test-fixture: obvious placeholder for the operator's home address
    const OPERATOR_WORK: &str = "222 Placeholder Rd, Examplecity"; // pii-test-fixture: obvious placeholder for the operator's work address

    fn operator_day() -> Value {
        json!([{ "summary": OPERATOR_EVENT_SUMMARY, "location": OPERATOR_EVENT_PLACE }])
    }

    /// Everything a guest's answer must never contain.
    fn assert_no_operator_context(out: &str) {
        for leaked in [OPERATOR_EVENT_SUMMARY, OPERATOR_EVENT_PLACE, OPERATOR_HOME, OPERATOR_WORK] {
            assert!(!out.contains(leaked), "leaked operator context {leaked:?} into: {out}");
        }
        assert!(
            !out.to_lowercase().contains("calendar"),
            "no attribution derived from operator data: {out}"
        );
    }

    #[tokio::test]
    async fn a_guest_omitting_the_location_is_asked_and_the_calendar_is_never_read() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingCalendar(Arc<AtomicUsize>, Vec<Value>);
        #[async_trait]
        impl CalendarSource for CountingCalendar {
            async fn events_now(&self) -> Vec<Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                self.1.clone()
            }
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let tool = Weather {
            cfg: cfg_full(
                &server,
                routine_of(Some(OPERATOR_HOME), None),
                Arc::new(CountingCalendar(
                    hits.clone(),
                    operator_day().as_array().cloned().unwrap(),
                )),
            ),
        };

        // `execute` (no caller) IS the guest/unknown path: untrusted by
        // construction. Asserted explicitly below via `execute_with_caller` too.
        let out = tool.execute(json!({"when": "current"})).await.unwrap();

        assert_eq!(out, ASK_MESSAGE, "a guest with no location must be ASKED");
        assert_no_operator_context(&out);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the operator's calendar must not be read on a guest's behalf"
        );
        assert_eq!(geo.hits(), 0, "nothing resolved — nothing to geocode");

        // ...and identically for an explicitly-guest caller.
        let out = tool
            .execute_with_caller(json!({"when": "current"}), guest())
            .await
            .unwrap()
            .text;
        assert_eq!(out, ASK_MESSAGE);
        assert_no_operator_context(&out);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The legitimate guest use must be completely unaffected.
    #[tokio::test]
    async fn a_guest_naming_a_location_gets_a_normal_forecast() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", "Reykjavik");
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(&server, routine_of(Some(OPERATOR_HOME), None), events(operator_day())),
        };
        let out = tool
            .execute_with_caller(json!({"location": "Reykjavik", "when": "current"}), guest())
            .await
            .unwrap()
            .text;
        geo.assert();
        assert!(out.contains("clear sky"), "a real forecast: {out}");
        assert_no_operator_context(&out);
    }

    /// POSITIVE CONTROL for the gate: the SAME config and the SAME arguments,
    /// asked as the operator, must still resolve from the calendar and attribute
    /// it. Without this, "the guest is asked" would also pass if the fix had
    /// simply switched inference off for everybody.
    #[tokio::test]
    async fn the_operator_omitting_the_location_still_gets_the_calendar_answer() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct").query_param("q", OPERATOR_EVENT_PLACE);
            then.status(200).json_body(geo_body());
        });
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather {
            cfg: cfg_full(&server, routine_of(Some(OPERATOR_HOME), None), events(operator_day())),
        };
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        geo.assert();
        assert!(out.contains(OPERATOR_EVENT_PLACE), "{out}");
        assert!(out.contains(OPERATOR_EVENT_SUMMARY), "must attribute the event: {out}");
        assert!(out.to_lowercase().contains("calendar"), "{out}");
    }

    /// The routine (home/work addresses) is gated separately from the calendar:
    /// a guest with an empty calendar must still not be answered with the
    /// operator's home address.
    ///
    /// **This test must not depend on the wall clock.** `Routine::pick` chooses
    /// work over home on a weekday between 09:00 and 17:59, from the REAL clock
    /// (`local_hour_and_weekday`), so a positive control that hardcoded "the
    /// operator gets HOME" failed every weekday daytime run — observed picking
    /// the office at 09:03 on a Friday. A security test that is red on half the
    /// calendar is a test people learn to ignore, so the claim is asserted the
    /// way it is actually meant: **the guest gets NEITHER routine address, the
    /// operator gets exactly ONE of them.** Which one is business logic that the
    /// hour-parameterised checks at the bottom of this test (and
    /// `location::tests::the_routine_picks_*`) pin down deterministically.
    #[tokio::test]
    async fn a_guest_never_falls_back_to_the_operators_home_address() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let routine = routine_of(Some(OPERATOR_HOME), Some(OPERATOR_WORK));
        let tool = Weather {
            cfg: cfg_full(&server, routine.clone(), Arc::new(NoCalendar)),
        };
        let out = tool
            .execute_with_caller(json!({"when": "current"}), guest())
            .await
            .unwrap()
            .text;
        assert_eq!(out, ASK_MESSAGE);
        assert_no_operator_context(&out);
        assert_eq!(geo.hits(), 0, "nothing resolved — nothing to geocode");

        // The operator, same config, still gets a routine answer (positive
        // control). WHICH routine location depends on the hour, so assert on
        // "one of them, attributed as a routine" rather than on home.
        server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let out = tool.execute_as_operator(json!({"when": "current"})).await.unwrap();
        let home = out.contains(OPERATOR_HOME);
        let work = out.contains(OPERATOR_WORK);
        assert!(
            home ^ work,
            "the operator's routine fallback must survive and name exactly one \
             configured location: {out}"
        );
        assert!(
            out.contains("using your home location") || out.contains("using your work location"),
            "and attribute it as the routine inference it is: {out}"
        );
        assert!(geo.hits() >= 1, "the operator's resolved location WAS geocoded");

        // ...and deterministically, at every time of day and both kinds of day:
        // the guest is always ASKED and the operator always gets a routine hit.
        // This is the same gate driven through the hour-parameterised resolver,
        // so weekday-daytime / weekday-evening / weekend are all covered on
        // every run instead of whichever one the clock happens to be in.
        for (hour, weekday) in [(11u32, true), (20, true), (11, false), (3, false)] {
            let g = location::resolve_with_calendar(
                None, &NoCalendar, &routine, hour, weekday, guest(),
            )
            .await;
            assert_eq!(
                g,
                Resolved::AskUser,
                "guest must be ASKED at hour={hour} weekday={weekday}"
            );
            assert!(g.attribution().is_none());

            let o = location::resolve_with_calendar(
                None, &NoCalendar, &routine, hour, weekday, operator(),
            )
            .await;
            match &o {
                Resolved::Found { location, source: LocationSource::Routine(_) } => {
                    assert!(
                        location == OPERATOR_HOME || location == OPERATOR_WORK,
                        "hour={hour} weekday={weekday}: {location}"
                    );
                }
                other => panic!("operator must still get a routine hit at hour={hour} weekday={weekday}, got {other:?}"),
            }
        }
    }

    // ── tomorrow → /data/2.5/forecast, tomorrow extraction ───────────────────

    #[tokio::test]
    async fn tomorrow_hits_forecast_and_extracts_second_day() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let fc = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "when": "tomorrow"})).await.unwrap();
        fc.assert();
        // Second distinct date is 2026-06-10 with "light rain", 12–19.  // pii-test-fixture
        assert!(out.contains("2026-06-10"));  // pii-test-fixture
        assert!(out.contains("light rain"));
        assert!(out.contains("12") && out.contains("19"));
        // must NOT report today's clear sky as tomorrow
        assert!(!out.contains("2026-06-09"));  // pii-test-fixture
    }

    // ── week → /data/2.5/forecast, full outlook ──────────────────────────────

    #[tokio::test]
    async fn week_summarises_all_days() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let fc = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/forecast");
            then.status(200).json_body(forecast_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "SF", "when": "week"})).await.unwrap();
        fc.assert();
        // Three distinct days present (clamped to what the mock returns).
        assert!(out.contains("3-day forecast"));
        assert!(out.contains("2026-06-09"));  // pii-test-fixture
        assert!(out.contains("2026-06-10"));  // pii-test-fixture
        assert!(out.contains("2026-06-11"));  // pii-test-fixture
        assert!(out.contains("few clouds"));
    }

    // ── coord pair skips geocoding ───────────────────────────────────────────

    #[tokio::test]
    async fn coord_pair_skips_geocode() {
        let server = MockServer::start();
        let geo = server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let wx = server.mock(|when, then| {
            when.method(GET).path("/data/2.5/weather");
            then.status(200).json_body(current_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let out = tool.execute_as_operator(json!({"location": "37.77,-122.41"})).await.unwrap();
        // geocode endpoint should NOT have been called
        assert_eq!(geo.hits(), 0);
        wx.assert();
        assert!(out.contains("clear sky"));
    }

    // ── invalid `when` ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn invalid_when_errors() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/geo/1.0/direct");
            then.status(200).json_body(geo_body());
        });
        let tool = Weather { cfg: cfg_for(&server, None) };
        let r = tool.execute_as_operator(json!({"location": "SF", "when": "yesterday"})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    // ── forecast parsing helpers ─────────────────────────────────────────────

    #[test]
    fn group_by_date_preserves_order_and_groups() {
        let body = forecast_body();
        let list = body.get("list").and_then(Value::as_array).unwrap();
        let grouped = group_by_date(list);
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0, "2026-06-09");  // pii-test-fixture
        assert_eq!(grouped[0].1.len(), 2);
        assert_eq!(grouped[1].0, "2026-06-10");  // pii-test-fixture
        assert_eq!(grouped[1].1.len(), 2);
        assert_eq!(grouped[2].1.len(), 1);
    }

    #[test]
    fn summarise_points_min_max_and_condition() {
        let body = forecast_body();
        let list = body.get("list").and_then(Value::as_array).unwrap();
        let grouped = group_by_date(list);
        let (date, pts) = &grouped[1]; // tomorrow
        let s = summarise_points(date, pts).unwrap();
        assert_eq!(s.condition, "light rain");
        assert_eq!(s.temp_min, 12.0);
        assert_eq!(s.temp_max, 19.0);
    }

    #[test]
    fn temp_conversion_dual() {
        // f = c*9/5 + 32, rounded to whole degrees in the rendered string.
        assert_eq!(c_to_f(0.0), 32.0);
        assert_eq!(c_to_f(100.0), 212.0);
        assert_eq!(c_to_f(22.0), 71.6);
        assert_eq!(dual_temp(22.0), "72°F / 22°C"); // 71.6 rounds to 72
        assert_eq!(dual_temp(0.0), "32°F / 0°C");
        assert_eq!(dual_range(12.0, 20.0), "54–68°F / 12–20°C");
    }

    #[test]
    fn what_to_wear_spans_temp_range() {
        // Cold → coat/gloves; mild → light jacket; warm → t-shirt.
        assert!(what_to_wear(-5.0, "clear sky", None).to_lowercase().contains("coat"));
        assert!(what_to_wear(-5.0, "clear sky", None).to_lowercase().contains("glove"));
        assert!(what_to_wear(18.0, "clear sky", None).to_lowercase().contains("light jacket"));
        assert!(what_to_wear(25.0, "clear sky", None).to_lowercase().contains("t-shirt"));
        assert!(what_to_wear(33.0, "clear sky", None).to_lowercase().contains("hydrated"));
        // Rain adds an umbrella/waterproof modifier.
        let rainy = what_to_wear(15.0, "light rain", None).to_lowercase();
        assert!(rainy.contains("umbrella") || rainy.contains("waterproof"), "{rainy}");
        // Snow adds boots.
        assert!(what_to_wear(-2.0, "light snow", None).to_lowercase().contains("boots"));
        // Strong wind adds a windproof note.
        assert!(what_to_wear(10.0, "clear sky", Some(10.0)).to_lowercase().contains("wind"));
    }

    #[test]
    fn precip_phrase_labels_clearly() {
        assert_eq!(precip_phrase(None, None, None), None);
        assert_eq!(
            precip_phrase(Some(0.6), None, None).unwrap(),
            "precipitation 60% chance"
        );
        let both = precip_phrase(Some(0.8), Some(2.5), None).unwrap();
        assert!(both.contains("80% chance"));
        assert!(both.contains("2.5 mm rain"));
        assert!(precip_phrase(None, None, Some(4.0)).unwrap().contains("4.0 mm snow"));
    }

    // ── registration ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_weather_stub_without_key() {
        let mut reg = ToolRegistry::new();
        let key = std::env::var("OPENWEATHER_API_KEY").ok();
        std::env::remove_var("OPENWEATHER_API_KEY");
        register(&mut reg);
        if let Some(k) = key { std::env::set_var("OPENWEATHER_API_KEY", k); }
        assert!(reg.contains("weather"));
    }

    #[test]
    fn tool_name_and_schema_stable() {
        let t = Weather { cfg: offline_cfg(Routine::default(), Arc::new(NoCalendar)) };
        assert_eq!(t.name(), "weather");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert!(p["properties"]["location"].is_object());
        assert!(p["properties"]["when"]["enum"].is_array());
        assert!(p["properties"]["days"].is_object());
        let d = t.description().to_lowercase();
        // describes the resolution chain rather than a default city
        assert!(d.contains("calendar"));
        assert!(d.contains("optional"));
        // and steers the model to use this over a web search, for any place + days
        assert!(d.contains("web search"));
        assert!(d.contains("days"));
        assert!(d.contains("any"));
    }

    /// THE SCHEMA REGRESSION GUARD. "Tampa" reached the user because it was the
    /// first example in this tool's own `location` description — the model had
    /// nothing else and copied it. Nothing a model reads may name a place.
    #[test]
    fn description_and_schema_name_no_city() {
        let t = Weather { cfg: offline_cfg(Routine::default(), Arc::new(NoCalendar)) };
        let mut texts = vec![t.description().to_lowercase()];
        let p = t.parameters();
        for key in ["location", "days", "when"] {
            texts.push(p["properties"][key]["description"].to_string().to_lowercase());
        }
        // Cities that have actually been invented in live turns, plus the usual
        // schema-example suspects.
        for city in [
            "tampa", "florida", "paris", "london", "new york", "san jose",
            "foster city", "san francisco", "omaha", "denver", "seattle", "tokyo",
        ] {
            for t in &texts {
                assert!(!t.contains(city), "schema text must not name {city:?}: {t}");
            }
        }
        // ...and the location description must positively describe the CHAIN.
        let loc = p["properties"]["location"]["description"].as_str().unwrap().to_lowercase();
        assert!(loc.contains("calendar"), "{loc}");
        assert!(loc.contains("routine") || loc.contains("home/work"), "{loc}");
        assert!(loc.contains("ask"), "{loc}");
    }
}
