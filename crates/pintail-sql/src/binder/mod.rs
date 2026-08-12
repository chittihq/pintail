mod function;

use function::{
    bind_between, bind_case, bind_cast, bind_convert, bind_in_list, bind_interval_arithmetic,
    bind_like, bind_scalar, bind_scalar_function, bind_window_function,
    ensure_supported_text_collation,
};

use std::{cell::Cell, fmt};

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, Value};
use sqlparser::ast::{
    BinaryOperator, CastKind, CeilFloorKind, DateTimeField, Distinct, DuplicateTreatment, Expr,
    Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, Ident, JoinConstraint,
    JoinOperator, LimitClause, ObjectName, OrderByKind, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, SetOperator, SetQuantifier, Statement, TableFactor,
    TableWithJoins, UnaryOperator, Value as SqlValue, WildcardAdditionalOptions,
};

use crate::bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundFrameBound, BoundFrameOffset, BoundFrom, BoundJoin, BoundJoinKind, BoundLimit,
    BoundOrderKey, BoundProjection, BoundQuery, BoundRecursive, BoundSetOpKind, BoundTable,
    BoundWindow, BoundWindowFrame, BoundWindowOrderKey, DEFAULT_TEXT_COLLATION, DatePart,
    IntervalUnit, ScalarFunction, UnaryOp, WindowFunction,
};

/// Binds parsed SQL against one immutable catalog view.
pub struct Binder<'catalog> {
    catalog: &'catalog CatalogSnapshot,
    current_database: Option<&'catalog str>,
    next_derived_id: Cell<u64>,
    /// Relations visible only to expressions in this query. They are never
    /// added to its FROM plan; their columns carry `outer = true`.
    outer_tables: Vec<BoundTable>,
}

#[derive(Clone)]
struct BoundCte {
    name: String,
    column_names: Vec<String>,
    query: BoundQuery,
    /// Recursive self-reference target: scans of this table read the
    /// working delta instead of inlining the CTE body.
    working: Option<BoundTable>,
}

type SubqueryResolver<'resolver> = dyn Fn(&Query) -> Result<BoundQuery, BindError> + 'resolver;

impl<'catalog> Binder<'catalog> {
    /// Constructs a binder with an optional current database.
    #[must_use]
    pub const fn new(
        catalog: &'catalog CatalogSnapshot,
        current_database: Option<&'catalog str>,
    ) -> Self {
        Self {
            catalog,
            current_database,
            next_derived_id: Cell::new(u64::MAX),
            outer_tables: Vec::new(),
        }
    }

    fn bind_subquery(
        &self,
        query: &Query,
        ctes: &[BoundCte],
        visible_tables: &[BoundTable],
    ) -> Result<BoundQuery, BindError> {
        let mut outer_tables = visible_tables.to_vec();
        for table in &mut outer_tables {
            for column in &mut table.columns {
                column.outer = true;
            }
        }
        let nested = Self {
            catalog: self.catalog,
            current_database: self.current_database,
            next_derived_id: Cell::new(self.next_derived_id.get()),
            outer_tables,
        };
        let result = nested.bind_query(query, ctes);
        self.next_derived_id.set(nested.next_derived_id.get());
        result
    }

    /// Resolves and type-checks a parsed query statement.
    ///
    /// # Errors
    ///
    /// Returns an explicit catalog, ambiguity, type, literal, or unsupported
    /// syntax error. Only query statements are accepted by this entry point.
    pub fn bind(&self, statement: &Statement) -> Result<BoundQuery, BindError> {
        let Statement::Query(query) = statement else {
            return Err(BindError::UnsupportedStatement(statement.to_string()));
        };
        let mut bound = self.bind_query(query, &[])?;
        // Each expression was checked for a single supported collation as it
        // was bound. This resolves the query as a whole, which is the level
        // the executor works at: one comparison rule for the whole plan. A
        // query whose filter is general_ci and whose ORDER BY is 0900_ai_ci
        // passes every per-expression check and still has no single answer.
        bound.text_collation = resolve_query_collation(&bound.source_collations())?;
        Ok(bound)
    }

    fn bind_query(&self, query: &Query, outer_ctes: &[BoundCte]) -> Result<BoundQuery, BindError> {
        if query.fetch.is_some()
            || !query.locks.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return Err(BindError::UnsupportedQueryClause(query.to_string()));
        }

