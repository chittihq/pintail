//! Date and time kernels over packed temporal units: parts, interval
//! arithmetic, dates of a moment, differences, and casts between temporal
//! types. Row evaluation reads a temporal as its canonical text and parses
//! it back; these read the units the text is derived from, and decline
//! where the text would not be canonical.

use chrono::{NaiveDateTime, Timelike as _};
use pintail_sql::{DatePart, IntervalUnit, ScalarFunction};
use pintail_types::{DataType, Value};

use super::{Effects, Operand, operand};
use crate::array::ValidityMask;
use crate::batch::{ColumnVector, LazyText, RecordBatch, TypedValues};
use crate::execution::ExecError;
use crate::expression::CompiledExpr;
use crate::expression::temporal::{apply_interval, date_part};

mod repeated;

pub(super) use repeated::repeated_column;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A temporal column's packed units, when its text is derived from them.
pub(super) struct Temporal<'batch> {
    units: &'batch [i64],
    pub(super) validity: &'batch ValidityMask,
    /// `None` for a date (units are days), the precision for a datetime
    /// (units are microseconds).
    fsp: Option<u8>,
}

impl Temporal<'_> {
    /// Row `row` as the date-time row evaluation parses from its text.
    fn datetime(&self, row: usize) -> Option<NaiveDateTime> {
        let micros = match self.fsp {
            None => self.units[row].checked_mul(MICROS_PER_DAY)?,
            Some(_) => self.spelled(row),
        };
        Some(chrono::DateTime::from_timestamp_micros(micros)?.naive_utc())
    }

    /// Row `row`'s units at the precision its text spells: the text
    /// carries `fsp` fraction digits, so that is all a parse of it
    /// recovers.
    pub(super) fn spelled(&self, row: usize) -> i64 {
        let unit = self.units[row];
        match self.fsp {
            None => unit,
            Some(fsp) => unit - unit.rem_euclid(fraction_step(fsp)),
        }
    }

    /// The least and greatest unit of a column with no NULL row, where
    /// every row's unit is a value; `None` for an empty column or one with
    /// a NULL, whose NULL rows hold no instant to bound.
    fn bounds(&self) -> Option<(i64, i64)> {
        if !self.validity.no_nulls() {
            return None;
        }
        Some((
            pintail_simd::min_i64(self.units)?,
            pintail_simd::max_i64(self.units)?,
        ))
    }

    /// The units whose canonical text has a four-digit year, where text
    /// order is time order.
    pub(super) fn four_digit_years(&self) -> std::ops::RangeInclusive<i64> {
        let day = |year, month, day| {
            chrono::NaiveDate::from_ymd_opt(year, month, day).map_or(0, |date| {
                date.signed_duration_since(chrono::NaiveDate::default())
                    .num_days()
            })
        };
        let (first, last) = (day(0, 1, 1), day(9999, 12, 31));
        match self.fsp {
            None => first..=last,
            Some(_) => first * MICROS_PER_DAY..=(last + 1) * MICROS_PER_DAY - 1,
        }
    }
}

/// The microseconds one step of a datetime's `fsp` fraction digits spans.
const fn fraction_step(fsp: u8) -> i64 {
    match fsp {
        0 => 1_000_000,
        1 => 100_000,
        2 => 10_000,
        3 => 1_000,
        4 => 100,
        5 => 10,
        _ => 1,
    }
}

/// `units` floored to whole multiples of `STEP`, each plus `offset`: the
/// loop of a reading at one precision, its divisor a constant so the
/// remainder is a multiply and the whole-microsecond case no arithmetic
/// but the add.
fn spelled_plus<const STEP: i64>(units: &[i64], offset: i64, out: &mut Vec<i64>) {
    out.extend(
        units
            .iter()
            .map(|unit| (unit - unit.rem_euclid(STEP)).wrapping_add(offset)),
    );
}

/// A column's packed temporal units, when it has them.
pub(super) fn temporal_column(column: &ColumnVector) -> Option<Temporal<'_>> {
    let fsp = match column.data_type() {
        DataType::Date32 => None,
        DataType::DateTime64 { fsp } => Some(fsp),
        _ => return None,
    };
    let (TypedValues::Temporal { units, text }, validity) = column.typed()? else {
        return None;
    };
    // The text's spelling is not read here, only the units, and they are
    // the exact instant wherever a packed temporal is built: every
    // construction parses each row with the strict parser and abandons the
    // packed column when any row fails. A column read from storage keeps
    // the text as written, so requiring derived text kept every stored
    // DATE and DATETIME off these kernels.
    let _ = text;
    Some(Temporal {
        units,
        validity,
        fsp,
    })
}

/// Whether canonical text can spell `value`'s year, as derived text must.
fn spellable(value: NaiveDateTime) -> bool {
    use chrono::Datelike as _;
    (0..=9999).contains(&value.year())
}

/// One date-time argument of a batch kernel: a packed temporal column, or
/// a constant row evaluation would parse the same way on every row.
enum Moment<'batch> {
    Column(Temporal<'batch>),
    Fixed(NaiveDateTime),
}

/// One row of a [`Moment`].
enum At {
    Null,
    Value(NaiveDateTime),
}

impl Moment<'_> {
    /// Row `row`'s date-time, or `None` for a row the kernel cannot mirror.
    fn at(&self, row: usize) -> Option<At> {
        match self {
            Self::Fixed(value) => Some(At::Value(*value)),
            Self::Column(column) if !column.validity.is_valid(row) => Some(At::Null),
            Self::Column(column) => column.datetime(row).map(At::Value),
        }
    }
}

fn moment<'operand>(operand: &'operand Operand<'_>) -> Option<Moment<'operand>> {
    match operand {
        Operand::Column(column) => temporal_column(column).map(Moment::Column),
        Operand::Constant(Value::Utf8(text)) => {
            crate::expression::temporal::parse_mysql_datetime(text)
                .ok()
                .map(Moment::Fixed)
        }
        Operand::Constant(_) => None,
    }
}

/// `DATE(x)` and `LAST_DAY(x)`: a date from a packed temporal.
/// `DATE_FORMAT(column, 'constant')` over packed units.
///
/// Row evaluation forces the column's text, parses it back into a
/// date-time, and formats that - the round trip e14 measured as `YEAR()`'s
/// cost on the same columns. The units are already the date-time the parse
/// would recover, so this formats straight from them. Only a constant
/// format qualifies: a per-row format is row evaluation's.
pub(super) fn date_format_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    // The planner appends the session's calendar locale, which names the
    // days and months; a time argument takes a statement date as well,
    // and that is row evaluation's.
    // Last comes the binder's zero-date policy (signed), which decides
    // only how a date written as text reads; a packed temporal holds a
    // real instant in every row, so no row here depends on it.
    let args = match args {
        [rest @ .., CompiledExpr::Literal(Value::Int64(_))] if rest.len() >= 2 => rest,
        args => args,
    };
    let (argument, format, locale) = match args {
        [argument, format] => (argument, format, 0),
        [
            argument,
            format,
            CompiledExpr::Literal(Value::UInt64(locale)),
        ] => (argument, format, usize::try_from(*locale).unwrap_or(0)),
        _ => return None,
    };
    let locale = crate::calendar_locale::locale(locale);
    if data_type.is_some_and(|declared| declared != DataType::Utf8) {
        return None;
    }
    let CompiledExpr::Literal(Value::Utf8(format)) = format else {
        return None;
    };
    let input = operand(batch, argument, effects)?;
    let Operand::Column(column) = &input else {
        return None;
    };
    let units = temporal_column(column)?;
    if let Some(step) = format_step(format, units.fsp) {
        return coded_date_format(batch, &units, format, locale, step);
    }
    let mut text = crate::array::StrColumn::default();
    for row in 0..batch.row_count() {
        if !units.validity.is_valid(row) {
            text.push(b"");
            continue;
        }
        let formatted = crate::expression::temporal::mysql_date_format_locale(
            units.datetime(row)?,
            format,
            locale,
        );
        text.push(formatted.as_bytes());
    }
    Some(ColumnVector::from_typed(
        DataType::Utf8,
        TypedValues::Utf8(text),
        units.validity.clone(),
    ))
}

