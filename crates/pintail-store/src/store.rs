use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::AtomicUsize},
};

use fs2::FileExt;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, StoredRow, TableSchema};
use rayon::prelude::*;

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
const DEFAULT_BLOCK_ROWS: usize = 64 * 1024;
const DEFAULT_COMPACTION_FAN_IN: usize = 4;
const DEFAULT_MAX_COMPACTION_ROWS: u64 = 250_000;
const SIZE_TIER_RATIO: u64 = 4;
static PROJECTED_SCAN_POOL: OnceLock<Result<rayon::ThreadPool, String>> = OnceLock::new();

fn projected_scan_pool() -> Result<&'static rayon::ThreadPool, StoreError> {
    PROJECTED_SCAN_POOL
        .get_or_init(|| {
            let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
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
    /// Maximum rows retained in one compaction output buffer and segment.
    pub max_compaction_rows: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            block_rows: DEFAULT_BLOCK_ROWS,
            wal_sync: WalSync::Checkpoint,
            compaction_fan_in: DEFAULT_COMPACTION_FAN_IN,
            max_compaction_rows: DEFAULT_MAX_COMPACTION_ROWS,
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

    /// Moves projected values out of this scan row.
    #[must_use]
    pub fn into_values(self) -> Vec<pintail_types::Value> {
        self.values
    }

    /// Keeps values at the supplied positions in caller order.
    ///
    /// Positions are expected to have been validated against this row's
    /// projected layout.
    #[must_use]
    pub fn project_values(mut self, positions: &[usize]) -> Self {
        self.values = positions
            .iter()
            .map(|position| self.values[*position].clone())
            .collect();
        self
    }

    /// Estimates bytes retained by this projected row.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + std::mem::size_of_val(self.key.parts())
            + self.key.heap_bytes()
            + self.values.capacity() * size_of::<pintail_types::Value>()
            + self
                .values
                .iter()
                .map(pintail_types::Value::heap_bytes)
                .sum::<usize>()
    }
}

/// Physical work performed by a projected range scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanStats {
    segments_pruned: usize,
    segments_read: usize,
    blocks_pruned: usize,
    blocks_read: usize,
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

    /// Returns logical primary-key blocks selected by range zone maps.
    #[must_use]
    pub fn blocks_read(self) -> usize {
        self.blocks_read
    }

    /// Returns blocks whose encoded values were decompressed and decoded.
    #[must_use]
    pub fn blocks_decoded(self) -> usize {
        self.blocks_decoded
    }

    fn add(&mut self, other: Self) {
        self.segments_pruned += other.segments_pruned;
        self.segments_read += other.segments_read;
        self.blocks_pruned += other.blocks_pruned;
        self.blocks_read += other.blocks_read;
        self.blocks_decoded += other.blocks_decoded;
    }
}

/// Rows and physical counters from a projected range scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedScan {
    rows: Vec<ProjectedRow>,
    stats: ScanStats,
    retained_bytes: usize,
}

impl ProjectedScan {
    /// Returns visible projected rows in key order.
    #[must_use]
    pub fn rows(&self) -> &[ProjectedRow] {
        &self.rows
    }

    /// Moves visible projected rows into a pull-based consumer.
    #[must_use]
    pub fn into_rows(self) -> Vec<ProjectedRow> {
        self.rows
    }

    /// Returns bytes retained by the projected row set.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Returns pruning and decoding counters.
    #[must_use]
    pub fn stats(&self) -> ScanStats {
        self.stats
    }
}

/// Pull-based projected scan over immutable segments and WAL-backed rows.
pub struct ProjectedScanStream {
    snapshot: TableSnapshot,
    segments: Vec<segment::SegmentMeta>,
    start: PrimaryKey,
    end: PrimaryKey,
    column_ids: Vec<u32>,
    next_segment: usize,
    pruned_segments: usize,
    merge: Option<MergedProjectedStream>,
}

