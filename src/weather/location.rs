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
//!
//! ## Whose calendar? Whose home? (TRTR-05 privacy gate)
//! Steps 2 and 3 are **process-global, not per-caller**: `events_now()` reads the
//! OPERATOR's Google calendar and the routine reads the OPERATOR's configured home
//! and work addresses. Terminus is a multi-principal gateway — a houseguest with a
//! guest grant can call `weather` — so resolving an omitted location for whoever
//! happens to be asking would answer a guest's "what's the weather?" with the
//! operator's whereabouts, attributed out loud ("using <place> — from your calendar
//! (<event summary>)"). An appointment summary and its address are among the most
//! sensitive things this fleet holds.
//!
//! So both inference steps are gated on [`crate::tool::CallerContext`], which the
//! gateway derives from the server-verified principal. A caller who is not
//! positively entitled to a source SKIPS it, and skipping every source lands on
//! **ask** — which is exactly the behaviour this module was already built to make
//! safe and honest. An explicit location is unaffected: it never touched either
//! source in the first place, so the legitimate guest use ("weather in Paris?")
//! works identically.
//!
//! The asymmetry that decides every ambiguous case: a spurious "which location did
//! you mean?" costs one turn; a leaked home address cannot be taken back. Hence
//! `CallerContext::untrusted()` — the value produced by an absent, unauthenticated
//! or unrecognised principal, and by any dispatch path that does not thread one —
//! grants nothing.

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::CallerContext;

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

    /// Events overlapping the inclusive date window `[start, end]`, for a
    /// consumer that must look FORWARD rather than at today (WXLOC-04's
    /// severe-weather travel watch).
    ///
    /// ## Why this returns [`CalendarWindow`] rather than `Vec<Value>`
    ///
    /// [`events_now`](CalendarSource::events_now) fails SOFT — an unreachable
    /// calendar yields an empty list — and that is exactly right for location
    /// resolution, where the fallback is another honest step (routine→ask).
    ///
    /// It is exactly WRONG for a watch. "Your calendar shows no travel in the
    /// next three days" and "I could not read your calendar" are different
    /// answers, and collapsing them produces the one failure mode a severe
    /// weather feature must never have: a confident all-clear that was never
    /// checked. So this window is a three-way answer — events, configured but
    /// unavailable, or not configured at all — and the caller is forced to
    /// decide what each means.
    ///
    /// The DEFAULT is [`CalendarWindow::NotConfigured`], not an empty event
    /// list, so an implementation that only knows about today (or a future one
    /// that forgets to override this) degrades to "could not check" rather than
    /// to a silent all-clear.
    async fn events_between(
        &self,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> CalendarWindow {
        let _ = (start, end);
        CalendarWindow::NotConfigured
    }
}

/// The answer to a forward-looking calendar query — see
/// [`CalendarSource::events_between`] for why this is not just a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarWindow {
    /// The calendar was read. An EMPTY vec here genuinely means "nothing
    /// scheduled", and only here.
    Events(Vec<Value>),
    /// A calendar is configured but could not be read (network, auth, parse).
    /// Carries a short reason for the user-facing "could not check" line.
    Unavailable(String),
    /// No calendar is configured at all.
    NotConfigured,
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

    /// Explicit rather than inherited: with no calendar configured a watch has
    /// not checked anything, and must say so.
    async fn events_between(
        &self,
        _start: chrono::NaiveDate,
        _end: chrono::NaiveDate,
    ) -> CalendarWindow {
        CalendarWindow::NotConfigured
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

/// Routine locations. Non-secret addresses, but sensitive ones.
///
/// LOCREG-01: this is now the WEATHER-SHAPED VIEW of the shared location
/// registry ([`crate::locations`]), not a pair of env vars. Build it with
/// [`Routine::resolve_for`] on the dispatch path; [`Routine::from_env`] survives
/// only as the legacy input that feeds into it.
#[derive(Debug, Clone, Default)]
pub struct Routine {
    pub home: Option<String>,
    pub work: Option<String>,
    /// Where the caller says they are RIGHT NOW — the registry's `current`
    /// entry, typically temporary. Outranks home/work: a traveller's weather is
    /// where they are, and because the entry expires, the override cannot
    /// outlive the trip.
    pub current: Option<String>,
}

/// A routine plus whether the registry could actually be read.
///
/// The flag is the whole reason this is a struct and not a bare `Routine`:
/// "you have no home saved" and "I couldn't read your saved locations" must not
/// collapse into one answer, and a `Routine` with `home: None` cannot tell them
/// apart. See [`crate::locations::Lookup`].
#[derive(Debug, Clone, Default)]
pub struct RoutineResolution {
    pub routine: Routine,
    /// `true` when the registry exists but could not be read. NOT "empty".
    pub degraded: bool,
}

/// Env var naming the ONE principal the legacy `COMMUTE_*` fallback applies to.
///
/// See [`Routine::resolve_for`] for why the fallback is scoped to a single
/// principal rather than applied to everyone.
pub const LEGACY_PRINCIPAL_ENV: &str = "TERMINUS_COMMUTE_LEGACY_PRINCIPAL";

/// Default legacy principal: the assistant service. Today, per TERM #577, every
/// human talking to Lumina arrives as that one identity — so this default
/// reproduces exactly today's behaviour and widens nothing.
pub const DEFAULT_LEGACY_PRINCIPAL: &str = "lumina";

fn legacy_principal() -> String {
    std::env::var(LEGACY_PRINCIPAL_ENV)
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_LEGACY_PRINCIPAL.to_string())
}