        let mut ctes = outer_ctes.to_vec();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                if cte.from.is_some() || cte.materialized.is_some() {
                    return Err(BindError::UnsupportedQueryClause(cte.to_string()));
                }
                if ctes
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(&cte.alias.name.value))
                {
                    return Err(BindError::DuplicateRelation(cte.alias.name.value.clone()));
                }
                // Under WITH RECURSIVE, a CTE that fails to bind normally
                // (its body references itself) binds as a recursive
                // fixpoint instead.
                let bound = match self.bind_query(&cte.query, &ctes) {
                    Ok(bound) => bound,
                    Err(error) if with.recursive => {
                        self.bind_recursive_cte(cte, &ctes).map_err(|_| error)?
                    }
                    Err(error) => return Err(error),
                };
                if !cte.alias.columns.is_empty()
                    && cte.alias.columns.len() != bound.projection.len()
                {
                    return Err(BindError::IncompatibleSetOperation(format!(
                        "CTE {} declares {} columns but produces {}",
                        cte.alias.name.value,
                        cte.alias.columns.len(),
                        bound.projection.len()
                    )));
                }
                ctes.push(BoundCte {
                    name: cte.alias.name.value.clone(),
                    column_names: cte
                        .alias
                        .columns
                        .iter()
                        .map(|column| column.name.value.clone())
                        .collect(),
                    query: bound,
                    working: None,
                });
            }
        }

        let mut bound = self.bind_set_expr(&query.body, &ctes)?;
        bind_order_by(query, &mut bound)?;
        bound.limit = query.limit_clause.as_ref().map(bind_limit).transpose()?;
        Ok(bound)
    }

    /// Binds one `WITH RECURSIVE` CTE whose body is `anchor UNION [ALL]
    /// member`. The anchor binds without the CTE in scope and fixes the
    /// column layout; the member binds against a synthetic working table
    /// and re-executes per iteration. Canonical restrictions apply —
    /// aggregates, windows, DISTINCT, GROUP BY, ORDER BY, LIMIT, and
    /// nested set chains reject, and the member must scan the working
    /// table exactly once.
    #[allow(clippy::too_many_lines)] // linear anchor->working->member validation sequence
    fn bind_recursive_cte(
        &self,
        cte: &sqlparser::ast::Cte,
        ctes: &[BoundCte],
    ) -> Result<BoundQuery, BindError> {
        let unsupported = || BindError::UnsupportedQueryClause(cte.to_string());
        let query = &cte.query;
        if query.order_by.is_some()
            || query.limit_clause.is_some()
            || query.with.is_some()
            || query.fetch.is_some()
        {
            return Err(unsupported());
        }
        let SetExpr::SetOperation {
            op: SetOperator::Union,
            set_quantifier:
                quantifier @ (SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None),
            left,
            right,
        } = query.body.as_ref()
        else {
            return Err(unsupported());
        };
        let mut anchor = self.bind_set_expr(left, ctes)?;
        if !cte.alias.columns.is_empty() && cte.alias.columns.len() != anchor.projection.len() {
            return Err(unsupported());
        }

        // The working table carries the anchor's layout under the CTE name;
        // recursive scans of it read the previous iteration's delta.
        let table_id = self.next_derived_id.get();
        self.next_derived_id.set(table_id.saturating_sub(1));
        let table_id = pintail_catalog::TableId::new(table_id);
        let database_id = pintail_catalog::DatabaseId::new(u64::MAX);
        let columns = anchor
            .projection
            .iter()
            .enumerate()
            .map(|(index, projection)| BoundColumn {
                database_id,
                table_id,
                column_id: u32::try_from(index + 1).unwrap_or(u32::MAX - 3),
                relation_name: cte.alias.name.value.clone(),
                name: cte.alias.columns.get(index).map_or_else(
                    || projection.name.clone(),
                    |column| column.name.value.clone(),
                ),
                data_type: projection.expr.data_type.unwrap_or(DataType::Utf8),
                nullable: true,
                collation: anchor.result_collation(&projection.expr),
                outer: false,
                using_shadowed: false,
            })
            .collect();
        let working = BoundTable {
            database_id,
            table_id,
            database_name: String::new(),
            table_name: cte.alias.name.value.clone(),
            relation_name: cte.alias.name.value.clone(),
            schema_version: 0,
            columns,
            row_count: None,
            estimated_rows: None,
            key_column_ids: Vec::new(),
            input: None,
        };
        let mut scope = ctes.to_vec();
        scope.push(BoundCte {
            name: cte.alias.name.value.clone(),
            column_names: cte
                .alias
                .columns
                .iter()
                .map(|column| column.name.value.clone())
                .collect(),
            query: anchor.clone(),
            working: Some(working.clone()),
        });

        let member = self.bind_set_expr(right, &scope)?;
        let canonical = member.recursive.is_none()
            && member.union_all.is_empty()
            && member.set_ops.is_empty()
            && !member.union_distinct
            && member.aggregates.is_empty()
            && member.windows.is_empty()
            && !member.distinct
            && member.group_by.is_empty()
            && member.having.is_none()
            && member.order_by.is_empty()
            && member.limit.is_none()
            && member.projection.len() == anchor.projection.len();
        if !canonical {
            return Err(unsupported());
        }
        let references = member
            .tables
            .iter()
            .filter(|table| table.table_id == table_id && table.database_id == database_id)
            .count();
        if references != 1 {
            return Err(unsupported());
        }
        // Iteration deltas append to batches typed by the anchor layout, so
        // member columns must already execute as the same storage type.
        for (anchor_item, member_item) in anchor.projection.iter().zip(&member.projection) {
            let compatible = match (anchor_item.expr.data_type, member_item.expr.data_type) {
                (Some(anchor_type), Some(member_type)) => {
                    anchor_type.storage_type() == member_type.storage_type()
                }
                (Some(_) | None, None) => true,
                (None, Some(_)) => false,
            };
            if !compatible {
                return Err(unsupported());
            }
        }
        if !matches!(quantifier, SetQuantifier::All) && has_json_projection(&anchor) {
            return Err(unsupported());
        }
        anchor.recursive = Some(Box::new(BoundRecursive {
            database_id,
            table_id,
            member,
            distinct: !matches!(quantifier, SetQuantifier::All),
        }));
        Ok(anchor)
    }

    fn bind_set_expr(
        &self,
        expression: &SetExpr,
        ctes: &[BoundCte],
    ) -> Result<BoundQuery, BindError> {
        match expression {
            SetExpr::Select(select) => self.bind_select(select, ctes),
            SetExpr::Query(query) => self.bind_query(query, ctes),
            SetExpr::SetOperation {
                left,
                op: SetOperator::Union,
                set_quantifier:
                    quantifier @ (SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None),
                right,
            } => {
                let mut left = self.bind_set_operand(left, ctes)?;
                let mut right = self.bind_set_operand(right, ctes)?;
                // The flat UNION chain applies one final DISTINCT. Preserve a
                // completed DISTINCT or INTERSECT/EXCEPT left subtree behind
                // an internal derived boundary before appending a later UNION.
                if !left.set_ops.is_empty()
                    || !left.order_by.is_empty()
                    || left.limit.is_some()
                    || matches!(quantifier, SetQuantifier::All) && left.union_distinct
                {
                    left = self.wrap_set_operand(left);
                }
                unify_union_layout(&mut left, &mut right)?;
                if !matches!(quantifier, SetQuantifier::All) {
                    reject_json_set_keys(&left)?;
                    left.union_distinct = true;
                }
                left.union_all.push(right);
                Ok(left)
            }
            SetExpr::SetOperation {
                left,
                op: op @ (SetOperator::Intersect | SetOperator::Except),
                set_quantifier:
                    quantifier @ (SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None),
                right,
            } => {
                let mut left = self.bind_set_operand(left, ctes)?;
                let mut right = self.bind_set_operand(right, ctes)?;
                if !left.order_by.is_empty() || left.limit.is_some() {
                    left = self.wrap_set_operand(left);
                }
                unify_union_layout(&mut left, &mut right)?;
                reject_json_set_keys(&left)?;
                let all = matches!(quantifier, SetQuantifier::All);
                let kind = match (op, all) {
                    (SetOperator::Intersect, false) => BoundSetOpKind::Intersect,
                    (SetOperator::Intersect, true) => BoundSetOpKind::IntersectAll,
                    (_, false) => BoundSetOpKind::Except,
                    (_, true) => BoundSetOpKind::ExceptAll,
                };
                left.set_ops.push((kind, right));
                Ok(left)
            }
            _ => Err(BindError::UnsupportedQueryBody(expression.to_string())),
        }
    }

    fn bind_set_operand(
        &self,
        expression: &SetExpr,
        ctes: &[BoundCte],
    ) -> Result<BoundQuery, BindError> {
        let bound = self.bind_set_expr(expression, ctes)?;
        if matches!(expression, SetExpr::Query(_)) {
            Ok(self.wrap_set_operand(bound))
        } else {
            Ok(bound)
        }
    }

    fn wrap_set_operand(&self, input: BoundQuery) -> BoundQuery {
        let relation = self.bind_derived_table(
            "<set-operand>".to_owned(),
            "<set-operand>".to_owned(),
            &[],
            input,
        );
        let projection = relation
            .columns
            .iter()
            .cloned()
            .map(|column| BoundProjection {
                name: column.name.clone(),
                expr: BoundExpr {
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                    kind: BoundExprKind::Column(column),
                },
            })
            .collect();
        BoundQuery {
            // Overwritten by bind() once the whole query is known.
            text_collation: DEFAULT_TEXT_COLLATION,
            from: vec![BoundFrom {
                base: relation.clone(),
                joins: Vec::new(),
            }],
            tables: vec![relation],
            projection,
            filter: None,
            group_by: Vec::new(),
            aggregates: Vec::new(),
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
        }
    }

    #[allow(clippy::too_many_lines)] // linear clause-binding sequence reads best unsplit
    fn bind_select(&self, select: &Select, ctes: &[BoundCte]) -> Result<BoundQuery, BindError> {
        validate_select_shape(select)?;

        let BoundFromScope {
            mut from,
            mut tables,
            wildcard_order,
        } = self.bind_from(select, ctes)?;
        let filter_scope = expression_scope(&tables, &self.outer_tables);
        let resolve_filter_subquery =
            |query: &Query| self.bind_subquery(query, ctes, &filter_scope);
        // Correlated EXISTS conjuncts decorrelate into semi/anti joins
        // before general binding; everything else re-ANDs below.
        let mut residual_filter: Option<BoundExpr> = None;
        if let Some(selection) = &select.selection {
            for conjunct in split_and_conjuncts(selection) {
                let bound = if let Expr::Exists { subquery, negated } = conjunct
                    && self.bind_query(subquery, ctes).is_err()
                {
                    if self
                        .decorrelate_exists(subquery, *negated, &mut from, &mut tables, ctes)
                        .is_ok()
                    {
                        continue;
                    }
                    bind_expr(conjunct, &filter_scope, Some(&resolve_filter_subquery))?
                } else if let Expr::InSubquery {
                    expr,
                    subquery,
                    negated,
                } = conjunct
                    && self.bind_query(subquery, ctes).is_err()
                {
                    if self
                        .decorrelate_in(expr, subquery, *negated, &mut from, &mut tables, ctes)
                        .is_ok()
                    {
                        continue;
                    }
                    bind_expr(conjunct, &filter_scope, Some(&resolve_filter_subquery))?
                } else {
                    bind_expr(conjunct, &filter_scope, Some(&resolve_filter_subquery))?
                };
                residual_filter = Some(match residual_filter {
                    None => bound,
                    Some(existing) => BoundExpr {
                        data_type: Some(DataType::Boolean),
                        nullable: existing.nullable || bound.nullable,
                        kind: BoundExprKind::Binary {
                            op: BinaryOp::And,
                            left: Box::new(existing),
                            right: Box::new(bound),
                        },
                    },
                });
            }
        }
        let filter = residual_filter;
        if let Some(filter) = &filter
            && !is_truth_value(filter.data_type)
        {
            return Err(BindError::ExpectedPredicate {
                actual: filter.data_type,
            });
        }
        // Correlated scalar subqueries in the select list decorrelate into
        // LEFT JOINs for per-key aggregates and unique-key lookups;
        // non-canonical shapes keep their original unsupported error.
        let mut projection_items: Vec<SelectItem> = select.projection.clone();
        // `OVER w` resolves against the WINDOW clause before anything is
        // bound. Substituting here keeps the named-window map out of the
        // expression walk, which would otherwise have to thread it through
        // every recursive call to reach the one place that reads it.
        if !select.named_window.is_empty() {
            for item in &mut projection_items {
                let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) = item
                else {
                    continue;
                };
                substitute_named_windows(expr, &select.named_window)?;
            }
        }
        let mut zero_folds: Vec<String> = Vec::new();
        for item in &mut projection_items {
            let (original, alias) = match item {
                SelectItem::UnnamedExpr(expr) => (expr.clone(), None),
                SelectItem::ExprWithAlias { expr, alias } => (expr.clone(), Some(alias.clone())),
                _ => continue,
            };
            let Expr::Subquery(subquery) = &original else {
                continue;
            };
            if self.bind_query(subquery, ctes).is_ok() {
                continue;
            }
            let Ok((replacement, counts)) =
                self.decorrelate_scalar(subquery, &mut from, &mut tables, ctes)
            else {
                continue;
            };
            if counts
                && let Expr::CompoundIdentifier(identifiers) = &replacement
                && let Some(relation) = identifiers.first()
            {
                zero_folds.push(relation.value.clone());
            }
            let alias = alias.unwrap_or_else(|| Ident::new(projection_name(&original)));
            *item = SelectItem::ExprWithAlias {
                expr: replacement,
                alias,
            };
        }
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let expression_tables = expression_scope(&tables, &self.outer_tables);
        let resolve_subquery = |query: &Query| self.bind_subquery(query, ctes, &expression_tables);
        let mut projection = bind_projection(
            &projection_items,
            &tables,
            &expression_tables,
            &wildcard_order,
            Some(&mut aggregates),
            Some(&mut windows),
            Some(&resolve_subquery),
        )?;
        // A missing group leaves scalar COUNT at NULL through the LEFT
        // JOIN; MySQL returns 0 there.
        for relation in &zero_folds {
            for item in &mut projection {
                let folds = matches!(&item.expr.kind, BoundExprKind::Column(column)
                    if column.relation_name == *relation && column.name == SCALAR_VALUE_COLUMN);
                if folds {
                    let inner = item.expr.clone();
                    item.expr = BoundExpr {
                        data_type: inner.data_type,
                        nullable: false,
                        kind: BoundExprKind::Scalar {
                            function: ScalarFunction::Coalesce,
                            args: vec![
                                inner,
                                BoundExpr {
                                    data_type: Some(DataType::Int64),
                                    nullable: false,
                                    kind: BoundExprKind::Literal(Value::Int64(0)),
                                },
                            ],
                        },
                    };
                }
            }
        }
        let group_by = bind_group_by(
            &select.group_by,
            &projection_items,
            &expression_tables,
            Some(&resolve_subquery),
        )?;
        let mut having = select
            .having
            .as_ref()
            .map(|expr| {
                bind_aggregate_expr(
                    expr,
                    &expression_tables,
                    &mut aggregates,
                    Some(&resolve_subquery),
                )
            })
            .transpose()?;
        if let Some(predicate) = &having
            && !is_truth_value(predicate.data_type)
        {
            return Err(BindError::ExpectedPredicate {
                actual: predicate.data_type,
            });
        }
        if !windows.is_empty() && select.distinct.is_some() {
            return Err(BindError::UnsupportedQueryClause(
                "window functions cannot combine with DISTINCT".to_owned(),
            ));
        }
        // `ANY_VALUE` is not an aggregate in MySQL — it is a passthrough that
        // exempts its argument from the ONLY_FULL_GROUP_BY check. So a query
        // whose only aggregate-looking call is `ANY_VALUE`, with no GROUP BY,
        // is not aggregated at all and yields one row per input row.
        // Treating it as an aggregate turned `SELECT ANY_VALUE(name) FROM t
        // WHERE <no matches>` into one NULL row where MySQL returns none.
        if group_by.is_empty()
            && !aggregates.is_empty()
            && aggregates
                .iter()
                .all(|aggregate| aggregate.function == AggregateFunction::AnyValue)
        {
            let inlined = aggregates
                .iter()
                .map(|aggregate| aggregate.expr.clone())
                .collect::<Vec<_>>();
            for item in &mut projection {
                inline_any_value(&mut item.expr, &inlined)?;
            }
            if let Some(predicate) = &mut having {
                inline_any_value(predicate, &inlined)?;
            }
            aggregates.clear();
        }
        if !group_by.is_empty() || !aggregates.is_empty() {
            for item in &mut projection {
                rewrite_group_references(&mut item.expr, &group_by)?;
            }
            if let Some(predicate) = &mut having {
                rewrite_group_references(predicate, &group_by)?;
            }
            // Windows evaluate above the aggregation, so their arguments,
            // PARTITION BY, and ORDER BY re-express in terms of group keys
            // and aggregate outputs (q07's share-of-category shape).
            for window in &mut windows {
                if let WindowFunction::Aggregate(aggregate) = &mut window.function
                    && let Some(expr) = &mut aggregate.expr
                {
                    rewrite_group_references(expr, &group_by)?;
                }
                for expr in &mut window.partition_by {
                    rewrite_group_references(expr, &group_by)?;
                }
                for key in &mut window.order_by {
                    rewrite_group_references(&mut key.expr, &group_by)?;
                }
            }
        } else if having.is_some() {
            return Err(BindError::InvalidGrouping(
                "HAVING requires GROUP BY or an aggregate".to_owned(),
            ));
        }
        let distinct = match select.distinct {
            None | Some(Distinct::All) => false,
            Some(Distinct::Distinct) => true,
            Some(Distinct::On(_)) => {
                return Err(BindError::UnsupportedQueryClause("DISTINCT ON".to_owned()));
            }
        };
        if distinct
            && projection
                .iter()
                .any(|item| item.expr.data_type == Some(DataType::Json))
        {
            return Err(BindError::UnsupportedQueryClause(
                "DISTINCT over JSON values requires JSON-aware equality".to_owned(),
            ));
        }
        if distinct {
            for item in &projection {
                ensure_supported_text_collation(&[&item.expr])?;
            }
        }
        Ok(BoundQuery {
            // Overwritten by bind() once the whole query is known.
            text_collation: DEFAULT_TEXT_COLLATION,
            from,
            tables,
            projection,
            filter,
            group_by,
            aggregates,
            having,
            distinct,
            order_by: Vec::new(),
            hidden_sort_columns: 0,
            union_all: Vec::new(),
            union_distinct: false,
            set_ops: Vec::new(),
            recursive: None,
            windows,
            limit: None,
        })
    }

    /// Rewrites a correlated `[NOT] EXISTS` conjunct into a semi/anti join
    /// when the subquery is the canonical single-table, single-equality
    /// form; anything else keeps the original unsupported-subquery error.
    fn decorrelate_exists(
        &self,
        subquery: &Query,
        negated: bool,
        from: &mut [BoundFrom],
        tables: &mut Vec<BoundTable>,
        ctes: &[BoundCte],
    ) -> Result<(), BindError> {
        let unsupported = || BindError::UnsupportedSubquery(subquery.to_string());
        let SetExpr::Select(inner) = subquery.body.as_ref() else {
            return Err(unsupported());
        };
        let simple = inner.from.len() == 1
            && inner.from[0].joins.is_empty()
            && matches!(inner.group_by, GroupByExpr::Expressions(ref exprs, _) if exprs.is_empty())
            && inner.having.is_none()
            && subquery.limit_clause.is_none()
            && subquery.order_by.is_none();
        if !simple {
            return Err(unsupported());
        }
        let probe_table = self.bind_table(&inner.from[0].relation, ctes)?;
        if tables.iter().any(|existing| {
            existing
                .relation_name
                .eq_ignore_ascii_case(&probe_table.relation_name)
        }) {
            return Err(unsupported());
        }
        let Some(selection) = &inner.selection else {
            return Err(unsupported());
        };
        // Conjuncts that bind against the inner table alone stay inside a
        // derived filtered input; exactly one leftover conjunct must be the
        // correlation equality.
        let probe_scope = vec![probe_table.clone()];
        let mut inner_only: Vec<&Expr> = Vec::new();
        let mut correlated: Vec<&Expr> = Vec::new();
        for conjunct in split_and_conjuncts(selection) {
            if bind_expr(conjunct, &probe_scope, None).is_ok() {
                inner_only.push(conjunct);
            } else {
                correlated.push(conjunct);
            }
        }
        if correlated.is_empty() {
            return Err(unsupported());
        }
        let inner_table = if inner_only.is_empty() {
            probe_table
        } else {
            self.filtered_join_input(subquery, &probe_table, &inner_only, ctes)
                .ok_or_else(unsupported)?
        };
        tables.push(inner_table.clone());
        // Every correlated conjunct must be an equality spanning exactly
        // the inner table and the outer scope; together they become the
        // (possibly multi-key) join condition.
        let inner_key = (inner_table.database_id, inner_table.table_id);
        let mut condition: Option<BoundExpr> = None;
        for conjunct in correlated {
            let bound = bind_expr(conjunct, tables, None).map_err(|_| {
                tables.pop();
                unsupported()
            })?;
            if !is_correlation_equality(&bound, inner_key) {
                tables.pop();
                return Err(unsupported());
            }
            condition = Some(match condition {
                None => bound,
                Some(existing) => and_bound(existing, bound),
            });
        }
        let condition = condition.expect("correlated conjuncts are non-empty");
        let Some(last) = from.last_mut() else {
            tables.pop();
            return Err(unsupported());
        };
        last.joins.push(BoundJoin {
            kind: if negated {
                BoundJoinKind::Anti
            } else {
                BoundJoinKind::Semi
            },
            table: inner_table,
            condition: Some(condition),
        });
        Ok(())
    }

    /// Rewrites a correlated `IN`/`NOT IN` conjunct as a semi/anti join.
    /// The projected inner expression pairs with the outer operand as one
    /// join equality; correlated inner conjuncts add further equalities and
    /// inner-only conjuncts stay in a derived filtered input. `NOT IN`
    /// additionally requires both membership sides to be non-nullable —
    /// with a possible NULL on either side, `MySQL`'s three-valued `NOT IN`
    /// diverges from an anti join.
    #[allow(clippy::too_many_lines)] // linear canonical-shape validation reads best unsplit
    fn decorrelate_in(
        &self,
        outer: &Expr,
        subquery: &Query,
        negated: bool,
        from: &mut [BoundFrom],
        tables: &mut Vec<BoundTable>,
        ctes: &[BoundCte],
    ) -> Result<(), BindError> {
        let unsupported = || BindError::UnsupportedSubquery(subquery.to_string());
        let SetExpr::Select(inner) = subquery.body.as_ref() else {
            return Err(unsupported());
        };
        let simple = inner.from.len() == 1
            && inner.from[0].joins.is_empty()
            && matches!(inner.group_by, GroupByExpr::Expressions(ref exprs, _) if exprs.is_empty())
            && inner.having.is_none()
            && subquery.limit_clause.is_none()
            && subquery.order_by.is_none();
        if !simple {
            return Err(unsupported());
        }
        let [
            SelectItem::UnnamedExpr(projected)
            | SelectItem::ExprWithAlias {
                expr: projected, ..
            },
        ] = inner.projection.as_slice()
        else {
            return Err(unsupported());
        };
        let probe_table = self.bind_table(&inner.from[0].relation, ctes)?;
        if tables.iter().any(|existing| {
            existing
                .relation_name
                .eq_ignore_ascii_case(&probe_table.relation_name)
        }) {
            return Err(unsupported());
        }
        let probe_scope = vec![probe_table.clone()];
        let mut inner_only: Vec<&Expr> = Vec::new();
        let mut correlated: Vec<&Expr> = Vec::new();
        if let Some(selection) = &inner.selection {
            for conjunct in split_and_conjuncts(selection) {
                if bind_expr(conjunct, &probe_scope, None).is_ok() {
                    inner_only.push(conjunct);
                } else {
                    correlated.push(conjunct);
                }
            }
        }
        let inner_table = if inner_only.is_empty() {
            probe_table
        } else {
            self.filtered_join_input(subquery, &probe_table, &inner_only, ctes)
                .ok_or_else(unsupported)?
        };
        let inner_key = (inner_table.database_id, inner_table.table_id);
        // The outer operand must not reference the inner relation; the
        // projected expression must reference only the inner relation.
        let outer_value = bind_expr(outer, tables, None)?;
        if expr_tables(&outer_value).contains(&inner_key) {
            return Err(unsupported());
        }
        tables.push(inner_table.clone());
        let projected_value = bind_expr(projected, tables, None).map_err(|_| {
            tables.pop();
            unsupported()
        })?;
        let projected_tables = expr_tables(&projected_value);
        if projected_tables.is_empty() || projected_tables.iter().any(|key| *key != inner_key) {
            tables.pop();
            return Err(unsupported());
        }
        if negated && (outer_value.nullable || projected_value.nullable) {
            tables.pop();
            return Err(unsupported());
        }
        let membership = BoundExpr {
            data_type: Some(DataType::Boolean),
            nullable: outer_value.nullable || projected_value.nullable,
            kind: BoundExprKind::Binary {
                op: BinaryOp::Equal,
                left: Box::new(outer_value),
                right: Box::new(projected_value),
            },
        };
        let mut condition = membership;
        for conjunct in correlated {
            let bound = bind_expr(conjunct, tables, None).map_err(|_| {
                tables.pop();
                unsupported()
            })?;
            if !is_correlation_equality(&bound, inner_key) {
                tables.pop();
                return Err(unsupported());
            }
            condition = and_bound(condition, bound);
        }
        let Some(last) = from.last_mut() else {
            tables.pop();
            return Err(unsupported());
        };
        last.joins.push(BoundJoin {
            kind: if negated {
                BoundJoinKind::Anti
            } else {
                BoundJoinKind::Semi
            },
            table: inner_table,
            condition: Some(condition),
        });
        Ok(())
    }

    /// Rewrites one correlated scalar subquery from the select list into a
    /// scalar or LEFT JOIN. Aggregate forms group by their correlation keys;
    /// non-aggregate lookups use scalar-join semantics so the executor raises
    /// `MySQL`'s cardinality error if an outer row matches more than one row.
    /// Returns the replacement expression and whether the caller must fold
    /// NULL to zero (COUNT over an absent group).
    #[allow(clippy::too_many_lines)] // linear canonical-shape validation reads best unsplit
    fn decorrelate_scalar(
        &self,
        subquery: &Query,
        from: &mut [BoundFrom],
        tables: &mut Vec<BoundTable>,
        ctes: &[BoundCte],
    ) -> Result<(Expr, bool), BindError> {
        let unsupported = || BindError::UnsupportedSubquery(subquery.to_string());
        let SetExpr::Select(inner) = subquery.body.as_ref() else {
            return Err(unsupported());
        };
        let simple = inner.from.len() == 1
            && inner.from[0].joins.is_empty()
            && matches!(inner.group_by, GroupByExpr::Expressions(ref exprs, _) if exprs.is_empty())
            && inner.having.is_none()
            && inner.distinct.is_none()
            && subquery.limit_clause.is_none()
            && subquery.order_by.is_none();
        if !simple {
            return Err(unsupported());
        }
        let [
            SelectItem::UnnamedExpr(projected)
            | SelectItem::ExprWithAlias {
                expr: projected, ..
            },
        ] = inner.projection.as_slice()
        else {
            return Err(unsupported());
        };
        let function_name = match projected {
            Expr::Function(function) => Some(function.name.to_string().to_ascii_uppercase()),
            _ => None,
        };
        let aggregate = function_name
            .as_deref()
            .is_some_and(|name| matches!(name, "COUNT" | "SUM" | "MIN" | "MAX" | "AVG"));
        let counts = function_name.as_deref() == Some("COUNT");
        let probe_table = self.bind_table(&inner.from[0].relation, ctes)?;
        if tables.iter().any(|existing| {
            existing
                .relation_name
                .eq_ignore_ascii_case(&probe_table.relation_name)
        }) {
            return Err(unsupported());
        }
        let Some(selection) = &inner.selection else {
            return Err(unsupported());
        };
        let probe_scope = vec![probe_table.clone()];
        if !aggregate && bind_expr(projected, &probe_scope, None).is_err() {
            return Err(unsupported());
        }
        let mut inner_only: Vec<&Expr> = Vec::new();
        let mut keys: Vec<(String, &Expr)> = Vec::new();
        for conjunct in split_and_conjuncts(selection) {
            if bind_expr(conjunct, &probe_scope, None).is_ok() {
                inner_only.push(conjunct);
                continue;
            }
            let Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } = conjunct
            else {
                return Err(unsupported());
            };
            let inner_column = |expr: &Expr| -> Option<String> {
                let bound = bind_expr(expr, &probe_scope, None).ok()?;
                match bound.kind {
                    BoundExprKind::Column(column) => Some(column.name),
                    _ => None,
                }
            };
            let (key, outer_side) = if let Some(name) = inner_column(left) {
                (name, right.as_ref())
            } else if let Some(name) = inner_column(right) {
                (name, left.as_ref())
            } else {
                return Err(unsupported());
            };
            if bind_expr(outer_side, tables, None).is_err()
                || keys
                    .iter()
                    .any(|(existing, _)| existing.eq_ignore_ascii_case(&key))
            {
                return Err(unsupported());
            }
            keys.push((key, outer_side));
        }
        if keys.is_empty() {
            return Err(unsupported());
        }
        let mut derived_query = subquery.clone();
        let SetExpr::Select(derived_select) = derived_query.body.as_mut() else {
            return Err(unsupported());
        };
        derived_select.projection = keys
            .iter()
            .map(|(name, _)| SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(name.clone()))))
            .chain(std::iter::once(SelectItem::ExprWithAlias {
                expr: projected.clone(),
                alias: Ident::new(SCALAR_VALUE_COLUMN),
            }))
            .collect();
        derived_select.selection = inner_only
            .iter()
            .map(|conjunct| (*conjunct).clone())
            .reduce(|left, right| Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(right),
            });
        if aggregate {
            derived_select.group_by = GroupByExpr::Expressions(
                keys.iter()
                    .map(|(name, _)| Expr::Identifier(Ident::new(name.clone())))
                    .collect(),
                Vec::new(),
            );
        }
        let alias = format!("__scalar_{}", tables.len());
        let input = self.bind_query(&derived_query, ctes)?;
        let derived = self.bind_derived_table(alias.clone(), alias.clone(), &[], input);
        tables.push(derived.clone());
        let condition_ast = keys
            .iter()
            .map(|(name, outer_side)| Expr::BinaryOp {
                left: Box::new(Expr::CompoundIdentifier(vec![
                    Ident::new(alias.clone()),
                    Ident::new(name.clone()),
                ])),
                op: BinaryOperator::Eq,
                right: Box::new((*outer_side).clone()),
            })
            .reduce(|left, right| Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(right),
            })
            .expect("correlation keys are non-empty");
        let condition = match bind_expr(&condition_ast, tables, None) {
            Ok(condition) => condition,
            Err(error) => {
                tables.pop();
                return Err(error);
            }
        };
        let Some(last) = from.last_mut() else {
            tables.pop();
            return Err(unsupported());
        };
        last.joins.push(BoundJoin {
            kind: if aggregate {
                BoundJoinKind::Left
            } else {
                BoundJoinKind::Scalar
            },
            table: derived,
            condition: Some(condition),
        });
        Ok((
            Expr::CompoundIdentifier(vec![Ident::new(alias), Ident::new(SCALAR_VALUE_COLUMN)]),
            counts,
        ))
    }

    /// Rebinds a decorrelated subquery as a derived relation that keeps its
    /// inner-only conjuncts as a filter, exposing every inner column under
    /// the original relation name so the correlation equality still binds.
    fn filtered_join_input(
        &self,
        subquery: &Query,
        probe_table: &BoundTable,
        inner_only: &[&Expr],
        ctes: &[BoundCte],
    ) -> Option<BoundTable> {
        let filter = inner_only
            .iter()
            .map(|conjunct| (*conjunct).clone())
            .reduce(|left, right| Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(right),
            })?;
        let mut derived_query = subquery.clone();
        let SetExpr::Select(derived_select) = derived_query.body.as_mut() else {
            return None;
        };
        derived_select.projection =
            vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())];
        derived_select.selection = Some(filter);
        let input = self.bind_query(&derived_query, ctes).ok()?;
        Some(self.bind_derived_table(
            probe_table.relation_name.clone(),
            probe_table.relation_name.clone(),
            &[],
            input,
        ))
    }

    #[allow(clippy::too_many_lines)] // linear join-constraint dispatch reads best unsplit
    fn bind_from(&self, select: &Select, ctes: &[BoundCte]) -> Result<BoundFromScope, BindError> {
        self.bind_from_items(&select.from, ctes)
    }

    #[allow(clippy::too_many_lines)] // linear join-constraint dispatch reads best unsplit
    fn bind_from_items(
        &self,
        from_items: &[TableWithJoins],
        ctes: &[BoundCte],
    ) -> Result<BoundFromScope, BindError> {
        let mut from = Vec::with_capacity(from_items.len());
        let mut tables = Vec::new();
        // Column order an unqualified `*` expands to. USING/NATURAL joins
        // reorder it per the standard MySQL follows: join columns first
        // (left occurrence), then the remaining left columns, then the
        // remaining right columns.
        let mut wildcard_order: Vec<BoundColumn> = Vec::new();
        for table_with_joins in from_items {
            let flattened = flatten_parenthesized_root_joins(table_with_joins)?;
            let table_with_joins = flattened.as_ref().unwrap_or(table_with_joins);
            // MySQL RIGHT JOIN is LEFT JOIN with swapped inputs; the linear
            // join chain expresses the two-table form directly. RIGHT JOINs
            // inside longer chains keep rejecting in bind_join_operator.
            let flipped = if let [join] = table_with_joins.joins.as_slice()
                && let JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) =
                    &join.join_operator
            {
                Some(sqlparser::ast::TableWithJoins {
                    relation: join.relation.clone(),
                    joins: vec![sqlparser::ast::Join {
                        relation: table_with_joins.relation.clone(),
                        global: false,
                        join_operator: JoinOperator::LeftOuter(constraint.clone()),
                    }],
                })
            } else {
                None
            };
            let table_with_joins = flipped.as_ref().unwrap_or(table_with_joins);
            let base = self.bind_table(&table_with_joins.relation, ctes)?;
            reject_duplicate_relation(&tables, &base)?;
            tables.push(base.clone());
            let mut item_wildcard: Vec<BoundColumn> = base.columns.clone();

            let mut joins = Vec::with_capacity(table_with_joins.joins.len());
            for join in &table_with_joins.joins {
                if join.global {
                    return Err(BindError::UnsupportedQueryClause(join.to_string()));
                }
                let (kind, constraint) = bind_join_operator(&join.join_operator)?;
                let mut relation = self.bind_join_relation(&join.relation, ctes)?;
                for visible in &relation.tables {
                    reject_duplicate_relation(&tables, visible)?;
                }
                if matches!(kind, BoundJoinKind::Left | BoundJoinKind::Scalar) {
                    for column in &mut relation.table.columns {
                        column.nullable = true;
                    }
                    for column in &mut relation.wildcard_order {
                        column.nullable = true;
                    }
                    for visible in &mut relation.tables {
                        for column in &mut visible.columns {
                            column.nullable = true;
                        }
                    }
                }
                tables.extend(relation.tables);
                let table = relation.table;
                let condition = match constraint {
                    JoinConstraint::On(condition) => {
                        item_wildcard.extend(relation.wildcard_order.iter().cloned());
                        let join_scope = expression_scope(&tables, &self.outer_tables);
                        let resolve_subquery =
                            |query: &Query| self.bind_subquery(query, ctes, &join_scope);
                        Some(bind_expr(condition, &join_scope, Some(&resolve_subquery))?)
                    }
                    JoinConstraint::None if kind == BoundJoinKind::Cross => {
                        item_wildcard.extend(relation.wildcard_order.iter().cloned());
                        None
                    }
                    JoinConstraint::None if kind == BoundJoinKind::Inner => {
                        item_wildcard.extend(relation.wildcard_order.iter().cloned());
                        None
                    }
                    JoinConstraint::Using(names) if kind != BoundJoinKind::Cross => {
                        let columns = names
                            .iter()
                            .map(|name| {
                                let parts = object_name_parts(name)?;
                                match parts.as_slice() {
                                    [column] => Ok((*column).to_owned()),
                                    _ => Err(BindError::UnsupportedJoinConstraint(format!(
                                        "USING({name})"
                                    ))),
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Some(Self::bind_using_join(
                            &columns,
                            &table,
                            &mut item_wildcard,
                            &mut tables,
                        )?)
                    }
                    JoinConstraint::Natural if kind != BoundJoinKind::Cross => {
                        // NATURAL join columns are the shared names, in the
                        // order the left side exposes them. No shared names
                        // means a plain unconditioned inner join.
                        let columns = item_wildcard
                            .iter()
                            .filter(|column| !column.using_shadowed)
                            .filter(|column| {
                                table
                                    .columns
                                    .iter()
                                    .any(|right| right.name.eq_ignore_ascii_case(&column.name))
                            })
                            .map(|column| column.name.clone())
                            .collect::<Vec<_>>();
                        if columns.is_empty() {
                            item_wildcard.extend(table.columns.iter().cloned());
                            None
                        } else {
                            Some(Self::bind_using_join(
                                &columns,
                                &table,
                                &mut item_wildcard,
                                &mut tables,
                            )?)
                        }
                    }
                    JoinConstraint::Using(_) | JoinConstraint::Natural | JoinConstraint::None => {
                        return Err(BindError::UnsupportedJoinConstraint(format!(
                            "{constraint:?}"
                        )));
                    }
                };
                if let Some(condition) = &condition
                    && !is_truth_value(condition.data_type)
                {
                    return Err(BindError::ExpectedPredicate {
                        actual: condition.data_type,
                    });
                }
                joins.push(BoundJoin {
                    kind,
                    table,
                    condition,
                });
            }
            from.push(BoundFrom { base, joins });
            wildcard_order.append(&mut item_wildcard);
        }
        Ok(BoundFromScope {
            from,
            tables,
            wildcard_order,
        })
    }

    fn bind_join_relation(
        &self,
        factor: &TableFactor,
        ctes: &[BoundCte],
    ) -> Result<BoundJoinRelation, BindError> {
        let TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } = factor
        else {
            let table = self.bind_table(factor, ctes)?;
            return Ok(BoundJoinRelation {
                wildcard_order: table.columns.clone(),
                tables: vec![table.clone()],
                table,
            });
        };
        if alias.is_some() {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        }

        let scope = self.bind_from_items(std::slice::from_ref(table_with_joins), ctes)?;
        if scope
            .from
            .iter()
            .flat_map(|source| &source.joins)
            .any(|join| {
                !matches!(
                    join.kind,
                    BoundJoinKind::Inner | BoundJoinKind::Cross | BoundJoinKind::Left
                )
            })
        {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        }
        let columns = scope
            .tables
            .iter()
            .flat_map(|table| table.columns.iter().cloned())
            .collect::<Vec<_>>();
        let projection = columns
            .iter()
            .cloned()
            .map(|column| BoundProjection {
                name: column.name.clone(),
                expr: BoundExpr {
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                    kind: BoundExprKind::Column(column),
                },
            })
            .collect();
        let input = BoundQuery {
            // Overwritten by bind() once the whole query is known.
            text_collation: DEFAULT_TEXT_COLLATION,
            from: scope.from,
            tables: scope.tables.clone(),
            projection,
            filter: None,
            group_by: Vec::new(),
            aggregates: Vec::new(),
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
        };
        let table_id = self.next_derived_id.get();
        self.next_derived_id.set(table_id.saturating_sub(1));
        let table = BoundTable {
            database_id: pintail_catalog::DatabaseId::new(u64::MAX),
            table_id: pintail_catalog::TableId::new(table_id),
            database_name: String::new(),
            table_name: "<join-group>".to_owned(),
            relation_name: "<join-group>".to_owned(),
            schema_version: 0,
            columns,
            row_count: None,
            estimated_rows: None,
            key_column_ids: Vec::new(),
            input: Some(Box::new(input)),
        };
        Ok(BoundJoinRelation {
            table,
            tables: scope.tables,
            wildcard_order: scope.wildcard_order,
        })
    }

    /// Desugars one `USING`/`NATURAL` join: binds the equality condition
    /// against the left occurrence of every join column, shadows the right
    /// occurrences from unqualified resolution, and reorders the wildcard
    /// expansion to the standard join-columns-first layout.
    fn bind_using_join(
        columns: &[String],
        right: &BoundTable,
        item_wildcard: &mut Vec<BoundColumn>,
        tables: &mut [BoundTable],
    ) -> Result<BoundExpr, BindError> {
        let mut condition: Option<BoundExpr> = None;
        let mut front = Vec::with_capacity(columns.len());
        for name in columns {
            let left_matches = item_wildcard
                .iter()
                .filter(|column| !column.using_shadowed && column.name.eq_ignore_ascii_case(name))
                .cloned()
                .collect::<Vec<_>>();
            let left = match left_matches.as_slice() {
                [column] => column.clone(),
                [] => return Err(BindError::UnknownColumn(name.clone())),
                _ => return Err(BindError::AmbiguousColumn(name.clone())),
            };
            let right_column = right
                .columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(name))
                .cloned()
                .ok_or_else(|| BindError::UnknownColumn(name.clone()))?;
            let equality = Expr::BinaryOp {
                left: Box::new(Expr::CompoundIdentifier(vec![
                    Ident::new(left.relation_name.clone()),
                    Ident::new(left.name.clone()),
                ])),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::CompoundIdentifier(vec![
                    Ident::new(right_column.relation_name.clone()),
                    Ident::new(right_column.name.clone()),
                ])),
            };
            let bound = bind_expr(&equality, tables, None)?;
            condition = Some(match condition {
                None => bound,
                Some(existing) => and_bound(existing, bound),
            });
            // Move the left occurrence to the join-column block up front.
            item_wildcard.retain(|column| {
                !(column.relation_name == left.relation_name && column.name == left.name)
            });
            front.push(left);
        }
        // Shadow the consumed right-side columns in the resolution scope
        // only; the executed join schema is untouched.
        if let Some(bound_right) = tables.last_mut() {
            for column in &mut bound_right.columns {
                if columns
                    .iter()
                    .any(|name| column.name.eq_ignore_ascii_case(name))
                {
                    column.using_shadowed = true;
                }
            }
        }
        front.append(item_wildcard);
        item_wildcard.extend(front);
        item_wildcard.extend(
            right
                .columns
                .iter()
                .filter(|column| {
                    !columns
                        .iter()
                        .any(|name| column.name.eq_ignore_ascii_case(name))
                })
                .cloned(),
        );
        Ok(condition.expect("USING joins carry at least one column"))
    }

    #[allow(clippy::too_many_lines)]
    fn bind_table(&self, factor: &TableFactor, ctes: &[BoundCte]) -> Result<BoundTable, BindError> {
        if let TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample,
        } = factor
        {
            if *lateral || sample.is_some() {
                return Err(BindError::UnsupportedTableFactor(factor.to_string()));
            }
            let alias = alias
                .as_ref()
                .ok_or_else(|| BindError::UnsupportedTableFactor(factor.to_string()))?;
            if !alias.columns.is_empty() || alias.at.is_some() {
                return Err(BindError::UnsupportedTableFactor(factor.to_string()));
            }
            let input = self.bind_query(subquery, ctes)?;
            return Ok(self.bind_derived_table(
                alias.name.value.clone(),
                alias.name.value.clone(),
                &[],
                input,
            ));
        }

        let TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } = factor
        else {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        };
        if args.is_some()
            || !with_hints.is_empty()
            || version.is_some()
            || *with_ordinality
            || !partitions.is_empty()
            || json_path.is_some()
            || sample.is_some()
            || !index_hints.is_empty()
        {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        }
        if let Some(alias) = alias
            && (!alias.columns.is_empty() || alias.at.is_some())
        {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        }

        let parts = object_name_parts(name)?;
        if let [name] = parts.as_slice()
            && let Some(cte) = ctes
                .iter()
                .rev()
                .find(|cte| cte.name.eq_ignore_ascii_case(name))
        {
            let relation_name = alias
                .as_ref()
                .map_or_else(|| cte.name.clone(), |alias| alias.name.value.clone());
            // A recursive self-reference scans the working table rather
            // than inlining the CTE body.
            if let Some(working) = &cte.working {
                let mut table = working.clone();
                table.relation_name.clone_from(&relation_name);
                for column in &mut table.columns {
                    column.relation_name.clone_from(&relation_name);
                }
                return Ok(table);
            }
            return Ok(self.bind_derived_table(
                cte.name.clone(),
                relation_name,
                &cte.column_names,
                cte.query.clone(),
            ));
        }

        let (database, table) = self.resolve_table(name)?;
        let relation_name = alias
            .as_ref()
            .map_or_else(|| table.name().to_owned(), |alias| alias.name.value.clone());
        let columns = table
            .schema()
            .columns()
            .iter()
            .map(|column| BoundColumn {
                database_id: database.id(),
                table_id: table.id(),
                column_id: column.id(),
                relation_name: relation_name.clone(),
                name: column.name().to_owned(),
                data_type: column.data_type(),
                nullable: column.is_nullable(),
                collation: column.collation().map(str::to_owned),
                outer: false,
                using_shadowed: false,
            })
            .collect();

        Ok(BoundTable {
            database_id: database.id(),
            table_id: table.id(),
            database_name: database.name().to_owned(),
            table_name: table.name().to_owned(),
            relation_name,
            schema_version: table.schema().version(),
            columns,
            row_count: table.statistics().row_count(),
            estimated_rows: table.statistics().estimated_row_count(),
            key_column_ids: table.key_column_ids().to_vec(),
            input: None,
        })
    }

    fn bind_derived_table(
        &self,
        table_name: String,
        relation_name: String,
        column_names: &[String],
        input: BoundQuery,
    ) -> BoundTable {
        let table_id = self.next_derived_id.get();
        self.next_derived_id.set(table_id.saturating_sub(1));
        let table_id = pintail_catalog::TableId::new(table_id);
        let database_id = pintail_catalog::DatabaseId::new(u64::MAX);
        let columns = input
            .projection
            .iter()
            .enumerate()
            .map(|(index, projection)| BoundColumn {
                database_id,
                table_id,
                column_id: u32::try_from(index + 1).unwrap_or(u32::MAX - 3),
                relation_name: relation_name.clone(),
                name: column_names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| projection.name.clone()),
                data_type: projection.expr.data_type.unwrap_or(DataType::Utf8),
                nullable: projection.expr.nullable,
                collation: input.result_collation(&projection.expr),
                outer: false,
                using_shadowed: false,
            })
            .collect();
        BoundTable {
            database_id,
            table_id,
            database_name: String::new(),
            table_name,
            relation_name,
            schema_version: 0,
            columns,
            row_count: None,
            estimated_rows: None,
            key_column_ids: Vec::new(),
            input: Some(Box::new(input)),
        }
    }

    fn resolve_table(&self, name: &ObjectName) -> Result<(&DatabaseEntry, &TableEntry), BindError> {
        let parts = object_name_parts(name)?;
        let (database_name, table_name) = match parts.as_slice() {
            [table] => (
                self.current_database.ok_or(BindError::NoCurrentDatabase)?,
                *table,
            ),
            [database, table] => (*database, *table),
            _ => return Err(BindError::InvalidObjectName(name.to_string())),
        };
        let database = self
            .catalog
            .database(database_name)
            .ok_or_else(|| BindError::UnknownDatabase(database_name.to_owned()))?;
        let table = database
            .table(table_name)
            .ok_or_else(|| BindError::UnknownTable {
                database: database.name().to_owned(),
                table: table_name.to_owned(),
            })?;
        Ok((database, table))
    }
}

