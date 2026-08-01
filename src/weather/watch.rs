//! WXLOC-04 — severe-weather WATCH: travel disruption and heat-wave power risk.
//!
//! One LLM-callable tool: `weather_severe_alerts`.
//!
//! # What this is for
//!
//! Not "what's the weather" — [`crate::weather`] already answers that. This
//! answers *"is something coming that I need to act on **before** it matters?"*,
//! for exactly two things the operator actually acts on:
//!
//! 1. **Storms that disrupt AIR TRAVEL.** The value is advance warning about a
//!    trip the operator ACTUALLY HAS, so this keys off real upcoming calendar
//!    travel and checks the weather AT THE DESTINATION ON THE TRAVEL DAY. A
//!    generic "storms somewhere this week" is noise; "the airport you fly into
//!    on Thursday has a gale and heavy snow" is a decision.
//! 2. **Heat waves, as a HOME POWER / HVAC risk.** The concern is concrete: the
//!    apartment can lose power under sustained heat load. So the trigger is
//!    SUSTAINED heat at the home location — days that stay hot *and nights that
//!    do not cool down*, because that is what keeps the AC at high duty cycle —
//!    and the framing is power, not comfort. "It will be 31° on Saturday" is not
//!    this.
//!
//! # PULL, not push — and why (a deliberate scope decision, WXLOC-04)
//!
//! The item is titled "watch", which implies a proactive notification. Terminus
//! has **no sanctioned proactive delivery path to a human**, and this module
//! does not invent one. Surveyed before building:
//!
//! - There is no Matrix client in this crate. The only Matrix reference is a
//!   reachability probe in `crate::sysversion`; `crate::reminder`'s own module
//!   doc records that **lumina-core holds the Matrix connection**.
//! - The only user-facing *timed* feature this crate owns, `reminder`, is
//!   explicitly built as **poll-from-outside**: state in Postgres plus
//!   `reminder_poll`, which lumina-core calls and then delivers. That is the
//!   established boundary, not an accident.
//! - `synapse_*`, `vigil_*` and `routines_*` are SSH control surfaces for
//!   processes on other hosts; none of them accepts a message from here.
//! - The only egress that could reach a person at all is `google_email_send`, a
//!   model-invoked tool with no scheduling, addressing or quiet-hours concept.
//! - The in-process loops that exist (`compiler::scheduler`, the mesh health
//!   sweep, the MINT supervisor) are ops machinery, config-gated, and produce
//!   no user-facing output. `intake::breakfix`'s "escalate" writes a log line.
//!
//! So this ships the **assessment core as a pull-mode tool**. A correct thing
//! Lumina can call — on a turn, or from a schedule that lives where scheduling
//! already lives — beats a half-built push path that fails silently because its
//! channel was never configured. Wiring a proactive knock is real work in
//! lumina-core (which owns Matrix *and* the assistant's presence budget), not a
//! bolt-on here. **Nothing in this module claims to have been delivered.**
//!
//! # Entitlement (non-negotiable — same gate as location inference)
//!
//! This reads the OPERATOR's calendar and the OPERATOR's home location. Terminus
//! is a multi-principal gateway, so both are gated on
//! [`CallerContext`](crate::tool::CallerContext), exactly as
//! [`crate::weather::location`] gates them, and INDEPENDENTLY: the travel watch
//! needs `may_infer_from_calendar` (the gateway grants it only to a caller
//! already entitled to `google_calendar_today`), the heat watch needs
//! `may_infer_from_routine` (`commute_estimate`).
//!
//! An unentitled caller — a guest, an unknown principal, an absent principal, or
//! the un-threaded [`RustTool::execute`] path — gets **nothing**, and, more
//! strongly, **causes no read of either source**: not a fetch that is discarded,
//! but no fetch at all, so the operator's whereabouts are never in this
//! process's memory on a guest's behalf. Asserted by tests that COUNT reads.
//!
//! A guest must never be able to learn that the operator is travelling, where
//! they are going, what the appointment is, or where they live.
//!
//! # Never cached
//!
//! A stale severe-weather answer is worse than none. The tool name contains both
//! `severe` and `alert`, so `crate::tool_cache`'s never-cached rule excludes it —
//! verified by a test in this module against the real `policy_for`, not assumed.
//! (Note the trap that test exists to catch: the name also starts with
//! `weather`, which HAS a 20-minute cache policy in `SEED_POLICY`. It is only
//! the never-cached rule, checked first, that saves it. Rename this tool to
//! something like `weather_watch` and it silently becomes cacheable.)
//!
//! # Derived, not official — see [`Provenance`]
//!
//! The OpenWeather endpoints this integration uses — `/data/2.5/weather` and
//! `/data/2.5/forecast` — return **no `alerts` array**. Government alerts
//! (NWS/met-office) come only from One Call 3.0 (`/data/3.0/onecall`), which is
//! a separately-subscribed product; 2.5's One Call was retired. So by default
//! every finding here is DERIVED from forecast numbers using the documented
//! thresholds below, and says so in the output. When One Call is subscribed and
//! `OPENWEATHER_ONECALL_ALERTS` is enabled, official alerts are fetched and
//! reported FIRST and labelled official — an authoritative alert beats any
//! threshold this module invents.
//!
//! # Where WXLOC-01 plugs in
//!
//! [`WatchLocations`] is the seam, and LOCREG-01 filled it: the production
//! implementation is [`ResolvedLocations`], the calling identity's own record in
//! the shared location registry. The watch asks "what is home for THIS request?"
//! and nothing in this module reads an env var. The `COMMUTE_HOME` fallback that
//! used to sit behind this seam was REMOVED in round 4 — a shared service
//! principal (TERM #577) cannot distinguish people, so it handed the operator's
//! address to everyone entitled. "No home location configured" is therefore a
//! first-class, tested outcome rather than an afterthought.

use async_trait::async_trait;
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::ToolError;
use crate::tool::{CallerContext, RustTool};
use crate::weather::location::{location_from_event, CalendarSource, CalendarWindow, Routine};

// ── Thresholds ───────────────────────────────────────────────────────────────
//
// Every number here is a judgement call, so every number here is justified. The
// governing rule is DO NOT CRY WOLF: a plain warm summer day is not a heat wave
// and light rain is not travel disruption. A watch that fires on ordinary
// weather trains the operator to ignore it, which is worse than not shipping it.

/// Sustained wind at which air travel actually degrades: 17.2 m/s ≈ 38 mph ≈
/// 62 km/h, i.e. Beaufort force 8 (gale).
///
/// Chosen because it is the point where crosswind limits, ground-handling stops
/// and holding start to bite in practice, not because it is a round number. A
/// breezy 10 m/s day delays nothing.
const TRAVEL_WIND_MS: f64 = 17.2;

/// Heavy rain over one 3-hour forecast step (mm). 10 mm/3h is "heavy rain" on
/// the usual rate scale; it is the level associated with low visibility and
/// standing water on surfaces, not with an inconvenient commute.
const TRAVEL_RAIN_3H_MM: f64 = 10.0;

/// Frozen precipitation over one 3-hour step (mm water-equivalent).
///
/// Deliberately an order of magnitude below the rain threshold: snow is
/// disproportionately disruptive to aviation (de-icing queues, runway clearing,
/// reduced arrival rates) at accumulations that would be trivial as rain. 2.5 mm
/// w.e. is roughly 2–3 cm of snow.
const TRAVEL_SNOW_3H_MM: f64 = 2.5;

/// At or below this (°C) precipitation may be freezing — the icing case.
const FREEZING_C: f64 = 0.0;

/// Daily HIGH at or above which residential cooling load is high: 32.2 °C = 90 °F.
///
/// 90 °F is the conventional US heat-advisory neighbourhood and is roughly where
/// a residential AC stops cycling and starts running.
const HEAT_DAY_MAX_C: f64 = 32.2;

/// Overnight LOW at or above which the building never sheds its heat: 21.1 °C = 70 °F.
///
/// **This is the load-bearing half of the heat test, and the reason it is about
/// power rather than temperature.** A 34 °C afternoon followed by a 15 °C night
/// is not a grid or HVAC problem — the apartment cools, the AC rests, the
/// thermal mass resets. Heat becomes a power problem when the nights stop
/// helping: the AC runs near-continuously, load stacks across the neighbourhood,
/// and that is when supply fails. Requiring BOTH a hot day and a warm night is
/// what separates "a hot spell" from "a sustained load event", and is why an
/// ordinary hot day does not fire this.
const HEAT_NIGHT_MIN_C: f64 = 21.1;

/// Consecutive qualifying days before this is a heat WAVE rather than a hot day.
/// Two days is the minimum that can be called "sustained"; one day is weather.
const HEAT_RUN_DAYS: usize = 2;

/// Run length at which the heat finding escalates to severe.
const HEAT_SEVERE_RUN_DAYS: usize = 3;

/// Peak at which a single day escalates the finding regardless of run length:
/// 37.8 °C = 100 °F.
const HEAT_SEVERE_MAX_C: f64 = 37.8;

/// Longest forward window this tool will assess, bounded by what the free
/// 5-day/3-hour forecast product actually covers (~6 distinct calendar days).
/// Asking for more would silently return less; the report states the horizon it
/// really covered.
pub const MAX_HORIZON_DAYS: usize = 6;

/// Default horizon: far enough ahead to change a decision, near enough that the
/// forecast is worth acting on.
pub const DEFAULT_HORIZON_DAYS: usize = 3;

// ── Weather primitives ───────────────────────────────────────────────────────

/// One day of forecast, reduced to just what a watch needs to judge severity.
///
/// Built from raw OpenWeather 3-hour forecast points by [`day_weather`] rather
/// than reusing `crate::weather`'s `DaySummary`, which carries presentation
/// fields (dual-unit strings, "what to wear") and drops the two things severity
/// actually turns on: wind and the numeric condition ids.
#[derive(Debug, Clone, PartialEq)]
pub struct DayWeather {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub temp_min_c: f64,
    pub temp_max_c: f64,
    /// Max of `wind.speed` and `wind.gust` across the day (m/s). Gust matters
    /// more than mean wind for whether an aircraft can land.
    pub max_wind_ms: f64,
    /// Largest single-step rain volume (mm), i.e. peak RATE, not daily total —
    /// 12 mm in three hours is a squall; 12 mm spread over a day is drizzle.
    pub max_rain_3h_mm: f64,
    /// Largest single-step snow volume (mm water-equivalent), same reasoning.
    pub max_snow_3h_mm: f64,
    /// OpenWeather numeric condition ids seen during the day.
    pub condition_ids: Vec<u16>,
    /// Most frequent textual description, for the human-readable line.
    pub description: String,
}

