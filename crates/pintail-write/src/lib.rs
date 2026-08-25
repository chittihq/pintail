//! Binding for the statements a LOCAL (Pintail-owned, writable) database
//! accepts: `CREATE TABLE` and `INSERT`.
//!
//! Replicated databases stay read-only; nothing here is reachable for them
//! (`docs/design/writable-mode.md`, issue #7). This crate owns the
//! *translation and validation* half of the write path — turning a parsed
//! statement into a table definition or a batch of storage rows, with every
//! MySQL rejection this surface can produce. Durability and catalog
//! publication live above it, so the wire, the HTTP API, and tests share
//! one definition of what a write MEANS.
//!
//! The types a locally declared column lands on come from
//! [`pintail_probe::declared_column`], the same mapping a probe of a real
//! MySQL column uses. A local table is therefore typed exactly as a
//! mirrored one, and the differential gates cover both.

mod engine;

pub use engine::{LocalDatabase, WriteOutcome};

use pintail_probe::{DeclaredColumn, SourceColumn, SourceKey, SourceTable, declared_column};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};
use sqlparser::ast::{
    ColumnOption, CreateTable, DataType as SqlDataType, Expr, Insert, ObjectName, Statement,
    TableConstraint, Value as SqlValue, ValueWithSpan,
};
use thiserror::Error;

/// Why a write statement was refused.
///
/// Each variant carries the `MySQL` error number a client expects, so the
/// wire layer reports a duplicate key as 1062 rather than a generic
/// failure.
#[derive(Debug, Error)]
pub enum WriteError {
    /// The statement is not one a local database accepts.
    #[error("{0}")]
    Unsupported(String),
    /// The statement is malformed or contradicts the table definition.
    #[error("{0}")]
    Invalid(String),
    /// A table of this name already exists (`MySQL` 1050).
    #[error("Table '{0}' already exists")]
    TableExists(String),
    /// No such table (`MySQL` 1146).
    #[error("Table '{0}' doesn't exist")]
    UnknownTable(String),
    /// No such column (`MySQL` 1054).
    #[error("Unknown column '{0}' in 'field list'")]
    UnknownColumn(String),
    /// A row repeats a primary key (`MySQL` 1062).
    #[error("Duplicate entry '{0}' for key 'PRIMARY'")]
    DuplicateKey(String),
    /// A `NOT NULL` column received no value (`MySQL` 1048).
    #[error("Column '{0}' cannot be null")]
    NotNull(String),
}

impl WriteError {
    /// The `MySQL` error number for this rejection.
    ///
    /// Clients branch on these: a duplicate key is retried or reported as a
    /// conflict, while 1064 means the statement itself was wrong.
    #[must_use]
    pub const fn mysql_code(&self) -> u16 {
        match self {
            Self::Unsupported(_) | Self::Invalid(_) => 1064,
            Self::TableExists(_) => 1050,
            Self::UnknownTable(_) => 1146,
            Self::UnknownColumn(_) => 1054,
            Self::DuplicateKey(_) => 1062,
            Self::NotNull(_) => 1048,
        }
    }

    /// The `SQLSTATE` for this rejection.
    #[must_use]
    pub const fn sqlstate(&self) -> &'static str {
        match self {
            Self::Unsupported(_) | Self::Invalid(_) => "42000",
            Self::TableExists(_) => "42S01",
            Self::UnknownTable(_) => "42S02",
            Self::UnknownColumn(_) => "42S22",
            Self::DuplicateKey(_) => "23000",
            Self::NotNull(_) => "23000",
        }
    }
}

/// A bound `CREATE TABLE`: the table a local database should publish.
#[derive(Clone, Debug)]
pub struct CreateTablePlan {
    /// The table as a probe would have described it, so the catalog, the
    /// schema, and the read path are built by the existing code.
    pub table: SourceTable,
    /// Whether `IF NOT EXISTS` was given, making an existing table a no-op
    /// rather than error 1050.
    pub if_not_exists: bool,
}

/// A bound `INSERT`: rows ready for a transactional commit.
#[derive(Clone, Debug)]
pub struct InsertPlan {
    /// Target table name, as written.
    pub table: String,
    /// Rows in statement order, already typed and keyed.
    pub rows: Vec<StoredRow>,
}

