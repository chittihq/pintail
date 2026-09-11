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
            Some(fsp) => {
                let step = 10_i64.pow(6 - u32::from(fsp.min(6)));
                unit - unit.rem_euclid(step)
            }
        }
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
    text.derived().then_some(Temporal {
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
pub(super) fn date_of_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    function: ScalarFunction,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [argument] = args else {
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
            _ => from.date().signed_duration_since(to.date()).num_days(),
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
    let mut parts = Vec::with_capacity(input.units.len());
    let mut valid = Vec::with_capacity(input.units.len());
    for row in 0..input.units.len() {
        if !input.validity.is_valid(row) {
            parts.push(0);
            valid.push(false);
            continue;
        }
        let value = input.datetime(row)?;
        parts.push(i64::try_from(date_part(value, part)).ok()?);
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
    let amount = crate::expression::mysql_i64(amount).ok()?;
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
        if !spellable(shifted) {
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
    let [argument] = args else {
        return None;
    };
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
    use super::CompiledExpr;
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
        // Text kept as written: row evaluation reads the text itself.
        let written = ColumnVector::new(
            DataType::DateTime64 { fsp: 0 },
            vec![Value::Utf8("2024-01-05 10:00:00".to_owned())],
        )
        .expect("column");
        let batch = RecordBatch::new(1, vec![written]).expect("batch");
        let expression = scalar(
            ScalarFunction::DatePart(DatePart::Year),
            vec![CompiledExpr::Column(0)],
            DataType::Int64,
        );
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
        assert!(agrees_with_rows(&expression, &batch, DataType::Int64));
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
