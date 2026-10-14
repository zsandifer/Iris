//! When a routine runs: once at a pinned local date and time, on a small
//! repeat rule (daily, weekly, monthly, yearly), or on a cron pattern.
//!
//! All times are wall-clock times in the routine's IANA time zone. Next
//! runs are worked out from the scheduled slot (never from when a run
//! actually started), so a late or slow run does not drift the schedule.
//!
//! Daylight saving time:
//! - a time that does not exist (spring forward) runs at the next valid
//!   time, the moment the clocks jump;
//! - a time that happens twice (fall back) runs only the first time.
//!
//! Croner follows the same two rules for fixed-time cron patterns.

use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// How far ahead a rule searches for its next date (a little over ten
/// years: the longest gap a valid rule can have).
const MAX_SEARCH_DAYS: i64 = 3700;
/// Cron patterns may not run more often than this.
pub const MIN_CRON_GAP_MINS: i64 = 30;

/// A repeat rule. `every` counts days, weeks, months, or years from the
/// `anchor` date (default 1: every one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(flatten)]
    pub repeat: Repeat,
    #[serde(default = "one")]
    pub every: u32,
    /// Local time of day, "HH:MM" (24-hour).
    pub time: String,
    /// "YYYY-MM-DD": the first day, week, month, or year that counts for
    /// `every`. Needed when `every` is more than 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "freq", rename_all = "lowercase")]
pub enum Repeat {
    Daily,
    /// `days`: 0 = Sunday … 6 = Saturday.
    Weekly {
        days: Vec<u8>,
    },
    /// Either `day` (1–31; months without it use their last day) or `nth`
    /// (1–4, or -1 for the last) `weekday` (0 = Sunday … 6 = Saturday).
    Monthly {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        day: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nth: Option<i8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weekday: Option<u8>,
    },
    /// `month` 1–12, `day` 1–31 (Feb 29 runs on Feb 28 in other years).
    Yearly {
        month: u8,
        day: u8,
    },
}

/// A parsed, validated schedule in a time zone.
#[derive(Debug, Clone)]
pub struct Plan {
    pub when: When,
    pub tz: TimeZone,
    /// First local date a repeating schedule may run on.
    pub starts: Option<Date>,
    /// Last local date a repeating schedule may run on (inclusive).
    pub ends: Option<Date>,
}

#[derive(Debug, Clone)]
pub enum When {
    Once(DateTime),
    Rule(ParsedRule),
    Cron(Box<croner::Cron>),
}

#[derive(Debug, Clone)]
pub struct ParsedRule {
    pub repeat: Repeat,
    pub every: i64,
    pub time: Time,
    pub anchor: Option<Date>,
}

pub fn parse_tz(name: &str) -> Result<TimeZone, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("I need a time zone, like America/Chicago.".into());
    }
    TimeZone::get(name).map_err(|_| format!("I don't know the time zone \"{name}\"."))
}

pub fn parse_date(field: &str, s: &str) -> Result<Date, String> {
    Date::from_str(s.trim())
        .map_err(|_| format!("I need {field} as a date like 2026-10-29, not \"{s}\"."))
}

pub fn parse_time(s: &str) -> Result<Time, String> {
    let bad = || format!("I need a time like 17:30, not \"{s}\".");
    let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
    let (h, m): (i8, i8) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    Time::new(h, m, 0, 0).map_err(|_| bad())
}

/// "YYYY-MM-DDTHH:MM" (a space instead of the T works too).
pub fn parse_local(s: &str) -> Result<DateTime, String> {
    let bad = || format!("I need a date and time like 2026-10-29T08:00, not \"{s}\".");
    let t = s.trim().replace(' ', "T");
    let (d, tm) = t.split_once('T').ok_or_else(bad)?;
    let date = Date::from_str(d).map_err(|_| bad())?;
    let time = parse_time(&tm[..tm.len().min(5)]).map_err(|_| bad())?;
    Ok(date.to_datetime(time))
}

fn weekday(n: u8) -> Result<Weekday, String> {
    Weekday::from_sunday_zero_offset(n as i8)
        .map_err(|_| "Weekdays go from 0 (Sunday) to 6 (Saturday).".to_string())
}

/// Parse a 5-field cron pattern (minute granularity only).
pub fn parse_cron(expr: &str) -> Result<croner::Cron, String> {
    let expr = expr.trim();
    if expr.split_whitespace().count() != 5 && !expr.starts_with('@') {
        return Err(
            "A custom schedule needs five parts: minute hour day month weekday \
             (for example 30 17 * * 6)."
                .into(),
        );
    }
    croner::Cron::from_str(expr).map_err(|e| format!("I can't read that custom schedule: {e}."))
}

