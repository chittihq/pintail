use std::cell::Cell;

use super::{
    AggregateFunction, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundQuery,
    BoundTable, WindowFunction,
};
use pintail_catalog::{DatabaseId, TableId};
use pintail_types::DataType;

/// An aggregate belongs to the nearest scope supplying one of its columns.
/// Its stable output identity lets a dependent query consume the grouped value.
pub(super) fn lift(
    expression: &mut BoundExpr,
    aggregates: &mut Vec<BoundAggregate>,
    tables: &[BoundTable],
    next_id: &Cell<u64>,
) {
    match &mut expression.kind {
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            lift_query(query, aggregates, tables, next_id);
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            lift(expr, aggregates, tables, next_id);
            lift_query(query, aggregates, tables, next_id);
        }
        _ => children(expression, &mut |child| {
            lift(child, aggregates, tables, next_id);
        }),
    }
}

fn lift_query(
    query: &mut BoundQuery,
    outer: &mut Vec<BoundAggregate>,
    tables: &[BoundTable],
    next_id: &Cell<u64>,
) {
    let mut replacements = Vec::new();
    let mut identities = Vec::new();
    for mut aggregate in std::mem::take(&mut query.aggregates) {
        if aggregate.declared
            && aggregate.function != AggregateFunction::AnyValue
            && aggregate.expr.as_ref().and_then(outer_only) == Some(true)
        {
            if let Some(expr) = &mut aggregate.expr {
                rebase(expr, tables);
            }
            let id = next_id.get();
            next_id.set(id.saturating_sub(1));
            let column = output_column(&aggregate, id);
            let mut reference = column.clone();
            reference.outer = true;
            if let Some(old) = aggregate.output_column.replace(Box::new(column)) {
                identities.push((*old, reference.clone()));
            }
            replacements.push(BoundExprKind::Column(reference));
            outer.push(aggregate);
        } else {
            replacements.push(BoundExprKind::Aggregate(
                query.group_by.len() + query.aggregates.len(),
            ));
            query.aggregates.push(aggregate);
        }
    }
    let group_count = query.group_by.len();
    query_expressions(query, &mut |expr| {
        rewrite_slots(expr, group_count, &replacements);
    });
    if !identities.is_empty() {
        rewrite_identities(query, &identities);
    }
    for branch in &mut query.union_all {
        lift_query(branch, outer, tables, next_id);
    }
    for (_, branch) in &mut query.set_ops {
        lift_query(branch, outer, tables, next_id);
    }
}

pub(super) fn output_column(aggregate: &BoundAggregate, id: u64) -> BoundColumn {
    let source = aggregate.expr.as_ref().and_then(|expr| match &expr.kind {
        BoundExprKind::Column(column) if expr.data_type == aggregate.data_type => Some(column),
        _ => None,
    });
    let float_decimals = if matches!(
        aggregate.data_type,
        Some(DataType::Float32 | DataType::Float64)
    ) {
        BoundExpr {
            kind: BoundExprKind::Aggregate(0),
            data_type: aggregate.data_type,
            nullable: aggregate.nullable,
        }
        .numeric_decimals(std::slice::from_ref(aggregate))
    } else {
        None
    };
    BoundColumn {
        database_id: DatabaseId::new(u64::MAX),
        table_id: TableId::new(id),
        column_id: 0,
        relation_name: format!("<aggregate-{id}>"),
        name: "<aggregate-value>".into(),
        data_type: aggregate.data_type.unwrap_or(DataType::Utf8),
        nullable: aggregate.nullable,
        collation: source.and_then(|column| column.collation.clone()),
        enum_labels: source.and_then(|column| column.enum_labels.clone()),
        geometry: source.is_some_and(|column| column.geometry),
        timestamp: source.is_some_and(|column| column.timestamp),
        binary_width: aggregate.expr.as_ref().and_then(BoundExpr::binary_width),
        bit_width: source.and_then(|column| column.bit_width),
        float_decimals,
        outer: false,
        using_shadowed: false,
    }
}

fn outer_only(expr: &BoundExpr) -> Option<bool> {
    match &expr.kind {
        BoundExprKind::Column(column) => column.outer.then_some(true),
        BoundExprKind::Literal(_) => Some(false),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => outer_only(expr),
        BoundExprKind::Binary { left, right, .. } => Some(outer_only(left)? | outer_only(right)?),
        BoundExprKind::Scalar { args, .. } => args
            .iter()
            .try_fold(false, |found, arg| Some(found | outer_only(arg)?)),
        _ => None,
    }
}

fn same_column(left: &BoundColumn, right: &BoundColumn) -> bool {
    left.database_id == right.database_id
        && left.table_id == right.table_id
        && left.column_id == right.column_id
        && left
            .relation_name
            .eq_ignore_ascii_case(&right.relation_name)
}

fn rebase(expr: &mut BoundExpr, tables: &[BoundTable]) {
    if let BoundExprKind::Column(column) = &mut expr.kind {
        if let Some(parent) = tables
            .iter()
            .flat_map(|table| &table.columns)
            .find(|parent| same_column(parent, column))
        {
            *column = parent.clone();
        }
    } else {
        children(expr, &mut |child| rebase(child, tables));
    }
}

