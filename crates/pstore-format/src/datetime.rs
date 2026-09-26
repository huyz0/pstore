//! RFC 3339 datetimes as microseconds since the Unix epoch, UTC (M9h.3).
//!
//! ⚠️ **One parser**, for a write and for a filter literal alike: a string is a datetime if
//! and only if [`parse`] accepts it. Two parsers would let a literal be a datetime in a
//! filter and refused at the door, or the reverse.
//!
//! Strict on purpose: `YYYY-MM-DDTHH:MM:SS`, then an optional `.` and 1 to 6 digits, then `Z`
//! or `±HH:MM`. Uppercase `T` and `Z`, no space, no leap second, and the year in
//! `0000..=9999` **after** conversion to UTC.

/// The first microsecond of `0000-01-01T00:00:00Z`.
pub const MIN: i64 = -62_167_219_200_000_000;
/// The last microsecond of `9999-12-31T23:59:59.999999Z`.
pub const MAX: i64 = 253_402_300_799_999_999;

const MICROS: i64 = 1_000_000;
const DAY: i64 = 86_400;

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in(y: i64, m: i64) -> i64 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to `y-m-d`, proleptic Gregorian (Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `y-m-d` from days since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `n` ASCII digits at the front of `s` as a number, and the rest.
fn digits(s: &[u8], n: usize) -> Option<(i64, &[u8])> {
    let (head, rest) = (s.get(..n)?, s.get(n..)?);
    let mut v = 0i64;
    for b in head {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + i64::from(b - b'0');
    }
    Some((v, rest))
}

fn byte(s: &[u8], want: u8) -> Option<&[u8]> {
    match s.split_first() {
        Some((b, rest)) if *b == want => Some(rest),
        _ => None,
    }
}

/// Microseconds since the epoch, UTC, or `None` if `s` is not a datetime by this module's
/// rules.
#[must_use]
pub fn parse(s: &str) -> Option<i64> {
    let s = s.as_bytes();
    let (y, s) = digits(s, 4)?;
    let (mo, s) = digits(byte(s, b'-')?, 2)?;
    let (d, s) = digits(byte(s, b'-')?, 2)?;
    let (h, s) = digits(byte(s, b'T')?, 2)?;
    let (mi, s) = digits(byte(s, b':')?, 2)?;
    let (se, mut s) = digits(byte(s, b':')?, 2)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in(y, mo) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut frac = 0i64;
    if let Some(rest) = byte(s, b'.') {
        let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(1..=6).contains(&n) {
            return None;
        }
        let (f, rest) = digits(rest, n)?;
        frac = f * 10_i64.pow(6 - n as u32);
        s = rest;
    }
    let offset = match s.split_first() {
        Some((b'Z', [])) => 0,
        Some((sign @ (b'+' | b'-'), rest)) => {
            let (oh, rest) = digits(rest, 2)?;
            let (om, rest) = digits(byte(rest, b':')?, 2)?;
            if !rest.is_empty() || oh > 23 || om > 59 {
                return None;
            }
            let off = (oh * 60 + om) * 60;
            if *sign == b'-' { -off } else { off }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * DAY + h * 3600 + mi * 60 + se - offset;
    let t = secs * MICROS + frac;
    (MIN..=MAX).contains(&t).then_some(t)
}

/// `t` as `YYYY-MM-DDTHH:MM:SS[.f]Z`: UTC, a four-digit year, and a fraction only when it is
/// non-zero, with trailing zeros removed.
#[must_use]
pub fn format(t: i64) -> String {
    let (secs, frac) = (t.div_euclid(MICROS), t.rem_euclid(MICROS));
    let (days, rem) = (secs.div_euclid(DAY), secs.rem_euclid(DAY));
    let (y, m, d) = civil_from_days(days);
    let mut out = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn the_bounds_are_the_first_and_last_microsecond() {
        assert_eq!(parse("0000-01-01T00:00:00Z"), Some(MIN));
        assert_eq!(parse("9999-12-31T23:59:59.999999Z"), Some(MAX));
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("1969-12-31T23:59:59.999999Z"), Some(-1));
        assert_eq!(format(MIN), "0000-01-01T00:00:00Z");
        assert_eq!(format(MAX), "9999-12-31T23:59:59.999999Z");
        assert_eq!(format(-1), "1969-12-31T23:59:59.999999Z");
    }

    #[test]
    fn every_day_of_four_centuries_round_trips() {
        // 1600 through 2000 covers every leap rule: /4, /100, /400.
        let (mut y, mut m, mut d) = (1600, 1, 1);
        let mut days = days_from_civil(1600, 1, 1);
        while y < 2001 {
            assert_eq!(civil_from_days(days), (y, m, d));
            let s = format!("{y:04}-{m:02}-{d:02}T12:34:56.000100Z");
            let t = parse(&s).unwrap();
            assert_eq!(t, (days * DAY + 45_296) * MICROS + 100);
            assert_eq!(format(t), s.replace(".000100", ".0001"));
            d += 1;
            if d > days_in(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
            days += 1;
        }
    }

    #[test]
    fn offsets_move_the_instant_and_the_range_is_checked_after() {
        assert_eq!(
            parse("2024-05-01T14:30:00+02:00"),
            parse("2024-05-01T12:30:00Z")
        );
        assert_eq!(
            parse("2024-05-01T10:00:00-02:30"),
            parse("2024-05-01T12:30:00Z")
        );
        assert_eq!(
            parse("2024-05-01T12:30:00-00:00"),
            parse("2024-05-01T12:30:00Z")
        );
        assert_eq!(parse("9999-12-31T23:59:59-01:00"), None);
        assert_eq!(parse("0000-01-01T00:00:00+01:00"), None);
        assert!(parse("9999-12-31T23:59:59+01:00").is_some());
        assert!(parse("0000-01-01T00:00:00-01:00").is_some());
    }

    #[test]
    fn fractions_scale_to_microseconds() {
        let base = parse("2024-05-01T12:30:00Z").unwrap();
        assert_eq!(parse("2024-05-01T12:30:00.5Z"), Some(base + 500_000));
        assert_eq!(parse("2024-05-01T12:30:00.000001Z"), Some(base + 1));
        assert_eq!(parse("2024-05-01T12:30:00.123456Z"), Some(base + 123_456));
        assert_eq!(format(base + 500_000), "2024-05-01T12:30:00.5Z");
        assert_eq!(format(base + 120_000), "2024-05-01T12:30:00.12Z");
    }

    #[test]
    fn what_is_not_rfc_3339_by_these_rules_is_refused() {
        for s in [
            "2024-05-01",
            "2024-05-01T12:30:00",
            "2024-05-01T12:30:00.1234567Z",
            "2024-05-01T12:30:00.Z",
            "2024-05-01 12:30:00Z",
            "2024-05-01t12:30:00Z",
            "2024-05-01T12:30:00z",
            "2024-05-01T12:30:60Z",
            "2024-05-01T24:00:00Z",
            "2024-05-01T12:60:00Z",
            "2024-05-01T12:30:00+24:00",
            "2024-05-01T12:30:00+01:60",
            "2024-05-01T12:30:00+0100",
            "2024-05-01T12:30:00+01:00x",
            "2024-05-01T12:30:00Zx",
            "2024-13-01T00:00:00Z",
            "2024-00-01T00:00:00Z",
            "2024-01-00T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "10000-01-01T00:00:00Z",
            "+2024-01-01T00:00:00Z",
            "2024-1-01T00:00:00Z",
            "",
        ] {
            assert_eq!(parse(s), None, "{s}");
        }
        for s in [
            "2024-02-29T00:00:00Z",
            "2000-02-29T00:00:00Z",
            "2024-04-30T23:59:59Z",
            "2024-05-01T12:30:00+23:59",
            "2024-05-01T12:30:00-23:59",
        ] {
            assert!(parse(s).is_some(), "{s}");
        }
        // The offset's upper bounds are inclusive, and move the instant by all of them.
        assert_eq!(
            parse("2024-05-02T00:00:00+23:59"),
            parse("2024-05-01T00:01:00Z")
        );
    }
}
