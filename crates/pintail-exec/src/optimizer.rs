use std::collections::{BTreeMap, BTreeSet};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundJoinKind, BoundLimit, BoundOrderKey, BoundProjection, BoundQuery, ScalarFunction,
    WindowFunction,
};
use pintail_types::{DataType, Value};

use crate::{
    LogicalPlan, Scan,
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
        STATEMENT_NOW.set(Some(statement_now()));
        let plan = fold_constants(plan);
        // After folding so `DATE(c) = CURDATE()` sees a literal; before
        // pushdown so the produced column ranges reach the scan's pruning
        // bounds and vectorized filter mask.
        let plan = crate::temporal_rewrite::rewrite_temporal_predicates(plan);
        let mut plan = push_predicates(plan);
        // After pushdown, because it reads the constants pushdown placed on
        // scans and writes new ones back onto other scans. Twice, so a
        // constant derived at one join can cross a second one.
        for _ in 0..2 {
            plan = propagate_join_constants(plan);
        }
        let mut plan = replace_metadata_counts(plan);
        sink_semi_joins(&mut plan);
        nest_outer_join_bridges(&mut plan);
        let plan = reorder_cross_joins(plan);
        let plan = push_aggregates_through_identity_joins(plan);
        let mut plan = plan;
        prune_derived_columns(&mut plan);
        prune_projections(&mut plan);
        push_top_k_below_left_joins(&mut plan);
        push_limits(&mut plan);
        plan
    }
}

/// `ORDER BY` columns of a LEFT JOIN's preserved table, then `LIMIT n`,
/// needs only that table's first n rows in the same order: a LEFT JOIN
/// keeps every left row and gives each one at least one output row, so the
/// first n output rows come from at most the first n left rows. The table
/// is sorted and cut to n before the joins, and the sort above them stays,
/// so it orders the few joined rows exactly as it ordered the many.
///
/// Ties keep their meaning: rows equal on the keys go by arrival in both
/// sorts, and the joins keep the left rows' arrival order.
fn push_top_k_below_left_joins(plan: &mut LogicalPlan) {
    if let LogicalPlan::Limit { input, limit } = plan
        && let LogicalPlan::Sort {
            input: sorted,
            keys,
            ..
        } = input.as_mut()
        && let LogicalPlan::Project {
            input: joined,
            expressions,
        } = sorted.as_mut()
        && matches!(
            joined.as_ref(),
            LogicalPlan::Join {
                kind: BoundJoinKind::Left,
                ..
            }
        )
    {
        let key_columns = keys
            .iter()
            .map(|key| {
                match expressions
                    .get(key.index)
                    .map(|projection| &projection.expr.kind)
                {
                    Some(BoundExprKind::Column(column)) if !column.outer => Some(column.clone()),
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>();
        if let Some(key_columns) = key_columns
            && !key_columns.is_empty()
        {
            let rows = limit.offset.saturating_add(limit.count);
            let keys = keys.clone();
            let mut spine: &mut LogicalPlan = joined.as_mut();
            while let LogicalPlan::Join {
                kind: BoundJoinKind::Left,
                left,
                ..
            } = spine
            {
                spine = left.as_mut();
            }
            if let LogicalPlan::Scan(scan) = spine
                && scan.limit.is_none()
                && let Some(ordered) = sort_scan_prefix(scan, &key_columns, &keys, rows)
            {
                *spine = deferred_scan_prefix(scan, &key_columns, &keys, rows).unwrap_or(ordered);
            }
        }
    }
    match plan {
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            inputs.iter_mut().for_each(push_top_k_below_left_joins);
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            push_top_k_below_left_joins(left);
            push_top_k_below_left_joins(right);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            push_top_k_below_left_joins(anchor);
            push_top_k_below_left_joins(member);
        }
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Limit { input, .. } => push_top_k_below_left_joins(input),
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
}

/// Moves a semi join down a join chain to the lowest join whose left input
/// holds every relation its condition reads.
///
/// `EXISTS (SELECT 1 FROM s WHERE s.k = b.k)` over `a JOIN b LEFT JOIN c
/// LEFT JOIN d` binds as a semi join above the whole chain, so every row the
/// joins to `c` and `d` fan out to is built before the test drops it. The
/// test reads only `b`, and neither an inner join nor a LEFT join changes
/// the `b` values of the rows it emits from a surviving row, so testing the
/// `a JOIN b` rows first keeps exactly the same output rows and lets the
/// fan-out multiply only the rows that pass.
fn sink_semi_joins(plan: &mut LogicalPlan) {
    match plan {
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            inputs.iter_mut().for_each(sink_semi_joins);
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            sink_semi_joins(left);
            sink_semi_joins(right);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            sink_semi_joins(anchor);
            sink_semi_joins(member);
        }
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Limit { input, .. } => sink_semi_joins(input),
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
    sink_semi_join(plan);
}

fn sink_semi_join(plan: &mut LogicalPlan) {
    let LogicalPlan::Join {
        left,
        right,
        kind: BoundJoinKind::Semi,
        condition: Some(condition),
    } = plan
    else {
        return;
    };
    let LogicalPlan::Join {
        left: kept,
        right: passed,
        kind: BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Cross,
        ..
    } = left.as_ref()
    else {
        return;
    };
    let mut columns = BTreeSet::new();
    collect_expr_columns(condition, &mut columns);
    if expression_contains_subquery(condition)
        || columns.is_empty()
        || !columns.iter().all(|column| {
            let table = (column.0, column.1, column.2.clone());
            (contains_table(kept, &table) || contains_table(right, &table))
                && !contains_table(passed, &table)
        })
    {
        return;
    }
    let LogicalPlan::Join {
        left: semi_left,
        right: semi_right,
        condition: semi_condition,
        ..
    } = std::mem::replace(plan, LogicalPlan::Empty)
    else {
        unreachable!("matched a join above");
    };
    let LogicalPlan::Join {
        left: kept,
        right: passed,
        kind,
        condition: passed_condition,
    } = *semi_left
    else {
        unreachable!("matched a join above");
    };
    let mut sunk = LogicalPlan::Join {
        left: kept,
        right: semi_right,
        kind: BoundJoinKind::Semi,
        condition: semi_condition,
    };
    sink_semi_join(&mut sunk);
    *plan = LogicalPlan::Join {
        left: Box::new(sunk),
        right: passed,
        kind,
        condition: passed_condition,
    };
}

/// Joins a LEFT-joined relation to the relations it is reached through
/// before joining the result to the preserved side, when only duplicate-
/// and NULL-insensitive aggregates read it.
///
/// `a LEFT JOIN b ON b.k = a.k LEFT JOIN c ON c.b = b.id AND c.a = a.id`
/// grouped by `a` builds a row for every `b` of every `a`, null-extended
/// wherever no `c` matches, though `b` serves only to reach `c`. When every
/// aggregate is `COUNT(DISTINCT ...)`, `MIN` or `MAX` of a value that is NULL
/// whenever `c` is, the null-extended rows add nothing but the group itself,
/// which `a` keeps anyway, and repeated `c` rows add nothing at all. So
/// `a LEFT JOIN (b JOIN c ON c.b = b.id) ON b.k = a.k AND c.a = a.id` gives
/// every group the same values while building only the rows that reach a
/// `c`. Each bridging relation must be one that a matched `c` cannot exist
/// without (a condition further along compares its columns), no grouping
/// key, aggregate or later join may read it, and the joins after `c` must
/// be LEFT joins.
fn nest_outer_join_bridges(plan: &mut LogicalPlan) {
    match plan {
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            inputs.iter_mut().for_each(nest_outer_join_bridges);
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            nest_outer_join_bridges(left);
            nest_outer_join_bridges(right);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            nest_outer_join_bridges(anchor);
            nest_outer_join_bridges(member);
        }
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Limit { input, .. } => nest_outer_join_bridges(input),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            nest_outer_join_bridges(input);
            if let Some((bridge, target)) = outer_join_bridge(input, group_by, aggregates) {
                let joined = std::mem::replace(input.as_mut(), LogicalPlan::Empty);
                **input = nest_bridge(joined, bridge, target);
            }
        }
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
}

/// One join of a left-deep chain: its kind, right input and condition.
type SpineJoin<'a> = (BoundJoinKind, &'a LogicalPlan, Option<&'a BoundExpr>);

/// A left-deep join chain as its first input and each join above it.
fn join_spine(plan: &LogicalPlan) -> (&LogicalPlan, Vec<SpineJoin<'_>>) {
    let mut joins = Vec::new();
    let mut spine = plan;
    while let LogicalPlan::Join {
        left,
        right,
        kind,
        condition,
    } = spine
    {
        joins.push((*kind, right.as_ref(), condition.as_ref()));
        spine = left;
    }
    joins.reverse();
    (spine, joins)
}

/// The relations whose columns an expression above `plan` can read.
///
/// A derived table exposes only its own columns: the relations inside it
/// are out of reach, and one that shares a name with a relation outside,
/// as an unaliased table does when a derived table scans it again, is a
/// different relation instance under the same key.
fn plan_tables(plan: &LogicalPlan, out: &mut BTreeSet<TableKey>) {
    match plan {
        LogicalPlan::Scan(scan) => {
            out.insert(table_key(&scan.table));
        }
        LogicalPlan::Derived { columns, .. } => {
            out.extend(columns.iter().map(column_table_key));
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                plan_tables(input, out);
            }
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            plan_tables(left, out);
            plan_tables(right, out);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            plan_tables(anchor, out);
            plan_tables(member, out);
        }
        LogicalPlan::Window { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => plan_tables(input, out),
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

/// Whether `expr` is NULL whenever every relation in `tables` is.
fn null_when(expr: &BoundExpr, tables: &BTreeSet<TableKey>) -> bool {
    match &expr.kind {
        BoundExprKind::Literal(Value::Null) => true,
        BoundExprKind::Column(column) => {
            !column.outer && tables.contains(&column_table_key(column))
        }
        BoundExprKind::Unary { expr, .. } => null_when(expr, tables),
        BoundExprKind::Binary { op, left, right } => match op {
            BinaryOp::And | BinaryOp::Or => null_when(left, tables) && null_when(right, tables),
            _ => null_when(left, tables) || null_when(right, tables),
        },
        // CASE WHEN .. THEN v [ELSE w] END: NULL when every result is.
        BoundExprKind::Scalar {
            function: ScalarFunction::If,
            args,
        } => args.len() == 3 && null_when(&args[1], tables) && null_when(&args[2], tables),
        BoundExprKind::Scalar {
            function: ScalarFunction::Cast(_),
            args,
        } => args.first().is_some_and(|arg| null_when(arg, tables)),
        _ => false,
    }
}

/// The first bridging join and the target join's positions in the chain,
/// when the aggregate above it admits nesting the bridge.
#[allow(clippy::too_many_lines)] // one validate pass over the chain, in order
fn outer_join_bridge(
    input: &LogicalPlan,
    group_by: &[BoundExpr],
    aggregates: &[BoundAggregate],
) -> Option<(usize, usize)> {
    if aggregates.is_empty()
        || !aggregates.iter().all(|aggregate| {
            aggregate.order_within.is_empty()
                && aggregate.separator.is_none()
                && aggregate.expr.is_some()
                && match aggregate.function {
                    AggregateFunction::Count => aggregate.distinct,
                    // Text extremes pick one spelling among values that
                    // compare equal, and arrival order decides which.
                    AggregateFunction::Minimum | AggregateFunction::Maximum => matches!(
                        aggregate.expr.as_ref().and_then(|expr| expr.data_type),
                        Some(
                            DataType::Int8
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
                    ),
                    _ => false,
                }
        })
    {
        return None;
    }
    let (_, joins) = join_spine(input);
    let tables = joins
        .iter()
        .map(|(_, right, _)| {
            let mut tables = BTreeSet::new();
            plan_tables(right, &mut tables);
            tables
        })
        .collect::<Vec<_>>();
    let mut grouped = BTreeSet::new();
    for expr in group_by {
        grouped.extend(referenced_tables(expr));
    }
    for target in (1..joins.len()).rev() {
        if joins[target..]
            .iter()
            .any(|(kind, _, _)| *kind != BoundJoinKind::Left)
        {
            break;
        }
        // The target and every later join a missing target row
        // null-extends: aggregates must be NULL on all of those rows.
        let mut nulled = tables[target].clone();
        for (index, (_, _, condition)) in joins.iter().enumerate().skip(target + 1) {
            if condition.is_some_and(|condition| {
                conjuncts_of(condition)
                    .into_iter()
                    .any(|conjunct| !null_rejected_tables(conjunct).is_disjoint(&nulled))
            }) {
                nulled.extend(tables[index].iter().cloned());
            }
        }
        if !aggregates.iter().all(|aggregate| {
            aggregate
                .expr
                .as_ref()
                .is_some_and(|expr| null_when(expr, &nulled))
        }) {
            continue;
        }
        let mut read_outside = grouped.clone();
        for aggregate in aggregates {
            read_outside.extend(aggregate.expr.iter().flat_map(referenced_tables));
        }
        for (_, _, condition) in &joins[target + 1..] {
            read_outside.extend(
                condition
                    .iter()
                    .flat_map(|condition| referenced_tables(condition)),
            );
        }
        // A bridging relation joins the nest only through its whole key, so
        // the nest holds at most one row per target row: a relation that
        // matched many nest rows would multiply the nest instead of pruning
        // the chain.
        let mut bridge = target;
        let mut reached = tables[target].clone();
        while bridge > 0 {
            let index = bridge - 1;
            if joins[index].0 != BoundJoinKind::Left
                || !read_outside.is_disjoint(&tables[index])
                || !joins[bridge..=target].iter().any(|(_, _, condition)| {
                    condition.is_some_and(|condition| {
                        keyed_from(joins[index].1, &conjuncts_of(condition), &reached)
                    })
                })
            {
                break;
            }
            bridge = index;
            reached.extend(tables[index].iter().cloned());
        }
        // A grouping key read from the nest or after it would group the
        // null-extended rows the nest no longer builds.
        if bridge == target
            || tables[bridge..]
                .iter()
                .any(|tables| !tables.is_disjoint(&grouped))
            || joins[bridge..=target].iter().any(|(_, _, condition)| {
                condition.is_none_or(|condition| {
                    expression_contains_subquery(condition) || is_volatile(condition)
                })
            })
        {
            continue;
        }
        // Each nested join past the first needs a condition of its own
        // inside the nest, or it would become a Cartesian product there.
        let mut inside = tables[bridge].clone();
        let mut linked = true;
        for index in bridge + 1..=target {
            inside.extend(tables[index].iter().cloned());
            linked &= joins[index].2.is_some_and(|condition| {
                conjuncts_of(condition).into_iter().any(|conjunct| {
                    let read = referenced_tables(conjunct);
                    read.is_subset(&inside) && !read.is_disjoint(&tables[index])
                })
            });
        }
        if !linked {
            continue;
        }
        // The target must be keyed on the preserved side too, or nesting
        // only moves the fan-out inside.
        let keyed = joins[target].2.is_some_and(|condition| {
            conjuncts_of(condition).into_iter().any(|conjunct| {
                let read = referenced_tables(conjunct);
                !read.is_disjoint(&tables[target]) && !read.is_subset(&reached)
            })
        });
        if keyed {
            return Some((bridge, target));
        }
    }
    None
}

/// Whether `conjuncts` equate every key column of the relation `plan`
/// scans with a column of the relations in `from`.
fn keyed_from(plan: &LogicalPlan, conjuncts: &[&BoundExpr], from: &BTreeSet<TableKey>) -> bool {
    let mut scanned = plan;
    while let LogicalPlan::Project { input, .. } | LogicalPlan::Filter { input, .. } = scanned {
        scanned = input;
    }
    let LogicalPlan::Scan(scan) = scanned else {
        return false;
    };
    let table = table_key(&scan.table);
    let keys = &scan.table.key_column_ids;
    !keys.is_empty()
        && keys.iter().all(|key| {
            conjuncts.iter().any(|conjunct| {
                let BoundExprKind::Binary {
                    op: BinaryOp::Equal,
                    left,
                    right,
                } = &conjunct.kind
                else {
                    return false;
                };
                let (BoundExprKind::Column(left), BoundExprKind::Column(right)) =
                    (&left.kind, &right.kind)
                else {
                    return false;
                };
                [(left, right), (right, left)].iter().any(|(own, other)| {
                    !own.outer
                        && !other.outer
                        && column_table_key(own) == table
                        && own.column_id == *key
                        && from.contains(&column_table_key(other))
                })
            })
        })
}

fn nest_bridge(plan: LogicalPlan, bridge: usize, target: usize) -> LogicalPlan {
    let mut joins = Vec::new();
    let mut spine = plan;
    while let LogicalPlan::Join {
        left,
        right,
        kind,
        condition,
    } = spine
    {
        joins.push((kind, *right, condition));
        spine = *left;
    }
    joins.reverse();
    let mut rest = joins.split_off(target + 1);
    let nested_joins = joins.split_off(bridge);
    let mut preserved = spine;
    for (kind, right, condition) in joins {
        preserved = LogicalPlan::Join {
            left: Box::new(preserved),
            right: Box::new(right),
            kind,
            condition,
        };
    }
    let mut reached = BTreeSet::new();
    let mut outer = Vec::new();
    let mut nested: Option<LogicalPlan> = None;
    for (_, right, condition) in nested_joins {
        plan_tables(&right, &mut reached);
        let mut inner = Vec::new();
        for conjunct in condition.map(split_conjunction).unwrap_or_default() {
            if referenced_tables(&conjunct).is_subset(&reached) {
                inner.push(conjunct);
            } else {
                outer.push(conjunct);
            }
        }
        let inner = inner.into_iter().reduce(and_expr);
        nested = Some(match nested {
            None => match inner {
                None => right,
                Some(predicate) => LogicalPlan::Filter {
                    input: Box::new(right),
                    predicate,
                },
            },
            Some(left) => LogicalPlan::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind: BoundJoinKind::Inner,
                condition: inner,
            },
        });
    }
    let mut plan = LogicalPlan::Join {
        left: Box::new(preserved),
        right: Box::new(nested.expect("the bridge holds at least two joins")),
        kind: BoundJoinKind::Left,
        condition: outer.into_iter().reduce(and_expr),
    };
    for (kind, right, condition) in rest.drain(..) {
        plan = LogicalPlan::Join {
            left: Box::new(plan),
            right: Box::new(right),
            kind,
            condition,
        };
    }
    plan
}

/// The scan's first `rows` rows under `keys`, when every key column is one
/// of the scan's own: a projection of the scan's columns, sorted and cut.
fn sort_scan_prefix(
    scan: &Scan,
    key_columns: &[BoundColumn],
    keys: &[BoundOrderKey],
    rows: u64,
) -> Option<LogicalPlan> {
    let columns = scan
        .projected_column_ids
        .iter()
        .map(|id| {
            scan.table
                .columns
                .iter()
                .find(|column| column.column_id == *id)
                .cloned()
        })
        .collect::<Option<Vec<_>>>()?;
    // Ascending on the table's own leading key columns is the order its
    // scan already produces, which the key-order join streams without
    // sorting anything; that plan is the better one, so it is left alone.
    if keys.iter().all(|key| key.ascending)
        && key_columns.len() <= scan.table.key_column_ids.len()
        && key_columns
            .iter()
            .zip(&scan.table.key_column_ids)
            .all(|(column, id)| column.column_id == *id)
    {
        return None;
    }
    let scan_key = table_key(&scan.table);
    let keys = keys
        .iter()
        .zip(key_columns)
        .map(|(key, column)| {
            if column_table_key(column) != scan_key {
                return None;
            }
            let index = columns
                .iter()
                .position(|candidate| candidate.column_id == column.column_id)?;
            Some(BoundOrderKey { index, ..*key })
        })
        .collect::<Option<Vec<_>>>()?;
    let expressions = columns
        .into_iter()
        .map(|column| BoundProjection {
            name: column.name.clone(),
            expr: BoundExpr {
                data_type: Some(column.data_type),
                nullable: column.nullable,
                kind: BoundExprKind::Column(column),
            },
        })
        .collect();
    Some(LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Project {
                input: Box::new(LogicalPlan::Scan(scan.clone())),
                expressions,
            }),
            keys,
            trim: 0,
        }),
        limit: BoundLimit {
            offset: 0,
            count: rows,
        },
    })
}

