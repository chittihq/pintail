//! Compiled expressions evaluated a batch at a time over packed columns.
//!
//! Row-at-a-time evaluation turns every input cell into a [`Value`] and a
//! temporal or decimal one into text, parses that text back, and formats
//! its answer as text again: per row, per expression. Here an expression
//! whose every node has a kernel is evaluated once per batch over the
//! packed units the scan already produced, and answers with a packed column
//! whose text is derived only when something reads it as text.
//!
//! A kernel answers exactly what row evaluation would, or declines: given
//! an input shape, a declared type or a result it does not mirror - text
//! kept as written, a year outside what canonical text can spell - it
//! returns `None` and the caller evaluates the batch row by row. A row that
//! would raise an error declines the batch too, so the error the statement
//! reports is the one row evaluation raises, at the row it reaches first.
//! Rows the batch's selection excludes are computed but never decline it.
//!
//! What evaluation does besides answering - `MySQL`'s warnings - is held as
//! [`Effects`] and recorded only once the whole expression has answered, so
//! a batch declined part way leaves nothing behind for row evaluation to
//! record a second time.

use std::borrow::Cow;

use pintail_sql::{BinaryOp, ScalarFunction};
use pintail_types::{DataType, Value};

use super::CompiledExpr;
use crate::batch::{ColumnVector, RecordBatch};

mod compare;
mod conditional;
mod functions;
mod numeric;
mod temporal;

/// Warnings a batch's evaluation raised, for the selected rows only.
#[derive(Default)]
pub(super) struct Effects {
    divisions_by_zero: u64,
}

impl Effects {
    /// Notes a division by zero answered with NULL at `row`, when row
    /// evaluation would have read that row.
    fn division_by_zero(&mut self, batch: &RecordBatch, row: usize) {
        if batch.selection().is_selected(row) {
            self.divisions_by_zero = self.divisions_by_zero.saturating_add(1);
        }
    }

    fn record(self) {
        crate::execution::note_divisions_by_zero(self.divisions_by_zero);
    }
}

impl CompiledExpr {
    /// The expression evaluated over every row of `batch` as one column of
    /// `data_type`, when every node has a kernel; `None` sends the caller
    /// to row-at-a-time evaluation.
    pub(crate) fn evaluate_column(
        &self,
        batch: &RecordBatch,
        data_type: Option<DataType>,
    ) -> Option<ColumnVector> {
        let mut effects = Effects::default();
        let column = kernel(self, batch, data_type, &mut effects)?;
        effects.record();
        Some(column)
    }
}

fn kernel(
    expr: &CompiledExpr,
    batch: &RecordBatch,
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    match expr {
        CompiledExpr::Column(index) => batch
            .column(*index)
            .filter(|column| data_type.is_none_or(|declared| declared == column.data_type()))
            .cloned(),
        CompiledExpr::IsNull { expr, negated } => {
            compare::is_null_column(batch, expr, *negated, data_type, effects)
        }
        CompiledExpr::Unary {
            op,
            expr: argument,
            data_type: own,
            ..
        } => functions::unary_column(batch, *op, argument, *own, data_type, effects),
        CompiledExpr::Binary {
            op:
                op @ (BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessOrEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterOrEqual),
            left,
            right,
            collation,
            ..
        } => compare::comparison_column(batch, *op, left, right, *collation, data_type, effects),
        CompiledExpr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or | BinaryOp::Xor),
            left,
            right,
            ..
        } => compare::logic_column(batch, *op, left, right, data_type, effects),
        CompiledExpr::Binary {
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
            data_type: Some(DataType::Decimal { .. }),
            ..
        } => numeric::decimal_chain_column(expr, batch, data_type, effects),
        CompiledExpr::Binary {
            op: op @ (BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply),
            left,
            right,
            data_type: Some(DataType::Int64),
            ..
        } => numeric::integer_column(batch, *op, left, right, data_type, effects),
        CompiledExpr::Scalar {
            function:
                function @ (ScalarFunction::If | ScalarFunction::Coalesce | ScalarFunction::NullIf),
            args,
            argument_types,
            data_type: own,
            collation,
            ..
        } => {
            // The answer takes the node's own type, as row evaluation's does.
            if data_type.is_some_and(|declared| Some(declared) != *own) {
                return None;
            }
            match function {
                ScalarFunction::If => conditional::if_column(batch, args, *own, effects),
                ScalarFunction::Coalesce => {
                    conditional::coalesce_column(batch, args, *own, effects)
                }
                _ => conditional::null_if_column(
                    batch,
                    args,
                    argument_types,
                    *own,
                    *collation,
                    effects,
                ),
            }
        }
        CompiledExpr::Scalar {
            function,
            args,
            argument_types,
            literal_regex,
            data_type: own,
            collation,
            ..
        } => attempt(effects, |effects| {
            specific_scalar(batch, *function, args, data_type, effects)
        })
        .or_else(|| {
            // The answer takes the node's own type, as row evaluation's does.
            if data_type.is_some_and(|declared| Some(declared) != *own) {
                return None;
            }
            let call = functions::Call {
                function: *function,
                args,
                argument_types,
                literal_regex: literal_regex.as_ref(),
                data_type: *own,
                collation: *collation,
            };
            functions::scalar_column(batch, &call, effects)
        }),
        _ => None,
    }
}

