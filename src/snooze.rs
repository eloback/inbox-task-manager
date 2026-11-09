//! Snooze times as people type them (R4.1): `30m`, `2h`, `1h30m`, `2d`, `45`
//! (minutes), `14:30`, `5pm`, `today 17:00`, `tomorrow 9:00`, `friday 2pm`,
//! `next mon`, `2026-12-01 14:30`, read by itm itself in local wall-clock
//! time, and any other phrase (`next week`, `+3 days`) as GNU `date -d`
//! reads it, when there is one ([`crate::system::date_reads`]).

use std::sync::LazyLock;

use jiff::{Timestamp, ToSpan, civil, tz::TimeZone};
use regex_lite::Regex;

use crate::error::{Error, Result, bail};
use crate::times;

pub const MAX_SNOOZE: i64 = 10 * 365 * 86400;

/// How long a task stays snoozed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// Until this epoch second.
    Time(i64),
    /// No timer: until one of its agents needs you (R4.5).
    Agent,
}

impl Until {
    /// A snooze as typed: "agent", a time itm reads itself, else what `date`
    /// reads the phrase as (an epoch second; GNU `date -d` in itm,
    /// [`crate::system::date_reads`]).
    pub fn parse(text: &str, now: i64, tz: &TimeZone, date: impl FnOnce(&str) -> Option<i64>) -> Result<Until> {
        match Until::read(text, now, tz)? {
            Some(until) => Ok(until),
            None => checked(text, date(text.trim()).ok_or_else(|| unreadable(text))?, now).map(Until::Time),
        }
    }

    /// A snooze as typed, without running anything: "agent" or a time itm
    /// reads itself. None for a phrase only GNU `date` reads (`next week`):
    /// a screen has it read by a worker ([`Until::parse`]).
    pub fn read(text: &str, now: i64, tz: &TimeZone) -> Result<Option<Until>> {
        match text.trim().to_lowercase().as_str() {
            "agent" | "until an agent needs me" => Ok(Some(Until::Agent)),
            _ => Ok(parse_when(text, now, tz)?.map(Until::Time)),
        }
    }

    pub fn describe(self, now: i64, tz: &TimeZone) -> String {
        match self {
            Until::Agent => "until an agent needs you".into(),
            Until::Time(ts) => format!("until {}", times::clock_or_date(ts, now, tz)),
        }
    }
}

/// The snooze dialog's choices, in time order without past ones (UX
/// principle 9), then "Until an agent needs me" when it could ever wake the task.
pub fn presets(now: i64, tz: &TimeZone, agent: bool) -> Vec<(&'static str, Until)> {
    let today = Timestamp::from_second(now).map(|t| t.to_zoned(tz.clone()).date()).unwrap_or_default();
    let at = |days: i64, hour: i8| -> Option<i64> {
        let day = today.checked_add(days.days()).ok()?;
        Some(day.at(hour, 0, 0, 0).to_zoned(tz.clone()).ok()?.timestamp().as_second())
    };
    let to_monday = 7 - i64::from(today.weekday().to_monday_zero_offset()); // next week's, never today
    let mut timed: Vec<(&str, i64)> = [
        ("30 minutes", Some(now + 1800)),
        ("1 hour", Some(now + 3600)),
        ("This evening", at(0, 20)),
        ("Tomorrow morning", at(1, 9)),
        ("Next Monday", at(to_monday, 9)),
    ]
    .into_iter()
    .filter_map(|(label, when)| when.filter(|&w| w > now).map(|w| (label, w)))
    .collect();
    timed.sort_by_key(|&(_, when)| when);
    let mut presets: Vec<(&str, Until)> = timed.into_iter().map(|(label, when)| (label, Until::Time(when))).collect();
    if agent {
        presets.push(("Until an agent needs me", Until::Agent));
    }
    presets
}

/// The epoch second a snooze typed as `text` ends, read by itm itself: in
/// the future, at most 10 years away. None for a phrase only GNU `date` reads.
fn parse_when(text: &str, now: i64, tz: &TimeZone) -> Result<Option<i64>> {
    when(text, now, tz)?.map(|when| checked(text, when, now)).transpose()
}

