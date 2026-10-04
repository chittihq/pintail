//! Dependent subqueries of a select list, answered only for the rows a
//! `LIMIT` keeps.
//!
//! A projection holding a correlated subquery is answered row by row: every
//! input row plans and executes each of its subqueries. Under
//! `ORDER BY .. LIMIT n` the sort sits above that projection, so a page of
//! twenty rows out of a thousand candidates paid for a thousand rows of
//! subqueries and then threw nine hundred and eighty answers away. The same
//! held with no `ORDER BY` at all: the projection drained its whole input
//! before the limit above it saw a row.
//!
//! Which rows survive depends only on the sort keys, so the columns that
//! are not sort keys can wait. This operator evaluates the projection in
//! two steps:
//!
//! 1. For every input row, the columns that are sort keys or carry no
//!    dependent subquery are evaluated as before; each deferred column holds
//!    a NULL placeholder; and the input row itself rides behind them. The
//!    ordinary sort and limit operators then choose the surviving rows from
//!    exactly the key values, in exactly the arrival order, they were given
//!    before, so the same rows survive in the same order, ties included.
//! 2. For each surviving row, the deferred columns are evaluated against the
//!    input row carried with it, through the same per-row path.
//!
//! What stays as it was:
//!
//! - **The answer**: a deferred column's value is a function of its input
//!   row alone, so evaluating it after the sort gives the value it would
//!   have had before it. Volatile expressions are never deferred - their
//!   values depend on how many rows were evaluated before them.
//! - **Sort keys**: a column the sort reads is evaluated for every row,
//!   dependent subquery or not. Only the others wait.
//! - **Memory**: with a sort, the candidates are cut back to the rows the
//!   limit can still return whenever they outgrow a bound, so the operator
//!   holds a bounded number of input rows rather than all of them. Rows
//!   with equal keys order by arrival and a cut keeps them in that order,
//!   so cutting early keeps the rows one final sort would have kept.
//!   Without a sort the input is simply not read past the limit.
//!
//! One thing does change: a subquery that would fail on a row the limit
//! discards (more than one row returned, say) is no longer run, so the
//! statement answers where it used to raise.

use std::time::Instant;

use pintail_sql::{
    BoundColumn, BoundExpr, BoundExprKind, BoundOrderKey, BoundProjection, ScalarFunction,
};
use pintail_types::{DataType, Value};

use super::memo::DependentMemo;
use super::{
    DependentRow, ExecError, MemoryTracker, PhysicalPlan, PullOperator, ScanProvider, batch_row,
    build_operator, estimated_row_payload_bytes, expression_has_dependent_subquery, hollow_clone,
    projection_output_columns, record_dependent_memo, resolve_dependent_expr_subqueries,
    rows_to_columns,
};
use crate::collation::Collation;
use crate::expression::CompiledExpr;
use crate::{DEFAULT_BATCH_ROWS, RecordBatch};

/// Candidates held beyond the rows the limit keeps before they are cut back.
const CANDIDATE_SLACK_ROWS: usize = 8_192;

fn shape(plan: &PhysicalPlan) -> Option<(&[BoundProjection], &[BoundOrderKey])> {
    match plan {
        PhysicalPlan::Sort { input, keys, .. } => match input.as_ref() {
            PhysicalPlan::Project { expressions, .. } => Some((expressions, keys)),
            _ => None,
        },
        PhysicalPlan::Project { expressions, .. } => Some((expressions, &[])),
        _ => None,
    }
}

