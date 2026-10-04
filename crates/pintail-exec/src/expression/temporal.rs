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

/// `MySQL`'s weekday of a date, 0 for Monday, counted from [`calc_daynr`]
/// so the year zero has no leap day, as in `MySQL`.
pub(super) fn mysql_weekday(date: NaiveDate) -> u32 {
    let [year, month, day] = date_fields(date);
    calc_weekday(calc_daynr(year, month, day), false)
}

pub(super) fn parse_mysql_datetime(value: &str) -> Result<NaiveDateTime, ExecError> {
    let value = value.trim();
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
    if let Some((whole, fraction)) = trimmed.split_once('.')
        && matches!(whole.len(), 12 | 14)
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && !fraction.is_empty()
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        let nanos = fraction
            .bytes()
            .take(9)
            .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'))
            * 10_u32.pow(
                u32::try_from(9_usize.saturating_sub(fraction.len()))
                    .map_err(|_| ExecError::InvalidDateTime)?,
            );
        return parse_mysql_datetime(whole)?
            .with_nanosecond(nanos)
            .ok_or(ExecError::InvalidDateTime);
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
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(value);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .or_else(|| parse_relaxed_datetime(value, false))
        .ok_or(ExecError::InvalidDateTime)
}

pub(super) fn parse_calendar_cast(text: &str) -> Result<NaiveDateTime, ExecError> {
    parse_mysql_datetime(text)
        .or_else(|_| parse_relaxed_datetime(text.trim(), true).ok_or(ExecError::InvalidDateTime))
}

fn temporal_digits<'a>(input: &mut &'a str, max: usize) -> Option<&'a str> {
    let end = input.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 || end > max {
        return None;
    }
    let digits = &input[..end];
    *input = &input[end..];
    Some(digits)
}

fn temporal_separator(input: &mut &str, clock: bool) -> Option<()> {
    let end = input
        .bytes()
        .take_while(|byte| {
            byte.is_ascii_punctuation() || (clock && (byte.is_ascii_whitespace() || *byte == b'T'))
        })
        .count();
    if end == 0 {
        return None;
    }
    *input = &input[end..];
    Some(())
}

/// Date fields and clock fields accept mixed and repeated punctuation.
/// In an untyped argument, a colon-only triple remains a TIME duration.
fn parse_relaxed_datetime(text: &str, calendar_context: bool) -> Option<NaiveDateTime> {
    let (clock, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !calendar_context
        && clock.contains(':')
        && clock
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b':')
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if let Some((date, clock)) = text.split_once('.')
        && date.len() == 8
        && clock.len() == 6
        && date
            .bytes()
            .chain(clock.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return numeric_datetime(format!("{date}{clock}").parse().ok()?);
    }
    let mut remaining = text;
    let written_year = temporal_digits(&mut remaining, 4)?;
    let mut year = written_year.parse::<i32>().ok()?;
    if written_year.len() <= 2 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    temporal_separator(&mut remaining, false)?;
    let month = temporal_digits(&mut remaining, 2)?.parse().ok()?;
    temporal_separator(&mut remaining, false)?;
    let day = temporal_digits(&mut remaining, 2)?.parse().ok()?;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let mut clock = [0_u32; 3];
    for part in &mut clock {
        if remaining.is_empty() {
            break;
        }
        temporal_separator(&mut remaining, true)?;
        *part = temporal_digits(&mut remaining, 2)?.parse().ok()?;
    }
    let fraction = if remaining.is_empty() {
        ""
    } else {
        remaining.strip_prefix('.')?
    };
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let nanos = fraction
        .bytes()
        .take(9)
        .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'))
        * 10_u32.pow(u32::try_from(9_usize.saturating_sub(fraction.len())).ok()?);
    date.and_hms_nano_opt(clock[0], clock[1], clock[2], nanos)
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
        DatePart::DayOfWeek => u64::from((mysql_weekday(value.date()) + 1) % 7 + 1),
        // MySQL WEEKDAY: 0 = Monday .. 6 = Sunday.
        DatePart::WeekDay => u64::from(mysql_weekday(value.date())),
        DatePart::DayOfYear => {
            let [year, month, day] = date_fields(value.date());
            u64::try_from(calc_daynr(year, month, day) - calc_daynr(year, 1, 1) + 1).unwrap_or(0)
        }
        DatePart::Week => u64::from(mysql_calc_week(value.date(), 0).1),
        DatePart::IsoWeek => u64::from(mysql_calc_week(value.date(), 3).1),
        DatePart::WeekMode(mode) | DatePart::ExtractWeek(mode) => {
            u64::from(mysql_calc_week(value.date(), u32::from(mode)).1)
        }
    }
}

/// The week `EXTRACT(WEEK FROM ...)` counts for calendar fields under a
/// `WEEK()` mode. `MySQL` counts it for a zero date or a zero month or
/// day too, with the unsigned arithmetic [`calc_week_fields`] keeps.
pub(super) fn week_of_fields(year: u32, month: u32, day: u32, mode: u32) -> u32 {
    calc_week_fields(year, month, day, week_mode(mode)).1
}

/// A date's year, month and day as `MySQL`'s day counting reads them.
fn date_fields(date: NaiveDate) -> [u32; 3] {
    [
        u32::try_from(date.year()).unwrap_or(0),
        date.month(),
        date.day(),
    ]
}

/// `MySQL` `YEARWEEK(date, mode)`: `year * 100 + week` of the week the
/// date falls in, counted as a week of the year that holds it, so early
/// January can belong to the previous year's last week.
pub(super) fn mysql_yearweek(date: NaiveDate, mode: u32) -> u64 {
    let [year, month, day] = date_fields(date);
    let (year, week) = calc_week_fields(year, month, day, week_mode(mode) | WEEK_YEAR);
    // MySQL multiplies the year in 32 bits, so the year before the year
    // zero (which wraps) wraps again here.
    u64::from(year.wrapping_mul(100)) + u64::from(week)
}

/// Days between year 0 and the Unix epoch in `MySQL`'s `TO_DAYS` calendar.
pub(super) const TO_DAYS_EPOCH_OFFSET: i64 = 719_528;