fn reject_duplicate_relation(tables: &[BoundTable], table: &BoundTable) -> Result<(), BindError> {
    if tables.iter().any(|existing| {
        existing
            .relation_name
            .eq_ignore_ascii_case(&table.relation_name)
    }) {
        Err(BindError::DuplicateRelation(table.relation_name.clone()))
    } else {
        Ok(())
    }
}

fn unify_union_layout(left: &mut BoundQuery, right: &mut BoundQuery) -> Result<(), BindError> {
    if left.projection.len() != right.projection.len() {
        return Err(BindError::IncompatibleSetOperation(
            "UNION ALL branches have different column counts".to_owned(),
        ));
    }
    for (index, (left, right)) in left
        .projection
        .iter_mut()
        .zip(&mut right.projection)
        .enumerate()
    {
        let Some(unified) = unify_union_types(left.expr.data_type, right.expr.data_type) else {
            return Err(BindError::IncompatibleSetOperation(format!(
                "UNION ALL column {} has types {:?} and {:?}",
                index + 1,
                left.expr.data_type,
                right.expr.data_type
            )));
        };
        // A decimal-unified pair must coerce branch VALUES too: decimals
        // execute as canonical text, so an integer branch reaching a
        // decimal-typed consumer would surface as a raw integer.
        if let Some(decimal @ DataType::Decimal { .. }) = unified {
            wrap_in_decimal_cast(&mut left.expr, decimal);
            wrap_in_decimal_cast(&mut right.expr, decimal);
        }
        left.expr.data_type = unified;
        right.expr.data_type = unified;
        let nullable = left.expr.nullable || right.expr.nullable;
        left.expr.nullable = nullable;
        right.expr.nullable = nullable;
    }
    Ok(())
}

fn has_json_projection(query: &BoundQuery) -> bool {
    query
        .projection
        .iter()
        .any(|item| item.expr.data_type == Some(DataType::Json))
}

