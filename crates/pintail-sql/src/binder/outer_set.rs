//! The set-at-a-time form of a correlated subquery.
//!
//! `(SELECT SUM(x) FROM t JOIN u ON .. WHERE t.k = o.k AND u.c IN (SELECT ..
//! WHERE p.g = o2.g))` is one question asked once per outer row. Asked row
//! by row it is planned and executed once per row; nothing about it needs
//! that. The outer rows' values can be a relation of their own: join it to
//! the subquery's tables, group by which outer row each joined row belongs
//! to, and one execution answers every outer row at once.
//!
//! The form built here is that query. For each outer relation the subquery
//! names, the FROM clause gains a virtual relation under the same name,
//! carrying an ordinal (which outer tuple) and the columns the subquery
//! reads from it. The subquery's own text is otherwise untouched: its
//! references to the outer query now resolve to those relations because
//! they are in scope, and every rewrite the binder already applies to a
//! query's own FROM - a correlated `IN` becoming a semi join - applies to
//! them. The query groups by the ordinal and projects the ordinal and the
//! original value.
//!
//! What keeps the answers the per-row ones:
//!
//! - **Only an ungrouped aggregate over one FROM item** takes the form. It
//!   yields exactly one row per outer row whatever the data, so no outer
//!   row can raise the more-than-one-row error and none can come back with
//!   no row; an ordinal absent from the grouped result is an outer row no
//!   inner row matched, and the executor answers it with the value the
//!   aggregate has over no rows.
//! - **`ORDER BY .. LIMIT 1` over a join** is the other shape that is at
//!   most one row whatever the data. Its form ranks the joined rows of each
//!   ordinal by the same keys and keeps the first; an ordinal with no row
//!   is NULL. Rows that tie on every key have no defined first in either
//!   reading.
//! - **A subquery read row by row** - the members an `IN` tests, the rows
//!   an `EXISTS` asks for - has no ORDER BY or LIMIT to keep, so its form
//!   is the same join without the grouping: every joined row comes back
//!   under its ordinal, NULL members included, and the executor hands each
//!   outer row the rows of its own tuple. The three-valued answer of
//!   `IN` and `NOT IN` is then computed from those members exactly as it
//!   is from the members a per-row execution returns.
//! - **The outer rows must join something.** With no conjunct equating the
//!   first inner table with an outer value there is nothing to join on,
//!   and the form is not built. Where the equality names a later table of
//!   an all-INNER join chain, the chain is written in the order the
//!   equalities connect it - INNER joins and their conditions commute -
//!   so the outer rows still restrict the first table read.
//! - **Names keep their meaning or the form is refused.** The inner scope
//!   shadows the outer one in the original; here both are in one FROM, so
//!   an inner relation named like an outer one is a duplicate, and a bare
//!   column name both sides carry is ambiguous. Either fails to bind, and a
//!   form that fails to bind is not built: the subquery stays per-row.
//! - **Conjuncts move only where a filter and a join condition are the
//!   same thing**: a WHERE conjunct that reads nothing beyond the first
//!   inner table and the outer relations becomes that table's join
//!   condition, so the outer rows restrict it before the later joins rather
//!   than after. Later joins are INNER or LEFT with that table on the
//!   preserved side, where filtering it first or last keeps the same rows.
//! - **The outer values are compared as columns**, which is what they are;
//!   each virtual column carries its outer column's type and collation.
//!   ENUM, BIT and spatial outer columns are refused.

use std::ops::ControlFlow;

use pintail_catalog::{DatabaseId, TableId};
use pintail_types::DataType;
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, Ident, Join, JoinConstraint, JoinOperator, ObjectName,
    OrderBy, OrderByKind, Query, SelectItem, SetExpr, Statement, TableAlias, TableFactor,
    TableWithJoins,
};

use super::{Binder, BoundCte, and_all, bind_expr, split_and_conjuncts};
use crate::bound::{
    BoundColumn, BoundExpr, BoundExprKind, BoundLimit, BoundQuery, BoundTable, OuterSetKind,
    OuterSetQuery, OuterSetRelation,
};

/// Name of the ordinal column every virtual relation carries first.
const ORDINAL_COLUMN: &str = "<ordinal>";

