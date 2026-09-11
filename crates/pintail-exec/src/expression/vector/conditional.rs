//! `IF` (which `CASE` binds as), `COALESCE` and `NULLIF` a batch at a time.
//!
//! Row evaluation reads a branch only for the rows that reach it: the
//! condition of every row, then each row's chosen branch; `COALESCE` an
//! argument only where every earlier one was NULL. Here each branch is
//! evaluated over the batch with its selection narrowed to the rows that
//! reach it, so a warning or an error belongs to the same rows it belongs
//! to in row evaluation. The chosen value becomes the answer through the
//! same per-row code row evaluation uses, so the two answer alike.

use pintail_types::{DataType, Value};

use super::{Effects, Operand, operand};
use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, SelectionMask, TypedValues};
use crate::collation::Collation;
use crate::execution::gather;
use crate::expression::{CompiledExpr, cast_scalar, if_result, mysql_truth, null_if};

/// Row `row` of an operand, as row evaluation reads it.
fn value_at(operand: &Operand<'_>, row: usize) -> Option<Value> {
    match operand {
        Operand::Column(column) => column.value_owned(row),
        Operand::Constant(value) => Some((*value).clone()),
    }
}

/// A branch's answer over its rows, held past the narrowed batch it was
/// read from: a column shares its buffers, so keeping one is cheap.
enum Branch {
    Column(ColumnVector),
    Constant(Value),
}

impl Branch {
    fn of(operand: Operand<'_>) -> Self {
        match operand {
            Operand::Column(column) => Self::Column(column.into_owned()),
            Operand::Constant(value) => Self::Constant(value.clone()),
        }
    }

    fn value(&self, row: usize) -> Option<Value> {
        match self {
            Self::Column(column) => column.value_owned(row),
            Self::Constant(value) => Some(value.clone()),
        }
    }

    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Column(column) => match column.typed() {
                Some((_, validity)) => !validity.is_valid(row),
                None => matches!(column.value(row), Some(Value::Null) | None),
            },
            Self::Constant(value) => matches!(value, Value::Null),
        }
    }
}

/// `batch` with only the rows of `rows` selected.
fn narrowed(batch: &RecordBatch, rows: &[bool]) -> Option<RecordBatch> {
    let mut narrowed = batch.clone();
    let mut selection = SelectionMask::all(batch.row_count());
    for (row, keep) in rows.iter().enumerate() {
        if !keep {
            selection.set(row, false).ok()?;
        }
    }
    narrowed.set_selection(selection).ok()?;
    Some(narrowed)
}

/// The answer column: each selected row's value, NULL elsewhere, as the
/// row path builds it.
fn answer(data_type: DataType, values: Vec<Value>) -> Option<ColumnVector> {
    ColumnVector::new(data_type, values).ok()
}

/// Whether the answer's per-row conversion leaves a value of `data_type`
/// as it is: a packed integer, a canonical decimal at the answer's own
/// scale (which keeps its label and casts to itself), a canonical temporal,
/// plain text.
const fn converts_to_itself(data_type: DataType) -> bool {
    matches!(
        data_type,
        DataType::Int64
            | DataType::UInt64
            | DataType::Decimal { .. }
            | DataType::Date32
            | DataType::DateTime64 { .. }
            | DataType::Utf8
    )
}

