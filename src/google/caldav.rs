//! Calendar tools over CalDAV. IMPLEMENTED BY GOOG-01.
//!
//! Four tools, each holding a [`GoogleConfig`]:
//!   - `google_calendar_today`     — events today across all configured calendars
//!   - `google_calendar_week`      — events over a 7-day window (default starts today)
//!   - `google_calendar_add`       — create an event on the Lumina calendar
//!   - `google_calendar_conflicts` — list events overlapping a given window
//!
//! Auth: HTTP Basic with `cfg.email` / `cfg.app_password` (Gmail App Password).
//! Per-calendar CalDAV URL: `https://www.google.com/calendar/dav/{calendar_id}/events`.
//!
//! The CalDAV read path uses a `REPORT` `calendar-query` (Depth: 1) and parses the
//! 207 Multi-Status iCal response. The write path PUTs a hand-generated VCALENDAR.
//! All date/time values are validated before being embedded into XML or iCal text.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;
use super::GoogleConfig;

// ── Constants ──────────────────────────────────────────────────────────────

/// Base of the Google CalDAV per-calendar collection URL, used for READ
/// (REPORT calendar-query). The full collection URL is
/// `{CALDAV_BASE}/{calendar_id}/events`.
const CALDAV_BASE: &str = "https://www.google.com/calendar/dav";


/// Per-calendar CalDAV collection URL (the events collection).
fn calendar_url(calendar_id: &str) -> String {
    format!("{}/{}/events", CALDAV_BASE, calendar_id)
}

// ── CalendarEvent ──────────────────────────────────────────────────────────

/// A single event parsed from an iCalendar VEVENT block, annotated with the
/// calendar it came from.
#[derive(Debug, Clone, PartialEq)]
struct CalendarEvent {
    uid: String,
    summary: String,
    dtstart: String,
    dtend: String,
    location: Option<String>,
    /// The calendar id this event was fetched from.
    calendar: String,
}

// ── Date/time validation & conversion ────────────────────────────────────────

/// Validate a CalDAV date/time string before embedding it in XML.
///
/// Accepted formats (RFC 5545 / RFC 4791):
/// - Date-only:        `YYYYMMDD` (8 digits)
/// - Date-time local:  `YYYYMMDDTHHMMSS` (15 chars)
/// - Date-time UTC:    `YYYYMMDDTHHMMSSZ` (16 chars, trailing `Z`)
fn validate_caldav_datetime(value: &str) -> Result<(), ToolError> {
    let valid = match value.len() {
        8 => value.bytes().all(|b| b.is_ascii_digit()),
        15 => {
            value[..8].bytes().all(|b| b.is_ascii_digit())
                && value.as_bytes()[8] == b'T'
                && value[9..].bytes().all(|b| b.is_ascii_digit())
        }
        16 => {
            value[..8].bytes().all(|b| b.is_ascii_digit())
                && value.as_bytes()[8] == b'T'
                && value[9..15].bytes().all(|b| b.is_ascii_digit())
                && value.as_bytes()[15] == b'Z'
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "invalid CalDAV date/time '{}' — expected YYYYMMDD, YYYYMMDDTHHMMSS, or YYYYMMDDTHHMMSSZ",
            value
        )))
    }
}

/// Parse a flexible ISO-8601 input into a UTC `DateTime`.
///
/// Accepts RFC 3339 (`2026-06-08T14:30:00Z`, with or without offset), the naive // pii-test-fixture
/// form `2026-06-08T14:30:00` (assumed UTC), and the date-only form // pii-test-fixture
/// `2026-06-08` (assumed midnight UTC). Rejects everything else with // pii-test-fixture
/// [`ToolError::InvalidArgument`].
fn parse_iso8601_utc(input: &str) -> Result<chrono::DateTime<chrono::Utc>, ToolError> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

    let s = input.trim();
    // RFC 3339 with offset / Z.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Naive datetime, assume UTC.
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
    }
    // Date-only, assume midnight UTC.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(ndt) = d.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
    }
    Err(ToolError::InvalidArgument(format!(
        "invalid ISO-8601 datetime '{}' — expected e.g. 2026-06-08T14:30:00Z", // pii-test-fixture
        input
    )))
}

