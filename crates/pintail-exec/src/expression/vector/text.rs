//! Text functions over packed plain text, read in place.
//!
//! Row evaluation builds each cell's text as a value and hands it to the
//! function. These read the same text from the column's buffer and apply
//! the same case mapping, length or matcher, building no value for the
//! input. An ENUM or SET reads as its label, which is the text the column
//! holds, so these functions read it as text too; text that is not UTF-8,
//! which row evaluation reads lossily, stays with the row function.

use pintail_sql::ScalarFunction;
use pintail_types::{DataType, Value};

use super::{Operand, truth_column};
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{ColumnVector, TypedValues};
use crate::collation::Collation;
use crate::expression::like_matches;

/// A text column's buffer: plain text, or an ENUM or SET's labels.
fn text_of(column: &ColumnVector) -> Option<(&StrColumn, &ValidityMask)> {
    if column.data_type() != DataType::Utf8 {
        return None;
    }
    match column.typed()? {
        (TypedValues::Utf8(text), validity) => Some((text, validity)),
        _ => None,
    }
}

/// `f` over each row's text, `None` for NULL rows; `None` overall for text
/// that is not UTF-8.
fn each_text<R>(column: &ColumnVector, mut f: impl FnMut(&str) -> R) -> Option<Vec<Option<R>>> {
    let (text, validity) = text_of(column)?;
    (0..text.len())
        .map(|row| {
            if !validity.is_valid(row) {
                return Some(None);
            }
            text.views()[row].with_bytes(text.heap(), |bytes| {
                std::str::from_utf8(bytes).ok().map(|text| Some(f(text)))
            })
        })
        .collect()
}

pub(super) fn packed_text(
    function: ScalarFunction,
    operands: &[Operand<'_>],
    declared: DataType,
    collation: Collation,
) -> Option<ColumnVector> {
    let Some(Operand::Column(column)) = operands.first() else {
        return None;
    };
    match (function, &operands[1..]) {
        (ScalarFunction::Upper | ScalarFunction::Lower, []) if declared == DataType::Utf8 => {
            let upper = function == ScalarFunction::Upper;
            let (_, validity) = text_of(column)?;
            let mapped = each_text(column, |text| {
                if upper {
                    text.to_uppercase()
                } else {
                    text.to_lowercase()
                }
            })?;
            let mut text = StrColumn::default();
            for value in &mapped {
                text.push(value.as_deref().unwrap_or_default().as_bytes());
            }
            Some(ColumnVector::from_typed(
                declared,
                TypedValues::Utf8(text),
                validity.clone(),
            ))
        }
        (ScalarFunction::Length | ScalarFunction::CharLength, [])
            if declared.storage_type() == DataType::UInt64 =>
        {
            let bytes = function == ScalarFunction::Length;
            let (_, validity) = text_of(column)?;
            let lengths = each_text(column, |text| {
                u64::try_from(if bytes {
                    text.len()
                } else {
                    text.chars().count()
                })
                .unwrap_or(u64::MAX)
            })?;
            Some(ColumnVector::from_typed(
                declared,
                TypedValues::UInt64(lengths.iter().map(|length| length.unwrap_or(0)).collect()),
                validity.clone(),
            ))
        }
        (ScalarFunction::Like { negated, escape }, [Operand::Constant(Value::Utf8(pattern))])
            if declared == DataType::Boolean =>
        {
            let matched = each_text(column, |text| {
                like_matches(text, pattern, escape, false, collation) != negated
            })?;
            truth_column(matched.into_iter())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pintail_sql::ScalarFunction;
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, scalar};
    use crate::batch::ColumnVector;
    use crate::collation::Collation;
    use crate::expression::CompiledExpr;

    fn texts(values: &[Option<&str>]) -> ColumnVector {
        ColumnVector::new(
            DataType::Utf8,
            values
                .iter()
                .map(|value| value.map_or(Value::Null, |text| Value::Utf8(text.to_owned())))
                .collect(),
        )
        .expect("text")
    }

    /// An ENUM column reads as its labels.
    #[test]
    fn text_functions_read_an_enum_as_its_labels() {
        let values = ["shipped", "pending", "shipped", "pending"]
            .iter()
            .enumerate()
            .map(|(row, label)| {
                if row == 2 {
                    Value::Null
                } else {
                    Value::Enum {
                        index: u64::from(*label == "shipped") + 1,
                        label: (*label).to_owned(),
                    }
                }
            })
            .collect::<Vec<_>>();
        let batch = batch_of(vec![
            ColumnVector::new(DataType::Utf8, values).expect("enum"),
        ]);
        for (expression, declared) in [
            (
                scalar(
                    ScalarFunction::Upper,
                    vec![CompiledExpr::Column(0)],
                    DataType::Utf8,
                ),
                DataType::Utf8,
            ),
            (
                scalar(
                    ScalarFunction::Length,
                    vec![CompiledExpr::Column(0)],
                    DataType::UInt64,
                ),
                DataType::UInt64,
            ),
        ] {
            assert!(
                agrees_with_rows(&expression, &batch, declared),
                "{expression:?}"
            );
        }
    }

    #[test]
    fn text_functions_match_row_evaluation() {
        let batch = batch_of(vec![texts(&[
            Some("shipped"),
            Some("Straße"),
            None,
            Some("ÉTÉ"),
            Some(""),
            Some("reship_me"),
            Some("50%ship"),
            Some("x"),
        ])]);
        let column = || CompiledExpr::Column(0);
        for (function, declared) in [
            (ScalarFunction::Upper, DataType::Utf8),
            (ScalarFunction::Lower, DataType::Utf8),
            (ScalarFunction::Length, DataType::UInt64),
            (ScalarFunction::CharLength, DataType::UInt64),
        ] {
            let expression = scalar(function, vec![column()], declared);
            assert!(
                agrees_with_rows(&expression, &batch, declared),
                "{function:?}"
            );
        }
        for pattern in ["%ship%", "s_ipped", "%\\%%", "", "%"] {
            for negated in [false, true] {
                for name in ["utf8mb4_0900_ai_ci", "utf8mb4_bin"] {
                    let expression = CompiledExpr::Scalar {
                        function: ScalarFunction::Like {
                            negated,
                            escape: Some('\\'),
                        },
                        args: vec![
                            column(),
                            CompiledExpr::Literal(Value::Utf8(pattern.to_owned())),
                        ],
                        argument_types: vec![Some(DataType::Utf8); 2],
                        literal_regex: None,
                        data_type: Some(DataType::Boolean),
                        collation: Collation::from_mysql_name(name).expect("collation"),
                        overflow: None,
                    };
                    assert!(
                        agrees_with_rows(&expression, &batch, DataType::Boolean),
                        "{pattern} {negated} {name}"
                    );
                }
            }
        }
    }
}
