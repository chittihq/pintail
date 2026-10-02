//! Aggregation below an inner join.
//!
//! `SELECT d.region, SUM(f.amount) FROM facts f JOIN dims d ON f.dim = d.id
//! GROUP BY d.region` joins every fact row and then folds it away. When every
//! aggregate reads one relation, that relation can be folded first, grouped
//! by the columns the rest of the query reads from it, and the join then
//! meets one row per group instead of one per fact:
//!
//! ```text
//! SELECT d.region, SUM(f.s) FROM
//!   (SELECT dim, SUM(amount) s FROM facts GROUP BY dim) f
//!   JOIN dims d ON f.dim = d.id GROUP BY d.region
//! ```
//!
//! Every row of a partial group carries the same values in the columns the
//! join and grouping read, so each predicate and key sees exactly what it saw
//! row by row, and a partial group meets each joined row once for each row
//! it stands for. That makes the rewrite exact for SUM, COUNT, MIN and MAX,
//! re-folded as SUM, SUM, MIN and MAX, under three conditions checked here:
//!
//! - every join in the query is inner, so no row is null-extended or
//!   dropped by a match count;
//! - the grouped columns compare by value alone - integers and temporal
//!   values, never text, whose collations can equate distinct spellings and
//!   make the group's representative a choice;
//! - the aggregates are not DISTINCT and fold exact types, since a float sum
//!   regrouped rounds differently.
//!
//! Only a GROUP BY query is rewritten: an ungrouped COUNT over no rows is 0,
//! where the re-folded SUM would be NULL.

use pintail_types::DataType;

use super::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundFrom,
    BoundJoinKind, BoundProjection, BoundQuery, BoundTable, and_bound,
};

/// A relation smaller than this gains too little to pay for its own fold.
const MINIMUM_ROWS: u64 = 50_000;

/// Rewrites `query` to fold its aggregated relation below the joins, when
/// the shape allows it and the relation is large enough to repay the fold.
pub(super) fn apply(
    query: BoundQuery,
    derive: impl FnOnce(String, String, BoundQuery) -> BoundTable,
) -> BoundQuery {
    let Some(target) = target(&query) else {
        return query;
    };
    let mut query = query;
    let _ = rewrite(&mut query, &target, derive);
    query
}

/// The relation whose rows every aggregate reads, when the query's shape
/// admits folding it first.
fn target(query: &BoundQuery) -> Option<BoundTable> {
    if query.group_by.is_empty()
        || query.aggregates.is_empty()
        || !query.windows.is_empty()
        || query.distinct
        || query.recursive.is_some()
        || !query.union_all.is_empty()
        || !query.set_ops.is_empty()
    {
        return None;
    }
    let [source] = query.from.as_slice() else {
        return None;
    };
    if source.joins.is_empty()
        || source
            .joins
            .iter()
            .any(|join| join.kind != BoundJoinKind::Inner || join.scalar_aggregate)
    {
        return None;
    }
    let relations = std::iter::once(&source.base)
        .chain(source.joins.iter().map(|join| &join.table))
        .collect::<Vec<_>>();
    // One relation supplies every aggregate's input; COUNT(*) reads none.
    let mut read = None::<&BoundTable>;
    for aggregate in &query.aggregates {
        if !foldable(aggregate) {
            return None;
        }
        let Some(expr) = &aggregate.expr else {
            continue;
        };
        let mut columns = Vec::new();
        if !columns_of(expr, &mut columns) {
            return None;
        }
        for column in columns {
            let owner = relations.iter().find(|table| owns(table, &column))?;
            if read.is_some_and(|table| !same_relation(table, owner)) {
                return None;
            }
            read = Some(owner);
        }
    }
    let target = read?;
    let rows = target.row_count.or(target.estimated_rows)?;
    if target.input.is_some()
        || rows < MINIMUM_ROWS
        || relations
            .iter()
            .any(|table| !same_relation(table, target) && table_rows(table) > rows)
    {
        return None;
    }
    Some(target.clone())
}

pub(super) fn table_rows(table: &BoundTable) -> u64 {
    table.row_count.or(table.estimated_rows).unwrap_or(0)
}

