//! Executing what [`crate::bind_create_table`] and [`crate::bind_insert`]
//! produced: publishing a table, and committing rows.
//!
//! The catalog (SQLite) and the data (the shared WAL) live in different
//! durability domains, so `CREATE TABLE` cannot be one atomic write. It is
//! ordered instead — the row lands as `creating`, storage is published,
//! and only then does the row flip to `ready` — so a crash anywhere in
//! between leaves a leftover [`LocalDatabase::recover`] can identify and
//! remove. A `CREATE TABLE` that never returned to its client never
//! happened.

use std::path::{Path, PathBuf};

use pintail_meta::MetaStore;
use pintail_probe::{ProbeReport, SourceTable};
use pintail_store::{StoreOptions, TableSnapshot, TableStore, table_directory};
use pintail_types::StoredRow;
use sqlparser::ast::Statement;

use crate::{WriteError, bind_create_table, bind_insert};

/// What a completed write did, for the client's result packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    /// A table was published, or already existed under `IF NOT EXISTS`.
    TableCreated {
        /// The table's name as declared.
        table: String,
        /// Whether an existing table made this a no-op.
        existed: bool,
    },
    /// Rows were committed.
    RowsInserted {
        /// Affected-row count for the client.
        rows: u64,
        /// The commit version the store assigned.
        version: u64,
    },
}

/// The write half of one local database.
///
/// Holds no state: every call reopens the control plane and the table it
/// touches, so a writer never caches a catalog that another process may
/// have changed. Phase 2 is autocommit-only, so a statement is a
/// transaction.
pub struct LocalDatabase {
    database_id: String,
    tables_root: PathBuf,
    metadata_path: PathBuf,
}

impl LocalDatabase {
    /// Binds one local database's write path to its storage root.
    #[must_use]
    pub fn new(data_dir: &Path, metadata_path: &Path, database_id: &str) -> Self {
        Self {
            database_id: database_id.to_owned(),
            tables_root: data_dir.join("databases").join(database_id).join("tables"),
            metadata_path: metadata_path.to_owned(),
        }
    }

    /// Executes one statement, autocommitting it.
    ///
    /// # Errors
    ///
    /// Returns the binding rejection for an unsupported or invalid
    /// statement, and an [`WriteError::Invalid`] carrying the underlying
    /// failure when storage or the control plane cannot complete the write.
    pub fn execute(&self, statement: &Statement) -> Result<WriteOutcome, WriteError> {
        match statement {
            Statement::CreateTable(_) => self.create_table(statement),
            Statement::Insert(_) => self.insert(statement),
            other => Err(WriteError::Unsupported(format!(
                "`{other}` is not a statement a local database accepts"
            ))),
        }
    }

    /// The tables this database currently publishes.
    ///
    /// # Errors
    ///
    /// Returns an error when the control plane cannot be read.
    pub fn catalog(&self) -> Result<Vec<SourceTable>, WriteError> {
        let metadata = self.metadata()?;
        let database = metadata
            .database(&self.database_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::Invalid(format!("no database {}", self.database_id)))?;
        let Some(probe) = database.probe_json else {
            return Ok(Vec::new());
        };
        let report: ProbeReport = serde_json::from_str(&probe).map_err(internal)?;
        Ok(report.tables)
    }

    /// Removes tables left mid-creation by a crash, then rebuilds the
    /// catalog from what actually survived. Idempotent, and safe to call on
    /// every open.
    ///
    /// # Errors
    ///
    /// Returns an error when the control plane or the filesystem cannot be
    /// read.
    pub fn recover(&self) -> Result<Vec<String>, WriteError> {
        let metadata = self.metadata()?;
        let incomplete = metadata
            .incomplete_local_tables(&self.database_id)
            .map_err(internal)?;
        for table in &incomplete {
            // Directory first, then the row: a directory with no row is
            // invisible, while a row with no directory fails every read.
            let directory = table_directory(&self.tables_root, table);
            if directory.exists() {
                std::fs::remove_dir_all(&directory).map_err(internal)?;
            }
            metadata
                .remove_local_table(&self.database_id, table)
                .map_err(internal)?;
        }
        if !incomplete.is_empty() {
            self.publish_catalog(&metadata)?;
        }
        Ok(incomplete)
    }

