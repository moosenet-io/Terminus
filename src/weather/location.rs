//! Location resolution for the weather tool — calendar, then routine, then ASK.
//!
//! Operator requirement (2026-07-31): *"I'd like this to be built so lumina first
//! tries to reference the google calendar. then it should try to infer locations based
//! home and office routines, then ask."*
//!
//! ## Why this exists
//! The weather tool previously resolved a missing location from a single
//! `COMMUTE_HOME` env var, and its own JSON schema advertised
//! `"e.g. 'Tampa', 'Tampa, Florida', 'Paris'"` as the example. With `COMMUTE_HOME`
//! unset the tool failed, the model retried, and **it answered for Tampa** — copied
//! straight out of the schema example. An example in a schema becomes a default in
//! practice. Earlier the same gap produced "Foster City, San Jose".
//!
//! Inventing a location is the worst possible failure here: the answer is confidently
//! wrong and gives the user no signal that it is wrong. Asking is slightly slower and
//! always honest.
//!
//! ## Resolution order
//! 1. **Explicit** — the caller named a place. Always wins.
//! 2. **Calendar** — today's events carry locations; if the user is travelling, the
//!    weather they care about is where they will BE. Advisory: the answer says which
//!    location it used and why, so a wrong inference is visible and correctable.
//! 3. **Routine** — home/work from configuration, chosen by time of day.
//! 4. **Ask** — never guess.

use async_trait::async_trait;
use serde_json::Value;

/// Where today's calendar events come from.
///
/// This is the SEAM that keeps the weather tool from reaching Google itself.
/// The production implementation is [`crate::google::caldav::GoogleCalendarSource`],
/// which goes through the module that already owns Google access (CalDAV, Basic
/// auth, `GoogleConfig`) — the weather tool never builds an API client, never
/// reads a credential, and never sees `GOOGLE_APP_PASSWORD`.
///
/// Fallible by construction: an implementation returns an EMPTY list when the
/// calendar is unreachable, so a calendar outage degrades to routine→ask. A
/// missing calendar must never produce an invented location.
#[async_trait]
pub trait CalendarSource: Send + Sync {
    /// Events relevant to "now", each a JSON object with (optionally) `summary`,
    /// `location` and `status` — the shape [`from_calendar`] consumes.
    async fn events_now(&self) -> Vec<Value>;
}

/// The no-calendar implementation: used when Google is not configured.
///
/// Its existence is the explicit statement of the degradation path — with no
/// calendar the chain is routine→ask, never a guess.
pub struct NoCalendar;

#[async_trait]
impl CalendarSource for NoCalendar {
    async fn events_now(&self) -> Vec<Value> {
        Vec::new()
    }
}

/// Where a resolved location came from — carried so the answer can SAY which place it
/// used. A silently-substituted location is indistinguishable from a wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationSource {
    /// The caller named it.
    Explicit,
    /// Derived from a calendar event, with the event's summary for attribution.
    Calendar(String),
    /// A configured routine location (home/work).
    Routine(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Found { location: String, source: LocationSource },
    /// Nothing usable — the caller must ASK rather than invent one.
    AskUser,
}

impl Resolved {
    /// How the answer should attribute this location to the user. `None` for an
    /// explicit location (they already know what they asked for).
    pub fn attribution(&self) -> Option<String> {
        match self {
            Resolved::Found { source: LocationSource::Explicit, .. } => None,
            Resolved::Found { location, source: LocationSource::Calendar(sum) } => {
                Some(format!("using {location} — from your calendar ({sum})"))
            }
            Resolved::Found { location, source: LocationSource::Routine(which) } => {
                Some(format!("using your {which} location, {location}"))
            }
            Resolved::AskUser => None,
        }
    }
}

/// The message returned when no location can be resolved.
///
/// Deliberately a QUESTION, not an error: the user asked a reasonable thing and the
/// assistant simply needs one more fact. It must never read as a tool malfunction, and
/// it must never suggest a specific city — suggesting one is how "Tampa" happened.
pub const ASK_MESSAGE: &str =
    "I don't know which location you mean. Tell me the city and I'll check — \
     or say \"remember this is home\" and I'll use it from now on.";

