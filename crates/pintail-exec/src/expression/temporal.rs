//! `MySQL` temporal semantics: datetime parsing, date-part extraction,
//! week and yearweek modes, interval arithmetic, time-zone conversion
//! and `DATE_FORMAT`.

use chrono::{
    Datelike, Duration, FixedOffset, LocalResult, Months, NaiveDate, NaiveDateTime, TimeZone,
    Timelike, Utc,
};
use pintail_sql::{DatePart, IntervalUnit};
use pintail_types::Value;

use super::scalar_string;
use crate::ExecError;

pub(super) fn parse_mysql_datetime(value: &str) -> Result<NaiveDateTime, ExecError> {
    // Digits alone are a packed date or date and time: YYMMDD, YYYYMMDD,
    // YYMMDDHHMMSS or YYYYMMDDHHMMSS.
    let trimmed = value.trim();
    if matches!(trimmed.len(), 6 | 8 | 12 | 14) && trimmed.bytes().all(|byte| byte.is_ascii_digit())
    {
        let number: i128 = trimmed.parse().map_err(|_| ExecError::InvalidDateTime)?;
        let number = if trimmed.len() == 12 && number < 700_101_000_000 {
            number + 20_000_000_000_000
        } else if trimmed.len() == 12 {
            number + 19_000_000_000_000
        } else {
            number
        };
        return numeric_datetime(number).ok_or(ExecError::InvalidDateTime);
    }
    // Any punctuation may separate the date's parts: 2006.1.1 and 98/02/03
    // are dates.
    if let Some(rewritten) = dashed_date(value) {
        return parse_mysql_datetime(&rewritten);
    }
    // A year written with one or two digits names the nearest century the
    // way MySQL reads it: 00-69 are 2000-2069 and 70-99 are 1970-1999.
    let year_digits = value.find('-').unwrap_or(0);
    if (1..=2).contains(&year_digits)
        && value[..year_digits]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        let year: u32 = value[..year_digits]
            .parse()
            .map_err(|_| ExecError::InvalidDateTime)?;
        let century = if year < 70 { 2000 } else { 1900 };
        return parse_mysql_datetime(&format!("{}{}", century + year, &value[year_digits..]));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(value);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or(ExecError::InvalidDateTime)
}

pub(super) fn date_part(value: NaiveDateTime, part: DatePart) -> u64 {
    match part {
        DatePart::Year => u64::try_from(value.year()).unwrap_or(0),
        DatePart::Month => u64::from(value.month()),
        DatePart::Day => u64::from(value.day()),
        DatePart::Hour => u64::from(value.hour()),
        DatePart::Minute => u64::from(value.minute()),
        DatePart::Second => u64::from(value.second()),
        DatePart::Quarter => u64::from((value.month() - 1) / 3 + 1),
        // MySQL DAYOFWEEK: 1 = Sunday .. 7 = Saturday.
        DatePart::DayOfWeek => u64::from(value.weekday().num_days_from_sunday() + 1),
        // MySQL WEEKDAY: 0 = Monday .. 6 = Sunday.
        DatePart::WeekDay => u64::from(value.weekday().num_days_from_monday()),
        DatePart::DayOfYear => u64::from(value.ordinal()),
        DatePart::Week => mysql_week_mode0(value.date()),
        DatePart::IsoWeek => u64::from(value.date().iso_week().week()),
        DatePart::WeekMode(mode) => u64::from(mysql_calc_week(value.date(), u32::from(mode)).1),
    }
}

/// `MySQL` `WEEK` default mode 0: Sunday-start weeks numbered 0-53. Week 1
/// begins on the year's first Sunday; days before it are week 0, and a
/// year that starts on Sunday starts in week 1.
fn mysql_week_mode0(date: chrono::NaiveDate) -> u64 {
    let january_first = date.with_ordinal(1).expect("ordinal 1 is valid");
    let offset = u64::from(january_first.weekday().num_days_from_sunday());
    let week = (u64::from(date.ordinal()) - 1 + offset) / 7;
    if offset == 0 { week + 1 } else { week }
}

