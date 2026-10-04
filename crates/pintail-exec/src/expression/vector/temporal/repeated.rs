//! Temporal text that repeats within a batch, including dates no packed
//! calendar can represent. Evaluate each distinct input with the scalar
//! evaluator, then gather its answer and diagnostics by code. The cache is
//! local to this call: types, locale, SQL policy and statement date remain
//! exactly those of the compiled expression.

use std::collections::HashMap;

use pintail_sql::ScalarFunction;
use pintail_types::{DataType, Value};

use super::super::functions::Call;
use super::super::{Effects, Operand};
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{ColumnVector, RecordBatch, TypedValues};
use crate::execution::capture_conversion_warnings;
use crate::expression::evaluate_eager_scalar_typed;

const MAX_DISTINCT: usize = 64;

/// These functions depend only on their bound arguments and propagate
/// NULL. Keeping an explicit list excludes volatile calls and functions
/// whose arguments carry state outside their values.
const fn temporal(function: ScalarFunction) -> bool {
    matches!(
        function,
        ScalarFunction::DateFormat
            | ScalarFunction::TimeFormat
            | ScalarFunction::DatePart(_)
            | ScalarFunction::DateInterval { .. }
            | ScalarFunction::DateDiff
            | ScalarFunction::TimestampDiff { .. }
            | ScalarFunction::Date
            | ScalarFunction::Time
            | ScalarFunction::DayName
            | ScalarFunction::MonthName
            | ScalarFunction::LastDay
            | ScalarFunction::ToDays
            | ScalarFunction::ToSeconds
            | ScalarFunction::YearWeek
            | ScalarFunction::TimeToSec
            | ScalarFunction::AddTime
            | ScalarFunction::SubTime
            | ScalarFunction::TimeDiff
            | ScalarFunction::Cast(
                DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. }
            )
            | ScalarFunction::DeclaredCast {
                target: DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. },
                characters: None,
                ..
            }
    )
}

