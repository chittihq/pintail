use std::{cmp::Ordering, fmt};

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, Value};
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, LimitClause, ObjectName, OrderByKind, Query, Select,
    SelectFlavor, SelectItem, SetExpr, ShowStatementOptions, Statement, TableFactor,
    Value as SqlValue, WildcardAdditionalOptions,
};

/// One metadata result-column description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataField {
    /// MySQL-compatible field name.
    pub name: String,
    /// Result scalar type.
    pub data_type: DataType,
    /// Whether rows can contain `NULL`.
    pub nullable: bool,
}

/// Fully materialized deterministic metadata response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataResult {
    /// Ordered result fields.
    pub fields: Vec<MetadataField>,
    /// Ordered result rows.
    pub rows: Vec<Vec<Value>>,
}

/// Executes one supported `MySQL` metadata statement against an immutable catalog.
///
/// # Errors
///
/// Returns an explicit unsupported-shape or unknown-object error.
pub fn execute_metadata(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<MetadataResult, MetadataError> {
    match statement {
        Statement::ShowDatabases {
            terse: false,
            history: false,
            show_options,
        } if empty_options(show_options) => Ok(single_string_result(
            "Database",
            catalog.databases().map(DatabaseEntry::name),
        )),
        Statement::ShowTables {
            terse: false,
            history: false,
            extended: false,
            full: false,
            external: false,
            show_options,
        } if simple_options(show_options) => {
            let database = resolve_show_database(show_options, catalog, current_database)?;
            Ok(single_string_result(
                &format!("Tables_in_{}", database.name()),
                database.tables().map(TableEntry::name),
            ))
        }
        Statement::ShowColumns {
            extended: false,
            full: false,
            show_options,
        } if simple_options(show_options) => {
            let name = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .ok_or_else(|| MetadataError::Unsupported(statement.to_string()))?;
            let (_, table) = resolve_table(name, catalog, current_database)?;
            Ok(describe_table(table))
        }
        Statement::ExplainTable {
            hive_format: None,
            table_name,
            ..
        } => {
            let (_, table) = resolve_table(table_name, catalog, current_database)?;
            Ok(describe_table(table))
        }
        Statement::Query(query) => execute_information_schema(query, catalog),
        _ => Err(MetadataError::Unsupported(statement.to_string())),
    }
}

fn execute_information_schema(
    query: &Query,
    catalog: &CatalogSnapshot,
) -> Result<MetadataResult, MetadataError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(MetadataError::Unsupported(query.to_string()));
    };
    if query_has_unsupported_clauses(query)
        || select_has_unsupported_clauses(select)
        || select.from.len() != 1
        || !select.from[0].joins.is_empty()
    {
        return Err(MetadataError::Unsupported(query.to_string()));
    }
    let TableFactor::Table {
        name,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
        ..
    } = &select.from[0].relation
    else {
        return Err(MetadataError::Unsupported(query.to_string()));
    };
    if !with_hints.is_empty() || !partitions.is_empty() || !index_hints.is_empty() {
        return Err(MetadataError::Unsupported(query.to_string()));
    }

    let mut result = information_schema_table(name, catalog)?;
    if let Some(predicate) = &select.selection {
        let mut filtered = Vec::with_capacity(result.rows.len());
        for row in result.rows {
            if evaluate_metadata_predicate(predicate, &result.fields, &row)? {
                filtered.push(row);
            }
        }
        result.rows = filtered;
    }
    result = project_metadata_result(result, &select.projection)?;
    order_metadata_result(&mut result, query)?;
    limit_metadata_result(&mut result, query)?;
    Ok(result)
}

fn query_has_unsupported_clauses(query: &Query) -> bool {
    query.with.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
}

fn select_has_unsupported_clauses(select: &Select) -> bool {
    let empty_grouping =
        matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty());
    !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select
            .select_modifiers
            .as_ref()
            .is_some_and(sqlparser::ast::SelectModifiers::is_any_set)
        || select.top.is_some()
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !empty_grouping
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.flavor != SelectFlavor::Standard
}

fn information_schema_table(
    name: &ObjectName,
    catalog: &CatalogSnapshot,
) -> Result<MetadataResult, MetadataError> {
    let parts = object_name_parts(name)?;
    let [database, table] = parts.as_slice() else {
        return Err(MetadataError::Unsupported(name.to_string()));
    };
    if !database.eq_ignore_ascii_case("information_schema") {
        return Err(MetadataError::Unsupported(name.to_string()));
    }
    if table.eq_ignore_ascii_case("schemata") {
        Ok(information_schemata(catalog))
    } else if table.eq_ignore_ascii_case("tables") {
        Ok(information_tables(catalog))
    } else if table.eq_ignore_ascii_case("columns") {
        Ok(information_columns(catalog))
    } else {
        Err(MetadataError::UnknownTable((*table).to_owned()))
    }
}