/// A source `TIMESTAMP` read in a session zone, `SessionTimestamp(column,
/// '+05:30')` or `SessionTimestamp(column, 'Europe/Paris')`, over packed
/// units.
///
/// The stored units are UTC and the session reads each one shifted by the
/// zone's offset at that instant, so the reading is the units plus the
/// offset: what row evaluation reaches by spelling each row, converting the
/// text and spelling it again. With a column form here, `DATE()`, `HOUR()`
/// or `DATE_FORMAT()` of the reading keep their own packed kernels instead
/// of declining a whole key to row evaluation.
///
/// A fixed zone has one offset. A named zone's offset changes at its
/// transitions (daylight saving, a change of standard time), so it is
/// looked up per instant - but instants arrive in runs that share a UTC
/// second, and a column in time order asks about the same second many
/// times, so the lookup is repeated only when the second changes. A
/// reading whose year canonical text cannot spell declines.
pub(super) fn session_timestamp_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    use crate::expression::temporal::ZoneReading;
    let [argument, CompiledExpr::Literal(Value::Utf8(zone))] = args else {
        return None;
    };
    let zone = ZoneReading::of(zone)?;
    let input = operand(batch, argument, effects)?;
    let Operand::Column(column) = &input else {
        return None;
    };
    let units = temporal_column(column)?;
    let fsp = units.fsp?;
    if data_type.is_some_and(|declared| declared != DataType::DateTime64 { fsp }) {
        return None;
    }
    let spellable = units.four_digit_years();
    let mut shifted = Vec::with_capacity(units.units.len());
    match zone {
        ZoneReading::Fixed(seconds) => {
            let offset = i64::from(seconds) * 1_000_000;
            // Flooring to the precision and adding the offset both keep
            // order, so a column with no NULL is spellable throughout when
            // its least and greatest readings are - and then no row needs
            // a check of its own.
            if let Some((least, greatest)) = units.bounds() {
                let step = fraction_step(fsp);
                for unit in [least, greatest] {
                    let reading = (unit - unit.rem_euclid(step)).checked_add(offset)?;
                    if !spellable.contains(&reading) {
                        return None;
                    }
                }
                match fsp {
                    0 => spelled_plus::<1_000_000>(units.units, offset, &mut shifted),
                    1 => spelled_plus::<100_000>(units.units, offset, &mut shifted),
                    2 => spelled_plus::<10_000>(units.units, offset, &mut shifted),
                    3 => spelled_plus::<1_000>(units.units, offset, &mut shifted),
                    4 => spelled_plus::<100>(units.units, offset, &mut shifted),
                    5 => spelled_plus::<10>(units.units, offset, &mut shifted),
                    _ => spelled_plus::<1>(units.units, offset, &mut shifted),
                }
            }
            for row in shifted.len()..units.units.len() {
                if !units.validity.is_valid(row) {
                    shifted.push(0);
                    continue;
                }
                let reading = units.spelled(row).checked_add(offset)?;
                if !spellable.contains(&reading) {
                    return None;
                }
                shifted.push(reading);
            }
        }
        ZoneReading::Named(_) => {
            // The second last looked up and its offset in microseconds.
            let mut last: Option<(i64, i64)> = None;
            for row in 0..units.units.len() {
                if !units.validity.is_valid(row) {
                    shifted.push(0);
                    continue;
                }
                let spelled = units.spelled(row);
                let second = spelled.div_euclid(1_000_000);
                let offset = match last {
                    Some((known, offset)) if known == second => offset,
                    _ => {
                        let offset = i64::from(zone.seconds_east(second)?) * 1_000_000;
                        last = Some((second, offset));
                        offset
                    }
                };
                let reading = spelled.checked_add(offset)?;
                if !spellable.contains(&reading) {
                    return None;
                }
                shifted.push(reading);
            }
        }
    }
    Some(ColumnVector::from_typed(
        DataType::DateTime64 { fsp },
        TypedValues::Temporal {
            units: shifted,
            text: LazyText::datetime(fsp),
        },
        units.validity.clone(),
    ))
}

/// No zone reads an instant a day or more from UTC: an offset is held as a
/// fixed offset, which is strictly inside a day either way.
const WIDEST_OFFSET_MICROS: i64 = MICROS_PER_DAY;

/// `SessionTimestamp(column, zone) op literal` as a mask, looking the
/// zone's offset up only for the rows it could decide.
///
/// A reading is its instant plus an offset of less than a day, so an
/// instant more than a day clear of the literal compares with it the way
/// its reading does, whatever the offset - and a filter for a month of
/// rows leaves most of a table that far from both its ends. Only the rows
/// within a day of the literal are read through the zone, each exactly as
/// [`session_timestamp_column`] reads it.
///
/// The answer is the packed comparison's over that column: it takes the
/// same literals (canonical, with the column's own fraction digits) and
/// declines where that kernel would - here, wherever some row's reading
/// could leave the years canonical text spells. `None` also for any other
/// expression or operator.
pub(super) fn session_comparison_mask(
    batch: &RecordBatch,
    reading: &CompiledExpr,
    op: pintail_sql::BinaryOp,
    literal: &Value,
) -> Option<crate::SelectionMask> {
    use pintail_sql::BinaryOp;

    use crate::expression::temporal::ZoneReading;
    let CompiledExpr::Scalar {
        function: ScalarFunction::SessionTimestamp,
        args,
        data_type,
        ..
    } = reading
    else {
        return None;
    };
    let [
        CompiledExpr::Column(column),
        CompiledExpr::Literal(Value::Utf8(zone)),
    ] = args.as_slice()
    else {
        return None;
    };
    let zone = ZoneReading::of(zone)?;
    let units = temporal_column(batch.column(*column)?)?;
    let fsp = units.fsp?;
    if data_type.is_some_and(|declared| declared != DataType::DateTime64 { fsp }) {
        return None;
    }
    let Value::Utf8(text) = literal else {
        return None;
    };
    let expected_len = if fsp == 0 { 19 } else { 20 + usize::from(fsp) };
    if text.len() != expected_len {
        return None;
    }
    let literal = crate::batch::parse_datetime_micros(text)?;
    if !matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual
    ) {
        return None;
    }
    // Every reading lies strictly between its instant less a precision step
    // and a day, and its instant plus a day. With those bounds inside the
    // spellable years for the least and greatest unit - a NULL row's
    // included, whatever it holds - no row's reading can leave them.
    let step = fraction_step(fsp);
    let spellable = units.four_digit_years();
    let least = pintail_simd::min_i64(units.units)?;
    let greatest = pintail_simd::max_i64(units.units)?;
    let below = least.checked_sub(step + WIDEST_OFFSET_MICROS)?;
    let above = greatest.checked_add(WIDEST_OFFSET_MICROS)?;
    if !spellable.contains(&below) || !spellable.contains(&above) {
        return None;
    }
    let compare = move |reading: i64| match op {
        BinaryOp::Equal => reading == literal,
        BinaryOp::NotEqual => reading != literal,
        BinaryOp::Less => reading < literal,
        BinaryOp::LessOrEqual => reading <= literal,
        BinaryOp::Greater => reading > literal,
        _ => reading >= literal,
    };
    // Set if an instant inside the spellable years had no offset, which the
    // calendar rules out: the mask is then abandoned rather than trusted.
    let unread = std::sync::atomic::AtomicBool::new(false);
    let mask = crate::expression::selection::select_words(units.units, units.validity, |unit| {
        // `low < reading < high`, both exclusive.
        let low = unit - step - WIDEST_OFFSET_MICROS;
        let high = unit + WIDEST_OFFSET_MICROS;
        if low >= literal {
            // The reading is above the literal.
            return matches!(
                op,
                BinaryOp::NotEqual | BinaryOp::Greater | BinaryOp::GreaterOrEqual
            );
        }
        if high <= literal {
            // The reading is below the literal.
            return matches!(
                op,
                BinaryOp::NotEqual | BinaryOp::Less | BinaryOp::LessOrEqual
            );
        }
        let spelled = unit - unit.rem_euclid(step);
        let Some(seconds) = zone.seconds_east(spelled.div_euclid(1_000_000)) else {
            unread.store(true, std::sync::atomic::Ordering::Relaxed);
            return false;
        };
        compare(spelled + i64::from(seconds) * 1_000_000)
    });
    (!unread.into_inner()).then_some(mask)
}