/// `MySQL` `TIMESTAMPDIFF`: complete units from `from` to `to`, truncated
/// toward zero (negative when `to` precedes `from`). `Chrono`'s duration
/// accessors already truncate toward zero for the clock units.
pub(super) fn timestamp_diff(from: NaiveDateTime, to: NaiveDateTime, unit: IntervalUnit) -> i64 {
    // MySQL's year zero has no leap day: an interval across the end of its
    // February is a day shorter than the calendar here counts.
    let before_march = |value: NaiveDateTime| value.year() == 0 && value.month() <= 2;
    let leap_day = Duration::days(i64::from(before_march(from)) - i64::from(before_march(to)));
    let elapsed = to.signed_duration_since(from) - leap_day;
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
    shift_interval(value, amount, unit, subtract)?
        // A result past the DATETIME range is NULL, as in MySQL, not a date
        // with a five-digit year.
        .filter(|shifted| (0..=9999).contains(&shifted.year()))
        .ok_or(ExecError::InvalidDateTime)
}

/// [`apply_interval`] before its range check: `None` only where the
/// calendar itself cannot hold the result.
pub(super) fn shift_interval(
    value: NaiveDateTime,
    amount: i64,
    unit: IntervalUnit,
    subtract: bool,
) -> Result<Option<NaiveDateTime>, ExecError> {
    let amount = if subtract {
        amount.checked_neg().ok_or(ExecError::NumericOverflow)?
    } else {
        amount
    };
    Ok(match unit {
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
    })
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

/// The seconds east of UTC of a zone written as a fixed offset (`+05:30`),
/// as a session-zone reading parses it; `None` for a named zone, whose
/// offset can change within a column, and for text that is no zone.
pub(super) fn fixed_zone_seconds(text: &str) -> Option<i32> {
    match timezone_spec(text)? {
        ZoneSpec::Fixed(offset) => Some(offset.local_minus_utc()),
        ZoneSpec::Named(_) => None,
    }
}

/// A session zone as a reading of UTC instants: the seconds east of UTC it
/// reads each instant at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ZoneReading {
    /// One offset for every instant.
    Fixed(i32),
    /// An offset that depends on the instant (daylight saving, history).
    Named(chrono_tz::Tz),
}

impl ZoneReading {
    /// The zone `text` names, as a session-zone reading parses it.
    pub(super) fn of(text: &str) -> Option<Self> {
        Some(match timezone_spec(text)? {
            ZoneSpec::Fixed(offset) => Self::Fixed(offset.local_minus_utc()),
            ZoneSpec::Named(zone) => Self::Named(zone),
        })
    }

    /// Reads a stored timestamp using a zone resolved when its expression
    /// was compiled. Zero timestamps remain the stored spelling.
    pub(super) fn read_value(self, value: &Value) -> Result<Value, ExecError> {
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }
        let text = scalar_string(value)?;
        if text.starts_with("0000-00-00") {
            return Ok(Value::Utf8(text));
        }
        let mut spelled = [0; READING_BYTES];
        if let Some(reading) = ZoneCursor::new(self).read_canonical(text.as_bytes(), &mut spelled) {
            return Ok(Value::Utf8(
                String::from_utf8(reading.to_vec()).expect("canonical text is ASCII"),
            ));
        }
        Ok(self.read_value_generally(&text))
    }

    /// [`Self::read_value`] through the general conversion, which also
    /// reads text in shapes other than the canonical one.
    pub(crate) fn read_value_generally(self, text: &str) -> Value {
        let zone = match self {
            Self::Fixed(seconds) => {
                ZoneSpec::Fixed(FixedOffset::east_opt(seconds).expect("parsed offset"))
            }
            Self::Named(zone) => ZoneSpec::Named(zone),
        };
        let utc = ZoneSpec::Fixed(FixedOffset::east_opt(0).expect("UTC offset"));
        convert_tz_zones(text, false, || Some((utc, zone))).map_or(Value::Null, Value::Utf8)
    }

    /// Seconds east of UTC at the instant `utc_seconds` after the epoch:
    /// the offset `with_timezone` applies to that instant, so a reading
    /// built from it is the one the text conversion spells. `None` for an
    /// instant the calendar cannot hold.
    pub(super) fn seconds_east(self, utc_seconds: i64) -> Option<i32> {
        match self {
            Self::Fixed(seconds) => Some(seconds),
            Self::Named(zone) => {
                use chrono::Offset as _;
                let instant = chrono::DateTime::from_timestamp(utc_seconds, 0)?.naive_utc();
                Some(
                    zone.offset_from_utc_datetime(&instant)
                        .fix()
                        .local_minus_utc(),
                )
            }
        }
    }
}

/// The widest canonical datetime text: `YYYY-MM-DD HH:MM:SS.ffffff`.
pub(crate) const READING_BYTES: usize = 26;

/// A session zone reading stored `TIMESTAMP` text one value after another.
///
/// The general conversion parses each value into a calendar date-time,
/// resolves the zone by name, converts through the calendar and formats a
/// fresh string: a few hundred nanoseconds a row, which a named zone's
/// filter over a column with a zero `TIMESTAMP` (and so no packed units)
/// spent on every row. Stored text is canonical, so here it is read as
/// seconds and a fraction, shifted by the zone's offset at that second and
/// spelled back into a buffer. A column in time order asks about the same
/// second many times, so the offset is looked up again only when the
/// second changes.
pub(crate) struct ZoneCursor {
    zone: ZoneReading,
    /// The second last looked up and the zone's offset at it.
    last: Option<(i64, i32)>,
}

impl ZoneCursor {
    pub(crate) const fn new(zone: ZoneReading) -> Self {
        Self { zone, last: None }
    }

    fn seconds_east(&mut self, second: i64) -> Option<i32> {
        if let Some((known, offset)) = self.last
            && known == second
        {
            return Some(offset);
        }
        let offset = self.zone.seconds_east(second)?;
        self.last = Some((second, offset));
        Some(offset)
    }