fn information_schemata(catalog: &CatalogSnapshot) -> MetadataResult {
    let fields = metadata_fields(&[
        ("CATALOG_NAME", DataType::Utf8, false),
        ("SCHEMA_NAME", DataType::Utf8, false),
        ("DEFAULT_CHARACTER_SET_NAME", DataType::Utf8, false),
        ("DEFAULT_COLLATION_NAME", DataType::Utf8, false),
        ("SQL_PATH", DataType::Utf8, true),
    ]);
    let rows = catalog
        .databases()
        .map(|database| {
            vec![
                utf8("def"),
                utf8(database.name()),
                utf8("utf8mb4"),
                utf8("utf8mb4_general_ci"),
                Value::Null,
            ]
        })
        .collect();
    MetadataResult { fields, rows }
}

fn information_tables(catalog: &CatalogSnapshot) -> MetadataResult {
    let fields = metadata_fields(&[
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("TABLE_TYPE", DataType::Utf8, false),
        ("ENGINE", DataType::Utf8, false),
        ("TABLE_ROWS", DataType::UInt64, true),
    ]);
    let rows = catalog
        .databases()
        .flat_map(|database| {
            database.tables().map(move |table| {
                vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8("BASE TABLE"),
                    utf8("PINTAIL"),
                    table
                        .statistics()
                        .row_count()
                        .map_or(Value::Null, Value::UInt64),
                ]
            })
        })
        .collect();
    MetadataResult { fields, rows }
}

fn information_columns(catalog: &CatalogSnapshot) -> MetadataResult {
    let fields = metadata_fields(&[
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("COLUMN_NAME", DataType::Utf8, false),
        ("ORDINAL_POSITION", DataType::UInt64, false),
        ("COLUMN_DEFAULT", DataType::Utf8, true),
        ("IS_NULLABLE", DataType::Utf8, false),
        ("DATA_TYPE", DataType::Utf8, false),
        ("COLUMN_TYPE", DataType::Utf8, false),
        ("COLUMN_KEY", DataType::Utf8, false),
        ("EXTRA", DataType::Utf8, false),
    ]);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for table in database.tables() {
            for (index, column) in table.schema().columns().iter().enumerate() {
                let column_type = mysql_type(column.data_type());
                rows.push(vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8(column.name()),
                    Value::UInt64(u64::try_from(index + 1).expect("column ordinal fits u64")),
                    Value::Null,
                    utf8(if column.is_nullable() { "YES" } else { "NO" }),
                    utf8(mysql_data_type(column.data_type())),
                    utf8(&column_type),
                    utf8(""),
                    utf8(""),
                ]);
            }
        }
    }
    MetadataResult { fields, rows }
}

fn metadata_fields(definitions: &[(&str, DataType, bool)]) -> Vec<MetadataField> {
    definitions
        .iter()
        .map(|(name, data_type, nullable)| MetadataField {
            name: (*name).to_owned(),
            data_type: *data_type,
            nullable: *nullable,
        })
        .collect()
}

fn utf8(value: &str) -> Value {
    Value::Utf8(value.to_owned())
}

fn project_metadata_result(
    source: MetadataResult,
    projection: &[SelectItem],
) -> Result<MetadataResult, MetadataError> {
    if projection.len() == 1 && projection[0].to_string().eq_ignore_ascii_case("COUNT(*)") {
        return Ok(MetadataResult {
            fields: metadata_fields(&[("COUNT(*)", DataType::UInt64, false)]),
            rows: vec![vec![Value::UInt64(
                u64::try_from(source.rows.len()).expect("metadata row count fits u64"),
            )]],
        });
    }

    let mut columns = Vec::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(options) | SelectItem::QualifiedWildcard(_, options)
                if wildcard_is_plain(options) =>
            {
                columns.extend(
                    source
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(index, field)| (index, field.clone())),
                );
            }
            SelectItem::UnnamedExpr(expr) => {
                let index = metadata_expr_column(expr, &source.fields)?;
                columns.push((index, source.fields[index].clone()));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let index = metadata_expr_column(expr, &source.fields)?;
                let mut field = source.fields[index].clone();
                field.name.clone_from(&alias.value);
                columns.push((index, field));
            }
            _ => return Err(MetadataError::Unsupported(item.to_string())),
        }
    }
    let fields = columns.iter().map(|(_, field)| field.clone()).collect();
    let rows = source
        .rows
        .into_iter()
        .map(|row| {
            columns
                .iter()
                .map(|(index, _)| row[*index].clone())
                .collect()
        })
        .collect();
    Ok(MetadataResult { fields, rows })
}

