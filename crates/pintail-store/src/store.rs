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

/// One contiguous key-range slice of a scan, classified by how its rows
/// become visible: directly (disjoint unique-key segments untouched by the
/// memtable), through a bounded last-write-wins merge over an overlapping
/// cluster, or from the memtable alone (a gap between clusters).
enum ScanPart {
    Direct {
        segments: Vec<segment::SegmentMeta>,
    },
    /// A contiguous row range of one segment, provably untouched by newer
    /// segments or the memtable (granule-level sweep classification).
    DirectRange {
        segment: segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
    },
    Merge {
        segments: Vec<segment::SegmentMeta>,
        lo: std::ops::Bound<PrimaryKey>,
        hi: std::ops::Bound<PrimaryKey>,
    },
    MemtableOnly {
        lo: std::ops::Bound<PrimaryKey>,
        hi: std::ops::Bound<PrimaryKey>,
    },
}

/// Whether `key` lies within the inclusive/exclusive bound pair.
fn bounds_contain(
    lo: &std::ops::Bound<PrimaryKey>,
    hi: &std::ops::Bound<PrimaryKey>,
    key: &PrimaryKey,
) -> bool {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    (match lo {
        Included(bound) => key >= bound,
        Excluded(bound) => key > bound,
        Unbounded => true,
    }) && (match hi {
        Included(bound) => key <= bound,
        Excluded(bound) => key < bound,
        Unbounded => true,
    })
}

/// Pull-based projected scan over immutable segments and WAL-backed rows.
///
/// The scanned key range is partitioned into [`ScanPart`]s at open time;
/// merge cost is paid only inside clusters whose key ranges actually overlap
/// (docs/decisions.md, "Merge-on-read uses granule-level sweep-line
/// classification").
pub struct ProjectedScanStream {
    snapshot: TableSnapshot,
    segments: Vec<segment::SegmentMeta>,
    start: PrimaryKey,
    end: PrimaryKey,
    column_ids: Vec<u32>,
    next_segment: usize,
    pruned_segments: usize,
    candidate_segments: usize,
    reported_pruned: bool,
    parts: std::collections::VecDeque<ScanPart>,
    memtable_cursor: Option<(std::ops::Bound<PrimaryKey>, std::ops::Bound<PrimaryKey>)>,
    direct_range: Option<(segment::SegmentMeta, u64, u64)>,
    merge: Option<MergedProjectedStream>,
}

struct MergedProjectedStream {
    streams: Vec<segment::SegmentRowStream>,
    heads: Vec<Option<segment::SegmentRowHeader>>,
    memtable_head: Option<StoredRow>,
    reported_segments: bool,
    lo: std::ops::Bound<PrimaryKey>,
    hi: std::ops::Bound<PrimaryKey>,
}

/// Whether `BTreeMap::range((lo, hi))` may be called without panicking and
/// can yield rows: rejects inverted ranges and the empty equal-bound forms.
fn bound_range_is_searchable(
    lo: &std::ops::Bound<PrimaryKey>,
    hi: &std::ops::Bound<PrimaryKey>,
) -> bool {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    let lo_key = match lo {
        Included(key) | Excluded(key) => key,
        Unbounded => return true,
    };
    let hi_key = match hi {
        Included(key) | Excluded(key) => key,
        Unbounded => return true,
    };
    match lo_key.cmp(hi_key) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => matches!((lo, hi), (Included(_), Included(_))),
    }
}

enum MergedWinnerSource {
    Segment {
        segment_index: usize,
        row_index: usize,
    },
    Memtable(Vec<pintail_types::Value>),
}

/// One bounded set of projected values from an independently visible segment.
pub struct ProjectedValueChunk {
    rows: Vec<Vec<pintail_types::Value>>,
    stats: ScanStats,
    retained_bytes: usize,
}

/// Chooses surviving row ranges from a chunk's decoded predicate columns:
/// `Ok(None)` keeps every row (no restriction); ranges must be ascending and
/// disjoint. Errors abort the scan.
pub type PrewhereSelect<'a> = &'a (
        dyn Fn(&[DecodedColumn], usize) -> Result<Option<Vec<std::ops::Range<usize>>>, String>
            + Sync
    );