/// Extract a usable location string from one calendar event.
///
/// Skips virtual-meeting "locations" (a Zoom link is not a place) — treating one as a
/// location would produce a confidently wrong forecast for a URL.
pub fn location_from_event(event: &Value) -> Option<String> {
    let loc = event.get("location").and_then(Value::as_str)?.trim();
    if loc.is_empty() {
        return None;
    }
    let l = loc.to_lowercase();
    // Both the URL form AND the prose form. A calendar's `location` for a video call
    // is usually written by a human or an add-in ("Microsoft Teams Meeting"), not as a
    // bare URL — my first list only covered URLs and would have sent a Teams meeting
    // to the weather API as though it were a city. Caught by this module's own test.
    let virtual_markers = [
        // URL / host forms
        "http://", "https://", "zoom.", "meet.google", "teams.microsoft", "webex.",
        "whereby.", "gotomeet", "bluejeans",
        // prose forms an invite actually writes
        "teams meeting", "microsoft teams", "google meet", "zoom meeting", "webex meeting",
        "hangout", "video call", "virtual", "online", "remote",
        // dial-in / placeholder forms
        "phone", "dial-in", "dial in", "conference call", "call-in",
        "tbd", "tba", "n/a", "none",
    ];
    if virtual_markers.iter().any(|m| l.contains(m)) {
        return None;
    }
    Some(loc.to_string())
}

/// Pick a location from today's calendar events, preferring the earliest that has a
/// real place. Returns the location and the event summary for attribution.
pub fn from_calendar(events: &[Value]) -> Option<(String, String)> {
    for ev in events {
        // A declined or cancelled event is not where the user will be.
        let status = ev.get("status").and_then(Value::as_str).unwrap_or("");
        if status.eq_ignore_ascii_case("cancelled") || status.eq_ignore_ascii_case("declined") {
            continue;
        }
        if let Some(loc) = location_from_event(ev) {
            let summary = ev
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("an event")
                .to_string();
            return Some((loc, summary));
        }
    }
    None
}

/// Routine locations from configuration. Non-secret addresses.
#[derive(Debug, Clone, Default)]
pub struct Routine {
    pub home: Option<String>,
    pub work: Option<String>,
}

impl Routine {
    pub fn from_env() -> Self {
        let get = |k: &str| {
            std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        Self { home: get("COMMUTE_HOME"), work: get("COMMUTE_WORK") }
    }

    /// Pick home or work by time of day.
    ///
    /// `hour_local` is 0..=23. Work hours are a WEAKER signal than the calendar (which
    /// knows where you actually are), which is why this sits below it in the order.
    /// Falls back to whichever is configured when only one is.
    pub fn pick(&self, hour_local: u32, is_weekday: bool) -> Option<(String, &'static str)> {
        let at_work = is_weekday && (9..=17).contains(&hour_local);
        match (at_work, &self.work, &self.home) {
            (true, Some(w), _) => Some((w.clone(), "work")),
            (_, _, Some(h)) => Some((h.clone(), "home")),
            (_, Some(w), None) => Some((w.clone(), "work")),
            _ => None,
        }
    }
}

/// The full chain.
pub fn resolve(
    explicit: Option<&str>,
    calendar_events: &[Value],
    routine: &Routine,
    hour_local: u32,
    is_weekday: bool,
) -> Resolved {
    if let Some(e) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Resolved::Found { location: e.to_string(), source: LocationSource::Explicit };
    }
    if let Some((loc, summary)) = from_calendar(calendar_events) {
        return Resolved::Found { location: loc, source: LocationSource::Calendar(summary) };
    }
    if let Some((loc, which)) = routine.pick(hour_local, is_weekday) {
        return Resolved::Found { location: loc, source: LocationSource::Routine(which) };
    }
    Resolved::AskUser
}

/// The full chain, fetching the calendar through the sanctioned seam.
///
/// This is what the weather tool actually calls (`WeatherTool::resolve_location`);
/// `resolve` above is the pure core it delegates to. Keeping the fetch here — and
/// SHORT-CIRCUITING it when an explicit location was given — means a named
/// location never costs a calendar round-trip.
pub async fn resolve_with_calendar(
    explicit: Option<&str>,
    calendar: &dyn CalendarSource,
    routine: &Routine,
    hour_local: u32,
    is_weekday: bool,
) -> Resolved {
    if let Some(e) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Resolved::Found { location: e.to_string(), source: LocationSource::Explicit };
    }
    let events = calendar.events_now().await;
    resolve(None, &events, routine, hour_local, is_weekday)
}