/// Binds `CREATE TABLE` into the table a local database should publish.
///
/// # Errors
///
/// Returns [`WriteError::Unsupported`] for table features outside the MVP
/// (`docs/design/writable-mode.md` puts UNIQUE beyond the primary key,
/// foreign keys, CHECK and secondary indexes out of scope) and
/// [`WriteError::Invalid`] for a definition that cannot describe a table —
/// no primary key, a duplicate column, or a key naming a column that does
/// not exist.
pub fn bind_create_table(statement: &Statement) -> Result<CreateTablePlan, WriteError> {
    let Statement::CreateTable(create) = statement else {
        return Err(WriteError::Unsupported(
            "only CREATE TABLE is supported here".to_owned(),
        ));
    };
    reject_unsupported_table_features(create)?;

    let name = object_name(&create.name)?;
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut primary_key = Vec::new();
    for (index, column) in create.columns.iter().enumerate() {
        let ordinal = u32::try_from(index + 1)
            .map_err(|_| WriteError::Invalid("too many columns".to_owned()))?;
        let column_name = column.name.value.clone();
        if columns
            .iter()
            .any(|existing: &SourceColumn| existing.name.eq_ignore_ascii_case(&column_name))
        {
            return Err(WriteError::Invalid(format!(
                "Duplicate column name '{column_name}'"
            )));
        }
        let mut nullable = true;
        for option in &column.options {
            match &option.option {
                ColumnOption::NotNull => nullable = false,
                ColumnOption::Null => {}
                ColumnOption::PrimaryKey(_) => {
                    primary_key.push(column_name.clone());
                    nullable = false;
                }
                other => {
                    return Err(WriteError::Unsupported(format!(
                        "column option {other} is not supported on a local table"
                    )));
                }
            }
        }
        columns.push(declare(ordinal, &column_name, &column.data_type, nullable)?);
    }

    for constraint in &create.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => {
                if !primary_key.is_empty() {
                    return Err(WriteError::Invalid(
                        "Multiple primary key defined".to_owned(),
                    ));
                }
                for column in &key.columns {
                    primary_key.push(index_column_name(column)?);
                }
            }
            other => {
                return Err(WriteError::Unsupported(format!(
                    "table constraint {other} is not supported on a local table"
                )));
            }
        }
    }

    if primary_key.is_empty() {
        // A keyless local table has no merge identity, so UPDATE and DELETE
        // could never address a row and INSERT could not detect a duplicate.
        // Refusing here is better than accepting a table that can only ever
        // be appended to.
        return Err(WriteError::Invalid(format!(
            "table '{name}' needs a PRIMARY KEY; a local table without one has no row identity"
        )));
    }
    for key_column in &primary_key {
        let Some(column) = columns
            .iter_mut()
            .find(|column| column.name.eq_ignore_ascii_case(key_column))
        else {
            return Err(WriteError::Invalid(format!(
                "Key column '{key_column}' doesn't exist in table"
            )));
        };
        // MySQL makes every primary-key column NOT NULL whether or not the
        // declaration said so.
        column.nullable = false;
    }

    Ok(CreateTablePlan {
        table: SourceTable {
            name,
            engine: Some("Pintail".to_owned()),
            estimated_rows: Some(0),
            rows_are_exact: true,
            columns,
            key: SourceKey {
                mode: KeyMode::Primary,
                index_name: Some("PRIMARY".to_owned()),
                columns: primary_key,
            },
            unique_keys: Vec::new(),
            requires_reconciliation: false,
            foreign_keys: Vec::new(),
            secondary_indexes: Vec::new(),
            warnings: Vec::new(),
        },
        if_not_exists: create.if_not_exists,
    })
}