/// Format a UTC `DateTime` into CalDAV/iCal basic UTC form `YYYYMMDDTHHMMSSZ`.
fn to_caldav_utc(dt: &chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

// ── REPORT request body ──────────────────────────────────────────────────────

/// Build a CalDAV `calendar-query` REPORT XML body for the given UTC range.
///
/// Callers MUST validate `start`/`end` with [`validate_caldav_datetime`] before
/// passing them here so untrusted text can never reach the XML.
fn build_report_body(start: &str, end: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{}" end="{}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#,
        start, end
    )
}

// ── iCalendar parsing ────────────────────────────────────────────────────────

/// Unfold RFC 5545 folded lines: a CRLF/LF followed by SPACE or TAB is a fold
/// indicator; the leading whitespace is dropped.
fn unfold_ical(s: &str) -> String {
    s.replace("\r\n", "\n").replace("\n ", "").replace("\n\t", "")
}

/// Unescape RFC 5545 §3.3.11 TEXT escapes: `\n`/`\N` → newline, `\,` → `,`,
/// `\;` → `;`, `\\` → `\`.
fn unescape_ical_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Extract the value of a named iCalendar property from a (single) VEVENT block.
/// Handles both `KEY:value` and parameterized `KEY;param=val:value`.
fn extract_property(block: &str, key: &str) -> Option<String> {
    let key_upper = key.to_uppercase();
    let colon = format!("{}:", key_upper);
    let semi = format!("{};", key_upper);
    for line in block.lines() {
        let upper = line.to_uppercase();
        if upper.starts_with(&colon) || upper.starts_with(&semi) {
            let raw = line.splitn(2, ':').nth(1).unwrap_or("").trim();
            let value = unescape_ical_text(raw);
            return if value.is_empty() { None } else { Some(value) };
        }
    }
    None
}

/// Parse an iCal Multi-Status body into events, tagging each with `calendar`.
fn parse_ical(body: &str, calendar: &str) -> Vec<CalendarEvent> {
    let unfolded = unfold_ical(body);
    let mut events = Vec::new();

    for block in unfolded.split("BEGIN:VEVENT") {
        if !block.contains("END:VEVENT") {
            continue;
        }
        let block = match block.split("END:VEVENT").next() {
            Some(b) => b,
            None => continue,
        };

        let uid = extract_property(block, "UID").unwrap_or_default();
        let summary = extract_property(block, "SUMMARY").unwrap_or_default();
        let dtstart = extract_property(block, "DTSTART").unwrap_or_default();
        let dtend = extract_property(block, "DTEND").unwrap_or_default();
        let location = extract_property(block, "LOCATION");

        if uid.is_empty() && summary.is_empty() && dtstart.is_empty() {
            continue;
        }

        events.push(CalendarEvent {
            uid,
            summary,
            dtstart,
            dtend,
            location,
            calendar: calendar.to_string(),
        });
    }

    events
}

/// De-duplicate events by UID (keeping first occurrence) and sort by DTSTART.
fn dedup_and_sort(mut events: Vec<CalendarEvent>) -> Vec<CalendarEvent> {
    let mut seen = std::collections::HashSet::new();
    events.retain(|e| e.uid.is_empty() || seen.insert(e.uid.clone()));
    events.sort_by(|a, b| a.dtstart.cmp(&b.dtstart));
    events
}

// ── iCal generation (for add) ────────────────────────────────────────────────

/// Escape a value for embedding in an iCal TEXT property (RFC 5545 §3.3.11).
fn escape_ical_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Build a complete VCALENDAR/VEVENT document for `google_calendar_add`.
///
/// `dtstamp`/`dtstart`/`dtend` MUST already be in CalDAV UTC basic form
/// (`YYYYMMDDTHHMMSSZ`) and validated. `title`/`description`/`location` are
/// escaped per RFC 5545 before insertion.
fn build_ical_event(
    uid: &str,
    dtstamp: &str,
    dtstart: &str,
    dtend: &str,
    title: &str,
    description: Option<&str>,
    location: Option<&str>,
) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Lumina//Terminus GOOG-01//EN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{}", uid),
        format!("DTSTAMP:{}", dtstamp),
        format!("DTSTART:{}", dtstart),
        format!("DTEND:{}", dtend),
        format!("SUMMARY:{}", escape_ical_text(title)),
    ];
    if let Some(d) = description.filter(|s| !s.is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape_ical_text(d)));
    }
    if let Some(l) = location.filter(|s| !s.is_empty()) {
        lines.push(format!("LOCATION:{}", escape_ical_text(l)));
    }
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());
    lines.join("\r\n")
}

