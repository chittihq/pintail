//! Folding a LEFT-joined branch keyed by grouping columns before the join.
//!
//! `SELECT g.id, AVG(e.x), COUNT(DISTINCT o.y), SUM(o.z) FROM g
//! LEFT JOIN e ON e.g_id = g.id LEFT JOIN o ON ... GROUP BY g.id` pairs
//! every row of `e` with every row the other joins produce for the same
//! group: the group's rows are the product of the two branches, and a join
//! chain with several such branches multiplies their sizes together. When
//! the branch is joined only on columns the query groups by, every row of a
//! group meets the same set of branch rows, so the product is exactly the
//! rest of the group repeated once per branch row (once when none match).
//! That makes the branch foldable by its join key first:
//!
//! ```text
//! SELECT g.id, MIN(e.a), COUNT(DISTINCT o.y), SUM(o.z) * COALESCE(MAX(e.n), 1)
//! FROM g LEFT JOIN (SELECT g_id, AVG(x) a, SUM(1) n FROM e GROUP BY g_id) e
//!   ON e.g_id = g.id LEFT JOIN o ON ... GROUP BY g.id
//! ```
//!
//! Each group meets one folded row, the same one on every row:
//!
//! - an aggregate of the branch alone sees its rows each repeated as often
//!   as the rest of the group has rows, which AVG, MIN and MAX of exact
//!   values ignore, so the per-key answer is the group's answer;
//! - an aggregate of the rest sees each row repeated once per branch row:
//!   DISTINCT aggregates, MIN, MAX, `ANY_VALUE`, bitwise AND/OR and AVG of
//!   exact values ignore that, while COUNT and exact SUM scale by the
//!   branch's row count, restored by multiplying the folded count back.
//!
//! The grouping columns the branch joins on must compare by value alone,
//! so every row of a group carries one key; an aggregate reading both
//! branches at once, or any other read of the branch, leaves the query as
//! written.

use pintail_types::{DataType, Value};

use super::pre_aggregate::{
    by_value, columns_of, conjuncts, owns, reads_only_slots, rebind, same_relation, table_rows,
};
use super::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundFrom,
    BoundJoinKind, BoundProjection, BoundQuery, BoundTable, ScalarFunction, and_bound,
    arithmetic_type,
};

/// A branch smaller than this repeats too few rows to repay its fold.
const MINIMUM_ROWS: u64 = 10_000;

/// Rewrites `query` to fold the first LEFT-joined branch that qualifies.
pub(super) fn apply(
    query: BoundQuery,
    derive: impl FnOnce(String, String, BoundQuery) -> BoundTable,
) -> BoundQuery {
    if query.group_by.is_empty()
        || query.aggregates.is_empty()
        || !query.windows.is_empty()
        || query.distinct
        || query.recursive.is_some()
        || !query.union_all.is_empty()
        || !query.set_ops.is_empty()
        || query.from.len() != 1
        || query.from[0].joins.iter().any(|join| join.scalar_aggregate)
        || !query
            .projection
            .iter()
            .all(|item| reads_only_slots(&item.expr))
        || !query.having.iter().all(reads_only_slots)
    {
        return query;
    }
    let Some(plan) = (0..query.from[0].joins.len()).find_map(|index| candidate(&query, index))
    else {
        return query;
    };
    let mut query = query;
    rewrite(&mut query, &plan, derive);
    query
}

/// How an aggregate outside the branch behaves when every row of its group
/// is repeated the same number of times.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeated {
    /// The answer is unchanged.
    Ignored,
    /// The answer is multiplied by the repeat count.
    Scaled,
}

struct Plan {
    join: usize,
    /// Branch columns the join equates with grouping columns, in the
    /// derived table's key order.
    keys: Vec<BoundColumn>,
    /// Join conjuncts that read the branch alone; they filter it.
    filters: Vec<BoundExpr>,
    /// Indexes into `query.aggregates` of the branch's own aggregates.
    branch: Vec<usize>,
    /// Indexes of aggregates of the rest whose answer scales.
    scaled: Vec<usize>,
}

