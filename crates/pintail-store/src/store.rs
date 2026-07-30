use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
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
const WRITER_LOCK_FILE: &str = ".writer.lock";
const DEFAULT_MEMTABLE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_BLOCK_ROWS: usize = 64 * 1024;
const DEFAULT_COMPACTION_FAN_IN: usize = 4;
const SIZE_TIER_RATIO: u64 = 4;

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
    /// Number of similarly sized overlapping segments merged in one pass.
    pub compaction_fan_in: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            block_rows: DEFAULT_BLOCK_ROWS,
            wal_sync: WalSync::Checkpoint,
            compaction_fan_in: DEFAULT_COMPACTION_FAN_IN,
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
}

/// A scan row containing only the requested user columns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedRow {
    key: PrimaryKey,
    values: Vec<pintail_types::Value>,
    version: u64,
}

impl ProjectedRow {
    /// Returns the physical primary, unique, or generated row key.
    #[must_use]
    pub fn key(&self) -> &PrimaryKey {
        &self.key
    }

    /// Returns values in the caller's requested column-ID order.
    #[must_use]
    pub fn values(&self) -> &[pintail_types::Value] {
        &self.values
    }

    /// Returns the winning source version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }
}

/// Physical work performed by a projected range scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanStats {
    segments_pruned: usize,
    segments_read: usize,
    blocks_pruned: usize,
    blocks_decoded: usize,
}

impl ScanStats {
    /// Returns segments rejected from manifest key bounds.
    #[must_use]
    pub fn segments_pruned(self) -> usize {
        self.segments_pruned
    }

    /// Returns segments whose block metadata was inspected.
    #[must_use]
    pub fn segments_read(self) -> usize {
        self.segments_read
    }

    /// Returns key blocks rejected by typed zone maps.
    #[must_use]
    pub fn blocks_pruned(self) -> usize {
        self.blocks_pruned
    }

    /// Returns blocks whose encoded values were decompressed and decoded.
    #[must_use]
    pub fn blocks_decoded(self) -> usize {
        self.blocks_decoded
    }
}

/// Rows and physical counters from a projected range scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedScan {
    rows: Vec<ProjectedRow>,
    stats: ScanStats,
}

impl ProjectedScan {
    /// Returns visible projected rows in key order.
    #[must_use]
    pub fn rows(&self) -> &[ProjectedRow] {
        &self.rows
    }

    /// Returns pruning and decoding counters.
    #[must_use]
    pub fn stats(&self) -> ScanStats {
        self.stats
    }
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
    next_append_row_id: u64,
    table_id: u64,
    truncate_wal_on_flush: bool,
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
        std::fs::create_dir_all(directory)
            .map_err(|error| StoreError::io("create table directory", error))?;
        let directory = std::fs::canonicalize(directory)
            .map_err(|error| StoreError::io("canonicalize table directory", error))?;

        let writer_lock = open_lock(&directory.join(WRITER_LOCK_FILE))?;
        FileExt::try_lock_exclusive(&writer_lock).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::WriterBusy
            } else {
                StoreError::io("lock table writer", error)
            }
        })?;

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
        let (mut wal, recovery) = Wal::open(wal_path, options.wal_sync)?;
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
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        self.ingest_at_sequence(sequence, rows)
    }

    pub(crate) fn ingest_at_sequence(
        &mut self,
        sequence: u64,
        mut rows: Vec<StoredRow>,
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
            if self.manifest.segments.len() >= self.options.compaction_fan_in {
                self.compact()?;
            }
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

        let segment = segment::write(
            &self.directory,
            self.manifest.next_segment_id,
            &self.schema,
            &rows,
            self.options.block_rows,
            segment::Compression::Lz4,
            false,
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
    pub fn compact(&mut self) -> Result<CompactionOutcome, StoreError> {
        let Some(plan) = self.compaction_plan()? else {
            return Ok(CompactionOutcome {
                input_segments: 0,
                output_rows: 0,
                output_path: None,
            });
        };
        let full_merge = plan.indices.len() == self.manifest.segments.len();
        let mut merged = BTreeMap::new();
        for index in &plan.indices {
            let meta = &self.manifest.segments[*index];
            for row in segment::read(&self.directory, meta, &self.schema)? {
                apply_latest(&mut merged, row);
            }
            std::thread::yield_now();
        }
        let rows = merged
            .into_values()
            .filter(|row| !full_merge || !row.is_deleted())
            .collect::<Vec<_>>();

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

        let output_path = if rows.is_empty() {
            None
        } else {
            let compression = if full_merge {
                segment::Compression::Zstd
            } else {
                segment::Compression::Lz4
            };
            let output = segment::write(
                &self.directory,
                next_manifest.next_segment_id,
                &self.schema,
                &rows,
                self.options.block_rows,
                compression,
                full_merge,
            )?;
            next_manifest.next_segment_id = next_manifest
                .next_segment_id
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?;
            let path = self.directory.join(&output.file_name);
            next_manifest.segments.push(output);
            Some(path)
        };
        manifest::publish(&self.directory, &next_manifest)?;

        let previous = std::mem::replace(&mut self.manifest, Arc::new(next_manifest));
        self.retired.push(RetiredGeneration {
            readers: Arc::downgrade(&previous),
            paths: retired_paths,
        });
        Ok(CompactionOutcome {
            input_segments: plan.indices.len(),
            output_rows: rows.len(),
            output_path,
        })
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
                minimum: meta.min_key.clone(),
                maximum: meta.max_key.clone(),
            });
        }
        candidates.sort_by_key(|candidate| (candidate.size, candidate.index));
        for window in candidates.windows(self.options.compaction_fan_in) {
            let smallest = window.first().expect("non-empty window").size;
            let largest = window.last().expect("non-empty window").size;
            if largest > smallest.saturating_mul(SIZE_TIER_RATIO) || !ranges_overlap(window) {
                continue;
            }
            return Ok(Some(CompactionPlan {
                indices: window.iter().map(|candidate| candidate.index).collect(),
                debt_bytes: window.iter().map(|candidate| candidate.size).sum(),
            }));
        }
        Ok(None)
    }
}