    fn create_table(&self, statement: &Statement) -> Result<WriteOutcome, WriteError> {
        let plan = bind_create_table(statement)?;
        let name = plan.table.name.clone();
        let metadata = self.metadata()?;

        if self
            .catalog()?
            .iter()
            .any(|table| table.name.eq_ignore_ascii_case(&name))
        {
            return if plan.if_not_exists {
                Ok(WriteOutcome::TableCreated {
                    table: name,
                    existed: true,
                })
            } else {
                Err(WriteError::TableExists(name))
            };
        }

        let key_json = serde_json::to_string(&plan.table.key.columns).map_err(internal)?;
        metadata
            .begin_local_table(&self.database_id, &name, &key_json)
            .map_err(|error| WriteError::TableExists(format!("{name}: {error}")))?;

        // Publishing storage is the step that can fail on the filesystem;
        // the 'creating' row above is what makes that failure recoverable
        // rather than a permanent half-table.
        let published = (|| -> Result<(), WriteError> {
            let schema = plan
                .table
                .table_schema_with_version(1)
                .map_err(|error| WriteError::Invalid(error.to_string()))?;
            let directory = table_directory(&self.tables_root, &name);
            std::fs::create_dir_all(&directory).map_err(internal)?;
            // Opening a transactional store writes its empty manifest; the
            // handle is dropped immediately because nothing is being
            // written yet.
            drop(TableStore::open(&directory, schema, local_store_options()).map_err(internal)?);
            let columns_json = serde_json::to_string(&plan.table.columns).map_err(internal)?;
            self.metadata()?
                .record_schema_history(&self.database_id, &name, 1, None, &columns_json, &now())
                .map_err(internal)
        })();
        if let Err(error) = published {
            // Leave nothing half-built behind: recovery would also clean
            // this up, but the client is being told the statement failed.
            let _ = self.recover();
            return Err(error);
        }

        metadata
            .finish_local_table(&self.database_id, &name)
            .map_err(internal)?;
        self.publish_catalog(&metadata)?;
        Ok(WriteOutcome::TableCreated {
            table: name,
            existed: false,
        })
    }

    fn insert(&self, statement: &Statement) -> Result<WriteOutcome, WriteError> {
        // The target has to be resolved before binding, because binding
        // types every literal against the table's declared columns.
        let target = insert_target(statement)?;
        let catalog = self.catalog()?;
        let table = catalog
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(&target))
            .ok_or(WriteError::UnknownTable(target))?;
        let plan = bind_insert(statement, table)?;
        if plan.rows.is_empty() {
            return Ok(WriteOutcome::RowsInserted {
                rows: 0,
                version: 0,
            });
        }

        let schema = table
            .table_schema_with_version(1)
            .map_err(|error| WriteError::Invalid(error.to_string()))?;
        let directory = table_directory(&self.tables_root, &table.name);
        let mut store =
            TableStore::open(&directory, schema, local_store_options()).map_err(internal)?;
        reject_existing_keys(&store.snapshot(), &plan.rows)?;