/// Which of the shapes that have a form a subquery is.
enum Shape<'a> {
    /// An ungrouped aggregate: one row per outer row as written.
    Aggregate,
    /// `ORDER BY .. LIMIT 1`: the first row of an ordering.
    First(&'a OrderBy),
    /// Every row, in no order.
    Rows,
}

fn binds(expr: &Expr, scope: &[BoundTable]) -> bool {
    bind_expr(expr, scope, None).is_ok()
}

/// Whether `conjunct` is an equality between `table` and the relations
/// already `placed`: it binds over both and over neither alone.
fn connects(conjunct: &Expr, placed: &[BoundTable], table: &BoundTable) -> bool {
    let mut inner = conjunct;
    while let Expr::Nested(nested) = inner {
        inner = nested;
    }
    if !matches!(
        inner,
        Expr::BinaryOp {
            op: BinaryOperator::Eq,
            ..
        }
    ) || binds(conjunct, placed)
        || binds(conjunct, std::slice::from_ref(table))
    {
        return false;
    }
    let mut both = placed.to_vec();
    both.push(table.clone());
    binds(conjunct, &both)
}

fn inner_join(relation: TableFactor, condition: Expr) -> Join {
    Join {
        relation,
        global: false,
        join_operator: JoinOperator::Inner(JoinConstraint::On(condition)),
    }
}