fn wildcard_is_plain(options: &WildcardAdditionalOptions) -> bool {
    options == &WildcardAdditionalOptions::default()
}

fn metadata_expr_column(
    expression: &Expr,
    fields: &[MetadataField],
) -> Result<usize, MetadataError> {
    let name = match expression {
        Expr::Identifier(identifier) => identifier.value.as_str(),
        Expr::CompoundIdentifier(identifiers) => identifiers
            .last()
            .map(|identifier| identifier.value.as_str())
            .ok_or_else(|| MetadataError::Unsupported(expression.to_string()))?,
        _ => return Err(MetadataError::Unsupported(expression.to_string())),
    };
    fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| MetadataError::UnknownColumn(name.to_owned()))
}

fn evaluate_metadata_predicate(
    expression: &Expr,
    fields: &[MetadataField],
    row: &[Value],
) -> Result<bool, MetadataError> {
    match expression {
        Expr::Nested(expression) => evaluate_metadata_predicate(expression, fields, row),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(evaluate_metadata_predicate(left, fields, row)?
            && evaluate_metadata_predicate(right, fields, row)?),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => Ok(evaluate_metadata_predicate(left, fields, row)?
            || evaluate_metadata_predicate(right, fields, row)?),
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) =>
        {
            let equal = metadata_equal(
                &metadata_expr_value(left, fields, row)?,
                &metadata_expr_value(right, fields, row)?,
            );
            Ok(equal.is_some_and(|equal| {
                if *op == BinaryOperator::Eq {
                    equal
                } else {
                    !equal
                }
            }))
        }
        Expr::IsNull(expression) => Ok(matches!(
            metadata_expr_value(expression, fields, row)?,
            Value::Null
        )),
        Expr::IsNotNull(expression) => Ok(!matches!(
            metadata_expr_value(expression, fields, row)?,
            Value::Null
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = metadata_expr_value(expr, fields, row)?;
            if matches!(needle, Value::Null) {
                return Ok(false);
            }
            let mut found = false;
            let mut contains_null = false;
            for candidate in list {
                match metadata_equal(&needle, &metadata_expr_value(candidate, fields, row)?) {
                    Some(true) => found = true,
                    Some(false) => {}
                    None => contains_null = true,
                }
            }
            Ok(if found {
                !*negated
            } else if contains_null {
                false
            } else {
                *negated
            })
        }
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char: None,
        } => {
            let value = metadata_expr_value(expr, fields, row)?;
            let pattern = metadata_expr_value(pattern, fields, row)?;
            if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                return Ok(false);
            }
            let matched = match (&value, &pattern) {
                (Value::Utf8(value), Value::Utf8(pattern)) => metadata_like(value, pattern),
                _ => false,
            };
            Ok(if *negated { !matched } else { matched })
        }
        _ => Err(MetadataError::Unsupported(expression.to_string())),
    }
}

fn metadata_expr_value(
    expression: &Expr,
    fields: &[MetadataField],
    row: &[Value],
) -> Result<Value, MetadataError> {
    match expression {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            Ok(row[metadata_expr_column(expression, fields)?].clone())
        }
        Expr::Nested(expression) => metadata_expr_value(expression, fields, row),
        Expr::Value(value) => match &value.value {
            SqlValue::SingleQuotedString(value)
            | SqlValue::DoubleQuotedString(value)
            | SqlValue::NationalStringLiteral(value) => Ok(utf8(value)),
            SqlValue::Number(value, _) => value
                .parse::<u64>()
                .map(Value::UInt64)
                .map_err(|_| MetadataError::Unsupported(expression.to_string())),
            SqlValue::Boolean(value) => Ok(Value::Boolean(*value)),
            SqlValue::Null => Ok(Value::Null),
            _ => Err(MetadataError::Unsupported(expression.to_string())),
        },
        _ => Err(MetadataError::Unsupported(expression.to_string())),
    }
}