/// Whether `expr` reads a source `TIMESTAMP` in a named session zone: the
/// reason reported when a key over one has no packed column after all.
pub(super) fn reads_named_session_zone(expr: &CompiledExpr) -> bool {
    match expr {
        CompiledExpr::Scalar {
            function: ScalarFunction::SessionTimestamp,
            args,
            ..
        } => match args.as_slice() {
            [_, CompiledExpr::Literal(Value::Utf8(zone))] => {
                crate::expression::temporal::fixed_zone_seconds(zone).is_none()
            }
            _ => false,
        },
        CompiledExpr::Scalar { args, .. } => args.iter().any(reads_named_session_zone),
        _ => false,
    }
}

/// The units a constant `DATE_FORMAT` pattern cannot see within: a day
/// when it names only calendar fields, an hour when it adds hour fields,
/// `None` when it reads minutes or finer. Unknown directives print their
/// own character, so they read nothing; every other directive is listed
/// here by what it reads, and anything unlisted keeps the per-row format.
fn format_step(format: &str, fsp: Option<u8>) -> Option<i64> {
    let mut hourly = false;
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            continue;
        }
        match characters.next() {
            None => break,
            Some(
                'a' | 'b' | 'c' | 'D' | 'd' | 'e' | 'j' | 'M' | 'm' | 'U' | 'u' | 'V' | 'v' | 'W'
                | 'w' | 'X' | 'x' | 'Y' | 'y' | '%',
            ) => {}
            Some('H' | 'h' | 'I' | 'k' | 'l' | 'p') => hourly = true,
            Some(other) if other.is_ascii_alphabetic() => return None,
            Some(_) => {}
        }
    }
    Some(match (fsp, hourly) {
        // A date's units are days, and its hour is always midnight.
        (None, _) => 1,
        (Some(_), false) => MICROS_PER_DAY,
        (Some(_), true) => 3_600_000_000,
    })
}

/// `DATE_FORMAT` of a pattern blind within `step` units: formatted once per
/// distinct step, the column coded by it. A recent window of events holds
/// a few dozen days, so a million rows format a few dozen strings, and
/// what groups or compares the answer next reads its codes.
fn coded_date_format(
    batch: &RecordBatch,
    units: &Temporal<'_>,
    format: &str,
    locale: &crate::calendar_locale::CalendarLocale,
    step: i64,
) -> Option<ColumnVector> {
    let rows = batch.row_count();
    let mut codes = Vec::with_capacity(rows);
    let mut heap = Vec::<u8>::new();
    let mut offsets = vec![0_usize];
    let mut by_step = std::collections::HashMap::<i64, u32>::new();
    let mut last: Option<(i64, u32)> = None;
    for row in 0..rows {
        if !units.validity.is_valid(row) {
            codes.push(0);
            continue;
        }
        let bucket = units.units[row].div_euclid(step);
        let code = match last {
            Some((previous, code)) if previous == bucket => code,
            _ => {
                let code = if let Some(code) = by_step.get(&bucket) {
                    *code
                } else {
                    let code = u32::try_from(offsets.len() - 1).ok()?;
                    let formatted = crate::expression::temporal::mysql_date_format_locale(
                        units.datetime(row)?,
                        format,
                        locale,
                    );
                    heap.extend_from_slice(formatted.as_bytes());
                    offsets.push(heap.len());
                    by_step.insert(bucket, code);
                    code
                };
                last = Some((bucket, code));
                code
            }
        };
        codes.push(code);
    }
    if offsets.len() == 1 {
        // Every row NULL: a one-entry dictionary keeps each code in range.
        offsets.push(0);
    }
    let text =
        crate::array::StrColumn::from_dictionary(&heap, &offsets, codes, units.validity.clone());
    Some(ColumnVector::from_typed(
        DataType::Utf8,
        TypedValues::Utf8(text),
        units.validity.clone(),
    ))
}

pub(super) fn date_of_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    function: ScalarFunction,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    // The binder appends the session's zero-date policy to `DATE(x)`. It
    // decides only how a zero or partial calendar casts, and a packed
    // temporal holds neither: every row parsed strictly into a real
    // instant, so the policy cannot change any row this kernel answers.
    let ([argument] | [argument, CompiledExpr::Literal(Value::UInt64(_))]) = args else {
        return None;
    };
    if data_type != Some(DataType::Date32) {
        return None;
    }
    let input = operand(batch, argument, effects)?;
    if !input.varies() {
        return None;
    }
    dates_of(batch, &input, function)
}

fn dates_of(
    batch: &RecordBatch,
    input: &Operand<'_>,
    function: ScalarFunction,
) -> Option<ColumnVector> {
    use chrono::Datelike as _;
    let input = moment(input)?;
    if function == ScalarFunction::Date
        && let Moment::Column(column) = &input
    {
        return days_of(column);
    }
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    let mut days = Vec::with_capacity(batch.row_count());
    let mut valid = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        let At::Value(value) = input.at(row)? else {
            days.push(0);
            valid.push(false);
            continue;
        };
        let date = value.date();
        let date = if function == ScalarFunction::LastDay {
            let first_next = if date.month() == 12 {
                chrono::NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
            } else {
                chrono::NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
            };
            // Row evaluation answers NULL where the calendar gives out.
            let Some(last) = first_next.and_then(|first| first.pred_opt()) else {
                days.push(0);
                valid.push(false);
                continue;
            };
            last
        } else {
            date
        };
        if !(0..=9999).contains(&date.year()) {
            return None;
        }
        days.push(date.signed_duration_since(epoch).num_days());
        valid.push(true);
    }
    Some(ColumnVector::from_typed(
        DataType::Date32,
        TypedValues::Temporal {
            units: days,
            text: LazyText::date(),
        },
        ValidityMask::from_bools(&valid),
    ))
}