fn reject_json_set_keys(query: &BoundQuery) -> Result<(), BindError> {
    if has_json_projection(query) {
        Err(BindError::IncompatibleSetOperation(
            "set duplicate handling over JSON values requires JSON-aware equality".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// Wraps a UNION branch expression in a CAST to the unified decimal type
/// unless it already has it (or is a bare NULL, which any type absorbs).
fn wrap_in_decimal_cast(expr: &mut BoundExpr, unified: DataType) {
    if expr.data_type == Some(unified) || matches!(expr.kind, BoundExprKind::Literal(Value::Null)) {
        return;
    }
    let nullable = expr.nullable;
    let inner = std::mem::replace(
        expr,
        BoundExpr {
            kind: BoundExprKind::Literal(Value::Null),
            data_type: None,
            nullable: true,
        },
    );
    *expr = BoundExpr {
        kind: BoundExprKind::Scalar {
            function: ScalarFunction::Cast(unified),
            args: vec![inner],
        },
        data_type: Some(unified),
        nullable,
    };
}

/// MySQL-style result type for a UNION column pair: equal types pass
/// through; numeric families widen (values are already Int64/UInt64/Float64
/// at execution, so widening only fixes the declared metadata). Mixed
/// signed/UInt64 and integer/decimal pairs unify to a decimal wide enough
/// for both sides, exactly as `MySQL` does (`BIGINT` with `BIGINT UNSIGNED`
/// is `DECIMAL(20,0)`). Cross-kind pairs (text vs number, temporal vs
/// number) stay rejected.
// Outer None = incompatible pair; inner None = still untyped (NULL literals
// on both branches). A dedicated enum would just restate Option twice.
#[allow(clippy::option_option, clippy::too_many_lines)]
fn unify_union_types(left: Option<DataType>, right: Option<DataType>) -> Option<Option<DataType>> {
    fn unsigned_rank(data_type: DataType) -> Option<u8> {
        match data_type {
            DataType::UInt8 => Some(0),
            DataType::UInt16 | DataType::Year => Some(1),
            DataType::UInt32 => Some(2),
            DataType::UInt64 => Some(3),
            _ => None,
        }
    }
    fn signed_rank(data_type: DataType) -> Option<u8> {
        match data_type {
            DataType::Boolean | DataType::Int8 => Some(0),
            DataType::Int16 => Some(1),
            DataType::Int32 => Some(2),
            DataType::Int64 => Some(3),
            _ => None,
        }
    }
    fn is_float(data_type: DataType) -> bool {
        matches!(data_type, DataType::Float32 | DataType::Float64)
    }
    fn integer_digits(data_type: DataType) -> Option<u8> {
        match data_type {
            DataType::Boolean | DataType::Int8 | DataType::UInt8 => Some(3),
            DataType::Int16 | DataType::UInt16 => Some(5),
            DataType::Year => Some(4),
            DataType::Int32 | DataType::UInt32 => Some(10),
            DataType::Int64 => Some(19),
            DataType::UInt64 => Some(20),
            _ => None,
        }
    }
    fn is_numeric(data_type: DataType) -> bool {
        unsigned_rank(data_type).is_some()
            || signed_rank(data_type).is_some()
            || is_float(data_type)
            || matches!(data_type, DataType::Decimal { .. })
    }

    let (left, right) = match (left, right) {
        (None, other) | (other, None) => return Some(other),
        (Some(left), Some(right)) => (left, right),
    };
    if left == right {
        return Some(Some(left));
    }
    if let (Some(l), Some(r)) = (unsigned_rank(left), unsigned_rank(right)) {
        return Some(Some(if l >= r { left } else { right }));
    }
    if let (Some(l), Some(r)) = (signed_rank(left), signed_rank(right)) {
        return Some(Some(if l >= r { left } else { right }));
    }
    // Mixed sign: safe as Int64 while the unsigned side fits an i64.
    let mixed_sign = (signed_rank(left).is_some() && unsigned_rank(right).is_some_and(|r| r < 3))
        || (unsigned_rank(left).is_some_and(|l| l < 3) && signed_rank(right).is_some());
    if mixed_sign {
        return Some(Some(DataType::Int64));
    }
    // Signed with UInt64: neither integer type holds both ranges, so MySQL
    // widens to DECIMAL(20,0); the caller casts both branch values.
    if (signed_rank(left).is_some() && right == DataType::UInt64)
        || (left == DataType::UInt64 && signed_rank(right).is_some())
    {
        return Some(Some(DataType::Decimal {
            precision: 20,
            scale: 0,
        }));
    }
    // Integer with decimal: keep the decimal scale and widen the integer
    // part to whichever side needs more digits, as MySQL does.
    let int_decimal = match (left, right) {
        (other, decimal @ DataType::Decimal { .. })
        | (decimal @ DataType::Decimal { .. }, other) => {
            integer_digits(other).map(|digits| (decimal, digits))
        }
        _ => None,
    };
    if let Some((DataType::Decimal { precision, scale }, digits)) = int_decimal {
        return Some(Some(DataType::Decimal {
            precision: (precision - scale)
                .max(digits)
                .saturating_add(scale)
                .min(38),
            scale,
        }));
    }
    if let (
        DataType::Decimal {
            precision: lp,
            scale: ls,
        },
        DataType::Decimal {
            precision: rp,
            scale: rs,
        },
    ) = (left, right)
    {
        // MySQL sizes the result to hold both sides' integer AND fraction
        // digits: max integer part plus max scale.
        let scale = ls.max(rs);
        return Some(Some(DataType::Decimal {
            precision: (lp - ls).max(rp - rs).saturating_add(scale).min(38),
            scale,
        }));
    }
    if is_numeric(left) && is_numeric(right) && (is_float(left) || is_float(right)) {
        return Some(Some(DataType::Float64));
    }
    None
}

fn bind_join_operator(
    operator: &JoinOperator,
) -> Result<(BoundJoinKind, &JoinConstraint), BindError> {
    match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::StraightJoin(constraint) => Ok((BoundJoinKind::Inner, constraint)),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            Ok((BoundJoinKind::Left, constraint))
        }
        JoinOperator::Semi(constraint) | JoinOperator::LeftSemi(constraint) => {
            Ok((BoundJoinKind::Semi, constraint))
        }
        JoinOperator::Anti(constraint) | JoinOperator::LeftAnti(constraint) => {
            Ok((BoundJoinKind::Anti, constraint))
        }
        JoinOperator::CrossJoin(constraint) => Ok((BoundJoinKind::Cross, constraint)),
        JoinOperator::Right(_)
        | JoinOperator::RightOuter(_)
        | JoinOperator::FullOuter(_)
        | JoinOperator::RightSemi(_)
        | JoinOperator::RightAnti(_)
        | JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::AsOf { .. }
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => {
            Err(BindError::UnsupportedJoinOperator(format!("{operator:?}")))
        }
    }
}

/// Removes parentheses around a root left-deep join group that is exactly
/// representable by [`BoundFrom`]. INNER/CROSS/LEFT chains flatten generally:
/// the logical planner lowers the resulting vector in order, preserving the
/// parenthesized group's left-associative row-preservation boundary. A nested
/// group used as a later join's right input still rejects because that needs a
/// bushy join tree rather than [`BoundFrom`]'s linear representation.
fn flatten_parenthesized_root_joins(
    table: &TableWithJoins,
) -> Result<Option<TableWithJoins>, BindError> {
    let TableFactor::NestedJoin {
        table_with_joins: nested,
        alias,
    } = &table.relation
    else {
        return Ok(None);
    };
    if alias.is_some() {
        return Err(BindError::UnsupportedTableFactor(
            table.relation.to_string(),
        ));
    }
    let recursively_flattened = flatten_parenthesized_root_joins(nested)?;
    let mut flattened = recursively_flattened.unwrap_or_else(|| (**nested).clone());
    for join in &flattened.joins {
        if matches!(join.relation, TableFactor::NestedJoin { .. }) {
            return Err(BindError::UnsupportedTableFactor(join.relation.to_string()));
        }
        let (kind, _) = bind_join_operator(&join.join_operator)?;
        if !matches!(
            kind,
            BoundJoinKind::Inner | BoundJoinKind::Cross | BoundJoinKind::Left
        ) {
            return Err(BindError::UnsupportedTableFactor(
                table.relation.to_string(),
            ));
        }
    }
    flattened.joins.extend(table.joins.iter().cloned());
    Ok(Some(flattened))
}

fn validate_select_shape(select: &Select) -> Result<(), BindError> {
    if !select.optimizer_hints.is_empty()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        // A WINDOW clause is resolved by substitution before binding, so it
        // is no longer an unsupported shape.
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
    {
        return Err(BindError::UnsupportedQueryClause(select.to_string()));
    }
    Ok(())
}

fn bind_projection(
    items: &[SelectItem],
    tables: &[BoundTable],
    expression_tables: &[BoundTable],
    wildcard_order: &[BoundColumn],
    mut aggregates: Option<&mut Vec<BoundAggregate>>,
    mut windows: Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<Vec<BoundProjection>, BindError> {
    let mut projection = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let bound = bind_expr_inner(
                    expr,
                    expression_tables,
                    &mut aggregates,
                    &mut windows,
                    subqueries,
                )?;
                projection.push(BoundProjection {
                    name: projection_name(expr),
                    expr: bound,
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let bound = bind_expr_inner(
                    expr,
                    expression_tables,
                    &mut aggregates,
                    &mut windows,
                    subqueries,
                )?;
                projection.push(BoundProjection {
                    name: alias.value.clone(),
                    expr: bound,
                });
            }
            SelectItem::Wildcard(options) => {
                reject_wildcard_options(options)?;
                if tables.is_empty() {
                    return Err(BindError::WildcardWithoutTable);
                }
                // The FROM clause dictates the expansion order (USING and
                // NATURAL joins reorder it); tables joined later for
                // decorrelated subqueries never appear in it.
                projection.extend(
                    wildcard_order
                        .iter()
                        .cloned()
                        .map(|column| BoundProjection {
                            name: column.name.clone(),
                            expr: BoundExpr {
                                data_type: Some(column.data_type),
                                nullable: column.nullable,
                                kind: BoundExprKind::Column(column),
                            },
                        }),
                );
            }
            SelectItem::QualifiedWildcard(kind, options) => {
                reject_wildcard_options(options)?;
                let SelectItemQualifiedWildcardKind::ObjectName(name) = kind else {
                    return Err(BindError::UnsupportedProjection(item.to_string()));
                };
                let table = resolve_wildcard_table(name, tables)?;
                extend_wildcard(&mut projection, table);
            }
            SelectItem::ExprWithAliases { .. } => {
                return Err(BindError::UnsupportedProjection(item.to_string()));
            }
        }
    }
    Ok(projection)
}

fn bind_group_by(
    group_by: &GroupByExpr,
    projection: &[SelectItem],
    tables: &[BoundTable],
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<Vec<BoundExpr>, BindError> {
    let GroupByExpr::Expressions(expressions, modifiers) = group_by else {
        return Err(BindError::UnsupportedQueryClause(group_by.to_string()));
    };
    if !modifiers.is_empty() {
        return Err(BindError::UnsupportedQueryClause(group_by.to_string()));
    }
    let groups = expressions
        .iter()
        .map(|expr| {
            let Expr::Identifier(identifier) = expr else {
                return bind_expr(expr, tables, subqueries);
            };
            let aliases = projection
                .iter()
                .filter_map(|item| match item {
                    SelectItem::ExprWithAlias { expr, alias }
                        if alias.value.eq_ignore_ascii_case(&identifier.value) =>
                    {
                        Some(expr)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            match aliases.as_slice() {
                [alias_expression] => bind_expr(alias_expression, tables, subqueries),
                [] => bind_expr(expr, tables, subqueries),
                _ => Err(BindError::InvalidGrouping(format!(
                    "GROUP BY alias {} is ambiguous",
                    identifier.value
                ))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if groups
        .iter()
        .any(|expression| expression.data_type == Some(DataType::Json))
    {
        return Err(BindError::InvalidGrouping(
            "GROUP BY over JSON values requires JSON-aware equality".to_owned(),
        ));
    }
    for expression in &groups {
        ensure_supported_text_collation(&[expression])?;
    }
    Ok(groups)
}

fn reject_wildcard_options(options: &WildcardAdditionalOptions) -> Result<(), BindError> {
    if options.to_string().is_empty() {
        Ok(())
    } else {
        Err(BindError::UnsupportedProjection(options.to_string()))
    }
}

fn extend_wildcard(projection: &mut Vec<BoundProjection>, table: &BoundTable) {
    projection.extend(table.columns.iter().cloned().map(|column| BoundProjection {
        name: column.name.clone(),
        expr: BoundExpr {
            data_type: Some(column.data_type),
            nullable: column.nullable,
            kind: BoundExprKind::Column(column),
        },
    }));
}

fn resolve_wildcard_table<'a>(
    name: &ObjectName,
    tables: &'a [BoundTable],
) -> Result<&'a BoundTable, BindError> {
    let parts = object_name_parts(name)?;
    let matches = tables
        .iter()
        .filter(|table| match parts.as_slice() {
            [relation] => table.relation_name.eq_ignore_ascii_case(relation),
            [database, table_name] => {
                table.database_name.eq_ignore_ascii_case(database)
                    && table.table_name.eq_ignore_ascii_case(table_name)
            }
            _ => false,
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [table] => Ok(*table),
        [] => Err(BindError::UnknownRelation(name.to_string())),
        _ => Err(BindError::AmbiguousRelation(name.to_string())),
    }
}

fn bind_expr(
    expr: &Expr,
    tables: &[BoundTable],
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let mut aggregates = None;
    let mut windows = None;
    bind_expr_inner(expr, tables, &mut aggregates, &mut windows, subqueries)
}

fn bind_aggregate_expr(
    expr: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Vec<BoundAggregate>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let mut aggregate_context = Some(aggregates);
    bind_expr_inner(expr, tables, &mut aggregate_context, &mut None, subqueries)
}

#[allow(clippy::too_many_lines)]
fn bind_expr_inner(
    expr: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    match expr {
        Expr::Identifier(identifier) => bind_column(std::slice::from_ref(identifier), tables),
        Expr::CompoundIdentifier(identifiers) => bind_column(identifiers, tables),
        Expr::Value(value) => bind_literal(&value.value),
        Expr::Nested(expr) => bind_expr_inner(expr, tables, aggregates, windows, subqueries),
        Expr::Collate {
            expr: inner,
            collation,
        } => {
            let parts = object_name_parts(collation)?;
            let [name] = parts.as_slice() else {
                return Err(BindError::UnsupportedExpression(expr.to_string()));
            };
            if !name.eq_ignore_ascii_case(DEFAULT_TEXT_COLLATION) {
                return Err(BindError::UnsupportedExpression(format!(
                    "COLLATE {name} is unsupported; the initial executable profile is \
                     {DEFAULT_TEXT_COLLATION}"
                )));
            }
            let bound = bind_expr_inner(inner, tables, aggregates, windows, subqueries)?;
            if bound.data_type != Some(DataType::Utf8) {
                return Err(BindError::UnsupportedExpression(expr.to_string()));
            }
            // COLLATE changes coercibility in MySQL. Pintail exposes one
            // executable text profile, so applying that same profile is an
            // execution no-op after validating that the operand does not
            // carry an incompatible source collation.
            ensure_supported_text_collation(&[&bound])?;
            Ok(bound)
        }
        // sqlparser gives CEIL/FLOOR dedicated nodes (for the `TO field`
        // form); only the plain numeric spelling is supported.
        Expr::Extract {
            field, expr: inner, ..
        } => {
            let part = match field {
                DateTimeField::Year => DatePart::Year,
                DateTimeField::Month => DatePart::Month,
                DateTimeField::Day => DatePart::Day,
                DateTimeField::Hour => DatePart::Hour,
                DateTimeField::Minute => DatePart::Minute,
                DateTimeField::Second => DatePart::Second,
                DateTimeField::Quarter => DatePart::Quarter,
                DateTimeField::Week(None) => DatePart::Week,
                _ => return Err(BindError::UnsupportedExpression(expr.to_string())),
            };
            bind_scalar(
                ScalarFunction::DatePart(part),
                vec![bind_expr_inner(
                    inner, tables, aggregates, windows, subqueries,
                )?],
            )
        }
        Expr::Ceil { expr: inner, field } | Expr::Floor { expr: inner, field } => {
            if !matches!(
                field,
                CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
            ) {
                return Err(BindError::UnsupportedExpression(expr.to_string()));
            }
            let function = if matches!(expr, Expr::Ceil { .. }) {
                ScalarFunction::Ceil { decimal: false }
            } else {
                ScalarFunction::Floor { decimal: false }
            };
            bind_scalar(
                function,
                vec![bind_expr_inner(
                    inner, tables, aggregates, windows, subqueries,
                )?],
            )
        }
        Expr::UnaryOp { op, expr } => {
            bind_unary(*op, expr, tables, aggregates, windows, subqueries)
        }
        Expr::BinaryOp { left, op, right } => {
            bind_binary(left, op, right, tables, aggregates, windows, subqueries)
        }
        Expr::IsNull(expr) => bind_is_null(expr, false, tables, aggregates, windows, subqueries),
        Expr::IsNotNull(expr) => bind_is_null(expr, true, tables, aggregates, windows, subqueries),
        // A window call may appear anywhere inside a projection expression
        // (share-of-total arithmetic, CASE arms); scopes without a window
        // list (WHERE, HAVING, aggregate arguments) still reject it.
        Expr::Function(function) if function.over.is_some() => {
            if windows.is_some() {
                bind_window_function(function, tables, aggregates, windows, subqueries)
            } else {
                Err(BindError::UnsupportedExpression(expr.to_string()))
            }
        }
        Expr::Function(function)
            if aggregates.is_some() && aggregate_function_name(function).is_some() =>
        {
            bind_aggregate(function, tables, aggregates, windows, subqueries)
        }
        Expr::Function(function) => {
            bind_scalar_function(function, tables, aggregates, windows, subqueries)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => bind_in_list(
            expr, list, *negated, tables, aggregates, windows, subqueries,
        ),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => bind_between(
            expr, low, high, *negated, tables, aggregates, windows, subqueries,
        ),
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        } => bind_like(
            expr,
            pattern,
            *negated,
            escape_char.as_ref(),
            tables,
            aggregates,
            windows,
            subqueries,
        ),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => bind_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            tables,
            aggregates,
            windows,
            subqueries,
        ),
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            array: false,
            format: None,
        } => bind_cast(expr, data_type, tables, aggregates, windows, subqueries),
        Expr::Convert { .. } => bind_convert(expr, tables, aggregates, windows, subqueries),
        Expr::Substring {
            expr,
            substring_from: Some(from),
            substring_for,
            ..
        } => {
            let mut args = vec![
                bind_expr_inner(expr, tables, aggregates, windows, subqueries)?,
                bind_expr_inner(from, tables, aggregates, windows, subqueries)?,
            ];
            if let Some(length) = substring_for {
                args.push(bind_expr_inner(
                    length, tables, aggregates, windows, subqueries,
                )?);
            }
            bind_scalar(ScalarFunction::Substring, args)
        }
        Expr::Trim {
            trim_where: None,
            trim_what: None,
            expr,
            trim_characters: None,
        } => bind_scalar(
            ScalarFunction::Trim,
            vec![bind_expr_inner(
                expr, tables, aggregates, windows, subqueries,
            )?],
        ),
        Expr::RLike {
            negated,
            expr: inner,
            pattern,
            ..
        } => bind_scalar(
            ScalarFunction::RegexpLike { negated: *negated },
            vec![
                bind_expr_inner(inner, tables, aggregates, windows, subqueries)?,
                bind_expr_inner(pattern, tables, aggregates, windows, subqueries)?,
            ],
        ),
        Expr::Subquery(query) => bind_scalar_subquery(query, subqueries),
        Expr::Exists { subquery, negated } => {
            let resolver =
                subqueries.ok_or_else(|| BindError::UnsupportedSubquery(subquery.to_string()))?;
            let mut query = resolver(subquery)?;
            // Row presence is all that matters; one row decides EXISTS.
            if query.limit.is_none() {
                query.limit = Some(BoundLimit {
                    offset: 0,
                    count: 1,
                });
            }
            Ok(BoundExpr {
                kind: BoundExprKind::ExistsSubquery {
                    query: Box::new(query),
                    negated: *negated,
                },
                data_type: Some(DataType::Boolean),
                nullable: false,
            })
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => bind_in_subquery(
            expr, subquery, *negated, tables, aggregates, windows, subqueries,
        ),
        _ => Err(BindError::UnsupportedExpression(expr.to_string())),
    }
}

fn bind_scalar_subquery(
    query: &Query,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let values = match bind_constant_subquery(query) {
        Ok(values) => values,
        Err(BindError::UnsupportedSubquery(_)) => {
            let resolver =
                subqueries.ok_or_else(|| BindError::UnsupportedSubquery(query.to_string()))?;
            let query = resolver(query)?;
            if query
                .projection
                .len()
                .saturating_sub(query.hidden_sort_columns)
                != 1
            {
                return Err(BindError::UnsupportedSubquery(
                    "scalar subquery must produce exactly one column".to_owned(),
                ));
            }
            let projection = &query.projection[0];
            let data_type = projection.expr.data_type;
            return Ok(BoundExpr {
                kind: BoundExprKind::ScalarSubquery(Box::new(query)),
                data_type,
                nullable: true,
            });
        }
        Err(error) => return Err(error),
    };
    match values.as_slice() {
        [] => Ok(BoundExpr {
            kind: BoundExprKind::Literal(Value::Null),
            data_type: None,
            nullable: true,
        }),
        [value] => Ok(value.clone()),
        _ => Err(BindError::InvalidScalarSubqueryRows(values.len())),
    }
}

fn bind_in_subquery(
    expr: &Expr,
    query: &Query,
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let expr = bind_expr_inner(expr, tables, aggregates, windows, subqueries)?;
    let values = match bind_constant_subquery(query) {
        Ok(values) => values,
        Err(BindError::UnsupportedSubquery(_)) => {
            let resolver =
                subqueries.ok_or_else(|| BindError::UnsupportedSubquery(query.to_string()))?;
            let query = resolver(query)?;
            if query
                .projection
                .len()
                .saturating_sub(query.hidden_sort_columns)
                != 1
            {
                return Err(BindError::UnsupportedSubquery(
                    "IN subquery must produce exactly one column".to_owned(),
                ));
            }
            let projection = &query.projection[0];
            if !comparable(expr.data_type, projection.expr.data_type) {
                return Err(BindError::InvalidScalarFunction("IN subquery".to_owned()));
            }
            return Ok(BoundExpr {
                kind: BoundExprKind::InSubquery {
                    expr: Box::new(expr),
                    query: Box::new(query),
                    negated,
                },
                data_type: Some(DataType::Boolean),
                nullable: true,
            });
        }
        Err(error) => return Err(error),
    };
    let mut args = vec![expr];
    args.extend(values);
    if args[1..]
        .iter()
        .any(|value| !comparable(args[0].data_type, value.data_type))
    {
        return Err(BindError::InvalidScalarFunction("IN subquery".to_owned()));
    }
    bind_scalar(ScalarFunction::InList { negated }, args)
}

fn bind_constant_subquery(query: &Query) -> Result<Vec<BoundExpr>, BindError> {
    if query.with.is_some()
        || query.order_by.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return Err(BindError::UnsupportedSubquery(query.to_string()));
    }
    let mut values = bind_constant_set_expr(&query.body)?;
    if let Some(limit) = &query.limit_clause {
        let limit = bind_limit(limit)?;
        let start = usize::try_from(limit.offset)
            .unwrap_or(usize::MAX)
            .min(values.len());
        let end = start
            .saturating_add(usize::try_from(limit.count).unwrap_or(usize::MAX))
            .min(values.len());
        values = values[start..end].to_vec();
    }
    Ok(values)
}

fn bind_constant_set_expr(expression: &SetExpr) -> Result<Vec<BoundExpr>, BindError> {
    match expression {
        SetExpr::Select(select) if select.from.is_empty() && select.selection.is_none() => {
            validate_select_shape(select)?;
            if select.distinct.is_some() {
                return Err(BindError::UnsupportedSubquery(select.to_string()));
            }
            let [item] = select.projection.as_slice() else {
                return Err(BindError::UnsupportedSubquery(select.to_string()));
            };
            let (SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias {
                expr: expression, ..
            }) = item
            else {
                return Err(BindError::UnsupportedSubquery(select.to_string()));
            };
            let mut aggregates = None;
            Ok(vec![bind_expr_inner(
                expression,
                &[],
                &mut aggregates,
                &mut None,
                None,
            )?])
        }
        SetExpr::SetOperation {
            left,
            op: SetOperator::Union,
            set_quantifier: SetQuantifier::All,
            right,
        } => {
            let mut values = bind_constant_set_expr(left)?;
            values.extend(bind_constant_set_expr(right)?);
            Ok(values)
        }
        SetExpr::Query(query) => bind_constant_subquery(query),
        _ => Err(BindError::UnsupportedSubquery(expression.to_string())),
    }
}

fn bind_column(identifiers: &[Ident], tables: &[BoundTable]) -> Result<BoundExpr, BindError> {
    let mut matches = tables
        .iter()
        .flat_map(|table| &table.columns)
        .filter(|column| match identifiers {
            // USING/NATURAL right-side columns resolve only through a
            // qualified reference; unqualified names see the left side.
            [column_name] => {
                !column.using_shadowed && column.name.eq_ignore_ascii_case(&column_name.value)
            }
            [relation, column_name] => {
                column.relation_name.eq_ignore_ascii_case(&relation.value)
                    && column.name.eq_ignore_ascii_case(&column_name.value)
            }
            [database, table_name, column_name] => {
                let table = tables.iter().find(|table| {
                    table.database_id == column.database_id && table.table_id == column.table_id
                });
                table.is_some_and(|table| {
                    table.database_name.eq_ignore_ascii_case(&database.value)
                        && table.table_name.eq_ignore_ascii_case(&table_name.value)
                        && column.name.eq_ignore_ascii_case(&column_name.value)
                })
            }
            _ => false,
        })
        .collect::<Vec<_>>();

    // SQL name resolution searches the innermost query scope first. An
    // unqualified local column therefore shadows an identically named outer
    // column instead of becoming ambiguous merely because correlation is
    // available.
    if matches.iter().any(|column| !column.outer) {
        matches.retain(|column| !column.outer);
    }

    let column = match matches.as_slice() {
        [column] => (*column).clone(),
        [] => {
            return Err(BindError::UnknownColumn(
                identifiers
                    .iter()
                    .map(|identifier| identifier.value.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
            ));
        }
        _ => {
            return Err(BindError::AmbiguousColumn(
                identifiers
                    .last()
                    .map_or_else(String::new, |identifier| identifier.value.clone()),
            ));
        }
    };

    Ok(BoundExpr {
        data_type: Some(column.data_type),
        nullable: column.nullable,
        kind: BoundExprKind::Column(column),
    })
}

fn bind_literal(value: &SqlValue) -> Result<BoundExpr, BindError> {
    let (value, declared) = match value {
        SqlValue::Null => (Value::Null, None),
        SqlValue::Boolean(value) => (Value::Boolean(*value), None),
        SqlValue::SingleQuotedString(value) => (Value::Utf8(value.clone()), None),
        SqlValue::Number(value, _) => parse_number(value)?,
        _ => return Err(BindError::UnsupportedLiteral(value.to_string())),
    };
    Ok(BoundExpr {
        data_type: declared.or_else(|| value.data_type()),
        nullable: matches!(value, Value::Null),
        kind: BoundExprKind::Literal(value),
    })
}

/// Types a numeric literal the way `MySQL` does.
///
/// An exponent makes the literal approximate (`DOUBLE`); a bare decimal point
/// keeps it exact (`DECIMAL`). Typing every dotted literal as `Float64` meant
/// exact-value rounding never applied to one, so `ROUND(1.005, 2)` answered 1
/// where `MySQL` answers 1.01 — the f64 nearest to 1.005 is below it, and no
/// amount of care in the rounding kernel recovers a digit the carrier lost.
///
/// The declared type rides alongside the value because a decimal travels on
/// the canonical-text carrier, whose own `data_type()` is `Utf8`.
fn parse_number(value: &str) -> Result<(Value, Option<DataType>), BindError> {
    if value.contains(['e', 'E']) {
        return value
            .parse::<f64>()
            .map(|number| (Value::float64(number), Some(DataType::Float64)))
            .map_err(|_| BindError::InvalidNumericLiteral(value.to_owned()));
    }
    if let Some((integer, fraction)) = value.split_once('.') {
        // Reject anything that is not digits with an optional sign, rather
        // than silently accepting a shape the decimal carrier cannot hold.
        let digits = integer.trim_start_matches(['-', '+']);
        if !fraction.chars().all(|character| character.is_ascii_digit())
            || !digits.chars().all(|character| character.is_ascii_digit())
        {
            return Err(BindError::InvalidNumericLiteral(value.to_owned()));
        }
        let scale = u8::try_from(fraction.len())
            .map_err(|_| BindError::InvalidNumericLiteral(value.to_owned()))?
            .min(MAX_DECIMAL_SCALE);
        let precision = u8::try_from(digits.len().saturating_add(fraction.len()))
            .unwrap_or(MAX_DECIMAL_PRECISION)
            .clamp(scale.max(1), MAX_DECIMAL_PRECISION);
        return Ok((
            Value::Utf8(value.to_owned()),
            Some(DataType::Decimal { precision, scale }),
        ));
    }
    if let Ok(parsed) = value.parse::<i64>() {
        return Ok((Value::Int64(parsed), Some(DataType::Int64)));
    }
    value
        .parse::<u64>()
        .map(|parsed| (Value::UInt64(parsed), Some(DataType::UInt64)))
        .map_err(|_| BindError::InvalidNumericLiteral(value.to_owned()))
}

fn bind_unary(
    operator: UnaryOperator,
    expr: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let expr = bind_expr_inner(expr, tables, aggregates, windows, subqueries)?;
    let (op, data_type) = match operator {
        UnaryOperator::Plus if is_numeric(expr.data_type) => (UnaryOp::Plus, expr.data_type),
        UnaryOperator::Minus if is_numeric(expr.data_type) => (UnaryOp::Minus, expr.data_type),
        UnaryOperator::Not if is_truth_value(expr.data_type) => {
            (UnaryOp::Not, Some(DataType::Boolean))
        }
        _ => {
            return Err(BindError::InvalidUnaryType {
                operation: operator.to_string(),
                actual: expr.data_type,
            });
        }
    };
    Ok(BoundExpr {
        nullable: expr.nullable,
        data_type,
        kind: BoundExprKind::Unary {
            op,
            expr: Box::new(expr),
        },
    })
}

fn bind_binary(
    left: &Expr,
    operator: &BinaryOperator,
    right: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    // MySQL's inline interval arithmetic (`expr ± INTERVAL n unit`,
    // `INTERVAL n unit + expr`) is DATE_ADD/DATE_SUB spelled as an operator.
    if matches!(operator, BinaryOperator::Plus | BinaryOperator::Minus) {
        if let Expr::Interval(interval) = right {
            return bind_interval_arithmetic(
                left,
                interval,
                matches!(operator, BinaryOperator::Minus),
                tables,
                aggregates,
                windows,
                subqueries,
            );
        }
        if let (BinaryOperator::Plus, Expr::Interval(interval)) = (operator, left) {
            return bind_interval_arithmetic(
                right, interval, false, tables, aggregates, windows, subqueries,
            );
        }
    }
    let left = bind_expr_inner(left, tables, aggregates, windows, subqueries)?;
    let right = bind_expr_inner(right, tables, aggregates, windows, subqueries)?;
    ensure_binary_collation(operator, &left, &right)?;
    if matches!(operator, BinaryOperator::Arrow | BinaryOperator::LongArrow) {
        return bind_scalar(
            ScalarFunction::JsonExtract {
                unquote: matches!(operator, BinaryOperator::LongArrow),
            },
            vec![left, right],
        );
    }
    let (op, data_type) = match operator {
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo
        | BinaryOperator::MyIntegerDivide
            if is_numeric(left.data_type) && is_numeric(right.data_type) =>
        {
            let op = match operator {
                BinaryOperator::Plus => BinaryOp::Add,
                BinaryOperator::Minus => BinaryOp::Subtract,
                BinaryOperator::Multiply => BinaryOp::Multiply,
                BinaryOperator::Divide => BinaryOp::Divide,
                BinaryOperator::Modulo => BinaryOp::Modulo,
                BinaryOperator::MyIntegerDivide => BinaryOp::IntegerDivide,
                _ => unreachable!("matched arithmetic operators"),
            };
            let result = arithmetic_type(op, left.data_type, right.data_type);
            (op, result)
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
            if comparable(left.data_type, right.data_type) =>
        {
            let op = match operator {
                BinaryOperator::Eq => BinaryOp::Equal,
                BinaryOperator::NotEq => BinaryOp::NotEqual,
                BinaryOperator::Lt => BinaryOp::Less,
                BinaryOperator::LtEq => BinaryOp::LessOrEqual,
                BinaryOperator::Gt => BinaryOp::Greater,
                BinaryOperator::GtEq => BinaryOp::GreaterOrEqual,
                _ => unreachable!("matched comparison operators"),
            };
            (op, Some(DataType::Boolean))
        }
        BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Xor
            if is_truth_value(left.data_type) && is_truth_value(right.data_type) =>
        {
            let op = match operator {
                BinaryOperator::And => BinaryOp::And,
                BinaryOperator::Or => BinaryOp::Or,
                BinaryOperator::Xor => BinaryOp::Xor,
                _ => unreachable!("matched logic operators"),
            };
            (op, Some(DataType::Boolean))
        }
        _ => {
            return Err(BindError::InvalidBinaryTypes {
                operation: operator.to_string(),
                left: left.data_type,
                right: right.data_type,
            });
        }
    };
    if is_exact_decimal_comparison(op, &left, &right) {
        return Ok(bind_exact_decimal_comparison(op, left, right));
    }

    Ok(BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type,
        kind: BoundExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
    })
}

fn is_collation_sensitive_binary(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

fn ensure_binary_collation(
    operator: &BinaryOperator,
    left: &BoundExpr,
    right: &BoundExpr,
) -> Result<(), BindError> {
    if is_collation_sensitive_binary(operator) {
        ensure_supported_text_collation(&[left, right])?;
    }
    Ok(())
}

fn is_exact_decimal_comparison(op: BinaryOp, left: &BoundExpr, right: &BoundExpr) -> bool {
    matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual
    ) && (matches!(left.data_type, Some(DataType::Decimal { .. }))
        || matches!(right.data_type, Some(DataType::Decimal { .. })))
        && left.data_type.and_then(exact_numeric_digits).is_some()
        && right.data_type.and_then(exact_numeric_digits).is_some()
}

fn bind_exact_decimal_comparison(
    op: BinaryOp,
    mut left: BoundExpr,
    mut right: BoundExpr,
) -> BoundExpr {
    if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
        // Keep equality as a binary predicate so the physical planner can
        // still extract equi-join keys. Casting both sides to one scale also
        // gives the text-backed DECIMAL carrier one canonical hash key.
        let (left_scale, left_integer) =
            exact_numeric_digits(left.data_type.expect("typed")).expect("exact numeric comparison");
        let (right_scale, right_integer) = exact_numeric_digits(right.data_type.expect("typed"))
            .expect("exact numeric comparison");
        let scale = left_scale.max(right_scale);
        let unified = DataType::Decimal {
            precision: left_integer
                .max(right_integer)
                .saturating_add(scale)
                .min(MAX_DECIMAL_PRECISION),
            scale,
        };
        wrap_in_decimal_cast(&mut left, unified);
        wrap_in_decimal_cast(&mut right, unified);
        return BoundExpr {
            nullable: left.nullable || right.nullable,
            data_type: Some(DataType::Boolean),
            kind: BoundExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
        };
    }
    BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Scalar {
            function: ScalarFunction::DecimalComparison { op },
            args: vec![left, right],
        },
    }
}

/// Top-level AND conjuncts of a predicate expression.
fn split_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conjuncts = split_and_conjuncts(left);
            conjuncts.extend(split_and_conjuncts(right));
            conjuncts
        }
        Expr::Nested(inner) => split_and_conjuncts(inner),
        other => vec![other],
    }
}

/// Distinct (database, table) pairs referenced by a bound expression.
/// Whether a bound conjunct is an equality spanning exactly the inner
/// relation and the outer scope (a decorrelation join key).
fn is_correlation_equality(
    expr: &BoundExpr,
    inner_key: (pintail_catalog::DatabaseId, pintail_catalog::TableId),
) -> bool {
    let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left,
        right,
    } = &expr.kind
    else {
        return false;
    };
    let sides_split = (expr_tables(left).iter().all(|key| *key == inner_key)
        && expr_tables(right).iter().all(|key| *key != inner_key))
        || (expr_tables(right).iter().all(|key| *key == inner_key)
            && expr_tables(left).iter().all(|key| *key != inner_key));
    sides_split && !expr_tables(left).is_empty() && !expr_tables(right).is_empty()
}

/// Boolean conjunction of two bound predicates.
fn and_bound(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: left.nullable || right.nullable,
        kind: BoundExprKind::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

fn expr_tables(expr: &BoundExpr) -> Vec<(pintail_catalog::DatabaseId, pintail_catalog::TableId)> {
    fn walk(
        expr: &BoundExpr,
        out: &mut Vec<(pintail_catalog::DatabaseId, pintail_catalog::TableId)>,
    ) {
        match &expr.kind {
            BoundExprKind::Column(column) => {
                let key = (column.database_id, column.table_id);
                if !out.contains(&key) {
                    out.push(key);
                }
            }
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                walk(expr, out);
            }
            BoundExprKind::Binary { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            BoundExprKind::Scalar { args, .. } => {
                for argument in args {
                    walk(argument, out);
                }
            }
            BoundExprKind::InSubquery { expr, .. } => walk(expr, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

/// `MOD(a, b)` is the `%` operator spelled as a function.
fn bind_modulo(mut args: Vec<BoundExpr>) -> BoundExpr {
    let right = args.pop().expect("two arguments");
    let left = args.pop().expect("two arguments");
    let data_type = arithmetic_type(BinaryOp::Modulo, left.data_type, right.data_type);
    BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type,
        kind: BoundExprKind::Binary {
            op: BinaryOp::Modulo,
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

fn bind_is_null(
    expr: &Expr,
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let expr = bind_expr_inner(expr, tables, aggregates, windows, subqueries)?;
    Ok(BoundExpr {
        kind: BoundExprKind::IsNull {
            expr: Box::new(expr),
            negated,
        },
        data_type: Some(DataType::Boolean),
        nullable: false,
    })
}

#[allow(clippy::too_many_lines)] // one aggregate-shape table, clearer unsplit
fn bind_aggregate(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    // Aggregate arguments never see a window scope (MySQL rejects
    // SUM(ROW_NUMBER() OVER ...)); the parameter exists for call-site
    // uniformity.
    _windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
    {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    }
    let name = object_name_parts(&function.name)?;
    let [_] = name.as_slice() else {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    };
    let aggregate_function = aggregate_function_name(function)
        .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    };
    // JSON_OBJECTAGG is the one aggregate taking a pair. Rather than widen
    // BoundAggregate to carry two expressions — which would ripple through
    // spill, the two-pass lanes and merging — the pair becomes a single
    // JSON_OBJECT(k, v) per row, reusing its key coercion and escaping. The
    // aggregate then merges those one-member objects.
    let valid_arity = match aggregate_function {
        AggregateFunction::JsonObjectAgg => arguments.args.len() == 2,
        AggregateFunction::GroupConcat => !arguments.args.is_empty(),
        AggregateFunction::Count
            if arguments.duplicate_treatment == Some(DuplicateTreatment::Distinct) =>
        {
            !arguments.args.is_empty()
        }
        _ => arguments.args.len() == 1,
    };
    if !valid_arity {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    }
    // GROUP_CONCAT owns SEPARATOR and an aggregate-local ORDER BY; every
    // other aggregate rejects clauses.
    let mut separator = None;
    let mut order_within = Vec::new();
    for clause in &arguments.clauses {
        if aggregate_function != AggregateFunction::GroupConcat {
            return Err(BindError::UnsupportedAggregate(function.to_string()));
        }
        match clause {
            sqlparser::ast::FunctionArgumentClause::Separator(value) => {
                let sqlparser::ast::ValueWithSpan {
                    value: SqlValue::SingleQuotedString(text),
                    ..
                } = value
                else {
                    return Err(BindError::UnsupportedAggregate(function.to_string()));
                };
                separator = Some(text.clone());
            }
            sqlparser::ast::FunctionArgumentClause::OrderBy(keys) => {
                for key in keys {
                    let bound = bind_expr(&key.expr, tables, subqueries)?;
                    if bound.data_type == Some(DataType::Json) {
                        return Err(BindError::UnsupportedAggregate(format!(
                            "{function}: ORDER BY over JSON requires JSON-aware ordering"
                        )));
                    }
                    ensure_supported_text_collation(&[&bound])?;
                    order_within.push((bound, key.options.asc.unwrap_or(true)));
                }
            }
            _ => return Err(BindError::UnsupportedAggregate(function.to_string())),
        }
    }
    let distinct = match arguments.duplicate_treatment {
        Some(DuplicateTreatment::Distinct) => true,
        None | Some(DuplicateTreatment::All) => false,
    };
    // MySQL's grammar has no JSON_ARRAYAGG(DISTINCT ...).
    if distinct && aggregate_function == AggregateFunction::JsonArrayAgg {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    }
    let expr = if aggregate_function == AggregateFunction::JsonObjectAgg {
        let mut pair = Vec::with_capacity(2);
        for argument in &arguments.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument else {
                return Err(BindError::UnsupportedAggregate(function.to_string()));
            };
            pair.push(bind_expr(expr, tables, subqueries)?);
        }
        Some(bind_scalar(ScalarFunction::JsonObject, pair)?)
    } else if aggregate_function == AggregateFunction::Count && distinct && arguments.args.len() > 1
    {
        let mut values = Vec::with_capacity(arguments.args.len());
        for argument in &arguments.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument else {
                return Err(BindError::UnsupportedAggregate(function.to_string()));
            };
            values.push(bind_expr(expr, tables, subqueries)?);
        }
        // COUNT(DISTINCT a,b,...) ignores a row when ANY component is NULL.
        // A canonical JSON array is an unambiguous composite key; the normal
        // DISTINCT set still applies MySQL text collation to its carrier.
        let mut null_condition = None;
        for value in &values {
            let is_null = BoundExpr {
                kind: BoundExprKind::IsNull {
                    expr: Box::new(value.clone()),
                    negated: false,
                },
                data_type: Some(DataType::Boolean),
                nullable: false,
            };
            null_condition = Some(match null_condition {
                None => is_null,
                Some(left) => BoundExpr {
                    kind: BoundExprKind::Binary {
                        op: BinaryOp::Or,
                        left: Box::new(left),
                        right: Box::new(is_null),
                    },
                    data_type: Some(DataType::Boolean),
                    nullable: false,
                },
            });
        }
        let tuple = bind_scalar(ScalarFunction::JsonArray, values)?;
        let tuple_or_null = bind_scalar(
            ScalarFunction::If,
            vec![
                null_condition.expect("multi-expression DISTINCT is nonempty"),
                BoundExpr {
                    kind: BoundExprKind::Literal(Value::Null),
                    data_type: None,
                    nullable: true,
                },
                tuple,
            ],
        )?;
        // The JSON rendering is only an internal collision-free tuple
        // encoding, not a user-visible JSON value requiring JSON equality.
        Some(bind_scalar(
            ScalarFunction::Cast(DataType::Utf8),
            vec![tuple_or_null],
        )?)
    } else if aggregate_function == AggregateFunction::GroupConcat && arguments.args.len() > 1 {
        let mut values = Vec::with_capacity(arguments.args.len());
        for argument in &arguments.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument else {
                return Err(BindError::UnsupportedAggregate(function.to_string()));
            };
            values.push(bind_expr(expr, tables, subqueries)?);
        }
        Some(bind_scalar(ScalarFunction::Concat, values)?)
    } else {
        match &arguments.args[0] {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                Some(bind_expr(expr, tables, subqueries)?)
            }
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                if aggregate_function == AggregateFunction::Count && !distinct =>
            {
                None
            }
            _ => return Err(BindError::UnsupportedAggregate(function.to_string())),
        }
    };
    if distinct && expr.as_ref().and_then(|expression| expression.data_type) == Some(DataType::Json)
    {
        return Err(BindError::UnsupportedAggregate(format!(
            "{function}: DISTINCT over JSON requires JSON-aware equality"
        )));
    }
    if (distinct
        || matches!(
            aggregate_function,
            AggregateFunction::Minimum | AggregateFunction::Maximum
        ))
        && let Some(expression) = &expr
    {
        ensure_supported_text_collation(&[expression])?;
    }
    let (data_type, nullable) = aggregate_result_type(aggregate_function, expr.as_ref())?;
    let aggregate = BoundAggregate {
        function: aggregate_function,
        expr,
        distinct,
        data_type,
        nullable,
        separator,
        order_within,
    };
    let aggregate_list = aggregates
        .as_deref_mut()
        .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
    let index = aggregate_list
        .iter()
        .position(|existing| existing == &aggregate)
        .unwrap_or_else(|| {
            let index = aggregate_list.len();
            aggregate_list.push(aggregate);
            index
        });
    Ok(BoundExpr {
        kind: BoundExprKind::Aggregate(index),
        data_type,
        nullable,
    })
}

fn aggregate_function_name(function: &Function) -> Option<AggregateFunction> {
    let parts = object_name_parts(&function.name).ok()?;
    let [name] = parts.as_slice() else {
        return None;
    };
    match name.to_ascii_uppercase().as_str() {
        "COUNT" => Some(AggregateFunction::Count),
        "SUM" => Some(AggregateFunction::Sum),
        "AVG" => Some(AggregateFunction::Average),
        "MIN" => Some(AggregateFunction::Minimum),
        "MAX" => Some(AggregateFunction::Maximum),
        "GROUP_CONCAT" => Some(AggregateFunction::GroupConcat),
        "JSON_ARRAYAGG" => Some(AggregateFunction::JsonArrayAgg),
        "JSON_OBJECTAGG" => Some(AggregateFunction::JsonObjectAgg),
        "ANY_VALUE" => Some(AggregateFunction::AnyValue),
        // MySQL spells the population forms three ways and the sample form
        // one; VARIANCE and VAR_POP are likewise the same function.
        "STDDEV" | "STD" | "STDDEV_POP" => Some(AggregateFunction::StdDev { sample: false }),
        "STDDEV_SAMP" => Some(AggregateFunction::StdDev { sample: true }),
        "VARIANCE" | "VAR_POP" => Some(AggregateFunction::Variance { sample: false }),
        "VAR_SAMP" => Some(AggregateFunction::Variance { sample: true }),
        "BIT_AND" => Some(AggregateFunction::BitAnd),
        "BIT_OR" => Some(AggregateFunction::BitOr),
        "BIT_XOR" => Some(AggregateFunction::BitXor),
        _ => None,
    }
}

fn aggregate_result_type(
    function: AggregateFunction,
    expr: Option<&BoundExpr>,
) -> Result<(Option<DataType>, bool), BindError> {
    let input_type = expr.and_then(|expr| expr.data_type);
    match function {
        AggregateFunction::Count => Ok((Some(DataType::UInt64), false)),
        AggregateFunction::Average if is_numeric(input_type) => {
            // MySQL AVG over exact numerics stays exact: DECIMAL widened by
            // div_precision_increment fraction digits. Floats and text keep
            // the double path.
            let exact = input_type.and_then(exact_numeric_digits);
            if let Some((scale, integer_digits)) = exact {
                let result_scale = scale
                    .saturating_add(DIVISION_SCALE_INCREMENT)
                    .min(MAX_DECIMAL_SCALE);
                Ok((
                    Some(DataType::Decimal {
                        precision: integer_digits
                            .saturating_add(result_scale)
                            .min(MAX_DECIMAL_PRECISION),
                        scale: result_scale,
                    }),
                    true,
                ))
            } else {
                Ok((Some(DataType::Float64), true))
            }
        }
        AggregateFunction::Sum if is_numeric(input_type) => {
            // MySQL SUM over DECIMAL stays DECIMAL at the input's scale with
            // widened precision; emitting Float64 here let display rounding
            // diverge one ulp from MySQL in downstream divisions.
            if let Some(DataType::Decimal { precision, scale }) = input_type {
                return Ok((
                    Some(DataType::Decimal {
                        precision: precision.saturating_add(10).min(MAX_DECIMAL_PRECISION),
                        scale,
                    }),
                    true,
                ));
            }
            let carrier = input_type.map(DataType::storage_type);
            let result = if carrier == Some(DataType::UInt64) {
                DataType::UInt64
            } else if carrier == Some(DataType::Float64)
                || matches!(carrier, Some(DataType::Utf8 | DataType::Binary))
            {
                DataType::Float64
            } else {
                DataType::Int64
            };
            Ok((Some(result), true))
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum
            if is_mysql_scalar(input_type) && input_type != Some(DataType::Json) =>
        {
            Ok((input_type, true))
        }
        AggregateFunction::GroupConcat if is_mysql_scalar(input_type) => {
            Ok((Some(DataType::Utf8), true))
        }
        AggregateFunction::JsonArrayAgg | AggregateFunction::JsonObjectAgg
            if is_mysql_scalar(input_type) =>
        {
            Ok((Some(DataType::Json), true))
        }
        // ANY_VALUE is a passthrough: it returns one of the input's own
        // values, so it keeps the input's type.
        AggregateFunction::AnyValue if is_mysql_scalar(input_type) => Ok((input_type, true)),
        // MySQL returns DOUBLE for both families regardless of input type.
        AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. }
            if is_numeric(input_type) =>
        {
            Ok((Some(DataType::Float64), true))
        }
        // The bit folds coerce their argument to BIGINT UNSIGNED and return
        // it, including for an empty group where the answer is the fold's
        // identity rather than NULL.
        AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor
            if is_numeric(input_type) =>
        {
            Ok((Some(DataType::UInt64), false))
        }
        _ => Err(BindError::InvalidAggregateType {
            function,
            actual: input_type,
        }),
    }
}

/// Replaces `ANY_VALUE` aggregate references with the argument itself, for
/// the ungrouped case where `MySQL` does not aggregate at all.
fn inline_any_value(expr: &mut BoundExpr, inlined: &[Option<BoundExpr>]) -> Result<(), BindError> {
    if let BoundExprKind::Aggregate(index) = &expr.kind {
        let argument = inlined
            .get(*index)
            .cloned()
            .flatten()
            .ok_or_else(|| BindError::UnsupportedExpression("ANY_VALUE()".to_owned()))?;
        *expr = argument;
        return Ok(());
    }
    match &mut expr.kind {
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            inline_any_value(expr, inlined)
        }
        BoundExprKind::Binary { left, right, .. } => {
            inline_any_value(left, inlined)?;
            inline_any_value(right, inlined)
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                inline_any_value(argument, inlined)?;
            }
            Ok(())
        }
        BoundExprKind::InSubquery { expr, .. } => inline_any_value(expr, inlined),
        BoundExprKind::Aggregate(_)
        | BoundExprKind::Column(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_) => Ok(()),
    }
}

/// Replaces `OVER w` with the spec `w` names, before binding.
///
/// `MySQL` also allows `OVER (w ORDER BY …)`, which inherits from `w` and
/// extends it. The merge below accepts only legal additive forms: a derived
/// spec cannot replace PARTITION BY, ORDER BY, or a frame already supplied by
/// the base window.
fn merge_named_window(
    mut base: sqlparser::ast::WindowSpec,
    mut extension: sqlparser::ast::WindowSpec,
    name: &Ident,
) -> Result<sqlparser::ast::WindowSpec, BindError> {
    if !extension.partition_by.is_empty()
        || (!base.order_by.is_empty() && !extension.order_by.is_empty())
        // A frame may be used by `OVER w` directly, but MySQL forbids
        // inheriting it into another specification.
        || base.window_frame.is_some()
    {
        return Err(BindError::UnsupportedQueryClause(format!(
            "window {name} is illegally redefined"
        )));
    }
    if base.order_by.is_empty() {
        base.order_by = std::mem::take(&mut extension.order_by);
    }
    base.window_frame = extension.window_frame.take();
    base.window_name = None;
    Ok(base)
}

fn resolve_named_window(
    name: &Ident,
    definitions: &[sqlparser::ast::NamedWindowDefinition],
    before: Option<usize>,
    stack: &mut Vec<String>,
) -> Result<sqlparser::ast::WindowSpec, BindError> {
    use sqlparser::ast::NamedWindowExpr;
    let index = definitions
        .iter()
        .position(|definition| definition.0.value.eq_ignore_ascii_case(&name.value))
        .ok_or_else(|| BindError::UnsupportedQueryClause(format!("unknown window {name}")))?;
    if before.is_some_and(|before| index >= before) {
        return Err(BindError::UnsupportedQueryClause(format!(
            "window {name} is a forward or cyclic reference"
        )));
    }
    let folded = name.value.to_ascii_lowercase();
    if stack.contains(&folded) {
        return Err(BindError::UnsupportedQueryClause(format!(
            "cyclic window definition at {name}"
        )));
    }
    stack.push(folded);
    let resolved = match &definitions[index].1 {
        NamedWindowExpr::NamedWindow(parent) => {
            resolve_named_window(parent, definitions, Some(index), stack)
        }
        NamedWindowExpr::WindowSpec(spec) => {
            if let Some(parent) = &spec.window_name {
                let base = resolve_named_window(parent, definitions, Some(index), stack)?;
                merge_named_window(base, spec.clone(), parent)
            } else {
                Ok(spec.clone())
            }
        }
    };
    stack.pop();
    resolved
}

fn substitute_named_windows(
    expr: &mut Expr,
    definitions: &[sqlparser::ast::NamedWindowDefinition],
) -> Result<(), BindError> {
    use sqlparser::ast::WindowType;
    match expr {
        Expr::Function(function) => {
            // `OVER w` is its own AST variant, not a spec carrying a name.
            // The spec-with-a-name form is `OVER (w ORDER BY …)`, which
            // inherits and extends; that rejects below.
            let named = match &function.over {
                Some(WindowType::NamedWindow(name)) => Some(name.clone()),
                Some(WindowType::WindowSpec(spec)) => spec.window_name.clone(),
                None => None,
            };
            if let Some(name) = named {
                let extension = match &function.over {
                    Some(WindowType::WindowSpec(spec)) => Some(spec.clone()),
                    _ => None,
                };
                let resolved = resolve_named_window(&name, definitions, None, &mut Vec::new())?;
                let merged = extension.map_or(Ok(resolved.clone()), |extension| {
                    merge_named_window(resolved, extension, &name)
                })?;
                function.over = Some(WindowType::WindowSpec(merged));
            }
            // The walk must continue into the arguments: the binder recurses
            // into them, so an OVER w nested inside COALESCE(...) would
            // otherwise reach binding unresolved.
            if let FunctionArguments::List(arguments) = &mut function.args {
                for argument in &mut arguments.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(inner),
                        ..
                    } = argument
                    {
                        substitute_named_windows(inner, definitions)?;
                    }
                }
            }
            Ok(())
        }
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) | Expr::Cast { expr, .. } => {
            substitute_named_windows(expr, definitions)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                substitute_named_windows(operand, definitions)?;
            }
            for arm in conditions {
                substitute_named_windows(&mut arm.condition, definitions)?;
                substitute_named_windows(&mut arm.result, definitions)?;
            }
            if let Some(otherwise) = else_result {
                substitute_named_windows(otherwise, definitions)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            substitute_named_windows(left, definitions)?;
            substitute_named_windows(right, definitions)
        }
        _ => Ok(()),
    }
}

