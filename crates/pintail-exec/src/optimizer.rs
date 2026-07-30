use std::collections::BTreeSet;

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundProjection, UnaryOp};
use pintail_types::{DataType, Float64, Value};

use crate::LogicalPlan;

/// Deterministic rule-based logical optimizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Optimizer;

impl Optimizer {
    /// Applies semantics-preserving v1 logical rewrites.
    #[must_use]
    pub fn optimize(plan: LogicalPlan) -> LogicalPlan {
        let plan = fold_constants(plan);
        let plan = push_predicates(plan);
        let plan = reorder_cross_joins(plan);
        let mut plan = plan;
        prune_projections(&mut plan);
        push_limits(&mut plan);
        plan
    }
}

fn fold_constants(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(fold_constants).collect(),
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
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(fold_constants(*input)),
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(fold_constants(*input)),
            limit,
        },
    }
}

fn fold_expr(expr: BoundExpr) -> BoundExpr {
    let folded = match expr.kind {
        BoundExprKind::Column(_) | BoundExprKind::Literal(_) => return expr,
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
    };

    evaluate_constant(&folded).map_or(folded, literal_expr)
}

fn evaluate_constant(expr: &BoundExpr) -> Option<Value> {
    match &expr.kind {
        BoundExprKind::Column(_) => None,
        BoundExprKind::Literal(value) => Some(value.clone()),
        BoundExprKind::Unary { op, expr } => {
            let value = evaluate_constant(expr)?;
            evaluate_unary(*op, value)
        }
        BoundExprKind::Binary { op, left, right } => {
            let left = evaluate_constant(left)?;
            let right = evaluate_constant(right)?;
            evaluate_binary(*op, left, right, expr.data_type)
        }
        BoundExprKind::IsNull { expr, negated } => {
            let is_null = matches!(evaluate_constant(expr)?, Value::Null);
            Some(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
    }
}

fn evaluate_unary(op: UnaryOp, value: Value) -> Option<Value> {
    if matches!(value, Value::Null) {
        return Some(Value::Null);
    }
    match (op, value) {
        (UnaryOp::Plus, value @ (Value::Int64(_) | Value::UInt64(_) | Value::Float64(_))) => {
            Some(value)
        }
        (UnaryOp::Minus, Value::Int64(value)) => value.checked_neg().map(Value::Int64),
        (UnaryOp::Minus, Value::Float64(value)) => Some(Value::float64(-value.get())),
        (UnaryOp::Not, value) => scalar_truth(&value).map(|value| Value::Boolean(!value)),
        _ => None,
    }
}

fn evaluate_binary(
    op: BinaryOp,
    left: Value,
    right: Value,
    result_type: Option<DataType>,
) -> Option<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Some(Value::Null);
    }

    match op {
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
            let left = scalar_truth(&left)?;
            let right = scalar_truth(&right)?;
            let value = match op {
                BinaryOp::And => left && right,
                BinaryOp::Or => left || right,
                BinaryOp::Xor => left ^ right,
                _ => unreachable!("matched logical operation"),
            };
            Some(Value::Boolean(value))
        }
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessOrEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterOrEqual => evaluate_comparison(op, &left, &right).map(Value::Boolean),
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::IntegerDivide
        | BinaryOp::Modulo => evaluate_arithmetic(op, left, right, result_type),
    }
}

fn evaluate_comparison(op: BinaryOp, left: &Value, right: &Value) -> Option<bool> {
    if left.data_type() != right.data_type() {
        return None;
    }
    Some(match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        BinaryOp::Less => left < right,
        BinaryOp::LessOrEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterOrEqual => left >= right,
        _ => return None,
    })
}

fn evaluate_arithmetic(
    op: BinaryOp,
    left: Value,
    right: Value,
    result_type: Option<DataType>,
) -> Option<Value> {
    match result_type {
        Some(DataType::Float64) => {
            let left = numeric_f64(&left)?;
            let right = numeric_f64(&right)?;
            let value = match op {
                BinaryOp::Add => left + right,
                BinaryOp::Subtract => left - right,
                BinaryOp::Multiply => left * right,
                BinaryOp::Divide if right != 0.0 => left / right,
                BinaryOp::IntegerDivide if right != 0.0 => (left / right).trunc(),
                BinaryOp::Modulo if right != 0.0 => left % right,
                _ => return None,
            };
            value
                .is_finite()
                .then(|| Value::Float64(Float64::new(value)))
        }
        Some(DataType::UInt64) => {
            let (Value::UInt64(left), Value::UInt64(right)) = (left, right) else {
                return None;
            };
            match op {
                BinaryOp::Add => left.checked_add(right).map(Value::UInt64),
                BinaryOp::Subtract => left.checked_sub(right).map(Value::UInt64),
                BinaryOp::Multiply => left.checked_mul(right).map(Value::UInt64),
                BinaryOp::IntegerDivide if right != 0 => Some(Value::UInt64(left / right)),
                BinaryOp::Modulo if right != 0 => Some(Value::UInt64(left % right)),
                _ => None,
            }
        }
        Some(DataType::Int64) => {
            let left = numeric_i64(&left)?;
            let right = numeric_i64(&right)?;
            match op {
                BinaryOp::Add => left.checked_add(right).map(Value::Int64),
                BinaryOp::Subtract => left.checked_sub(right).map(Value::Int64),
                BinaryOp::Multiply => left.checked_mul(right).map(Value::Int64),
                BinaryOp::IntegerDivide if right != 0 => left.checked_div(right).map(Value::Int64),
                BinaryOp::Modulo if right != 0 => left.checked_rem(right).map(Value::Int64),
                _ => None,
            }
        }
        None | Some(DataType::Boolean | DataType::Utf8 | DataType::Binary) => None,
    }
}