/// Rows a sorted prefix may hold for its other columns to be read by key
/// afterwards rather than beside the sort columns of every row. Matches the
/// most keys a key lookup join accepts from a bounded driving input.
const DEFERRED_PREFIX_ROWS: u64 = 64;

/// The scan's first `rows` rows under `keys`, as `sort_scan_prefix` gives
/// them, with only the sort columns and the primary key read for every
/// row: those are sorted and cut, and the rest of the row is then read by
/// key for the few rows that survive. A wide table otherwise decodes every
/// column of every row to keep a handful.
///
/// The sorted side reads the table under a relation name no query can
/// spell, so the columns above still resolve to the joined, full rows. It
/// keeps the scan's predicates; the rows it names by key already passed
/// them, so the full read carries none. Each surviving row finds exactly
/// its own row by key in the same snapshot, so the LEFT join adds and drops
/// nothing, and it emits in the sorted side's order, arrival ties included.
fn deferred_scan_prefix(
    scan: &Scan,
    key_columns: &[BoundColumn],
    keys: &[BoundOrderKey],
    rows: u64,
) -> Option<LogicalPlan> {
    if rows == 0 || rows > DEFERRED_PREFIX_ROWS {
        return None;
    }
    let [primary] = scan.table.key_column_ids.as_slice() else {
        return None;
    };
    let primary_column = scan
        .table
        .columns
        .iter()
        .find(|column| column.column_id == *primary)?;
    if !crate::execution::integer_key_type(Some(primary_column.data_type))
        || !scan.projected_column_ids.contains(primary)
    {
        return None;
    }
    // The predicates are evaluated over the scan's own columns, so the
    // columns they read stay beside the key and the sort columns.
    let mut read = BTreeSet::new();
    for predicate in &scan.predicates {
        collect_expr_columns(predicate, &mut read);
    }
    let narrow_ids = scan
        .projected_column_ids
        .iter()
        .copied()
        .filter(|id| {
            id == primary
                || key_columns.iter().any(|column| column.column_id == *id)
                || read.iter().any(|(_, _, _, column)| column == id)
        })
        .collect::<Vec<_>>();
    if narrow_ids.len() >= scan.projected_column_ids.len() {
        return None;
    }
    let relation = format!("<ordered {}>", scan.table.relation_name);
    let mut narrow = scan.clone();
    narrow.projected_column_ids = narrow_ids;
    let identity = (scan.table.database_id, scan.table.table_id);
    let original = scan.table.relation_name.clone();
    for column in &mut narrow.table.columns {
        relation.clone_into(&mut column.relation_name);
    }
    relation.clone_into(&mut narrow.table.relation_name);
    for predicate in &mut narrow.predicates {
        if !rename_relation(predicate, identity, &original, &relation) {
            return None;
        }
    }
    let narrow_keys = key_columns
        .iter()
        .map(|column| {
            let mut column = column.clone();
            relation.clone_into(&mut column.relation_name);
            column
        })
        .collect::<Vec<_>>();
    let narrow_primary = narrow
        .table
        .columns
        .iter()
        .find(|column| column.column_id == *primary)?
        .clone();
    let ordered = sort_scan_prefix(&narrow, &narrow_keys, keys, rows)?;
    let mut wide = scan.clone();
    wide.predicates.clear();
    let column_expr = |column: BoundColumn| BoundExpr {
        data_type: Some(column.data_type),
        nullable: column.nullable,
        kind: BoundExprKind::Column(column),
    };
    let condition = BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: false,
        kind: BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: Box::new(column_expr(narrow_primary)),
            right: Box::new(column_expr(primary_column.clone())),
        },
    };
    Some(LogicalPlan::Join {
        left: Box::new(ordered),
        right: Box::new(LogicalPlan::Scan(wide)),
        kind: BoundJoinKind::Left,
        condition: Some(condition),
    })
}

/// Gives the columns of `identity` under `relation` in `expr` the relation
/// name `renamed`; false when the expression holds anything it cannot walk.
fn rename_relation(
    expr: &mut BoundExpr,
    identity: (DatabaseId, TableId),
    relation: &str,
    renamed: &str,
) -> bool {
    match &mut expr.kind {
        BoundExprKind::Column(column) => {
            if !column.outer
                && column.relation_name.eq_ignore_ascii_case(relation)
                && (column.database_id, column.table_id) == identity
            {
                renamed.clone_into(&mut column.relation_name);
            }
            true
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            rename_relation(expr, identity, relation, renamed)
        }
        BoundExprKind::Binary { left, right, .. } => {
            rename_relation(left, identity, relation, renamed)
                && rename_relation(right, identity, relation, renamed)
        }
        BoundExprKind::Scalar { args, .. } => args
            .iter_mut()
            .all(|argument| rename_relation(argument, identity, relation, renamed)),
        BoundExprKind::Literal(_) => true,
        _ => false,
    }
}

/// Drops the columns of a derived table that nothing above it reads, with
/// the projection expressions that produced them, so the scans beneath it
/// read only what is used. A subquery rewritten as a join reads its table
/// through such a derived input carrying every column.
///
/// Only where the derived table's consumer finds its columns by reference:
/// a sort key, DISTINCT or a set operation reads its input by position,
/// and dropping a column would move the ones after it.
fn prune_derived_columns(plan: &mut LogicalPlan) {
    loop {
        let mut required = BTreeSet::new();
        collect_columns(plan, &mut required, true, true);
        if !trim_derived_columns(plan, &required, false) {
            break;
        }
    }
}

/// Whether a derived table whose rows come from `plan` is one `MySQL` merges
/// into the query that reads it - rows from scans, filters and joins, with
/// no grouping, DISTINCT, limit, window or set operation to materialize it -
/// so a select-list expression nothing reads is never evaluated there, and
/// dropping it changes no warning or error.
fn merges(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Scan(_) | LogicalPlan::OneRow | LogicalPlan::Empty => true,
        LogicalPlan::Filter { input, .. } => merges(input),
        LogicalPlan::Join { left, right, .. } => merges(left) && merges(right),
        LogicalPlan::CrossJoin { inputs } => inputs.iter().all(merges),
        LogicalPlan::Derived { input, .. } => match input.as_ref() {
            LogicalPlan::Project { input, .. } => merges(input),
            _ => false,
        },
        _ => false,
    }
}