/// Generate a unique-ish id from a UTC timestamp (no `uuid` dependency).
/// Combines compact timestamp + nanoseconds; suffixed with a fixed domain.
fn generate_uid(now: &chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Timelike;
    format!(
        "{}-{}@lumina.terminus",
        now.format("%Y%m%dT%H%M%SZ"),
        now.nanosecond()
    )
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

/// Issue a CalDAV REPORT against one calendar and parse returned events.
/// On any transport/HTTP error returns `Err` so callers can choose to skip.
async fn report_calendar(
    http: &reqwest::Client,
    cfg: &GoogleConfig,
    calendar_id: &str,
    start: &str,
    end: &str,
) -> Result<Vec<CalendarEvent>, ToolError> {
    validate_caldav_datetime(start)?;
    validate_caldav_datetime(end)?;
    let body = build_report_body(start, end);
    let url = calendar_url(calendar_id);

    let method = reqwest::Method::from_bytes(b"REPORT")
        .map_err(|e| ToolError::Http(format!("invalid HTTP method: {}", e)))?;

    let resp = http
        .request(method, &url)
        .basic_auth(&cfg.email, Some(&cfg.app_password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Depth", "1")
        .body(body)
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("CalDAV REPORT failed: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(ToolError::Http(format!(
            "CalDAV REPORT returned {} for {}",
            status, calendar_id
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| ToolError::Http(format!("reading CalDAV body: {}", e)))?;
    Ok(parse_ical(&text, calendar_id))
}

/// Query all configured calendars for a window, skipping ones that error.
/// Returns dedup'd, sorted events.
async fn collect_events(
    http: &reqwest::Client,
    cfg: &GoogleConfig,
    start: &str,
    end: &str,
) -> Result<Vec<CalendarEvent>, ToolError> {
    // Validate once up front so a bad window fails fast with InvalidArgument.
    validate_caldav_datetime(start)?;
    validate_caldav_datetime(end)?;

    let mut all = Vec::new();
    for cal in cfg.all_calendar_ids() {
        match report_calendar(http, cfg, &cal, start, end).await {
            Ok(mut evs) => all.append(&mut evs),
            Err(e) => {
                tracing::warn!("google_calendar: skipping calendar {} — {}", cal, e);
            }
        }
    }
    Ok(dedup_and_sort(all))
}

/// Format a list of events into a readable text block.
fn format_events(title: &str, events: &[CalendarEvent]) -> String {
    if events.is_empty() {
        return format!("{}\n\n(no events)", title);
    }
    let mut out = format!("{}\n", title);
    for e in events {
        let time = if e.dtend.is_empty() {
            e.dtstart.clone()
        } else {
            format!("{} – {}", e.dtstart, e.dtend)
        };
        let name = if e.summary.is_empty() {
            "(untitled)"
        } else {
            e.summary.as_str()
        };
        out.push_str(&format!("\n• {} [{}]", name, time));
        if let Some(loc) = &e.location {
            out.push_str(&format!(" @ {}", loc));
        }
        out.push_str(&format!("  ({})", e.calendar));
    }
    out
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// `google_calendar_today` — events today across all configured calendars.
struct CalendarToday {
    cfg: GoogleConfig,
    http: reqwest::Client,
}

#[async_trait]
impl RustTool for CalendarToday {
    fn name(&self) -> &str {
        "google_calendar_today"
    }
    fn description(&self) -> &str {
        "Check today's calendar and schedule — list today's events, meetings, and \
         appointments (your agenda for today) across all configured Google calendars. \
         Use when the user asks about their calendar, schedule, agenda, meetings, \
         appointments, what's on today, or whether they're free today."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let today = chrono::Utc::now().date_naive();
        let start = format!("{}T000000Z", today.format("%Y%m%d"));
        let end = format!("{}T235959Z", today.format("%Y%m%d"));
        let events = collect_events(&self.http, &self.cfg, &start, &end).await?;
        Ok(format_events(
            &format!("Events for {}", today.format("%Y-%m-%d")),
            &events,
        ))
    }
}

/// `google_calendar_week` — events over a 7-day window (default starts today).
struct CalendarWeek {
    cfg: GoogleConfig,
    http: reqwest::Client,
}

#[async_trait]
impl RustTool for CalendarWeek {
    fn name(&self) -> &str {
        "google_calendar_week"
    }
    fn description(&self) -> &str {
        "Check the calendar and schedule for the week — list events, meetings, and \
         appointments over a 7-day window (your upcoming agenda/schedule). Use when the \
         user asks what's on this week, their weekly schedule, agenda, upcoming events or \
         meetings, or whether they're free on an upcoming day. Optional start_date \
         YYYY-MM-DD (default today)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "start_date": {
                    "type": "string",
                    "description": "Window start date YYYY-MM-DD (default today, UTC)."
                }
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let start_date = match args.get("start_date").and_then(|v| v.as_str()) {
            Some(s) => chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| {
                ToolError::InvalidArgument(format!(
                    "invalid start_date '{}' — expected YYYY-MM-DD",
                    s
                ))
            })?,
            None => chrono::Utc::now().date_naive(),
        };
        let end_date = start_date + chrono::Duration::days(7);
        let start = format!("{}T000000Z", start_date.format("%Y%m%d"));
        let end = format!("{}T000000Z", end_date.format("%Y%m%d"));
        let events = collect_events(&self.http, &self.cfg, &start, &end).await?;
        Ok(format_events(
            &format!(
                "Events {} → {} (7 days)",
                start_date.format("%Y-%m-%d"),
                end_date.format("%Y-%m-%d")
            ),
            &events,
        ))
    }
}

/// `google_calendar_add` — create an event on the Lumina calendar.
struct CalendarAdd {
    cfg: GoogleConfig,
    http: reqwest::Client,
}

#[async_trait]
impl RustTool for CalendarAdd {
    fn name(&self) -> &str {
        "google_calendar_add"
    }
    fn description(&self) -> &str {
        "Add or create a calendar event, meeting, or appointment on the Lumina calendar. \
         Use when the user wants to schedule, add, book, or create an event, meeting, or \
         appointment. Args: title, start, end (ISO-8601), optional description, location."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Event title (SUMMARY)."},
                "start": {"type": "string", "description": "Start time, ISO-8601 (e.g. 2026-06-08T14:00:00Z)."}, // pii-test-fixture
                "end": {"type": "string", "description": "End time, ISO-8601."},
                "description": {"type": "string", "description": "Optional description."},
                "location": {"type": "string", "description": "Optional location."}
            },
            "required": ["title", "start", "end"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("title is required".into()))?;
        let start_in = args
            .get("start")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("start is required".into()))?;
        let end_in = args
            .get("end")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("end is required".into()))?;
        let description = args.get("description").and_then(|v| v.as_str());
        let location = args.get("location").and_then(|v| v.as_str());

        let start_dt = parse_iso8601_utc(start_in)?;
        let end_dt = parse_iso8601_utc(end_in)?;
        if end_dt < start_dt {
            return Err(ToolError::InvalidArgument(
                "end must not be before start".into(),
            ));
        }

        let now = chrono::Utc::now();
        let dtstart = to_caldav_utc(&start_dt);
        let dtend = to_caldav_utc(&end_dt);
        let dtstamp = to_caldav_utc(&now);
        let uid = generate_uid(&now);

        // Defensive: validate the generated date strings before embedding.
        validate_caldav_datetime(&dtstart)?;
        validate_caldav_datetime(&dtend)?;
        validate_caldav_datetime(&dtstamp)?;

        let ical = build_ical_event(
            &uid, &dtstamp, &dtstart, &dtend, title, description, location,
        );

        // Write to the account's OWN calendar via the legacy endpoint with App
        // Password Basic auth — this is the only combination Google permits without
        // OAuth. The v2 endpoint requires a Bearer token (401), and the legacy
        // endpoint rejects writes to group calendars the account doesn't own (403).
        // (Confirmed against the caldav reference library: only the own calendar is
        // discoverable/writable for this account.)
        let target_calendar = self.cfg.email.clone();
        let url = format!("{}/{}/events/{}.ics", CALDAV_BASE, target_calendar, uid);

        let resp = self
            .http
            .put(&url)
            .basic_auth(&self.cfg.email, Some(&self.cfg.app_password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ical)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("CalDAV PUT failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ToolError::Http(format!(
                "CalDAV PUT returned {} creating event",
                status
            )));
        }

        Ok(format!(
            "Created event \"{}\" at {} (HTTP {}). UID {}",
            title,
            dtstart,
            status.as_u16(),
            uid
        ))
    }
}

