//! Catalog and schema versioning for Pintail.

use std::{collections::BTreeMap, fmt, sync::Arc};

use pintail_types::TableSchema;

/// Stable source-database identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseId(u64);

impl DatabaseId {
    /// Constructs a database identifier.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying catalog value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Stable source-table identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TableId(u64);

impl TableId {
    /// Constructs a table identifier.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying catalog value used by physical storage.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Statistics available to logical and physical planning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TableStatistics {
    row_count: Option<u64>,
}

impl TableStatistics {
    /// Constructs statistics with a known exact row count.
    #[must_use]
    pub const fn with_row_count(row_count: u64) -> Self {
        Self {
            row_count: Some(row_count),
        }
    }

    /// Returns the exact visible row count, when one has been collected.
    #[must_use]
    pub const fn row_count(self) -> Option<u64> {
        self.row_count
    }
}

/// An immutable table entry in a catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableEntry {
    id: TableId,
    name: String,
    schema: Arc<TableSchema>,
    statistics: TableStatistics,
    key_column_ids: Vec<u32>,
}

impl TableEntry {
    /// Constructs a table entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the table name is empty or when two column names
    /// differ only by ASCII case.
    pub fn new(
        id: TableId,
        name: impl Into<String>,
        schema: TableSchema,
        statistics: TableStatistics,
    ) -> Result<Self, CatalogError> {
        let name = name.into();
        if name.is_empty() {
            return Err(CatalogError::EmptyTableName);
        }

        let mut columns = BTreeMap::new();
        for column in schema.columns() {
            let normalized = normalize_name(column.name());
            if let Some(existing) = columns.insert(normalized, column.name()) {
                return Err(CatalogError::DuplicateColumnName {
                    table: name,
                    first: existing.to_owned(),
                    second: column.name().to_owned(),
                });
            }
        }

        Ok(Self {
            id,
            name,
            schema: Arc::new(schema),
            statistics,
            key_column_ids: Vec::new(),
        })
    }

    /// Declares the stable columns that produce the physical primary or
    /// unique storage key, in key-component order.
    ///
    /// # Errors
    ///
    /// Returns an error for append-row-id tables, an empty declaration,
    /// duplicate IDs, or IDs absent from the table schema.
    pub fn with_key_columns(
        mut self,
        column_ids: impl IntoIterator<Item = u32>,
    ) -> Result<Self, CatalogError> {
        if self.schema.key_mode() == pintail_types::KeyMode::AppendRowId {
            return Err(CatalogError::SyntheticKeyColumns);
        }
        let column_ids = column_ids.into_iter().collect::<Vec<_>>();
        if column_ids.is_empty() {
            return Err(CatalogError::EmptyKeyColumns);
        }
        let mut seen = std::collections::HashSet::with_capacity(column_ids.len());
        for id in &column_ids {
            if !seen.insert(*id) {
                return Err(CatalogError::DuplicateKeyColumn(*id));
            }
            if !self
                .schema
                .columns()
                .iter()
                .any(|column| column.id() == *id)
            {
                return Err(CatalogError::UnknownKeyColumn(*id));
            }
        }
        self.key_column_ids = column_ids;
        Ok(self)
    }

    /// Returns the stable table identifier.
    #[must_use]
    pub const fn id(&self) -> TableId {
        self.id
    }

    /// Returns the source table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the versioned physical schema.
    #[must_use]
    pub fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Returns planner statistics captured with this snapshot.
    #[must_use]
    pub const fn statistics(&self) -> TableStatistics {
        self.statistics
    }

    /// Returns stable physical key columns in component order.
    #[must_use]
    pub fn key_column_ids(&self) -> &[u32] {
        &self.key_column_ids
    }

    /// Looks up a column by an ASCII case-insensitive source name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&pintail_types::Column> {
        self.schema
            .columns()
            .iter()
            .find(|column| column.name().eq_ignore_ascii_case(name))
    }
}

/// An immutable database entry in a catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseEntry {
    id: DatabaseId,
    name: String,
    tables: BTreeMap<String, Arc<TableEntry>>,
    table_names_by_id: BTreeMap<TableId, String>,
}