fn metadata_equal(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Utf8(left), Value::Utf8(right)) => Some(left.eq_ignore_ascii_case(right)),
        _ => Some(left == right),
    }
}

fn metadata_like(value: &str, pattern: &str) -> bool {
    let value = value.to_lowercase().into_bytes();
    let pattern = pattern.to_lowercase().into_bytes();
    let mut matches = vec![false; value.len() + 1];
    matches[0] = true;
    for token in pattern {
        if token == b'%' {
            for index in 1..=value.len() {
                matches[index] |= matches[index - 1];
            }
        } else {
            for index in (1..=value.len()).rev() {
                matches[index] = matches[index - 1] && (token == b'_' || token == value[index - 1]);
            }
            matches[0] = false;
        }
    }
    matches[value.len()]
}

fn order_metadata_result(result: &mut MetadataResult, query: &Query) -> Result<(), MetadataError> {
    let Some(order_by) = &query.order_by else {
        return Ok(());
    };
    let OrderByKind::Expressions(expressions) = &order_by.kind else {
        return Err(MetadataError::Unsupported(order_by.to_string()));
    };
    if order_by.interpolate.is_some()
        || expressions
            .iter()
            .any(|expression| expression.with_fill.is_some())
    {
        return Err(MetadataError::Unsupported(order_by.to_string()));
    }
    let keys = expressions
        .iter()
        .map(|expression| {
            let index = metadata_expr_column(&expression.expr, &result.fields)?;
            Ok((index, expression.options.asc.unwrap_or(true)))
        })
        .collect::<Result<Vec<_>, MetadataError>>()?;
    result.rows.sort_by(|left, right| {
        for (index, ascending) in &keys {
            let ordering = metadata_order(&left[*index], &right[*index]);
            if ordering != Ordering::Equal {
                return if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                };
            }
        }
        Ordering::Equal
    });
    Ok(())
}

fn metadata_order(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Utf8(left), Value::Utf8(right)) => left.to_lowercase().cmp(&right.to_lowercase()),
        _ => left.cmp(right),
    }
}

fn limit_metadata_result(result: &mut MetadataResult, query: &Query) -> Result<(), MetadataError> {
    let Some(limit) = &query.limit_clause else {
        return Ok(());
    };
    let (offset, count) = match limit {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => (
            offset
                .as_ref()
                .map_or(Ok(0), |offset| metadata_usize(&offset.value))?,
            metadata_usize(limit)?,
        ),
        LimitClause::OffsetCommaLimit { offset, limit } => {
            (metadata_usize(offset)?, metadata_usize(limit)?)
        }
        LimitClause::LimitOffset { .. } => {
            return Err(MetadataError::Unsupported(limit.to_string()));
        }
    };
    result.rows = result.rows.drain(..).skip(offset).take(count).collect();
    Ok(())
}

fn metadata_usize(expression: &Expr) -> Result<usize, MetadataError> {
    let Expr::Value(value) = expression else {
        return Err(MetadataError::Unsupported(expression.to_string()));
    };
    let SqlValue::Number(value, _) = &value.value else {
        return Err(MetadataError::Unsupported(expression.to_string()));
    };
    value
        .parse()
        .map_err(|_| MetadataError::Unsupported(expression.to_string()))
}

fn single_string_result<'a>(
    field: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> MetadataResult {
    MetadataResult {
        fields: vec![MetadataField {
            name: field.to_owned(),
            data_type: DataType::Utf8,
            nullable: false,
        }],
        rows: values
            .into_iter()
            .map(|value| vec![Value::Utf8(value.to_owned())])
            .collect(),
    }
}

fn describe_table(table: &TableEntry) -> MetadataResult {
    let fields = ["Field", "Type", "Null", "Key", "Default", "Extra"]
        .into_iter()
        .map(|name| MetadataField {
            name: name.to_owned(),
            data_type: DataType::Utf8,
            nullable: false,
        })
        .collect();
    let rows = table
        .schema()
        .columns()
        .iter()
        .map(|column| {
            vec![
                Value::Utf8(column.name().to_owned()),
                Value::Utf8(mysql_type(column.data_type())),
                Value::Utf8(if column.is_nullable() { "YES" } else { "NO" }.to_owned()),
                Value::Utf8(String::new()),
                Value::Utf8(if column.is_nullable() { "NULL" } else { "" }.to_owned()),
                Value::Utf8(String::new()),
            ]
        })
        .collect();
    MetadataResult { fields, rows }
}