impl ParsedRule {
    pub fn parse(rule: &Rule) -> Result<Self, String> {
        let max_every = match rule.repeat {
            Repeat::Daily => 365,
            Repeat::Weekly { .. } => 52,
            Repeat::Monthly { .. } => 24,
            Repeat::Yearly { .. } => 10,
        };
        if rule.every == 0 || rule.every > max_every {
            return Err(format!(
                "\"Every\" has to be between 1 and {max_every} for this kind of routine."
            ));
        }
        match &rule.repeat {
            Repeat::Daily => {}
            Repeat::Weekly { days } => {
                if days.is_empty() {
                    return Err("Pick at least one day of the week.".into());
                }
                for d in days {
                    weekday(*d)?;
                }
            }
            Repeat::Monthly {
                day,
                nth,
                weekday: wd,
            } => match (day, nth, wd) {
                (Some(d), None, None) if (1..=31).contains(d) => {}
                (None, Some(1..=4 | -1), Some(w)) => {
                    weekday(*w)?;
                }
                _ => {
                    return Err(
                        "A monthly routine runs on a day (1 to 31) or on the first, \
                         second, third, fourth or last weekday of the month."
                            .into(),
                    );
                }
            },
            Repeat::Yearly { month, day } => {
                if !(1..=12).contains(month) || !(1..=31).contains(day) {
                    return Err(
                        "A yearly routine needs a month (1 to 12) and a day (1 to 31).".into(),
                    );
                }
                Date::new(2024, *month as i8, *day as i8)
                    .map_err(|_| "That day doesn't exist in that month.".to_string())?;
            }
        }
        let anchor = rule
            .anchor
            .as_deref()
            .map(|a| parse_date("the first date", a))
            .transpose()?;
        if rule.every > 1 && anchor.is_none() {
            return Err("A routine that skips some days or weeks needs a first date.".into());
        }
        Ok(Self {
            repeat: rule.repeat.clone(),
            every: rule.every as i64,
            time: parse_time(&rule.time)?,
            anchor,
        })
    }

    /// Whether the rule runs on local date `d`.
    pub fn matches(&self, d: Date) -> bool {
        let every_ok = |n: i64| n.rem_euclid(self.every) == 0;
        let anchor = self.anchor.unwrap_or(d);
        match &self.repeat {
            Repeat::Daily => every_ok(days_between(anchor, d)),
            Repeat::Weekly { days } => {
                let wd = d.weekday().to_sunday_zero_offset() as u8;
                days.contains(&wd) && every_ok(days_between(week_start(anchor), week_start(d)) / 7)
            }
            Repeat::Monthly {
                day,
                nth,
                weekday: wd,
            } => {
                if !every_ok(months_between(anchor, d)) {
                    return false;
                }
                match (day, nth, wd) {
                    (Some(day), _, _) => d.day() == (*day as i8).min(d.days_in_month()),
                    (None, Some(n), Some(w)) => {
                        let Ok(w) = weekday(*w) else { return false };
                        d.nth_weekday_of_month(*n, w).ok() == Some(d)
                    }
                    _ => false,
                }
            }
            Repeat::Yearly { month, day } => {
                every_ok((d.year() - anchor.year()) as i64)
                    && d.month() == *month as i8
                    && d.day() == (*day as i8).min(d.days_in_month())
            }
        }
    }
}

fn days_between(a: Date, b: Date) -> i64 {
    b.duration_since(a).as_secs().div_euclid(86_400)
}

fn week_start(d: Date) -> Date {
    let back = d.weekday().to_sunday_zero_offset() as i64;
    d.checked_sub(jiff::Span::new().days(back)).unwrap_or(d)
}

fn months_between(a: Date, b: Date) -> i64 {
    (b.year() as i64 * 12 + b.month() as i64) - (a.year() as i64 * 12 + a.month() as i64)
}

/// The instant a local date and time happens: the first one when the
/// clocks fall back, the moment they jump when it does not exist.
pub fn resolve(tz: &TimeZone, dt: DateTime) -> Option<Timestamp> {
    match tz.to_ambiguous_timestamp(dt).offset() {
        AmbiguousOffset::Unambiguous { offset } => offset.to_timestamp(dt).ok(),
        AmbiguousOffset::Fold { before, .. } => before.to_timestamp(dt).ok(),
        AmbiguousOffset::Gap { before, .. } => {
            // With the offset from before the gap, the time lands after
            // the transition; the transition itself is the first valid time.
            let past = before.to_timestamp(dt).ok()?;
            let probe = past.checked_add(jiff::SignedDuration::from_secs(1)).ok()?;
            tz.preceding(probe).next().map(|t| t.timestamp())
        }
    }
}