/// `MySQL` `YEARWEEK` default mode 0: `year * 100 + week`, where dates in
/// week 0 report the final week of the previous year instead.
pub(super) fn mysql_yearweek(date: chrono::NaiveDate) -> u64 {
    let week = mysql_week_mode0(date);
    if week > 0 {
        return u64::try_from(date.year()).unwrap_or(0) * 100 + week;
    }
    let previous_december =
        chrono::NaiveDate::from_ymd_opt(date.year() - 1, 12, 31).expect("december 31 is valid");
    let january_first = previous_december
        .with_ordinal(1)
        .expect("ordinal 1 is valid");
    let offset = u64::from(january_first.weekday().num_days_from_sunday());
    // Count the date as a continuation of the previous year's weeks.
    let days = u64::from(previous_december.ordinal()) + u64::from(date.ordinal()) - 1;
    let week = (days + offset) / 7 + u64::from(offset == 0);
    u64::try_from(date.year() - 1).unwrap_or(0) * 100 + week
}

/// Days between year 0 and the Unix epoch in `MySQL`'s `TO_DAYS` calendar.
pub(super) const TO_DAYS_EPOCH_OFFSET: i64 = 719_528;

/// `MySQL` `TIMESTAMPDIFF`: complete units from `from` to `to`, truncated
/// toward zero (negative when `to` precedes `from`). `Chrono`'s duration
/// accessors already truncate toward zero for the clock units.
pub(super) fn timestamp_diff(from: NaiveDateTime, to: NaiveDateTime, unit: IntervalUnit) -> i64 {
    let elapsed = to.signed_duration_since(from);
    match unit {
        IntervalUnit::Second => elapsed.num_seconds(),
        IntervalUnit::Minute => elapsed.num_minutes(),
        IntervalUnit::Hour => elapsed.num_hours(),
        IntervalUnit::Day => elapsed.num_days(),
        IntervalUnit::Month => complete_months(from, to),
        IntervalUnit::Year => complete_months(from, to) / 12,
    }
}

/// Calendar months fully elapsed between two datetimes: the month delta,
/// minus one when the later day-of-month/time has not yet reached the
/// earlier one (`MySQL`'s boundary rule, e.g. Jan 31 -> Feb 29 is 0 months).
fn complete_months(from: NaiveDateTime, to: NaiveDateTime) -> i64 {
    let (early, late, sign) = if to >= from {
        (from, to, 1)
    } else {
        (to, from, -1)
    };
    let mut months = i64::from(late.year() - early.year()) * 12 + i64::from(late.month())
        - i64::from(early.month());
    if (late.day(), late.time()) < (early.day(), early.time()) {
        months -= 1;
    }
    sign * months
}

pub(super) fn apply_interval(
    value: NaiveDateTime,
    amount: i64,
    unit: IntervalUnit,
    subtract: bool,
) -> Result<NaiveDateTime, ExecError> {
    let amount = if subtract {
        amount.checked_neg().ok_or(ExecError::NumericOverflow)?
    } else {
        amount
    };
    match unit {
        IntervalUnit::Year | IntervalUnit::Month => {
            let months = if unit == IntervalUnit::Year {
                amount.checked_mul(12).ok_or(ExecError::NumericOverflow)?
            } else {
                amount
            };
            let magnitude =
                u32::try_from(months.unsigned_abs()).map_err(|_| ExecError::NumericOverflow)?;
            if months < 0 {
                value.checked_sub_months(Months::new(magnitude))
            } else {
                value.checked_add_months(Months::new(magnitude))
            }
        }
        IntervalUnit::Day => {
            Duration::try_days(amount).and_then(|delta| value.checked_add_signed(delta))
        }
        IntervalUnit::Hour => {
            Duration::try_hours(amount).and_then(|delta| value.checked_add_signed(delta))
        }
        IntervalUnit::Minute => {
            Duration::try_minutes(amount).and_then(|delta| value.checked_add_signed(delta))
        }
        IntervalUnit::Second => {
            Duration::try_seconds(amount).and_then(|delta| value.checked_add_signed(delta))
        }
    }
    // A result past the DATETIME range is NULL, as in MySQL, not a date with
    // a five-digit year.
    .filter(|shifted| (0..=9999).contains(&shifted.year()))
    .ok_or(ExecError::InvalidDateTime)
}