/// The local hour (0..=23) and weekday-ness used for routine inference.
///
/// Reads the process-local timezone (`chrono::Local`, i.e. `TZ`/`/etc/localtime`)
/// rather than UTC: "is it a workday morning?" is a question about the user's
/// clock, and a UTC answer picks the wrong routine for most of the world.
pub fn local_hour_and_weekday() -> (u32, bool) {
    use chrono::{Datelike, Timelike, Weekday};
    let now = chrono::Local::now();
    let weekday = !matches!(now.weekday(), Weekday::Sat | Weekday::Sun);
    (now.hour(), weekday)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn routine(home: Option<&str>, work: Option<&str>) -> Routine {
        Routine {
            home: home.map(str::to_string),
            work: work.map(str::to_string),
        }
    }

    #[test]
    fn an_explicit_location_always_wins() {
        let evs = vec![json!({"summary": "Trip", "location": "Denver"})];
        let r = resolve(Some("Paris"), &evs, &routine(Some("Omaha"), None), 10, true);
        assert_eq!(
            r,
            Resolved::Found { location: "Paris".into(), source: LocationSource::Explicit }
        );
        assert!(r.attribution().is_none(), "no need to explain a location they just gave");
    }

    #[test]
    fn the_calendar_beats_the_routine() {
        // The whole point: if you are travelling, the weather you care about is where
        // you will BE, not where you live.
        let evs = vec![json!({"summary": "Client onsite", "location": "Denver, CO"})];
        let r = resolve(None, &evs, &routine(Some("Omaha"), None), 10, true);
        match &r {
            Resolved::Found { location, source: LocationSource::Calendar(s) } => {
                assert_eq!(location, "Denver, CO");
                assert_eq!(s, "Client onsite");
            }
            other => panic!("expected a calendar hit, got {other:?}"),
        }
        // ...and it SAYS so, so a wrong inference is visible.
        assert!(r.attribution().unwrap().contains("calendar"));
    }

    #[test]
    fn a_video_call_is_not_a_place() {
        // Treating a Zoom link as a location yields a confidently wrong forecast.
        for virt in [
            "https://zoom.us/j/123",
            "Microsoft Teams Meeting",
            "meet.google.com/abc",
            "Phone",
            "TBD",
        ] {
            let evs = vec![json!({"summary": "Sync", "location": virt})];
            let r = resolve(None, &evs, &routine(Some("Omaha"), None), 10, true);
            match r {
                Resolved::Found { source: LocationSource::Routine(_), .. } => {}
                other => panic!("{virt} must not be used as a place, got {other:?}"),
            }
        }
    }

    #[test]
    fn cancelled_and_declined_events_are_skipped() {
        let evs = vec![
            json!({"summary": "Cancelled trip", "location": "Denver", "status": "cancelled"}),
            json!({"summary": "Real trip", "location": "Austin"}),
        ];
        match resolve(None, &evs, &routine(Some("Omaha"), None), 10, true) {
            Resolved::Found { location, .. } => assert_eq!(location, "Austin"),
            other => panic!("expected Austin, got {other:?}"),
        }
    }

    #[test]
    fn the_routine_picks_work_during_working_hours() {
        let r = resolve(None, &[], &routine(Some("Home St"), Some("Office Rd")), 11, true);
        match &r {
            Resolved::Found { location, source: LocationSource::Routine(w) } => {
                assert_eq!(location, "Office Rd");
                assert_eq!(*w, "work");
            }
            other => panic!("got {other:?}"),
        }
        assert!(r.attribution().unwrap().contains("work"));
    }

    #[test]
    fn the_routine_picks_home_outside_working_hours_and_at_weekends() {
        for (hour, weekday) in [(7u32, true), (20, true), (11, false)] {
            match resolve(None, &[], &routine(Some("Home St"), Some("Office Rd")), hour, weekday) {
                Resolved::Found { location, .. } => assert_eq!(location, "Home St"),
                other => panic!("hour={hour} weekday={weekday}: {other:?}"),
            }
        }
    }

    #[test]
    fn with_nothing_configured_it_ASKS_rather_than_inventing() {
        // THE bug this module exists for. With no location, no calendar and no
        // routine, the old path failed and the model answered for Tampa — the first
        // example in the tool's own schema.
        let r = resolve(None, &[], &routine(None, None), 10, true);
        assert_eq!(r, Resolved::AskUser);
    }

    #[test]
    fn the_ask_message_names_no_city() {
        // Suggesting a city is precisely how Tampa (and Foster City) happened.
        let m = ASK_MESSAGE.to_lowercase();
        for city in ["tampa", "paris", "omaha", "san francisco", "foster city", "new york"] {
            assert!(!m.contains(city), "the ask must not seed a city, found {city:?}");
        }
        assert!(m.contains("which location") || m.contains("don't know which"));
    }

    #[test]
    fn an_empty_or_whitespace_explicit_location_falls_through() {
        // "" must not be treated as a real answer.
        let r = resolve(Some("   "), &[], &routine(Some("Home St"), None), 10, true);
        match r {
            Resolved::Found { location, source: LocationSource::Routine(_) } => {
                assert_eq!(location, "Home St")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn an_event_with_no_location_is_skipped() {
        let evs = vec![json!({"summary": "Focus time"})];
        match resolve(None, &evs, &routine(Some("Home St"), None), 10, true) {
            Resolved::Found { source: LocationSource::Routine(_), .. } => {}
            other => panic!("got {other:?}"),
        }
    }
}