fn exact(data_type: Option<DataType>) -> bool {
    matches!(
        data_type.map(DataType::storage_type),
        Some(
            DataType::Boolean
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Decimal { .. }
        )
    )
}

fn same_column(left: &BoundColumn, right: &BoundColumn) -> bool {
    !left.outer
        && !right.outer
        && left.database_id == right.database_id
        && left.table_id == right.table_id
        && left.column_id == right.column_id
        && left
            .relation_name
            .eq_ignore_ascii_case(&right.relation_name)
}

#[allow(clippy::too_many_lines)] // one linear sequence of shape checks
fn candidate(query: &BoundQuery, index: usize) -> Option<Plan> {
    let source = &query.from[0];
    let join = &source.joins[index];
    let branch = &join.table;
    if join.kind != BoundJoinKind::Left
        || branch.input.is_some()
        || table_rows(branch) < MINIMUM_ROWS
    {
        return None;
    }
    let grouped = query
        .group_by
        .iter()
        .filter_map(|expr| match &expr.kind {
            BoundExprKind::Column(column) => Some(column),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut keys = Vec::<BoundColumn>::new();
    let mut filters = Vec::new();
    let mut parts = Vec::new();
    conjuncts(join.condition.clone()?, &mut parts);
    for part in parts {
        let mut columns = Vec::new();
        if !columns_of(&part, &mut columns) {
            return None;
        }
        if columns.iter().all(|column| owns(branch, column)) {
            filters.push(part);
            continue;
        }
        let BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } = &part.kind
        else {
            return None;
        };
        let (BoundExprKind::Column(left), BoundExprKind::Column(right)) = (&left.kind, &right.kind)
        else {
            return None;
        };
        let (inner, outer) = if owns(branch, left) {
            (left, right)
        } else {
            (right, left)
        };
        if !owns(branch, inner)
            || owns(branch, outer)
            || !by_value(Some(inner.data_type))
            || !by_value(Some(outer.data_type))
            || !grouped.iter().any(|column| same_column(column, outer))
        {
            return None;
        }
        if !keys.iter().any(|key| key.column_id == inner.column_id) {
            keys.push(inner.clone());
        }
    }
    // Keyed by the whole primary key, the branch never repeats a row.
    if keys.is_empty()
        || (!branch.key_column_ids.is_empty()
            && branch
                .key_column_ids
                .iter()
                .all(|id| keys.iter().any(|key| key.column_id == *id)))
    {
        return None;
    }
    // Nothing but the branch's own aggregates may read it.
    let reads_branch = |expr: &BoundExpr| {
        let mut columns = Vec::new();
        !columns_of(expr, &mut columns) || columns.iter().any(|column| owns(branch, column))
    };
    if query.filter.iter().any(reads_branch)
        || query.group_by.iter().any(reads_branch)
        || source
            .joins
            .iter()
            .enumerate()
            .any(|(other, join)| other != index && join.condition.iter().any(reads_branch))
        || query
            .tables
            .iter()
            .filter(|table| same_relation(table, branch))
            .count()
            != 1
    {
        return None;
    }
    let mut branch_aggregates = Vec::new();
    let mut scaled = Vec::new();
    for (position, aggregate) in query.aggregates.iter().enumerate() {
        if !aggregate.order_within.is_empty() || aggregate.separator.is_some() {
            return None;
        }
        let input = aggregate.expr.as_ref().and_then(|expr| expr.data_type);
        let mut columns = Vec::new();
        if let Some(expr) = &aggregate.expr
            && !columns_of(expr, &mut columns)
        {
            return None;
        }
        let in_branch = columns.iter().filter(|column| owns(branch, column)).count();
        if in_branch > 0 {
            if in_branch != columns.len() {
                return None;
            }
            match aggregate.function {
                AggregateFunction::Average if exact(input) => {}
                AggregateFunction::Minimum | AggregateFunction::Maximum
                    if exact(input) || by_value(input) => {}
                _ => return None,
            }
            branch_aggregates.push(position);
            continue;
        }
        let repeated = match aggregate.function {
            _ if aggregate.distinct
                && !matches!(
                    aggregate.function,
                    AggregateFunction::GroupConcat
                        | AggregateFunction::JsonArrayAgg
                        | AggregateFunction::JsonObjectAgg
                ) =>
            {
                Repeated::Ignored
            }
            AggregateFunction::Minimum
            | AggregateFunction::Maximum
            | AggregateFunction::AnyValue
            | AggregateFunction::BitAnd
            | AggregateFunction::BitOr => Repeated::Ignored,
            AggregateFunction::Average if exact(input) => Repeated::Ignored,
            AggregateFunction::Count => Repeated::Scaled,
            AggregateFunction::Sum if exact(input) && exact(aggregate.data_type) => {
                Repeated::Scaled
            }
            _ => return None,
        };
        if repeated == Repeated::Scaled {
            scaled.push(position);
        }
    }
    Some(Plan {
        join: index,
        keys,
        filters,
        branch: branch_aggregates,
        scaled,
    })
}

fn slot(index: usize, data_type: Option<DataType>, nullable: bool) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Aggregate(index),
        data_type,
        nullable,
    }
}

