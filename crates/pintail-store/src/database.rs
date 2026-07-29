use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use pintail_types::{StoredRow, TableSchema};

use crate::{
    CompactionOutcome, FlushOutcome, IngestOutcome, StorageMetrics, StoreError, StoreOptions,
    TableSnapshot, TableStore, wal::Wal,
};

const WAL_FILE: &str = "database.wal";
const WRITER_LOCK_FILE: &str = ".database.writer.lock";
const TABLES_DIRECTORY: &str = "tables";

/// The single-writer handle for all physical tables in one source database.
///
/// A database owns one monotonically sequenced WAL. Table manifests and
/// immutable segments remain isolated below `tables/<table_id>`.
pub struct DatabaseStore {
    _writer_lock: File,
    directory: PathBuf,
    wal: Wal,
    tables: BTreeMap<u64, TableStore>,
    last_sequence: u64,
}

impl DatabaseStore {
    /// Opens one database and replays its shared WAL into registered tables.
    ///
    /// Table IDs must be unique and stable catalog identifiers. Every table ID
    /// still present in the WAL must be registered so its rows cannot be
    /// silently discarded during recovery.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate or missing table IDs, invalid options,
    /// filesystem failures, a competing writer, or corrupt durable data.
    pub fn open(
        directory: impl AsRef<Path>,
        schemas: impl IntoIterator<Item = (u64, TableSchema)>,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        validate_options(options)?;
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(directory.join(TABLES_DIRECTORY))
            .map_err(|error| StoreError::io("create database directory", error))?;

        let writer_lock = open_lock(&directory.join(WRITER_LOCK_FILE))?;
        FileExt::try_lock_exclusive(&writer_lock).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::WriterBusy
            } else {
                StoreError::io("lock database writer", error)
            }
        })?;

        let mut schemas_by_id = BTreeMap::new();
        for (table_id, schema) in schemas {
            if schemas_by_id.insert(table_id, schema).is_some() {
                return Err(StoreError::FormatLimit(format!(
                    "duplicate database table ID {table_id}"
                )));
            }
        }

        let wal_path = directory.join(WAL_FILE);
        let (mut wal, recovery) = Wal::open(&wal_path, options.wal_sync)?;
        for batch in &recovery.batches {
            if !schemas_by_id.contains_key(&batch.table_id) {
                return Err(StoreError::UnknownTable {
                    table_id: batch.table_id,
                });
            }
        }

        let mut tables = BTreeMap::new();
        for (table_id, schema) in schemas_by_id {
            let table = TableStore::open_with_wal(
                directory.join(TABLES_DIRECTORY).join(table_id.to_string()),
                &wal_path,
                table_id,
                schema,
                options,
                false,
            )?;
            tables.insert(table_id, table);
        }
        let last_sequence = tables
            .values()
            .map(TableStore::last_sequence)
            .max()
            .unwrap_or(recovery.last_sequence)
            .max(recovery.last_sequence);

        if !recovery.batches.is_empty() && tables.values().all(|table| !table.has_pending_rows()) {
            wal.reset()?;
        }

        Ok(Self {
            _writer_lock: writer_lock,
            directory,
            wal,
            tables,
            last_sequence,
        })
    }

    /// Validates and appends one table batch to the database WAL.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table, invalid rows, sequence overflow,
    /// or durable storage failure.
    pub fn ingest(
        &mut self,
        table_id: u64,
        rows: Vec<StoredRow>,
    ) -> Result<IngestOutcome, StoreError> {
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        let table = self
            .tables
            .get_mut(&table_id)
            .ok_or(StoreError::UnknownTable { table_id })?;
        let outcome = table.ingest_at_sequence(sequence, rows);
        self.last_sequence = self.last_sequence.max(table.last_sequence());
        let outcome = outcome?;
        self.reset_wal_if_fully_flushed()?;
        Ok(outcome)
    }

    /// Synchronizes the shared WAL under the configured checkpoint policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot synchronize the WAL.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.wal.sync()
    }

    /// Flushes one table without discarding another table's WAL records.
    ///
    /// The shared WAL is truncated only if every registered table has an empty
    /// memtable after this flush.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or failed durable publication.
    pub fn flush(&mut self, table_id: u64) -> Result<FlushOutcome, StoreError> {
        let outcome = self
            .tables
            .get_mut(&table_id)
            .ok_or(StoreError::UnknownTable { table_id })?
            .flush()?;
        self.reset_wal_if_fully_flushed()?;
        Ok(outcome)
    }

    /// Flushes every registered table and checkpoints the now-redundant WAL.
    ///
    /// # Errors
    ///
    /// Returns an error when any table or WAL publication fails.
    pub fn flush_all(&mut self) -> Result<Vec<(u64, FlushOutcome)>, StoreError> {
        let mut outcomes = Vec::with_capacity(self.tables.len());
        for (table_id, table) in &mut self.tables {
            outcomes.push((*table_id, table.flush()?));
        }
        self.reset_wal_if_fully_flushed()?;
        Ok(outcomes)
    }

    /// Runs one bounded compaction pass for a table.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or failed compaction.
    pub fn compact(&mut self, table_id: u64) -> Result<CompactionOutcome, StoreError> {
        self.tables
            .get_mut(&table_id)
            .ok_or(StoreError::UnknownTable { table_id })?
            .compact()
    }

    /// Reclaims obsolete files for one table after pinned snapshots release.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or failed file removal.
    pub fn reclaim_obsolete_segments(&mut self, table_id: u64) -> Result<usize, StoreError> {
        self.tables
            .get_mut(&table_id)
            .ok_or(StoreError::UnknownTable { table_id })?
            .reclaim_obsolete_segments()
    }

    /// Pins the current immutable read view for one table.
    ///
    /// # Errors
    ///
    /// Returns an error when the table ID is not registered.
    pub fn snapshot(&self, table_id: u64) -> Result<TableSnapshot, StoreError> {
        self.tables
            .get(&table_id)
            .map(TableStore::snapshot)
            .ok_or(StoreError::UnknownTable { table_id })
    }

    /// Returns one table's current memory and maintenance metrics.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or unreadable segment metadata.
    pub fn metrics(&self, table_id: u64) -> Result<StorageMetrics, StoreError> {
        self.tables
            .get(&table_id)
            .ok_or(StoreError::UnknownTable { table_id })?
            .metrics()
    }

    /// Returns the database storage directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn reset_wal_if_fully_flushed(&mut self) -> Result<(), StoreError> {
        if self.tables.values().all(|table| !table.has_pending_rows()) {
            self.wal.reset()?;
        }
        Ok(())
    }
}

fn validate_options(options: StoreOptions) -> Result<(), StoreError> {
    if options.block_rows == 0 {
        return Err(StoreError::FormatLimit(
            "segment block row target must be non-zero".into(),
        ));
    }
    if options.compaction_fan_in < 2 {
        return Err(StoreError::FormatLimit(
            "compaction fan-in must be at least two".into(),
        ));
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File, StoreError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| StoreError::io(format!("open writer lock {}", path.display()), error))
}
