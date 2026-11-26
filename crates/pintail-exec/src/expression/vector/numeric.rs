//! Integer arithmetic, exact decimal arithmetic and decimal comparison
//! over packed columns.
//!
//! Row evaluation reads a decimal as text, parses it into an exact
//! fraction, and formats its answer as text again. A packed decimal column
//! needs neither step: its units are the fraction, whatever spelling the
//! text beside them carries. Decimal arithmetic runs through the same
//! exact fractions
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

/// A decimal operand's scaled units: a packed decimal column, an integer
/// column (scale zero), or a constant.
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
            ) if matches!(column.data_type(), DataType::Decimal { .. }) => {
                // The text's spelling does not matter here, only the units,
                // and they are always the exact value: every construction
                // of a packed decimal parses each row with
                // `parse_decimal_scaled`, which answers `None` rather than
                // rounding when a digit past the scale is not zero, and
                // abandons the packed column when any row answers `None`.
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

/// A TIME column as `[-]HHMMSS[.ffffff]` units at `scale`, with its
/// validity. `None` when any row's text is not a TIME the strict parser
/// accepts, which is row evaluation's to answer.
fn time_numbers(
    column: &ColumnVector,
    scale: u8,
) -> Option<(Vec<i128>, crate::array::ValidityMask)> {
    let (TypedValues::Utf8(text), validity) = column.typed()? else {
        return None;
    };
    // The number is built at microsecond scale and rounded down to the
    // target's, half away from zero, which is what a cast to a narrower
    // decimal does: `99:59:59.999999` at scale zero is 995960, not 995959.
    let step = 10_i128.checked_pow(u32::from(6_u8.checked_sub(scale)?))?;
    let mut units = Vec::with_capacity(text.len());
    for row in 0..text.len() {
        if !validity.is_valid(row) {
            units.push(0);
            continue;
        }
        let micros = text.views()[row].with_bytes(text.heap(), |bytes| {
            pintail_types::parse_time_micros(std::str::from_utf8(bytes).ok()?)
        })?;
        let magnitude = i128::from(micros.unsigned_abs());
        let seconds = magnitude / 1_000_000;
        let hhmmss = seconds / 3_600 * 10_000 + seconds / 60 % 60 * 100 + seconds % 60;
        let full = hhmmss.checked_mul(1_000_000)? + magnitude % 1_000_000;
        let number = full.checked_add(step / 2)? / step;
        units.push(if micros < 0 { -number } else { number });
    }
    Some((units, validity.clone()))
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
    // A TIME compares as the number `[-]HHMMSS[.ffffff]`, which is what the
    // binder casts both sides of a TIME comparison to. The column carries
    // text, so the row path parses it, builds the number and boxes a value
    // per row, three times over for a BETWEEN; this parses once into a
    // packed decimal and hands the comparison above it packed units.
    if let Operand::Column(column) = &input
        && let DataType::Time64 { .. } = column.data_type()
        && let Some(units) = time_numbers(column, scale)
    {
        let limit = 10_i128.checked_pow(u32::from(precision))?;
        if units
            .0
            .iter()
            .any(|value| value.unsigned_abs() >= limit.unsigned_abs())
        {
            // Let scalar evaluation clamp out-of-range values to the
            // declared precision rather than emitting oversized decimals.
            return None;
        }
        return Some(ColumnVector::from_typed(
            target,
            TypedValues::Decimal128 {
                values: DecimalUnits::Wide(units.0),
                scale,
                text: LazyText::decimal(scale),
            },
            units.1,
        ));
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

/// Whether this operand is an exact number a rescale can compare: a
/// decimal or integer column, or an integer literal. Text and floating
/// point are left out - `MySQL` compares a decimal against either by
/// converting to double, which a rescale would not reproduce.
fn exactly_numeric(operand: &Operand<'_>) -> bool {
    match operand {
        Operand::Column(column) => matches!(
            column.data_type(),
            DataType::Decimal { .. } | DataType::Int64 | DataType::UInt64
        ),
        Operand::Constant(Value::Int64(_) | Value::UInt64(_) | Value::Boolean(_)) => true,
        Operand::Constant(_) => false,
    }
}

/// Whether this operand is a decimal column.
fn is_decimal_column(operand: &Operand<'_>) -> bool {
    matches!(operand, Operand::Column(column) if matches!(column.data_type(), DataType::Decimal { .. }))
}

/// Two scaled decimals compared exactly, without widening either.
///
/// Lifting both to the larger scale is the obvious way and overflows for
/// the shape that needs it most: `DECIMAL(38,0)` against `DECIMAL(20,3)`
/// wants every unit multiplied by a thousand, which leaves `i128`. The
/// integer parts are compared first and settle almost every row; only a
/// tie reaches the fractions, and a fraction is smaller than its own
/// scale's power, so lifting one cannot overflow. Rust truncates division
/// toward zero, so a negative value's two parts are both non-positive and
/// order together.
fn exact_ordering(left: (i128, u8), right: (i128, u8)) -> Option<Ordering> {
    let (left_units, left_scale) = left;
    let (right_units, right_scale) = right;
    // One scale on both sides - two whole numbers, most often - is the
    // units themselves.
    if left_scale == right_scale {
        return Some(left_units.cmp(&right_units));
    }
    let power = |scale: u8| 10_i128.checked_pow(u32::from(scale));
    let (left_power, right_power) = (power(left_scale)?, power(right_scale)?);
    let whole = (left_units / left_power).cmp(&(right_units / right_power));
    if whole != Ordering::Equal {
        return Some(whole);
    }
    let common = left_scale.max(right_scale);
    let lifted =
        |units: i128, scale: u8| power(common - scale).and_then(|factor| units.checked_mul(factor));
    Some(
        lifted(left_units % left_power, left_scale)?
            .cmp(&lifted(right_units % right_power, right_scale)?),
    )
}

/// The expression under a decimal cast that only widens it.
///
/// A comparison binds both sides as casts to one decimal type holding each
/// exactly. That type can need more digits than a packed decimal has -
/// `DECIMAL(38,0)` against `DECIMAL(20,3)` wants 41 - and the cast then
/// declines, taking the whole comparison to the row path. Widening never
/// rounds, so the comparison under the casts is the same one, and it is
/// made at the common scale instead of a common type.
fn under_widening_decimal_cast<'expr>(
    expr: &'expr CompiledExpr,
    batch: &RecordBatch,
) -> &'expr CompiledExpr {
    let CompiledExpr::Scalar { function, args, .. } = expr else {
        return expr;
    };
    let target = match function {
        pintail_sql::ScalarFunction::Cast(target)
        | pintail_sql::ScalarFunction::DeclaredCast { target, .. } => *target,
        _ => return expr,
    };
    let DataType::Decimal { precision, scale } = target else {
        return expr;
    };
    let [CompiledExpr::Column(index)] = args.as_slice() else {
        return expr;
    };
    let Some(source) = batch.column(*index).map(ColumnVector::data_type) else {
        return expr;
    };
    let room = integer_digits(precision, scale);
    match source {
        DataType::Decimal {
            precision: from_precision,
            scale: from_scale,
        } if scale >= from_scale && room >= integer_digits(from_precision, from_scale) => &args[0],
        // i64 spans 19 digits, u64 twenty.
        DataType::Int64 if room >= 19 => &args[0],
        DataType::UInt64 if room >= 20 => &args[0],
        _ => expr,
    }
}

/// Each row's ordering of two exact numbers, lifted to their common scale.
///
/// `DECIMAL(38,0)` against `DECIMAL(20,3)`, or a decimal against an
/// integer, are the same comparison row evaluation makes exactly - but the
/// equality kernel beside this one takes only two decimals of one declared
/// type, so every other pairing went row by row. A lift that would
/// overflow `i128` declines the batch rather than wrapping.
pub(super) fn scaled_orderings(
    batch: &RecordBatch,
    left: &CompiledExpr,
    right: &CompiledExpr,
    effects: &mut Effects,
) -> Option<Vec<Option<Ordering>>> {
    let rows = batch.row_count();
    let left = operand(batch, under_widening_decimal_cast(left, batch), effects)?;
    let right = operand(batch, under_widening_decimal_cast(right, batch), effects)?;
    let (left, right) = (&left, &right);
    if !exactly_numeric(left) || !exactly_numeric(right) {
        return None;
    }
    // Two integers are the integer path's, which needs no rescale.
    if !is_decimal_column(left) && !is_decimal_column(right) {
        return None;
    }
    let (left, right) = (scaled(left)?, scaled(right)?);
    let mut orderings = Vec::with_capacity(rows);
    for row in 0..rows {
        let (Some((left, left_scale)), Some((right, right_scale))) = (left.at(row), right.at(row))
        else {
            orderings.push(None);
            continue;
        };
        orderings.push(Some(exact_ordering(
            (left, left_scale),
            (right, right_scale),
        )?));
    }
    Some(orderings)
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
        let ordering = if left_scale == right_scale {
            left.cmp(&right)
        } else {
            rescale(left, left_scale)?.cmp(&rescale(right, right_scale)?)
        };
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

/// A decimal subtree's units at its own scale: one value for every row, or
/// one per row with its validity.
#[derive(Clone)]
pub(super) enum Lane {
    Fixed(i128, u8),
    Rows {
        units: Vec<i128>,
        valid: Vec<bool>,
        scale: u8,
    },
}

impl Lane {
    const fn scale(&self) -> u8 {
        match self {
            Self::Fixed(_, scale) | Self::Rows { scale, .. } => *scale,
        }
    }

    #[inline]
    fn at(&self, row: usize) -> Option<i128> {
        match self {
            Self::Fixed(units, _) => Some(*units),
            Self::Rows { units, valid, .. } => valid[row].then(|| units[row]),
        }
    }

    /// A column's units, read once into a lane.
    fn read(scaled: &Scaled<'_>, rows: usize) -> Self {
        let scale = match scaled {
            Scaled::Decimal(_, scale, _) => *scale,
            Scaled::Integers(Integers::Fixed(value)) => return Self::Fixed(*value, 0),
            Scaled::Integers(_) => 0,
            Scaled::Fixed(units, scale) => return Self::Fixed(*units, *scale),
        };
        let mut units = Vec::with_capacity(rows);
        let mut valid = Vec::with_capacity(rows);
        for row in 0..rows {
            let value = scaled.at(row);
            units.push(value.map_or(0, |(units, _)| units));
            valid.push(value.is_some());
        }
        Self::Rows {
            units,
            valid,
            scale,
        }
    }
}

/// Whether `expr` is a chain [`unit_lane`] answers from the batch alone:
/// columns, constants, and decimal `+`, `-`, unary minus and `*` over
/// them. Nothing in one calls a function, so nothing is volatile and
/// nothing warns, and two equal ones answer the same lane.
fn plain_chain(expr: &CompiledExpr) -> bool {
    match expr {
        CompiledExpr::Column(_)
        | CompiledExpr::Literal(Value::Int64(_) | Value::UInt64(_) | Value::Utf8(_)) => true,
        CompiledExpr::Binary {
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply,
            left,
            right,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => plain_chain(left) && plain_chain(right),
        CompiledExpr::Unary {
            op: UnaryOp::Plus | UnaryOp::Minus,
            expr,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => plain_chain(expr),
        _ => false,
    }
}

/// The steps of `expr`'s decimal chain that are plain chains themselves,
/// outermost first, for [`super::SharedLanes`] to find the repeated ones.
pub(super) fn shareable_subtrees<'expr>(
    expr: &'expr CompiledExpr,
    found: &mut Vec<&'expr CompiledExpr>,
) {
    let children: [&CompiledExpr; 2] = match expr {
        CompiledExpr::Binary {
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply,
            left,
            right,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => [left, right],
        CompiledExpr::Unary {
            op: UnaryOp::Plus | UnaryOp::Minus,
            expr: inner,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => [inner, inner],
        _ => return,
    };
    if plain_chain(expr) {
        found.push(expr);
    }
    shareable_subtrees(children[0], found);
    if !std::ptr::eq(children[0], children[1]) {
        shareable_subtrees(children[1], found);
    }
}

/// [`unit_lane_of`], taking a subtree the batch's other expressions
/// already computed from the lanes they share, and keeping one they will
/// want.
fn unit_lane(expr: &CompiledExpr, batch: &RecordBatch, effects: &mut Effects) -> Option<Lane> {
    let slot = effects.shared.as_ref().and_then(|shared| {
        if shared.batch != Some(std::ptr::from_ref(batch)) {
            return None;
        }
        shared.lanes.iter().position(|(subtree, _)| subtree == expr)
    });
    let Some(slot) = slot else {
        return unit_lane_of(expr, batch, effects);
    };
    if let Some(lane) = effects
        .shared
        .as_ref()
        .and_then(|shared| shared.lanes[slot].1.clone())
    {
        crate::counters::count(|counters| {
            counters.decimal_lanes_shared = counters.decimal_lanes_shared.saturating_add(1);
        });
        return Some(lane);
    }
    let lane = unit_lane_of(expr, batch, effects)?;
    if let Some(shared) = effects.shared.as_mut() {
        shared.lanes[slot].1 = Some(lane.clone());
    }
    Some(lane)
}

/// `expr` as units at its declared scale, for a chain of decimal `+`, `-`,
/// unary minus and `*` over columns and constants in which every step's
/// exact answer has the scale the step declares: a sum or a difference at
/// the wider operand's scale, a product at the two scales added. Each step
/// is then exact on the units themselves, one widening multiply and one
/// add or one multiply per row, and the answer is the one the exact
/// fractions give, since nothing in such a chain is rounded. No fraction
/// is built and no divisor sought per row.
///
/// `None` declines: a division, a step whose declared scale is not its
/// exact one (a product past the scale cap rounds), or a selected row
/// that leaves 128 bits. The exact fractions answer those.
fn unit_lane_of(expr: &CompiledExpr, batch: &RecordBatch, effects: &mut Effects) -> Option<Lane> {
    let rows = batch.row_count();
    match expr {
        CompiledExpr::Literal(value @ (Value::Int64(_) | Value::UInt64(_) | Value::Utf8(_))) => {
            decimal_units_of(value).map(|(units, scale)| Lane::Fixed(units, scale))
        }
        CompiledExpr::Unary {
            op: op @ (UnaryOp::Plus | UnaryOp::Minus),
            expr: inner,
            data_type: Some(DataType::Decimal { scale, .. }),
            ..
        } => {
            let inner = unit_lane(inner, batch, effects)?;
            if inner.scale() != *scale {
                return None;
            }
            if *op == UnaryOp::Plus {
                return Some(inner);
            }
            Some(match inner {
                Lane::Fixed(units, scale) => Lane::Fixed(units.checked_neg()?, scale),
                Lane::Rows {
                    mut units,
                    valid,
                    scale,
                } => {
                    for (row, units) in units.iter_mut().enumerate() {
                        // Only `i128::MIN` has no negation.
                        *units = match units.checked_neg() {
                            Some(negated) => negated,
                            None if valid[row] && batch.selection().is_selected(row) => {
                                return None;
                            }
                            None => 0,
                        };
                    }
                    Lane::Rows {
                        units,
                        valid,
                        scale,
                    }
                }
            })
        }
        CompiledExpr::Binary {
            op: op @ (BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply),
            left,
            right,
            data_type: Some(DataType::Decimal { scale, .. }),
            ..
        } => {
            let left = unit_lane(left, batch, effects)?;
            let right = unit_lane(right, batch, effects)?;
            unit_step(*op, &left, &right, *scale, batch)
        }
        CompiledExpr::Literal(_)
        | CompiledExpr::Binary {
            op: BinaryOp::Divide,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => None,
        // Anything else is read at its own answer's value, as the exact
        // fractions read the boundary of their chain.
        CompiledExpr::Column(index) => {
            let leaf = Operand::Column(std::borrow::Cow::Borrowed(batch.column(*index)?));
            Some(Lane::read(&scaled(&leaf)?, rows))
        }
        nested => {
            let column = kernel(nested, batch, declared(nested), effects)?;
            let leaf = Operand::Column(std::borrow::Cow::Owned(column));
            Some(Lane::read(&scaled(&leaf)?, rows))
        }
    }
}

/// One step of [`unit_lane`]'s chain, at `scale`, or `None` when that is
/// not the step's exact scale or a selected row overflows.
fn unit_step(
    op: BinaryOp,
    left: &Lane,
    right: &Lane,
    scale: u8,
    batch: &RecordBatch,
) -> Option<Lane> {
    let (left_scale, right_scale) = (left.scale(), right.scale());
    // The factor each side is widened by; a product widens neither.
    let (left_factor, right_factor) = if op == BinaryOp::Multiply {
        if left_scale.checked_add(right_scale)? != scale {
            return None;
        }
        (1, 1)
    } else {
        if left_scale.max(right_scale) != scale {
            return None;
        }
        (
            10_i128.checked_pow(u32::from(scale - left_scale))?,
            10_i128.checked_pow(u32::from(scale - right_scale))?,
        )
    };
    let step = |left: i128, right: i128| match op {
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Add => left
            .checked_mul(left_factor)?
            .checked_add(right.checked_mul(right_factor)?),
        _ => left
            .checked_mul(left_factor)?
            .checked_sub(right.checked_mul(right_factor)?),
    };
    if let (Lane::Fixed(left, _), Lane::Fixed(right, _)) = (left, right) {
        return Some(Lane::Fixed(step(*left, *right)?, scale));
    }
    let rows = batch.row_count();
    let mut units = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let answer = match (left.at(row), right.at(row)) {
            (Some(left), Some(right)) => match step(left, right) {
                Some(answer) => Some(answer),
                None if batch.selection().is_selected(row) => return None,
                None => None,
            },
            _ => None,
        };
        units.push(answer.unwrap_or(0));
        valid.push(answer.is_some());
    }
    Some(Lane::Rows {
        units,
        valid,
        scale,
    })
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
    // What a declined attempt's nested kernels recorded is taken back, so
    // the exact path below records it once.
    let lane = super::attempt(effects, |effects| {
        let Lane::Rows { units, valid, .. } = unit_lane(expr, batch, effects)? else {
            return None;
        };
        crate::counters::count(|counters| {
            counters.decimal_unit_batches = counters.decimal_unit_batches.saturating_add(1);
        });
        Some(ColumnVector::from_typed(
            own,
            TypedValues::Decimal128 {
                values: DecimalUnits::Wide(units),
                scale,
                text: LazyText::decimal(scale),
            },
            ValidityMask::from_bools(&valid),
        ))
    });
    if lane.is_some() {
        return lane;
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

    /// A TIME cast to its number answers what row evaluation answers.
    ///
    /// This is the cast every TIME comparison binds to, so the boundary
    /// values are the ones that matter: the ends of the range, both signs
    /// of zero-adjacent, and the three-digit hours where text order stops
    /// agreeing with time order.
    #[test]
    fn time_numbers_match_row_evaluation() {
        let clocks = [
            Some("-838:59:59.000000"),
            Some("-100:00:00.000001"),
            Some("-00:00:00.000001"),
            Some("00:00:00.000000"),
            Some("00:00:00.000001"),
            Some("99:59:59.999999"),
            Some("100:00:00.000000"),
            Some("838:59:59.000000"),
            None,
        ];
        for fsp in [0_u8, 3, 6] {
            let column = ColumnVector::new(
                DataType::Time64 { fsp: 6 },
                clocks
                    .iter()
                    .map(|clock| clock.map_or(Value::Null, |text| Value::Utf8(text.to_owned())))
                    .collect(),
            )
            .expect("time column");
            let batch = batch_of(vec![column]);
            let target = DataType::Decimal {
                precision: 7 + fsp,
                scale: fsp,
            };
            // The cast has to carry the argument's declared type, or row
            // evaluation reads the text as a plain string and overflows.
            let expression = CompiledExpr::Scalar {
                function: ScalarFunction::Cast(target),
                argument_types: vec![Some(DataType::Time64 { fsp: 6 })],
                args: vec![CompiledExpr::Column(0)],
                literal_regex: None,
                session_zone: None,
                variables: None,
                data_type: Some(target),
                collation: crate::collation::Collation::default(),
                overflow: None,
            };
            assert!(
                agrees_with_rows(&expression, &batch, target),
                "a TIME cast at scale {fsp} has a kernel and agrees"
            );
        }
    }

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

    fn text(text: &str) -> CompiledExpr {
        literal(Value::Utf8(text.to_owned()))
    }

    /// `CAST(inner AS target)`, the cast an exact equality gives a side.
    fn decimal_cast(inner: CompiledExpr, target: DataType) -> CompiledExpr {
        CompiledExpr::Scalar {
            function: ScalarFunction::Cast(target),
            argument_types: vec![None],
            args: vec![inner],
            literal_regex: None,
            session_zone: None,
            variables: None,
            data_type: Some(target),
            collation: crate::collation::Collation::default(),
            overflow: None,
        }
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

    /// One step over whole numbers - an integer total plus one, less a
    /// column, times a count - and one at a wider scale: computed on the
    /// units, and what row evaluation answers.
    #[test]
    fn whole_number_steps_match_row_evaluation() {
        let batch = batch_of(vec![
            decimal(
                30,
                0,
                &[
                    Some(7),
                    Some(-9),
                    None,
                    Some(i64::MAX),
                    Some(0),
                    Some(123_456_789_012),
                ],
            ),
            signed(&[
                Some(3),
                None,
                Some(1),
                Some(i64::MAX),
                Some(-4),
                Some(-1_000_000),
            ]),
        ]);
        let steps = [
            (BinaryOp::Add, column(0), literal(Value::Int64(1)), 0),
            (BinaryOp::Add, column(0), column(1), 0),
            (BinaryOp::Subtract, column(1), column(0), 0),
            (BinaryOp::Multiply, column(0), literal(Value::Int64(-3)), 0),
            (
                BinaryOp::Add,
                column(0),
                literal(Value::Utf8("2.50".to_owned())),
                2,
            ),
            (
                BinaryOp::Multiply,
                column(1),
                literal(Value::Utf8("0.5".to_owned())),
                1,
            ),
        ];
        for (op, left, right, scale) in steps {
            let expression = binary(op, left, right, decimal_type(scale));
            agrees_with_warnings(&expression, &batch, decimal_type(scale));
        }
    }

    /// Chains of `+`, `-`, unary minus and `*` whose every step keeps its
    /// exact scale are answered on scaled units, and agree with row
    /// evaluation; a product declared below its exact scale, a division
    /// and a selected row past 128 bits take the exact fractions, and
    /// agree too.
    #[test]
    fn exact_scale_chains_run_on_units() {
        let batch = fixture();
        let step = |op, left, right, scale| binary(op, left, right, decimal_type(scale));
        let discounted = step(
            BinaryOp::Multiply,
            column(0),
            step(BinaryOp::Subtract, literal(Value::Int64(1)), column(1), 4),
            6,
        );
        let charged = step(
            BinaryOp::Multiply,
            discounted.clone(),
            step(BinaryOp::Add, text("1.5"), column(1), 4),
            10,
        );
        let negated = CompiledExpr::Unary {
            op: UnaryOp::Minus,
            expr: Box::new(discounted.clone()),
            data_type: Some(decimal_type(6)),
            overflow: None,
        };
        // Row 3, unselected, overflows the product of the integers.
        let mixed = step(
            BinaryOp::Add,
            step(
                BinaryOp::Multiply,
                step(BinaryOp::Multiply, column(2), column(0), 2),
                column(2),
                2,
            ),
            negated.clone(),
            6,
        );
        for (expression, scale) in [(discounted, 6), (charged, 10), (negated, 6), (mixed, 6)] {
            let _ = crate::counters::take_exec_counters();
            agrees_with_warnings(&expression, &batch, decimal_type(scale));
            assert_eq!(
                crate::counters::take_exec_counters().decimal_unit_batches,
                1,
                "{expression:?} runs on units"
            );
        }
        let wide = text("9999999999999999999999.99");
        let declines = [
            (step(BinaryOp::Multiply, column(0), column(1), 4), 4),
            (
                step(
                    BinaryOp::Add,
                    step(BinaryOp::Divide, column(0), column(1), 6),
                    column(0),
                    6,
                ),
                6,
            ),
            (
                step(
                    BinaryOp::Multiply,
                    step(BinaryOp::Multiply, column(0), wide.clone(), 4),
                    wide,
                    6,
                ),
                6,
            ),
        ];
        for (expression, scale) in declines {
            let _ = crate::counters::take_exec_counters();
            if expression
                .evaluate_column(&batch, Some(decimal_type(scale)))
                .is_some()
            {
                agrees_with_warnings(&expression, &batch, decimal_type(scale));
            }
            assert_eq!(
                crate::counters::take_exec_counters().decimal_unit_batches,
                0,
                "{expression:?} takes the exact fractions"
            );
        }
    }

    /// Two expressions over one batch that hold the same exact decimal
    /// chain compute it once and answer what each answers alone; a chain
    /// that calls a function is never shared, and a projection whose
    /// expressions repeat nothing lends nothing.
    #[test]
    fn repeated_chains_are_computed_once_per_batch() {
        let batch = fixture();
        let step = |op, left, right, scale| binary(op, left, right, decimal_type(scale));
        let discounted = step(
            BinaryOp::Multiply,
            column(0),
            step(BinaryOp::Subtract, literal(Value::Int64(1)), column(1), 4),
            6,
        );
        let charged = step(
            BinaryOp::Multiply,
            discounted.clone(),
            step(BinaryOp::Add, literal(Value::Int64(1)), column(1), 4),
            10,
        );
        let expressions = [(discounted, 6), (charged, 10)];
        let mut shared = super::super::SharedLanes::of(
            &batch,
            expressions.iter().map(|(expression, _)| expression),
        )
        .expect("the product repeats");
        let _ = crate::counters::take_exec_counters();
        for (expression, scale) in &expressions {
            let alone = expression
                .evaluate_column(&batch, Some(decimal_type(*scale)))
                .expect("a kernel");
            let sharing = expression
                .evaluate_column_sharing(&batch, Some(decimal_type(*scale)), &mut shared)
                .expect("a kernel");
            for row in batch.selection().selected_rows() {
                assert_eq!(
                    sharing.value(row),
                    alone.value(row),
                    "{expression:?} at {row}"
                );
            }
        }
        assert_eq!(
            crate::counters::take_exec_counters().decimal_lanes_shared,
            1
        );
        let cast = decimal_cast(column(2), decimal_type(2));
        let through_a_function = step(BinaryOp::Multiply, cast, column(0), 4);
        assert!(
            super::super::SharedLanes::of(&batch, [&through_a_function, &through_a_function])
                .is_none(),
            "a chain calling a function is not shared"
        );
        assert!(
            super::super::SharedLanes::of(&batch, expressions.iter().take(1).map(|(e, _)| e))
                .is_none()
        );
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
        // A cast that drops fraction digits rounds, which the packed cast
        // leaves to the row function; the answer is row evaluation's.
        let narrow = DataType::Decimal {
            precision: 12,
            scale: 1,
        };
        let rounding = scalar(ScalarFunction::Cast(narrow), vec![column(0)], narrow);
        assert!(agrees_with_rows(&rounding, &batch, narrow));
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

    /// A DECIMAL column against a constant is one integer comparison over
    /// the packed units, and names the rows row evaluation names: the
    /// constant at the column's scale, at a narrower one, with digits past
    /// it on either side of zero, and under the cast an equality gives it.
    #[test]
    fn a_decimal_constant_compares_the_packed_units() {
        let batch = fixture();
        let cast = |inner: CompiledExpr| decimal_cast(inner, decimal_type(2));
        let ordering = [
            BinaryOp::Less,
            BinaryOp::LessOrEqual,
            BinaryOp::Greater,
            BinaryOp::GreaterOrEqual,
        ];
        let mut expressions = Vec::new();
        for op in ordering {
            for constant in [
                "10.50", "-3.33", "0.0", "7", "-0.01", "10.505", "-3.335", "0.070", "999.99",
            ] {
                expressions.push(scalar(
                    ScalarFunction::DecimalComparison { op },
                    vec![column(0), text(constant)],
                    DataType::Boolean,
                ));
                expressions.push(scalar(
                    ScalarFunction::DecimalComparison { op },
                    vec![text(constant), column(0)],
                    DataType::Boolean,
                ));
                // A signed integer column is units at scale zero.
                expressions.push(scalar(
                    ScalarFunction::DecimalComparison { op },
                    vec![column(2), text(constant)],
                    DataType::Boolean,
                ));
            }
            expressions.push(scalar(
                ScalarFunction::DecimalComparison { op },
                vec![column(0), literal(Value::Int64(0))],
                DataType::Boolean,
            ));
        }
        for constant in ["10.50", "-3.33", "0.07", "10.5", "5"] {
            expressions.push(binary(
                BinaryOp::Equal,
                column(0),
                cast(text(constant)),
                DataType::Boolean,
            ));
            expressions.push(binary(
                BinaryOp::Equal,
                cast(column(0)),
                cast(text(constant)),
                DataType::Boolean,
            ));
        }
        for constant in ["10.50", "0.07", "5"] {
            expressions.push(binary(
                BinaryOp::NotEqual,
                cast(text(constant)),
                column(0),
                DataType::Boolean,
            ));
        }
        for expression in &expressions {
            let _ = crate::take_exec_counters();
            let mask = expression
                .evaluate_filter_mask(&batch)
                .expect("mask evaluation")
                .unwrap_or_else(|| panic!("{expression:?} has a packed mask"));
            assert_eq!(
                crate::take_exec_counters().decimal_masks_packed,
                1,
                "{expression:?} compared the units"
            );
            for row in batch.selection().selected_rows() {
                let expected = matches!(
                    expression.evaluate(&batch, row).expect("row evaluation"),
                    Value::Boolean(true)
                );
                assert_eq!(
                    mask.is_selected(row),
                    expected,
                    "{expression:?} at row {row}"
                );
            }
        }
    }

    /// A cast that would round the constant, a comparison of the column
    /// with bare text, and text that is no decimal are not the units' to
    /// answer.
    #[test]
    fn a_rounded_or_textual_constant_is_left_to_the_kernels() {
        let batch = fixture();
        let narrow = DataType::Decimal {
            precision: 12,
            scale: 2,
        };
        for expression in [
            binary(
                BinaryOp::Equal,
                column(0),
                decimal_cast(text("10.505"), narrow),
                DataType::Boolean,
            ),
            binary(BinaryOp::Equal, column(0), text("10.50"), DataType::Boolean),
            scalar(
                ScalarFunction::DecimalComparison { op: BinaryOp::Less },
                vec![column(0), text("ten")],
                DataType::Boolean,
            ),
        ] {
            let _ = crate::take_exec_counters();
            let _ = expression.evaluate_filter_mask(&batch);
            assert_eq!(
                crate::take_exec_counters().decimal_masks_packed,
                0,
                "{expression:?} left the units alone"
            );
        }
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
