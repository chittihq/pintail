//! Integer arithmetic, exact decimal arithmetic and decimal comparison
//! over packed columns.
//!
//! Row evaluation reads a decimal as text, parses it into an exact
//! fraction, and formats its answer as text again. A decimal column whose
//! text is derived from its scaled units needs neither step: the units are
//! the fraction. Decimal arithmetic runs through the same exact fractions
//! row evaluation uses, node for node, so the digits `MySQL` keeps inside
//! a division and the rounding at the top come out the same.

use std::cmp::Ordering;

use pintail_sql::{BinaryOp, UnaryOp};
use pintail_types::{DataType, Value};

use super::{Effects, Operand, declared, kernel, operand, truth_column};
use crate::array::ValidityMask;
use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, TypedValues};
use crate::expression::{CompiledExpr, DecimalRational, decimal_units_of};

/// An integer operand: a packed signed or unsigned column, or a constant.
pub(super) enum Integers<'operand> {
    Signed(&'operand [i64], &'operand ValidityMask),
    Unsigned(&'operand [u64], &'operand ValidityMask),
    Fixed(i128),
}

impl Integers<'_> {
    /// Row `row`'s value, `None` for NULL.
    pub(super) fn at(&self, row: usize) -> Option<i128> {
        match self {
            Self::Fixed(value) => Some(*value),
            Self::Signed(values, validity) => {
                validity.is_valid(row).then(|| i128::from(values[row]))
            }
            Self::Unsigned(values, validity) => {
                validity.is_valid(row).then(|| i128::from(values[row]))
            }
        }
    }
}

pub(super) fn integers<'operand>(operand: &'operand Operand<'_>) -> Option<Integers<'operand>> {
    match operand {
        Operand::Column(column) => match (column.data_type().storage_type(), column.typed()?) {
            (DataType::Int64, (TypedValues::Int64(values), validity)) => {
                Some(Integers::Signed(values, validity))
            }
            (DataType::UInt64, (TypedValues::UInt64(values), validity)) => {
                Some(Integers::Unsigned(values, validity))
            }
            _ => None,
        },
        Operand::Constant(Value::Int64(value)) => Some(Integers::Fixed(i128::from(*value))),
        Operand::Constant(Value::UInt64(value)) => Some(Integers::Fixed(i128::from(*value))),
        Operand::Constant(_) => None,
    }
}

/// A signed integer operand. Row evaluation reads both sides of signed
/// arithmetic as signed; an unsigned value past the signed range is its
/// error to raise.
fn signed<'operand>(operand: &'operand Operand<'_>) -> Option<Integers<'operand>> {
    integers(operand).filter(|integers| match integers {
        Integers::Unsigned(..) => false,
        Integers::Fixed(value) => i64::try_from(*value).is_ok(),
        Integers::Signed(..) => true,
    })
}

/// `+`, `-` and `*` of signed integers answering a signed integer.
pub(super) fn integer_column(
    batch: &RecordBatch,
    op: BinaryOp,
    left: &CompiledExpr,
    right: &CompiledExpr,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    if data_type.is_some_and(|declared| declared != DataType::Int64) {
        return None;
    }
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    if !left.varies() && !right.varies() {
        return None;
    }
    let (left, right) = (signed(&left)?, signed(&right)?);
    let rows = batch.row_count();
    let mut values = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let (Some(from), Some(by)) = (left.at(row), right.at(row)) else {
            values.push(0);
            valid.push(false);
            continue;
        };
        let (from, by) = (i64::try_from(from).ok()?, i64::try_from(by).ok()?);
        let answer = match op {
            BinaryOp::Add => from.checked_add(by),
            BinaryOp::Subtract => from.checked_sub(by),
            _ => from.checked_mul(by),
        };
        match answer {
            Some(answer) => {
                values.push(answer);
                valid.push(true);
            }
            // Out of range: row evaluation's error, for a row it reads.
            None if batch.selection().is_selected(row) => return None,
            None => {
                values.push(0);
                valid.push(false);
            }
        }
    }
    Some(ColumnVector::from_typed(
        DataType::Int64,
        TypedValues::Int64(values),
        ValidityMask::from_bools(&valid),
    ))
}

