//! Projected scans: the row and chunk shapes readers consume, column
//! decoding, and the merged multi-source scan stream.

use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicUsize},
};

use pintail_types::{PrimaryKey, StoredRow};
use rayon::prelude::*;

use super::{TableSnapshot, projected_scan_pool};
use crate::{StoreError, segment};

/// A scan row containing only the requested user columns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedRow {
    pub(super) key: PrimaryKey,
    pub(super) values: Vec<pintail_types::Value>,
    pub(super) version: u64,
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
    pub(super) segments_pruned: usize,
    pub(super) segments_read: usize,
    pub(super) blocks_pruned: usize,
    pub(super) blocks_read: usize,
    pub(super) blocks_decoded: usize,
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

    pub(super) fn add(&mut self, other: Self) {
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
    pub(super) rows: Vec<ProjectedRow>,
    pub(super) stats: ScanStats,
    pub(super) retained_bytes: usize,
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
pub(super) enum ScanPart {
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
    pub(super) snapshot: TableSnapshot,
    pub(super) segments: Vec<segment::SegmentMeta>,
    pub(super) start: PrimaryKey,
    pub(super) end: PrimaryKey,
    pub(super) column_ids: Vec<u32>,
    pub(super) next_segment: usize,
    pub(super) pruned_segments: usize,
    pub(super) candidate_segments: usize,
    pub(super) reported_pruned: bool,
    pub(super) parts: std::collections::VecDeque<ScanPart>,
    pub(super) memtable_cursor: Option<(std::ops::Bound<PrimaryKey>, std::ops::Bound<PrimaryKey>)>,
    pub(super) direct_range: Option<(segment::SegmentMeta, u64, u64)>,
    /// Rows per slice that last fit the budget for the pending direct range.
    pub(super) direct_slice_rows: Option<u64>,
    pub(super) merge: Option<MergedProjectedStream>,
}

pub(super) struct MergedProjectedStream {
    streams: Vec<segment::SegmentRowStream>,
    heads: Vec<Option<segment::SegmentRowHeader>>,
    memtable_head: Option<StoredRow>,
    reported_segments: bool,
    lo: std::ops::Bound<PrimaryKey>,
    hi: std::ops::Bound<PrimaryKey>,
}

/// Whether `BTreeMap::range((lo, hi))` may be called without panicking and
/// can yield rows: rejects inverted ranges and the empty equal-bound forms.
pub(super) fn bound_range_is_searchable(
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
/// Per-row validity of a decoded column.
///
/// Every NOT NULL column - the common case - used to carry a byte per row
/// that was uniformly true: 20MB per 20M-row column, written by the decoder
/// and scanned again by the executor's mask builder, all to say "no nulls".
/// All-valid is now a count, produced and consumed without touching memory
/// per row. Columns that really hold nulls keep the byte-per-row form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnValidity {
    /// Every row is valid; this many rows.
    AllValid(usize),
    /// Per-row validity, `true` = non-null.
    Bytes(Vec<bool>),
}

impl<'validity> IntoIterator for &'validity ColumnValidity {
    type Item = bool;
    type IntoIter = Box<dyn Iterator<Item = bool> + 'validity>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl ColumnValidity {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::AllValid(count) => *count,
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn is_valid(&self, row: usize) -> bool {
        match self {
            Self::AllValid(count) => row < *count,
            Self::Bytes(bytes) => bytes.get(row).copied().unwrap_or(false),
        }
    }

    /// Rows the backing store could hold without growing.
    #[must_use]
    pub fn capacity(&self) -> usize {
        match self {
            Self::AllValid(count) => *count,
            Self::Bytes(bytes) => bytes.capacity(),
        }
    }

    /// Whether no row is null - the executor's fast paths key off this.
    #[must_use]
    pub fn all_valid(&self) -> bool {
        match self {
            Self::AllValid(_) => true,
            Self::Bytes(bytes) => bytes.iter().all(|valid| *valid),
        }
    }

    #[must_use]
    pub fn iter(&self) -> Box<dyn Iterator<Item = bool> + '_> {
        match self {
            Self::AllValid(count) => Box::new(std::iter::repeat_n(true, *count)),
            Self::Bytes(bytes) => Box::new(bytes.iter().copied()),
        }
    }

    /// Splits off the tail at `at`, mirroring `Vec::split_off` so decoded
    /// columns slice into batches without expanding the all-valid form.
    #[must_use]
    pub fn split_off(&mut self, at: usize) -> Self {
        match self {
            Self::AllValid(count) => {
                let tail = count.saturating_sub(at);
                *count = at.min(*count);
                Self::AllValid(tail)
            }
            Self::Bytes(bytes) => Self::Bytes(bytes.split_off(at)),
        }
    }

    /// The byte-per-row form, for consumers not yet migrated.
    #[must_use]
    pub fn into_bytes(self) -> Vec<bool> {
        match self {
            Self::AllValid(count) => vec![true; count],
            Self::Bytes(bytes) => bytes,
        }
    }
}

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
        validity: ColumnValidity,
    },
    /// Packed unsigned integers; null slots hold zero.
    UInt64 {
        /// One packed value per row.
        values: Vec<u64>,
        /// Per-row null mask (`true` = non-null).
        validity: ColumnValidity,
    },
    /// Packed IEEE-754 bit patterns; null slots hold zero.
    Float64 {
        /// One packed bit pattern per row.
        bits: Vec<u64>,
        /// Per-row null mask (`true` = non-null).
        validity: ColumnValidity,
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
        validity: ColumnValidity,
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
        validity: ColumnValidity,
    },
    /// UTF-8 bytes in one arena; row `i` spans `heap[offsets[i]..offsets[i+1]]`
    /// and null rows span zero bytes.
    Utf8 {
        /// Concatenated UTF-8 payloads.
        heap: Vec<u8>,
        /// `len + 1` row boundaries into `heap`.
        offsets: Vec<usize>,
        /// Per-row null mask (`true` = non-null).
        validity: ColumnValidity,
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
                if validity.is_valid(row) {
                    pintail_types::Value::Int64(values[row])
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::UInt64 { values, validity } => {
                if validity.is_valid(row) {
                    pintail_types::Value::UInt64(values[row])
                } else {
                    pintail_types::Value::Null
                }
            }
            Self::Float64 { bits, validity } => {
                if validity.is_valid(row) {
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
                if validity.is_valid(row) {
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
                if validity.is_valid(row) {
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
                if validity.is_valid(row) {
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
                .zip(validity.iter())
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
                .zip(validity.iter())
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
                .zip(validity.iter())
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
                .zip(validity.iter())
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
                .zip(validity.iter())
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
                    .decode_direct_range_within(segment, start_row, end_row, memory_limit)
                    .map(Some);
            } else if let Some(segment) = self.segments.get(self.next_segment).cloned() {
                self.next_segment += 1;
                return match self.decode_column_chunk(segment.clone(), memory_limit) {
                    // A segment the budget cannot hold whole is read in row
                    // slices instead of refused: a compacted table can hold
                    // tens of millions of rows in one segment.
                    Err(StoreError::MemoryLimitExceeded { .. }) if segment.row_count > 1 => self
                        .decode_direct_range_within(
                            segment.clone(),
                            0,
                            segment.row_count,
                            memory_limit,
                        )
                        .map(Some),
                    other => other.map(Some),
                };
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
        self.direct_slice_rows = None;
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
            let segment = segments.into_iter().next().expect("one segment");
            return match self.decode_column_chunk_maybe_filtered(
                segment.clone(),
                memory_limit,
                prewhere,
            ) {
                // Too large for the budget whole: row slices, unfiltered,
                // and the caller's predicate still runs over every row.
                Err(StoreError::MemoryLimitExceeded { .. }) if segment.row_count > 1 => self
                    .decode_direct_range_within(segment.clone(), 0, segment.row_count, memory_limit)
                    .map(|chunk| vec![chunk]),
                other => other.map(|chunk| vec![chunk]),
            };
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
            let predicate_blocks_read = fetch.blocks_read;
            let predicate_blocks_pruned = fetch.blocks_pruned;
            let predicate_blocks_decoded = fetch.blocks_decoded;
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
                        blocks_read: predicate_blocks_read + fetch.blocks_read,
                        blocks_pruned: predicate_blocks_pruned + fetch.blocks_pruned,
                        blocks_decoded: predicate_blocks_decoded + fetch.blocks_decoded,
                        ..ScanStats::default()
                    },
                    retained_bytes,
                });
            }
            let mut chunk = self.decode_column_chunk(segment, memory_limit)?;
            chunk.stats.blocks_read += predicate_blocks_read;
            chunk.stats.blocks_pruned += predicate_blocks_pruned;
            chunk.stats.blocks_decoded += predicate_blocks_decoded;
            return Ok(chunk);
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

    /// Decodes `[start_row, end_row)` of `segment` within `memory_limit`, in
    /// as many slices as the budget needs: a slice that does not fit is
    /// halved and retried, the size that fits is kept for the rest of the
    /// segment, and the remainder stays queued as the next direct range.
    /// A slice is never finer than a block, which is what the reader
    /// decodes at once, so the budget must hold one block of the projection.
    fn decode_direct_range_within(
        &mut self,
        segment: segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
        memory_limit: usize,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        // Slices are whole blocks: the reader decodes a block at once, so a
        // slice cut inside one pays for the whole block anyway, and a slice
        // straddling two would shrink toward single rows.
        let block = u64::try_from(segment::block_rows(
            &self.snapshot.directory,
            &segment,
            &self.snapshot.schema,
        )?)
        .unwrap_or(u64::MAX)
        .max(1);
        let span = end_row.saturating_sub(start_row).max(1);
        let align = |rows: u64| rows.div_ceil(block).max(1).saturating_mul(block).min(span);
        let mut rows = align(self.direct_slice_rows.unwrap_or(span));
        loop {
            let slice_end = start_row.saturating_add(rows).min(end_row);
            match self.decode_column_chunk_rows(&segment, start_row, slice_end, memory_limit) {
                Ok(chunk) => {
                    if slice_end < end_row {
                        self.direct_range = Some((segment, slice_end, end_row));
                        self.direct_slice_rows = Some(rows);
                    } else {
                        self.direct_slice_rows = None;
                    }
                    return Ok(chunk);
                }
                Err(StoreError::MemoryLimitExceeded { .. }) if rows > block => {
                    rows = align(rows / 2);
                }
                Err(error) => return Err(error),
            }
        }
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
            estimated_bytes: 0,
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

pub(super) fn columns_to_rows(
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