fn window_frame_offset_expr(edge: &sqlparser::ast::WindowFrameBound) -> Option<&Expr> {
    use sqlparser::ast::WindowFrameBound as Edge;
    match edge {
        Edge::Preceding(Some(expr)) | Edge::Following(Some(expr)) => Some(expr.as_ref()),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn bind_window_frame(
    spec: &sqlparser::ast::WindowSpec,
    function: &Function,
    order_by: &[BoundWindowOrderKey],
) -> Result<Option<BoundWindowFrame>, BindError> {
    let Some(frame) = &spec.window_frame else {
        return Ok(None);
    };
    let unsupported = || BindError::UnsupportedQueryClause(format!("window frame on {function}"));
    // MySQL supports ROWS and RANGE but rejects GROUPS. A bounded RANGE must
    // have exactly one numeric ORDER BY expression; its offsets are applied
    // to that key's values during execution rather than treated as row counts.
    let range = match frame.units {
        sqlparser::ast::WindowFrameUnits::Rows => false,
        sqlparser::ast::WindowFrameUnits::Range => true,
        sqlparser::ast::WindowFrameUnits::Groups => return Err(unsupported()),
    };
    let offsets = [
        window_frame_offset_expr(&frame.start_bound),
        frame.end_bound.as_ref().and_then(window_frame_offset_expr),
    ];
    if range && offsets.iter().any(Option::is_some) {
        let [order] = order_by else {
            return Err(unsupported());
        };
        let valid = |expr: &Expr| match expr {
            Expr::Interval(_) => matches!(
                order.expr.data_type,
                Some(DataType::Date32 | DataType::DateTime64 { .. })
            ),
            Expr::Value(_) => matches!(
                order.expr.data_type,
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
                        | DataType::Float32
                        | DataType::Float64
                        | DataType::Decimal { .. }
                        | DataType::Year
                )
            ),
            _ => false,
        };
        if offsets.into_iter().flatten().any(|expr| !valid(expr)) {
            return Err(unsupported());
        }
    }
    let bound = |edge: &sqlparser::ast::WindowFrameBound, preceding_side: bool| {
        use sqlparser::ast::WindowFrameBound as Edge;
        let offset = |value: &Option<Box<Expr>>| -> Result<BoundFrameOffset, BindError> {
            let Some(expr) = value else {
                return Err(unsupported());
            };
            match expr.as_ref() {
                Expr::Value(value) => match &value.value {
                    sqlparser::ast::Value::Number(text, _) => {
                        if range {
                            let scale = text
                                .split_once('.')
                                .map_or(0, |(_, fraction)| fraction.len());
                            let scale = u8::try_from(scale).map_err(|_| unsupported())?;
                            let units = pintail_types::parse_decimal_scaled(text, scale)
                                .filter(|units| *units >= 0)
                                .ok_or_else(unsupported)?;
                            Ok(BoundFrameOffset::Numeric { units, scale })
                        } else {
                            Ok(BoundFrameOffset::Rows(
                                text.parse::<u64>().map_err(|_| unsupported())?,
                            ))
                        }
                    }
                    _ => Err(unsupported()),
                },
                Expr::Interval(interval)
                    if range
                        && interval.leading_precision.is_none()
                        && interval.last_field.is_none()
                        && interval.fractional_seconds_precision.is_none() =>
                {
                    let value = match interval.value.as_ref() {
                        Expr::Value(value) => match &value.value {
                            sqlparser::ast::Value::Number(text, _)
                            | sqlparser::ast::Value::SingleQuotedString(text) => {
                                text.parse::<u64>().map_err(|_| unsupported())?
                            }
                            _ => return Err(unsupported()),
                        },
                        _ => return Err(unsupported()),
                    };
                    let unit = match interval.leading_field {
                        Some(DateTimeField::Year) => IntervalUnit::Year,
                        Some(DateTimeField::Month) => IntervalUnit::Month,
                        Some(DateTimeField::Day) => IntervalUnit::Day,
                        Some(DateTimeField::Hour) => IntervalUnit::Hour,
                        Some(DateTimeField::Minute) => IntervalUnit::Minute,
                        Some(DateTimeField::Second) => IntervalUnit::Second,
                        _ => return Err(unsupported()),
                    };
                    Ok(BoundFrameOffset::Interval { value, unit })
                }
                _ => Err(unsupported()),
            }
        };
        match edge {
            Edge::CurrentRow => Ok(BoundFrameBound::CurrentRow),
            Edge::Preceding(None) if preceding_side => Ok(BoundFrameBound::UnboundedPreceding),
            Edge::Preceding(None) => Err(unsupported()),
            Edge::Preceding(value) => Ok(BoundFrameBound::Preceding(offset(value)?)),
            Edge::Following(None) if preceding_side => Err(unsupported()),
            Edge::Following(None) => Ok(BoundFrameBound::UnboundedFollowing),
            Edge::Following(value) => Ok(BoundFrameBound::Following(offset(value)?)),
        }
    };
    let start = bound(&frame.start_bound, true)?;
    // The shorthand `ROWS <n> PRECEDING` means `... AND CURRENT ROW`.
    let end = match &frame.end_bound {
        None => BoundFrameBound::CurrentRow,
        Some(edge) => bound(edge, false)?,
    };
    // MySQL rejects a frame whose start follows its end. Accepting it here
    // produced an empty frame and a NULL for every row — a plausible answer
    // to a query that should not have bound.
    let rank = |edge: BoundFrameBound| match edge {
        BoundFrameBound::UnboundedPreceding => 0_u8,
        BoundFrameBound::Preceding(_) => 1,
        BoundFrameBound::CurrentRow => 2,
        BoundFrameBound::Following(_) => 3,
        BoundFrameBound::UnboundedFollowing => 4,
    };
    let comparable_value = |offset: BoundFrameOffset| match offset {
        BoundFrameOffset::Rows(value) => Some((i128::from(value), 0_u8)),
        BoundFrameOffset::Numeric { units, scale } => Some((units, scale)),
        BoundFrameOffset::Interval { .. } => None,
    };
    let offset_cmp = |left: BoundFrameOffset, right: BoundFrameOffset| {
        let ((left, left_scale), (right, right_scale)) =
            comparable_value(left).zip(comparable_value(right))?;
        let scale = left_scale.max(right_scale);
        let rescale = |units: i128, from: u8| {
            10_i128
                .checked_pow(u32::from(scale - from))
                .and_then(|factor| units.checked_mul(factor))
        };
        Some(rescale(left, left_scale)?.cmp(&rescale(right, right_scale)?))
    };
    let reversed_within_side = match (start, end) {
        (BoundFrameBound::Preceding(start), BoundFrameBound::Preceding(end)) => {
            offset_cmp(start, end) == Some(std::cmp::Ordering::Less)
        }
        (BoundFrameBound::Following(start), BoundFrameBound::Following(end)) => {
            offset_cmp(start, end) == Some(std::cmp::Ordering::Greater)
        }
        _ => false,
    };
    if rank(start) > rank(end) || reversed_within_side {
        return Err(BindError::UnsupportedQueryClause(format!(
            "window frame start follows its end on {function}"
        )));
    }
    Ok(Some(BoundWindowFrame { range, start, end }))
}

fn rewrite_group_references(expr: &mut BoundExpr, group_by: &[BoundExpr]) -> Result<(), BindError> {
    if let Some(index) = group_by.iter().position(|group| group == expr) {
        expr.kind = BoundExprKind::GroupKey(index);
        return Ok(());
    }
    match &mut expr.kind {
        BoundExprKind::Column(column) => Err(BindError::UngroupedColumn(format!(
            "{}.{}",
            column.relation_name, column.name
        ))),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            rewrite_group_references(expr, group_by)
        }
        BoundExprKind::Binary { left, right, .. } => {
            rewrite_group_references(left, group_by)?;
            rewrite_group_references(right, group_by)
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                rewrite_group_references(argument, group_by)?;
            }
            Ok(())
        }
        BoundExprKind::Aggregate(index) => {
            *index = index.saturating_add(group_by.len());
            Ok(())
        }
        BoundExprKind::InSubquery { expr, .. } => rewrite_group_references(expr, group_by),
        // Window references resolve above the aggregation; the window's own
        // internals are rewritten separately by the caller.
        BoundExprKind::Window(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_) => Ok(()),
    }
}

