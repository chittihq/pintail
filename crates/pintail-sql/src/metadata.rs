use std::fmt;

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, Value};
use sqlparser::ast::{ObjectName, ShowStatementOptions, Statement};

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
        _ => Err(MetadataError::Unsupported(statement.to_string())),
    }
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
                Value::Utf8(mysql_type(column.data_type()).to_owned()),
                Value::Utf8(if column.is_nullable() { "YES" } else { "NO" }.to_owned()),
                Value::Utf8(String::new()),
                Value::Utf8(if column.is_nullable() { "NULL" } else { "" }.to_owned()),
                Value::Utf8(String::new()),
            ]
        })
        .collect();
    MetadataResult { fields, rows }
}

const fn mysql_type(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Boolean => "tinyint(1)",
        DataType::Int64 => "bigint",
        DataType::UInt64 => "bigint unsigned",
        DataType::Float64 => "double",
        DataType::Utf8 => "text",
        DataType::Binary => "blob",
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
}