/// Applies one simple `MySQL` interval to a canonical temporal scalar. Window
/// `RANGE` bounds use the same calendar arithmetic as `DATE_ADD`/`DATE_SUB`.
pub(crate) fn shift_temporal_value(
    value: &Value,
    amount: u64,
    unit: IntervalUnit,
    add: bool,
) -> Result<Value, ExecError> {
    let input = scalar_string(value)?;
    let datetime = parse_mysql_datetime(&input)?;
    let amount = i64::try_from(amount).map_err(|_| ExecError::NumericOverflow)?;
    let shifted = apply_interval(datetime, amount, unit, !add)?;
    let date_only = input.len() <= 10
        && matches!(
            unit,
            IntervalUnit::Year | IntervalUnit::Month | IntervalUnit::Day
        );
    Ok(Value::Utf8(
        shifted
            .format(if date_only {
                "%Y-%m-%d"
            } else {
                "%Y-%m-%d %H:%M:%S"
            })
            .to_string(),
    ))
}

/// A `CONVERT_TZ` zone argument: numeric offset or IANA name.
enum ZoneSpec {
    Fixed(FixedOffset),
    Named(chrono_tz::Tz),
}

fn timezone_spec(text: &str) -> Option<ZoneSpec> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_prefix('-'))
    {
        let (hours, minutes) = rest.split_once(':')?;
        let hours: i32 = hours.parse().ok()?;
        let minutes: i32 = minutes.parse().ok()?;
        if !(0..=59).contains(&minutes) {
            return None;
        }
        let mut seconds = (hours * 60 + minutes) * 60;
        if trimmed.starts_with('-') {
            seconds = -seconds;
        }
        // MySQL accepts offsets in [-13:59, +14:00].
        if !((-14 * 3600 + 60)..=(14 * 3600)).contains(&seconds) {
            return None;
        }
        return FixedOffset::east_opt(seconds).map(ZoneSpec::Fixed);
    }
    chrono_tz::Tz::from_str_insensitive(trimmed)
        .ok()
        .map(ZoneSpec::Named)
}

/// `CONVERT_TZ` on the canonical datetime text carrier. Ambiguous local
/// times (DST fall-back) take the earlier offset like `MySQL`; nonexistent
/// local times (spring-forward gap) return None, a documented divergence.
pub(super) fn convert_tz(text: &str, from: &str, to: &str) -> Option<String> {
    let trimmed = text.trim();
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| {
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        })
        .ok()?;
    let fraction_digits = trimmed
        .rsplit_once('.')
        .map_or(0, |(_, fraction)| fraction.len().min(6));
    let from = timezone_spec(from)?;
    let to = timezone_spec(to)?;
    let utc = match from {
        ZoneSpec::Fixed(offset) => match offset.from_local_datetime(&naive) {
            LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                value.with_timezone(&Utc)
            }
            LocalResult::None => return None,
        },
        ZoneSpec::Named(zone) => match zone.from_local_datetime(&naive) {
            LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                value.with_timezone(&Utc)
            }
            LocalResult::None => return None,
        },
    };
    let converted = match to {
        ZoneSpec::Fixed(offset) => utc.with_timezone(&offset).naive_local(),
        ZoneSpec::Named(zone) => utc.with_timezone(&zone).naive_local(),
    };
    let base = converted.format("%Y-%m-%d %H:%M:%S").to_string();
    if fraction_digits == 0 {
        return Some(base);
    }
    let micros = format!("{:06}", converted.and_utc().timestamp_subsec_micros());
    Some(format!("{base}.{}", &micros[..fraction_digits]))
}

/// Translates a `MySQL` format string into a chrono *parse* format for
/// `STR_TO_DATE`.
///
/// This is the direction `DATE_FORMAT` used to share, and it carries the same
/// defect: directives outside the mapped set are forwarded to chrono, whose
/// dialect assigns several of the same letters different meanings, so an
/// unmapped directive parses against the wrong field instead of erroring.
/// Emitting output could be fixed by rendering each directive directly;
/// parsing cannot borrow that fix, because it needs a real parser rather than
/// a renderer. Tracked separately — see the `STR_TO_DATE` note in
/// `docs/limitations.md`.
pub(super) fn chrono_parse_format(value: &str) -> Option<String> {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            output.push('%');
            return None;
        };
        output.push_str(match specifier {
            'c' => "%-m",
            'e' => "%-d",
            'M' => "%B",
            'k' => "%-H",
            'l' => "%-I",
            'i' => "%M",
            's' => "%S",
            'f' => "%6f",
            'Y' => "%Y",
            'y' => "%y",
            'm' => "%m",
            'd' => "%d",
            'H' => "%H",
            'h' | 'I' => "%I",
            'p' => "%p",
            'b' => "%b",
            'W' => "%A",
            'a' => "%a",
            'j' => "%j",
            'r' => "%I:%M:%S %p",
            'T' => "%H:%M:%S",
            '%' => "%%",
            _ => return None,
        });
    }
    Some(output)
}