impl DatabaseEntry {
    /// Constructs and indexes a database entry.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty database name or duplicate table name or
    /// identifier.
    pub fn new(
        id: DatabaseId,
        name: impl Into<String>,
        tables: impl IntoIterator<Item = TableEntry>,
    ) -> Result<Self, CatalogError> {
        let name = name.into();
        if name.is_empty() {
            return Err(CatalogError::EmptyDatabaseName);
        }

        let mut tables_by_name: BTreeMap<String, Arc<TableEntry>> = BTreeMap::new();
        let mut table_names_by_id: BTreeMap<TableId, String> = BTreeMap::new();
        for table in tables {
            let normalized = normalize_name(table.name());
            if let Some(existing) = tables_by_name.get(&normalized) {
                return Err(CatalogError::DuplicateTableName {
                    database: name,
                    first: existing.name().to_owned(),
                    second: table.name().to_owned(),
                });
            }
            if let Some(existing_name) = table_names_by_id.get(&table.id()) {
                return Err(CatalogError::DuplicateTableId {
                    database: name,
                    id: table.id(),
                    first: existing_name.to_owned(),
                    second: table.name().to_owned(),
                });
            }
            table_names_by_id.insert(table.id(), normalized.clone());
            tables_by_name.insert(normalized, Arc::new(table));
        }

        Ok(Self {
            id,
            name,
            tables: tables_by_name,
            table_names_by_id,
        })
    }

    /// Returns the stable database identifier.
    #[must_use]
    pub const fn id(&self) -> DatabaseId {
        self.id
    }

    /// Returns the source database name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Looks up a table by an ASCII case-insensitive source name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableEntry> {
        self.tables.get(&normalize_name(name)).map(AsRef::as_ref)
    }

    /// Looks up a table by stable identifier.
    #[must_use]
    pub fn table_by_id(&self, id: TableId) -> Option<&TableEntry> {
        self.table_names_by_id
            .get(&id)
            .and_then(|name| self.tables.get(name))
            .map(AsRef::as_ref)
    }

    /// Iterates tables in normalized-name order for deterministic metadata
    /// results.
    pub fn tables(&self) -> impl ExactSizeIterator<Item = &TableEntry> {
        self.tables.values().map(AsRef::as_ref)
    }
}

/// A point-in-time immutable view of query-visible databases and tables.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogSnapshot {
    databases: BTreeMap<String, Arc<DatabaseEntry>>,
    database_names_by_id: BTreeMap<DatabaseId, String>,
}

impl CatalogSnapshot {
    /// Constructs and indexes a complete catalog snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate database names or identifiers.
    pub fn new(databases: impl IntoIterator<Item = DatabaseEntry>) -> Result<Self, CatalogError> {
        let mut databases_by_name: BTreeMap<String, Arc<DatabaseEntry>> = BTreeMap::new();
        let mut database_names_by_id: BTreeMap<DatabaseId, String> = BTreeMap::new();
        for database in databases {
            let normalized = normalize_name(database.name());
            if let Some(existing) = databases_by_name.get(&normalized) {
                return Err(CatalogError::DuplicateDatabaseName {
                    first: existing.name().to_owned(),
                    second: database.name().to_owned(),
                });
            }
            if let Some(existing_name) = database_names_by_id.get(&database.id()) {
                return Err(CatalogError::DuplicateDatabaseId {
                    id: database.id(),
                    first: existing_name.to_owned(),
                    second: database.name().to_owned(),
                });
            }
            database_names_by_id.insert(database.id(), normalized.clone());
            databases_by_name.insert(normalized, Arc::new(database));
        }

        Ok(Self {
            databases: databases_by_name,
            database_names_by_id,
        })
    }

    /// Looks up a database by an ASCII case-insensitive source name.
    #[must_use]
    pub fn database(&self, name: &str) -> Option<&DatabaseEntry> {
        self.databases.get(&normalize_name(name)).map(AsRef::as_ref)
    }

    /// Looks up a database by stable identifier.
    #[must_use]
    pub fn database_by_id(&self, id: DatabaseId) -> Option<&DatabaseEntry> {
        self.database_names_by_id
            .get(&id)
            .and_then(|name| self.databases.get(name))
            .map(AsRef::as_ref)
    }

    /// Iterates databases in normalized-name order for deterministic metadata
    /// results.
    pub fn databases(&self) -> impl ExactSizeIterator<Item = &DatabaseEntry> {
        self.databases.values().map(AsRef::as_ref)
    }
}

