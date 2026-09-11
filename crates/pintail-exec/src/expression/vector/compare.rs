//! Comparisons, `IS NULL` and three-valued logic a batch at a time.
//!
//! Row evaluation compares integers as integers and everything textual -
//! temporals included, which it reads as canonical text - under the node's
//! collation. These kernels compare the same way where the packed form
//! orders as that text does: integers by value, temporals of one type by
//! the units their canonical text is spelled from, and text by its bytes
//! under the same collation, without building a value per cell.

use std::cmp::Ordering;

use pintail_sql::BinaryOp;
use pintail_types::{DataType, Value};

use super::numeric::{integers, same_scale_units};
use super::temporal::{Temporal, temporal_column};
use super::{Effects, Operand, operand, truth_column};
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{ColumnVector, RecordBatch, TypedValues};
use crate::collation::Collation;
use crate::expression::{CompiledExpr, compare_utf8_mysql, evaluate_logic};

const fn holds(op: BinaryOp, ordering: Ordering) -> bool {
    match op {
        BinaryOp::Equal => ordering.is_eq(),
        BinaryOp::NotEqual => ordering.is_ne(),
        BinaryOp::Less => ordering.is_lt(),
        BinaryOp::LessOrEqual => ordering.is_le(),
        BinaryOp::Greater => ordering.is_gt(),
        _ => ordering.is_ge(),
    }
}

/// Each row's ordering of the two sides, `None` where either is NULL.
type Orderings = Vec<Option<Ordering>>;

pub(super) fn comparison_column(
    batch: &RecordBatch,
    op: BinaryOp,
    left: &CompiledExpr,
    right: &CompiledExpr,
    collation: Collation,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    if data_type.is_some_and(|declared| declared != DataType::Boolean) {
        return None;
    }
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    if !left.varies() && !right.varies() {
        return None;
    }
    let rows = batch.row_count();
    let equality = matches!(op, BinaryOp::Equal | BinaryOp::NotEqual);
    let orderings = integer_orderings(&left, &right, rows)
        .or_else(|| temporal_orderings(&left, &right, rows))
        .or_else(|| {
            equality
                .then(|| decimal_equalities(&left, &right, rows))
                .flatten()
        })
        .or_else(|| text_orderings(&left, &right, rows, collation))?;
    truth_column(
        orderings
            .into_iter()
            .map(|ordering| ordering.map(|ordering| holds(op, ordering))),
    )
}

fn integer_orderings(left: &Operand<'_>, right: &Operand<'_>, rows: usize) -> Option<Orderings> {
    let (left, right) = (integers(left)?, integers(right)?);
    Some(
        (0..rows)
            .map(|row| Some(left.at(row)?.cmp(&right.at(row)?)))
            .collect(),
    )
}

/// Decimals of one type: row evaluation compares their canonical texts,
/// which are equal exactly when their units are. Only equality reads
/// these orderings; decimal order is bound as its own comparison.
fn decimal_equalities(left: &Operand<'_>, right: &Operand<'_>, rows: usize) -> Option<Orderings> {
    let data_type = [left, right]
        .into_iter()
        .find_map(|operand| match operand {
            Operand::Column(column) => Some(column.data_type()),
            Operand::Constant(_) => None,
        })?;
    let left = same_scale_units(left, data_type, rows)?;
    let right = same_scale_units(right, data_type, rows)?;
    Some(
        left.into_iter()
            .zip(right)
            .map(|(left, right)| Some(left?.cmp(&right?)))
            .collect(),
    )
}

/// One side of a temporal comparison: a packed column, or a constant
/// already in that column type's canonical text.
enum Spelled<'operand> {
    Column(Temporal<'operand>),
    Fixed(i64),
}

impl Spelled<'_> {
    fn at(&self, row: usize) -> Option<i64> {
        match self {
            Self::Fixed(unit) => Some(*unit),
            Self::Column(column) if !column.validity.is_valid(row) => None,
            Self::Column(column) => Some(column.spelled(row)),
        }
    }
}

fn spelled<'operand>(
    operand: &'operand Operand<'_>,
    data_type: DataType,
) -> Option<Spelled<'operand>> {
    match operand {
        Operand::Column(column) => temporal_column(column).map(Spelled::Column),
        // The binder hands a literal compared with a temporal over in that
        // type's canonical text; any other spelling is row evaluation's.
        Operand::Constant(Value::Utf8(text)) => {
            let (unit, canonical) = match data_type {
                DataType::Date32 => {
                    let days = pintail_types::parse_date_days(text)?;
                    (days, pintail_types::format_date_days(days)?)
                }
                DataType::DateTime64 { fsp } => {
                    let micros = pintail_types::parse_datetime_micros(text)?;
                    (micros, pintail_types::format_datetime_micros(micros, fsp)?)
                }
                _ => return None,
            };
            (canonical == *text).then_some(Spelled::Fixed(unit))
        }
        Operand::Constant(_) => None,
    }
}

