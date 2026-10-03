//! Five-field cron expressions (`minute hour day-of-month month
//! day-of-week`) evaluated in the machine's local time.
//!
//! Supported: `*`, numbers, lists (`1,15`), ranges (`1-5`), steps (`*/10`,
//! `0-30/5`), month and weekday names (`jan`, `mon`), `7` for Sunday and
//! the shorthands `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`.
//! As in cron, when both day fields are restricted a day matches if
//! either does; a day field starting with `*` (`*/2`) counts as
//! unrestricted for that rule, like Vixie cron.
//!
//! Times are matched on the local wall clock, minute by minute of the
//! calendar, so days of 23 or 25 hours (daylight saving changes) neither
//! skip nor repeat runs: a wall-clock minute that happens twice (fall
//! back) runs once, at its first occurrence after the previous run; one
//! that does not exist (spring forward) runs at the same offset after the
//! change (02:30 → 03:30), as `mktime` normalises it.

use blongo_protocol::Timestamp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + min);
        }
        s.parse::<u32>()
            .map_err(|_| format!("`{s}` is not a number"))
    };
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("bad step in `{part}`"))?,
            ),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // `5/10` means from 5 to the end, every 10.
            (v, if part.contains('/') { max } else { v })
        };
        // Sunday may be written 7.
        let (lo, hi) = if max == 6 {
            (lo.min(7), hi.min(7))
        } else {
            (lo, hi)
        };
        let top = if max == 6 { 7 } else { max };
        if lo < min || hi > top || lo > hi {
            return Err(format!("`{part}` is out of range {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << (v % if max == 6 { 7 } else { 64 });
            v += step;
        }
    }
    Ok((bits, !text.starts_with('*')))
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let expanded = match text {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = parts[..] else {
            return Err("a schedule has five fields: minute hour day month weekday".into());
        };
        let (minutes, _) = field(minute, 0, 59, &[])?;
        let (hours, _) = field(hour, 0, 23, &[])?;
        let (days, days_restricted) = field(dom, 1, 31, &[])?;
        let (months, _) = field(month, 1, 12, &MONTHS)?;
        let (weekdays, weekdays_restricted) = field(dow, 0, 6, &WEEKDAYS)?;
        Ok(Self {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_restricted,
            weekdays_restricted,
        })
    }

    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let dom = self.days & (1 << day) != 0;
        let dow = self.weekdays & (1 << weekday) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// The first matching minute strictly after `after` (local time), or
    /// `None` if nothing matches within about four years (e.g. Feb 30).
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        let after_secs = after.0.div_euclid(1000);
        // The wall-clock minute after `after`'s; the walk below moves on
        // the calendar, never by fixed numbers of seconds.
        let mut w = Wall::of(&local(after_secs)).next_minute();
        let limit = w.days() + 4 * 366;
        while w.days() < limit {
            if self.months & (1 << w.month) == 0 {
                w = w.next_month();
                continue;
            }
            if !self.day_matches(w.day, w.weekday()) {
                w = w.next_day();
                continue;
            }
            if self.hours & (1 << w.hour) == 0 {
                w = w.next_hour();
                continue;
            }
            if self.minutes & (1 << w.minute) == 0 {
                w = w.next_minute();
                continue;
            }
            if let Some(secs) = instant(&w, after_secs) {
                return Some(Timestamp(secs * 1000));
            }
            w = w.next_minute();
        }
        None
    }

    /// The next run after `now` for a schedule that last ran at
    /// `last_run`: when the clock was set back since (fall back, or a
    /// restart inside the repeated hour), wall-clock minutes up to the
    /// last run's do not run again.
    pub fn next_run(&self, now: Timestamp, last_run: Option<Timestamp>) -> Option<Timestamp> {
        let mut next = self.next_after(now)?;
        if let Some(last) = last_run {
            let last_wall = Wall::of(&local(last.0.div_euclid(1000)));
            // A clock change moves the wall clock back by at most a few
            // hours; past that the wall clock is ahead again anyway.
            while next.0 - last.0 < 4 * 3600 * 1000
                && Wall::of(&local(next.0.div_euclid(1000))) <= last_wall
            {
                next = self.next_after(next)?;
            }
        }
        Some(next)
    }

    /// The shortest time between two of the next `samples` runs from
    /// `from`, in milliseconds (`None` with fewer than two runs).
    pub fn min_gap(&self, from: Timestamp, samples: usize) -> Option<i64> {
        let mut prev = self.next_after(from)?;
        let mut gap: Option<i64> = None;
        for _ in 1..samples {
            let Some(next) = self.next_after(prev) else {
                break;
            };
            let d = next.0 - prev.0;
            gap = Some(gap.map_or(d, |g| g.min(d)));
            prev = next;
        }
        gap
    }
}

/// A wall-clock minute on the proleptic Gregorian calendar (ordered by
/// time: the fields are in significance order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Wall {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
}

impl Wall {
    fn of(t: &Local) -> Self {
        Self {
            year: t.year,
            month: t.month,
            day: t.day,
            hour: t.hour,
            minute: t.minute,
        }
    }

