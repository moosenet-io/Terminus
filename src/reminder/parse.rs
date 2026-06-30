//! Natural-language time parsing for reminders.
//!
//! The public entry point is [`parse_time`], which takes a natural-language
//! phrase, a timezone, and a reference "now" (so tests can pin the clock) and
//! returns the resolved UTC instant.
//!
//! Patterns handled:
//! - relative:    "in 30 minutes", "in 2 hours", "in 45 seconds", "in 1 day"
//! - absolute-today: "at 3pm", "at 15:00", "at 8:30 PM" (rolls to tomorrow if past)
//! - tomorrow:    "tomorrow at 9am", "tomorrow 9am"
//! - explicit date: "June 15 at 6:00 AM", "june 15 6am"
//! - weekday:     "next Monday at 10am", "monday at 10am"
//! - shorthand:   "tonight at 8", "tonight", "this evening", "noon", "midnight"
//!
//! All resolution happens in the supplied timezone, then converts to UTC. A
//! resolved time that is in the past (for clock-only phrases) rolls forward to
//! the next occurrence.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;

/// Error returned when a phrase cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not parse time: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a natural-language time phrase into a UTC instant.
///
/// `tz` is the timezone the phrase is expressed in. `now_utc` is the reference
/// instant (UTC); tests pass a fixed value so results are deterministic.
pub fn parse_time(phrase: &str, tz: Tz, now_utc: DateTime<Utc>) -> Result<DateTime<Utc>, ParseError> {
    let now_local = now_utc.with_timezone(&tz);
    let raw = phrase.trim();
    if raw.is_empty() {
        return Err(ParseError("empty phrase".into()));
    }
    let s = raw.to_lowercase();

    // 1. Relative: "in N <unit>"
    if let Some(rest) = s.strip_prefix("in ") {
        return parse_relative(rest.trim(), now_utc);
    }

    // 2. Named instants without an explicit clock part.
    if s == "noon" {
        return resolve_clock(now_local, tz, 12, 0, true);
    }
    if s == "midnight" {
        return resolve_clock(now_local, tz, 0, 0, true);
    }
    if s == "this evening" || s == "tonight" {
        // Default tonight/this evening to 20:00.
        return resolve_clock(now_local, tz, 20, 0, true);
    }

    // 3. tomorrow [at] <clock>
    if let Some(rest) = s.strip_prefix("tomorrow") {
        let rest = rest.trim().strip_prefix("at").unwrap_or(rest).trim();
        let (h, m) = if rest.is_empty() {
            (9, 0) // "tomorrow" alone → 9am
        } else {
            parse_clock(rest)?
        };
        let target_date = (now_local + Duration::days(1)).date_naive();
        return assemble(tz, target_date, h, m, now_utc, false);
    }

    // 4. "tonight at <clock>" / "this evening at <clock>".
    // Evening context: a bare hour 1–11 with no am/pm is interpreted as PM.
    if let Some(rest) = s.strip_prefix("tonight") {
        let rest = rest.trim().strip_prefix("at").unwrap_or(rest).trim();
        let (h, m) = if rest.is_empty() { (20, 0) } else { parse_clock_evening(rest)? };
        return resolve_clock(now_local, tz, h, m, true);
    }
    if let Some(rest) = s.strip_prefix("this evening") {
        let rest = rest.trim().strip_prefix("at").unwrap_or(rest).trim();
        let (h, m) = if rest.is_empty() { (20, 0) } else { parse_clock_evening(rest)? };
        return resolve_clock(now_local, tz, h, m, true);
    }

    // 5. Weekday: "next monday at 10am", "monday at 10am"
    if let Some(result) = parse_weekday(&s, tz, now_local, now_utc)? {
        return Ok(result);
    }

    // 6. Explicit month/day: "june 15 at 6:00 am"
    if let Some(result) = parse_month_day(&s, tz, now_local, now_utc)? {
        return Ok(result);
    }

    // 7. Absolute-today clock: "at 3pm", "3pm", "15:00", "8:30 pm"
    let clock_part = s.strip_prefix("at ").map(str::trim).unwrap_or(&s);
    let (h, m) = parse_clock(clock_part)?;
    resolve_clock(now_local, tz, h, m, false)
}

