//! A row constructor's `IN (subquery)` over a few members, answered as a
//! comparison with each member rather than a subquery per row.
//!
//! The binder rewrites `(a, b) IN (SELECT x, y FROM ..)` into `EXISTS
//! (SELECT 1 FROM (SELECT x, y FROM ..) AS <row-members> WHERE a = m0 AND
//! b = m1)`, plus a second EXISTS over `(..) IS NULL` for the undecided
//! answer. Each reads the outer row, so the dependent path answers it once
//! per outer row: a clone of the whole expression, a lookup and a compile
//! for every row, where the members themselves never change.
//!
//! When the member subquery reads nothing of the outer row, it is executed
//! once here. Holding at most [`MAX_MEMBERS`] rows, the EXISTS becomes
//! `IF(w(m1) OR w(m2) OR .., TRUE, FALSE)`, where `w(m)` is the EXISTS's own
//! WHERE with member `m`'s values in place of its columns: an ordinary
//! expression over the outer row, evaluated a batch at a time.
//!
//! What it must never change, and how each is kept:
//!
//! - **Comparison semantics.** Each member's values become literals typed as
//!   the columns they replace, as the per-row path makes literals of the
//!   outer row's. A column whose comparisons depend on more than its type -
//!   an ENUM, a TIMESTAMP, a BIT, a geometry, a fixed-width binary or a
//!   FLOAT of declared precision - leaves the subquery to the dependent
//!   path, and so does any comparison whose collation would change once a
//!   member column is a literal.
//! - **NULL.** `w(m)` is the WHERE itself, so a member's NULL makes it NULL
//!   exactly where the WHERE was NULL; `IF` answers false for that as
//!   EXISTS does for a row the WHERE does not pass. With no member the
//!   answer is false.
//! - **Errors.** A member subquery that fails, runs out of memory or holds
//!   more rows declines, and the dependent path answers - raising whatever
//!   it raises on the row that raises it.
//! - **Volatility.** A member subquery calling `RAND()` or the like is left
//!   to the dependent path, which runs it fresh.

use pintail_sql::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundQuery, ROW_MEMBERS_RELATION,
    ScalarFunction,
};
use pintail_types::{DataType, Value};

use super::{Execution, ScanProvider, bound_query_has_outer_refs};
use crate::collation::Collation;
use crate::{LogicalPlanner, Optimizer, PhysicalPlanner};

/// Member rows expanded into comparisons at most: every outer row compares
/// with each of them.
const MAX_MEMBERS: usize = 64;

/// `EXISTS` / `NOT EXISTS` over a row constructor's members as an expression
/// over the outer row, or `None` when the dependent path must answer it.
pub(super) fn expand(
    query: &BoundQuery,
    negated: bool,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<std::time::Instant>,
    collation: Collation,
) -> Option<BoundExpr> {
    let (members, filter) = shape(query)?;
    let rows = materialize(members, provider, memory_limit, deadline, collation)?;
    let mut any: Option<BoundExpr> = None;
    for row in &rows {
        let mut condition = filter.clone();
        if !substitute(&mut condition, row, &query.from[0].base)
            || !same_collations(filter, &condition)
        {
            return None;
        }
        any = Some(match any {
            None => condition,
            Some(previous) => BoundExpr {
                nullable: previous.nullable || condition.nullable,
                data_type: Some(DataType::Boolean),
                kind: BoundExprKind::Binary {
                    op: BinaryOp::Or,
                    left: Box::new(previous),
                    right: Box::new(condition),
                },
            },
        });
    }
    crate::counters::count(|counters| {
        counters.row_members_expanded = counters.row_members_expanded.saturating_add(1);
    });
    let boolean = |value: bool| BoundExpr {
        nullable: false,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Literal(Value::Boolean(value)),
    };
    Some(match any {
        None => boolean(negated),
        Some(any) => BoundExpr {
            nullable: false,
            data_type: Some(DataType::Boolean),
            kind: BoundExprKind::Scalar {
                function: ScalarFunction::If,
                args: vec![any, boolean(!negated), boolean(negated)],
            },
        },
    })
}

