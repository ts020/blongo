//! Five-field cron expressions (`minute hour day-of-month month
//! day-of-week`) evaluated in the machine's local time.
//!
//! Supported: `*`, numbers, lists (`1,15`), ranges (`1-5`), steps (`*/10`,
//! `0-30/5`), month and weekday names (`jan`, `mon`), `7` for Sunday and
//! the shorthands `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`.
//! As in cron, when both day fields are restricted a day matches if
//! either does.

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
    Ok((bits, text != "*"))
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

    fn day_matches(&self, t: &Local) -> bool {
        let dom = self.days & (1 << t.day) != 0;
        let dow = self.weekdays & (1 << t.weekday) != 0;
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
        // Start at the next whole minute.
        let mut secs = after.0.div_euclid(1000) / 60 * 60 + 60;
        let limit = secs + 4 * 366 * 86_400;
        while secs < limit {
            let t = local(secs);
            if self.months & (1 << t.month) == 0 {
                // First minute of the next month.
                secs += (i64::from(days_in_month(t.year, t.month) - t.day) * 1440
                    + i64::from(23 - t.hour) * 60
                    + i64::from(60 - t.minute))
                    * 60;
                continue;
            }
            if !self.day_matches(&t) {
                secs += (i64::from(23 - t.hour) * 60 + i64::from(60 - t.minute)) * 60;
                continue;
            }
            if self.hours & (1 << t.hour) == 0 {
                secs += i64::from(60 - t.minute) * 60;
                continue;
            }
            if self.minutes & (1 << t.minute) == 0 {
                secs += 60;
                continue;
            }
            return Some(Timestamp(secs * 1000));
        }
        None
    }
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
    /// 0 = Sunday
    weekday: u32,
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
        weekday: tm.tm_wday as u32,
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
        weekday: (days + 4).rem_euclid(7) as u32,
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
            (t.year, t.month, t.day, t.hour, t.minute, t.weekday),
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
                assert_eq!(t.weekday, 1);
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
}
