//! Scalar functions a batch at a time.
//!
//! Most scalar functions read every argument for every row and answer from
//! the values alone. Their arguments are evaluated here a batch at a time,
//! by kernels where they have them, and each row answers through the very
//! function row evaluation calls, so the two answer alike by construction
//! and a kernel above can read the answer as a column.
//!
//! A few answer from packed arguments directly: `GREATEST` and `LEAST` pick
//! an argument's units, `ABS` and `SIGN` read units, and decimal `ROUND`,
//! `TRUNCATE`, `CEIL` and `FLOOR` round units - each only where that is
//! the row function's answer, the row function otherwise.

use std::cmp::Ordering;

use pintail_sql::{ScalarFunction, UnaryOp};
use pintail_types::{DataType, Value};

use super::{Effects, Operand, operand};
use crate::array::ValidityMask;
use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, TypedValues};
use crate::collation::Collation;
use crate::execution::gather;
use crate::expression::{
    CompiledExpr, CompiledRegex, compare_utf8_mysql, declared_render_cap,
    evaluate_eager_scalar_typed, evaluate_unary, mysql_decimals,
};

/// A scalar call's parts, as the compiled node carries them.
pub(super) struct Call<'call> {
    pub(super) function: ScalarFunction,
    pub(super) args: &'call [CompiledExpr],
    pub(super) argument_types: &'call [Option<DataType>],
    pub(super) literal_regex: Option<&'call CompiledRegex>,
    pub(super) data_type: Option<DataType>,
    pub(super) collation: Collation,
}

/// Functions row evaluation answers from their arguments' values alone,
/// every argument read for every row. The conditionals read arguments
/// lazily and have their own kernels.
const fn eager(function: ScalarFunction) -> bool {
    !matches!(
        function,
        ScalarFunction::If | ScalarFunction::Coalesce | ScalarFunction::NullIf
    )
}

/// Functions that parse a document for every row they answer.
///
/// The adapter below is a trade: it reads the rows row evaluation would
/// have read, and in exchange everything above it in the expression runs
/// packed. That pays when the function itself is cheap next to the rest of
/// the expression, and loses when it is not - a JSON reader parses its
/// document per row, so adapting one pays that cost again on top of the
/// row path it replaces. Measured over 130,000 rows,
/// `JSON_EXTRACT(meta, '$.score') > 2` in a `WHERE` clause cost 123 ms
/// adapted against 75 ms read row by row.
const fn parses_a_document(function: ScalarFunction) -> bool {
    matches!(
        function,
        ScalarFunction::JsonExtract { .. }
            | ScalarFunction::JsonContains
            | ScalarFunction::JsonContainsPath
            | ScalarFunction::JsonDepth
            | ScalarFunction::JsonKeys
            | ScalarFunction::JsonLength
            | ScalarFunction::JsonMemberOf
            | ScalarFunction::JsonOverlaps
            | ScalarFunction::JsonSearch
            | ScalarFunction::JsonValue
    )
}

/// Functions row evaluation feeds a computed decimal's internal digits,
/// which no kernel's answer holds.
const fn reads_internal_digits(function: ScalarFunction) -> bool {
    matches!(
        function,
        ScalarFunction::Round { decimal: true }
            | ScalarFunction::Truncate { decimal: true }
            | ScalarFunction::Ceil { decimal: true }
            | ScalarFunction::Floor { decimal: true }
            | ScalarFunction::Cast(DataType::Decimal { .. })
            | ScalarFunction::DeclaredCast {
                target: DataType::Decimal { .. },
                ..
            }
    )
}

fn value_at(operand: &Operand<'_>, row: usize) -> Option<Value> {
    match operand {
        Operand::Column(column) => column.value_owned(row),
        Operand::Constant(value) => Some((*value).clone()),
    }
}