impl Routine {
    /// The LEGACY input: `COMMUTE_HOME` / `COMMUTE_WORK`.
    ///
    /// Kept as a fallback so nothing regresses for the operator on the day this
    /// ships, and deliberately NOT the primary source any more — see
    /// [`Routine::resolve_for`].
    pub fn from_env() -> Self {
        let get = |k: &str| {
            std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        Self { home: get("COMMUTE_HOME"), work: get("COMMUTE_WORK"), current: None }
    }

    /// Build this caller's routine from the shared registry, with the legacy
    /// env vars as a narrow fallback. **This is the LOCREG-01 wiring point.**
    ///
    /// ## Precedence, and why it is this way round
    ///
    /// 1. **The registry**, per caller. What the user told the assistant to
    ///    remember beats what was configured on a host years ago — otherwise
    ///    "I've moved" would appear to work and change nothing.
    /// 2. **`COMMUTE_HOME` / `COMMUTE_WORK`**, but ONLY for the single principal
    ///    named by [`LEGACY_PRINCIPAL_ENV`], and only for a slot the registry
    ///    leaves empty.
    ///
    /// The scoping in (2) is the load-bearing part. Those env vars are
    /// PROCESS-GLOBAL and hold the OPERATOR's own addresses; applying them to
    /// every caller would hand one person's home address to whoever else is
    /// entitled — the exact failure class fixed twice already this sprint. They
    /// are a migration bridge for one identity, not a default for everyone, and
    /// the moment that identity saves a `home` the registry wins and the bridge
    /// stops mattering.
    ///
    /// Today, per TERM #577, that one identity is also the only one any human
    /// reaches this code through, so this reproduces current behaviour exactly.
    /// **When #577 closes this needs re-examining, not carrying forward**: a
    /// per-person key will no longer match the legacy principal's record, and
    /// the right answer is almost certainly to retire the env fallback rather
    /// than re-point it.
    ///
    /// An unentitled caller gets an EMPTY routine and the store is never
    /// touched — `crate::locations` makes that decision before any read.
    pub fn resolve_for(
        store: &dyn crate::locations::store::LocationStore,
        key: Option<&crate::locations::CallerKey>,
        caller: CallerContext,
        legacy: &Routine,
    ) -> RoutineResolution {
        Self::resolve_for_legacy_principal(store, key, caller, legacy, &legacy_principal())
    }

    /// [`Routine::resolve_for`] with the legacy principal passed explicitly.
    ///
    /// Exists so the tests can exercise the fallback's SCOPING without mutating
    /// process env — `std::env::set_var` from a parallel test is a race, and the
    /// property under test here ("one principal, not everyone") is exactly the
    /// kind that a flaky test would quietly stop enforcing.
    pub fn resolve_for_legacy_principal(
        store: &dyn crate::locations::store::LocationStore,
        key: Option<&crate::locations::CallerKey>,
        caller: CallerContext,
        legacy: &Routine,
        legacy_principal: &str,
    ) -> RoutineResolution {
        use crate::locations::{self, Listing};

        let (mut home, mut work, mut current) = (None, None, None);
        let mut degraded = false;

        match locations::list(store, key, caller) {
            Listing::Entries { live, .. } => {
                for (name, entry) in live {
                    match name.as_str() {
                        locations::HOME => home = Some(entry.value),
                        locations::WORK => work = Some(entry.value),
                        locations::CURRENT => current = Some(entry.value),
                        // A user-chosen name ("the cabin") is real registry data
                        // and reachable by NAME through `locations::lookup`; it
                        // just has no slot in the home/work routine, which is
                        // what this view is for.
                        _ => {}
                    }
                }
            }
            // No entitlement, or no identity: nothing was read and nothing is
            // known. Not degraded — this is a correct, complete answer.
            Listing::Denied => {
                return RoutineResolution { routine: Routine::default(), degraded: false }
            }
            Listing::Unavailable(_) => degraded = true,
        }

        // The legacy bridge, for one principal, filling only empty slots.
        let is_legacy = key.map(|k| k.principal() == legacy_principal).unwrap_or(false);
        if is_legacy && !degraded {
            home = home.or_else(|| legacy.home.clone());
            work = work.or_else(|| legacy.work.clone());
        }

        RoutineResolution { routine: Routine { home, work, current }, degraded }
    }

    /// Pick the routine location: current, else work during working hours, else
    /// home.
    ///
    /// `hour_local` is 0..=23. Work hours are a WEAKER signal than the calendar (which
    /// knows where you actually are), which is why this sits below it in the order.
    /// Falls back to whichever is configured when only one is.
    pub fn pick(&self, hour_local: u32, is_weekday: bool) -> Option<(String, &'static str)> {
        // A live `current` entry is the user saying, in as many words, where
        // they are — no inference beats that.
        if let Some(c) = &self.current {
            return Some((c.clone(), "current"));
        }
        let at_work = is_weekday && (9..=17).contains(&hour_local);
        match (at_work, &self.work, &self.home) {
            (true, Some(w), _) => Some((w.clone(), "work")),
            (_, _, Some(h)) => Some((h.clone(), "home")),
            (_, Some(w), None) => Some((w.clone(), "work")),
            _ => None,
        }
    }
}

/// What to say when the location could not be resolved AND the registry could
/// not be read.
///
/// Distinct from [`ASK_MESSAGE`] on purpose: that one means "I don't know where
/// you mean", this one means "I couldn't check what you've told me". Reporting
/// a read failure as an empty registry would quietly teach the user that
/// nothing is saved, and would invite exactly the confident guess this module
/// exists to prevent.
pub const REGISTRY_UNAVAILABLE_MESSAGE: &str =
    "I couldn't read your saved locations just now, so I can't tell whether you have one set. \
     Tell me the city and I'll check the weather for it.";

/// The full chain, for a caller whose entitlement to each source of OPERATOR
/// context is already decided (see this module's privacy note).
///
/// `caller` gates steps 2 and 3 INDEPENDENTLY. The calendar check is repeated
/// here even though [`resolve_with_calendar`] already declines to FETCH events
/// for an unentitled caller: this function is public and pure, and a future
/// caller that hands it an events slice from somewhere else must not be able to
/// route operator data through it just because it skipped the fetch gate. Two
/// cheap checks, no way in.
pub fn resolve(
    explicit: Option<&str>,
    calendar_events: &[Value],
    routine: &Routine,
    hour_local: u32,
    is_weekday: bool,
    caller: CallerContext,
) -> Resolved {
    if let Some(e) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Resolved::Found { location: e.to_string(), source: LocationSource::Explicit };
    }
    if caller.may_infer_from_calendar() {
        if let Some((loc, summary)) = from_calendar(calendar_events) {
            return Resolved::Found { location: loc, source: LocationSource::Calendar(summary) };
        }
    }
    if caller.may_infer_from_routine() {
        if let Some((loc, which)) = routine.pick(hour_local, is_weekday) {
            return Resolved::Found { location: loc, source: LocationSource::Routine(which) };
        }
    }
    Resolved::AskUser
}