/// A constant as a packed column of `rows` copies in the answer's type,
/// when its conversion leaves it canonical there; NULL as a column of NULLs.
fn constant_column(value: &Value, data_type: DataType, rows: usize) -> Option<ColumnVector> {
    let valid = crate::array::ValidityMask::from_bools(&vec![!matches!(value, Value::Null); rows]);
    let typed = match (data_type, value) {
        (DataType::Int64, Value::Int64(value)) => TypedValues::Int64(vec![*value; rows]),
        (DataType::Int64, Value::Null) => TypedValues::Int64(vec![0; rows]),
        (DataType::UInt64, Value::UInt64(value)) => TypedValues::UInt64(vec![*value; rows]),
        (DataType::UInt64, Value::Null) => TypedValues::UInt64(vec![0; rows]),
        (DataType::Decimal { scale, .. }, value) => {
            let units = match value {
                Value::Null => 0,
                Value::Utf8(text) => {
                    let units = pintail_types::parse_decimal_scaled(text, scale)?;
                    (pintail_types::format_decimal_scaled(units, scale) == *text)
                        .then_some(units)?
                }
                _ => return None,
            };
            TypedValues::Decimal128 {
                values: DecimalUnits::Wide(vec![units; rows]),
                scale,
                text: LazyText::decimal(scale),
            }
        }
        (DataType::Date32 | DataType::DateTime64 { .. }, value) => {
            let (unit, text) = match (data_type, value) {
                (_, Value::Null) => (0, None),
                (DataType::Date32, Value::Utf8(text)) => {
                    let days = pintail_types::parse_date_days(text)?;
                    (days, Some(pintail_types::format_date_days(days)? == *text))
                }
                (DataType::DateTime64 { fsp }, Value::Utf8(text)) => {
                    let micros = pintail_types::parse_datetime_micros(text)?;
                    (
                        micros,
                        Some(pintail_types::format_datetime_micros(micros, fsp)? == *text),
                    )
                }
                _ => return None,
            };
            if text == Some(false) {
                return None;
            }
            TypedValues::Temporal {
                units: vec![unit; rows],
                text: match data_type {
                    DataType::DateTime64 { fsp } => LazyText::datetime(fsp),
                    _ => LazyText::date(),
                },
            }
        }
        (DataType::Utf8, Value::Utf8(text)) => {
            let mut column = crate::array::StrColumn::default();
            for _ in 0..rows {
                column.push(text.as_bytes());
            }
            TypedValues::Utf8(column)
        }
        (DataType::Utf8, Value::Null) => {
            let mut column = crate::array::StrColumn::default();
            for _ in 0..rows {
                column.push(&[]);
            }
            TypedValues::Utf8(column)
        }
        _ => return None,
    };
    Some(ColumnVector::from_typed(data_type, typed, valid))
}

/// The answer gathered straight from packed branches: `sources[source]` at
/// row for each row's `(source, row)`. Every branch must already hold the
/// answer's type in a form its per-row conversion leaves as it is - a
/// column of exactly that type, or a constant converted once - so copying
/// units answers what converting each value would. `None` otherwise.
fn packed_answer(
    branches: &[Branch],
    picks: &[(u32, u32)],
    data_type: DataType,
    convert: &dyn Fn(&Value) -> Option<Value>,
) -> Option<ColumnVector> {
    if !converts_to_itself(data_type) {
        return None;
    }
    let rows = picks.len();
    let constants = branches
        .iter()
        .map(|branch| match branch {
            Branch::Column(_) => Some(None),
            Branch::Constant(value) => constant_column(&convert(value)?, data_type, rows).map(Some),
        })
        .collect::<Option<Vec<_>>>()?;
    let sources = branches
        .iter()
        .zip(&constants)
        .map(|(branch, constant)| match (branch, constant) {
            (Branch::Column(column), _) => {
                (column.data_type() == data_type && gather::packed(column)).then_some(column)
            }
            (Branch::Constant(_), Some(constant)) => Some(constant),
            (Branch::Constant(_), None) => None,
        })
        .collect::<Option<Vec<&ColumnVector>>>()?;
    gather::gather(&sources, picks, data_type).ok()
}

/// Each row's pick of the branch it takes, row by row; a row no branch
/// answers (unselected) takes the first, which nothing reads.
fn picks_of(chosen: &[Option<usize>]) -> Option<Vec<(u32, u32)>> {
    chosen
        .iter()
        .enumerate()
        .map(|(row, branch)| {
            Some((
                u32::try_from(branch.unwrap_or(0)).ok()?,
                u32::try_from(row).ok()?,
            ))
        })
        .collect()
}

