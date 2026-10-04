//! Dates `MySQL` rewrites as it copies them into an intermediate result.
//!
//! A source can hold a DATE or DATETIME no calendar has: a day past its
//! month's end written under `ALLOW_INVALID_DATES`, a zero month or day.
//! `MySQL` checks a value against the session's mode when it writes it into
//! a grouping, deduplication or union result, and a value the mode rejects
//! is written as the zero date. A column copied as it is gets that check
//! only under `NO_ZERO_DATE`, `NO_ZERO_IN_DATE` or `ALLOW_INVALID_DATES`; a
//! computed value, or one converted to another calendar type, always does.
//! The rejections are a day past its month's end unless the mode has
//! `ALLOW_INVALID_DATES`, and a zero month or day under `NO_ZERO_IN_DATE`.
//!
//! The check is a calendar CAST to the value's own type whose policy
//! carries [`COPY_CHECK`]. Column statistics that prove every stored value
//! a real calendar date leave the key as it is, so the check costs nothing
//! there.

use pintail_types::{DataType, Value};

use crate::bound::{
    AggregateFunction, BoundExpr, BoundExprKind, BoundQuery, BoundTable, ScalarFunction,
};

/// Policy bit marking a calendar CAST as the copy check: a value the mode
/// rejects becomes the zero date rather than NULL.
pub const COPY_CHECK: u64 = 0b10_0000;

/// `expr` as `MySQL` writes it into an intermediate result. `converted`
/// says the copy changes the value's type, which checks it in every mode.
pub(super) fn copied(expr: BoundExpr, tables: &[BoundTable], converted: bool) -> BoundExpr {
    let Some(target @ (DataType::Date32 | DataType::DateTime64 { .. })) = expr.data_type else {
        return expr;
    };
    // A TIMESTAMP holds only real instants and the zero date.
    if expr.is_source_timestamp()
        || expr.session_timestamp_source().is_some()
        || is_copy_check(&expr)
    {
        return expr;
    }
    let mode = crate::session_parse_mode();
    let column = matches!(expr.kind, BoundExprKind::Column(_));
    if column && !converted && !mode.copies_temporals_by_reading() {
        return expr;
    }
    if proven_calendar(&expr, tables) {
        return expr;
    }
    let policy = (u64::from(mode.no_zero_in_date) << 1)
        | (u64::from(mode.allow_invalid_dates) << 2)
        | COPY_CHECK;
    BoundExpr {
        data_type: expr.data_type,
        nullable: expr.nullable,
        kind: BoundExprKind::Scalar {
            function: ScalarFunction::Cast(target),
            args: vec![
                expr,
                BoundExpr {
                    data_type: Some(DataType::UInt64),
                    nullable: false,
                    kind: BoundExprKind::Literal(Value::UInt64(policy)),
                },
            ],
        },
    }
}

fn is_copy_check(expr: &BoundExpr) -> bool {
    matches!(
        &expr.kind,
        BoundExprKind::Scalar { function: ScalarFunction::Cast(_), args }
            if matches!(
                args.get(1).map(|policy| &policy.kind),
                Some(BoundExprKind::Literal(Value::UInt64(policy))) if policy & COPY_CHECK != 0
            )
    )
}