struct MergedProjectedStream {
    streams: Vec<segment::SegmentRowStream>,
    heads: Vec<Option<StoredRow>>,
    memtable_head: Option<StoredRow>,
    reported_segments: bool,
}

/// One bounded set of projected values from an independently visible segment.
pub struct ProjectedValueChunk {
    rows: Vec<Vec<pintail_types::Value>>,
    stats: ScanStats,
    retained_bytes: usize,
}

/// One bounded column-major projection from an independently visible segment.
pub struct ProjectedColumnChunk {
    columns: Vec<Vec<pintail_types::Value>>,
    row_count: usize,
    stats: ScanStats,
    retained_bytes: usize,
}

impl ProjectedValueChunk {
    /// Returns projected values in physical key order.
    #[must_use]
    pub fn rows(&self) -> &[Vec<pintail_types::Value>] {
        &self.rows
    }

    /// Moves the projected values into the pull-based executor.
    #[must_use]
    pub fn into_rows(self) -> Vec<Vec<pintail_types::Value>> {
        self.rows
    }

    /// Returns bytes retained by the projected values.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Returns pruning and decoding counters for this segment.
    #[must_use]
    pub const fn stats(&self) -> ScanStats {
        self.stats
    }
}

impl ProjectedColumnChunk {
    /// Returns projected columns in query projection order.
    #[must_use]
    pub fn columns(&self) -> &[Vec<pintail_types::Value>] {
        &self.columns
    }

    /// Moves projected columns into a columnar executor.
    #[must_use]
    pub fn into_columns(self) -> Vec<Vec<pintail_types::Value>> {
        self.columns
    }

    /// Returns the number of physical rows represented by the columns.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns bytes retained by the projected columns.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Returns pruning and decoding counters for this segment.
    #[must_use]
    pub const fn stats(&self) -> ScanStats {
        self.stats
    }
}