/// Whether the expression calls a function whose value depends on when, or
/// how often, it is evaluated. A subquery is looked at only from outside:
/// its own select list runs once per outer row either way.
fn calls_volatile_function(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Scalar { function, args } => {
            matches!(
                function,
                ScalarFunction::Rand
                    | ScalarFunction::Uuid
                    | ScalarFunction::UuidShort
                    | ScalarFunction::UserVariableRead
                    | ScalarFunction::UserVariableAssign
            ) || args.iter().any(calls_volatile_function)
        }
        BoundExprKind::InSubquery { expr, .. }
        | BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => calls_volatile_function(expr),
        BoundExprKind::Binary { left, right, .. } => {
            calls_volatile_function(left) || calls_volatile_function(right)
        }
        BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

fn deferrable(index: usize, projection: &BoundProjection, keys: &[BoundOrderKey]) -> bool {
    expression_has_dependent_subquery(&projection.expr)
        && !calls_volatile_function(&projection.expr)
        && !keys.iter().any(|key| key.index == index)
}

/// Whether the input of a limit is a projection (directly, or under the
/// limit's sort) with a dependent-subquery column that can wait for it.
pub(super) fn applies(input: &PhysicalPlan, offset: u64, count: u64) -> bool {
    usize::try_from(offset.saturating_add(count)).is_ok()
        && shape(input).is_some_and(|(expressions, keys)| {
            expressions
                .iter()
                .enumerate()
                .any(|(index, projection)| deferrable(index, projection, keys))
        })
}

/// A column evaluated for every input row.
enum Eager {
    /// Waits for the limit: a NULL placeholder until then.
    Deferred,
    Compiled(CompiledExpr),
    /// A sort key with a dependent subquery of its own.
    Dependent,
}

/// The first `skip + take` rows of `rows` under `keys`, less the first
/// `skip`, through the ordinary sort and limit operators.
fn leading_rows(
    rows: Vec<Vec<Value>>,
    column_types: &[DataType],
    keys: &[BoundOrderKey],
    (skip, take): (u64, u64),
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let source = PullOperator::Rows {
        rows,
        cursor: 0,
        column_types: column_types.to_vec(),
    };
    let ordered = if keys.is_empty() {
        source
    } else {
        PullOperator::Sort {
            input: Box::new(source),
            keys: keys.to_vec(),
            column_types: column_types.to_vec(),
            top_k: usize::try_from(skip.saturating_add(take)).ok(),
            trim: 0,
            state: None,
            collation,
            fallback: None,
        }
    };
    let mut limited = PullOperator::Limit {
        input: Box::new(ordered),
        skip,
        take,
    };
    let mut kept = Vec::new();
    while let Some(batch) = limited.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            kept.push(batch_row(&batch, row)?);
        }
    }
    limited.release_reservations(memory);
    Ok(kept)
}

fn payload_bytes(rows: &[Vec<Value>]) -> usize {
    rows.iter()
        .map(|row| estimated_row_payload_bytes(row))
        .fold(0_usize, usize::saturating_add)
}