fn signed_one() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::Int64(1)),
        data_type: Some(DataType::Int64),
        nullable: false,
    }
}

fn column_expr(column: &BoundColumn) -> BoundExpr {
    BoundExpr {
        data_type: Some(column.data_type),
        nullable: column.nullable,
        kind: BoundExprKind::Column(column.clone()),
    }
}

/// Replaces every read of each scaled slot with that slot times `factor`.
fn scale_slots(expr: &mut BoundExpr, scaled: &[(usize, BoundExpr)]) {
    if let BoundExprKind::Aggregate(index) = &expr.kind {
        if let Some((_, replacement)) = scaled.iter().find(|(slot, _)| slot == index) {
            *expr = replacement.clone();
        }
        return;
    }
    match &mut expr.kind {
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            scale_slots(expr, scaled);
        }
        BoundExprKind::Binary { left, right, .. } => {
            scale_slots(left, scaled);
            scale_slots(right, scaled);
        }
        BoundExprKind::Scalar { args, .. } => {
            for arg in args {
                scale_slots(arg, scaled);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_lines)] // one linear rebuild pass
fn rewrite(
    query: &mut BoundQuery,
    plan: &Plan,
    derive: impl FnOnce(String, String, BoundQuery) -> BoundTable,
) {
    let groups = query.group_by.len();
    let branch = query.from[0].joins[plan.join].table.clone();
    let key_count = plan.keys.len();
    let mut aggregates = plan
        .branch
        .iter()
        .map(|&position| BoundAggregate {
            output_column: None,
            declared: true,
            ..query.aggregates[position].clone()
        })
        .collect::<Vec<_>>();
    if !plan.scaled.is_empty() {
        // Counted as a signed SUM(1), so scaling a signed answer keeps it
        // signed.
        aggregates.push(BoundAggregate {
            output_column: None,
            declared: true,
            function: AggregateFunction::Sum,
            expr: Some(signed_one()),
            distinct: false,
            data_type: Some(DataType::Int64),
            nullable: true,
            separator: None,
            order_within: Vec::new(),
        });
    }
    let mut projection = plan
        .keys
        .iter()
        .enumerate()
        .map(|(index, column)| BoundProjection {
            name: column.name.clone(),
            expr: BoundExpr {
                kind: BoundExprKind::GroupKey(index),
                data_type: Some(column.data_type),
                nullable: column.nullable,
            },
        })
        .collect::<Vec<_>>();
    for (index, aggregate) in aggregates.iter().enumerate() {
        projection.push(BoundProjection {
            name: format!("<branch-{index}>"),
            expr: slot(key_count + index, aggregate.data_type, aggregate.nullable),
        });
    }
    let folded = BoundQuery {
        text_collation: query.text_collation,
        from: vec![BoundFrom {
            base: branch.clone(),
            joins: Vec::new(),
        }],
        tables: vec![branch.clone()],
        projection,
        filter: plan.filters.iter().cloned().reduce(and_bound),
        group_by: plan.keys.iter().map(column_expr).collect(),
        aggregates,
        windows: Vec::new(),
        having: None,
        distinct: false,
        order_by: Vec::new(),
        hidden_sort_columns: 0,
        union_all: Vec::new(),
        union_distinct: false,
        set_ops: Vec::new(),
        limit: None,
        recursive: None,
        outer_set: None,
    };
    let mut derived = derive(
        branch.table_name.clone(),
        branch.relation_name.clone(),
        folded,
    );
    derived.estimated_rows = Some(table_rows(&branch));

    // The join keeps its key equalities, now against the folded keys; the
    // branch-only conjuncts moved into the fold.
    let join = &mut query.from[0].joins[plan.join];
    let mut parts = Vec::new();
    if let Some(condition) = join.condition.take() {
        conjuncts(condition, &mut parts);
    }
    let mut kept = parts
        .into_iter()
        .filter(|part| {
            let mut columns = Vec::new();
            columns_of(part, &mut columns);
            !columns.iter().all(|column| owns(&branch, column))
        })
        .collect::<Vec<_>>();
    for part in &mut kept {
        rebind(part, &branch, &plan.keys, &derived);
    }
    join.condition = kept.into_iter().reduce(and_bound);
    join.table = derived.clone();
    for table in &mut query.tables {
        if same_relation(table, &branch) {
            *table = derived.clone();
        }
    }

    // A branch aggregate is the same on every row of its group.
    for (index, &position) in plan.branch.iter().enumerate() {
        let column = derived.columns[key_count + index].clone();
        let aggregate = &mut query.aggregates[position];
        aggregate.function = AggregateFunction::Minimum;
        aggregate.distinct = false;
        aggregate.expr = Some(column_expr(&column));
    }
    if plan.scaled.is_empty() {
        return;
    }
    let count = derived.columns[key_count + plan.branch.len()].clone();
    let count_slot = groups + query.aggregates.len();
    query.aggregates.push(BoundAggregate {
        output_column: None,
        declared: false,
        function: AggregateFunction::Maximum,
        expr: Some(column_expr(&count)),
        distinct: false,
        data_type: Some(DataType::Int64),
        nullable: true,
        separator: None,
        order_within: Vec::new(),
    });
    // A group no branch row matched was repeated once.
    let factor = BoundExpr {
        kind: BoundExprKind::Scalar {
            function: ScalarFunction::Coalesce,
            args: vec![slot(count_slot, Some(DataType::Int64), true), signed_one()],
        },
        data_type: Some(DataType::Int64),
        nullable: false,
    };
    let scaled = plan
        .scaled
        .iter()
        .map(|&position| {
            let aggregate = &query.aggregates[position];
            let original = slot(groups + position, aggregate.data_type, aggregate.nullable);
            let replacement = BoundExpr {
                data_type: arithmetic_type(
                    BinaryOp::Multiply,
                    aggregate.data_type,
                    Some(DataType::Int64),
                ),
                nullable: aggregate.nullable,
                kind: BoundExprKind::Binary {
                    op: BinaryOp::Multiply,
                    left: Box::new(original),
                    right: Box::new(factor.clone()),
                },
            };
            (groups + position, replacement)
        })
        .collect::<Vec<_>>();
    for item in &mut query.projection {
        scale_slots(&mut item.expr, &scaled);
    }
    if let Some(having) = &mut query.having {
        scale_slots(having, &scaled);
    }
}
