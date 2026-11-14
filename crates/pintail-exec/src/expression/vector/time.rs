//! `TIME` kernels: the clock parts and the second count of a TIME column.
//!
//! A TIME is stored as its canonical text, `[-]HH:MM:SS[.ffffff]` with two
//! or three hour digits. Row evaluation reads that text the way it reads
//! any text given where a time is wanted: as a packed number, as a date and
//! time in each of the spellings one may take, and only then as a duration.
//! Here a column whose every selected value is canonical is packed once, as
//! signed microseconds, and each function answers from those units. A value
//! spelled any other way declines the batch, and row evaluation answers.

use pintail_sql::{DatePart, ScalarFunction};
use pintail_types::{DataType, Value};

use super::Operand;
use crate::array::ValidityMask;
use crate::batch::{Cell, ColumnVector, RecordBatch, TypedValues};

const MICROS_PER_SECOND: i64 = 1_000_000;

/// The signed microseconds of a canonical TIME; `None` for any other text.
fn canonical_micros(text: &[u8]) -> Option<i64> {
    let (negative, body) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    let colon = body.iter().position(|byte| *byte == b':')?;
    if !(2..=3).contains(&colon) || body.len() < colon + 6 || body[colon + 3] != b':' {
        return None;
    }
    let number = |digits: &[u8]| -> Option<i64> {
        digits.iter().try_fold(0_i64, |value, digit| {
            digit
                .is_ascii_digit()
                .then(|| value * 10 + i64::from(digit - b'0'))
        })
    };
    let hours = number(&body[..colon])?;
    let minutes = number(&body[colon + 1..colon + 3])?;
    let seconds = number(&body[colon + 4..colon + 6])?;
    if hours > 838 || minutes > 59 || seconds > 59 {
        return None;
    }
    let fraction = match &body[colon + 6..] {
        [] => 0,
        [b'.', digits @ ..] if (1..=6).contains(&digits.len()) => {
            let mut micros = number(digits)?;
            for _ in digits.len()..6 {
                micros *= 10;
            }
            micros
        }
        _ => return None,
    };
    let micros = (hours * 3_600 + minutes * 60 + seconds) * MICROS_PER_SECOND + fraction;
    Some(if negative { -micros } else { micros })
}

/// A TIME column's values as signed microseconds, zero where the row is
/// NULL or outside the batch's selection; `None` when the column is not a
/// TIME held as text or a selected value is not canonical.
fn packed<'column>(
    batch: &RecordBatch,
    column: &'column ColumnVector,
) -> Option<(Vec<i64>, &'column ValidityMask)> {
    if !matches!(column.data_type(), DataType::Time64 { .. }) {
        return None;
    }
    let (TypedValues::Utf8(_), validity) = column.typed()? else {
        return None;
    };
    let mut units = Vec::with_capacity(column.len());
    for row in 0..column.len() {
        if !validity.is_valid(row) {
            units.push(0);
            continue;
        }
        let micros = column.with_cell(row, |cell| match cell {
            Cell::Text(text) => canonical_micros(text),
            Cell::Value(Value::Utf8(text)) => canonical_micros(text.as_bytes()),
            _ => None,
        });
        match micros {
            Some(micros) => units.push(micros),
            None if batch.selection().is_selected(row) => return None,
            None => units.push(0),
        }
    }
    Some((units, validity))
}

