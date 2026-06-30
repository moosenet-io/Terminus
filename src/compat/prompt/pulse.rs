//! DPROMPT-06: Situational Pulse — live, deterministic per-turn context.
//!
//! The pulse is a ~50-token block recomputed on every turn with **zero LLM
//! cost**.  It reports the current time/day in the user's timezone, their
//! location, the next calendar event, and overall system health.
//!
//! All inputs come from configuration or lightweight on-disk caches that are
//! refreshed by background routines (CalDAV peek, Sentinel health).  The pulse
//! itself never makes a network call — it only formats already-available data.
//!
//! Time handling is dependency-free: we read `SystemTime` and apply a fixed
//! UTC offset (`LUMINA_TZ_OFFSET_HOURS`, default `-8` Pacific).  This keeps the
//! crate free of `chrono` while remaining fully deterministic for tests, which
//! inject an explicit epoch via [`SituationalPulse::build_at`].

use std::time::{SystemTime, UNIX_EPOCH};

/// Default timezone offset (hours from UTC) when the user has none configured.
const DEFAULT_TZ_OFFSET_HOURS: i64 = -8; // America/Los_Angeles (standard time)

/// Inputs to the pulse that come from caches / settings, not the clock.
#[derive(Debug, Clone, Default)]
pub struct PulseInputs {
    /// Human-readable location, e.g. `"Foster City"`.  Omitted when `None`.
    pub location: Option<String>,
    /// Timezone label shown to the user, e.g. `"PT"`.
    pub tz_label: Option<String>,
    /// UTC offset in hours; falls back to [`DEFAULT_TZ_OFFSET_HOURS`].
    pub tz_offset_hours: Option<i64>,
    /// Next calendar event description, e.g. `"Team standup 9am"`.  `None` →
    /// "Calendar: clear"; an explicit sentinel handles "unavailable".
    pub next_event: Option<String>,
    /// `true` when the calendar cache could not be read at all.
    pub calendar_unavailable: bool,
    /// Most-severe active alert, e.g. `"weather tool degraded"`.  `None` →
    /// "Systems: healthy".
    pub active_alert: Option<String>,
}

/// A computed situational pulse.
#[derive(Debug, Clone)]
pub struct SituationalPulse {
    text: String,
}

impl SituationalPulse {
    /// Build the pulse from the current wall clock.
    pub fn build(inputs: &PulseInputs) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::build_at(now, inputs)
    }

    /// Build the pulse for an explicit UTC epoch (seconds).  Used by tests for
    /// determinism and by callers that already have a timestamp.
    pub fn build_at(utc_secs: i64, inputs: &PulseInputs) -> Self {
        let offset = inputs.tz_offset_hours.unwrap_or_else(|| {
            std::env::var("LUMINA_TZ_OFFSET_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_TZ_OFFSET_HOURS)
        });
        let local = utc_secs + offset * 3600;
        let (weekday, hour, minute) = civil_time(local);
        let tz = inputs.tz_label.as_deref().unwrap_or("PT");

        // "Monday 9:15 PM PT."
        let (h12, ampm) = to_12h(hour);
        let mut parts: Vec<String> =
            vec![format!("{} {}:{:02} {} {}.", weekday, h12, minute, ampm, tz)];

        if let Some(loc) = inputs.location.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("{loc}."));
        }

        if inputs.calendar_unavailable {
            parts.push("Calendar: unavailable.".to_string());
        } else if let Some(ev) = inputs.next_event.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("Next: {ev}."));
        } else {
            parts.push("Calendar: clear.".to_string());
        }

        match inputs.active_alert.as_deref().filter(|s| !s.is_empty()) {
            Some(a) => parts.push(format!("Alert: {a}.")),
            None => parts.push("Systems: healthy.".to_string()),
        }

        SituationalPulse { text: parts.join(" ") }
    }

    /// The formatted pulse string.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// Convert a 24h hour to a 12h hour + AM/PM marker.
fn to_12h(hour24: u32) -> (u32, &'static str) {
    let ampm = if hour24 < 12 { "AM" } else { "PM" };
    let h = hour24 % 12;
    let h = if h == 0 { 12 } else { h };
    (h, ampm)
}