/// A decimal operand's scaled units: a packed decimal column whose text is
/// derived from them, an integer column (scale zero), or a constant.
enum Scaled<'operand> {
    Decimal(&'operand DecimalUnits, u8, &'operand ValidityMask),
    Integers(Integers<'operand>),
    Fixed(i128, u8),
}

impl Scaled<'_> {
    /// Row `row` as units and scale, `None` for NULL.
    fn at(&self, row: usize) -> Option<(i128, u8)> {
        match self {
            Self::Fixed(units, scale) => Some((*units, *scale)),
            Self::Decimal(units, scale, validity) => validity
                .is_valid(row)
                .then(|| units.get(row).map(|units| (units, *scale)))
                .flatten(),
            Self::Integers(integers) => integers.at(row).map(|value| (value, 0)),
        }
    }
}

fn scaled<'operand>(operand: &'operand Operand<'_>) -> Option<Scaled<'operand>> {
    match operand {
        Operand::Column(column) => match column.typed()? {
            (
                TypedValues::Decimal128 {
                    values,
                    scale,
                    text,
                },
                validity,
            ) if text.derived() && matches!(column.data_type(), DataType::Decimal { .. }) => {
                Some(Scaled::Decimal(values, *scale, validity))
            }
            _ => integers(operand).map(Scaled::Integers),
        },
        Operand::Constant(Value::Null) => None,
        Operand::Constant(value) => {
            decimal_units_of(value).map(|(units, scale)| Scaled::Fixed(units, scale))
        }
    }
}

/// Digits a DECIMAL of `precision` and `scale` holds left of its point.
const fn integer_digits(precision: u8, scale: u8) -> u8 {
    precision.saturating_sub(scale)
}

/// `CAST(x AS DECIMAL(p, s))` of a decimal or integer that the target holds
/// exactly: at least as many fraction digits, room for every integer
/// digit. Comparisons bind both sides as such casts to one type. A cast
/// that rounds or overflows is row evaluation's.
pub(super) fn decimal_cast_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    target: DataType,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [argument] = args else {
        return None;
    };
    let DataType::Decimal { precision, scale } = target else {
        return None;
    };
    if data_type.is_some_and(|declared| declared != target) {
        return None;
    }
    // A cast reads a computed decimal's internal digits, which its answer's
    // units no longer hold, so only a column or a constant is cast here. A
    // constant is cast too: comparisons cast their literal side to the
    // common type, and the comparison above needs it as a column.
    if !matches!(argument, CompiledExpr::Column(_) | CompiledExpr::Literal(_))
        || matches!(argument, CompiledExpr::Literal(Value::DecimalAverage(_)))
    {
        return None;
    }
    let input = operand(batch, argument, effects)?;
    if let Operand::Column(column) = &input
        && let DataType::Decimal {
            precision: from_precision,
            scale: from_scale,
        } = column.data_type()
        && (from_scale > scale
            || integer_digits(from_precision, from_scale) > integer_digits(precision, scale))
    {
        return None;
    }
    let input = scaled(&input)?;
    let limit = 10_i128.checked_pow(u32::from(precision))?;
    let mut units = Vec::with_capacity(batch.row_count());
    let mut valid = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        let Some((value, from_scale)) = input.at(row) else {
            units.push(0);
            valid.push(false);
            continue;
        };
        let widened = 10_i128
            .checked_pow(u32::from(scale.checked_sub(from_scale)?))
            .and_then(|factor| value.checked_mul(factor))
            .filter(|widened| widened.unsigned_abs() < limit.unsigned_abs())?;
        units.push(widened);
        valid.push(true);
    }
    Some(ColumnVector::from_typed(
        target,
        TypedValues::Decimal128 {
            values: DecimalUnits::Wide(units),
            scale,
            text: LazyText::decimal(scale),
        },
        ValidityMask::from_bools(&valid),
    ))
}

