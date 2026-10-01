mod layer;
#[cfg(test)]
mod layer_tests;
#[cfg(test)]
mod lifecycle_tests;
mod scan;
pub(crate) mod side_index;
mod snapshot;
mod statistics;

pub(crate) use layer::{LayerIndexSlot, MemtableImage};
pub use scan::{
    ColumnValidity, DecodedColumn, PrewhereRanges, PrewhereSelect, ProjectedColumnChunk,
    ProjectedRow, ProjectedScan, ProjectedScanStream, ProjectedValueChunk, ScanStats, ValidityIter,
};
pub use side_index::{
    IndexKey, IndexLookup, IndexProbe, TextKeyFn, TextKeyer, override_side_index,
    side_index_cache_usage, side_index_enabled, side_index_note, side_index_totals,
    side_index_trace,
};
pub use snapshot::{BackupArtifacts, BackupSegment, GroupedFoldSpan, TableSnapshot};

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    ops::{Bound, RangeBounds},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::AtomicUsize},
};

use fs2::FileExt;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, StoredRow, TableSchema};

use crate::{
    StoreError,
    manifest::{self, Manifest},
    memtable::Memtable,
    publication::Publisher,
    segment,
    wal::{RecoveredBatch, Wal, WalColumn},
};

const WAL_FILE: &str = "table.wal";

#[derive(Clone, Copy)]
enum AppendKeyPolicy {
    Generate,
    Preserve,
}
pub(crate) const WRITER_LOCK_FILE: &str = ".writer.lock";
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
            //
            // Stays at the CPU count by default. e65 measured a doubled pool
            // winning under the CPU quota a typical container deployment
            // runs under; e88 reproduced no such gain on bare metal, only
            // added scheduling contention, so there is nothing here to
            // weigh against leaving it alone (docs/design/
            // production-hardening-todo.md, section H; experiments/
            // RESULTS.md e88). Deployments that want the container quota's
            // benefit can still opt in with the env var.
            //
            // A doubled pool did surface a wrong answer under `tests/e2e`
            // while this was measured, but it is not this default's to
            // avoid: the same check failed again with the pool back at the
            // CPU count, at a different row and value, so the width is not
            // the cause (G14, e90).
            let threads = std::env::var("PINTAIL_SCAN_THREADS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|threads| *threads > 0)
                .unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
                });
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                // Matches the main thread's 8 MiB. Left at rayon's default a
                // worker had a quarter of that, so how deep a recursion
                // could go depended on whether rayon ran the work on a
                // worker or inline on the caller - a difference that varies
                // between runs of the same query.
                .stack_size(8 * 1024 * 1024)
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

/// The sizes an operator may override for every table of the process:
/// `PINTAIL_MEMTABLE_KB` (memtable bytes that request a flush),
/// `PINTAIL_COMPACTION_INPUT_ROWS` and `PINTAIL_COMPACTION_OUTPUT_ROWS` (the
/// rows one merge reads and the rows one of its outputs holds). Read once.
/// Small values make a modest table walk through every flush and merge
/// shape, which is what a crash harness needs to reach them in seconds.
fn size_overrides() -> (Option<usize>, Option<u64>, Option<u64>) {
    static OVERRIDES: OnceLock<(Option<usize>, Option<u64>, Option<u64>)> = OnceLock::new();
    fn read(name: &str) -> Option<u64> {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
    }
    *OVERRIDES.get_or_init(|| {
        (
            read("PINTAIL_MEMTABLE_KB")
                .and_then(|kilobytes| usize::try_from(kilobytes.saturating_mul(1024)).ok()),
            read("PINTAIL_COMPACTION_INPUT_ROWS"),
            read("PINTAIL_COMPACTION_OUTPUT_ROWS"),
        )
    })
}

