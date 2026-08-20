mod scan;
mod snapshot;

pub use scan::{
    ColumnValidity, DecodedColumn, PrewhereSelect, ProjectedColumnChunk, ProjectedRow,
    ProjectedScan, ProjectedScanStream, ProjectedValueChunk, ScanStats,
};
pub use snapshot::{BackupArtifacts, BackupSegment, TableSnapshot};

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::AtomicUsize},
};

use fs2::FileExt;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, StoredRow, TableSchema};

use crate::{
    StoreError,
    manifest::{self, Manifest},
    memtable::Memtable,
    segment,
    wal::{RecoveredBatch, Wal, WalColumn},
};

const WAL_FILE: &str = "table.wal";

#[derive(Clone, Copy)]
enum AppendKeyPolicy {
    Generate,
    Preserve,
}
const WRITER_LOCK_FILE: &str = ".writer.lock";
const DEFAULT_MEMTABLE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_BLOCK_ROWS: usize = 16 * 1024;
const DEFAULT_COMPACTION_FAN_IN: usize = 4;
// One flush of a default memtable yields a segment of a few hundred thousand
// rows, so both compaction bounds have to sit well above that: an input bound
// below the natural segment size rejects every window and stops compaction
// entirely, and an output bound below it splits one merge into more segments
// than it consumed.
const DEFAULT_MAX_COMPACTION_INPUT_ROWS: u64 = 8_000_000;
const DEFAULT_MAX_COMPACTION_ROWS: u64 = 4_000_000;
const DEFAULT_MAX_COMPACTION_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_COMPACTION_FILE_PRESSURE: usize = 16;
const DEFAULT_COMPACTION_DISK_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const SIZE_TIER_RATIO: u64 = 4;
static PROJECTED_SCAN_POOL: OnceLock<Result<rayon::ThreadPool, String>> = OnceLock::new();

/// Threads available to decode projected column chunks.
///
/// Callers size their prefetch width from this. The decode runs in this pool,
/// so a width below its thread count leaves threads idle for the whole scan -
/// which is what a hardcoded width of eight did on a sixteen-thread host.
pub fn projected_scan_width() -> usize {
    projected_scan_pool().map_or(1, rayon::ThreadPool::current_num_threads)
}

fn projected_scan_pool() -> Result<&'static rayon::ThreadPool, StoreError> {
    PROJECTED_SCAN_POOL
        .get_or_init(|| {
            // Overridable, because it was not. This pool is separate from the
            // one the executor uses, so `RAYON_NUM_THREADS` never reached it -
            // every thread sweep taken against this engine held scans at full
            // width while believing it was varying them, and the serial
            // fractions that came out described only the operators above the
            // scan. It is also a real tuning knob: two pools each sized to the
            // machine put twice the core count of runnable threads on it
            // whenever aggregation overlaps scanning.
            let threads = std::env::var("PINTAIL_SCAN_THREADS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|threads| *threads > 0)
                .unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
                });
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|index| format!("pintail-scan-{index}"))
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| {
            StoreError::FormatLimit(format!("cannot initialize projected scan pool: {error}"))
        })
}

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
/// WAL header length (`MAGIC` + version byte); the truncation floor when a
/// transactional log holds no commit record at all.
const HEADER_LENGTH_FOR_TRUNCATION: u64 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreOptions {
    /// Memtable bytes that request a flush.
    pub memtable_bytes: usize,
    /// Target rows per segment block.
    pub block_rows: usize,
    /// WAL synchronization policy.
    pub wal_sync: WalSync,
    /// Number of similarly sized overlapping segments merged in one pass.
    pub compaction_fan_in: usize,
    /// Maximum total input rows admitted to one compaction pass.
    pub max_compaction_input_rows: u64,
    /// Maximum rows retained in one compaction output buffer and segment.
    pub max_compaction_rows: u64,
    /// Maximum bytes retained in one compaction output buffer and segment.
    /// Whichever of this and [`Self::max_compaction_rows`] is reached first
    /// closes the output segment, so wide rows cannot make the buffer grow
    /// with the row bound.
    pub max_compaction_output_bytes: usize,
    /// Live segment count above which key-adjacent files in one size tier are
    /// merged even when their key ranges do not overlap. Append-only sources
    /// produce nothing but disjoint segments, and without this they would
    /// never consolidate.
    pub compaction_file_pressure: usize,
    /// Free bytes that must remain after a merge writes its output. A merge
    /// holds its inputs until the new segments are published, so it needs
    /// their size again transiently; below this floor the pass is deferred
    /// rather than risking a full volume mid-write.
    pub compaction_disk_reserve_bytes: u64,
    /// Whether size-tier merges run on a background thread instead of
    /// inline on the ingest path.
    pub background_compaction: bool,
    /// Local writable-table mode: rows become visible only through
    /// [`TableStore::commit`], and recovery replays exactly the committed
    /// WAL prefix (docs/design/writable-mode.md, phase 1).
    pub transactional: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            transactional: false,
            block_rows: DEFAULT_BLOCK_ROWS,
            wal_sync: WalSync::Checkpoint,
            compaction_fan_in: DEFAULT_COMPACTION_FAN_IN,
            max_compaction_input_rows: DEFAULT_MAX_COMPACTION_INPUT_ROWS,
            max_compaction_rows: DEFAULT_MAX_COMPACTION_ROWS,
            max_compaction_output_bytes: DEFAULT_MAX_COMPACTION_OUTPUT_BYTES,
            compaction_file_pressure: DEFAULT_COMPACTION_FILE_PRESSURE,
            compaction_disk_reserve_bytes: DEFAULT_COMPACTION_DISK_RESERVE_BYTES,
            background_compaction: true,
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

/// Result of publishing the current memtable as an immutable segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlushOutcome {
    row_count: usize,
    segment_path: Option<PathBuf>,
}

/// Result of publishing a sorted snapshot chunk directly as a segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkIngestOutcome {
    row_count: usize,
    segment_path: Option<PathBuf>,
}

impl BulkIngestOutcome {
    /// Returns the number of rows published into the immutable segment.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the published segment, or `None` for an empty chunk.
    #[must_use]
    pub fn segment_path(&self) -> Option<&Path> {
        self.segment_path.as_deref()
    }
}

impl FlushOutcome {
    /// Returns the number of latest row versions written to the segment.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the published segment, or `None` when the memtable was empty.
    #[must_use]
    pub fn segment_path(&self) -> Option<&Path> {
        self.segment_path.as_deref()
    }
}

/// Current amount of immutable data eligible for one compaction pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionStatus {
    segment_count: usize,
    eligible_segments: usize,
    debt_bytes: u64,
}

/// Point-in-time values exported by the storage metrics surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageMetrics {
    memtable_bytes: usize,
    segment_count: usize,
    compaction_debt_bytes: u64,
}