/// Reduce OpenWeather forecast points for ONE date into a [`DayWeather`].
/// Returns `None` when the points carry no usable temperature.
pub fn day_weather(date: &str, points: &[&Value]) -> Option<DayWeather> {
    let mut temp_min = f64::INFINITY;
    let mut temp_max = f64::NEG_INFINITY;
    let mut max_wind = 0.0f64;
    let mut max_rain = 0.0f64;
    let mut max_snow = 0.0f64;
    let mut ids: Vec<u16> = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for p in points {
        if let Some(main) = p.get("main") {
            for key in ["temp", "temp_min", "temp_max"] {
                if let Some(t) = main.get(key).and_then(Value::as_f64) {
                    temp_min = temp_min.min(t);
                    temp_max = temp_max.max(t);
                }
            }
        }
        if let Some(w) = p.get("wind") {
            for key in ["speed", "gust"] {
                if let Some(v) = w.get(key).and_then(Value::as_f64) {
                    max_wind = max_wind.max(v);
                }
            }
        }
        max_rain = max_rain.max(volume_3h(p.get("rain")));
        max_snow = max_snow.max(volume_3h(p.get("snow")));
        for w in p.get("weather").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = w.get("id").and_then(Value::as_u64) {
                let id = id as u16;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if let Some(d) = w.get("description").and_then(Value::as_str) {
                *counts.entry(d.to_string()).or_insert(0) += 1;
            }
        }
    }

    if !temp_min.is_finite() || !temp_max.is_finite() {
        return None;
    }
    Some(DayWeather {
        date: date.to_string(),
        temp_min_c: temp_min,
        temp_max_c: temp_max,
        max_wind_ms: max_wind,
        max_rain_3h_mm: max_rain,
        max_snow_3h_mm: max_snow,
        condition_ids: ids,
        description: counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(d, _)| d)
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

fn volume_3h(obj: Option<&Value>) -> f64 {
    obj.and_then(|o| {
        o.get("3h")
            .and_then(Value::as_f64)
            .or_else(|| o.get("1h").and_then(Value::as_f64))
    })
    .unwrap_or(0.0)
}

/// Group a raw `/data/2.5/forecast` `list` into per-day [`DayWeather`],
/// chronological.
pub fn days_from_forecast(list: &[Value]) -> Vec<DayWeather> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for p in list {
        let Some(d) = p
            .get("dt_txt")
            .and_then(Value::as_str)
            .and_then(|s| s.split_whitespace().next())
            .map(str::to_string)
        else {
            continue;
        };
        if !groups.contains_key(&d) {
            order.push(d.clone());
        }
        groups.entry(d).or_default().push(p);
    }
    order
        .into_iter()
        .filter_map(|d| {
            let pts = groups.remove(&d).unwrap_or_default();
            day_weather(&d, &pts)
        })
        .collect()
}

// ── Hazards and severity ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Expect delays. Worth knowing about a flight; not worth waking anyone.
    Disruptive,
    /// Expect cancellations / a real safety or supply impact.
    Severe,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Disruptive => "disruptive",
            Severity::Severe => "severe",
        }
    }
}

/// A single reason a day is disruptive, already phrased for a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hazard {
    pub text: String,
    pub severity: Severity,
}

impl Hazard {
    fn new(severity: Severity, text: impl Into<String>) -> Self {
        Self { text: text.into(), severity }
    }
}

/// Judge one day AT A TRAVEL DESTINATION. Empty ⇒ nothing disruptive; that is a
/// real "checked, clear", distinct from "could not check".
///
/// Ordinary weather returns nothing on purpose. Light rain, a breeze and an
/// overcast sky are not travel disruption.
pub fn travel_hazards(d: &DayWeather) -> Vec<Hazard> {
    let mut out = Vec::new();
    let has = |id: u16| d.condition_ids.contains(&id);
    let group = |g: u16| d.condition_ids.iter().any(|i| i / 100 == g);

    if has(781) {
        out.push(Hazard::new(Severity::Severe, "tornado in the forecast"));
    }
    if has(771) {
        out.push(Hazard::new(Severity::Severe, "squalls"));
    }
    if has(511) {
        out.push(Hazard::new(Severity::Severe, "freezing rain (icing)"));
    }
    if d.max_wind_ms >= TRAVEL_WIND_MS {
        out.push(Hazard::new(
            Severity::Severe,
            format!(
                "gale-force wind, gusting {:.0} km/h ({:.0} mph)",
                d.max_wind_ms * 3.6,
                d.max_wind_ms * 2.237
            ),
        ));
    }
    if d.max_snow_3h_mm >= TRAVEL_SNOW_3H_MM {
        // Heavy snow ids escalate; any qualifying snow is at least disruptive.
        let sev = if has(602) || has(622) { Severity::Severe } else { Severity::Disruptive };
        out.push(Hazard::new(
            sev,
            format!("snow ({:.1} mm/3h water-equivalent)", d.max_snow_3h_mm),
        ));
    }
    // Icing: precipitation at or below freezing, even if the volume is modest.
    if d.temp_min_c <= FREEZING_C && (d.max_rain_3h_mm > 0.0 || d.max_snow_3h_mm > 0.0) && !has(511)
    {
        out.push(Hazard::new(
            Severity::Disruptive,
            "precipitation at or below freezing — de-icing likely",
        ));
    }
    if group(2) {
        // Convective weather is the single largest cause of air-traffic delay.
        out.push(Hazard::new(Severity::Disruptive, "thunderstorms"));
    }
    if d.max_rain_3h_mm >= TRAVEL_RAIN_3H_MM {
        out.push(Hazard::new(
            Severity::Disruptive,
            format!("heavy rain ({:.1} mm/3h)", d.max_rain_3h_mm),
        ));
    }
    out
}

// ── Travel ───────────────────────────────────────────────────────────────────

/// What kind of trip a calendar event is. Flights are called out because the
/// operator is a frequent flyer and a flight is the case where advance warning
/// changes a decision (rebook) rather than just a mood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TravelKind {
    Flight,
    Trip,
}

impl TravelKind {
    fn noun(self) -> &'static str {
        match self {
            TravelKind::Flight => "flight",
            TravelKind::Trip => "trip",
        }
    }
}

/// Markers that make a calendar event flight-shaped. Deliberately conservative:
/// a miss downgrades a flight to a "trip" (still watched, still reported), while
/// a false positive would tell the operator a dentist appointment is a flight.
const FLIGHT_MARKERS: &[&str] = &[
    "flight", "flying", "fly to", "airport", "airline", "boarding", "departure",
    "depart ", "landing", "layover", "terminal ", "nonstop", "red-eye", "redeye",
];

/// One piece of real upcoming travel, extracted from the calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelPlan {
    pub summary: String,
    pub destination: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub kind: TravelKind,
}

/// Extract travel from calendar events within `[start, end]`.
///
/// The filtering is what makes this "real upcoming travel, not a generic
/// forecast": an event must have a REAL place (`location_from_event` already
/// rejects Zoom/Teams/TBD/phone "locations"), must not be cancelled or declined,
/// and must fall inside the horizon. Everything else is ignored.
pub fn travel_plans(events: &[Value], start: NaiveDate, end: NaiveDate) -> Vec<TravelPlan> {
    let mut out: Vec<TravelPlan> = Vec::new();
    for ev in events {
        let status = ev.get("status").and_then(Value::as_str).unwrap_or("");
        if status.eq_ignore_ascii_case("cancelled") || status.eq_ignore_ascii_case("declined") {
            continue;
        }
        let Some(destination) = location_from_event(ev) else {
            continue;
        };
        let Some(date) = event_date(ev) else { continue };
        if date < start || date > end {
            continue;
        }
        let summary = ev
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("an event")
            .to_string();
        let hay = format!("{} {}", summary, destination).to_lowercase();
        let kind = if FLIGHT_MARKERS.iter().any(|m| hay.contains(m)) {
            TravelKind::Flight
        } else {
            TravelKind::Trip
        };
        let date_s = date.format("%Y-%m-%d").to_string();
        // One check per (destination, date): two meetings in one city on one day
        // are one weather question.
        if out.iter().any(|p| p.destination == destination && p.date == date_s) {
            continue;
        }
        out.push(TravelPlan { summary, destination, date: date_s, kind });
    }
    out
}

/// Parse the `dtstart` iCal stamp (`YYYYMMDD` or `YYYYMMDDTHHMMSS[Z]`) to a date.
fn event_date(ev: &Value) -> Option<NaiveDate> {
    let s = ev.get("dtstart").and_then(Value::as_str)?;
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 8 {
        return None;
    }
    NaiveDate::parse_from_str(&digits[..8], "%Y%m%d").ok()
}

// ── Heat ─────────────────────────────────────────────────────────────────────

/// A run of consecutive days that each meet BOTH the hot-day and warm-night
/// tests — the thing that actually stresses power and HVAC.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatRun {
    pub days: Vec<DayWeather>,
    pub severity: Severity,
}

impl HeatRun {
    pub fn peak_c(&self) -> f64 {
        self.days.iter().fold(f64::NEG_INFINITY, |a, d| a.max(d.temp_max_c))
    }
    pub fn warmest_night_c(&self) -> f64 {
        self.days.iter().fold(f64::NEG_INFINITY, |a, d| a.max(d.temp_min_c))
    }
}

fn qualifies_as_heat_day(d: &DayWeather) -> bool {
    d.temp_max_c >= HEAT_DAY_MAX_C && d.temp_min_c >= HEAT_NIGHT_MIN_C
}

/// Longest run of qualifying CALENDAR-CONSECUTIVE days, if it reaches
/// [`HEAT_RUN_DAYS`].
///
/// **Adjacency is verified against the dates, not assumed from the slice order.**
/// It is tempting to rely on [`days_from_forecast`] producing a
/// contiguous chronological list — but the provider can omit a date, and then two
/// qualifying entries that are days apart would sit next to each other in the
/// slice and be reported as a "sustained" run. That is a false heat-wave warning
/// built out of a gap in the data, which is precisely the crying-wolf failure the
/// thresholds exist to avoid. A day whose date cannot be parsed also breaks the
/// run rather than extending it — fail safe, since "sustained" is the entire
/// claim being made.
pub fn heat_run(days: &[DayWeather]) -> Option<HeatRun> {
    let mut best: Vec<DayWeather> = Vec::new();
    let mut cur: Vec<DayWeather> = Vec::new();
    let mut prev_date: Option<NaiveDate> = None;
    for d in days {
        let parsed = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok();
        let adjacent = match (prev_date, parsed) {
            (Some(p), Some(c)) => c == p + chrono::Duration::days(1),
            // No previous day yet: a run may start here.
            (None, Some(_)) => true,
            // Unparseable date: cannot prove adjacency, so do not claim it.
            _ => false,
        };
        if qualifies_as_heat_day(d) && parsed.is_some() {
            if !adjacent {
                cur.clear();
            }
            cur.push(d.clone());
            if cur.len() > best.len() {
                best = cur.clone();
            }
        } else {
            cur.clear();
        }
        prev_date = parsed;
    }
    if best.len() < HEAT_RUN_DAYS {
        return None;
    }
    let peak = best.iter().fold(f64::NEG_INFINITY, |a, d| a.max(d.temp_max_c));
    let severity = if best.len() >= HEAT_SEVERE_RUN_DAYS || peak >= HEAT_SEVERE_MAX_C {
        Severity::Severe
    } else {
        Severity::Disruptive
    };
    Some(HeatRun { days: best, severity })
}

// ── Report ───────────────────────────────────────────────────────────────────

/// Why something could not be assessed. Every variant is a reason the answer is
/// "I did not check", never "there is nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// The caller is not entitled to this source of operator context.
    /// Rendered without naming what could not be read (see [`WatchReport::render`]).
    NotEntitled,
    /// The source is not configured at all (no calendar; no home location).
    NotConfigured(String),
    /// Configured, but the read or the forecast failed.
    Failed(String),
}

/// Where a finding came from. An authoritative alert outranks anything derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A government/meteorological-agency alert relayed by the provider.
    Official,
    /// Computed here from forecast numbers using this module's thresholds.
    Derived,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TravelFinding {
    pub plan: TravelPlan,
    pub hazards: Vec<Hazard>,
    pub provenance: Provenance,
    /// Official alert headlines for the destination, when available.
    pub official: Vec<String>,
}