/// `IF(condition, then, otherwise)`.
pub(super) fn if_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [condition, then, otherwise] = args else {
        return None;
    };
    let declared = data_type?;
    let condition = operand(batch, condition, effects)?;
    let rows = batch.row_count();
    let selection = batch.selection();
    let mut takes_then = vec![false; rows];
    let mut takes_otherwise = vec![false; rows];
    for row in selection.selected_rows() {
        // A condition row evaluation cannot read as a truth value is its
        // error to raise.
        let truth = mysql_truth(&value_at(&condition, row)?).ok()?;
        if truth.unwrap_or(false) {
            takes_then[row] = true;
        } else {
            takes_otherwise[row] = true;
        }
    }
    let then_batch = narrowed(batch, &takes_then)?;
    let otherwise_batch = narrowed(batch, &takes_otherwise)?;
    let branches = [
        Branch::of(operand(&then_batch, then, effects)?),
        Branch::of(operand(&otherwise_batch, otherwise, effects)?),
    ];
    let chosen = (0..rows)
        .map(|row| {
            if takes_then[row] {
                Some(0)
            } else {
                takes_otherwise[row].then_some(1)
            }
        })
        .collect::<Vec<_>>();
    let convert = |value: &Value| if_result(value, Some(declared)).ok();
    if let Some(packed) = packed_answer(&branches, &picks_of(&chosen)?, declared, &convert) {
        return Some(packed);
    }
    let mut values = Vec::with_capacity(rows);
    for (row, branch) in chosen.iter().enumerate() {
        values.push(match branch {
            Some(branch) => convert(&branches[*branch].value(row)?)?,
            None => Value::Null,
        });
    }
    answer(declared, values)
}

/// `COALESCE(first, ...)`: each argument read only where every earlier one
/// was NULL.
pub(super) fn coalesce_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let declared = data_type?;
    let rows = batch.row_count();
    let mut open = (0..rows)
        .map(|row| batch.selection().is_selected(row))
        .collect::<Vec<_>>();
    // Each row's first argument that is not NULL; a row every argument
    // leaves NULL answers NULL.
    let mut chosen = vec![None; rows];
    let mut branches = Vec::with_capacity(args.len());
    for argument in args {
        if !open.contains(&true) {
            break;
        }
        let reading = narrowed(batch, &open)?;
        let branch = Branch::of(operand(&reading, argument, effects)?);
        for row in 0..rows {
            if open[row] && !branch.is_null(row) {
                chosen[row] = Some(branches.len());
                open[row] = false;
            }
        }
        branches.push(branch);
    }
    if !branches
        .iter()
        .any(|branch| matches!(branch, Branch::Column(_)))
    {
        return None;
    }
    // The rows no argument answers are NULL in the answer; a NULL branch
    // stands for them where the answer is gathered.
    let nulls = branches.len();
    branches.push(Branch::Constant(Value::Null));
    let convert = |value: &Value| cast_scalar(value, Some(declared)).ok();
    let null_where_open = chosen
        .iter()
        .enumerate()
        .map(|(row, branch)| branch.or_else(|| batch.selection().is_selected(row).then_some(nulls)))
        .collect::<Vec<_>>();
    if let Some(packed) = packed_answer(&branches, &picks_of(&null_where_open)?, declared, &convert)
    {
        return Some(packed);
    }
    let mut values = Vec::with_capacity(rows);
    for (row, branch) in chosen.iter().enumerate() {
        values.push(match branch {
            Some(branch) => convert(&branches[*branch].value(row)?)?,
            None => Value::Null,
        });
    }
    answer(declared, values)
}

/// `NULLIF(left, right)`, both read for every row.
pub(super) fn null_if_column(
    batch: &RecordBatch,
    args: &[CompiledExpr],
    argument_types: &[Option<DataType>],
    data_type: Option<DataType>,
    collation: Collation,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    let [left, right] = args else {
        return None;
    };
    let declared = data_type?;
    let left = operand(batch, left, effects)?;
    let right = operand(batch, right, effects)?;
    if !left.varies() && !right.varies() {
        return None;
    }
    let mut values = Vec::with_capacity(batch.row_count());
    for row in 0..batch.row_count() {
        if !batch.selection().is_selected(row) {
            values.push(Value::Null);
            continue;
        }
        let (left, right) = (value_at(&left, row)?, value_at(&right, row)?);
        values.push(null_if(&left, &right, argument_types, Some(declared), collation).ok()?);
    }
    answer(declared, values)
}

#[cfg(test)]
mod tests {
    use pintail_sql::{BinaryOp, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::super::testing::{agrees_with_rows, batch_of, binary, scalar};
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, DecimalUnits, LazyText, RecordBatch, TypedValues};
    use crate::expression::CompiledExpr;