    /// The reading of canonical stored text, `YYYY-MM-DD HH:MM:SS` with up
    /// to six fraction digits, spelled into `out`: byte for byte what
    /// [`ZoneReading::read_value`] answers through the general conversion.
    /// Every zone offset is whole seconds, so the fraction is carried over
    /// as written, with its own digits.
    ///
    /// `None` for anything else: the zero `TIMESTAMP` (which every zone
    /// reads as stored), text in another shape, a date the calendar
    /// rejects, and a reading outside the years canonical text spells.
    /// Those take the general conversion.
    pub(crate) fn read_canonical<'out>(
        &mut self,
        text: &[u8],
        out: &'out mut [u8; READING_BYTES],
    ) -> Option<&'out [u8]> {
        if text.len() > READING_BYTES {
            return None;
        }
        let micros = crate::batch::parse_datetime_micros(std::str::from_utf8(text).ok()?)?;
        let second = micros.div_euclid(1_000_000);
        let local = second.checked_add(i64::from(self.seconds_east(second)?))?;
        let (year, month, day) = pintail_types::civil_from_days(local.div_euclid(86_400));
        if !(0..=9999).contains(&year) {
            return None;
        }
        let clock = local.rem_euclid(86_400);
        let fields = [
            (0, 4, year),
            (5, 2, month),
            (8, 2, day),
            (11, 2, clock / 3_600),
            (14, 2, clock % 3_600 / 60),
            (17, 2, clock % 60),
        ];
        for (start, width, mut value) in fields {
            for position in (start..start + width).rev() {
                out[position] = b'0' + u8::try_from(value % 10).ok()?;
                value /= 10;
            }
        }
        for position in [4, 7] {
            out[position] = b'-';
        }
        out[10] = b' ';
        for position in [13, 16] {
            out[position] = b':';
        }
        out[19..text.len()].copy_from_slice(&text[19..]);
        Some(&out[..text.len()])
    }
}

/// A datetime offset is a signed two-digit hour and minute suffix.
pub(crate) fn has_timestamp_offset(text: &str) -> bool {
    let text = text.trim_end();
    let bytes = text.as_bytes();
    bytes.len() >= 15
        && matches!(bytes.get(bytes.len() - 6), Some(b'+' | b'-'))
        && bytes.get(bytes.len() - 3) == Some(&b':')
        && text[..text.len() - 6].contains([' ', 'T'])
        && text[..text.len() - 6]
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .count()
            >= 6
}

/// Explicit input offsets resolve before calendar or clock extraction. The
/// destination is captured in the expression so worker threads need no session state.
pub(super) fn normalize_timestamp_offset(text: &str, zone: &str) -> Option<String> {
    if !has_timestamp_offset(text) {
        return Some(text.to_owned());
    }
    let text = text.trim_end();
    let (calendar, offset) = text.split_at(text.len() - 6);
    if offset == "-00:00"
        || !offset.as_bytes()[1..3].iter().all(u8::is_ascii_digit)
        || !offset.as_bytes()[4..6].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let ZoneSpec::Fixed(offset) = timezone_spec(offset)? else {
        return None;
    };
    let naive = parse_mysql_datetime(calendar).ok()?;
    let fsp = calendar
        .rsplit_once('.')
        .map_or(0, |(_, digits)| digits.len().min(6));
    let rounded = naive
        .with_nanosecond(0)?
        .checked_add_signed(Duration::microseconds(i64::from(
            (naive.nanosecond() + 500) / 1_000,
        )))?;
    let utc = offset
        .from_local_datetime(&rounded)
        .single()?
        .with_timezone(&Utc);
    let local = if zone == "SYSTEM" {
        utc.with_timezone(&chrono::Local).naive_local()
    } else {
        match timezone_spec(zone)? {
            ZoneSpec::Fixed(offset) => utc.with_timezone(&offset).naive_local(),
            ZoneSpec::Named(zone) => utc.with_timezone(&zone).naive_local(),
        }
    };
    if !(0..=9999).contains(&local.year()) {
        return None;
    }
    Some(super::format_with_fraction(
        local,
        u8::try_from(fsp).ok()?,
        "%Y-%m-%d %H:%M:%S",
    ))
}

/// `CONVERT_TZ` on the canonical datetime text carrier. Ambiguous local
/// times (DST fall-back) take the earlier offset; nonexistent local times
/// resolve to the first instant after the gap.
pub(super) fn convert_tz(text: &str, from: &str, to: &str) -> Option<String> {
    convert_tz_impl(text, from, to, false)
}

pub(super) fn convert_tz_bounded(text: &str, from: &str, to: &str) -> Option<String> {
    convert_tz_impl(text, from, to, true)
}

fn convert_tz_impl(text: &str, from: &str, to: &str, bounded: bool) -> Option<String> {
    convert_tz_zones(text, bounded, || {
        Some((timezone_spec(from)?, timezone_spec(to)?))
    })
}

fn convert_tz_zones(
    text: &str,
    bounded: bool,
    zones: impl FnOnce() -> Option<(ZoneSpec, ZoneSpec)>,
) -> Option<String> {
    let trimmed = text.trim();
    let naive = canonical_datetime(trimmed)
        .ok_or(())
        .or_else(|()| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| {
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        })
        .ok()?;
    let fraction_digits = trimmed
        .rsplit_once('.')
        .map_or(0, |(_, fraction)| fraction.len().min(6));
    let (from, to) = zones()?;
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
            LocalResult::None => chrono_tz::GapInfo::new(&naive, &zone)?
                .end?
                .with_timezone(&Utc),
        },
    };
    // CONVERT_TZ leaves an out-of-range input unchanged after resolving its
    // source zone. Internal session-zone conversion has no such restriction.
    let converted = if bounded && !(1..=super::UNIX_TIMESTAMP_MAX).contains(&utc.timestamp()) {
        naive
    } else {
        match to {
            ZoneSpec::Fixed(offset) => utc.with_timezone(&offset).naive_local(),
            ZoneSpec::Named(zone) => utc.with_timezone(&zone).naive_local(),
        }
    };
    let base = canonical_datetime_text(converted)
        .unwrap_or_else(|| converted.format("%Y-%m-%d %H:%M:%S").to_string());
    if fraction_digits == 0 {
        return Some(base);
    }
    let micros = format!("{:06}", converted.and_utc().timestamp_subsec_micros());
    Some(format!("{base}.{}", &micros[..fraction_digits]))
}