/// Temporals of one type, whose canonical texts share one width, so text
/// order is unit order wherever the year has four digits.
fn temporal_orderings(left: &Operand<'_>, right: &Operand<'_>, rows: usize) -> Option<Orderings> {
    let kind = |operand: &Operand<'_>| match operand {
        Operand::Column(column) => Some(column.data_type()),
        Operand::Constant(_) => None,
    };
    let data_type = kind(left).or_else(|| kind(right))?;
    if [kind(left), kind(right)]
        .into_iter()
        .flatten()
        .any(|kind| kind != data_type)
    {
        return None;
    }
    let (left, right) = (spelled(left, data_type)?, spelled(right, data_type)?);
    let years = match (&left, &right) {
        (Spelled::Column(column), _) | (_, Spelled::Column(column)) => column.four_digit_years(),
        _ => return None,
    };
    let mut orderings = Vec::with_capacity(rows);
    for row in 0..rows {
        let (Some(from), Some(to)) = (left.at(row), right.at(row)) else {
            orderings.push(None);
            continue;
        };
        if !years.contains(&from) || !years.contains(&to) {
            return None;
        }
        orderings.push(Some(from.cmp(&to)));
    }
    Some(orderings)
}

/// One side of a text comparison.
enum Text<'operand> {
    Column(&'operand StrColumn, &'operand ValidityMask),
    Fixed(&'operand str),
}

/// What reading one row of a [`Text`] gave.
enum Read<R> {
    Text(R),
    Null,
    /// Bytes that are not UTF-8, which row evaluation reads lossily.
    Unreadable,
}

impl Text<'_> {
    /// `f` over row `row`'s text.
    fn with<R>(&self, row: usize, f: impl FnOnce(&str) -> R) -> Read<R> {
        match self {
            Self::Fixed(text) => Read::Text(f(text)),
            Self::Column(_, validity) if !validity.is_valid(row) => Read::Null,
            Self::Column(column, _) => column.views()[row].with_bytes(column.heap(), |bytes| {
                std::str::from_utf8(bytes).map_or(Read::Unreadable, |text| Read::Text(f(text)))
            }),
        }
    }
}

fn text<'operand>(operand: &'operand Operand<'_>) -> Option<Text<'operand>> {
    match operand {
        Operand::Column(column) if column.data_type() == DataType::Utf8 => match column.typed()? {
            (TypedValues::Utf8(text), validity) => Some(Text::Column(text, validity)),
            _ => None,
        },
        Operand::Constant(Value::Utf8(text)) => Some(Text::Fixed(text)),
        _ => None,
    }
}

fn text_orderings(
    left: &Operand<'_>,
    right: &Operand<'_>,
    rows: usize,
    collation: Collation,
) -> Option<Orderings> {
    let (left, right) = (text(left)?, text(right)?);
    let mut orderings = Vec::with_capacity(rows);
    for row in 0..rows {
        let compared = left.with(row, |left| {
            right.with(row, |right| compare_utf8_mysql(left, right, collation))
        });
        orderings.push(match compared {
            Read::Text(Read::Text(ordering)) => Some(ordering),
            Read::Null | Read::Text(Read::Null) => None,
            Read::Unreadable | Read::Text(Read::Unreadable) => return None,
        });
    }
    Some(orderings)
}

/// Whether row `row` of `column` is NULL, read from its validity where it
/// is packed.
fn is_null(column: &ColumnVector, row: usize) -> bool {
    match column.typed() {
        Some((_, validity)) => !validity.is_valid(row),
        None => matches!(column.value(row), Some(Value::Null) | None),
    }
}

pub(super) fn is_null_column(
    batch: &RecordBatch,
    expr: &CompiledExpr,
    negated: bool,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    if data_type.is_some_and(|declared| declared != DataType::Boolean) {
        return None;
    }
    let Operand::Column(column) = operand(batch, expr, effects)? else {
        return None;
    };
    truth_column((0..batch.row_count()).map(|row| Some(is_null(&column, row) != negated)))
}

