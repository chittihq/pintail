use std::{collections::HashSet, fmt};

use crate::{DataType, StoredRow};

/// Physical sort-key and duplicate-resolution mode selected by the catalog.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    /// Use the source table's primary key.
    #[default]
    Primary,
    /// Use the first source unique index when no primary key exists.
    Unique,
    /// Generate a monotonically increasing row ID and retain every row.
    AppendRowId,
}

/// A stable table column definition.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Column {
    id: u32,
    name: String,
    data_type: DataType,
    nullable: bool,
}

impl Column {
    /// Constructs a column definition.
    #[must_use]
    pub fn new(id: u32, name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            id,
            name: name.into(),
            data_type,
            nullable,
        }
    }

    /// Returns the stable column identifier.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Returns the source column name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the logical type.
    #[must_use]
    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns whether the column accepts `NULL`.
    #[must_use]
    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// A versioned ordered set of table columns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableSchema {
    version: u32,
    columns: Vec<Column>,
    key_mode: KeyMode,
}

impl TableSchema {
    /// Constructs and validates a table schema.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty schema, empty or duplicate names, or
    /// duplicate stable identifiers.
    pub fn new(version: u32, columns: Vec<Column>) -> Result<Self, SchemaError> {
        Self::with_key_mode(version, columns, KeyMode::Primary)
    }

    /// Constructs a schema with an explicit catalog-selected key fallback.
    ///
    /// # Errors
    ///
    /// Returns the same structural errors as [`Self::new`].
    pub fn with_key_mode(
        version: u32,
        columns: Vec<Column>,
        key_mode: KeyMode,
    ) -> Result<Self, SchemaError> {
        if columns.is_empty() {
            return Err(SchemaError::EmptySchema);
        }

        let mut ids = HashSet::with_capacity(columns.len());
        let mut names = HashSet::with_capacity(columns.len());
        for column in &columns {
            if !column.data_type.is_valid() {
                return Err(SchemaError::InvalidDataType {
                    column: column.name.clone(),
                    data_type: column.data_type,
                });
            }
            if column.name.is_empty() {
                return Err(SchemaError::EmptyColumnName);
            }
            if !ids.insert(column.id) {
                return Err(SchemaError::DuplicateColumnId(column.id));
            }
            if column.id >= u32::MAX - 2 {
                return Err(SchemaError::ReservedColumnId(column.id));
            }
            if !names.insert(column.name.clone()) {
                return Err(SchemaError::DuplicateColumnName(column.name.clone()));
            }
        }

        Ok(Self {
            version,
            columns,
            key_mode,
        })
    }

    /// Returns the schema version embedded in segments.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Returns columns in physical order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns how storage keys and duplicate resolution are selected.
    #[must_use]
    pub fn key_mode(&self) -> KeyMode {
        self.key_mode
    }

    /// Validates a row against column arity, nullability, and types.
    ///
    /// # Errors
    ///
    /// Returns the first schema mismatch.
    pub fn validate_row(&self, row: &StoredRow) -> Result<(), SchemaError> {
        if row.values().len() != self.columns.len() {
            return Err(SchemaError::WrongArity {
                expected: self.columns.len(),
                actual: row.values().len(),
            });
        }

        for (column, value) in self.columns.iter().zip(row.values()) {
            match value.data_type() {
                None if !column.nullable => {
                    return Err(SchemaError::NullInRequiredColumn(column.name.clone()));
                }
                Some(actual) if !column.data_type.accepts(actual) => {
                    return Err(SchemaError::WrongType {
                        column: column.name.clone(),
                        expected: column.data_type,
                        actual,
                    });
                }
                _ => {}
            }
        }

        Ok(())
    }
}

/// A schema or typed-row validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaError {
    /// A table must contain at least one user column.
    EmptySchema,
    /// A row key must contain at least one component.
    EmptyPrimaryKey,
    /// Column names must not be empty.
    EmptyColumnName,
    /// Stable column identifiers must be unique.
    DuplicateColumnId(u32),
    /// The top three identifiers belong to storage system columns.
    ReservedColumnId(u32),
    /// Column names must be unique.
    DuplicateColumnName(String),
    /// A parameterized logical type is outside Pintail's supported range.
    InvalidDataType {
        /// Column containing the invalid type.
        column: String,
        /// Invalid logical type.
        data_type: DataType,
    },
    /// A row has a different number of values than its schema.
    WrongArity {
        /// Required number of values.
        expected: usize,
        /// Supplied number of values.
        actual: usize,
    },
    /// A non-nullable column received `NULL`.
    NullInRequiredColumn(String),
    /// A scalar value does not match its column type.
    WrongType {
        /// Column name.
        column: String,
        /// Declared type.
        expected: DataType,
        /// Supplied type.
        actual: DataType,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => formatter.write_str("a table schema cannot be empty"),
            Self::EmptyPrimaryKey => formatter.write_str("a primary key cannot be empty"),
            Self::EmptyColumnName => formatter.write_str("a column name cannot be empty"),
            Self::DuplicateColumnId(id) => write!(formatter, "duplicate column id {id}"),
            Self::ReservedColumnId(id) => {
                write!(formatter, "column id {id} is reserved for storage metadata")
            }
            Self::DuplicateColumnName(name) => write!(formatter, "duplicate column name {name}"),
            Self::InvalidDataType { column, data_type } => {
                write!(formatter, "column {column} has invalid type {data_type:?}")
            }
            Self::WrongArity { expected, actual } => {
                write!(
                    formatter,
                    "row has {actual} values; schema requires {expected}"
                )
            }
            Self::NullInRequiredColumn(column) => {
                write!(formatter, "required column {column} received NULL")
            }
            Self::WrongType {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column} requires {expected:?}, received {actual:?}"
            ),
        }
    }
}

impl std::error::Error for SchemaError {}
