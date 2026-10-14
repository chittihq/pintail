//! A WHERE condition on a derived set operation, tested inside its branches.
//!
//! The column of a `UNION`, `INTERSECT` or `EXCEPT` is plain text whatever
//! its branches read, so above the operation an ENUM compares by its label.
//! `MySQL` does not test a condition on such a derived table above it,
//! though: a condition that reads only the derived table's columns is moved
//! into every branch, and there `s = 3` meets the branch's own ENUM column
//! and compares its position. The two readings differ exactly where a branch
//! hands on an ENUM or SET column, or an IF, CASE or NULLIF over one, so
//! only then is the condition moved here; every other query keeps its plan.

use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Query, Select, SelectItem, SetExpr, SetOperator, TableFactor};

use super::{
    Binder, and_bound, bind_column, bind_expr, choice_as_number, chooses_declared,
    expression_scope, is_declared_column, is_truth_value,
};
use crate::bound::ScalarFunction;
use crate::bound::{BoundExpr, BoundExprKind, BoundFrom, BoundJoinKind, BoundQuery, BoundTable};

/// The name the binder gives the derived table it wraps a set operand in.
const SET_OPERAND: &str = "<set-operand>";

/// Whether a table of this query is a derived set operation a condition
/// could be moved into. Decided once per query, before any conjunct is
/// looked at.
pub(super) fn has_set_input(tables: &[BoundTable]) -> bool {
    tables.iter().any(|table| {
        table.input.as_deref().is_some_and(|input| {
            (!input.union_all.is_empty() || !input.set_ops.is_empty())
                && input.limit.is_none()
                && input.recursive.is_none()
        })
    })
}

impl Binder<'_> {
    /// Moves `conjunct` into the branches of the derived set operation it
    /// reads, and answers whether it did. It moves only when the conjunct
    /// reads one derived table and nothing else, calls nothing whose answer
    /// changes between evaluations, the derived table is not the optional
    /// side of an outer join, every branch is a plain SELECT without
    /// grouping, windows or LIMIT, and a branch hands on a declared value
    /// in a column the conjunct reads.
    pub(super) fn filter_set_branches(
        &self,
        conjunct: &Expr,
        selection: &Expr,
        select: &Select,
        from: &mut [BoundFrom],
        tables: &mut [BoundTable],
    ) -> bool {
        let Some((position, read)) = single_relation(conjunct, tables) else {
            return false;
        };
        let target = &tables[position];
        let id = target.table_id;
        let required = from.iter().any(|source| {
            source.base.table_id == id
                || source.joins.iter().any(|join| {
                    join.table.table_id == id
                        && matches!(join.kind, BoundJoinKind::Inner | BoundJoinKind::Cross)
                })
        });
        let Some(input) = target.input.as_deref() else {
            return false;
        };
        let Some(written) = derived_source(&select.from, &target.relation_name) else {
            return false;
        };
        if !required
            || written.with.is_some()
            || written.order_by.is_some()
            || written.limit_clause.is_some()
            || written.fetch.is_some()
        {
            return false;
        }
        let mut selects = Vec::new();
        if !written_branches(&written.body, &mut selects) {
            return false;
        }
        let mut branches = Vec::new();
        bound_branches(input, &mut branches);
        if branches.len() < 2
            || branches.len() != selects.len()
            || !branches
                .iter()
                .zip(&selects)
                .all(|(branch, select)| plain_branch(branch, select))
        {
            return false;
        }
        let declared = read.iter().any(|&column| {
            branches.iter().any(|branch| {
                branch
                    .projection
                    .get(column)
                    .is_some_and(|item| hands_on_declared(&item.expr))
            })
        });
        if !declared || fixed_twice(selection, tables, position, &read) {
            return false;
        }
        let Some(conditions) =
            self.branch_conditions(conjunct, &branches, &selects, tables, position)
        else {
            return false;
        };
        let Some(mut filtered) = tables[position].input.clone() else {
            return false;
        };
        let mut conditions = conditions.into_iter();
        filter_branches(&mut filtered, &mut conditions);
        for source in from.iter_mut() {
            if source.base.table_id == id {
                source.base.input = Some(filtered.clone());
            }
            for join in &mut source.joins {
                if join.table.table_id == id {
                    join.table.input = Some(filtered.clone());
                }
            }
        }
        tables[position].input = Some(filtered);
        true
    }

    /// `conjunct` as each branch tests it: over the branch's own select
    /// list, bound where the branch's columns are what they were declared
    /// as. `None` when a branch cannot test it.
    fn branch_conditions(
        &self,
        conjunct: &Expr,
        branches: &[&BoundQuery],
        selects: &[&Select],
        tables: &[BoundTable],
        position: usize,
    ) -> Option<Vec<BoundExpr>> {
        let mut conditions = Vec::with_capacity(branches.len());
        for (branch, select) in branches.iter().zip(selects) {
            let mut condition = conjunct.clone();
            let flow: ControlFlow<()> =
                sqlparser::ast::visit_expressions_mut(&mut condition, |node| {
                    let identifiers = match node {
                        Expr::Identifier(identifier) => std::slice::from_ref(identifier),
                        Expr::CompoundIdentifier(identifiers) => identifiers.as_slice(),
                        _ => return ControlFlow::Continue(()),
                    };
                    let Some(column) = column_position(identifiers, tables, position) else {
                        return ControlFlow::Break(());
                    };
                    let (SelectItem::UnnamedExpr(item)
                    | SelectItem::ExprWithAlias { expr: item, .. }) = &select.projection[column]
                    else {
                        return ControlFlow::Break(());
                    };
                    *node = Expr::Nested(Box::new(item.clone()));
                    ControlFlow::Continue(())
                });
            if flow.is_break() {
                return None;
            }
            let scope = expression_scope(&branch.tables, &self.outer_tables);
            let bound = bind_expr(&condition, &scope, None).ok()?;
            if !is_truth_value(bound.data_type) {
                return None;
            }
            conditions.push(choice_as_number(bound));
        }
        Some(conditions)
    }
}