impl StorageMetrics {
    /// Returns the current mutable-table byte estimate.
    #[must_use]
    pub fn memtable_bytes(self) -> usize {
        self.memtable_bytes
    }

    /// Returns live immutable segment count.
    #[must_use]
    pub fn segment_count(self) -> usize {
        self.segment_count
    }

    /// Returns bytes eligible for the next bounded compaction pass.
    #[must_use]
    pub fn compaction_debt_bytes(self) -> u64 {
        self.compaction_debt_bytes
    }
}

impl CompactionStatus {
    /// Returns all live segments in the pinned manifest generation.
    #[must_use]
    pub fn segment_count(self) -> usize {
        self.segment_count
    }

    /// Returns segments selected by the next size-tier pass.
    #[must_use]
    pub fn eligible_segments(self) -> usize {
        self.eligible_segments
    }

    /// Returns bytes that the next compaction pass must rewrite.
    #[must_use]
    pub fn debt_bytes(self) -> u64 {
        self.debt_bytes
    }
}

/// Result of one bounded size-tier compaction pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionOutcome {
    input_segments: usize,
    output_rows: usize,
    output_path: Option<PathBuf>,
    deferred: Option<&'static str>,
}

impl CompactionOutcome {
    /// Returns the number of segments replaced by this pass.
    #[must_use]
    pub fn input_segments(&self) -> usize {
        self.input_segments
    }

    /// Returns rows retained after version and tombstone resolution.
    #[must_use]
    pub fn output_rows(&self) -> usize {
        self.output_rows
    }

    /// Returns the replacement segment, if the merge retained any rows.
    #[must_use]
    pub fn output_path(&self) -> Option<&Path> {
        self.output_path.as_deref()
    }

    /// Returns why an eligible merge was not run, when one was skipped.
    #[must_use]
    pub fn deferred_reason(&self) -> Option<&'static str> {
        self.deferred
    }
}

struct RetiredGeneration {
    readers: Weak<Manifest>,
    paths: Vec<PathBuf>,
}

/// The single-writer handle for one physical table.
pub struct TableStore {
    _writer_lock: File,
    directory: PathBuf,
    schema: TableSchema,
    options: StoreOptions,
    wal: Wal,
    memtable: Memtable,
    manifest: Arc<Manifest>,
    retired: Vec<RetiredGeneration>,
    last_sequence: u64,
    /// Highest committed local-transaction version (transactional mode).
    commit_version: u64,
    next_append_row_id: u64,
    table_id: u64,
    truncate_wal_on_flush: bool,
    /// In-flight background merge, at most one. The thread only reads
    /// immutable input segments and writes chunk files nothing references
    /// yet; publication happens on this handle's thread.
    background: Option<BackgroundMerge>,
    /// The most recent background-merge failure, surfaced for diagnostics;
    /// the merge itself is retried by the next eligible pass.
    last_background_error: Option<String>,
}

