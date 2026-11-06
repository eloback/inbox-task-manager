//! Epoch seconds and how itm shows them to people (DESIGN.md section 3,
//! "Human times": "in 1h 30m", "2d ago", "tomorrow 09:00").

use jiff::{Timestamp, ToSpan, Zoned, tz::TimeZone};

pub fn now() -> i64 {
    Timestamp::now().as_second()
}

fn local(ts: i64, tz: &TimeZone) -> Option<Zoned> {
    Timestamp::from_second(ts).ok().map(|t| t.to_zoned(tz.clone()))
}

/// "in 1h 30m", "2d ago", "now": a distance in time, rounded down.
pub fn rel(ts: i64, now: i64) -> String {
    let delta = ts - now;
    let s = delta.abs();
    let text = if s < 60 {
        return "now".into();
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        let (h, m) = (s / 3600, s % 3600 / 60);
        if m > 0 && h < 3 { format!("{h}h {m}m") } else { format!("{h}h") }
    } else {
        format!("{}d", s / 86400)
    };
    if delta > 0 { format!("in {text}") } else { format!("{text} ago") }
}

/// How long ago, without "ago": "4m", "2h".
pub fn age(ts: i64, now: i64) -> String {
    let text = rel(ts, now);
    text.strip_suffix(" ago").map(str::to_owned).unwrap_or(text)
}

/// "14:30" today, else "2026-12-01 14:30" (the CLI's messages).
pub fn clock_or_date(ts: i64, now: i64, tz: &TimeZone) -> String {
    let (Some(when), Some(today)) = (local(ts, tz), local(now, tz)) else { return "?".into() };
    if when.date() == today.date() {
        when.strftime("%H:%M").to_string()
    } else {
        when.strftime("%Y-%m-%d %H:%M").to_string()
    }
}

/// "14:30" today, "tomorrow 09:00", else "Fri 06 Nov 14:00" (the switcher).
pub fn day_and_clock(ts: i64, now: i64, tz: &TimeZone) -> String {
    let (Some(when), Some(today)) = (local(ts, tz), local(now, tz)) else { return "?".into() };
    if when.date() == today.date() {
        when.strftime("%H:%M").to_string()
    } else if today.date().checked_add(1.day()).is_ok_and(|d| d == when.date()) {
        when.strftime("tomorrow %H:%M").to_string()
    } else {
        when.strftime("%a %d %b %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_round_down_and_say_which_way() {
        let now = 1_000_000;
        assert_eq!(rel(now + 30, now), "now");
        assert_eq!(rel(now + 5 * 60, now), "in 5m");
        assert_eq!(rel(now + 90 * 60, now), "in 1h 30m");
        assert_eq!(rel(now + 4 * 3600 + 600, now), "in 4h");
        assert_eq!(rel(now - 2 * 86400 - 5, now), "2d ago");
        assert_eq!(age(now - 240, now), "4m");
    }

    #[test]
    fn days_are_named_in_local_time() {
        let tz = TimeZone::fixed(jiff::tz::offset(-3));
        let at =
            |s: &str| s.parse::<jiff::civil::DateTime>().unwrap().to_zoned(tz.clone()).unwrap().timestamp().as_second();
        let now = at("2026-11-04T10:00");
        assert_eq!(day_and_clock(at("2026-11-04T17:00"), now, &tz), "17:00");
        assert_eq!(day_and_clock(at("2026-11-05T09:00"), now, &tz), "tomorrow 09:00");
        assert_eq!(day_and_clock(at("2026-11-06T14:00"), now, &tz), "Fri 06 Nov 14:00");
        assert_eq!(clock_or_date(at("2026-12-01T14:30"), now, &tz), "2026-12-01 14:30");
    }
}
