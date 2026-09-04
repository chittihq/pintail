//! `ONLY_FULL_GROUP_BY` functional dependence: which columns the grouping
//! keys already decide.
//!
//! `MySQL` does not require a selected column to be listed in `GROUP BY`. It
//! requires the column to hold ONE value per group, and it proves that where
//! it can: grouping by a table's primary key fixes every other column of that
//! row, and an equality carries that proof across a join. Applications lean
//! on this constantly - `GROUP BY o.id` then selecting `o.placed_at`, or
//! grouping by a foreign key and selecting the joined dimension's name -
//! because the alternative is repeating a dozen columns in `GROUP BY`.
//!
//! Refusing those queries is not a stricter reading of the standard, it is a
//! different answer to the same SQL, so the analysis lives here.
//!
//! What is deliberately NOT here: determination through a `UNION`, through a
//! view's own grouping, or through a derived table's key. Those need the
//! inner query's keys projected outward, which the bound tree does not carry
//! yet, and their absence only costs a refusal.

use std::collections::HashSet;

use crate::bound::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundFrom, BoundJoinKind, BoundTable,
};

/// One column of one query-visible relation.
///
/// Keyed by relation name rather than table ID: a self-join gives both sides
/// the same table ID, and grouping by one alias's primary key says nothing
/// about the other alias's columns. Relation names are unique within a
/// query, which `DuplicateRelation` enforces, and they compare without
/// regard to case, so the lowered spelling is the identity.
type ColumnKey = (String, u32);

fn column_key(column: &BoundColumn) -> ColumnKey {
    (column.relation_name.to_ascii_lowercase(), column.column_id)
}

fn relation_key(table: &BoundTable) -> String {
    table.relation_name.to_ascii_lowercase()
}

/// The columns the grouping keys of one query functionally determine.
pub(super) struct DeterminedColumns {
    columns: HashSet<ColumnKey>,
}

impl DeterminedColumns {
    /// Whether every row of a group carries the same value for this column.
    ///
    /// An outer reference is never claimed here: it resolves in an enclosing
    /// scope, so its relation name is not one of the names this analysis
    /// walked and a match would be a coincidence of spelling.
    pub(super) fn contains(&self, column: &BoundColumn) -> bool {
        !column.outer && self.columns.contains(&column_key(column))
    }
}

/// Columns of `expr`, or `None` when the expression contains something this
/// analysis cannot reason about (a subquery, a window, an aggregate).
///
/// `None` is not "no columns": it means the expression's value is not
/// decided by the columns it mentions, so an equality against it proves
/// nothing.
fn expr_columns<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundColumn>) -> bool {
    match &expr.kind {
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Column(column) => {
            out.push(column);
            true
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            expr_columns(expr, out)
        }
        BoundExprKind::Binary { left, right, .. } => {
            expr_columns(left, out) && expr_columns(right, out)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(|arg| expr_columns(arg, out)),
        BoundExprKind::Aggregate(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::InSubquery { .. } => false,
    }
}

/// Top-level `AND` operands of a predicate.
///
/// Only conjuncts count. An equality under `OR` holds for some rows and not
/// others, so it decides nothing about a group.
fn conjuncts<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        _ => out.push(expr),
    }
}

/// `(left, right)` of every equality conjunct in a predicate.
fn equalities<'a>(expr: &'a BoundExpr, out: &mut Vec<(&'a BoundExpr, &'a BoundExpr)>) {
    let mut parts = Vec::new();
    conjuncts(expr, &mut parts);
    for part in parts {
        if let BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } = &part.kind
        {
            out.push((left, right));
        }
    }
}

/// One side of an equality, when it is a plain column of this query.
fn plain_column(expr: &BoundExpr) -> Option<&BoundColumn> {
    match &expr.kind {
        BoundExprKind::Column(column) if !column.outer => Some(column),
        _ => None,
    }
}

/// Whether an expression's value is fixed once `columns` are fixed.
///
/// `exclude` names a relation whose columns do not count even when they are
/// determined: an outer join's `ON` clause may equate two columns of its own
/// inner table, which says nothing about how that table was reached.
fn decided_by(expr: &BoundExpr, columns: &HashSet<ColumnKey>, exclude: Option<&str>) -> bool {
    let mut referenced = Vec::new();
    if !expr_columns(expr, &mut referenced) {
        return false;
    }
    referenced.iter().all(|column| {
        if column.outer {
            return true;
        }
        let key = column_key(column);
        exclude != Some(key.0.as_str()) && columns.contains(&key)
    })
}