/// `DATE(column)` straight off the units: a datetime's day is its
/// microseconds floored to whole days since the epoch, the date the
/// calendar conversion arrives at, without one. Grouping a recent window by
/// day reads this for every row, where the conversion cost more than the
/// rest of the aggregate. A unit whose year canonical text cannot spell
/// declines, as the calendar path does.
fn days_of(column: &Temporal<'_>) -> Option<ColumnVector> {
    let spellable = column.four_digit_years();
    let mut days = Vec::with_capacity(column.units.len());
    // With no NULL row the least and greatest unit bound every row, and
    // the loop is one division a row with nothing to branch on.
    if let Some((least, greatest)) = column.bounds() {
        if !spellable.contains(&least) || !spellable.contains(&greatest) {
            return None;
        }
        match column.fsp {
            None => days.extend_from_slice(column.units),
            Some(_) => days.extend(
                column
                    .units
                    .iter()
                    .map(|unit| unit.div_euclid(MICROS_PER_DAY)),
            ),
        }
    }
    for (row, unit) in column.units.iter().enumerate().skip(days.len()) {
        if !column.validity.is_valid(row) {
            days.push(0);
            continue;
        }
        if !spellable.contains(unit) {
            return None;
        }
        days.push(match column.fsp {
            None => *unit,
            Some(_) => unit.div_euclid(MICROS_PER_DAY),
        });
    }
    Some(ColumnVector::from_typed(
        DataType::Date32,
        TypedValues::Temporal {
            units: days,
            text: LazyText::date(),
        },
        column.validity.clone(),
    ))
}

/// `DATEDIFF(a, b)` and `TIMESTAMPDIFF(unit, from, to)` over packed
/// temporals and constants.
pub(super) fn difference_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    function: ScalarFunction,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [left, right] = args else {
        return None;
    };
    if data_type != Some(DataType::Int64) {
        return None;
    }
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    // At least one side varies by row, or there is nothing to vectorize.
    if !left.varies() && !right.varies() {
        return None;
    }
    differences_of(batch, &left, &right, function)
}

fn differences_of(
    batch: &RecordBatch,
    left: &Operand<'_>,
    right: &Operand<'_>,
    function: ScalarFunction,
) -> Option<ColumnVector> {
    let (left, right) = (moment(left)?, moment(right)?);
    let mut differences = Vec::with_capacity(batch.row_count());
    let mut valid = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        let (At::Value(from), At::Value(to)) = (left.at(row)?, right.at(row)?) else {
            differences.push(0);
            valid.push(false);
            continue;
        };
        differences.push(match function {
            ScalarFunction::TimestampDiff { unit } => {
                crate::expression::temporal::timestamp_diff(from, to, unit)
            }
            _ => {
                crate::expression::temporal::mysql_daynr(from.date())
                    - crate::expression::temporal::mysql_daynr(to.date())
            }
        });
        valid.push(true);
    }
    Some(ColumnVector::from_typed(
        DataType::Int64,
        TypedValues::Int64(differences),
        ValidityMask::from_bools(&valid),
    ))
}

/// `YEAR(x)`, `MONTH(x)` and the other single parts of a packed temporal.
pub(super) fn date_part_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    part: DatePart,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [argument] = args else {
        return None;
    };
    // Row evaluation answers a plain integer.
    let declared = data_type.unwrap_or(DataType::Int64);
    if declared.storage_type() != DataType::Int64 {
        return None;
    }
    let Operand::Column(input) = operand(batch, argument, effects)? else {
        return None;
    };
    parts_of(&input, part, declared)
}

fn parts_of(input: &ColumnVector, part: DatePart, declared: DataType) -> Option<ColumnVector> {
    let input = temporal_column(input)?;
    // The units within which the part cannot change: a clock part reads
    // only its own unit of the day, every other part only the date. Rows
    // of a window ordered by time repeat the previous row's step, so the
    // calendar conversion runs once per step rather than once per row.
    let step = match (input.fsp, part) {
        (None, _) => 1,
        (Some(_), DatePart::Hour) => 3_600_000_000,
        (Some(_), DatePart::Minute) => 60_000_000,
        (Some(_), DatePart::Second) => 1_000_000,
        (Some(_), _) => MICROS_PER_DAY,
    };
    let mut last: Option<(i64, i64)> = None;
    let mut parts = Vec::with_capacity(input.units.len());
    let mut valid = Vec::with_capacity(input.units.len());
    for row in 0..input.units.len() {
        if !input.validity.is_valid(row) {
            parts.push(0);
            valid.push(false);
            continue;
        }
        let bucket = input.units[row].div_euclid(step);
        let value = match last {
            Some((previous, value)) if previous == bucket => value,
            _ => {
                let value = i64::try_from(date_part(input.datetime(row)?, part)).ok()?;
                last = Some((bucket, value));
                value
            }
        };
        parts.push(value);
        valid.push(true);
    }
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Int64(parts),
        ValidityMask::from_bools(&valid),
    ))
}

/// `DATE_ADD`/`DATE_SUB` of a packed temporal and a constant amount.
pub(super) fn date_interval_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    unit: IntervalUnit,
    subtract: bool,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [argument, CompiledExpr::Literal(amount)] = args else {
        return None;
    };
    // A NULL amount makes every row NULL; that is row evaluation's to say.
    if matches!(amount, Value::Null) {
        return None;
    }
    // A malformed constant can warn during coercion. A packed probe that
    // declines must leave that warning to the path that reads each row.
    let amount = crate::execution::without_new_warnings(|| {
        if unit == IntervalUnit::Second
            && crate::expression::interval_second_micros(amount).ok()? % 1_000_000 != 0
        {
            return None;
        }
        crate::expression::mysql_i64(amount).ok()
    })??;
    let Operand::Column(input) = operand(batch, argument, effects)? else {
        return None;
    };
    shifted(batch, &input, amount, unit, subtract, data_type)
}