/// Builds the limit over `input` (see [`applies`]) as materialized rows.
#[allow(clippy::too_many_lines)] // two evaluation passes around one sort
pub(super) fn build(
    input: PhysicalPlan,
    offset: u64,
    count: u64,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<(PullOperator, Vec<BoundColumn>), ExecError> {
    let (input, expressions, keys, trim) = match input {
        PhysicalPlan::Sort {
            input, keys, trim, ..
        } => match *input {
            PhysicalPlan::Project { input, expressions } => (input, expressions, keys, trim),
            _ => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "deferred projection needs a projection under its sort",
                ));
            }
        },
        PhysicalPlan::Project { input, expressions } => (input, expressions, Vec::new(), 0),
        _ => {
            return Err(ExecError::InvalidPhysicalPlan(
                "deferred projection needs a projection",
            ));
        }
    };
    let keep = usize::try_from(offset.saturating_add(count)).unwrap_or(usize::MAX);
    let (mut input, columns) = build_operator(*input, provider, memory, collation)?;
    let width = expressions.len();
    let visible = width.saturating_sub(trim);
    let mut output_columns = projection_output_columns(&expressions);
    output_columns.truncate(visible);

    let eager = expressions
        .iter()
        .enumerate()
        .map(|(index, projection)| {
            Ok(if deferrable(index, projection, &keys) {
                Eager::Deferred
            } else if expression_has_dependent_subquery(&projection.expr) {
                Eager::Dependent
            } else {
                Eager::Compiled(CompiledExpr::compile(
                    &projection.expr,
                    &columns,
                    collation,
                )?)
            })
        })
        .collect::<Result<Vec<_>, ExecError>>()?;
    let projected_types = expressions
        .iter()
        .map(|projection| projection.expr.data_type.unwrap_or(DataType::Utf8))
        .collect::<Vec<_>>();
    let input_types = columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<_>>();
    let candidate_types = projected_types
        .iter()
        .chain(&input_types)
        .copied()
        .collect::<Vec<_>>();

    // Pass one: every input row, with its deferred columns left empty.
    let mut eager_memo = DependentMemo::for_expressions(
        expressions
            .iter()
            .zip(&eager)
            .filter(|(_, eager)| matches!(eager, Eager::Dependent))
            .map(|(projection, _)| &projection.expr),
    );
    let cut_at = keep.saturating_add(keep.max(CANDIDATE_SLACK_ROWS));
    let mut candidates: Vec<Vec<Value>> = Vec::new();
    let mut reserved = 0_usize;
    let mut input_rows = 0_usize;
    'input: while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            if keys.is_empty() && candidates.len() >= keep {
                break 'input;
            }
            let mut values = Vec::with_capacity(width.saturating_add(columns.len()));
            let context = DependentRow {
                batch: &batch,
                row,
                columns: &columns,
                provider,
                memory,
                collation,
                ahead: &[],
            };
            eager_memo.begin_row();
            for (projection, eager) in expressions.iter().zip(&eager) {
                values.push(match eager {
                    Eager::Deferred => Value::Null,
                    Eager::Compiled(compiled) => compiled.evaluate(&batch, row)?,
                    Eager::Dependent => {
                        let mut expression = hollow_clone(&projection.expr);
                        resolve_dependent_expr_subqueries(
                            &mut expression,
                            &context,
                            &mut eager_memo,
                        )?;
                        CompiledExpr::compile(&expression, &columns, collation)?
                            .evaluate(&batch, row)?
                    }
                });
            }
            values.extend(batch_row(&batch, row)?);
            let row_bytes = estimated_row_payload_bytes(&values);
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            memory.reserve(row_bytes)?;
            reserved = reserved.saturating_add(row_bytes);
            candidates.push(values);
            input_rows = input_rows.saturating_add(1);
            if !keys.is_empty() && candidates.len() >= cut_at {
                // Keep the offset rows too: the final cut skips them.
                candidates = leading_rows(
                    candidates,
                    &candidate_types,
                    &keys,
                    (0, u64::try_from(keep).unwrap_or(u64::MAX)),
                    memory,
                    collation,
                )?;
                let held = payload_bytes(&candidates);
                memory.release(reserved.saturating_sub(held));
                reserved = reserved.min(held);
            }
        }
    }
    input.release_reservations(memory);
    drop(input);
    record_dependent_memo(eager_memo.finish(memory));
    let mut survivors = leading_rows(
        candidates,
        &candidate_types,
        &keys,
        (offset, count),
        memory,
        collation,
    )?;
    let held = payload_bytes(&survivors);
    memory.release(reserved.saturating_sub(held));

    // Pass two: the deferred columns, for the rows that are left.
    let deferred = expressions
        .iter()
        .zip(&eager)
        .enumerate()
        .filter(|(_, (_, eager))| matches!(eager, Eager::Deferred))
        .map(|(index, (projection, _))| (index, projection))
        .collect::<Vec<_>>();
    let profile = memory.profile.as_ref().map(|sink| {
        let slot = sink.enter(format!(
            "DeferredProject columns={} input_rows={input_rows} rows={}",
            deferred.len(),
            survivors.len()
        ));
        (sink, slot, Instant::now())
    });
    let carried = survivors
        .iter_mut()
        .map(|row| row.split_off(width.min(row.len())))
        .collect::<Vec<_>>();
    let mut memo =
        DependentMemo::for_expressions(deferred.iter().map(|(_, projection)| &projection.expr));
    for (chunk_index, chunk) in carried.chunks(DEFAULT_BATCH_ROWS).enumerate() {
        let batch = RecordBatch::new(chunk.len(), rows_to_columns(chunk, &input_types)?)?;
        let base = chunk_index.saturating_mul(DEFAULT_BATCH_ROWS);
        for row in 0..chunk.len() {
            let context = DependentRow {
                batch: &batch,
                row,
                columns: &columns,
                provider,
                memory,
                collation,
                ahead: &[],
            };
            memo.begin_row();
            for (index, projection) in &deferred {
                let mut expression = hollow_clone(&projection.expr);
                resolve_dependent_expr_subqueries(&mut expression, &context, &mut memo)?;
                let value = CompiledExpr::compile(&expression, &columns, collation)?
                    .evaluate(&batch, row)?;
                if let Some(slot) = survivors
                    .get_mut(base.saturating_add(row))
                    .and_then(|values| values.get_mut(*index))
                {
                    *slot = value;
                }
            }
        }
    }
    record_dependent_memo(memo.finish(memory));
    if let Some((sink, slot, started)) = profile {
        sink.leave(slot, started.elapsed());
    }
    for row in &mut survivors {
        row.truncate(visible);
    }
    let mut column_types = projected_types;
    column_types.truncate(visible);
    Ok((
        PullOperator::Rows {
            rows: survivors,
            cursor: 0,
            column_types,
        },
        output_columns,
    ))
}