impl Default for StoreOptions {
    fn default() -> Self {
        let (memtable_bytes, input_rows, output_rows) = size_overrides();
        Self {
            memtable_bytes: memtable_bytes.unwrap_or(DEFAULT_MEMTABLE_BYTES),
            transactional: false,
            block_rows: DEFAULT_BLOCK_ROWS,
            wal_sync: WalSync::Checkpoint,
            compaction_fan_in: DEFAULT_COMPACTION_FAN_IN,
            max_compaction_input_rows: input_rows.unwrap_or(DEFAULT_MAX_COMPACTION_INPUT_ROWS),
            max_compaction_rows: output_rows.unwrap_or(DEFAULT_MAX_COMPACTION_ROWS),
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
    layer_index_bytes: usize,
    segment_count: usize,
    compaction_debt_bytes: u64,
}

impl StorageMetrics {
    /// Returns the current mutable-table byte estimate.
    #[must_use]
    pub fn memtable_bytes(self) -> usize {
        self.memtable_bytes
    }

    /// Returns the bytes of the key index scans keep for the newer segments
    /// layered over this table's bases: resolved by the first scan to need
    /// it, dropped with the manifest generation that names those segments.
    #[must_use]
    pub fn layer_index_bytes(self) -> usize {
        self.layer_index_bytes
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
/// Hands every open store an identity no other open in this process
/// shares.
///
/// A table's directory and manifest generation do not identify the table:
/// a directory can be reclaimed, and what replaces it starts from an empty
/// manifest and walks the same generations, so two tables can present the
/// same `(directory, generation)` pair. Anything caching a result against
/// that pair needs this alongside it.
pub(crate) static STORE_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub struct TableStore {
    /// This writer's claim on the table: it holds the table's writer lock,
    /// and hands it back to the process - or lets it go - when dropped.
    publication: Publisher,
    /// Distinguishes this open from any other, including an earlier table
    /// at the same path. Never persisted: a reopen is a new instance, and
    /// caches keyed on it correctly stop recognizing their old entries.
    instance: u64,
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
    /// Set by whoever must not wait for this table's merges to finish.
    merge_yield: Arc<std::sync::atomic::AtomicBool>,
}

/// One background size-tier merge in flight.
struct BackgroundMerge {
    worker: std::thread::JoinHandle<()>,
    receiver: std::sync::mpsc::Receiver<Result<Vec<segment::SegmentMeta>, StoreError>>,
    input_files: Vec<String>,
}

/// A table's log is cut back only after a manifest naming the flushed rows is
/// published, so a log that no longer starts at the first write proves a
/// manifest existed. Without it the open used to start an empty table, sweep
/// the flushed segments as orphans and serve the log's tail as the whole
/// table - losing every flushed row without an error. The caller refuses
/// instead, before anything is swept.
fn refuse_a_lost_manifest(
    directory: &Path,
    truncate_wal_on_flush: bool,
    recovery: &crate::wal::Recovery,
    table_id: u64,
) -> Result<(), StoreError> {
    if !truncate_wal_on_flush || directory.join(manifest::FILE_NAME).exists() {
        return Ok(());
    }
    let first = recovery
        .batches
        .iter()
        .filter(|batch| batch.table_id == table_id)
        .map(|batch| batch.sequence)
        .min();
    if first.is_some_and(|first| first > 1) {
        return Err(StoreError::corrupt_manifest(
            0,
            "the manifest is missing but the write-ahead log starts after rows that were already \
             flushed",
        ));
    }
    Ok(())
}

impl Drop for TableStore {
    fn drop(&mut self) {
        // Hold the writer lock (and input files) until output publication has
        // stopped. Dropping a JoinHandle detaches it; a subsequent writer's
        // orphan sweep must never race the old worker's temporary-file rename.
        // A merge that finishes is published before the lock goes: a writer
        // that closes with its merge thrown away, and whose successor starts
        // the same merge again, never gets one done.
        self.settle_background_merge();
    }
}

impl TableStore {
    fn discard_background_merge(&mut self) {
        if let Some(merge) = self.background.take() {
            let _ = merge.worker.join();
        }
    }

    /// Waits for the merge in flight and publishes what it wrote. A merge
    /// asked to yield ends early with nothing to publish.
    fn settle_background_merge(&mut self) {
        let Some(merge) = self.background.take() else {
            return;
        };
        let BackgroundMerge {
            worker,
            receiver,
            input_files,
        } = merge;
        let outcome = receiver.recv();
        let _ = worker.join();
        match outcome {
            Ok(Ok(outputs)) => {
                if let Err(error) = self.publish_merge(&input_files, outputs) {
                    self.last_background_error = Some(error.to_string());
                } else {
                    let _ = self.reclaim_obsolete_segments();
                }
            }
            Ok(Err(error)) => self.last_background_error = Some(error.to_string()),
            Err(_) => {}
        }
    }

    /// Has this table's background merges stop early once `flag` is set,
    /// so closing the table does not wait for one. What a stopped merge
    /// wrote is swept at the next open and the merge is planned again.
    pub fn yield_merges_to(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) {
        self.merge_yield = flag;
    }

    /// Moves this table's compaction forward when no write does: publishes
    /// a finished merge and starts the next one the table's segments call
    /// for. Returns whether a merge is now running.
    ///
    /// A flush does the same, which is all a table under steady writes
    /// needs; one whose writes stopped is left with the segments its last
    /// flushes wrote unless something calls this.
    ///
    /// # Errors
    ///
    /// Returns an error when a manifest cannot be published or segment
    /// metadata cannot be read.
    pub fn maintain(&mut self) -> Result<bool, StoreError> {
        if !self.options.background_compaction {
            if self.compact()?.input_segments() > 0 {
                self.reclaim_obsolete_segments()?;
            }
            return Ok(false);
        }
        if self.poll_background_merge()? {
            self.reclaim_obsolete_segments()?;
        }
        if self.background.is_none() {
            self.spawn_background_merge()?;
        }
        Ok(self.background.is_some())
    }

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

    #[allow(clippy::too_many_lines)] // one linear recovery sequence
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

        let lock_path = directory.join(WRITER_LOCK_FILE);
        let publication = Publisher::claim(&directory, &lock_path, || {
            let writer_lock = open_lock(&lock_path)?;
            lock_writer(&writer_lock, "lock table writer")?;
            Ok(writer_lock)
        })?;
        // Recovery can truncate the log or rewrite the manifest, and then
        // publishes once the open ends; an open that changed nothing a
        // reader reads - the usual case, and every replication cycle's -
        // leaves the generation alone. Swept orphans are files no manifest
        // names, so no reader ever read them.
        let opening = publication.publishing();
        let mut changed = false;

        let mut manifest = manifest::load(&directory, &schema)?;
        let schema_upgrade = manifest.schema_version < schema.version();
        for meta in &manifest.segments {
            if schema_upgrade {
                segment::read(&directory, meta, &schema)?;
            } else {
                segment::verify(&directory, meta, &schema)?;
            }
        }
        let (mut wal, mut recovery) = Wal::open(wal_path, options.wal_sync)?;
        refuse_a_lost_manifest(&directory, truncate_wal_on_flush, &recovery, table_id)?;
        if collapse_identical_segments(&directory, &mut manifest)? {
            changed = true;
        }
        remove_orphan_segments(&directory, &manifest)?;
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
                changed = true;
                wal.truncate_to(offset)?;
            }
            // The dropped records are gone from the log, so their sequences
            // must be free again. Keeping the highest sequence ever written
            // leaves a gap the next write starts after, and a gap at the
            // very start of the log is indistinguishable from rows that were
            // flushed and whose manifest was lost: the table then refuses
            // every open (`refuse_a_lost_manifest`).
            recovery.last_sequence = committed.map_or(0, |commit| commit.sequence);
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
            changed = true;
            wal.reset()?;
        }
        if schema_upgrade {
            changed = true;
            publish_schema_upgrade(&directory, &mut manifest, &schema)?;
        }
        let next_append_row_id =
            find_next_append_row_id(&directory, &manifest, &schema, &memtable)?;
        let manifest = Arc::new(manifest);
        if !changed {
            opening.unchanged();
        }

        Ok(Self {
            publication,
            instance: STORE_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
            merge_yield: Arc::default(),
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
        let _published = self.publication.publishing();
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
        // A merge that finished since the last write is published now rather
        // than at the next flush, which a table of small writes may not
        // reach for a long time.
        if self.background.is_some() && self.poll_background_merge()? {
            self.reclaim_obsolete_segments()?;
            // And the next merge starts behind it: one merge per flush
            // falls behind a table whose merges each leave several
            // segments, as a fold of one key range does.
            self.spawn_background_merge()?;
        }
        let _published = self.publication.publishing();
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
        let _published = self.publication.publishing();
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
        // Old inputs must no longer be read when reclaimed, and a completed
        // result must never be published into the new empty generation.
        self.discard_background_merge();
        let _published = self.publication.publishing();
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
        rows: Vec<StoredRow>,
    ) -> Result<BulkIngestOutcome, StoreError> {
        self.ingest_snapshot_chunk(rows, None)
    }

    /// Publishes one snapshot chunk that is the whole source content of the
    /// key range `covers`, retiring in the same manifest swap every earlier
    /// snapshot copy lying entirely inside that range.
    ///
    /// A resumed copy re-reads a range whenever its journal is behind the
    /// store: a crash between a chunk's publication and its journal entry, or
    /// a control plane restored from before the last chunks landed. Without
    /// the retirement the re-read chunk was published beside its first copy,
    /// the two segments overlapped key for key, and every scan of the table
    /// fell to the row-merging path for good. Only snapshot copies qualify
    /// (every row at version zero, no tombstones), so replicated changes are
    /// never dropped. With no rows the call only retires, which is how a copy
    /// that finds the source ends earlier than its last run removes the tail.
    ///
    /// # Errors
    ///
    /// As [`Self::bulk_ingest_snapshot`], and for rows outside `covers`.
    pub fn bulk_ingest_snapshot_covering(
        &mut self,
        rows: Vec<StoredRow>,
        covers: (Bound<PrimaryKey>, Bound<PrimaryKey>),
    ) -> Result<BulkIngestOutcome, StoreError> {
        if let Some(row) = rows.iter().find(|row| !covers.contains(row.key())) {
            return Err(StoreError::FormatLimit(format!(
                "snapshot chunk row {:?} lies outside the range the chunk covers",
                row.key()
            )));
        }
        self.ingest_snapshot_chunk(rows, Some(&covers))
    }

    #[allow(clippy::too_many_lines)] // one linear validate-write-publish sequence
    fn ingest_snapshot_chunk(
        &mut self,
        mut rows: Vec<StoredRow>,
        covers: Option<&(Bound<PrimaryKey>, Bound<PrimaryKey>)>,
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
        let mut superseded = covers.map_or_else(Vec::new, |covers| {
            self.manifest
                .segments
                .iter()
                .filter(|meta| {
                    meta.max_version == 0
                        && meta.unique_keys
                        && meta.smas.as_ref().is_none_or(|smas| smas.tombstones == 0)
                        && covers.contains(&meta.min_key)
                        && covers.contains(&meta.max_key)
                })
                .map(|meta| meta.file_name.clone())
                .collect::<Vec<_>>()
        });
        if rows.is_empty() && superseded.is_empty() {
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

        let _published = self.publication.publishing();
        let mut next_manifest = self.manifest.as_ref().clone();
        next_manifest.generation = next_manifest
            .generation
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        next_manifest.epoch = next_manifest
            .epoch
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        let mut segment_path = None;
        if !rows.is_empty() {
            let segment = segment::write(
                &self.directory,
                self.manifest.next_segment_id,
                &self.schema,
                &rows,
                self.options.block_rows,
                segment::Compression::AdaptiveLz4,
                true,
            )?;
            segment_path = Some(self.directory.join(&segment.file_name));
            // A chunk re-read from an unchanged source encodes to the same
            // bytes as its first copy, whatever its key type; that copy goes
            // too; dropping one of two identical files changes no answer.
            for meta in &self.manifest.segments {
                if !superseded.contains(&meta.file_name)
                    && same_segment_span(meta, &segment)
                    && same_file_contents(
                        &self.directory.join(&meta.file_name),
                        &self.directory.join(&segment.file_name),
                    )?
                {
                    superseded.push(meta.file_name.clone());
                }
            }
            next_manifest.next_segment_id = next_manifest
                .next_segment_id
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?;
            next_manifest.segments.push(segment);
        }
        next_manifest
            .segments
            .retain(|meta| !superseded.contains(&meta.file_name));
        manifest::publish(&self.directory, &next_manifest)?;
        let previous = std::mem::replace(&mut self.manifest, Arc::new(next_manifest));
        if !superseded.is_empty() {
            self.retired.push(RetiredGeneration {
                readers: Arc::downgrade(&previous),
                paths: superseded
                    .iter()
                    .map(|file_name| self.directory.join(file_name))
                    .collect(),
            });
        }
        Ok(BulkIngestOutcome {
            row_count: rows.len(),
            segment_path,
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
        let _published = self.publication.publishing();
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
        crash_point("store.flush.after_segment")?;
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
        crash_point("store.flush.after_manifest")?;

        self.manifest = Arc::new(next_manifest);
        self.memtable.clear();
        if self.truncate_wal_on_flush {
            self.wal.reset()?;
        }
        crash_point("store.flush.after_wal_reset")?;
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
            layer_index_bytes: self.manifest.layer_index.bytes(),
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

    /// Publishes a background merge that has finished, if any, and says
    /// whether it did. Cheap when the merge is still running.
    fn poll_background_merge(&mut self) -> Result<bool, StoreError> {
        let Some(merge) = &self.background else {
            return Ok(false);
        };
        let outcome = match merge.receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(false),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.background = None;
                self.last_background_error =
                    Some("background merge thread exited without a result".to_owned());
                return Ok(false);
            }
        };
        let Some(merge) = self.background.take() else {
            return Ok(false);
        };
        let _ = merge.worker.join();
        let outputs = match outcome {
            Ok(outputs) => outputs,
            Err(error) => {
                // The merge is optional: unmerged segments still resolve by
                // streaming merge-on-read, and orphan chunk files are swept
                // at the next open. Record and move on.
                self.last_background_error = Some(error.to_string());
                return Ok(false);
            }
        };
        self.publish_merge(&merge.input_files, outputs)?;
        Ok(true)
    }

    /// Replaces a finished merge's inputs with its outputs in a new manifest.
    fn publish_merge(
        &mut self,
        input_files: &[String],
        outputs: Vec<segment::SegmentMeta>,
    ) -> Result<(), StoreError> {
        let inputs = input_files.iter().collect::<std::collections::HashSet<_>>();
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
        let _published = self.publication.publishing();
        crash_point("store.merge.before_publish")?;
        manifest::publish(&self.directory, &next_manifest)?;
        crash_point("store.merge.after_publish")?;
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
        let drop_tombstones = merge_drops_tombstones(&self.manifest.segments, &plan.indices);
        let window = plan.window.clone();
        if window.is_some() {
            pintail_log::log_debug!(
                "pintail store compaction folds one key range of a cluster: {} of {} segments",
                plan.indices.len(),
                self.manifest.segments.len()
            );
        }
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
        let _published = self.publication.publishing();
        manifest::publish(&self.directory, &next_manifest)?;
        crash_point("store.merge.after_reserve")?;
        self.manifest = Arc::new(next_manifest);
        let directory = self.directory.clone();
        let schema = self.schema.clone();
        let options = self.options;
        let yield_flag = Arc::clone(&self.merge_yield);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("pintail-compaction".to_owned())
            .spawn(move || {
                let result = run_background_merge(
                    &directory,
                    &schema,
                    options,
                    &input_metas,
                    full_merge,
                    drop_tombstones,
                    window.as_ref(),
                    id_base,
                    &yield_flag,
                );
                // The owner joins this worker before releasing its writer
                // lock; the next open may then sweep unpublished chunks.
                let _ = sender.send(result);
            })
            .map_err(|error| StoreError::io("spawn compaction thread", error))?;
        self.background = Some(BackgroundMerge {
            worker,
            receiver,
            input_files,
        });
        Ok(())
    }

    /// Runs one bounded size-tier merge of similarly sized overlapping files.
    ///
    /// A merge that covers the complete manifest writes zstd at the coldest
    /// tier. A merge drops tombstones when nothing left out of it could hold
    /// an older version of a key they delete (see `merge_drops_tombstones`);
    /// otherwise it keeps them to suppress those versions.
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
        let drop_tombstones = merge_drops_tombstones(&self.manifest.segments, &plan.indices);
        let _published = self.publication.publishing();
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
        let window = plan.window.as_ref();
        let mut was_inside = false;
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
            // A windowed merge closes its output at each boundary of the
            // window and drops deletes only inside it.
            let inside = in_merge_window(window, &minimum);
            if window.is_some() && inside != was_inside && !rows.is_empty() {
                let path = write_compaction_chunk(
                    &self.directory,
                    &self.schema,
                    self.options.block_rows,
                    compression,
                    &mut next_manifest,
                    &rows,
                )?;
                output_path.get_or_insert(path);
                rows.clear();
                buffered_bytes = 0;
            }
            was_inside = inside;
            let drops = if window.is_some() {
                inside
            } else {
                drop_tombstones
            };
            if !drops || !winner.is_deleted() {
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
        pintail_log::log_debug!(
            "pintail store compaction merged {} of {} segments into {output_rows} rows, tombstones {}",
            plan.indices.len(),
            previous.segments.len(),
            if drop_tombstones { "dropped" } else { "kept" }
        );
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
        let published = self.publication.publishing();
        let mut reclaimed = 0;
        let mut retained = Vec::new();
        // A reader that opened the table from its files holds a manifest of
        // its own, which this writer's retired generations know nothing of:
        // the files it names are pinned through the registry instead.
        let pinned = pinned_manifests(&self.directory);
        let read_elsewhere = |path: &PathBuf| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    pinned.iter().any(|manifest| {
                        manifest
                            .segments
                            .iter()
                            .any(|segment| segment.file_name == name)
                    })
                })
        };
        for generation in self.retired.drain(..) {
            if generation.readers.upgrade().is_some() || generation.paths.iter().any(read_elsewhere)
            {
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
        } else {
            published.unchanged();
        }
        Ok(reclaimed)
    }

    /// Pins an immutable view of rows currently visible to readers.
    #[must_use]
    pub fn snapshot(&self) -> TableSnapshot {
        register_pinned_manifest(&self.directory, &self.manifest);
        TableSnapshot {
            instance: self.instance,
            memtable: self.memtable.snapshot(),
            memtable_image: self.memtable.image(),
            memtable_oldest: self.memtable.oldest_version(),
            manifest: Arc::clone(&self.manifest),
            directory: self.directory.clone(),
            schema: self.schema.clone(),
            estimated_bytes: self.memtable.estimated_bytes(),
        }
    }

    /// Returns the table directory.
    /// Moves the table's directory to `new_directory`, keeping the writer
    /// open: the WAL and lock handles follow the directory, the manifest
    /// and segments are addressed relative to it from here on. A background
    /// compaction in flight is collected first so no worker writes into the
    /// old path. Snapshots taken before the move keep the old path and
    /// fail on their next read, so the caller moves at a quiet boundary (a
    /// DDL position in the stream).
    ///
    /// # Errors
    ///
    /// Returns an error when a background merge fails to finish, the target
    /// already exists, or the filesystem refuses the move.
    pub fn rename_directory(&mut self, new_directory: impl AsRef<Path>) -> Result<(), StoreError> {
        while self.background.is_some() {
            self.poll_background_merge()?;
            if self.background.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let new_directory = new_directory.as_ref();
        if new_directory.exists() {
            return Err(StoreError::io(
                "rename table directory",
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} already exists", new_directory.display()),
                ),
            ));
        }
        let published = self.publication.publishing();
        std::fs::rename(&self.directory, new_directory)
            .map_err(|error| StoreError::io("rename table directory", error))?;
        self.directory = std::fs::canonicalize(new_directory)
            .map_err(|error| StoreError::io("canonicalize renamed table directory", error))?;
        drop(published);
        self.publication.relocate(&self.directory);
        Ok(())
    }

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
        // Two segments are enough to plan when they overlap. The fan-in is
        // there to amortize a rewrite over several inputs, which is the
        // right instinct when merging only saves file handles - but an
        // overlap is not that. A key present in two segments puts every
        // scan of the table on the merging path, and a merging scan of two
        // million rows with one percent of them changed measured 1429 ms
        // against 14 ms for the same rows in one segment. The rewrite that
        // removes it measured 1552 ms, once. It repays after 1.1 scans
        // (experiments/RESULTS.md e91), so waiting for a fourth segment
        // before considering it is a hundredfold read penalty held open
        // for a write cost the second query already covers.
        if self.manifest.segments.len() < 2 {
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
                min_version: meta.min_version,
                max_version: meta.max_version,
                unique_keys: meta.unique_keys,
            });
        }
        candidates.sort_by_key(|candidate| (candidate.size, candidate.index));
        if candidates.len() >= self.options.compaction_fan_in {
            for window in candidates.windows(self.options.compaction_fan_in) {
                let selected = window.iter().collect::<Vec<_>>();
                if !self.admits_window(&selected)
                    || !ranges_overlap(&selected)
                    || splits_a_layer(&selected, &candidates)
                {
                    continue;
                }
                return Ok(Some(plan_for(&selected)));
            }
        }
        // No full-width window qualified. An overlapping pair still earns
        // its rewrite, and it is the shape a table takes right after a
        // flush: one large base and one small tail covering the rows that
        // changed. The size tier deliberately refuses that pairing, since
        // rewriting a base to absorb a tail a hundredth its size is poor
        // value when the only prize is fewer files. Overlap is the case
        // where the prize is the scan, so the tier does not apply - the
        // per-pass row budget still does, so one pass stays bounded.
        candidates.sort_by(|left, right| left.minimum.cmp(&right.minimum));
        for window in candidates.windows(2) {
            let selected = window.iter().collect::<Vec<_>>();
            if !ranges_overlap(&selected) {
                continue;
            }
            let rows = selected
                .iter()
                .map(|candidate| candidate.row_count)
                .sum::<u64>();
            if rows <= self.options.max_compaction_input_rows {
                let cluster = self.overlap_cluster(&candidates, selected);
                // A cluster the budget cannot take whole is folded a key
                // range at a time rather than a few of its segments at a
                // time.
                if splits_a_layer(&cluster, &candidates)
                    && let Some(plan) = self.windowed_plan(&candidates, &cluster)
                {
                    return Ok(Some(plan));
                }
                return Ok(Some(plan_for(&cluster)));
            }
        }
        // A segment no other reaches that still carries deletes (or a key
        // twice) is what a windowed merge leaves of the changes that fell
        // outside every base. Nothing is left for its deletes to hide, yet
        // a scan reads it row by row to apply them: rewriting it alone
        // drops them and leaves a segment that decodes directly.
        if let Some(lone) = candidates.iter().find(|candidate| {
            !candidate.unique_keys
                && candidate.row_count <= self.options.max_compaction_input_rows
                && candidates
                    .iter()
                    .all(|other| other.index == candidate.index || !other.overlaps(candidate))
        }) {
            return Ok(Some(plan_for(&[lone])));
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

    /// Grows an overlapping pair to every segment its key span reaches, while
    /// the row budget allows. A tail of changes spread across a table
    /// overlaps each of the disjoint files an earlier merge left behind;
    /// merging it with only one of them keeps its deletes for the others
    /// and leaves the output overlapping them still, so every scan stays on
    /// the row-wise merge. Taking the whole cluster resolves it in one pass.
    fn overlap_cluster<'a>(
        &self,
        candidates: &'a [CompactionCandidate],
        pair: Vec<&'a CompactionCandidate>,
    ) -> Vec<&'a CompactionCandidate> {
        let mut selected = pair;
        loop {
            let (Some(low), Some(high)) = (
                selected.iter().map(|candidate| &candidate.minimum).min(),
                selected.iter().map(|candidate| &candidate.maximum).max(),
            ) else {
                return selected;
            };
            let reached = candidates
                .iter()
                .filter(|candidate| {
                    candidate.minimum <= *high
                        && candidate.maximum >= *low
                        && !selected
                            .iter()
                            .any(|chosen| chosen.index == candidate.index)
                })
                .collect::<Vec<_>>();
            if reached.is_empty() {
                return selected;
            }
            let rows = selected
                .iter()
                .chain(&reached)
                .map(|candidate| candidate.row_count)
                .sum::<u64>();
            if rows > self.options.max_compaction_input_rows {
                return selected;
            }
            selected.extend(reached);
        }
    }

    /// A merge of one key range of the cluster `seed` belongs to, for a
    /// cluster too large to merge whole: the cluster's bases (unique-key
    /// segments with nothing older over their keys) in key order, as many
    /// consecutive ones as the row budget takes, with every other segment
    /// that reaches their range. The range starts at the first base the
    /// cluster's oldest change segment reaches, so successive passes walk
    /// that segment across the table instead of folding the lowest keys
    /// again while it waits. `None` when the cluster has no such shape or
    /// even one base does not fit beside the changes.
    fn windowed_plan(
        &self,
        candidates: &[CompactionCandidate],
        seed: &[&CompactionCandidate],
    ) -> Option<CompactionPlan> {
        // Everything the seed reaches, whatever it holds.
        let mut cluster = seed.to_vec();
        loop {
            let reached = candidates
                .iter()
                .filter(|candidate| {
                    !cluster.iter().any(|member| member.index == candidate.index)
                        && cluster.iter().any(|member| member.overlaps(candidate))
                })
                .collect::<Vec<_>>();
            if reached.is_empty() {
                break;
            }
            cluster.extend(reached);
        }
        let mut by_age = cluster.clone();
        by_age.sort_by_key(|candidate| (candidate.min_version, candidate.max_version));
        let mut bases: Vec<&CompactionCandidate> = Vec::new();
        let mut changes: Vec<&CompactionCandidate> = Vec::new();
        for candidate in by_age {
            let under_everything = cluster.iter().all(|other| {
                other.index == candidate.index
                    || !other.overlaps(candidate)
                    || other.min_version >= candidate.max_version
            });
            if candidate.unique_keys
                && under_everything
                && bases.iter().all(|base| !base.overlaps(candidate))
            {
                bases.push(candidate);
            } else {
                changes.push(candidate);
            }
        }
        let oldest = *changes.first()?;
        if bases.len() < 2 {
            return None;
        }
        bases.sort_by(|left, right| left.minimum.cmp(&right.minimum));
        let first = bases
            .iter()
            .position(|base| base.maximum >= oldest.minimum)?;
        let change_rows = changes.iter().map(|change| change.row_count).sum::<u64>();
        let mut rows = change_rows;
        let mut end = first;
        while end < bases.len()
            && rows.saturating_add(bases[end].row_count) <= self.options.max_compaction_input_rows
        {
            rows = rows.saturating_add(bases[end].row_count);
            end += 1;
        }
        if end == first || oldest.minimum > bases[end - 1].maximum {
            // No base fits beside the changes, or the oldest change segment
            // does not reach the range: folding it would rewrite bases for
            // nothing.
            return None;
        }
        let low = (first > 0).then(|| bases[first].minimum.clone());
        let high = bases.get(end).map(|base| base.minimum.clone());
        let reaches = |candidate: &CompactionCandidate| {
            low.as_ref().is_none_or(|low| candidate.maximum >= *low)
                && high.as_ref().is_none_or(|high| candidate.minimum < *high)
        };
        let mut selected = bases[first..end].to_vec();
        selected.extend(changes.iter().copied().filter(|change| reaches(change)));
        // Deletes are dropped inside the range, so nothing left out may
        // reach it.
        let closed = candidates.iter().all(|candidate| {
            selected
                .iter()
                .any(|member| member.index == candidate.index)
                || !reaches(candidate)
        });
        if !closed || (low.is_none() && high.is_none()) {
            return None;
        }
        let mut plan = plan_for(&selected);
        plan.window = Some((low, high));
        Some(plan)
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

/// A crash-consistency boundary of the store: between two durable steps of a
/// flush or of a merge's publication. In a build with failpoints it is the
/// failpoint of the same name, which stops the process there as a kill at
/// that instant would; otherwise nothing.
fn crash_point(site: &'static str) -> Result<(), StoreError> {
    pintail_failpoint::hit(site).map_err(|error| StoreError::io("recovery failpoint", error))
}

fn plan_for(window: &[&CompactionCandidate]) -> CompactionPlan {
    CompactionPlan {
        indices: window.iter().map(|candidate| candidate.index).collect(),
        debt_bytes: window.iter().map(|candidate| candidate.size).sum(),
        window: None,
    }
}

/// Whether merging `selected` would fold a segment into only some of the
/// segments it lies over: one of them is selected with it, another is left
/// out, and the two are not layered themselves. The output would then hold
/// rows as old as the segment folded in while still reaching over the one
/// left out - neither a base nor wholly newer than one - and a scan can no
/// longer tell the changes from what they changed.
fn splits_a_layer(selected: &[&CompactionCandidate], candidates: &[CompactionCandidate]) -> bool {
    let chosen = |candidate: &CompactionCandidate| {
        selected
            .iter()
            .any(|member| member.index == candidate.index)
    };
    selected.iter().any(|over| {
        selected.iter().any(|under| {
            over.lies_over(under)
                && candidates
                    .iter()
                    .any(|left| !chosen(left) && over.lies_over(left) && !under.lies_over(left))
        })
    })
}

struct CompactionCandidate {
    index: usize,
    size: u64,
    row_count: u64,
    minimum: PrimaryKey,
    maximum: PrimaryKey,
    min_version: u64,
    max_version: u64,
    unique_keys: bool,
}

impl CompactionCandidate {
    fn overlaps(&self, other: &Self) -> bool {
        self.minimum <= other.maximum && self.maximum >= other.minimum
    }

    /// Whether this segment lies over `other`: their keys overlap and
    /// everything here is at least as new as everything there, some of it
    /// newer.
    fn lies_over(&self, other: &Self) -> bool {
        self.overlaps(other)
            && self.min_version >= other.max_version
            && self.max_version > other.max_version
    }
}

/// The key range a merge folds: from the first key (inclusive, `None` for
/// the lowest) up to the second (exclusive, `None` for the highest).
type MergeWindow = (Option<PrimaryKey>, Option<PrimaryKey>);

struct CompactionPlan {
    indices: Vec<usize>,
    debt_bytes: u64,
    /// When set, the merge folds only this key range: there every selected
    /// segment's rows resolve to one per key and deletes are dropped, since
    /// every segment that reaches the range is selected. Rows of the
    /// selected segments outside it are written through as they are - the
    /// newest version of a key, a delete kept - into segments of their own,
    /// so no output spans a boundary of the range.
    window: Option<MergeWindow>,
}

/// Whether `key` lies inside a merge's window; every key does without one.
fn in_merge_window(window: Option<&MergeWindow>, key: &PrimaryKey) -> bool {
    window.is_none_or(|(low, high)| {
        low.as_ref().is_none_or(|low| key >= low) && high.as_ref().is_none_or(|high| key < high)
    })
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
#[allow(clippy::too_many_arguments)]
fn run_background_merge(
    directory: &Path,
    schema: &TableSchema,
    options: StoreOptions,
    input_metas: &[segment::SegmentMeta],
    full_merge: bool,
    drop_tombstones: bool,
    window: Option<&MergeWindow>,
    id_base: u64,
    yield_flag: &std::sync::atomic::AtomicBool,
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
    let mut was_inside = false;
    let mut write_chunk = |rows: &[StoredRow], next_id: &mut u64| -> Result<(), StoreError> {
        let output = segment::write(
            directory,
            *next_id,
            schema,
            rows,
            options.block_rows,
            compression,
            // A merge emits one row per key; the flag also promises no
            // tombstones, which only a chunk without them can keep.
            rows.iter().all(|row| !row.is_deleted()),
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
        if yield_flag.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StoreError::io(
                "background merge",
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "the merge yielded to an operator action",
                ),
            ));
        }
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
        // A windowed merge closes its output at each boundary of the window
        // and drops deletes only inside it.
        let inside = in_merge_window(window, &minimum);
        if window.is_some() && inside != was_inside && !rows.is_empty() {
            write_chunk(&rows, &mut next_id)?;
            rows.clear();
            buffered_bytes = 0;
        }
        was_inside = inside;
        let drops = if window.is_some() {
            inside
        } else {
            drop_tombstones
        };
        if !drops || !winner.is_deleted() {
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
        // A merge emits one row per key; the flag also promises no
        // tombstones, which only a chunk without them can keep.
        rows.iter().all(|row| !row.is_deleted()),
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

/// Publishes `manifest` under the newer `schema` its segments were just
/// verified readable by: a new generation and epoch, the schema's version
/// and fingerprint.
fn publish_schema_upgrade(
    directory: &Path,
    manifest: &mut Manifest,
    schema: &TableSchema,
) -> Result<(), StoreError> {
    manifest.generation = manifest
        .generation
        .checked_add(1)
        .ok_or(StoreError::SequenceOverflow)?;
    manifest.epoch = manifest
        .epoch
        .checked_add(1)
        .ok_or(StoreError::SequenceOverflow)?;
    manifest.schema_version = schema.version();
    manifest.schema_fingerprint = segment::schema_fingerprint(schema);
    manifest::publish(directory, manifest)
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

/// Drops from the manifest every segment whose file is byte for byte the same
/// as a later one, and publishes the result; the dropped files are then
/// orphans the open's sweep removes once no reader pins them.
///
/// A resumed snapshot copy used to republish ranges the store already held,
/// leaving identical segment pairs that overlap key for key. Nothing was
/// wrong in any answer - merge-on-read resolves a tie to one of two equal
/// rows - but the overlap kept the table off every disjoint-segment path
/// for good, and compaction never saw a write that would start it. Removing
/// one of two identical files changes no row, version or tombstone, so this
/// needs no other precondition. Candidates are the segments with equal key
/// span, row count and version span; only those have their bytes compared.
fn collapse_identical_segments(
    directory: &Path,
    manifest: &mut Manifest,
) -> Result<bool, StoreError> {
    let mut order = (0..manifest.segments.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        let (left, right) = (&manifest.segments[*left], &manifest.segments[*right]);
        (
            &left.min_key,
            &left.max_key,
            left.row_count,
            left.min_version,
            left.max_version,
        )
            .cmp(&(
                &right.min_key,
                &right.max_key,
                right.row_count,
                right.min_version,
                right.max_version,
            ))
    });
    let mut duplicates = std::collections::HashSet::new();
    for pair in order.windows(2) {
        let (left, right) = (&manifest.segments[pair[0]], &manifest.segments[pair[1]]);
        if !same_segment_span(left, right) {
            continue;
        }
        if same_file_contents(
            &directory.join(&left.file_name),
            &directory.join(&right.file_name),
        )? {
            // The sort is stable, so the pair is in manifest order and the
            // earlier segment - the one merge-on-read lets the later outrank -
            // is the one to drop.
            duplicates.insert(pair[0]);
        }
    }
    if duplicates.is_empty() {
        return Ok(false);
    }
    let mut index = 0;
    manifest.segments.retain(|_| {
        let keep = !duplicates.contains(&index);
        index += 1;
        keep
    });
    manifest.generation = manifest
        .generation
        .checked_add(1)
        .ok_or(StoreError::SequenceOverflow)?;
    manifest.epoch = manifest
        .epoch
        .checked_add(1)
        .ok_or(StoreError::SequenceOverflow)?;
    manifest::publish(directory, manifest)?;
    Ok(true)
}

fn same_segment_span(left: &segment::SegmentMeta, right: &segment::SegmentMeta) -> bool {
    left.min_key == right.min_key
        && left.max_key == right.max_key
        && left.row_count == right.row_count
        && left.min_version == right.min_version
        && left.max_version == right.max_version
}

fn same_file_contents(left: &Path, right: &Path) -> Result<bool, StoreError> {
    use std::io::Read;
    let open = |path: &Path| {
        File::open(path).map_err(|error| {
            StoreError::io(
                format!("open segment {} for comparison", path.display()),
                error,
            )
        })
    };
    let (mut left_file, mut right_file) = (open(left)?, open(right)?);
    let length = |file: &File, path: &Path| {
        file.metadata()
            .map(|metadata| metadata.len())
            .map_err(|error| StoreError::io(format!("inspect segment {}", path.display()), error))
    };
    if length(&left_file, left)? != length(&right_file, right)? {
        return Ok(false);
    }
    let mut left_buffer = vec![0_u8; 1 << 16];
    let mut right_buffer = vec![0_u8; 1 << 16];
    loop {
        let read = left_file
            .read(&mut left_buffer)
            .map_err(|error| StoreError::io("read segment for comparison", error))?;
        if read == 0 {
            return Ok(true);
        }
        right_file
            .read_exact(&mut right_buffer[..read])
            .map_err(|error| StoreError::io("read segment for comparison", error))?;
        if left_buffer[..read] != right_buffer[..read] {
            return Ok(false);
        }
    }
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

/// Whether a merge of the `selected` segments may drop its tombstones:
/// nothing left out of it could hold an older version of a key they delete.
/// A segment left out is harmless when its keys lie outside the merged
/// span, or when every row it holds is newer than every merged row - its
/// row for a key then wins over the tombstone whether that is kept or not.
fn merge_drops_tombstones(segments: &[segment::SegmentMeta], selected: &[usize]) -> bool {
    let chosen = selected
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let merged = || selected.iter().filter_map(|index| segments.get(*index));
    let (Some(low), Some(high), Some(newest)) = (
        merged().map(|meta| &meta.min_key).min(),
        merged().map(|meta| &meta.max_key).max(),
        merged().map(|meta| meta.max_version).max(),
    ) else {
        return true;
    };
    segments
        .iter()
        .enumerate()
        .filter(|(index, _)| !chosen.contains(index))
        .all(|(_, meta)| meta.max_key < *low || meta.min_key > *high || meta.min_version > newest)
}