/// Decompose a (possibly negative) local epoch into (weekday, hour, minute).
///
/// 1970-01-01 was a Thursday.  We compute the day index relative to the epoch,
/// flooring toward negative infinity so pre-epoch times (never expected, but
/// safe) still produce a valid weekday.
fn civil_time(local_secs: i64) -> (&'static str, u32, u32) {
    let day = local_secs.div_euclid(86_400);
    let secs_of_day = local_secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    // 1970-01-01 = Thursday (index 4 with Sunday=0).
    let wd = (day.rem_euclid(7) + 4).rem_euclid(7) as usize;
    const NAMES: [&str; 7] = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    (NAMES[wd], hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2021-01-04 is a Monday.  21:15:00 UTC → epoch 1609794900.
    const MON_2115_UTC: i64 = 1_609_794_900;

    #[test]
    fn includes_time_location_calendar_alerts() {
        let inputs = PulseInputs {
            location: Some("Foster City".into()),
            tz_label: Some("PT".into()),
            tz_offset_hours: Some(0), // keep UTC for a deterministic assertion
            next_event: Some("Team standup 9am".into()),
            ..Default::default()
        };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        let s = p.as_str();
        assert!(s.contains("Monday"), "got: {s}");
        assert!(s.contains("9:15 PM PT"), "got: {s}");
        assert!(s.contains("Foster City"));
        assert!(s.contains("Next: Team standup 9am"));
        assert!(s.contains("Systems: healthy"));
    }

    #[test]
    fn missing_calendar_says_unavailable() {
        let inputs = PulseInputs { calendar_unavailable: true, tz_offset_hours: Some(0), ..Default::default() };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        assert!(p.as_str().contains("Calendar: unavailable"));
    }

    #[test]
    fn no_event_says_clear() {
        let inputs = PulseInputs { tz_offset_hours: Some(0), ..Default::default() };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        assert!(p.as_str().contains("Calendar: clear"));
    }

    #[test]
    fn alert_shown_when_present() {
        let inputs = PulseInputs {
            tz_offset_hours: Some(0),
            active_alert: Some("weather tool degraded".into()),
            ..Default::default()
        };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        assert!(p.as_str().contains("Alert: weather tool degraded"));
        assert!(!p.as_str().contains("Systems: healthy"));
    }

    #[test]
    fn no_location_omits_it() {
        let inputs = PulseInputs { tz_offset_hours: Some(0), ..Default::default() };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        // No double space and no empty location token.
        assert!(!p.as_str().contains("  "));
    }

    #[test]
    fn offset_shifts_local_time() {
        // -8h from 21:15 UTC Monday → 13:15 PT Monday.
        let inputs = PulseInputs { tz_offset_hours: Some(-8), ..Default::default() };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        assert!(p.as_str().contains("1:15 PM"), "got: {}", p.as_str());
        assert!(p.as_str().contains("Monday"));
    }

    #[test]
    fn pulse_is_compact() {
        let inputs = PulseInputs {
            location: Some("Foster City".into()),
            next_event: Some("Team standup 9am tomorrow".into()),
            tz_offset_hours: Some(0),
            ..Default::default()
        };
        let p = SituationalPulse::build_at(MON_2115_UTC, &inputs);
        // ~50-token budget → well under 80 words.
        assert!(p.as_str().split_whitespace().count() < 40);
    }

    #[test]
    fn weekday_algorithm_known_dates() {
        // 1970-01-01 Thursday
        assert_eq!(civil_time(0).0, "Thursday");
        // +1 day → Friday
        assert_eq!(civil_time(86_400).0, "Friday");
        // 2021-01-04 Monday
        assert_eq!(civil_time(MON_2115_UTC).0, "Monday");
    }

    #[test]
    fn midnight_and_noon_format() {
        assert_eq!(to_12h(0), (12, "AM"));
        assert_eq!(to_12h(12), (12, "PM"));
        assert_eq!(to_12h(13), (1, "PM"));
        assert_eq!(to_12h(23), (11, "PM"));
    }
}