/// Computes the columns determined by `group_by`.
///
/// The rules, each one `MySQL` documents for `ONLY_FULL_GROUP_BY`:
///
/// 1. every grouping key that is a plain column determines itself;
/// 2. an equality conjunct in `WHERE` or an inner `ON` determines the column
///    on one side from determined columns on the other (a constant on the
///    other side is the degenerate case, and determines outright);
/// 3. a table whose whole `NOT NULL` unique key is determined has every one
///    of its columns determined;
/// 4. for the inner side of an outer join, an `ON` equality against the
///    outer side determines a key component well enough for rule 3 - such a
///    row either matched the one key row it could match, or is NULL
///    throughout.
pub(super) fn determined_columns(
    group_by: &[BoundExpr],
    from: &[BoundFrom],
    tables: &[BoundTable],
    filter: Option<&BoundExpr>,
) -> DeterminedColumns {
    let mut columns: HashSet<ColumnKey> = group_by
        .iter()
        .filter_map(plain_column)
        .map(column_key)
        .collect();

    // Equalities that hold for every row a group contains: WHERE filters the
    // whole result, and an inner ON is a filter wearing another hat.
    let mut filters: Vec<(&BoundExpr, &BoundExpr)> = Vec::new();
    if let Some(filter) = filter {
        equalities(filter, &mut filters);
    }
    // Equalities that hold only for the rows an outer join matched, kept
    // against the relation they may instead be NULL for.
    let mut outer: Vec<(String, &BoundExpr, &BoundExpr)> = Vec::new();
    let mut null_supplied: HashSet<String> = HashSet::new();
    for item in from {
        for join in &item.joins {
            let Some(condition) = &join.condition else {
                continue;
            };
            match join.kind {
                BoundJoinKind::Inner => equalities(condition, &mut filters),
                BoundJoinKind::Left | BoundJoinKind::Scalar => {
                    let relation = relation_key(&join.table);
                    null_supplied.insert(relation.clone());
                    let mut found = Vec::new();
                    equalities(condition, &mut found);
                    outer.extend(
                        found
                            .into_iter()
                            .map(|(left, right)| (relation.clone(), left, right)),
                    );
                }
                // A semi or anti join contributes no visible column, and a
                // cross join has no condition to read.
                BoundJoinKind::Semi | BoundJoinKind::Anti | BoundJoinKind::Cross => {}
            }
        }
    }

    // Each rule can feed the next - a key component decided by an equality
    // unlocks its table, whose columns decide the next join - so this runs
    // to a fixpoint rather than in one pass.
    loop {
        let before = columns.len();

        for (left, right) in &filters {
            for (target, source) in [(*left, *right), (*right, *left)] {
                if let Some(column) = plain_column(target)
                    && decided_by(source, &columns, None)
                {
                    columns.insert(column_key(column));
                }
            }
        }

        for table in tables {
            if table.key_column_ids.is_empty() {
                continue;
            }
            let relation = relation_key(table);
            let inner_side = null_supplied.contains(&relation);
            let determined = table.key_column_ids.iter().all(|id| {
                let Some(column) = table.columns.iter().find(|column| column.column_id == *id)
                else {
                    return false;
                };
                if columns.contains(&column_key(column)) {
                    // A NULL unique key repeats: several rows can carry it
                    // and they need not agree on anything else. The inner
                    // side of an outer join is exempt - the rows that
                    // brought the NULL are the unmatched ones, and those are
                    // NULL in every column of this table.
                    return !column.nullable || inner_side;
                }
                inner_side
                    && outer.iter().any(|(joined, left, right)| {
                        *joined == relation
                            && key_pinned_by(column, left, right, &columns, &relation)
                    })
            });
            if determined {
                columns.extend(table.columns.iter().map(column_key));
            }
        }

        if columns.len() == before {
            break;
        }
    }

    DeterminedColumns { columns }
}

/// Whether one equality of an outer join's `ON` pins `column` of the inner
/// table to a value the group already fixes.
fn key_pinned_by(
    column: &BoundColumn,
    left: &BoundExpr,
    right: &BoundExpr,
    columns: &HashSet<ColumnKey>,
    relation: &str,
) -> bool {
    [(left, right), (right, left)]
        .into_iter()
        .any(|(side, other)| {
            plain_column(side).is_some_and(|target| column_key(target) == column_key(column))
                && decided_by(other, columns, Some(relation))
        })
}