fn shifted(
    batch: &RecordBatch,
    input: &ColumnVector,
    amount: i64,
    unit: IntervalUnit,
    subtract: bool,
    data_type: Option<DataType>,
) -> Option<ColumnVector> {
    let input = temporal_column(input)?;
    // Row evaluation answers a date where a date moves by whole days or
    // more, and a datetime otherwise, at the declared precision.
    let date_only = input.fsp.is_none()
        && matches!(
            unit,
            IntervalUnit::Year | IntervalUnit::Month | IntervalUnit::Day
        );
    let out_fsp = match (date_only, data_type) {
        (true, Some(DataType::Date32)) => None,
        (false, Some(DataType::DateTime64 { fsp })) => Some(fsp),
        _ => return None,
    };
    let selection = batch.selection();
    let mut units = Vec::with_capacity(input.units.len());
    let mut valid = Vec::with_capacity(input.units.len());
    for row in 0..input.units.len() {
        if !input.validity.is_valid(row) {
            units.push(0);
            valid.push(false);
            continue;
        }
        let value = input.datetime(row)?;
        // Days and clock units from or into the year zero answer MySQL's
        // zero date, which packed units cannot spell: row evaluation's.
        let by_month = matches!(unit, IntervalUnit::Year | IntervalUnit::Month);
        let in_year_zero = |moment: NaiveDateTime| chrono::Datelike::year(&moment) == 0;
        if !by_month && in_year_zero(value) {
            return None;
        }
        let shifted = match apply_interval(value, amount, unit, subtract) {
            Ok(shifted) => shifted,
            // Row evaluation answers an impossible date-time with NULL.
            Err(ExecError::InvalidDateTime) => {
                units.push(0);
                valid.push(false);
                continue;
            }
            // Row evaluation raises this for a row it reads; the batch goes
            // to it, so the error is its own.
            Err(_) if selection.is_selected(row) => return None,
            Err(_) => {
                units.push(0);
                valid.push(false);
                continue;
            }
        };
        if !spellable(shifted) || (!by_month && in_year_zero(shifted)) {
            return None;
        }
        let unit = match out_fsp {
            None => shifted.and_utc().timestamp().div_euclid(86_400),
            Some(fsp) => {
                let micros = shifted.and_utc().timestamp_micros();
                // The answer keeps the fraction digits its type declares.
                let step = 10_i64.pow(6 - u32::from(fsp.min(6)));
                debug_assert_eq!(shifted.nanosecond() % 1_000, 0);
                micros - micros.rem_euclid(step)
            }
        };
        units.push(unit);
        valid.push(true);
    }
    let (declared, text) = match out_fsp {
        None => (DataType::Date32, LazyText::date()),
        Some(fsp) => (DataType::DateTime64 { fsp }, LazyText::datetime(fsp)),
    };
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Temporal { units, text },
        ValidityMask::from_bools(&valid),
    ))
}