fn numeric_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Float64(value) => Some(value.get()),
        Value::Null
        | Value::Boolean(_)
        | Value::Int64(_)
        | Value::UInt64(_)
        | Value::Utf8(_)
        | Value::Binary(_) => None,
    }
}

fn numeric_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Boolean(value) => Some(i64::from(*value)),
        Value::Int64(value) => Some(*value),
        Value::UInt64(value) => i64::try_from(*value).ok(),
        _ => None,
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
    scalar_truth(value)
}

fn scalar_truth(value: &Value) -> Option<bool> {
    match value {
        Value::Null => Some(false),
        Value::Boolean(value) => Some(*value),
        Value::Int64(value) => Some(*value != 0),
        Value::UInt64(value) => Some(*value != 0),
        Value::Float64(value) => Some(value.get() != 0.0),
        Value::Utf8(_) | Value::Binary(_) => None,
    }
}

fn push_predicates(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(push_predicates).collect(),
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
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(push_predicates(*input)),
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
        _ => false,
    }
}

fn contains_table(plan: &LogicalPlan, table: TableKey) -> bool {
    match plan {
        LogicalPlan::Scan(scan) => table_key(&scan.table) == table,
        LogicalPlan::CrossJoin { inputs } => {
            inputs.iter().any(|input| contains_table(input, table))
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
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
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(reorder_cross_joins(*input)),
            predicate,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(reorder_cross_joins(*input)),
            expressions,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(reorder_cross_joins(*input)),
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
        LogicalPlan::Scan(scan) => {
            for predicate in &scan.predicates {
                collect_expr_columns(predicate, required);
            }
        }
        LogicalPlan::CrossJoin { inputs } => {
            for input in inputs {
                collect_plan_columns(input, required);
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_expr_columns(predicate, required);
            collect_plan_columns(input, required);
        }
        LogicalPlan::Project { input, expressions } => {
            for expression in expressions {
                collect_expr_columns(&expression.expr, required);
            }
            collect_plan_columns(input, required);
        }
        LogicalPlan::Distinct { input } | LogicalPlan::Limit { input, .. } => {
            collect_plan_columns(input, required);
        }
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn prune_scan_columns(plan: &mut LogicalPlan, required: &BTreeSet<ColumnKey>) {
    match plan {
        LogicalPlan::Scan(scan) => {
            scan.projected_column_ids = scan
                .table
                .columns
                .iter()
                .filter(|column| required.contains(&column_key(column)))
                .map(|column| column.column_id)
                .collect();
        }
        LogicalPlan::CrossJoin { inputs } => {
            for input in inputs {
                prune_scan_columns(input, required);
            }
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
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
        LogicalPlan::CrossJoin { inputs } => {
            for input in inputs {
                push_limits(input);
            }
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input } => push_limits(input),
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
}

fn set_input_limit(plan: &mut LogicalPlan, rows: u64) {
    match plan {
        LogicalPlan::Scan(scan) if scan.predicates.is_empty() => {
            scan.limit = Some(scan.limit.map_or(rows, |existing| existing.min(rows)));
        }
        LogicalPlan::Project { input, .. } => set_input_limit(input, rows),
        LogicalPlan::Scan(_)
        | LogicalPlan::Empty
        | LogicalPlan::OneRow
        | LogicalPlan::CrossJoin { .. }
        | LogicalPlan::Filter { .. }
        | LogicalPlan::Distinct { .. }
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
        BoundExprKind::Literal(_) => {}
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
        let database =
            DatabaseEntry::new(DatabaseId::new(9), "app", [events, users]).expect("database");
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
        let plan = optimized("SELECT NULL IS NULL AS yes, NULL + 1 AS absent");
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
    }
}