/// Whether every DATE or DATETIME column `expr` reads is proven by its
/// table's statistics to hold only real calendar dates. Text and numbers
/// read as dates are checked by the session's mode as they are read.
fn proven_calendar(expr: &BoundExpr, tables: &[BoundTable]) -> bool {
    match &expr.kind {
        BoundExprKind::Column(column) => {
            if column.timestamp
                || !matches!(
                    column.data_type,
                    DataType::Date32 | DataType::DateTime64 { .. }
                )
            {
                return true;
            }
            tables
                .iter()
                .find(|table| {
                    table.database_id == column.database_id && table.table_id == column.table_id
                })
                .and_then(|table| table.column_statistics.as_ref())
                .and_then(|statistics| statistics.get().column(column.column_id).copied())
                .is_some_and(|facts| facts.calendar_exact)
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            proven_calendar(expr, tables)
        }
        BoundExprKind::Binary { left, right, .. } => {
            proven_calendar(left, tables) && proven_calendar(right, tables)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(|arg| proven_calendar(arg, tables)),
        _ => false,
    }
}

/// Applies the copy check to everything `query` writes into an
/// intermediate result: its grouping keys, the values DISTINCT compares,
/// `COUNT(DISTINCT)` arguments, and the columns of a deduplicating union,
/// in every branch. A branch value the union converts to another calendar
/// type is checked in every mode. Window partitions are left as they are.
pub(super) fn check_copies(query: &mut BoundQuery) {
    let set = !query.union_all.is_empty() || !query.set_ops.is_empty();
    let deduplicated = query.union_distinct || !query.set_ops.is_empty();
    check_branch(query, set, deduplicated);
}

fn check_branch(query: &mut BoundQuery, set: bool, deduplicated: bool) {
    // A rollup, and a grouping with a DISTINCT aggregate, sorts its rows
    // and totals each run instead of copying them: its keys are as stored.
    if super::ROLLUP_GROUPING.get() {
        return;
    }
    let tables = std::mem::take(&mut query.tables);
    let filtered =
        query.filter.is_some() || query.from.iter().any(|source| !source.joins.is_empty());
    // What MySQL reads in the order of a covering source index it compares
    // as stored, with no copy.
    let through_index = |expr: &BoundExpr, query: &BoundQuery| {
        matches!(&expr.kind, BoundExprKind::Column(column)
        if super::index_read::reads_column_through_index(
            &tables,
            filtered,
            column,
            query
                .projection
                .iter()
                .map(|item| &item.expr)
                .chain(query.aggregates.iter().filter_map(|aggregate| aggregate.expr.as_ref())),
        ))
    };
    let sorted = query.aggregates.iter().any(|aggregate| aggregate.distinct);
    let indexed_key = matches!(query.group_by.as_slice(), [key] if through_index(key, query));
    let copied_groups = !query.group_by.is_empty() && !sorted && !indexed_key;
    if copied_groups {
        let keys = std::mem::take(&mut query.group_by);
        query.group_by = keys
            .into_iter()
            .map(|key| copied(key, &tables, false))
            .collect();
    }
    let indexed_distinct = query.group_by.is_empty()
        && (query
            .projection
            .iter()
            .any(|item| through_index(&item.expr, query))
            || query.aggregates.iter().any(|aggregate| {
                aggregate.distinct
                    && aggregate.function == AggregateFunction::Count
                    && aggregate
                        .expr
                        .as_ref()
                        .is_some_and(|expr| through_index(expr, query))
            }));
    for aggregate in &mut query.aggregates {
        // Each DISTINCT aggregate copies its argument to remove duplicates.
        if aggregate.distinct
            && !indexed_distinct
            && matches!(
                aggregate.function,
                AggregateFunction::Count | AggregateFunction::GroupConcat
            )
        {
            aggregate.expr = aggregate
                .expr
                .take()
                .map(|expr| copied(expr, &tables, false));
        }
    }
    let removes_duplicates = (query.distinct && !indexed_distinct) || deduplicated;
    check_projection(query, &tables, set, removes_duplicates, copied_groups);
    query.tables = tables;
    for branch in &mut query.union_all {
        check_branch(branch, set, deduplicated);
    }
    for (_, branch) in &mut query.set_ops {
        check_branch(branch, set, deduplicated);
    }
}

/// The copy check on each output column a branch writes into its result.
/// `set` says the branch belongs to a set operation, `removes_duplicates`
/// that its rows are compared as they are copied, and `copied_groups` that
/// it groups through a copied result.
fn check_projection(
    query: &mut BoundQuery,
    tables: &[BoundTable],
    set: bool,
    removes_duplicates: bool,
    copied_groups: bool,
) {
    let visible = query.projection.len() - query.hidden_sort_columns;
    for item in query.projection.iter_mut().take(visible) {
        let expr = std::mem::replace(
            &mut item.expr,
            BoundExpr {
                data_type: None,
                nullable: true,
                kind: BoundExprKind::Literal(Value::Null),
            },
        );
        item.expr = match expr {
            // A union's conversion to the column's common type.
            BoundExpr {
                kind:
                    BoundExprKind::Scalar {
                        function: ScalarFunction::Cast(target),
                        args,
                    },
                data_type,
                nullable,
            } if set && args.len() == 1 && args[0].data_type != Some(target) => copied(
                BoundExpr {
                    kind: BoundExprKind::Scalar {
                        function: ScalarFunction::Cast(target),
                        args,
                    },
                    data_type,
                    nullable,
                },
                tables,
                true,
            ),
            expr if removes_duplicates => copied(expr, tables, false),
            // A computed value the grouping result stores beside its keys.
            expr if copied_groups && matches!(expr.kind, BoundExprKind::Scalar { .. }) => {
                copied(expr, tables, true)
            }
            expr => expr,
        };
    }
}