/// `when`, the end of a snooze typed as `text`, if it is in the future and at most 10 years away.
fn checked(text: &str, when: i64, now: i64) -> Result<i64> {
    if when <= now {
        bail!("snooze time must be in the future: {text}");
    }
    if when - now > MAX_SNOOZE {
        bail!("snooze time is too far away: {text}");
    }
    Ok(when)
}

static DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:([0-9]+)d)?(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?$").unwrap());

static DAY_AND_CLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:(?P<day>today|tomorrow|(?P<next>next\s+)?(?P<weekday>[a-z]{3,})|(?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2}))(?:\s+|$))?",
        r"(?:(?:at\s+)?(?P<hour>[0-9]{1,2})(?::(?P<minute>[0-9]{2}))?\s*(?P<ampm>am|pm)?)?$",
    ))
    .unwrap()
});

const WEEKDAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

fn unreadable(text: &str) -> Error {
    Error::Other(format!("cannot understand snooze time: {text}"))
}

/// What itm reads itself; None for any other phrase.
fn when(text: &str, now: i64, tz: &TimeZone) -> Result<Option<i64>> {
    let spec: String = text.trim().to_lowercase().split_whitespace().collect();
    if spec.is_empty() {
        bail!("empty snooze time");
    }
    let too_far = || Error::Other(format!("snooze time is too far away: {text}"));
    let number =
        |digits: &str, unit: i64| digits.parse::<i64>().ok().and_then(|n| n.checked_mul(unit)).ok_or_else(too_far);
    if spec.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(Some(now + number(&spec, 60)?));
    }
    if let Some(parts) = DURATION.captures(&spec)
        && parts.iter().skip(1).any(|part| part.is_some())
    {
        let mut total = now;
        for (i, unit) in [86400, 3600, 60, 1].into_iter().enumerate() {
            if let Some(digits) = parts.get(i + 1) {
                total = total.checked_add(number(digits.as_str(), unit)?).ok_or_else(too_far)?;
            }
        }
        return Ok(Some(total));
    }
    day_and_clock(text, now, tz)
}

