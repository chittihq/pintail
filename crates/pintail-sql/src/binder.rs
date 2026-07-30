use std::fmt;

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, Value};
use sqlparser::ast::{
    BinaryOperator, Distinct, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator, LimitClause, ObjectName,
    Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor,
    UnaryOperator, Value as SqlValue, WildcardAdditionalOptions,
};

use crate::bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundFrom,
    BoundJoin, BoundJoinKind, BoundLimit, BoundProjection, BoundQuery, BoundTable, UnaryOp,
};

/// Binds parsed SQL against one immutable catalog view.
pub struct Binder<'catalog> {
    catalog: &'catalog CatalogSnapshot,
    current_database: Option<&'catalog str>,
}

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
        self.bind_query(query)
    }

    fn bind_query(&self, query: &Query) -> Result<BoundQuery, BindError> {
        if query.with.is_some()
            || query.order_by.is_some()
            || query.fetch.is_some()
            || !query.locks.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return Err(BindError::UnsupportedQueryClause(query.to_string()));
        }

        let SetExpr::Select(select) = query.body.as_ref() else {
            return Err(BindError::UnsupportedQueryBody(query.body.to_string()));
        };
        validate_select_shape(select)?;

        let (from, tables) = self.bind_from(select)?;
        let filter = select
            .selection
            .as_ref()
            .map(|expr| bind_expr(expr, &tables))
            .transpose()?;
        if let Some(filter) = &filter
            && !is_truth_value(filter.data_type)
        {
            return Err(BindError::ExpectedPredicate {
                actual: filter.data_type,
            });
        }
        let group_by = bind_group_by(&select.group_by, &tables)?;
        let mut aggregates = Vec::new();
        let mut projection = bind_projection(&select.projection, &tables, Some(&mut aggregates))?;
        let mut having = select
            .having
            .as_ref()
            .map(|expr| bind_aggregate_expr(expr, &tables, &mut aggregates))
            .transpose()?;
        if let Some(predicate) = &having
            && !is_truth_value(predicate.data_type)
        {
            return Err(BindError::ExpectedPredicate {
                actual: predicate.data_type,
            });
        }
        if !group_by.is_empty() || !aggregates.is_empty() {
            for item in &mut projection {
                rewrite_group_references(&mut item.expr, &group_by)?;
            }
            if let Some(predicate) = &mut having {
                rewrite_group_references(predicate, &group_by)?;
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
        let limit = query.limit_clause.as_ref().map(bind_limit).transpose()?;

        Ok(BoundQuery {
            from,
            tables,
            projection,
            filter,
            group_by,
            aggregates,
            having,
            distinct,
            limit,
        })
    }

    fn bind_from(&self, select: &Select) -> Result<(Vec<BoundFrom>, Vec<BoundTable>), BindError> {
        let mut from = Vec::with_capacity(select.from.len());
        let mut tables = Vec::new();
        for table_with_joins in &select.from {
            let base = self.bind_table(&table_with_joins.relation)?;
            reject_duplicate_relation(&tables, &base)?;
            tables.push(base.clone());

            let mut joins = Vec::with_capacity(table_with_joins.joins.len());
            for join in &table_with_joins.joins {
                if join.global {
                    return Err(BindError::UnsupportedQueryClause(join.to_string()));
                }
                let table = self.bind_table(&join.relation)?;
                reject_duplicate_relation(&tables, &table)?;
                tables.push(table.clone());
                let (kind, constraint) = bind_join_operator(&join.join_operator)?;
                let condition = match constraint {
                    JoinConstraint::On(condition) => Some(bind_expr(condition, &tables)?),
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

    fn bind_table(&self, factor: &TableFactor) -> Result<BoundTable, BindError> {
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
        })
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
) -> Result<Vec<BoundProjection>, BindError> {
    let mut projection = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let bound = bind_expr_inner(expr, tables, &mut aggregates)?;
                projection.push(BoundProjection {
                    name: projection_name(expr),
                    expr: bound,
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                projection.push(BoundProjection {
                    name: alias.value.clone(),
                    expr: bind_expr_inner(expr, tables, &mut aggregates)?,
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
    tables: &[BoundTable],
) -> Result<Vec<BoundExpr>, BindError> {
    let GroupByExpr::Expressions(expressions, modifiers) = group_by else {
        return Err(BindError::UnsupportedQueryClause(group_by.to_string()));
    };
    if !modifiers.is_empty() {
        return Err(BindError::UnsupportedQueryClause(group_by.to_string()));
    }
    expressions
        .iter()
        .map(|expr| bind_expr(expr, tables))
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

fn bind_expr(expr: &Expr, tables: &[BoundTable]) -> Result<BoundExpr, BindError> {
    let mut aggregates = None;
    bind_expr_inner(expr, tables, &mut aggregates)
}

fn bind_aggregate_expr(
    expr: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Vec<BoundAggregate>,
) -> Result<BoundExpr, BindError> {
    let mut aggregate_context = Some(aggregates);
    bind_expr_inner(expr, tables, &mut aggregate_context)
}

fn bind_expr_inner(
    expr: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
) -> Result<BoundExpr, BindError> {
    match expr {
        Expr::Identifier(identifier) => bind_column(std::slice::from_ref(identifier), tables),
        Expr::CompoundIdentifier(identifiers) => bind_column(identifiers, tables),
        Expr::Value(value) => bind_literal(&value.value),
        Expr::Nested(expr) => bind_expr_inner(expr, tables, aggregates),
        Expr::UnaryOp { op, expr } => bind_unary(*op, expr, tables, aggregates),
        Expr::BinaryOp { left, op, right } => bind_binary(left, op, right, tables, aggregates),
        Expr::IsNull(expr) => bind_is_null(expr, false, tables, aggregates),
        Expr::IsNotNull(expr) => bind_is_null(expr, true, tables, aggregates),
        Expr::Function(function) if aggregates.is_some() => {
            bind_aggregate(function, tables, aggregates)
        }
        _ => Err(BindError::UnsupportedExpression(expr.to_string())),
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
) -> Result<BoundExpr, BindError> {
    let expr = bind_expr_inner(expr, tables, aggregates)?;
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
) -> Result<BoundExpr, BindError> {
    let left = bind_expr_inner(left, tables, aggregates)?;
    let right = bind_expr_inner(right, tables, aggregates)?;
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

fn bind_is_null(
    expr: &Expr,
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
) -> Result<BoundExpr, BindError> {
    let expr = bind_expr_inner(expr, tables, aggregates)?;
    Ok(BoundExpr {
        kind: BoundExprKind::IsNull {
            expr: Box::new(expr),
            negated,
        },
        data_type: Some(DataType::Boolean),
        nullable: false,
    })
}

fn bind_aggregate(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
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
    let [name] = name.as_slice() else {
        return Err(BindError::UnsupportedAggregate(function.to_string()));
    };
    let aggregate_function = match name.to_ascii_uppercase().as_str() {
        "COUNT" => AggregateFunction::Count,
        "SUM" => AggregateFunction::Sum,
        "AVG" => AggregateFunction::Average,
        "MIN" => AggregateFunction::Minimum,
        "MAX" => AggregateFunction::Maximum,
        "GROUP_CONCAT" => AggregateFunction::GroupConcat,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
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
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(bind_expr(expr, tables)?),
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

fn aggregate_result_type(
    function: AggregateFunction,
    expr: Option<&BoundExpr>,
) -> Result<(Option<DataType>, bool), BindError> {
    let input_type = expr.and_then(|expr| expr.data_type);
    match function {
        AggregateFunction::Count => Ok((Some(DataType::UInt64), false)),
        AggregateFunction::Average if is_numeric(input_type) => Ok((Some(DataType::Float64), true)),
        AggregateFunction::Sum if is_numeric(input_type) => {
            let result = if input_type == Some(DataType::UInt64) {
                DataType::UInt64
            } else if input_type == Some(DataType::Float64)
                || matches!(input_type, Some(DataType::Utf8 | DataType::Binary))
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
        BoundExprKind::Aggregate(index) => {
            *index = index.saturating_add(group_by.len());
            Ok(())
        }
        BoundExprKind::Literal(_) | BoundExprKind::GroupKey(_) => Ok(()),
    }
}

fn arithmetic_type(
    op: BinaryOp,
    left: Option<DataType>,
    right: Option<DataType>,
) -> Option<DataType> {
    if left.is_none() || right.is_none() {
        return None;
    }
    if op == BinaryOp::Divide
        || left == Some(DataType::Float64)
        || right == Some(DataType::Float64)
        || matches!(left, Some(DataType::Utf8 | DataType::Binary))
        || matches!(right, Some(DataType::Utf8 | DataType::Binary))
    {
        Some(DataType::Float64)
    } else if left == Some(DataType::UInt64) && right == Some(DataType::UInt64) {
        Some(DataType::UInt64)
    } else {
        Some(DataType::Int64)
    }
}

fn comparable(left: Option<DataType>, right: Option<DataType>) -> bool {
    is_mysql_scalar(left) && is_mysql_scalar(right)
}

fn is_numeric(data_type: Option<DataType>) -> bool {
    is_mysql_scalar(data_type)
}

fn is_truth_value(data_type: Option<DataType>) -> bool {
    is_mysql_scalar(data_type)
}

fn is_mysql_scalar(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
        None | Some(
            DataType::Boolean
                | DataType::Int64
                | DataType::UInt64
                | DataType::Float64
                | DataType::Utf8
                | DataType::Binary
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
        let database =
            DatabaseEntry::new(DatabaseId::new(7), "Analytics", [events, users]).expect("database");
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
    fn applies_mysql_scalar_coercion_and_rejects_unsupported_syntax_explicitly() {
        let query = bind("SELECT Name + active FROM Events WHERE Name").expect("MySQL coercion");
        assert_eq!(query.projection[0].expr.data_type, Some(DataType::Float64));
        assert!(matches!(
            bind("SELECT * FROM Events ORDER BY id"),
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
