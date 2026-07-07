//! Weather tool — current conditions and forecasts via OpenWeatherMap.
//!
//! One LLM-callable tool:
//!   weather  — current / tomorrow / this-week weather for a location.
//!
//! Location resolution (BUG 1): when `location` is omitted or empty the tool
//! defaults to the operator's home address from the COMMUTE_HOME env var (the
//! same source of truth the commute tools use). This is what stops the model
//! from asking "which city?" — the description advertises the default and the
//! code injects it deterministically. If COMMUTE_HOME is also unset and no
//! location was given, a clear NotConfigured error is returned rather than a
//! silent failure.
//!
//! FUTURE ENHANCEMENT: an Engram "where does {user} live" lookup could be a
//! further fallback when COMMUTE_HOME is unset. That is intentionally OUT OF
//! SCOPE here — COMMUTE_HOME is the single env-based source of truth, per the
//! repo's inference de-bloat rules (env/Python over LLM).
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
//!   COMMUTE_HOME         — default location when none is supplied
//!
//! NOTE: temperatures are always fetched and displayed in metric+imperial, so
//! OPENWEATHER_UNITS is no longer consulted (canonical fetch is always metric).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

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
    /// Operator home address, reused from the commute tools' COMMUTE_HOME so a
    /// bare `weather` call resolves to "where I live" without re-prompting.
    home: Option<String>,
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
        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            units: CANONICAL_UNITS.to_string(),
            home: std::env::var("COMMUTE_HOME").ok().filter(|s| !s.is_empty()),
        })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Resolve the caller-supplied location into something the OWM API accepts.
    /// An absent/empty location falls back to COMMUTE_HOME (BUG 1); if that is
    /// also unset we return a clear, actionable NotConfigured error rather than
    /// silently guessing.
    fn resolve_location(&self, input: Option<&str>) -> Result<String, ToolError> {
        let trimmed = input.map(str::trim).filter(|s| !s.is_empty());
        match trimmed {
            Some(loc) => Ok(loc.to_string()),
            None => self.home.clone().ok_or_else(|| {
                ToolError::NotConfigured(
                    "No location given and COMMUTE_HOME is not configured. \
                     Set COMMUTE_HOME to a default home address or pass a 'location'."
                        .into(),
                )
            }),
        }
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

    Err(ToolError::NotFound(format!(
        "Could not geocode '{location}' (try a city name, e.g. 'San Jose, CA')"
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
        "Get the weather for ANY place — ALWAYS use this tool for weather questions \
instead of a web search. It works for any city, town, address, landmark, or \
'lat,lon' anywhere in the world, not just the user's home. It returns BOTH current \
conditions AND multi-day forecasts (up to ~5–6 days ahead) directly from live \
weather data. \
Pass 'location' (e.g. 'Tampa', 'Tampa, Florida', 'Paris', '123 Main St, San Jose CA') \
— it is OPTIONAL and defaults to the user's home, so you never need to ask which \
city when they mean home. \
Pass 'days' (1–7) for a forecast: days=1 (or omit) gives current conditions; \
days=3 gives a 3-day forecast with each day's high/low and conditions; days=5 gives \
a 5-day forecast, etc. (clamped to what the data provides). \
The legacy 'when' field still works ('current', 'tomorrow', 'week') but prefer \
'days'. Returns a short human-readable summary."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "Any city, town, address, landmark, or 'lat,lon' — e.g. 'Tampa', 'Tampa, Florida', 'Paris'. Optional; defaults to the user's home."
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

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let location = self.cfg.resolve_location(args["location"].as_str())?;
        let mode = Mode::resolve(&args)?;

        let client = WeatherConfig::client()?;
        let (lat, lon) = geocode(&client, &self.cfg, &location).await?;

        match mode {
            Mode::Current => {
                let body = fetch_current(&client, &self.cfg, lat, lon).await?;
                Ok(format_current(&self.cfg, &location, &body))
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
                Ok(format!(
                    "Tomorrow's weather for {location} — {}",
                    format_day(&self.cfg, &day)
                ))
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
                Ok(out)
            }
        }
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

    fn cfg_for(server: &MockServer, home: Option<&str>) -> WeatherConfig {
        WeatherConfig {
            api_key: "testkey".into(),
            base_url: server.base_url(),
            units: "metric".into(),
            home: home.map(str::to_string),
        }
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

    // ── location resolution (BUG 1) ──────────────────────────────────────────

    #[test]
    fn resolve_explicit_location_passthrough() {
        let c = WeatherConfig {
            api_key: "k".into(),
            base_url: "http://x".into(),
            units: "metric".into(),
            home: Some("Reno NV".into()),
        };
        assert_eq!(c.resolve_location(Some("Paris")).unwrap(), "Paris");
    }

    #[test]
    fn resolve_omitted_location_falls_back_to_home() {
        let c = WeatherConfig {
            api_key: "k".into(),
            base_url: "http://x".into(),
            units: "metric".into(),
            home: Some("123 Home St".into()),
        };
        assert_eq!(c.resolve_location(None).unwrap(), "123 Home St");
        // empty string is treated as omitted
        assert_eq!(c.resolve_location(Some("  ")).unwrap(), "123 Home St");
    }

    #[test]
    fn resolve_missing_location_and_home_errors() {
        let c = WeatherConfig {
            api_key: "k".into(),
            base_url: "http://x".into(),
            units: "metric".into(),
            home: None,
        };
        match c.resolve_location(None) {
            Err(ToolError::NotConfigured(msg)) => {
                assert!(msg.contains("COMMUTE_HOME"));
                assert!(msg.contains("location"));
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }
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
        let out = tool.execute(json!({"location": "Tampa Florida"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF", "days": 3})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF", "days": 7})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF", "days": 1})).await.unwrap();
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
            .execute(json!({"location": "SF", "days": 3, "when": "current"}))
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
        let out = tool.execute(json!({"when": "current"})).await.unwrap();
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
        match tool.execute(json!({"location": "Nowhere, ZZ", "when": "current"})).await {
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
        let out = tool.execute(json!({"location": "San Francisco", "when": "current"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF", "days": 3})).await.unwrap();
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
        let out = tool.execute(json!({"location": "San Francisco"})).await.unwrap();
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
        let out = tool.execute(json!({"when": "current"})).await.unwrap();
        geo.assert();
        assert!(out.contains("1 Home Rd"));
    }

    #[tokio::test]
    async fn omitted_location_no_home_errors() {
        let server = MockServer::start();
        let tool = Weather { cfg: cfg_for(&server, None) };
        let r = tool.execute(json!({"when": "current"})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
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
        let out = tool.execute(json!({"location": "SF", "when": "tomorrow"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "SF", "when": "week"})).await.unwrap();
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
        let out = tool.execute(json!({"location": "37.77,-122.41"})).await.unwrap();
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
        let r = tool.execute(json!({"location": "SF", "when": "yesterday"})).await;
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
        let cfg = WeatherConfig {
            api_key: "k".into(),
            base_url: "http://x".into(),
            units: "metric".into(),
            home: None,
        };
        let t = Weather { cfg };
        assert_eq!(t.name(), "weather");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert!(p["properties"]["location"].is_object());
        assert!(p["properties"]["when"]["enum"].is_array());
        assert!(p["properties"]["days"].is_object());
        let d = t.description().to_lowercase();
        // description advertises the home default so the model won't re-prompt
        assert!(d.contains("home"));
        assert!(d.contains("optional"));
        // and steers the model to use this over a web search, for any place + days
        assert!(d.contains("web search"));
        assert!(d.contains("days"));
        assert!(d.contains("any"));
    }
}