/// SUM, COUNT, MIN and MAX re-fold exactly; each of the others either
/// needs every row (DISTINCT, `GROUP_CONCAT`) or is not a fold of folds.
fn foldable(aggregate: &BoundAggregate) -> bool {
    if aggregate.distinct || !aggregate.order_within.is_empty() || aggregate.separator.is_some() {
        return false;
    }
    let input = aggregate.expr.as_ref().and_then(|expr| expr.data_type);
    match aggregate.function {
        AggregateFunction::Count => true,
        AggregateFunction::Sum => {
            matches!(
                input.map(DataType::storage_type),
                Some(DataType::Int64 | DataType::UInt64 | DataType::Decimal { .. })
            ) && matches!(
                aggregate.data_type,
                Some(DataType::Int64 | DataType::UInt64 | DataType::Decimal { .. })
            )
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum => by_value(input),
        _ => false,
    }
}

/// Types whose equal values are the same value: grouping by them never
/// merges spellings a comparison could still tell apart.
pub(super) fn by_value(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
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
                | DataType::Date32
                | DataType::DateTime64 { .. }
        )
    )
}

pub(super) fn owns(table: &BoundTable, column: &BoundColumn) -> bool {
    !column.outer
        && table.database_id == column.database_id
        && table.table_id == column.table_id
        && table
            .relation_name
            .eq_ignore_ascii_case(&column.relation_name)
}

pub(super) fn same_relation(left: &BoundTable, right: &BoundTable) -> bool {
    left.database_id == right.database_id
        && left.table_id == right.table_id
        && left
            .relation_name
            .eq_ignore_ascii_case(&right.relation_name)
}