fn rewrite_slots(expr: &mut BoundExpr, groups: usize, replacements: &[BoundExprKind]) {
    if let BoundExprKind::Aggregate(index) = &expr.kind {
        if let Some(replacement) = index
            .checked_sub(groups)
            .and_then(|index| replacements.get(index))
        {
            expr.kind = replacement.clone();
        }
    } else {
        children(expr, &mut |child| {
            rewrite_slots(child, groups, replacements);
        });
    }
}

fn children(expr: &mut BoundExpr, visit: &mut impl FnMut(&mut BoundExpr)) {
    match &mut expr.kind {
        BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. }
        | BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::InSubquery { expr, .. } => visit(expr),
        BoundExprKind::Binary { left, right, .. } => {
            visit(left);
            visit(right);
        }
        BoundExprKind::Scalar { args, .. } => args.iter_mut().for_each(visit),
        _ => {}
    }
}

fn query_expressions(query: &mut BoundQuery, visit: &mut impl FnMut(&mut BoundExpr)) {
    for projection in &mut query.projection {
        visit(&mut projection.expr);
    }
    for expr in query
        .filter
        .iter_mut()
        .chain(&mut query.having)
        .chain(&mut query.group_by)
    {
        visit(expr);
    }
    for aggregate in &mut query.aggregates {
        if let Some(expr) = &mut aggregate.expr {
            visit(expr);
        }
        for (expr, _) in &mut aggregate.order_within {
            visit(expr);
        }
    }
    for window in &mut query.windows {
        match &mut window.function {
            WindowFunction::Aggregate(aggregate) => {
                if let Some(expr) = &mut aggregate.expr {
                    visit(expr);
                }
            }
            WindowFunction::Offset { expr, default, .. } => {
                visit(expr);
                if let Some(default) = default {
                    visit(default);
                }
            }
            WindowFunction::Extreme { expr, .. } => visit(expr),
            _ => {}
        }
        for expr in &mut window.partition_by {
            visit(expr);
        }
        for key in &mut window.order_by {
            visit(&mut key.expr);
        }
    }
    for source in &mut query.from {
        for join in &mut source.joins {
            if let Some(condition) = &mut join.condition {
                visit(condition);
            }
        }
    }
}

fn rewrite_identities(query: &mut BoundQuery, identities: &[(BoundColumn, BoundColumn)]) {
    fn rewrite(expr: &mut BoundExpr, identities: &[(BoundColumn, BoundColumn)]) {
        match &mut expr.kind {
            BoundExprKind::Column(column) => {
                if let Some((_, replacement)) =
                    identities.iter().find(|(old, _)| same_column(old, column))
                {
                    *column = replacement.clone();
                }
            }
            BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
                rewrite_identities(query, identities);
            }
            BoundExprKind::InSubquery { expr, query, .. } => {
                rewrite(expr, identities);
                rewrite_identities(query, identities);
            }
            _ => children(expr, &mut |child| rewrite(child, identities)),
        }
    }
    query_expressions(query, &mut |expr| rewrite(expr, identities));
    for source in &mut query.from {
        if let Some(input) = &mut source.base.input {
            rewrite_identities(input, identities);
        }
        for join in &mut source.joins {
            if let Some(input) = &mut join.table.input {
                rewrite_identities(input, identities);
            }
        }
    }
    if let Some(recursive) = &mut query.recursive {
        rewrite_identities(&mut recursive.member, identities);
    }
    for branch in &mut query.union_all {
        rewrite_identities(branch, identities);
    }
    for (_, branch) in &mut query.set_ops {
        rewrite_identities(branch, identities);
    }
}

/// Correlated predicates still read representative input columns after grouping.
/// Retain their identities alongside the explicitly requested aggregate values.
pub(super) fn retain_correlated_inputs(
    expression: &mut BoundExpr,
    aggregates: &mut Vec<BoundAggregate>,
    tables: &[BoundTable],
    groups: &[BoundExpr],
) {
    fn retain(
        expr: &mut BoundExpr,
        aggregates: &mut Vec<BoundAggregate>,
        tables: &[BoundTable],
        groups: &[BoundExpr],
        nested: bool,
    ) {
        match &mut expr.kind {
            BoundExprKind::Column(column) if nested && column.outer => {
                let Some(parent) = tables
                    .iter()
                    .flat_map(|table| &table.columns)
                    .find(|parent| !parent.outer && same_column(parent, column))
                else {
                    return;
                };
                if groups.iter().any(|group| matches!(&group.kind, BoundExprKind::Column(key) if same_column(key, parent)))
                    || aggregates.iter().any(|aggregate| aggregate.output_column.as_deref().is_some_and(|output| same_column(output, parent))) {
                    return;
                }
                aggregates.push(BoundAggregate {
                    output_column: Some(Box::new(parent.clone())),
                    declared: false,
                    function: AggregateFunction::AnyValue,
                    data_type: Some(parent.data_type),
                    expr: Some(BoundExpr::column(parent.clone())),
                    distinct: false,
                    nullable: true,
                    separator: None,
                    order_within: Vec::new(),
                });
            }
            BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
                query_expressions(query, &mut |expr| {
                    retain(expr, aggregates, tables, groups, true);
                });
            }
            BoundExprKind::InSubquery { expr, query, .. } => {
                retain(expr, aggregates, tables, groups, nested);
                query_expressions(query, &mut |expr| {
                    retain(expr, aggregates, tables, groups, true);
                });
            }
            _ => children(expr, &mut |child| {
                retain(child, aggregates, tables, groups, nested);
            }),
        }
    }
    retain(expression, aggregates, tables, groups, false);
}
