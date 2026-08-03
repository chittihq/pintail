use std::collections::BTreeSet;

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundProjection, ScalarFunction,
};
use pintail_types::{DataType, Value};

use crate::{
    LogicalPlan,
    expression::{
        evaluate_binary as evaluate_runtime_binary, evaluate_unary as evaluate_runtime_unary,
        mysql_truth,
    },
};

/// Deterministic rule-based logical optimizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Optimizer;

impl Optimizer {
    /// Applies semantics-preserving v1 logical rewrites.
    #[must_use]
    pub fn optimize(plan: LogicalPlan) -> LogicalPlan {
        // MySQL pins every current-time function in a statement to one
        // timestamp; capture it once so folding sees a single instant.
        STATEMENT_NOW.set(Some(StatementNow {
            local: chrono::Local::now().naive_local(),
            unix: chrono::Utc::now().timestamp(),
        }));
        let plan = fold_constants(plan);
        let plan = push_predicates(plan);
        let plan = replace_metadata_counts(plan);
        let plan = reorder_cross_joins(plan);
        let plan = push_aggregates_through_identity_joins(plan);
        let mut plan = plan;
        prune_projections(&mut plan);
        push_limits(&mut plan);
        plan
    }
}

#[allow(clippy::too_many_lines)] // exhaustive plan-node recursion reads best unsplit
fn push_aggregates_through_identity_joins(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let input = push_aggregates_through_identity_joins(*input);
            let mut referenced = BTreeSet::new();
            for expression in &group_by {
                referenced.extend(referenced_tables(expression));
            }
            for aggregate in &aggregates {
                if let Some(expression) = &aggregate.expr {
                    referenced.extend(referenced_tables(expression));
                }
            }
            let input = if let LogicalPlan::CrossJoin { mut inputs } = input {
                inputs.retain(|input| !is_unreferenced_identity(input, &referenced));
                match inputs.len() {
                    0 => LogicalPlan::OneRow,
                    1 => inputs.pop().expect("one retained cross-join input"),
                    _ => LogicalPlan::CrossJoin { inputs },
                }
            } else {
                input
            };
            LogicalPlan::Aggregate {
                input: Box::new(input),
                group_by,
                aggregates,
            }
        }
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            columns,
        },
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs
                .into_iter()
                .map(push_aggregates_through_identity_joins)
                .collect(),
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs
                .into_iter()
                .map(push_aggregates_through_identity_joins)
                .collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(push_aggregates_through_identity_joins(*left)),
            right: Box::new(push_aggregates_through_identity_joins(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(push_aggregates_through_identity_joins(*anchor)),
            member: Box::new(push_aggregates_through_identity_joins(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(push_aggregates_through_identity_joins(*left)),
            right: Box::new(push_aggregates_through_identity_joins(*right)),
            kind,
            condition,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            predicate,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            expressions,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            trim,
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            keys,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(push_aggregates_through_identity_joins(*input)),
            limit,
        },
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
    }
}

fn is_unreferenced_identity(plan: &LogicalPlan, referenced: &BTreeSet<TableKey>) -> bool {
    match plan {
        LogicalPlan::OneRow => true,
        LogicalPlan::Scan(scan) => {
            scan.predicates.is_empty()
                && scan.limit.is_none()
                && scan.table.row_count == Some(1)
                && !referenced.contains(&table_key(&scan.table))
        }
        _ => false,
    }
}

#[allow(clippy::too_many_lines)]
fn replace_metadata_counts(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } if group_by.is_empty()
            && aggregates.as_slice()
                == [BoundAggregate {
                    function: AggregateFunction::Count,
                    expr: None,
                    distinct: false,
                    data_type: Some(DataType::UInt64),
                    nullable: false,
                    separator: None,
                    order_within: Vec::new(),
                }] =>
        {
            match *input {
                LogicalPlan::Scan(scan) if scan.predicates.is_empty() => {
                    if let Some(row_count) = scan.table.row_count {
                        LogicalPlan::Project {
                            input: Box::new(LogicalPlan::OneRow),
                            expressions: vec![BoundProjection {
                                name: "COUNT(*)".to_owned(),
                                expr: literal_expr(Value::UInt64(row_count)),
                            }],
                        }
                    } else {
                        LogicalPlan::Aggregate {
                            input: Box::new(LogicalPlan::Scan(scan)),
                            group_by,
                            aggregates,
                        }
                    }
                }
                input => LogicalPlan::Aggregate {
                    input: Box::new(replace_metadata_counts(input)),
                    group_by,
                    aggregates,
                },
            }
        }
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(replace_metadata_counts).collect(),
        },
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(replace_metadata_counts(*input)),
            columns,
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs.into_iter().map(replace_metadata_counts).collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(replace_metadata_counts(*left)),
            right: Box::new(replace_metadata_counts(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(replace_metadata_counts(*anchor)),
            member: Box::new(replace_metadata_counts(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(replace_metadata_counts(*left)),
            right: Box::new(replace_metadata_counts(*right)),
            kind,
            condition,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(replace_metadata_counts(*input)),
            predicate,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(replace_metadata_counts(*input)),
            group_by,
            aggregates,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(replace_metadata_counts(*input)),
            expressions,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(replace_metadata_counts(*input)),
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(replace_metadata_counts(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            trim,
            input: Box::new(replace_metadata_counts(*input)),
            keys,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(replace_metadata_counts(*input)),
            limit,
        },
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
    }
}

#[allow(clippy::too_many_lines)] // exhaustive plan-node recursion reads best unsplit
fn fold_constants(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(fold_constants(*input)),
            columns,
        },
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(fold_constants).collect(),
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs.into_iter().map(fold_constants).collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(fold_constants(*left)),
            right: Box::new(fold_constants(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(fold_constants(*anchor)),
            member: Box::new(fold_constants(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(fold_constants(*left)),
            right: Box::new(fold_constants(*right)),
            kind,
            condition: condition.map(fold_expr),
        },
        LogicalPlan::Filter { input, predicate } => {
            let input = fold_constants(*input);
            let predicate = fold_expr(predicate);
            match literal_truth(&predicate) {
                Some(true) => input,
                Some(false) => LogicalPlan::Empty,
                None => LogicalPlan::Filter {
                    input: Box::new(input),
                    predicate,
                },
            }
        }
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(fold_constants(*input)),
            expressions: expressions
                .into_iter()
                .map(|projection| BoundProjection {
                    name: projection.name,
                    expr: fold_expr(projection.expr),
                })
                .collect(),
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(fold_constants(*input)),
            group_by: group_by.into_iter().map(fold_expr).collect(),
            aggregates: aggregates
                .into_iter()
                .map(|aggregate| BoundAggregate {
                    expr: aggregate.expr.map(fold_expr),
                    ..aggregate
                })
                .collect(),
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(fold_constants(*input)),
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(fold_constants(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            trim,
            input: Box::new(fold_constants(*input)),
            keys,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(fold_constants(*input)),
            limit,
        },
    }
}

#[derive(Clone, Copy)]
struct StatementNow {
    local: chrono::NaiveDateTime,
    unix: i64,
}

thread_local! {
    static STATEMENT_NOW: std::cell::Cell<Option<StatementNow>> =
        const { std::cell::Cell::new(None) };
}

/// The literal a zero-argument current-time function folds to under the
/// pinned statement timestamp.
fn statement_time_literal(function: ScalarFunction, now: StatementNow) -> Option<Value> {
    match function {
        ScalarFunction::Now => Some(Value::Utf8(
            now.local.format("%Y-%m-%d %H:%M:%S").to_string(),
        )),
        ScalarFunction::CurrentDate => Some(Value::Utf8(now.local.format("%Y-%m-%d").to_string())),
        ScalarFunction::Curtime => Some(Value::Utf8(now.local.format("%H:%M:%S").to_string())),
        ScalarFunction::UnixTimestamp => Some(Value::UInt64(u64::try_from(now.unix).unwrap_or(0))),
        _ => None,
    }
}

fn fold_expr(expr: BoundExpr) -> BoundExpr {
    let folded = match expr.kind {
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Literal(_) => return expr,
        BoundExprKind::InSubquery {
            expr: child,
            query,
            negated,
        } => BoundExpr {
            kind: BoundExprKind::InSubquery {
                expr: Box::new(fold_expr(*child)),
                query,
                negated,
            },
            data_type: expr.data_type,
            nullable: expr.nullable,
        },
        BoundExprKind::Unary { op, expr: child } => BoundExpr {
            kind: BoundExprKind::Unary {
                op,
                expr: Box::new(fold_expr(*child)),
            },
            data_type: expr.data_type,
            nullable: expr.nullable,
        },
        BoundExprKind::Binary { op, left, right } => BoundExpr {
            kind: BoundExprKind::Binary {
                op,
                left: Box::new(fold_expr(*left)),
                right: Box::new(fold_expr(*right)),
            },
            data_type: expr.data_type,
            nullable: expr.nullable,
        },
        BoundExprKind::IsNull {
            expr: child,
            negated,
        } => BoundExpr {
            kind: BoundExprKind::IsNull {
                expr: Box::new(fold_expr(*child)),
                negated,
            },
            data_type: expr.data_type,
            nullable: expr.nullable,
        },
        BoundExprKind::Scalar { function, args } => {
            if args.is_empty()
                && let Some(now) = STATEMENT_NOW.get()
                && let Some(value) = statement_time_literal(function, now)
            {
                return BoundExpr {
                    kind: BoundExprKind::Literal(value),
                    data_type: expr.data_type,
                    nullable: false,
                };
            }
            BoundExpr {
                kind: BoundExprKind::Scalar {
                    function,
                    args: args.into_iter().map(fold_expr).collect(),
                },
                data_type: expr.data_type,
                nullable: expr.nullable,
            }
        }
    };

    evaluate_constant(&folded).map_or(folded, literal_expr)
}

fn evaluate_constant(expr: &BoundExpr) -> Option<Value> {
    match &expr.kind {
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::InSubquery { .. }
        | BoundExprKind::Scalar { .. } => None,
        BoundExprKind::Literal(value) => Some(value.clone()),
        BoundExprKind::Unary { op, expr: child } => {
            let value = evaluate_constant(child)?;
            evaluate_runtime_unary(*op, &value, expr.data_type).ok()
        }
        BoundExprKind::Binary { op, left, right } => {
            let left = evaluate_constant(left)?;
            let right = evaluate_constant(right)?;
            evaluate_runtime_binary(*op, &left, &right, expr.data_type).ok()
        }
        BoundExprKind::IsNull { expr, negated } => {
            let is_null = matches!(evaluate_constant(expr)?, Value::Null);
            Some(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
    }
}

fn literal_expr(value: Value) -> BoundExpr {
    BoundExpr {
        data_type: value.data_type(),
        nullable: matches!(value, Value::Null),
        kind: BoundExprKind::Literal(value),
    }
}

fn literal_truth(expr: &BoundExpr) -> Option<bool> {
    let BoundExprKind::Literal(value) = &expr.kind else {
        return None;
    };
    mysql_truth(value).ok().map(|value| value.unwrap_or(false))
}

fn push_predicates(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(push_predicates(*input)),
            columns,
        },
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(push_predicates).collect(),
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs.into_iter().map(push_predicates).collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(push_predicates(*left)),
            right: Box::new(push_predicates(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(push_predicates(*anchor)),
            member: Box::new(push_predicates(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(push_predicates(*left)),
            right: Box::new(push_predicates(*right)),
            kind,
            condition,
        },
        LogicalPlan::Filter { input, predicate } => {
            let mut input = push_predicates(*input);
            let mut residual = Vec::new();
            for conjunct in split_conjunction(predicate) {
                if !push_conjunct(&mut input, &conjunct) {
                    residual.push(conjunct);
                }
            }
            match residual.into_iter().reduce(and_expr) {
                None => input,
                Some(predicate) => LogicalPlan::Filter {
                    input: Box::new(input),
                    predicate,
                },
            }
        }
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(push_predicates(*input)),
            expressions,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(push_predicates(*input)),
            group_by,
            aggregates,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(push_predicates(*input)),
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(push_predicates(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            trim,
            input: Box::new(push_predicates(*input)),
            keys,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(push_predicates(*input)),
            limit,
        },
    }
}

fn split_conjunction(expr: BoundExpr) -> Vec<BoundExpr> {
    let BoundExpr {
        kind,
        data_type,
        nullable,
    } = expr;
    match kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut values = split_conjunction(*left);
            values.extend(split_conjunction(*right));
            values
        }
        kind => vec![BoundExpr {
            kind,
            data_type,
            nullable,
        }],
    }
}

fn and_expr(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

fn push_conjunct(plan: &mut LogicalPlan, predicate: &BoundExpr) -> bool {
    let referenced_tables = referenced_tables(predicate);
    if referenced_tables.len() != 1 {
        return false;
    }
    let table = *referenced_tables
        .iter()
        .next()
        .expect("single referenced table");

    match plan {
        LogicalPlan::Scan(scan) if table_key(&scan.table) == table => {
            // Recursive working tables are virtual: their scans replay
            // in-memory deltas that never see storage predicates, so
            // filters must stay above as Filter nodes.
            if scan.table.database_id == pintail_catalog::DatabaseId::new(u64::MAX)
                && scan.table.input.is_none()
            {
                return false;
            }
            scan.predicates.push(predicate.clone());
            true
        }
        LogicalPlan::CrossJoin { inputs } => {
            let mut matching = inputs
                .iter_mut()
                .filter(|input| contains_table(input, table));
            let Some(input) = matching.next() else {
                return false;
            };
            if matching.next().is_some() {
                return false;
            }
            push_conjunct(input, predicate)
        }
        LogicalPlan::Join {
            left, right, kind, ..
        } => {
            let left_contains = contains_table(left, table);
            let right_contains = contains_table(right, table);
            match (kind, left_contains, right_contains) {
                (_, true, false) => push_conjunct(left, predicate),
                (
                    pintail_sql::BoundJoinKind::Inner | pintail_sql::BoundJoinKind::Cross,
                    false,
                    true,
                ) => push_conjunct(right, predicate),
                _ => false,
            }
        }
        _ => false,
    }
}

fn contains_table(plan: &LogicalPlan, table: TableKey) -> bool {
    match plan {
        LogicalPlan::Scan(scan) => table_key(&scan.table) == table,
        LogicalPlan::Recursive { anchor, member, .. } => {
            contains_table(anchor, table) || contains_table(member, table)
        }
        LogicalPlan::Derived { input, columns } => {
            columns
                .iter()
                .any(|column| (column.database_id, column.table_id) == table)
                || contains_table(input, table)
        }
        LogicalPlan::CrossJoin { inputs } => {
            inputs.iter().any(|input| contains_table(input, table))
        }
        LogicalPlan::UnionAll { inputs } => inputs.iter().any(|input| contains_table(input, table)),
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            contains_table(left, table) || contains_table(right, table)
        }
        LogicalPlan::Window { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => contains_table(input, table),
        LogicalPlan::Empty | LogicalPlan::OneRow => false,
    }
}

fn reorder_cross_joins(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::CrossJoin { inputs } => {
            let mut inputs = inputs
                .into_iter()
                .map(reorder_cross_joins)
                .collect::<Vec<_>>();
            inputs.sort_by_key(|input| input.estimated_rows().unwrap_or(u64::MAX));
            LogicalPlan::CrossJoin { inputs }
        }
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(reorder_cross_joins(*input)),
            columns,
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs.into_iter().map(reorder_cross_joins).collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(reorder_cross_joins(*left)),
            right: Box::new(reorder_cross_joins(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(reorder_cross_joins(*anchor)),
            member: Box::new(reorder_cross_joins(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(reorder_cross_joins(*left)),
            right: Box::new(reorder_cross_joins(*right)),
            kind,
            condition,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(reorder_cross_joins(*input)),
            predicate,
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(reorder_cross_joins(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(reorder_cross_joins(*input)),
            group_by,
            aggregates,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(reorder_cross_joins(*input)),
            expressions,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(reorder_cross_joins(*input)),
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            trim,
            input: Box::new(reorder_cross_joins(*input)),
            keys,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(reorder_cross_joins(*input)),
            limit,
        },
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
    }
}

fn prune_projections(plan: &mut LogicalPlan) {
    let mut required = BTreeSet::new();
    collect_plan_columns(plan, &mut required);
    prune_scan_columns(plan, &required);
}

fn collect_plan_columns(plan: &LogicalPlan, required: &mut BTreeSet<ColumnKey>) {
    match plan {
        LogicalPlan::Recursive { anchor, member, .. } => {
            collect_plan_columns(anchor, required);
            collect_plan_columns(member, required);
        }
        LogicalPlan::Scan(scan) => {
            for predicate in &scan.predicates {
                collect_expr_columns(predicate, required);
            }
        }
        LogicalPlan::Derived { input, .. } => collect_plan_columns(input, required),
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                collect_plan_columns(input, required);
            }
        }
        LogicalPlan::SetOp { left, right, .. } => {
            collect_plan_columns(left, required);
            collect_plan_columns(right, required);
        }
        LogicalPlan::Join {
            left,
            right,
            condition,
            ..
        } => {
            collect_plan_columns(left, required);
            collect_plan_columns(right, required);
            if let Some(condition) = condition {
                collect_expr_columns(condition, required);
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_expr_columns(predicate, required);
            collect_plan_columns(input, required);
        }
        LogicalPlan::Window { input, windows, .. } => {
            for window in windows {
                if let pintail_sql::WindowFunction::Aggregate(aggregate) = &window.function
                    && let Some(expr) = &aggregate.expr
                {
                    collect_expr_columns(expr, required);
                }
                for expr in &window.partition_by {
                    collect_expr_columns(expr, required);
                }
                for key in &window.order_by {
                    collect_expr_columns(&key.expr, required);
                }
            }
            collect_plan_columns(input, required);
        }
        LogicalPlan::Project { input, expressions } => {
            for expression in expressions {
                collect_expr_columns(&expression.expr, required);
            }
            collect_plan_columns(input, required);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            for expression in group_by {
                collect_expr_columns(expression, required);
            }
            for aggregate in aggregates {
                if let Some(expression) = &aggregate.expr {
                    collect_expr_columns(expression, required);
                }
                for (key, _) in &aggregate.order_within {
                    collect_expr_columns(key, required);
                }
            }
            collect_plan_columns(input, required);
        }
        LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => {
            collect_plan_columns(input, required);
        }
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn prune_scan_columns(plan: &mut LogicalPlan, required: &BTreeSet<ColumnKey>) {
    match plan {
        LogicalPlan::Recursive { anchor, member, .. } => {
            prune_scan_columns(anchor, required);
            prune_scan_columns(member, required);
        }
        LogicalPlan::Scan(scan) => {
            scan.projected_column_ids = scan
                .table
                .columns
                .iter()
                .filter(|column| required.contains(&column_key(column)))
                .map(|column| column.column_id)
                .collect();
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                prune_scan_columns(input, required);
            }
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            prune_scan_columns(left, required);
            prune_scan_columns(right, required);
        }
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => prune_scan_columns(input, required),
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn push_limits(plan: &mut LogicalPlan) {
    match plan {
        LogicalPlan::Limit { input, limit } => {
            let rows = limit.offset.saturating_add(limit.count);
            set_input_limit(input, rows);
            push_limits(input);
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                push_limits(input);
            }
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            push_limits(left);
            push_limits(right);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            push_limits(anchor);
            push_limits(member);
        }
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Aggregate { input, .. } => push_limits(input),
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
}

fn set_input_limit(plan: &mut LogicalPlan, rows: u64) {
    match plan {
        LogicalPlan::Scan(scan) if scan.predicates.is_empty() => {
            scan.limit = Some(scan.limit.map_or(rows, |existing| existing.min(rows)));
        }
        LogicalPlan::Project { input, .. } => set_input_limit(input, rows),
        // A row cap cannot cross the fixpoint boundary.
        LogicalPlan::Recursive { .. }
        | LogicalPlan::Scan(_)
        | LogicalPlan::Empty
        | LogicalPlan::OneRow
        | LogicalPlan::CrossJoin { .. }
        | LogicalPlan::UnionAll { .. }
        | LogicalPlan::SetOp { .. }
        | LogicalPlan::Join { .. }
        | LogicalPlan::Filter { .. }
        | LogicalPlan::Aggregate { .. }
        | LogicalPlan::Window { .. }
        | LogicalPlan::Distinct { .. }
        | LogicalPlan::Sort { .. }
        | LogicalPlan::Derived { .. }
        | LogicalPlan::Limit { .. } => {}
    }
}

fn referenced_tables(expr: &BoundExpr) -> BTreeSet<TableKey> {
    let mut columns = BTreeSet::new();
    collect_expr_columns(expr, &mut columns);
    columns
        .into_iter()
        .map(|column| (column.0, column.1))
        .collect()
}

fn collect_expr_columns(expr: &BoundExpr, columns: &mut BTreeSet<ColumnKey>) {
    match &expr.kind {
        BoundExprKind::Column(column) => {
            columns.insert(column_key(column));
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            collect_expr_columns(expr, columns);
        }
        BoundExprKind::Binary { left, right, .. } => {
            collect_expr_columns(left, columns);
            collect_expr_columns(right, columns);
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                collect_expr_columns(argument, columns);
            }
        }
        BoundExprKind::InSubquery { expr, .. } => collect_expr_columns(expr, columns),
        BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => {}
    }
}

type TableKey = (DatabaseId, TableId);
type ColumnKey = (DatabaseId, TableId, u32);

fn table_key(table: &pintail_sql::BoundTable) -> TableKey {
    (table.database_id, table.table_id)
}

fn column_key(column: &BoundColumn) -> ColumnKey {
    (column.database_id, column.table_id, column.column_id)
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, BoundExprKind, parse_statement};
    use pintail_types::{Column, DataType, TableSchema};

    use crate::{LogicalPlan, LogicalPlanner};

    use super::Optimizer;

    fn optimized(sql: &str) -> LogicalPlan {
        let events = table(1, "events", 100);
        let users = table(2, "users", 20);
        let singleton = table(3, "singleton", 1);
        let database = DatabaseEntry::new(DatabaseId::new(9), "app", [events, users, singleton])
            .expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(sql).expect("parse");
        let query = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        Optimizer::optimize(LogicalPlanner::plan(query))
    }

    fn table(id: u64, name: &str, rows: u64) -> TableEntry {
        TableEntry::new(
            TableId::new(id),
            name,
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(rows),
        )
        .expect("table")
    }

    fn project_input(plan: LogicalPlan) -> LogicalPlan {
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("project root");
        };
        *input
    }

    #[test]
    fn pushes_predicates_and_prunes_columns_without_unsafe_limit_pushdown() {
        let plan = optimized("SELECT name FROM events WHERE id > 10 LIMIT 5");
        let LogicalPlan::Limit { input, .. } = plan else {
            panic!("limit root");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("project");
        };
        let LogicalPlan::Scan(scan) = *input else {
            panic!("scan");
        };
        assert_eq!(scan.projected_column_ids, [1, 2]);
        assert_eq!(scan.predicates.len(), 1);
        assert_eq!(scan.limit, None);
    }

    #[test]
    fn propagates_limits_into_predicate_free_scans() {
        let plan = optimized("SELECT name FROM events LIMIT 5");
        let LogicalPlan::Limit { input, .. } = plan else {
            panic!("limit root");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("project");
        };
        let LogicalPlan::Scan(scan) = *input else {
            panic!("scan");
        };
        assert_eq!(scan.limit, Some(5));
    }

    #[test]
    fn prunes_unreferenced_columns() {
        let LogicalPlan::Scan(scan) = project_input(optimized("SELECT name FROM events")) else {
            panic!("scan");
        };
        assert_eq!(scan.projected_column_ids, [2]);
    }

    #[test]
    fn orders_cross_join_inputs_by_catalog_cardinality() {
        let LogicalPlan::CrossJoin { inputs } =
            project_input(optimized("SELECT events.name FROM events, users"))
        else {
            panic!("cross join");
        };
        let LogicalPlan::Scan(first) = &inputs[0] else {
            panic!("first scan");
        };
        let LogicalPlan::Scan(second) = &inputs[1] else {
            panic!("second scan");
        };
        assert_eq!(first.table.table_name, "users");
        assert_eq!(second.table.table_name, "events");
        assert!(first.projected_column_ids.is_empty());
        assert_eq!(second.projected_column_ids, [2]);
    }

    #[test]
    fn retains_multi_table_predicates_above_cross_join() {
        let input = project_input(optimized(
            "SELECT events.name FROM events, users WHERE events.id = users.id",
        ));
        let LogicalPlan::Filter { input, .. } = input else {
            panic!("residual filter");
        };
        assert!(matches!(*input, LogicalPlan::CrossJoin { .. }));
    }

    #[test]
    fn keeps_right_side_where_predicates_above_left_joins() {
        let input = project_input(optimized(
            "SELECT events.name FROM events \
             LEFT JOIN users ON events.id = users.id \
             WHERE users.name = 'selected'",
        ));
        let LogicalPlan::Filter { input, .. } = input else {
            panic!("right-side WHERE must remain above left join");
        };
        assert!(matches!(
            *input,
            LogicalPlan::Join {
                kind: pintail_sql::BoundJoinKind::Left,
                ..
            }
        ));
    }

    #[test]
    fn folds_constant_filters_to_empty_or_removes_them() {
        assert_eq!(
            project_input(optimized("SELECT 1 WHERE 2 + 2 = 5")),
            LogicalPlan::Empty
        );
        assert_eq!(
            project_input(optimized("SELECT 1 WHERE 2 + 2 = 4")),
            LogicalPlan::OneRow
        );
    }

    #[test]
    fn does_not_push_limit_through_distinct() {
        let plan = optimized("SELECT DISTINCT name FROM events LIMIT 3");
        let LogicalPlan::Limit { input, .. } = plan else {
            panic!("limit");
        };
        let LogicalPlan::Distinct { input } = *input else {
            panic!("distinct");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("project");
        };
        let LogicalPlan::Scan(scan) = *input else {
            panic!("scan");
        };
        assert_eq!(scan.limit, None);
    }

    #[test]
    fn constant_folding_preserves_null_semantics() {
        let plan = optimized(
            "SELECT NULL IS NULL AS yes, NULL + 1 AS absent, \
             TRUE OR NULL AS still_true, FALSE AND NULL AS still_false, \
             'A' = 'a' AS collated_equal",
        );
        let LogicalPlan::Project { expressions, .. } = plan else {
            panic!("project");
        };
        assert!(matches!(
            expressions[0].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::Boolean(true))
        ));
        assert!(matches!(
            expressions[1].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::Null)
        ));
        assert_eq!(
            expressions[2].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::Boolean(true))
        );
        assert_eq!(
            expressions[3].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::Boolean(false))
        );
        assert_eq!(
            expressions[4].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::Boolean(true))
        );
    }

    #[test]
    fn answers_predicate_free_count_star_from_exact_catalog_metadata() {
        let plan = optimized("SELECT COUNT(*) AS rows FROM events");
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("client projection");
        };
        let LogicalPlan::Project { input, expressions } = *input else {
            panic!("metadata projection");
        };
        assert_eq!(
            expressions[0].expr.kind,
            BoundExprKind::Literal(pintail_types::Value::UInt64(100))
        );
        assert_eq!(*input, LogicalPlan::OneRow);

        let plan = project_input(optimized("SELECT COUNT(*) FROM events WHERE id > 0"));
        assert!(matches!(plan, LogicalPlan::Aggregate { .. }));
    }

    #[test]
    fn pushes_aggregates_through_unreferenced_identity_cross_joins_only() {
        let plan = project_input(optimized("SELECT COUNT(events.id) FROM events, singleton"));
        let LogicalPlan::Aggregate { input, .. } = plan else {
            panic!("aggregate");
        };
        assert!(matches!(*input, LogicalPlan::Scan(_)));

        let plan = project_input(optimized(
            "SELECT singleton.id, COUNT(events.id) \
             FROM events, singleton GROUP BY singleton.id",
        ));
        let LogicalPlan::Aggregate { input, .. } = plan else {
            panic!("aggregate");
        };
        assert!(matches!(*input, LogicalPlan::CrossJoin { .. }));
    }
}
