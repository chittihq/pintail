use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use fs2::FileExt;
use pintail_types::{PrimaryKey, StoredRow, TableSchema};

use crate::{StoreError, memtable::Memtable, wal::Wal};

const WAL_FILE: &str = "table.wal";
const WRITER_LOCK_FILE: &str = ".writer.lock";
const DEFAULT_MEMTABLE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_BLOCK_ROWS: usize = 64 * 1024;

/// WAL durability policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WalSync {
    /// Synchronize every accepted batch before returning.
    Always,
    /// Synchronize when [`TableStore::checkpoint`] is called.
    #[default]
    Checkpoint,
    /// Do not explicitly synchronize WAL writes.
    Off,
}

/// Storage settings fixed for the lifetime of an open table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreOptions {
    /// Memtable bytes that request a flush.
    pub memtable_bytes: usize,
    /// Target rows per segment block.
    pub block_rows: usize,
    /// WAL synchronization policy.
    pub wal_sync: WalSync,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            block_rows: DEFAULT_BLOCK_ROWS,
            wal_sync: WalSync::Checkpoint,
        }
    }
}

/// Result of accepting one atomic WAL batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngestOutcome {
    sequence: u64,
    accepted_rows: usize,
    visible_rows: usize,
    should_flush: bool,
}

impl IngestOutcome {
    /// Returns the WAL sequence assigned to this batch.
    #[must_use]
    pub fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the number of validated and logged rows.
    #[must_use]
    pub fn accepted_rows(self) -> usize {
        self.accepted_rows
    }

    /// Returns rows that replaced an older or absent in-memory version.
    #[must_use]
    pub fn visible_rows(self) -> usize {
        self.visible_rows
    }

    /// Returns whether the configured memtable limit has been reached.
    #[must_use]
    pub fn should_flush(self) -> bool {
        self.should_flush
    }
}

/// The single-writer handle for one physical table.
pub struct TableStore {
    _writer_lock: File,
    directory: PathBuf,
    schema: TableSchema,
    options: StoreOptions,
    wal: Wal,
    memtable: Memtable,
    last_sequence: u64,
}

impl TableStore {
    /// Opens a table, exclusively claims its writer lock, and replays its WAL.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures, a competing writer, corrupt
    /// WAL bytes, or recovered rows that do not match the supplied schema.
    pub fn open(
        directory: impl AsRef<Path>,
        schema: TableSchema,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory)
            .map_err(|error| StoreError::io("create table directory", error))?;

        let writer_lock = open_lock(&directory.join(WRITER_LOCK_FILE))?;
        FileExt::try_lock_exclusive(&writer_lock).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::WriterBusy
            } else {
                StoreError::io("lock table writer", error)
            }
        })?;

        let (wal, recovery) = Wal::open(&directory.join(WAL_FILE), options.wal_sync)?;
        let mut memtable = Memtable::default();
        for (_, rows) in recovery.batches {
            for row in rows {
                schema.validate_row(&row)?;
                memtable.apply(&row);
            }
        }

        Ok(Self {
            _writer_lock: writer_lock,
            directory,
            schema,
            options,
            wal,
            memtable,
            last_sequence: recovery.last_sequence,
        })
    }

    /// Validates and durably orders one atomic row batch.
    ///
    /// The WAL append completes before any row becomes visible to a new
    /// snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, encoding, or WAL I/O fails.
    pub fn ingest(&mut self, rows: Vec<StoredRow>) -> Result<IngestOutcome, StoreError> {
        for row in &rows {
            self.schema.validate_row(row)?;
        }
        if rows.is_empty() {
            return Ok(IngestOutcome {
                sequence: self.last_sequence,
                accepted_rows: 0,
                visible_rows: 0,
                should_flush: false,
            });
        }

        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        self.wal.append(sequence, &rows)?;

        let accepted_rows = rows.len();
        let visible_rows = rows
            .into_iter()
            .filter(|row| self.memtable.apply(row))
            .count();
        self.last_sequence = sequence;

        Ok(IngestOutcome {
            sequence,
            accepted_rows,
            visible_rows,
            should_flush: self.memtable.estimated_bytes() >= self.options.memtable_bytes,
        })
    }

    /// Synchronizes accepted WAL bytes under the checkpoint policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot synchronize the WAL.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.wal.sync()
    }

    /// Pins an immutable view of rows currently visible to readers.
    #[must_use]
    pub fn snapshot(&self) -> TableSnapshot {
        TableSnapshot {
            memtable: self.memtable.snapshot(),
        }
    }

    /// Returns the table directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// A reader-owned immutable table view.
pub struct TableSnapshot {
    memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
}

impl TableSnapshot {
    /// Returns visible rows in primary-key order, excluding tombstones.
    ///
    /// # Errors
    ///
    /// This initial in-memory implementation is infallible; the result type
    /// preserves the interface used when immutable segments are attached.
    pub fn scan(&self) -> Result<Vec<StoredRow>, StoreError> {
        Ok(self
            .memtable
            .values()
            .filter(|row| !row.is_deleted())
            .cloned()
            .collect())
    }
}

fn open_lock(path: &Path) -> Result<File, StoreError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| StoreError::io("open table writer lock", error))
}