    fn decimal(units: &[Option<i64>]) -> ColumnVector {
        ColumnVector::from_typed(
            DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(
                    units.iter().map(|units| units.unwrap_or(0)).collect(),
                ),
                scale: 2,
                text: LazyText::decimal(2),
            },
            ValidityMask::from_bools(&units.iter().map(Option::is_some).collect::<Vec<_>>()),
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

    /// Columns: total DECIMAL(12,2) with zeros and NULLs, a divisor, a
    /// note. Row 3 is unselected.
    fn fixture() -> RecordBatch {
        batch_of(vec![
            decimal(&[
                Some(1050),
                Some(0),
                None,
                Some(0),
                Some(-775),
                Some(99),
                Some(0),
                Some(1),
            ]),
            decimal(&[
                Some(200),
                Some(0),
                Some(100),
                Some(0),
                Some(0),
                None,
                Some(300),
                Some(0),
            ]),
            texts(&[
                Some("alpha"),
                None,
                Some("beta"),
                Some("alpha"),
                Some("ALPHA"),
                Some(""),
                None,
                Some("gamma"),
            ]),
        ])
    }

    const RESULT: DataType = DataType::Decimal {
        precision: 14,
        scale: 2,
    };

    fn column(index: usize) -> CompiledExpr {
        CompiledExpr::Column(index)
    }

    fn literal(value: Value) -> CompiledExpr {
        CompiledExpr::Literal(value)
    }

    fn decimal_comparison(op: BinaryOp, left: CompiledExpr, right: CompiledExpr) -> CompiledExpr {
        scalar(
            ScalarFunction::DecimalComparison { op },
            vec![left, right],
            DataType::Boolean,
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)] // a table of cases, one check over them all
    fn conditionals_match_row_evaluation_and_its_warnings() {
        let batch = fixture();
        let zero = || literal(Value::Int64(0));
        let quotient = || {
            binary(
                BinaryOp::Divide,
                column(0),
                column(1),
                DataType::Decimal {
                    precision: 20,
                    scale: 6,
                },
            )
        };
        let expressions = [
            // CASE WHEN total = 0 THEN NULL ELSE total END
            (
                scalar(
                    ScalarFunction::If,
                    vec![
                        decimal_comparison(BinaryOp::Equal, column(0), zero()),
                        literal(Value::Null),
                        column(0),
                    ],
                    RESULT,
                ),
                RESULT,
            ),
            // A branch at a narrower scale keeps its own label.
            (
                scalar(
                    ScalarFunction::If,
                    vec![
                        decimal_comparison(BinaryOp::Greater, column(0), zero()),
                        zero(),
                        column(0),
                    ],
                    RESULT,
                ),
                RESULT,
            ),
            // Only rows whose divisor is not zero reach the division.
            (
                scalar(
                    ScalarFunction::If,
                    vec![
                        decimal_comparison(BinaryOp::Equal, column(1), zero()),
                        zero(),
                        quotient(),
                    ],
                    DataType::Decimal {
                        precision: 20,
                        scale: 6,
                    },
                ),
                DataType::Decimal {
                    precision: 20,
                    scale: 6,
                },
            ),
            (
                scalar(
                    ScalarFunction::Coalesce,
                    vec![
                        scalar(ScalarFunction::NullIf, vec![column(0), zero()], RESULT),
                        literal(Value::Utf8("1.25".to_owned())),
                    ],
                    RESULT,
                ),
                RESULT,
            ),
            (
                scalar(
                    ScalarFunction::Coalesce,
                    vec![column(2), literal(Value::Utf8("none".to_owned()))],
                    DataType::Utf8,
                ),
                DataType::Utf8,
            ),
            (
                scalar(
                    ScalarFunction::If,
                    vec![
                        binary(
                            BinaryOp::Equal,
                            column(2),
                            literal(Value::Utf8("alpha".to_owned())),
                            DataType::Boolean,
                        ),
                        literal(Value::Utf8("a".to_owned())),
                        literal(Value::Utf8("b".to_owned())),
                    ],
                    DataType::Utf8,
                ),
                DataType::Utf8,
            ),
        ];
        for (expression, declared) in &expressions {
            assert!(
                agrees_with_rows(expression, &batch, *declared),
                "{expression:?} has a kernel"
            );
            let _ = crate::execution::take_session_division_warnings();
            expression
                .evaluate_column(&batch, Some(*declared))
                .expect("kernel");
            let kernel = crate::execution::take_session_division_warnings();
            for row in batch.selection().selected_rows() {
                expression.evaluate(&batch, row).expect("row evaluation");
            }
            let rows = crate::execution::take_session_division_warnings();
            assert_eq!(kernel, rows, "{expression:?} warnings");
        }
    }
}