/// One background size-tier merge in flight.
struct BackgroundMerge {
    receiver: std::sync::mpsc::Receiver<Result<Vec<segment::SegmentMeta>, StoreError>>,
    input_files: Vec<String>,
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
        let wal_path = directory.join(WAL_FILE);
        Self::open_with_wal(&directory, &wal_path, 0, schema, options, true)
    }

    pub(crate) fn open_with_wal(
        directory: &Path,
        wal_path: &Path,
        table_id: u64,
        schema: TableSchema,
        options: StoreOptions,
        truncate_wal_on_flush: bool,
    ) -> Result<Self, StoreError> {
        validate_store_options(options)?;
        std::fs::create_dir_all(directory)
            .map_err(|error| StoreError::io("create table directory", error))?;
        let directory = std::fs::canonicalize(directory)
            .map_err(|error| StoreError::io("canonicalize table directory", error))?;

        let writer_lock = open_lock(&directory.join(WRITER_LOCK_FILE))?;
        lock_writer(&writer_lock, "lock table writer")?;

        let mut manifest = manifest::load(&directory, &schema)?;
        let schema_upgrade = manifest.schema_version < schema.version();
        for meta in &manifest.segments {
            if schema_upgrade {
                segment::read(&directory, meta, &schema)?;
            } else {
                segment::verify(&directory, meta, &schema)?;
            }
        }
        remove_orphan_segments(&directory, &manifest)?;
        let (mut wal, mut recovery) = Wal::open(wal_path, options.wal_sync)?;
        let mut commit_version = manifest.committed_version;
        if options.transactional {
            // Rows after the last commit record were never acknowledged;
            // drop them from replay and from the log itself.
            let committed = recovery.last_commit;
            let committed_batches = committed.map_or(0, |commit| commit.batches);
            if recovery.batches.len() > committed_batches {
                recovery.batches.truncate(committed_batches);
                let offset =
                    committed.map_or(HEADER_LENGTH_FOR_TRUNCATION, |commit| commit.end_offset);
                wal.truncate_to(offset)?;
            }
            if let Some(commit) = committed {
                commit_version = commit_version.max(commit.version);
            }
        }
        let recovery_last_sequence = recovery.last_sequence;
        let recovered_batches = recovery
            .batches
            .iter()
            .any(|batch| batch.table_id == table_id);
        let mut memtable = Memtable::default();
        for batch in recovery.batches {
            let RecoveredBatch {
                sequence,
                table_id: recovered_table_id,
                columns,
                rows,
            } = batch;
            if recovered_table_id != table_id {
                continue;
            }
            if sequence <= manifest.flushed_sequence {
                continue;
            }
            for row in rows {
                let row = adapt_recovered_row(&schema, &columns, &row)?;
                memtable.apply(&row);
            }
        }
        if truncate_wal_on_flush
            && recovered_batches
            && recovery_last_sequence <= manifest.flushed_sequence
        {
            wal.reset()?;
        }
        if schema_upgrade {
            manifest.generation = manifest
                .generation
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?;
            manifest.epoch = manifest
                .epoch
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?;
            manifest.schema_version = schema.version();
            manifest.schema_fingerprint = segment::schema_fingerprint(&schema);
            manifest::publish(&directory, &manifest)?;
        }
        let next_append_row_id =
            find_next_append_row_id(&directory, &manifest, &schema, &memtable)?;
        let manifest = Arc::new(manifest);

        Ok(Self {
            _writer_lock: writer_lock,
            directory,
            schema,
            options,
            wal,
            memtable,
            last_sequence: recovery_last_sequence.max(manifest.flushed_sequence),
            manifest,
            retired: Vec::new(),
            next_append_row_id,
            table_id,
            truncate_wal_on_flush,
            commit_version,
            background: None,
            last_background_error: None,
        })
    }

    /// The highest committed local-transaction version.
    #[must_use]
    pub const fn commit_version(&self) -> u64 {
        self.commit_version
    }

    /// Durably commits one local transaction: the row batch and a commit
    /// record reach the log, one fsync makes both durable, and only then
    /// do the rows become visible. Rows are stamped with the assigned
    /// commit version. Returns that version.
    ///
    /// # Errors
    ///
    /// Returns an error on a non-transactional store, on validation
    /// failure, or when WAL I/O fails; a failed commit leaves nothing
    /// visible.
    pub fn commit(&mut self, rows: Vec<StoredRow>) -> Result<u64, StoreError> {
        if !self.options.transactional {
            return Err(StoreError::FormatLimit(
                "commit requires a transactional store".into(),
            ));
        }
        let version = self
            .commit_version
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        let rows: Vec<StoredRow> = rows
            .into_iter()
            .map(|row| {
                StoredRow::new(
                    row.key().clone(),
                    row.values().to_vec(),
                    version,
                    row.is_deleted(),
                )
            })
            .collect();
        for row in &rows {
            self.schema.validate_row(row)?;
        }
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        if !rows.is_empty() {
            self.wal
                .append(sequence, self.table_id, &self.schema, &rows)?;
        }
        let commit_sequence = sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        self.wal.append_commit(commit_sequence, version)?;
        self.wal.sync_force()?;
        // Durable: apply and publish.
        for row in &rows {
            self.memtable.apply(row);
        }
        self.last_sequence = commit_sequence;
        self.commit_version = version;
        if self.memtable.estimated_bytes() >= self.options.memtable_bytes {
            self.flush()?;
            self.advance_compaction()?;
            self.reclaim_obsolete_segments()?;
        }
        Ok(version)
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
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        self.ingest_at_sequence_with_append_policy(sequence, rows, AppendKeyPolicy::Generate)
    }

    /// Validates and durably orders one polling-scan batch, dropping rows
    /// whose latest visible version already holds identical content before
    /// they reach the WAL (GOAL.md §5.1 no-op suppression). Polling re-reads
    /// the same rows every cycle; without suppression each cycle re-ingests
    /// unchanged data as new versions and storage balloons between
    /// compactions. Steady-state polling storage must match CDC's.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, the point lookups, encoding, or
    /// WAL I/O fails.
    pub fn ingest_scan(&mut self, rows: Vec<StoredRow>) -> Result<IngestOutcome, StoreError> {
        if self.schema.key_mode() == KeyMode::AppendRowId {
            // Generated keys never collide with stored rows, so there is
            // nothing to suppress against.
            return self.ingest(rows);
        }
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if !self.scan_row_is_noop(&row)? {
                kept.push(row);
            }
        }
        self.ingest(kept)
    }

    /// Whether a scan row's content matches its key's latest visible
    /// version: same deletion state and identical values. Non-matching and
    /// unknown keys must be ingested.
    fn scan_row_is_noop(&self, row: &StoredRow) -> Result<bool, StoreError> {
        // The memtable always holds the newest version of a key when it
        // holds the key at all.
        if let Some(current) = self.memtable.snapshot().get(row.key()) {
            return Ok(current.is_deleted() == row.is_deleted() && current.values() == row.values());
        }
        let scan_memory = AtomicUsize::new(0);
        let budget = segment::ScanMemoryBudget::new(&scan_memory, usize::MAX);
        let mut best: Option<(u64, bool, usize, usize)> = None;
        for (segment_index, meta) in self.manifest.segments.iter().enumerate() {
            if row.key() < &meta.min_key || row.key() > &meta.max_key {
                continue;
            }
            if !segment::might_contain_key(&self.directory, meta, &self.schema, row.key())? {
                continue;
            }
            let headers = segment::read_row_headers_range(
                &self.directory,
                meta,
                &self.schema,
                row.key(),
                row.key(),
                &budget,
            )?;
            for header in headers.rows {
                if best
                    .as_ref()
                    .is_none_or(|(version, ..)| header.version >= *version)
                {
                    best = Some((
                        header.version,
                        header.deleted,
                        segment_index,
                        header.physical_index,
                    ));
                }
            }
        }
        let Some((_, deleted, segment_index, row_index)) = best else {
            return Ok(false);
        };
        if deleted != row.is_deleted() {
            return Ok(false);
        }
        if deleted {
            // Both sides are tombstones: re-ingesting one is a no-op.
            return Ok(true);
        }
        let projection = (0..self.schema.columns().len()).collect::<Vec<_>>();
        let fetch = segment::read_projected_rows(
            &self.directory,
            &self.manifest.segments[segment_index],
            &self.schema,
            &projection,
            &[row_index],
            &budget,
        )?;
        Ok(fetch.columns.len() == row.values().len()
            && fetch
                .columns
                .iter()
                .zip(row.values())
                .all(|(column, value)| column.first() == Some(value)))
    }

    /// Validates and durably orders one CDC batch.
    ///
    /// In append-row-ID mode, the caller-provided unsigned key is preserved so
    /// replay of the same deterministic source version remains idempotent.
    /// Snapshot and ordinary ingest continue to allocate local row IDs.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid rows, append keys, sequence overflow, or
    /// durable storage failure.
    pub fn ingest_cdc(&mut self, rows: Vec<StoredRow>) -> Result<IngestOutcome, StoreError> {
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        self.ingest_at_sequence_with_append_policy(sequence, rows, AppendKeyPolicy::Preserve)
    }

    pub(crate) fn ingest_at_sequence(
        &mut self,
        sequence: u64,
        rows: Vec<StoredRow>,
    ) -> Result<IngestOutcome, StoreError> {
        self.ingest_at_sequence_with_append_policy(sequence, rows, AppendKeyPolicy::Generate)
    }

    fn ingest_at_sequence_with_append_policy(
        &mut self,
        sequence: u64,
        mut rows: Vec<StoredRow>,
        append_key_policy: AppendKeyPolicy,
    ) -> Result<IngestOutcome, StoreError> {
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
        if self.schema.key_mode() == KeyMode::AppendRowId {
            match append_key_policy {
                AppendKeyPolicy::Generate => {
                    for row in &mut rows {
                        let row_id = self.next_append_row_id;
                        self.next_append_row_id = self
                            .next_append_row_id
                            .checked_add(1)
                            .ok_or(StoreError::SequenceOverflow)?;
                        let storage_key = PrimaryKey::new(vec![KeyPart::UInt64(row_id)])?;
                        *row = StoredRow::new(
                            storage_key,
                            row.values().to_vec(),
                            row.version(),
                            row.is_deleted(),
                        );
                    }
                }
                AppendKeyPolicy::Preserve => {
                    for row in &rows {
                        let [KeyPart::UInt64(row_id)] = row.key().parts() else {
                            return Err(StoreError::FormatLimit(
                                "CDC append key must contain one UInt64 component".to_owned(),
                            ));
                        };
                        if *row_id == 0 {
                            return Err(StoreError::FormatLimit(
                                "CDC append key must be non-zero".to_owned(),
                            ));
                        }
                        self.next_append_row_id = self
                            .next_append_row_id
                            .max(row_id.checked_add(1).ok_or(StoreError::SequenceOverflow)?);
                    }
                }
            }
        }

        if sequence <= self.last_sequence {
            return Err(StoreError::FormatLimit(format!(
                "WAL sequence {sequence} must follow {}",
                self.last_sequence
            )));
        }
        self.wal
            .append(sequence, self.table_id, &self.schema, &rows)?;

        let accepted_rows = rows.len();
        let visible_rows = rows
            .into_iter()
            .filter(|row| self.memtable.apply(row))
            .count();
        self.last_sequence = sequence;
        let should_flush = self.memtable.estimated_bytes() >= self.options.memtable_bytes;
        if should_flush {
            self.flush()?;
            self.advance_compaction()?;
            self.reclaim_obsolete_segments()?;
        }

        Ok(IngestOutcome {
            sequence,
            accepted_rows,
            visible_rows,
            should_flush,
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

    /// Publishes a compatible metadata-only schema evolution.
    ///
    /// Existing rows are flushed first. Segment readers then project columns
    /// by stable ID: dropped columns disappear, while newly added nullable
    /// columns read as `NULL` in older segments.
    ///
    /// # Errors
    ///
    /// Returns an error when the version does not advance, physical key mode
    /// changes, an old segment is incompatible, or durable publication fails.
    pub fn evolve_schema(&mut self, schema: TableSchema) -> Result<(), StoreError> {
        if schema.version() <= self.schema.version() {
            return Err(StoreError::IncompatibleSchema(format!(
                "schema version {} must advance beyond {}",
                schema.version(),
                self.schema.version()
            )));
        }
        if schema.key_mode() != self.schema.key_mode() {
            return Err(StoreError::IncompatibleSchema(
                "physical key mode changed".to_owned(),
            ));
        }
        self.flush()?;
        for segment in &self.manifest.segments {
            segment::read(&self.directory, segment, &schema)?;
        }
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.schema_version = schema.version();
        next_manifest.schema_fingerprint = segment::schema_fingerprint(&schema);
        manifest::publish(&self.directory, &next_manifest)?;
        self.schema = schema;
        self.manifest = Arc::new(next_manifest);
        Ok(())
    }

    /// Publishes an empty table generation before a full resnapshot.
    ///
    /// Existing reader snapshots retain their old immutable segments until
    /// they are released. The WAL and mutable row state are discarded because
    /// the caller has already marked the source as requiring a full rebuild.
    ///
    /// # Errors
    ///
    /// Returns an error when the WAL cannot be reset, the empty manifest
    /// cannot be published, or obsolete segments cannot be reclaimed.
    pub fn reset_for_resnapshot(&mut self) -> Result<(), StoreError> {
        self.wal.reset()?;
        let mut next_manifest = Manifest::empty(&self.schema);
        next_manifest.generation = self
            .manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = self
            .manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.next_segment_id = self.manifest.next_segment_id;
        manifest::publish(&self.directory, &next_manifest)?;

        self.memtable.clear();
        self.last_sequence = 0;
        self.next_append_row_id = 1;
        let previous = std::mem::replace(&mut self.manifest, Arc::new(next_manifest));
        let paths = previous
            .segments
            .iter()
            .map(|segment| self.directory.join(&segment.file_name))
            .collect();
        self.retired.push(RetiredGeneration {
            readers: Arc::downgrade(&previous),
            paths,
        });
        self.reclaim_obsolete_segments()?;
        Ok(())
    }

    /// Publishes one initial-snapshot chunk directly as an immutable segment.
    ///
    /// This path bypasses both the WAL and memtable. It is intended only for
    /// source rows that can be replayed from a durable snapshot-chunk journal;
    /// normal CDC and polling writes must continue to use [`Self::ingest`].
    /// Rows are sorted here, and duplicate primary/unique keys within the
    /// chunk collapse to the greatest version before publication.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid rows, pending memtable data, or a failed
    /// checksummed segment/manifest publication.
    pub fn bulk_ingest_snapshot(
        &mut self,
        mut rows: Vec<StoredRow>,
    ) -> Result<BulkIngestOutcome, StoreError> {
        if self.has_pending_rows() {
            return Err(StoreError::FormatLimit(
                "direct snapshot ingest requires an empty memtable".to_owned(),
            ));
        }
        for row in &rows {
            self.schema.validate_row(row)?;
            if row.is_deleted() {
                return Err(StoreError::FormatLimit(
                    "direct snapshot ingest cannot contain tombstones".to_owned(),
                ));
            }
        }
        if rows.is_empty() {
            return Ok(BulkIngestOutcome {
                row_count: 0,
                segment_path: None,
            });
        }
        rows.sort_by(|left, right| {
            left.key()
                .cmp(right.key())
                .then_with(|| left.version().cmp(&right.version()))
        });
        if self.schema.key_mode() == KeyMode::AppendRowId {
            for row in &rows {
                if let [KeyPart::UInt64(row_id)] = row.key().parts() {
                    self.next_append_row_id = self.next_append_row_id.max(row_id.saturating_add(1));
                }
            }
        } else {
            let mut deduplicated: Vec<StoredRow> = Vec::with_capacity(rows.len());
            for row in rows {
                if let Some(previous) = deduplicated.last_mut()
                    && previous.key() == row.key()
                {
                    *previous = row;
                } else {
                    deduplicated.push(row);
                }
            }
            rows = deduplicated;
        }

        let segment = segment::write(
            &self.directory,
            self.manifest.next_segment_id,
            &self.schema,
            &rows,
            self.options.block_rows,
            segment::Compression::AdaptiveLz4,
            true,
        )?;
        let segment_path = self.directory.join(&segment.file_name);
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.next_segment_id = next_manifest
            .next_segment_id
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.segments.push(segment);
        manifest::publish(&self.directory, &next_manifest)?;
        self.manifest = Arc::new(next_manifest);
        Ok(BulkIngestOutcome {
            row_count: rows.len(),
            segment_path: Some(segment_path),
        })
    }

    pub(crate) fn has_pending_rows(&self) -> bool {
        !self.memtable.snapshot().is_empty()
    }

    pub(crate) fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Publishes the current memtable as a checksummed immutable PTSEG file.
    ///
    /// The segment is synchronized before an atomic manifest swap. Only after
    /// that durable publication does Pintail clear memory and truncate the
    /// flushed WAL. Recovery therefore sees either the old WAL state or the
    /// new manifest state.
    ///
    /// # Errors
    ///
    /// Returns an error when segment encoding or durable publication fails.
    pub fn flush(&mut self) -> Result<FlushOutcome, StoreError> {
        let rows = self
            .memtable
            .snapshot()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(FlushOutcome {
                row_count: 0,
                segment_path: None,
            });
        }

        // The memtable is a map, so a flush provably holds one row per key.
        // `unique_keys` also promises the segment carries no deletes, because
        // the columnar direct path it unlocks applies no tombstone filter — so
        // a flush that carries even one tombstone stays off the direct path.
        let unique_keys = rows.iter().all(|row| !row.is_deleted());
        let segment = segment::write(
            &self.directory,
            self.manifest.next_segment_id,
            &self.schema,
            &rows,
            self.options.block_rows,
            segment::Compression::AdaptiveLz4,
            unique_keys,
        )?;
        let segment_path = self.directory.join(&segment.file_name);
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.flushed_sequence = self.last_sequence;
        next_manifest.committed_version = self.commit_version;
        next_manifest.next_segment_id = next_manifest
            .next_segment_id
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.segments.push(segment);
        manifest::publish(&self.directory, &next_manifest)?;

        self.manifest = Arc::new(next_manifest);
        self.memtable.clear();
        if self.truncate_wal_on_flush {
            self.wal.reset()?;
        }
        Ok(FlushOutcome {
            row_count: rows.len(),
            segment_path: Some(segment_path),
        })
    }

    /// Calculates the next size-tier compaction candidate and its byte debt.
    ///
    /// # Errors
    ///
    /// Returns an error when segment metadata or checksummed key ranges cannot
    /// be read.
    pub fn compaction_status(&self) -> Result<CompactionStatus, StoreError> {
        let plan = self.compaction_plan()?;
        Ok(CompactionStatus {
            segment_count: self.manifest.segments.len(),
            eligible_segments: plan.as_ref().map_or(0, |plan| plan.indices.len()),
            debt_bytes: plan.map_or(0, |plan| plan.debt_bytes),
        })
    }

    /// Returns memory, segment, and compaction-debt metric values.
    ///
    /// # Errors
    ///
    /// Returns an error when live segment sizes cannot be inspected.
    pub fn metrics(&self) -> Result<StorageMetrics, StoreError> {
        let compaction = self.compaction_status()?;
        Ok(StorageMetrics {
            memtable_bytes: self.memtable.estimated_bytes(),
            segment_count: compaction.segment_count(),
            compaction_debt_bytes: compaction.debt_bytes(),
        })
    }

    /// Moves compaction forward without stalling ingest: publishes a
    /// finished background merge, spawns a new one when pressure calls for
    /// it, or falls back to the inline pass when backgrounding is off.
    fn advance_compaction(&mut self) -> Result<(), StoreError> {
        if !self.options.background_compaction {
            if self.manifest.segments.len() >= self.options.compaction_fan_in {
                self.compact()?;
            }
            return Ok(());
        }
        self.poll_background_merge()?;
        if self.background.is_none()
            && self.manifest.segments.len() >= self.options.compaction_fan_in
        {
            self.spawn_background_merge()?;
        }
        Ok(())
    }

    /// Publishes a background merge that has finished, if any. Cheap when
    /// the merge is still running.
    fn poll_background_merge(&mut self) -> Result<(), StoreError> {
        let Some(merge) = &self.background else {
            return Ok(());
        };
        let outcome = match merge.receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.background = None;
                self.last_background_error =
                    Some("background merge thread exited without a result".to_owned());
                return Ok(());
            }
        };
        let Some(merge) = self.background.take() else {
            return Ok(());
        };
        let outputs = match outcome {
            Ok(outputs) => outputs,
            Err(error) => {
                // The merge is optional: unmerged segments still resolve by
                // streaming merge-on-read, and orphan chunk files are swept
                // at the next open. Record and move on.
                self.last_background_error = Some(error.to_string());
                return Ok(());
            }
        };
        let inputs = merge
            .input_files
            .iter()
            .collect::<std::collections::HashSet<_>>();
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        let retired_paths = next_manifest
            .segments
            .iter()
            .filter(|meta| inputs.contains(&meta.file_name))
            .map(|meta| self.directory.join(&meta.file_name))
            .collect::<Vec<_>>();
        // Publication removes exactly the merged inputs BY NAME: flushes
        // during the merge appended segments this filter must keep.
        next_manifest
            .segments
            .retain(|meta| !inputs.contains(&meta.file_name));
        next_manifest.segments.extend(outputs);
        manifest::publish(&self.directory, &next_manifest)?;
        let previous = std::mem::replace(&mut self.manifest, Arc::new(next_manifest));
        self.retired.push(RetiredGeneration {
            readers: Arc::downgrade(&previous),
            paths: retired_paths,
        });
        Ok(())
    }

    /// Starts a size-tier merge on a background thread. The thread reads
    /// immutable inputs and writes chunk files nothing references; segment
    /// IDs come from a range reserved here so concurrent flushes never
    /// collide with them.
    fn spawn_background_merge(&mut self) -> Result<(), StoreError> {
        const RESERVED_SEGMENT_IDS: u64 = 65_536;
        let Some(plan) = self.compaction_plan()? else {
            return Ok(());
        };
        if !self.merge_fits_on_disk(&plan)? {
            return Ok(());
        }
        let full_merge = plan.indices.len() == self.manifest.segments.len();
        let input_metas = plan
            .indices
            .iter()
            .map(|index| self.manifest.segments[*index].clone())
            .collect::<Vec<_>>();
        let input_files = input_metas
            .iter()
            .map(|meta| meta.file_name.clone())
            .collect::<Vec<_>>();
        // Reserve an ID range through a manifest publish, so the reservation
        // survives a restart mid-merge.
        let id_base = self.manifest.next_segment_id;
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.next_segment_id = next_manifest
            .next_segment_id
            .checked_add(RESERVED_SEGMENT_IDS)
            .ok_or(StoreError::SequenceOverflow)?;
        manifest::publish(&self.directory, &next_manifest)?;
        self.manifest = Arc::new(next_manifest);
        let directory = self.directory.clone();
        let schema = self.schema.clone();
        let options = self.options;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("pintail-compaction".to_owned())
            .spawn(move || {
                let result = run_background_merge(
                    &directory,
                    &schema,
                    options,
                    &input_metas,
                    full_merge,
                    id_base,
                );
                // A dropped receiver means the store is gone; the orphan
                // sweep at the next open cleans any chunks written.
                let _ = sender.send(result);
            })
            .map_err(|error| StoreError::io("spawn compaction thread", error))?;
        self.background = Some(BackgroundMerge {
            receiver,
            input_files,
        });
        Ok(())
    }

    /// Runs one bounded size-tier merge of similarly sized overlapping files.
    ///
    /// A merge that covers the complete manifest drops tombstones immediately
    /// and writes zstd at the coldest tier. Partial merges retain tombstones so
    /// they can still suppress older versions outside the selected set.
    ///
    /// # Errors
    ///
    /// Returns an error when input validation, output writing, or atomic
    /// manifest publication fails.
    #[allow(clippy::too_many_lines)]
    pub fn compact(&mut self) -> Result<CompactionOutcome, StoreError> {
        if self.background.is_some() {
            self.poll_background_merge()?;
            if self.background.is_some() {
                return Ok(CompactionOutcome {
                    input_segments: 0,
                    output_rows: 0,
                    output_path: None,
                    deferred: Some("a background merge is in flight"),
                });
            }
        }
        let Some(plan) = self.compaction_plan()? else {
            return Ok(CompactionOutcome {
                input_segments: 0,
                output_rows: 0,
                output_path: None,
                deferred: None,
            });
        };
        if !self.merge_fits_on_disk(&plan)? {
            // Correctness does not depend on merging: the unmerged segments
            // still resolve through streaming merge-on-read. Filling the
            // volume mid-write would put that at risk, so defer instead.
            return Ok(CompactionOutcome {
                input_segments: 0,
                output_rows: 0,
                output_path: None,
                deferred: Some("free disk space cannot cover the planned merge"),
            });
        }
        let full_merge = plan.indices.len() == self.manifest.segments.len();
        let mut streams = Vec::with_capacity(plan.indices.len());
        for index in &plan.indices {
            let meta = &self.manifest.segments[*index];
            streams.push(segment::SegmentRowStream::open(
                &self.directory,
                meta,
                &self.schema,
            )?);
        }
        let mut heads = streams
            .iter_mut()
            .map(segment::SegmentRowStream::next_row)
            .collect::<Result<Vec<_>, _>>()?;

        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        let selected = plan
            .indices
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let retired_paths = next_manifest
            .segments
            .iter()
            .enumerate()
            .filter(|(index, _)| selected.contains(index))
            .map(|(_, meta)| self.directory.join(&meta.file_name))
            .collect::<Vec<_>>();
        next_manifest.segments = next_manifest
            .segments
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !selected.contains(index))
            .map(|(_, meta)| meta)
            .collect();

        let compression = if full_merge {
            segment::Compression::Zstd
        } else {
            segment::Compression::AdaptiveLz4
        };
        let output_row_limit =
            usize::try_from(self.options.max_compaction_rows).unwrap_or(usize::MAX);
        let mut rows = Vec::with_capacity(output_row_limit.min(64 * 1024));
        let mut output_rows = 0_usize;
        let mut buffered_bytes = 0_usize;
        let mut output_path = None;
        while let Some(minimum) = heads
            .iter()
            .filter_map(|row| row.as_ref().map(StoredRow::key))
            .min()
            .cloned()
        {
            let mut winner = None;
            for (stream, head) in streams.iter_mut().zip(&mut heads) {
                while head.as_ref().is_some_and(|row| row.key() == &minimum) {
                    let Some(candidate) = head.take() else {
                        return Err(StoreError::FormatLimit(
                            "matching compaction head disappeared".into(),
                        ));
                    };
                    if winner
                        .as_ref()
                        .is_none_or(|current: &StoredRow| candidate.version() >= current.version())
                    {
                        winner = Some(candidate);
                    }
                    *head = stream.next_row()?;
                }
            }
            let Some(winner) = winner else {
                return Err(StoreError::FormatLimit(
                    "compaction minimum has no winning row".into(),
                ));
            };
            if !full_merge || !winner.is_deleted() {
                buffered_bytes = buffered_bytes.saturating_add(winner.estimated_bytes());
                rows.push(winner);
                output_rows = output_rows.saturating_add(1);
            }
            if rows.len() >= output_row_limit
                || buffered_bytes >= self.options.max_compaction_output_bytes
            {
                let path = write_compaction_chunk(
                    &self.directory,
                    &self.schema,
                    self.options.block_rows,
                    compression,
                    full_merge,
                    &mut next_manifest,
                    &rows,
                )?;
                output_path.get_or_insert(path);
                rows.clear();
                buffered_bytes = 0;
            }
        }
        if !rows.is_empty() {
            let path = write_compaction_chunk(
                &self.directory,
                &self.schema,
                self.options.block_rows,
                compression,
                full_merge,
                &mut next_manifest,
                &rows,
            )?;
            output_path.get_or_insert(path);
        }
        manifest::publish(&self.directory, &next_manifest)?;

        let previous = std::mem::replace(&mut self.manifest, Arc::new(next_manifest));
        self.retired.push(RetiredGeneration {
            readers: Arc::downgrade(&previous),
            paths: retired_paths,
        });
        Ok(CompactionOutcome {
            input_segments: plan.indices.len(),
            output_rows,
            output_path,
            deferred: None,
        })
    }

    /// Whether the volume can hold the merge's output alongside its inputs.
    fn merge_fits_on_disk(&self, plan: &CompactionPlan) -> Result<bool, StoreError> {
        let available = fs2::available_space(&self.directory)
            .map_err(|error| StoreError::io("inspect free space for compaction", error))?;
        Ok(available
            >= plan
                .debt_bytes
                .saturating_add(self.options.compaction_disk_reserve_bytes))
    }

    /// Deletes obsolete segments after every snapshot that pins them releases.
    ///
    /// # Errors
    ///
    /// Returns an error when an eligible obsolete file cannot be removed.
    pub fn reclaim_obsolete_segments(&mut self) -> Result<usize, StoreError> {
        let mut reclaimed = 0;
        let mut retained = Vec::new();
        for generation in self.retired.drain(..) {
            if generation.readers.upgrade().is_some() {
                retained.push(generation);
                continue;
            }
            for path in generation.paths {
                match std::fs::remove_file(&path) {
                    Ok(()) => reclaimed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(StoreError::io(
                            format!("remove obsolete segment {}", path.display()),
                            error,
                        ));
                    }
                }
            }
        }
        self.retired = retained;
        if reclaimed > 0 {
            segment::sync_directory(&self.directory)?;
        }
        Ok(reclaimed)
    }

    /// Pins an immutable view of rows currently visible to readers.
    #[must_use]
    pub fn snapshot(&self) -> TableSnapshot {
        register_pinned_manifest(&self.directory, &self.manifest);
        TableSnapshot {
            memtable: self.memtable.snapshot(),
            manifest: Arc::clone(&self.manifest),
            directory: self.directory.clone(),
            schema: self.schema.clone(),
        }
    }

    /// Returns the table directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the logical schema enforced by this store handle.
    #[must_use]
    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn compaction_plan(&self) -> Result<Option<CompactionPlan>, StoreError> {
        if self.manifest.segments.len() < self.options.compaction_fan_in {
            return Ok(None);
        }
        let mut candidates = Vec::with_capacity(self.manifest.segments.len());
        for (index, meta) in self.manifest.segments.iter().enumerate() {
            let size = std::fs::metadata(self.directory.join(&meta.file_name))
                .map_err(|error| StoreError::io("inspect segment for compaction", error))?
                .len();
            candidates.push(CompactionCandidate {
                index,
                size,
                row_count: meta.row_count,
                minimum: meta.min_key.clone(),
                maximum: meta.max_key.clone(),
            });
        }
        candidates.sort_by_key(|candidate| (candidate.size, candidate.index));
        for window in candidates.windows(self.options.compaction_fan_in) {
            let selected = window.iter().collect::<Vec<_>>();
            if !self.admits_window(&selected) || !ranges_overlap(&selected) {
                continue;
            }
            return Ok(Some(plan_for(&selected)));
        }
        // Nothing overlaps, so no merge would collapse a row version. Merging
        // still pays for itself once the manifest holds many files: every scan
        // opens and prunes each one. Take neighbours in key order so the
        // output stays disjoint from everything it did not consume, which is
        // what keeps SMA value pruning eligible.
        if self.manifest.segments.len() < self.options.compaction_file_pressure {
            return Ok(None);
        }
        candidates.sort_by(|left, right| left.minimum.cmp(&right.minimum));
        for window in candidates.windows(self.options.compaction_fan_in) {
            let selected = window.iter().collect::<Vec<_>>();
            if self.admits_window(&selected) {
                return Ok(Some(plan_for(&selected)));
            }
        }
        Ok(None)
    }

    /// Reports whether one candidate window fits the configured size tier and
    /// per-pass row budget.
    fn admits_window(&self, window: &[&CompactionCandidate]) -> bool {
        let sizes = window.iter().map(|candidate| candidate.size);
        let (Some(smallest), Some(largest)) = (sizes.clone().min(), sizes.max()) else {
            return false;
        };
        let row_count = window
            .iter()
            .map(|candidate| candidate.row_count)
            .sum::<u64>();
        row_count <= self.options.max_compaction_input_rows
            && largest <= smallest.saturating_mul(SIZE_TIER_RATIO)
    }
}