impl TravelFinding {
    pub fn severity(&self) -> Severity {
        self.hazards.iter().map(|h| h.severity).max().unwrap_or(Severity::Disruptive)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeatFinding {
    pub location: String,
    /// The derived sustained-load run, when the thresholds found one.
    ///
    /// `None` means this finding exists ONLY because an official alert is in
    /// force. An authoritative alert outranks anything derived, so it must be
    /// reported even when this module's own thresholds did not independently
    /// fire — otherwise the tool would quietly suppress a met-office heat
    /// warning because its own numbers disagreed. (The travel path always
    /// behaved this way; heat did not, which is the inconsistency this
    /// `Option` fixes.)
    pub run: Option<HeatRun>,
    pub provenance: Provenance,
    pub official: Vec<String>,
}

/// The result of one subject's assessment.
///
/// The three fields are kept separate rather than collapsed into an enum because
/// a partial answer is normal and must stay legible: two destinations checked,
/// one unreachable, one flagged. `checked` is what makes an empty `findings`
/// mean "clear" — with `checked` empty and no gaps, nothing happened at all.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectReport<F> {
    pub findings: Vec<F>,
    /// Human labels of what WAS successfully assessed.
    pub checked: Vec<String>,
    pub gaps: Vec<Gap>,
}

/// Hand-written rather than derived: `#[derive(Default)]` on a generic struct
/// adds an `F: Default` bound, which the finding types deliberately do not
/// satisfy (a "default TravelFinding" is meaningless). The empty report does not
/// need one.
impl<F> Default for SubjectReport<F> {
    fn default() -> Self {
        Self { findings: Vec::new(), checked: Vec::new(), gaps: Vec::new() }
    }
}

impl<F> SubjectReport<F> {
    fn gap(g: Gap) -> Self {
        Self { findings: Vec::new(), checked: Vec::new(), gaps: vec![g] }
    }
    /// Checked something, found nothing severe. The ONLY basis for an all-clear.
    pub fn is_clear(&self) -> bool {
        self.findings.is_empty() && self.gaps.is_empty() && !self.checked.is_empty()
    }
    pub fn checked_nothing(&self) -> bool {
        self.checked.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchReport {
    /// Days actually covered — may be less than requested if the forecast is short.
    pub horizon_days: usize,
    pub travel: SubjectReport<TravelFinding>,
    pub heat: SubjectReport<HeatFinding>,
}

impl WatchReport {
    pub fn has_findings(&self) -> bool {
        !self.travel.findings.is_empty() || !self.heat.findings.is_empty()
    }

    /// Did anything at all get assessed? False ⇒ the answer must not read as an
    /// all-clear under any formatting.
    pub fn checked_anything(&self) -> bool {
        !self.travel.checked_nothing() || !self.heat.checked_nothing()
    }

    /// Render for a human.
    ///
    /// Three rules this must never break:
    /// 1. "checked, nothing severe" and "could not check" are visibly different.
    /// 2. Nothing is asserted about a source that was not read — a
    ///    [`Gap::NotEntitled`] renders as a flat "not available to you", naming
    ///    neither the calendar nor a home location, because *"I can't tell you
    ///    about the operator's travel"* is itself a disclosure that there is
    ///    travel to tell about.
    /// 3. A derived finding says it is derived.
    pub fn render(&self) -> String {
        let mut out = String::new();

        // Entitlement first: if BOTH subjects are barred, say one flat thing and
        // stop. No horizon, no structure, nothing to infer from.
        let barred = |s: &[Gap]| s.len() == 1 && s[0] == Gap::NotEntitled;
        if barred(&self.travel.gaps)
            && barred(&self.heat.gaps)
            && !self.checked_anything()
        {
            return "Severe-weather watch is not available to you. Ask about a \
                    specific place and I'll check the forecast there."
                .to_string();
        }

        out.push_str(&format!(
            "Severe-weather watch — next {} day{}.\n",
            self.horizon_days,
            if self.horizon_days == 1 { "" } else { "s" }
        ));

        // ── Travel ──
        out.push_str("\nTravel:\n");
        // The "checked" line is printed whenever something WAS checked and came
        // back clean, even alongside a gap — it is scoped to the named items, so
        // it cannot be read as a blanket all-clear, and the gap lines follow it.
        // Suppressing it whenever any gap existed would throw away the honest
        // half of a partial answer.
        if self.travel.findings.is_empty() {
            if self.travel.checked_nothing() {
                if self.travel.gaps.is_empty() {
                    out.push_str("- Nothing checked.\n");
                }
            } else {
                out.push_str(&format!(
                    "- Checked {}: nothing disruptive expected.\n",
                    self.travel.checked.join("; ")
                ));
            }
        }
        for f in &self.travel.findings {
            out.push_str(&format!(
                "- {} {} on {} — {} at {}. Your {} \"{}\" is at risk: {}.\n",
                severity_marker(f.severity()),
                f.severity().label().to_uppercase(),
                f.plan.date,
                f.hazards.iter().map(|h| h.text.as_str()).collect::<Vec<_>>().join(", "),
                f.plan.destination,
                f.plan.kind.noun(),
                f.plan.summary,
                why_travel_matters(f.plan.kind, f.severity()),
            ));
            for a in &f.official {
                out.push_str(&format!("    Official alert: {a}\n"));
            }
        }
        if !self.travel.findings.is_empty() && !self.travel.checked.is_empty() {
            out.push_str(&format!("- Also checked: {}.\n", self.travel.checked.join("; ")));
        }
        for g in &self.travel.gaps {
            out.push_str(&format!("- {}\n", render_gap(g)));
        }

        // ── Heat / home power ──
        out.push_str("\nHome heat load:\n");
        if self.heat.findings.is_empty() {
            if self.heat.checked_nothing() {
                if self.heat.gaps.is_empty() {
                    out.push_str("- Nothing checked.\n");
                }
            } else {
                out.push_str(&format!(
                    "- Checked {}: no sustained heat build-up.\n",
                    self.heat.checked.join("; ")
                ));
            }
        }
        for f in &self.heat.findings {
            let Some(r) = &f.run else {
                // Official-alert-only: the met service has issued a heat warning
                // that this module's own thresholds did not independently
                // reproduce. Report it plainly; do not manufacture derived
                // numbers to justify it.
                out.push_str(
                    "- [!!] SEVERE — an official heat alert is in force for your home area. \
                     Treat it as a power and cooling-load risk: pre-cool, ease other load, \
                     and have a plan for an outage.\n",
                );
                for a in &f.official {
                    out.push_str(&format!("    Official alert: {a}\n"));
                }
                continue;
            };
            out.push_str(&format!(
                "- {} {} — {} consecutive days at or above {:.0}°C/{:.0}°F with nights \
                 staying at or above {:.0}°C/{:.0}°F ({} to {}, peaking {:.0}°C/{:.0}°F, \
                 warmest night {:.0}°C/{:.0}°F). That is sustained cooling load, not just \
                 a hot spell: the flat never sheds its heat overnight, so the AC runs \
                 near-continuously and the supply is what gives out. Worth pre-cooling, \
                 easing other load, and having a plan for an outage.\n",
                severity_marker(r.severity),
                r.severity.label().to_uppercase(),
                r.days.len(),
                HEAT_DAY_MAX_C,
                c_to_f(HEAT_DAY_MAX_C),
                HEAT_NIGHT_MIN_C,
                c_to_f(HEAT_NIGHT_MIN_C),
                r.days.first().map(|d| d.date.as_str()).unwrap_or("?"),
                r.days.last().map(|d| d.date.as_str()).unwrap_or("?"),
                r.peak_c(),
                c_to_f(r.peak_c()),
                r.warmest_night_c(),
                c_to_f(r.warmest_night_c()),
            ));
            for a in &f.official {
                out.push_str(&format!("    Official alert: {a}\n"));
            }
        }
        for g in &self.heat.gaps {
            out.push_str(&format!("- {}\n", render_gap(g)));
        }

        // Provenance. Only claim "derived" when something was actually derived.
        if self.has_findings() && self.any_derived() {
            out.push_str(
                "\nThese are DERIVED from forecast data using documented thresholds — \
                 not official meteorological alerts. Check the local met service before \
                 acting on anything time-critical.\n",
            );
        }
        if !self.checked_anything() {
            out.push_str(
                "\nNothing above was actually checked — treat this as \"unknown\", \
                 not \"all clear\".\n",
            );
        }
        out
    }

    fn any_derived(&self) -> bool {
        self.travel.findings.iter().any(|f| f.provenance == Provenance::Derived)
            || self.heat.findings.iter().any(|f| f.provenance == Provenance::Derived)
    }
}

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

fn severity_marker(s: Severity) -> &'static str {
    match s {
        Severity::Severe => "[!!]",
        Severity::Disruptive => "[!]",
    }
}

fn why_travel_matters(kind: TravelKind, sev: Severity) -> &'static str {
    match (kind, sev) {
        (TravelKind::Flight, Severity::Severe) => {
            "cancellations and ground stops are likely — check the airline now and \
             consider rebooking while there are still seats"
        }
        (TravelKind::Flight, Severity::Disruptive) => {
            "expect delays; leave slack for a missed connection"
        }
        (TravelKind::Trip, Severity::Severe) => "expect roads and transport to be badly affected",
        (TravelKind::Trip, Severity::Disruptive) => "allow extra travel time",
    }
}

fn render_gap(g: &Gap) -> String {
    match g {
        // Says nothing about WHAT could not be read. See `render`'s rule 2.
        Gap::NotEntitled => "Not available to you.".to_string(),
        Gap::NotConfigured(what) => {
            format!("Could not check — {what}. This is unknown, not clear.")
        }
        Gap::Failed(why) => format!("Could not check — {why}. This is unknown, not clear."),
    }
}

// ── Seams ────────────────────────────────────────────────────────────────────

/// Which places this watch cares about, beyond wherever the calendar says the
/// operator will be.
///
/// **This is the WXLOC-01 plug point**, and LOCREG-01 filled it: the production
/// implementation is [`ResolvedLocations`], a [`Routine`] resolved from THIS
/// CALLER's record in the shared location registry. `None` here is the normal
/// case for a caller who has saved nothing, not an edge case, and it must
/// produce "no home location is configured", never a guessed city.
pub trait WatchLocations: Send + Sync {
    /// The place whose power/HVAC risk matters. `None` ⇒ not configured.
    fn home(&self) -> Option<String>;

    /// `true` when the location source EXISTS but could not be read.
    ///
    /// Without this, an unreadable registry and an empty one both arrive as
    /// `home() == None` and get rendered as "no home location is configured" —
    /// the same absence/failure collapse `crate::locations` refuses to make.
    /// Defaults to `false` so a source that cannot fail (a fixture, a plain
    /// `Routine`) says nothing about it.
    fn unavailable(&self) -> bool {
        false
    }
}

impl WatchLocations for Routine {
    fn home(&self) -> Option<String> {
        self.home.clone()
    }
}

/// A [`Routine`] resolved from the shared registry for ONE caller, carrying
/// whether that read succeeded.
///
/// LOCREG-01 + the un-keyed-path fix: the watch used to read
/// `WeatherConfig::routine` — the process-global `COMMUTE_*` pair holding the
/// OPERATOR's addresses — for anyone entitled to the routine. This is the
/// per-caller replacement, and a caller with no identity gets an EMPTY one.
pub(crate) struct ResolvedLocations {
    routine: Routine,
    degraded: bool,
}

impl WatchLocations for ResolvedLocations {
    fn home(&self) -> Option<String> {
        self.routine.home.clone()
    }

