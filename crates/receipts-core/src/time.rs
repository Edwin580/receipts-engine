//! Naive (zone-less) wall-clock timestamps as `i64` microseconds since
//! `1970-01-01T00:00:00`, matching DuckDB `TIMESTAMP`. See ADR 0003.

pub const MICROS_PER_SECOND: i64 = 1_000_000;
pub const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// Days since 1970-01-01 for a proleptic Gregorian date.
/// Howard Hinnant's `days_from_civil`; exact for every `i32` year.
pub const fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)`.
pub const fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as i32;
    (year, month, day)
}

pub const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Parses exactly `YYYY-MM-DDTHH:MM:SS` with an optional `.` and 1–6
/// fractional digits (Socrata `floating_timestamp`). Anything else, including
/// out-of-range fields, leap seconds, zone suffixes, and surrounding
/// whitespace, returns `None`.
pub fn parse_naive_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = digits(&b[0..4])? as i32;
    let month = digits(&b[5..7])?;
    let day = digits(&b[8..10])?;
    let hour = digits(&b[11..13])?;
    let minute = digits(&b[14..16])?;
    let second = digits(&b[17..19])?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let micros = match &b[19..] {
        [] => 0,
        [b'.', frac @ ..] if (1..=6).contains(&frac.len()) => {
            digits(frac)? * 10u32.pow(6 - frac.len() as u32)
        }
        _ => return None,
    };
    let seconds = days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3600
        + i64::from(minute) * 60
        + i64::from(second);
    Some(seconds * MICROS_PER_SECOND + i64::from(micros))
}

fn digits(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

/// Formats as `YYYY-MM-DDTHH:MM:SS.ffffff`. Inverse of
/// [`parse_naive_timestamp`] for years 0..=9999.
pub fn format_naive_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let secs = rem / MICROS_PER_SECOND;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:06}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        rem % MICROS_PER_SECOND
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(parse_naive_timestamp("1970-01-01T00:00:00"), Some(0));
        assert_eq!(
            parse_naive_timestamp("2024-01-01T00:00:00.000"),
            Some(1_704_067_200 * MICROS_PER_SECOND)
        );
        assert_eq!(
            parse_naive_timestamp("1900-01-01T00:00:00"),
            Some(-2_208_988_800 * MICROS_PER_SECOND)
        );
        assert_eq!(parse_naive_timestamp("2024-03-10T02:30:00.5"), {
            // 02:30 does not exist in New York that night; naive time keeps it.
            Some((1_710_037_800) * MICROS_PER_SECOND + 500_000)
        });
    }

    #[test]
    fn fractional_precision() {
        assert_eq!(
            parse_naive_timestamp("1970-01-01T00:00:00.1"),
            Some(100_000)
        );
        assert_eq!(
            parse_naive_timestamp("1970-01-01T00:00:00.123"),
            Some(123_000)
        );
        assert_eq!(
            parse_naive_timestamp("1970-01-01T00:00:00.123456"),
            Some(123_456)
        );
        assert_eq!(parse_naive_timestamp("1970-01-01T00:00:00.1234567"), None);
        assert_eq!(parse_naive_timestamp("1970-01-01T00:00:00."), None);
    }

    #[test]
    fn rejects_malformed_and_out_of_range() {
        for s in [
            "",
            "2024-01-01",
            "2024-01-01 00:00:00",
            "2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00+00:00",
            " 2024-01-01T00:00:00",
            "2024-13-01T00:00:00",
            "2024-00-01T00:00:00",
            "2023-02-29T00:00:00",
            "2024-04-31T00:00:00",
            "2024-01-01T24:00:00",
            "2024-01-01T00:60:00",
            "2024-01-01T00:00:60",
            "2024-01-01T00:00:0a",
            "+024-01-01T00:00:00",
        ] {
            assert_eq!(parse_naive_timestamp(s), None, "{s:?}");
        }
        assert!(parse_naive_timestamp("2024-02-29T00:00:00").is_some());
        assert!(parse_naive_timestamp("2000-02-29T00:00:00").is_some());
        assert!(parse_naive_timestamp("1900-02-29T00:00:00").is_none());
    }

    proptest! {
        #[test]
        fn civil_round_trip(days in -1_000_000i64..1_000_000) {
            let (y, m, d) = civil_from_days(days);
            prop_assert!(d >= 1 && d <= days_in_month(y, m));
            prop_assert_eq!(days_from_civil(y, m, d), days);
        }

        #[test]
        fn format_parse_round_trip(micros in -62_135_596_800_000_000i64..253_402_300_799_999_999) {
            let s = format_naive_timestamp(micros);
            prop_assert_eq!(parse_naive_timestamp(&s), Some(micros));
        }
    }
}