fn plan_for(window: &[&CompactionCandidate]) -> CompactionPlan {
    CompactionPlan {
        indices: window.iter().map(|candidate| candidate.index).collect(),
        debt_bytes: window.iter().map(|candidate| candidate.size).sum(),
    }
}

struct CompactionCandidate {
    index: usize,
    size: u64,
    row_count: u64,
    minimum: PrimaryKey,
    maximum: PrimaryKey,
}

struct CompactionPlan {
    indices: Vec<usize>,
    debt_bytes: u64,
}

fn ranges_overlap(candidates: &[&CompactionCandidate]) -> bool {
    let mut by_key = candidates.to_vec();
    by_key.sort_by(|left, right| left.minimum.cmp(&right.minimum));
    let Some(first) = by_key.first() else {
        return false;
    };
    let mut maximum = first.maximum.clone();
    for candidate in by_key.into_iter().skip(1) {
        if candidate.minimum > maximum {
            return false;
        }
        if candidate.maximum > maximum {
            maximum = candidate.maximum.clone();
        }
    }
    true
}

struct ProjectedCandidate {
    key: PrimaryKey,
    version: u64,
    deleted: bool,
    source: ProjectedSource,
}

impl ProjectedCandidate {
    fn estimated_bytes(&self) -> usize {
        Self::estimated_bytes_for_key(&self.key)
    }