/// The one table `conjunct` reads and the positions of the columns it reads
/// there, when it reads a single derived set operation of this query and
/// holds nothing that keeps `MySQL` from moving it.
fn single_relation(conjunct: &Expr, tables: &[BoundTable]) -> Option<(usize, Vec<usize>)> {
    let mut relation = None;
    let mut read = Vec::new();
    let flow: ControlFlow<()> = sqlparser::ast::visit_expressions(conjunct, |node| {
        let identifiers = match node {
            Expr::Identifier(identifier) => std::slice::from_ref(identifier),
            Expr::CompoundIdentifier(identifiers) => identifiers.as_slice(),
            Expr::Subquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::AnyOp { .. }
            | Expr::AllOp { .. } => return ControlFlow::Break(()),
            Expr::Function(function) => {
                let volatile = function.over.is_some()
                    || function.name.0.last().is_some_and(|part| {
                        matches!(
                            part.to_string().to_ascii_uppercase().as_str(),
                            "RAND" | "UUID" | "UUID_SHORT" | "RANDOM_BYTES" | "SLEEP"
                        )
                    });
                return if volatile {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                };
            }
            _ => return ControlFlow::Continue(()),
        };
        let Ok(BoundExpr {
            kind: BoundExprKind::Column(column),
            ..
        }) = bind_column(identifiers, tables)
        else {
            return ControlFlow::Break(());
        };
        let Some(position) = tables
            .iter()
            .position(|table| table.table_id == column.table_id && table.input.is_some())
        else {
            return ControlFlow::Break(());
        };
        if relation.is_some_and(|known| known != position) {
            return ControlFlow::Break(());
        }
        relation = Some(position);
        let Some(index) = usize::try_from(column.column_id)
            .ok()
            .and_then(|id| id.checked_sub(1))
        else {
            return ControlFlow::Break(());
        };
        if !read.contains(&index) {
            read.push(index);
        }
        ControlFlow::Continue(())
    });
    if flow.is_break() {
        return None;
    }
    relation.map(|position| (position, read))
}

/// Whether the WHERE clause equates one of the `read` columns with a
/// constant in two of its conjuncts. `MySQL` folds two such equalities
/// against each other before it moves anything, and finds the pair
/// impossible when the constants differ as the text they are above the set
/// operation; left where they are written, they answer the same.
fn fixed_twice(selection: &Expr, tables: &[BoundTable], position: usize, read: &[usize]) -> bool {
    let fixed = |conjunct: &Expr| {
        let Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq,
            right,
        } = conjunct
        else {
            return None;
        };
        let column = |side: &Expr| match side {
            Expr::Identifier(identifier) => {
                column_position(std::slice::from_ref(identifier), tables, position)
            }
            Expr::CompoundIdentifier(identifiers) => column_position(identifiers, tables, position),
            _ => None,
        };
        match (left.as_ref(), right.as_ref()) {
            (side, Expr::Value(_)) | (Expr::Value(_), side) => column(side),
            _ => None,
        }
    };
    let mut seen = Vec::new();
    for column in super::split_and_conjuncts(selection)
        .into_iter()
        .filter_map(fixed)
    {
        if read.contains(&column) && seen.contains(&column) {
            return true;
        }
        seen.push(column);
    }
    false
}