const WEEK_MONDAY_FIRST: u32 = 1;
const WEEK_YEAR: u32 = 2;
const WEEK_FIRST_WEEKDAY: u32 = 4;

/// `MySQL`'s mode-to-flag mapping: a mode without `WEEK_MONDAY_FIRST` flips
/// `WEEK_FIRST_WEEKDAY`, which is why modes 0/2 and 1/3 pair up the way they
/// do.
const fn week_mode(mode: u32) -> u32 {
    let format = mode & 7;
    if format & WEEK_MONDAY_FIRST == 0 {
        format ^ WEEK_FIRST_WEEKDAY
    } else {
        format
    }
}

const fn days_in_year(year: i32) -> i32 {
    if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
        366
    } else {
        365
    }
}

/// `MySQL`'s `calc_week`, returning `(year, week)`.
///
/// Ported rather than approximated. The four modes disagree about both the
/// first day of the week and whether week 1 must contain four days of the new
/// year, and chrono's ISO week matches only mode 3 — so `%U %u %V %v` cannot
/// be served by borrowing another library's week number. The paired year
/// (`%X`, `%x`) is why this returns the year too: a date in early January can
/// belong to the last week of the previous year.
fn mysql_calc_week(date: NaiveDate, mode: u32) -> (i32, u32) {
    let flags = week_mode(mode);
    let monday_first = flags & WEEK_MONDAY_FIRST != 0;
    let mut week_year = flags & WEEK_YEAR != 0;
    let first_weekday = flags & WEEK_FIRST_WEEKDAY != 0;

    let daynr = date.num_days_from_ce();
    let mut year = date.year();
    let first = NaiveDate::from_ymd_opt(year, 1, 1).expect("january 1 is valid");
    let mut first_daynr = first.num_days_from_ce();
    // MySQL's `calc_weekday`: 0 is Sunday under a Sunday-first mode and
    // Monday otherwise.
    let mut weekday = if monday_first {
        first.weekday().num_days_from_monday()
    } else {
        first.weekday().num_days_from_sunday()
    };

    if date.month() == 1 && date.day() <= 7 - weekday {
        if !week_year && ((first_weekday && weekday != 0) || (!first_weekday && weekday >= 4)) {
            return (year, 0);
        }
        week_year = true;
        year -= 1;
        let length = days_in_year(year);
        first_daynr -= length;
        weekday = (weekday + 53 * 7 - u32::try_from(length).unwrap_or(365)) % 7;
    }

    let offset = i32::try_from(weekday).unwrap_or(0);
    let days = if (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4) {
        daynr - (first_daynr + (7 - offset))
    } else {
        daynr - (first_daynr - offset)
    };

    if week_year && days >= 52 * 7 {
        weekday = (weekday + u32::try_from(days_in_year(year)).unwrap_or(365)) % 7;
        if (!first_weekday && weekday < 4) || (first_weekday && weekday == 0) {
            return (year + 1, 1);
        }
    }
    (year, u32::try_from(days / 7 + 1).unwrap_or(0))
}

/// `MySQL`'s ordinal suffix for `%D`: 11th/12th/13th are the exceptions to
/// the last-digit rule.
const fn ordinal_suffix(day: u32) -> &'static str {
    match (day % 100, day % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    }
}

/// Renders one `MySQL` `DATE_FORMAT` directive inventory.
///
/// This used to translate the format string into a chrono format string and
/// hand it over, mapping nine directives and forwarding the rest unchanged.
/// That silently produced wrong output wherever the two dialects use the same
/// letter differently: `%W` returned a week number rather than a weekday
/// name, `%D` returned `02/29/24` rather than `29th`, and `%v` returned a
/// whole formatted date rather than a week number. None of it errored, which
/// broke the rule that a query fails explicitly rather than returning a
/// plausible incompatible result. Emitting directly is the only way to be
/// sure a directive means what `MySQL` says it means.
///
/// Unknown directives copy the bare character, which is `MySQL`'s documented
/// behaviour — `%q` is `q`, not an error.
pub(super) fn mysql_date_format(value: NaiveDateTime, format: &str) -> String {
    mysql_date_format_locale(value, format, crate::calendar_locale::locale(0))
}