fn mysql_type(data_type: DataType) -> String {
    match data_type {
        DataType::Boolean => "tinyint(1)".to_owned(),
        DataType::Int8 => "tinyint".to_owned(),
        DataType::Int16 => "smallint".to_owned(),
        DataType::Int32 => "int".to_owned(),
        DataType::Int64 => "bigint".to_owned(),
        DataType::UInt8 => "tinyint unsigned".to_owned(),
        DataType::UInt16 => "smallint unsigned".to_owned(),
        DataType::UInt32 => "int unsigned".to_owned(),
        DataType::UInt64 => "bigint unsigned".to_owned(),
        DataType::Float32 => "float".to_owned(),
        DataType::Float64 => "double".to_owned(),
        DataType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        DataType::Date32 => "date".to_owned(),
        DataType::DateTime64 { fsp } => format!("datetime({fsp})"),
        DataType::Time64 { fsp } => format!("time({fsp})"),
        DataType::Utf8 => "text".to_owned(),
        DataType::Binary => "blob".to_owned(),
        DataType::Json => "json".to_owned(),
    }
}

const fn mysql_data_type(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Boolean | DataType::Int8 | DataType::UInt8 => "tinyint",
        DataType::Int16 | DataType::UInt16 => "smallint",
        DataType::Int32 | DataType::UInt32 => "int",
        DataType::Int64 | DataType::UInt64 => "bigint",
        DataType::Float32 => "float",
        DataType::Float64 => "double",
        DataType::Decimal { .. } => "decimal",
        DataType::Date32 => "date",
        DataType::DateTime64 { .. } => "datetime",
        DataType::Time64 { .. } => "time",
        DataType::Utf8 => "text",
        DataType::Binary => "blob",
        DataType::Json => "json",
    }
}

fn resolve_show_database<'a>(
    options: &ShowStatementOptions,
    catalog: &'a CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<&'a DatabaseEntry, MetadataError> {
    let name = options
        .show_in
        .as_ref()
        .and_then(|show_in| show_in.parent_name.as_ref())
        .map(object_name_parts)
        .transpose()?
        .and_then(|parts| (parts.len() == 1).then(|| parts[0]))
        .or(current_database)
        .ok_or(MetadataError::NoCurrentDatabase)?;
    catalog
        .database(name)
        .ok_or_else(|| MetadataError::UnknownDatabase(name.to_owned()))
}

fn resolve_table<'a>(
    name: &ObjectName,
    catalog: &'a CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<(&'a DatabaseEntry, &'a TableEntry), MetadataError> {
    let parts = object_name_parts(name)?;
    let (database, table) = match parts.as_slice() {
        [table] => (
            current_database.ok_or(MetadataError::NoCurrentDatabase)?,
            *table,
        ),
        [database, table] => (*database, *table),
        _ => return Err(MetadataError::InvalidObjectName(name.to_string())),
    };
    let database = catalog
        .database(database)
        .ok_or_else(|| MetadataError::UnknownDatabase(database.to_owned()))?;
    let table = database
        .table(table)
        .ok_or_else(|| MetadataError::UnknownTable(table.to_owned()))?;
    Ok((database, table))
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<&str>, MetadataError> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|identifier| identifier.value.as_str())
                .ok_or_else(|| MetadataError::InvalidObjectName(name.to_string()))
        })
        .collect()
}

fn empty_options(options: &ShowStatementOptions) -> bool {
    options.show_in.is_none() && simple_options(options)
}

fn simple_options(options: &ShowStatementOptions) -> bool {
    options.starts_with.is_none()
        && options.limit.is_none()
        && options.limit_from.is_none()
        && options.filter_position.is_none()
}

/// Metadata statement failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataError {
    /// Statement or extension is outside the supported compatibility surface.
    Unsupported(String),
    /// A table name requires a current database.
    NoCurrentDatabase,
    /// No catalog database has this name.
    UnknownDatabase(String),
    /// No catalog table has this name.
    UnknownTable(String),
    /// No metadata field has this name.
    UnknownColumn(String),
    /// An object name has an unsupported shape.
    InvalidObjectName(String),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(statement) => {
                write!(formatter, "unsupported metadata statement: {statement}")
            }
            Self::NoCurrentDatabase => formatter.write_str("no current database selected"),
            Self::UnknownDatabase(database) => write!(formatter, "unknown database {database}"),
            Self::UnknownTable(table) => write!(formatter, "unknown table {table}"),
            Self::UnknownColumn(column) => write!(formatter, "unknown column {column}"),
            Self::InvalidObjectName(name) => write!(formatter, "invalid object name {name}"),
        }
    }
}