/// Fraction digits and integer digits an exact numeric type contributes to
/// decimal result-type inference. `None` for types that are not exact
/// numerics (floats, text, temporal).
fn exact_numeric_digits(data_type: DataType) -> Option<(u8, u8)> {
    match data_type {
        DataType::Decimal { precision, scale } => Some((scale, precision.saturating_sub(scale))),
        DataType::Boolean
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => Some((0, 20)),
        DataType::Year => Some((0, 4)),
        _ => None,
    }
}

/// `MySQL` `div_precision_increment` default: division and `AVG` widen the
/// dividend's scale by four fraction digits.
/// Aggregate output column of a decorrelated scalar-subquery derived table.
const SCALAR_VALUE_COLUMN: &str = "__scalar_value";

/// Everything one FROM clause contributes to binding: the join structure,
/// the resolution scope, and the unqualified-`*` expansion order.
struct BoundFromScope {
    from: Vec<BoundFrom>,
    tables: Vec<BoundTable>,
    wildcard_order: Vec<BoundColumn>,
}

/// One join operand plus the individual relations and wildcard layout that a
/// parenthesized join group continues to expose to its containing query.
struct BoundJoinRelation {
    table: BoundTable,
    tables: Vec<BoundTable>,
    wildcard_order: Vec<BoundColumn>,
}

fn expression_scope(local: &[BoundTable], outer: &[BoundTable]) -> Vec<BoundTable> {
    let mut visible = Vec::with_capacity(local.len().saturating_add(outer.len()));
    visible.extend_from_slice(local);
    visible.extend_from_slice(outer);
    visible
}

const DIVISION_SCALE_INCREMENT: u8 = 4;
/// Pintail v1 decimal bounds (`DataType::is_valid`).
const MAX_DECIMAL_SCALE: u8 = 30;
const MAX_DECIMAL_PRECISION: u8 = 38;

fn division_result_type(left: DataType, right: DataType) -> Option<DataType> {
    let (left_scale, left_integer) = exact_numeric_digits(left)?;
    let (right_scale, _) = exact_numeric_digits(right)?;
    // MySQL: result scale is dividend scale + increment; the integer part
    // can grow by the divisor's scale (dividing by a small fraction).
    let scale = left_scale
        .saturating_add(DIVISION_SCALE_INCREMENT)
        .min(MAX_DECIMAL_SCALE);
    let precision = left_integer
        .saturating_add(right_scale)
        .saturating_add(scale)
        .min(MAX_DECIMAL_PRECISION);
    Some(DataType::Decimal { precision, scale })
}

fn arithmetic_type(
    op: BinaryOp,
    left: Option<DataType>,
    right: Option<DataType>,
) -> Option<DataType> {
    let (Some(left), Some(right)) = (left, right) else {
        return None;
    };
    // Arithmetic over exact numerics with a DECIMAL operand follows MySQL
    // decimal semantics and evaluates exactly on scaled units. Integer-only
    // expressions keep their integer fast paths untouched.
    if op == BinaryOp::Divide
        && let Some(result) = division_result_type(left, right)
    {
        return Some(result);
    }
    if matches!(
        op,
        BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Modulo
    ) && (matches!(left, DataType::Decimal { .. }) || matches!(right, DataType::Decimal { .. }))
        && let (Some((left_scale, left_integer)), Some((right_scale, right_integer))) =
            (exact_numeric_digits(left), exact_numeric_digits(right))
    {
        let (scale, integer_digits) = if op == BinaryOp::Multiply {
            (
                left_scale
                    .saturating_add(right_scale)
                    .min(MAX_DECIMAL_SCALE),
                left_integer.saturating_add(right_integer),
            )
        } else if op == BinaryOp::Modulo {
            (left_scale.max(right_scale), left_integer.min(right_integer))
        } else {
            (
                left_scale.max(right_scale),
                left_integer.max(right_integer).saturating_add(1),
            )
        };
        return Some(DataType::Decimal {
            precision: integer_digits
                .saturating_add(scale)
                .min(MAX_DECIMAL_PRECISION),
            scale,
        });
    }
    if op == BinaryOp::Divide
        || left == DataType::Float64
        || right == DataType::Float64
        || matches!(
            left,
            DataType::Utf8 | DataType::Binary | DataType::Decimal { .. } | DataType::Float32
        )
        || matches!(
            right,
            DataType::Utf8 | DataType::Binary | DataType::Decimal { .. } | DataType::Float32
        )
    {
        Some(DataType::Float64)
    } else if left == DataType::UInt64 && right == DataType::UInt64 {
        Some(DataType::UInt64)
    } else {
        Some(DataType::Int64)
    }
}

fn comparable(left: Option<DataType>, right: Option<DataType>) -> bool {
    is_mysql_scalar(left)
        && is_mysql_scalar(right)
        && left != Some(DataType::Json)
        && right != Some(DataType::Json)
}

fn is_numeric(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
        None | Some(
            DataType::Boolean
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Year
                | DataType::Float32
                | DataType::Float64
                | DataType::Decimal { .. }
                | DataType::Utf8
                | DataType::Binary
        )
    )
}

fn is_truth_value(data_type: Option<DataType>) -> bool {
    is_mysql_scalar(data_type)
}

fn is_mysql_scalar(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
        None | Some(
            DataType::Boolean
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Year
                | DataType::Float32
                | DataType::Float64
                | DataType::Decimal { .. }
                | DataType::Date32
                | DataType::DateTime64 { .. }
                | DataType::Time64 { .. }
                | DataType::Utf8
                | DataType::Binary
                | DataType::Json
        )
    )
}

fn bind_limit(limit: &LimitClause) -> Result<BoundLimit, BindError> {
    match limit {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => Ok(BoundLimit {
            offset: offset
                .as_ref()
                .map_or(Ok(0), |offset| unsigned_literal(&offset.value))?,
            count: unsigned_literal(limit)?,
        }),
        LimitClause::OffsetCommaLimit { offset, limit } => Ok(BoundLimit {
            offset: unsigned_literal(offset)?,
            count: unsigned_literal(limit)?,
        }),
        LimitClause::LimitOffset { .. } => Err(BindError::InvalidLimit(limit.to_string())),
    }
}

fn bind_order_by(query: &Query, bound: &mut BoundQuery) -> Result<(), BindError> {
    let Some(order_by) = &query.order_by else {
        return Ok(());
    };
    if order_by.interpolate.is_some() {
        return Err(BindError::InvalidOrderBy(order_by.to_string()));
    }
    let OrderByKind::Expressions(expressions) = &order_by.kind else {
        return Err(BindError::InvalidOrderBy(order_by.to_string()));
    };
    // MySQL lets ORDER BY reach source columns that never made it into the
    // select list. That needs a hidden trailing projection column (trimmed
    // after the sort), which is only sound when the projection is a plain
    // row-per-row mapping of one scope: aggregation, DISTINCT, windows, and
    // UNION all change the row set or reject unprojected sorts outright.
    let allow_hidden = bound.group_by.is_empty()
        && bound.aggregates.is_empty()
        && bound.windows.is_empty()
        && !bound.distinct
        && bound.union_all.is_empty();
    let visible = bound.projection.len();
    let keys = expressions
        .iter()
        .map(|order| {
            if order.with_fill.is_some() {
                return Err(BindError::InvalidOrderBy(order.to_string()));
            }
            let index = resolve_order_index(
                &order.expr,
                visible,
                &mut bound.projection,
                &bound.tables,
                allow_hidden,
            )?;
            if bound
                .projection
                .get(index)
                .is_some_and(|projection| projection.expr.data_type == Some(DataType::Json))
            {
                return Err(BindError::InvalidOrderBy(
                    "ORDER BY over JSON values requires JSON-aware ordering".to_owned(),
                ));
            }
            if let Some(projection) = bound.projection.get(index) {
                ensure_supported_text_collation(&[&projection.expr])?;
            }
            let ascending = order.options.asc.unwrap_or(true);
            Ok(BoundOrderKey {
                index,
                ascending,
                nulls_first: order.options.nulls_first.unwrap_or(ascending),
                decimal: matches!(
                    bound.projection.get(index).and_then(|p| p.expr.data_type),
                    Some(DataType::Decimal { .. })
                ),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    bound.order_by = keys;
    bound.hidden_sort_columns = bound.projection.len() - visible;
    Ok(())
}

fn resolve_order_index(
    expr: &Expr,
    visible: usize,
    projection: &mut Vec<BoundProjection>,
    tables: &[BoundTable],
    allow_hidden: bool,
) -> Result<usize, BindError> {
    if let Expr::Value(value) = expr
        && let SqlValue::Number(value, _) = &value.value
        && !value.contains(['.', 'e', 'E'])
    {
        let ordinal = value
            .parse::<usize>()
            .map_err(|_| BindError::InvalidOrderBy(expr.to_string()))?;
        return ordinal
            .checked_sub(1)
            .filter(|index| *index < visible)
            .ok_or_else(|| BindError::InvalidOrderBy(expr.to_string()));
    }

    // A qualified name (e.id) refers to the source scope first, so two
    // outputs sharing its last part are not ambiguous; the output-name
    // match below stays as the fallback for rewritten projections.
    if let Expr::CompoundIdentifier(identifiers) = expr
        && let Ok(column) = bind_column(identifiers, tables)
        && let Some(index) = projection.iter().position(|item| item.expr == column)
    {
        return Ok(index);
    }
    let requested = match expr {
        Expr::Identifier(identifier) => identifier.value.clone(),
        _ => projection_name(expr),
    };
    let matches = projection[..visible]
        .iter()
        .enumerate()
        .filter(|(_, item)| item.name.eq_ignore_ascii_case(&requested))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => return Ok(*index),
        [] => {}
        _ => return Err(BindError::AmbiguousOrderBy(requested)),
    }

    // Not an output name: try the FROM scope the way MySQL does, first as an
    // already-projected source column (any alias), then as a hidden sort key.
    let identifiers = match expr {
        Expr::Identifier(identifier) => vec![identifier.clone()],
        Expr::CompoundIdentifier(identifiers) => identifiers.clone(),
        _ => return Err(BindError::InvalidOrderBy(expr.to_string())),
    };
    let Ok(column) = bind_column(&identifiers, tables) else {
        return Err(BindError::InvalidOrderBy(expr.to_string()));
    };
    if let Some(index) = projection.iter().position(|item| item.expr == column) {
        return Ok(index);
    }
    if !allow_hidden {
        return Err(BindError::InvalidOrderBy(expr.to_string()));
    }
    projection.push(BoundProjection {
        name: format!("<order-{}>", projection.len()),
        expr: column,
    });
    Ok(projection.len() - 1)
}

fn unsigned_literal(expr: &Expr) -> Result<u64, BindError> {
    let Expr::Value(value) = expr else {
        return Err(BindError::InvalidLimit(expr.to_string()));
    };
    let SqlValue::Number(value, _) = &value.value else {
        return Err(BindError::InvalidLimit(expr.to_string()));
    };
    value
        .parse()
        .map_err(|_| BindError::InvalidLimit(expr.to_string()))
}

fn projection_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(identifier) => identifier.value.clone(),
        Expr::CompoundIdentifier(identifiers) => identifiers
            .last()
            .map_or_else(|| expr.to_string(), |identifier| identifier.value.clone()),
        _ => expr.to_string(),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<&str>, BindError> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|identifier| identifier.value.as_str())
                .ok_or_else(|| BindError::InvalidObjectName(name.to_string()))
        })
        .collect()
}