pub(super) fn mysql_date_format_locale(
    value: NaiveDateTime,
    format: &str,
    locale: &crate::calendar_locale::CalendarLocale,
) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(format.len());
    let mut characters = format.chars();
    let hour12 = match value.hour() % 12 {
        0 => 12,
        other => other,
    };
    let meridiem = if value.hour() < 12 { "AM" } else { "PM" };
    // Writing into the buffer rather than building a String per directive
    // keeps a row-loop format free of per-directive allocation.
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            output.push('%');
            break;
        };
        let written = match specifier {
            'a' => {
                output.push_str(locale.short_days[value.weekday().num_days_from_monday() as usize]);
                Ok(())
            }
            'b' => {
                output.push_str(locale.short_months[value.month0() as usize]);
                Ok(())
            }
            'c' => write!(output, "{}", value.month()),
            'D' => write!(output, "{}{}", value.day(), ordinal_suffix(value.day())),
            'd' => write!(output, "{:02}", value.day()),
            'e' => write!(output, "{}", value.day()),
            'f' => write!(output, "{:06}", value.and_utc().timestamp_subsec_micros()),
            'H' => write!(output, "{:02}", value.hour()),
            'h' | 'I' => write!(output, "{hour12:02}"),
            'i' => write!(output, "{:02}", value.minute()),
            'j' => write!(output, "{:03}", value.ordinal()),
            'k' => write!(output, "{}", value.hour()),
            'l' => write!(output, "{hour12}"),
            'M' => {
                output.push_str(locale.months[value.month0() as usize]);
                Ok(())
            }
            'm' => write!(output, "{:02}", value.month()),
            'p' => {
                output.push_str(meridiem);
                Ok(())
            }
            'r' => write!(
                output,
                "{hour12:02}:{:02}:{:02} {meridiem}",
                value.minute(),
                value.second()
            ),
            'S' | 's' => write!(output, "{:02}", value.second()),
            'T' => write!(
                output,
                "{:02}:{:02}:{:02}",
                value.hour(),
                value.minute(),
                value.second()
            ),
            'U' => write!(output, "{:02}", mysql_calc_week(value.date(), 0).1),
            'u' => write!(output, "{:02}", mysql_calc_week(value.date(), 1).1),
            'V' => write!(output, "{:02}", mysql_calc_week(value.date(), 2).1),
            'v' => write!(output, "{:02}", mysql_calc_week(value.date(), 3).1),
            'W' => {
                output.push_str(locale.days[value.weekday().num_days_from_monday() as usize]);
                Ok(())
            }
            'w' => write!(output, "{}", value.weekday().num_days_from_sunday()),
            'X' => write!(output, "{:04}", mysql_calc_week(value.date(), 2).0),
            'x' => write!(output, "{:04}", mysql_calc_week(value.date(), 3).0),
            'Y' => write!(output, "{:04}", value.year()),
            'y' => write!(output, "{:02}", value.year().rem_euclid(100)),
            '%' => {
                output.push('%');
                Ok(())
            }
            other => {
                output.push(other);
                Ok(())
            }
        };
        // Writing into a String is infallible; the Result exists only because
        // fmt::Write is generic over sinks that can fail.
        debug_assert!(written.is_ok(), "writing into a String cannot fail");
    }
    output
}

/// The date and time `MySQL` reads from an integer given where a date is
/// expected: YYMMDD, YYYYMMDD, YYMMDDHHMMSS or YYYYMMDDHHMMSS, with a
/// two-digit year taking the nearest century.
pub(super) fn numeric_datetime(number: i128) -> Option<NaiveDateTime> {
    let (date, time) = match number {
        101..=691_231 => (number + 20_000_000, 0),
        700_101..=991_231 => (number + 19_000_000, 0),
        10_000_101..=99_991_231 => (number, 0),
        101_000_000..=691_231_235_959 => (number / 1_000_000 + 20_000_000, number % 1_000_000),
        700_101_000_000..=991_231_235_959 => (number / 1_000_000 + 19_000_000, number % 1_000_000),
        10_000_101_000_000..=99_991_231_235_959 => (number / 1_000_000, number % 1_000_000),
        _ => return None,
    };
    let part = |value: i128, divisor: i128| u32::try_from(value / divisor % 100).ok();
    NaiveDate::from_ymd_opt(
        i32::try_from(date / 10_000).ok()?,
        part(date, 100)?,
        part(date, 1)?,
    )?
    .and_hms_opt(part(time, 10_000)?, part(time, 100)?, part(time, 1)?)
}

