//! Dates: nanoseconds since 1970 <-> year, month, day, hour, minute,
//! second. No time zones: the clock is read as it is.

use alloc::string::String;
use core::fmt::Write;

pub const NS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 of a calendar date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The calendar date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn num(s: &str, digits: usize) -> Option<i64> {
    if s.len() != digits || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `2026-10-06`, `2026-10-06T14:30` or `2026-10-06T14:30:05`, as
/// nanoseconds since 1970.
pub fn parse_date(s: &str) -> Option<i64> {
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y = num(parts.next()?, 4)?;
    let m = num(parts.next()?, 2)?;
    let d = num(parts.next()?, 2)?;
    if parts.next().is_some() || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let (mut hh, mut mm, mut ss) = (0, 0, 0);
    if let Some(t) = time {
        let mut parts = t.split(':');
        hh = num(parts.next()?, 2)?;
        mm = num(parts.next()?, 2)?;
        if let Some(s) = parts.next() {
            ss = num(s, 2)?;
        }
        if parts.next().is_some() || hh > 23 || mm > 59 || ss > 59 {
            return None;
        }
    }
    let secs = days_from_civil(y, m, d) * SECS_PER_DAY + hh * 3600 + mm * 60 + ss;
    secs.checked_mul(NS_PER_SEC)
}

/// `2026-10-06 14:30:05`.
pub fn format_date(ns: i64) -> String {
    let secs = ns.div_euclid(NS_PER_SEC);
    let days = secs.div_euclid(SECS_PER_DAY);
    let rem = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let mut s = String::new();
    let _ = write!(s, "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60);
    s
}

/// `2026-10-06T14:30:05`, for JSON.
pub fn iso_date(ns: i64) -> String {
    let mut s = format_date(ns);
    // SAFETY of the index: format_date always writes "YYYY-MM-DD hh:mm:ss"
    // for years 0..=9999; replace the space only if it's where we expect.
    if s.as_bytes().get(10) == Some(&b' ') {
        s.replace_range(10..11, "T");
    }
    s
}

/// A calendar date and time as nanoseconds since 1970, or `None` if it
/// isn't a real date.
pub fn from_parts(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) || !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..61).contains(&ss) {
        return None;
    }
    (days_from_civil(y, m, d) * SECS_PER_DAY + hh * 3600 + mm * 60 + ss).checked_mul(NS_PER_SEC)
}