/// `google_calendar_conflicts` — list events overlapping a given window.
struct CalendarConflicts {
    cfg: GoogleConfig,
    http: reqwest::Client,
}

#[async_trait]
impl RustTool for CalendarConflicts {
    fn name(&self) -> &str {
        "google_calendar_conflicts"
    }
    fn description(&self) -> &str {
        "Check whether any calendar events overlap a window. Args: start, end (ISO-8601)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "start": {"type": "string", "description": "Window start, ISO-8601."},
                "end": {"type": "string", "description": "Window end, ISO-8601."}
            },
            "required": ["start", "end"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let start_in = args
            .get("start")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("start is required".into()))?;
        let end_in = args
            .get("end")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("end is required".into()))?;

        let start_dt = parse_iso8601_utc(start_in)?;
        let end_dt = parse_iso8601_utc(end_in)?;
        if end_dt < start_dt {
            return Err(ToolError::InvalidArgument(
                "end must not be before start".into(),
            ));
        }

        let start = to_caldav_utc(&start_dt);
        let end = to_caldav_utc(&end_dt);
        let events = collect_events(&self.http, &self.cfg, &start, &end).await?;

        if events.is_empty() {
            return Ok(format!(
                "No conflicts: the window {} → {} is free.",
                start, end
            ));
        }
        Ok(format_events(
            &format!(
                "CONFLICT: {} event(s) overlap {} → {}",
                events.len(),
                start,
                end
            ),
            &events,
        ))
    }
}