/// A catalog, ambiguity, type, literal, or supported-surface binding error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindError {
    /// This binder entry point accepts query statements only.
    UnsupportedStatement(String),
    /// A query-level clause is not implemented yet.
    UnsupportedQueryClause(String),
    /// A set operation or other query body is not implemented yet.
    UnsupportedQueryBody(String),
    /// UNION ALL branches do not expose compatible result layouts.
    IncompatibleSetOperation(String),
    /// A derived table or table extension is not implemented yet.
    UnsupportedTableFactor(String),
    /// A projection extension is not implemented yet.
    UnsupportedProjection(String),
    /// A join kind is outside the v1 operator surface.
    UnsupportedJoinOperator(String),
    /// A join constraint cannot yet be lowered safely.
    UnsupportedJoinConstraint(String),
    /// A scalar expression is not implemented yet.
    UnsupportedExpression(String),
    /// A subquery requires relational execution not implemented by this path.
    UnsupportedSubquery(String),
    /// An aggregate call uses an unsupported shape or modifier.
    UnsupportedAggregate(String),
    /// A literal representation is not implemented yet.
    UnsupportedLiteral(String),
    /// A table name requires a current database.
    NoCurrentDatabase,
    /// No catalog database has this name.
    UnknownDatabase(String),
    /// No catalog table has this name.
    UnknownTable {
        /// Resolved database spelling.
        database: String,
        /// Requested table spelling.
        table: String,
    },
    /// No visible relation has this qualifier.
    UnknownRelation(String),
    /// More than one visible relation has this qualifier.
    AmbiguousRelation(String),
    /// The same query-visible table name or alias occurs twice.
    DuplicateRelation(String),
    /// No visible column has this name.
    UnknownColumn(String),
    /// More than one visible column has this unqualified name.
    AmbiguousColumn(String),
    /// An object name has an unsupported number or kind of parts.
    InvalidObjectName(String),
    /// `*` has no input relation to expand.
    WildcardWithoutTable,
    /// A numeric literal cannot be represented by Pintail's scalar types.
    InvalidNumericLiteral(String),
    /// An operator does not accept this operand type.
    InvalidUnaryType {
        /// SQL operator.
        operation: String,
        /// Operand type.
        actual: Option<DataType>,
    },
    /// An operator does not accept this pair of operand types.
    InvalidBinaryTypes {
        /// SQL operator.
        operation: String,
        /// Left operand type.
        left: Option<DataType>,
        /// Right operand type.
        right: Option<DataType>,
    },
    /// An aggregate does not accept this input type.
    InvalidAggregateType {
        /// Aggregate operation.
        function: AggregateFunction,
        /// Input expression type.
        actual: Option<DataType>,
    },
    /// A scalar function has invalid arguments or result types.
    InvalidScalarFunction(String),
    /// A scalar subquery produced more than one row.
    InvalidScalarSubqueryRows(usize),
    /// A selected column is neither grouped nor aggregated.
    UngroupedColumn(String),
    /// GROUP BY and HAVING have an invalid combination.
    InvalidGrouping(String),
    /// A row filter does not have `MySQL` truth-value semantics.
    ExpectedPredicate {
        /// Actual expression type.
        actual: Option<DataType>,
    },
    /// `LIMIT` is not a non-negative integer literal.
    InvalidLimit(String),
    /// ORDER BY does not resolve to one projected output.
    InvalidOrderBy(String),
    /// ORDER BY matches multiple output aliases.
    AmbiguousOrderBy(String),
}

impl fmt::Display for BindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedStatement(value) => {
                write!(formatter, "unsupported statement: {value}")
            }
            Self::UnsupportedQueryClause(value) => {
                write!(formatter, "unsupported query clause: {value}")
            }
            Self::UnsupportedQueryBody(value) => {
                write!(formatter, "unsupported query body: {value}")
            }
            Self::IncompatibleSetOperation(value) => formatter.write_str(value),
            Self::UnsupportedTableFactor(value) => {
                write!(formatter, "unsupported table expression: {value}")
            }
            Self::UnsupportedProjection(value) => {
                write!(formatter, "unsupported projection: {value}")
            }
            Self::UnsupportedJoinOperator(value) => {
                write!(formatter, "unsupported join operator: {value}")
            }
            Self::UnsupportedJoinConstraint(value) => {
                write!(formatter, "unsupported join constraint: {value}")
            }
            Self::UnsupportedExpression(value) => {
                write!(formatter, "unsupported expression: {value}")
            }
            Self::UnsupportedSubquery(value) => {
                write!(formatter, "unsupported subquery: {value}")
            }
            Self::UnsupportedAggregate(value) => {
                write!(formatter, "unsupported aggregate: {value}")
            }
            Self::UnsupportedLiteral(value) => {
                write!(formatter, "unsupported literal: {value}")
            }
            Self::NoCurrentDatabase => formatter.write_str("no current database selected"),
            Self::UnknownDatabase(name) => write!(formatter, "unknown database {name}"),
            Self::UnknownTable { database, table } => {
                write!(formatter, "unknown table {database}.{table}")
            }
            Self::UnknownRelation(name) => write!(formatter, "unknown relation {name}"),
            Self::AmbiguousRelation(name) => write!(formatter, "ambiguous relation {name}"),
            Self::DuplicateRelation(name) => write!(formatter, "duplicate relation {name}"),
            Self::UnknownColumn(name) => write!(formatter, "unknown column {name}"),
            Self::AmbiguousColumn(name) => write!(formatter, "ambiguous column {name}"),
            Self::InvalidObjectName(name) => write!(formatter, "invalid object name {name}"),
            Self::WildcardWithoutTable => {
                formatter.write_str("cannot expand wildcard without a table")
            }
            Self::InvalidNumericLiteral(value) => {
                write!(formatter, "numeric literal is out of range: {value}")
            }
            Self::InvalidUnaryType { operation, actual } => {
                write!(formatter, "operator {operation} does not accept {actual:?}")
            }
            Self::InvalidBinaryTypes {
                operation,
                left,
                right,
            } => write!(
                formatter,
                "operator {operation} does not accept {left:?} and {right:?}"
            ),
            Self::InvalidAggregateType { function, actual } => {
                write!(
                    formatter,
                    "aggregate {function:?} does not accept {actual:?}"
                )
            }
            Self::InvalidScalarFunction(function) => {
                write!(formatter, "invalid scalar function {function}")
            }
            Self::InvalidScalarSubqueryRows(rows) => {
                write!(formatter, "scalar subquery produced {rows} rows")
            }
            Self::UngroupedColumn(column) => {
                write!(
                    formatter,
                    "column {column} is neither grouped nor aggregated"
                )
            }
            Self::InvalidGrouping(message) => formatter.write_str(message),
            Self::ExpectedPredicate { actual } => {
                write!(formatter, "row filter has non-boolean type {actual:?}")
            }
            Self::InvalidLimit(value) => write!(formatter, "invalid LIMIT value {value}"),
            Self::InvalidOrderBy(value) => write!(formatter, "invalid ORDER BY expression {value}"),
            Self::AmbiguousOrderBy(value) => {
                write!(formatter, "ambiguous ORDER BY output {value}")
            }
        }
    }
}

impl std::error::Error for BindError {}