fn ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}

pub fn zoned(tz: &TimeZone, at_ms: i64) -> Option<Zoned> {
    Timestamp::from_millisecond(at_ms)
        .ok()
        .map(|t| t.to_zoned(tz.clone()))
}

/// The next local time a cron pattern matches after (or at) `civil`.
fn cron_after(cron: &croner::Cron, civil: DateTime, inclusive: bool) -> Option<DateTime> {
    let from = civil.to_zoned(TimeZone::UTC).ok()?;
    cron.find_next_occurrence(&from, inclusive)
        .ok()
        .map(|z| z.datetime())
}

/// A local time the clocks skip (spring forward).
fn in_gap(tz: &TimeZone, dt: DateTime) -> bool {
    matches!(
        tz.to_ambiguous_timestamp(dt).offset(),
        AmbiguousOffset::Gap { .. }
    )
}

impl Plan {
    /// Whether a cron pattern has a real (not moved) time less than
    /// `MIN_CRON_GAP_MINS` from instant `t`, other than `t` itself.
    fn cron_runs_near(&self, cron: &croner::Cron, t: i64) -> bool {
        let gap = MIN_CRON_GAP_MINS * 60_000;
        let Some(mut civil) = zoned(&self.tz, t - gap).map(|z| z.datetime()) else {
            return false;
        };
        let mut inclusive = true;
        for _ in 0..200 {
            let Some(next) = cron_after(cron, civil, inclusive) else {
                return false;
            };
            let Some(at) = resolve(&self.tz, next).map(ms) else {
                return false;
            };
            if at > t + gap && !in_gap(&self.tz, next) {
                return false;
            }
            if !in_gap(&self.tz, next) && at != t && (t - at).abs() < gap {
                return true;
            }
            civil = next;
            inclusive = false;
        }
        false
    }

    /// The first run strictly after `after_ms` (UTC milliseconds), or None
    /// when the schedule is over.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        match &self.when {
            When::Once(dt) => resolve(&self.tz, *dt).map(ms).filter(|t| *t > after_ms),
            When::Rule(rule) => {
                let after = zoned(&self.tz, after_ms)?;
                let mut d = after.date();
                if let Some(s) = self.starts
                    && s > d
                {
                    d = s;
                }
                for _ in 0..MAX_SEARCH_DAYS {
                    if self.ends.is_some_and(|e| d > e) {
                        return None;
                    }
                    if rule.matches(d)
                        && let Some(t) = resolve(&self.tz, d.to_datetime(rule.time))
                        && ms(t) > after_ms
                    {
                        return Some(ms(t));
                    }
                    d = d.tomorrow().ok()?;
                }
                None
            }
            When::Cron(cron) => {
                // Match the pattern against local wall-clock times (each
                // happens once), then place them like the other schedules:
                // a time inside a fall-back hour runs the first time only,
                // one in a spring-forward gap runs when the clocks jump.
                let mut civil = zoned(&self.tz, after_ms)?.datetime();
                let mut inclusive = false;
                if let Some(s) = self.starts
                    && s > civil.date()
                {
                    civil = s.to_datetime(Time::midnight());
                    inclusive = true;
                }
                // Times that resolve to or before `after_ms` (the repeated
                // hour, or several times in one gap) are passed over, and a
                // time moved to the jump is dropped when the pattern runs
                // within MIN_CRON_GAP_MINS of it anyway.
                for _ in 0..1000 {
                    let next = cron_after(cron, civil, inclusive)?;
                    if self.ends.is_some_and(|e| next.date() > e) {
                        return None;
                    }
                    let t = ms(resolve(&self.tz, next)?);
                    if t > after_ms && !(in_gap(&self.tz, next) && self.cron_runs_near(cron, t)) {
                        return Some(t);
                    }
                    civil = next;
                    inclusive = false;
                }
                None
            }
        }
    }

    /// Up to `n` upcoming runs after `after_ms`, and at most `remaining`
    /// (what is left of `max_runs`).
    pub fn upcoming(&self, after_ms: i64, n: usize, remaining: Option<i64>) -> Vec<i64> {
        let n = remaining.map_or(n, |r| n.min(r.max(0) as usize));
        let mut out = Vec::with_capacity(n);
        let mut at = after_ms;
        while out.len() < n {
            match self.next_after(at) {
                Some(t) => {
                    out.push(t);
                    at = t;
                }
                None => break,
            }
        }
        out
    }

    /// "Sat Oct 17, 5:30 PM" in the routine's time zone.
    pub fn local_text(&self, at_ms: i64) -> String {
        local_text(&self.tz, at_ms)
    }
}