    fn estimated_bytes_for_key(key: &PrimaryKey) -> usize {
        size_of::<Self>()
            + size_of::<PrimaryKey>()
            + 2 * std::mem::size_of_val(key.parts())
            + 2 * key.heap_bytes()
            + 4 * size_of::<usize>()
    }
}

#[derive(Clone, Copy)]
enum ProjectedSource {
    Segment {
        segment_index: usize,
        row_index: usize,
    },
    Memtable,
}

fn apply_projected_latest(
    rows: &mut BTreeMap<PrimaryKey, ProjectedCandidate>,
    row: ProjectedCandidate,
) {
    if rows
        .get(&row.key)
        .is_none_or(|current| row.version >= current.version)
    {
        rows.insert(row.key.clone(), row);
    }
}

fn apply_latest(rows: &mut BTreeMap<PrimaryKey, StoredRow>, row: StoredRow) {
    if rows
        .get(row.key())
        .is_none_or(|current| row.version() >= current.version())
    {
        rows.insert(row.key().clone(), row);
    }
}

/// The background thread's merge: same winner-per-key loop as the inline
/// pass, writing chunks from a reserved segment-ID range and returning
/// their metadata for publication on the store's thread.
fn run_background_merge(
    directory: &Path,
    schema: &TableSchema,
    options: StoreOptions,
    input_metas: &[segment::SegmentMeta],
    full_merge: bool,
    id_base: u64,
) -> Result<Vec<segment::SegmentMeta>, StoreError> {
    let mut streams = Vec::with_capacity(input_metas.len());
    for meta in input_metas {
        streams.push(segment::SegmentRowStream::open(directory, meta, schema)?);
    }
    let mut heads = streams
        .iter_mut()
        .map(segment::SegmentRowStream::next_row)
        .collect::<Result<Vec<_>, _>>()?;
    let compression = if full_merge {
        segment::Compression::Zstd
    } else {
        segment::Compression::AdaptiveLz4
    };
    let output_row_limit = usize::try_from(options.max_compaction_rows).unwrap_or(usize::MAX);
    let mut rows = Vec::with_capacity(output_row_limit.min(64 * 1024));
    let mut buffered_bytes = 0_usize;
    let mut next_id = id_base;
    let mut outputs = Vec::new();
    let mut write_chunk = |rows: &[StoredRow], next_id: &mut u64| -> Result<(), StoreError> {
        let output = segment::write(
            directory,
            *next_id,
            schema,
            rows,
            options.block_rows,
            compression,
            full_merge,
        )?;
        *next_id = next_id.checked_add(1).ok_or(StoreError::SequenceOverflow)?;
        outputs.push(output);
        Ok(())
    };
    while let Some(minimum) = heads
        .iter()
        .filter_map(|row| row.as_ref().map(StoredRow::key))
        .min()
        .cloned()
    {
        let mut winner = None;
        for (stream, head) in streams.iter_mut().zip(&mut heads) {
            while head.as_ref().is_some_and(|row| row.key() == &minimum) {
                let Some(candidate) = head.take() else {
                    return Err(StoreError::FormatLimit(
                        "matching compaction head disappeared".into(),
                    ));
                };
                if winner
                    .as_ref()
                    .is_none_or(|current: &StoredRow| candidate.version() >= current.version())
                {
                    winner = Some(candidate);
                }
                *head = stream.next_row()?;
            }
        }
        let Some(winner) = winner else {
            return Err(StoreError::FormatLimit(
                "compaction minimum has no winning row".into(),
            ));
        };
        if !full_merge || !winner.is_deleted() {
            buffered_bytes = buffered_bytes.saturating_add(winner.estimated_bytes());
            rows.push(winner);
        }
        if rows.len() >= output_row_limit || buffered_bytes >= options.max_compaction_output_bytes {
            write_chunk(&rows, &mut next_id)?;
            rows.clear();
            buffered_bytes = 0;
        }
    }
    if !rows.is_empty() {
        write_chunk(&rows, &mut next_id)?;
    }
    Ok(outputs)
}