/// `14:30`, `5pm`, `tomorrow 9:00`, `friday 2pm`, `next mon`, `2026-12-01 14:30`,
/// read like GNU `date -d` reads them: a weekday is today when it matches and
/// `next` skips today; a day without a clock starts at midnight, except
/// `tomorrow`, which keeps the time of day; a clock alone is the next time
/// it shows. None when `text` is not of this form.
fn day_and_clock(text: &str, now: i64, tz: &TimeZone) -> Result<Option<i64>> {
    let lower = text.trim().to_lowercase();
    let Some(m) = DAY_AND_CLOCK.captures(&lower) else { return Ok(None) };
    let day = m.name("day").map(|d| d.as_str());
    if day.is_none() && m.name("hour").is_none() {
        return Ok(None);
    }
    let invalid_date = || Error::Other(format!("invalid date: {text}"));
    let invalid_time = || Error::Other(format!("invalid time: {text}"));
    let base = Timestamp::from_second(now).map_err(|_| invalid_date())?.to_zoned(tz.clone());
    let date = if let Some(iso) = m.name("date") {
        iso.as_str().parse::<civil::Date>().map_err(|_| invalid_date())?
    } else if let Some(word) = m.name("weekday") {
        let word = word.as_str();
        let Some(weekday) = WEEKDAYS.iter().position(|name| name.starts_with(word)) else { return Ok(None) };
        let mut ahead = (weekday as i64 - base.weekday().to_monday_zero_offset() as i64).rem_euclid(7);
        if m.name("next").is_some() && ahead == 0 {
            ahead = 7;
        }
        base.date().checked_add(ahead.days()).map_err(|_| invalid_date())?
    } else {
        base.date().checked_add(i64::from(day == Some("tomorrow")).days()).map_err(|_| invalid_date())?
    };
    let clock = match m.name("hour") {
        None if day == Some("tomorrow") => {
            base.time().with().subsec_nanosecond(0).build().map_err(|_| invalid_time())?
        }
        None => civil::Time::midnight(),
        Some(hour) => {
            let mut hour: i8 = hour.as_str().parse().map_err(|_| invalid_time())?;
            let minute: i8 = m.name("minute").map_or(Ok(0), |x| x.as_str().parse()).map_err(|_| invalid_time())?;
            if let Some(ampm) = m.name("ampm") {
                if !(1..=12).contains(&hour) {
                    return Err(invalid_time());
                }
                hour = hour % 12 + if ampm.as_str() == "pm" { 12 } else { 0 };
            }
            civil::Time::new(hour, minute, 0, 0).map_err(|_| invalid_time())?
        }
    };
    // A DST gap moves the time forward, a fold takes the earlier one (jiff's "compatible").
    let mut target = date.to_datetime(clock).to_zoned(tz.clone()).map_err(|_| invalid_date())?;
    if day.is_none() && target.timestamp().as_second() <= now {
        target = target.checked_add(1.day()).map_err(|_| invalid_date())?; // a calendar day: same clock across DST
    }
    Ok(Some(target.timestamp().as_second()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// São Paulo without DST, as a POSIX rule: no time zone database needed.
    fn tz() -> TimeZone {
        TimeZone::posix("<-03>3").unwrap()
    }

    fn at(local: &str) -> i64 {
        local.parse::<civil::DateTime>().unwrap().to_zoned(tz()).unwrap().timestamp().as_second()
    }

    /// Wednesday 2026-11-04 10:00.
    fn base() -> i64 {
        at("2026-11-04T10:00")
    }

    #[test]
    fn durations() {
        let b = base();
        for (text, expected) in
            [("30m", b + 1800), ("1h30m", b + 5400), ("2d", b + 2 * 86400), ("1h 30m", b + 5400), ("45", b + 45 * 60)]
        {
            assert_eq!(parse_when(text, b, &tz()), Ok(Some(expected)), "{text}");
        }
    }

    #[test]
    fn phrases_itm_reads_itself() {
        let cases = [
            ("14:30", "2026-11-04T14:30"),
            ("9:00", "2026-11-05T09:00"),
            ("tomorrow 9:00", "2026-11-05T09:00"),
            ("tomorrow", "2026-11-05T10:00"),
            ("Tomorrow at 2pm", "2026-11-05T14:00"),
            ("friday 2pm", "2026-11-06T14:00"),
            ("wed 18:00", "2026-11-04T18:00"),
            ("next wed", "2026-11-11T00:00"),
            ("next friday 9am", "2026-11-06T09:00"),
            ("thurs 8:15", "2026-11-05T08:15"),
            ("today 17:00", "2026-11-04T17:00"),
            ("2026-12-01 14:30", "2026-12-01T14:30"),
            ("2026-12-01", "2026-12-01T00:00"),
            ("5pm", "2026-11-04T17:00"),
            ("at 9:30", "2026-11-05T09:30"),
            ("12am", "2026-11-05T00:00"),
            ("12pm", "2026-11-04T12:00"),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_when(text, base(), &tz()), Ok(Some(at(expected))), "{text}");
        }
    }

    /// What no `date` reads: GNU `date` is not installed.
    fn no_date(_: &str) -> Option<i64> {
        None
    }

    #[test]
    fn invalid_phrases() {
        for text in [
            "",
            "25:00",
            "soonish please",
            "0m",
            "0",
            "²",
            "99999999d",
            "tomorrow 25:00",
            "friday 13pm",
            "0am",
            "2026-13-01",
            "0001-01-01",
            "9999-12-31 23:59",
            "monkey",
            "sundae 9:00",
            "wednesday",
            "next week",
            "99999999999999999999",
        ] {
            assert!(Until::parse(text, base(), &tz(), no_date).is_err(), "{text:?} should be refused");
        }
        let said = |text| Until::parse(text, base(), &tz(), no_date).unwrap_err().to_string();
        assert_eq!(said("next week"), "cannot understand snooze time: next week");
        assert_eq!(said("0m"), "snooze time must be in the future: 0m");
        assert_eq!(said("2026-13-01"), "invalid date: 2026-13-01");
    }

    #[test]
    fn r4_1_other_phrases_go_to_gnu_date() {
        // Read by itm itself: none for date; the rest is date's, checked like itm's own.
        assert_eq!(parse_when("next week", base(), &tz()), Ok(None));
        assert_eq!(parse_when("+3 days", base(), &tz()), Ok(None));
        assert_eq!(Until::read("next week", base(), &tz()), Ok(None));
        assert_eq!(Until::read("2h", base(), &tz()), Ok(Some(Until::Time(base() + 7200))));
        let week = base() + 7 * 86400;
        let mut asked = vec![];
        let date = |phrase: &str| {
            asked.push(phrase.to_string());
            Some(week)
        };
        assert_eq!(Until::parse("  next week ", base(), &tz(), date), Ok(Until::Time(week)));
        assert_eq!(asked, ["next week"], "trimmed");
        // date is asked only about what itm does not read itself.
        let never = |_: &str| -> Option<i64> { panic!("date asked about a phrase itm reads") };
        assert_eq!(Until::parse("tomorrow 9:00", base(), &tz(), never), Ok(Until::Time(at("2026-11-05T09:00"))));
        assert!(Until::parse("25:00", base(), &tz(), never).is_err());
        // What date reads is in the future and at most 10 years away, like any snooze.
        let said = |text, at: i64| Until::parse(text, base(), &tz(), |_| Some(at)).unwrap_err().to_string();
        assert_eq!(said("last week", base() - 7 * 86400), "snooze time must be in the future: last week");
        assert_eq!(said("+20 years", base() + 20 * 366 * 86400), "snooze time is too far away: +20 years");
    }

    #[test]
    fn a_clock_keeps_its_wall_time_across_a_dst_change() {
        // New York, the evening before DST ends (2026-11-01 02:00 back to 01:00).
        let ny = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let now = "2026-10-31T23:00".parse::<civil::DateTime>().unwrap().to_zoned(ny.clone()).unwrap();
        let when = parse_when("22:00", now.timestamp().as_second(), &ny).unwrap().unwrap();
        let local = Timestamp::from_second(when).unwrap().to_zoned(ny);
        assert_eq!(local.datetime(), "2026-11-01T22:00".parse::<civil::DateTime>().unwrap());
    }

    #[test]
    fn presets_are_in_time_order_without_past_ones() {
        let labels = |now: &str, agent| -> Vec<&str> { presets(at(now), &tz(), agent).iter().map(|p| p.0).collect() };
        // Wednesday morning: the evening comes before tomorrow and Monday.
        let wednesday =
            ["30 minutes", "1 hour", "This evening", "Tomorrow morning", "Next Monday", "Until an agent needs me"];
        assert_eq!(labels("2026-11-04T10:00", true), wednesday);
        // Wednesday 19:45: the evening is closer than an hour.
        assert_eq!(labels("2026-11-04T19:45", false)[..3], ["This evening", "30 minutes", "1 hour"]);
        // Wednesday 21:00: the evening has passed.
        assert!(!labels("2026-11-04T21:00", false).contains(&"This evening"));
        // Monday morning: next Monday is a week away.
        let monday = presets(at("2026-11-09T08:00"), &tz(), false);
        assert_eq!(monday.last().unwrap(), &("Next Monday", Until::Time(at("2026-11-16T09:00"))));
    }

    #[test]
    fn until_an_agent_and_descriptions() {
        assert_eq!(Until::parse("Agent", base(), &tz(), no_date), Ok(Until::Agent));
        assert_eq!(Until::parse("2h", base(), &tz(), no_date), Ok(Until::Time(base() + 7200)));
        assert_eq!(Until::Agent.describe(base(), &tz()), "until an agent needs you");
        assert_eq!(Until::Time(at("2026-11-04T17:00")).describe(base(), &tz()), "until 17:00");
    }
}
