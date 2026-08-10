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

/// Parses the labels out of a `MySQL` `ENUM(...)` or `SET(...)` declaration,
/// in declaration order.
///
/// The order is the point: `MySQL` compares and sorts an ENUM by its
/// one-based declaration index, so the position in this list *is* the sort
/// key. Returns `None` when the text is not that kind of declaration or is
/// malformed, leaving the caller to treat the column as plain text rather
/// than inventing an ordering.
#[must_use]
pub fn declaration_labels(column_type: &str, kind: &str) -> Option<Vec<String>> {
    let declaration = column_type.trim();
    let prefix = format!("{kind}(");
    if !declaration.to_ascii_lowercase().starts_with(&prefix) || !declaration.ends_with(')') {
        return None;
    }
    let body = &declaration[prefix.len()..declaration.len() - 1];
    let mut labels = Vec::new();
    let mut characters = body.chars().peekable();
    while characters.peek().is_some() {
        if characters.next() != Some('\'') {
            return None;
        }
        let mut label = String::new();
        loop {
            match characters.next() {
                Some('\\') => label.push(characters.next()?),
                // A doubled quote is an escaped quote, not the end.
                Some('\'') if characters.peek() == Some(&'\'') => {
                    characters.next();
                    label.push('\'');
                }
                Some('\'') => break,
                Some(character) => label.push(character),
                None => return None,
            }
        }
        labels.push(label);
        match characters.next() {
            Some(',') => {}
            None => break,
            _ => return None,
        }
    }
    (!labels.is_empty()).then_some(labels)
}

/// A stable table column definition.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Column {
    id: u32,
    name: String,
    data_type: DataType,
    nullable: bool,
    collation: Option<String>,
    /// Declared ENUM labels in declaration order, when the source column is
    /// an ENUM. Absent for every other column.
    ///
    /// Held on the column rather than on each value because the order is a
    /// property of the declaration: storing the index per row would repeat
    /// one number a million times and go stale the moment the declaration
    /// changed. The read path reattaches it.
    enum_labels: Option<Vec<String>>,
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
            collation: None,
            enum_labels: None,
        }
    }

    /// Attaches the source text collation used for query semantics. Storage
    /// encoding is unchanged; non-text columns should leave this absent.
    #[must_use]
    pub fn with_collation(mut self, collation: Option<String>) -> Self {
        self.collation = collation;
        self
    }

    /// Attaches source ENUM labels in declaration order. Storage encoding is
    /// unchanged; the labels only let the read path recover the sort order
    /// `MySQL` uses.
    #[must_use]
    pub fn with_enum_labels(mut self, enum_labels: Option<Vec<String>>) -> Self {
        self.enum_labels = enum_labels;
        self
    }

    /// Returns the declared ENUM labels in declaration order, when any.
    #[must_use]
    pub fn enum_labels(&self) -> Option<&[String]> {
        self.enum_labels.as_deref()
    }

    /// Returns the one-based declaration index of `label`, when declared.
    #[must_use]
    pub fn enum_index_of(&self, label: &str) -> Option<u16> {
        self.enum_labels.as_ref().and_then(|labels| {
            labels
                .iter()
                .position(|declared| declared == label)
                .and_then(|position| u16::try_from(position + 1).ok())
        })
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

    /// Returns the source text collation, when the column is textual.
    #[must_use]
    pub fn collation(&self) -> Option<&str> {
        self.collation.as_deref()
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