    /// Days since 1970-01-01 (Howard Hinnant's days-from-civil).
    fn days(&self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// 0 = Sunday.
    fn weekday(&self) -> u32 {
        (self.days() + 4).rem_euclid(7) as u32
    }

    fn next_month(self) -> Self {
        let (year, month) = if self.month == 12 {
            (self.year + 1, 1)
        } else {
            (self.year, self.month + 1)
        };
        Self {
            year,
            month,
            day: 1,
            hour: 0,
            minute: 0,
        }
    }

    fn next_day(self) -> Self {
        if self.day >= days_in_month(self.year, self.month) {
            self.next_month()
        } else {
            Self {
                day: self.day + 1,
                hour: 0,
                minute: 0,
                ..self
            }
        }
    }

    fn next_hour(self) -> Self {
        if self.hour == 23 {
            self.next_day()
        } else {
            Self {
                hour: self.hour + 1,
                minute: 0,
                ..self
            }
        }
    }

    fn next_minute(self) -> Self {
        if self.minute == 59 {
            self.next_hour()
        } else {
            Self {
                minute: self.minute + 1,
                ..self
            }
        }
    }
}

/// The first instant after `after` (seconds) showing the wall-clock
/// minute `w`: of a repeated minute, the occurrence after `after`; a
/// minute skipped by a clock change maps to `mktime`'s normalisation.
#[cfg(unix)]
fn instant(w: &Wall, after: i64) -> Option<i64> {
    let mut best: Option<i64> = None;
    for isdst in [-1, 0, 1] {
        // SAFETY: mktime reads and normalises the `tm` we own.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = w.year - 1900;
        tm.tm_mon = w.month as i32 - 1;
        tm.tm_mday = w.day as i32;
        tm.tm_hour = w.hour as i32;
        tm.tm_min = w.minute as i32;
        tm.tm_isdst = isdst;
        let t = unsafe { libc::mktime(&mut tm) };
        if t == -1 {
            continue;
        }
        let t = t as i64;
        // With an explicit DST flag, only an instant that really shows
        // this wall time counts (a forced flag would shift it otherwise).
        if isdst != -1 && Wall::of(&local(t)) != *w {
            continue;
        }
        if t > after && best.is_none_or(|b| t < b) {
            best = Some(t);
        }
    }
    best
}

#[cfg(not(unix))]
fn instant(w: &Wall, after: i64) -> Option<i64> {
    let t = w.days() * 86_400 + i64::from(w.hour) * 3600 + i64::from(w.minute) * 60;
    (t > after).then_some(t)
}

/// Broken-down local time.
struct Local {
    year: i32,
    /// 1-12
    month: u32,
    /// 1-31
    day: u32,
    hour: u32,
    minute: u32,
}

/// `YYYY-MM-DD HH:MM` in the machine's local time (UTC where local time
/// is not available), for showing schedule times.
pub fn format_local(ms: i64) -> String {
    let t = local(ms.div_euclid(1000));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

#[cfg(unix)]
fn local(secs: i64) -> Local {
    // SAFETY: localtime_r only writes the `tm` we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return utc(secs);
    }
    Local {
        year: tm.tm_year + 1900,
        month: tm.tm_mon as u32 + 1,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    }
}

/// Other platforms: UTC (documented gap).
#[cfg(not(unix))]
fn local(secs: i64) -> Local {
    utc(secs)
}

fn utc(secs: i64) -> Local {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    Local {
        year: (y + i64::from(m <= 2)) as i32,
        month: m as u32,
        day: d as u32,
        hour: (rem / 3600) as u32,
        minute: (rem % 3600 / 60) as u32,
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp(secs * 1000)
    }

    #[test]
    fn parses_fields_and_refuses_nonsense() {
        assert!(Cron::parse("* * * * *").is_ok());
        assert!(Cron::parse("*/15 9-17 * * mon-fri").is_ok());
        assert!(Cron::parse("0 0 1,15 jan,jul 7").is_ok());
        assert!(Cron::parse("@daily").is_ok());
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "a * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn utc_conversion_matches_known_dates() {
        // 2026-10-03 12:34 UTC is a Saturday.
        let t = utc(1_791_030_840);
        assert_eq!(
            (
                t.year,
                t.month,
                t.day,
                t.hour,
                t.minute,
                Wall::of(&t).weekday()
            ),
            (2026, 10, 3, 12, 34, 6)
        );
        let t = utc(951_782_400); // 2000-02-29
        assert_eq!((t.year, t.month, t.day), (2000, 2, 29));
    }

    #[test]
    fn next_times_in_local_time() {
        // Whatever the zone, "every minute" is the next whole minute.
        let c = Cron::parse("* * * * *").unwrap();
        assert_eq!(c.next_after(at(600)), Some(at(660)));
        assert_eq!(c.next_after(Timestamp(600_500)), Some(at(660)));
        // The next match always has the asked-for local fields.
        let now = Timestamp::now();
        for expr in [
            "30 9 * * *",
            "0 */6 * * *",
            "15 10 * * mon",
            "0 0 1 * *",
            "0 12 29 2 *",
        ] {
            let c = Cron::parse(expr).unwrap();
            let next = c.next_after(now).unwrap();
            assert!(next > now);
            let t = local(next.0 / 1000);
            let fields: Vec<&str> = expr.split_whitespace().collect();
            if let Ok(m) = fields[0].parse::<u32>() {
                assert_eq!(t.minute, m, "{expr}");
            }
            if let Ok(h) = fields[1].parse::<u32>() {
                assert_eq!(t.hour, h, "{expr}");
            }
            if fields[4] == "mon" {
                assert_eq!(Wall::of(&t).weekday(), 1);
            }
            if fields[2] == "29" {
                assert_eq!((t.month, t.day), (2, 29));
            }
            // And nothing earlier matches: the minute before is not one.
            let earlier = c.next_after(Timestamp(next.0 - 120_000)).unwrap();
            assert_eq!(earlier, next, "{expr}");
        }
        assert!(Cron::parse("0 0 30 2 *").unwrap().next_after(now).is_none());
    }

    #[test]
    fn day_fields_starting_with_a_star_are_unrestricted() {
        // `*/2` in day-of-month does not turn the weekday into an "or".
        let c = Cron::parse("0 0 */2 * mon").unwrap();
        assert!(!c.days_restricted && c.weekdays_restricted);
        assert!(c.day_matches(3, 1));
        assert!(!c.day_matches(3, 2));
        let c = Cron::parse("0 0 1 * mon").unwrap();
        assert!(c.day_matches(1, 3) && c.day_matches(2, 1));
    }

    #[test]
    fn min_gap_finds_the_closest_runs() {
        let now = Timestamp::now();
        let gap = |e: &str| Cron::parse(e).unwrap().min_gap(now, 500).unwrap();
        assert_eq!(gap("* * * * *"), 60_000);
        assert_eq!(gap("*/15 * * * *"), 900_000);
        // Every minute, but only during one hour a day.
        assert_eq!(gap("* 3 * * *"), 60_000);
        assert!(gap("0 9 * * *") >= 23 * 3600 * 1000);
    }

    /// Daylight saving changes, in a child process with `TZ` set (the
    /// zone is process-wide, so it is not changed under other tests).
    /// Unix only: elsewhere schedules run in UTC (no local time zones).
    #[cfg(unix)]
    #[test]
    fn daylight_saving_changes_neither_skip_nor_repeat() {
        if std::env::var_os("BLONGO_CRON_DST_CHILD").is_some() {
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cron::tests::dst_in_new_york",
                "--ignored",
                "--nocapture",
            ])
            .env("TZ", "America/New_York")
            .env("BLONGO_CRON_DST_CHILD", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "run by daylight_saving_changes_neither_skip_nor_repeat with TZ set"]
    fn dst_in_new_york() {
        assert!(std::env::var_os("BLONGO_CRON_DST_CHILD").is_some());
        // SAFETY: this process is the child, run for this test alone.
        unsafe extern "C" {
            fn tzset();
        }
        unsafe { tzset() };
        assert_eq!(format_local(1_772_946_030_000), "2026-03-08 00:00");
        let next = |e: &str, from: i64| {
            Cron::parse(e)
                .unwrap()
                .next_after(at(from))
                .map(|t| t.0 / 1000)
        };
        // Spring forward (Sunday 2026-03-08 has 23 hours): Monday's
        // midnight run is not skipped.
        assert_eq!(next("0 0 * * 1", 1_772_946_030), Some(1_773_028_800));
        // 02:30 does not exist that night: it runs at 03:30 (EDT).
        assert_eq!(next("30 2 * * *", 1_772_949_600), Some(1_772_955_000));
        // Hourly: 01:00 EST, then 03:00 EDT.
        assert_eq!(next("0 * * * *", 1_772_949_600), Some(1_772_953_200));
        // Fall back (2026-11-01, 01:00-01:59 happens twice): 01:30 runs
        // once, at its first occurrence, and next on the following day.
        let first = next("30 1 * * *", 1_793_505_600).unwrap();
        assert_eq!(first, 1_793_511_000);
        assert_eq!(next("30 1 * * *", first), Some(1_793_601_000));
        // Every minute: after 01:59 EDT comes 02:00 EST (the repeated hour
        // does not run again).
        assert_eq!(next("* * * * *", 1_793_512_740), Some(1_793_516_400));
        // A restart inside the repeated hour does not rerun minutes that
        // already ran: last run 01:30 EDT, now 01:10 EST.
        let c = Cron::parse("30 1 * * *").unwrap();
        assert_eq!(
            c.next_run(at(1_793_513_400), Some(at(first)))
                .map(|t| t.0 / 1000),
            Some(1_793_601_000)
        );
        // Without a previous run it is the 01:30 EST occurrence.
        assert_eq!(
            c.next_run(at(1_793_513_400), None).map(|t| t.0 / 1000),
            Some(1_793_514_600)
        );
    }
}