impl ProjectedScanStream {
    /// Decodes the next independently visible segment within `memory_limit`.
    ///
    /// # Errors
    ///
    /// Returns a precise storage, corruption, schema, or memory-limit error.
    pub fn next_chunk(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<ProjectedValueChunk>, StoreError> {
        let Some(chunk) = self.next_column_chunk(memory_limit)? else {
            return Ok(None);
        };
        let stats = chunk.stats;
        let row_count = chunk.row_count;
        let rows = columns_to_rows(chunk.columns, row_count)?;
        let retained_bytes = size_of::<ProjectedValueChunk>()
            .saturating_add(
                rows.capacity()
                    .saturating_mul(size_of::<Vec<pintail_types::Value>>()),
            )
            .saturating_add(
                rows.iter()
                    .map(|values| {
                        values
                            .capacity()
                            .saturating_mul(size_of::<pintail_types::Value>())
                            .saturating_add(
                                values.iter().map(pintail_types::Value::heap_bytes).sum(),
                            )
                    })
                    .sum(),
            );
        Ok(Some(ProjectedValueChunk {
            rows,
            stats,
            retained_bytes,
        }))
    }

    /// Decodes the next independently visible segment in column-major form.
    ///
    /// # Errors
    ///
    /// Returns a precise storage, corruption, schema, or memory-limit error.
    #[allow(clippy::too_many_lines)]
    pub fn next_column_chunk(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        if self.merge.is_some() {
            return self.next_merged_column_chunk(memory_limit);
        }
        let Some(segment) = self.segments.get(self.next_segment).cloned() else {
            return Ok(None);
        };
        self.next_segment += 1;
        self.decode_column_chunk(segment, memory_limit).map(Some)
    }

    /// Decodes several independently visible segments concurrently.
    ///
    /// The supplied memory budget is divided across the selected segments, so
    /// their aggregate temporary and retained memory cannot exceed it.
    ///
    /// # Errors
    ///
    /// Returns a precise storage, corruption, schema, or memory-limit error.
    pub fn next_column_chunks(
        &mut self,
        max_chunks: usize,
        memory_limit: usize,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        if self.merge.is_some() {
            return Ok(self.next_column_chunk(memory_limit)?.into_iter().collect());
        }
        let max_chunks = if memory_limit < 64 * 1024 * 1024 {
            1
        } else {
            max_chunks
        };
        let chunk_count = max_chunks
            .max(1)
            .min(self.segments.len().saturating_sub(self.next_segment));
        if chunk_count == 0 {
            return Ok(Vec::new());
        }
        let first_segment = self.next_segment;
        let segments = self.segments
            [self.next_segment..self.next_segment.saturating_add(chunk_count)]
            .to_vec();
        self.next_segment = self.next_segment.saturating_add(chunk_count);
        if chunk_count == 1 {
            return segments
                .into_iter()
                .map(|segment| self.decode_column_chunk(segment, memory_limit))
                .collect();
        }
        let per_chunk_limit = memory_limit / chunk_count;
        let decoded = projected_scan_pool()?.install(|| {
            segments
                .into_par_iter()
                .map(|segment| self.decode_column_chunk(segment, per_chunk_limit))
                .collect()
        });
        if matches!(decoded, Err(StoreError::MemoryLimitExceeded { .. })) {
            self.next_segment = first_segment;
            return self.next_column_chunks(chunk_count.div_ceil(2), memory_limit);
        }
        decoded
    }

    #[allow(clippy::too_many_lines)]
    fn next_merged_column_chunk(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        const MAX_MERGED_CHUNK_ROWS: usize = 8 * 1024;
        let projection = self
            .column_ids
            .iter()
            .map(|id| {
                self.snapshot
                    .schema
                    .columns()
                    .iter()
                    .position(|column| column.id() == *id)
                    .ok_or_else(|| {
                        StoreError::FormatLimit(format!("unknown projected column id {id}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let merge = self.merge.as_mut().expect("checked merged scan");
        let chunk_rows = if projection.is_empty() {
            MAX_MERGED_CHUNK_ROWS
        } else {
            memory_limit
                .checked_div(
                    projection
                        .len()
                        .saturating_mul(size_of::<pintail_types::Value>())
                        .saturating_mul(2),
                )
                .unwrap_or(0)
                .clamp(1, MAX_MERGED_CHUNK_ROWS)
        };
        let mut columns = projection
            .iter()
            .map(|_| Vec::with_capacity(chunk_rows))
            .collect::<Vec<_>>();
        let mut row_count = 0;
        while row_count < chunk_rows {
            let minimum = merge
                .heads
                .iter()
                .filter_map(|row| row.as_ref().map(StoredRow::key))
                .chain(merge.memtable_head.as_ref().map(StoredRow::key).into_iter())
                .min()
                .cloned();
            let Some(minimum) = minimum else {
                break;
            };
            let mut winner = None;
            for (stream, head) in merge.streams.iter_mut().zip(&mut merge.heads) {
                while head.as_ref().is_some_and(|row| row.key() == &minimum) {
                    let candidate = head.take().expect("matching stream head");
                    if winner
                        .as_ref()
                        .is_none_or(|current: &StoredRow| candidate.version() >= current.version())
                    {
                        winner = Some(candidate);
                    }
                    *head = stream.next_row()?;
                }
            }
            if merge
                .memtable_head
                .as_ref()
                .is_some_and(|row| row.key() == &minimum)
            {
                let candidate = merge.memtable_head.take().expect("matching memtable head");
                if winner
                    .as_ref()
                    .is_none_or(|current: &StoredRow| candidate.version() >= current.version())
                {
                    winner = Some(candidate);
                }
                merge.memtable_head = self
                    .snapshot
                    .memtable
                    .range((
                        std::ops::Bound::Excluded(minimum.clone()),
                        std::ops::Bound::Included(self.end.clone()),
                    ))
                    .next()
                    .map(|(_, row)| row.clone());
            }
            let winner = winner.expect("minimum key has a winning row");
            if winner.key() < &self.start || winner.key() > &self.end || winner.is_deleted() {
                continue;
            }
            for (output, index) in columns.iter_mut().zip(&projection) {
                output.push(winner.values()[*index].clone());
            }
            row_count += 1;
        }
        if row_count == 0 {
            return Ok(None);
        }
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<Vec<pintail_types::Value>>()),
            )
            .saturating_add(
                columns
                    .iter()
                    .map(|values| {
                        values
                            .capacity()
                            .saturating_mul(size_of::<pintail_types::Value>())
                            .saturating_add(
                                values.iter().map(pintail_types::Value::heap_bytes).sum(),
                            )
                    })
                    .sum(),
            );
        if retained_bytes > memory_limit {
            return Err(StoreError::MemoryLimitExceeded {
                used: 0,
                requested: retained_bytes,
                limit: memory_limit,
            });
        }
        let first_chunk = !std::mem::replace(&mut merge.reported_segments, true);
        Ok(Some(ProjectedColumnChunk {
            columns,
            row_count,
            stats: ScanStats {
                segments_read: usize::from(first_chunk) * self.segments.len(),
                segments_pruned: usize::from(first_chunk) * self.pruned_segments,
                ..ScanStats::default()
            },
            retained_bytes,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_column_chunk(
        &self,
        segment: segment::SegmentMeta,
        memory_limit: usize,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        if self.start <= segment.min_key && self.end >= segment.max_key {
            let projection = self
                .column_ids
                .iter()
                .map(|id| {
                    self.snapshot
                        .schema
                        .columns()
                        .iter()
                        .position(|column| column.id() == *id)
                        .ok_or_else(|| {
                            StoreError::FormatLimit(format!("unknown projected column id {id}"))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let row_count = usize::try_from(segment.row_count)
                .map_err(|_| StoreError::FormatLimit("segment row count exceeds usize".into()))?;
            let scan_memory = AtomicUsize::new(0);
            let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
            let row_index_bytes = row_count.saturating_mul(size_of::<usize>());
            scan_budget.reserve(row_index_bytes)?;
            let row_indices = (0..row_count).collect::<Vec<_>>();
            let fetch = segment::read_projected_rows(
                &self.snapshot.directory,
                &segment,
                &self.snapshot.schema,
                &projection,
                &row_indices,
                &scan_budget,
            )?;
            scan_budget.release(row_index_bytes);
            let retained_bytes = size_of::<ProjectedColumnChunk>()
                .saturating_add(
                    fetch
                        .columns
                        .capacity()
                        .saturating_mul(size_of::<Vec<pintail_types::Value>>()),
                )
                .saturating_add(
                    fetch
                        .columns
                        .iter()
                        .map(|values| {
                            values
                                .capacity()
                                .saturating_mul(size_of::<pintail_types::Value>())
                                .saturating_add(
                                    values.iter().map(pintail_types::Value::heap_bytes).sum(),
                                )
                        })
                        .sum(),
                );
            scan_budget.release(fetch.reserved_bytes);
            scan_budget.reserve(retained_bytes)?;
            return Ok(ProjectedColumnChunk {
                columns: fetch.columns,
                row_count,
                stats: ScanStats {
                    segments_read: 1,
                    blocks_decoded: fetch.blocks_decoded,
                    ..ScanStats::default()
                },
                retained_bytes,
            });
        }
        let mut manifest = self.snapshot.manifest.as_ref().clone();
        manifest.segments = vec![segment];
        let chunk = TableSnapshot {
            memtable: Arc::new(BTreeMap::new()),
            manifest: Arc::new(manifest),
            directory: self.snapshot.directory.clone(),
            schema: self.snapshot.schema.clone(),
        };
        let projected = chunk.scan_projected_range_bounded(
            &self.start,
            &self.end,
            &self.column_ids,
            memory_limit,
        )?;
        let stats = projected.stats();
        let rows = projected
            .into_rows()
            .into_iter()
            .map(ProjectedRow::into_values)
            .collect::<Vec<_>>();
        let row_count = rows.len();
        let columns = rows_to_columns(rows, self.column_ids.len())?;
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<Vec<pintail_types::Value>>()),
            )
            .saturating_add(
                columns
                    .iter()
                    .map(|values| {
                        values
                            .capacity()
                            .saturating_mul(size_of::<pintail_types::Value>())
                            .saturating_add(
                                values.iter().map(pintail_types::Value::heap_bytes).sum(),
                            )
                    })
                    .sum(),
            );
        Ok(ProjectedColumnChunk {
            columns,
            row_count,
            stats,
            retained_bytes,
        })
    }

    /// Returns immutable segments that will be decoded.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Returns immutable segments excluded by key-range or bloom pruning.
    #[must_use]
    pub const fn pruned_segment_count(&self) -> usize {
        self.pruned_segments
    }
}

fn columns_to_rows(
    mut columns: Vec<Vec<pintail_types::Value>>,
    row_count: usize,
) -> Result<Vec<Vec<pintail_types::Value>>, StoreError> {
    if columns.iter().any(|column| column.len() != row_count) {
        return Err(StoreError::FormatLimit(
            "projected column length differs from its segment row count".into(),
        ));
    }
    for column in &mut columns {
        column.reverse();
    }
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        rows.push(
            columns
                .iter_mut()
                .map(|column| {
                    column.pop().ok_or_else(|| {
                        StoreError::FormatLimit("projected column ended before its rows".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(rows)
}

fn rows_to_columns(
    rows: Vec<Vec<pintail_types::Value>>,
    column_count: usize,
) -> Result<Vec<Vec<pintail_types::Value>>, StoreError> {
    let row_count = rows.len();
    let mut columns = (0..column_count)
        .map(|_| Vec::with_capacity(row_count))
        .collect::<Vec<_>>();
    for row in rows {
        if row.len() != column_count {
            return Err(StoreError::FormatLimit(
                "projected row length differs from its projection".into(),
            ));
        }
        for (column, value) in columns.iter_mut().zip(row) {
            column.push(value);
        }
    }
    Ok(columns)
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
        if options.max_compaction_rows == 0 {
            return Err(StoreError::FormatLimit(
                "maximum compaction rows must be non-zero".into(),
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
        self.ingest_at_sequence_with_append_policy(sequence, rows, AppendKeyPolicy::Generate)
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
            segment::Compression::Lz4,
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
    #[allow(clippy::too_many_lines)]
    pub fn compact(&mut self) -> Result<CompactionOutcome, StoreError> {
        let Some(plan) = self.compaction_plan()? else {
            return Ok(CompactionOutcome {
                input_segments: 0,
                output_rows: 0,
                output_path: None,
            });
        };
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
            segment::Compression::Lz4
        };
        let output_row_limit =
            usize::try_from(self.options.max_compaction_rows).unwrap_or(usize::MAX);
        let mut rows = Vec::with_capacity(output_row_limit.min(64 * 1024));
        let mut output_rows = 0_usize;
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
                rows.push(winner);
                output_rows = output_rows.saturating_add(1);
            }
            if rows.len() >= output_row_limit {
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
#[derive(Clone)]
pub struct TableSnapshot {
    memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    manifest: Arc<Manifest>,
    directory: PathBuf,
    schema: TableSchema,
}

/// Immutable files pinned by a reader snapshot for native backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupArtifacts {
    generation: u64,
    manifest: Vec<u8>,
    segments: Vec<BackupSegment>,
}

impl BackupArtifacts {
    /// Returns the pinned manifest generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the encoded storage manifest that references the pinned files.
    #[must_use]
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Returns the immutable segment files referenced by the manifest.
    #[must_use]
    pub fn segments(&self) -> &[BackupSegment] {
        &self.segments
    }
}

/// One immutable storage segment pinned for backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSegment {
    file_name: String,
    path: PathBuf,
}

impl BackupSegment {
    /// Returns the portable segment file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Returns the local path to the pinned segment.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TableSnapshot {
    /// Opens a reader-only snapshot without claiming the table writer lock.
    ///
    /// The reader pins one durable manifest and merges complete WAL records
    /// newer than that manifest. A concurrent manifest publication causes a
    /// bounded retry, so a reader cannot combine an old segment set with a
    /// newly truncated WAL.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing table directory, corrupt manifest,
    /// segment, or WAL, incompatible schema, or repeated concurrent manifest
    /// replacement.
    pub fn open(directory: impl AsRef<Path>, schema: TableSchema) -> Result<Self, StoreError> {
        let directory = std::fs::canonicalize(directory.as_ref())
            .map_err(|error| StoreError::io("canonicalize table reader directory", error))?;
        for _ in 0..8 {
            let manifest = Arc::new(manifest::load(&directory, &schema)?);
            register_pinned_manifest(&directory, &manifest);
            let recovery = crate::wal::recover_read_only(&directory.join(WAL_FILE))?;
            let latest = manifest::load(&directory, &schema)?;
            if manifest.generation != latest.generation
                || manifest.epoch != latest.epoch
                || manifest.flushed_sequence != latest.flushed_sequence
            {
                continue;
            }
            let mut memtable = Memtable::default();
            for batch in recovery.batches {
                if batch.table_id != 0 || batch.sequence <= manifest.flushed_sequence {
                    continue;
                }
                for row in batch.rows {
                    let row = adapt_recovered_row(&schema, &batch.columns, &row)?;
                    memtable.apply(&row);
                }
            }
            let verification = manifest
                .segments
                .iter()
                .try_for_each(|meta| segment::verify(&directory, meta, &schema));
            if let Err(error) = verification {
                let current = manifest::load(&directory, &schema)?;
                if current.generation != manifest.generation || current.epoch != manifest.epoch {
                    continue;
                }
                return Err(error);
            }
            return Ok(Self {
                memtable: memtable.snapshot(),
                manifest,
                directory,
                schema,
            });
        }
        Err(StoreError::FormatLimit(
            "table manifest changed during eight reader-open attempts".to_owned(),
        ))
    }

    /// Returns the catalog schema pinned with this reader snapshot.
    #[must_use]
    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Captures the encoded manifest and immutable segment paths pinned by
    /// this reader. The caller must retain this snapshot while reading the
    /// returned paths so compaction cannot reclaim them.
    ///
    /// # Errors
    ///
    /// Returns an error if the pinned manifest cannot be encoded.
    pub fn backup_artifacts(&self) -> Result<BackupArtifacts, StoreError> {
        let segments = self
            .manifest
            .segments
            .iter()
            .map(|segment| BackupSegment {
                file_name: segment.file_name.clone(),
                path: self.directory.join(&segment.file_name),
            })
            .collect();
        Ok(BackupArtifacts {
            generation: self.manifest.generation,
            manifest: manifest::encode(&self.manifest)?,
            segments,
        })
    }

    /// Returns the minimum and maximum retained storage keys in this snapshot.
    ///
    /// Bounds can include tombstoned keys; they are intended for safe scan
    /// planning rather than visible-row cardinality.
    #[must_use]
    pub fn key_bounds(&self) -> Option<(PrimaryKey, PrimaryKey)> {
        let segment_minimum = self
            .manifest
            .segments
            .iter()
            .map(|segment| &segment.min_key)
            .min();
        let segment_maximum = self
            .manifest
            .segments
            .iter()
            .map(|segment| &segment.max_key)
            .max();
        let memtable_minimum = self.memtable.keys().next();
        let memtable_maximum = self.memtable.keys().next_back();
        let minimum = segment_minimum
            .into_iter()
            .chain(memtable_minimum)
            .min()?
            .clone();
        let maximum = segment_maximum
            .into_iter()
            .chain(memtable_maximum)
            .max()?
            .clone();
        Some((minimum, maximum))
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
        self.scan_projected_range_bounded(start, end, column_ids, usize::MAX)
    }

    /// Opens a bounded pull scan, using a direct segment path when possible
    /// and a block-wise last-write-wins merge otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt point-lookup bloom filter.
    pub fn scan_projected_range_stream(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<Option<ProjectedScanStream>, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for id in column_ids {
            if !seen.insert(*id) {
                return Err(StoreError::FormatLimit(format!(
                    "projection repeats column id {id}"
                )));
            }
            if !self
                .schema
                .columns()
                .iter()
                .any(|column| column.id() == *id)
            {
                return Err(StoreError::FormatLimit(format!(
                    "unknown projected column id {id}"
                )));
            }
        }
        let mut segments = Vec::new();
        let mut pruned_segments = 0;
        let mut independently_visible = self.memtable.is_empty();
        for meta in &self.manifest.segments {
            let overlaps = segment::overlaps_key_range(meta, start, end);
            let point_might_match = start != end
                || segment::might_contain_key(&self.directory, meta, &self.schema, start)?;
            if !overlaps || !point_might_match {
                pruned_segments += 1;
            } else {
                independently_visible &= meta.unique_keys;
                segments.push(meta.clone());
            }
        }
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        independently_visible &= !segments
            .windows(2)
            .any(|pair| pair[0].max_key >= pair[1].min_key);
        let candidate_rows = segments
            .iter()
            .map(|segment| segment.row_count)
            .sum::<u64>()
            .saturating_add(u64::try_from(self.memtable.len()).unwrap_or(u64::MAX));
        if !independently_visible && candidate_rows < 64 * 1024 {
            return Ok(None);
        }
        let merge = if independently_visible {
            None
        } else {
            let mut streams = segments
                .iter()
                .map(|meta| segment::SegmentRowStream::open(&self.directory, meta, &self.schema))
                .collect::<Result<Vec<_>, _>>()?;
            let heads = streams
                .iter_mut()
                .map(segment::SegmentRowStream::next_row)
                .collect::<Result<Vec<_>, _>>()?;
            let memtable_head = self
                .memtable
                .range(start.clone()..=end.clone())
                .next()
                .map(|(_, row)| row.clone());
            Some(MergedProjectedStream {
                streams,
                heads,
                memtable_head,
                reported_segments: false,
            })
        };
        Ok(Some(ProjectedScanStream {
            snapshot: self.clone(),
            segments,
            start: start.clone(),
            end: end.clone(),
            column_ids: column_ids.to_vec(),
            next_segment: 0,
            pruned_segments,
            merge,
        }))
    }

    /// Scans a projected range while enforcing a caller-owned memory budget
    /// over candidate, winner, and late-materialized row state.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::scan_projected_range`], plus
    /// [`StoreError::MemoryLimitExceeded`] before retained scan state crosses
    /// `memory_limit`.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_bounded(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        memory_limit: usize,
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

        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        let scan_pool = projected_scan_pool()?;
        let segment_scans = scan_pool.install(|| {
            self.manifest
                .segments
                .par_iter()
                .enumerate()
                .map(|(segment_index, segment_meta)| {
                    let overlaps = segment::overlaps_key_range(segment_meta, start, end);
                    let point_might_match = start != end
                        || segment::might_contain_key(
                            &self.directory,
                            segment_meta,
                            &self.schema,
                            start,
                        )?;
                    if !overlaps || !point_might_match {
                        return Ok((
                            ScanStats {
                                segments_pruned: 1,
                                ..ScanStats::default()
                            },
                            Vec::new(),
                        ));
                    }
                    let scan = segment::read_row_headers_range(
                        &self.directory,
                        segment_meta,
                        &self.schema,
                        start,
                        end,
                        &scan_budget,
                    )?;
                    let stats = ScanStats {
                        segments_read: 1,
                        blocks_pruned: scan.stats.pruned,
                        blocks_read: scan.stats.read,
                        blocks_decoded: scan.stats.decoded,
                        ..ScanStats::default()
                    };
                    let scan_reserved = scan.reserved_bytes;
                    let candidates = scan
                        .rows
                        .into_iter()
                        .map(|row| {
                            let candidate = ProjectedCandidate {
                                key: row.key,
                                version: row.version,
                                deleted: row.deleted,
                                source: ProjectedSource::Segment {
                                    segment_index,
                                    row_index: row.physical_index,
                                },
                            };
                            scan_budget.reserve(candidate.estimated_bytes())?;
                            Ok(candidate)
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    scan_budget.release(scan_reserved);
                    Ok((stats, candidates))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })?;

        let mut stats = ScanStats::default();
        let mut latest = BTreeMap::new();
        for (segment_stats, candidates) in segment_scans {
            stats.add(segment_stats);
            for candidate in candidates {
                apply_projected_latest(&mut latest, candidate);
            }
        }
        for (_, row) in self.memtable.range(start.clone()..=end.clone()) {
            let candidate_bytes = ProjectedCandidate::estimated_bytes_for_key(row.key());
            scan_budget.reserve(candidate_bytes)?;
            let candidate = ProjectedCandidate {
                key: row.key().clone(),
                version: row.version(),
                deleted: row.is_deleted(),
                source: ProjectedSource::Memtable,
            };
            apply_projected_latest(&mut latest, candidate);
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
                    let projected_bytes = size_of::<Vec<pintail_types::Value>>()
                        .saturating_add(
                            projection
                                .len()
                                .saturating_mul(size_of::<pintail_types::Value>()),
                        )
                        .saturating_add(
                            projection
                                .iter()
                                .map(|index| row.values()[*index].heap_bytes())
                                .fold(0_usize, usize::saturating_add),
                        );
                    scan_budget.reserve(projected_bytes)?;
                    *values = Some(
                        projection
                            .iter()
                            .map(|index| row.values()[*index].clone())
                            .collect(),
                    );
                }
            }
        }
        let segment_fetches = scan_pool.install(|| {
            segment_rows
                .into_iter()
                .collect::<Vec<_>>()
                .into_par_iter()
                .map(|(segment_index, mut selected)| {
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
                        &scan_budget,
                    )?;
                    let fetched_bytes = fetch
                        .columns
                        .iter()
                        .map(|values| {
                            size_of::<Vec<pintail_types::Value>>()
                                + values.len() * size_of::<pintail_types::Value>()
                                + values
                                    .iter()
                                    .map(pintail_types::Value::heap_bytes)
                                    .sum::<usize>()
                        })
                        .sum();
                    let values = columns_to_rows(fetch.columns, selected.len())?;
                    scan_budget.release(fetch.reserved_bytes);
                    scan_budget.reserve(fetched_bytes)?;
                    Ok((selected, values, fetch.blocks_decoded))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })?;
        for (selected, values, blocks_decoded) in segment_fetches {
            stats.blocks_decoded += blocks_decoded;
            for ((_, winner_index), values) in selected.into_iter().zip(values) {
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
        let retained_bytes = size_of::<ProjectedScan>()
            + rows.capacity() * size_of::<ProjectedRow>()
            + rows
                .iter()
                .map(|row| {
                    row.estimated_bytes()
                        .saturating_sub(size_of::<ProjectedRow>())
                })
                .sum::<usize>();
        Ok(ProjectedScan {
            rows,
            stats,
            retained_bytes,
        })
    }
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