/// The position, in the table at `position`, of the column `identifiers`
/// name.
fn column_position(
    identifiers: &[sqlparser::ast::Ident],
    tables: &[BoundTable],
    position: usize,
) -> Option<usize> {
    let Ok(BoundExpr {
        kind: BoundExprKind::Column(column),
        ..
    }) = bind_column(identifiers, tables)
    else {
        return None;
    };
    if column.table_id != tables[position].table_id {
        return None;
    }
    usize::try_from(column.column_id).ok()?.checked_sub(1)
}

/// The query written for the derived table `name` in this FROM clause.
fn derived_source<'a>(from: &'a [sqlparser::ast::TableWithJoins], name: &str) -> Option<&'a Query> {
    fn factor<'a>(factor: &'a TableFactor, name: &str) -> Option<&'a Query> {
        match factor {
            TableFactor::Derived {
                lateral: false,
                subquery,
                alias: Some(alias),
                ..
            } if alias.name.value == name => Some(subquery),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => derived_source(std::slice::from_ref(table_with_joins), name),
            _ => None,
        }
    }
    from.iter().find_map(|item| {
        factor(&item.relation, name).or_else(|| {
            item.joins
                .iter()
                .find_map(|join| factor(&join.relation, name))
        })
    })
}

/// The SELECTs of a set operation in the order they are written; false
/// when an operand is anything else.
fn written_branches<'a>(body: &'a SetExpr, out: &mut Vec<&'a Select>) -> bool {
    match body {
        SetExpr::Select(select) => {
            out.push(select);
            true
        }
        SetExpr::SetOperation {
            left,
            right,
            op: SetOperator::Union | SetOperator::Intersect | SetOperator::Except,
            ..
        } => written_branches(left, out) && written_branches(right, out),
        _ => false,
    }
}

/// Whether `query` only wraps a set operand the binder had to keep whole.
fn wraps_operand(query: &BoundQuery) -> bool {
    matches!(query.from.as_slice(), [source]
        if source.joins.is_empty() && source.base.table_name == SET_OPERAND)
}

/// The bound branches of a set operation, in the order they were written.
fn bound_branches<'a>(query: &'a BoundQuery, out: &mut Vec<&'a BoundQuery>) {
    match query
        .from
        .first()
        .and_then(|source| source.base.input.as_deref())
    {
        Some(operand) if wraps_operand(query) => bound_branches(operand, out),
        _ => out.push(query),
    }
    for branch in &query.union_all {
        bound_branches(branch, out);
    }
    for (_, branch) in &query.set_ops {
        bound_branches(branch, out);
    }
}

/// Adds the next condition to each branch's WHERE, in the order
/// [`bound_branches`] lists them.
fn filter_branches(query: &mut BoundQuery, conditions: &mut impl Iterator<Item = BoundExpr>) {
    if wraps_operand(query) {
        if let Some(operand) = query.from[0].base.input.as_deref_mut() {
            filter_branches(operand, conditions);
        }
        // The table list holds its own copy of the wrapped operand.
        let operand = query.from[0].base.input.clone();
        if let Some(table) = query.tables.first_mut() {
            table.input = operand;
        }
    } else if let Some(condition) = conditions.next() {
        query.filter = Some(match query.filter.take() {
            Some(filter) => and_bound(filter, condition),
            None => condition,
        });
    }
    for branch in &mut query.union_all {
        filter_branches(branch, conditions);
    }
    for (_, branch) in &mut query.set_ops {
        filter_branches(branch, conditions);
    }
}

/// Whether a branch is a SELECT the condition can simply join the WHERE of.
fn plain_branch(branch: &BoundQuery, select: &Select) -> bool {
    !wraps_operand(branch)
        && branch.group_by.is_empty()
        && branch.aggregates.is_empty()
        && branch.windows.is_empty()
        && branch.having.is_none()
        && branch.order_by.is_empty()
        && branch.limit.is_none()
        && branch.hidden_sort_columns == 0
        && branch.recursive.is_none()
        && branch.outer_set.is_none()
        && branch.projection.len() == select.projection.len()
        && select.projection.iter().all(|item| {
            matches!(
                item,
                SelectItem::UnnamedExpr(_) | SelectItem::ExprWithAlias { .. }
            )
        })
}

/// Whether a branch's select item hands on a value that reads differently
/// inside the branch than above the set operation.
fn hands_on_declared(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Scalar {
            function: ScalarFunction::PlainText,
            args,
        } => is_declared_column(&args[0]),
        _ => chooses_declared(expression),
    }
}