/// Collects the columns `expr` reads. False when it holds anything this
/// rewrite does not look inside - a subquery or a prepared set.
pub(super) fn columns_of(expr: &BoundExpr, out: &mut Vec<BoundColumn>) -> bool {
    match &expr.kind {
        BoundExprKind::Column(column) => {
            out.push(column.clone());
            true
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            columns_of(expr, out)
        }
        BoundExprKind::Binary { left, right, .. } => {
            columns_of(left, out) && columns_of(right, out)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(|arg| columns_of(arg, out)),
        _ => false,
    }
}

/// Whether `expr` reads nothing but grouping slots, aggregate slots and
/// constants - what a projection or HAVING may hold above the fold.
pub(super) fn reads_only_slots(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::GroupKey(_) | BoundExprKind::Aggregate(_) | BoundExprKind::Literal(_) => {
            true
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            reads_only_slots(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            reads_only_slots(left) && reads_only_slots(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(reads_only_slots),
        _ => false,
    }
}

pub(super) fn conjuncts(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr.kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            conjuncts(*left, out);
            conjuncts(*right, out);
        }
        kind => out.push(BoundExpr { kind, ..expr }),
    }
}

/// Replaces every read of a folded column with the derived table's column.
pub(super) fn rebind(
    expr: &mut BoundExpr,
    target: &BoundTable,
    grouped: &[BoundColumn],
    derived: &BoundTable,
) {
    match &mut expr.kind {
        BoundExprKind::Column(column) if owns(target, column) => {
            if let Some(index) = grouped
                .iter()
                .position(|kept| kept.column_id == column.column_id)
            {
                *column = derived.columns[index].clone();
            }
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            rebind(expr, target, grouped, derived);
        }
        BoundExprKind::Binary { left, right, .. } => {
            rebind(left, target, grouped, derived);
            rebind(right, target, grouped, derived);
        }
        BoundExprKind::Scalar { args, .. } => {
            for arg in args {
                rebind(arg, target, grouped, derived);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_lines)] // one linear validate-then-rebuild pass
fn rewrite(
    query: &mut BoundQuery,
    target: &BoundTable,
    derive: impl FnOnce(String, String, BoundQuery) -> BoundTable,
) -> Option<()> {
    // Above the fold only grouping and aggregate slots are readable.
    if !query
        .projection
        .iter()
        .all(|item| reads_only_slots(&item.expr))
        || !query.having.iter().all(reads_only_slots)
    {
        return None;
    }
    // WHERE conjuncts over the folded relation alone move into the fold;
    // one that also reads another relation would need its rows.
    let mut kept = Vec::new();
    let mut pushed = Vec::new();
    if let Some(filter) = query.filter.clone() {
        let mut parts = Vec::new();
        conjuncts(filter, &mut parts);
        for part in parts {
            let mut columns = Vec::new();
            if !columns_of(&part, &mut columns) {
                return None;
            }
            let folded = columns.iter().filter(|column| owns(target, column)).count();
            if folded == 0 {
                kept.push(part);
            } else if folded == columns.len() {
                pushed.push(part);
            } else {
                return None;
            }
        }
    }
    // The columns the rest of the query reads from the folded relation are
    // what the fold groups by.
    let mut grouped = Vec::<BoundColumn>::new();
    let mut outside = kept.iter().collect::<Vec<_>>();
    outside.extend(&query.group_by);
    let conditions = query.from[0]
        .joins
        .iter()
        .filter_map(|join| join.condition.as_ref())
        .collect::<Vec<_>>();
    outside.extend(conditions);
    for expr in outside {
        let mut columns = Vec::new();
        if !columns_of(expr, &mut columns) {
            return None;
        }
        for column in columns.into_iter().filter(|column| owns(target, column)) {
            if !grouped
                .iter()
                .any(|kept| kept.column_id == column.column_id)
            {
                grouped.push(column);
            }
        }
    }
    if grouped.is_empty()
        || !grouped
            .iter()
            .all(|column| by_value(Some(column.data_type)))
        // Grouping by the whole key leaves one row per group: no fold.
        || (!target.key_column_ids.is_empty()
            && target
                .key_column_ids
                .iter()
                .all(|id| grouped.iter().any(|column| column.column_id == *id)))
    {
        return None;
    }

    let group_count = grouped.len();
    let mut projection = grouped
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
    for (index, aggregate) in query.aggregates.iter().enumerate() {
        projection.push(BoundProjection {
            name: format!("<partial-{index}>"),
            expr: BoundExpr {
                kind: BoundExprKind::Aggregate(group_count + index),
                data_type: aggregate.data_type,
                nullable: aggregate.nullable,
            },
        });
    }
    let partial = BoundQuery {
        text_collation: query.text_collation,
        from: vec![BoundFrom {
            base: target.clone(),
            joins: Vec::new(),
        }],
        tables: vec![target.clone()],
        projection,
        filter: pushed.into_iter().reduce(and_bound),
        group_by: grouped
            .iter()
            .map(|column| BoundExpr {
                kind: BoundExprKind::Column(column.clone()),
                data_type: Some(column.data_type),
                nullable: column.nullable,
            })
            .collect(),
        aggregates: query
            .aggregates
            .iter()
            .map(|aggregate| BoundAggregate {
                output_column: None,
                declared: true,
                ..aggregate.clone()
            })
            .collect(),
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
        outer_set_refusal: None,
    };
    let mut derived = derive(
        target.table_name.clone(),
        target.relation_name.clone(),
        partial,
    );
    // The fold's output is unknown until it runs; the relation's own size
    // bounds it, and keeps join planning where it was.
    derived.estimated_rows = target.row_count.or(target.estimated_rows);

    // Re-fold each partial: counts and sums add, extremes stay extremes.
    for (index, aggregate) in query.aggregates.iter_mut().enumerate() {
        let column = derived.columns[group_count + index].clone();
        aggregate.function = match aggregate.function {
            AggregateFunction::Count | AggregateFunction::Sum => AggregateFunction::Sum,
            other => other,
        };
        aggregate.expr = Some(BoundExpr {
            data_type: Some(column.data_type),
            nullable: column.nullable,
            kind: BoundExprKind::Column(column),
        });
    }
    for expr in &mut query.group_by {
        rebind(expr, target, &grouped, &derived);
    }
    let mut filter = kept;
    for expr in &mut filter {
        rebind(expr, target, &grouped, &derived);
    }
    query.filter = filter.into_iter().reduce(and_bound);
    let source = &mut query.from[0];
    for join in &mut source.joins {
        if let Some(condition) = &mut join.condition {
            rebind(condition, target, &grouped, &derived);
        }
    }
    if same_relation(&source.base, target) {
        source.base = derived.clone();
    } else if let Some(join) = source
        .joins
        .iter_mut()
        .find(|join| same_relation(&join.table, target))
    {
        join.table = derived.clone();
    }
    for table in &mut query.tables {
        if same_relation(table, target) {
            *table = derived.clone();
        }
    }
    Some(())
}
