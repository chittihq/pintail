//! The set-at-a-time form of a correlated scalar aggregate subquery.
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
//! - **The outer rows must join something.** With no conjunct equating the
//!   first inner table with an outer value there is nothing to join on,
//!   and the form is not built.
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
    OrderByKind, Query, SelectItem, SetExpr, Statement, TableAlias, TableFactor, TableWithJoins,
};

use super::{Binder, BoundCte, and_all, bind_expr, split_and_conjuncts};
use crate::bound::{
    BoundColumn, BoundExpr, BoundExprKind, BoundLimit, BoundQuery, BoundTable, OuterSetQuery,
    OuterSetRelation,
};

/// Name of the ordinal column every virtual relation carries first.
const ORDINAL_COLUMN: &str = "<ordinal>";

impl Binder<'_> {
    /// The set-at-a-time form of `query`, whose per-row binding is `bound`
    /// and whose outer scope is `visible`; `None` when the shape is not one
    /// the form answers exactly.
    #[allow(clippy::too_many_lines)] // one shape check and one rewrite, read top to bottom
    pub(super) fn outer_set_form(
        &self,
        query: &Query,
        ctes: &[BoundCte],
        visible: &[BoundTable],
        bound: &BoundQuery,
    ) -> Option<OuterSetQuery> {
        // A subquery of a subquery is substituted by its parent's per-row
        // execution before it runs; its form would hold stale references.
        if !self.outer_tables.is_empty()
            || !bound.group_by.is_empty()
            || !bound.windows.is_empty()
            || bound.having.is_some()
            || bound.distinct
            || bound.projection.len() != bound.hidden_sort_columns + 1
            || bound.from.len() != 1
            || !bound.union_all.is_empty()
            || !bound.set_ops.is_empty()
            || bound.recursive.is_some()
            || query.with.is_some()
        {
            return None;
        }
        // An ungrouped aggregate is one row per outer row as written. So is
        // the first row of an ordering: `ORDER BY .. LIMIT 1`.
        let first_of = if bound.aggregates.is_empty() {
            let first_row = BoundLimit {
                offset: 0,
                count: 1,
            };
            let order = query.order_by.as_ref()?;
            let OrderByKind::Expressions(keys) = &order.kind else {
                return None;
            };
            // A key that is a constant is a select-list position.
            if bound.limit != Some(first_row)
                || order.interpolate.is_some()
                || keys.iter().any(|key| matches!(key.expr, Expr::Value(_)))
            {
                return None;
            }
            Some(order)
        } else {
            if bound.limit.is_some() || query.order_by.is_some() || query.limit_clause.is_some() {
                return None;
            }
            None
        };
        let SetExpr::Select(inner) = query.body.as_ref() else {
            return None;
        };
        let [
            SelectItem::UnnamedExpr(projected)
            | SelectItem::ExprWithAlias {
                expr: projected, ..
            },
        ] = inner.projection.as_slice()
        else {
            return None;
        };
        let [source] = inner.from.as_slice() else {
            return None;
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
            return None;
        }
        let selection = inner.selection.as_ref()?;

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
        if refused || flow.is_break() || relations.is_empty() {
            return None;
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
                    column_id: u32::try_from(offset + 2).ok()?,
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

        // Conjuncts the first inner table can take as its join condition:
        // they read it and the outer relations and nothing else.
        let conjuncts = split_and_conjuncts(selection);
        let mut joined: Vec<&Expr> = Vec::new();
        let mut kept: Vec<&Expr> = Vec::new();
        let base_scope = factors
            .iter()
            .map(|factor| self.bind_table(factor, &scoped_ctes))
            .chain(std::iter::once(self.bind_table(&source.relation, ctes)))
            .collect::<Result<Vec<_>, _>>()
            .ok();
        for conjunct in conjuncts {
            if base_scope
                .as_ref()
                .is_some_and(|scope| bind_expr(conjunct, scope, None).is_ok())
            {
                joined.push(conjunct);
            } else {
                kept.push(conjunct);
            }
        }
        let inner_join = |relation: TableFactor, condition: Expr| Join {
            relation,
            global: false,
            join_operator: JoinOperator::Inner(JoinConstraint::On(condition)),
        };
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
        // With nothing to join the first inner table on, every outer row
        // would pair with every row of it. The per-row path, which shares
        // answers between equal outer values, is the better plan for that.
        // A lookup over a single table is the hash index's.
        if joined.is_empty() || (first_of.is_some() && source.joins.is_empty()) {
            return None;
        }
        joins.push(inner_join(source.relation.clone(), and_all(&joined)?));
        joins.extend(source.joins.iter().cloned());

        let from = TableWithJoins {
            relation: factors[0].clone(),
            joins,
        };
        let set_bound = if let Some(order) = first_of {
            // The first row of each outer row's ordering: rank the joined
            // rows within their ordinal and keep rank one.
            let filter = and_all(&kept).map_or_else(String::new, |kept| format!(" WHERE {kept}"));
            let ranked = format!(
                "SELECT `ordinal`, `value` FROM (SELECT {first_ordinal} AS `ordinal`, {projected} \
                 AS `value`, ROW_NUMBER() OVER (PARTITION BY {first_ordinal} {order}) AS `rank` \
                 FROM {from}{filter}) AS `<ranked>` WHERE `rank` = 1"
            );
            let Ok(Statement::Query(ranked)) = crate::parse_statement(&ranked) else {
                return None;
            };
            let set_bound = self.bind_query(&ranked, &scoped_ctes).ok()?;
            if !set_bound.aggregates.is_empty() || !set_bound.group_by.is_empty() {
                return None;
            }
            set_bound
        } else {
            let mut set_query = query.clone();
            let SetExpr::Select(select) = set_query.body.as_mut() else {
                return None;
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
            let set_bound = self.bind_query(&set_query, &scoped_ctes).ok()?;
            if set_bound.aggregates.is_empty() || set_bound.group_by.len() != 1 {
                return None;
            }
            set_bound
        };
        if set_bound.projection.len() != 2
            || set_bound.hidden_sort_columns != 0
            || !set_bound.union_all.is_empty()
            || !set_bound.set_ops.is_empty()
        {
            return None;
        }
        Some(OuterSetQuery {
            query: set_bound,
            text: query.to_string(),
            relations: described,
        })
    }
}
