//! Authoritative fleet clock — the `time_now` core tool (CLK-01).
//!
//! Purpose: give agents and the review capstone a single AUTHORITATIVE source
//! of the current date/time — the fleet system clock — so time-based decisions
//! (notably `review_run` epic mode's enforcement of the Fable-OAuth window,
//! open through 2026-07-19) gate on the real clock rather than the drift-prone
//! harness date.
//!
//! ## Contract
//!   - Pure system clock. No network, no secrets, no hardcoded infra values.
//!   - Returns JSON with: `utc_iso8601`, `unix`, `date` (YYYY-MM-DD),
//!     `time` (HH:MM:SS), `weekday`, `tz` (default "UTC").
//!   - Optional `tz` argument renders `date`/`time`/`weekday` in that IANA
//!     timezone (e.g. "America/New_York"); `utc_iso8601`/`unix` are always
//!     the UTC instant regardless. An INVALID `tz` falls back to UTC and adds
//!     a `note` field explaining the fallback — it is NOT an error.

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

const DATE_FMT: &str = "%Y-%m-%d";
const TIME_FMT: &str = "%H:%M:%S";
const WEEKDAY_FMT: &str = "%A";

/// Build the `time_now` payload from a concrete UTC instant, so tests can pin
/// the clock and assert exact field formatting offline (no dependence on the
/// wall clock at test time).
fn build_payload(now_utc: DateTime<Utc>, tz_arg: Option<&str>) -> Value {
    let utc_iso8601 = now_utc.to_rfc3339_opts(SecondsFormat::Secs, true);
    let unix = now_utc.timestamp();

    // Default rendering is UTC. If a `tz` arg is supplied and parses to a
    // valid IANA zone, render local fields in it; otherwise fall back to UTC
    // and attach a `note` explaining why.
    match tz_arg {
        None => {
            json!({
                "utc_iso8601": utc_iso8601,
                "unix": unix,
                "date": now_utc.format(DATE_FMT).to_string(),
                "time": now_utc.format(TIME_FMT).to_string(),
                "weekday": now_utc.format(WEEKDAY_FMT).to_string(),
                "tz": "UTC",
            })
        }
        Some(name) => match name.parse::<Tz>() {
            Ok(tz) => {
                let local = tz.from_utc_datetime(&now_utc.naive_utc());
                json!({
                    "utc_iso8601": utc_iso8601,
                    "unix": unix,
                    "date": local.format(DATE_FMT).to_string(),
                    "time": local.format(TIME_FMT).to_string(),
                    "weekday": local.format(WEEKDAY_FMT).to_string(),
                    "tz": tz.name(),
                })
            }
            Err(_) => {
                json!({
                    "utc_iso8601": utc_iso8601,
                    "unix": unix,
                    "date": now_utc.format(DATE_FMT).to_string(),
                    "time": now_utc.format(TIME_FMT).to_string(),
                    "weekday": now_utc.format(WEEKDAY_FMT).to_string(),
                    "tz": "UTC",
                    "note": format!("unknown timezone '{name}', fell back to UTC"),
                })
            }
        },
    }
}

pub struct TimeNow;

#[async_trait]
impl RustTool for TimeNow {
    fn name(&self) -> &str {
        "time_now"
    }

    fn description(&self) -> &str {
        "Return the authoritative fleet date/time from the system clock as JSON \
         (utc_iso8601, unix, date, time, weekday, tz). Optional `tz` arg (IANA name, \
         e.g. \"America/New_York\") renders local date/time; an invalid tz falls back \
         to UTC with a note. Use this instead of the harness date for time-gated decisions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tz": {
                    "type": "string",
                    "description": "Optional IANA timezone name (e.g. \"America/New_York\"). Defaults to UTC; invalid values fall back to UTC with a note."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let tz_arg = args.get("tz").and_then(Value::as_str).filter(|s| !s.is_empty());
        let body = build_payload(Utc::now(), tz_arg);
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(TimeNow)) {
        tracing::error!("time: failed to register tool: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests (offline — clock is pinned, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant: 2026-07-12T20:30:45Z (a Sunday in UTC).
    fn fixed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 12, 20, 30, 45).unwrap()
    }

    #[test]
    fn test_time_now_registered() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("time_now"));
    }

    #[test]
    fn test_default_utc_field_formatting() {
        let p = build_payload(fixed(), None);
        assert_eq!(p["utc_iso8601"], "2026-07-12T20:30:45Z");
        assert_eq!(p["unix"], fixed().timestamp());
        assert_eq!(p["date"], "2026-07-12");
        assert_eq!(p["time"], "20:30:45");
        assert_eq!(p["weekday"], "Sunday");
        assert_eq!(p["tz"], "UTC");
        assert!(p.get("note").is_none());
    }

    #[test]
    fn test_local_tz_rendering_keeps_utc_instant() {
        // America/New_York is UTC-4 in July (EDT): 20:30:45Z -> 16:30:45 local.
        let p = build_payload(fixed(), Some("America/New_York"));
        assert_eq!(p["utc_iso8601"], "2026-07-12T20:30:45Z", "utc instant unchanged");
        assert_eq!(p["unix"], fixed().timestamp(), "unix instant unchanged");
        assert_eq!(p["date"], "2026-07-12");
        assert_eq!(p["time"], "16:30:45");
        assert_eq!(p["weekday"], "Sunday");
        assert_eq!(p["tz"], "America/New_York");
    }

    #[test]
    fn test_local_tz_can_cross_date_boundary() {
        // Asia/Tokyo is UTC+9: 20:30:45Z on the 12th -> 05:30:45 on the 13th.
        let p = build_payload(fixed(), Some("Asia/Tokyo"));
        assert_eq!(p["date"], "2026-07-13");
        assert_eq!(p["time"], "05:30:45");
        assert_eq!(p["weekday"], "Monday");
        assert_eq!(p["tz"], "Asia/Tokyo");
    }

    #[test]
    fn test_invalid_tz_falls_back_to_utc_with_note() {
        let p = build_payload(fixed(), Some("Not/AZone"));
        assert_eq!(p["tz"], "UTC");
        assert_eq!(p["date"], "2026-07-12");
        assert_eq!(p["time"], "20:30:45");
        assert!(
            p["note"].as_str().unwrap().contains("Not/AZone"),
            "note should name the bad zone"
        );
    }

    #[tokio::test]
    async fn test_execute_returns_wellformed_json_from_live_clock() {
        let tool = TimeNow;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(parsed["utc_iso8601"].as_str().unwrap().ends_with('Z'));
        assert!(parsed["unix"].as_i64().unwrap() > 0);
        assert_eq!(parsed["date"].as_str().unwrap().len(), 10);
        assert_eq!(parsed["time"].as_str().unwrap().len(), 8);
        assert_eq!(parsed["tz"], "UTC");
    }

    #[tokio::test]
    async fn test_execute_empty_tz_arg_is_treated_as_absent() {
        let tool = TimeNow;
        let out = tool.execute(json!({"tz": ""})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["tz"], "UTC");
        assert!(parsed.get("note").is_none());
    }
}