struct CompactionCandidate {
    index: usize,
    size: u64,
    minimum: PrimaryKey,
    maximum: PrimaryKey,
}

struct CompactionPlan {
    indices: Vec<usize>,
    debt_bytes: u64,
}

fn ranges_overlap(candidates: &[CompactionCandidate]) -> bool {
    let mut by_key = candidates.iter().collect::<Vec<_>>();
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

/// A reader-owned immutable table view.
pub struct TableSnapshot {
    memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    manifest: Arc<Manifest>,
    directory: PathBuf,
    schema: TableSchema,
}

impl TableSnapshot {
    /// Returns the catalog schema pinned with this reader snapshot.
    #[must_use]
    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Returns visible rows in primary-key order, excluding tombstones.
    ///
    /// # Errors
    ///
    pub fn scan(&self) -> Result<Vec<StoredRow>, StoreError> {
        if self.memtable.is_empty()
            && let [segment_meta] = self.manifest.segments.as_slice()
            && segment_meta.unique_keys
        {
            return Ok(segment::read(&self.directory, segment_meta, &self.schema)?
                .into_iter()
                .filter(|row| !row.is_deleted())
                .collect());
        }
        let mut latest = BTreeMap::new();
        for segment_meta in &self.manifest.segments {
            for row in segment::read(&self.directory, segment_meta, &self.schema)? {
                apply_latest(&mut latest, row);
            }
        }
        for row in self.memtable.values() {
            apply_latest(&mut latest, row.clone());
        }
        Ok(latest
            .into_values()
            .filter(|row| !row.is_deleted())
            .collect())
    }

    /// Returns one visible primary/unique key using footer range and bloom
    /// pruning before any segment block is decoded.
    ///
    /// # Errors
    ///
    /// Returns a precise segment corruption or filesystem error.
    pub fn get(&self, key: &PrimaryKey) -> Result<Option<StoredRow>, StoreError> {
        let column_ids = self
            .schema
            .columns()
            .iter()
            .map(pintail_types::Column::id)
            .collect::<Vec<_>>();
        let scan = self.scan_projected_range(key, key, &column_ids)?;
        Ok(scan
            .rows
            .into_iter()
            .next()
            .map(|row| StoredRow::new(row.key, row.values, row.version, false)))
    }

    /// Returns visible rows in one inclusive key range, pruning disjoint
    /// segments by footer key bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, corrupt segment, or filesystem
    /// failure.
    pub fn scan_range(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
    ) -> Result<Vec<StoredRow>, StoreError> {
        self.scan_range_versions(start, end, 0, u64::MAX)
    }

    /// Returns latest retained rows in inclusive key and source-version
    /// ranges, pruning segments whose complete version bounds are disjoint.
    ///
    /// This is a retained-version filter, not a historical snapshot API:
    /// memtable insertion and compaction may already have collapsed older
    /// versions of a key.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, corrupt segment, or filesystem
    /// failure.
    pub fn scan_range_versions(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        min_version: u64,
        max_version: u64,
    ) -> Result<Vec<StoredRow>, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        if min_version > max_version {
            return Err(StoreError::FormatLimit(
                "scan version range start follows its end".into(),
            ));
        }
        let mut latest = BTreeMap::new();
        for segment_meta in &self.manifest.segments {
            if segment_meta.max_version < min_version
                || segment_meta.min_version > max_version
                || !segment::overlaps_key_range(segment_meta, start, end)
            {
                continue;
            }
            for row in segment::read(&self.directory, segment_meta, &self.schema)? {
                if row.version() >= min_version
                    && row.version() <= max_version
                    && row.key() >= start
                    && row.key() <= end
                {
                    apply_latest(&mut latest, row);
                }
            }
        }
        for (_, row) in self.memtable.range(start.clone()..=end.clone()) {
            if row.version() >= min_version && row.version() <= max_version {
                apply_latest(&mut latest, row.clone());
            }
        }
        Ok(latest
            .into_values()
            .filter(|row| !row.is_deleted())
            .collect())
    }

    /// Scans an inclusive key range while decoding only requested user
    /// columns after segment and key-block pruning.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate/unknown column ID,
    /// incompatible schema, corrupt block, or filesystem failure.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<ProjectedScan, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let projection = column_ids
            .iter()
            .map(|id| {
                if !seen.insert(*id) {
                    return Err(StoreError::FormatLimit(format!(
                        "projection repeats column id {id}"
                    )));
                }
                self.schema
                    .columns()
                    .iter()
                    .position(|column| column.id() == *id)
                    .ok_or_else(|| {
                        StoreError::FormatLimit(format!("unknown projected column id {id}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut stats = ScanStats::default();
        let mut latest = BTreeMap::new();
        for (segment_index, segment_meta) in self.manifest.segments.iter().enumerate() {
            let overlaps = segment::overlaps_key_range(segment_meta, start, end);
            let point_might_match = start != end
                || segment::might_contain_key(&self.directory, segment_meta, &self.schema, start)?;
            if !overlaps || !point_might_match {
                stats.segments_pruned += 1;
                continue;
            }
            stats.segments_read += 1;
            let scan = segment::read_row_headers_range(
                &self.directory,
                segment_meta,
                &self.schema,
                start,
                end,
            )?;
            stats.blocks_pruned += scan.stats.blocks_pruned;
            stats.blocks_decoded += scan.stats.blocks_decoded;
            for row in scan.rows {
                apply_projected_latest(
                    &mut latest,
                    ProjectedCandidate {
                        key: row.key,
                        version: row.version,
                        deleted: row.deleted,
                        source: ProjectedSource::Segment {
                            segment_index,
                            row_index: row.physical_index,
                        },
                    },
                );
            }
        }
        for (_, row) in self.memtable.range(start.clone()..=end.clone()) {
            apply_projected_latest(
                &mut latest,
                ProjectedCandidate {
                    key: row.key().clone(),
                    version: row.version(),
                    deleted: row.is_deleted(),
                    source: ProjectedSource::Memtable,
                },
            );
        }

        let mut winners = latest
            .into_values()
            .filter(|row| !row.deleted)
            .map(|candidate| (candidate, None))
            .collect::<Vec<_>>();
        let mut segment_rows = BTreeMap::<usize, Vec<(usize, usize)>>::new();
        for (winner_index, (candidate, values)) in winners.iter_mut().enumerate() {
            match candidate.source {
                ProjectedSource::Segment {
                    segment_index,
                    row_index,
                } => segment_rows
                    .entry(segment_index)
                    .or_default()
                    .push((row_index, winner_index)),
                ProjectedSource::Memtable => {
                    let row = self.memtable.get(&candidate.key).ok_or_else(|| {
                        StoreError::FormatLimit(
                            "winning memtable row disappeared from pinned snapshot".into(),
                        )
                    })?;
                    *values = Some(
                        projection
                            .iter()
                            .map(|index| row.values()[*index].clone())
                            .collect(),
                    );
                }
            }
        }
        for (segment_index, mut selected) in segment_rows {
            selected.sort_unstable_by_key(|(row_index, _)| *row_index);
            let row_indices = selected
                .iter()
                .map(|(row_index, _)| *row_index)
                .collect::<Vec<_>>();
            let fetch = segment::read_projected_rows(
                &self.directory,
                &self.manifest.segments[segment_index],
                &self.schema,
                &projection,
                &row_indices,
            )?;
            stats.blocks_decoded += fetch.blocks_decoded;
            for ((_, winner_index), values) in selected.into_iter().zip(fetch.values) {
                winners[winner_index].1 = Some(values);
            }
        }
        let rows = winners
            .into_iter()
            .map(|(row, values)| {
                Ok(ProjectedRow {
                    key: row.key,
                    values: values.ok_or_else(|| {
                        StoreError::FormatLimit("projected winner was not late-materialized".into())
                    })?,
                    version: row.version,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(ProjectedScan { rows, stats })
    }
}

struct ProjectedCandidate {
    key: PrimaryKey,
    version: u64,
    deleted: bool,
    source: ProjectedSource,
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
            if wal_column.data_type != column.data_type() {
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
        if path
            .extension()
            .is_some_and(|extension| extension == "ptseg")
            && !live.contains(file_name)
        {
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