impl Binder<'_> {
    /// The set-at-a-time form of `query`, whose per-row binding is `bound`
    /// and whose outer scope is `visible`; the reason there is none when the
    /// shape is not one the form answers exactly.
    #[allow(clippy::too_many_lines)] // one shape check and one rewrite, read top to bottom
    pub(super) fn outer_set_form(
        &self,
        query: &Query,
        ctes: &[BoundCte],
        visible: &[BoundTable],
        bound: &BoundQuery,
    ) -> Result<OuterSetQuery, &'static str> {
        // A subquery of a subquery is substituted by its parent's per-row
        // execution before it runs; its form would hold stale references.
        if !self.outer_tables.is_empty() {
            return Err("it is nested inside another subquery");
        }
        if !bound.group_by.is_empty() || bound.having.is_some() {
            return Err("it has GROUP BY or HAVING");
        }
        if !bound.windows.is_empty() || bound.distinct {
            return Err("it has a window function or DISTINCT");
        }
        if bound.from.len() != 1
            || !bound.union_all.is_empty()
            || !bound.set_ops.is_empty()
            || bound.recursive.is_some()
            || query.with.is_some()
        {
            return Err("it is not one SELECT over one FROM item");
        }
        let single = bound.projection.len() == bound.hidden_sort_columns + 1;
        // An ungrouped aggregate is one row per outer row as written. So is
        // the first row of an ordering: `ORDER BY .. LIMIT 1`. Anything else
        // is read row by row, which only an unordered, unlimited subquery
        // can be.
        let shape = if !bound.aggregates.is_empty() {
            if bound.limit.is_some() || query.order_by.is_some() || query.limit_clause.is_some() {
                return Err("its aggregate is under ORDER BY or LIMIT");
            }
            Shape::Aggregate
        } else if bound.limit.is_none() && query.order_by.is_none() && query.limit_clause.is_none()
        {
            Shape::Rows
        } else {
            let first_row = BoundLimit {
                offset: 0,
                count: 1,
            };
            let refused = "its LIMIT is not the first row of an ORDER BY";
            let order = query.order_by.as_ref().ok_or(refused)?;
            let OrderByKind::Expressions(keys) = &order.kind else {
                return Err(refused);
            };
            // A key that is a constant is a select-list position.
            if bound.limit != Some(first_row)
                || order.interpolate.is_some()
                || keys.iter().any(|key| matches!(key.expr, Expr::Value(_)))
            {
                return Err(refused);
            }
            Shape::First(order)
        };
        let SetExpr::Select(inner) = query.body.as_ref() else {
            return Err("it is not one SELECT over one FROM item");
        };
        let projected = match inner.projection.as_slice() {
            [
                SelectItem::UnnamedExpr(projected)
                | SelectItem::ExprWithAlias {
                    expr: projected, ..
                },
            ] if single => Some(projected),
            _ => None,
        };
        if projected.is_none() && !matches!(shape, Shape::Rows) {
            return Err("its select list is not one expression");
        }
        let [source] = inner.from.as_slice() else {
            return Err("it is not one SELECT over one FROM item");
        };
        if source.joins.iter().any(|join| {
            join.global
                || !matches!(
                    join.join_operator,
                    JoinOperator::Join(_)
                        | JoinOperator::Inner(_)
                        | JoinOperator::Left(_)
                        | JoinOperator::LeftOuter(_)
                )
        }) {
            return Err("it joins by something other than INNER or LEFT");
        }
        let selection = inner
            .selection
            .as_ref()
            .ok_or("it has no WHERE to join the outer rows on")?;

        // Every qualified name that resolves in the outer scope. A name the
        // subquery resolves itself is also collected when an outer relation
        // answers to it; the extra column is carried and never read.
        let mut relations: Vec<(String, Vec<BoundColumn>)> = Vec::new();
        let mut refused = false;
        let flow: ControlFlow<()> = sqlparser::ast::visit_expressions(query, |node| {
            if let Expr::CompoundIdentifier(parts) = node
                && parts.len() == 2
                && let Ok(BoundExpr {
                    kind: BoundExprKind::Column(column),
                    ..
                }) = bind_expr(node, visible, None)
            {
                if column.enum_labels.is_some() || column.geometry || column.bit_width.is_some() {
                    refused = true;
                    return ControlFlow::Break(());
                }
                let position = relations
                    .iter()
                    .position(|(name, _)| name.eq_ignore_ascii_case(&column.relation_name))
                    .unwrap_or_else(|| {
                        relations.push((column.relation_name.clone(), Vec::new()));
                        relations.len() - 1
                    });
                let columns = &mut relations[position].1;
                if !columns.iter().any(|existing| {
                    existing.table_id == column.table_id && existing.column_id == column.column_id
                }) {
                    columns.push(column);
                }
            }
            ControlFlow::Continue(())
        });
        if refused || flow.is_break() {
            return Err("it reads an ENUM, BIT or spatial outer column");
        }
        if relations.is_empty() {
            return Err("it names no outer column by its relation");
        }

        let database_id = DatabaseId::new(u64::MAX);
        let mut scoped_ctes = ctes.to_vec();
        let mut described = Vec::with_capacity(relations.len());
        let mut factors = Vec::with_capacity(relations.len());
        for (index, (relation, columns)) in relations.iter().enumerate() {
            let table_id = self.next_derived_id.get();
            self.next_derived_id.set(table_id.saturating_sub(1));
            let table_id = TableId::new(table_id);
            let name = format!("<outer-rows-{index}>");
            let mut served = vec![BoundColumn {
                database_id,
                table_id,
                column_id: 1,
                relation_name: relation.clone(),
                name: ORDINAL_COLUMN.to_owned(),
                data_type: DataType::UInt64,
                nullable: false,
                collation: None,
                enum_labels: None,
                geometry: false,
                timestamp: false,
                binary_width: None,
                bit_width: None,
                float_decimals: None,
                outer: false,
                using_shadowed: false,
            }];
            for (offset, column) in columns.iter().enumerate() {
                served.push(BoundColumn {
                    database_id,
                    table_id,
                    column_id: u32::try_from(offset + 2)
                        .map_err(|_| "it reads too many outer columns")?,
                    outer: false,
                    using_shadowed: false,
                    ..column.clone()
                });
            }
            scoped_ctes.push(BoundCte {
                name: name.clone(),
                column_names: Vec::new(),
                // Never read: a working table is scanned, not inlined.
                query: bound.clone(),
                working: Some(BoundTable {
                    database_id,
                    table_id,
                    database_name: String::new(),
                    table_name: name.clone(),
                    relation_name: relation.clone(),
                    schema_version: 0,
                    columns: served,
                    row_count: None,
                    estimated_rows: None,
                    key_column_ids: Vec::new(),
                    column_statistics: None,
                    input: None,
                }),
            });
            factors.push(TableFactor::Table {
                name: ObjectName::from(vec![Ident::with_quote('`', name)]),
                alias: Some(TableAlias {
                    explicit: true,
                    name: Ident::with_quote('`', relation.clone()),
                    columns: Vec::new(),
                    at: None,
                }),
                args: None,
                with_hints: Vec::new(),
                version: None,
                with_ordinality: false,
                partitions: Vec::new(),
                json_path: None,
                sample: None,
                index_hints: Vec::new(),
            });
            described.push(OuterSetRelation {
                table_id,
                columns: columns.clone(),
            });
        }
        let ordinal = |relation: &str| {
            Expr::CompoundIdentifier(vec![
                Ident::with_quote('`', relation.to_owned()),
                Ident::with_quote('`', ORDINAL_COLUMN.to_owned()),
            ])
        };
        let first_ordinal = ordinal(&relations[0].0);

        let unbound = "its outer rows or first table do not bind beside each other";
        let outer_scope = factors
            .iter()
            .map(|factor| self.bind_table(factor, &scoped_ctes))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unbound)?;
        let first_table = self
            .bind_table(&source.relation, ctes)
            .map_err(|_| unbound)?;
        let mut joins = Vec::with_capacity(factors.len() + source.joins.len());
        for (index, factor) in factors.iter().enumerate().skip(1) {
            joins.push(inner_join(
                factor.clone(),
                Expr::BinaryOp {
                    left: Box::new(ordinal(&relations[index].0)),
                    op: BinaryOperator::Eq,
                    right: Box::new(first_ordinal.clone()),
                },
            ));
        }

        // Conjuncts the first inner table can take as its join condition:
        // they read it and the outer relations and nothing else.
        let conjuncts = split_and_conjuncts(selection);
        let mut base_scope = outer_scope.clone();
        base_scope.push(first_table.clone());
        let (joined, kept): (Vec<&Expr>, Vec<&Expr>) = conjuncts
            .iter()
            .copied()
            .partition(|conjunct| binds(conjunct, &base_scope));
        let first_connects = joined
            .iter()
            .any(|conjunct| connects(conjunct, &outer_scope, &first_table));
        // A lookup over a single table is the hash index's.
        if matches!(shape, Shape::First(_)) && source.joins.is_empty() {
            return Err("it is a first-row lookup over one table, which the hash index answers");
        }
        let kept = if first_connects {
            joins.push(inner_join(
                source.relation.clone(),
                and_all(&joined).ok_or(unbound)?,
            ));
            joins.extend(source.joins.iter().cloned());
            kept
        } else if let Some(kept) =
            self.connect_inner_chain(source, &conjuncts, &outer_scope, ctes, &mut joins)
        {
            kept
        } else if joined.is_empty() || matches!(shape, Shape::Rows) {
            // With nothing to join the first inner table on, every outer
            // row would pair with every row of it. The per-row path, which
            // shares answers between equal outer values, is the better plan
            // for that.
            return Err("no equality joins its tables to the outer rows");
        } else {
            joins.push(inner_join(
                source.relation.clone(),
                and_all(&joined).ok_or(unbound)?,
            ));
            joins.extend(source.joins.iter().cloned());
            kept
        };

        let from = TableWithJoins {
            relation: factors[0].clone(),
            joins,
        };
        let rebound = "its form does not bind";
        let (set_bound, kind) = match (shape, projected) {
            (Shape::First(order), Some(projected)) => {
                // The first row of each outer row's ordering: rank the joined
                // rows within their ordinal and keep rank one.
                let filter =
                    and_all(&kept).map_or_else(String::new, |kept| format!(" WHERE {kept}"));
                let ranked = format!(
                    "SELECT `ordinal`, `value` FROM (SELECT {first_ordinal} AS `ordinal`, \
                     {projected} AS `value`, ROW_NUMBER() OVER (PARTITION BY {first_ordinal} \
                     {order}) AS `rank` FROM {from}{filter}) AS `<ranked>` WHERE `rank` = 1"
                );
                let Ok(Statement::Query(ranked)) = crate::parse_statement(&ranked) else {
                    return Err(rebound);
                };
                let set_bound = self
                    .bind_query(&ranked, &scoped_ctes)
                    .map_err(|_| rebound)?;
                if !set_bound.aggregates.is_empty() || !set_bound.group_by.is_empty() {
                    return Err(rebound);
                }
                (set_bound, OuterSetKind::Value)
            }
            (Shape::Aggregate, Some(projected)) => {
                let mut set_query = query.clone();
                let SetExpr::Select(select) = set_query.body.as_mut() else {
                    return Err(rebound);
                };
                select.from = vec![from];
                select.selection = and_all(&kept);
                select.projection = vec![
                    SelectItem::ExprWithAlias {
                        expr: first_ordinal.clone(),
                        alias: Ident::new("ordinal"),
                    },
                    SelectItem::ExprWithAlias {
                        expr: projected.clone(),
                        alias: Ident::new("value"),
                    },
                ];
                select.group_by = GroupByExpr::Expressions(vec![first_ordinal], Vec::new());
                let set_bound = self
                    .bind_query(&set_query, &scoped_ctes)
                    .map_err(|_| rebound)?;
                if set_bound.aggregates.is_empty() || set_bound.group_by.len() != 1 {
                    return Err(rebound);
                }
                (set_bound, OuterSetKind::Value)
            }
            (Shape::Rows, projected) => {
                let mut set_query = query.clone();
                let SetExpr::Select(select) = set_query.body.as_mut() else {
                    return Err(rebound);
                };
                let value = match projected {
                    Some(projected) => projected.clone(),
                    None => crate::parse_expression("1").map_err(|_| rebound)?,
                };
                select.from = vec![from];
                select.selection = and_all(&kept);
                select.projection = vec![
                    SelectItem::ExprWithAlias {
                        expr: first_ordinal,
                        alias: Ident::new("ordinal"),
                    },
                    SelectItem::ExprWithAlias {
                        expr: value,
                        alias: Ident::new("value"),
                    },
                ];
                let set_bound = self
                    .bind_query(&set_query, &scoped_ctes)
                    .map_err(|_| rebound)?;
                if !set_bound.aggregates.is_empty()
                    || !set_bound.group_by.is_empty()
                    || !set_bound.windows.is_empty()
                    || set_bound.distinct
                    || set_bound.limit.is_some()
                {
                    return Err(rebound);
                }
                let kind = if projected.is_some() {
                    OuterSetKind::Members
                } else {
                    OuterSetKind::Presence
                };
                (set_bound, kind)
            }
            (Shape::Aggregate | Shape::First(_), None) => {
                return Err("its select list is not one expression");
            }
        };
        if set_bound.projection.len() != 2
            || set_bound.hidden_sort_columns != 0
            || !set_bound.union_all.is_empty()
            || !set_bound.set_ops.is_empty()
        {
            return Err(rebound);
        }
        Ok(OuterSetQuery {
            query: set_bound,
            kind,
            text: query.to_string(),
            relations: described,
        })
    }

    /// Writes an all-INNER join chain in the order equalities connect it to
    /// the outer rows: each table joins on every conjunct that reads only
    /// it and the relations before it, the first of them an equality with
    /// those relations. Returns the conjuncts no join took, for WHERE;
    /// `None` when a join is not INNER with an ON condition or some table
    /// is connected by no equality.
    fn connect_inner_chain<'a>(
        &self,
        source: &'a TableWithJoins,
        conjuncts: &[&'a Expr],
        outer_scope: &[BoundTable],
        ctes: &[BoundCte],
        joins: &mut Vec<Join>,
    ) -> Option<Vec<&'a Expr>> {
        let mut pool: Vec<Option<&Expr>> = conjuncts.iter().copied().map(Some).collect();
        let mut tables = vec![(
            &source.relation,
            self.bind_table(&source.relation, ctes).ok()?,
        )];
        for join in &source.joins {
            match &join.join_operator {
                JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
                    match constraint {
                        JoinConstraint::On(condition) => {
                            pool.extend(split_and_conjuncts(condition).into_iter().map(Some));
                        }
                        JoinConstraint::None => {}
                        _ => return None,
                    }
                }
                _ => return None,
            }
            tables.push((&join.relation, self.bind_table(&join.relation, ctes).ok()?));
        }
        let mut placed = outer_scope.to_vec();
        let mut ordered = Vec::with_capacity(tables.len());
        while !tables.is_empty() {
            let next = tables.iter().position(|(_, table)| {
                pool.iter()
                    .flatten()
                    .any(|conjunct| connects(conjunct, &placed, table))
            })?;
            let (factor, table) = tables.remove(next);
            placed.push(table);
            let mut taken = Vec::new();
            for slot in &mut pool {
                if let Some(conjunct) = slot
                    && binds(conjunct, &placed)
                {
                    taken.push(*conjunct);
                    *slot = None;
                }
            }
            ordered.push(inner_join(factor.clone(), and_all(&taken)?));
        }
        joins.extend(ordered);
        Some(pool.into_iter().flatten().collect())
    }
}