fn write_compaction_chunk(
    directory: &Path,
    schema: &TableSchema,
    block_rows: usize,
    compression: segment::Compression,
    unique_keys: bool,
    manifest: &mut Manifest,
    rows: &[StoredRow],
) -> Result<PathBuf, StoreError> {
    let output = segment::write(
        directory,
        manifest.next_segment_id,
        schema,
        rows,
        block_rows,
        compression,
        unique_keys,
    )?;
    manifest.next_segment_id = manifest
        .next_segment_id
        .checked_add(1)
        .ok_or(StoreError::SequenceOverflow)?;
    let path = directory.join(&output.file_name);
    manifest.segments.push(output);
    Ok(path)
}

fn validate_store_options(options: StoreOptions) -> Result<(), StoreError> {
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
    if options.max_compaction_rows == 0 || options.max_compaction_input_rows == 0 {
        return Err(StoreError::FormatLimit(
            "compaction row bounds must be non-zero".into(),
        ));
    }
    if options.max_compaction_output_bytes == 0 {
        return Err(StoreError::FormatLimit(
            "compaction output byte bound must be non-zero".into(),
        ));
    }
    Ok(())
}

/// Acquires an exclusive writer flock, absorbing transient `WouldBlock`s.
///
/// A concurrently spawned child process briefly keeps inherited copies of
/// every open file description alive, so a lock the previous owner just
/// released can still read as held for a few milliseconds (reproduced at a
/// 3.8% rate under a spawn loop on macOS; every hold cleared within 5ms).
/// A short bounded retry separates that from a genuinely busy writer.
pub(crate) fn lock_writer(lock: &File, context: &'static str) -> Result<(), StoreError> {
    const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);
    const RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(2);
    let start = std::time::Instant::now();
    loop {
        match FileExt::try_lock_exclusive(lock) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= RETRY_BUDGET {
                    return Err(StoreError::WriterBusy);
                }
                std::thread::sleep(RETRY_STEP);
            }
            Err(error) => return Err(StoreError::io(context, error)),
        }
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