/// One varying text argument and constants everywhere else. Packed
/// instant kernels have already had their turn; a high-cardinality or
/// unsupported input declines before evaluating any scalar or warning.
pub(in super::super) fn repeated_column(
    batch: &RecordBatch,
    call: &Call<'_>,
    operands: &[Operand<'_>],
    effects: &mut Effects,
) -> Option<ColumnVector> {
    if !temporal(call.function) {
        return None;
    }
    let mut varying = operands
        .iter()
        .enumerate()
        .filter_map(|(position, operand)| {
            if let Operand::Column(column) = operand {
                Some((position, column))
            } else {
                None
            }
        });
    let Some((position, source)) = varying.next() else {
        return constant_column(batch, call, operands);
    };
    if varying.next().is_some()
        || !matches!(
            source.data_type(),
            DataType::Date32
                | DataType::DateTime64 { .. }
                | DataType::Time64 { .. }
                | DataType::Utf8
        )
    {
        return None;
    }
    let (TypedValues::Utf8(text), validity) = source.typed()? else {
        return None;
    };
    if text.declared_enum_labels().is_some() || text.declared_set_members().is_some() {
        return None;
    }
    let (codes, representatives) = dictionary(batch, text, validity)?;
    let mut arguments = operands
        .iter()
        .map(|operand| match operand {
            Operand::Constant(value) => (*value).clone(),
            Operand::Column(_) => Value::Null,
        })
        .collect::<Vec<_>>();
    // Code zero represents NULL and unselected rows. All admitted calls
    // propagate NULL without evaluating the other constant arguments.
    let mut answers = vec![Value::Null];
    let mut diagnostics = vec![(Vec::new(), 0)];
    for row in &representatives {
        arguments[position] = source.value_owned(*row)?;
        let (answer, warnings) = capture_conversion_warnings(|| {
            evaluate_eager_scalar_typed(
                call.function,
                &arguments,
                call.argument_types,
                call.literal_regex,
                call.data_type,
                call.collation,
            )
        });
        answers.push(answer.ok()?);
        diagnostics.push(warnings);
    }
    let column = gather(call.data_type?, &codes, &answers)?;
    // Nothing above this point has changed diagnostics. Defer replay to
    // Effects so a declined parent or a quiet attempt discards them too.
    for code in &codes {
        effects.conversions(&diagnostics[*code as usize]);
    }
    crate::counters::count(|counters| {
        counters.temporal_values_evaluated = counters
            .temporal_values_evaluated
            .saturating_add(u64::try_from(representatives.len()).unwrap_or(u64::MAX));
    });
    Some(column)
}

/// A constant temporal subtree can keep an enclosing arithmetic expression
/// off the row path too. For example, `TO_DAYS` of the statement's captured
/// date remains a scalar node even though its argument is a literal.
/// Warning-producing constants decline: those diagnostics belong to each
/// row at its position among the surrounding expression's other warnings.
fn constant_column(
    batch: &RecordBatch,
    call: &Call<'_>,
    operands: &[Operand<'_>],
) -> Option<ColumnVector> {
    let codes = (0..batch.row_count())
        .map(|row| u32::from(batch.selection().is_selected(row)))
        .collect::<Vec<_>>();
    if !codes.contains(&1) {
        return gather(call.data_type?, &codes, &[Value::Null]);
    }
    let arguments = operands
        .iter()
        .map(|operand| match operand {
            Operand::Constant(value) => Some((*value).clone()),
            Operand::Column(_) => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let answer = crate::execution::without_new_warnings(|| {
        evaluate_eager_scalar_typed(
            call.function,
            &arguments,
            call.argument_types,
            call.literal_regex,
            call.data_type,
            call.collation,
        )
        .ok()
    })??;
    let column = gather(call.data_type?, &codes, &[Value::Null, answer])?;
    crate::counters::count(|counters| {
        counters.temporal_values_evaluated = counters.temporal_values_evaluated.saturating_add(1);
    });
    Some(column)
}

/// Codes and one representative selected row per distinct non-NULL input.
/// Use stored codes where available; otherwise hash borrowed bytes and
/// copy only new dictionary entries. Stop at the bound, before scalar work.
fn dictionary(
    batch: &RecordBatch,
    text: &StrColumn,
    validity: &ValidityMask,
) -> Option<(Vec<u32>, Vec<usize>)> {
    let mut codes = vec![0; batch.row_count()];
    let mut representatives = Vec::new();
    if let Some((source_codes, values)) = text.dictionary() {
        if values.len() > MAX_DISTINCT {
            return None;
        }
        let mut mapped = vec![0; values.len()];
        for row in batch.selection().selected_rows() {
            if !validity.is_valid(row) {
                continue;
            }
            let code = mapped.get_mut(source_codes[row] as usize)?;
            if *code == 0 {
                representatives.push(row);
                *code = u32::try_from(representatives.len()).ok()?;
            }
            codes[row] = *code;
        }
    } else {
        let mut by_text = HashMap::<Vec<u8>, u32>::new();
        for row in batch.selection().selected_rows() {
            if !validity.is_valid(row) {
                continue;
            }
            codes[row] = text.views()[row].with_bytes(text.heap(), |bytes| {
                if let Some(code) = by_text.get(bytes) {
                    return Some(*code);
                }
                if representatives.len() == MAX_DISTINCT {
                    return None;
                }
                representatives.push(row);
                let code = u32::try_from(representatives.len()).ok()?;
                by_text.insert(bytes.to_vec(), code);
                Some(code)
            })?;
        }
    }
    Some((codes, representatives))
}

/// Gather integers as units and text as codes. In particular, temporal
/// text must keep zero dates and its original fractional-second spelling;
/// attempting to repack every answer would parse the same text per row.
fn gather(declared: DataType, codes: &[u32], answers: &[Value]) -> Option<ColumnVector> {
    let validity = ValidityMask::from_bools(
        &codes
            .iter()
            .map(|code| !matches!(answers[*code as usize], Value::Null))
            .collect::<Vec<_>>(),
    );
    let typed = match declared.storage_type() {
        DataType::Int64 => {
            let values = answers
                .iter()
                .map(|answer| match answer {
                    Value::Null => Some(0),
                    Value::Int64(value) => Some(*value),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            TypedValues::Int64(codes.iter().map(|code| values[*code as usize]).collect())
        }
        DataType::UInt64 => {
            let values = answers
                .iter()
                .map(|answer| match answer {
                    Value::Null => Some(0),
                    Value::UInt64(value) => Some(*value),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            TypedValues::UInt64(codes.iter().map(|code| values[*code as usize]).collect())
        }
        DataType::Utf8 => {
            let mut heap = Vec::new();
            let mut offsets = vec![0];
            for answer in answers {
                match answer {
                    Value::Null => {}
                    Value::Utf8(text) => heap.extend_from_slice(text.as_bytes()),
                    _ => return None,
                }
                offsets.push(heap.len());
            }
            TypedValues::Utf8(StrColumn::from_dictionary(
                &heap,
                &offsets,
                codes.to_vec(),
                validity.clone(),
            ))
        }
        _ => return None,
    };
    Some(ColumnVector::from_typed(declared, typed, validity))
}

#[cfg(test)]
mod tests;