/// A date part of a value `MySQL` does not store as a date: an integer is a
/// packed date and time, or for the time-of-day parts a packed HHMMSS time,
/// and text that holds only a time still has an hour, minute and second.
pub(super) fn date_part_of(value: &Value, integer: bool, part: DatePart) -> Result<u64, ExecError> {
    let number = match value {
        Value::Int64(number) if integer => Some(i128::from(*number)),
        Value::UInt64(number) if integer => Some(i128::from(*number)),
        _ => None,
    };
    if matches!(part, DatePart::Hour | DatePart::Minute | DatePart::Second) {
        let time = match number {
            // A number with more digits than the largest TIME is a date and
            // time; one between the two is neither.
            Some(number) if number.unsigned_abs() < 10_000_000_000 => {
                Some(packed_time(number.unsigned_abs()).ok_or(ExecError::InvalidDateTime)?)
            }
            Some(_) => None,
            None => {
                let text = scalar_string(value)?;
                // Seven digits or fewer are a packed TIME, HHMMSS.
                if text.len() <= 7
                    && !text.is_empty()
                    && text.bytes().all(|byte| byte.is_ascii_digit())
                {
                    let number: u128 = text.parse().map_err(|_| ExecError::InvalidDateTime)?;
                    Some(packed_time(number).ok_or(ExecError::InvalidDateTime)?)
                } else {
                    match parse_mysql_datetime(&text) {
                        Ok(datetime) => return Ok(date_part(datetime, part)),
                        Err(_) => Some(text_time(&text).ok_or(ExecError::InvalidDateTime)?),
                    }
                }
            }
        };
        if let Some((hours, minutes, seconds)) = time {
            return Ok(match part {
                DatePart::Hour => hours,
                DatePart::Minute => minutes,
                _ => seconds,
            });
        }
    }
    let datetime = match number {
        Some(number) => numeric_datetime(number).ok_or(ExecError::InvalidDateTime)?,
        None => parse_mysql_datetime(&scalar_string(value)?)?,
    };
    Ok(date_part(datetime, part))
}

/// HHMMSS packed into an integer, up to the largest TIME.
fn packed_time(number: u128) -> Option<(u64, u64, u64)> {
    let minutes = u64::try_from(number / 100 % 100).ok()?;
    let seconds = u64::try_from(number % 100).ok()?;
    (number <= 8_385_959 && minutes < 60 && seconds < 60)
        .then(|| Some((u64::try_from(number / 10_000).ok()?, minutes, seconds)))
        .flatten()
}

/// A time written `[-]H:MM[:SS[.fraction]]`; hours are not limited to a day.
fn text_time(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.strip_prefix('-').unwrap_or(text);
    let field = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let mut fields = text.split(':');
    let hours = field(fields.next()?)?;
    let minutes = field(fields.next()?)?;
    let seconds = match fields.next() {
        None => 0,
        Some(seconds) => field(seconds.split_once('.').map_or(seconds, |(whole, _)| whole))?,
    };
    // Past the largest TIME, the value is that largest TIME.
    (fields.next().is_none() && minutes < 60 && seconds < 60).then_some(if hours > 838 {
        (838, 59, 59)
    } else {
        (hours, minutes, seconds)
    })
}

/// A date written with punctuation other than `-` between its parts,
/// rewritten with dashes; `None` when it already uses dashes or is not a
/// date.
fn dashed_date(value: &str) -> Option<String> {
    let value = value.trim_start();
    let year_end = value.find(|character: char| !character.is_ascii_digit())?;
    let separator = value[year_end..].chars().next()?;
    // A colon separates the parts of a time, never of a date.
    if year_end == 0
        || year_end > 4
        || matches!(separator, '-' | ':')
        || !separator.is_ascii_punctuation()
    {
        return None;
    }
    let rest = &value[year_end + 1..];
    let month_end = rest.find(|character: char| !character.is_ascii_digit())?;
    if !(1..=2).contains(&month_end) || rest[month_end..].chars().next()? != separator {
        return None;
    }
    let day = &rest[month_end + 1..];
    let day_end = day
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(day.len());
    if !(1..=2).contains(&day_end) {
        return None;
    }
    Some(format!(
        "{}-{}-{}",
        &value[..year_end],
        &rest[..month_end],
        day
    ))
}