/// Reads the canonical datetime text a datetime column renders,
/// `YYYY-MM-DD HH:MM:SS` with up to six fraction digits, without the
/// general format parser: `CONVERT_TZ` over a column met nothing else and
/// spent most of its time interpreting the format per row. Anything else,
/// or a date the calendar rejects, answers `None` and takes the general
/// parser, which decides what it means.
fn canonical_datetime(text: &str) -> Option<NaiveDateTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        bytes[range].iter().try_fold(0_u32, |value, byte| {
            byte.is_ascii_digit()
                .then(|| value * 10 + u32::from(byte - b'0'))
        })
    };
    let micros = match bytes.get(19..) {
        Some([]) => 0,
        Some([b'.', digits @ ..]) if (1..=6).contains(&digits.len()) => {
            let fraction = number(20..bytes.len())?;
            let scale = 6 - u32::try_from(digits.len()).ok()?;
            fraction * 10_u32.pow(scale)
        }
        _ => return None,
    };
    NaiveDate::from_ymd_opt(
        i32::try_from(number(0..4)?).ok()?,
        number(5..7)?,
        number(8..10)?,
    )?
    .and_hms_micro_opt(number(11..13)?, number(14..16)?, number(17..19)?, micros)
}

/// `YYYY-MM-DD HH:MM:SS` for a datetime in the four-digit years, written
/// directly rather than through the general formatter. `None` otherwise,
/// and for a leap second, which the general formatter spells its own way.
fn canonical_datetime_text(value: NaiveDateTime) -> Option<String> {
    if !(0..=9999).contains(&value.year()) || value.nanosecond() >= 1_000_000_000 {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    ))
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

/// `MySQL`'s `calc_week` of a date under a `WEEK()` mode, returning
/// `(year, week)`.
///
/// Ported rather than approximated. The four modes disagree about both the
/// first day of the week and whether week 1 must contain four days of the new
/// year, and chrono's ISO week matches only mode 3. Counting from
/// [`calc_daynr`] rather than chrono's calendar also keeps the year zero
/// `MySQL`'s: chrono gives it a leap day, `MySQL` does not.
fn mysql_calc_week(date: NaiveDate, mode: u32) -> (u32, u32) {
    let [year, month, day] = date_fields(date);
    calc_week_fields(year, month, day, week_mode(mode))
}

/// `MySQL`'s day number of a calendar written as fields (`calc_daynr`),
/// whether or not a calendar holds it: a zero day counts as the last day
/// of the month before, a zero month as the December before, and the year
/// zero has no leap day. `2024-02-00` is January 31st; `0000-00-xx` is day
/// zero.
pub(super) fn calc_daynr(year: u32, month: u32, day: u32) -> i64 {
    if year == 0 && month == 0 {
        return 0;
    }
    let mut year = i64::from(year);
    let month = i64::from(month);
    let mut days = 365 * year + 31 * (month - 1) + i64::from(day);
    if month <= 2 {
        year -= 1;
    } else {
        days -= (month * 4 + 23) / 10;
    }
    // Division truncates toward zero, as the C it mirrors does, which is
    // what leaves the year zero without a leap day.
    let centuries = ((year / 100 + 1) * 3) / 4;
    days + year / 4 - centuries
}

/// [`calc_daynr`] of a calendar date: what `TO_DAYS` answers, and what
/// `DATEDIFF` subtracts.
pub(super) fn mysql_daynr(date: NaiveDate) -> i64 {
    let [year, month, day] = date_fields(date);
    calc_daynr(year, month, day)
}

/// `MySQL`'s `calc_weekday`: 0 is Monday, or Sunday when `sunday_first`.
fn calc_weekday(daynr: i64, sunday_first: bool) -> u32 {
    u32::try_from((daynr + 5 + i64::from(sunday_first)).rem_euclid(7)).unwrap_or(0)
}

/// `MySQL`'s `calc_days_in_year`, under which the year zero is not leap.
fn calc_days_in_year(year: u32) -> u32 {
    if year.is_multiple_of(4)
        && (!year.is_multiple_of(100) || (year.is_multiple_of(400) && year != 0))
    {
        366
    } else {
        365
    }
}