    fn unavailable(&self) -> bool {
        self.degraded
    }
}

/// Multi-day forecast (and, where available, official alerts) for a named place.
///
/// A trait so the assessment core can be driven from fixtures — including a
/// FAILING source, which is how "could not check" is tested rather than asserted.
#[async_trait]
pub trait ForecastSource: Send + Sync {
    /// Chronological daily forecast. `Err` ⇒ could not check (never "clear").
    async fn daily(&self, place: &str) -> Result<Vec<DayWeather>, String>;

    /// Official meteorological alerts for `place`.
    ///
    /// `Ok(None)` means NO OFFICIAL FEED IS AVAILABLE — the default, because the
    /// `/data/2.5/*` endpoints this integration uses return no `alerts` array at
    /// all. It does NOT mean "no alerts are in force"; a caller must not read it
    /// as an all-clear.
    async fn official_alerts(&self, place: &str) -> Result<Option<Vec<String>>, String> {
        let _ = place;
        Ok(None)
    }
}

// ── The assessment core ──────────────────────────────────────────────────────

/// Run the watch.
///
/// All I/O is behind the three trait objects, so this whole function is
/// exercised by the tests below with no network.
///
/// Entitlement is checked BEFORE the corresponding source is touched, and the
/// two subjects are gated independently — a caller entitled to the routine but
/// not the calendar gets the heat watch and nothing about travel.
pub async fn run_watch(
    requested_days: usize,
    caller: CallerContext,
    calendar: &dyn CalendarSource,
    locations: &dyn WatchLocations,
    forecast: &dyn ForecastSource,
    today: NaiveDate,
) -> WatchReport {
    let horizon = requested_days.clamp(1, MAX_HORIZON_DAYS);
    let end = today + chrono::Duration::days(horizon as i64 - 1);

    let travel = assess_travel(caller, calendar, forecast, today, end).await;
    // The horizon binds BOTH subjects. Passing it only to travel would let the
    // heat watch report a run that falls outside the "next N days" the report
    // says it covered — an answer that contradicts its own header.
    let heat = assess_heat(caller, locations, forecast, today, end).await;

    WatchReport { horizon_days: horizon, travel, heat }
}

async fn assess_travel(
    caller: CallerContext,
    calendar: &dyn CalendarSource,
    forecast: &dyn ForecastSource,
    start: NaiveDate,
    end: NaiveDate,
) -> SubjectReport<TravelFinding> {
    // GATE FIRST. Not "fetch then discard" — the operator's calendar must not be
    // read on an unentitled caller's behalf at all.
    if !caller.may_infer_from_calendar() {
        return SubjectReport::gap(Gap::NotEntitled);
    }

    let events = match calendar.events_between(start, end).await {
        CalendarWindow::Events(e) => e,
        CalendarWindow::Unavailable(why) => {
            return SubjectReport::gap(Gap::Failed(format!("your calendar could not be read ({why})")))
        }
        CalendarWindow::NotConfigured => {
            return SubjectReport::gap(Gap::NotConfigured("no calendar is connected".into()))
        }
    };

    let plans = travel_plans(&events, start, end);
    if plans.is_empty() {
        // A genuine, positive "checked": the calendar answered and holds no travel.
        return SubjectReport {
            findings: Vec::new(),
            checked: vec!["your calendar (no trips scheduled)".to_string()],
            gaps: Vec::new(),
        };
    }

    let mut report = SubjectReport::default();
    for plan in plans {
        let label = format!("{} on {}", plan.destination, plan.date);
        let days = match forecast.daily(&plan.destination).await {
            Ok(d) => d,
            Err(e) => {
                report.gaps.push(Gap::Failed(format!("no forecast for {label} ({e})")));
                continue;
            }
        };
        let Some(day) = days.iter().find(|d| d.date == plan.date) else {
            report.gaps.push(Gap::Failed(format!(
                "the forecast does not reach {label} yet"
            )));
            continue;
        };

        // An ENABLED official feed that FAILS is a gap, not "no alerts in
        // force" — discarding the error here would let an unavailable
        // authoritative source render as a clean check, which is the exact
        // dishonest-degradation this module exists to avoid. (`Ok(None)` is
        // different: it means no feed is configured, and the derived thresholds
        // are then the whole answer, already labelled as derived.)
        let official = match forecast.official_alerts(&plan.destination).await {
            Ok(o) => o.unwrap_or_default(),
            Err(e) => {
                report.gaps.push(Gap::Failed(format!(
                    "the official alert feed for {label} could not be reached ({e}); \
                     only derived thresholds were applied"
                )));
                Vec::new()
            }
        };
        let hazards = travel_hazards(day);

        if hazards.is_empty() && official.is_empty() {
            report.checked.push(label);
            continue;
        }
        report.findings.push(TravelFinding {
            plan,
            // An official alert with no derived hazard still deserves reporting,
            // so give it a placeholder hazard rather than an empty list.
            hazards: if hazards.is_empty() {
                vec![Hazard::new(Severity::Severe, "an official weather alert is in force")]
            } else {
                hazards
            },
            provenance: if official.is_empty() { Provenance::Derived } else { Provenance::Official },
            official,
        });
    }
    report
}

async fn assess_heat(
    caller: CallerContext,
    locations: &dyn WatchLocations,
    forecast: &dyn ForecastSource,
    start: NaiveDate,
    end: NaiveDate,
) -> SubjectReport<HeatFinding> {
    // GATE FIRST — same reasoning as travel; the home address is not read.
    if !caller.may_infer_from_routine() {
        return SubjectReport::gap(Gap::NotEntitled);
    }
    // "Could not read what you saved" is not "you saved nothing" — see
    // `WatchLocations::unavailable`.
    if locations.unavailable() {
        return SubjectReport::gap(Gap::Failed(
            "your saved locations could not be read, so I don't know where to watch".into(),
        ));
    }
    let Some(home) = locations.home() else {
        return SubjectReport::gap(Gap::NotConfigured(
            "no home location is configured, so there is nowhere to watch".into(),
        ));
    };

    let days = match forecast.daily(&home).await {
        Ok(d) => d,
        Err(e) => {
            // Deliberately does NOT echo the home address into the failure line.
            return SubjectReport::gap(Gap::Failed(format!(
                "the forecast for your home area could not be fetched ({e})"
            )));
        }
    };

    // Same rule as travel: an ENABLED official feed that fails is a gap, never
    // a silent "no alerts". The derived thresholds still run, so this degrades
    // to a partial answer that SAYS it is partial.
    let mut gaps = Vec::new();
    let official = match forecast.official_alerts(&home).await {
        Ok(o) => o.unwrap_or_default(),
        Err(e) => {
            gaps.push(Gap::Failed(format!(
                "the official alert feed for your home area could not be reached ({e}); \
                 only derived thresholds were applied"
            )));
            Vec::new()
        }
    };

    // Only days INSIDE the requested horizon count, so the answer cannot report
    // a run the header says it did not look at.
    let in_horizon: Vec<DayWeather> = days
        .into_iter()
        .filter(|d| {
            NaiveDate::parse_from_str(&d.date, "%Y-%m-%d")
                .map(|dt| dt >= start && dt <= end)
                .unwrap_or(false)
        })
        .collect();

    // An empty window is NOT a clean check. The provider answered, but about
    // days outside the horizon we were asked about — so nothing in the
    // requested window was actually assessed, and saying "no sustained heat
    // build-up" would be an all-clear for a period we never looked at. (This
    // case only became reachable when the horizon filter above was added; the
    // filter and this guard belong together.)
    if in_horizon.is_empty() {
        gaps.push(Gap::Failed(
            "the forecast does not reach the days you asked about".into(),
        ));
        return SubjectReport { findings: Vec::new(), checked: Vec::new(), gaps };
    }

    let run = heat_run(&in_horizon);
    let checked = vec!["your home area".to_string()];

    // A finding is warranted when EITHER the derived thresholds fired OR an
    // official alert is in force. An authoritative alert is never suppressed
    // just because this module's own numbers disagreed with it.
    if run.is_none() && official.is_empty() {
        return SubjectReport { findings: Vec::new(), checked, gaps };
    }
    SubjectReport {
        findings: vec![HeatFinding {
            location: home,
            run,
            provenance: if official.is_empty() { Provenance::Derived } else { Provenance::Official },
            official,
        }],
        checked,
        gaps,
    }
}

// ── Production forecast source ───────────────────────────────────────────────

/// Live OpenWeather-backed [`ForecastSource`].
///
/// Reuses `crate::weather`'s geocoding and `/data/2.5/forecast` call rather than
/// building a second client — one API key, one base URL, one parser.
struct OwmForecast {
    cfg: crate::weather::WeatherConfig,
}

#[async_trait]
impl ForecastSource for OwmForecast {
    async fn daily(&self, place: &str) -> Result<Vec<DayWeather>, String> {
        let client = crate::weather::WeatherConfig::client().map_err(|e| e.to_string())?;
        let (lat, lon) = crate::weather::geocode(&client, &self.cfg, place)
            .await
            .map_err(|e| e.to_string())?;
        let body = crate::weather::fetch_forecast(&client, &self.cfg, lat, lon)
            .await
            .map_err(|e| e.to_string())?;
        let list = body
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| "the provider returned no forecast data".to_string())?;
        Ok(days_from_forecast(list))
    }