/// Binds `INSERT` against a table's definition, producing storage rows.
///
/// Rows are stamped with version 0; the store assigns the real commit
/// version, so nothing here can invent one.
///
/// # Errors
///
/// Returns an error for an unsupported INSERT shape, an unknown column, a
/// row whose value count disagrees with the column list, a `NOT NULL`
/// violation, a value that cannot be typed, or a primary key repeated
/// inside the same statement.
pub fn bind_insert(statement: &Statement, table: &SourceTable) -> Result<InsertPlan, WriteError> {
    let Statement::Insert(insert) = statement else {
        return Err(WriteError::Unsupported(
            "only INSERT is supported here".to_owned(),
        ));
    };
    reject_unsupported_insert_features(insert)?;

    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return Err(WriteError::Unsupported(
            "INSERT into a table function is not supported".to_owned(),
        ));
    };
    let target = object_name(name)?;
    if !target.eq_ignore_ascii_case(&table.name) {
        return Err(WriteError::UnknownTable(target));
    }

    // An empty column list means every column in declaration order, which
    // is what MySQL does.
    let named: Vec<&SourceColumn> = if insert.columns.is_empty() {
        table.columns.iter().collect()
    } else {
        insert
            .columns
            .iter()
            .map(|column| {
                let name = object_name(column)?;
                table
                    .columns
                    .iter()
                    .find(|candidate| candidate.name.eq_ignore_ascii_case(&name))
                    .ok_or(WriteError::UnknownColumn(name))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| WriteError::Unsupported("INSERT needs VALUES".to_owned()))?;
    let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref() else {
        return Err(WriteError::Unsupported(
            "only INSERT ... VALUES is supported".to_owned(),
        ));
    };

    let key_columns = table.key.columns.clone();
    let mut rows = Vec::with_capacity(values.rows.len());
    let mut seen_keys = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let row = &row.content;
        if row.len() != named.len() {
            return Err(WriteError::Invalid(format!(
                "Column count doesn't match value count: {} columns, {} values",
                named.len(),
                row.len()
            )));
        }
        // Start every column at NULL, then place the named ones. A column
        // absent from the list keeps NULL, and the NOT NULL check below
        // catches the ones that may not.
        let mut values_by_id = vec![Value::Null; table.columns.len()];
        for (column, expr) in named.iter().zip(row) {
            let position = table
                .columns
                .iter()
                .position(|candidate| candidate.name.eq_ignore_ascii_case(&column.name))
                .ok_or_else(|| WriteError::UnknownColumn(column.name.clone()))?;
            values_by_id[position] = literal_value(expr, column)?;
        }
        for (column, value) in table.columns.iter().zip(&values_by_id) {
            if !column.nullable && matches!(value, Value::Null) {
                return Err(WriteError::NotNull(column.name.clone()));
            }
        }

        let key = primary_key(table, &key_columns, &values_by_id)?;
        let rendered = render_key(&key);
        if seen_keys.contains(&rendered) {
            return Err(WriteError::DuplicateKey(rendered));
        }
        seen_keys.push(rendered);
        rows.push(StoredRow::new(key, values_by_id, 0, false));
    }

    Ok(InsertPlan {
        table: table.name.clone(),
        rows,
    })
}