/// `MySQL`'s `calc_week` over calendar fields and raw week flags,
/// returning `(year, week)`, with its unsigned arithmetic kept: a date
/// before its own year's first day (a zero month) counts a wrapped day
/// difference, and the year before the year zero wraps too. `MySQL` prints
/// what that arithmetic gives, so this gives the same.
fn calc_week_fields(year: u32, month: u32, day: u32, flags: u32) -> (u32, u32) {
    let monday_first = flags & WEEK_MONDAY_FIRST != 0;
    let mut week_year = flags & WEEK_YEAR != 0;
    let first_weekday = flags & WEEK_FIRST_WEEKDAY != 0;
    let daynr = calc_daynr(year, month, day);
    let mut first_daynr = calc_daynr(year, 1, 1);
    let mut weekday = calc_weekday(first_daynr, !monday_first);
    let mut year = year;
    if month == 1 && day <= 7 - weekday {
        if !week_year && ((first_weekday && weekday != 0) || (!first_weekday && weekday >= 4)) {
            return (year, 0);
        }
        week_year = true;
        year = year.wrapping_sub(1);
        let length = calc_days_in_year(year);
        first_daynr -= i64::from(length);
        weekday = (weekday + 53 * 7 - length) % 7;
    }
    let since = if (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4) {
        daynr - (first_daynr + i64::from(7 - weekday))
    } else {
        daynr - (first_daynr - i64::from(weekday))
    };
    let days = u32::try_from(since.rem_euclid(1 << 32)).unwrap_or(0);
    if week_year && days >= 52 * 7 {
        weekday = (weekday + calc_days_in_year(year)) % 7;
        if (!first_weekday && weekday < 4) || (first_weekday && weekday == 0) {
            return (year.wrapping_add(1), 1);
        }
    }
    (year, days / 7 + 1)
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
#[cfg(test)]
pub(super) fn mysql_date_format(value: NaiveDateTime, format: &str) -> String {
    mysql_date_format_locale(value, format, crate::calendar_locale::locale(0))
}

pub(super) fn mysql_date_format_locale(
    value: NaiveDateTime,
    format: &str,
    locale: &crate::calendar_locale::CalendarLocale,
) -> String {
    let calendar = [
        u32::try_from(value.year()).unwrap_or(0),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.and_utc().timestamp_subsec_micros(),
    ];
    // Only a zero month (or a zero year and month) answers NULL, and a
    // date-time has neither.
    mysql_date_format_calendar(calendar, format, locale).unwrap_or_default()
}

/// `DATE_FORMAT` of calendar fields - year, month, day, hour, minute,
/// second, microsecond - as `MySQL` formats a date it holds as fields
/// rather than as a day: a stored February 30th prints its own parts, and
/// a zero day or month prints as zero while the weekday, week and
/// day-of-year directives count from the day [`calc_daynr`] gives it.
/// `2024-02-00` is a Wednesday in week 5 because January 31st is.
///
/// `None` (SQL NULL) where `MySQL` refuses a directive: a month name of a
/// zero month, and a weekday of a date whose year and month are both zero.
pub(super) fn mysql_date_format_calendar(
    [year, month, day, hour, minute, second, micros]: [u32; 7],
    format: &str,
    locale: &crate::calendar_locale::CalendarLocale,
) -> Option<String> {
    use std::fmt::Write as _;

    // Digits left-padded with zeros to `width`, the padding going before a
    // sign as `MySQL`'s does: a day of year of -5 prints `0-5`.
    fn padded(output: &mut String, value: impl std::fmt::Display, width: usize) {
        let text = value.to_string();
        for _ in text.len()..width {
            output.push('0');
        }
        output.push_str(&text);
    }

    let mut output = String::with_capacity(format.len());
    let mut characters = format.chars();
    let hour12 = (hour % 24 + 11) % 12 + 1;
    let meridiem = if hour % 24 < 12 { "AM" } else { "PM" };
    let daynr = || calc_daynr(year, month, day);
    let weekday =
        |sunday_first: bool| (month != 0 || year != 0).then(|| calc_weekday(daynr(), sunday_first));
    let week = |flags: u32| calc_week_fields(year, month, day, flags);
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            output.push('%');
            break;
        };
        match specifier {
            'a' => output.push_str(locale.short_days[weekday(false)? as usize]),
            'W' => output.push_str(locale.days[weekday(false)? as usize]),
            'w' => padded(&mut output, weekday(true)?, 1),
            'b' => output.push_str(locale.short_months[month.checked_sub(1)? as usize]),
            'M' => output.push_str(locale.months[month.checked_sub(1)? as usize]),
            'c' => padded(&mut output, month, 1),
            'm' => padded(&mut output, month, 2),
            'D' => {
                padded(&mut output, day, 1);
                output.push_str(ordinal_suffix(day));
            }
            'd' => padded(&mut output, day, 2),
            'e' => padded(&mut output, day, 1),
            'f' => padded(&mut output, micros, 6),
            'H' => padded(&mut output, hour, 2),
            'h' | 'I' => padded(&mut output, hour12, 2),
            'i' => padded(&mut output, minute, 2),
            'j' => padded(&mut output, daynr() - calc_daynr(year, 1, 1) + 1, 3),
            'k' => padded(&mut output, hour, 1),
            'l' => padded(&mut output, hour12, 1),
            'p' => output.push_str(meridiem),
            'r' => {
                // Writing into a String cannot fail.
                let _ = write!(output, "{hour12:02}:{minute:02}:{second:02} {meridiem}");
            }
            'S' | 's' => padded(&mut output, second, 2),
            'T' => {
                let _ = write!(output, "{hour:02}:{minute:02}:{second:02}");
            }
            'U' => padded(&mut output, week(WEEK_FIRST_WEEKDAY).1, 2),
            'u' => padded(&mut output, week(WEEK_MONDAY_FIRST).1, 2),
            'V' => padded(&mut output, week(WEEK_YEAR | WEEK_FIRST_WEEKDAY).1, 2),
            'v' => padded(&mut output, week(WEEK_YEAR | WEEK_MONDAY_FIRST).1, 2),
            'X' => padded(&mut output, week(WEEK_YEAR | WEEK_FIRST_WEEKDAY).0, 4),
            'x' => padded(&mut output, week(WEEK_YEAR | WEEK_MONDAY_FIRST).0, 4),
            'Y' => padded(&mut output, year, 4),
            'y' => padded(&mut output, year % 100, 2),
            other => output.push(other),
        }
    }
    Some(output)
}

/// `TIME_FORMAT` of a time in microseconds. The clock directives print as
/// `DATE_FORMAT` prints them, the hour as it is (it may pass 23, and `%h`
/// and `%p` read it within its day); a negative time carries one leading
/// minus sign. `None` (SQL NULL) for a directive that needs a date: a
/// name, a day of the year, a week.
pub(super) fn mysql_time_format(micros: i128, format: &str) -> Option<String> {
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character == '%'
            && matches!(
                characters.next(),
                Some('D' | 'j' | 'U' | 'u' | 'V' | 'v' | 'X' | 'x')
            )
        {
            return None;
        }
    }
    let seconds = micros.unsigned_abs() / 1_000_000;
    let clock = [
        0,
        0,
        0,
        u32::try_from(seconds / 3_600).ok()?,
        u32::try_from(seconds / 60 % 60).ok()?,
        u32::try_from(seconds % 60).ok()?,
        u32::try_from(micros.unsigned_abs() % 1_000_000).ok()?,
    ];
    let body = mysql_date_format_calendar(clock, format, crate::calendar_locale::locale(0))?;
    Some(if micros < 0 { format!("-{body}") } else { body })
}