/// Each row's decimal units at `scale`, for decimals of one type or a
/// constant written in that type's canonical text, `None` for NULL. Their
/// texts are equal exactly when their units are.
pub(super) fn same_scale_units(
    operand: &Operand<'_>,
    data_type: DataType,
    rows: usize,
) -> Option<Vec<Option<i128>>> {
    let DataType::Decimal { scale, .. } = data_type else {
        return None;
    };
    match operand {
        Operand::Column(column) if column.data_type() == data_type => {
            let Scaled::Decimal(units, _, validity) = scaled(operand)? else {
                return None;
            };
            Some(
                (0..rows)
                    .map(|row| validity.is_valid(row).then(|| units.get(row)).flatten())
                    .collect(),
            )
        }
        Operand::Constant(Value::Utf8(text)) => {
            let units = pintail_types::parse_decimal_scaled(text, scale)?;
            (pintail_types::format_decimal_scaled(units, scale) == *text)
                .then(|| vec![Some(units); rows])
        }
        _ => None,
    }
}

/// A decimal comparison of two exact numbers, each rescaled to the larger
/// scale.
pub(super) fn decimal_comparison_column(
    batch: &RecordBatch,
    op: BinaryOp,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [left, right] = args else {
        return None;
    };
    if data_type.is_some_and(|declared| declared != DataType::Boolean) {
        return None;
    }
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    if !left.varies() && !right.varies() {
        return None;
    }
    let (left, right) = (scaled(&left)?, scaled(&right)?);
    let mut answers = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        let (Some((left, left_scale)), Some((right, right_scale))) = (left.at(row), right.at(row))
        else {
            answers.push(None);
            continue;
        };
        let common = left_scale.max(right_scale);
        let rescale = |units: i128, scale: u8| {
            10_i128
                .checked_pow(u32::from(common - scale))
                .and_then(|factor| units.checked_mul(factor))
        };
        let ordering = rescale(left, left_scale)?.cmp(&rescale(right, right_scale)?);
        answers.push(Some(match op {
            BinaryOp::Equal => ordering == Ordering::Equal,
            BinaryOp::NotEqual => ordering != Ordering::Equal,
            BinaryOp::Less => ordering == Ordering::Less,
            BinaryOp::LessOrEqual => ordering != Ordering::Greater,
            BinaryOp::Greater => ordering == Ordering::Greater,
            BinaryOp::GreaterOrEqual => ordering != Ordering::Less,
            _ => return None,
        }));
    }
    truth_column(answers.into_iter())
}

/// A decimal subtree's exact value per row, or one value for every row.
enum Exact {
    Rows(Vec<Option<DecimalRational>>),
    Fixed(Option<DecimalRational>),
}

impl Exact {
    fn at(&self, row: usize) -> Option<DecimalRational> {
        match self {
            Self::Rows(rows) => rows[row],
            Self::Fixed(value) => *value,
        }
    }
}