/// `HOUR`, `MINUTE`, `SECOND` and `TIME_TO_SEC` of a TIME column, from its
/// packed microseconds: a clock part is the field as written, whatever the
/// sign and with an hour count past a day; the second count is signed and
/// drops the fraction. `None` for any other call or column.
pub(super) fn time_column(
    batch: &RecordBatch,
    function: ScalarFunction,
    operands: &[Operand<'_>],
    declared: DataType,
) -> Option<ColumnVector> {
    let [Operand::Column(input)] = operands else {
        return None;
    };
    if declared != DataType::Int64 {
        return None;
    }
    let part = match function {
        ScalarFunction::TimeToSec => None,
        ScalarFunction::DatePart(part @ (DatePart::Hour | DatePart::Minute | DatePart::Second)) => {
            Some(part)
        }
        _ => return None,
    };
    let (mut units, validity) = packed(batch, input)?;
    for unit in &mut units {
        let seconds = *unit / MICROS_PER_SECOND;
        *unit = match part {
            None => seconds,
            Some(DatePart::Hour) => seconds.abs() / 3_600,
            Some(DatePart::Minute) => seconds.abs() / 60 % 60,
            Some(_) => seconds.abs() % 60,
        };
    }
    crate::counters::count(|counters| {
        counters.time_kernel_batches = counters.time_kernel_batches.saturating_add(1);
    });
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Int64(units),
        validity.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use pintail_sql::{DatePart, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, scalar};
    use super::canonical_micros;
    use crate::batch::ColumnVector;
    use crate::expression::CompiledExpr;

    const TIME: DataType = DataType::Time64 { fsp: 6 };

    fn typed_scalar(function: ScalarFunction, data_type: DataType) -> CompiledExpr {
        let CompiledExpr::Scalar {
            function,
            args,
            literal_regex,
            session_zone,
            variables,
            data_type,
            collation,
            overflow,
            ..
        } = scalar(function, vec![CompiledExpr::Column(0)], data_type)
        else {
            unreachable!()
        };
        CompiledExpr::Scalar {
            function,
            args,
            argument_types: vec![Some(TIME)],
            literal_regex,
            session_zone,
            variables,
            data_type,
            collation,
            overflow,
        }
    }

    fn times(texts: &[Option<&str>]) -> crate::RecordBatch {
        let values = texts
            .iter()
            .map(|text| text.map_or(Value::Null, |text| Value::Utf8(text.to_owned())))
            .collect::<Vec<_>>();
        // Packed, as a scan hands a TIME column over.
        let column = ColumnVector::new(TIME, values).expect("column");
        let (typed, validity) = column.typed().expect("typed");
        batch_of(vec![ColumnVector::from_typed(
            TIME,
            typed.clone(),
            validity.clone(),
        )])
    }

    #[test]
    fn canonical_times_pack_and_other_spellings_do_not() {
        assert_eq!(canonical_micros(b"00:00:00"), Some(0));
        assert_eq!(canonical_micros(b"-00:00:01.5"), Some(-1_500_000));
        assert_eq!(
            canonical_micros(b"838:59:59.000000"),
            Some(3_020_399_000_000)
        );
        assert_eq!(canonical_micros(b"-838:59:59"), Some(-3_020_399_000_000));
        for other in [
            "",
            "1:02:03",
            "12:60:00",
            "12:00:60",
            "839:00:00",
            "12:00",
            "120000",
            "1 02:03:04",
            "12:00:00.",
            "12:00:00.1234567",
            "+12:00:00",
            " 12:00:00",
            "2024-01-01 12:00:00",
        ] {
            assert_eq!(canonical_micros(other.as_bytes()), None, "{other:?}");
        }
    }

    /// Clock parts and second counts of a TIME column answer from packed
    /// units, and answer what row evaluation does: negative values, values
    /// past a day, values that also read as a date, NULLs and fractions.
    #[test]
    fn time_kernels_answer_as_row_evaluation_does() {
        // More distinct values than the repeated-value path reads once each.
        let spread = (0..120_u32)
            .map(|step| {
                let seconds = step * 7_919 % 400_000;
                format!(
                    "{}{:02}:{:02}:{:02}.{:06}",
                    if step.is_multiple_of(5) { "-" } else { "" },
                    seconds / 3_600,
                    seconds / 60 % 60,
                    seconds % 60,
                    step * 8_191
                )
            })
            .collect::<Vec<_>>();
        let mut texts = vec![
            Some("00:00:00"),
            Some("-00:00:01.500000"),
            None,
            Some("not selected"),
            Some("838:59:59.000000"),
            Some("-838:59:59.000000"),
            Some("12:11:10.000000"),
            Some("20:24:11.000000"),
            Some("24:00:00.000000"),
            Some("100:59:07.999999"),
            Some("-25:00:59.000001"),
            Some("23:59:59.999999"),
        ];
        texts.extend(spread.iter().map(|text| Some(text.as_str())));
        for function in [
            ScalarFunction::DatePart(DatePart::Hour),
            ScalarFunction::DatePart(DatePart::Minute),
            ScalarFunction::DatePart(DatePart::Second),
            ScalarFunction::TimeToSec,
        ] {
            let batch = times(&texts);
            let _ = crate::take_exec_counters();
            let expression = typed_scalar(function, DataType::Int64);
            assert!(
                agrees_with_rows(&expression, &batch, DataType::Int64),
                "{function:?} has a kernel"
            );
            assert!(
                crate::take_exec_counters().time_kernel_batches >= 1,
                "{function:?} read packed units"
            );
        }
    }

    /// A selected value spelled any other way is row evaluation's to read.
    #[test]
    fn a_time_spelled_another_way_is_not_packed() {
        let batch = times(&[Some("00:00:00"), Some("1:2:3"), None, None]);
        let _ = crate::take_exec_counters();
        let expression = typed_scalar(ScalarFunction::TimeToSec, DataType::Int64);
        let _ = expression.evaluate_column(&batch, Some(DataType::Int64));
        assert_eq!(crate::take_exec_counters().time_kernel_batches, 0);
    }
}