/// The full chain, fetching the calendar through the sanctioned seam.
///
/// This is what the weather tool actually calls (`WeatherTool::resolve_location`);
/// `resolve` above is the pure core it delegates to. Keeping the fetch here — and
/// SHORT-CIRCUITING it when an explicit location was given — means a named
/// location never costs a calendar round-trip.
///
/// TRTR-05: the fetch is ALSO short-circuited for a caller not entitled to the
/// operator's calendar. Not fetching (rather than fetching and then discarding)
/// is the stronger property and the one the tests assert on: an unentitled
/// caller must not cause a read of the operator's calendar at all, so the data
/// is never in this process's memory on their behalf — nothing to leak through a
/// log line, an error message or a later refactor.
pub async fn resolve_with_calendar(
    explicit: Option<&str>,
    calendar: &dyn CalendarSource,
    routine: &Routine,
    hour_local: u32,
    is_weekday: bool,
    caller: CallerContext,
) -> Resolved {
    if let Some(e) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Resolved::Found { location: e.to_string(), source: LocationSource::Explicit };
    }
    let events = if caller.may_infer_from_calendar() {
        calendar.events_now().await
    } else {
        Vec::new()
    };
    resolve(None, &events, routine, hour_local, is_weekday, caller)
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
            current: None,
        }
    }

    /// A caller entitled to BOTH sources of operator context — what the gateway
    /// derives for the operator's own identity (it is allowed
    /// `google_calendar_today` and `commute_estimate` directly).
    ///
    /// TRTR-05: an entitled `CallerContext` can only be minted inside
    /// `crate::gateway_framework`, so tests reach it through the `cfg(test)`-only
    /// `entitled_for_test_only` affordance rather than through any production
    /// constructor — there is no longer a public one to widen.
    fn operator() -> CallerContext {
        CallerContext::entitled_for_test_only(true, true)
    }

    /// A household guest: allowed to call `weather`, entitled to neither source.
    /// Identical to `CallerContext::untrusted()` — which is the point: the
    /// unauthenticated/unknown caller and the known-but-unentitled caller are
    /// treated exactly alike, so there is no third, softer path.
    fn guest() -> CallerContext {
        CallerContext::default()
    }

    /// A calendar that COUNTS reads, so a test can assert the operator's
    /// calendar was never touched — not merely that its data didn't appear in
    /// the answer.
    struct CountingCalendar {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        events: Vec<Value>,
    }

    #[async_trait]
    impl CalendarSource for CountingCalendar {
        async fn events_now(&self) -> Vec<Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.events.clone()
        }
    }

    /// The operator's calendar as it realistically looks: an event whose SUMMARY
    /// is itself sensitive (what the appointment is) and whose location is a
    /// street address. Placeholders only — never a real name or address.
    fn sensitive_events() -> Vec<Value> {
        vec![json!({
            "summary": "Dentist appointment — 000 Placeholder St", // pii-test-fixture: obvious placeholder, this test asserts it NEVER reaches a guest
            "location": "000 Placeholder St, Examplecity", // pii-test-fixture: obvious placeholder standing in for a home/appointment address
        })]
    }

    #[test]
    fn an_explicit_location_always_wins() {
        let evs = vec![json!({"summary": "Trip", "location": "Denver"})];
        let r = resolve(Some("Paris"), &evs, &routine(Some("Omaha"), None), 10, true, operator());
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
        let r = resolve(None, &evs, &routine(Some("Omaha"), None), 10, true, operator());
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
            let r = resolve(None, &evs, &routine(Some("Omaha"), None), 10, true, operator());
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
        match resolve(None, &evs, &routine(Some("Omaha"), None), 10, true, operator()) {
            Resolved::Found { location, .. } => assert_eq!(location, "Austin"),
            other => panic!("expected Austin, got {other:?}"),
        }
    }

    #[test]
    fn the_routine_picks_work_during_working_hours() {
        let r = resolve(None, &[], &routine(Some("Home St"), Some("Office Rd")), 11, true, operator());
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
            match resolve(None, &[], &routine(Some("Home St"), Some("Office Rd")), hour, weekday, operator()) {
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
        let r = resolve(None, &[], &routine(None, None), 10, true, operator());
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
        let r = resolve(Some("   "), &[], &routine(Some("Home St"), None), 10, true, operator());
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
        match resolve(None, &evs, &routine(Some("Home St"), None), 10, true, operator()) {
            Resolved::Found { source: LocationSource::Routine(_), .. } => {}
            other => panic!("got {other:?}"),
        }
    }

    // ── TRTR-05 privacy gate ────────────────────────────────────────────────
    // The leak these exist for: `weather` is granted to household GUESTS, and
    // without a caller gate a guest asking "what's the weather?" is answered
    // with the OPERATOR's whereabouts — attributed out loud, event summary and
    // all. Assert on the SUMMARY specifically: it is the most sensitive part,
    // and it is the part the attribution string quotes verbatim.

    #[tokio::test]
    async fn a_guest_who_omits_the_location_never_reads_the_operators_calendar() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cal = CountingCalendar { calls: calls.clone(), events: sensitive_events() };
        let r = resolve_with_calendar(
            None,
            &cal,
            &routine(Some("000 Placeholder St"), Some("111 Placeholder Ave")), // pii-test-fixture: obvious placeholders standing in for the operator's home/work addresses
            10,
            true,
            guest(),
        )
        .await;

        assert_eq!(r, Resolved::AskUser, "a guest with no location must be ASKED");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the operator's calendar must not be READ on a guest's behalf at all"
        );
        assert!(r.attribution().is_none(), "no attribution derived from operator data");
    }

    #[tokio::test]
    async fn a_guests_explicit_location_still_works_and_costs_no_calendar_read() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cal = CountingCalendar { calls: calls.clone(), events: sensitive_events() };
        let r =
            resolve_with_calendar(Some("Paris"), &cal, &routine(None, None), 10, true, guest()).await;
        assert_eq!(
            r,
            Resolved::Found { location: "Paris".into(), source: LocationSource::Explicit },
            "the legitimate guest use — a named place — must be unchanged"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// POSITIVE CONTROL. Without this, "guest gets asked" would also pass if the
    /// fix had simply disabled inference for everybody.
    #[tokio::test]
    async fn the_operator_still_gets_the_calendar_chain_and_the_attribution() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cal = CountingCalendar { calls: calls.clone(), events: sensitive_events() };
        let r = resolve_with_calendar(None, &cal, &routine(None, None), 10, true, operator()).await;
        match &r {
            Resolved::Found { location, source: LocationSource::Calendar(summary) } => {
                assert!(location.contains("Placeholder St"));
                assert!(summary.contains("Dentist"));
            }
            other => panic!("the operator must still get the calendar chain, got {other:?}"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(r.attribution().unwrap().contains("calendar"));
    }

    #[test]
    fn an_unentitled_caller_cannot_route_events_through_the_pure_resolver() {
        // `resolve` is public: the fetch gate is not the only door. Handing it
        // events directly must not work either.
        let r = resolve(None, &sensitive_events(), &routine(Some("000 Placeholder St"), None), 10, true, guest()); // pii-test-fixture: obvious placeholder home address
        assert_eq!(r, Resolved::AskUser);
    }

    #[test]
    fn the_two_sources_are_gated_independently() {
        // Entitled to the routine but not the calendar: the calendar event is
        // ignored, the routine still answers. (And the converse.)
        let routine_only = CallerContext::entitled_for_test_only(false, true);
        match resolve(None, &sensitive_events(), &routine(Some("Home St"), None), 10, true, routine_only) {
            Resolved::Found { location, source: LocationSource::Routine(_) } => {
                assert_eq!(location, "Home St")
            }
            other => panic!("got {other:?}"),
        }
        let calendar_only = CallerContext::entitled_for_test_only(true, false);
        match resolve(None, &[], &routine(Some("Home St"), None), 10, true, calendar_only) {
            Resolved::AskUser => {}
            other => panic!("no calendar hit and no routine entitlement must ASK, got {other:?}"),
        }
    }

    // ── LOCREG-01: the registry as the routine tier ─────────────────────────
    //
    // Fixtures are obvious placeholders. This repo publishes a PII-scrubbed
    // public mirror and a "realistic" address in a test is indistinguishable
    // from a real one to anyone reading it later.

    mod registry {
        use super::*;
        use crate::locations::store::fake::{BrokenStore, CountingStore};
        use crate::locations::store::LocationStore as _;
        use crate::locations::{self, CallerKey};

        const STORED_HOME: &str = "1 Placeholder Way, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a saved home address
        const LEGACY_HOME: &str = "9 Legacy Lane, Examplecity"; // pii-test-fixture: obvious placeholder standing in for the operator's COMMUTE_HOME
        const OTHER_HOME: &str = "2 Otherplace Road, Examplecity"; // pii-test-fixture: obvious placeholder standing in for another caller's home address

        fn key(name: &str) -> CallerKey {
            CallerKey::for_principal_name(name).unwrap()
        }

        fn legacy() -> Routine {
            Routine { home: Some(LEGACY_HOME.into()), work: None, current: None }
        }

        fn seed(store: &CountingStore, k: &CallerKey, name: &str, value: &str, hours: Option<i64>) {
            match locations::set(store, Some(k), operator(), name, value, hours, true) {
                locations::WriteOutcome::Stored { .. } => {}
                other => panic!("seed failed: {other:?}"),
            }
        }

        /// POSITIVE CONTROL for the whole wiring: a caller with a stored home
        /// gets it back through the routine tier. An implementation that always
        /// answered "not set" would pass every negative test below and fail this.
        #[test]
        fn a_stored_home_resolves_through_the_routine_tier() {
            let s = CountingStore::new();
            let k = key("alpha");
            seed(&s, &k, locations::HOME, STORED_HOME, None);

            let r = Routine::resolve_for(&s, Some(&k), operator(), &Routine::default());
            assert!(!r.degraded);
            match resolve(None, &[], &r.routine, 20, true, operator()) {
                Resolved::Found { location, source: LocationSource::Routine(w) } => {
                    assert_eq!(location, STORED_HOME);
                    assert_eq!(w, "home");
                }
                other => panic!("expected the stored home, got {other:?}"),
            }
        }

        #[test]
        fn the_registry_beats_the_legacy_env_vars() {
            let s = CountingStore::new();
            let k = key("alpha");
            seed(&s, &k, locations::HOME, STORED_HOME, None);

            let r = Routine::resolve_for_legacy_principal(&s, Some(&k), operator(), &legacy(), "alpha");
            assert_eq!(r.routine.home.as_deref(), Some(STORED_HOME), "'I've moved' must actually take effect");
        }

        #[test]
        fn the_legacy_env_fallback_still_resolves_when_the_registry_is_empty() {
            // The no-regression case: the operator has COMMUTE_HOME set and
            // nothing saved yet. Weather must keep working exactly as before.
            let s = CountingStore::new();
            let r = Routine::resolve_for_legacy_principal(&s, Some(&key("alpha")), operator(), &legacy(), "alpha");
            assert_eq!(r.routine.home.as_deref(), Some(LEGACY_HOME));
            match resolve(None, &[], &r.routine, 20, true, operator()) {
                Resolved::Found { location, .. } => assert_eq!(location, LEGACY_HOME),
                other => panic!("got {other:?}"),
            }
        }

        #[test]
        fn the_legacy_env_fallback_is_scoped_to_one_principal() {
            // The operator's process-global COMMUTE_HOME must not become
            // everybody else's home the moment they are entitled.
            let s = CountingStore::new();
            let r = Routine::resolve_for_legacy_principal(&s, Some(&key("bravo")), operator(), &legacy(), "alpha");
            assert_eq!(r.routine.home, None, "another principal must not inherit COMMUTE_HOME");
            assert!(!format!("{r:?}").contains("Legacy"));
        }

        #[test]
        fn one_callers_registry_home_is_invisible_to_another() {
            let s = CountingStore::new();
            seed(&s, &key("bravo"), locations::HOME, OTHER_HOME, None);
            let r = Routine::resolve_for(&s, Some(&key("alpha")), operator(), &Routine::default());
            assert_eq!(r.routine.home, None);
            assert!(!format!("{r:?}").contains("Otherplace"), "another caller's home leaked");
        }

        #[test]
        fn an_unentitled_caller_causes_no_registry_read_and_gets_asked() {
            let s = CountingStore::new();
            let k = key("alpha");
            seed(&s, &k, locations::HOME, STORED_HOME, None);
            let reads_before = s.reads();

            let r = Routine::resolve_for_legacy_principal(&s, Some(&k), guest(), &legacy(), "alpha");
            assert_eq!(s.reads(), reads_before, "an unentitled caller must cause ZERO registry reads");
            assert!(!r.degraded, "a refusal is not a degradation");
            assert_eq!(r.routine.home, None);
            assert_eq!(resolve(None, &[], &r.routine, 20, true, guest()), Resolved::AskUser);
            assert!(!format!("{r:?}").contains("Placeholder"));
        }

        #[test]
        fn a_live_current_location_outranks_home_and_work() {
            let s = CountingStore::new();
            let k = key("alpha");
            seed(&s, &k, locations::HOME, STORED_HOME, None);
            seed(&s, &k, locations::CURRENT, "Denver", Some(168));

            let r = Routine::resolve_for(&s, Some(&k), operator(), &Routine::default());
            match resolve(None, &[], &r.routine, 20, true, operator()) {
                Resolved::Found { location, source: LocationSource::Routine(w) } => {
                    assert_eq!(location, "Denver");
                    assert_eq!(w, "current");
                }
                other => panic!("got {other:?}"),
            }
        }

        #[test]
        fn an_expired_current_location_stops_outranking_home() {
            // "I'm in Denver this week" must not still be true next month.
            let s = CountingStore::new();
            let k = key("alpha");
            seed(&s, &k, locations::HOME, STORED_HOME, None);
            seed(&s, &k, locations::CURRENT, "Denver", Some(1));

            let mut doc = s.snapshot();
            doc.caller_mut(&k.storage_key())
                .locations
                .get_mut(locations::CURRENT)
                .unwrap()
                .expires_at_unix = Some(chrono::Utc::now().timestamp() - 1);
            s.save(&doc).unwrap();

            let r = Routine::resolve_for(&s, Some(&k), operator(), &Routine::default());
            assert_eq!(r.routine.current, None, "an expired travel location must not resolve");
            match resolve(None, &[], &r.routine, 20, true, operator()) {
                Resolved::Found { location, .. } => assert_eq!(location, STORED_HOME),
                other => panic!("got {other:?}"),
            }
        }

        #[test]
        fn an_empty_registry_and_an_unreadable_one_are_different_answers() {
            let empty = CountingStore::new();
            let e = Routine::resolve_for(&empty, Some(&key("alpha")), operator(), &Routine::default());
            assert!(!e.degraded, "empty is NOT degraded — it is a complete, honest 'nothing set'");
            assert_eq!(e.routine.home, None);

            let broken = BrokenStore;
            let b = Routine::resolve_for(&broken, Some(&key("alpha")), operator(), &Routine::default());
            assert!(b.degraded, "a read failure must be reported as such, never as 'nothing set'");
            assert_eq!(b.routine.home, None);
        }

        #[test]
        fn a_degraded_registry_does_not_silently_fall_back_to_the_legacy_env() {
            // Answering with COMMUTE_HOME when we could not read what the user
            // actually saved would present a stale address as the current one.
            let r = Routine::resolve_for_legacy_principal(&BrokenStore, Some(&key("alpha")), operator(), &legacy(), "alpha");
            assert!(r.degraded);
            assert_eq!(r.routine.home, None);
        }

        #[test]
        fn the_unavailable_message_is_not_the_ask_message_and_names_no_city() {
            assert_ne!(REGISTRY_UNAVAILABLE_MESSAGE, ASK_MESSAGE);
            let m = REGISTRY_UNAVAILABLE_MESSAGE.to_lowercase();
            for city in ["tampa", "paris", "omaha", "san francisco", "foster city", "new york"] {
                assert!(!m.contains(city), "found {city:?}");
            }
            assert!(m.contains("couldn't read"));
        }

        #[test]
        fn the_ask_message_offers_to_remember_rather_than_remembering() {
            // The capture point: weather OFFERS, `location_set` stores. Storing
            // as a side effect of answering a question is not consent.
            assert!(ASK_MESSAGE.to_lowercase().contains("remember this is home"));
        }
    }

    #[test]
    fn the_default_caller_context_is_the_fail_closed_one() {
        // Load-bearing: every dispatch path that does not thread a caller, and
        // every future one that forgets to, must land here.
        let d = CallerContext::default();
        assert!(!d.may_infer_from_calendar());
        assert!(!d.may_infer_from_routine());
        assert_eq!(d, CallerContext::untrusted());
    }
}