/// Structural catalog validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    /// Database names cannot be empty.
    EmptyDatabaseName,
    /// Table names cannot be empty.
    EmptyTableName,
    /// Two databases have the same case-insensitive name.
    DuplicateDatabaseName {
        /// First spelling.
        first: String,
        /// Conflicting spelling.
        second: String,
    },
    /// Two databases have the same stable identifier.
    DuplicateDatabaseId {
        /// Conflicting identifier.
        id: DatabaseId,
        /// First database name.
        first: String,
        /// Second database name.
        second: String,
    },
    /// Two tables in one database have the same case-insensitive name.
    DuplicateTableName {
        /// Database containing the conflict.
        database: String,
        /// First spelling.
        first: String,
        /// Conflicting spelling.
        second: String,
    },
    /// Two tables in one database have the same stable identifier.
    DuplicateTableId {
        /// Database containing the conflict.
        database: String,
        /// Conflicting identifier.
        id: TableId,
        /// First table name.
        first: String,
        /// Second table name.
        second: String,
    },
    /// Two columns in one table have the same case-insensitive name.
    DuplicateColumnName {
        /// Table containing the conflict.
        table: String,
        /// First spelling.
        first: String,
        /// Conflicting spelling.
        second: String,
    },
    /// A physical primary/unique key declaration cannot be empty.
    EmptyKeyColumns,
    /// Append-row-id tables have no source key columns.
    SyntheticKeyColumns,
    /// A physical key declaration repeated one stable column ID.
    DuplicateKeyColumn(u32),
    /// A physical key declaration named an ID absent from the schema.
    UnknownKeyColumn(u32),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDatabaseName => formatter.write_str("database name cannot be empty"),
            Self::EmptyTableName => formatter.write_str("table name cannot be empty"),
            Self::DuplicateDatabaseName { first, second } => {
                write!(formatter, "database names {first} and {second} conflict")
            }
            Self::DuplicateDatabaseId { id, first, second } => write!(
                formatter,
                "database ID {} is shared by {first} and {second}",
                id.get()
            ),
            Self::DuplicateTableName {
                database,
                first,
                second,
            } => write!(
                formatter,
                "table names {first} and {second} conflict in database {database}"
            ),
            Self::DuplicateTableId {
                database,
                id,
                first,
                second,
            } => write!(
                formatter,
                "table ID {} is shared by {first} and {second} in database {database}",
                id.get()
            ),
            Self::DuplicateColumnName {
                table,
                first,
                second,
            } => write!(
                formatter,
                "column names {first} and {second} conflict in table {table}"
            ),
            Self::EmptyKeyColumns => formatter.write_str("key column declaration cannot be empty"),
            Self::SyntheticKeyColumns => {
                formatter.write_str("append-row-id tables have no source key columns")
            }
            Self::DuplicateKeyColumn(id) => write!(formatter, "duplicate key column ID {id}"),
            Self::UnknownKeyColumn(id) => write!(formatter, "unknown key column ID {id}"),
        }
    }
}