/// One projected column decoded straight into packed columnar storage.
///
/// Typed variants pad null slots with defaults and carry per-row validity so
/// a columnar executor can adopt them without materializing per-row values;
/// `Values` is the row-value fallback for shapes without a packed layout
/// (Boolean, Binary, merged or memtable rows).
#[derive(Clone, Debug)]
pub enum DecodedColumn {
    /// Row values, one per row.
    Values(Vec<pintail_types::Value>),
    /// Packed signed integers; null slots hold zero.
    Int64 {
        /// One packed value per row.
        values: Vec<i64>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
    /// Packed unsigned integers; null slots hold zero.
    UInt64 {
        /// One packed value per row.
        values: Vec<u64>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
    /// Packed IEEE-754 bit patterns; null slots hold zero.
    Float64 {
        /// One packed bit pattern per row.
        bits: Vec<u64>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
    /// Fixed-width native units decoded from a PTSEG v2 column; canonical
    /// text regenerates through `units.format` only where a consumer needs
    /// it.
    NativeUnits {
        /// The unit interpretation (date days, datetime micros, or scaled
        /// decimal) tied to the column's schema type.
        units: crate::segment::NativeUnits,
        /// One packed unit value per row; null slots hold zero.
        values: Vec<i64>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
    /// Dictionary-coded UTF-8: `codes[i]` indexes the (small) distinct-entry
    /// arena; null rows hold code 0 with `validity` false. Produced when a
    /// column's blocks arrive dictionary-encoded, so 20M rows of a 5-value
    /// column ship as 20M u32s plus a few entry bytes.
    DictionaryUtf8 {
        /// Distinct entry bytes.
        dict_heap: Vec<u8>,
        /// `entries + 1` boundaries into `dict_heap`.
        dict_offsets: Vec<usize>,
        /// One entry index per row.
        codes: Vec<u32>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
    /// UTF-8 bytes in one arena; row `i` spans `heap[offsets[i]..offsets[i+1]]`
    /// and null rows span zero bytes.
    Utf8 {
        /// Concatenated UTF-8 payloads.
        heap: Vec<u8>,
        /// `len + 1` row boundaries into `heap`.
        offsets: Vec<usize>,
        /// Per-row null mask (`true` = non-null).
        validity: Vec<bool>,
    },
}

impl DecodedColumn {
    /// Returns the number of rows in the column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Values(values) => values.len(),
            Self::Int64 { validity, .. }
            | Self::UInt64 { validity, .. }
            | Self::Float64 { validity, .. }
            | Self::NativeUnits { validity, .. }
            | Self::DictionaryUtf8 { validity, .. }
            | Self::Utf8 { validity, .. } => validity.len(),
        }
    }

    /// Returns whether the column has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Estimates bytes retained by the column's owned buffers.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Values(values) => values
                .capacity()
                .saturating_mul(size_of::<pintail_types::Value>())
                .saturating_add(values.iter().map(pintail_types::Value::heap_bytes).sum()),
            Self::Int64 { values, validity }
            | Self::NativeUnits {
                values, validity, ..
            } => values
                .capacity()
                .saturating_mul(size_of::<i64>())
                .saturating_add(validity.capacity()),
            Self::UInt64 { values, validity } => values
                .capacity()
                .saturating_mul(size_of::<u64>())
                .saturating_add(validity.capacity()),
            Self::Float64 { bits, validity } => bits
                .capacity()
                .saturating_mul(size_of::<u64>())
                .saturating_add(validity.capacity()),
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => heap
                .capacity()
                .saturating_add(offsets.capacity().saturating_mul(size_of::<usize>()))
                .saturating_add(validity.capacity()),
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => dict_heap
                .capacity()
                .saturating_add(dict_offsets.capacity().saturating_mul(size_of::<usize>()))
                .saturating_add(codes.capacity().saturating_mul(size_of::<u32>()))
                .saturating_add(validity.capacity()),
        }
    }

    /// Splits off the first `count` rows (clamped to the column length),
    /// leaving the remainder in place. Used by executors slicing one decoded
    /// chunk into fixed-size batches.
    #[must_use]
    pub fn take_prefix(&mut self, count: usize) -> Self {
        let count = count.min(self.len());
        match self {
            Self::Values(values) => {
                let rest = values.split_off(count);
                Self::Values(std::mem::replace(values, rest))
            }
            Self::Int64 { values, validity } => {
                let rest_values = values.split_off(count);
                let rest_validity = validity.split_off(count);
                Self::Int64 {
                    values: std::mem::replace(values, rest_values),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
            Self::NativeUnits {
                units,
                values,
                validity,
            } => {
                let rest_values = values.split_off(count);
                let rest_validity = validity.split_off(count);
                Self::NativeUnits {
                    units: *units,
                    values: std::mem::replace(values, rest_values),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
            Self::UInt64 { values, validity } => {
                let rest_values = values.split_off(count);
                let rest_validity = validity.split_off(count);
                Self::UInt64 {
                    values: std::mem::replace(values, rest_values),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
            Self::Float64 { bits, validity } => {
                let rest_bits = bits.split_off(count);
                let rest_validity = validity.split_off(count);
                Self::Float64 {
                    bits: std::mem::replace(bits, rest_bits),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => {
                let rest_codes = codes.split_off(count);
                let rest_validity = validity.split_off(count);
                Self::DictionaryUtf8 {
                    dict_heap: dict_heap.clone(),
                    dict_offsets: dict_offsets.clone(),
                    codes: std::mem::replace(codes, rest_codes),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => {
                let cut = offsets[count];
                let rest_heap = heap.split_off(cut);
                let rest_offsets = offsets[count..]
                    .iter()
                    .map(|offset| offset - cut)
                    .collect::<Vec<_>>();
                offsets.truncate(count + 1);
                let rest_validity = validity.split_off(count);
                Self::Utf8 {
                    heap: std::mem::replace(heap, rest_heap),
                    offsets: std::mem::replace(offsets, rest_offsets),
                    validity: std::mem::replace(validity, rest_validity),
                }
            }
        }
    }

    /// Materializes one row's value, or `None` past the end.
    ///
    /// # Panics
    ///
    /// Panics if stored native units cannot regenerate their text, which the
    /// writer's round-trip probe makes impossible.
    #[must_use]
    pub fn value_at(&self, row: usize) -> Option<pintail_types::Value> {
        if row >= self.len() {
            return None;
        }
        Some(match self {
            Self::Values(values) => values[row].clone(),
            Self::Int64 { values, validity } => {
                if validity[row] {
                    pintail_types::Value::Int64(values[row])
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::UInt64 { values, validity } => {
                if validity[row] {
                    pintail_types::Value::UInt64(values[row])
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::Float64 { bits, validity } => {
                if validity[row] {
                    pintail_types::Value::Float64(pintail_types::Float64::new(f64::from_bits(
                        bits[row],
                    )))
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::NativeUnits {
                units,
                values,
                validity,
            } => {
                if validity[row] {
                    let text = units
                        .format(values[row])
                        .expect("stored native units round-trip");
                    pintail_types::Value::Utf8(text)
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => {
                if validity[row] {
                    let code = codes[row] as usize;
                    let bytes = dict_heap[dict_offsets[code]..dict_offsets[code + 1]].to_vec();
                    let text = String::from_utf8(bytes).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    });
                    pintail_types::Value::Utf8(text)
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => {
                if validity[row] {
                    let bytes = heap[offsets[row]..offsets[row + 1]].to_vec();
                    let text = String::from_utf8(bytes).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    });
                    pintail_types::Value::Utf8(text)
                } else {
                    pintail_types::Value::Null
                }
            }
        })
    }

    /// Materializes the column into per-row values.
    ///
    /// # Panics
    ///
    /// Panics if stored native units cannot regenerate their text, which the
    /// writer's round-trip probe makes impossible.
    #[must_use]
    pub fn into_values(self) -> Vec<pintail_types::Value> {
        match self {
            Self::Values(values) => values,
            Self::Int64 { values, validity } => values
                .into_iter()
                .zip(validity)
                .map(|(value, valid)| {
                    if valid {
                        pintail_types::Value::Int64(value)
                    } else {
                        pintail_types::Value::Null
                    }
                })
                .collect(),
            Self::UInt64 { values, validity } => values
                .into_iter()
                .zip(validity)
                .map(|(value, valid)| {
                    if valid {
                        pintail_types::Value::UInt64(value)
                    } else {
                        pintail_types::Value::Null
                    }
                })
                .collect(),
            Self::Float64 { bits, validity } => bits
                .into_iter()
                .zip(validity)
                .map(|(bits, valid)| {
                    if valid {
                        pintail_types::Value::Float64(pintail_types::Float64::new(f64::from_bits(
                            bits,
                        )))
                    } else {
                        pintail_types::Value::Null
                    }
                })
                .collect(),
            Self::NativeUnits {
                units,
                values,
                validity,
            } => values
                .into_iter()
                .zip(validity)
                .map(|(value, valid)| {
                    if valid {
                        pintail_types::Value::Utf8(
                            units.format(value).expect("stored native units round-trip"),
                        )
                    } else {
                        pintail_types::Value::Null
                    }
                })
                .collect(),
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => codes
                .iter()
                .zip(validity)
                .map(|(code, valid)| {
                    if valid {
                        let code = *code as usize;
                        let bytes = dict_heap[dict_offsets[code]..dict_offsets[code + 1]].to_vec();
                        let text = String::from_utf8(bytes).unwrap_or_else(|error| {
                            String::from_utf8_lossy(error.as_bytes()).into_owned()
                        });
                        pintail_types::Value::Utf8(text)
                    } else {
                        pintail_types::Value::Null
                    }
                })
                .collect(),
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => validity
                .iter()
                .enumerate()
                .map(|(row, valid)| {
                    if !valid {
                        return pintail_types::Value::Null;
                    }
                    let bytes = heap[offsets[row]..offsets[row + 1]].to_vec();
                    // Arena bytes were UTF-8-validated at block decode; the
                    // lossy fallback never fires but avoids a panic path.
                    let text = String::from_utf8(bytes).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    });
                    pintail_types::Value::Utf8(text)
                })
                .collect(),
        }
    }
}

/// One bounded column-major projection from an independently visible segment.
pub struct ProjectedColumnChunk {
    columns: Vec<DecodedColumn>,
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
    pub fn columns(&self) -> &[DecodedColumn] {
        &self.columns
    }

    /// Moves the packed projected columns into a columnar executor.
    #[must_use]
    pub fn into_decoded_columns(self) -> Vec<DecodedColumn> {
        self.columns
    }

    /// Materializes projected columns into per-row values.
    #[must_use]
    pub fn into_columns(self) -> Vec<Vec<pintail_types::Value>> {
        self.columns
            .into_iter()
            .map(DecodedColumn::into_values)
            .collect()
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
        let rows = columns_to_rows(
            chunk
                .columns
                .into_iter()
                .map(DecodedColumn::into_values)
                .collect(),
            row_count,
        )?;
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
        loop {
            if self.merge.is_some() {
                if let Some(chunk) = self.next_merged_column_chunk(memory_limit)? {
                    return Ok(Some(chunk));
                }
                self.merge = None;
            } else if self.memtable_cursor.is_some() {
                if let Some(chunk) = self.next_memtable_chunk(memory_limit)? {
                    return Ok(Some(chunk));
                }
                self.memtable_cursor = None;
            } else if let Some((segment, start_row, end_row)) = self.direct_range.take() {
                return self
                    .decode_column_chunk_rows(&segment, start_row, end_row, memory_limit)
                    .map(Some);
            } else if let Some(segment) = self.segments.get(self.next_segment).cloned() {
                self.next_segment += 1;
                return self.decode_column_chunk(segment, memory_limit).map(Some);
            }
            if !self.advance_part()? {
                return Ok(None);
            }
        }
    }

    /// Activates the next classified scan part, returning `false` at the end.
    fn advance_part(&mut self) -> Result<bool, StoreError> {
        let Some(part) = self.parts.pop_front() else {
            return Ok(false);
        };
        self.merge = None;
        self.memtable_cursor = None;
        self.direct_range = None;
        match part {
            ScanPart::Direct { segments } => {
                self.segments = segments;
                self.next_segment = 0;
            }
            ScanPart::DirectRange {
                segment,
                start_row,
                end_row,
            } => {
                self.segments = Vec::new();
                self.next_segment = 0;
                self.direct_range = Some((segment, start_row, end_row));
            }
            ScanPart::Merge { segments, lo, hi } => {
                let mut streams = segments
                    .iter()
                    .map(|meta| {
                        segment::SegmentRowStream::open_headers(
                            &self.snapshot.directory,
                            meta,
                            &self.snapshot.schema,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let heads = streams
                    .iter_mut()
                    .map(segment::SegmentRowStream::next_header)
                    .collect::<Result<Vec<_>, _>>()?;
                let memtable_head = if bound_range_is_searchable(&lo, &hi) {
                    self.snapshot
                        .memtable
                        .range((lo.clone(), hi.clone()))
                        .next()
                        .map(|(_, row)| row.clone())
                } else {
                    None
                };
                self.segments = segments;
                self.next_segment = self.segments.len();
                self.merge = Some(MergedProjectedStream {
                    streams,
                    heads,
                    memtable_head,
                    reported_segments: false,
                    lo,
                    hi,
                });
            }
            ScanPart::MemtableOnly { lo, hi } => {
                self.segments = Vec::new();
                self.next_segment = 0;
                self.memtable_cursor = Some((lo, hi));
            }
        }
        Ok(true)
    }

    /// Produces the next chunk of memtable-resident rows for a gap part.
    fn next_memtable_chunk(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        const MAX_MEMTABLE_CHUNK_ROWS: usize = 8 * 1024;
        let Some((lo, hi)) = self.memtable_cursor.clone() else {
            return Ok(None);
        };
        if !bound_range_is_searchable(&lo, &hi) {
            self.memtable_cursor = None;
            return Ok(None);
        }
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
        let chunk_rows = if projection.is_empty() {
            MAX_MEMTABLE_CHUNK_ROWS
        } else {
            memory_limit
                .checked_div(
                    projection
                        .len()
                        .saturating_mul(size_of::<pintail_types::Value>())
                        .saturating_mul(2),
                )
                .unwrap_or(0)
                .clamp(1, MAX_MEMTABLE_CHUNK_ROWS)
        };
        let mut columns = projection
            .iter()
            .map(|_| Vec::new())
            .collect::<Vec<Vec<pintail_types::Value>>>();
        let mut row_count = 0usize;
        let mut last_key = None;
        for (key, row) in self.snapshot.memtable.range((lo, hi.clone())) {
            last_key = Some(key.clone());
            if row.is_deleted() {
                continue;
            }
            for (column, position) in columns.iter_mut().zip(&projection) {
                column.push(row.values()[*position].clone());
            }
            row_count += 1;
            if row_count >= chunk_rows {
                break;
            }
        }
        match last_key {
            Some(key) => {
                self.memtable_cursor = Some((std::ops::Bound::Excluded(key), hi));
            }
            None => self.memtable_cursor = None,
        }
        if row_count == 0 {
            // A window that held only tombstones: continue into the next
            // window, or finish the part when the range is drained.
            if self.memtable_cursor.is_some() {
                return self.next_memtable_chunk(memory_limit);
            }
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
        Ok(Some(ProjectedColumnChunk {
            columns: columns.into_iter().map(DecodedColumn::Values).collect(),
            row_count,
            stats: ScanStats::default(),
            retained_bytes,
        }))
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
        self.next_column_chunks_inner(max_chunks, memory_limit, None)
    }

    /// Like [`Self::next_column_chunks`], but full direct segments decode
    /// filter-first: the predicate columns decode alone, `select` chooses the
    /// surviving row ranges (or `None` to keep everything), and only those
    /// ranges of the full projection decode afterwards.
    ///
    /// # Errors
    ///
    /// Returns a precise storage, corruption, schema, or memory-limit error.
    pub fn next_column_chunks_filtered(
        &mut self,
        max_chunks: usize,
        memory_limit: usize,
        predicate_ids: &[u32],
        select: PrewhereSelect<'_>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        self.next_column_chunks_inner(max_chunks, memory_limit, Some((predicate_ids, select)))
    }

    fn next_column_chunks_inner(
        &mut self,
        max_chunks: usize,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        if self.merge.is_some() || self.memtable_cursor.is_some() || self.direct_range.is_some() {
            return Ok(self.next_column_chunk(memory_limit)?.into_iter().collect());
        }
        if self.next_segment >= self.segments.len() {
            if !self.advance_part()? {
                return Ok(Vec::new());
            }
            return self.next_column_chunks_inner(max_chunks, memory_limit, prewhere);
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
                .map(|segment| {
                    self.decode_column_chunk_maybe_filtered(segment, memory_limit, prewhere)
                })
                .collect();
        }
        let per_chunk_limit = memory_limit / chunk_count;
        let decoded = projected_scan_pool()?.install(|| {
            segments
                .into_par_iter()
                .map(|segment| {
                    self.decode_column_chunk_maybe_filtered(segment, per_chunk_limit, prewhere)
                })
                .collect()
        });
        if matches!(decoded, Err(StoreError::MemoryLimitExceeded { .. })) {
            self.next_segment = first_segment;
            return self.next_column_chunks_inner(chunk_count.div_ceil(2), memory_limit, prewhere);
        }
        decoded
    }

    /// Routes one segment through the filter-first path when a predicate
    /// selector applies and the segment decodes as a full direct chunk.
    fn decode_column_chunk_maybe_filtered(
        &self,
        segment: segment::SegmentMeta,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let full_direct = self.start <= segment.min_key && self.end >= segment.max_key;
        if let Some((predicate_ids, select)) = prewhere
            && full_direct
            && !predicate_ids.is_empty()
        {
            let map_projection = |ids: &[u32]| -> Result<Vec<usize>, StoreError> {
                ids.iter()
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
                    .collect()
            };
            let predicate_projection = map_projection(predicate_ids)?;
            let row_count = usize::try_from(segment.row_count)
                .map_err(|_| StoreError::FormatLimit("segment row count exceeds usize".into()))?;
            let scan_memory = AtomicUsize::new(0);
            let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
            let fetch = segment::read_projected_columns(
                &self.snapshot.directory,
                &segment,
                &self.snapshot.schema,
                &predicate_projection,
                0,
                row_count,
                &scan_budget,
            )?;
            let predicate_blocks = fetch.blocks_decoded;
            let ranges = select(&fetch.columns, row_count).map_err(StoreError::FormatLimit)?;
            let predicate_reserved = fetch.reserved_bytes;
            drop(fetch);
            scan_budget.release(predicate_reserved);
            if let Some(ranges) = ranges {
                let projection = map_projection(&self.column_ids)?;
                let fetch = segment::read_projected_column_ranges(
                    &self.snapshot.directory,
                    &segment,
                    &self.snapshot.schema,
                    &projection,
                    &ranges,
                    &scan_budget,
                )?;
                let retained_bytes = size_of::<ProjectedColumnChunk>()
                    .saturating_add(
                        fetch
                            .columns
                            .capacity()
                            .saturating_mul(size_of::<DecodedColumn>()),
                    )
                    .saturating_add(
                        fetch
                            .columns
                            .iter()
                            .map(DecodedColumn::retained_bytes)
                            .sum(),
                    );
                scan_budget.release(fetch.reserved_bytes);
                scan_budget.reserve(retained_bytes)?;
                return Ok(ProjectedColumnChunk {
                    columns: fetch.columns,
                    row_count: ranges.iter().map(std::iter::ExactSizeIterator::len).sum(),
                    stats: ScanStats {
                        segments_read: 1,
                        blocks_read: fetch.blocks_read,
                        blocks_pruned: fetch.blocks_pruned,
                        blocks_decoded: predicate_blocks + fetch.blocks_decoded,
                        ..ScanStats::default()
                    },
                    retained_bytes,
                });
            }
        }
        self.decode_column_chunk(segment, memory_limit)
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
        let part_lo = merge.lo.clone();
        let part_hi = merge.hi.clone();
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
        let mut winner_sources = Vec::with_capacity(chunk_rows);
        while winner_sources.len() < chunk_rows {
            let minimum = merge
                .heads
                .iter()
                .filter_map(|row| row.as_ref().map(|row| &row.key))
                .chain(merge.memtable_head.as_ref().map(StoredRow::key))
                .min()
                .cloned();
            let Some(minimum) = minimum else {
                break;
            };
            let mut winner = None::<(u64, bool, MergedWinnerSource)>;
            for (segment_index, (stream, head)) in
                merge.streams.iter_mut().zip(&mut merge.heads).enumerate()
            {
                while head.as_ref().is_some_and(|row| row.key == minimum) {
                    let candidate = head.take().expect("matching stream head");
                    if winner
                        .as_ref()
                        .is_none_or(|current| candidate.version >= current.0)
                    {
                        winner = Some((
                            candidate.version,
                            candidate.deleted,
                            MergedWinnerSource::Segment {
                                segment_index,
                                row_index: candidate.physical_index,
                            },
                        ));
                    }
                    *head = stream.next_header()?;
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
                    .is_none_or(|current| candidate.version() >= current.0)
                {
                    winner = Some((
                        candidate.version(),
                        candidate.is_deleted(),
                        MergedWinnerSource::Memtable(
                            projection
                                .iter()
                                .map(|index| candidate.values()[*index].clone())
                                .collect(),
                        ),
                    ));
                }
                let reseek_lo = std::ops::Bound::Excluded(minimum.clone());
                merge.memtable_head = if bound_range_is_searchable(&reseek_lo, &part_hi) {
                    self.snapshot
                        .memtable
                        .range((reseek_lo, part_hi.clone()))
                        .next()
                        .map(|(_, row)| row.clone())
                } else {
                    None
                };
            }
            let winner = winner.expect("minimum key has a winning row");
            if !bounds_contain(&part_lo, &part_hi, &minimum) || winner.1 {
                continue;
            }
            winner_sources.push(winner.2);
        }
        let row_count = winner_sources.len();
        if row_count == 0 {
            return Ok(None);
        }
        let first_chunk = !std::mem::replace(&mut merge.reported_segments, true);
        let report_pruned = first_chunk && !std::mem::replace(&mut self.reported_pruned, true);
        let mut winner_values = vec![None; row_count];
        let mut segment_rows = BTreeMap::<usize, Vec<(usize, usize)>>::new();
        for (winner_index, source) in winner_sources.into_iter().enumerate() {
            match source {
                MergedWinnerSource::Segment { .. } if projection.is_empty() => {
                    winner_values[winner_index] = Some(Vec::new());
                }
                MergedWinnerSource::Segment {
                    segment_index,
                    row_index,
                } => segment_rows
                    .entry(segment_index)
                    .or_default()
                    .push((row_index, winner_index)),
                MergedWinnerSource::Memtable(values) => {
                    winner_values[winner_index] = Some(values);
                }
            }
        }
        let mut blocks_decoded = 0;
        for (segment_index, selected) in segment_rows {
            let row_indices = selected
                .iter()
                .map(|(row_index, _)| *row_index)
                .collect::<Vec<_>>();
            let scan_memory = AtomicUsize::new(0);
            let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
            let fetch = segment::read_projected_rows(
                &self.snapshot.directory,
                &self.segments[segment_index],
                &self.snapshot.schema,
                &projection,
                &row_indices,
                &scan_budget,
            )?;
            blocks_decoded += fetch.blocks_decoded;
            let values = columns_to_rows(fetch.columns, selected.len())?;
            scan_budget.release(fetch.reserved_bytes);
            for ((_, winner_index), values) in selected.into_iter().zip(values) {
                winner_values[winner_index] = Some(values);
            }
        }
        let mut columns = projection
            .iter()
            .map(|_| Vec::with_capacity(row_count))
            .collect::<Vec<_>>();
        for values in winner_values {
            let values = values.ok_or_else(|| {
                StoreError::FormatLimit("merged winner was not late-materialized".into())
            })?;
            for (column, value) in columns.iter_mut().zip(values) {
                column.push(value);
            }
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
        Ok(Some(ProjectedColumnChunk {
            columns: columns.into_iter().map(DecodedColumn::Values).collect(),
            row_count,
            stats: ScanStats {
                segments_read: usize::from(first_chunk) * self.segments.len(),
                segments_pruned: usize::from(report_pruned) * self.pruned_segments,
                blocks_decoded,
                ..ScanStats::default()
            },
            retained_bytes,
        }))
    }

    /// Decodes one contiguous row range of a segment (a granule-classified
    /// direct part) into a column chunk, bypassing merge machinery.
    fn decode_column_chunk_rows(
        &self,
        segment: &segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
        memory_limit: usize,
    ) -> Result<ProjectedColumnChunk, StoreError> {
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
        let start = usize::try_from(start_row)
            .map_err(|_| StoreError::FormatLimit("range start exceeds usize".into()))?;
        let end = usize::try_from(end_row)
            .map_err(|_| StoreError::FormatLimit("range end exceeds usize".into()))?;
        let row_count = end.saturating_sub(start);
        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        let fetch = segment::read_projected_columns(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            &projection,
            start,
            end,
            &scan_budget,
        )?;
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                fetch
                    .columns
                    .capacity()
                    .saturating_mul(size_of::<DecodedColumn>()),
            )
            .saturating_add(
                fetch
                    .columns
                    .iter()
                    .map(DecodedColumn::retained_bytes)
                    .sum(),
            );
        scan_budget.release(fetch.reserved_bytes);
        scan_budget.reserve(retained_bytes)?;
        Ok(ProjectedColumnChunk {
            columns: fetch.columns,
            row_count,
            stats: ScanStats {
                segments_read: 1,
                blocks_decoded: fetch.blocks_decoded,
                blocks_read: fetch.blocks_read,
                blocks_pruned: fetch.blocks_pruned,
                ..ScanStats::default()
            },
            retained_bytes,
        })
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
            let fetch = segment::read_projected_columns(
                &self.snapshot.directory,
                &segment,
                &self.snapshot.schema,
                &projection,
                0,
                row_count,
                &scan_budget,
            )?;
            let retained_bytes = size_of::<ProjectedColumnChunk>()
                .saturating_add(
                    fetch
                        .columns
                        .capacity()
                        .saturating_mul(size_of::<DecodedColumn>()),
                )
                .saturating_add(
                    fetch
                        .columns
                        .iter()
                        .map(DecodedColumn::retained_bytes)
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
                    blocks_read: fetch.blocks_read,
                    blocks_pruned: fetch.blocks_pruned,
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
        let columns = rows_to_columns(rows, self.column_ids.len())?
            .into_iter()
            .map(DecodedColumn::Values)
            .collect::<Vec<_>>();
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<DecodedColumn>()),
            )
            .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum());
        Ok(ProjectedColumnChunk {
            columns,
            row_count,
            stats,
            retained_bytes,
        })
    }

    /// Returns the scanned key range.
    #[must_use]
    pub fn key_range(&self) -> (&PrimaryKey, &PrimaryKey) {
        (&self.start, &self.end)
    }

    /// Returns the projected stable column IDs in output order.
    #[must_use]
    pub fn column_ids(&self) -> &[u32] {
        &self.column_ids
    }

    /// Returns the snapshot this stream decodes from.
    #[must_use]
    pub const fn snapshot(&self) -> &TableSnapshot {
        &self.snapshot
    }

    /// Returns immutable segments that will be decoded.
    #[must_use]
    pub const fn segment_count(&self) -> usize {
        self.candidate_segments
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
            if self.manifest.segments.len() >= self.options.compaction_fan_in {
                self.compact()?;
            }
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

        // A flushed segment provably holds one row per key (the memtable is a
        // map) and `unique_keys` would let scans take the columnar direct
        // path — worth 6.9x on a 20M-row scan when it was tried. It is off
        // because the direct path decodes a whole segment in one reservation,
        // so a query with a small ceiling that previously streamed through the
        // chunked merge path fails instead: `MemoryLimitExceeded { requested:
        // 263280, limit: 65536 }` in the storage key-pruning tests. Turning
        // this on requires the direct path to size its work to the budget
        // first; see the notes in experiments/RESULTS.md (e24 follow-up 2).
        let unique_keys = false;
        let segment = segment::write(
            &self.directory,
            self.manifest.next_segment_id,
            &self.schema,
            &rows,
            self.options.block_rows,
            segment::Compression::Lz4,
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
            segment::Compression::Lz4
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
    /// The snapshot's data identity when every visible row is
    /// segment-resident: `(table directory, manifest generation)` with an
    /// empty memtable. Two snapshots with the same identity see byte-for-
    /// byte identical data, so exactness-preserving caches (the settled
    /// aggregate memo) key on it; any ingest or flush changes it.
    #[must_use]
    pub fn settled_identity(&self) -> Option<(&std::path::Path, u64)> {
        self.memtable
            .is_empty()
            .then(|| (self.directory.as_path(), self.manifest.generation))
    }

    /// Per-segment SMAs plus residual memtable rows, when the fold is
    /// provably exact under merge-on-read (WS3-B, docs/decisions.md):
    /// every segment carries SMAs and zero tombstones, segment key ranges
    /// are pairwise disjoint (no cross-segment overlays), and every
    /// memtable row is a pure insert above the whole segment key space.
    /// Any tombstone, overlap, or update returns `None` — MIN/MAX cannot
    /// be delta-adjusted under deletes, so the fold never tries.
    #[must_use]
    pub fn sma_fold_state(&self) -> Option<(Vec<&crate::segment::SegmentSmas>, Vec<&StoredRow>)> {
        let mut segments: Vec<&crate::segment::SegmentMeta> =
            self.manifest.segments.iter().collect();
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        for pair in segments.windows(2) {
            if pair[1].min_key <= pair[0].max_key {
                return None;
            }
        }
        let mut smas = Vec::with_capacity(segments.len());
        for meta in &segments {
            let sma = meta.smas.as_ref()?;
            if sma.tombstones != 0 {
                return None;
            }
            smas.push(sma);
        }
        let max_segment_key = segments.last().map(|meta| &meta.max_key);
        let mut rows = Vec::with_capacity(self.memtable.len());
        for row in self.memtable.values() {
            if row.is_deleted() || max_segment_key.is_some_and(|max| row.key() <= max) {
                return None;
            }
            rows.push(row);
        }
        Some((smas, rows))
    }

    /// The segment-resident identity plus the memtable rows, when every
    /// memtable row is a pure insert above the segment key space (no
    /// tombstones, no updates of segment rows). The delta-maintained
    /// aggregate memo merges these rows onto the generation-keyed result;
    /// any overlap or delete makes the merge unsound and returns `None`.
    #[must_use]
    pub fn insert_only_delta(&self) -> Option<(&std::path::Path, u64, Vec<&StoredRow>)> {
        if self.memtable.is_empty() {
            return None;
        }
        let max_segment_key = self
            .manifest
            .segments
            .iter()
            .map(|meta| &meta.max_key)
            .max();
        let mut rows = Vec::with_capacity(self.memtable.len());
        for row in self.memtable.values() {
            if row.is_deleted() || max_segment_key.is_some_and(|max| row.key() <= max) {
                return None;
            }
            rows.push(row);
        }
        Some((self.directory.as_path(), self.manifest.generation, rows))
    }

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

    /// Reports, per live segment, whether skipping it on scan-predicate
    /// statistics alone is sound.
    ///
    /// Skipping is safe only for a segment whose key range no other live
    /// segment touches. Where ranges overlap, the skipped segment may hold
    /// the winning version of a key whose older, predicate-matching version
    /// survives in a segment that is still read, which would emit a stale
    /// row. Deciding this per segment rather than for the whole manifest
    /// matters at scale: a large table under continuous replication almost
    /// always has some overlap somewhere, and a single overlapping pair used
    /// to disable pruning for every other segment.
    fn value_prunable_segments(&self) -> Vec<bool> {
        let segments = &self.manifest.segments;
        let mut order = (0..segments.len()).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            segments[*left]
                .min_key
                .cmp(&segments[*right].min_key)
                .then_with(|| segments[*left].max_key.cmp(&segments[*right].max_key))
        });
        let mut prunable = vec![false; segments.len()];
        let mut highest_end: Option<&PrimaryKey> = None;
        for (position, index) in order.iter().copied().enumerate() {
            let meta = &segments[index];
            let touches_earlier = highest_end.is_some_and(|end| end >= &meta.min_key);
            let touches_later = order
                .get(position + 1)
                .is_some_and(|next| segments[*next].min_key <= meta.max_key);
            prunable[index] = !touches_earlier && !touches_later;
            if highest_end.is_none_or(|end| end < &meta.max_key) {
                highest_end = Some(&meta.max_key);
            }
        }
        prunable
    }

    /// Scans an inclusive key range while decoding only requested user
    /// columns after segment and key-block pruning.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate/unknown column ID,
    /// incompatible schema, corrupt block, or filesystem failure.
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
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_stream(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<Option<ProjectedScanStream>, StoreError> {
        self.scan_projected_range_stream_pruned(start, end, column_ids, &[])
    }

    /// [`Self::scan_projected_range_stream`] with scan-predicate value
    /// bounds: segments whose statistics prove every row fails a bound are
    /// skipped without decoding. Value pruning engages only on manifests
    /// whose segments have pairwise-disjoint key ranges and no tombstones —
    /// under overlapping row versions a skipped segment could hide the
    /// winning version of another segment's key.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt point-lookup bloom filter.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_stream_pruned(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        bounds: &[crate::segment::ColumnBounds],
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
        let prunable = if bounds.is_empty() {
            Vec::new()
        } else {
            self.value_prunable_segments()
        };
        for (index, meta) in self.manifest.segments.iter().enumerate() {
            let overlaps = segment::overlaps_key_range(meta, start, end);
            let point_might_match = start != end
                || segment::might_contain_key(&self.directory, meta, &self.schema, start)?;
            let value_disjoint = prunable.get(index).copied().unwrap_or(false)
                && segment::sma_disjoint(meta, bounds);
            if !overlaps || !point_might_match || value_disjoint {
                pruned_segments += 1;
            } else {
                segments.push(meta.clone());
            }
        }
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        let candidate_rows = segments
            .iter()
            .map(|segment| segment.row_count)
            .sum::<u64>()
            .saturating_add(u64::try_from(self.memtable.len()).unwrap_or(u64::MAX));

        // Partition [start, end] into contiguous parts by a sweep over the
        // sorted segment key ranges: clusters of overlapping segments merge
        // only within their own bounds; everything between clusters is served
        // directly or from the memtable alone (docs/decisions.md,
        // "Merge-on-read uses granule-level sweep-line classification").
        let memtable_has_rows = |lo: &std::ops::Bound<PrimaryKey>,
                                 hi: &std::ops::Bound<PrimaryKey>| {
            bound_range_is_searchable(lo, hi)
                && self
                    .memtable
                    .range((lo.clone(), hi.clone()))
                    .next()
                    .is_some()
        };
        let mut parts = std::collections::VecDeque::new();
        let mut needs_visibility_resolution = false;
        let mut cursor = std::ops::Bound::Included(start.clone());
        let mut index = 0;
        while index < segments.len() {
            let mut next = index + 1;
            let mut cluster_max = segments[index].max_key.clone();
            let mut all_unique = segments[index].unique_keys;
            while next < segments.len() && segments[next].min_key <= cluster_max {
                if segments[next].max_key > cluster_max {
                    cluster_max = segments[next].max_key.clone();
                }
                all_unique &= segments[next].unique_keys;
                next += 1;
            }
            let part_lo = segments[index].min_key.clone().max(start.clone());
            let part_hi = cluster_max.min(end.clone());
            let gap_hi = std::ops::Bound::Excluded(part_lo.clone());
            if memtable_has_rows(&cursor, &gap_hi) {
                parts.push_back(ScanPart::MemtableOnly {
                    lo: cursor.clone(),
                    hi: gap_hi,
                });
                needs_visibility_resolution = true;
            }
            let lo_bound = std::ops::Bound::Included(part_lo.clone());
            let hi_bound = std::ops::Bound::Included(part_hi.clone());
            let direct =
                next - index == 1 && all_unique && !memtable_has_rows(&lo_bound, &hi_bound);
            if direct {
                // Coalesce runs of direct clusters so parallel prefetch keeps
                // its full width across them.
                if let Some(ScanPart::Direct { segments: previous }) = parts.back_mut() {
                    previous.extend_from_slice(&segments[index..next]);
                } else {
                    parts.push_back(ScanPart::Direct {
                        segments: segments[index..next].to_vec(),
                    });
                }
            } else {
                needs_visibility_resolution = true;
                parts.push_back(ScanPart::Merge {
                    segments: segments[index..next].to_vec(),
                    lo: std::ops::Bound::Included(part_lo),
                    hi: std::ops::Bound::Included(part_hi.clone()),
                });
            }
            cursor = std::ops::Bound::Excluded(part_hi);
            index = next;
        }
        let scan_end = std::ops::Bound::Included(end.clone());
        if memtable_has_rows(&cursor, &scan_end) {
            parts.push_back(ScanPart::MemtableOnly {
                lo: cursor,
                hi: scan_end,
            });
            needs_visibility_resolution = true;
        }
        if needs_visibility_resolution && candidate_rows < 64 * 1024 {
            return Ok(None);
        }
        let parts = self.refine_merge_parts(start, end, parts);
        Ok(Some(ProjectedScanStream {
            snapshot: self.clone(),
            candidate_segments: segments.len(),
            segments: Vec::new(),
            start: start.clone(),
            end: end.clone(),
            column_ids: column_ids.to_vec(),
            next_segment: 0,
            pruned_segments,
            reported_pruned: false,
            parts,
            memtable_cursor: None,
            direct_range: None,
            merge: None,
        }))
    }

    /// Granule-level refinement of merge clusters (docs/decisions.md,
    /// "Merge-on-read uses granule-level sweep-line classification"): a
    /// base+tail cluster whose dominant segment has unique keys splits into
    /// direct row-ranges of the base outside the overlap span plus one merge
    /// bounded to the actual overlap, located through the base's footer
    /// sparse index. Best effort: any obstacle keeps the coarse part.
    fn refine_merge_parts(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        parts: std::collections::VecDeque<ScanPart>,
    ) -> std::collections::VecDeque<ScanPart> {
        use std::ops::Bound::{Excluded, Included};
        let mut refined = std::collections::VecDeque::with_capacity(parts.len());
        for part in parts {
            let ScanPart::Merge { segments, lo, hi } = part else {
                refined.push_back(part);
                continue;
            };
            if segments.len() != 2 {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let (base_index, tail_index) = if segments[0].row_count >= segments[1].row_count {
                (0, 1)
            } else {
                (1, 0)
            };
            let base = &segments[base_index];
            let tail = &segments[tail_index];
            let refinable = base.unique_keys
                && base.row_count >= tail.row_count.saturating_mul(4)
                && *start <= base.min_key
                && *end >= base.max_key;
            if !refinable {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            // Overlap span: tail keys plus memtable keys inside the part.
            let mut overlap_lo = tail.min_key.clone();
            let mut overlap_hi = tail.max_key.clone();
            if bound_range_is_searchable(&lo, &hi) {
                if let Some((first, _)) = self.memtable.range((lo.clone(), hi.clone())).next()
                    && *first < overlap_lo
                {
                    overlap_lo = first.clone();
                }
                if let Some((last, _)) = self.memtable.range((lo.clone(), hi.clone())).next_back()
                    && *last > overlap_hi
                {
                    overlap_hi = last.clone();
                }
            }
            let Ok(sparse) = segment::read_sparse_index(&self.directory, base) else {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            };
            if sparse.len() < 2 {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let prefix_granules = sparse.partition_point(|(_, key)| *key < overlap_lo);
            let suffix_start = sparse.partition_point(|(_, key)| *key <= overlap_hi);
            let prefix_rows = prefix_granules
                .checked_sub(1)
                .map_or(0, |granule| sparse[granule].0);
            let suffix_rows = if suffix_start < sparse.len() {
                base.row_count - sparse[suffix_start].0
            } else {
                0
            };
            // Refining only pays when a meaningful share of the base skips
            // the merge entirely.
            if (prefix_rows + suffix_rows).saturating_mul(4) < base.row_count {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let merge_lo = if prefix_granules >= 1 {
                Included(sparse[prefix_granules - 1].1.clone())
            } else {
                lo.clone()
            };
            let merge_hi = if suffix_start < sparse.len() {
                Excluded(sparse[suffix_start].1.clone())
            } else {
                hi.clone()
            };
            if prefix_rows > 0 {
                refined.push_back(ScanPart::DirectRange {
                    segment: base.clone(),
                    start_row: 0,
                    end_row: prefix_rows,
                });
            }
            refined.push_back(ScanPart::Merge {
                segments: segments.clone(),
                lo: merge_lo,
                hi: merge_hi,
            });
            if suffix_rows > 0 {
                refined.push_back(ScanPart::DirectRange {
                    segment: base.clone(),
                    start_row: sparse[suffix_start].0,
                    end_row: base.row_count,
                });
            }
        }
        refined
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
        self.scan_projected_range_bounded_pruned(start, end, column_ids, memory_limit, &[])
    }

    /// [`Self::scan_projected_range_bounded`] with scan-predicate value
    /// bounds; see [`Self::scan_projected_range_stream_pruned`] for the
    /// pruning contract.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt segment.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_bounded_pruned(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        memory_limit: usize,
        bounds: &[crate::segment::ColumnBounds],
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
        let prunable = if bounds.is_empty() {
            Vec::new()
        } else {
            self.value_prunable_segments()
        };
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
                    let value_disjoint = prunable.get(segment_index).copied().unwrap_or(false)
                        && segment::sma_disjoint(segment_meta, bounds);
                    if !overlaps || !point_might_match || value_disjoint {
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