pub fn local_text(tz: &TimeZone, at_ms: i64) -> String {
    zoned(tz, at_ms)
        .map(|z| z.strftime("%a %b %-d, %-I:%M %p").to_string())
        .unwrap_or_default()
}

/// "5:30 PM".
pub fn time_text(t: Time) -> String {
    let h = t.hour();
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{:02} {ap}", t.minute())
}

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn day_name(n: u8) -> &'static str {
    DAY_NAMES.get(n as usize).copied().unwrap_or("?")
}

fn short_day(n: u8) -> &'static str {
    &day_name(n)[..3]
}

pub fn ordinal(n: i64) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn every_text(every: i64, unit: &str) -> String {
    match every {
        1 => format!("Every {unit}"),
        2 => format!("Every other {unit}"),
        n => format!("Every {n} {unit}s"),
    }
}

/// One line for people: "Every Sat · 5:30 PM", "Monthly on the 1st · 9:00 AM".
pub fn describe_rule(rule: &ParsedRule) -> String {
    let when = match &rule.repeat {
        Repeat::Daily if rule.every == 1 => "Every day".to_string(),
        Repeat::Daily => every_text(rule.every, "day"),
        Repeat::Weekly { days } => {
            let mut days = days.clone();
            days.sort_unstable();
            days.dedup();
            let names = if days.len() == 7 {
                "day".to_string()
            } else if days == [1, 2, 3, 4, 5] {
                "weekday".to_string()
            } else if days.len() == 1 {
                match rule.every {
                    1 => short_day(days[0]).to_string(),
                    _ => day_name(days[0]).to_string(),
                }
            } else {
                days.iter()
                    .map(|d| short_day(*d))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match rule.every {
                1 => format!("Every {names}"),
                2 => format!("Every other week on {names}"),
                n => format!("Every {n} weeks on {names}"),
            }
        }
        Repeat::Monthly { day, nth, weekday } => {
            let on = match (day, nth, weekday) {
                (Some(d), _, _) => format!("the {}", ordinal(*d as i64)),
                (None, Some(-1), Some(w)) => format!("the last {}", day_name(*w)),
                (None, Some(n), Some(w)) => format!("the {} {}", ordinal(*n as i64), day_name(*w)),
                _ => "?".into(),
            };
            match rule.every {
                1 => format!("Monthly on {on}"),
                n => format!("{} on {on}", every_text(n, "month")),
            }
        }
        Repeat::Yearly { month, day } => {
            let m = MONTHS.get(*month as usize - 1).copied().unwrap_or("?");
            match rule.every {
                1 => format!("Every year on {m} {day}"),
                n => format!("{} on {m} {day}", every_text(n, "year")),
            }
        }
    };
    format!("{when} · {}", time_text(rule.time))
}

pub fn describe_once(dt: DateTime) -> String {
    format!(
        "{} · {}",
        dt.date().strftime("%a %b %-d, %Y"),
        time_text(dt.time())
    )
}

/// Check a cron pattern does not run more often than every
/// `MIN_CRON_GAP_MINS` (each run starts an AI engine): over a whole day of
/// its minute and hour fields (any two of its times of day, including the
/// step from the last time of one day to the first of the next), and over
/// its next runs (which also covers daylight-saving jumps).
pub fn check_cron_gap(plan: &Plan, expr: &str, from_ms: i64) -> Result<(), String> {
    let too_often = || {
        format!(
            "That custom schedule runs too often. Routines can run at most every \
             {MIN_CRON_GAP_MINS} minutes."
        )
    };
    let gap = MIN_CRON_GAP_MINS * 60_000;
    let runs = plan.upcoming(from_ms, 12, None);
    if runs.windows(2).any(|w| w[1] - w[0] < gap) {
        return Err(too_often());
    }
    // Around the next daylight-saving changes, where the clock jumps can
    // bring two times closer than they look.
    if let Ok(from) = Timestamp::from_millisecond(from_ms) {
        for tr in plan.tz.following(from).take(4) {
            let at = ms(tr.timestamp());
            if at - from_ms > 400 * 86_400_000 {
                break;
            }
            let day = 86_400_000;
            let runs: Vec<i64> = plan
                .upcoming(at - day, 200, None)
                .into_iter()
                .take_while(|t| *t <= at + day)
                .collect();
            if runs.windows(2).any(|w| w[1] - w[0] < gap) {
                return Err(too_often());
            }
        }
    }
    let mins = day_minutes(expr)?;
    if let (Some(first), Some(last)) = (mins.first(), mins.last())
        && (mins.windows(2).any(|w| w[1] - w[0] < MIN_CRON_GAP_MINS)
            || first + 24 * 60 - last < MIN_CRON_GAP_MINS)
    {
        return Err(too_often());
    }
    Ok(())
}

/// The minutes of the day (0..1440) a cron pattern's minute and hour
/// fields allow, in order. Shorthands (`@daily`) run at most hourly.
fn day_minutes(expr: &str) -> Result<Vec<i64>, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Ok(Vec::new());
    }
    let daily = parse_cron(&format!("{} {} * * *", fields[0], fields[1]))?;
    let tz = TimeZone::UTC;
    let start = DateTime::constant(2024, 1, 1, 0, 0, 0, 0)
        .to_zoned(tz.clone())
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut from = start.clone();
    let mut inclusive = true;
    while let Ok(next) = daily.find_next_occurrence(&from, inclusive) {
        let mins = (next.timestamp().as_second() - start.timestamp().as_second()) / 60;
        if mins >= 24 * 60 {
            break;
        }
        out.push(mins);
        from = next;
        inclusive = false;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn at(tz_name: &str, s: &str) -> i64 {
        resolve(&tz(tz_name), parse_local(s).unwrap())
            .unwrap()
            .as_millisecond()
    }

    fn local(tz_name: &str, ms: i64) -> String {
        zoned(&tz(tz_name), ms)
            .unwrap()
            .strftime("%Y-%m-%d %H:%M %:z")
            .to_string()
    }

    fn rule_plan(tz_name: &str, rule: Rule) -> Plan {
        Plan {
            when: When::Rule(ParsedRule::parse(&rule).unwrap()),
            tz: tz(tz_name),
            starts: None,
            ends: None,
        }
    }

    fn rule(repeat: Repeat, every: u32, time: &str, anchor: Option<&str>) -> Rule {
        Rule {
            repeat,
            every,
            time: time.into(),
            anchor: anchor.map(String::from),
        }
    }

    fn next_n(plan: &Plan, tz_name: &str, from: &str, n: usize) -> Vec<String> {
        plan.upcoming(at(tz_name, from), n, None)
            .into_iter()
            .map(|t| local(tz_name, t))
            .collect()
    }

    const CHI: &str = "America/Chicago";

    #[test]
    fn rule_json_round_trips() {
        let r = rule(
            Repeat::Weekly { days: vec![6] },
            2,
            "17:30",
            Some("2026-10-17"),
        );
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"freq": "weekly", "days": [6], "every": 2, "time": "17:30", "anchor": "2026-10-17"})
        );
        assert_eq!(serde_json::from_value::<Rule>(v).unwrap(), r);
        let m: Rule = serde_json::from_value(
            serde_json::json!({"freq": "monthly", "nth": -1, "weekday": 5, "time": "09:00"}),
        )
        .unwrap();
        assert_eq!(m.every, 1);
        assert!(matches!(
            m.repeat,
            Repeat::Monthly {
                nth: Some(-1),
                weekday: Some(5),
                day: None
            }
        ));
    }

    #[test]
    fn weekly_interval_counts_weeks_from_the_anchor() {
        // Every other week on Sunday and Wednesday, from the week of Sun Oct 11.
        let p = rule_plan(
            CHI,
            rule(
                Repeat::Weekly { days: vec![0, 3] },
                2,
                "10:00",
                Some("2026-10-14"),
            ),
        );
        assert_eq!(
            next_n(&p, CHI, "2026-10-12T00:00", 5),
            [
                "2026-10-14 10:00 -05:00",
                "2026-10-25 10:00 -05:00",
                "2026-10-28 10:00 -05:00",
                "2026-11-08 10:00 -06:00",
                "2026-11-11 10:00 -06:00",
            ]
        );
        // A week before the anchor's week is an "on" week too (counting is
        // symmetric), so editing the anchor never shifts the pattern.
        assert_eq!(
            next_n(&p, CHI, "2026-09-30T12:00", 1),
            ["2026-10-11 10:00 -05:00"]
        );
    }

    #[test]
    fn last_friday_of_the_month() {
        let p = rule_plan(
            CHI,
            rule(
                Repeat::Monthly {
                    day: None,
                    nth: Some(-1),
                    weekday: Some(5),
                },
                1,
                "09:00",
                None,
            ),
        );
        assert_eq!(
            next_n(&p, CHI, "2026-10-01T00:00", 3),
            [
                "2026-10-30 09:00 -05:00",
                "2026-11-27 09:00 -06:00",
                "2026-12-25 09:00 -06:00",
            ]
        );
        let second_tue = rule_plan(
            CHI,
            rule(
                Repeat::Monthly {
                    day: None,
                    nth: Some(2),
                    weekday: Some(2),
                },
                1,
                "09:00",
                None,
            ),
        );
        assert_eq!(
            next_n(&second_tue, CHI, "2026-10-01T00:00", 1),
            ["2026-10-13 09:00 -05:00"]
        );
    }

    #[test]
    fn monthly_day_31_uses_the_last_day_of_short_months() {
        let p = rule_plan(
            CHI,
            rule(
                Repeat::Monthly {
                    day: Some(31),
                    nth: None,
                    weekday: None,
                },
                1,
                "09:00",
                None,
            ),
        );
        assert_eq!(
            next_n(&p, CHI, "2027-01-15T00:00", 4),
            [
                "2027-01-31 09:00 -06:00",
                "2027-02-28 09:00 -06:00",
                "2027-03-31 09:00 -05:00",
                "2027-04-30 09:00 -05:00",
            ]
        );
        // Every third month from January.
        let q = rule_plan(
            CHI,
            rule(
                Repeat::Monthly {
                    day: Some(1),
                    nth: None,
                    weekday: None,
                },
                3,
                "09:00",
                Some("2027-01-01"),
            ),
        );
        assert_eq!(
            next_n(&q, CHI, "2027-01-02T00:00", 2),
            ["2027-04-01 09:00 -05:00", "2027-07-01 09:00 -05:00"]
        );
    }

    #[test]
    fn yearly_leap_day_and_daily_interval() {
        let p = rule_plan(
            CHI,
            rule(Repeat::Yearly { month: 2, day: 29 }, 1, "08:00", None),
        );
        assert_eq!(
            next_n(&p, CHI, "2027-01-01T00:00", 2),
            ["2027-02-28 08:00 -06:00", "2028-02-29 08:00 -06:00"]
        );
        let d = rule_plan(CHI, rule(Repeat::Daily, 3, "07:15", Some("2026-10-13")));
        assert_eq!(
            next_n(&d, CHI, "2026-10-13T08:00", 2),
            ["2026-10-16 07:15 -05:00", "2026-10-19 07:15 -05:00"]
        );
    }

    #[test]
    fn dst_gap_runs_at_the_next_valid_time() {
        // US clocks jump from 2:00 to 3:00 on 2027-03-14.
        let p = rule_plan(CHI, rule(Repeat::Daily, 1, "02:30", None));
        assert_eq!(
            next_n(&p, CHI, "2027-03-13T12:00", 3),
            [
                "2027-03-14 03:00 -05:00",
                "2027-03-15 02:30 -05:00",
                "2027-03-16 02:30 -05:00",
            ]
        );
        // Exactly the start of the gap is also missing.
        let two = rule_plan(CHI, rule(Repeat::Daily, 1, "02:00", None));
        assert_eq!(
            next_n(&two, CHI, "2027-03-13T12:00", 1),
            ["2027-03-14 03:00 -05:00"]
        );
        let once = Plan {
            when: When::Once(parse_local("2027-03-14T02:15").unwrap()),
            tz: tz(CHI),
            starts: None,
            ends: None,
        };
        assert_eq!(
            next_n(&once, CHI, "2027-03-01T00:00", 3),
            ["2027-03-14 03:00 -05:00"]
        );
    }

    #[test]
    fn dst_overlap_runs_only_the_first_time() {
        // US clocks fall back from 2:00 to 1:00 on 2026-11-01.
        let p = rule_plan(CHI, rule(Repeat::Daily, 1, "01:30", None));
        let runs = p.upcoming(at(CHI, "2026-10-31T12:00"), 3, None);
        let shown: Vec<String> = runs.iter().map(|t| local(CHI, *t)).collect();
        assert_eq!(
            shown,
            [
                "2026-11-01 01:30 -05:00",
                "2026-11-02 01:30 -06:00",
                "2026-11-03 01:30 -06:00",
            ]
        );
        // The next run after the first 1:30 is the next day, not the
        // repeated 1:30 an hour later.
        assert!(runs[1] - runs[0] > 24 * 3_600_000);
        let cron = Plan {
            when: When::Cron(Box::new(parse_cron("30 1 * * *").unwrap())),
            tz: tz(CHI),
            starts: None,
            ends: None,
        };
        assert_eq!(cron.upcoming(at(CHI, "2026-10-31T12:00"), 3, None), runs);
        // Several times a day, and every half hour within the repeated hour:
        // each local time still runs once.
        let multi = Plan {
            when: When::Cron(Box::new(parse_cron("0 1,13 * * *").unwrap())),
            ..cron.clone()
        };
        assert_eq!(
            next_n(&multi, CHI, "2026-10-31T12:00", 3),
            [
                "2026-10-31 13:00 -05:00",
                "2026-11-01 01:00 -05:00",
                "2026-11-01 13:00 -06:00",
            ]
        );
        let half = Plan {
            when: When::Cron(Box::new(parse_cron("*/30 1 * * *").unwrap())),
            ..cron.clone()
        };
        assert_eq!(
            next_n(&half, CHI, "2026-11-01T00:00", 3),
            [
                "2026-11-01 01:00 -05:00",
                "2026-11-01 01:30 -05:00",
                "2026-11-02 01:00 -06:00",
            ]
        );
        // Asked from inside the repeated hour: never the second 1:30.
        let second_one = at(CHI, "2026-11-01T01:00") + 3_600_000 + 60_000;
        assert_eq!(
            local(CHI, half.next_after(second_one).unwrap()),
            "2026-11-02 01:00 -06:00"
        );
    }

    #[test]
    fn cron_gap_and_limits() {
        let plan = Plan {
            when: When::Cron(Box::new(parse_cron("30 2 * * *").unwrap())),
            tz: tz(CHI),
            starts: None,
            ends: None,
        };
        assert_eq!(
            next_n(&plan, CHI, "2027-03-13T12:00", 1),
            ["2027-03-14 03:00 -05:00"]
        );
        // With several times a day the missing 2:30 still runs at 3:00.
        let multi = Plan {
            when: When::Cron(Box::new(parse_cron("30 2,14 * * *").unwrap())),
            ..plan.clone()
        };
        assert_eq!(
            next_n(&multi, CHI, "2027-03-13T12:00", 3),
            [
                "2027-03-13 14:30 -06:00",
                "2027-03-14 03:00 -05:00",
                "2027-03-14 14:30 -05:00",
            ]
        );
        // Every half hour, off the hour: the 2:15 the clocks skip is not
        // squeezed in at 3:00, 15 minutes after 1:45 and before 3:15.
        let halves = "15,45 * * * *";
        let half_plan = Plan {
            when: When::Cron(Box::new(parse_cron(halves).unwrap())),
            ..plan.clone()
        };
        assert_eq!(
            next_n(&half_plan, CHI, "2027-03-14T01:30", 3),
            [
                "2027-03-14 01:45 -06:00",
                "2027-03-14 03:15 -05:00",
                "2027-03-14 03:45 -05:00",
            ]
        );
        assert!(check_cron_gap(&half_plan, halves, at(CHI, "2026-10-14T12:00")).is_ok());
        // Exactly 30 minutes away is allowed: the skipped 2:30 runs at 3:00,
        // between 1:30 and 3:30.
        let hourly = Plan {
            when: When::Cron(Box::new(parse_cron("30 * * * *").unwrap())),
            ..plan.clone()
        };
        assert_eq!(
            next_n(&hourly, CHI, "2027-03-14T01:00", 3),
            [
                "2027-03-14 01:30 -06:00",
                "2027-03-14 03:00 -05:00",
                "2027-03-14 03:30 -05:00",
            ]
        );
        // A pattern only crowded by the jump is refused up front: 1:50 and
        // 3:05 are 15 minutes apart that night.
        let jumpy = "5,50 1,3 * * *";
        let jumpy_plan = Plan {
            when: When::Cron(Box::new(parse_cron(jumpy).unwrap())),
            ..plan.clone()
        };
        assert!(check_cron_gap(&jumpy_plan, jumpy, at(CHI, "2026-10-14T12:00")).is_err());
        // Without daylight saving the same pattern is fine.
        let utc_plan = Plan {
            tz: tz("UTC"),
            ..jumpy_plan.clone()
        };
        assert!(check_cron_gap(&utc_plan, jumpy, at("UTC", "2026-10-14T12:00")).is_ok());
        assert!(parse_cron("0 30 17 * * 6").is_err(), "no seconds field");
        assert!(parse_cron("nonsense").is_err());
        let often = Plan {
            when: When::Cron(Box::new(parse_cron("*/5 * * * *").unwrap())),
            ..plan.clone()
        };
        assert!(check_cron_gap(&often, "*/5 * * * *", 0).is_err());
        assert!(check_cron_gap(&plan, "0 3 * * *", 0).is_ok());
        // A short gap late in the day, past the first dozen runs, still
        // counts: 20:40 and 21:00 are 20 minutes apart.
        let late = "0,40 8-20/2,21 * * *";
        let late_plan = Plan {
            when: When::Cron(Box::new(parse_cron(late).unwrap())),
            ..plan.clone()
        };
        assert!(check_cron_gap(&late_plan, late, 0).is_err());
        // And so does the step from one day's last run to the next day's first.
        let wrap = "10,50 0,23 * * *";
        let wrap_plan = Plan {
            when: When::Cron(Box::new(parse_cron(wrap).unwrap())),
            ..plan.clone()
        };
        assert!(check_cron_gap(&wrap_plan, wrap, 0).is_err());
        assert_eq!(day_minutes("15 9,17 * * 1-5").unwrap(), [555, 1035]);
        assert!(day_minutes("@daily").unwrap().is_empty());
    }

    #[test]
    fn end_date_and_start_date_bound_the_runs() {
        let mut p = rule_plan(
            CHI,
            rule(Repeat::Weekly { days: vec![6] }, 1, "17:30", None),
        );
        p.starts = Some(parse_date("start", "2026-10-20").unwrap());
        p.ends = Some(parse_date("end", "2026-10-31").unwrap());
        assert_eq!(
            next_n(&p, CHI, "2026-10-13T00:00", 5),
            ["2026-10-24 17:30 -05:00", "2026-10-31 17:30 -05:00"]
        );
        assert_eq!(p.next_after(at(CHI, "2026-10-31T17:30")), None);
        let mut c = Plan {
            when: When::Cron(Box::new(parse_cron("0 9 * * 1").unwrap())),
            ..p.clone()
        };
        c.ends = Some(parse_date("end", "2026-10-26").unwrap());
        assert_eq!(
            next_n(&c, CHI, "2026-10-13T00:00", 5),
            ["2026-10-26 09:00 -05:00"]
        );
    }

    #[test]
    fn max_runs_limits_upcoming_and_once_runs_once() {
        let p = rule_plan(CHI, rule(Repeat::Daily, 1, "09:00", None));
        assert_eq!(p.upcoming(at(CHI, "2026-10-13T00:00"), 3, Some(2)).len(), 2);
        assert!(p.upcoming(0, 3, Some(0)).is_empty());
        let once = Plan {
            when: When::Once(parse_local("2026-10-29T08:00").unwrap()),
            tz: tz(CHI),
            starts: None,
            ends: None,
        };
        let t = once.next_after(at(CHI, "2026-10-13T00:00")).unwrap();
        assert_eq!(local(CHI, t), "2026-10-29 08:00 -05:00");
        assert_eq!(once.next_after(t), None, "a one-off never repeats");
        assert_eq!(once.local_text(t), "Thu Oct 29, 8:00 AM");
    }

    #[test]
    fn validation_and_descriptions() {
        let bad = [
            rule(Repeat::Weekly { days: vec![] }, 1, "09:00", None),
            rule(Repeat::Weekly { days: vec![7] }, 1, "09:00", None),
            rule(Repeat::Daily, 2, "09:00", None),
            rule(Repeat::Daily, 1, "25:00", None),
            rule(Repeat::Yearly { month: 2, day: 30 }, 1, "09:00", None),
            rule(
                Repeat::Monthly {
                    day: None,
                    nth: Some(5),
                    weekday: Some(1),
                },
                1,
                "09:00",
                None,
            ),
        ];
        for r in bad {
            assert!(ParsedRule::parse(&r).is_err(), "{r:?}");
        }
        let d = |r: Rule| describe_rule(&ParsedRule::parse(&r).unwrap());
        assert_eq!(
            d(rule(Repeat::Weekly { days: vec![6] }, 1, "17:30", None)),
            "Every Sat · 5:30 PM"
        );
        assert_eq!(
            d(rule(
                Repeat::Weekly { days: vec![0] },
                2,
                "10:00",
                Some("2026-10-11")
            )),
            "Every other week on Sunday · 10:00 AM"
        );
        assert_eq!(
            d(rule(
                Repeat::Monthly {
                    day: Some(1),
                    nth: None,
                    weekday: None
                },
                1,
                "09:00",
                None
            )),
            "Monthly on the 1st · 9:00 AM"
        );
        assert_eq!(
            d(rule(
                Repeat::Monthly {
                    day: None,
                    nth: Some(-1),
                    weekday: Some(5)
                },
                1,
                "00:05",
                None
            )),
            "Monthly on the last Friday · 12:05 AM"
        );
        assert_eq!(
            d(rule(
                Repeat::Weekly {
                    days: vec![5, 1, 2, 3, 4]
                },
                1,
                "12:00",
                None
            )),
            "Every weekday · 12:00 PM"
        );
        assert_eq!(
            describe_once(parse_local("2026-10-29 08:00").unwrap()),
            "Thu Oct 29, 2026 · 8:00 AM"
        );
        assert!(parse_tz("Not/AZone").is_err());
        assert!(parse_local("2026-10-29").is_err());
    }
}