pub(super) fn scalar_column(
    batch: &RecordBatch,
    call: &Call<'_>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let declared = call.data_type?;
    if !eager(call.function) {
        return None;
    }
    // Where row evaluation reads the first argument's internal digits, only
    // an argument that has none beyond its value is read here: a constant,
    // or a column packed from its units.
    if reads_internal_digits(call.function) {
        match call.args.first()? {
            CompiledExpr::Literal(Value::DecimalAverage(_)) => return None,
            CompiledExpr::Literal(_) => {}
            CompiledExpr::Column(index) if gather::packed(batch.column(*index)?) => {}
            _ => return None,
        }
    }
    let operands = call
        .args
        .iter()
        .map(|argument| operand(batch, argument, effects))
        .collect::<Option<Vec<_>>>()?;
    if !operands.iter().any(Operand::varies) {
        return None;
    }
    if let Some(packed) = packed(batch, call, &operands, declared) {
        return Some(packed);
    }
    // Nothing packed answers this call, so what follows reads the selected
    // rows one at a time. A caller with a row path of its own declines here
    // instead: adapting would build a column for every row on top of the
    // per-row evaluation it was meant to replace.
    if !effects.adapts() || parses_a_document(call.function) {
        return None;
    }
    let rows = batch.row_count();
    let mut values = Vec::with_capacity(rows);
    let mut arguments = Vec::with_capacity(operands.len());
    for row in 0..rows {
        if !batch.selection().is_selected(row) {
            values.push(Value::Null);
            continue;
        }
        arguments.clear();
        for operand in &operands {
            arguments.push(value_at(operand, row)?);
        }
        // An error is row evaluation's to raise, at the row it reaches.
        values.push(
            evaluate_eager_scalar_typed(
                call.function,
                &arguments,
                call.argument_types,
                call.literal_regex,
                call.data_type,
                call.collation,
            )
            .ok()?,
        );
    }
    ColumnVector::new(declared, values).ok()
}

/// `NOT`, unary `-` and unary `+` of an argument a batch at a time: each row
/// through row evaluation's own operator, or, negating a packed integer or
/// decimal, by its units.
pub(super) fn unary_column(
    batch: &RecordBatch,
    op: UnaryOp,
    argument: &CompiledExpr,
    own: Option<DataType>,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let declared = own?;
    if data_type.is_some_and(|data_type| data_type != declared) {
        return None;
    }
    let input = operand(batch, argument, effects)?;
    if !input.varies() {
        return None;
    }
    if op == UnaryOp::Minus
        && let Some(negated) = negated(batch, &input, declared)
    {
        return Some(negated);
    }
    let mut values = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        if !batch.selection().is_selected(row) {
            values.push(Value::Null);
            continue;
        }
        values.push(evaluate_unary(op, &value_at(&input, row)?, own).ok()?);
    }
    ColumnVector::new(declared, values).ok()
}

/// A packed integer or decimal negated by its units. Row evaluation
/// negates a decimal's text, which spells a negated zero `-0`; units cannot,
/// so a decimal holding a zero goes row by row.
fn negated(batch: &RecordBatch, input: &Operand<'_>, declared: DataType) -> Option<ColumnVector> {
    let Operand::Column(column) = input else {
        return None;
    };
    if column.data_type() != declared || !gather::packed(column) {
        return None;
    }
    let (typed, validity) = column.typed()?;
    let typed = match typed {
        TypedValues::Int64(values) if declared.storage_type() == DataType::Int64 => {
            let mut negated = Vec::with_capacity(values.len());
            for (row, value) in values.iter().enumerate() {
                negated.push(match value.checked_neg() {
                    Some(value) => value,
                    None if validity.is_valid(row) && batch.selection().is_selected(row) => {
                        return None;
                    }
                    None => 0,
                });
            }
            TypedValues::Int64(negated)
        }
        TypedValues::Decimal128 { values, scale, .. } => {
            let mut negated = Vec::with_capacity(values.len());
            for row in 0..values.len() {
                let value = values.get(row).unwrap_or(0);
                if value == 0 && validity.is_valid(row) {
                    return None;
                }
                negated.push(value.checked_neg()?);
            }
            TypedValues::Decimal128 {
                values: DecimalUnits::Wide(negated),
                scale: *scale,
                text: LazyText::decimal(*scale),
            }
        }
        _ => return None,
    };
    Some(ColumnVector::from_typed(declared, typed, validity.clone()))
}