/// The member subquery and the EXISTS's WHERE, when `query` is the binder's
/// row-membership EXISTS over members that read no outer row.
fn shape(query: &BoundQuery) -> Option<(&BoundQuery, &BoundExpr)> {
    let [source] = query.from.as_slice() else {
        return None;
    };
    let members = source.base.input.as_deref()?;
    if !source.joins.is_empty()
        || source.base.relation_name != ROW_MEMBERS_RELATION
        || !query.group_by.is_empty()
        || !query.aggregates.is_empty()
        || !query.windows.is_empty()
        || query.having.is_some()
        || !query.union_all.is_empty()
        || !query.set_ops.is_empty()
        || query.recursive.is_some()
        || query
            .limit
            .is_some_and(|limit| limit.offset != 0 || limit.count == 0)
        || bound_query_has_outer_refs(members)
        || super::memo::query_is_volatile(members)
    {
        return None;
    }
    Some((members, query.filter.as_ref()?))
}

/// The member rows, at most [`MAX_MEMBERS`] of them.
fn materialize(
    members: &BoundQuery,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<std::time::Instant>,
    collation: Collation,
) -> Option<Vec<Vec<Value>>> {
    let width = members.projection.len();
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(members.clone())),
        collation,
    )
    .ok()?;
    if physical.output_fields().len() < width {
        return None;
    }
    let mut execution =
        Execution::start_with_deadline(physical, provider, memory_limit, deadline, collation)
            .ok()?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().ok()? {
        for row in batch.selection().selected_rows() {
            if rows.len() == MAX_MEMBERS {
                return None;
            }
            rows.push(
                (0..width)
                    .map(|column| batch.column(column)?.value(row).cloned())
                    .collect::<Option<Vec<_>>>()?,
            );
        }
    }
    Some(rows)
}

/// Replaces every member column of `expression` with that member's value
/// and reads every outer column as the operator's own. `false` for anything
/// the expansion does not carry.
fn substitute(
    expression: &mut BoundExpr,
    row: &[Value],
    members: &pintail_sql::BoundTable,
) -> bool {
    match &mut expression.kind {
        BoundExprKind::Column(column) if column.outer => {
            column.outer = false;
            true
        }
        BoundExprKind::Column(column) => {
            let Some(position) = member_position(column, members) else {
                return false;
            };
            let Some(value) = row.get(position) else {
                return false;
            };
            expression.nullable = matches!(value, Value::Null);
            expression.kind = BoundExprKind::Literal(value.clone());
            true
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            substitute(expr, row, members)
        }
        BoundExprKind::Binary { left, right, .. } => {
            substitute(left, row, members) && substitute(right, row, members)
        }
        BoundExprKind::Scalar { args, .. } => args
            .iter_mut()
            .all(|argument| substitute(argument, row, members)),
        _ => false,
    }
}

/// Where `column` sits among the members' columns, when it is one whose
/// comparisons a typed literal reproduces.
fn member_position(column: &BoundColumn, members: &pintail_sql::BoundTable) -> Option<usize> {
    if column.database_id != members.database_id
        || column.table_id != members.table_id
        || !column
            .relation_name
            .eq_ignore_ascii_case(&members.relation_name)
        || column.enum_labels.is_some()
        || column.timestamp
        || column.geometry
        || column.bit_width.is_some()
        || column.binary_width.is_some()
        || (column.float_decimals.is_some()
            && matches!(column.data_type, DataType::Float32 | DataType::Float64))
    {
        return None;
    }
    members
        .columns
        .iter()
        .position(|candidate| candidate.column_id == column.column_id)
}

/// Whether every operation of `substituted` compares under the collation
/// the same operation of `original` does. A leaf compares nothing: a
/// literal carries no collation of its own.
fn same_collations(original: &BoundExpr, substituted: &BoundExpr) -> bool {
    if !matches!(
        original.kind,
        BoundExprKind::Column(_) | BoundExprKind::Literal(_)
    ) && original.text_collation() != substituted.text_collation()
    {
        return false;
    }
    match (&original.kind, &substituted.kind) {
        (
            BoundExprKind::Unary { expr: left, .. } | BoundExprKind::IsNull { expr: left, .. },
            BoundExprKind::Unary { expr: right, .. } | BoundExprKind::IsNull { expr: right, .. },
        ) => same_collations(left, right),
        (
            BoundExprKind::Binary {
                left: original_left,
                right: original_right,
                ..
            },
            BoundExprKind::Binary { left, right, .. },
        ) => same_collations(original_left, left) && same_collations(original_right, right),
        (
            BoundExprKind::Scalar {
                args: original_args,
                ..
            },
            BoundExprKind::Scalar { args, .. },
        ) => original_args
            .iter()
            .zip(args)
            .all(|(original, substituted)| same_collations(original, substituted)),
        _ => true,
    }
}