/// `expr` as exact fractions, node for node as row evaluation's decimal
/// chain takes it; `None` declines the batch.
fn exact(expr: &CompiledExpr, batch: &RecordBatch, effects: &mut Effects) -> Option<Exact> {
    let rows = batch.row_count();
    match expr {
        CompiledExpr::Literal(Value::Null) => Some(Exact::Fixed(None)),
        CompiledExpr::Literal(value) => DecimalRational::from_value(value)
            .ok()
            .flatten()
            .map(|value| Exact::Fixed(Some(value))),
        CompiledExpr::Unary {
            op: op @ (UnaryOp::Plus | UnaryOp::Minus),
            expr: inner,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => {
            let inner = exact(inner, batch, effects)?;
            if *op == UnaryOp::Plus {
                return Some(inner);
            }
            Some(match inner {
                Exact::Fixed(value) => {
                    Exact::Fixed(value.map(DecimalRational::negated).transpose().ok()?)
                }
                Exact::Rows(values) => Exact::Rows(
                    values
                        .into_iter()
                        .map(|value| value.map(DecimalRational::negated).transpose())
                        .collect::<Result<_, _>>()
                        .ok()?,
                ),
            })
        }
        CompiledExpr::Binary {
            op: op @ (BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide),
            left,
            right,
            data_type: Some(DataType::Decimal { scale, .. }),
            ..
        } => {
            let (left, right) = (exact(left, batch, effects)?, exact(right, batch, effects)?);
            // The digits a division keeps for its parent: `MySQL` holds
            // fractions in base-1e9 words.
            let internal_scale = (*scale).max(1).div_ceil(9).saturating_mul(9);
            let mut values = Vec::with_capacity(rows);
            for row in 0..rows {
                let (Some(left), Some(right)) = (left.at(row), right.at(row)) else {
                    values.push(None);
                    continue;
                };
                values.push(match op {
                    BinaryOp::Add => Some(left.add_sub(right, false).ok()?),
                    BinaryOp::Subtract => Some(left.add_sub(right, true).ok()?),
                    BinaryOp::Multiply => Some(left.multiply(right).ok()?),
                    _ if right.numerator == 0 => {
                        effects.division_by_zero(batch, row);
                        None
                    }
                    _ => Some(left.divide(right).ok()??.truncated(internal_scale).ok()?),
                });
            }
            Some(Exact::Rows(values))
        }
        // Anything else is read at its own answer's value, as row
        // evaluation reads the boundary of its chain.
        leaf => {
            let column = match leaf {
                CompiledExpr::Column(index) => std::borrow::Cow::Borrowed(batch.column(*index)?),
                nested => {
                    std::borrow::Cow::Owned(kernel(nested, batch, declared(nested), effects)?)
                }
            };
            let leaf = Operand::Column(column);
            let scaled = scaled(&leaf)?;
            let mut values = Vec::with_capacity(rows);
            for row in 0..rows {
                values.push(match scaled.at(row) {
                    None => None,
                    Some((units, scale)) => Some(
                        DecimalRational::new(units, 10_i128.checked_pow(u32::from(scale))?).ok()?,
                    ),
                });
            }
            Some(Exact::Rows(values))
        }
    }
}

/// Decimal `+`, `-`, `*` and `/`, evaluated exactly and rounded once to the
/// declared scale.
pub(super) fn decimal_chain_column(
    expr: &CompiledExpr,
    batch: &RecordBatch,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let own = declared(expr)?;
    let DataType::Decimal { scale, .. } = own else {
        return None;
    };
    if data_type.is_some_and(|declared| declared != own) {
        return None;
    }
    let Exact::Rows(values) = exact(expr, batch, effects)? else {
        return None;
    };
    let mut units = Vec::with_capacity(values.len());
    let mut valid = Vec::with_capacity(values.len());
    for value in values {
        units.push(match value {
            Some(value) => value.rounded_units(scale).ok()?,
            None => 0,
        });
        valid.push(value.is_some());
    }
    Some(ColumnVector::from_typed(
        own,
        TypedValues::Decimal128 {
            values: DecimalUnits::Wide(units),
            scale,
            text: LazyText::decimal(scale),
        },
        ValidityMask::from_bools(&valid),
    ))
}

#[cfg(test)]
mod tests {
    use pintail_sql::{BinaryOp, ScalarFunction, UnaryOp};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, binary, scalar};
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, TypedValues};
    use crate::expression::CompiledExpr;

    fn decimal(precision: u8, scale: u8, units: &[Option<i64>]) -> ColumnVector {
        ColumnVector::from_typed(
            DataType::Decimal { precision, scale },
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(
                    units.iter().map(|units| units.unwrap_or(0)).collect(),
                ),
                scale,
                text: LazyText::decimal(scale),
            },
            ValidityMask::from_bools(&units.iter().map(Option::is_some).collect::<Vec<_>>()),
        )
    }

    fn signed(values: &[Option<i64>]) -> ColumnVector {
        ColumnVector::from_typed(
            DataType::Int64,
            TypedValues::Int64(values.iter().map(|value| value.unwrap_or(0)).collect()),
            ValidityMask::from_bools(&values.iter().map(Option::is_some).collect::<Vec<_>>()),
        )
    }

    /// Columns: a DECIMAL(12,2), b DECIMAL(10,4) with zeros, an INT.
    /// Row 3 is unselected and holds what would fail a selected row.
    fn fixture() -> RecordBatch {
        batch_of(vec![
            decimal(
                12,
                2,
                &[
                    Some(1050),
                    Some(-333),
                    Some(0),
                    Some(99_999),
                    None,
                    Some(7),
                    Some(123_456_789),
                    Some(-1),
                ],
            ),
            decimal(
                10,
                4,
                &[
                    Some(30_000),
                    Some(0),
                    Some(12_345),
                    Some(0),
                    Some(5),
                    None,
                    Some(-70_000),
                    Some(1),
                ],
            ),
            signed(&[
                Some(3),
                Some(-7),
                None,
                Some(i64::MAX),
                Some(0),
                Some(11),
                Some(-2),
                Some(1),
            ]),
        ])
    }

    const fn decimal_type(scale: u8) -> DataType {
        DataType::Decimal {
            precision: 30,
            scale,
        }
    }

    fn column(index: usize) -> CompiledExpr {
        CompiledExpr::Column(index)
    }

    fn literal(value: Value) -> CompiledExpr {
        CompiledExpr::Literal(value)
    }

    /// The kernel answers, agrees with row evaluation, and records the
    /// divisions by zero row evaluation records - for selected rows only.
    fn agrees_with_warnings(expression: &CompiledExpr, batch: &RecordBatch, data_type: DataType) {
        let _ = crate::execution::take_session_division_warnings();
        let column = expression
            .evaluate_column(batch, Some(data_type))
            .unwrap_or_else(|| panic!("{expression:?} has a kernel"));
        let kernel_warnings = crate::execution::take_session_division_warnings();
        for row in batch.selection().selected_rows() {
            let expected = expression.evaluate(batch, row).expect("row evaluation");
            assert_eq!(
                column.value(row),
                Some(&expected),
                "{expression:?} at row {row}"
            );
        }
        let row_warnings = crate::execution::take_session_division_warnings();
        assert_eq!(kernel_warnings, row_warnings, "{expression:?} warnings");
    }

    #[test]
    fn decimal_arithmetic_matches_row_evaluation_and_its_warnings() {
        let batch = fixture();
        let a_over_b = binary(BinaryOp::Divide, column(0), column(1), decimal_type(6));
        let expressions = [
            (
                binary(BinaryOp::Add, column(0), column(1), decimal_type(4)),
                4,
            ),
            (
                binary(BinaryOp::Subtract, column(0), column(1), decimal_type(4)),
                4,
            ),
            (
                binary(BinaryOp::Multiply, column(0), column(1), decimal_type(6)),
                6,
            ),
            (a_over_b.clone(), 6),
            (
                binary(
                    BinaryOp::Divide,
                    column(0),
                    literal(Value::Int64(7)),
                    decimal_type(6),
                ),
                6,
            ),
            (
                binary(BinaryOp::Multiply, column(0), column(2), decimal_type(2)),
                2,
            ),
            // A division inside a product keeps its internal digits.
            (
                binary(
                    BinaryOp::Multiply,
                    binary(
                        BinaryOp::Divide,
                        column(0),
                        literal(Value::Int64(3)),
                        decimal_type(6),
                    ),
                    literal(Value::Int64(3)),
                    decimal_type(6),
                ),
                6,
            ),
            (
                binary(
                    BinaryOp::Divide,
                    binary(
                        BinaryOp::Add,
                        column(0),
                        literal(Value::Utf8("1.5".to_owned())),
                        decimal_type(2),
                    ),
                    column(1),
                    decimal_type(6),
                ),
                6,
            ),
            (
                binary(
                    BinaryOp::Subtract,
                    CompiledExpr::Unary {
                        op: UnaryOp::Minus,
                        expr: Box::new(column(0)),
                        data_type: Some(decimal_type(2)),
                        overflow: None,
                    },
                    a_over_b,
                    decimal_type(6),
                ),
                6,
            ),
        ];
        for (expression, scale) in &expressions {
            agrees_with_warnings(expression, &batch, decimal_type(*scale));
        }
    }

    #[test]
    fn integer_arithmetic_matches_row_evaluation() {
        let batch = fixture();
        for op in [BinaryOp::Add, BinaryOp::Subtract, BinaryOp::Multiply] {
            // Row 3 overflows the sum and the product, but is not selected.
            let expression = binary(op, column(2), literal(Value::Int64(2)), DataType::Int64);
            assert!(
                agrees_with_rows(&expression, &batch, DataType::Int64),
                "{op:?}"
            );
        }
        // Selected, the overflow is row evaluation's error to raise.
        let mut batch = fixture();
        batch
            .set_selection(crate::batch::SelectionMask::all(batch.row_count()))
            .expect("selection");
        let expression = binary(
            BinaryOp::Add,
            column(2),
            literal(Value::Int64(2)),
            DataType::Int64,
        );
        assert!(
            expression
                .evaluate_column(&batch, Some(DataType::Int64))
                .is_none()
        );
    }

    #[test]
    fn widening_decimal_casts_and_equality_match_row_evaluation() {
        let batch = fixture();
        let wide = DataType::Decimal {
            precision: 24,
            scale: 4,
        };
        let cast =
            |argument: CompiledExpr| scalar(ScalarFunction::Cast(wide), vec![argument], wide);
        for argument in [column(0), column(1), column(2)] {
            assert!(
                agrees_with_rows(&cast(argument.clone()), &batch, wide),
                "{argument:?} widens"
            );
        }
        // Equality between decimals is bound as a comparison of both sides
        // cast to one type.
        for op in [BinaryOp::Equal, BinaryOp::NotEqual] {
            for (left, right) in [
                (cast(column(0)), cast(column(1))),
                (cast(column(0)), cast(column(2))),
                (cast(column(0)), literal(Value::Utf8("10.5000".to_owned()))),
                (cast(column(1)), cast(literal(Value::Int64(0)))),
            ] {
                let expression = binary(op, left, right, DataType::Boolean);
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Boolean),
                    "{expression:?} has a kernel"
                );
            }
        }
        // A cast that drops fraction digits rounds; row evaluation's.
        let narrow = DataType::Decimal {
            precision: 12,
            scale: 1,
        };
        let rounding = scalar(ScalarFunction::Cast(narrow), vec![column(0)], narrow);
        assert!(rounding.evaluate_column(&batch, Some(narrow)).is_none());
        // A cast reads a division's internal digits, not its answer's.
        let wider = DataType::Decimal {
            precision: 30,
            scale: 8,
        };
        let internal = scalar(
            ScalarFunction::Cast(wider),
            vec![binary(
                BinaryOp::Divide,
                column(0),
                literal(Value::Int64(3)),
                decimal_type(6),
            )],
            wider,
        );
        assert!(internal.evaluate_column(&batch, Some(wider)).is_none());
    }

    #[test]
    fn decimal_comparisons_match_row_evaluation() {
        let batch = fixture();
        for op in [
            BinaryOp::Equal,
            BinaryOp::NotEqual,
            BinaryOp::Less,
            BinaryOp::LessOrEqual,
            BinaryOp::Greater,
            BinaryOp::GreaterOrEqual,
        ] {
            for args in [
                vec![column(0), column(1)],
                vec![column(0), column(2)],
                vec![column(1), literal(Value::Utf8("1.2345".to_owned()))],
                vec![literal(Value::Int64(0)), column(1)],
            ] {
                let expression = scalar(
                    ScalarFunction::DecimalComparison { op },
                    args,
                    DataType::Boolean,
                );
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Boolean),
                    "{expression:?} has a kernel"
                );
            }
        }
    }
}