/// One pass of [`prune_derived_columns`]; `by_reference` says whether the
/// consumer of `plan` finds its columns by reference. Whether anything
/// was dropped.
fn trim_derived_columns(
    plan: &mut LogicalPlan,
    required: &BTreeSet<ColumnKey>,
    by_reference: bool,
) -> bool {
    match plan {
        LogicalPlan::Derived { input, columns } => {
            let mut changed = false;
            if let LogicalPlan::Project {
                input: source,
                expressions,
            } = input.as_mut()
                && by_reference
                && expressions.len() == columns.len()
            {
                // Where the derived table materializes, an unread column is
                // dropped only when it is a bare column reference: reading
                // a column raises nothing, so no warning or error is lost.
                let merged = merges(source);
                let mut keep = columns
                    .iter()
                    .zip(expressions.iter())
                    .map(|(column, projection)| {
                        required.contains(&column_key(column))
                            || !(merged || matches!(projection.expr.kind, BoundExprKind::Column(_)))
                    })
                    .collect::<Vec<_>>();
                // One column stays whatever is read, so the rows remain.
                if let Some(first) = keep.first_mut()
                    && !columns
                        .iter()
                        .any(|column| required.contains(&column_key(column)))
                {
                    *first = true;
                }
                if keep.contains(&false) {
                    let mut flags = keep.iter();
                    columns.retain(|_| flags.next().copied().unwrap_or(true));
                    let mut flags = keep.iter();
                    expressions.retain(|_| flags.next().copied().unwrap_or(true));
                    changed = true;
                }
            }
            trim_derived_columns(input, required, false) || changed
        }
        LogicalPlan::Project { input, .. } | LogicalPlan::Aggregate { input, .. } => {
            trim_derived_columns(input, required, true)
        }
        LogicalPlan::Filter { input, .. } | LogicalPlan::Window { input, .. } => {
            trim_derived_columns(input, required, by_reference)
        }
        LogicalPlan::Join { left, right, .. } => {
            let left = trim_derived_columns(left, required, by_reference);
            trim_derived_columns(right, required, by_reference) || left
        }
        LogicalPlan::CrossJoin { inputs } => {
            let mut changed = false;
            for input in inputs {
                changed |= trim_derived_columns(input, required, by_reference);
            }
            changed
        }
        LogicalPlan::UnionAll { inputs } => {
            let mut changed = false;
            for input in inputs {
                changed |= trim_derived_columns(input, required, false);
            }
            changed
        }
        LogicalPlan::SetOp { left, right, .. } => {
            let left = trim_derived_columns(left, required, false);
            trim_derived_columns(right, required, false) || left
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            let anchor = trim_derived_columns(anchor, required, false);
            trim_derived_columns(member, required, false) || anchor
        }
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Limit { input, .. } => trim_derived_columns(input, required, false),
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => false,
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
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
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
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            key_collations,
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
                    output_column: None,
                    declared: true,
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
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
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
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            key_collations,
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
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
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
                // A constant-false filter must keep its input's SHAPE: the
                // bare Empty node carries no columns, so a projection above
                // it died with "physical input is missing <column>" (found
                // by the grammar fuzzer - WHERE 'zz' IS NULL folds false).
                // LIMIT 0 answers the empty set while the schema survives.
                Some(false) => LogicalPlan::Limit {
                    input: Box::new(input),
                    limit: pintail_sql::BoundLimit {
                        offset: 0,
                        count: 0,
                    },
                },
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
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            key_collations,
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

/// A parsed session time zone for statement-pinned time functions.
#[derive(Clone, Copy)]
enum SessionZone {
    Fixed(chrono::FixedOffset),
    Named(chrono_tz::Tz),
}

thread_local! {
    static SESSION_TIMESTAMP_MICROS: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    static SESSION_DEFAULT_WEEK_FORMAT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    static SESSION_TIME_ZONE: std::cell::Cell<Option<SessionZone>> =
        const { std::cell::Cell::new(None) };
}

/// Install a fixed statement clock, or restore the real clock with `None`.
pub fn set_session_timestamp_micros(timestamp: Option<i64>) {
    SESSION_TIMESTAMP_MICROS.set(timestamp);
}

/// The fixed clock that participates in query-sharing identity.
#[must_use]
pub fn session_timestamp_micros() -> Option<i64> {
    SESSION_TIMESTAMP_MICROS.get()
}

/// The local calendar date of this session's clock, needed by temporal casts.
#[must_use]
pub fn session_statement_date() -> i32 {
    chrono::Datelike::num_days_from_ce(&statement_now().local.date())
}

fn statement_now() -> StatementNow {
    let utc = SESSION_TIMESTAMP_MICROS
        .get()
        .and_then(chrono::DateTime::from_timestamp_micros)
        .unwrap_or_else(chrono::Utc::now);
    let local = match SESSION_TIME_ZONE.get() {
        None => utc.with_timezone(&chrono::Local).naive_local(),
        Some(SessionZone::Fixed(offset)) => utc.with_timezone(&offset).naive_local(),
        Some(SessionZone::Named(zone)) => utc.with_timezone(&zone).naive_local(),
    };
    StatementNow {
        local,
        utc: utc.naive_utc(),
        unix: utc.timestamp(),
    }
}

/// Installs the default week mode for planning, clamping it to 0 through 7.
/// `None` restores mode zero.
pub fn set_session_default_week_format(mode: Option<u8>) {
    SESSION_DEFAULT_WEEK_FORMAT.set(mode.unwrap_or(0).min(7));
}

/// The installed default week mode, also used to separate cached answers.
#[must_use]
pub fn session_default_week_format() -> u8 {
    SESSION_DEFAULT_WEEK_FORMAT.get()
}

/// The installed session zone's identity, or `None` for the host zone.
///
/// Two executions observe the same clock offset only when this matches, so
/// a caller deciding whether one execution can answer another's request
/// puts this in the comparison.
#[must_use]
pub fn session_time_zone_key() -> Option<String> {
    SESSION_TIME_ZONE.get().map(|zone| match zone {
        SessionZone::Fixed(offset) => format!("f{}", offset.local_minus_utc()),
        SessionZone::Named(zone) => format!("n{}", zone.name()),
    })
}

/// Installs the session time zone that `NOW`/`CURDATE`/`CURTIME` observe on
/// this thread ("SYSTEM" or `None` restores the host zone). Numeric offsets
/// use `MySQL`'s `[-13:59, +14:00]` range; names resolve case-insensitively
/// through the embedded tz database. Returns `false` for an unknown or
/// out-of-range zone, leaving the previous setting in place.
#[must_use]
pub fn set_session_time_zone(zone: Option<&str>) -> bool {
    let Some(zone) = zone else {
        SESSION_TIME_ZONE.set(None);
        pintail_sql::set_session_timestamp_zone(None);
        return true;
    };
    let trimmed = zone.trim();
    if trimmed.eq_ignore_ascii_case("system") {
        SESSION_TIME_ZONE.set(None);
        pintail_sql::set_session_timestamp_zone(None);
        return true;
    }
    if trimmed.starts_with(['+', '-']) {
        let Some((hours, minutes)) = trimmed[1..].split_once(':') else {
            return false;
        };
        let (Ok(hours), Ok(minutes)) = (hours.parse::<i32>(), minutes.parse::<i32>()) else {
            return false;
        };
        if !(0..=59).contains(&minutes) {
            return false;
        }
        let mut seconds = (hours * 60 + minutes) * 60;
        if trimmed.starts_with('-') {
            seconds = -seconds;
        }
        if !((-14 * 3600 + 60)..=(14 * 3600)).contains(&seconds) {
            return false;
        }
        let Some(offset) = chrono::FixedOffset::east_opt(seconds) else {
            return false;
        };
        SESSION_TIME_ZONE.set(Some(SessionZone::Fixed(offset)));
        // Stored TIMESTAMP values are UTC: only another offset converts them.
        pintail_sql::set_session_timestamp_zone((seconds != 0).then_some(trimmed));
        return true;
    }
    let Ok(zone) = chrono_tz::Tz::from_str_insensitive(trimmed) else {
        return false;
    };
    SESSION_TIME_ZONE.set(Some(SessionZone::Named(zone)));
    pintail_sql::set_session_timestamp_zone(Some(zone.name()));
    true
}

#[derive(Clone, Copy)]
struct StatementNow {
    local: chrono::NaiveDateTime,
    utc: chrono::NaiveDateTime,
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
        ScalarFunction::UtcTimestamp => {
            Some(Value::Utf8(now.utc.format("%Y-%m-%d %H:%M:%S").to_string()))
        }
        ScalarFunction::UtcDate => Some(Value::Utf8(now.utc.format("%Y-%m-%d").to_string())),
        ScalarFunction::UtcTime => Some(Value::Utf8(now.utc.format("%H:%M:%S").to_string())),
        ScalarFunction::UnixTimestamp => Some(Value::UInt64(u64::try_from(now.unix).unwrap_or(0))),
        _ => None,
    }
}

/// `MySQL` prunes constant disjuncts BEFORE row evaluation: `x OR TRUE` is
/// TRUE even where x would error row-wise (an unsigned subtraction
/// underflow, say), and `x AND FALSE` is FALSE the same way. Found by the
/// grammar fuzzer: Pintail evaluated the doomed side and errored where
/// `MySQL` answers.
fn fold_binary(
    op: BinaryOp,
    left: BoundExpr,
    right: BoundExpr,
    data_type: Option<DataType>,
    nullable: bool,
) -> BoundExpr {
    if matches!(op, BinaryOp::Or | BinaryOp::And) {
        let absorbing = matches!(op, BinaryOp::Or);
        for side in [&left, &right] {
            if literal_truth_of(side) == Some(absorbing) {
                return BoundExpr {
                    kind: BoundExprKind::Literal(Value::Boolean(absorbing)),
                    data_type: Some(DataType::Boolean),
                    nullable: false,
                };
            }
        }
        // The neutral element drops away: `x OR FALSE` is x, `x AND TRUE`
        // is x - but only when the kept side is already Boolean-shaped,
        // so typing is preserved.
        if literal_truth_of(&right) == Some(!absorbing) && left.data_type == Some(DataType::Boolean)
        {
            return left;
        }
        if literal_truth_of(&left) == Some(!absorbing) && right.data_type == Some(DataType::Boolean)
        {
            return right;
        }
    }
    BoundExpr {
        kind: BoundExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        data_type,
        nullable,
    }
}

/// Folds a binary node whose operands fold to constants. A constant that
/// cannot evaluate - an overflow - fails the same way at run time; its
/// operands stay as written, so the error prints the expression the
/// statement wrote, as `MySQL`'s does.
fn fold_arithmetic(
    op: BinaryOp,
    left: BoundExpr,
    right: BoundExpr,
    data_type: Option<DataType>,
    nullable: bool,
) -> BoundExpr {
    let (written_left, written_right) = (left.clone(), right.clone());
    let folded = fold_binary(op, fold_expr(left), fold_expr(right), data_type, nullable);
    if let Some(value) = evaluate_constant(&folded) {
        return BoundExpr {
            nullable: matches!(value, Value::Null),
            data_type: folded.data_type.or_else(|| value.data_type()),
            kind: BoundExprKind::Literal(value),
        };
    }
    if let BoundExprKind::Binary { left, right, .. } = &folded.kind
        && matches!(left.kind, BoundExprKind::Literal(_))
        && matches!(right.kind, BoundExprKind::Literal(_))
    {
        return BoundExpr {
            kind: BoundExprKind::Binary {
                op,
                left: Box::new(written_left),
                right: Box::new(written_right),
            },
            ..folded
        };
    }
    folded
}

fn capture_timestamp_offsets(function: ScalarFunction, args: &mut [BoundExpr]) {
    let count = match function {
        ScalarFunction::DateDiff
        | ScalarFunction::TimestampDiff { .. }
        | ScalarFunction::AddTime
        | ScalarFunction::SubTime
        | ScalarFunction::TimeDiff => 2,
        ScalarFunction::Date
        | ScalarFunction::Time
        | ScalarFunction::DatePart(_)
        | ScalarFunction::ExtractTime { .. }
        | ScalarFunction::ExtractMicros { .. }
        | ScalarFunction::PackedDateParts { .. }
        | ScalarFunction::DateFormat
        | ScalarFunction::TimeFormat
        | ScalarFunction::DateInterval { .. }
        | ScalarFunction::DayName
        | ScalarFunction::MonthName
        | ScalarFunction::LastDay
        | ScalarFunction::ToDays
        | ScalarFunction::ToSeconds
        | ScalarFunction::YearWeek
        | ScalarFunction::TimeToSec
        | ScalarFunction::UnixTimestamp
        | ScalarFunction::ConvertTz
        | ScalarFunction::Cast(
            DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. },
        )
        | ScalarFunction::DeclaredCast {
            target: DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. },
            ..
        } => 1,
        _ => return,
    };
    for argument in args.iter_mut().take(count) {
        if argument.data_type != Some(DataType::Utf8)
            || matches!(&argument.kind, BoundExprKind::Literal(Value::Utf8(text)) if !crate::expression::has_timestamp_offset(text))
            || matches!(
                &argument.kind,
                BoundExprKind::Scalar {
                    function: ScalarFunction::NormalizeTimestampOffset,
                    ..
                }
            )
        {
            continue;
        }
        let zone = SESSION_TIME_ZONE.get().map_or_else(
            || "SYSTEM".to_owned(),
            |zone| match zone {
                SessionZone::Fixed(offset) => offset.to_string(),
                SessionZone::Named(zone) => zone.name().to_owned(),
            },
        );
        *argument = BoundExpr {
            data_type: Some(DataType::Utf8),
            nullable: true,
            kind: BoundExprKind::Scalar {
                function: ScalarFunction::NormalizeTimestampOffset,
                args: vec![
                    argument.clone(),
                    BoundExpr {
                        data_type: Some(DataType::Utf8),
                        nullable: false,
                        kind: BoundExprKind::Literal(Value::Utf8(zone)),
                    },
                ],
            },
        };
    }
}

/// A TIME given where a date is read is the statement's date at that time,
/// as `MySQL` converts it: `DAY(t)` is today's day, and a time past
/// midnight or below zero runs into the days around it. The argument
/// becomes a cast to DATETIME carrying the statement's date, so every
/// date function reads it the way it reads any datetime. The time-of-day
/// parts read the time itself and are left alone.
fn read_time_arguments_as_dates(function: ScalarFunction, args: &mut [BoundExpr]) {
    use pintail_sql::DatePart;
    let positions: &[usize] = match function {
        ScalarFunction::DatePart(DatePart::Hour | DatePart::Minute | DatePart::Second) => return,
        ScalarFunction::DatePart(_)
        | ScalarFunction::DayName
        | ScalarFunction::MonthName
        | ScalarFunction::ToDays
        | ScalarFunction::ToSeconds
        | ScalarFunction::YearWeek
        | ScalarFunction::LastDay
        | ScalarFunction::UnixTimestamp
        | ScalarFunction::ConvertTz => &[0],
        ScalarFunction::DateDiff | ScalarFunction::TimestampDiff { .. } => &[0, 1],
        _ => return,
    };
    for position in positions {
        let Some(argument) = args.get_mut(*position) else {
            continue;
        };
        let Some(DataType::Time64 { fsp }) = argument.data_type else {
            continue;
        };
        let target = DataType::DateTime64 { fsp };
        let mut cast = vec![argument.clone()];
        if let Some(now) = STATEMENT_NOW.get() {
            cast.push(BoundExpr {
                kind: BoundExprKind::Literal(Value::Utf8(now.local.format("%Y-%m-%d").to_string())),
                data_type: Some(DataType::Date32),
                nullable: false,
            });
        }
        *argument = BoundExpr {
            kind: BoundExprKind::Scalar {
                function: ScalarFunction::Cast(target),
                args: cast,
            },
            data_type: Some(target),
            nullable: true,
        };
    }
}