/// Parse "N <unit>" relative offset from `now_utc`.
fn parse_relative(rest: &str, now_utc: DateTime<Utc>) -> Result<DateTime<Utc>, ParseError> {
    let mut parts = rest.split_whitespace();
    let n_str = parts
        .next()
        .ok_or_else(|| ParseError(format!("missing amount in 'in {rest}'")))?;
    let unit = parts
        .next()
        .ok_or_else(|| ParseError(format!("missing unit in 'in {rest}'")))?;
    let n: i64 = n_str
        .parse()
        .map_err(|_| ParseError(format!("'{n_str}' is not a number")))?;
    if n < 0 {
        return Err(ParseError("relative amount must not be negative".into()));
    }

    let dur = match unit.trim_end_matches(',') {
        "second" | "seconds" | "sec" | "secs" | "s" => Duration::seconds(n),
        "minute" | "minutes" | "min" | "mins" | "m" => Duration::minutes(n),
        "hour" | "hours" | "hr" | "hrs" | "h" => Duration::hours(n),
        "day" | "days" | "d" => Duration::days(n),
        "week" | "weeks" | "wk" | "wks" => Duration::weeks(n),
        other => return Err(ParseError(format!("unknown unit '{other}'"))),
    };
    Ok(now_utc + dur)
}

/// Parse a clock string into (hour24, minute).
///
/// Accepts: "3pm", "3 pm", "8:30pm", "8:30 PM", "15:00", "9", "9am".
fn parse_clock(s: &str) -> Result<(u32, u32), ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError("empty clock".into()));
    }
    let lower = s.to_lowercase();

    // Detect am/pm suffix.
    let (body, ampm) = if let Some(b) = lower.strip_suffix("pm") {
        (b.trim(), Some(true))
    } else if let Some(b) = lower.strip_suffix("am") {
        (b.trim(), Some(false))
    } else if let Some(b) = lower.strip_suffix('p') {
        (b.trim(), Some(true))
    } else if let Some(b) = lower.strip_suffix('a') {
        (b.trim(), Some(false))
    } else {
        (lower.as_str(), None)
    };

    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };
    let mut hour: u32 = h_str
        .trim()
        .parse()
        .map_err(|_| ParseError(format!("bad hour '{h_str}'")))?;
    let minute: u32 = m_str
        .trim()
        .parse()
        .map_err(|_| ParseError(format!("bad minute '{m_str}'")))?;

    if minute > 59 {
        return Err(ParseError(format!("minute {minute} out of range")));
    }

    match ampm {
        Some(true) => {
            // pm: 12pm stays 12, 1–11pm add 12.
            if hour == 12 {
                // noon
            } else if hour < 12 {
                hour += 12;
            } else {
                return Err(ParseError(format!("hour {hour} invalid with pm")));
            }
        }
        Some(false) => {
            // am: 12am → 0.
            if hour == 12 {
                hour = 0;
            } else if hour > 12 {
                return Err(ParseError(format!("hour {hour} invalid with am")));
            }
        }
        None => {
            if hour > 23 {
                return Err(ParseError(format!("hour {hour} out of range")));
            }
        }
    }

    Ok((hour, minute))
}

/// Like [`parse_clock`], but a bare hour 1–11 with no am/pm marker is treated
/// as PM (evening context: "tonight at 8" → 20:00). An explicit am/pm or a
/// 24-hour value (>= 12, or with ':') is honored as-is.
fn parse_clock_evening(s: &str) -> Result<(u32, u32), ParseError> {
    let lower = s.trim().to_lowercase();
    let has_marker = lower.ends_with("am")
        || lower.ends_with("pm")
        || lower.ends_with('a')
        || lower.ends_with('p');
    let (h, m) = parse_clock(s)?;
    if !has_marker && h >= 1 && h <= 11 {
        Ok((h + 12, m))
    } else {
        Ok((h, m))
    }
}