impl std::error::Error for MetadataError {}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_types::{Column, DataType, TableSchema, Value};

    use crate::{execute_metadata, parse_statement};

    fn catalog() -> CatalogSnapshot {
        let table = TableEntry::new(
            TableId::new(2),
            "Events",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(3),
        )
        .expect("table");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        CatalogSnapshot::new([database]).expect("catalog")
    }

    #[test]
    fn serves_show_and_describe_from_one_catalog_snapshot() {
        let catalog = catalog();
        let databases = execute_metadata(
            &parse_statement("SHOW DATABASES").expect("parse"),
            &catalog,
            None,
        )
        .expect("databases");
        assert_eq!(databases.rows, [vec![Value::Utf8("Analytics".to_owned())]]);

        let tables = execute_metadata(
            &parse_statement("SHOW TABLES FROM Analytics").expect("parse"),
            &catalog,
            None,
        )
        .expect("tables");
        assert_eq!(tables.rows, [vec![Value::Utf8("Events".to_owned())]]);

        let columns = execute_metadata(
            &parse_statement("DESCRIBE Analytics.Events").expect("parse"),
            &catalog,
            None,
        )
        .expect("columns");
        assert_eq!(columns.fields[0].name, "Field");
        assert_eq!(columns.rows[0][0], Value::Utf8("id".to_owned()));
        assert_eq!(
            columns.rows[0][1],
            Value::Utf8("bigint unsigned".to_owned())
        );
        assert_eq!(columns.rows[1][2], Value::Utf8("YES".to_owned()));
    }

    #[test]
    fn serves_basic_information_schema_queries() {
        let catalog = catalog();
        let schemata = execute_metadata(
            &parse_statement(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name = 'analytics'",
            )
            .expect("parse"),
            &catalog,
            None,
        )
        .expect("schemata");
        assert_eq!(schemata.rows, [vec![Value::Utf8("Analytics".to_owned())]]);

        let tables = execute_metadata(
            &parse_statement(
                "SELECT table_name, table_rows FROM information_schema.tables \
                 WHERE table_schema = 'Analytics' ORDER BY table_name LIMIT 1",
            )
            .expect("parse"),
            &catalog,
            None,
        )
        .expect("tables");
        assert_eq!(
            tables.rows,
            [vec![Value::Utf8("Events".to_owned()), Value::UInt64(3)]]
        );

        let columns = execute_metadata(
            &parse_statement(
                "SELECT column_name AS name, ordinal_position, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = 'analytics' AND table_name LIKE 'eve%' \
                 ORDER BY ordinal_position",
            )
            .expect("parse"),
            &catalog,
            None,
        )
        .expect("columns");
        assert_eq!(columns.fields[0].name, "name");
        assert_eq!(
            columns.rows,
            [
                vec![
                    Value::Utf8("id".to_owned()),
                    Value::UInt64(1),
                    Value::Utf8("NO".to_owned()),
                ],
                vec![
                    Value::Utf8("name".to_owned()),
                    Value::UInt64(2),
                    Value::Utf8("YES".to_owned()),
                ],
            ]
        );

        let count = execute_metadata(
            &parse_statement(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_schema = 'Analytics'",
            )
            .expect("parse"),
            &catalog,
            None,
        )
        .expect("count");
        assert_eq!(count.rows, [vec![Value::UInt64(2)]]);
    }

    #[test]
    fn information_schema_filters_null_with_mysql_three_valued_logic() {
        let catalog = catalog();
        for predicate in [
            "column_default = NULL",
            "column_default != 'value'",
            "column_default NOT IN ('value')",
            "column_name NOT IN ('missing', NULL)",
            "column_default NOT LIKE '%'",
        ] {
            let sql = format!("SELECT COUNT(*) FROM information_schema.columns WHERE {predicate}");
            let result = execute_metadata(&parse_statement(&sql).expect("parse"), &catalog, None)
                .expect("metadata query");
            assert_eq!(
                result.rows,
                [vec![Value::UInt64(0)]],
                "predicate: {predicate}"
            );
        }

        let result = execute_metadata(
            &parse_statement(
                "SELECT column_name FROM information_schema.columns \
                 WHERE column_name IN ('id', NULL)",
            )
            .expect("parse"),
            &catalog,
            None,
        )
        .expect("metadata query");
        assert_eq!(result.rows, [vec![Value::Utf8("id".to_owned())]]);
    }
}