/// The calendar fields of a stored date or date-time spelled as stored:
/// `YYYY-MM-DD` with an optional ` HH:MM:SS` and fraction, where a month
/// or day may be zero or a day past its month's end. `None` for any other
/// spelling.
pub(super) fn stored_calendar(text: &str) -> Option<[u32; 7]> {
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let digits = bytes.get(range)?;
        digits.iter().all(u8::is_ascii_digit).then(|| {
            digits
                .iter()
                .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'))
        })
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let mut calendar = [number(0..4)?, number(5..7)?, number(8..10)?, 0, 0, 0, 0];
    if bytes.len() > 10 {
        if bytes.len() < 19 || bytes[10] != b' ' || bytes[13] != b':' || bytes[16] != b':' {
            return None;
        }
        calendar[3] = number(11..13)?;
        calendar[4] = number(14..16)?;
        calendar[5] = number(17..19)?;
        if bytes.len() > 19 {
            let fraction = &bytes[20..];
            if bytes[19] != b'.' || fraction.is_empty() || fraction.len() > 6 {
                return None;
            }
            let digits = number(20..bytes.len())?;
            calendar[6] = digits * 10_u32.pow(u32::try_from(6 - fraction.len()).ok()?);
        }
    }
    let [_, month, day, hour, minute, second, _] = calendar;
    (month <= 12 && day <= 31 && hour <= 23 && minute <= 59 && second <= 59).then_some(calendar)
}

/// A date whose day runs past its month's end, as the day it runs into:
/// `2024-02-30 10:00:00` is `2024-03-01 10:00:00.000000`. `None` for a
/// date the calendar holds, a zero month or day, and anything else.
pub(super) fn rolled_calendar_text(text: &str) -> Option<String> {
    let [year, month, day, hour, minute, second, micros] = stored_calendar(text.trim())?;
    if month == 0 || day == 0 {
        return None;
    }
    let first = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, 1)?;
    let date = first.checked_add_signed(chrono::Duration::days(i64::from(day) - 1))?;
    if date.month() == month {
        return None;
    }
    Some(format!(
        "{} {hour:02}:{minute:02}:{second:02}.{micros:06}",
        date.format("%Y-%m-%d")
    ))
}

/// Text read as a TIME: the fields `MySQL` takes from it.
pub(super) struct TextTime {
    /// Whether the text opened with a minus sign.
    pub negative: bool,
    /// Hours, with any leading day count folded in.
    pub hours: u64,
    /// Minutes, below 60.
    pub minutes: u64,
    /// Seconds, below 60.
    pub seconds: u64,
    /// The fraction digits as written.
    pub fraction: String,
    /// The year, month and day when the text was a full date and time,
    /// whose clock this is. A month or day may be zero.
    pub calendar: Option<[u32; 3]>,
}

/// Reads text as a TIME the way `MySQL` does.
///
/// Twelve characters or more are tried as a date and time first, and count
/// as one only when a space separates the date from the clock or the text
/// is all digits: `2024-01-15T10:20:30` is not, and neither is any date
/// alone. Everything else is a duration: an optional day count and a space,
/// then `H:M:S`, `H:M`, or one number read as `HHMMSS` - so `2024-01-15`
/// is the number 2024, twenty minutes and twenty-four seconds, with the
/// rest dropped. A fraction may follow; anything after it is ignored.
/// `None` where `MySQL` answers NULL: nothing readable, a minute or second
/// past 59, a date and time no calendar holds, or an exponent.
pub(super) fn text_time_of(text: &str) -> Option<TextTime> {
    let body = text.trim_start_matches(|character: char| character.is_ascii_whitespace());
    let (negative, body) = body
        .strip_prefix('-')
        .map_or((false, body), |rest| (true, rest));
    if body.len() >= 12 {
        match datetime_clock(body) {
            DatetimeText::Clock(clock) => return Some(clock),
            DatetimeText::NoCalendar => return None,
            DatetimeText::Duration => {}
        }
    }
    let duration = pintail_types::read_duration_text(body)?;
    Some(TextTime {
        negative,
        hours: duration.hours,
        minutes: duration.minutes,
        seconds: duration.seconds,
        fraction: duration.fraction.to_owned(),
        calendar: None,
    })
}

/// What twelve or more characters of text are when a TIME is read from
/// them.
enum DatetimeText {
    /// Not a date and time at all: read as a duration instead.
    Duration,
    /// A date and time no calendar holds.
    NoCalendar,
    /// A date and time, and its clock.
    Clock(TextTime),
}

fn datetime_clock(body: &str) -> DatetimeText {
    let trimmed = body.trim_end();
    let whole = trimmed.split('.').next().unwrap_or(trimmed);
    let compact = whole.bytes().all(|byte| byte.is_ascii_digit());
    // Fewer than twelve digits are a number read as HHMMSS, whatever
    // follows the point.
    if compact && whole.len() < 12 {
        return DatetimeText::Duration;
    }
    if !compact {
        // The date is three fields, and a space ends it.
        let Some((date, _)) = trimmed.split_once(|character: char| character.is_ascii_whitespace())
        else {
            return DatetimeText::Duration;
        };
        let separators = date.bytes().filter(u8::is_ascii_punctuation).count();
        if separators < 2 || !date.starts_with(|character: char| character.is_ascii_digit()) {
            return DatetimeText::Duration;
        }
    }
    let clock = |calendar: [u32; 3], hours: u32, minutes: u32, seconds: u32, micros: u32| {
        DatetimeText::Clock(TextTime {
            negative: false,
            hours: u64::from(hours),
            minutes: u64::from(minutes),
            seconds: u64::from(seconds),
            fraction: format!("{micros:06}"),
            calendar: Some(calendar),
        })
    };
    if let Ok(value) = parse_mysql_datetime(trimmed) {
        return clock(
            [
                u32::try_from(value.year()).unwrap_or(0),
                value.month(),
                value.day(),
            ],
            value.hour(),
            value.minute(),
            value.second(),
            value.and_utc().timestamp_subsec_micros(),
        );
    }
    // A zero month or day still has its clock, however much of the clock
    // is written.
    partial_calendar_clock(trimmed).map_or(
        DatetimeText::NoCalendar,
        |[year, month, day, hour, minute, second, micros]| {
            clock([year, month, day], hour, minute, second, micros)
        },
    )
}