fn adapt_recovered_row(
    schema: &TableSchema,
    wal_columns: &[WalColumn],
    row: &StoredRow,
) -> Result<StoredRow, StoreError> {
    if row.values().len() != wal_columns.len() {
        return Err(StoreError::IncompatibleSchema(format!(
            "WAL row has {} values for {} recorded columns",
            row.values().len(),
            wal_columns.len()
        )));
    }
    let mut values = Vec::with_capacity(schema.columns().len());
    for column in schema.columns() {
        if let Some((index, wal_column)) = wal_columns
            .iter()
            .enumerate()
            .find(|(_, wal_column)| wal_column.id == column.id())
        {
            if wal_column.data_type != column.data_type().storage_type() {
                return Err(StoreError::IncompatibleSchema(format!(
                    "column {} ({}) changed physical type",
                    column.name(),
                    column.id()
                )));
            }
            values.push(row.values()[index].clone());
        } else if column.is_nullable() {
            values.push(pintail_types::Value::Null);
        } else {
            return Err(StoreError::IncompatibleSchema(format!(
                "required column {} ({}) is absent from an unflushed WAL row",
                column.name(),
                column.id()
            )));
        }
    }
    let adapted = StoredRow::new(row.key().clone(), values, row.version(), row.is_deleted());
    schema.validate_row(&adapted)?;
    Ok(adapted)
}