// ── Registration ─────────────────────────────────────────────────────────────

/// Register: google_calendar_today, _week, _add, _conflicts.
pub fn register(registry: &mut ToolRegistry, cfg: &GoogleConfig) {
    let http = reqwest::Client::new();
    registry.register_or_replace(Box::new(CalendarToday {
        cfg: cfg.clone(),
        http: http.clone(),
    }));
    registry.register_or_replace(Box::new(CalendarWeek {
        cfg: cfg.clone(),
        http: http.clone(),
    }));
    registry.register_or_replace(Box::new(CalendarAdd {
        cfg: cfg.clone(),
        http: http.clone(),
    }));
    registry.register_or_replace(Box::new(CalendarConflicts {
        cfg: cfg.clone(),
        http,
    }));
}

// ── Tests (no network) ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GoogleConfig {
        GoogleConfig {
            email: "<email>".into(), // pii-test-fixture
            app_password: "secret".into(),
            peter_email: "<email>".into(), // pii-test-fixture
            lumina_calendar_id: Some("<email>".into()), // pii-test-fixture
            extra_calendars: vec![],
        }
    }

    #[test]
    fn calendar_url_is_per_calendar_events_collection() {
        assert_eq!(
            calendar_url("<email>"), // pii-test-fixture
            "https://www.google.com/calendar/dav/<email>/events" // pii-test-fixture
        );
    }

    // ── REPORT body ──────────────────────────────────────────────────────────

    #[test]
    fn build_report_body_is_well_formed() {
        let body = build_report_body("20260601T000000Z", "20260601T235959Z");
        assert!(body.contains(r#"<?xml version="1.0" encoding="utf-8"?>"#));
        assert!(body.contains("C:calendar-query"));
        assert!(body.contains(r#"name="VCALENDAR""#));
        assert!(body.contains(r#"name="VEVENT""#));
        assert!(body.contains(r#"start="20260601T000000Z""#));
        assert!(body.contains(r#"end="20260601T235959Z""#));
    }

    // ── datetime validation ────────────────────────────────────────────────────

    #[test]
    fn validate_caldav_datetime_accepts_valid() {
        assert!(validate_caldav_datetime("20260601").is_ok());
        assert!(validate_caldav_datetime("20260601T090000").is_ok());
        assert!(validate_caldav_datetime("20260601T090000Z").is_ok());
    }

    #[test]
    fn validate_caldav_datetime_rejects_injection() {
        assert!(validate_caldav_datetime("/><evil/>").is_err());
        assert!(validate_caldav_datetime("2026-06-01T00:00:00Z").is_err()); // pii-test-fixture
        assert!(validate_caldav_datetime("20260601T000000Z&x=1").is_err());
        assert!(validate_caldav_datetime("").is_err());
        assert!(validate_caldav_datetime("not-a-date").is_err());
    }

    #[test]
    fn parse_iso8601_accepts_rfc3339_and_naive_and_date() {
        assert!(parse_iso8601_utc("2026-06-08T14:30:00Z").is_ok()); // pii-test-fixture
        assert!(parse_iso8601_utc("2026-06-08T14:30:00+02:00").is_ok()); // pii-test-fixture
        assert!(parse_iso8601_utc("2026-06-08T14:30:00").is_ok()); // pii-test-fixture
        assert!(parse_iso8601_utc("2026-06-08T14:30").is_ok()); // pii-test-fixture
        assert!(parse_iso8601_utc("2026-06-08").is_ok()); // pii-test-fixture
    }

    #[test]
    fn parse_iso8601_rejects_garbage() {
        assert!(parse_iso8601_utc("nope").is_err());
        assert!(parse_iso8601_utc("06/08/2026").is_err());
        assert!(parse_iso8601_utc("").is_err());
    }

    #[test]
    fn parse_iso8601_offset_converts_to_utc() {
        // 14:30+02:00 == 12:30Z
        let dt = parse_iso8601_utc("2026-06-08T14:30:00+02:00").unwrap(); // pii-test-fixture
        assert_eq!(to_caldav_utc(&dt), "20260608T123000Z");
    }

    // ── iCal parsing ───────────────────────────────────────────────────────────

    #[test]
    fn parse_ical_extracts_summary_dtstart_uid() {
        let ical = concat!(
            "BEGIN:VCALENDAR\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:<email>\r\n", // pii-test-fixture
            "SUMMARY:Team standup\r\n",
            "DTSTART:20260601T090000Z\r\n",
            "DTEND:20260601T093000Z\r\n",
            "LOCATION:Room A\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );
        let events = parse_ical(ical, "<email>"); // pii-test-fixture
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uid, "<email>"); // pii-test-fixture
        assert_eq!(e.summary, "Team standup");
        assert_eq!(e.dtstart, "20260601T090000Z");
        assert_eq!(e.dtend, "20260601T093000Z");
        assert_eq!(e.location.as_deref(), Some("Room A"));
        assert_eq!(e.calendar, "<email>"); // pii-test-fixture
    }

    #[test]
    fn parse_ical_handles_parameterized_and_folded() {
        let ical = "BEGIN:VEVENT\r\nUID:tz@x\r\nSUMMARY:Long titl\r\n e\r\nDTSTART;TZID=America/Los_Angeles:20260601T090000\r\nEND:VEVENT\r\n";
        let events = parse_ical(ical, "c");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].dtstart, "20260601T090000");
        assert_eq!(events[0].summary, "Long title");
    }

    #[test]
    fn parse_ical_empty_yields_nothing() {
        assert!(parse_ical("", "c").is_empty());
        assert!(parse_ical("BEGIN:VCALENDAR\nEND:VCALENDAR\n", "c").is_empty());
    }

    // ── dedup / sort ────────────────────────────────────────────────────────────

    #[test]
    fn dedup_removes_duplicate_uids_and_sorts() {
        let evs = vec![
            CalendarEvent {
                uid: "u2".into(),
                summary: "later".into(),
                dtstart: "20260601T120000Z".into(),
                dtend: "".into(),
                location: None,
                calendar: "a".into(),
            },
            CalendarEvent {
                uid: "u1".into(),
                summary: "earlier".into(),
                dtstart: "20260601T090000Z".into(),
                dtend: "".into(),
                location: None,
                calendar: "a".into(),
            },
            CalendarEvent {
                uid: "u1".into(),
                summary: "dup".into(),
                dtstart: "20260601T090000Z".into(),
                dtend: "".into(),
                location: None,
                calendar: "b".into(),
            },
        ];
        let out = dedup_and_sort(evs);
        assert_eq!(out.len(), 2, "duplicate UID removed");
        assert_eq!(out[0].uid, "u1", "sorted by dtstart");
        assert_eq!(out[1].uid, "u2");
    }

    // ── iCal generation ─────────────────────────────────────────────────────────

    #[test]
    fn build_ical_event_well_formed() {
        let ical = build_ical_event(
            "uid-1@lumina",
            "20260608T120000Z",
            "20260608T140000Z",
            "20260608T150000Z",
            "Lunch, meeting",
            Some("Notes;\nwith escapes"),
            Some("Cafe"),
        );
        assert!(ical.starts_with("BEGIN:VCALENDAR"));
        assert!(ical.contains("BEGIN:VEVENT"));
        assert!(ical.contains("UID:uid-1@lumina"));
        assert!(ical.contains("DTSTART:20260608T140000Z"));
        assert!(ical.contains("DTEND:20260608T150000Z"));
        // comma escaped in SUMMARY
        assert!(ical.contains("SUMMARY:Lunch\\, meeting"));
        // semicolon and newline escaped in DESCRIPTION
        assert!(ical.contains("DESCRIPTION:Notes\\;\\nwith escapes"));
        assert!(ical.contains("LOCATION:Cafe"));
        assert!(ical.trim_end().ends_with("END:VCALENDAR"));
    }

    #[test]
    fn build_ical_event_omits_empty_optionals() {
        let ical = build_ical_event(
            "u", "20260608T120000Z", "20260608T140000Z", "20260608T150000Z", "Title", None, None,
        );
        assert!(!ical.contains("DESCRIPTION:"));
        assert!(!ical.contains("LOCATION:"));
    }

    #[test]
    fn escape_ical_text_handles_specials() {
        assert_eq!(escape_ical_text("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
    }

    #[test]
    fn generate_uid_has_domain_and_timestamp() {
        let now = chrono::Utc::now();
        let uid = generate_uid(&now);
        assert!(uid.ends_with("@lumina.terminus"));
        assert!(uid.contains('T'));
    }

    // ── formatting ──────────────────────────────────────────────────────────────

    #[test]
    fn format_events_empty_and_nonempty() {
        assert!(format_events("Title", &[]).contains("no events"));
        let evs = vec![CalendarEvent {
            uid: "u".into(),
            summary: "Standup".into(),
            dtstart: "20260601T090000Z".into(),
            dtend: "20260601T093000Z".into(),
            location: Some("Room A".into()),
            calendar: "cal@x".into(),
        }];
        let out = format_events("Today", &evs);
        assert!(out.contains("Standup"));
        assert!(out.contains("Room A"));
        assert!(out.contains("cal@x"));
        assert!(out.contains("20260601T090000Z"));
    }

    // ── tool metadata / arg validation (no network) ──────────────────────────────

    #[tokio::test]
    async fn add_rejects_missing_required_args() {
        let tool = CalendarAdd {
            cfg: cfg(),
            http: reqwest::Client::new(),
        };
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn add_rejects_bad_iso_before_network() {
        let tool = CalendarAdd {
            cfg: cfg(),
            http: reqwest::Client::new(),
        };
        let err = tool
            .execute(json!({"title":"x","start":"nope","end":"2026-06-08T10:00:00Z"})) // pii-test-fixture
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn add_rejects_end_before_start() {
        let tool = CalendarAdd {
            cfg: cfg(),
            http: reqwest::Client::new(),
        };
        let err = tool
            .execute(json!({
                "title":"x",
                "start":"2026-06-08T12:00:00Z", // pii-test-fixture
                "end":"2026-06-08T10:00:00Z" // pii-test-fixture
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn conflicts_rejects_bad_iso() {
        let tool = CalendarConflicts {
            cfg: cfg(),
            http: reqwest::Client::new(),
        };
        let err = tool
            .execute(json!({"start":"bad","end":"also-bad"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn week_rejects_bad_start_date() {
        let tool = CalendarWeek {
            cfg: cfg(),
            http: reqwest::Client::new(),
        };
        let err = tool
            .execute(json!({"start_date":"2026/06/08"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn tool_names_match_spec() {
        let http = reqwest::Client::new();
        let today = CalendarToday { cfg: cfg(), http: http.clone() };
        let week = CalendarWeek { cfg: cfg(), http: http.clone() };
        let add = CalendarAdd { cfg: cfg(), http: http.clone() };
        let conflicts = CalendarConflicts { cfg: cfg(), http };
        assert_eq!(today.name(), "google_calendar_today");
        assert_eq!(week.name(), "google_calendar_week");
        assert_eq!(add.name(), "google_calendar_add");
        assert_eq!(conflicts.name(), "google_calendar_conflicts");
    }

    #[test]
    fn tool_parameters_are_objects() {
        let http = reqwest::Client::new();
        let add = CalendarAdd { cfg: cfg(), http };
        let p = add.parameters();
        assert_eq!(p["type"], "object");
        assert!(p["required"].as_array().unwrap().contains(&json!("title")));
    }
}