/// Resolve connection settings once, before expressions move to worker threads.
fn capture_scalar_session(function: ScalarFunction, args: &mut Vec<BoundExpr>) {
    capture_timestamp_offsets(function, args);
    read_time_arguments_as_dates(function, args);
    if matches!(
        function,
        ScalarFunction::Cast(DataType::Year)
            | ScalarFunction::DeclaredCast {
                target: DataType::Year,
                ..
            }
    ) && args.len() == 1
        && matches!(args[0].data_type, Some(DataType::Time64 { .. }))
        && let Some(now) = STATEMENT_NOW.get()
    {
        args.push(BoundExpr {
            kind: BoundExprKind::Literal(Value::Int64(i64::from(chrono::Datelike::year(
                &now.local,
            )))),
            data_type: Some(DataType::Int64),
            nullable: false,
        });
    }
    let date_arity = match function {
        ScalarFunction::Date
        | ScalarFunction::Cast(DataType::Date32 | DataType::DateTime64 { .. })
        | ScalarFunction::DeclaredCast {
            target: DataType::Date32 | DataType::DateTime64 { .. },
            ..
        } => Some(1),
        ScalarFunction::DateInterval {
            unit:
                pintail_sql::IntervalUnit::Year
                | pintail_sql::IntervalUnit::Month
                | pintail_sql::IntervalUnit::Day,
            ..
        } => Some(2),
        _ => None,
    };
    if date_arity == Some(args.len())
        && matches!(args[0].data_type, Some(DataType::Time64 { .. }))
        && let Some(now) = STATEMENT_NOW.get()
    {
        args.push(BoundExpr {
            kind: BoundExprKind::Literal(Value::Utf8(now.local.format("%Y-%m-%d").to_string())),
            data_type: Some(DataType::Date32),
            nullable: false,
        });
    }
    let locale_arity = match function {
        ScalarFunction::DayName | ScalarFunction::MonthName => Some(1),
        ScalarFunction::DateFormat => Some(2),
        _ => None,
    };
    // The binder may leave the session's date policy (a signed literal)
    // after the written arguments; the locale goes before it.
    let before_policy = locale_arity.is_some_and(|arity| {
        args.len() == arity + 1 && args[arity].data_type == Some(DataType::Int64)
    });
    if locale_arity == Some(args.len()) || before_policy {
        let at = locale_arity.unwrap_or(args.len());
        args.insert(
            at,
            BoundExpr {
                kind: BoundExprKind::Literal(Value::UInt64(u64::from(
                    crate::calendar_locale::session_locale_id(),
                ))),
                data_type: Some(DataType::UInt8),
                nullable: false,
            },
        );
    }
    if function == ScalarFunction::DateFormat
        && args.len() == 3
        && matches!(args[0].data_type, Some(DataType::Time64 { .. }))
        && let Some(now) = STATEMENT_NOW.get()
    {
        args.push(BoundExpr {
            kind: BoundExprKind::Literal(Value::Utf8(now.local.format("%Y-%m-%d").to_string())),
            data_type: Some(DataType::Date32),
            nullable: false,
        });
    }
    // Capture the connection's zone in the plan: execution may run
    // on workers that do not carry the caller's thread-local state.
    if matches!(
        function,
        ScalarFunction::UnixTimestamp | ScalarFunction::FromUnixTime
    ) && args.len()
        == if function == ScalarFunction::FromUnixTime {
            2
        } else {
            1
        }
        && let Some(zone) = SESSION_TIME_ZONE.get()
    {
        let zone = match zone {
            SessionZone::Fixed(offset) => offset.to_string(),
            SessionZone::Named(zone) => zone.name().to_owned(),
        };
        args.push(BoundExpr {
            kind: BoundExprKind::Literal(Value::Utf8(zone)),
            data_type: Some(DataType::Utf8),
            nullable: false,
        });
    }
}

fn fold_expr(expr: BoundExpr) -> BoundExpr {
    // Keep exact-decimal arithmetic as a tree. The executor evaluates a
    // chain as one reduced rational so enclosing operations see MySQL's
    // unrounded division intermediates. Folding the inner node here would
    // materialize it at its advertised scale and irreversibly lose those
    // guard digits before the outer operation runs.
    if matches!(expr.data_type, Some(DataType::Decimal { .. }))
        && matches!(
            expr.kind,
            BoundExprKind::Binary {
                op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
                ..
            }
        )
    {
        return expr;
    }
    let folded = match expr.kind {
        BoundExprKind::PreparedIn { .. }
        | BoundExprKind::Column(_)
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
        BoundExprKind::Binary { op, left, right } => {
            return fold_arithmetic(op, *left, *right, expr.data_type, expr.nullable);
        }
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
            let function = match function {
                ScalarFunction::DatePart(pintail_sql::DatePart::Week) => ScalarFunction::DatePart(
                    pintail_sql::DatePart::WeekMode(session_default_week_format()),
                ),
                ScalarFunction::DatePart(pintail_sql::DatePart::ExtractWeek(_)) => {
                    ScalarFunction::DatePart(pintail_sql::DatePart::ExtractWeek(
                        session_default_week_format(),
                    ))
                }
                function => function,
            };
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
            let mut args: Vec<_> = args.into_iter().map(fold_expr).collect();
            capture_scalar_session(function, &mut args);
            BoundExpr {
                kind: BoundExprKind::Scalar { function, args },
                data_type: expr.data_type,
                nullable: expr.nullable,
            }
        }
    };

    match evaluate_constant(&folded) {
        // The fold keeps the expression's DECLARED type: NULL = NULL is a
        // Boolean expression whose value happens to be NULL, and retyping
        // it from the value made the wire advertise VAR_STRING where MySQL
        // says LONGLONG.
        Some(value) => BoundExpr {
            nullable: matches!(value, Value::Null),
            data_type: folded.data_type.or_else(|| value.data_type()),
            kind: BoundExprKind::Literal(value),
        },
        None => folded,
    }
}