fn remove_orphan_segments(directory: &Path, manifest: &Manifest) -> Result<(), StoreError> {
    let pinned = pinned_manifests(directory);
    let mut live = manifest
        .segments
        .iter()
        .map(|segment| segment.file_name.as_str())
        .collect::<std::collections::HashSet<_>>();
    for pinned_manifest in &pinned {
        live.extend(
            pinned_manifest
                .segments
                .iter()
                .map(|segment| segment.file_name.as_str()),
        );
    }
    let mut removed = false;
    for entry in std::fs::read_dir(directory)
        .map_err(|error| StoreError::io("list table directory", error))?
    {
        let entry = entry.map_err(|error| StoreError::io("read table directory entry", error))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let orphan_segment = path
            .extension()
            .is_some_and(|extension| extension == "ptseg")
            && !live.contains(file_name);
        let interrupted_segment_write =
            file_name.starts_with(".segment-") && file_name.ends_with(".ptseg.tmp");
        if orphan_segment || interrupted_segment_write {
            std::fs::remove_file(&path).map_err(|error| {
                StoreError::io(format!("remove orphan segment {}", path.display()), error)
            })?;
            removed = true;
        }
    }
    if removed {
        segment::sync_directory(directory)?;
    }
    Ok(())
}

type SnapshotRegistry = BTreeMap<PathBuf, Vec<Weak<Manifest>>>;

fn snapshot_registry() -> &'static Mutex<SnapshotRegistry> {
    static REGISTRY: OnceLock<Mutex<SnapshotRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn register_pinned_manifest(directory: &Path, manifest: &Arc<Manifest>) {
    let mut registry = snapshot_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let manifests = registry.entry(directory.to_path_buf()).or_default();
    manifests.retain(|pinned| pinned.strong_count() > 0);
    if !manifests
        .iter()
        .any(|pinned| pinned.ptr_eq(&Arc::downgrade(manifest)))
    {
        manifests.push(Arc::downgrade(manifest));
    }
}

fn pinned_manifests(directory: &Path) -> Vec<Arc<Manifest>> {
    let mut registry = snapshot_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(manifests) = registry.get_mut(directory) else {
        return Vec::new();
    };
    let pinned = manifests
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    manifests.retain(|manifest| manifest.strong_count() > 0);
    if manifests.is_empty() {
        registry.remove(directory);
    }
    pinned
}

fn find_next_append_row_id(
    directory: &Path,
    manifest: &Manifest,
    schema: &TableSchema,
    memtable: &Memtable,
) -> Result<u64, StoreError> {
    if schema.key_mode() != KeyMode::AppendRowId {
        return Ok(1);
    }
    let mut maximum = 0;
    for row in memtable.snapshot().values() {
        maximum = maximum.max(append_row_id(row.key())?);
    }
    for meta in &manifest.segments {
        for row in segment::read(directory, meta, schema)? {
            maximum = maximum.max(append_row_id(row.key())?);
        }
    }
    maximum.checked_add(1).ok_or(StoreError::SequenceOverflow)
}

fn append_row_id(key: &PrimaryKey) -> Result<u64, StoreError> {
    match key.parts() {
        [KeyPart::UInt64(row_id)] => Ok(*row_id),
        _ => Err(StoreError::IncompatibleSchema(
            "append-rowid table contains a non-generated storage key".into(),
        )),
    }
}
