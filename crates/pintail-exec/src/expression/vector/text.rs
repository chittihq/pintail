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
fn each_text<R: Clone>(
    column: &ColumnVector,
    mut f: impl FnMut(&str) -> R,
) -> Option<Vec<Option<R>>> {
    let (text, validity) = text_of(column)?;
    // A coded column calls `f` once per distinct value and gathers the
    // answers by code: a hundred thousand rows of ten distinct names run it
    // ten times, and the per-row views are never built.
    if let Some((codes, values)) = text.dictionary() {
        let mapped = values.iter().map(|value| f(value)).collect::<Vec<_>>();
        return codes
            .iter()
            .enumerate()
            .map(|(row, code)| {
                if !validity.is_valid(row) {
                    return Some(None);
                }
                mapped.get(usize::try_from(*code).ok()?).cloned().map(Some)
            })
            .collect();
    }
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
            let (source, validity) = text_of(column)?;
            // A coded column answers from its distinct values, and stays
            // coded; reading it row by row would materialize every view
            // first, which is the cost this avoids.
            if let Some(coded) = source.map_dictionary(|text| {
                if upper {
                    text.to_uppercase()
                } else {
                    text.to_lowercase()
                }
            }) {
                return Some(ColumnVector::from_typed(
                    declared,
                    TypedValues::Utf8(coded),
                    validity.clone(),
                ));
            }
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

    /// The same values as a coded column, the way a dictionary-encoded
    /// block arrives from storage.
    fn coded(values: &[Option<&str>]) -> ColumnVector {
        let mut distinct: Vec<&str> = Vec::new();
        let mut codes = Vec::with_capacity(values.len());
        let mut valid = Vec::with_capacity(values.len());
        for value in values {
            let text = value.unwrap_or_default();
            let code = distinct
                .iter()
                .position(|held| *held == text)
                .unwrap_or_else(|| {
                    distinct.push(text);
                    distinct.len() - 1
                });
            codes.push(u32::try_from(code).expect("small dictionary"));
            valid.push(value.is_some());
        }
        let mut heap = Vec::new();
        let mut offsets = vec![0];
        for text in &distinct {
            heap.extend_from_slice(text.as_bytes());
            offsets.push(heap.len());
        }
        let validity = pintail_store::ColumnValidity::Bytes(valid.clone());
        let column = crate::array::StrColumn::from_dictionary(&heap, &offsets, &codes, &validity);
        ColumnVector::from_typed(
            DataType::Utf8,
            crate::batch::TypedValues::Utf8(column),
            crate::array::ValidityMask::from_bools(&valid),
        )
    }

    /// A function of a coded column is a function of its distinct values.
    ///
    /// Two things have to hold: the answer is the one the row path gives,
    /// and the work is proportional to the dictionary rather than the rows,
    /// which shows as the answer still being coded, with one entry per
    /// distinct input rather than one per row.
    #[test]
    fn a_coded_column_answers_from_its_distinct_values() {
        let values = [
            Some("red"),
            Some("green"),
            Some("red"),
            Some("blue"),
            None,
            Some("green"),
            Some("red"),
        ];
        let batch = batch_of(vec![coded(&values)]);
        for function in [ScalarFunction::Upper, ScalarFunction::Lower] {
            let call = scalar(function, vec![CompiledExpr::Column(0)], DataType::Utf8);
            assert!(
                agrees_with_rows(&call, &batch, DataType::Utf8),
                "a coded column has a kernel"
            );
            let answer = call
                .evaluate_column(&batch, Some(DataType::Utf8))
                .expect("a coded column has a kernel");
            let (typed, _) = answer.typed().expect("typed");
            let crate::batch::TypedValues::Utf8(text) = typed else {
                panic!("text answers as text");
            };
            let (codes, distinct) = text.dictionary().expect("the answer stays coded");
            assert_eq!(codes.len(), values.len());
            // Three colours, plus the placeholder the NULL row codes to:
            // one entry per distinct stored value, not one per row.
            assert_eq!(
                distinct.len(),
                4,
                "one entry per distinct input, not one per row"
            );
        }
        // LIKE reads the same dictionary and answers a plain Boolean column.
        let call = scalar(
            ScalarFunction::Like {
                negated: false,
                escape: None,
            },
            vec![
                CompiledExpr::Column(0),
                CompiledExpr::Literal(Value::Utf8("re%".to_owned())),
            ],
            DataType::Boolean,
        );
        assert!(agrees_with_rows(&call, &batch, DataType::Boolean));
    }

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