#[allow(clippy::too_many_lines)] // one arm per expression kind reads best unsplit
fn evaluate_constant(expr: &BoundExpr) -> Option<Value> {
    match &expr.kind {
        // Integer casts of decimals may warn. Keep them in execution so
        // diagnostics are produced for each evaluated row, not during planning.
        BoundExprKind::Scalar {
            function:
                ScalarFunction::DeclaredCast {
                    target: DataType::Int64 | DataType::UInt64,
                    ..
                },
            args,
        } if matches!(
            args.first().and_then(|arg| arg.data_type),
            Some(DataType::Decimal { .. })
        ) =>
        {
            None
        }
        // A binary declaration carries the identity width for downstream
        // bit folds, including CAST(NULL AS BINARY(n)). A literal alone
        // cannot preserve it, so retain the declaration during planning.
        BoundExprKind::Scalar {
            function:
                ScalarFunction::DeclaredCast {
                    target: DataType::Binary,
                    characters: Some(_),
                },
            ..
        } => None,
        BoundExprKind::Scalar {
            function: ScalarFunction::Lpad | ScalarFunction::Rpad,
            ..
        } if expr.binary_width().is_some() => None,
        BoundExprKind::Scalar {
            function:
                ScalarFunction::DateInterval { .. }
                | ScalarFunction::Cast(_)
                | ScalarFunction::DeclaredCast { .. },
            args,
        } if args
            .iter()
            .all(|arg| matches!(arg.kind, BoundExprKind::Literal(_))) =>
        {
            // A literal interval or cast is statement-invariant - the binder
            // casts a literal compared with a TIME or a mixed temporal, and
            // left unfolded it was re-cast for every row. Use the runtime
            // evaluator to retain its month-end, NULL and precision rules;
            // an error stays in the original expression for execution.
            let collation = crate::collation::Collation::from_mysql_name(
                pintail_sql::session_default_collation(),
            )
            .unwrap_or_default();
            let compiled = crate::expression::CompiledExpr::compile(expr, &[], collation).ok()?;
            compiled
                .evaluate(&crate::RecordBatch::new(1, Vec::new()).ok()?, 0)
                .ok()
        }
        // DATE of a literal names one day for the whole statement. Left
        // unfolded it kept an enclosing interval and the comparison above
        // it off the scan's bounds, and re-read the text for every row. A
        // literal it cannot read warns, and that warning belongs to the
        // execution, so only a quiet evaluation folds.
        BoundExprKind::Scalar {
            function: ScalarFunction::Date,
            args,
        } if args
            .iter()
            .all(|arg| matches!(arg.kind, BoundExprKind::Literal(_))) =>
        {
            let collation = crate::collation::Collation::from_mysql_name(
                pintail_sql::session_default_collation(),
            )
            .unwrap_or_default();
            let compiled = crate::expression::CompiledExpr::compile(expr, &[], collation).ok()?;
            let batch = crate::RecordBatch::new(1, Vec::new()).ok()?;
            crate::execution::without_new_warnings(|| compiled.evaluate(&batch, 0).ok())
                .flatten()
                .filter(|value| !matches!(value, Value::Null))
        }
        BoundExprKind::PreparedIn { .. }
        | BoundExprKind::Column(_)
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
            if *op == BinaryOp::IntegerDivide
                || matches!(expr.data_type, Some(DataType::Float32 | DataType::Float64))
            {
                // Numeric consumers read a decimal subtree's internal digits. Evaluating
                // each child to its display value would round them away.
                let compiled = crate::expression::CompiledExpr::compile(
                    expr,
                    &[],
                    crate::collation::Collation::from_mysql_name(
                        pintail_sql::session_default_collation(),
                    )
                    .unwrap_or_default(),
                )
                .ok()?;
                return compiled
                    .evaluate(&crate::RecordBatch::new(1, Vec::new()).ok()?, 0)
                    .ok();
            }
            // Constant folding sees only literals, never a column, so the
            // CONNECTION collation is the one that applies - exactly as it
            // does in MySQL, where 'x' = 'x   ' answers differently under a
            // PAD SPACE session collation than under the NO PAD default.
            evaluate_runtime_binary(
                *op,
                &left,
                &right,
                expr.data_type,
                crate::collation::Collation::from_mysql_name(
                    pintail_sql::session_default_collation(),
                )
                .unwrap_or_default(),
            )
            .ok()
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

/// Definite truth of a folded literal, `None` for anything else INCLUDING
/// the NULL literal: `NULL OR x` is x-or-NULL under three-valued logic and
/// must not be pruned.
fn literal_truth_of(expr: &BoundExpr) -> Option<bool> {
    match &expr.kind {
        BoundExprKind::Literal(Value::Null) => None,
        BoundExprKind::Literal(value) => mysql_truth(value).ok().flatten(),
        _ => None,
    }
}

fn literal_truth(expr: &BoundExpr) -> Option<bool> {
    let BoundExprKind::Literal(value) = &expr.kind else {
        return None;
    };
    mysql_truth(value).ok().map(|value| value.unwrap_or(false))
}

#[allow(clippy::too_many_lines)] // structural walk, one arm per plan node
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
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
            anchor: Box::new(push_predicates(*anchor)),
            member: Box::new(push_predicates(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => {
            let mut left = push_predicates(*left);
            let mut right = push_predicates(*right);
            // An inner or semi join's condition drops every row it cannot
            // match, which includes the null-extended rows of an outer join
            // below it whose columns the condition compares.
            if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Semi)
                && let Some(condition) = &condition
            {
                for conjunct in conjuncts_of(condition) {
                    let rejected = null_rejected_tables(conjunct);
                    reject_null_extension(&mut left, &rejected);
                    if kind == BoundJoinKind::Inner {
                        reject_null_extension(&mut right, &rejected);
                    }
                }
            }
            LogicalPlan::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                condition,
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            let mut input = push_predicates(*input);
            for conjunct in conjuncts_of(&predicate) {
                reject_null_extension(&mut input, &null_rejected_tables(conjunct));
            }
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
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            key_collations,
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

/// Turns a LEFT join into an inner one when a predicate above it rejects
/// the null-extended rows it adds.
///
/// A WHERE conjunct such as `r.k = 5` is never true for a row whose `r`
/// columns are all NULL, so a LEFT join producing `r` keeps no row the
/// inner join would not, and the conjunct may then sink into `r` itself
/// instead of waiting above every join the chain fans out through. The
/// converted join's own condition rejects nulls in turn: `r.a = l.a` drops
/// the rows where an outer join further down null-extended `l`.
fn reject_null_extension(plan: &mut LogicalPlan, rejected: &BTreeSet<TableKey>) {
    if rejected.is_empty() {
        return;
    }
    match plan {
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => {
            // Only a relation the predicate can read counts: a derived
            // table's inner scan of a same-named table is not the one the
            // predicate compares.
            let exposed = || {
                let mut tables = BTreeSet::new();
                plan_tables(right, &mut tables);
                tables
            };
            if *kind == BoundJoinKind::Left && !exposed().is_disjoint(rejected) {
                *kind = BoundJoinKind::Inner;
                if let Some(condition) = condition {
                    for conjunct in conjuncts_of(condition) {
                        let implied = null_rejected_tables(conjunct);
                        reject_null_extension(left, &implied);
                        reject_null_extension(right, &implied);
                    }
                }
            }
            reject_null_extension(left, rejected);
            if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Cross) {
                reject_null_extension(right, rejected);
            }
        }
        LogicalPlan::CrossJoin { inputs } => {
            for input in inputs {
                reject_null_extension(input, rejected);
            }
        }
        LogicalPlan::Filter { input, .. } => reject_null_extension(input, rejected),
        _ => {}
    }
}

/// The relations whose all-NULL row makes `predicate` NULL or false.
fn null_rejected_tables(predicate: &BoundExpr) -> BTreeSet<TableKey> {
    match &predicate.kind {
        BoundExprKind::Binary { op, left, right } => match op {
            BinaryOp::And => {
                let mut tables = null_rejected_tables(left);
                tables.extend(null_rejected_tables(right));
                tables
            }
            BinaryOp::Or => null_rejected_tables(left)
                .intersection(&null_rejected_tables(right))
                .cloned()
                .collect(),
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual => {
                let mut tables = null_propagating_tables(left);
                tables.extend(null_propagating_tables(right));
                tables
            }
            _ => BTreeSet::new(),
        },
        BoundExprKind::IsNull {
            expr,
            negated: true,
        }
        // `NULL NOT IN (<empty set>)` is true, so only the positive test.
        | BoundExprKind::PreparedIn {
            expr,
            negated: false,
            ..
        } => null_propagating_tables(expr),
        BoundExprKind::Scalar {
            function:
                ScalarFunction::Like { .. }
                | ScalarFunction::InList { .. }
                | ScalarFunction::Between { .. },
            args,
        } => args.first().map_or_else(BTreeSet::new, null_propagating_tables),
        _ => BTreeSet::new(),
    }
}

/// The relations whose all-NULL row makes `expr` evaluate to NULL.
fn null_propagating_tables(expr: &BoundExpr) -> BTreeSet<TableKey> {
    match &expr.kind {
        BoundExprKind::Column(column) if !column.outer => {
            BTreeSet::from([column_table_key(column)])
        }
        BoundExprKind::Unary { expr, .. } => null_propagating_tables(expr),
        BoundExprKind::Binary { op, left, right } => match op {
            BinaryOp::And | BinaryOp::Or => null_propagating_tables(left)
                .intersection(&null_propagating_tables(right))
                .cloned()
                .collect(),
            _ => {
                let mut tables = null_propagating_tables(left);
                tables.extend(null_propagating_tables(right));
                tables
            }
        },
        BoundExprKind::Scalar {
            function: ScalarFunction::Cast(_),
            args,
        } => args
            .first()
            .map_or_else(BTreeSet::new, null_propagating_tables),
        _ => BTreeSet::new(),
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
    // A subquery surviving to this point is dependent (uncorrelated ones
    // were materialized during logical planning), and dependent resolution
    // exists only at Filter level. Pushing one into a scan looked safe for
    // a self-join, where the outer and inner references collapse to one
    // table key, and compiled into "unresolved subquery".
    if expression_contains_subquery(predicate) {
        return false;
    }
    let referenced_tables = referenced_tables(predicate);
    if referenced_tables.len() != 1 {
        return false;
    }
    let table = referenced_tables
        .iter()
        .next()
        .expect("single referenced table");

    match plan {
        LogicalPlan::Scan(scan) if &table_key(&scan.table) == table => {
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

fn contains_table(plan: &LogicalPlan, table: &TableKey) -> bool {
    match plan {
        LogicalPlan::Scan(scan) => &table_key(&scan.table) == table,
        LogicalPlan::Recursive { anchor, member, .. } => {
            contains_table(anchor, table) || contains_table(member, table)
        }
        LogicalPlan::Derived { input, columns } => {
            columns
                .iter()
                .any(|column| column_table_key(column) == *table)
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
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => contains_table(input, table),
        LogicalPlan::Empty | LogicalPlan::OneRow => false,
    }
}

/// Turns a filtered Cartesian product into joins.
///
/// `FROM a, b WHERE a.id = b.a_id` is the SQL-89 join, and it is what TPC-H
/// and a great deal of generated and legacy SQL are written in. Bound
/// literally it is a cross join under a filter, which for six tables is an
/// estimate in the quadrillions: a Cartesian product that runs until the
/// query's memory ceiling or time limit stops it.
///
/// The conversion is the standard one: a conjunct that references tables from
/// exactly two sides, one of them already in the tree, becomes that join's
/// condition. Conjuncts that reference one table stay behind for predicate
/// pushdown, and anything left over stays in the filter, so the rule only ever
/// moves predicates it can attribute and never drops one.
///
/// Inputs that nothing links to remain a Cartesian product with the rest,
/// which is the honest outcome: the query really did ask for one.
fn infer_joins_from_filter(input: LogicalPlan, predicate: BoundExpr) -> LogicalPlan {
    infer_joins(input, predicate, false)
}

/// [`infer_joins_from_filter`], with `filtered` choosing whether a scan's
/// cost hint counts the predicates it carries (see [`filtered_scan_rows`]).
fn infer_joins(input: LogicalPlan, predicate: BoundExpr, filtered: bool) -> LogicalPlan {
    let LogicalPlan::CrossJoin { inputs } = input else {
        return LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        };
    };
    let mut components = inputs;
    let mut conjuncts = split_conjunction(predicate);
    // Merge whichever pair a conjunct links, repeatedly, rather than growing one
    // tree from the first input. Growing from the first input abandons the whole
    // rewrite the moment that input links to nothing - a one-row `config` table
    // crossed with a linked fact and dimension left the entire product intact.
    while let Some((left, right)) = find_linked_pair(&components, &conjuncts, filtered) {
        // Removed high-index-first so the low index stays valid.
        let candidate = components.remove(right);
        let tree = components.remove(left);
        let mut condition: Option<BoundExpr> = None;
        let mut kept = Vec::with_capacity(conjuncts.len());
        for conjunct in conjuncts {
            if joins_two_sides(&conjunct, &tree, &candidate) {
                condition = Some(match condition {
                    None => conjunct,
                    Some(existing) => and_expr(existing, conjunct),
                });
            } else {
                kept.push(conjunct);
            }
        }
        conjuncts = kept;
        // The RIGHT side is the one hashed into memory; the left one probes
        // it. Cross-join reordering sorts inputs smallest-first, so merging in
        // index order put the smaller relation on the left and hashed the
        // larger - joining a five-row dimension to a twenty-million-row fact
        // built the fact and could spill it. An unknown estimate is treated as
        // large, so it is probed rather than built.
        //
        // A reordered inner join run puts the side holding a relation pinned
        // by its key on the left instead: it is the small one, and a small
        // probe side is read first and filters the build side by its keys,
        // which the build side's scan applies before decoding the rest of
        // each row. A small build side only bounds the probe scan's range.
        //
        // Statistics extend that to a side they show is small - at most the
        // rows a probe side is read ahead for, and a quarter of the other
        // side or less - when neither side is pinned.
        let pinned_side = (filtered && has_pinned_key(&tree) != has_pinned_key(&candidate))
            .then(|| has_pinned_key(&candidate))
            .or_else(|| {
                if !filtered || has_pinned_key(&tree) || has_pinned_key(&candidate) {
                    return None;
                }
                let tree_rows = crate::estimate::expected_rows(&tree)?;
                let candidate_rows = crate::estimate::expected_rows(&candidate)?;
                let small = |rows: u64, other: u64| {
                    rows <= crate::execution::PROBE_PREFETCH_ROWS
                        && rows.saturating_mul(crate::execution::PROBE_PREFETCH_BUILD_RATIO)
                            <= other
                };
                if small(candidate_rows, tree_rows) {
                    Some(true)
                } else if small(tree_rows, candidate_rows) {
                    Some(false)
                } else {
                    None
                }
            });
        let (left, right) = if let Some(candidate_pinned) = pinned_side {
            if candidate_pinned {
                (candidate, tree)
            } else {
                (tree, candidate)
            }
        } else if JoinCost::for_plan(&candidate, filtered).rows
            <= JoinCost::for_plan(&tree, filtered).rows
        {
            (tree, candidate)
        } else {
            (candidate, tree)
        };
        components.push(LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind: pintail_sql::BoundJoinKind::Inner,
            condition,
        });
    }

    let mut plan = match components.len() {
        0 => LogicalPlan::OneRow,
        1 => components.pop().unwrap_or(LogicalPlan::OneRow),
        // Whatever nothing linked stays a product, because the query asked for
        // one.
        _ => LogicalPlan::CrossJoin { inputs: components },
    };
    if let Some(predicate) = conjuncts.into_iter().reduce(and_expr) {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate,
        };
    }
    plan
}

/// Cost hints are deliberately separate from `LogicalPlan::estimated_rows`:
/// the latter is a conservative bound used by execution guards. These guesses
/// choose an order only; they never remove a predicate, row, or runtime check.
struct JoinCost {
    rows: u64,
    /// Rows before the plan's own predicates, so a join to a key can keep
    /// the share of the key side that survives them.
    base: u64,
    keys: Vec<BTreeSet<ColumnKey>>,
}

impl JoinCost {
    fn for_plan(plan: &LogicalPlan, filtered: bool) -> Self {
        match plan {
            LogicalPlan::Scan(scan) => {
                let key: BTreeSet<_> = scan
                    .table
                    .columns
                    .iter()
                    .filter(|column| scan.table.key_column_ids.contains(&column.column_id))
                    .map(column_key)
                    .collect();
                let rows = scan.estimated_rows().unwrap_or(u64::MAX);
                Self {
                    rows: if filtered {
                        crate::estimate::scan_rows(scan)
                            .unwrap_or_else(|| filtered_scan_rows(scan, rows))
                    } else {
                        rows
                    },
                    base: rows,
                    keys: if key.is_empty() {
                        Vec::new()
                    } else {
                        vec![key]
                    },
                }
            }
            LogicalPlan::Filter { input, .. } | LogicalPlan::Sort { input, .. } => {
                Self::for_plan(input, filtered)
            }
            // A reordered run's relation is often a scan under the
            // projection that trims its columns; it has the scan's rows.
            LogicalPlan::Project { input, .. } if filtered => Self::for_plan(input, filtered),
            LogicalPlan::Join {
                left,
                right,
                kind: BoundJoinKind::Inner,
                condition: Some(condition),
            } => Self::joined(
                &Self::for_plan(left, filtered),
                &Self::for_plan(right, filtered),
                &conjuncts_of(condition),
            ),
            _ => {
                let rows = plan.estimated_rows().unwrap_or(u64::MAX);
                Self {
                    rows,
                    base: rows,
                    keys: Vec::new(),
                }
            }
        }
    }

    fn joined(left: &Self, right: &Self, conditions: &[&BoundExpr]) -> Self {
        let columns: BTreeSet<_> = conditions
            .iter()
            .flat_map(|expr| join_equalities(expr))
            .flat_map(|(left, right)| [column_key(&left), column_key(&right)])
            .collect();
        let left_key = left.keys.iter().any(|key| key.is_subset(&columns));
        let right_key = right.keys.iter().any(|key| key.is_subset(&columns));
        // A row joined to a key side finds its match only if that key's row
        // survived the key side's own predicates, so it keeps their share.
        let surviving = |rows: u64, key_side: &Self| {
            u64::try_from(
                u128::from(rows) * u128::from(key_side.rows.min(key_side.base))
                    / u128::from(key_side.base.max(1)),
            )
            .unwrap_or(u64::MAX)
        };
        let rows = match (left_key, right_key) {
            (true, true) => left.rows.min(right.rows),
            (true, false) => surviving(right.rows, left),
            (false, true) => surviving(left.rows, right),
            (false, false) => left.rows.saturating_mul(right.rows),
        };
        // Joining to a key preserves the other side's key as a cost hint.
        // Coercions, collations and missing statistics can make these estimates
        // inaccurate; execution still compares all keys and retains duplicates.
        let mut keys = Vec::new();
        if right_key {
            keys.extend(left.keys.iter().cloned());
        }
        if left_key {
            keys.extend(right.keys.iter().cloned());
        }
        // The keys carried on are the other side's, and so is the count
        // their surviving share is measured against: a filtered relation
        // joined through a key still marks its partner's key as filtered.
        let base = match (left_key, right_key) {
            (true, true) => left.base.min(right.base),
            (true, false) => right.base,
            (false, true) => left.base,
            (false, false) => rows,
        }
        .max(rows);
        Self { rows, base, keys }
    }
}

/// A scan's rows as a join-order hint once its own predicates apply: a
/// whole single-column primary key equal to a constant, or in a list of
/// them, names at most that many rows; every other equality or list over a
/// constant is taken to keep a tenth. Deterministic and cheap, and used only
/// to order joins, never to bound anything.
fn filtered_scan_rows(scan: &Scan, rows: u64) -> u64 {
    if let Some(pinned) = pinned_key_rows(scan) {
        return rows.min(pinned);
    }
    let selective = scan
        .predicates
        .iter()
        .filter(|predicate| constant_membership(predicate).is_some())
        .count();
    (0..selective).fold(rows, |rows, _| (rows / 10).max(1))
}

/// Constants a scan's predicates pin its whole single-column primary key
/// to, when one of them does.
fn pinned_key_rows(scan: &Scan) -> Option<u64> {
    let [key] = scan.table.key_column_ids.as_slice() else {
        return None;
    };
    scan.predicates
        .iter()
        .filter_map(|predicate| {
            let (column, listed) = constant_membership(predicate)?;
            (column.column_id == *key
                && column.table_id == scan.table.table_id
                && column.database_id == scan.table.database_id)
                .then_some(listed)
        })
        .min()
}

/// `column = constant` or `column IN (constants)`: the column and how many
/// constants it is compared with.
fn constant_membership(predicate: &BoundExpr) -> Option<(&BoundColumn, u64)> {
    fn column(expr: &BoundExpr) -> Option<&BoundColumn> {
        match &expr.kind {
            BoundExprKind::Column(column) if !column.outer => Some(column),
            _ => None,
        }
    }
    let constant = |expr: &BoundExpr| matches!(expr.kind, BoundExprKind::Literal(_));
    match &predicate.kind {
        BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } => match (column(left), column(right)) {
            (Some(column), None) if constant(right) => Some((column, 1)),
            (None, Some(column)) if constant(left) => Some((column, 1)),
            _ => None,
        },
        BoundExprKind::Scalar {
            function: pintail_sql::ScalarFunction::InList { negated: false },
            args,
        } if args.len() > 1 && args[1..].iter().all(constant) => {
            Some((column(&args[0])?, u64::try_from(args.len() - 1).ok()?))
        }
        _ => None,
    }
}

/// Whether a scan in `plan`, beneath filters, projections and inner joins,
/// is pinned by its whole primary key (see [`pinned_key_rows`]).
fn has_pinned_key(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Scan(scan) => pinned_key_rows(scan).is_some(),
        LogicalPlan::Filter { input, .. } | LogicalPlan::Project { input, .. } => {
            has_pinned_key(input)
        }
        LogicalPlan::Join {
            left,
            right,
            kind: BoundJoinKind::Inner,
            ..
        } => has_pinned_key(left) || has_pinned_key(right),
        _ => false,
    }
}

/// Reorders a run of inner joins written as `a JOIN b ON .. JOIN c ON ..`
/// when a later relation is pinned by its whole primary key and the first
/// is not. Written order joins every relation to the first before the
/// pinned one is reached, so a filter keeping one row of a small table ran
/// after the large one had been joined whole. The run is taken apart into
/// its relations and ON conjuncts and joined again as a filtered product
/// is, with each scan's cost hint counting its own predicates, so the
/// pinned relation and what it links to are joined first. A run the rebuild
/// cannot link completely is left as written.
fn reorder_inner_join_run(plan: LogicalPlan) -> LogicalPlan {
    fn flatten(
        plan: &LogicalPlan,
        relations: &mut Vec<LogicalPlan>,
        conjuncts: &mut Vec<BoundExpr>,
    ) {
        if let LogicalPlan::Join {
            left,
            right,
            kind: BoundJoinKind::Inner,
            condition: Some(condition),
        } = plan
            && !expression_contains_subquery(condition)
        {
            flatten(left, relations, conjuncts);
            flatten(right, relations, conjuncts);
            conjuncts.extend(split_conjunction(condition.clone()));
            return;
        }
        relations.push(plan.clone());
    }
    let mut relations = Vec::new();
    let mut conjuncts = Vec::new();
    flatten(&plan, &mut relations, &mut conjuncts);
    if relations.len() < 3 {
        return plan;
    }
    let pinned = !has_pinned_key(&relations[0]) && relations[1..].iter().any(has_pinned_key);
    // Without a pinned relation, a run is rebuilt only when every relation's
    // statistics are known and the rebuilt order is estimated to do well
    // under half the written order's work.
    let informed = !pinned
        && relations
            .iter()
            .all(|relation| crate::estimate::expected_rows(relation).is_some());
    if relations.len() < 3 || !(pinned || informed) || conjuncts.iter().any(is_volatile) {
        return plan;
    }
    let Some(predicate) = conjuncts.into_iter().reduce(and_expr) else {
        return plan;
    };
    let rebuilt = infer_joins(
        LogicalPlan::CrossJoin { inputs: relations },
        predicate,
        true,
    );
    let complete = match &rebuilt {
        LogicalPlan::Filter { input, .. } => {
            !matches!(input.as_ref(), LogicalPlan::CrossJoin { .. })
        }
        other => !matches!(other, LogicalPlan::CrossJoin { .. }),
    };
    if !complete {
        return plan;
    }
    if informed {
        let (Some(written), Some(reordered)) = (join_work(&plan), join_work(&rebuilt)) else {
            return plan;
        };
        if reordered.saturating_mul(2) >= written {
            return plan;
        }
    }
    rebuilt
}

/// Estimated work of an inner join tree: the rows every join in it
/// produces, with scans counting their own predicates. Each relation is
/// read once whatever the order, so what an order changes is the size of
/// the results in between.
fn join_work(plan: &LogicalPlan) -> Option<u128> {
    match plan {
        LogicalPlan::Join {
            left,
            right,
            kind: BoundJoinKind::Inner,
            condition: Some(condition),
        } => {
            let left_cost = JoinCost::for_plan(left, true);
            let right_cost = JoinCost::for_plan(right, true);
            let joined = JoinCost::joined(&left_cost, &right_cost, &conjuncts_of(condition));
            Some(join_work(left)? + join_work(right)? + u128::from(joined.rows))
        }
        LogicalPlan::Filter { input, .. } | LogicalPlan::Project { input, .. } => join_work(input),
        LogicalPlan::Join { .. } | LogicalPlan::CrossJoin { .. } => None,
        _ => Some(0),
    }
}

/// Choose the connected pair with the smallest estimated intermediate work.
/// Source order breaks ties. In a cyclic graph, the first linked pair can be
/// two dimensions sharing a non-unique attribute, expanding millions of rows
/// before either fact-table key is applied.
fn find_linked_pair(
    components: &[LogicalPlan],
    conjuncts: &[BoundExpr],
    filtered: bool,
) -> Option<(usize, usize)> {
    let estimates: Vec<_> = components
        .iter()
        .map(|component| JoinCost::for_plan(component, filtered))
        .collect();
    let mut best = None;
    for left in 0..components.len() {
        for right in (left + 1)..components.len() {
            let conditions: Vec<_> = conjuncts
                .iter()
                .filter(|conjunct| joins_two_sides(conjunct, &components[left], &components[right]))
                .collect();
            if conditions.is_empty() {
                continue;
            }
            let joined = JoinCost::joined(&estimates[left], &estimates[right], &conditions);
            let work = u128::from(joined.rows)
                + u128::from(estimates[left].rows)
                + u128::from(estimates[right].rows);
            if best.is_none_or(|(cost, _, _)| work < cost) {
                best = Some((work, left, right));
            }
        }
    }
    best.map(|(_, left, right)| (left, right))
}

/// Whether a conjunct is usable as the join condition between two sides.
///
/// Three things must hold, and each was learned from a way this went wrong:
///
/// - It must be an EQUALITY. The physical join builds hash keys from equalities;
///   handing it `a.x < b.x` turns a query that executed as a filtered product
///   into `UnsupportedJoinCondition`.
/// - Each operand must reference exactly one side. `a.x = b.y + c.z` names three
///   relations and cannot be a key for this pair.
/// - Neither operand may be volatile. A filter evaluates `RAND()` once per
///   candidate PAIR while a hash join evaluates it once per input ROW, so
///   `a.x = b.x + ROUND(RAND())` would quietly change how many rows come back.
fn joins_two_sides(conjunct: &BoundExpr, left: &LogicalPlan, right: &LogicalPlan) -> bool {
    let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left: lhs,
        right: rhs,
    } = &conjunct.kind
    else {
        return false;
    };
    if is_volatile(lhs) || is_volatile(rhs) {
        return false;
    }
    let sides = |expr: &BoundExpr| {
        let tables = referenced_tables(expr);
        if tables.is_empty() {
            return None;
        }
        let in_left = tables.iter().all(|table| contains_table(left, table));
        let in_right = tables.iter().all(|table| contains_table(right, table));
        match (in_left, in_right) {
            (true, false) => Some(false),
            (false, true) => Some(true),
            _ => None,
        }
    };
    matches!((sides(lhs), sides(rhs)), (Some(a), Some(b)) if a != b)
}

/// Whether an expression's value can differ between two evaluations of the same
/// row, which makes it unsafe to move between a filter and a join key.
pub(crate) fn is_volatile(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Scalar { function, args } => {
            matches!(
                function,
                ScalarFunction::Rand
                    | ScalarFunction::Uuid
                    | ScalarFunction::UuidShort
                    | ScalarFunction::UserVariableRead
                    | ScalarFunction::UserVariableAssign
            ) || args.iter().any(is_volatile)
        }
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => is_volatile(expr),
        BoundExprKind::Binary { left, right, .. } => is_volatile(left) || is_volatile(right),
        // Anything whose shape is not walked here is treated as volatile, so a
        // new expression kind fails closed rather than silently becoming a join
        // key.
        BoundExprKind::Column(_) | BoundExprKind::Literal(_) => false,
        _ => true,
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
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
            anchor: Box::new(reorder_cross_joins(*anchor)),
            member: Box::new(reorder_cross_joins(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => reorder_join(*left, *right, kind, condition),
        LogicalPlan::Filter { input, predicate } => {
            infer_joins_from_filter(reorder_cross_joins(*input), predicate)
        }
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
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            key_collations,
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

/// A join with its inputs reordered, and an inner join run beginning at
/// it reordered as [`reorder_inner_join_run`] decides.
fn reorder_join(
    left: LogicalPlan,
    right: LogicalPlan,
    kind: BoundJoinKind,
    condition: Option<BoundExpr>,
) -> LogicalPlan {
    let join = LogicalPlan::Join {
        left: Box::new(reorder_cross_joins(left)),
        right: Box::new(reorder_cross_joins(right)),
        kind,
        condition,
    };
    if kind == BoundJoinKind::Inner {
        reorder_inner_join_run(join)
    } else {
        join
    }
}

fn prune_projections(plan: &mut LogicalPlan) {
    let mut required = BTreeSet::new();
    collect_plan_columns(plan, &mut required, true);
    // Attribution needs the relation instance; a projection does not. Two
    // aliases of one table are two reads of one file, and the scan under
    // each is planned from the same column set, so the relation is dropped
    // here rather than narrowing one alias to a width the other does not
    // share.
    let required: BTreeSet<ScanColumnKey> = required
        .into_iter()
        .map(|(database, table, _, column)| (database, table, column))
        .collect();
    prune_scan_columns(plan, &required);
    let mut payload = BTreeSet::new();
    collect_plan_columns(plan, &mut payload, false);
    trim_predicate_payloads(plan, &payload, false);
}

/// A join need not carry a scan-only predicate column after filtering it.
/// Keep the storage projection intact; drop the column at the scan boundary.
fn trim_predicate_payloads(plan: &mut LogicalPlan, required: &BTreeSet<ColumnKey>, in_join: bool) {
    match plan {
        LogicalPlan::Scan(scan) if in_join && !scan.predicates.is_empty() => {
            let expressions: Vec<_> = scan
                .table
                .columns
                .iter()
                .filter(|column| {
                    scan.projected_column_ids.contains(&column.column_id)
                        && required.contains(&column_key(column))
                })
                .map(|column| BoundProjection {
                    name: column.name.clone(),
                    expr: BoundExpr {
                        kind: BoundExprKind::Column(column.clone()),
                        data_type: Some(column.data_type),
                        nullable: column.nullable,
                    },
                })
                .collect();
            if !expressions.is_empty() && expressions.len() < scan.projected_column_ids.len() {
                let input = std::mem::replace(plan, LogicalPlan::Empty);
                *plan = LogicalPlan::Project {
                    input: Box::new(input),
                    expressions,
                };
            }
        }
        LogicalPlan::Join { left, right, .. } => {
            trim_predicate_payloads(left, required, true);
            trim_predicate_payloads(right, required, true);
        }
        LogicalPlan::Filter { input, .. } => trim_predicate_payloads(input, required, in_join),
        LogicalPlan::Derived { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => trim_predicate_payloads(input, required, false),
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                trim_predicate_payloads(input, required, false);
            }
        }
        LogicalPlan::SetOp { left, right, .. } => {
            trim_predicate_payloads(left, required, false);
            trim_predicate_payloads(right, required, false);
        }
        LogicalPlan::Recursive { anchor, member, .. } => {
            trim_predicate_payloads(anchor, required, false);
            trim_predicate_payloads(member, required, false);
        }
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => {}
    }
}

fn collect_plan_columns(
    plan: &LogicalPlan,
    required: &mut BTreeSet<ColumnKey>,
    scan_predicates: bool,
) {
    collect_columns(plan, required, scan_predicates, false);
}

/// [`collect_plan_columns`]; `readers_only` leaves out what a derived table's
/// select list passes through unchanged under its own name. A nested join
/// bound as a derived table keeps its tables' names, so each of its columns
/// is produced by a reference to the very column it names, which is no
/// reader of it. Only the derived-column trim may ask this: the scan pruning
/// must still see every column a kept select list reads.
#[allow(clippy::too_many_lines)] // exhaustive plan-node recursion reads best unsplit
fn collect_columns(
    plan: &LogicalPlan,
    required: &mut BTreeSet<ColumnKey>,
    scan_predicates: bool,
    readers_only: bool,
) {
    match plan {
        LogicalPlan::Recursive { anchor, member, .. } => {
            collect_columns(anchor, required, scan_predicates, readers_only);
            collect_columns(member, required, scan_predicates, readers_only);
        }
        LogicalPlan::Scan(scan) => {
            for predicate in scan.predicates.iter().filter(|_| scan_predicates) {
                collect_expr_columns(predicate, required);
            }
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                collect_columns(input, required, scan_predicates, readers_only);
            }
        }
        LogicalPlan::SetOp { left, right, .. } => {
            collect_columns(left, required, scan_predicates, readers_only);
            collect_columns(right, required, scan_predicates, readers_only);
        }
        LogicalPlan::Join {
            left,
            right,
            condition,
            ..
        } => {
            collect_columns(left, required, scan_predicates, readers_only);
            collect_columns(right, required, scan_predicates, readers_only);
            if let Some(condition) = condition {
                collect_expr_columns(condition, required);
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_expr_columns(predicate, required);
            collect_columns(input, required, scan_predicates, readers_only);
        }
        LogicalPlan::Window { input, windows, .. } => {
            for window in windows {
                // Every window function's value expression has to be
                // collected, or projection pushdown prunes the column it
                // reads and binding fails with MissingColumn.
                match &window.function {
                    pintail_sql::WindowFunction::Aggregate(aggregate) => {
                        if let Some(expr) = &aggregate.expr {
                            collect_expr_columns(expr, required);
                        }
                    }
                    pintail_sql::WindowFunction::Offset { expr, default, .. } => {
                        collect_expr_columns(expr, required);
                        if let Some(default) = default {
                            collect_expr_columns(default, required);
                        }
                    }
                    pintail_sql::WindowFunction::Extreme { expr, .. } => {
                        collect_expr_columns(expr, required);
                    }
                    pintail_sql::WindowFunction::RowNumber
                    | pintail_sql::WindowFunction::Rank
                    | pintail_sql::WindowFunction::DenseRank
                    | pintail_sql::WindowFunction::NTile(_) => {}
                }
                for expr in &window.partition_by {
                    collect_expr_columns(expr, required);
                }
                for key in &window.order_by {
                    collect_expr_columns(&key.expr, required);
                }
            }
            collect_columns(input, required, scan_predicates, readers_only);
        }
        LogicalPlan::Project { input, expressions } => {
            for expression in expressions {
                collect_expr_columns(&expression.expr, required);
            }
            collect_columns(input, required, scan_predicates, readers_only);
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
            collect_columns(input, required, scan_predicates, readers_only);
        }
        LogicalPlan::Derived { input, columns } => match input.as_ref() {
            LogicalPlan::Project {
                input: source,
                expressions,
            } if readers_only && expressions.len() == columns.len() => {
                for (column, expression) in columns.iter().zip(expressions) {
                    if !matches!(
                        &expression.expr.kind,
                        BoundExprKind::Column(passed) if column_key(passed) == column_key(column)
                    ) {
                        collect_expr_columns(&expression.expr, required);
                    }
                }
                collect_columns(source, required, scan_predicates, readers_only);
            }
            _ => collect_columns(input, required, scan_predicates, readers_only),
        },
        LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => {
            collect_columns(input, required, scan_predicates, readers_only);
        }
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn prune_scan_columns(plan: &mut LogicalPlan, required: &BTreeSet<ScanColumnKey>) {
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
                .filter(|column| {
                    required.contains(&(column.database_id, column.table_id, column.column_id))
                })
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
        | LogicalPlan::Distinct { input, .. }
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
        | LogicalPlan::Distinct { input, .. }
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
        .map(|column| (column.0, column.1, column.2))
        .collect()
}

fn collect_expr_columns(expr: &BoundExpr, columns: &mut BTreeSet<ColumnKey>) {
    match &expr.kind {
        BoundExprKind::Column(column) => {
            columns.insert(column_key(column));
        }
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
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
        BoundExprKind::InSubquery { expr, query, .. } => {
            collect_expr_columns(expr, columns);
            collect_bound_query_columns(query, columns);
        }
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            collect_bound_query_columns(query, columns);
        }
        BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => {}
    }
}

fn expression_contains_subquery(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::InSubquery { .. } => true,
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => expression_contains_subquery(expr),
        BoundExprKind::Binary { left, right, .. } => {
            expression_contains_subquery(left) || expression_contains_subquery(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(expression_contains_subquery),
        BoundExprKind::Column(_)
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => false,
    }
}

fn collect_bound_query_columns(query: &BoundQuery, columns: &mut BTreeSet<ColumnKey>) {
    for projection in &query.projection {
        collect_expr_columns(&projection.expr, columns);
    }
    if let Some(filter) = &query.filter {
        collect_expr_columns(filter, columns);
    }
    for expression in &query.group_by {
        collect_expr_columns(expression, columns);
    }
    for aggregate in &query.aggregates {
        if let Some(expression) = &aggregate.expr {
            collect_expr_columns(expression, columns);
        }
        for (expression, _) in &aggregate.order_within {
            collect_expr_columns(expression, columns);
        }
    }
    for window in &query.windows {
        match &window.function {
            WindowFunction::Aggregate(aggregate) => {
                if let Some(expression) = &aggregate.expr {
                    collect_expr_columns(expression, columns);
                }
            }
            WindowFunction::Offset { expr, default, .. } => {
                collect_expr_columns(expr, columns);
                if let Some(default) = default {
                    collect_expr_columns(default, columns);
                }
            }
            WindowFunction::Extreme { expr, .. } => collect_expr_columns(expr, columns),
            WindowFunction::RowNumber
            | WindowFunction::Rank
            | WindowFunction::DenseRank
            | WindowFunction::NTile(_) => {}
        }
        for expression in &window.partition_by {
            collect_expr_columns(expression, columns);
        }
        for key in &window.order_by {
            collect_expr_columns(&key.expr, columns);
        }
    }
    if let Some(having) = &query.having {
        collect_expr_columns(having, columns);
    }
    for source in &query.from {
        if let Some(input) = &source.base.input {
            collect_bound_query_columns(input, columns);
        }
        for join in &source.joins {
            if let Some(input) = &join.table.input {
                collect_bound_query_columns(input, columns);
            }
            if let Some(condition) = &join.condition {
                collect_expr_columns(condition, columns);
            }
        }
    }
    for branch in &query.union_all {
        collect_bound_query_columns(branch, columns);
    }
    for (_, right) in &query.set_ops {
        collect_bound_query_columns(right, columns);
    }
    if let Some(recursive) = &query.recursive {
        collect_bound_query_columns(&recursive.member, columns);
    }
}

/// Identifies one relation instance, not one table.
///
/// A self-join puts two instances of the same physical table in one query,
/// and every rule that attributes a predicate, a derived constant or a
/// column reference has to tell them apart. The query-visible relation name
/// is what separates them: SQL requires the aliases in a `FROM` clause to be
/// unique, so within the scope any of these rules walks, the name is the
/// instance. Blocks nested below reuse names freely, which only ever makes a
/// rule refuse - `push_conjunct`'s scan arm is an exact match, so a name that
/// resolves to the wrong subtree finds nothing to attach to.
type TableKey = (DatabaseId, TableId, String);
type ColumnKey = (DatabaseId, TableId, String, u32);
/// A scan's projection, which relation instances share.
type ScanColumnKey = (DatabaseId, TableId, u32);

/// Carries a literal across a join equality so the other side can prune.
///
/// `WHERE a.x = 8 ... LEFT JOIN b ON b.x = a.x` says nothing to `b`'s scan,
/// which then reads every segment and discards almost all of it: a column
/// compared to another column yields no bound, so storage has nothing to
/// prune on. The same query with `b.x = 8` written out prunes normally. An
/// operational report joining ten tables on ids is entirely made of this
/// shape, and measured as eleven full scans to answer a question about a
/// hundred rows.
///
/// Only equal types and equal collations propagate: comparison semantics
/// must be identical on both sides or the derived predicate is not the same
/// question. Direction is bounded by join kind - see [`propagates_into`].
#[allow(clippy::too_many_lines)] // structural walk, one arm per plan node
fn propagate_join_constants(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(propagate_join_constants(*input)),
            columns,
        },
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs.into_iter().map(propagate_join_constants).collect(),
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs.into_iter().map(propagate_join_constants).collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(propagate_join_constants(*left)),
            right: Box::new(propagate_join_constants(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            limit,
            anchor: Box::new(propagate_join_constants(*anchor)),
            member: Box::new(propagate_join_constants(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => {
            let mut left = Box::new(propagate_join_constants(*left));
            let mut right = Box::new(propagate_join_constants(*right));
            if let Some(predicate) = &condition {
                let (into_left, into_right) = propagates_into(kind);
                let mut left_constants = BTreeMap::new();
                collect_constants(&left, &mut left_constants);
                let mut right_constants = BTreeMap::new();
                collect_constants(&right, &mut right_constants);
                for (first, second) in join_equalities(predicate) {
                    if into_right && let Some(value) = left_constants.get(&column_key(&first)) {
                        derive_into(&mut right, &second, &first, value);
                    }
                    if into_right && let Some(value) = left_constants.get(&column_key(&second)) {
                        derive_into(&mut right, &first, &second, value);
                    }
                    if into_left && let Some(value) = right_constants.get(&column_key(&first)) {
                        derive_into(&mut left, &second, &first, value);
                    }
                    if into_left && let Some(value) = right_constants.get(&column_key(&second)) {
                        derive_into(&mut left, &first, &second, value);
                    }
                }
            }
            LogicalPlan::Join {
                left,
                right,
                kind,
                condition,
            }
        }
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(propagate_join_constants(*input)),
            predicate,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(propagate_join_constants(*input)),
            group_by,
            aggregates,
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(propagate_join_constants(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(propagate_join_constants(*input)),
            expressions,
        },
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            input: Box::new(propagate_join_constants(*input)),
            key_collations,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            input: Box::new(propagate_join_constants(*input)),
            keys,
            trim,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(propagate_join_constants(*input)),
            limit,
        },
    }
}

/// Which sides of a join may receive a constant derived from the other.
///
/// INNER constrains both inputs equally, so a constant travels either way.
/// LEFT preserves its left input: a right row whose key cannot match is
/// dropped by the join anyway, so filtering the right early changes nothing,
/// while filtering the LEFT would delete rows the join promised to keep and
/// null-extend. ANTI is the reverse trap - removing right rows creates
/// matches that were not there - and SCALAR counts its matches, so neither
/// takes a derived filter. SEMI is safe in principle and excluded for now
/// because nothing measured needs it.
const fn propagates_into(kind: BoundJoinKind) -> (bool, bool) {
    match kind {
        BoundJoinKind::Inner => (true, true),
        BoundJoinKind::Left => (false, true),
        BoundJoinKind::Scalar
        | BoundJoinKind::Semi
        | BoundJoinKind::Anti
        | BoundJoinKind::Cross => (false, false),
    }
}

/// Equality pairs of plain columns in a conjunction, ignoring everything else.
fn join_equalities(predicate: &BoundExpr) -> Vec<(BoundColumn, BoundColumn)> {
    let mut pairs = Vec::new();
    let mut pending = vec![predicate];
    while let Some(expr) = pending.pop() {
        match &expr.kind {
            BoundExprKind::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                pending.push(left);
                pending.push(right);
            }
            BoundExprKind::Binary {
                op: BinaryOp::Equal,
                left,
                right,
            } => {
                if let (BoundExprKind::Column(first), BoundExprKind::Column(second)) =
                    (&left.kind, &right.kind)
                {
                    pairs.push((first.clone(), second.clone()));
                }
            }
            _ => {}
        }
    }
    pairs
}

/// Column-equals-literal facts already resting on the scans of a subtree.
fn collect_constants(plan: &LogicalPlan, out: &mut BTreeMap<ColumnKey, Value>) {
    let mut record = |predicate: &BoundExpr| {
        for conjunct in conjuncts_of(predicate) {
            let BoundExprKind::Binary {
                op: BinaryOp::Equal,
                left,
                right,
            } = &conjunct.kind
            else {
                continue;
            };
            let ((BoundExprKind::Column(column), BoundExprKind::Literal(value))
            | (BoundExprKind::Literal(value), BoundExprKind::Column(column))) =
                (&left.kind, &right.kind)
            else {
                continue;
            };
            // A NULL literal never equals anything, so it establishes no fact
            // worth carrying: the row is already gone.
            if !matches!(value, Value::Null) {
                out.insert(column_key(column), value.clone());
            }
        }
    };
    match plan {
        LogicalPlan::Scan(scan) => {
            for predicate in &scan.predicates {
                record(predicate);
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            record(predicate);
            collect_constants(input, out);
        }
        LogicalPlan::Join {
            left, right, kind, ..
        } => {
            // Only sides that survive unconditionally establish facts: the
            // null-supplying side of a LEFT join contributes NULLs instead.
            collect_constants(left, out);
            if matches!(kind, BoundJoinKind::Inner) {
                collect_constants(right, out);
            }
        }
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Derived { input, .. } => collect_constants(input, out),
        _ => {}
    }
}

fn conjuncts_of(predicate: &BoundExpr) -> Vec<&BoundExpr> {
    let mut found = Vec::new();
    let mut pending = vec![predicate];
    while let Some(expr) = pending.pop() {
        if let BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } = &expr.kind
        {
            pending.push(left);
            pending.push(right);
        } else {
            found.push(expr);
        }
    }
    found
}

/// Adds `target = value` to the scan of `target`'s table, when the two
/// columns really do compare by the same rules.
fn derive_into(plan: &mut LogicalPlan, target: &BoundColumn, source: &BoundColumn, value: &Value) {
    if target.data_type != source.data_type || target.collation != source.collation {
        return;
    }
    // An ENUM compares by its declared ordinal, so a literal that is merely
    // equal as text to one side is not the same predicate on the other.
    if target.enum_labels.is_some() || source.enum_labels.is_some() {
        return;
    }
    let predicate = BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: target.nullable,
        kind: BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: Box::new(BoundExpr {
                data_type: Some(target.data_type),
                nullable: target.nullable,
                kind: BoundExprKind::Column(target.clone()),
            }),
            right: Box::new(literal_expr(value.clone())),
        },
    };
    let table = column_table_key(target);
    // The key names one relation instance, so a self-join's two scans are
    // distinguishable and the constant reaches the side it was derived for.
    // More than one scan still answering to that key means the name was
    // reused by a nested block; the fact holds for only one of them, so the
    // subtree is left alone.
    if count_scans(plan, &table) != 1 {
        return;
    }
    add_scan_predicate(plan, &table, &predicate);
}

/// Scans of one table that [`add_scan_predicate`] could reach in a subtree.
fn count_scans(plan: &LogicalPlan, table: &TableKey) -> usize {
    match plan {
        LogicalPlan::Scan(scan) => usize::from(&table_key(&scan.table) == table),
        LogicalPlan::Join { left, right, .. } => {
            count_scans(left, table) + count_scans(right, table)
        }
        LogicalPlan::Filter { input, .. } => count_scans(input, table),
        LogicalPlan::CrossJoin { inputs } => {
            inputs.iter().map(|input| count_scans(input, table)).sum()
        }
        _ => 0,
    }
}

fn add_scan_predicate(plan: &mut LogicalPlan, table: &TableKey, predicate: &BoundExpr) {
    match plan {
        LogicalPlan::Scan(scan) => {
            if &table_key(&scan.table) == table
                && !scan.predicates.iter().any(|existing| existing == predicate)
            {
                scan.predicates.push(predicate.clone());
            }
        }
        LogicalPlan::Join {
            left, right, kind, ..
        } => {
            add_scan_predicate(left, table, predicate);
            // Never reach through into the preserved side of a LEFT join
            // from outside: the caller's direction rule decided that.
            if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Left) {
                add_scan_predicate(right, table, predicate);
            }
        }
        // A filter above a scan is one more conjunct beside this one.
        LogicalPlan::Filter { input, .. } => add_scan_predicate(input, table, predicate),
        LogicalPlan::CrossJoin { inputs } => {
            for input in inputs {
                add_scan_predicate(input, table, predicate);
            }
        }
        // Everything else changes what a row means before the join sees it:
        // a LIMIT, a window, an aggregate, DISTINCT, a sort with a trim, a
        // derived table, a set operation. A predicate that is true of the
        // join's input is not necessarily true of the scan beneath one of
        // these, so the walk stops here, exactly where pushdown stops. A
        // derived table also carries its own synthetic table id, so a base
        // scan under it is never the one a join equality names.
        _ => {}
    }
}

fn table_key(table: &pintail_sql::BoundTable) -> TableKey {
    (
        table.database_id,
        table.table_id,
        table.relation_name.clone(),
    )
}

/// The relation instance a column reference resolves to.
fn column_table_key(column: &BoundColumn) -> TableKey {
    (
        column.database_id,
        column.table_id,
        column.relation_name.clone(),
    )
}

fn column_key(column: &BoundColumn) -> ColumnKey {
    (
        column.database_id,
        column.table_id,
        column.relation_name.clone(),
        column.column_id,
    )
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

    /// Every scan in the plan as (table name, predicate count).
    fn scans(plan: &LogicalPlan) -> Vec<(String, usize)> {
        fn walk(plan: &LogicalPlan, found: &mut Vec<(String, usize)>) {
            match plan {
                LogicalPlan::Scan(scan) => {
                    found.push((scan.table.table_name.clone(), scan.predicates.len()));
                }
                LogicalPlan::Join { left, right, .. } | LogicalPlan::SetOp { left, right, .. } => {
                    walk(left, found);
                    walk(right, found);
                }
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Window { input, .. }
                | LogicalPlan::Distinct { input, .. }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Limit { input, .. }
                | LogicalPlan::Derived { input, .. } => walk(input, found),
                LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
                    for input in inputs {
                        walk(input, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(plan, &mut found);
        found
    }

    fn relation_scans(plan: &LogicalPlan) -> Vec<(String, usize)> {
        fn walk(plan: &LogicalPlan, found: &mut Vec<(String, usize)>) {
            match plan {
                LogicalPlan::Scan(scan) => {
                    found.push((scan.table.relation_name.clone(), scan.predicates.len()));
                }
                LogicalPlan::Join { left, right, .. } | LogicalPlan::SetOp { left, right, .. } => {
                    walk(left, found);
                    walk(right, found);
                }
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Window { input, .. }
                | LogicalPlan::Distinct { input, .. }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Limit { input, .. }
                | LogicalPlan::Derived { input, .. } => walk(input, found),
                LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
                    for input in inputs {
                        walk(input, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(plan, &mut found);
        found
    }

    fn predicates_on_relation(plan: &LogicalPlan, relation: &str) -> usize {
        relation_scans(plan)
            .into_iter()
            .filter(|(name, _)| name == relation)
            .map(|(_, count)| count)
            .sum()
    }

    fn predicates_on(plan: &LogicalPlan, table: &str) -> usize {
        scans(plan)
            .into_iter()
            .filter(|(name, _)| name == table)
            .map(|(_, count)| count)
            .sum()
    }

    #[test]
    fn a_constant_crosses_an_inner_join_equality_in_both_directions() {
        // users learns `u.id = 7` from `e.id = 7`, so its scan can prune.
        let plan =
            optimized("SELECT e.name FROM events e JOIN users u ON u.id = e.id WHERE e.id = 7");
        assert_eq!(predicates_on(&plan, "events"), 1);
        assert_eq!(predicates_on(&plan, "users"), 1);
        // And the other way: the constant sits on the right this time.
        let plan =
            optimized("SELECT e.name FROM events e JOIN users u ON u.id = e.id WHERE u.id = 7");
        assert_eq!(predicates_on(&plan, "events"), 1);
        assert_eq!(predicates_on(&plan, "users"), 1);
    }

    #[test]
    fn a_left_join_carries_a_constant_only_into_its_null_supplying_side() {
        // Safe: a users row whose id is not 7 could never have matched, so
        // dropping it early cannot change which events rows survive.
        let plan = optimized(
            "SELECT e.name FROM events e LEFT JOIN users u ON u.id = e.id WHERE e.id = 7",
        );
        assert_eq!(predicates_on(&plan, "users"), 1);
        // Unsafe in reverse, and refused: filtering events on a fact that
        // holds only for matched rows would delete rows the LEFT join
        // promised to keep and null-extend.
        let plan =
            optimized("SELECT e.name FROM events e LEFT JOIN users u ON u.id = e.id AND u.id = 7");
        assert_eq!(predicates_on(&plan, "events"), 0);
    }

    #[test]
    fn a_semi_join_takes_no_derived_predicate() {
        let plan = optimized(
            "SELECT e.name FROM events e WHERE e.id = 7 \
             AND EXISTS (SELECT 1 FROM users u WHERE u.id = e.id)",
        );
        assert_eq!(predicates_on(&plan, "events"), 1);
        assert_eq!(predicates_on(&plan, "users"), 0);
    }

    #[test]
    fn a_scan_under_a_derived_limit_gains_nothing() {
        // `d.id = e.id` names the derived table, whose id is synthetic, so
        // the constant cannot be attributed to the users scan beneath the
        // LIMIT - and must not be: filtering before LIMIT 1 changes which
        // row the derived table yields.
        let plan = optimized(
            "SELECT e.name FROM events e \
             JOIN (SELECT id FROM users ORDER BY id LIMIT 1) d ON d.id = e.id WHERE e.id = 7",
        );
        assert_eq!(predicates_on(&plan, "events"), 1);
        assert_eq!(predicates_on(&plan, "users"), 0);
    }

    #[test]
    fn a_self_join_attributes_a_predicate_to_the_alias_it_names() {
        let plan =
            optimized("SELECT a.name FROM events a JOIN events b ON b.id = a.id WHERE a.id = 7");
        // The two instances of `events` are separate relations, so `a.id = 7`
        // reaches `a`'s scan, and the join equality then carries the constant
        // to `b` as it would between two different tables. Both scans prune.
        assert_eq!(predicates_on_relation(&plan, "a"), 1);
        assert_eq!(predicates_on_relation(&plan, "b"), 1);
    }

    #[test]
    fn a_self_join_keeps_each_alias_to_its_own_predicate() {
        // Only `a` is filtered, and `b.id = a.id` is not an equality the
        // constant rule can carry `a.name = 'x'` across, so `b` stays open.
        let plan =
            optimized("SELECT a.id FROM events a JOIN events b ON b.id = a.id WHERE a.name = 'x'");
        assert_eq!(predicates_on_relation(&plan, "a"), 1);
        assert_eq!(predicates_on_relation(&plan, "b"), 0);
    }

    #[test]
    fn an_alias_reused_by_a_nested_block_is_left_alone() {
        // The outer `a` and the derived table's own `a` answer to the same
        // relation name. Attributing the outer predicate would have to choose
        // between them, so nothing is pushed into either scan and the filter
        // stays where binding put it.
        let plan = optimized(
            "SELECT a.name FROM events a \
             JOIN (SELECT a.id FROM events a WHERE a.name = 'x') d ON d.id = a.id \
             WHERE a.id = 7",
        );
        assert_eq!(predicates_on(&plan, "events"), 1);
    }

    #[test]
    fn a_derived_predicate_never_duplicates_across_repeated_passes() {
        let plan = optimized(
            "SELECT e.name FROM events e JOIN users u ON u.id = e.id WHERE e.id = 7 AND u.id = 7",
        );
        assert_eq!(predicates_on(&plan, "users"), 1);
        assert_eq!(predicates_on(&plan, "events"), 1);
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
    fn prunes_the_columns_of_a_derived_table_nothing_reads() {
        let LogicalPlan::Derived { input, columns } = project_input(optimized(
            "SELECT d.id FROM (SELECT id, name FROM users) AS d",
        )) else {
            panic!("derived");
        };
        assert_eq!(columns.len(), 1);
        let LogicalPlan::Project { input, expressions } = *input else {
            panic!("derived projection");
        };
        assert_eq!(expressions.len(), 1);
        let LogicalPlan::Scan(scan) = *input else {
            panic!("scan");
        };
        assert_eq!(scan.projected_column_ids, [1]);
    }

    /// A derived table joined to a DISTINCT input materializes, but a plain
    /// column reference nothing reads still goes: reading it raises nothing.
    #[test]
    fn prunes_unread_column_references_of_a_derived_table_that_materializes() {
        let LogicalPlan::Derived { columns, .. } = project_input(optimized(
            "SELECT d.id FROM (SELECT u.id, u.name, UPPER(u.name) AS shout FROM users u \
             JOIN (SELECT DISTINCT id FROM events) e ON e.id = u.id) AS d",
        )) else {
            panic!("derived");
        };
        assert_eq!(
            columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "shout"]
        );
    }

    /// A LEFT join whose ON holds a correlated IN widens its right input
    /// into a nested join bound as a derived table under the right table's
    /// own name. Its columns pass through unchanged, and one nothing reads
    /// must still leave the right table's scan.
    #[test]
    fn prunes_the_widened_right_input_of_an_outer_join() {
        fn projected(plan: &LogicalPlan, table: &str, found: &mut Vec<Vec<u32>>) {
            match plan {
                LogicalPlan::Scan(scan) if scan.table.table_name == table => {
                    found.push(scan.projected_column_ids.clone());
                }
                LogicalPlan::Join { left, right, .. } => {
                    projected(left, table, found);
                    projected(right, table, found);
                }
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Distinct { input, .. }
                | LogicalPlan::Derived { input, .. } => projected(input, table, found),
                _ => {}
            }
        }
        let plan = optimized(
            "SELECT u.id, COUNT(e.id) FROM users u LEFT JOIN events e ON e.id = u.id \
             AND e.id IN (SELECT s.id FROM singleton s WHERE s.name = u.name) GROUP BY u.id",
        );
        let mut found = Vec::new();
        projected(&plan, "events", &mut found);
        assert_eq!(found, [vec![1]]);
    }

    /// A derived table that groups is materialized, and every expression of
    /// its select list evaluated, whatever reads it.
    #[test]
    fn keeps_every_column_of_a_derived_table_that_groups() {
        let LogicalPlan::Derived { columns, .. } = project_input(optimized(
            "SELECT d.id FROM (SELECT id, COUNT(*) AS n FROM users GROUP BY id) AS d",
        )) else {
            panic!("derived");
        };
        assert_eq!(columns.len(), 2);
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

    /// A predicate spanning two tables becomes the join's condition.
    ///
    /// This used to assert the opposite - that the plan stayed a filter over a
    /// Cartesian product - because a two-table predicate cannot be pushed into
    /// either scan and there was nowhere else for it to go. That safety
    /// property is still checked below; what changed is that there is now
    /// somewhere for it to go, which is the difference between answering
    /// `FROM a, b WHERE a.id = b.id` and running it as a Cartesian product.
    #[test]
    fn a_predicate_spanning_two_tables_becomes_a_join_condition() {
        let input = project_input(optimized(
            "SELECT events.name FROM events, users WHERE events.id = users.id",
        ));
        let LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } = input
        else {
            panic!("expected the cross join and its filter to become a join");
        };
        assert_eq!(kind, pintail_sql::BoundJoinKind::Inner);
        assert!(condition.is_some(), "the predicate is the join condition");
        // The original property: neither scan absorbed a predicate it cannot
        // evaluate alone.
        for side in [*left, *right] {
            if let LogicalPlan::Scan(scan) = side {
                assert!(
                    scan.predicates.is_empty(),
                    "a two-table predicate must never be pushed into one scan"
                );
            }
        }
    }

    /// Nothing links these tables, so the Cartesian product is what was asked
    /// for and the plan must still say so.
    #[test]
    fn an_unlinked_cross_join_stays_a_cross_join() {
        let plan = project_input(optimized(
            "SELECT events.name FROM events, users WHERE events.id > 5",
        ));
        let inner = match plan {
            LogicalPlan::Filter { input, .. } => *input,
            other => other,
        };
        assert!(
            matches!(inner, LogicalPlan::CrossJoin { .. }),
            "no conjunct links the two relations, so they stay a product"
        );
    }

    #[test]
    fn keeps_right_side_where_predicates_above_left_joins() {
        // A null-extended row can pass `IS NULL`: the join stays LEFT and
        // the predicate above it.
        let input = project_input(optimized(
            "SELECT events.name FROM events \
             LEFT JOIN users ON events.id = users.id \
             WHERE users.name IS NULL",
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
    fn a_null_rejecting_where_turns_the_left_join_inner_and_sinks() {
        let input = project_input(optimized(
            "SELECT events.name FROM events \
             LEFT JOIN users ON events.id = users.id \
             WHERE users.name = 'selected'",
        ));
        let LogicalPlan::Join {
            kind: pintail_sql::BoundJoinKind::Inner,
            right,
            ..
        } = input
        else {
            panic!("a null-rejecting WHERE makes the left join inner: {input:?}");
        };
        let mut right = *right;
        while let LogicalPlan::Project { input, .. } = right {
            right = *input;
        }
        let LogicalPlan::Scan(scan) = right else {
            panic!("the predicate sinks into the joined scan");
        };
        assert_eq!(scan.predicates.len(), 1);
    }

    #[test]
    fn folds_constant_filters_to_empty_or_removes_them() {
        // A false filter becomes LIMIT 0 over the input, never a bare
        // Empty: the input's column shape must survive for projections.
        assert_eq!(
            project_input(optimized("SELECT 1 WHERE 2 + 2 = 5")),
            LogicalPlan::Limit {
                input: Box::new(LogicalPlan::OneRow),
                limit: pintail_sql::BoundLimit {
                    offset: 0,
                    count: 0
                },
            }
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
        let LogicalPlan::Distinct { input, .. } = *input else {
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
    fn joins_do_not_carry_scan_only_predicate_columns() {
        let plan = project_input(optimized(
            "SELECT e.id FROM events e, users u WHERE e.id = u.id AND e.name = 'selected'",
        ));
        let LogicalPlan::Join { left, right, .. } = plan else {
            panic!("join");
        };
        let trimmed = [left, right]
            .into_iter()
            .find_map(|side| match *side {
                LogicalPlan::Project { input, expressions } => Some((input, expressions)),
                _ => None,
            })
            .expect("scan-only payload must be projected away before joining");
        assert_eq!(trimmed.1.len(), 1);
        let LogicalPlan::Scan(scan) = *trimmed.0 else {
            panic!("scan");
        };
        assert_eq!(scan.predicates.len(), 1);
        assert_eq!(
            scan.projected_column_ids,
            [1, 2],
            "storage still needs the predicate column"
        );
    }

    #[test]
    fn constant_date_intervals_become_scan_literals() {
        for (sql, expected) in [
            ("DATE_ADD('1994-01-01', INTERVAL 1 YEAR)", "1995-01-01"),
            ("DATE_ADD('2024-02-29', INTERVAL 1 YEAR)", "2025-02-28"),
            ("DATE_SUB('2024-03-01', INTERVAL 1 DAY)", "2024-02-29"),
        ] {
            let LogicalPlan::Scan(scan) = project_input(optimized(&format!(
                "SELECT id FROM events WHERE name < {sql}"
            ))) else {
                panic!("scan");
            };
            let BoundExprKind::Binary { right, .. } = &scan.predicates[0].kind else {
                panic!("comparison");
            };
            assert_eq!(
                right.kind,
                BoundExprKind::Literal(pintail_types::Value::Utf8(expected.to_owned()))
            );
        }
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

    /// A non-equality predicate must not become a join condition.
    ///
    /// The physical join needs side-separable equalities to build hash keys;
    /// handing it `a.x < b.x` turns a query that used to execute as a filtered
    /// product into `UnsupportedJoinCondition`.
    #[test]
    fn an_inequality_does_not_become_a_join_condition() {
        let plan = project_input(optimized(
            "SELECT events.name FROM events, users WHERE events.id < users.id",
        ));
        let inner = match plan {
            LogicalPlan::Filter { input, .. } => *input,
            other => other,
        };
        assert!(
            matches!(inner, LogicalPlan::CrossJoin { .. }),
            "an inequality is not a hash-join key, so it stays a filtered product"
        );
    }

    /// A seed relation that links to nothing must not stop the rest joining.
    #[test]
    fn an_unlinked_relation_does_not_block_the_others() {
        fn has_join(plan: &LogicalPlan) -> bool {
            match plan {
                LogicalPlan::Join { .. } => true,
                LogicalPlan::CrossJoin { inputs } => inputs.iter().any(has_join),
                LogicalPlan::Filter { input, .. } | LogicalPlan::Project { input, .. } => {
                    has_join(input)
                }
                _ => false,
            }
        }

        let plan = project_input(optimized(
            "SELECT singleton.id FROM singleton, events, users WHERE events.id = users.id",
        ));
        assert!(
            has_join(&plan),
            "b and c are linked and must join even though a links to nothing"
        );
    }

    /// A volatile operand must not become a join key.
    ///
    /// A filter evaluates `RAND()` once per candidate PAIR; a hash join
    /// evaluates it once per input ROW. Moving it would change how many rows
    /// the query returns, silently.
    #[test]
    fn a_volatile_predicate_does_not_become_a_join_condition() {
        let plan = project_input(optimized(
            "SELECT events.name FROM events, users \
             WHERE events.id = users.id + ROUND(RAND())",
        ));
        let inner = match plan {
            LogicalPlan::Filter { input, .. } => *input,
            other => other,
        };
        assert!(
            matches!(inner, LogicalPlan::CrossJoin { .. }),
            "a volatile operand keeps the predicate in the filter"
        );
    }

    /// The smaller relation must be the one hashed.
    ///
    /// The right side is built into memory and the left probes it, so putting
    /// the larger relation on the right hashes a fact table to probe it with a
    /// dimension - the wrong way round, and the way that spills.
    #[test]
    fn the_smaller_relation_becomes_the_build_side() {
        // `events` has 100 rows in the fixture, `users` 20.
        let plan = project_input(optimized(
            "SELECT events.name FROM events, users WHERE events.id = users.id",
        ));
        let LogicalPlan::Join { left, right, .. } = plan else {
            panic!("expected an inferred join");
        };
        let name = |plan: &LogicalPlan| match plan {
            LogicalPlan::Scan(scan) => scan.table.table_name.clone(),
            _ => String::new(),
        };
        assert_eq!(name(&right), "users", "the smaller side is hashed");
        assert_eq!(name(&left), "events", "the larger side probes");
    }
}