        let rows = u64::try_from(plan.rows.len()).unwrap_or(u64::MAX);
        let version = store.commit(plan.rows).map_err(internal)?;
        Ok(WriteOutcome::RowsInserted { rows, version })
    }

    /// Rewrites the database's catalog from the tables that are actually
    /// published, so it is derived state rather than an independently
    /// maintained copy that a crash could desynchronize.
    fn publish_catalog(&self, metadata: &MetaStore) -> Result<(), WriteError> {
        let mut tables = Vec::new();
        for record in metadata
            .tables(&self.database_id)
            .map_err(internal)?
            .into_iter()
            .filter(|record| record.state == "ready")
        {
            let history = metadata
                .schema_history(&self.database_id, &record.name)
                .map_err(internal)?;
            let Some(latest) = history.last() else {
                continue;
            };
            let columns = serde_json::from_str(&latest.columns_json).map_err(internal)?;
            let key_columns: Vec<String> = record
                .primary_key_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(internal)?
                .unwrap_or_default();
            tables.push(local_source_table(&record.name, columns, key_columns));
        }
        tables.sort_by(|left, right| left.name.cmp(&right.name));

        let report = local_report(&self.database_id, tables);
        metadata
            .refresh_database_probe_json(
                &self.database_id,
                &serde_json::to_string(&report).map_err(internal)?,
                &now(),
            )
            .map_err(internal)
    }

    fn metadata(&self) -> Result<MetaStore, WriteError> {
        MetaStore::open(&self.metadata_path).map_err(internal)
    }
}

/// Refuses a row whose key is already stored (`MySQL` 1062).
///
/// Phase 2 has one serialized writer, so reading the pinned snapshot and
/// then committing cannot race another writer into a duplicate.
fn reject_existing_keys(snapshot: &TableSnapshot, rows: &[StoredRow]) -> Result<(), WriteError> {
    for row in rows {
        if snapshot.get(row.key()).map_err(internal)?.is_some() {
            return Err(WriteError::DuplicateKey(crate::render_key(row.key())));
        }
    }
    Ok(())
}

fn insert_target(statement: &Statement) -> Result<String, WriteError> {
    let Statement::Insert(insert) = statement else {
        return Err(WriteError::Unsupported("expected INSERT".to_owned()));
    };
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return Err(WriteError::Unsupported(
            "INSERT into a table function is not supported".to_owned(),
        ));
    };
    let rendered = name.to_string();
    Ok(rendered
        .rsplit('.')
        .next()
        .unwrap_or(&rendered)
        .trim_matches('`')
        .to_owned())
}

/// Storage options for a local table: transactional, so rows become visible
/// only through a durable commit.
fn local_store_options() -> StoreOptions {
    StoreOptions {
        transactional: true,
        ..StoreOptions::default()
    }
}

fn local_source_table(
    name: &str,
    columns: Vec<pintail_probe::SourceColumn>,
    key_columns: Vec<String>,
) -> SourceTable {
    SourceTable {
        name: name.to_owned(),
        engine: Some("Pintail".to_owned()),
        estimated_rows: None,
        rows_are_exact: false,
        columns,
        key: pintail_probe::SourceKey {
            mode: pintail_types::KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: key_columns,
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        foreign_keys: Vec::new(),
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
    }
}

/// A local database's catalog, shaped as the report the read path already
/// knows how to consume. Nothing here was probed: the capabilities describe
/// a database that is its own source, so no replication decision can read
/// them as a source's.
fn local_report(database: &str, tables: Vec<SourceTable>) -> ProbeReport {
    ProbeReport {
        database: database.to_owned(),
        server: pintail_probe::ServerIdentity {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            version_comment: "Pintail local database".to_owned(),
            flavor: pintail_probe::SourceFlavor::Mysql,
        },
        variables: std::collections::BTreeMap::new(),
        grants: Vec::new(),
        capabilities: pintail_probe::SourceCapabilities {
            log_bin: false,
            row_binlog: false,
            full_row_image: false,
            full_row_metadata: false,
            replication_grants: false,
            global_read_lock: false,
            gtid_available: false,
            recommended_mode: pintail_probe::RecommendedMode::Polling,
            reasons: Vec::new(),
        },
        tables,
        warnings: Vec::new(),
    }
}

fn now() -> String {
    // The control plane stores RFC-3339 timestamps; a local write does not
    // need better resolution than the second the statement ran in.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    format!("{seconds}")
}

fn internal(error: impl std::fmt::Display) -> WriteError {
    WriteError::Invalid(error.to_string())
}
