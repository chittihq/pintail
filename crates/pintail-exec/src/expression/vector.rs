//! Compiled expressions evaluated a batch at a time over packed columns.
//!
//! Row-at-a-time evaluation turns every input cell into a [`Value`] and a
//! temporal one into text, parses that text back, and formats its answer as
//! text again: per row, per expression. Here an expression whose every node
//! has a kernel is evaluated once per batch over the packed units the scan
//! already produced, and answers with a packed column whose text is derived
//! only when something reads it as text.
//!
//! A kernel answers exactly what row evaluation would, or declines: given
//! an input shape, a declared type or a result it does not mirror - text
//! kept as written, a year outside what canonical text can spell - it
//! returns `None` and the caller evaluates the batch row by row. Rows the
//! batch's selection excludes are computed but never fail the batch.

use chrono::{NaiveDateTime, Timelike as _};
use pintail_sql::{DatePart, IntervalUnit, ScalarFunction};
use pintail_types::{DataType, Value};

use super::CompiledExpr;
use super::temporal::{apply_interval, date_part};
use crate::array::ValidityMask;
use crate::batch::{ColumnVector, LazyText, RecordBatch, TypedValues};
use crate::execution::ExecError;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A temporal column's packed units, when its text is derived from them.
struct Temporal<'batch> {
    units: &'batch [i64],
    validity: &'batch ValidityMask,
    /// `None` for a date (units are days), the precision for a datetime
    /// (units are microseconds).
    fsp: Option<u8>,
}

impl Temporal<'_> {
    /// Row `row` as the date-time row evaluation parses from its text.
    fn datetime(&self, row: usize) -> Option<NaiveDateTime> {
        let unit = self.units[row];
        let micros = match self.fsp {
            None => unit.checked_mul(MICROS_PER_DAY)?,
            // The text carries `fsp` fraction digits, so that is all the
            // precision a parse of it recovers.
            Some(fsp) => {
                let step = 10_i64.pow(6 - u32::from(fsp.min(6)));
                unit - unit.rem_euclid(step)
            }
        };
        Some(chrono::DateTime::from_timestamp_micros(micros)?.naive_utc())
    }
}

/// The packed temporal units of column `index` of `batch`, when it has them.
fn temporal_column(batch: &RecordBatch, index: usize) -> Option<Temporal<'_>> {
    let column = batch.column(index)?;
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

impl CompiledExpr {
    /// The expression evaluated over every row of `batch` as one column of
    /// `data_type`, when every node has a kernel; `None` sends the caller
    /// to row-at-a-time evaluation.
    ///
    /// # Errors
    ///
    /// Returns the error row evaluation would raise for a selected row.
    pub(crate) fn evaluate_column(
        &self,
        batch: &RecordBatch,
        data_type: Option<DataType>,
    ) -> Result<Option<ColumnVector>, ExecError> {
        match self {
            Self::Column(index) => Ok(batch
                .column(*index)
                .filter(|column| data_type.is_none_or(|declared| declared == column.data_type()))
                .cloned()),
            Self::Scalar {
                function: ScalarFunction::DatePart(part),
                args,
                ..
            } => Ok(date_part_column(batch, args, *part, data_type)),
            Self::Scalar {
                function: ScalarFunction::DateInterval { unit, subtract },
                args,
                ..
            } => date_interval_column(batch, args, *unit, *subtract, data_type),
            _ => Ok(None),
        }
    }
}

/// `YEAR(x)`, `MONTH(x)` and the other single parts of a packed temporal.
fn date_part_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    part: DatePart,
    data_type: Option<DataType>,
) -> Option<ColumnVector> {
    let [CompiledExpr::Column(index)] = args else {
        return None;
    };
    // Row evaluation answers a plain integer.
    let declared = data_type.unwrap_or(DataType::Int64);
    if declared.storage_type() != DataType::Int64 {
        return None;
    }
    let input = temporal_column(batch, *index)?;
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
fn date_interval_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    unit: IntervalUnit,
    subtract: bool,
    data_type: Option<DataType>,
) -> Result<Option<ColumnVector>, ExecError> {
    let [CompiledExpr::Column(index), CompiledExpr::Literal(amount)] = args else {
        return Ok(None);
    };
    let Some(input) = temporal_column(batch, *index) else {
        return Ok(None);
    };
    // A NULL amount makes every row NULL; that is row evaluation's to say.
    if matches!(amount, Value::Null) {
        return Ok(None);
    }
    let Ok(amount) = super::mysql_i64(amount) else {
        return Ok(None);
    };
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
        _ => return Ok(None),
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
        let Some(value) = input.datetime(row) else {
            return Ok(None);
        };
        let shifted = match apply_interval(value, amount, unit, subtract) {
            Ok(shifted) => shifted,
            // Row evaluation answers an impossible date-time with NULL.
            Err(ExecError::InvalidDateTime) => {
                units.push(0);
                valid.push(false);
                continue;
            }
            Err(error) if selection.is_selected(row) => return Err(error),
            Err(_) => {
                units.push(0);
                valid.push(false);
                continue;
            }
        };
        if !spellable(shifted) {
            return Ok(None);
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
    Ok(Some(ColumnVector::from_typed(
        declared,
        TypedValues::Temporal { units, text },
        ValidityMask::from_bools(&valid),
    )))
}

#[cfg(test)]
mod tests {
    use pintail_sql::{DatePart, IntervalUnit, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::CompiledExpr;
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, LazyText, RecordBatch, SelectionMask, TypedValues};
    use crate::collation::Collation;

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

    /// Every row selected but the fourth, which a kernel must compute and
    /// never fail on.
    fn batch(column: ColumnVector) -> RecordBatch {
        let rows = column.len();
        let mut batch = RecordBatch::new(rows, vec![column]).expect("batch");
        let mut selection = SelectionMask::all(rows);
        selection.set(3, false).expect("row");
        batch.set_selection(selection).expect("selection");
        batch
    }

    fn scalar(
        function: ScalarFunction,
        args: Vec<CompiledExpr>,
        data_type: DataType,
    ) -> CompiledExpr {
        CompiledExpr::Scalar {
            function,
            argument_types: vec![None; args.len()],
            args,
            literal_regex: None,
            data_type: Some(data_type),
            collation: Collation::default(),
            overflow: None,
        }
    }

    /// The kernel's answer, when it gives one, is row evaluation's at every
    /// selected row. Returns whether it gave one.
    fn agrees_with_rows(
        expression: &CompiledExpr,
        batch: &RecordBatch,
        data_type: DataType,
    ) -> bool {
        let Some(column) = expression
            .evaluate_column(batch, Some(data_type))
            .expect("the kernel evaluates")
        else {
            return false;
        };
        assert_eq!(column.data_type(), data_type);
        for row in batch.selection().selected_rows() {
            let expected = expression.evaluate(batch, row).expect("row evaluation");
            assert_eq!(
                column.value(row),
                Some(&expected),
                "{expression:?} at row {row}"
            );
        }
        true
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
            expression
                .evaluate_column(&batch, Some(DataType::Int64))
                .expect("evaluates")
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
        assert!(
            expression
                .evaluate_column(&batch, Some(DataType::DateTime64 { fsp: 0 }))
                .expect("evaluates")
                .is_none()
        );
    }
}