/// `AND`, `OR` and `XOR` over per-row truth values, as row evaluation
/// combines them.
pub(super) fn logic_column(
    batch: &RecordBatch,
    op: BinaryOp,
    left: &CompiledExpr,
    right: &CompiledExpr,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    if data_type.is_some_and(|declared| declared != DataType::Boolean) {
        return None;
    }
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    if !left.varies() && !right.varies() {
        return None;
    }
    let value = |operand: &Operand<'_>, row: usize| -> Option<Value> {
        match operand {
            Operand::Column(column) => column.value_owned(row),
            Operand::Constant(value) => Some((*value).clone()),
        }
    };
    let mut answers = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        let answer = evaluate_logic(op, &value(&left, row)?, &value(&right, row)?);
        answers.push(match answer {
            Ok(Value::Boolean(answer)) => Some(answer),
            Ok(Value::Null) => None,
            _ if batch.selection().is_selected(row) => return None,
            _ => None,
        });
    }
    truth_column(answers.into_iter())
}

#[cfg(test)]
mod tests {
    use pintail_sql::{BinaryOp, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, binary, scalar};
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, LazyText, RecordBatch, TypedValues};
    use crate::collation::Collation;
    use crate::expression::CompiledExpr;

    const COMPARISONS: [BinaryOp; 6] = [
        BinaryOp::Equal,
        BinaryOp::NotEqual,
        BinaryOp::Less,
        BinaryOp::LessOrEqual,
        BinaryOp::Greater,
        BinaryOp::GreaterOrEqual,
    ];

    fn temporal(data_type: DataType, texts: &[Option<&str>]) -> ColumnVector {
        let (units, text) = match data_type {
            DataType::Date32 => (
                texts
                    .iter()
                    .map(|text| text.and_then(pintail_types::parse_date_days).unwrap_or(0))
                    .collect(),
                LazyText::date(),
            ),
            DataType::DateTime64 { fsp } => (
                texts
                    .iter()
                    .map(|text| {
                        text.and_then(pintail_types::parse_datetime_micros)
                            .unwrap_or(0)
                    })
                    .collect(),
                LazyText::datetime(fsp),
            ),
            _ => unreachable!("temporal types"),
        };
        ColumnVector::from_typed(
            data_type,
            TypedValues::Temporal { units, text },
            ValidityMask::from_bools(&texts.iter().map(Option::is_some).collect::<Vec<_>>()),
        )
    }

    fn texts(values: &[Option<&str>]) -> ColumnVector {
        ColumnVector::new(
            DataType::Utf8,
            values
                .iter()
                .map(|value| value.map_or(Value::Null, |text| Value::Utf8(text.to_owned())))
                .collect(),
        )
        .expect("text column")
    }

    fn integers(values: &[Option<i64>]) -> ColumnVector {
        ColumnVector::new(
            DataType::Int64,
            values
                .iter()
                .map(|value| value.map_or(Value::Null, Value::Int64))
                .collect(),
        )
        .expect("integer column")
    }

    /// Columns: two integers, a date, two datetimes, two texts.
    fn fixture() -> RecordBatch {
        batch_of(vec![
            integers(&[
                Some(1),
                Some(-5),
                None,
                Some(7),
                Some(0),
                Some(9),
                Some(i64::MIN),
                Some(3),
            ]),
            integers(&[
                Some(1),
                Some(5),
                Some(2),
                None,
                Some(-1),
                Some(9),
                Some(0),
                Some(4),
            ]),
            temporal(
                DataType::Date32,
                &[
                    Some("2024-06-15"),
                    Some("0001-01-01"),
                    None,
                    Some("2024-06-15"),
                    Some("9999-12-31"),
                    Some("1999-12-31"),
                    Some("2024-06-14"),
                    Some("2024-06-16"),
                ],
            ),
            temporal(
                DataType::DateTime64 { fsp: 0 },
                &[
                    Some("2024-06-15 00:00:00"),
                    Some("0001-01-01 00:00:01"),
                    Some("2024-01-01 12:00:00"),
                    None,
                    Some("9999-12-31 23:59:59"),
                    Some("1999-12-31 00:00:00"),
                    Some("2024-06-15 00:00:00"),
                    Some("2024-06-15 10:00:00"),
                ],
            ),
            temporal(
                DataType::DateTime64 { fsp: 6 },
                &[
                    Some("2024-06-15 00:00:00.000000"),
                    Some("0001-01-01 00:00:00.500000"),
                    Some("2024-01-01 12:00:00.000001"),
                    Some("2024-01-01 12:00:00.000000"),
                    None,
                    Some("1999-12-31 00:00:00.000000"),
                    Some("2024-06-15 00:00:00.999999"),
                    Some("2024-06-15 10:00:00.000000"),
                ],
            ),
            texts(&[
                Some("alpha"),
                Some("Alpha"),
                Some("beta"),
                None,
                Some(""),
                Some("alpha "),
                Some("\u{e9}t\u{e9}"),
                Some("ETE"),
            ]),
            texts(&[
                Some("ALPHA"),
                Some("alpha"),
                None,
                Some("gamma"),
                Some(" "),
                Some("alpha"),
                Some("ete"),
                Some("\u{e9}t\u{e9}"),
            ]),
        ])
    }

    fn column(index: usize) -> CompiledExpr {
        CompiledExpr::Column(index)
    }

    fn text(value: &str) -> CompiledExpr {
        CompiledExpr::Literal(Value::Utf8(value.to_owned()))
    }

    fn cast(argument: CompiledExpr, target: DataType) -> CompiledExpr {
        scalar(ScalarFunction::Cast(target), vec![argument], target)
    }

    fn with_collation(expression: CompiledExpr, name: &str) -> CompiledExpr {
        match expression {
            CompiledExpr::Binary {
                op,
                left,
                right,
                data_type,
                overflow,
                ..
            } => CompiledExpr::Binary {
                op,
                left,
                right,
                data_type,
                collation: Collation::from_mysql_name(name).expect("collation"),
                overflow,
            },
            other => other,
        }
    }

    #[test]
    fn comparisons_match_row_evaluation() {
        let batch = fixture();
        let six = DataType::DateTime64 { fsp: 6 };
        let pairs = [
            (column(0), column(1)),
            (column(0), CompiledExpr::Literal(Value::Int64(3))),
            (CompiledExpr::Literal(Value::UInt64(1)), column(1)),
            (column(2), text("2024-06-15")),
            (column(3), text("2024-06-15 00:00:00")),
            (column(4), column(4)),
            // Two temporal types meet as DATETIME(6), as the binder casts them.
            (cast(column(2), six), cast(column(3), six)),
            (cast(column(3), six), column(4)),
            (column(5), column(6)),
            (column(5), text("alpha")),
        ];
        for op in COMPARISONS {
            for (left, right) in &pairs {
                let expression = binary(op, left.clone(), right.clone(), DataType::Boolean);
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Boolean),
                    "{expression:?} has a kernel"
                );
                for name in ["utf8mb4_general_ci", "utf8mb4_0900_ai_ci", "utf8mb4_bin"] {
                    let expression = with_collation(expression.clone(), name);
                    assert!(agrees_with_rows(&expression, &batch, DataType::Boolean));
                }
            }
        }
    }

    #[test]
    fn a_temporal_literal_not_in_canonical_text_is_row_evaluations() {
        let batch = fixture();
        for literal in ["2024-6-15", "2024-06-15 00:00:00", "20240615"] {
            let expression = binary(BinaryOp::Equal, column(2), text(literal), DataType::Boolean);
            assert!(
                expression
                    .evaluate_column(&batch, Some(DataType::Boolean))
                    .is_none(),
                "{literal}"
            );
        }
    }

    #[test]
    fn null_tests_and_logic_match_row_evaluation() {
        let batch = fixture();
        let less = binary(BinaryOp::Less, column(0), column(1), DataType::Boolean);
        let named = binary(BinaryOp::Equal, column(5), text("alpha"), DataType::Boolean);
        let mut expressions = Vec::new();
        for index in [0, 2, 4, 5] {
            for negated in [false, true] {
                expressions.push(CompiledExpr::IsNull {
                    expr: Box::new(column(index)),
                    negated,
                });
            }
        }
        for op in [BinaryOp::And, BinaryOp::Or, BinaryOp::Xor] {
            expressions.push(binary(op, less.clone(), named.clone(), DataType::Boolean));
            expressions.push(binary(
                op,
                less.clone(),
                CompiledExpr::Literal(Value::Null),
                DataType::Boolean,
            ));
            expressions.push(binary(
                op,
                CompiledExpr::IsNull {
                    expr: Box::new(column(3)),
                    negated: false,
                },
                named.clone(),
                DataType::Boolean,
            ));
        }
        for expression in &expressions {
            assert!(
                agrees_with_rows(expression, &batch, DataType::Boolean),
                "{expression:?} has a kernel"
            );
        }
    }
}