/// Resolve a clock time on today's date in `tz`; if `now`-relative and already
/// past, roll to tomorrow. `force_today` keeps it today even if past (used by
/// named instants like "noon" only when explicitly desired — here false rolls).
fn resolve_clock(
    now_local: DateTime<Tz>,
    tz: Tz,
    hour: u32,
    minute: u32,
    _force_today: bool,
) -> Result<DateTime<Utc>, ParseError> {
    let today = now_local.date_naive();
    let candidate = assemble(tz, today, hour, minute, now_local.with_timezone(&Utc), true)?;
    Ok(candidate)
}

/// Build a UTC instant from a local date + clock, handling DST gaps/folds.
///
/// If `roll_if_past` and the result is <= now, advance by one day.
fn assemble(
    tz: Tz,
    date: NaiveDate,
    hour: u32,
    minute: u32,
    now_utc: DateTime<Utc>,
    roll_if_past: bool,
) -> Result<DateTime<Utc>, ParseError> {
    let time = NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| ParseError(format!("invalid clock {hour}:{minute}")))?;
    let mut d = date;
    loop {
        let naive = d.and_time(time);
        let local = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => dt,
            chrono::LocalResult::Ambiguous(dt, _) => dt, // DST fold: pick earlier
            chrono::LocalResult::None => {
                // DST gap (spring forward): bump 1h and retry that day.
                let bumped = d.and_time(
                    NaiveTime::from_hms_opt((hour + 1).min(23), minute, 0)
                        .unwrap_or(time),
                );
                match tz.from_local_datetime(&bumped) {
                    chrono::LocalResult::Single(dt) => dt,
                    chrono::LocalResult::Ambiguous(dt, _) => dt,
                    chrono::LocalResult::None => {
                        return Err(ParseError("unresolvable local time (DST)".into()))
                    }
                }
            }
        };
        let utc = local.with_timezone(&Utc);
        if roll_if_past && utc <= now_utc {
            d += Duration::days(1);
            continue;
        }
        return Ok(utc);
    }
}