    /// One Call 3.0 is the ONLY OpenWeather product that carries government
    /// alerts; `/data/2.5/weather` and `/data/2.5/forecast` (what this
    /// integration otherwise uses) return no `alerts` array, and 2.5's One Call
    /// was retired. One Call 3.0 is a separate subscription, so this is
    /// opt-in — enabling it on a key without that subscription would spend a
    /// round-trip to earn a 401 on every call.
    ///
    /// Returns `Ok(None)` when not enabled, which the caller treats as "no
    /// official feed", NOT as "no alerts in force".
    async fn official_alerts(&self, place: &str) -> Result<Option<Vec<String>>, String> {
        if !onecall_alerts_enabled() {
            return Ok(None);
        }
        let client = crate::weather::WeatherConfig::client().map_err(|e| e.to_string())?;
        let (lat, lon) = crate::weather::geocode(&client, &self.cfg, place)
            .await
            .map_err(|e| e.to_string())?;
        let url = format!("{}/data/3.0/onecall", self.cfg.base_url);
        let resp = client
            .get(&url)
            .query(&[
                ("lat", lat.to_string()),
                ("lon", lon.to_string()),
                ("exclude", "minutely,hourly".to_string()),
                ("units", "metric".to_string()),
                ("appid", self.cfg.api_key.clone()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            // A failed official lookup must not silently look like "no alerts".
            return Err(format!("official alert feed returned HTTP {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(Some(parse_official_alerts(&body)))
    }
}

fn onecall_alerts_enabled() -> bool {
    // Non-secret behavioural feature flag, so a plain env read is correct here
    // (skill S7: SecretManager is for tokens/keys/URLs-with-credentials).
    matches!(
        std::env::var("OPENWEATHER_ONECALL_ALERTS").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

/// Pull alert headlines out of a One Call 3.0 body. An ABSENT `alerts` key means
/// the provider reported none — which, unlike an unavailable feed, IS an
/// all-clear from an authoritative source, so it maps to an empty list.
pub fn parse_official_alerts(body: &Value) -> Vec<String> {
    body.get("alerts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|al| {
                    let event = al.get("event").and_then(Value::as_str)?;
                    let sender = al.get("sender_name").and_then(Value::as_str).unwrap_or("");
                    Some(if sender.is_empty() {
                        event.to_string()
                    } else {
                        format!("{event} ({sender})")
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Tool ─────────────────────────────────────────────────────────────────────

/// NAME IS LOAD-BEARING: it contains both `severe` and `alert`, which is what
/// makes `crate::tool_cache::is_never_cached` exclude it from the result cache.
/// See `never_cached_by_name` below before renaming anything here.
pub const TOOL_NAME: &str = "weather_severe_alerts";

struct SevereWeatherWatch {
    cfg: crate::weather::WeatherConfig,
}

impl SevereWeatherWatch {
    async fn run(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<&crate::locations::CallerKey>,
    ) -> Result<String, ToolError> {
        let days = match args.get("days").filter(|v| !v.is_null()) {
            Some(v) => v
                .as_i64()
                .ok_or_else(|| ToolError::InvalidArgument("'days' must be an integer".into()))?
                .max(1) as usize,
            None => DEFAULT_HORIZON_DAYS,
        };
        let calendar: Arc<dyn CalendarSource> = self.cfg.calendar.clone();
        // The home location comes from THIS CALLER's registry record, and from
        // nowhere else. With no identity it is empty. There is no `COMMUTE_*`
        // fallback any more — see `weather::location::Routine` for why it was
        // removed rather than narrowed.
        let locations = match key {
            None => ResolvedLocations { routine: Routine::default(), degraded: false },
            Some(k) => {
                let r = Routine::resolve_for(self.cfg.locations.as_ref(), Some(k), caller);
                ResolvedLocations { routine: r.routine, degraded: r.degraded }
            }
        };
        let forecast = OwmForecast { cfg: self.cfg.clone() };
        let today = chrono::Local::now().date_naive();
        let report =
            run_watch(days, caller, calendar.as_ref(), &locations, &forecast, today).await;
        Ok(report.render())
    }
}

#[async_trait]
impl RustTool for SevereWeatherWatch {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        // No city names anywhere — same rule as `weather`'s description, and for
        // the same reason: an example in a schema becomes a default in practice.
        "Check whether severe weather is coming that the USER needs to act on: (a) storms \
likely to disrupt a trip or flight they actually have scheduled, checked at the \
destination on the travel day, and (b) a sustained heat wave at their home that puts \
their power and air-conditioning at risk. Takes no location — it works out what to \
watch from the user's own calendar and home location, and it is the right tool for \
\"is anything coming I should worry about\", \"will the weather mess up my trip\", \
\"should I be worried about the heat\". For a plain forecast for a named place, use \
the 'weather' tool instead. Optional 'days' (1-6) sets how far ahead to look; the \
default is 3. Results are never cached. The answer distinguishes \"checked, nothing \
severe\" from \"could not check\" — do not report the second as an all-clear, and do \
not fill in anything it says it could not check."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_HORIZON_DAYS,
                    "description": "How many days ahead to watch. Default 3, clamped to what the forecast covers."
                }
            }
        })
    }

    /// Fail-closed entry point: no caller identity ⇒
    /// [`CallerContext::untrusted`] ⇒ neither the calendar nor the home location
    /// is read, and the answer discloses nothing.
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        self.run(args, CallerContext::untrusted(), None).await
    }

    /// Entitlement but no identity: the heat watch has no per-caller home to
    /// read, and there is nothing to substitute — the `COMMUTE_*` fallback is
    /// gone. It degrades to "no home location is configured", which is the
    /// honest answer for a caller nobody can name, and is deliberately a
    /// different sentence from "could not check".
    async fn execute_with_caller(
        &self,
        args: Value,
        caller: CallerContext,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text_only(self.run(args, caller, None).await?))
    }

    /// The full path: entitlement AND whose record to read.
    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<crate::locations::CallerKey>,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text_only(self.run(args, caller, key.as_ref()).await?))
    }
}

/// Register the watch tool. Shares `weather`'s config (one API key, one
/// calendar seam); silently absent when the provider is unconfigured, because a
/// severe-weather tool that cannot reach a provider should not be advertised.
pub(crate) fn register(
    registry: &mut crate::registry::ToolRegistry,
    cfg: &crate::weather::WeatherConfig,
) {
    registry.register_or_replace(Box::new(SevereWeatherWatch { cfg: cfg.clone() }));
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── fixtures ────────────────────────────────────────────────────────────

    fn day(date: &str) -> DayWeather {
        DayWeather {
            date: date.into(),
            temp_min_c: 14.0,
            temp_max_c: 22.0,
            max_wind_ms: 4.0,
            max_rain_3h_mm: 0.0,
            max_snow_3h_mm: 0.0,
            condition_ids: vec![800],
            description: "clear sky".into(),
        }
    }

    /// A pleasant day nobody should be warned about.
    fn mild(date: &str) -> DayWeather {
        let mut d = day(date);
        d.max_rain_3h_mm = 1.2; // light rain — explicitly NOT disruption
        d.condition_ids = vec![500];
        d.description = "light rain".into();
        d
    }

    fn storm(date: &str) -> DayWeather {
        let mut d = day(date);
        d.max_wind_ms = 22.0;
        d.max_snow_3h_mm = 6.0;
        d.condition_ids = vec![602, 200];
        d.description = "heavy snow".into();
        d
    }

    fn hot(date: &str, max_c: f64, min_c: f64) -> DayWeather {
        let mut d = day(date);
        d.temp_max_c = max_c;
        d.temp_min_c = min_c;
        d.description = "clear sky".into();
        d
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
    }

    fn operator() -> CallerContext {
        CallerContext::entitled_for_test_only(true, true)
    }
    fn guest() -> CallerContext {
        CallerContext::default()
    }

    /// A calendar that COUNTS reads, so a test can assert the operator's
    /// calendar was never TOUCHED — not merely that its contents didn't surface.
    struct CountingCalendar {
        calls: Arc<AtomicUsize>,
        window: CalendarWindow,
    }
    impl CountingCalendar {
        fn with(events: Vec<Value>) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Self { calls: calls.clone(), window: CalendarWindow::Events(events) }, calls)
        }
        fn windowed(w: CalendarWindow) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Self { calls: calls.clone(), window: w }, calls)
        }
    }
    #[async_trait]
    impl CalendarSource for CountingCalendar {
        async fn events_now(&self) -> Vec<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
        async fn events_between(&self, _s: NaiveDate, _e: NaiveDate) -> CalendarWindow {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.window.clone()
        }
    }

    /// A location source that COUNTS reads of the home address.
    struct CountingLocations {
        calls: Arc<AtomicUsize>,
        home: Option<String>,
    }
    impl CountingLocations {
        fn with(home: Option<&str>) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Self { calls: calls.clone(), home: home.map(str::to_string) }, calls)
        }
    }
    impl WatchLocations for CountingLocations {
        fn home(&self) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.home.clone()
        }
    }

    struct FakeForecast {
        days: Vec<DayWeather>,
        fail: Option<String>,
        official: Option<Vec<String>>,
        /// The ENABLED-but-broken official feed (distinct from `official: None`,
        /// which means no feed is configured at all).
        official_fail: Option<String>,
        calls: Arc<AtomicUsize>,
    }
    impl FakeForecast {
        fn ok(days: Vec<DayWeather>) -> Self {
            Self {
                days,
                fail: None,
                official: None,
                official_fail: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn failing() -> Self {
            Self {
                days: Vec::new(),
                fail: Some("provider unreachable".into()),
                official: None,
                official_fail: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn with_official(mut self, a: Vec<&str>) -> Self {
            self.official = Some(a.into_iter().map(str::to_string).collect());
            self
        }
        fn with_broken_official_feed(mut self) -> Self {
            self.official_fail = Some("official feed HTTP 503".into());
            self
        }
    }
    #[async_trait]
    impl ForecastSource for FakeForecast {
        async fn daily(&self, _place: &str) -> Result<Vec<DayWeather>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.days.clone()),
            }
        }
        async fn official_alerts(&self, _place: &str) -> Result<Option<Vec<String>>, String> {
            match &self.official_fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.official.clone()),
            }
        }
    }

    /// A flight event. Placeholders only — the destination is an obviously
    /// invented city and there is no real address anywhere in this file.
    fn flight_event(date: &str, dest: &str) -> Value {
        json!({
            "summary": "Flight to the client site",
            "location": dest,
            "status": "confirmed",
            "dtstart": format!("{date}T130000Z"),
        })
    }

    // ── TRAVEL: the primary positive case ───────────────────────────────────

    #[tokio::test]
    async fn entitled_operator_with_a_calendar_flight_into_severe_weather_is_flagged() {
        let (cal, cal_calls) = CountingCalendar::with(vec![flight_event("20260803", "Examplecity")]);
        let (loc, _) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![day("2026-08-01"), day("2026-08-02"), storm("2026-08-03")]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert_eq!(cal_calls.load(Ordering::SeqCst), 1, "the operator's calendar IS read for them");
        assert_eq!(r.travel.findings.len(), 1, "the flight must be flagged");
        let f = &r.travel.findings[0];
        assert_eq!(f.plan.kind, TravelKind::Flight, "a 'Flight to ...' event is a flight");
        assert_eq!(f.severity(), Severity::Severe);
        assert_eq!(f.provenance, Provenance::Derived);

        // ATTRIBUTION: the answer must say which trip, where and when — otherwise
        // it is a weather report, not a warning about something the user has.
        let text = r.render();
        assert!(text.contains("Flight to the client site"), "attributes the event: {text}");
        assert!(text.contains("Examplecity"), "names the destination: {text}");
        assert!(text.contains("2026-08-03"), "names the day: {text}");
        assert!(text.to_lowercase().contains("rebook"), "says why it matters: {text}");
        assert!(text.contains("DERIVED"), "labels derived findings: {text}");
    }

    /// MUTATION PROOF for the above: if `travel_hazards` stops flagging (the
    /// "silently flags nothing" regression), this fixture must go clear — so the
    /// positive test above cannot be satisfied by a do-nothing implementation.
    #[test]
    fn positive_control_the_storm_fixture_really_does_trip_the_thresholds() {
        let h = travel_hazards(&storm("2026-08-03"));
        assert!(!h.is_empty(), "the storm fixture must produce hazards");
        assert_eq!(h.iter().map(|x| x.severity).max(), Some(Severity::Severe));
        // ...and the mild fixture must NOT, or the positive case proves nothing.
        assert!(travel_hazards(&mild("2026-08-03")).is_empty());
    }

    /// Mutation: drop the wind threshold below the fixture and the finding
    /// disappears — i.e. the assertion above is really driven by the threshold.
    #[test]
    fn travel_thresholds_are_what_decide_disruption() {
        let mut windy = day("2026-08-03");
        windy.max_wind_ms = TRAVEL_WIND_MS - 0.1;
        assert!(travel_hazards(&windy).is_empty(), "just under the gale threshold: no alert");
        windy.max_wind_ms = TRAVEL_WIND_MS + 0.1;
        assert!(!travel_hazards(&windy).is_empty(), "just over: alert");

        let mut rainy = day("2026-08-03");
        rainy.max_rain_3h_mm = TRAVEL_RAIN_3H_MM - 0.1;
        assert!(travel_hazards(&rainy).is_empty(), "heavy-ish rain under threshold: no alert");
        rainy.max_rain_3h_mm = TRAVEL_RAIN_3H_MM + 0.1;
        assert!(!travel_hazards(&rainy).is_empty());

        let mut snowy = day("2026-08-03");
        snowy.max_snow_3h_mm = TRAVEL_SNOW_3H_MM - 0.1;
        // Below the snow threshold and above freezing: nothing.
        assert!(travel_hazards(&snowy).is_empty());
        snowy.max_snow_3h_mm = TRAVEL_SNOW_3H_MM + 0.1;
        assert!(!travel_hazards(&snowy).is_empty());
    }

    // ── HEAT: the primary positive case ─────────────────────────────────────

    #[tokio::test]
    async fn sustained_heat_at_home_is_flagged_as_a_power_and_hvac_risk() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, loc_calls) = CountingLocations::with(Some("Examplecity"));
        let fc = FakeForecast::ok(vec![
            hot("2026-08-01", 35.0, 24.0),
            hot("2026-08-02", 36.5, 23.5),
            hot("2026-08-03", 34.0, 22.0),
        ]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert_eq!(loc_calls.load(Ordering::SeqCst), 1);
        assert_eq!(r.heat.findings.len(), 1);
        let f = &r.heat.findings[0];
        let run = f.run.as_ref().expect("a derived run");
        assert_eq!(run.days.len(), 3);
        assert_eq!(run.severity, Severity::Severe, "3 consecutive days is severe");

        let text = r.render();
        // The FRAMING is the requirement: power/HVAC, not "it will be hot".
        let lower = text.to_lowercase();
        assert!(lower.contains("cooling load"), "framed as load: {text}");
        assert!(lower.contains("outage") || lower.contains("supply"), "framed as power: {text}");
        assert!(lower.contains("overnight"), "explains the warm-night mechanism: {text}");
    }

    /// Positive control + mutation for heat: an ordinary hot spell that does NOT
    /// meet the sustained-load definition must produce nothing, and nudging each
    /// threshold must flip the result. Without this, a heat check that always
    /// returned `None` would pass every other heat test.
    #[test]
    fn heat_thresholds_separate_a_hot_spell_from_a_sustained_load_event() {
        // Hot days, but the nights recover: not a power problem.
        let cool_nights = vec![
            hot("2026-08-01", 35.0, HEAT_NIGHT_MIN_C - 0.1),
            hot("2026-08-02", 36.0, HEAT_NIGHT_MIN_C - 0.1),
            hot("2026-08-03", 35.0, HEAT_NIGHT_MIN_C - 0.1),
        ];
        assert!(heat_run(&cool_nights).is_none(), "warm days + cool nights is not a heat wave");

        // Warm nights, but the days are not hot enough.
        let mild_days = vec![
            hot("2026-08-01", HEAT_DAY_MAX_C - 0.1, 24.0),
            hot("2026-08-02", HEAT_DAY_MAX_C - 0.1, 24.0),
        ];
        assert!(heat_run(&mild_days).is_none());

        // One qualifying day is weather, not a wave.
        let one_day = vec![hot("2026-08-01", 38.0, 25.0), hot("2026-08-02", 20.0, 12.0)];
        assert!(heat_run(&one_day).is_none(), "a single day must not fire");

        // Exactly the minimum run qualifies — the positive control.
        let two_days = vec![hot("2026-08-01", 33.0, 22.0), hot("2026-08-02", 33.0, 22.0)];
        let run = heat_run(&two_days).expect("two qualifying days IS a heat wave");
        assert_eq!(run.days.len(), HEAT_RUN_DAYS);
        assert_eq!(run.severity, Severity::Disruptive, "2 days, under 100F: not yet severe");

        // A run broken in the middle does not accumulate across the gap.
        let broken = vec![
            hot("2026-08-01", 34.0, 23.0),
            hot("2026-08-02", 20.0, 11.0),
            hot("2026-08-03", 34.0, 23.0),
        ];
        assert!(heat_run(&broken).is_none(), "non-consecutive days are not sustained");

        // A single extreme day inside a qualifying run escalates it.
        let spike = vec![
            hot("2026-08-01", 33.0, 22.0),
            hot("2026-08-02", HEAT_SEVERE_MAX_C + 0.5, 24.0),
        ];
        assert_eq!(heat_run(&spike).unwrap().severity, Severity::Severe);
    }

    /// REVIEW FINDING (gpt56, round 1): `heat_run` used to treat SLICE
    /// adjacency as CALENDAR adjacency. A provider that omits a date would then
    /// let two days that are actually days apart be reported as a "sustained"
    /// run — a false heat-wave warning manufactured out of a gap in the data.
    #[test]
    fn a_heat_run_must_be_consecutive_by_date_not_merely_adjacent_in_the_slice() {
        // 08-01 and 08-03 both qualify, but 08-02 is MISSING from the forecast.
        let with_a_hole = vec![hot("2026-08-01", 35.0, 24.0), hot("2026-08-03", 35.0, 24.0)];
        assert!(
            heat_run(&with_a_hole).is_none(),
            "a missing intermediate day must break the run, not be papered over"
        );

        // The control: the same two readings on genuinely consecutive dates DO
        // qualify — so the assertion above is about adjacency, not the values.
        let contiguous = vec![hot("2026-08-01", 35.0, 24.0), hot("2026-08-02", 35.0, 24.0)];
        assert!(heat_run(&contiguous).is_some());

        // A run may still be found AFTER a hole, as long as it is itself contiguous.
        let hole_then_run = vec![
            hot("2026-08-01", 35.0, 24.0),
            hot("2026-08-05", 35.0, 24.0),
            hot("2026-08-06", 35.0, 24.0),
        ];
        let r = heat_run(&hole_then_run).expect("the contiguous tail still qualifies");
        assert_eq!(r.days.len(), 2);
        assert_eq!(r.days[0].date, "2026-08-05");

        // An unparseable date cannot prove adjacency, so it must not extend a run.
        let bad_date = vec![
            hot("2026-08-01", 35.0, 24.0),
            hot("not-a-date", 35.0, 24.0),
            hot("2026-08-02", 35.0, 24.0),
        ];
        assert!(heat_run(&bad_date).is_none(), "fail safe on an unparseable date");
    }

    /// REVIEW FINDING (gpt56, round 1): an ENABLED official alert feed that
    /// FAILS was being discarded via `.ok().flatten().unwrap_or_default()`, so
    /// an unavailable authoritative source could render as a clean check. That
    /// is the dishonest degradation this module exists to prevent — and it
    /// contradicted `OwmForecast::official_alerts`'s own doc comment.
    #[tokio::test]
    async fn a_broken_official_feed_is_a_gap_not_a_silent_no_alerts() {
        let (cal, _) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        // Derived thresholds find NOTHING, so without this fix the whole report
        // would read as a clean all-clear despite an unreachable official feed.
        let fc = FakeForecast::ok(vec![mild("2026-08-01"), mild("2026-08-02"), mild("2026-08-03")])
            .with_broken_official_feed();

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert!(!r.travel.is_clear(), "an unreachable official feed is not a clean travel check");
        assert!(!r.heat.is_clear(), "...nor a clean heat check");
        assert!(r.travel.gaps.iter().any(|g| matches!(g, Gap::Failed(_))));
        assert!(r.heat.gaps.iter().any(|g| matches!(g, Gap::Failed(_))));

        let text = r.render();
        assert!(text.contains("official alert feed"), "{text}");
        assert!(text.contains("unknown, not clear"), "{text}");
        // The honest half of a partial answer survives: the derived check that
        // DID succeed is still reported, scoped to what it covered.
        assert!(text.contains("nothing disruptive expected"), "{text}");
        assert!(!text.contains("Homeplace"), "the home location is never echoed: {text}");
    }

    /// `Ok(None)` — no official feed CONFIGURED — is not a failure and must not
    /// produce a gap. This is the boundary the fix above must not overshoot.
    #[tokio::test]
    async fn no_official_feed_configured_is_not_a_gap() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        let fc = FakeForecast::ok(vec![mild("2026-08-01")]);
        let r = run_watch(1, operator(), &cal, &loc, &fc, today()).await;
        assert!(r.heat.gaps.is_empty(), "an absent feed is not an error");
        assert!(r.heat.is_clear());
    }

    // ── ENTITLEMENT: the non-negotiable one ─────────────────────────────────

    #[tokio::test]
    async fn a_guest_gets_nothing_and_causes_no_read_of_either_source() {
        // The operator's calendar and home are both populated and both sensitive.
        let (cal, cal_calls) = CountingCalendar::with(vec![json!({
            "summary": "Dentist appointment", // pii-test-fixture: placeholder, asserted never to reach a guest
            "location": "000 Placeholder St, Examplecity", // pii-test-fixture: placeholder home/appointment address
            "status": "confirmed",
            "dtstart": "20260802T090000Z",
        })]);
        let (loc, loc_calls) = CountingLocations::with(Some("000 Placeholder St, Examplecity")); // pii-test-fixture: placeholder home address
        let fc = FakeForecast::ok(vec![
            storm("2026-08-02"),
            hot("2026-08-01", 40.0, 28.0),
            hot("2026-08-02", 40.0, 28.0),
            hot("2026-08-03", 40.0, 28.0),
        ]);

        let r = run_watch(3, guest(), &cal, &loc, &fc, today()).await;

        // THE strong assertion: not "the data didn't appear", but "it was never read".
        assert_eq!(cal_calls.load(Ordering::SeqCst), 0, "a guest must not cause a calendar read");
        assert_eq!(loc_calls.load(Ordering::SeqCst), 0, "a guest must not cause a home-location read");
        assert_eq!(fc.calls.load(Ordering::SeqCst), 0, "and no forecast is fetched on their behalf");

        assert!(r.travel.findings.is_empty());
        assert!(r.heat.findings.is_empty());
        assert_eq!(r.travel.gaps, vec![Gap::NotEntitled]);
        assert_eq!(r.heat.gaps, vec![Gap::NotEntitled]);

        let text = r.render();
        for leak in [
            "Dentist",
            "Placeholder",
            "Examplecity",
            "2026-08-02",
            "calendar",
            "home",
            "heat",
            "travel",
            "Travel",
        ] {
            assert!(
                !text.contains(leak),
                "a guest must not learn '{leak}' — not the content, and not that \
                 there is content to withhold. Got: {text}"
            );
        }
    }

    /// The two sources are gated INDEPENDENTLY: entitlement to one must not leak
    /// the other. (Mutation: collapse the two flags into one and this fails.)
    #[tokio::test]
    async fn entitlement_is_per_source() {
        let (cal, cal_calls) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc, loc_calls) = CountingLocations::with(Some("Examplecity"));
        let fc = FakeForecast::ok(vec![storm("2026-08-02")]);

        // Routine only: heat is assessed, travel is not, and the calendar is untouched.
        let r = run_watch(
            3,
            CallerContext::entitled_for_test_only(false, true),
            &cal,
            &loc,
            &fc,
            today(),
        )
        .await;
        assert_eq!(cal_calls.load(Ordering::SeqCst), 0);
        assert_eq!(loc_calls.load(Ordering::SeqCst), 1);
        assert_eq!(r.travel.gaps, vec![Gap::NotEntitled]);
        assert!(r.heat.gaps.is_empty(), "the routine-entitled half still works");

        // Calendar only: the mirror image.
        let (cal2, cal2_calls) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc2, loc2_calls) = CountingLocations::with(Some("Examplecity"));
        let r2 = run_watch(
            3,
            CallerContext::entitled_for_test_only(true, false),
            &cal2,
            &loc2,
            &fc,
            today(),
        )
        .await;
        assert_eq!(cal2_calls.load(Ordering::SeqCst), 1);
        assert_eq!(loc2_calls.load(Ordering::SeqCst), 0);
        assert_eq!(r2.heat.gaps, vec![Gap::NotEntitled]);
        assert_eq!(r2.travel.findings.len(), 1);
    }

    /// The un-threaded `execute()` path is `untrusted()`, so the type-level
    /// guarantee holds end to end: no production constructor can widen it.
    #[test]
    fn the_unthreaded_path_is_untrusted() {
        let c = CallerContext::untrusted();
        assert!(!c.may_infer_from_calendar() && !c.may_infer_from_routine());
        assert_eq!(CallerContext::default(), c);
    }

    // ── LOCREG-01: the home location is per-caller, and un-keyed fails closed ─

    /// The same disclosure `WeatherConfig::resolve_location_for` was fixed for,
    /// in the OTHER weather tool. The heat watch used to read
    /// `WeatherConfig::routine` — the process-global `COMMUTE_*` pair holding
    /// the OPERATOR's own address — for anyone entitled to the routine.
    /// `weather_severe_alerts` overrides `execute_with_caller` and is reached
    /// through the default `execute_with_caller_key`, so this WAS live on the
    /// production path.
    ///
    /// Round 4: this now sets `COMMUTE_HOME`/`COMMUTE_WORK` FOR REAL in the
    /// process environment, because the claim under test is no longer "the
    /// un-keyed path doesn't pass the legacy routine along" but the stronger
    /// "nothing in this tool reads those variables, for any caller". It covers
    /// the un-keyed path AND two entitled, KEYED callers behind one service
    /// principal — the live TERM #577 shape, and the case the previous rounds'
    /// positive control was asserting the wrong way round.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_caller_can_reach_a_commute_env_home_through_the_watch() {
        const LEGACY_HOME: &str = "9 Legacy Lane, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a COMMUTE_HOME value that must never resolve
        const LEGACY_WORK: &str = "8 Legacy Court, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a COMMUTE_WORK value that must never resolve

        struct EnvGuard(Vec<(&'static str, Option<String>)>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (k, prev) in &self.0 {
                    match prev {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let _guard = EnvGuard(
            ["COMMUTE_HOME", "COMMUTE_WORK"]
                .iter()
                .map(|k| (*k, std::env::var(k).ok()))
                .collect(),
        );
        std::env::set_var("COMMUTE_HOME", LEGACY_HOME);
        std::env::set_var("COMMUTE_WORK", LEGACY_WORK);

        let cfg = crate::weather::WeatherConfig {
            api_key: String::new(),
            base_url: "http://127.0.0.1:1".into(),
            units: "metric".into(),
            routine: Routine::default(),
            calendar: Arc::new(crate::weather::location::NoCalendar),
            locations: Arc::new(crate::locations::store::fake::CountingStore::new()),
        };
        let tool = SevereWeatherWatch { cfg };

        let svc = crate::locations::CallerKey::for_principal_name("lumina").unwrap();
        let one = crate::locations::CallerKey::for_person("lumina", "someone").unwrap();
        let two = crate::locations::CallerKey::for_person("lumina", "someone-else").unwrap();
        let cases: [(&str, Option<&crate::locations::CallerKey>); 4] = [
            ("entitled, no identity", None),
            ("entitled, the shared service principal", Some(&svc)),
            ("entitled, one person behind it", Some(&one)),
            ("entitled, another person behind it", Some(&two)),
        ];
        for (what, key) in cases {
            let out = tool.run(json!({"days": 2}), operator(), key).await.unwrap();
            assert!(!out.contains("Legacy"), "{what}: a COMMUTE_* value leaked: {out}");
            assert!(!out.contains("Examplecity"), "{what}: an address leaked: {out}");
            assert!(
                out.to_lowercase().contains("no home location is configured"),
                "{what}: with nothing saved the honest answer is 'nothing configured': {out}"
            );
            // ...and "nothing saved" is a DIFFERENT sentence from "I couldn't
            // read what you saved" (`Gap::Failed`), which is the distinction
            // `WatchLocations::unavailable` exists to preserve. Both correctly
            // sit under the shared "this is unknown, not clear" framing — what
            // must never collapse is the REASON.
            assert!(
                !out.contains("could not be read"),
                "{what}: 'nothing saved' must not be reported as a read failure: {out}"
            );
            // And it must never read as an all-clear.
            assert!(
                !out.contains("no sustained heat build-up"),
                "{what}: an unconfigured home must not become an all-clear: {out}"
            );
        }
    }

    /// POSITIVE CONTROL: a KEYED caller with a saved home is still watched, so
    /// the test above cannot be satisfied by disabling the heat watch.
    #[tokio::test]
    async fn a_keyed_watch_uses_that_callers_saved_home() {
        use crate::locations::{self, CallerKey};

        const SAVED_HOME: &str = "1 Placeholder Way, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a saved home address
        let store = Arc::new(crate::locations::store::fake::CountingStore::new());
        let key = CallerKey::for_principal_name("alpha").unwrap();
        match locations::set(store.as_ref(), Some(&key), operator(), locations::HOME, SAVED_HOME, None, true) {
            locations::WriteOutcome::Stored { .. } => {}
            other => panic!("seed failed: {other:?}"),
        }

        let r = Routine::resolve_for(store.as_ref(), Some(&key), operator());
        let locs = ResolvedLocations { routine: r.routine, degraded: r.degraded };
        assert_eq!(locs.home().as_deref(), Some(SAVED_HOME));
        assert!(!locs.unavailable());

        let (cal, _) = CountingCalendar::with(vec![]);
        let fc = FakeForecast::failing();
        let report = run_watch(3, operator(), &cal, &locs, &fc, today()).await;
        // The forecast fails, but the point is that it was ATTEMPTED for the
        // caller's own home rather than skipped as "nothing configured".
        assert!(
            !matches!(report.heat.gaps.first(), Some(Gap::NotConfigured(_))),
            "the caller's saved home must be watched, got {:?}",
            report.heat.gaps
        );
    }

    /// "Could not read your saved locations" must not render as "you have no
    /// home configured" — the absence/failure collapse `crate::locations`
    /// refuses to make.
    #[tokio::test]
    async fn an_unreadable_registry_is_a_could_not_check_not_a_nothing_configured() {
        let locs = ResolvedLocations { routine: Routine::default(), degraded: true };
        let (cal, _) = CountingCalendar::with(vec![]);
        let fc = FakeForecast::failing();
        let r = run_watch(3, operator(), &cal, &locs, &fc, today()).await;
        match r.heat.gaps.first() {
            Some(Gap::Failed(why)) => assert!(why.contains("could not be read"), "{why}"),
            other => panic!("expected a could-not-check gap, got {other:?}"),
        }
    }

    // ── DEGRADE HONESTLY ────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_provider_failure_says_could_not_check_and_never_all_clear() {
        // Home and destination are DIFFERENT strings on purpose: the travel gap
        // may name the destination (the caller is entitled to their own
        // calendar, and "which trip couldn't I check?" is the useful part),
        // while the home gap must not echo the home address back out.
        let (cal, _) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        let fc = FakeForecast::failing();

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert!(r.travel.findings.is_empty() && r.heat.findings.is_empty());
        assert!(!r.travel.gaps.is_empty(), "the failure is reported, not swallowed");
        assert!(!r.heat.gaps.is_empty());
        assert!(!r.travel.is_clear(), "a failed check is NOT clear");
        assert!(!r.heat.is_clear());

        let text = r.render();
        assert!(text.contains("Could not check"), "{text}");
        assert!(text.contains("unknown, not clear"), "{text}");
        assert!(!text.contains("nothing disruptive expected"), "must not read as all-clear: {text}");
        assert!(!text.contains("no sustained heat build-up"), "{text}");
        assert!(
            text.contains("treat this as \"unknown\", not \"all clear\""),
            "with nothing checked at all the report must say so outright: {text}"
        );
        // The home location is never echoed into a failure line.
        assert!(!text.contains("Homeplace"), "the failure line leaks the home location: {text}");
    }

    /// The contrast case, which is what makes the assertion above meaningful:
    /// the SAME shape of input with a WORKING provider says "checked" in words
    /// that cannot be confused with "could not check".
    #[tokio::test]
    async fn checked_and_clear_is_worded_differently_from_could_not_check() {
        let (cal, _) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc, _) = CountingLocations::with(Some("Examplecity"));
        let fc = FakeForecast::ok(vec![mild("2026-08-01"), mild("2026-08-02"), mild("2026-08-03")]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert!(r.travel.is_clear() && r.heat.is_clear());
        let text = r.render();
        assert!(text.contains("nothing disruptive expected"), "{text}");
        assert!(text.contains("no sustained heat build-up"), "{text}");
        assert!(!text.contains("Could not check"), "{text}");
        assert!(!text.contains("unknown, not clear"), "{text}");
    }

    /// An unconfigured home (the LIVE state — nobody has saved one, and there is
    /// no env fallback) must never become a guessed location, and must not read
    /// as clear.
    #[tokio::test]
    async fn an_unconfigured_home_is_reported_not_invented() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, loc_calls) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![hot("2026-08-01", 40.0, 28.0), hot("2026-08-02", 40.0, 28.0)]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert_eq!(loc_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fc.calls.load(Ordering::SeqCst), 0, "nothing to fetch without a home");
        assert!(r.heat.findings.is_empty());
        assert!(!r.heat.is_clear());
        let text = r.render();
        assert!(text.contains("no home location is configured"), "{text}");
        assert!(text.contains("unknown, not clear"), "{text}");
    }

    #[tokio::test]
    async fn no_calendar_configured_is_could_not_check_not_no_travel() {
        let (cal, _) = CountingCalendar::windowed(CalendarWindow::NotConfigured);
        let (loc, _) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;
        assert_eq!(r.travel.gaps, vec![Gap::NotConfigured("no calendar is connected".into())]);
        assert!(!r.travel.is_clear());
        assert!(r.render().contains("unknown, not clear"));
    }

    #[tokio::test]
    async fn an_unreachable_calendar_is_could_not_check_not_no_travel() {
        let (cal, _) = CountingCalendar::windowed(CalendarWindow::Unavailable("timeout".into()));
        let (loc, _) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;
        assert!(matches!(r.travel.gaps.as_slice(), [Gap::Failed(_)]));
        assert!(!r.travel.is_clear());
    }

    /// An EMPTY calendar that was successfully read IS a real all-clear — the
    /// one case where "no events" means what it says. Mutation guard: if
    /// `CalendarWindow::Events(vec![])` ever collapsed into the NotConfigured
    /// path this flips.
    #[tokio::test]
    async fn an_empty_but_readable_calendar_is_genuinely_clear() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![]);
        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;
        assert!(r.travel.is_clear());
        assert!(r.render().contains("no trips scheduled"));
    }

    // ── NO CRYING WOLF ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn ordinary_weather_produces_no_alert() {
        let (cal, _) = CountingCalendar::with(vec![flight_event("20260802", "Examplecity")]);
        let (loc, _) = CountingLocations::with(Some("Examplecity"));
        // A warm, showery week: exactly the sort of thing a naive threshold fires on.
        let fc = FakeForecast::ok(vec![
            mild("2026-08-01"),
            mild("2026-08-02"),
            hot("2026-08-03", 29.0, 18.0),
        ]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;
        assert!(!r.has_findings(), "a mild week must not produce an alert: {r:?}");
        assert!(r.travel.is_clear() && r.heat.is_clear());
    }

    // ── NEVER CACHED ────────────────────────────────────────────────────────

    /// Checked against the REAL cache policy, not asserted about the rule.
    ///
    /// The trap: `TOOL_NAME` starts with `weather`, which carries a 20-minute
    /// TTL in `SEED_POLICY`. Only `is_never_cached` — evaluated first — keeps
    /// this tool out of the cache. This test is what stops a rename from
    /// silently making stale severe-weather answers cacheable.
    #[test]
    fn never_cached_by_name() {
        assert!(
            crate::tool_cache::policy_for(TOOL_NAME).is_none(),
            "{TOOL_NAME} must have NO cache policy"
        );
        // The mutation control: prove the cache would otherwise have caught it.
        assert!(
            crate::tool_cache::policy_for("weather").is_some(),
            "the plain weather tool IS cached — so the exclusion above is doing real work"
        );
        assert!(
            crate::tool_cache::policy_for("weather_watch").is_some(),
            "a name without alert/severe/warning WOULD be cached — do not rename to one"
        );
        assert!(TOOL_NAME.contains("severe") && TOOL_NAME.contains("alert"));
    }

    // ── parsing / plumbing ──────────────────────────────────────────────────

    #[test]
    fn travel_plans_ignore_what_is_not_travel() {
        let events = vec![
            json!({"summary": "Standup", "location": "https://zoom.us/j/1", "dtstart": "20260802T090000Z"}),
            json!({"summary": "Cancelled trip", "location": "Examplecity", "status": "cancelled", "dtstart": "20260802T090000Z"}),
            json!({"summary": "Declined offsite", "location": "Othertown", "status": "declined", "dtstart": "20260802T090000Z"}),
            json!({"summary": "No location", "dtstart": "20260802T090000Z"}),
            json!({"summary": "Too far out", "location": "Faraway", "dtstart": "20261102T090000Z"}),
            json!({"summary": "Client onsite", "location": "Othertown", "dtstart": "20260802T090000Z"}),
        ];
        let plans = travel_plans(&events, today(), today() + chrono::Duration::days(2));
        assert_eq!(plans.len(), 1, "got {plans:?}");
        assert_eq!(plans[0].destination, "Othertown");
        assert_eq!(plans[0].kind, TravelKind::Trip, "an onsite is a trip, not a flight");
    }

    #[test]
    fn flight_shaped_events_are_recognised_conservatively() {
        for (summary, expect) in [
            ("Flight to the client site", TravelKind::Flight),
            ("Depart for the conference", TravelKind::Flight),
            ("Airport pickup", TravelKind::Flight),
            ("Dentist appointment", TravelKind::Trip),
            ("Client onsite", TravelKind::Trip),
        ] {
            let ev = json!({"summary": summary, "location": "Othertown", "dtstart": "20260802T090000Z"});
            let p = travel_plans(&[ev], today(), today() + chrono::Duration::days(2));
            assert_eq!(p[0].kind, expect, "{summary}");
        }
    }

    #[test]
    fn all_day_and_zulu_dtstart_forms_both_parse() {
        for s in ["20260802", "20260802T090000Z", "20260802T090000"] {
            let ev = json!({"summary": "Trip", "location": "Othertown", "dtstart": s});
            let p = travel_plans(&[ev], today(), today() + chrono::Duration::days(2));
            assert_eq!(p.len(), 1, "dtstart {s} must parse");
        }
        let ev = json!({"summary": "Trip", "location": "Othertown"});
        assert!(travel_plans(&[ev], today(), today() + chrono::Duration::days(2)).is_empty());
    }

    #[test]
    fn day_weather_reduces_raw_forecast_points() {
        let list = vec![
            json!({"dt_txt": "2026-08-03 09:00:00", "main": {"temp": 20.0, "temp_min": 18.0, "temp_max": 22.0},
                   "wind": {"speed": 6.0, "gust": 19.0}, "snow": {"3h": 3.0},
                   "weather": [{"id": 601, "description": "snow"}]}),
            json!({"dt_txt": "2026-08-03 12:00:00", "main": {"temp": 24.0},
                   "wind": {"speed": 5.0}, "rain": {"3h": 2.0},
                   "weather": [{"id": 500, "description": "light rain"}]}),
            json!({"dt_txt": "2026-08-04 09:00:00", "main": {"temp": 30.0},
                   "weather": [{"id": 800, "description": "clear sky"}]}),
        ];
        let days = days_from_forecast(&list);
        assert_eq!(days.len(), 2);
        let d = &days[0];
        assert_eq!(d.date, "2026-08-03");
        assert_eq!(d.temp_min_c, 18.0);
        assert_eq!(d.temp_max_c, 24.0);
        assert_eq!(d.max_wind_ms, 19.0, "gust beats mean wind");
        assert_eq!(d.max_snow_3h_mm, 3.0);
        assert_eq!(d.max_rain_3h_mm, 2.0, "peak per-step rate, not the daily total");
        assert!(d.condition_ids.contains(&601) && d.condition_ids.contains(&500));
        // ...and this day is disruptive, which is what the reduction is FOR.
        assert!(!travel_hazards(d).is_empty());
    }

    #[test]
    fn official_alerts_are_parsed_and_an_absent_key_is_a_real_all_clear() {
        let body = json!({"alerts": [
            {"event": "Excessive Heat Warning", "sender_name": "NWS"},
            {"event": "Wind Advisory"},
        ]});
        assert_eq!(
            parse_official_alerts(&body),
            vec!["Excessive Heat Warning (NWS)".to_string(), "Wind Advisory".to_string()]
        );
        assert!(parse_official_alerts(&json!({"daily": []})).is_empty());
    }

    /// An official alert OUTRANKS a derived threshold: when the provider says
    /// something, the finding is labelled official and the derived disclaimer
    /// does not appear.
    #[tokio::test]
    async fn an_official_alert_is_preferred_and_labelled_official() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(Some("Examplecity"));
        let fc = FakeForecast::ok(vec![hot("2026-08-01", 35.0, 24.0), hot("2026-08-02", 35.0, 24.0)])
            .with_official(vec!["Excessive Heat Warning (met service)"]);

        let r = run_watch(2, operator(), &cal, &loc, &fc, today()).await;
        assert_eq!(r.heat.findings[0].provenance, Provenance::Official);
        let text = r.render();
        assert!(text.contains("Official alert: Excessive Heat Warning"), "{text}");
        assert!(!text.contains("DERIVED"), "an official finding is not labelled derived: {text}");
    }

    /// REVIEW FINDING (gpt56, round 2): the requested horizon bound TRAVEL but
    /// not HEAT, so `days=1` could report a heat run occurring days outside the
    /// "next 1 day" the report claimed to cover — an answer contradicting its
    /// own header.
    #[tokio::test]
    async fn the_horizon_bounds_the_heat_watch_too_not_just_travel() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        // The heat wave is on 08-04/08-05 — real, but OUTSIDE a 2-day horizon.
        let fc = FakeForecast::ok(vec![
            mild("2026-08-01"),
            mild("2026-08-02"),
            mild("2026-08-03"),
            hot("2026-08-04", 36.0, 24.0),
            hot("2026-08-05", 36.0, 24.0),
        ]);

        let short = run_watch(2, operator(), &cal, &loc, &fc, today()).await;
        assert!(
            short.heat.findings.is_empty(),
            "a heat run beyond the requested horizon must not be reported as within it"
        );
        assert!(short.heat.is_clear());

        // The control: widen the horizon to cover it and it IS reported — so the
        // assertion above is about the horizon, not about the fixture.
        let wide = run_watch(5, operator(), &cal, &loc, &fc, today()).await;
        assert_eq!(wide.heat.findings.len(), 1, "within a 5-day horizon it is real");
        assert_eq!(wide.heat.findings[0].run.as_ref().unwrap().days.len(), 2);
    }

    /// REVIEW FINDING (gpt56, round 2): an official heat alert was DROPPED
    /// whenever the derived thresholds did not independently fire — so a real
    /// met-office heat warning could be silently suppressed because this
    /// module's own numbers disagreed with it. The travel path never did this.
    #[tokio::test]
    async fn an_official_heat_alert_is_reported_even_when_derived_thresholds_do_not_fire() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        // Deliberately mild: nothing here trips HEAT_DAY_MAX_C/HEAT_NIGHT_MIN_C.
        let fc = FakeForecast::ok(vec![mild("2026-08-01"), mild("2026-08-02")])
            .with_official(vec!["Excessive Heat Warning (met service)"]);

        let r = run_watch(2, operator(), &cal, &loc, &fc, today()).await;

        assert_eq!(r.heat.findings.len(), 1, "an authoritative alert is never suppressed");
        let f = &r.heat.findings[0];
        assert!(f.run.is_none(), "there is no derived run — the alert is the whole basis");
        assert_eq!(f.provenance, Provenance::Official);

        let text = r.render();
        assert!(text.contains("official heat alert is in force"), "{text}");
        assert!(text.contains("Official alert: Excessive Heat Warning"), "{text}");
        assert!(!text.contains("DERIVED"), "an official-only finding is not derived: {text}");
        assert!(!text.contains("no sustained heat build-up"), "must not also claim clear: {text}");
        assert!(!text.contains("Homeplace"), "{text}");

        // The control: with no official alert the same mild forecast is clear —
        // so this is the alert doing the work, not the fixture.
        let quiet = FakeForecast::ok(vec![mild("2026-08-01"), mild("2026-08-02")]);
        let r2 = run_watch(2, operator(), &cal, &loc, &quiet, today()).await;
        assert!(r2.heat.findings.is_empty() && r2.heat.is_clear());
    }

    /// REVIEW FINDING (gpt56, round 3): a regression introduced by the round-2
    /// horizon fix itself. Once the forecast is filtered to the horizon, that
    /// filter can leave NOTHING — and the code still marked the home area
    /// "checked" and rendered an all-clear for a window it never looked at.
    #[tokio::test]
    async fn a_horizon_the_forecast_does_not_cover_is_not_an_all_clear() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(Some("Homeplace"));
        // The provider answers, but only about days BEFORE the horizon.
        let fc = FakeForecast::ok(vec![mild("2026-07-20"), mild("2026-07-21")]);

        let r = run_watch(3, operator(), &cal, &loc, &fc, today()).await;

        assert!(r.heat.findings.is_empty());
        assert!(!r.heat.is_clear(), "an uncovered window is unknown, not clear");
        assert!(r.heat.checked_nothing(), "nothing in the window was assessed");
        assert!(matches!(r.heat.gaps.as_slice(), [Gap::Failed(_)]));

        let text = r.render();
        assert!(text.contains("does not reach the days you asked about"), "{text}");
        assert!(text.contains("unknown, not clear"), "{text}");
        assert!(!text.contains("no sustained heat build-up"), "must not read as all-clear: {text}");

        // The control: the same shape of request WITH covering days is clear —
        // so this is the coverage gap doing the work, not the fixture.
        let covered = FakeForecast::ok(vec![mild("2026-08-01"), mild("2026-08-02")]);
        let r2 = run_watch(3, operator(), &cal, &loc, &covered, today()).await;
        assert!(r2.heat.is_clear());
        assert!(r2.render().contains("no sustained heat build-up"));
    }

    #[tokio::test]
    async fn the_horizon_is_clamped_and_reported() {
        let (cal, _) = CountingCalendar::with(vec![]);
        let (loc, _) = CountingLocations::with(None);
        let fc = FakeForecast::ok(vec![]);
        let r = run_watch(99, operator(), &cal, &loc, &fc, today()).await;
        assert_eq!(r.horizon_days, MAX_HORIZON_DAYS);
        assert!(r.render().contains(&format!("next {MAX_HORIZON_DAYS} days")));
    }

    /// The tool's own description must not name a city — the same trap that made
    /// `weather` answer for a city out of its schema.
    #[test]
    fn the_description_names_no_city() {
        let t = SevereWeatherWatch {
            cfg: crate::weather::WeatherConfig::for_test(),
        };
        let text = format!("{} {}", t.description(), t.parameters());
        for city in ["Tampa", "Paris", "San Jose", "Denver", "London", "New York"] {
            assert!(!text.contains(city), "the schema must name no city, found {city}");
        }
    }
}
