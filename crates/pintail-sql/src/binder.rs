use std::{cell::Cell, fmt};

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, Value};
use sqlparser::ast::{
    BinaryOperator, CastKind, CeilFloorKind, DataType as SqlDataType, DateTimeField, Distinct,
    DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Ident, JoinConstraint, JoinOperator, LimitClause, ObjectName, OrderByKind, Query,
    Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator, SetQuantifier,
    Statement, TableFactor, UnaryOperator, Value as SqlValue, WildcardAdditionalOptions,
    WindowType,
};

use crate::bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundFrom,
    BoundJoin, BoundJoinKind, BoundLimit, BoundOrderKey, BoundProjection, BoundQuery, BoundTable,
    BoundWindow, BoundWindowOrderKey, DatePart, IntervalUnit, ScalarFunction, UnaryOp,
    WindowFunction,
};

/// Binds parsed SQL against one immutable catalog view.
pub struct Binder<'catalog> {
    catalog: &'catalog CatalogSnapshot,
    current_database: Option<&'catalog str>,
    next_derived_id: Cell<u64>,
}

#[derive(Clone)]
struct BoundCte {
    name: String,
    column_names: Vec<String>,
    query: BoundQuery,
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
        }
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
        self.bind_query(query, &[])
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
            if with.recursive {
                return Err(BindError::UnsupportedQueryClause(with.to_string()));
            }
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
                let bound = self.bind_query(&cte.query, &ctes)?;
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
                });
            }
        }

        let mut bound = self.bind_set_expr(&query.body, &ctes)?;
        bind_order_by(query, &mut bound)?;
        bound.limit = query.limit_clause.as_ref().map(bind_limit).transpose()?;
        Ok(bound)
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
                set_quantifier: SetQuantifier::All,
                right,
            } => {
                let mut left = self.bind_set_expr(left, ctes)?;
                let mut right = self.bind_set_expr(right, ctes)?;
                unify_union_layout(&mut left, &mut right)?;
                left.union_all.push(right);
                Ok(left)
            }
            _ => Err(BindError::UnsupportedQueryBody(expression.to_string())),
        }
    }

    fn bind_select(&self, select: &Select, ctes: &[BoundCte]) -> Result<BoundQuery, BindError> {
        validate_select_shape(select)?;

        let (from, tables) = self.bind_from(select, ctes)?;
        let resolve_subquery = |query: &Query| self.bind_query(query, ctes);
        let filter = select
            .selection
            .as_ref()
            .map(|expr| bind_expr(expr, &tables, Some(&resolve_subquery)))
            .transpose()?;
        if let Some(filter) = &filter
            && !is_truth_value(filter.data_type)
        {
            return Err(BindError::ExpectedPredicate {
                actual: filter.data_type,
            });
        }
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let mut projection = bind_projection(
            &select.projection,
            &tables,
            Some(&mut aggregates),
            Some(&mut windows),
            Some(&resolve_subquery),
        )?;
        let group_by = bind_group_by(
            &select.group_by,
            &select.projection,
            &tables,
            Some(&resolve_subquery),
        )?;
        let mut having = select
            .having
            .as_ref()
            .map(|expr| {
                bind_aggregate_expr(expr, &tables, &mut aggregates, Some(&resolve_subquery))
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
        Ok(BoundQuery {
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
            windows,
            limit: None,
        })
    }

    fn bind_from(
        &self,
        select: &Select,
        ctes: &[BoundCte],
    ) -> Result<(Vec<BoundFrom>, Vec<BoundTable>), BindError> {
        let mut from = Vec::with_capacity(select.from.len());
        let mut tables = Vec::new();
        for table_with_joins in &select.from {
            let base = self.bind_table(&table_with_joins.relation, ctes)?;
            reject_duplicate_relation(&tables, &base)?;
            tables.push(base.clone());

            let mut joins = Vec::with_capacity(table_with_joins.joins.len());
            for join in &table_with_joins.joins {
                if join.global {
                    return Err(BindError::UnsupportedQueryClause(join.to_string()));
                }
                let table = self.bind_table(&join.relation, ctes)?;
                reject_duplicate_relation(&tables, &table)?;
                tables.push(table.clone());
                let (kind, constraint) = bind_join_operator(&join.join_operator)?;
                let condition = match constraint {
                    JoinConstraint::On(condition) => {
                        let resolve_subquery = |query: &Query| self.bind_query(query, ctes);
                        Some(bind_expr(condition, &tables, Some(&resolve_subquery))?)
                    }
                    JoinConstraint::None if kind == BoundJoinKind::Cross => None,
                    JoinConstraint::None if kind == BoundJoinKind::Inner => None,
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
        }
        Ok((from, tables))
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
        left.expr.data_type = unified;
        right.expr.data_type = unified;
        let nullable = left.expr.nullable || right.expr.nullable;
        left.expr.nullable = nullable;
        right.expr.nullable = nullable;
    }
    Ok(())
}

/// MySQL-style result type for a UNION column pair: equal types pass
/// through; numeric families widen (values are already Int64/UInt64/Float64
/// at execution, so widening only fixes the declared metadata). Mixed
/// signed/UInt64 and cross-kind pairs (text vs number, temporal vs number)
/// stay rejected.
// Outer None = incompatible pair; inner None = still untyped (NULL literals
// on both branches). A dedicated enum would just restate Option twice.
#[allow(clippy::option_option)]
fn unify_union_types(left: Option<DataType>, right: Option<DataType>) -> Option<Option<DataType>> {
    fn unsigned_rank(data_type: DataType) -> Option<u8> {
        match data_type {
            DataType::UInt8 => Some(0),
            DataType::UInt16 => Some(1),
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
        return Some(Some(DataType::Decimal {
            precision: lp.max(rp),
            scale: ls.max(rs),
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
        || !select.named_window.is_empty()
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
    mut aggregates: Option<&mut Vec<BoundAggregate>>,
    mut windows: Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<Vec<BoundProjection>, BindError> {
    let mut projection = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let bound =
                    bind_expr_inner(expr, tables, &mut aggregates, &mut windows, subqueries)?;
                projection.push(BoundProjection {
                    name: projection_name(expr),
                    expr: bound,
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let bound =
                    bind_expr_inner(expr, tables, &mut aggregates, &mut windows, subqueries)?;
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
                for table in tables {
                    extend_wildcard(&mut projection, table);
                }
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
    expressions
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
        .collect()
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
        // sqlparser gives CEIL/FLOOR dedicated nodes (for the `TO field`
        // form); only the plain numeric spelling is supported.
        Expr::Ceil { expr: inner, field } | Expr::Floor { expr: inner, field } => {
            if !matches!(
                field,
                CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
            ) {
                return Err(BindError::UnsupportedExpression(expr.to_string()));
            }
            let function = if matches!(expr, Expr::Ceil { .. }) {
                ScalarFunction::Ceil
            } else {
                ScalarFunction::Floor
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
        Expr::Subquery(query) => bind_scalar_subquery(query, subqueries),
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
            let [projection] = query.projection.as_slice() else {
                return Err(BindError::UnsupportedSubquery(
                    "scalar subquery must produce exactly one column".to_owned(),
                ));
            };
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
            let [projection] = query.projection.as_slice() else {
                return Err(BindError::UnsupportedSubquery(
                    "IN subquery must produce exactly one column".to_owned(),
                ));
            };
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
    let matches = tables
        .iter()
        .flat_map(|table| &table.columns)
        .filter(|column| match identifiers {
            [column_name] => column.name.eq_ignore_ascii_case(&column_name.value),
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
    let value = match value {
        SqlValue::Null => Value::Null,
        SqlValue::Boolean(value) => Value::Boolean(*value),
        SqlValue::SingleQuotedString(value) => Value::Utf8(value.clone()),
        SqlValue::Number(value, _) => parse_number(value)?,
        _ => return Err(BindError::UnsupportedLiteral(value.to_string())),
    };
    Ok(BoundExpr {
        data_type: value.data_type(),
        nullable: matches!(value, Value::Null),
        kind: BoundExprKind::Literal(value),
    })
}

fn parse_number(value: &str) -> Result<Value, BindError> {
    if value.contains(['.', 'e', 'E']) {
        return value
            .parse::<f64>()
            .map(Value::float64)
            .map_err(|_| BindError::InvalidNumericLiteral(value.to_owned()));
    }
    if let Ok(value) = value.parse::<i64>() {
        return Ok(Value::Int64(value));
    }
    value
        .parse::<u64>()
        .map(Value::UInt64)
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

    let (left, right) = coerce_decimal_comparison(op, left, right);

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

/// Decimal-vs-decimal comparisons: both sides carry canonical text at
/// runtime, and the generic text comparison would order them lexically.
/// Route them through the numeric carrier the way `MySQL` compares decimals
/// as numbers (exact within f64's 15 significant digits).
fn coerce_decimal_comparison(
    op: BinaryOp,
    left: BoundExpr,
    right: BoundExpr,
) -> (BoundExpr, BoundExpr) {
    if matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual
    ) && matches!(left.data_type, Some(DataType::Decimal { .. }))
        && matches!(right.data_type, Some(DataType::Decimal { .. }))
    {
        (cast_to_float(left), cast_to_float(right))
    } else {
        (left, right)
    }
}

fn cast_to_float(expr: BoundExpr) -> BoundExpr {
    let nullable = expr.nullable;
    BoundExpr {
        kind: BoundExprKind::Scalar {
            function: ScalarFunction::Cast(DataType::Float64),
            args: vec![expr],
        },
        data_type: Some(DataType::Float64),
        nullable,
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

/// A top-level `function(...) OVER (...)` projection item.
#[allow(clippy::too_many_lines)]
fn bind_window_function(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
    {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let Some(WindowType::WindowSpec(spec)) = &function.over else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    if spec.window_name.is_some() || spec.window_frame.is_some() {
        // v1 windows run MySQL's default frames only.
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let name_parts = object_name_parts(&function.name)?;
    let [name] = name_parts.as_slice() else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let upper = name.to_ascii_uppercase();
    let ranking = match upper.as_str() {
        "ROW_NUMBER" => Some(WindowFunction::RowNumber),
        "RANK" => Some(WindowFunction::Rank),
        "DENSE_RANK" => Some(WindowFunction::DenseRank),
        _ => None,
    };
    let (window_function, data_type, nullable) = if let Some(window_function) = ranking {
        if !arguments.args.is_empty() {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        }
        (window_function, Some(DataType::UInt64), false)
    } else {
        {
            let aggregate_function = aggregate_function_name(function)
                .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
            if arguments.args.len() != 1 {
                return Err(BindError::UnsupportedAggregate(function.to_string()));
            }
            let expr = match &arguments.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(bind_expr_inner(
                    expr, tables, aggregates, &mut None, subqueries,
                )?),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                    if aggregate_function == AggregateFunction::Count =>
                {
                    None
                }
                _ => return Err(BindError::UnsupportedAggregate(function.to_string())),
            };
            let (data_type, nullable) = aggregate_result_type(aggregate_function, expr.as_ref())?;
            (
                WindowFunction::Aggregate(BoundAggregate {
                    function: aggregate_function,
                    expr,
                    distinct: false,
                    data_type,
                    nullable,
                }),
                data_type,
                nullable,
            )
        }
    };
    let partition_by = spec
        .partition_by
        .iter()
        .map(|expr| bind_expr_inner(expr, tables, aggregates, &mut None, subqueries))
        .collect::<Result<Vec<_>, _>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|order| {
            if order.with_fill.is_some() {
                return Err(BindError::InvalidOrderBy(order.to_string()));
            }
            let ascending = order.options.asc.unwrap_or(true);
            Ok(BoundWindowOrderKey {
                expr: bind_expr_inner(&order.expr, tables, aggregates, &mut None, subqueries)?,
                ascending,
                nulls_first: order.options.nulls_first.unwrap_or(ascending),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let window = BoundWindow {
        function: window_function,
        partition_by,
        order_by,
        data_type,
        nullable,
    };
    let window_list = windows
        .as_deref_mut()
        .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
    let index = window_list
        .iter()
        .position(|existing| existing == &window)
        .unwrap_or_else(|| {
            let index = window_list.len();
            window_list.push(window.clone());
            index
        });
    Ok(BoundExpr {
        kind: BoundExprKind::Window(index),
        data_type,
        nullable,
    })
}

fn bind_scalar_function(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
    {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let name = object_name_parts(&function.name)?;
    let [name] = name.as_slice() else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let function_name = name.to_ascii_uppercase();
    if matches!(function_name.as_str(), "DATE_ADD" | "DATE_SUB") {
        return bind_date_interval(
            function,
            arguments,
            function_name == "DATE_SUB",
            tables,
            aggregates,
            windows,
            subqueries,
        );
    }
    if function_name == "TIMESTAMPDIFF" {
        return bind_timestamp_diff(function, arguments, tables, aggregates, windows, subqueries);
    }
    let mut args = Vec::with_capacity(arguments.args.len());
    for argument in &arguments.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) = argument else {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        };
        args.push(bind_expr_inner(
            expression, tables, aggregates, windows, subqueries,
        )?);
    }
    let scalar = match function_name.as_str() {
        "CONCAT" if !args.is_empty() => ScalarFunction::Concat,
        "SUBSTRING" | "SUBSTR" if matches!(args.len(), 2 | 3) => ScalarFunction::Substring,
        "LOWER" | "LCASE" if args.len() == 1 => ScalarFunction::Lower,
        "UPPER" | "UCASE" if args.len() == 1 => ScalarFunction::Upper,
        "TRIM" if args.len() == 1 => ScalarFunction::Trim,
        "LENGTH" if args.len() == 1 => ScalarFunction::Length,
        "CHAR_LENGTH" | "CHARACTER_LENGTH" if args.len() == 1 => ScalarFunction::CharLength,
        "REPLACE" if args.len() == 3 => ScalarFunction::Replace,
        "LEFT" if args.len() == 2 => ScalarFunction::Left,
        "RIGHT" if args.len() == 2 => ScalarFunction::Right,
        "LOCATE" if matches!(args.len(), 2 | 3) => ScalarFunction::Locate,
        "IF" if args.len() == 3 => ScalarFunction::If,
        "IFNULL" if args.len() == 2 => ScalarFunction::Coalesce,
        "COALESCE" if !args.is_empty() => ScalarFunction::Coalesce,
        "NULLIF" if args.len() == 2 => ScalarFunction::NullIf,
        "ROUND" if matches!(args.len(), 1 | 2) => ScalarFunction::Round,
        "CEIL" | "CEILING" if args.len() == 1 => ScalarFunction::Ceil,
        "FLOOR" if args.len() == 1 => ScalarFunction::Floor,
        "NOW" if args.is_empty() => ScalarFunction::Now,
        "CURDATE" if args.is_empty() => ScalarFunction::CurrentDate,
        "DATE" if args.len() == 1 => ScalarFunction::Date,
        "YEAR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Year),
        "MONTH" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Month),
        "DAY" | "DAYOFMONTH" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Day),
        "HOUR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Hour),
        "MINUTE" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Minute),
        "SECOND" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Second),
        "DATE_FORMAT" if args.len() == 2 => ScalarFunction::DateFormat,
        "DATEDIFF" if args.len() == 2 => ScalarFunction::DateDiff,
        "UNIX_TIMESTAMP" if args.len() <= 1 => ScalarFunction::UnixTimestamp,
        "FROM_UNIXTIME" if args.len() == 1 => ScalarFunction::FromUnixTime,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
    bind_scalar(scalar, args)
}

fn bind_date_interval(
    function: &Function,
    arguments: &sqlparser::ast::FunctionArgumentList,
    subtract: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(date)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Interval(interval))),
    ] = arguments.args.as_slice()
    else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    bind_interval_arithmetic(
        date, interval, subtract, tables, aggregates, windows, subqueries,
    )
    .map_err(|_| BindError::UnsupportedExpression(function.to_string()))
}

/// Shared by `DATE_ADD`/`DATE_SUB` and `MySQL`'s inline operator form
/// (`expr +- INTERVAL n unit`): both are the same `DateInterval` scalar.
#[allow(clippy::too_many_arguments)]
fn bind_interval_arithmetic(
    date: &Expr,
    interval: &sqlparser::ast::Interval,
    subtract: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if interval.leading_precision.is_some()
        || interval.last_field.is_some()
        || interval.fractional_seconds_precision.is_some()
    {
        return Err(BindError::UnsupportedExpression(interval.to_string()));
    }
    let Some(unit) = interval_unit_of(interval.leading_field.as_ref()) else {
        return Err(BindError::UnsupportedExpression(interval.to_string()));
    };
    bind_scalar(
        ScalarFunction::DateInterval { unit, subtract },
        vec![
            bind_expr_inner(date, tables, aggregates, windows, subqueries)?,
            bind_expr_inner(&interval.value, tables, aggregates, windows, subqueries)?,
        ],
    )
}

fn interval_unit_of(field: Option<&DateTimeField>) -> Option<IntervalUnit> {
    match field {
        Some(DateTimeField::Year) => Some(IntervalUnit::Year),
        Some(DateTimeField::Month) => Some(IntervalUnit::Month),
        Some(DateTimeField::Day) => Some(IntervalUnit::Day),
        Some(DateTimeField::Hour) => Some(IntervalUnit::Hour),
        Some(DateTimeField::Minute) => Some(IntervalUnit::Minute),
        Some(DateTimeField::Second) => Some(IntervalUnit::Second),
        _ => None,
    }
}

/// `TIMESTAMPDIFF(unit, from, to)` — the unit is a bare keyword argument.
fn bind_timestamp_diff(
    function: &Function,
    arguments: &sqlparser::ast::FunctionArgumentList,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(unit)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(from)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(to)),
    ] = arguments.args.as_slice()
    else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let Expr::Identifier(unit) = unit else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let unit = match unit.value.to_ascii_uppercase().as_str() {
        "YEAR" => IntervalUnit::Year,
        "MONTH" => IntervalUnit::Month,
        "DAY" => IntervalUnit::Day,
        "HOUR" => IntervalUnit::Hour,
        "MINUTE" => IntervalUnit::Minute,
        "SECOND" => IntervalUnit::Second,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
    bind_scalar(
        ScalarFunction::TimestampDiff { unit },
        vec![
            bind_expr_inner(from, tables, aggregates, windows, subqueries)?,
            bind_expr_inner(to, tables, aggregates, windows, subqueries)?,
        ],
    )
}

fn bind_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if list.is_empty() {
        return Err(BindError::UnsupportedExpression(expr.to_string()));
    }
    let mut args = Vec::with_capacity(list.len() + 1);
    args.push(bind_expr_inner(
        expr, tables, aggregates, windows, subqueries,
    )?);
    for value in list {
        args.push(bind_expr_inner(
            value, tables, aggregates, windows, subqueries,
        )?);
    }
    if args[1..]
        .iter()
        .any(|value| !comparable(args[0].data_type, value.data_type))
    {
        return Err(BindError::InvalidScalarFunction("IN".to_owned()));
    }
    bind_scalar(ScalarFunction::InList { negated }, args)
}

#[allow(clippy::too_many_arguments)]
fn bind_between(
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let args = vec![
        bind_expr_inner(expr, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(low, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(high, tables, aggregates, windows, subqueries)?,
    ];
    if !comparable(args[0].data_type, args[1].data_type)
        || !comparable(args[0].data_type, args[2].data_type)
    {
        return Err(BindError::InvalidScalarFunction("BETWEEN".to_owned()));
    }
    bind_scalar(ScalarFunction::Between { negated }, args)
}

#[allow(clippy::too_many_arguments)]
fn bind_like(
    expr: &Expr,
    pattern: &Expr,
    negated: bool,
    escape: Option<&sqlparser::ast::ValueWithSpan>,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let escape = match escape {
        None => None,
        Some(escape) => Some(
            match &escape.value {
                SqlValue::SingleQuotedString(value) => {
                    let mut chars = value.chars();
                    let character = chars.next();
                    if chars.next().is_none() {
                        character
                    } else {
                        None
                    }
                }
                _ => None,
            }
            .ok_or_else(|| BindError::InvalidScalarFunction("LIKE ESCAPE".to_owned()))?,
        ),
    };
    let args = vec![
        bind_expr_inner(expr, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(pattern, tables, aggregates, windows, subqueries)?,
    ];
    bind_scalar(ScalarFunction::Like { negated, escape }, args)
}

fn bind_case(
    operand: Option<&Expr>,
    conditions: &[sqlparser::ast::CaseWhen],
    else_result: Option<&Expr>,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if conditions.is_empty() {
        return Err(BindError::InvalidScalarFunction("CASE".to_owned()));
    }
    let operand = operand
        .map(|expression| bind_expr_inner(expression, tables, aggregates, windows, subqueries))
        .transpose()?;
    let mut result = else_result.map_or_else(
        || {
            Ok(BoundExpr {
                kind: BoundExprKind::Literal(Value::Null),
                data_type: None,
                nullable: true,
            })
        },
        |expression| bind_expr_inner(expression, tables, aggregates, windows, subqueries),
    )?;
    for clause in conditions.iter().rev() {
        let condition =
            bind_expr_inner(&clause.condition, tables, aggregates, windows, subqueries)?;
        let condition = if let Some(operand) = &operand {
            equality_expr(operand.clone(), condition)?
        } else {
            if !is_truth_value(condition.data_type) {
                return Err(BindError::ExpectedPredicate {
                    actual: condition.data_type,
                });
            }
            condition
        };
        let value = bind_expr_inner(&clause.result, tables, aggregates, windows, subqueries)?;
        result = bind_scalar(ScalarFunction::If, vec![condition, value, result])?;
    }
    Ok(result)
}

fn bind_cast(
    expr: &Expr,
    data_type: &SqlDataType,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let target = cast_data_type(data_type)
        .ok_or_else(|| BindError::InvalidScalarFunction(format!("CAST AS {data_type}")))?;
    bind_scalar(
        ScalarFunction::Cast(target),
        vec![bind_expr_inner(
            expr, tables, aggregates, windows, subqueries,
        )?],
    )
}

fn bind_convert(
    conversion: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let Expr::Convert {
        is_try,
        expr,
        data_type,
        charset,
        target_before_value,
        styles,
    } = conversion
    else {
        unreachable!("bind_convert is called only for CONVERT expressions");
    };
    if *is_try || *target_before_value || !styles.is_empty() {
        return Err(BindError::UnsupportedExpression(conversion.to_string()));
    }
    let target = match (data_type, charset) {
        (Some(data_type), _) => cast_data_type(data_type)
            .ok_or_else(|| BindError::InvalidScalarFunction(format!("CONVERT TO {data_type}")))?,
        (None, Some(charset)) if charset.to_string().eq_ignore_ascii_case("binary") => {
            DataType::Binary
        }
        (None, Some(_)) => DataType::Utf8,
        (None, None) => {
            return Err(BindError::InvalidScalarFunction(
                "CONVERT requires a target type or character set".to_owned(),
            ));
        }
    };
    bind_scalar(
        ScalarFunction::Cast(target),
        vec![bind_expr_inner(
            expr, tables, aggregates, windows, subqueries,
        )?],
    )
}

fn cast_data_type(data_type: &SqlDataType) -> Option<DataType> {
    let name = data_type.to_string().to_ascii_uppercase();
    if name.contains("BINARY") || name.contains("BLOB") {
        Some(DataType::Binary)
    } else if name.contains("CHAR") || name.contains("TEXT") {
        Some(DataType::Utf8)
    } else if name.contains("UNSIGNED") {
        Some(DataType::UInt64)
    } else if name.contains("DOUBLE")
        || name.contains("FLOAT")
        || name.contains("REAL")
        || name.contains("DECIMAL")
    {
        Some(DataType::Float64)
    } else if name.contains("INT") || name == "SIGNED" {
        Some(DataType::Int64)
    } else if name.contains("BOOL") {
        Some(DataType::Boolean)
    } else {
        None
    }
}

fn bind_scalar(function: ScalarFunction, args: Vec<BoundExpr>) -> Result<BoundExpr, BindError> {
    let (data_type, nullable) = match function {
        ScalarFunction::Concat
        | ScalarFunction::Substring
        | ScalarFunction::Lower
        | ScalarFunction::Upper
        | ScalarFunction::Trim
        | ScalarFunction::Replace
        | ScalarFunction::Left
        | ScalarFunction::Right => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Length | ScalarFunction::CharLength | ScalarFunction::Locate => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Like { .. }
        | ScalarFunction::InList { .. }
        | ScalarFunction::Between { .. } => (
            Some(DataType::Boolean),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::If => (
            common_result_type(&args[1..])?,
            args[1..].iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Coalesce => (
            common_result_type(&args)?,
            args.iter().all(|argument| argument.nullable),
        ),
        ScalarFunction::NullIf => (args[0].data_type, true),
        ScalarFunction::Round | ScalarFunction::Ceil | ScalarFunction::Floor => (
            Some(DataType::Float64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::TimestampDiff { .. } => (
            Some(DataType::Int64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Cast(target) => (Some(target), args[0].nullable),
        ScalarFunction::Now | ScalarFunction::CurrentDate => (Some(DataType::Utf8), false),
        ScalarFunction::Date
        | ScalarFunction::DateFormat
        | ScalarFunction::DateInterval { .. }
        | ScalarFunction::FromUnixTime => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::DatePart(_) | ScalarFunction::UnixTimestamp => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::DateDiff => (
            Some(DataType::Int64),
            args.iter().any(|argument| argument.nullable),
        ),
    };
    let mut args = args;
    // A decimal-unified IF/COALESCE must also coerce its branch VALUES:
    // integer branches would otherwise reach decimal-typed consumers (SUM,
    // the wire layer) as raw integers and lose the declared scale.
    if let Some(unified @ DataType::Decimal { .. }) = data_type
        && matches!(function, ScalarFunction::If | ScalarFunction::Coalesce)
    {
        let first_value = usize::from(matches!(function, ScalarFunction::If));
        for argument in args.iter_mut().skip(first_value) {
            if argument.data_type != Some(unified)
                && !matches!(argument.kind, BoundExprKind::Literal(Value::Null))
            {
                let nullable = argument.nullable;
                let inner = std::mem::replace(
                    argument,
                    BoundExpr {
                        kind: BoundExprKind::Literal(Value::Null),
                        data_type: None,
                        nullable: true,
                    },
                );
                *argument = BoundExpr {
                    kind: BoundExprKind::Scalar {
                        function: ScalarFunction::Cast(unified),
                        args: vec![inner],
                    },
                    data_type: Some(unified),
                    nullable,
                };
            }
        }
    }
    Ok(BoundExpr {
        kind: BoundExprKind::Scalar { function, args },
        data_type,
        nullable,
    })
}

fn common_result_type(args: &[BoundExpr]) -> Result<Option<DataType>, BindError> {
    let types = args
        .iter()
        .filter_map(|argument| argument.data_type)
        .collect::<Vec<_>>();
    let Some(first) = types.first().copied() else {
        return Ok(None);
    };
    if types.iter().all(|data_type| *data_type == first) {
        return Ok(Some(first));
    }
    if types
        .iter()
        .all(|data_type| is_mysql_scalar(Some(*data_type)))
    {
        if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Utf8 | DataType::Binary))
        {
            Ok(Some(DataType::Utf8))
        } else if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Float32 | DataType::Float64))
        {
            Ok(Some(DataType::Float64))
        } else if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Decimal { .. }))
        {
            // MySQL keeps CASE/COALESCE branches exact when decimals mix
            // with integers: unify to a decimal wide enough for every
            // branch. Collapsing to an integer type truncated the fraction
            // (2026-08-03 production-acceptance q01).
            let mut scale: u8 = 0;
            let mut integer_digits: u8 = 0;
            for data_type in &types {
                let (branch_scale, branch_integer) =
                    exact_numeric_digits(*data_type).unwrap_or((0, 20));
                scale = scale.max(branch_scale);
                integer_digits = integer_digits.max(branch_integer);
            }
            Ok(Some(DataType::Decimal {
                precision: integer_digits
                    .saturating_add(scale)
                    .min(MAX_DECIMAL_PRECISION),
                scale,
            }))
        } else if types.contains(&DataType::Int64) && types.contains(&DataType::UInt64) {
            Ok(Some(DataType::Float64))
        } else if types.contains(&DataType::UInt64) {
            Ok(Some(DataType::UInt64))
        } else {
            Ok(Some(DataType::Int64))
        }
    } else {
        Err(BindError::InvalidScalarFunction(
            "incompatible result types".to_owned(),
        ))
    }
}

fn equality_expr(left: BoundExpr, right: BoundExpr) -> Result<BoundExpr, BindError> {
    if !comparable(left.data_type, right.data_type) {
        return Err(BindError::InvalidBinaryTypes {
            operation: "=".to_owned(),
            left: left.data_type,
            right: right.data_type,
        });
    }
    Ok(BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: Box::new(left),
            right: Box::new(right),
        },
    })
}

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
    if !arguments.clauses.is_empty() || arguments.args.len() != 1 {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    }
    let distinct = match arguments.duplicate_treatment {
        Some(DuplicateTreatment::Distinct) => true,
        None | Some(DuplicateTreatment::All) => false,
    };
    let expr = match &arguments.args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
            Some(bind_expr(expr, tables, subqueries)?)
        }
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
            if aggregate_function == AggregateFunction::Count && !distinct =>
        {
            None
        }
        _ => return Err(BindError::UnsupportedAggregate(function.to_string())),
    };
    let (data_type, nullable) = aggregate_result_type(aggregate_function, expr.as_ref())?;
    let aggregate = BoundAggregate {
        function: aggregate_function,
        expr,
        distinct,
        data_type,
        nullable,
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
        AggregateFunction::Minimum | AggregateFunction::Maximum if is_mysql_scalar(input_type) => {
            Ok((input_type, true))
        }
        AggregateFunction::GroupConcat if is_mysql_scalar(input_type) => {
            Ok((Some(DataType::Utf8), true))
        }
        _ => Err(BindError::InvalidAggregateType {
            function,
            actual: input_type,
        }),
    }
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
        _ => None,
    }
}

/// `MySQL` `div_precision_increment` default: division and `AVG` widen the
/// dividend's scale by four fraction digits.
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
    // Division over exact numerics follows MySQL decimal semantics: the
    // result is a DECIMAL widened by div_precision_increment, evaluated
    // exactly. Everything else with a decimal operand still passes through
    // Float64 (limitations.md); routing it to an integer arm would truncate
    // the fraction.
    if op == BinaryOp::Divide
        && let Some(result) = division_result_type(left, right)
    {
        return Some(result);
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
    is_mysql_scalar(left) && is_mysql_scalar(right)
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
        .expect("table");
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
        let database =
            DatabaseEntry::new(DatabaseId::new(7), "Analytics", [events, users, payments])
                .expect("database");
        CatalogSnapshot::new([database]).expect("catalog")
    }

    fn bind(sql: &str) -> Result<crate::BoundQuery, BindError> {
        let catalog = catalog();
        let statement = parse_statement(sql).expect("parse");
        Binder::new(&catalog, Some("analytics")).bind(&statement)
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
        assert_eq!(total.data_type, Some(DataType::Float64));
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
    }

    #[test]
    fn rejects_unsupported_join_directions_and_constraints() {
        assert!(matches!(
            bind("SELECT * FROM Events RIGHT JOIN users ON Events.id = users.id"),
            Err(BindError::UnsupportedJoinOperator(_))
        ));
        assert!(matches!(
            bind("SELECT * FROM Events JOIN users USING (id)"),
            Err(BindError::UnsupportedJoinConstraint(_))
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

        assert!(matches!(
            bind("SELECT id FROM Events UNION SELECT id FROM users"),
            Err(BindError::UnsupportedQueryBody(_))
        ));
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
        assert!(matches!(
            bind("WITH RECURSIVE numbers AS (SELECT 1) SELECT * FROM numbers"),
            Err(BindError::UnsupportedQueryClause(_))
        ));
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