/// Parse "[next] <weekday> [at] <clock>".
fn parse_weekday(
    s: &str,
    tz: Tz,
    now_local: DateTime<Tz>,
    now_utc: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, ParseError> {
    let (force_next, rest) = match s.strip_prefix("next ") {
        Some(r) => (true, r),
        None => (false, s),
    };
    let mut words = rest.split_whitespace();
    let first = match words.next() {
        Some(w) => w,
        None => return Ok(None),
    };
    let weekday = match weekday_from_str(first) {
        Some(w) => w,
        None => return Ok(None),
    };
    // Remaining: optional "at" + clock.
    let remainder: String = words.collect::<Vec<_>>().join(" ");
    let clock_str = remainder.trim().strip_prefix("at").unwrap_or(&remainder).trim();
    let (h, m) = if clock_str.is_empty() { (9, 0) } else { parse_clock(clock_str)? };

    // Days until the target weekday.
    let today_wd = now_local.weekday();
    let mut days = (weekday.num_days_from_monday() as i64
        - today_wd.num_days_from_monday() as i64)
        .rem_euclid(7);
    if force_next && days == 0 {
        days = 7;
    }
    let target_date = (now_local + Duration::days(days)).date_naive();
    // If it's today (days==0, not forced) but the clock already passed, roll a week.
    let result = assemble(tz, target_date, h, m, now_utc, false)?;
    if days == 0 && result <= now_utc {
        let next_week = (now_local + Duration::days(7)).date_naive();
        return Ok(Some(assemble(tz, next_week, h, m, now_utc, false)?));
    }
    Ok(Some(result))
}

fn weekday_from_str(s: &str) -> Option<Weekday> {
    match s {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" | "tues" => Some(Weekday::Tue),
        "wednesday" | "wed" => Some(Weekday::Wed),
        "thursday" | "thu" | "thur" | "thurs" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Parse "<month> <day> [at] <clock>", e.g. "june 15 at 6:00 am".
fn parse_month_day(
    s: &str,
    tz: Tz,
    now_local: DateTime<Tz>,
    now_utc: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, ParseError> {
    let mut words = s.split_whitespace();
    let first = match words.next() {
        Some(w) => w,
        None => return Ok(None),
    };
    let month = match month_from_str(first) {
        Some(m) => m,
        None => return Ok(None),
    };
    let day_str = match words.next() {
        Some(w) => w.trim_end_matches(['s', 't', 'n', 'd', 'r', 'h', ',']), // strip "15th"/"1st"
        None => return Err(ParseError("month given without a day".into())),
    };
    let day: u32 = day_str
        .parse()
        .map_err(|_| ParseError(format!("bad day '{day_str}'")))?;

    let remainder: String = words.collect::<Vec<_>>().join(" ");
    let clock_str = remainder.trim().strip_prefix("at").unwrap_or(&remainder).trim();
    let (h, m) = if clock_str.is_empty() { (9, 0) } else { parse_clock(clock_str)? };

    // Pick the year: this year if the date is still in the future, else next year.
    let year = now_local.year();
    let date = NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| ParseError(format!("invalid date {month}/{day}")))?;
    let candidate = assemble(tz, date, h, m, now_utc, false)?;
    if candidate <= now_utc {
        let next = NaiveDate::from_ymd_opt(year + 1, month, day)
            .ok_or_else(|| ParseError(format!("invalid date {month}/{day}")))?;
        return Ok(Some(assemble(tz, next, h, m, now_utc, false)?));
    }
    Ok(Some(candidate))
}

fn month_from_str(s: &str) -> Option<u32> {
    match s {
        "january" | "jan" => Some(1),
        "february" | "feb" => Some(2),
        "march" | "mar" => Some(3),
        "april" | "apr" => Some(4),
        "may" => Some(5),
        "june" | "jun" => Some(6),
        "july" | "jul" => Some(7),
        "august" | "aug" => Some(8),
        "september" | "sep" | "sept" => Some(9),
        "october" | "oct" => Some(10),
        "november" | "nov" => Some(11),
        "december" | "dec" => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::America::Los_Angeles;

    /// Reference now: 2026-06-12 15:00:00 PDT == 2026-06-12 22:00:00 UTC.
    /// (PDT = UTC-7 in summer.)
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 12, 22, 0, 0).unwrap()
    }

    fn la_to_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Los_Angeles
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn parse(p: &str) -> DateTime<Utc> {
        parse_time(p, Los_Angeles, now()).expect(p)
    }

    // ── relative ──────────────────────────────────────────────────────────
    #[test]
    fn rel_minutes() {
        assert_eq!(parse("in 30 minutes"), now() + Duration::minutes(30));
    }
    #[test]
    fn rel_hours() {
        assert_eq!(parse("in 2 hours"), now() + Duration::hours(2));
    }
    #[test]
    fn rel_seconds() {
        assert_eq!(parse("in 45 seconds"), now() + Duration::seconds(45));
    }
    #[test]
    fn rel_single_minute_abbrev() {
        assert_eq!(parse("in 1 min"), now() + Duration::minutes(1));
    }
    #[test]
    fn rel_day() {
        assert_eq!(parse("in 1 day"), now() + Duration::days(1));
    }
    #[test]
    fn rel_90_seconds() {
        assert_eq!(parse("in 90 seconds"), now() + Duration::seconds(90));
    }

    // ── absolute-today ────────────────────────────────────────────────────
    #[test]
    fn at_3pm_future_today() {
        // now is 3pm local; "at 3pm" equals now → rolls to tomorrow.
        assert_eq!(parse("at 3pm"), la_to_utc(2026, 6, 13, 15, 0));
    }
    #[test]
    fn at_5pm_today() {
        // 5pm local is still in the future today.
        assert_eq!(parse("at 5pm"), la_to_utc(2026, 6, 12, 17, 0));
    }
    #[test]
    fn at_2pm_rolls_to_tomorrow() {
        // 2pm already passed (now 3pm) → tomorrow 2pm.
        assert_eq!(parse("at 2pm"), la_to_utc(2026, 6, 13, 14, 0));
    }
    #[test]
    fn at_24h_clock() {
        assert_eq!(parse("at 17:30"), la_to_utc(2026, 6, 12, 17, 30));
    }
    #[test]
    fn at_830_pm() {
        assert_eq!(parse("at 8:30 PM"), la_to_utc(2026, 6, 12, 20, 30));
    }
    #[test]
    fn bare_clock_no_at() {
        assert_eq!(parse("6pm"), la_to_utc(2026, 6, 12, 18, 0));
    }

    // ── tomorrow ──────────────────────────────────────────────────────────
    #[test]
    fn tomorrow_9am() {
        assert_eq!(parse("tomorrow at 9am"), la_to_utc(2026, 6, 13, 9, 0));
    }
    #[test]
    fn tomorrow_no_at() {
        assert_eq!(parse("tomorrow 9am"), la_to_utc(2026, 6, 13, 9, 0));
    }
    #[test]
    fn tomorrow_alone_defaults_9am() {
        assert_eq!(parse("tomorrow"), la_to_utc(2026, 6, 13, 9, 0));
    }

    // ── month/day ─────────────────────────────────────────────────────────
    #[test]
    fn june_15_6am() {
        assert_eq!(parse("June 15 at 6:00 AM"), la_to_utc(2026, 6, 15, 6, 0));
    }
    #[test]
    fn month_day_past_rolls_next_year() {
        // Jan 5 already passed in 2026 → 2027.
        assert_eq!(parse("january 5 at 8am"), la_to_utc(2027, 1, 5, 8, 0));
    }
    #[test]
    fn ordinal_day() {
        assert_eq!(parse("june 15th at 6am"), la_to_utc(2026, 6, 15, 6, 0));
    }

    // ── weekday ───────────────────────────────────────────────────────────
    #[test]
    fn next_monday_10am() {
        // 2026-06-12 is a Friday. Next Monday is 2026-06-15.
        assert_eq!(parse("next Monday at 10am"), la_to_utc(2026, 6, 15, 10, 0));
    }
    #[test]
    fn weekday_this_week() {
        // Saturday is tomorrow (2026-06-13).
        assert_eq!(parse("saturday at 10am"), la_to_utc(2026, 6, 13, 10, 0));
    }
    #[test]
    fn next_friday_is_a_week_out() {
        // Today is Friday; "next friday" forces +7 days → 2026-06-19.
        assert_eq!(parse("next friday at 9am"), la_to_utc(2026, 6, 19, 9, 0));
    }

    // ── shorthand ─────────────────────────────────────────────────────────
    #[test]
    fn tonight_at_8() {
        assert_eq!(parse("tonight at 8"), la_to_utc(2026, 6, 12, 20, 0));
    }
    #[test]
    fn tonight_alone() {
        assert_eq!(parse("tonight"), la_to_utc(2026, 6, 12, 20, 0));
    }
    #[test]
    fn this_evening() {
        assert_eq!(parse("this evening"), la_to_utc(2026, 6, 12, 20, 0));
    }
    #[test]
    fn noon_rolls_tomorrow() {
        // noon already passed (now 3pm) → tomorrow noon.
        assert_eq!(parse("noon"), la_to_utc(2026, 6, 13, 12, 0));
    }
    #[test]
    fn midnight_rolls_tomorrow() {
        assert_eq!(parse("midnight"), la_to_utc(2026, 6, 13, 0, 0));
    }

    // ── errors ────────────────────────────────────────────────────────────
    #[test]
    fn empty_is_error() {
        assert!(parse_time("", Los_Angeles, now()).is_err());
    }
    #[test]
    fn gibberish_is_error() {
        assert!(parse_time("flibbertigibbet", Los_Angeles, now()).is_err());
    }
    #[test]
    fn unknown_unit_is_error() {
        assert!(parse_time("in 5 fortnights", Los_Angeles, now()).is_err());
    }
}