/// Renders a primary key the way `MySQL` prints it in error 1062:
/// components joined by `-`.
#[must_use]
pub fn render_key(key: &PrimaryKey) -> String {
    key.parts()
        .iter()
        .map(|part| match part {
            KeyPart::Int64(value) => value.to_string(),
            KeyPart::UInt64(value) => value.to_string(),
            KeyPart::Utf8(text) => text.clone(),
            KeyPart::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn primary_key(
    table: &SourceTable,
    key_columns: &[String],
    values: &[Value],
) -> Result<PrimaryKey, WriteError> {
    let mut parts = Vec::with_capacity(key_columns.len());
    for name in key_columns {
        let position = table
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| WriteError::UnknownColumn(name.clone()))?;
        parts.push(match &values[position] {
            Value::Int64(value) => KeyPart::Int64(*value),
            Value::UInt64(value) => KeyPart::UInt64(*value),
            Value::Boolean(value) => KeyPart::Int64(i64::from(*value)),
            Value::Utf8(text) => KeyPart::Utf8(text.clone()),
            Value::Binary(bytes) => KeyPart::Binary(bytes.clone()),
            other => {
                return Err(WriteError::Invalid(format!(
                    "column '{name}' cannot be part of a primary key as {other:?}"
                )));
            }
        });
    }
    PrimaryKey::new(parts).map_err(|error| WriteError::Invalid(error.to_string()))
}

/// Types one literal against its column.
fn literal_value(expr: &Expr, column: &SourceColumn) -> Result<Value, WriteError> {
    let Expr::Value(ValueWithSpan { value, .. }) = expr else {
        // A local INSERT takes literals only: an expression would need the
        // full evaluator, and every function it could call is already
        // reachable from a SELECT over the inserted rows.
        return Err(WriteError::Unsupported(format!(
            "only literal values are supported in INSERT, got `{expr}`"
        )));
    };
    let text = match value {
        SqlValue::Null => return Ok(Value::Null),
        SqlValue::Boolean(flag) => return Ok(Value::Boolean(*flag)),
        SqlValue::Number(number, _) => number.clone(),
        SqlValue::SingleQuotedString(text) | SqlValue::DoubleQuotedString(text) => text.clone(),
        other => {
            return Err(WriteError::Unsupported(format!(
                "value `{other}` is not supported in INSERT"
            )));
        }
    };
    typed_value(&text, column)
}

/// Converts one literal's text into the physical value the column stores.
///
/// `DataType::storage_type` decides the variant: a `TINYINT` column stores
/// an `Int64` and a `DECIMAL` stores its exact text, exactly as a
/// replicated row of the same column does, so a locally written row and a
/// mirrored one are indistinguishable to every reader below this point.
fn typed_value(text: &str, column: &SourceColumn) -> Result<Value, WriteError> {
    let wrong = |reason: &str| {
        WriteError::Invalid(format!(
            "Incorrect value '{text}' for column '{}': {reason}",
            column.name
        ))
    };
    let value = match column.pintail_type.storage_type() {
        DataType::Boolean => match text {
            "0" => Value::Boolean(false),
            "1" => Value::Boolean(true),
            _ => return Err(wrong("expected 0 or 1")),
        },
        DataType::Int64 => {
            let number: i64 = text.parse().map_err(|_| wrong("expected an integer"))?;
            check_signed_width(column.pintail_type, number).map_err(wrong)?;
            Value::Int64(number)
        }
        DataType::UInt64 => {
            let number: u64 = text
                .parse()
                .map_err(|_| wrong("expected a non-negative integer"))?;
            check_unsigned_width(column.pintail_type, number).map_err(wrong)?;
            Value::UInt64(number)
        }
        DataType::Float64 => Value::float64(text.parse().map_err(|_| wrong("expected a number"))?),
        DataType::Utf8 => Value::Utf8(text.to_owned()),
        DataType::Binary => Value::Binary(text.as_bytes().to_vec()),
        other => {
            return Err(WriteError::Unsupported(format!(
                "column '{}' stores {other:?}, which a local INSERT cannot write yet",
                column.name
            )));
        }
    };
    Ok(value)
}

/// A narrow integer column must refuse a value it cannot hold. The schema
/// only checks the PHYSICAL variant, so without this a TINYINT column would
/// silently accept 99999 - a row no mirrored table could ever contain.
fn check_signed_width(data_type: DataType, value: i64) -> Result<(), &'static str> {
    let fits = match data_type {
        DataType::Int8 => i8::try_from(value).is_ok(),
        DataType::Int16 => i16::try_from(value).is_ok(),
        DataType::Int32 => i32::try_from(value).is_ok(),
        _ => true,
    };
    if fits {
        Ok(())
    } else {
        Err("Out of range value")
    }
}

fn check_unsigned_width(data_type: DataType, value: u64) -> Result<(), &'static str> {
    let fits = match data_type {
        DataType::UInt8 => u8::try_from(value).is_ok(),
        DataType::UInt16 => u16::try_from(value).is_ok(),
        DataType::UInt32 => u32::try_from(value).is_ok(),
        DataType::Year => value == 0 || (1901..=2155).contains(&value),
        _ => true,
    };
    if fits {
        Ok(())
    } else {
        Err("Out of range value")
    }
}

fn declare(
    ordinal: u32,
    name: &str,
    data_type: &SqlDataType,
    nullable: bool,
) -> Result<SourceColumn, WriteError> {
    let (bare, full, precision, scale) = mysql_terms(data_type)?;
    declared_column(&DeclaredColumn {
        ordinal,
        name,
        data_type: &bare,
        column_type: &full,
        numeric_precision: precision,
        numeric_scale: scale,
        datetime_precision: None,
        nullable,
        collation: text_collation(&bare),
    })
    .map_err(|error| WriteError::Invalid(format!("column '{name}': {error}")))
}

/// The declared collation for a textual column. Local tables are created in
/// the engine's own default so their comparison rules match a mirrored
/// `utf8mb4_0900_ai_ci` table.
fn text_collation(bare: &str) -> Option<&'static str> {
    matches!(
        bare,
        "char" | "varchar" | "text" | "tinytext" | "mediumtext" | "longtext" | "enum" | "set"
    )
    .then_some("utf8mb4_0900_ai_ci")
}