#[cfg(test)]
mod parity_surface {
    /// `parity.md` states how many function names are callable. That number
    /// drifted from 110 to 133 without anyone noticing, because nothing tied
    /// the prose to the code. This does.
    ///
    /// The extraction mirrors `scripts/function-surface.ts`: match arms whose
    /// head is one or more quoted upper-case names, plus the handful
    /// dispatched by an equality test ahead of the match.
    #[test]
    fn parity_states_the_real_callable_count() {
        // The binder is split across modules; scan every one of them or the
        // count silently drops the callables that moved out of this file.
        let sources = [include_str!("mod.rs"), include_str!("function.rs")];
        let mut names = std::collections::BTreeSet::new();
        for line in sources.iter().flat_map(|source| source.lines()) {
            let trimmed = line.trim_start();
            // Three dispatch shapes, not two. A matches!() guard ahead of the
            // match binds several names through one path - DATE_ADD is bound
            // that way - and reading only match arms and equality tests
            // counted it as absent while the binder resolved it.
            let dispatches = trimmed.contains("=>")
                || trimmed.contains("function_name ==")
                || trimmed.contains("matches!(function_name");
            if let Some(head) = trimmed.split("=>").next()
                && dispatches
            {
                let mut rest = head;
                while let Some(open) = rest.find('"') {
                    let after = &rest[open + 1..];
                    let Some(close) = after.find('"') else { break };
                    let candidate = &after[..close];
                    if !candidate.is_empty()
                        && candidate
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    {
                        names.insert(candidate.to_owned());
                    }
                    rest = &after[close + 1..];
                }
            }
        }
        let parity = include_str!("../../../../parity.md");
        let claimed: usize = parity
            .lines()
            .find(|line| line.starts_with("| Callable functions |"))
            .and_then(|line| {
                line.split('|')
                    .nth(2)
                    .and_then(|cell| cell.split_whitespace().next())
            })
            .and_then(|number| number.parse().ok())
            .expect("parity.md states a callable-function count");
        assert_eq!(
            claimed,
            names.len(),
            "parity.md claims {claimed} callable functions; the binder resolves {}. \
             Run `bun run scripts/function-surface.ts` and update the row.",
            names.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_types::{Column, DataType, TableSchema, Value};

    use super::{BindError, Binder};
    use crate::{AggregateFunction, BinaryOp, BoundExprKind, BoundJoinKind, parse_statement};

    fn catalog() -> CatalogSnapshot {
        let events = TableEntry::new(
            TableId::new(11),
            "Events",
            TableSchema::new(
                3,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "Name", DataType::Utf8, true),
                    Column::new(3, "active", DataType::Boolean, false),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(120),
        )
        .expect("table");
        let users = TableEntry::new(
            TableId::new(12),
            "users",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "email", DataType::Utf8, false),
                ],
            )
            .expect("schema"),
            TableStatistics::default(),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("users key");
        let payments = TableEntry::new(
            TableId::new(13),
            "payments",
            TableSchema::new(
                1,
                vec![Column::new(
                    1,
                    "amount",
                    DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    false,
                )],
            )
            .expect("schema"),
            TableStatistics::default(),
        )
        .expect("table");
        let legacy = TableEntry::new(
            TableId::new(14),
            "legacy",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "name", DataType::Utf8, false)
                        .with_collation(Some("latin1_swedish_ci".to_owned())),
                ],
            )
            .expect("schema"),
            TableStatistics::default(),
        )
        .expect("table");
        let database = DatabaseEntry::new(
            DatabaseId::new(7),
            "Analytics",
            [events, users, payments, legacy],
        )
        .expect("database");
        CatalogSnapshot::new([database]).expect("catalog")
    }

    fn bind(sql: &str) -> Result<crate::BoundQuery, BindError> {
        let catalog = catalog();
        let statement = parse_statement(sql).expect("parse");
        Binder::new(&catalog, Some("analytics")).bind(&statement)
    }

    #[test]
    fn unsupported_source_collations_project_but_never_compare_silently() {
        bind("SELECT name FROM legacy").expect("lossless projection is collation-independent");
        for sql in [
            "SELECT name FROM legacy WHERE name = 'a'",
            "SELECT name, COUNT(*) FROM legacy GROUP BY name",
            "SELECT DISTINCT name FROM legacy",
            "SELECT name FROM legacy ORDER BY name",
            "SELECT MIN(name) FROM legacy",
            "SELECT name LIKE 'a%' FROM legacy",
            "SELECT CONCAT(e.Name, l.name) AS x FROM Events e CROSS JOIN legacy l ORDER BY x",
        ] {
            let error = bind(sql).expect_err("unsupported collation must reject");
            assert!(
                error.to_string().contains("latin1_swedish_ci"),
                "{sql}: {error}"
            );
        }
    }

    #[test]
    fn explicit_collate_accepts_only_the_declared_text_profile() {
        bind("SELECT Name COLLATE utf8mb4_0900_ai_ci FROM Events ORDER BY Name")
            .expect("declared collation");
        bind("SELECT 'a' COLLATE utf8mb4_0900_ai_ci = 'A'")
            .expect("literal with declared collation");

        for sql in [
            "SELECT Name COLLATE utf8mb4_bin FROM Events",
            "SELECT name COLLATE utf8mb4_0900_ai_ci FROM legacy",
            "SELECT 1 COLLATE utf8mb4_0900_ai_ci",
        ] {
            let error = bind(sql).expect_err("unsupported collation boundary");
            assert!(error.to_string().contains("unsupported"), "{sql}: {error}");
        }
    }

    #[test]
    fn decorrelates_exists_with_derived_filter_and_two_equalities() {
        let query = bind(
            "SELECT e.id FROM Events e WHERE NOT EXISTS (\
             SELECT 1 FROM users u WHERE u.id = e.id AND u.id = e.id DIV 10 AND u.email <> 'x')",
        )
        .expect("bind");
        assert_eq!(query.from.len(), 1);
        assert_eq!(query.from[0].joins.len(), 1);
    }

    #[test]
    fn resolves_aliases_columns_types_and_limits() {
        let query = bind(
            "SELECT e.id, e.Name AS label FROM Events AS e \
             WHERE e.active AND e.id >= 10 LIMIT 5, 20",
        )
        .expect("bind");

        assert_eq!(query.tables.len(), 1);
        assert_eq!(query.tables[0].relation_name, "e");
        assert_eq!(query.tables[0].schema_version, 3);
        assert_eq!(query.tables[0].row_count, Some(120));
        assert_eq!(
            query
                .projection
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "label"]
        );
        assert_eq!(
            query.limit,
            Some(crate::BoundLimit {
                offset: 5,
                count: 20
            })
        );
        assert_eq!(
            query.filter.as_ref().expect("filter").data_type,
            Some(DataType::Boolean)
        );
    }

    #[test]
    fn expands_qualified_and_unqualified_wildcards_in_source_order() {
        let query = bind("SELECT e.*, users.email FROM Events e, users").expect("bind");
        assert_eq!(
            query
                .projection
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "Name", "active", "email"]
        );
    }

    #[test]
    fn binds_literals_and_numeric_coercion() {
        let query =
            bind("SELECT 1 + 2.5 AS total, NULL AS absent, 'hi' AS greeting").expect("bind");
        let total = &query.projection[0].expr;
        // INT + DECIMAL unifies to DECIMAL, as MySQL does. This asserted
        // Float64 only because a dotted literal used to be typed Float64.
        assert!(
            matches!(total.data_type, Some(DataType::Decimal { scale: 1, .. })),
            "expected a decimal total, got {:?}",
            total.data_type
        );
        assert!(matches!(
            total.kind,
            BoundExprKind::Binary {
                op: BinaryOp::Add,
                ..
            }
        ));
        assert_eq!(
            query.projection[1].expr.kind,
            BoundExprKind::Literal(Value::Null)
        );
        assert_eq!(
            query.projection[2].expr.kind,
            BoundExprKind::Literal(Value::Utf8("hi".to_owned()))
        );
    }

    #[test]
    fn rejects_unknown_and_ambiguous_names() {
        assert_eq!(
            bind("SELECT missing FROM Events"),
            Err(BindError::UnknownColumn("missing".to_owned()))
        );
        assert_eq!(
            bind("SELECT id FROM Events, users"),
            Err(BindError::AmbiguousColumn("id".to_owned()))
        );
        assert_eq!(
            bind("SELECT * FROM missing"),
            Err(BindError::UnknownTable {
                database: "Analytics".to_owned(),
                table: "missing".to_owned()
            })
        );
    }

    #[test]
    fn binds_explicit_inner_and_left_join_chains() {
        let query = bind(
            "SELECT e.Name, u.email FROM Events e \
             INNER JOIN users u ON e.id = u.id",
        )
        .expect("inner join");
        assert_eq!(query.from.len(), 1);
        assert_eq!(query.from[0].joins.len(), 1);
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Inner);
        assert!(query.from[0].joins[0].condition.is_some());

        let query = bind(
            "SELECT e.Name, u.email FROM Events e \
             LEFT JOIN users u ON e.id = u.id",
        )
        .expect("left join");
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Left);

        let query = bind(
            "SELECT e.Name, u.email FROM \
             (Events e INNER JOIN users u ON e.id = u.id)",
        )
        .expect("parenthesized inner join group");
        assert_eq!(query.from.len(), 1);
        assert_eq!(query.from[0].joins.len(), 1);
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Inner);

        let query = bind("SELECT * FROM (Events e LEFT JOIN users u ON e.id = u.id)")
            .expect("complete parenthesized left join group");
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Left);
        let query = bind(
            "SELECT * FROM (Events e LEFT JOIN users u ON e.id = u.id) \
             JOIN payments p ON p.amount = e.id",
        )
        .expect("root parenthesized left join followed by inner join");
        assert_eq!(query.from[0].joins.len(), 2);
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Left);
        assert_eq!(query.from[0].joins[1].kind, BoundJoinKind::Inner);

        let query = bind(
            "SELECT e.id, u.email, manager.email FROM Events e \
             LEFT JOIN (users u LEFT JOIN users manager ON manager.id = u.id) \
             ON u.id = e.id",
        )
        .expect("bushy right-side outer join group");
        assert_eq!(query.tables.len(), 3);
        assert!(query.from[0].joins[0].table.input.is_some());
        assert!(!query.projection[0].expr.nullable);
        assert!(query.projection[1].expr.nullable);
        assert!(query.projection[2].expr.nullable);
    }

    #[test]
    fn decimal_equality_remains_an_extractable_join_key() {
        let query = bind(
            "SELECT p.amount FROM payments p \
             JOIN payments q ON p.amount = q.amount",
        )
        .expect("decimal equi-join binds");
        let condition = query.from[0].joins[0]
            .condition
            .as_ref()
            .expect("join condition");
        assert!(matches!(
            condition.kind,
            BoundExprKind::Binary {
                op: BinaryOp::Equal,
                ..
            }
        ));
    }
    #[test]
    fn rejects_unsupported_join_directions_and_constraints() {
        // Two-table RIGHT JOIN flips into a LEFT JOIN with swapped inputs.
        let query = bind("SELECT * FROM Events RIGHT JOIN users ON Events.id = users.id")
            .expect("right join flips");
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Left);
        assert_eq!(query.from[0].base.table_name, "users");
        // RIGHT JOIN inside a longer chain still rejects.
        assert!(matches!(
            bind(
                "SELECT * FROM Events e JOIN users u ON e.id = u.id \
                 RIGHT JOIN users x ON x.id = e.id"
            ),
            Err(BindError::UnsupportedJoinOperator(_))
        ));
        // USING desugars to the left-right equality with the join column
        // leading the wildcard once and the right copy shadowed.
        let query = bind("SELECT * FROM Events JOIN users USING (id)").expect("using join binds");
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Inner);
        assert!(query.from[0].joins[0].condition.is_some());
        let names = query
            .projection
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.iter().filter(|name| **name == "id").count(), 1);
        assert_eq!(names.first(), Some(&"id"));
        // NATURAL resolves the shared column names the same way.
        let query = bind("SELECT id FROM Events NATURAL JOIN users").expect("natural join binds");
        assert!(query.from[0].joins[0].condition.is_some());
    }

    #[test]
    fn binds_char_and_rand() {
        let query = bind("SELECT CHAR(77, 121, NULL), RAND() FROM Events").expect("binds");
        assert_eq!(query.projection.len(), 2);
        // The seeded form stays unsupported.
        assert!(bind("SELECT RAND(3) FROM Events").is_err());
    }

    #[test]
    fn binds_mysql_time_cast_with_fractional_precision() {
        let query = bind("SELECT CAST('12:34:56.7896' AS TIME(3)) FROM Events").expect("binds");
        assert_eq!(
            query.projection[0].expr.data_type,
            Some(DataType::Time64 { fsp: 3 })
        );
    }

    #[test]
    fn binds_json_cast_as_a_typed_document() {
        let query = bind(r#"SELECT CAST('{"a":1}' AS JSON) FROM Events"#).expect("binds");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Json));
    }

    #[test]
    fn convert_using_rejects_charsets_it_cannot_transcode() {
        for charset in ["utf8", "utf8mb3", "utf8mb4", "binary"] {
            bind(&format!("SELECT CONVERT(Name USING {charset}) FROM Events"))
                .unwrap_or_else(|error| panic!("{charset} should bind: {error:?}"));
        }
        for charset in ["latin1", "ascii", "utf16"] {
            assert!(matches!(
                bind(&format!("SELECT CONVERT(Name USING {charset}) FROM Events")),
                Err(BindError::InvalidScalarFunction(_))
            ));
        }
    }

    #[test]
    fn binds_year_cast_as_a_mysql_year_type() {
        let query = bind("SELECT CAST(69 AS YEAR) FROM Events").expect("binds");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Year));
    }

    #[test]
    fn binds_the_one_argument_md5_form() {
        let query = bind("SELECT MD5(Name) FROM Events").expect("MD5 binds");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Utf8));
        assert!(bind("SELECT MD5(Name, Name) FROM Events").is_err());
    }

    #[test]
    fn binds_json_and_regexp_optional_arguments() {
        let query = bind(
            "SELECT JSON_EXTRACT(Name, '$.a', '$.b'), \
             REGEXP_LIKE(Name, '^x', 'cm') FROM Events",
        )
        .expect("JSON paths and REGEXP match_type bind");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Json));
        assert_eq!(query.projection[1].expr.data_type, Some(DataType::Boolean));
        assert!(matches!(
            bind("SELECT REGEXP_LIKE(CAST(Name AS BINARY), 'x') FROM Events"),
            Err(BindError::InvalidScalarFunction(_))
        ));
        assert!(matches!(
            bind("SELECT Name REGEXP CAST('x' AS BINARY) FROM Events"),
            Err(BindError::InvalidScalarFunction(_))
        ));
    }

    #[test]
    fn rejects_json_operations_without_json_aware_key_semantics() {
        let json = "JSON_EXTRACT(Name, '$.a')";
        assert!(matches!(
            bind(&format!("SELECT {json} = {json} FROM Events")),
            Err(BindError::InvalidBinaryTypes { .. })
        ));
        assert!(matches!(
            bind(&format!("SELECT {json} IN ({json}) FROM Events")),
            Err(BindError::InvalidScalarFunction(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT {json} BETWEEN {json} AND {json} FROM Events"
            )),
            Err(BindError::InvalidScalarFunction(_))
        ));
        assert!(matches!(
            bind(&format!("SELECT {json} FROM Events GROUP BY {json}")),
            Err(BindError::InvalidGrouping(_))
        ));
        assert!(matches!(
            bind(&format!("SELECT DISTINCT {json} FROM Events")),
            Err(BindError::UnsupportedQueryClause(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT {json} AS document FROM Events ORDER BY document"
            )),
            Err(BindError::InvalidOrderBy(_))
        ));
        assert!(matches!(
            bind(&format!("SELECT MIN({json}) FROM Events")),
            Err(BindError::InvalidAggregateType { .. })
        ));
        assert!(matches!(
            bind(&format!("SELECT COUNT(DISTINCT {json}) FROM Events")),
            Err(BindError::UnsupportedAggregate(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT ROW_NUMBER() OVER (PARTITION BY {json}) FROM Events"
            )),
            Err(BindError::InvalidGrouping(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT ROW_NUMBER() OVER (ORDER BY {json}) FROM Events"
            )),
            Err(BindError::InvalidOrderBy(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT GROUP_CONCAT(Name ORDER BY {json}) FROM Events"
            )),
            Err(BindError::UnsupportedAggregate(_))
        ));
        assert!(matches!(
            bind(&format!(
                "SELECT {json} FROM Events UNION SELECT {json} FROM Events"
            )),
            Err(BindError::IncompatibleSetOperation(_))
        ));
        assert!(
            bind(&format!(
                "SELECT {json} FROM Events UNION ALL SELECT {json} FROM Events"
            ))
            .is_ok()
        );
    }

    #[test]
    fn decorrelates_canonical_scalar_aggregate_subqueries() {
        let query = bind(
            "SELECT Name, (SELECT COUNT(*) FROM users WHERE users.id = Events.id) FROM Events",
        )
        .expect("scalar subquery decorrelates");
        assert_eq!(query.from[0].joins.len(), 1);
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Left);
        // COUNT folds the missing-group NULL to zero.
        assert!(matches!(
            &query.projection[1].expr.kind,
            BoundExprKind::Scalar {
                function: crate::ScalarFunction::Coalesce,
                ..
            }
        ));
        // SUM keeps NULL for missing groups: a plain derived column.
        let query = bind(
            "SELECT (SELECT SUM(users.id) FROM users WHERE users.id = Events.id) AS total \
             FROM Events",
        )
        .expect("scalar sum decorrelates");
        assert!(matches!(
            &query.projection[0].expr.kind,
            BoundExprKind::Column(_)
        ));
        // Correlation through a non-key column uses a guarded scalar join;
        // the executor, rather than the binder, owns the cardinality check.
        let query =
            bind("SELECT (SELECT id FROM users WHERE users.email = Events.Name) FROM Events")
                .expect("non-unique scalar lookup decorrelates with a runtime guard");
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Scalar);
        // A lookup correlated through the inner table's complete unique key
        // is scalar by construction but uses the same guarded operator, so a
        // stale or incorrect metadata declaration cannot produce a wrong row.
        let query = bind(
            "SELECT (SELECT email FROM users WHERE users.id = Events.id) AS user_name FROM Events",
        )
        .expect("unique-key scalar lookup decorrelates");
        assert_eq!(query.from[0].joins.len(), 1);
        assert_eq!(query.from[0].joins[0].kind, BoundJoinKind::Scalar);
        assert!(matches!(
            &query.projection[0].expr.kind,
            BoundExprKind::Column(_)
        ));
    }

    #[test]
    fn binds_datetime_helpers_and_inline_intervals() {
        let query = bind(
            "SELECT TIMESTAMPDIFF(SECOND, Name, Name), CEIL(id), FLOOR(id), \
             Name + INTERVAL 90 DAY, Name - INTERVAL 1 MONTH \
             FROM Events",
        )
        .expect("datetime helpers bind");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Int64));
        assert_eq!(query.projection[1].expr.data_type, Some(DataType::Float64));
        assert_eq!(query.projection[2].expr.data_type, Some(DataType::Float64));
        assert_eq!(query.projection[3].expr.data_type, Some(DataType::Utf8));
        assert_eq!(query.projection[4].expr.data_type, Some(DataType::Utf8));
        let query = bind(
            "SELECT WEEK(Name, 1), WEEK(Name, 2), WEEK(Name, 4), WEEK(Name, 5), \
             WEEK(Name, 6), WEEK(Name, 7) FROM Events",
        )
        .expect("all literal WEEK modes bind");
        assert!(
            query
                .projection
                .iter()
                .all(|item| item.expr.data_type == Some(DataType::UInt64))
        );
        // WEEK parses but is outside the supported unit set.
        assert!(matches!(
            bind("SELECT TIMESTAMPDIFF(WEEK, Name, Name) FROM Events"),
            Err(BindError::UnsupportedExpression(_))
        ));
    }

    #[test]
    fn applies_mysql_scalar_coercion_and_rejects_unsupported_syntax_explicitly() {
        let query = bind("SELECT Name + active FROM Events WHERE Name").expect("MySQL coercion");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Float64));
        assert!(matches!(
            bind("SELECT * FROM Events FETCH FIRST 1 ROW ONLY"),
            Err(BindError::UnsupportedQueryClause(_))
        ));
        assert!(matches!(
            bind("SELECT STR_TO_DATE('29th February 2024', '%D %M %Y')"),
            Err(BindError::UnsupportedExpression(_))
        ));
    }

    #[test]
    fn resolves_ordering_aliases_ordinals_and_mysql_null_defaults() {
        let query = bind("SELECT Name AS label, id FROM Events ORDER BY label DESC, 2 ASC")
            .expect("ordered query");
        assert_eq!(
            query.order_by,
            [
                crate::BoundOrderKey {
                    index: 0,
                    ascending: false,
                    nulls_first: false,
                    decimal: false,
                },
                crate::BoundOrderKey {
                    index: 1,
                    ascending: true,
                    nulls_first: true,
                    decimal: false,
                },
            ]
        );
        // MySQL lets ORDER BY reach unprojected source columns; the binder
        // appends a hidden trailing sort column trimmed after ordering.
        let hidden = bind("SELECT Name AS label FROM Events ORDER BY id").expect("hidden sort");
        assert_eq!(hidden.projection.len(), 2);
        assert_eq!(hidden.hidden_sort_columns, 1);
        assert_eq!(hidden.order_by[0].index, 1);
        let qualified = bind("SELECT e.Name AS label FROM Events e ORDER BY e.id DESC")
            .expect("qualified hidden sort");
        assert_eq!(qualified.hidden_sort_columns, 1);
        assert_eq!(qualified.order_by[0].index, 1);
        // A qualified ref to an already-projected column reuses its slot.
        let projected =
            bind("SELECT e.id AS key_id FROM Events e ORDER BY e.id").expect("projected ref");
        assert_eq!(projected.hidden_sort_columns, 0);
        assert_eq!(projected.order_by[0].index, 0);
        // Aggregated queries keep the strict behavior: no hidden columns.
        assert!(matches!(
            bind("SELECT COUNT(*) AS n FROM Events GROUP BY Name ORDER BY id"),
            Err(BindError::InvalidOrderBy(_))
        ));
    }

    #[test]
    fn union_all_unifies_numeric_branch_types() {
        // Events.id and users.id share a type; widen one side via CAST-free
        // literals instead: UInt vs Int literals unify to Int64.
        let query = bind("SELECT 1 AS v UNION ALL SELECT -2 ORDER BY v").expect("unified union");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Int64));
        assert!(matches!(
            bind("SELECT Name AS v FROM Events UNION ALL SELECT id FROM users"),
            Err(BindError::IncompatibleSetOperation(_))
        ));
    }

    #[test]
    fn binds_type_compatible_union_all_with_outer_clauses() {
        let query = bind(
            "SELECT id AS item FROM Events \
             UNION ALL SELECT id FROM users ORDER BY item DESC LIMIT 2",
        )
        .expect("union all");
        assert_eq!(query.union_all.len(), 1);
        assert_eq!(query.union_all[0].projection[0].name, "id");
        assert_eq!(query.order_by[0].index, 0);
        assert_eq!(query.limit.expect("limit").count, 2);

        // Plain UNION deduplicates; the flag records it for the planner.
        let distinct = bind(
            "SELECT email FROM users UNION SELECT email FROM users \
             UNION DISTINCT SELECT email FROM users",
        )
        .expect("union distinct");
        assert!(distinct.union_distinct);
        assert_eq!(distinct.union_all.len(), 2);
        let mixed = bind(
            "SELECT email FROM users UNION SELECT email FROM users \
             UNION ALL SELECT email FROM users",
        )
        .expect("a distinct left branch remains scoped before UNION ALL");
        assert_eq!(mixed.union_all.len(), 1);
        assert!(mixed.from[0].base.input.is_some());
        bind(
            "(SELECT id FROM Events ORDER BY id DESC LIMIT 1) \
             UNION ALL SELECT id FROM Events",
        )
        .expect("branch-local order and limit remain scoped");
        bind("SELECT 1 AS n EXCEPT SELECT 1 UNION ALL SELECT 2")
            .expect("set precedence remains representable");
        assert!(matches!(
            bind("SELECT id FROM Events UNION ALL SELECT email FROM users"),
            Err(BindError::IncompatibleSetOperation(_))
        ));
    }

    #[test]
    fn binds_string_condition_list_range_pattern_case_and_cast_expressions() {
        let query = bind(
            "SELECT CONCAT(LOWER(Name), UPPER('x')) AS text_value, \
             IF(active, id, 0) AS chosen, \
             CASE WHEN id BETWEEN 1 AND 3 THEN 'yes' ELSE 'no' END AS ranged, \
             Name LIKE 'a%' AS matched, id IN (1, 2, NULL) AS listed, \
             CAST(Name AS CHAR) AS cast_name, CONVERT(Name, CHAR) AS converted_name, \
             CONVERT(Name USING utf8mb4) AS transcoded_name FROM Events",
        )
        .expect("scalar functions");
        assert_eq!(query.projection.len(), 8);
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Utf8));
        assert_eq!(query.projection[1].expr.data_type, Some(DataType::Float64));
        assert_eq!(query.projection[3].expr.data_type, Some(DataType::Boolean));
        assert_eq!(query.projection[5].expr.data_type, Some(DataType::Utf8));
        assert_eq!(query.projection[6].expr.data_type, Some(DataType::Utf8));
        assert_eq!(query.projection[7].expr.data_type, Some(DataType::Utf8));
    }

    #[test]
    fn lowers_uncorrelated_constant_scalar_and_in_subqueries() {
        let query = bind(
            "SELECT (SELECT 1 + 2) AS scalar_value, \
             id IN (SELECT 1 UNION ALL SELECT 2) AS included FROM Events",
        )
        .expect("constant subqueries");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Int64));
        assert_eq!(query.projection[1].expr.data_type, Some(DataType::Boolean));
        assert_eq!(
            bind("SELECT (SELECT 1 UNION ALL SELECT 2)"),
            Err(BindError::InvalidScalarSubqueryRows(2))
        );
    }

    #[test]
    fn binds_table_reading_scalar_and_in_subqueries() {
        let query = bind(
            "SELECT (SELECT MAX(id) FROM users) AS largest_user, \
             id IN (SELECT id FROM users WHERE id >= 2) AS known \
             FROM Events",
        )
        .expect("relational subqueries");

        assert_eq!(query.projection[0].expr.data_type, Some(DataType::UInt64));
        assert_eq!(query.projection[1].expr.data_type, Some(DataType::Boolean));
    }

    #[test]
    fn binds_non_recursive_ctes_as_typed_relations() {
        let query = bind(
            "WITH recent AS (\
               SELECT id, Name AS label FROM Events WHERE id >= 10\
             ) \
             SELECT recent.id, recent.label FROM recent WHERE recent.id < 20",
        )
        .expect("plain CTE");

        assert_eq!(query.tables.len(), 1);
        assert_eq!(query.from[0].base.relation_name, "recent");
        assert_eq!(
            query
                .projection
                .iter()
                .map(|projection| projection.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "label"]
        );
        // A non-recursive CTE under the RECURSIVE keyword binds normally.
        bind("WITH RECURSIVE numbers AS (SELECT 1) SELECT * FROM numbers")
            .expect("non-recursive CTE under RECURSIVE");
    }

    #[test]
    fn binds_recursive_ctes_as_fixpoints() {
        let query = bind(
            "WITH RECURSIVE seq (n) AS (\
               SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 10\
             ) SELECT n FROM seq ORDER BY n",
        )
        .expect("recursive CTE");
        let input = query.tables[0].input.as_ref().expect("derived input");
        let recursive = input.recursive.as_ref().expect("recursive spec");
        assert!(!recursive.distinct);
        assert_eq!(recursive.member.projection.len(), 1);
        // Aggregates in the recursive member reject (surfaced as the
        // original bind error by the recursive fallback).
        assert!(
            bind(
                "WITH RECURSIVE bad (n) AS (\
                   SELECT 1 UNION ALL SELECT MAX(n) FROM bad\
                 ) SELECT n FROM bad"
            )
            .is_err()
        );
    }

    #[test]
    fn binds_grouping_aggregates_and_having_to_positional_slots() {
        let query = bind(
            "SELECT active, COUNT(*) AS rows, SUM(DISTINCT id) AS total \
             FROM Events GROUP BY active HAVING COUNT(*) > 1",
        )
        .expect("aggregate query");

        assert_eq!(query.group_by.len(), 1);
        assert_eq!(query.aggregates.len(), 2);
        assert_eq!(query.aggregates[0].function, AggregateFunction::Count);
        assert_eq!(query.aggregates[1].function, AggregateFunction::Sum);
        assert!(query.aggregates[1].distinct);
        assert!(matches!(
            query.projection[0].expr.kind,
            BoundExprKind::GroupKey(0)
        ));
        assert!(matches!(
            query.projection[1].expr.kind,
            BoundExprKind::Aggregate(1)
        ));
        assert!(query.having.is_some());
    }

    #[test]
    fn resolves_grouping_aliases_to_projection_expressions() {
        let query = bind(
            "SELECT active AS state, COUNT(*) AS rows \
             FROM Events GROUP BY state ORDER BY rows DESC",
        )
        .expect("grouping alias");

        assert_eq!(query.group_by.len(), 1);
        assert!(matches!(
            query.projection[0].expr.kind,
            BoundExprKind::GroupKey(0)
        ));
    }

    #[test]
    fn resolves_text_collation_through_query_layout_slots() {
        const SOURCE_COLLATION: &str = "fixture_source_ci";

        let mut grouped = bind("SELECT Name FROM Events GROUP BY Name").expect("group bind");
        let BoundExprKind::Column(column) = &mut grouped.group_by[0].kind else {
            panic!("grouping source column");
        };
        column.collation = Some(SOURCE_COLLATION.to_owned());
        assert_eq!(
            grouped.result_collation(&grouped.projection[0].expr),
            Some(SOURCE_COLLATION.to_owned())
        );

        let mut aggregate = bind("SELECT MIN(Name) FROM Events").expect("aggregate bind");
        let BoundExprKind::Column(column) = &mut aggregate.aggregates[0]
            .expr
            .as_mut()
            .expect("aggregate input")
            .kind
        else {
            panic!("aggregate source column");
        };
        column.collation = Some(SOURCE_COLLATION.to_owned());
        assert_eq!(
            aggregate.result_collation(&aggregate.projection[0].expr),
            Some(SOURCE_COLLATION.to_owned())
        );

        let mut window =
            bind("SELECT LAG(Name) OVER (ORDER BY id) FROM Events").expect("window bind");
        let crate::WindowFunction::Offset { expr, .. } = &mut window.windows[0].function else {
            panic!("offset window");
        };
        let BoundExprKind::Column(column) = &mut expr.kind else {
            panic!("window source column");
        };
        column.collation = Some(SOURCE_COLLATION.to_owned());
        assert_eq!(
            window.result_collation(&window.projection[0].expr),
            Some(SOURCE_COLLATION.to_owned())
        );
    }

    #[test]
    fn decimal_aggregates_use_the_numeric_executor_carrier() {
        let query = bind("SELECT SUM(amount), AVG(amount) FROM payments").expect("decimal bind");

        // SUM over DECIMAL stays DECIMAL at the input scale with widened
        // precision; AVG follows MySQL and widens the decimal by four
        // fraction digits (amount is DECIMAL(10,2)).
        assert_eq!(
            query.aggregates[0].data_type,
            Some(DataType::Decimal {
                precision: 22,
                scale: 2
            })
        );
        assert_eq!(
            query.aggregates[1].data_type,
            Some(DataType::Decimal {
                precision: 16,
                scale: 6
            })
        );
    }

    #[test]
    fn decimal_modulo_keeps_mysql_precision_and_scale() {
        let query = bind(
            "SELECT CAST(12.50 AS DECIMAL(4,2)) % CAST(0.70 AS DECIMAL(3,2)), \
             MOD(CAST(9007199254740993 AS DECIMAL(16,0)), 2)",
        )
        .expect("decimal modulo bind");

        assert_eq!(
            query.projection[0].expr.data_type,
            Some(DataType::Decimal {
                precision: 3,
                scale: 2,
            })
        );
        assert_eq!(
            query.projection[1].expr.data_type,
            Some(DataType::Decimal {
                precision: 16,
                scale: 0,
            })
        );
    }

    #[test]
    fn decimal_arithmetic_keeps_mysql_precision_and_scale() {
        let query = bind(
            "SELECT CAST(12.34 AS DECIMAL(5,2)) + CAST(1.234 AS DECIMAL(4,3)), \
             CAST(12.34 AS DECIMAL(5,2)) - CAST(1.234 AS DECIMAL(4,3)), \
             CAST(12.34 AS DECIMAL(5,2)) * CAST(1.234 AS DECIMAL(4,3)), \
             CAST(12.34 AS DECIMAL(5,2)) / CAST(1.234 AS DECIMAL(4,3)), \
             -CAST(12.34 AS DECIMAL(5,2))",
        )
        .expect("decimal arithmetic bind");

        let result_types = query
            .projection
            .iter()
            .map(|projection| projection.expr.data_type)
            .collect::<Vec<_>>();
        assert_eq!(
            result_types,
            vec![
                Some(DataType::Decimal {
                    precision: 7,
                    scale: 3,
                }),
                Some(DataType::Decimal {
                    precision: 7,
                    scale: 3,
                }),
                Some(DataType::Decimal {
                    precision: 9,
                    scale: 5,
                }),
                Some(DataType::Decimal {
                    precision: 12,
                    scale: 6,
                }),
                Some(DataType::Decimal {
                    precision: 5,
                    scale: 2,
                }),
            ]
        );
    }

    #[test]
    fn deduplicates_aggregates_and_enforces_full_grouping() {
        let query =
            bind("SELECT COUNT(*) AS first, COUNT(*) + 1 AS second FROM Events").expect("bind");
        assert_eq!(query.aggregates.len(), 1);

        assert!(matches!(
            bind("SELECT Name, COUNT(*) FROM Events"),
            Err(BindError::UngroupedColumn(_))
        ));
        assert!(matches!(
            bind("SELECT COUNT(DISTINCT *) FROM Events"),
            Err(BindError::UnsupportedAggregate(_))
        ));
    }
}

/// Picks the one collation a plan will compare with.
///
/// Absent any text, the default stands - it is never consulted, and giving it
/// a value keeps every downstream operator free of an Option it cannot act on.
fn resolve_query_collation(collations: &[String]) -> Result<&'static str, BindError> {
    match collations {
        [] => Ok(DEFAULT_TEXT_COLLATION),
        [only] => crate::bound::SUPPORTED_TEXT_COLLATIONS
            .into_iter()
            .find(|supported| supported == only)
            .ok_or_else(|| {
                BindError::UnsupportedExpression(format!(
                    "text collation {only} is unsupported; supported: {}",
                    crate::bound::SUPPORTED_TEXT_COLLATIONS.join(", "),
                ))
            }),
        many => Err(BindError::UnsupportedExpression(format!(
            "this query reads text in more than one collation ({}), and they do \
             not agree about trailing spaces or about characters above the BMP",
            many.join(", "),
        ))),
    }
}