/// `CAST(x AS DATETIME(n))` of a date, or of a date-time at no more
/// precision than `n`: the same instant, spelled with `n` fraction digits.
/// Comparisons between temporal types are bound as these casts, so both
/// sides meet as one type.
pub(super) fn cast_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    target: DataType,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [argument, rest @ ..] = args else {
        return None;
    };
    if !matches!(rest, [] | [CompiledExpr::Literal(Value::UInt64(_))]) {
        return None;
    }
    let DataType::DateTime64 { fsp: out_fsp } = target else {
        return None;
    };
    if data_type.is_some_and(|declared| declared != target) {
        return None;
    }
    let Operand::Column(input) = operand(batch, argument, effects)? else {
        return None;
    };
    let input = temporal_column(&input)?;
    let scale = match input.fsp {
        None => MICROS_PER_DAY,
        Some(fsp) if fsp <= out_fsp => 1,
        Some(_) => return None,
    };
    let mut units = Vec::with_capacity(input.units.len());
    for (row, unit) in input.units.iter().enumerate() {
        units.push(if input.validity.is_valid(row) {
            unit.checked_mul(scale)?
        } else {
            0
        });
    }
    Some(ColumnVector::from_typed(
        target,
        TypedValues::Temporal {
            units,
            text: LazyText::datetime(out_fsp),
        },
        input.validity.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use pintail_sql::{DatePart, IntervalUnit, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, scalar};
    use super::{CompiledExpr, MICROS_PER_DAY};
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, LazyText, RecordBatch, SelectionMask, TypedValues};

    /// Calendar edges and ordinary dates, with a NULL.
    const DATES: [Option<&str>; 12] = [
        Some("0000-01-01"),
        Some("0001-12-31"),
        Some("1970-01-01"),
        Some("1999-12-31"),
        Some("2000-02-29"),
        Some("2023-01-31"),
        Some("2023-03-31"),
        Some("2024-02-29"),
        Some("2024-12-31"),
        None,
        Some("9999-12-01"),
        Some("9999-12-31"),
    ];

    const TIMES: [&str; 4] = [
        "00:00:00",
        "12:34:56.789012",
        "23:59:59.999999",
        "05:06:07.1",
    ];

    /// Dates no tested interval can carry out of what canonical text spells.
    const ORDINARY: [Option<&str>; 8] = [
        Some("1970-01-01"),
        Some("1999-12-31"),
        Some("2000-02-29"),
        Some("2023-01-31"),
        Some("2023-03-31"),
        None,
        Some("2024-02-29"),
        Some("2024-12-31"),
    ];

    fn temporal(fsp: Option<u8>) -> ColumnVector {
        temporal_from(&DATES, fsp)
    }

    /// A packed temporal column, as a scan produces it: units, and text
    /// derived from them.
    fn temporal_from(dates: &[Option<&str>], fsp: Option<u8>) -> ColumnVector {
        let texts = dates
            .iter()
            .enumerate()
            .map(|(row, date)| {
                date.map(|date| match fsp {
                    None => date.to_owned(),
                    Some(_) => format!("{date} {}", TIMES[row % TIMES.len()]),
                })
            })
            .collect::<Vec<_>>();
        let units = texts
            .iter()
            .map(|text| {
                text.as_deref().map_or(0, |text| match fsp {
                    None => pintail_types::parse_date_days(text).expect("date"),
                    Some(fsp) => {
                        let micros = pintail_types::parse_datetime_micros(text).expect("datetime");
                        let step = 10_i64.pow(6 - u32::from(fsp));
                        micros - micros.rem_euclid(step)
                    }
                })
            })
            .collect();
        let validity =
            ValidityMask::from_bools(&texts.iter().map(Option::is_some).collect::<Vec<_>>());
        match fsp {
            None => ColumnVector::from_typed(
                DataType::Date32,
                TypedValues::Temporal {
                    units,
                    text: LazyText::date(),
                },
                validity,
            ),
            Some(fsp) => ColumnVector::from_typed(
                DataType::DateTime64 { fsp },
                TypedValues::Temporal {
                    units,
                    text: LazyText::datetime(fsp),
                },
                validity,
            ),
        }
    }

    fn batch(column: ColumnVector) -> RecordBatch {
        batch_of(vec![column])
    }

    #[test]
    fn date_parts_of_packed_temporals_match_row_evaluation() {
        let parts = [
            DatePart::Year,
            DatePart::Month,
            DatePart::Day,
            DatePart::Hour,
            DatePart::Minute,
            DatePart::Second,
            DatePart::Quarter,
            DatePart::DayOfWeek,
            DatePart::WeekDay,
            DatePart::DayOfYear,
            DatePart::Week,
            DatePart::IsoWeek,
            DatePart::WeekMode(3),
            DatePart::ExtractWeek(0),
        ];
        for fsp in [None, Some(0), Some(3), Some(6)] {
            let batch = batch(temporal(fsp));
            for part in parts {
                let expression = scalar(
                    ScalarFunction::DatePart(part),
                    vec![CompiledExpr::Column(0)],
                    DataType::Int64,
                );
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Int64),
                    "{part:?} over {fsp:?} has a kernel"
                );
            }
        }
    }

    #[test]
    fn interval_arithmetic_on_packed_temporals_matches_row_evaluation() {
        let units = [
            IntervalUnit::Year,
            IntervalUnit::Month,
            IntervalUnit::Day,
            IntervalUnit::Hour,
            IntervalUnit::Minute,
            IntervalUnit::Second,
        ];
        for fsp in [None, Some(0), Some(3), Some(6)] {
            // Ordinary dates: every combination has a kernel. Calendar
            // edges: one that carries a row out of range declines, and any
            // that answers still agrees.
            for (dates, every) in [(&ORDINARY[..], true), (&DATES[..], false)] {
                let batch = batch(temporal_from(dates, fsp));
                for unit in units {
                    let amounts: &[i64] =
                        if matches!(unit, IntervalUnit::Year | IntervalUnit::Month) {
                            &[0, 1, -1, 12, -13, 31, 400]
                        } else {
                            &[0, 1, -1, 12, -13, 31, 400, 100_000]
                        };
                    for (subtract, amount) in [false, true]
                        .into_iter()
                        .flat_map(|subtract| amounts.iter().map(move |amount| (subtract, *amount)))
                    {
                        let date_only = fsp.is_none()
                            && matches!(
                                unit,
                                IntervalUnit::Year | IntervalUnit::Month | IntervalUnit::Day
                            );
                        let declared = if date_only {
                            DataType::Date32
                        } else {
                            DataType::DateTime64 {
                                fsp: fsp.unwrap_or(0),
                            }
                        };
                        let expression = scalar(
                            ScalarFunction::DateInterval { unit, subtract },
                            vec![
                                CompiledExpr::Column(0),
                                CompiledExpr::Literal(Value::Int64(amount)),
                            ],
                            declared,
                        );
                        let answered = agrees_with_rows(&expression, &batch, declared);
                        assert!(
                            answered || !every,
                            "{unit:?} {amount} subtract={subtract} over {fsp:?} has a kernel"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn dates_of_packed_temporals_match_row_evaluation() {
        for fsp in [None, Some(0), Some(6)] {
            let batch = batch(temporal(fsp));
            for function in [ScalarFunction::Date, ScalarFunction::LastDay] {
                let expression = scalar(function, vec![CompiledExpr::Column(0)], DataType::Date32);
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Date32),
                    "{function:?} over {fsp:?} has a kernel"
                );
            }
            // `DATE(x)` as the binder builds it, with the session's zero-date
            // policy appended: every policy answers a packed row alike.
            for policy in [0_u64, 0b1, 0b11, 0b111, 0b1111] {
                let expression = scalar(
                    ScalarFunction::Date,
                    vec![
                        CompiledExpr::Column(0),
                        CompiledExpr::Literal(Value::UInt64(policy)),
                    ],
                    DataType::Date32,
                );
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Date32),
                    "DATE with policy {policy} over {fsp:?} has a kernel"
                );
            }
        }
    }

    /// `DATE_FORMAT` answers from the units what row evaluation answers
    /// from the text, for a date and for both ends of the fsp range, and a
    /// per-row format string still declines to the row path.
    #[test]
    fn date_formats_of_packed_temporals_match_row_evaluation() {
        for fsp in [None, Some(0), Some(3), Some(6)] {
            let batch = batch(temporal(fsp));
            for format in [
                "%Y-%m-%d %H:%i:%s.%f",
                "%Y-%m",
                "%d/%m/%Y",
                "%H:%i",
                "%W %M %Y",
                "%Y-%m-%d",
                "%Y-%m-%d %H:00",
                "%l %p %q%%%-",
                "%x-%v",
                "",
            ] {
                let expression = scalar(
                    ScalarFunction::DateFormat,
                    vec![
                        CompiledExpr::Column(0),
                        CompiledExpr::Literal(Value::Utf8(format.to_owned())),
                    ],
                    DataType::Utf8,
                );
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Utf8),
                    "{format:?} over {fsp:?} has a kernel"
                );
                // As the planner builds it, with the session's calendar
                // locale appended: names come from that locale.
                for locale in [0_u64, 1, 2, 5] {
                    let expression = scalar(
                        ScalarFunction::DateFormat,
                        vec![
                            CompiledExpr::Column(0),
                            CompiledExpr::Literal(Value::Utf8(format.to_owned())),
                            CompiledExpr::Literal(Value::UInt64(locale)),
                        ],
                        DataType::Utf8,
                    );
                    assert!(
                        agrees_with_rows(&expression, &batch, DataType::Utf8),
                        "{format:?} in locale {locale} over {fsp:?} has a kernel"
                    );
                    // And with the zero-date policy after it, which no
                    // packed row depends on.
                    for policy in [0_i64, 1, 4, 5] {
                        let expression = scalar(
                            ScalarFunction::DateFormat,
                            vec![
                                CompiledExpr::Column(0),
                                CompiledExpr::Literal(Value::Utf8(format.to_owned())),
                                CompiledExpr::Literal(Value::UInt64(locale)),
                                CompiledExpr::Literal(Value::Int64(policy)),
                            ],
                            DataType::Utf8,
                        );
                        assert!(
                            agrees_with_rows(&expression, &batch, DataType::Utf8),
                            "{format:?} with policy {policy} over {fsp:?} has a kernel"
                        );
                    }
                }
            }
            // The format has to be one constant for every row; a column in
            // its place is row evaluation's.
            assert!(
                super::date_format_column(
                    &batch,
                    &[CompiledExpr::Column(0), CompiledExpr::Column(0)],
                    Some(DataType::Utf8),
                    &mut super::Effects::default(),
                )
                .is_none(),
                "a per-row format declines"
            );
        }
    }

    /// A session-zone reading is the units shifted by the zone's offset at
    /// each instant, as row evaluation converts each row's text.
    #[test]
    fn session_zone_readings_of_packed_temporals_match_row_evaluation() {
        let reading = |zone: &str, fsp: u8| {
            scalar(
                ScalarFunction::SessionTimestamp,
                vec![
                    CompiledExpr::Column(0),
                    CompiledExpr::Literal(Value::Utf8(zone.to_owned())),
                ],
                DataType::DateTime64 { fsp },
            )
        };
        for fsp in [0_u8, 3, 6] {
            // Ordinary dates: every offset has a kernel. Calendar edges:
            // one that shifts a row out of four-digit years declines, and
            // any that answers still agrees.
            for (dates, every) in [(&ORDINARY[..], true), (&DATES[..], false)] {
                let batch = batch(temporal_from(dates, Some(fsp)));
                for zone in ["+05:30", "-08:00", "+00:00", "+14:00", "-13:59", "+5:45"] {
                    let declared = DataType::DateTime64 { fsp };
                    let answered = agrees_with_rows(&reading(zone, fsp), &batch, declared);
                    assert!(answered || !every, "{zone} over {fsp} has a kernel");
                    // What reads the reading keeps its own kernel.
                    let date = scalar(
                        ScalarFunction::Date,
                        vec![reading(zone, fsp)],
                        DataType::Date32,
                    );
                    let answered = agrees_with_rows(&date, &batch, DataType::Date32);
                    assert!(answered || !every, "DATE at {zone} over {fsp} has a kernel");
                }
                // A named zone reads each instant at its own offset.
                for zone in ["America/New_York", "Europe/London", "Australia/Lord_Howe"] {
                    let declared = DataType::DateTime64 { fsp };
                    let answered = agrees_with_rows(&reading(zone, fsp), &batch, declared);
                    assert!(answered || !every, "{zone} over {fsp} has a kernel");
                    assert!(reading(zone, fsp).reads_named_session_zone());
                    let date = scalar(
                        ScalarFunction::Date,
                        vec![reading(zone, fsp)],
                        DataType::Date32,
                    );
                    let answered = agrees_with_rows(&date, &batch, DataType::Date32);
                    assert!(answered || !every, "DATE at {zone} over {fsp} has a kernel");
                }
                // Text that names no zone has no reading.
                assert!(
                    reading("Nowhere/Land", fsp)
                        .evaluate_vector_column_quietly(&batch, Some(DataType::DateTime64 { fsp }))
                        .is_none()
                );
            }
        }
    }

    /// The instants either side of every kind of transition, to the
    /// microsecond: a spring-forward gap, an autumn fold, a half-hour
    /// daylight shift, a zone that skipped a whole day, one that changed
    /// its standard time, and local mean time before any standard time.
    #[test]
    fn named_zone_readings_match_row_evaluation_across_transitions() {
        // (zone, UTC instant of a transition)
        let transitions = [
            ("America/New_York", "2024-03-10 07:00:00"),
            ("America/New_York", "2024-11-03 06:00:00"),
            ("Europe/London", "2024-03-31 01:00:00"),
            ("Europe/London", "2024-10-27 01:00:00"),
            ("Australia/Lord_Howe", "2024-04-06 15:00:00"),
            ("Australia/Lord_Howe", "2024-10-05 15:30:00"),
            ("Pacific/Apia", "2011-12-30 10:00:00"),
            ("Asia/Kathmandu", "1985-12-31 18:30:00"),
            ("Europe/Amsterdam", "1937-06-30 23:40:28"),
            ("America/New_York", "1883-11-18 17:00:00"),
            ("Asia/Kolkata", "2024-06-01 00:00:00"),
            ("UTC", "2024-06-01 00:00:00"),
        ];
        for (zone, instant) in transitions {
            let base = crate::expression::temporal::parse_mysql_datetime(instant)
                .expect("instant")
                .and_utc()
                .timestamp_micros();
            for fsp in [0_u8, 3, 6] {
                // Rows walk across the transition forwards, then back, so a
                // remembered offset is always met by a row it is wrong for.
                let steps = [
                    -3_600_000_000_i64,
                    -1_000_000,
                    -1,
                    0,
                    1,
                    999_999,
                    1_000_000,
                    1_800_000_000,
                    3_600_000_000,
                    0,
                    -1,
                    -86_400_000_000,
                    86_400_000_000,
                ];
                // A stored column holds units at its own precision.
                let precision = super::fraction_step(fsp);
                let units: Vec<i64> = steps
                    .iter()
                    .map(|step| base + step)
                    .map(|unit| unit - unit.rem_euclid(precision))
                    .collect();
                let mut valid = vec![true; units.len()];
                valid[4] = false;
                let column = ColumnVector::from_typed(
                    DataType::DateTime64 { fsp },
                    TypedValues::Temporal {
                        units,
                        text: LazyText::datetime(fsp),
                    },
                    ValidityMask::from_bools(&valid),
                );
                let batch = batch(column);
                let reading = scalar(
                    ScalarFunction::SessionTimestamp,
                    vec![
                        CompiledExpr::Column(0),
                        CompiledExpr::Literal(Value::Utf8(zone.to_owned())),
                    ],
                    DataType::DateTime64 { fsp },
                );
                assert!(
                    agrees_with_rows(&reading, &batch, DataType::DateTime64 { fsp }),
                    "{zone} at {instant} over {fsp} has a kernel"
                );
                let hour = scalar(
                    ScalarFunction::DatePart(DatePart::Hour),
                    vec![reading],
                    DataType::Int64,
                );
                assert!(
                    agrees_with_rows(&hour, &batch, DataType::Int64),
                    "HOUR in {zone} at {instant} over {fsp} has a kernel"
                );
            }
        }
    }

    /// A filter on a named-zone reading keeps the rows row evaluation
    /// keeps: bounds spelled from the readings themselves, either side of a
    /// transition, as a range, as each comparison and with the literal
    /// first. A bound the packed comparison cannot hold to the column's
    /// precision has no mask.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn named_zone_filters_keep_the_rows_row_evaluation_keeps() {
        use pintail_sql::BinaryOp;

        use super::super::testing::binary;
        let transitions = [
            ("America/New_York", "2024-03-10 07:00:00"),
            ("America/New_York", "2024-11-03 06:00:00"),
            ("Australia/Lord_Howe", "2024-04-06 15:00:00"),
            ("Europe/London", "2024-10-27 01:00:00"),
        ];
        for (zone, instant) in transitions {
            let base = crate::expression::temporal::parse_mysql_datetime(instant)
                .expect("instant")
                .and_utc()
                .timestamp_micros();
            for fsp in [0_u8, 3, 6] {
                let precision = super::fraction_step(fsp);
                // A row every ten minutes for two hours either side, with
                // a fraction where the column holds one, and a NULL.
                // Then rows a day and more away, either side of the day an
                // offset can move a reading by.
                let far = [-40_i64, -3, -1, 1, 3, 40].into_iter().flat_map(|days| {
                    [-3_600_000_000_i64, 0, 3_600_000_000]
                        .map(move |hour| base + days * MICROS_PER_DAY + hour)
                });
                let units: Vec<i64> = (-12..=12_i64)
                    .map(|step| base + step * 600_000_000 + step.rem_euclid(7) * 123_457)
                    .chain(far)
                    .map(|unit| unit - unit.rem_euclid(precision))
                    .collect();
                let mut valid = vec![true; units.len()];
                valid[5] = false;
                let column = ColumnVector::from_typed(
                    DataType::DateTime64 { fsp },
                    TypedValues::Temporal {
                        units,
                        text: LazyText::datetime(fsp),
                    },
                    ValidityMask::from_bools(&valid),
                );
                let batch = batch(column);
                let reading = || {
                    scalar(
                        ScalarFunction::SessionTimestamp,
                        vec![
                            CompiledExpr::Column(0),
                            CompiledExpr::Literal(Value::Utf8(zone.to_owned())),
                        ],
                        DataType::DateTime64 { fsp },
                    )
                };
                let spelled = |row: usize| {
                    CompiledExpr::Literal(reading().evaluate(&batch, row).expect("reading"))
                };
                let agrees = |filter: &CompiledExpr, context: &str| {
                    let mask = filter
                        .evaluate_filter_mask(&batch)
                        .expect("no error")
                        .unwrap_or_else(|| panic!("{context} in {zone} over {fsp} has a mask"));
                    for row in 0..batch.row_count() {
                        let kept = filter.evaluate(&batch, row).expect("row evaluation")
                            == Value::Boolean(true);
                        assert_eq!(
                            mask.is_selected(row),
                            kept,
                            "{context} in {zone} over {fsp}, row {row}"
                        );
                    }
                };
                for (low, high) in [
                    (3, 20),
                    (10, 14),
                    (12, 12),
                    (20, 3),
                    (0, 24),
                    (25, 42),
                    (30, 36),
                    (27, 12),
                    (12, 40),
                ] {
                    let between = scalar(
                        ScalarFunction::Between { negated: false },
                        vec![reading(), spelled(low), spelled(high)],
                        DataType::Boolean,
                    );
                    agrees(&between, "BETWEEN");
                    for op in [
                        BinaryOp::Equal,
                        BinaryOp::NotEqual,
                        BinaryOp::Less,
                        BinaryOp::LessOrEqual,
                        BinaryOp::Greater,
                        BinaryOp::GreaterOrEqual,
                    ] {
                        let after = binary(op, reading(), spelled(low), DataType::Boolean);
                        agrees(&after, "a comparison");
                        let before = binary(op, spelled(high), reading(), DataType::Boolean);
                        agrees(&before, "a comparison with the literal first");
                        let both = binary(BinaryOp::And, after, before, DataType::Boolean);
                        agrees(&both, "a conjunction");
                    }
                }
                // A bound written to whole seconds against a column with a
                // fraction is left to the row path.
                if fsp > 0 {
                    let short = CompiledExpr::Literal(Value::Utf8(instant.to_owned()));
                    let filter = binary(
                        BinaryOp::GreaterOrEqual,
                        reading(),
                        short,
                        DataType::Boolean,
                    );
                    assert!(
                        filter
                            .evaluate_filter_mask(&batch)
                            .expect("no error")
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn differences_of_packed_temporals_match_row_evaluation() {
        let functions = [
            ScalarFunction::DateDiff,
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Year,
            },
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Month,
            },
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Day,
            },
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Hour,
            },
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Minute,
            },
            ScalarFunction::TimestampDiff {
                unit: IntervalUnit::Second,
            },
        ];
        for (left_fsp, right_fsp) in [(None, None), (Some(0), None), (Some(6), Some(3))] {
            // The second column runs the dates backwards, so each row pairs
            // two different moments.
            let mut reversed = DATES;
            reversed.reverse();
            let mut batch = RecordBatch::new(
                DATES.len(),
                vec![
                    temporal_from(&DATES, left_fsp),
                    temporal_from(&reversed, right_fsp),
                ],
            )
            .expect("batch");
            let mut selection = SelectionMask::all(DATES.len());
            selection.set(3, false).expect("row");
            batch.set_selection(selection).expect("selection");
            let pairs = [
                vec![CompiledExpr::Column(0), CompiledExpr::Column(1)],
                vec![
                    CompiledExpr::Column(0),
                    CompiledExpr::Literal(Value::Utf8("2024-02-29 12:00:00".to_owned())),
                ],
                vec![
                    CompiledExpr::Literal(Value::Utf8("2000-01-31".to_owned())),
                    CompiledExpr::Column(1),
                ],
            ];
            for function in functions {
                for args in &pairs {
                    let expression = scalar(function, args.clone(), DataType::Int64);
                    assert!(
                        agrees_with_rows(&expression, &batch, DataType::Int64),
                        "{function:?} {args:?} has a kernel"
                    );
                }
            }
        }
    }

    /// A kernel's argument may itself be a kernel's answer: the nested
    /// expression is evaluated a batch at a time and read as packed units.
    #[test]
    fn nested_kernels_match_row_evaluation() {
        let shift = |fsp: Option<u8>, unit: IntervalUnit, amount: i64| {
            let declared = match (fsp, unit) {
                (None, IntervalUnit::Year | IntervalUnit::Month | IntervalUnit::Day) => {
                    DataType::Date32
                }
                _ => DataType::DateTime64 {
                    fsp: fsp.unwrap_or(0),
                },
            };
            scalar(
                ScalarFunction::DateInterval {
                    unit,
                    subtract: amount < 0,
                },
                vec![
                    CompiledExpr::Column(0),
                    CompiledExpr::Literal(Value::Int64(amount.abs())),
                ],
                declared,
            )
        };
        for fsp in [None, Some(0), Some(6)] {
            let batch = batch(temporal_from(&ORDINARY, fsp));
            let last_day = scalar(
                ScalarFunction::LastDay,
                vec![CompiledExpr::Column(0)],
                DataType::Date32,
            );
            let expressions = [
                (
                    scalar(
                        ScalarFunction::DateDiff,
                        vec![last_day.clone(), CompiledExpr::Column(0)],
                        DataType::Int64,
                    ),
                    DataType::Int64,
                ),
                (
                    scalar(
                        ScalarFunction::TimestampDiff {
                            unit: IntervalUnit::Second,
                        },
                        vec![CompiledExpr::Column(0), shift(fsp, IntervalUnit::Day, 1)],
                        DataType::Int64,
                    ),
                    DataType::Int64,
                ),
                (
                    scalar(
                        ScalarFunction::DateDiff,
                        vec![shift(fsp, IntervalUnit::Month, -1), last_day.clone()],
                        DataType::Int64,
                    ),
                    DataType::Int64,
                ),
                (
                    scalar(
                        ScalarFunction::DatePart(DatePart::Quarter),
                        vec![shift(fsp, IntervalUnit::Month, 1)],
                        DataType::Int64,
                    ),
                    DataType::Int64,
                ),
                (
                    scalar(
                        ScalarFunction::Date,
                        vec![shift(fsp, IntervalUnit::Hour, -30)],
                        DataType::Date32,
                    ),
                    DataType::Date32,
                ),
                (
                    shift(fsp, IntervalUnit::Day, 3),
                    if fsp.is_none() {
                        DataType::Date32
                    } else {
                        DataType::DateTime64 {
                            fsp: fsp.unwrap_or(0),
                        }
                    },
                ),
            ];
            for (expression, declared) in &expressions {
                assert!(
                    agrees_with_rows(expression, &batch, *declared),
                    "{expression:?} over {fsp:?} has a kernel"
                );
            }
        }
    }

    /// Where a date kernel declines, the row function answers in its place,
    /// so the column is row evaluation's.
    #[test]
    fn a_kernel_declines_what_it_does_not_mirror() {
        // Text kept as written still has a kernel, because the units beside
        // it are the same instant the text spells: a column packs only when
        // every row parses strictly, and row evaluation parses that same
        // text. A spelling wider than the column's own fsp names the same
        // instant, so it agrees too.
        for written in [
            "2024-01-05 10:00:00",
            "2024-01-05 10:00:00.000",
            "2024-01-05 00:00:00",
        ] {
            let column = ColumnVector::new(
                DataType::DateTime64 { fsp: 0 },
                vec![Value::Utf8(written.to_owned())],
            )
            .expect("column");
            let batch = RecordBatch::new(1, vec![column]).expect("batch");
            let expression = scalar(
                ScalarFunction::DatePart(DatePart::Year),
                vec![CompiledExpr::Column(0)],
                DataType::Int64,
            );
            assert!(
                agrees_with_rows(&expression, &batch, DataType::Int64),
                "{written} answers as row evaluation does"
            );
        }
        // Text no strict parse accepts packs nothing, so the kernel has no
        // units to read and the row path answers alone.
        let unparsed = ColumnVector::new(
            DataType::DateTime64 { fsp: 0 },
            vec![Value::Utf8("not a datetime".to_owned())],
        )
        .expect("column");
        let batch = RecordBatch::new(1, vec![unparsed]).expect("batch");
        assert!(
            super::date_part_column(
                &batch,
                &[CompiledExpr::Column(0)],
                DatePart::Year,
                Some(DataType::Int64),
                &mut super::Effects::default(),
            )
            .is_none()
        );
        // A declared type that disagrees with row evaluation's answer.
        let batch = super::tests::batch(temporal(None));
        let expression = scalar(
            ScalarFunction::DateInterval {
                unit: IntervalUnit::Day,
                subtract: false,
            },
            vec![
                CompiledExpr::Column(0),
                CompiledExpr::Literal(Value::Int64(1)),
            ],
            DataType::DateTime64 { fsp: 0 },
        );
        assert!(agrees_with_rows(
            &expression,
            &batch,
            DataType::DateTime64 { fsp: 0 }
        ));
    }
}