/// Renders a parsed SQL type back into the two terms
/// `INFORMATION_SCHEMA.COLUMNS` reports, so the probe's mapping can be
/// reused verbatim.
fn mysql_terms(
    data_type: &SqlDataType,
) -> Result<(String, String, Option<u8>, Option<u8>), WriteError> {
    let full = data_type.to_string().to_ascii_lowercase();
    let bare = full
        .split(['(', ' '])
        .next()
        .unwrap_or(&full)
        .trim()
        .to_owned();
    let (precision, scale) = match data_type {
        SqlDataType::Decimal(info) | SqlDataType::Numeric(info) => match info {
            sqlparser::ast::ExactNumberInfo::PrecisionAndScale(precision, scale) => (
                Some(u8::try_from(*precision).map_err(|_| {
                    WriteError::Invalid("DECIMAL precision is out of range".to_owned())
                })?),
                Some(u8::try_from(*scale).map_err(|_| {
                    WriteError::Invalid("DECIMAL scale is out of range".to_owned())
                })?),
            ),
            sqlparser::ast::ExactNumberInfo::Precision(precision) => (
                Some(u8::try_from(*precision).map_err(|_| {
                    WriteError::Invalid("DECIMAL precision is out of range".to_owned())
                })?),
                Some(0),
            ),
            sqlparser::ast::ExactNumberInfo::None => {
                // MySQL's own default for a bare DECIMAL.
                (Some(10), Some(0))
            }
        },
        _ => (None, None),
    };
    Ok((bare, full, precision, scale))
}

/// The bare column name inside a `PRIMARY KEY (...)` member. An indexed
/// EXPRESSION has no column to key on, so it is refused rather than
/// rendered into a name that matches nothing.
fn index_column_name(column: &sqlparser::ast::IndexColumn) -> Result<String, WriteError> {
    match &column.column.expr {
        Expr::Identifier(ident) => Ok(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|ident| ident.value.clone())
            .ok_or_else(|| WriteError::Invalid("a key column needs a name".to_owned())),
        other => Err(WriteError::Unsupported(format!(
            "PRIMARY KEY over the expression `{other}` is not supported"
        ))),
    }
}

fn object_name(name: &ObjectName) -> Result<String, WriteError> {
    let rendered = name.to_string();
    let bare = rendered.rsplit('.').next().unwrap_or(&rendered);
    let cleaned = bare.trim_matches('`').to_owned();
    if cleaned.is_empty() {
        return Err(WriteError::Invalid("a table needs a name".to_owned()));
    }
    Ok(cleaned)
}

fn reject_unsupported_table_features(create: &CreateTable) -> Result<(), WriteError> {
    let unsupported = [
        (create.or_replace, "OR REPLACE"),
        (create.temporary, "TEMPORARY"),
        (create.external, "EXTERNAL"),
        (create.global.is_some(), "GLOBAL/LOCAL"),
        (create.transient, "TRANSIENT"),
        (create.query.is_some(), "CREATE TABLE ... AS SELECT"),
        (create.like.is_some(), "CREATE TABLE ... LIKE"),
        (create.clone.is_some(), "CREATE TABLE ... CLONE"),
        (create.partition_by.is_some(), "PARTITION BY"),
    ];
    for (present, feature) in unsupported {
        if present {
            return Err(WriteError::Unsupported(format!(
                "{feature} is not supported on a local table"
            )));
        }
    }
    if create.columns.is_empty() {
        return Err(WriteError::Invalid(
            "A table must have at least one column".to_owned(),
        ));
    }
    Ok(())
}

fn reject_unsupported_insert_features(insert: &Insert) -> Result<(), WriteError> {
    let unsupported = [
        (insert.or.is_some(), "INSERT OR"),
        (insert.ignore, "INSERT IGNORE"),
        (insert.replace_into, "REPLACE INTO"),
        (insert.on.is_some(), "ON DUPLICATE KEY UPDATE"),
        (insert.returning.is_some(), "RETURNING"),
        (insert.partitioned.is_some(), "PARTITION"),
    ];
    for (present, feature) in unsupported {
        if present {
            return Err(WriteError::Unsupported(format!(
                "{feature} is not supported on a local table"
            )));
        }
    }
    Ok(())
}