impl std::error::Error for CatalogError {}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use pintail_types::{Column, DataType, KeyMode, TableSchema};

    use super::{
        CatalogError, CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId,
        TableStatistics,
    };

    fn table(id: u64, name: &str, columns: &[(u32, &str)]) -> TableEntry {
        let schema = TableSchema::new(
            1,
            columns
                .iter()
                .map(|(id, name)| Column::new(*id, *name, DataType::Utf8, true))
                .collect(),
        )
        .expect("valid test schema");
        TableEntry::new(
            TableId::new(id),
            name,
            schema,
            TableStatistics::with_row_count(42),
        )
        .expect("valid table")
    }

    #[test]
    fn resolves_names_and_stable_ids_from_one_snapshot() {
        let events = table(9, "Events", &[(1, "EventId"), (2, "Payload")]);
        let database =
            DatabaseEntry::new(DatabaseId::new(7), "Analytics", [events]).expect("valid database");
        let catalog = CatalogSnapshot::new([database]).expect("valid catalog");

        let database = catalog.database("ANALYTICS").expect("database by name");
        assert_eq!(database.id(), DatabaseId::new(7));
        assert_eq!(
            catalog
                .database_by_id(DatabaseId::new(7))
                .expect("database by ID")
                .name(),
            "Analytics"
        );

        let table = database.table("events").expect("table by name");
        assert_eq!(table.id(), TableId::new(9));
        assert_eq!(table.statistics().row_count(), Some(42));
        assert_eq!(table.column("EVENTID").expect("column").id(), 1);
        assert_eq!(
            database
                .table_by_id(TableId::new(9))
                .expect("table by ID")
                .name(),
            "Events"
        );
    }

    #[test]
    fn metadata_iteration_is_deterministic() {
        let zeta = DatabaseEntry::new(
            DatabaseId::new(1),
            "zeta",
            [table(1, "z_table", &[(1, "value")])],
        )
        .expect("valid database");
        let alpha = DatabaseEntry::new(
            DatabaseId::new(2),
            "Alpha",
            [
                table(2, "Zulu", &[(1, "value")]),
                table(3, "alpha", &[(1, "value")]),
            ],
        )
        .expect("valid database");
        let catalog = CatalogSnapshot::new([zeta, alpha]).expect("valid catalog");

        assert_eq!(
            catalog
                .databases()
                .map(DatabaseEntry::name)
                .collect::<Vec<_>>(),
            ["Alpha", "zeta"]
        );
        assert_eq!(
            catalog
                .database("alpha")
                .expect("database")
                .tables()
                .map(TableEntry::name)
                .collect::<Vec<_>>(),
            ["alpha", "Zulu"]
        );
    }

    #[test]
    fn validates_explicit_physical_key_columns() {
        let keyed = table(1, "events", &[(1, "id"), (2, "name")])
            .with_key_columns([1, 2])
            .expect("declared physical key");
        assert_eq!(keyed.key_column_ids(), [1, 2]);

        assert_eq!(
            table(1, "events", &[(1, "id")])
                .with_key_columns([])
                .expect_err("empty declaration"),
            CatalogError::EmptyKeyColumns
        );
        assert_eq!(
            table(1, "events", &[(1, "id")])
                .with_key_columns([1, 1])
                .expect_err("duplicate declaration"),
            CatalogError::DuplicateKeyColumn(1)
        );
        assert_eq!(
            table(1, "events", &[(1, "id")])
                .with_key_columns([9])
                .expect_err("unknown declaration"),
            CatalogError::UnknownKeyColumn(9)
        );

        let append_schema = TableSchema::with_key_mode(
            1,
            vec![Column::new(1, "payload", DataType::Utf8, true)],
            KeyMode::AppendRowId,
        )
        .expect("append schema");
        let append = TableEntry::new(
            TableId::new(2),
            "log",
            append_schema,
            TableStatistics::default(),
        )
        .expect("append table");
        assert_eq!(
            append
                .with_key_columns([1])
                .expect_err("synthetic keys have no source mapping"),
            CatalogError::SyntheticKeyColumns
        );
    }

    #[test]
    fn rejects_case_insensitive_name_collisions() {
        let error = DatabaseEntry::new(
            DatabaseId::new(1),
            "db",
            [
                table(1, "Events", &[(1, "value")]),
                table(2, "events", &[(1, "value")]),
            ],
        )
        .expect_err("duplicate table");
        assert!(matches!(error, CatalogError::DuplicateTableName { .. }));

        let error = TableEntry::new(
            TableId::new(3),
            "readings",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "Value", DataType::Int64, false),
                    Column::new(2, "value", DataType::Int64, false),
                ],
            )
            .expect("case-sensitive physical schema"),
            TableStatistics::default(),
        )
        .expect_err("duplicate column");
        assert!(matches!(error, CatalogError::DuplicateColumnName { .. }));
    }

    #[test]
    fn rejects_stable_id_collisions() {
        let first =
            DatabaseEntry::new(DatabaseId::new(1), "first", []).expect("valid empty database");
        let second =
            DatabaseEntry::new(DatabaseId::new(1), "second", []).expect("valid empty database");
        let error = CatalogSnapshot::new([first, second]).expect_err("duplicate database ID");
        assert!(matches!(error, CatalogError::DuplicateDatabaseId { .. }));

        let error = DatabaseEntry::new(
            DatabaseId::new(2),
            "db",
            [
                table(7, "first", &[(1, "value")]),
                table(7, "second", &[(1, "value")]),
            ],
        )
        .expect_err("duplicate table ID");
        assert!(matches!(error, CatalogError::DuplicateTableId { .. }));
    }
}