/// `YYYY-MM-DD H[:M[:S[.f]]]` with a zero month or day: its fields.
fn partial_calendar_clock(text: &str) -> Option<[u32; 7]> {
    let (date, time) = text.split_once(|character: char| character.is_ascii_whitespace())?;
    let number = |field: &str| -> Option<u32> {
        (!field.is_empty() && field.len() <= 4 && field.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| field.parse().ok())
            .flatten()
    };
    let mut fields = date.split('-');
    let (year, month, day) = (
        number(fields.next()?)?,
        number(fields.next()?)?,
        number(fields.next()?)?,
    );
    if fields.next().is_some() || month > 12 || day > 31 || (month != 0 && day != 0) {
        return None;
    }
    let (clock, fraction) = time.trim().split_once('.').unwrap_or((time.trim(), ""));
    if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut parts = clock.split(':');
    let hour = number(parts.next()?)?;
    let minute = parts.next().map_or(Some(0), number)?;
    let second = parts.next().map_or(Some(0), number)?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let micros = format!("{fraction:0<6}").parse().ok()?;
    Some([year, month, day, hour, minute, second, micros])
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

/// A packed number's calendar fields as `MySQL` reads them - the same digit
/// layouts as [`numeric_datetime`] - kept as numbers so a zero month or day,
/// which no civil date can hold, survives: `200012010000` is
/// `2020-00-12 01:00:00`. `None` for a number outside every layout or a
/// field outside its range.
pub(super) fn numeric_calendar(number: i128) -> Option<[u32; 6]> {
    let (date, time) = match number {
        0 => (0, 0),
        101..=691_231 => (number + 20_000_000, 0),
        700_101..=991_231 => (number + 19_000_000, 0),
        10_000_101..=99_991_231 => (number, 0),
        101_000_000..=691_231_235_959 => (number / 1_000_000 + 20_000_000, number % 1_000_000),
        700_101_000_000..=991_231_235_959 => (number / 1_000_000 + 19_000_000, number % 1_000_000),
        10_000_101_000_000..=99_991_231_235_959 => (number / 1_000_000, number % 1_000_000),
        _ => return None,
    };
    let part = |value: i128, divisor: i128| u32::try_from(value / divisor % 100).ok();
    calendar_in_range([
        u32::try_from(date / 10_000).ok()?,
        part(date, 100)?,
        part(date, 1)?,
        part(time, 10_000)?,
        part(time, 100)?,
        part(time, 1)?,
    ])
}

/// Text read as `MySQL` reads a date written with any punctuation between
/// its parts: digit groups for year, month, day and an optional hour,
/// minute and second, a one- or two-digit year taking the century `MySQL`
/// gives it. `'12:00:00-12.34.56'` is `2012-00-00 12:34:56`; `'1-2-3'` is
/// `2001-02-03`. A trailing fraction is ignored. `None` unless the text is
/// digits and punctuation with three to six groups in range.
pub(super) fn loose_calendar(text: &str) -> Option<[u32; 6]> {
    let text = text.trim();
    if !text.starts_with(|character: char| character.is_ascii_digit())
        || !text.chars().all(|character| {
            character.is_ascii_digit() || character.is_ascii_punctuation() || character == ' '
        })
    {
        return None;
    }
    let groups = text
        .split(|character: char| !character.is_ascii_digit())
        .filter(|group| !group.is_empty())
        .collect::<Vec<_>>();
    if !(3..=7).contains(&groups.len()) {
        return None;
    }
    let mut parts = [0_u32; 6];
    for (slot, group) in parts.iter_mut().zip(&groups) {
        if group.len() > 4 {
            return None;
        }
        *slot = group.parse().ok()?;
    }
    if groups[0].len() <= 2 {
        parts[0] += if parts[0] < 70 { 2000 } else { 1900 };
    }
    calendar_in_range(parts)
}

/// [`loose_calendar`] for text whose first separator is a dash or a slash:
/// a date. A colon starts a time, and a leading sign a duration.
fn short_date(text: &str) -> Option<[u32; 6]> {
    let text = text.trim();
    let at = text.find(|character: char| !character.is_ascii_digit())?;
    (at > 0 && matches!(text.as_bytes()[at], b'-' | b'/'))
        .then(|| loose_calendar(text))
        .flatten()
}

fn calendar_in_range(parts: [u32; 6]) -> Option<[u32; 6]> {
    let [year, month, day, hour, minute, second] = parts;
    (year <= 9999 && month <= 12 && day <= 31 && hour <= 23 && minute <= 59 && second <= 59)
        .then_some(parts)
}

/// Calendar fields rendered as a `DATE` or `DATETIME` answer under the
/// session's zero-date policy - bit 0 `NO_ZERO_DATE`, bit 1
/// `NO_ZERO_IN_DATE`, bit 2 `ALLOW_INVALID_DATES` - or NULL where the policy
/// refuses them, as `MySQL` answers such a conversion.
pub(super) fn policy_calendar(parts: [u32; 6], policy: u64, datetime: bool) -> Value {
    let [year, month, day, hour, minute, second] = parts;
    let all_zero = year == 0 && month == 0 && day == 0;
    let refused = (policy & 1 != 0 && all_zero)
        || (policy & 2 != 0 && !all_zero && (month == 0 || day == 0))
        || (policy & 4 == 0
            && month != 0
            && day != 0
            && NaiveDate::from_ymd_opt(i32::try_from(year).unwrap_or(0), month, day).is_none());
    if refused {
        return Value::Null;
    }
    let date = format!("{year:04}-{month:02}-{day:02}");
    Value::Utf8(if datetime {
        format!("{date} {hour:02}:{minute:02}:{second:02}")
    } else {
        date
    })
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
                        // A date whose year is one digit, or whose parts
                        // are short, still has a clock: `'1-2-3'` is
                        // 2001-02-03 at midnight. Only a dash or slash
                        // separates date parts; a colon starts a time.
                        Err(_) => match short_date(&text) {
                            Some([_, _, _, hour, minute, second]) => {
                                Some((u64::from(hour), u64::from(minute), u64::from(second)))
                            }
                            None => Some(text_time(&text).ok_or(ExecError::InvalidDateTime)?),
                        },
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

/// EXTRACT carries a duration's sign across every requested clock field.
/// A calendar input contributes its day; a duration folds days into hours.
pub(super) fn extract_time(
    value: &Value,
    leading: DatePart,
    trailing: DatePart,
) -> Result<i64, ExecError> {
    let text = scalar_string(value)?;
    let text = text.trim();
    let whole = text.split_once('.').map_or(text, |(whole, _)| whole);
    let unsigned = whole.trim_start_matches(['-', '+']);
    let compact = unsigned.bytes().all(|byte| byte.is_ascii_digit());
    // Numeric TIME conversion rejects overflow; textual TIME conversion
    // clamps it. Neither interprets an eight-digit number as a date.
    if compact && unsigned.len() < 12 && matches!(value, Value::Int64(_) | Value::UInt64(_)) {
        let number = unsigned
            .parse::<u128>()
            .map_err(|_| ExecError::InvalidDateTime)?;
        packed_time(number).ok_or(ExecError::InvalidDateTime)?;
    }
    let calendar = if compact && unsigned.len() >= 12 {
        Some(
            parse_mysql_datetime(text)?
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        )
    } else if super::canonical_temporal_parts(text, true).is_none() {
        // A date with short parts, `'1-2-3'`, is 2001-02-03 at midnight - a
        // calendar, not a duration of one day and change.
        short_date(text).map(|[year, month, day, hour, minute, second]| {
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
        })
    } else {
        None
    };
    let text = calendar.as_deref().unwrap_or(text);
    let (day, clock) =
        super::canonical_temporal_parts(text, true).map_or((0, text), |(date, clock)| {
            (
                date[8..10].parse::<i64>().unwrap_or(0),
                clock.unwrap_or("00:00:00"),
            )
        });
    let time = super::parse_temporal_micros(clock).ok_or(ExecError::InvalidDateTime)?;
    let seconds =
        i64::try_from(time.micros.abs() / 1_000_000).map_err(|_| ExecError::NumericOverflow)?;
    let fields = [day, seconds / 3_600, seconds / 60 % 60, seconds % 60];
    let index = |part| match part {
        DatePart::Day => 0,
        DatePart::Hour => 1,
        DatePart::Minute => 2,
        _ => 3,
    };
    let number = fields[index(leading)..=index(trailing)]
        .iter()
        .fold(0, |total, part| total * 100 + part);
    Ok(if time.micros < 0 { -number } else { number })
}

/// Pack fractional seconds after a signed clock extraction. The fraction is
/// always six decimal places in a composite result.
pub(super) fn extract_micros(value: &Value, leading: Option<DatePart>) -> Result<i64, ExecError> {
    let text = scalar_string(value)?;
    let text = text.trim();
    let clock = super::canonical_temporal_parts(text, true)
        .map_or(text, |(_, clock)| clock.unwrap_or("00:00:00"));
    let micros = super::parse_temporal_micros(clock)
        .ok_or(ExecError::InvalidDateTime)?
        .micros;
    let fraction =
        i64::try_from(micros.abs() % 1_000_000).map_err(|_| ExecError::NumericOverflow)?;
    let Some(leading) = leading else {
        return Ok(fraction);
    };
    let whole = extract_time(value, leading, DatePart::Second)?;
    let packed = whole
        .abs()
        .checked_mul(1_000_000)
        .and_then(|whole| whole.checked_add(fraction))
        .ok_or(ExecError::NumericOverflow)?;
    Ok(if micros < 0 || text.starts_with('-') {
        -packed
    } else {
        packed
    })
}

/// HHMMSS packed into an integer, up to the largest TIME.
fn packed_time(number: u128) -> Option<(u64, u64, u64)> {
    let minutes = u64::try_from(number / 100 % 100).ok()?;
    let seconds = u64::try_from(number % 100).ok()?;
    (number <= 8_385_959 && minutes < 60 && seconds < 60)
        .then(|| Some((u64::try_from(number / 10_000).ok()?, minutes, seconds)))
        .flatten()
}

/// A time written `[-][days ]H:MM[:SS[.fraction]]`; days contribute 24 hours.
fn text_time(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.trim_start_matches(['-', '+']);
    let field = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let (days, clock) = match text.split_once(' ') {
        Some((days, clock)) => (field(days)?, clock.trim_start()),
        None => (0, text),
    };
    let mut fields = clock.split(':');
    let hours = days
        .saturating_mul(24)
        .saturating_add(field(fields.next()?)?);
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
    // A colon-only triple is a TIME here. A separate clock disambiguates
    // a date whose punctuation also happens to be colons.
    if year_end == 0
        || year_end > 4
        || separator == '-'
        || (separator == ':' && !value.contains([' ', 'T']))
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

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    use super::{canonical_datetime, canonical_datetime_text, convert_tz_bounded};

    #[test]
    fn canonical_datetime_text_reads_as_the_general_parser_does() {
        for text in [
            "2026-09-29 15:00:00",
            "2026-09-29 15:00:00.5",
            "2026-09-29 15:00:00.123456",
            "0001-01-01 00:00:00",
            "9999-12-31 23:59:59.999999",
            "2024-02-29 12:34:56.07",
        ] {
            assert_eq!(
                canonical_datetime(text),
                NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").ok(),
                "{text}"
            );
            let value = canonical_datetime(text).expect("canonical");
            assert_eq!(
                canonical_datetime_text(value),
                Some(value.format("%Y-%m-%d %H:%M:%S").to_string())
            );
        }
        // Everything else is left to the general parser.
        for text in [
            "2026-02-30 00:00:00",
            "2026-9-29 15:00:00",
            "2026-09-29 15:00:60",
            "2026-09-29 15:00:00.",
            "2026-09-29 15:00:00.1234567",
            "2026-09-29T15:00:00",
            "2026-09-29",
            "2026-09-29 15:00:0x",
        ] {
            assert_eq!(canonical_datetime(text), None, "{text}");
        }
    }

    #[test]
    fn converting_between_offsets_keeps_its_answers() {
        assert_eq!(
            convert_tz_bounded("2026-09-29 20:00:00", "+00:00", "+05:30").as_deref(),
            Some("2026-09-30 01:30:00")
        );
        assert_eq!(
            convert_tz_bounded("2026-09-29 20:00:00.25", "+00:00", "-03:00").as_deref(),
            Some("2026-09-29 17:00:00.25")
        );
        assert_eq!(
            convert_tz_bounded("2026-09-29", "+00:00", "+01:00").as_deref(),
            Some("2026-09-29 01:00:00")
        );
        assert_eq!(
            convert_tz_bounded("2026-02-30 00:00:00", "+00:00", "+01:00"),
            None
        );
    }
}
