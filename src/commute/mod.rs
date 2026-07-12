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
//! Named locations resolve from env so the operator never has to repeat an
//! address: "home", "work"/"office", and "family"/"family home" map to the
//! configured COMMUTE_HOME / COMMUTE_WORK / COMMUTE_FAMILY values. Any other
//! string is treated as a literal address (or "lat,lon") and geocoded.
//!
//! Required env:
//!   TOMTOM_API_KEY   — TomTom routing + geocoding (driving tools)
//! Optional env:
//!   COMMUTE_HOME     — default home address (origin default for commute_estimate)
//!   COMMUTE_WORK     — default work address (destination default)
//!   COMMUTE_FAMILY   — family / occasional-visit address
//!   SF511_API_TOKEN  — 511.org token for transit_plan (free, https://511.org/open-data/token)

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

const METERS_PER_MILE: f64 = 1609.34;

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct CommuteConfig {
    api_key: String,
    home: Option<String>,
    work: Option<String>,
    family: Option<String>,
}

impl CommuteConfig {
    fn from_env() -> Result<Self, ToolError> {
        let api_key = std::env::var("TOMTOM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("TOMTOM_API_KEY not set".into()))?;
        Ok(Self {
            api_key,
            home: std::env::var("COMMUTE_HOME").ok().filter(|s| !s.is_empty()),
            work: std::env::var("COMMUTE_WORK").ok().filter(|s| !s.is_empty()),
            family: std::env::var("COMMUTE_FAMILY").ok().filter(|s| !s.is_empty()),
        })
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

    /// Resolve a user-supplied location into a concrete address string.
    /// Named keywords map to the configured env values; everything else is
    /// returned as-is (a literal address or "lat,lon") for geocoding.
    fn resolve(&self, input: &str) -> Result<String, ToolError> {
        let key = input.trim().to_lowercase();
        match key.as_str() {
            "home" | "house" => self
                .home
                .clone()
                .ok_or_else(|| ToolError::NotConfigured("COMMUTE_HOME not configured".into())),
            "work" | "office" | "the office" => self
                .work
                .clone()
                .ok_or_else(|| ToolError::NotConfigured("COMMUTE_WORK not configured".into())),
            "family" | "family home" | "parents" => self
                .family
                .clone()
                .ok_or_else(|| ToolError::NotConfigured("COMMUTE_FAMILY not configured".into())),
            // A bare 3-letter IATA code (e.g. "SJC") geocodes to the wrong place;
            // expand known airports to a full name+city before geocoding.
            _ => Ok(expand_iata(&key).unwrap_or_else(|| input.trim().to_string())),
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
        "Traffic-aware commute estimate for a typical day. Defaults to home→work; \
pass from/to as 'home', 'work'/'office', 'family', or any address. Use arrive_by \
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

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // Treat empty strings as "not provided" so the model can omit them and
        // still get the home→work default (it sometimes passes "" explicitly).
        let from_in = args["from"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("home");
        let to_in = args["to"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("work");
        let depart_at = args["depart_at"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("now");
        let arrive_by = args["arrive_by"].as_str().filter(|s| !s.is_empty());

        let from_addr = self.cfg.resolve(from_in)?;
        let to_addr = self.cfg.resolve(to_in)?;

        let client = CommuteConfig::client()?;
        let o = geocode(&client, &self.cfg.api_key, &from_addr).await?;
        let d = geocode(&client, &self.cfg.api_key, &to_addr).await?;
        let route = calc_route(&client, &self.cfg.api_key, &o, &d, depart_at, arrive_by, "car").await?;

        Ok(format_route(&label(from_in, &from_addr), &label(to_in, &to_addr), &route, arrive_by))
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
home/work/family. origin is OPTIONAL and defaults to the user's home — only destination is required. \
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
        // origin defaults to "home" when omitted/empty (resolved via COMMUTE_HOME);
        // destination remains required.
        let origin_in = args["origin"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("home");
        let dest_in = args["destination"].as_str().filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'destination' is required (address, 'lat,lon', or home/work/family)".into()))?;
        let depart_at = args["depart_at"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("now");
        let arrive_by = args["arrive_by"].as_str().filter(|s| !s.is_empty());
        let mode = match args["mode"].as_str().unwrap_or("car") {
            m @ ("car" | "truck" | "pedestrian" | "bicycle") => m,
            _ => "car",
        };

        let origin_addr = self.cfg.resolve(origin_in)?;
        let dest_addr = self.cfg.resolve(dest_in)?;

        let client = CommuteConfig::client()?;
        let o = geocode(&client, &self.cfg.api_key, &origin_addr).await?;
        let d = geocode(&client, &self.cfg.api_key, &dest_addr).await?;
        let route = calc_route(&client, &self.cfg.api_key, &o, &d, depart_at, arrive_by, mode).await?;

        let mut out = format_route(&label(origin_in, &origin_addr), &label(dest_in, &dest_addr), &route, arrive_by);
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
        let loc_in = args["location"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'location' is required".into()))?;
        let radius = args["radius_miles"].as_f64().unwrap_or(10.0).clamp(1.0, 50.0);

        let loc_addr = self.cfg.resolve(loc_in)?;
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

/// Pretty label: show the keyword and the resolved address when they differ.
fn label(input: &str, resolved: &str) -> String {
    let k = input.trim().to_lowercase();
    if matches!(k.as_str(), "home" | "work" | "office" | "family" | "family home" | "parents" | "house" | "the office") {
        format!("{} ({})", titlecase(&k), short_addr(resolved))
    } else {
        input.trim().to_string()
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

    fn cfg() -> CommuteConfig {
        CommuteConfig {
            api_key: "testkey".into(),
            home: Some("1065 S Van Ness Ave, San Francisco, CA 94110".into()),
            work: Some("1051 E Hillsdale Blvd, Foster City, CA 94404".into()),
            family: Some("6390 El Paseo Dr, San Jose, CA".into()),
        }
    }

    #[test]
    fn resolve_named_locations() {
        let c = cfg();
        assert!(c.resolve("home").unwrap().contains("Van Ness"));
        assert!(c.resolve("Work").unwrap().contains("Hillsdale"));
        assert!(c.resolve("the office").unwrap().contains("Hillsdale"));
        assert!(c.resolve("family home").unwrap().contains("El Paseo"));
    }

    #[test]
    fn resolve_literal_address_passthrough() {
        let c = cfg();
        assert_eq!(c.resolve("123 Main St, Reno NV").unwrap(), "123 Main St, Reno NV");
        assert_eq!(c.resolve("37.75,-122.41").unwrap(), "37.75,-122.41");
    }

    #[test]
    fn resolve_unconfigured_named_errors() {
        let c = CommuteConfig { api_key: "k".into(), home: None, work: None, family: None };
        assert!(matches!(c.resolve("home"), Err(ToolError::NotConfigured(_))));
    }

    #[test]
    fn resolve_iata_airport_codes() {
        let c = cfg();
        // Bare IATA codes expand to a full airport address for geocoding.
        assert_eq!(c.resolve("SJC").unwrap(), "San Jose International Airport, San Jose, CA");
        assert_eq!(c.resolve("tpa").unwrap(), "Tampa International Airport, Tampa, FL");
        // Unknown 3-letter strings and non-codes pass through unchanged.
        assert_eq!(c.resolve("ZZZ").unwrap(), "ZZZ");
        assert_eq!(c.resolve("Reno").unwrap(), "Reno");
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
        assert_eq!(urlencode("1065 S Van Ness"), "1065%20S%20Van%20Ness");
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

    #[test]
    fn label_shows_keyword_and_short_address() {
        let l = label("home", "1065 S Van Ness Ave, San Francisco, CA 94110");
        assert!(l.contains("Home"));
        assert!(l.contains("1065 S Van Ness Ave"));
        // literal address passes through unchanged
        assert_eq!(label("Reno NV", "Reno NV"), "Reno NV");
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
        let t = RouteTraffic { cfg: cfg() };
        assert!(matches!(
            t.execute(json!({"origin":"home"})).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn route_traffic_omitted_origin_defaults_to_home() {
        // With origin omitted, the tool must resolve "home" (via COMMUTE_HOME)
        // rather than erroring. We can't reach the network here, but we verify
        // the default selection by confirming home resolution is attempted: a
        // missing-origin call must NOT return InvalidArgument for origin.
        let t = RouteTraffic { cfg: cfg() };
        let r = t.execute(json!({"destination": "work"})).await;
        // It will fail at the geocode HTTP step (no network), not at arg
        // validation — i.e. origin defaulted successfully.
        match r {
            Err(ToolError::InvalidArgument(_)) => panic!("origin should have defaulted to home"),
            _ => {}
        }
    }

    #[test]
    #[serial]
    fn register_adds_four_tools() {
        let mut reg = ToolRegistry::new();
        let home = std::env::var("COMMUTE_HOME").ok();
        let key = std::env::var("TOMTOM_API_KEY").ok();
        std::env::set_var("TOMTOM_API_KEY", "testkey");
        std::env::set_var("COMMUTE_HOME", "test home");
        register(&mut reg);
        if let Some(k) = key { std::env::set_var("TOMTOM_API_KEY", k); } else { std::env::remove_var("TOMTOM_API_KEY"); }
        if let Some(h) = home { std::env::set_var("COMMUTE_HOME", h); } else { std::env::remove_var("COMMUTE_HOME"); }
        assert!(reg.contains("commute_estimate"));
        assert!(reg.contains("route_traffic"));
        assert!(reg.contains("traffic_incidents"));
        assert!(reg.contains("transit_plan"));
    }
}
