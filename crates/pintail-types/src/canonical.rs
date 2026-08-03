//! Canonical text forms shared by the executor and the storage format.
//!
//! MySQL-sourced decimals and temporals travel as canonical text; these
//! parsers turn that text into comparable fixed-width integers, and the
//! formatters reproduce the canonical text exactly, so a value can round-trip
//! text → units → text without drift. PTSEG v2 relies on this: it stores the
//! units and regenerates the text.

/// Parses canonical `YYYY-MM-DD` into days since 1970-01-01 (proleptic
/// Gregorian, Howard Hinnant's algorithm). `None` on any deviation.
#[must_use]
pub fn parse_date_days(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digit = |byte: u8| -> Option<i64> { byte.is_ascii_digit().then(|| i64::from(byte - b'0')) };
    let year =
        digit(bytes[0])? * 1000 + digit(bytes[1])? * 100 + digit(bytes[2])? * 10 + digit(bytes[3])?;
    let month = digit(bytes[5])? * 10 + digit(bytes[6])?;
    let day = digit(bytes[8])? * 10 + digit(bytes[9])?;
    if !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    // Reject impossible calendar dates: the day-count arithmetic below would
    // otherwise normalize 2023-02-31 onto the same epoch day as 2023-03-03,
    // letting an invalid literal match a valid stored date.
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > month_days {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let month_shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// Parses canonical `YYYY-MM-DD HH:MM:SS[.ffffff]` into microseconds since
/// the epoch. `None` on any deviation from the canonical shape.
#[must_use]
pub fn parse_datetime_micros(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[10] != b' ' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let days = parse_date_days(&text[..10])?;
    let digit = |byte: u8| -> Option<i64> { byte.is_ascii_digit().then(|| i64::from(byte - b'0')) };
    let hour = digit(bytes[11])? * 10 + digit(bytes[12])?;
    let minute = digit(bytes[14])? * 10 + digit(bytes[15])?;
    let second = digit(bytes[17])? * 10 + digit(bytes[18])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut micros = 0_i64;
    let mut digits = 0_u32;
    if bytes.len() > 19 {
        // A trailing dot with no fractional digits is not canonical.
        if bytes[19] != b'.' || bytes.len() == 20 || bytes.len() > 26 {
            return None;
        }
        for &byte in &bytes[20..] {
            micros = micros * 10 + digit(byte)?;
            digits += 1;
        }
    }
    for _ in digits..6 {
        micros *= 10;
    }
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

/// Parses canonical decimal text (`[+-]digits[.digits]`) into an integer
/// scaled by `10^scale`. `None` on any deviation, overflow, or more fraction
/// digits than the declared scale carries (unless they are zeros).
#[must_use]
pub fn parse_decimal_scaled(text: &str, scale: u8) -> Option<i128> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (negative, rest) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut integer: i128 = 0;
    let mut fraction: i128 = 0;
    let mut fraction_digits: u8 = 0;
    let mut seen_dot = false;
    let mut seen_digit = false;
    for &byte in rest {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                let digit = i128::from(byte - b'0');
                if seen_dot {
                    if fraction_digits < scale {
                        fraction = fraction.checked_mul(10)?.checked_add(digit)?;
                        fraction_digits += 1;
                    } else if digit != 0 {
                        return None;
                    }
                } else {
                    integer = integer.checked_mul(10)?.checked_add(digit)?;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    if !seen_digit {
        return None;
    }
    while fraction_digits < scale {
        fraction = fraction.checked_mul(10)?;
        fraction_digits += 1;
    }
    let magnitude = integer
        .checked_mul(10_i128.checked_pow(u32::from(scale))?)?
        .checked_add(fraction)?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Formats days since 1970-01-01 as canonical `YYYY-MM-DD` (the inverse of
/// [`parse_date_days`]). `None` outside years 0000–9999, the widest range
/// canonical text can carry.
#[must_use]
pub fn format_date_days(days: i64) -> Option<String> {
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Formats microseconds since the epoch as canonical
/// `YYYY-MM-DD HH:MM:SS[.f{fsp}]` with exactly `fsp` fraction digits (the
/// inverse of [`parse_datetime_micros`] for values written at that
/// precision). `None` outside years 0000–9999 or for `fsp > 6`.
#[must_use]
pub fn format_datetime_micros(micros: i64, fsp: u8) -> Option<String> {
    if fsp > 6 {
        return None;
    }
    let seconds = micros.div_euclid(1_000_000);
    let sub_micros = micros.rem_euclid(1_000_000);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    let text = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if fsp > 0 {
        let fraction = sub_micros / 10_i64.pow(6 - u32::from(fsp));
        return Some(format!(
            "{text}.{fraction:0width$}",
            width = usize::from(fsp)
        ));
    }
    if sub_micros != 0 {
        // Sub-second payload in a zero-precision column cannot round-trip.
        return None;
    }
    Some(text)
}

/// Parses decimal text into an integer scaled by `10^scale`, rounding excess
/// fraction digits half away from zero the way `MySQL` rounds a decimal to a
/// narrower scale. Unlike [`parse_decimal_scaled`], which rejects text that
/// does not already fit the scale, this is the coercion used by casts,
/// division, and `AVG`.
#[must_use]
pub fn parse_decimal_rounded(text: &str, scale: u8) -> Option<i128> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (negative, rest) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut magnitude: i128 = 0;
    let mut fraction_digits: u8 = 0;
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut round_up = false;
    for &byte in rest {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                if seen_dot && fraction_digits >= scale {
                    if fraction_digits == scale {
                        round_up = byte >= b'5';
                    }
                    fraction_digits = fraction_digits.checked_add(1)?;
                    continue;
                }
                magnitude = magnitude
                    .checked_mul(10)?
                    .checked_add(i128::from(byte - b'0'))?;
                if seen_dot {
                    fraction_digits += 1;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    if !seen_digit {
        return None;
    }
    while fraction_digits < scale {
        magnitude = magnitude.checked_mul(10)?;
        fraction_digits += 1;
    }
    if round_up {
        magnitude = magnitude.checked_add(1)?;
    }
    Some(if negative { -magnitude } else { magnitude })
}

/// Divides two scaled integers, rounding half away from zero (`MySQL`
/// decimal division). The numerator must already carry the result scale.
#[must_use]
pub fn div_decimal_round_half_up(numerator: i128, denominator: i128) -> Option<i128> {
    if denominator == 0 {
        return None;
    }
    let quotient = numerator.checked_div(denominator)?;
    let remainder = numerator.checked_rem(denominator)?;
    if remainder == 0 {
        return Some(quotient);
    }
    let round = remainder
        .unsigned_abs()
        .checked_mul(2)
        .is_some_and(|doubled| doubled >= denominator.unsigned_abs());
    if !round {
        return Some(quotient);
    }
    let step = if (numerator < 0) == (denominator < 0) {
        1
    } else {
        -1
    };
    quotient.checked_add(step)
}

/// Formats an integer scaled by `10^scale` as canonical fixed-scale decimal
/// text (the inverse of [`parse_decimal_scaled`] for canonical inputs):
/// exactly `scale` fraction digits, no plus sign, no leading zeros beyond
/// one integer digit.
#[must_use]
pub fn format_decimal_scaled(value: i128, scale: u8) -> String {
    let negative = value < 0;
    let magnitude = value.unsigned_abs();
    let divisor = 10_u128.pow(u32::from(scale));
    let integer = magnitude / divisor;
    let fraction = magnitude % divisor;
    let sign = if negative { "-" } else { "" };
    if scale == 0 {
        format!("{sign}{integer}")
    } else {
        format!(
            "{sign}{integer}.{fraction:0width$}",
            width = usize::from(scale)
        )
    }
}

/// Hinnant's `civil_from_days`: the inverse of the day-count arithmetic in
/// [`parse_date_days`]. Public so the executor can extract date parts from
/// packed units without a text round-trip.
#[must_use]
pub const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_text_round_trips_across_the_canonical_range() {
        // 0000-01-01 .. 9999-12-31, stepping a prime so month/era boundaries
        // and leap shapes all appear.
        let first = parse_date_days("0000-01-01").expect("canonical minimum");
        let last = parse_date_days("9999-12-31").expect("canonical maximum");
        let mut days = first;
        while days <= last {
            let text = format_date_days(days).expect("in canonical range");
            assert_eq!(
                parse_date_days(&text),
                Some(days),
                "date round-trip failed at {text}"
            );
            days += 13;
        }
        assert_eq!(format_date_days(first - 1), None);
        assert_eq!(format_date_days(last + 1), None);
    }

    #[test]
    fn known_dates_format_exactly() {
        assert_eq!(format_date_days(0).as_deref(), Some("1970-01-01"));
        assert_eq!(format_date_days(19_782).as_deref(), Some("2024-02-29"));
        assert_eq!(format_date_days(11_016).as_deref(), Some("2000-02-29"));
        assert_eq!(format_date_days(-719_468).as_deref(), Some("0000-03-01"));
    }

    #[test]
    fn datetime_text_round_trips_across_precisions() {
        let cases = [
            ("1970-01-01 00:00:00", 0_u8),
            ("2023-06-15 23:59:59", 0),
            ("2023-06-15 12:34:56.5", 1),
            ("2023-06-15 12:34:56.123", 3),
            ("2023-06-15 12:34:56.123456", 6),
            ("9999-12-31 23:59:59.999999", 6),
            ("0000-01-01 00:00:00", 0),
        ];
        for (text, fsp) in cases {
            let micros = parse_datetime_micros(text).expect("canonical datetime");
            assert_eq!(
                format_datetime_micros(micros, fsp).as_deref(),
                Some(text),
                "datetime round-trip failed at {text}"
            );
        }
    }

    #[test]
    fn datetime_formatting_rejects_unrepresentable_values() {
        let with_fraction = parse_datetime_micros("2023-06-15 12:34:56.000001").expect("parses");
        // A zero-precision column cannot carry sub-second payload.
        assert_eq!(format_datetime_micros(with_fraction, 0), None);
        assert_eq!(format_datetime_micros(0, 7), None);
        let past_range = parse_datetime_micros("9999-12-31 23:59:59").expect("parses") + 1_000_000;
        assert_eq!(format_datetime_micros(past_range, 0), None);
    }

    #[test]
    fn decimal_text_round_trips() {
        let cases = [
            ("0.00", 2_u8),
            ("123.45", 2),
            ("-123.45", 2),
            ("0.05", 2),
            ("-0.05", 2),
            ("42", 0),
            ("-42", 0),
            ("999999999999999999.999999", 6),
            ("-999999999999999999.999999", 6),
        ];
        for (text, scale) in cases {
            let scaled = parse_decimal_scaled(text, scale).expect("canonical decimal");
            assert_eq!(
                format_decimal_scaled(scaled, scale),
                text,
                "decimal round-trip failed at {text}"
            );
        }
    }

    #[test]
    fn scaled_integers_format_exactly() {
        assert_eq!(format_decimal_scaled(0, 2), "0.00");
        assert_eq!(format_decimal_scaled(-5, 2), "-0.05");
        assert_eq!(format_decimal_scaled(12_345, 2), "123.45");
        assert_eq!(format_decimal_scaled(7, 0), "7");
        assert_eq!(format_decimal_scaled(-7, 0), "-7");
    }
}