/// A kernel that may decline after reading part of its expression: what
/// the part recorded is taken back, so whatever answers instead records it
/// once.
fn attempt(
    effects: &mut Effects,
    kernel: impl FnOnce(&mut Effects) -> Option<ColumnVector>,
) -> Option<ColumnVector> {
    let before = effects.divisions_by_zero;
    let column = kernel(effects);
    if column.is_none() {
        effects.divisions_by_zero = before;
    }
    column
}

/// The kernels written for one function.
fn specific_scalar(
    batch: &RecordBatch,
    function: ScalarFunction,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    effects: &mut Effects,
) -> Option<ColumnVector> {
    match function {
        ScalarFunction::DatePart(part) => {
            temporal::date_part_column(batch, args, part, data_type, effects)
        }
        ScalarFunction::DateInterval { unit, subtract } => {
            temporal::date_interval_column(batch, args, unit, subtract, data_type, effects)
        }
        ScalarFunction::Date | ScalarFunction::LastDay => {
            temporal::date_of_column(batch, args, function, data_type, effects)
        }
        ScalarFunction::DateDiff | ScalarFunction::TimestampDiff { .. } => {
            temporal::difference_column(batch, args, function, data_type, effects)
        }
        // A written CAST with no character count casts as the binder's own
        // casts do.
        ScalarFunction::Cast(target @ DataType::Decimal { .. })
        | ScalarFunction::DeclaredCast {
            target: target @ DataType::Decimal { .. },
            characters: None,
        } => numeric::decimal_cast_column(batch, args, target, data_type, effects),
        ScalarFunction::Cast(target)
        | ScalarFunction::DeclaredCast {
            target,
            characters: None,
        } => temporal::cast_column(batch, args, target, data_type, effects),
        ScalarFunction::DecimalComparison { op } => {
            numeric::decimal_comparison_column(batch, op, args, data_type, effects)
        }
        _ => None,
    }
}

/// The type an expression node declares for its answer; a column or a
/// constant declares none of its own.
fn declared(expr: &CompiledExpr) -> Option<DataType> {
    match expr {
        CompiledExpr::Unary { data_type, .. }
        | CompiledExpr::Binary { data_type, .. }
        | CompiledExpr::Scalar { data_type, .. } => *data_type,
        _ => None,
    }
}

/// One argument of a batch kernel, evaluated over the batch: a column the
/// batch holds or a kernel produced, or a constant.
enum Operand<'batch> {
    Column(Cow<'batch, ColumnVector>),
    Constant(&'batch Value),
}

impl Operand<'_> {
    const fn varies(&self) -> bool {
        matches!(self, Self::Column(_))
    }
}

/// `argument` over `batch`, or `None` when no kernel evaluates it.
fn operand<'batch>(
    batch: &'batch RecordBatch,
    argument: &'batch CompiledExpr,
    effects: &mut Effects,
) -> Option<Operand<'batch>> {
    match argument {
        CompiledExpr::Column(index) => batch
            .column(*index)
            .map(|column| Operand::Column(Cow::Borrowed(column))),
        CompiledExpr::Literal(value) => Some(Operand::Constant(value)),
        nested => kernel(nested, batch, declared(nested), effects)
            .map(|column| Operand::Column(Cow::Owned(column))),
    }
}

/// A boolean answer per row, NULL where `None`.
fn truth_column(answers: impl Iterator<Item = Option<bool>>) -> Option<ColumnVector> {
    ColumnVector::new(
        DataType::Boolean,
        answers
            .map(|answer| answer.map_or(Value::Null, Value::Boolean))
            .collect(),
    )
    .ok()
}

#[cfg(test)]
mod testing {
    use pintail_sql::ScalarFunction;
    use pintail_types::DataType;

    use super::CompiledExpr;
    use crate::batch::{ColumnVector, RecordBatch, SelectionMask};
    use crate::collation::Collation;

    /// The columns as a batch with every row selected but the fourth, which
    /// a kernel must compute and never decline or fail on.
    pub(super) fn batch_of(columns: Vec<ColumnVector>) -> RecordBatch {
        let rows = columns.first().map_or(0, ColumnVector::len);
        let mut batch = RecordBatch::new(rows, columns).expect("batch");
        let mut selection = SelectionMask::all(rows);
        selection.set(3, false).expect("row");
        batch.set_selection(selection).expect("selection");
        batch
    }

    pub(super) fn binary(
        op: pintail_sql::BinaryOp,
        left: CompiledExpr,
        right: CompiledExpr,
        data_type: DataType,
    ) -> CompiledExpr {
        CompiledExpr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            data_type: Some(data_type),
            collation: Collation::default(),
            overflow: None,
        }
    }

    pub(super) fn scalar(
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
    pub(super) fn agrees_with_rows(
        expression: &CompiledExpr,
        batch: &RecordBatch,
        data_type: DataType,
    ) -> bool {
        let Some(column) = expression.evaluate_column(batch, Some(data_type)) else {
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
}