fn packed(
    batch: &RecordBatch,
    call: &Call<'_>,
    operands: &[Operand<'_>],
    declared: DataType,
) -> Option<ColumnVector> {
    match call.function {
        ScalarFunction::Greatest { .. } | ScalarFunction::Least { .. } => extreme(
            operands,
            declared,
            matches!(call.function, ScalarFunction::Greatest { .. }),
            call.collation,
        ),
        ScalarFunction::Abs { decimal } => absolute(batch, operands, declared, decimal),
        ScalarFunction::Sign => sign(operands, declared),
        ScalarFunction::Round { decimal: true } | ScalarFunction::Truncate { decimal: true } => {
            rounded(
                operands,
                call.argument_types,
                declared,
                matches!(call.function, ScalarFunction::Round { .. }),
            )
        }
        ScalarFunction::Ceil { decimal: true } | ScalarFunction::Floor { decimal: true } => {
            integer_bound(
                batch,
                operands,
                declared,
                matches!(call.function, ScalarFunction::Ceil { .. }),
            )
        }
        _ => super::text::packed_text(call.function, operands, declared, call.collation),
    }
}

/// A packed column's rows in the order its values compare: integers and
/// decimals by value, temporals by the units their canonical text spells
/// wherever the year has four digits. `None` for NULL rows.
fn ordered_units(column: &ColumnVector) -> Option<Vec<Option<i128>>> {
    let (typed, validity) = column.typed()?;
    let rows = column.len();
    let at = |row: usize, unit: i128| validity.is_valid(row).then_some(unit);
    Some(match (column.data_type(), typed) {
        (_, TypedValues::Int64(values)) => (0..rows)
            .map(|row| at(row, i128::from(values[row])))
            .collect(),
        (_, TypedValues::UInt64(values)) => (0..rows)
            .map(|row| at(row, i128::from(values[row])))
            .collect(),
        (DataType::Decimal { .. }, TypedValues::Decimal128 { values, text, .. })
            if text.derived() =>
        {
            (0..rows)
                .map(|row| at(row, values.get(row).unwrap_or(0)))
                .collect()
        }
        (
            data_type @ (DataType::Date32 | DataType::DateTime64 { .. }),
            TypedValues::Temporal { units, text },
        ) if text.derived() => {
            let day = |year, month, day| {
                chrono::NaiveDate::from_ymd_opt(year, month, day).map(|date| {
                    i128::from(
                        date.signed_duration_since(chrono::NaiveDate::default())
                            .num_days(),
                    )
                })
            };
            let (first, last) = (day(0, 1, 1)?, day(9999, 12, 31)?);
            let (step, years) = match data_type {
                DataType::DateTime64 { fsp } => (
                    10_i64.pow(6 - u32::from(fsp.min(6))),
                    first * 86_400_000_000..=(last + 1) * 86_400_000_000 - 1,
                ),
                _ => (1, first..=last),
            };
            let mut ordered = Vec::with_capacity(rows);
            for (row, unit) in units.iter().enumerate() {
                let spelled = i128::from(unit - unit.rem_euclid(step));
                if validity.is_valid(row) && !years.contains(&spelled) {
                    return None;
                }
                ordered.push(at(row, spelled));
            }
            ordered
        }
        _ => return None,
    })
}

/// `GREATEST` and `LEAST` over arguments packed in the answer's type: each
/// row takes the argument row evaluation takes - a later one over the
/// current when it is greater, for `GREATEST`, or not greater, for `LEAST`
/// - and the answer copies its units. NULL anywhere answers NULL.
fn extreme(
    operands: &[Operand<'_>],
    declared: DataType,
    greatest: bool,
    collation: Collation,
) -> Option<ColumnVector> {
    let columns = operands
        .iter()
        .map(|operand| match operand {
            Operand::Column(column) if column.data_type() == declared && gather::packed(column) => {
                Some(&**column)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let rows = columns.first()?.len();
    let takes = |ordering: Ordering| (ordering == Ordering::Greater) == greatest;
    let mut picks = Vec::with_capacity(rows);
    if declared == DataType::Utf8 {
        let texts = columns
            .iter()
            .map(|column| gather::plain_text(column))
            .collect::<Option<Vec<_>>>()?;
        for row in 0..rows {
            let mut best: Option<usize> = None;
            for (index, (text, validity)) in texts.iter().enumerate() {
                if !validity.is_valid(row) {
                    best = None;
                    break;
                }
                let read = |text: &crate::array::StrColumn| {
                    text.views()[row].with_bytes(text.heap(), |bytes| {
                        std::str::from_utf8(bytes).ok().map(str::to_owned)
                    })
                };
                best = Some(match best {
                    None => index,
                    Some(current) => {
                        let ordering =
                            compare_utf8_mysql(&read(text)?, &read(texts[current].0)?, collation);
                        if takes(ordering) { index } else { current }
                    }
                });
            }
            picks.push(best);
        }
    } else {
        let units = columns
            .iter()
            .map(|column| ordered_units(column))
            .collect::<Option<Vec<_>>>()?;
        for row in 0..rows {
            let mut best: Option<usize> = None;
            for (index, column) in units.iter().enumerate() {
                let Some(value) = column[row] else {
                    best = None;
                    break;
                };
                best = Some(match best {
                    None => index,
                    Some(current) => {
                        let current_value = units[current][row].unwrap_or_default();
                        if takes(value.cmp(&current_value)) {
                            index
                        } else {
                            current
                        }
                    }
                });
            }
            picks.push(best);
        }
    }
    // A row NULL poisons answers NULL: it takes a column of NULLs.
    let nulls = null_column(declared, rows)?;
    let mut sources = columns;
    sources.push(&nulls);
    let null_source = u32::try_from(sources.len() - 1).ok()?;
    let picks = picks
        .into_iter()
        .enumerate()
        .map(|(row, best)| {
            Some((
                best.map_or(Some(null_source), |best| u32::try_from(best).ok())?,
                u32::try_from(row).ok()?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    gather::gather(&sources, &picks, declared).ok()
}

/// A column of `rows` NULLs, packed alike with packed columns of
/// `data_type`.
fn null_column(data_type: DataType, rows: usize) -> Option<ColumnVector> {
    let typed = match data_type {
        DataType::Decimal { scale, .. } => TypedValues::Decimal128 {
            values: DecimalUnits::Wide(vec![0; rows]),
            scale,
            text: LazyText::decimal(scale),
        },
        DataType::Date32 => TypedValues::Temporal {
            units: vec![0; rows],
            text: LazyText::date(),
        },
        DataType::DateTime64 { fsp } => TypedValues::Temporal {
            units: vec![0; rows],
            text: LazyText::datetime(fsp),
        },
        DataType::Utf8 => {
            let mut text = crate::array::StrColumn::default();
            for _ in 0..rows {
                text.push(&[]);
            }
            TypedValues::Utf8(text)
        }
        data_type if data_type.storage_type() == DataType::Int64 => {
            TypedValues::Int64(vec![0; rows])
        }
        data_type if data_type.storage_type() == DataType::UInt64 => {
            TypedValues::UInt64(vec![0; rows])
        }
        _ => return None,
    };
    Some(ColumnVector::from_typed(
        data_type,
        typed,
        ValidityMask::from_bools(&vec![false; rows]),
    ))
}

/// The one argument, as a packed column of the answer's type.
fn sole_column<'operand>(
    operands: &'operand [Operand<'_>],
    declared: DataType,
) -> Option<&'operand ColumnVector> {
    match operands {
        [Operand::Column(column)] if column.data_type() == declared && gather::packed(column) => {
            Some(column)
        }
        _ => None,
    }
}

/// `ABS` of a packed integer or decimal: the value without its sign.
fn absolute(
    batch: &RecordBatch,
    operands: &[Operand<'_>],
    declared: DataType,
    decimal: bool,
) -> Option<ColumnVector> {
    let column = sole_column(operands, declared)?;
    let (typed, validity) = column.typed()?;
    let typed = match typed {
        TypedValues::UInt64(_) => return Some(column.clone()),
        TypedValues::Int64(values) => {
            let mut absolute = Vec::with_capacity(values.len());
            for (row, value) in values.iter().enumerate() {
                absolute.push(match value.checked_abs() {
                    Some(value) => value,
                    // Out of range: row evaluation's error, for a row it reads.
                    None if validity.is_valid(row) && batch.selection().is_selected(row) => {
                        return None;
                    }
                    None => 0,
                });
            }
            TypedValues::Int64(absolute)
        }
        TypedValues::Decimal128 { values, scale, .. } if decimal => TypedValues::Decimal128 {
            values: DecimalUnits::Wide(
                (0..values.len())
                    .map(|row| values.get(row).map_or(Some(0), i128::checked_abs))
                    .collect::<Option<_>>()?,
            ),
            scale: *scale,
            text: LazyText::decimal(*scale),
        },
        _ => return None,
    };
    Some(ColumnVector::from_typed(declared, typed, validity.clone()))
}

/// `SIGN` of a packed integer or decimal: its units' sign.
fn sign(operands: &[Operand<'_>], declared: DataType) -> Option<ColumnVector> {
    if declared.storage_type() != DataType::Int64 {
        return None;
    }
    let [Operand::Column(column)] = operands else {
        return None;
    };
    if !matches!(
        column.data_type().storage_type(),
        DataType::Int64 | DataType::UInt64
    ) && !matches!(column.data_type(), DataType::Decimal { .. })
    {
        return None;
    }
    let units = ordered_units(column)?;
    let (_, validity) = column.typed()?;
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Int64(
            units
                .iter()
                .map(
                    |unit| match unit.map_or(Ordering::Equal, |unit| unit.cmp(&0)) {
                        Ordering::Less => -1,
                        Ordering::Equal => 0,
                        Ordering::Greater => 1,
                    },
                )
                .collect(),
        ),
        validity.clone(),
    ))
}

/// Decimal `ROUND` and `TRUNCATE` of a packed decimal to a constant, non-
/// negative number of digits, where the answer is spelled at the answer
/// type's scale: row evaluation keeps as many fraction digits as the
/// argument's type and the digit count allow.
fn rounded(
    operands: &[Operand<'_>],
    argument_types: &[Option<DataType>],
    declared: DataType,
    round: bool,
) -> Option<ColumnVector> {
    let [Operand::Column(column), Operand::Constant(digits)] = operands else {
        return None;
    };
    let DataType::Decimal {
        scale: declared_scale,
        ..
    } = declared
    else {
        return None;
    };
    let (
        TypedValues::Decimal128 {
            values,
            scale,
            text,
        },
        validity,
    ) = column.typed()?
    else {
        return None;
    };
    if !text.derived() || !matches!(column.data_type(), DataType::Decimal { .. }) {
        return None;
    }
    let digits = mysql_decimals(digits).ok()?;
    let input_scale = i64::from(*scale);
    let render_scale =
        u8::try_from(digits.clamp(0, declared_render_cap(argument_types, input_scale))).ok()?;
    if render_scale != declared_scale {
        return None;
    }
    let factor = 10_i128.checked_pow(u32::from(scale.checked_sub(render_scale)?))?;
    // Digits left of the point: the answer keeps scale zero and its low
    // `-digits` digits are zeroed, which is what the row path does after
    // its own divide. `ROUND(1234.56, -1)` is 1230.
    let zeroed = if digits < 0 {
        10_i128.checked_pow(u32::try_from(digits.saturating_neg()).ok()?.min(38))?
    } else {
        1
    };
    let mut units = Vec::with_capacity(values.len());
    for row in 0..values.len() {
        let value = values.get(row).unwrap_or(0);
        let whole = if round {
            // Half away from zero, as an exact decimal rounds. Rounding to
            // a place left of the point rounds at that place, so the
            // divisor carries the zeroed digits with it.
            let step = factor.checked_mul(zeroed)?;
            let magnitude = value
                .unsigned_abs()
                .checked_add((step / 2).unsigned_abs())?
                / step.unsigned_abs();
            let magnitude = i128::try_from(magnitude).ok()?;
            let magnitude = magnitude.checked_mul(zeroed)?;
            if value < 0 { -magnitude } else { magnitude }
        } else {
            // Truncation toward zero, then the low digits dropped.
            value / factor / zeroed * zeroed
        };
        units.push(whole);
    }
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Decimal128 {
            values: DecimalUnits::Wide(units),
            scale: render_scale,
            text: LazyText::decimal(render_scale),
        },
        validity.clone(),
    ))
}

/// Decimal `CEIL` and `FLOOR` of a packed decimal, answering a signed
/// integer: the whole part, moved up or down where a fraction remains.
fn integer_bound(
    batch: &RecordBatch,
    operands: &[Operand<'_>],
    declared: DataType,
    ceiling: bool,
) -> Option<ColumnVector> {
    if declared.storage_type() != DataType::Int64 {
        return None;
    }
    let [Operand::Column(column)] = operands else {
        return None;
    };
    let (
        TypedValues::Decimal128 {
            values,
            scale,
            text,
        },
        validity,
    ) = column.typed()?
    else {
        return None;
    };
    if !text.derived() || !matches!(column.data_type(), DataType::Decimal { .. }) {
        return None;
    }
    let factor = 10_i128.checked_pow(u32::from(*scale))?;
    let mut bounds = Vec::with_capacity(values.len());
    for row in 0..values.len() {
        let value = values.get(row).unwrap_or(0);
        let mut whole = value / factor;
        if value % factor != 0 {
            if ceiling && value > 0 {
                whole += 1;
            }
            if !ceiling && value < 0 {
                whole -= 1;
            }
        }
        bounds.push(match i64::try_from(whole) {
            Ok(whole) => whole,
            Err(_) if validity.is_valid(row) && batch.selection().is_selected(row) => {
                return None;
            }
            Err(_) => 0,
        });
    }
    Some(ColumnVector::from_typed(
        declared,
        TypedValues::Int64(bounds),
        validity.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use pintail_sql::{BinaryOp, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, binary, scalar};
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, TypedValues};
    use crate::collation::Collation;
    use crate::expression::CompiledExpr;

    const fn decimal_type(scale: u8) -> DataType {
        DataType::Decimal {
            precision: 16,
            scale,
        }
    }

    fn decimal(scale: u8, units: &[Option<i64>]) -> ColumnVector {
        ColumnVector::from_typed(
            decimal_type(scale),
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

    /// Columns: two decimals at scale 3, two integers (one reaching the
    /// signed minimum on the unselected row 3), two texts.
    fn fixture() -> RecordBatch {
        batch_of(vec![
            decimal(
                3,
                &[
                    Some(1_500),
                    Some(-2_505),
                    None,
                    Some(7),
                    Some(-1_500),
                    Some(0),
                    Some(999_999),
                    Some(2_499),
                ],
            ),
            decimal(
                3,
                &[
                    Some(1_500),
                    Some(-2_504),
                    Some(1),
                    Some(8),
                    Some(-1_501),
                    None,
                    Some(-999_999),
                    Some(2_500),
                ],
            ),
            signed(&[
                Some(3),
                Some(-7),
                None,
                Some(i64::MIN),
                Some(0),
                Some(9),
                Some(-9),
                Some(1),
            ]),
            signed(&[
                Some(3),
                Some(5),
                Some(2),
                Some(1),
                None,
                Some(-9),
                Some(9),
                Some(-1),
            ]),
            texts(&[
                Some("b"),
                Some("B"),
                None,
                Some("a"),
                Some("\u{e9}"),
                Some("e"),
                Some(""),
                Some("ab"),
            ]),
            texts(&[
                Some("B"),
                Some("b"),
                Some("x"),
                None,
                Some("e"),
                Some("\u{e9}"),
                Some(" "),
                Some("aB"),
            ]),
        ])
    }

    fn column(index: usize) -> CompiledExpr {
        CompiledExpr::Column(index)
    }

    fn literal(value: Value) -> CompiledExpr {
        CompiledExpr::Literal(value)
    }

    fn call(
        function: ScalarFunction,
        args: Vec<CompiledExpr>,
        data_type: DataType,
    ) -> CompiledExpr {
        scalar(function, args, data_type)
    }

    fn with_types(expression: CompiledExpr, types: Vec<Option<DataType>>) -> CompiledExpr {
        match expression {
            CompiledExpr::Scalar {
                function,
                args,
                literal_regex,
                data_type,
                collation,
                overflow,
                ..
            } => CompiledExpr::Scalar {
                function,
                args,
                argument_types: types,
                literal_regex,
                data_type,
                collation,
                overflow,
            },
            other => other,
        }
    }

    #[test]
    fn extremes_match_row_evaluation() {
        let batch = fixture();
        for (greatest, decimal) in [(true, true), (false, true), (true, false), (false, false)] {
            let function = if greatest {
                ScalarFunction::Greatest { decimal }
            } else {
                ScalarFunction::Least { decimal }
            };
            let (args, declared) = if decimal {
                (vec![column(0), column(1)], decimal_type(3))
            } else {
                (
                    vec![column(2), column(3), literal(Value::Int64(4))],
                    DataType::Int64,
                )
            };
            let expression = call(function, args, declared);
            assert!(
                agrees_with_rows(&expression, &batch, declared),
                "{expression:?}"
            );
        }
        for name in ["utf8mb4_0900_ai_ci", "utf8mb4_general_ci", "utf8mb4_bin"] {
            for function in [
                ScalarFunction::Greatest { decimal: false },
                ScalarFunction::Least { decimal: false },
            ] {
                let expression = CompiledExpr::Scalar {
                    function,
                    args: vec![column(4), column(5)],
                    argument_types: vec![Some(DataType::Utf8); 2],
                    literal_regex: None,
                    data_type: Some(DataType::Utf8),
                    collation: Collation::from_mysql_name(name).expect("collation"),
                    overflow: None,
                };
                assert!(
                    agrees_with_rows(&expression, &batch, DataType::Utf8),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn signs_and_magnitudes_match_row_evaluation() {
        let batch = fixture();
        for (expression, declared) in [
            (
                call(
                    ScalarFunction::Abs { decimal: false },
                    vec![column(2)],
                    DataType::Int64,
                ),
                DataType::Int64,
            ),
            (
                call(
                    ScalarFunction::Abs { decimal: true },
                    vec![column(0)],
                    decimal_type(3),
                ),
                decimal_type(3),
            ),
            (
                call(ScalarFunction::Sign, vec![column(1)], DataType::Int64),
                DataType::Int64,
            ),
            (
                call(ScalarFunction::Sign, vec![column(3)], DataType::Int64),
                DataType::Int64,
            ),
        ] {
            assert!(
                agrees_with_rows(&expression, &batch, declared),
                "{expression:?}"
            );
        }
        // Selected, the signed minimum's magnitude is row evaluation's error.
        let mut every = fixture();
        every
            .set_selection(crate::batch::SelectionMask::all(every.row_count()))
            .expect("selection");
        let expression = call(
            ScalarFunction::Abs { decimal: false },
            vec![column(2)],
            DataType::Int64,
        );
        assert!(
            expression
                .evaluate_column(&every, Some(DataType::Int64))
                .is_none()
        );
    }

    #[test]
    fn decimal_rounding_matches_row_evaluation() {
        let batch = fixture();
        for digits in [-2_i64, -1, 0, 1, 2, 3, 5] {
            for round in [true, false] {
                let function = if round {
                    ScalarFunction::Round { decimal: true }
                } else {
                    ScalarFunction::Truncate { decimal: true }
                };
                let scale = u8::try_from(digits.clamp(0, 3)).expect("scale");
                let expression = with_types(
                    call(
                        function,
                        vec![column(0), literal(Value::Int64(digits))],
                        decimal_type(scale),
                    ),
                    vec![Some(decimal_type(3)), Some(DataType::Int64)],
                );
                assert!(
                    agrees_with_rows(&expression, &batch, decimal_type(scale)),
                    "{expression:?}"
                );
            }
        }
        for function in [
            ScalarFunction::Ceil { decimal: true },
            ScalarFunction::Floor { decimal: true },
        ] {
            let expression = with_types(
                call(function, vec![column(1)], DataType::Int64),
                vec![Some(decimal_type(3))],
            );
            assert!(
                agrees_with_rows(&expression, &batch, DataType::Int64),
                "{expression:?}"
            );
        }
    }

    #[test]
    fn unary_operators_match_row_evaluation() {
        let batch = fixture();
        let unary = |op, argument, data_type| CompiledExpr::Unary {
            op,
            expr: Box::new(argument),
            data_type: Some(data_type),
            overflow: None,
        };
        let nonzero = decimal(
            2,
            &[
                Some(150),
                Some(-1),
                None,
                Some(7),
                Some(-99),
                Some(3),
                Some(1),
                Some(-5),
            ],
        );
        let with_nonzero = batch_of(vec![nonzero]);
        for (expression, batch, declared) in [
            // The signed minimum sits on the unselected row.
            (
                unary(pintail_sql::UnaryOp::Minus, column(2), DataType::Int64),
                &batch,
                DataType::Int64,
            ),
            // A zero negates to text row evaluation spells `-0`.
            (
                unary(pintail_sql::UnaryOp::Minus, column(0), decimal_type(3)),
                &batch,
                decimal_type(3),
            ),
            (
                unary(pintail_sql::UnaryOp::Minus, column(0), decimal_type(2)),
                &with_nonzero,
                decimal_type(2),
            ),
            (
                unary(
                    pintail_sql::UnaryOp::Not,
                    binary(BinaryOp::Less, column(2), column(3), DataType::Boolean),
                    DataType::Boolean,
                ),
                &batch,
                DataType::Boolean,
            ),
        ] {
            assert!(
                agrees_with_rows(&expression, batch, declared),
                "{expression:?}"
            );
        }
    }

    /// A JSON reader is not adapted: it parses its document for every row
    /// either way, so building a column of those answers only adds to the
    /// row evaluation a caller would do anyway. Its neighbour in the same
    /// position is adapted, which is what makes this a carve-out rather
    /// than the adapter being off.
    #[test]
    fn a_json_reader_declines_rather_than_being_read_row_by_row() {
        let batch = fixture();
        let extract = binary(
            BinaryOp::Greater,
            call(
                ScalarFunction::JsonExtract { unquote: false },
                vec![column(5), literal(Value::Utf8("$.score".to_owned()))],
                DataType::Json,
            ),
            literal(Value::Int64(2)),
            DataType::Boolean,
        );
        assert!(
            extract
                .evaluate_column(&batch, Some(DataType::Boolean))
                .is_none(),
            "a JSON reader in a predicate declines to the row path"
        );
        let quoted = binary(
            BinaryOp::Greater,
            call(
                ScalarFunction::SubstringIndex,
                vec![
                    column(5),
                    literal(Value::Utf8("-".to_owned())),
                    literal(Value::Int64(1)),
                ],
                DataType::Utf8,
            ),
            literal(Value::Utf8("a".to_owned())),
            DataType::Boolean,
        );
        assert!(
            quoted
                .evaluate_column(&batch, Some(DataType::Boolean))
                .is_some(),
            "a function of the same shape that does not parse a document is adapted"
        );
    }

    /// Any other function answers row by row through the row function,
    /// and a kernel above reads its answer as a column.
    #[test]
    fn row_functions_answer_as_columns_for_kernels_above() {
        let batch = fixture();
        let upper = call(
            ScalarFunction::Upper,
            vec![call(
                ScalarFunction::Concat,
                vec![column(4), literal(Value::Utf8("-x".to_owned()))],
                DataType::Utf8,
            )],
            DataType::Utf8,
        );
        assert!(agrees_with_rows(&upper, &batch, DataType::Utf8));
        let long = binary(
            BinaryOp::Greater,
            call(ScalarFunction::Length, vec![column(5)], DataType::Int64),
            literal(Value::Int64(1)),
            DataType::Boolean,
        );
        assert!(agrees_with_rows(&long, &batch, DataType::Boolean));
    }
}
