//! Projected scans: the row and chunk shapes readers consume, column
//! decoding, and the merged multi-source scan stream.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use pintail_types::{PrimaryKey, StoredRow};
use rayon::prelude::*;

use super::layer::{
    Cell, KeyList, KeyPartRef, KeyRef, LayerRows, LiveCells, SpanKey, SpanRow, image_cell,
};
use super::{TableSnapshot, projected_scan_pool};
use crate::{StoreError, segment, segment::ColumnDecode};

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
    pub(super) bytes_decompressed: u64,
    pub(super) values_decoded: u64,
    pub(super) blocks_value_skipped: usize,
    pub(super) index_slices: usize,
}

impl ScanStats {
    /// Returns the slices of segments read through the side index: only
    /// the rows a lookup named were decoded.
    #[must_use]
    pub fn index_slices(self) -> usize {
        self.index_slices
    }

    /// Returns the bytes the scan's decoded blocks decompressed to.
    #[must_use]
    pub fn bytes_decompressed(self) -> u64 {
        self.bytes_decompressed
    }

    /// Returns the column values the scan's reads delivered, predicate
    /// columns included.
    #[must_use]
    pub fn values_decoded(self) -> u64 {
        self.values_decoded
    }

    /// Adds what the columns of one read cost to these counters.
    fn with_decode(mut self, decode: &[ColumnDecode]) -> Self {
        for column in decode {
            self.bytes_decompressed = self
                .bytes_decompressed
                .saturating_add(column.bytes_decompressed);
            self.values_decoded = self.values_decoded.saturating_add(column.values_decoded);
        }
        self
    }

    /// Returns row blocks of direct segments skipped because their stored
    /// minimum and maximum prove no row satisfies a range predicate. Each
    /// such block is skipped in every projected column, so it also shows in
    /// [`Self::blocks_pruned`] once per column.
    #[must_use]
    pub fn blocks_value_skipped(self) -> usize {
        self.blocks_value_skipped
    }

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
        self.bytes_decompressed = self
            .bytes_decompressed
            .saturating_add(other.bytes_decompressed);
        self.values_decoded = self.values_decoded.saturating_add(other.values_decoded);
        self.blocks_value_skipped += other.blocks_value_skipped;
        self.index_slices += other.index_slices;
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
    /// One unique-key segment, wholly inside the scanned range, whose key
    /// span holds memtable rows. Decoded directly, column by column, with
    /// the rows the memtable supersedes masked out by their key and the
    /// memtable's live rows added; the row-wise merge is not paid. Needs
    /// the table's key column named (see
    /// [`ProjectedScanStream::enable_memtable_overlay`]); without it the
    /// part falls back to a merge over the segment.
    Overlay {
        segment: segment::SegmentMeta,
        /// The rows that overlay the segment: the memtable's when `None`,
        /// else a layered cluster's resolved newer rows.
        rows: Option<LayerRows>,
    },
    MemtableOnly {
        lo: std::ops::Bound<PrimaryKey>,
        hi: std::ops::Bound<PrimaryKey>,
        /// As for [`ScanPart::Overlay`]: the memtable's rows when `None`.
        rows: Option<LayerRows>,
    },
    /// A merge cluster of large base segments, pairwise disjoint and each
    /// with unique keys, under newer rows few enough to hold resolved: the
    /// small segments written after the bases and the memtable, merged into
    /// one row per key (a tombstone kept, since it masks). Served as an
    /// overlay of each base plus those rows in the gaps between bases, so the
    /// bases decode column by column instead of through the row-wise merge.
    /// Without an overlay key to mask by, it is the merge it replaces.
    Layered {
        segments: Vec<segment::SegmentMeta>,
        lo: std::ops::Bound<PrimaryKey>,
        hi: std::ops::Bound<PrimaryKey>,
        /// The base segments, in key order.
        bases: Vec<segment::SegmentMeta>,
        rows: LayerRows,
    },
}

/// The overlay part in progress: the segment's block boundaries, so a slice
/// knows the key span it covers and which memtable rows belong to it.
///
/// Consecutive overlay parts under the same rows are served as one, each
/// segment with its own boundaries: a part per segment handed the decoders
/// one segment's slices per round, so a table of small segments decoded a
/// few slices wide however many threads were idle.
pub(super) struct OverlayState {
    sparse: Vec<(String, Vec<(u64, PrimaryKey)>)>,
}

impl OverlayState {
    /// The block boundaries of `segment`; none for a segment not in the part.
    fn sparse_of(&self, segment: &segment::SegmentMeta) -> &[(u64, PrimaryKey)] {
        self.sparse
            .iter()
            .find(|(file_name, _)| *file_name == segment.file_name)
            .map(|(_, sparse)| sparse.as_slice())
            .unwrap_or_default()
    }
}

/// The integer at `row` of a decoded key column, whatever shape the decode
/// produced it in.
fn integer_at(column: &DecodedColumn, row: usize) -> Option<i128> {
    match column {
        DecodedColumn::Int64 { values, .. } | DecodedColumn::NativeUnits { values, .. } => {
            values.get(row).map(|value| i128::from(*value))
        }
        DecodedColumn::UInt64 { values, .. } => values.get(row).map(|value| i128::from(*value)),
        DecodedColumn::Values(values) => match values.get(row)? {
            pintail_types::Value::Int64(value) => Some(i128::from(*value)),
            pintail_types::Value::UInt64(value) => Some(i128::from(*value)),
            _ => None,
        },
        _ => None,
    }
}

/// The key part at `row` of a decoded key column: an integer, or the bytes
/// of a text or binary value, which are the bytes its key part holds.
fn key_part_at(column: &DecodedColumn, row: usize) -> Option<KeyPartRef<'_>> {
    if let Some(value) = integer_at(column, row) {
        return Some(KeyPartRef::Int(value));
    }
    match column.cell(row) {
        Cell::Text(bytes) => Some(KeyPartRef::Text(bytes)),
        Cell::Value(
            pintail_types::Value::Utf8(text) | pintail_types::Value::Enum { label: text, .. },
        ) => Some(KeyPartRef::Text(text.as_bytes())),
        Cell::Value(pintail_types::Value::Binary(bytes)) => Some(KeyPartRef::Binary(bytes)),
        _ => None,
    }
}

impl DecodedColumn {
    /// This column with `inserts` placed at their final positions
    /// (ascending, indexing the output). Integer columns stay packed when
    /// every inserted value is of their type or null; any other column, or
    /// a mismatched value, is rebuilt as plain values.
    #[cfg(test)]
    pub(super) fn interleave(self, inserts: &[(usize, &pintail_types::Value)]) -> Self {
        let cells = inserts
            .iter()
            .map(|(at, value)| (*at, Cell::Value(value)))
            .collect::<Vec<_>>();
        self.interleave_cells(&cells)
    }

    /// [`Self::interleave`] for cells, which a packed column takes as they
    /// are when they come packed from a column of its own shape.
    #[allow(clippy::too_many_lines)]
    pub(super) fn interleave_cells(self, inserts: &[(usize, Cell<'_>)]) -> Self {
        if inserts.is_empty() {
            return self;
        }
        match self {
            Self::Int64 { values, validity } => {
                match typed_inserts(inserts, |cell| match cell {
                    Cell::Value(pintail_types::Value::Int64(value)) => Some(Some(*value)),
                    Cell::Int(value) => Some(Some(value)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = interleave_typed(values, &validity, &typed);
                        Self::Int64 { values, validity }
                    }
                    None => Self::Values(interleave_values(
                        Self::Int64 { values, validity }.into_values(),
                        inserts,
                    )),
                }
            }
            Self::UInt64 { values, validity } => {
                match typed_inserts(inserts, |cell| match cell {
                    Cell::Value(pintail_types::Value::UInt64(value)) => Some(Some(*value)),
                    Cell::UInt(value) => Some(Some(value)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = interleave_typed(values, &validity, &typed);
                        Self::UInt64 { values, validity }
                    }
                    None => Self::Values(interleave_values(
                        Self::UInt64 { values, validity }.into_values(),
                        inserts,
                    )),
                }
            }
            Self::Float64 { bits, validity } => {
                match typed_inserts(inserts, |cell| match cell {
                    Cell::Value(pintail_types::Value::Float64(value)) => {
                        Some(Some(value.get().to_bits()))
                    }
                    Cell::Bits(bits) => Some(Some(bits)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (bits, validity) = interleave_typed(bits, &validity, &typed);
                        Self::Float64 { bits, validity }
                    }
                    None => Self::Values(interleave_values(
                        Self::Float64 { bits, validity }.into_values(),
                        inserts,
                    )),
                }
            }
            // Units stay packed for a value whose text they regenerate
            // exactly, which is what reading them back as text yields.
            Self::NativeUnits {
                units,
                values,
                validity,
            } => {
                match typed_inserts(inserts, |cell| match cell {
                    Cell::Value(pintail_types::Value::Utf8(text)) => {
                        units.parse_units(text).map(Some)
                    }
                    // Units read from a column of the same type are the
                    // units this one holds.
                    Cell::Units(theirs, value) if theirs == units => Some(Some(value)),
                    Cell::Text(bytes) => std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|text| units.parse_units(text))
                        .map(Some),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = interleave_typed(values, &validity, &typed);
                        Self::NativeUnits {
                            units,
                            values,
                            validity,
                        }
                    }
                    None => Self::Values(interleave_values(
                        Self::NativeUnits {
                            units,
                            values,
                            validity,
                        }
                        .into_values(),
                        inserts,
                    )),
                }
            }
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => match text_inserts(inserts) {
                Some(texts) => {
                    let (dict_heap, dict_offsets, typed) =
                        dictionary_codes(dict_heap, dict_offsets, &texts);
                    let (codes, validity) = interleave_typed(codes, &validity, &typed);
                    Self::DictionaryUtf8 {
                        dict_heap,
                        dict_offsets,
                        codes,
                        validity,
                    }
                }
                None => Self::Values(interleave_values(
                    Self::DictionaryUtf8 {
                        dict_heap,
                        dict_offsets,
                        codes,
                        validity,
                    }
                    .into_values(),
                    inserts,
                )),
            },
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => match text_inserts(inserts) {
                Some(texts) => interleave_text(&heap, &offsets, &validity, &texts),
                None => Self::Values(interleave_values(
                    Self::Utf8 {
                        heap,
                        offsets,
                        validity,
                    }
                    .into_values(),
                    inserts,
                )),
            },
            Self::Values(values) => Self::Values(interleave_values(values, inserts)),
        }
    }
}

/// A column of no rows in the packed shape a segment decodes `data_type`
/// to, for rows that never reached a segment to be interleaved into
/// ([`DecodedColumn::interleave`] keeps it packed while the values fit).
pub(super) fn empty_packed_column(data_type: pintail_types::DataType) -> DecodedColumn {
    use pintail_types::DataType;
    let validity = ColumnValidity::AllValid(0);
    match data_type {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            DecodedColumn::Int64 {
                values: Vec::new(),
                validity,
            }
        }
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            DecodedColumn::UInt64 {
                values: Vec::new(),
                validity,
            }
        }
        DataType::Float64 => DecodedColumn::Float64 {
            bits: Vec::new(),
            validity,
        },
        DataType::Utf8 => DecodedColumn::Utf8 {
            heap: Vec::new(),
            offsets: vec![0],
            validity,
        },
        other => match segment::NativeUnits::for_data_type(other) {
            Some(units) => DecodedColumn::NativeUnits {
                units,
                values: Vec::new(),
                validity,
            },
            None => DecodedColumn::Values(Vec::new()),
        },
    }
}

/// Values at their final positions, `None` for null.
type Placed<T> = Vec<(usize, Option<T>)>;

/// Text inserts as bytes (`None` for null), or `None` when one is not text.
fn text_inserts<'a>(inserts: &[(usize, Cell<'a>)]) -> Option<Placed<&'a [u8]>> {
    inserts
        .iter()
        .map(|(at, cell)| match *cell {
            Cell::Value(pintail_types::Value::Utf8(text)) => Some((*at, Some(text.as_bytes()))),
            Cell::Text(bytes) => Some((*at, Some(bytes))),
            Cell::Value(pintail_types::Value::Null) | Cell::Null => Some((*at, None)),
            _ => None,
        })
        .collect()
}

/// Each text insert as a code of the dictionary, the entries it did not
/// hold yet appended to it.
fn dictionary_codes(
    mut dict_heap: Vec<u8>,
    mut dict_offsets: Vec<usize>,
    texts: &[(usize, Option<&[u8]>)],
) -> (Vec<u8>, Vec<usize>, Placed<u32>) {
    let entries = dict_offsets.len().saturating_sub(1);
    let mut known = std::collections::HashMap::<Vec<u8>, u32>::with_capacity(entries);
    for entry in 0..entries {
        let code = u32::try_from(entry).expect("a block dictionary fits u32 codes");
        known
            .entry(dict_heap[dict_offsets[entry]..dict_offsets[entry + 1]].to_vec())
            .or_insert(code);
    }
    let typed = texts
        .iter()
        .map(|(at, text)| {
            let code = text.map(|bytes| {
                *known.entry(bytes.to_vec()).or_insert_with(|| {
                    let code = u32::try_from(dict_offsets.len() - 1)
                        .expect("a block dictionary fits u32 codes");
                    dict_heap.extend_from_slice(bytes);
                    dict_offsets.push(dict_heap.len());
                    code
                })
            });
            (*at, code)
        })
        .collect();
    (dict_heap, dict_offsets, typed)
}

/// A text arena with `texts` placed at their final positions (ascending,
/// indexing the output); a null spans zero bytes.
fn interleave_text(
    heap: &[u8],
    offsets: &[usize],
    validity: &ColumnValidity,
    texts: &[(usize, Option<&[u8]>)],
) -> DecodedColumn {
    let rows = offsets.len().saturating_sub(1);
    let total = rows + texts.len();
    let added = texts
        .iter()
        .map(|(_, text)| text.map_or(0, <[u8]>::len))
        .sum::<usize>();
    let mut out = Vec::with_capacity(heap.len() + added);
    let mut bounds = Vec::with_capacity(total + 1);
    let mut valid = Vec::with_capacity(total);
    bounds.push(0);
    let mut row = 0;
    let mut next = 0;
    while valid.len() < total {
        let at = valid.len();
        if next < texts.len() && (texts[next].0 == at || row >= rows) {
            if let Some(bytes) = texts[next].1 {
                out.extend_from_slice(bytes);
            }
            valid.push(texts[next].1.is_some());
            next += 1;
        } else {
            out.extend_from_slice(&heap[offsets[row]..offsets[row + 1]]);
            valid.push(validity.is_valid(row));
            row += 1;
        }
        bounds.push(out.len());
    }
    let validity = if valid.iter().all(|flag| *flag) {
        ColumnValidity::AllValid(total)
    } else {
        ColumnValidity::Bytes(valid)
    };
    DecodedColumn::Utf8 {
        heap: out,
        offsets: bounds,
        validity,
    }
}

/// The inserts as typed values (`None` for null), or `None` when one does
/// not fit the column's type.
fn typed_inserts<T>(
    inserts: &[(usize, Cell<'_>)],
    convert: impl Fn(Cell<'_>) -> Option<Option<T>>,
) -> Option<Vec<(usize, Option<T>)>> {
    inserts
        .iter()
        .map(|(at, cell)| convert(*cell).map(|value| (*at, value)))
        .collect()
}

/// `values` with typed `inserts` placed at their final positions; nulls
/// take a default placeholder and clear their validity bit.
fn interleave_typed<T: Copy + Default>(
    values: Vec<T>,
    validity: &ColumnValidity,
    inserts: &[(usize, Option<T>)],
) -> (Vec<T>, ColumnValidity) {
    let total = values.len() + inserts.len();
    let mut out = Vec::with_capacity(total);
    let mut valid = Vec::with_capacity(total);
    let mut existing = values.into_iter().enumerate();
    let mut pending = existing.next();
    let mut next_insert = 0;
    while out.len() < total {
        let at = out.len();
        if next_insert < inserts.len() && inserts[next_insert].0 == at {
            let (_, value) = inserts[next_insert];
            out.push(value.unwrap_or_default());
            valid.push(value.is_some());
            next_insert += 1;
        } else if let Some((index, value)) = pending {
            out.push(value);
            valid.push(validity.is_valid(index));
            pending = existing.next();
        } else {
            // An insert position past the end: append the rest in order.
            let (_, value) = inserts[next_insert];
            out.push(value.unwrap_or_default());
            valid.push(value.is_some());
            next_insert += 1;
        }
    }
    let validity = if valid.iter().all(|flag| *flag) {
        ColumnValidity::AllValid(total)
    } else {
        ColumnValidity::Bytes(valid)
    };
    (out, validity)
}

/// Plain values with `inserts` placed at their final positions.
fn interleave_values(
    values: Vec<pintail_types::Value>,
    inserts: &[(usize, Cell<'_>)],
) -> Vec<pintail_types::Value> {
    let total = values.len() + inserts.len();
    let mut out = Vec::with_capacity(total);
    let mut existing = values.into_iter();
    let mut next_insert = 0;
    while out.len() < total {
        let at = out.len();
        if next_insert < inserts.len() && inserts[next_insert].0 == at {
            out.push(inserts[next_insert].1.to_value());
            next_insert += 1;
        } else if let Some(value) = existing.next() {
            out.push(value);
        } else {
            out.push(inserts[next_insert].1.to_value());
            next_insert += 1;
        }
    }
    out
}

/// The memtable rows of one slice's key span, in key order: every key side
/// by side in one allocation and, for a live row, the row itself (a
/// tombstone carries `None`: it only masks).
pub(super) struct SpanRows<'a> {
    keys: KeyList,
    rows: Vec<SpanRow<'a>>,
}

impl<'a> SpanRows<'a> {
    fn new() -> Self {
        Self {
            keys: KeyList::default(),
            rows: Vec::new(),
        }
    }

    #[cfg(test)]
    fn from_rows(rows: &[(Vec<i128>, Option<&'a StoredRow>)]) -> Self {
        let mut span = Self::new();
        for (key, row) in rows {
            span.keys.push_ref(KeyRef::Ints(key));
            span.rows.push(row.map_or(SpanRow::Mask, SpanRow::Row));
        }
        span
    }

    #[cfg(test)]
    fn from_keys(rows: &[(PrimaryKey, Option<&'a StoredRow>)]) -> Self {
        let mut span = Self::new();
        for (key, row) in rows {
            span.keys.push(key);
            span.rows.push(row.map_or(SpanRow::Mask, SpanRow::Row));
        }
        span
    }

    fn len(&self) -> usize {
        self.rows.len()
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn key(&self, index: usize) -> KeyRef<'_> {
        self.keys.get(index)
    }

    fn is_live(&self, index: usize) -> bool {
        !matches!(self.rows[index], SpanRow::Mask)
    }

    fn live(&self) -> Vec<SpanRow<'a>> {
        self.rows
            .iter()
            .filter(|row| !matches!(row, SpanRow::Mask))
            .copied()
            .collect()
    }
}

/// The key of `row` compared with a memtable key, part by part: integers
/// by value, text and binary by their bytes, as the table's keys order.
fn compare_row_key(
    key_columns: &[&DecodedColumn],
    row: usize,
    key: KeyRef<'_>,
) -> std::cmp::Ordering {
    for (column, part) in key_columns.iter().zip(key.parts()) {
        match key_part_at(column, row) {
            Some(value) => match value.cmp(&part) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            },
            // A key column with no key part to read here cannot match any
            // memtable key; order it first so the walk moves on.
            None => return std::cmp::Ordering::Less,
        }
    }
    std::cmp::Ordering::Equal
}

/// Where one live memtable row of a slice goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    /// Over the segment row it supersedes, at that row's index in the
    /// decoded chunk: the chunk keeps its length and its other rows stay
    /// where they are.
    Replace(usize),
    /// At this position of the finished chunk: a key the segment does not
    /// hold, or one whose segment row the scan's filter had already dropped.
    Insert(usize),
}

/// What the memtable's rows for a slice's span do to the chunk decoded for
/// it (the segment rows the scan's filter kept, in key order).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OverlayEdits {
    /// Chunk rows a tombstone supersedes, ascending.
    deletes: Vec<usize>,
    /// One per live memtable row, in key order.
    placements: Vec<Placement>,
}

/// The edits found by looking each memtable key up in the segment's keys
/// rather than by walking the segment.
///
/// The walk below costs the segment whatever changed, so a table that took
/// two updates pays what one that took two million pays. Both sides are
/// sorted and the segment's keys are searchable, so the same answer can be
/// had for the cost of the change instead: measured over ten million rows,
/// twenty thousand changes cost 1.9 ms this way against 6.7 ms walking, and
/// two changes cost microseconds. Past roughly a twentieth of the segment
/// the walk is cheaper again, which is what `overlay_positions` decides.
fn searched_overlay_positions(
    key_columns: &[&DecodedColumn],
    row_count: usize,
    kept: Option<&[std::ops::Range<usize>]>,
    memtable: &SpanRows<'_>,
) -> OverlayEdits {
    // Kept rows before a position, from the ranges rather than by counting:
    // a prefix sum over the ranges answers it in a binary search.
    let prefix: Vec<usize> = kept
        .map(|ranges| {
            let mut total = 0;
            ranges
                .iter()
                .map(|range| {
                    let before = total;
                    total += range.len();
                    before
                })
                .collect()
        })
        .unwrap_or_default();
    let kept_before = |position: usize| -> usize {
        let Some(ranges) = kept else { return position };
        // The last range starting at or before `position`.
        let index = ranges.partition_point(|range| range.start < position);
        let mut count = if index == 0 { 0 } else { prefix[index - 1] };
        if index > 0 {
            let range = &ranges[index - 1];
            count += position.min(range.end).saturating_sub(range.start);
        }
        count
    };
    let is_kept = |row: usize| -> bool {
        let Some(ranges) = kept else { return true };
        let index = ranges.partition_point(|range| range.end <= row);
        ranges
            .get(index)
            .is_some_and(|range| range.start <= row && row < range.end)
    };
    // Where a key sits in the segment, or where it would be inserted.
    let search = |key: KeyRef<'_>| -> Result<usize, usize> {
        let mut low = 0_usize;
        let mut high = row_count;
        while low < high {
            let middle = low + (high - low) / 2;
            match compare_row_key(key_columns, middle, key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    };

    let mut edits = OverlayEdits::default();
    let mut inserted = 0_usize;
    for index in 0..memtable.len() {
        let (position, superseded) = match search(memtable.key(index)) {
            Ok(found) => (found, is_kept(found)),
            Err(insertion) => (insertion, false),
        };
        let chunk_row = kept_before(position);
        match (memtable.is_live(index), superseded) {
            (true, true) => edits.placements.push(Placement::Replace(chunk_row)),
            (true, false) => {
                edits.placements.push(Placement::Insert(
                    chunk_row.saturating_sub(edits.deletes.len()) + inserted,
                ));
                inserted += 1;
            }
            (false, true) => edits.deletes.push(chunk_row),
            (false, false) => {}
        }
    }
    edits
}

/// One row in two hundred: past this share of the segment, looking each
/// change up costs more than walking both sides once.
///
/// The first value here was one in twenty, taken from a measurement that
/// timed a mask built block by block rather than the whole-column lookup
/// this actually does. Timed against the real thing, a walk of ten million
/// keys costs about four milliseconds whatever changed, while the lookups
/// grow with the changes and pass it at one percent. Half of that is the
/// threshold, so the search is chosen only where it clearly wins rather
/// than where the two are level.
const SEARCHED_OVERLAY_SHARE: usize = 200;

/// The edits the memtable's rows for a slice's span make to the chunk
/// decoded for it. `key_columns` hold the keys of the `row_count` segment
/// rows the scan's filter judged (sorted, one row per key), `kept` the rows
/// of those it kept (`None`: all), which are the chunk's rows in order, and
/// `memtable` the span's rows sorted the same way.
///
/// A live memtable row whose key a kept segment row holds replaces that row
/// where it stands. A tombstone for one deletes it. Every other live row is
/// inserted, at the position that keeps the finished chunk in key order:
/// the kept rows before it that no tombstone removes, plus the rows
/// inserted before it. That includes a row whose segment version the filter
/// dropped - the filter runs again above, on the row's newer values.
fn overlay_positions(
    key_columns: &[&DecodedColumn],
    row_count: usize,
    kept: Option<&[std::ops::Range<usize>]>,
    memtable: &SpanRows<'_>,
) -> OverlayEdits {
    if memtable.len().saturating_mul(SEARCHED_OVERLAY_SHARE) <= row_count {
        return searched_overlay_positions(key_columns, row_count, kept, memtable);
    }
    // A single key column that decoded packed is compared straight from its
    // values: asking each row which shape its column has costs more than the
    // comparison it leads to.
    if let ([column], Some(keys)) = (key_columns, memtable.keys.single_integers()) {
        match column {
            DecodedColumn::UInt64 { values, .. } if values.len() >= row_count => {
                return walked_overlay_positions(row_count, kept, memtable, |row, next| {
                    i128::from(values[row]).cmp(&keys[next])
                });
            }
            DecodedColumn::Int64 { values, .. } if values.len() >= row_count => {
                return walked_overlay_positions(row_count, kept, memtable, |row, next| {
                    i128::from(values[row]).cmp(&keys[next])
                });
            }
            _ => {}
        }
    }
    walked_overlay_positions(row_count, kept, memtable, |row, next| {
        compare_row_key(key_columns, row, memtable.key(next))
    })
}

/// The walk of [`overlay_positions`], with `compare` ordering segment row
/// `row` against memtable row `next`.
fn walked_overlay_positions(
    row_count: usize,
    kept: Option<&[std::ops::Range<usize>]>,
    memtable: &SpanRows<'_>,
    compare: impl Fn(usize, usize) -> std::cmp::Ordering,
) -> OverlayEdits {
    let mut edits = OverlayEdits::default();
    // Kept rows passed so far: the chunk index of the next kept row.
    let mut chunk_row = 0_usize;
    let mut inserted = 0_usize;
    let mut range_index = 0_usize;
    let mut is_kept = |row: usize| -> bool {
        let Some(ranges) = kept else { return true };
        while range_index < ranges.len() && ranges[range_index].end <= row {
            range_index += 1;
        }
        ranges
            .get(range_index)
            .is_some_and(|range| range.start <= row && row < range.end)
    };
    let mut row = 0_usize;
    let mut next = 0_usize;
    while row < row_count && next < memtable.len() {
        match compare(row, next) {
            std::cmp::Ordering::Less => {
                chunk_row += usize::from(is_kept(row));
                row += 1;
            }
            std::cmp::Ordering::Equal => {
                let superseded = is_kept(row);
                match (memtable.is_live(next), superseded) {
                    (true, true) => edits.placements.push(Placement::Replace(chunk_row)),
                    (true, false) => {
                        edits.placements.push(Placement::Insert(
                            chunk_row - edits.deletes.len() + inserted,
                        ));
                        inserted += 1;
                    }
                    (false, true) => edits.deletes.push(chunk_row),
                    (false, false) => {}
                }
                chunk_row += usize::from(superseded);
                row += 1;
                next += 1;
            }
            std::cmp::Ordering::Greater => {
                // A key the segment does not hold: an insert.
                if memtable.is_live(next) {
                    edits.placements.push(Placement::Insert(
                        chunk_row - edits.deletes.len() + inserted,
                    ));
                    inserted += 1;
                }
                next += 1;
            }
        }
    }
    while next < memtable.len() {
        if memtable.is_live(next) {
            edits.placements.push(Placement::Insert(
                chunk_row - edits.deletes.len() + inserted,
            ));
            inserted += 1;
        }
        next += 1;
    }
    edits
}

/// `validity` with the rows at `patches` set to whether their new value is
/// non-null; an all-valid column stays a count while every patch is valid.
fn patch_validity<T>(validity: ColumnValidity, patches: &[(usize, Option<T>)]) -> ColumnValidity {
    if matches!(validity, ColumnValidity::AllValid(_))
        && patches.iter().all(|(_, value)| value.is_some())
    {
        return validity;
    }
    let mut flags = validity.into_bytes();
    for (at, value) in patches {
        flags[*at] = value.is_some();
    }
    ColumnValidity::Bytes(flags)
}

/// `values` with typed `patches` written over the rows they name.
fn patch_typed<T: Copy + Default>(
    mut values: Vec<T>,
    validity: ColumnValidity,
    patches: &[(usize, Option<T>)],
) -> (Vec<T>, ColumnValidity) {
    for (at, value) in patches {
        values[*at] = value.unwrap_or_default();
    }
    (values, patch_validity(validity, patches))
}

/// Plain values with `patches` written over the rows they name.
fn patch_values(
    mut values: Vec<pintail_types::Value>,
    patches: &[(usize, Cell<'_>)],
) -> Vec<pintail_types::Value> {
    for (at, cell) in patches {
        values[*at] = cell.to_value();
    }
    values
}

/// A text arena with `texts` in place of the rows they name (ascending); a
/// null spans zero bytes. The arena is rebuilt: a replacement of another
/// length moves every row after it.
fn patch_text(
    heap: &[u8],
    offsets: &[usize],
    validity: ColumnValidity,
    texts: &[(usize, Option<&[u8]>)],
) -> DecodedColumn {
    let rows = offsets.len().saturating_sub(1);
    let added = texts
        .iter()
        .map(|(_, text)| text.map_or(0, <[u8]>::len))
        .sum::<usize>();
    let mut out = Vec::with_capacity(heap.len() + added);
    let mut bounds = Vec::with_capacity(rows + 1);
    bounds.push(0);
    let mut next = 0;
    for row in 0..rows {
        if next < texts.len() && texts[next].0 == row {
            if let Some(bytes) = texts[next].1 {
                out.extend_from_slice(bytes);
            }
            next += 1;
        } else {
            out.extend_from_slice(&heap[offsets[row]..offsets[row + 1]]);
        }
        bounds.push(out.len());
    }
    DecodedColumn::Utf8 {
        heap: out,
        offsets: bounds,
        validity: patch_validity(validity, texts),
    }
}

impl DecodedColumn {
    /// This column with `patches` written over the rows they name
    /// (ascending, in bounds), its length unchanged. Fixed-width columns
    /// are patched where they stand when every new value is of their type
    /// or null; a text arena is rebuilt; any other column, or a mismatched
    /// value, becomes plain values.
    #[cfg(test)]
    pub(super) fn replace_rows(self, patches: &[(usize, &pintail_types::Value)]) -> Self {
        let cells = patches
            .iter()
            .map(|(at, value)| (*at, Cell::Value(value)))
            .collect::<Vec<_>>();
        self.replace_cells(&cells)
    }

    /// [`Self::replace_rows`] for cells.
    #[allow(clippy::too_many_lines)]
    pub(super) fn replace_cells(self, patches: &[(usize, Cell<'_>)]) -> Self {
        if patches.is_empty() {
            return self;
        }
        match self {
            Self::Int64 { values, validity } => {
                match typed_inserts(patches, |cell| match cell {
                    Cell::Value(pintail_types::Value::Int64(value)) => Some(Some(*value)),
                    Cell::Int(value) => Some(Some(value)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = patch_typed(values, validity, &typed);
                        Self::Int64 { values, validity }
                    }
                    None => Self::Values(patch_values(
                        Self::Int64 { values, validity }.into_values(),
                        patches,
                    )),
                }
            }
            Self::UInt64 { values, validity } => {
                match typed_inserts(patches, |cell| match cell {
                    Cell::Value(pintail_types::Value::UInt64(value)) => Some(Some(*value)),
                    Cell::UInt(value) => Some(Some(value)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = patch_typed(values, validity, &typed);
                        Self::UInt64 { values, validity }
                    }
                    None => Self::Values(patch_values(
                        Self::UInt64 { values, validity }.into_values(),
                        patches,
                    )),
                }
            }
            Self::Float64 { bits, validity } => {
                match typed_inserts(patches, |cell| match cell {
                    Cell::Value(pintail_types::Value::Float64(value)) => {
                        Some(Some(value.get().to_bits()))
                    }
                    Cell::Bits(bits) => Some(Some(bits)),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (bits, validity) = patch_typed(bits, validity, &typed);
                        Self::Float64 { bits, validity }
                    }
                    None => Self::Values(patch_values(
                        Self::Float64 { bits, validity }.into_values(),
                        patches,
                    )),
                }
            }
            Self::NativeUnits {
                units,
                values,
                validity,
            } => {
                match typed_inserts(patches, |cell| match cell {
                    Cell::Value(pintail_types::Value::Utf8(text)) => {
                        units.parse_units(text).map(Some)
                    }
                    // Units read from a column of the same type are the
                    // units this one holds.
                    Cell::Units(theirs, value) if theirs == units => Some(Some(value)),
                    Cell::Text(bytes) => std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|text| units.parse_units(text))
                        .map(Some),
                    Cell::Value(pintail_types::Value::Null) | Cell::Null => Some(None),
                    _ => None,
                }) {
                    Some(typed) => {
                        let (values, validity) = patch_typed(values, validity, &typed);
                        Self::NativeUnits {
                            units,
                            values,
                            validity,
                        }
                    }
                    None => Self::Values(patch_values(
                        Self::NativeUnits {
                            units,
                            values,
                            validity,
                        }
                        .into_values(),
                        patches,
                    )),
                }
            }
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => match text_inserts(patches) {
                Some(texts) => {
                    let (dict_heap, dict_offsets, typed) =
                        dictionary_codes(dict_heap, dict_offsets, &texts);
                    let (codes, validity) = patch_typed(codes, validity, &typed);
                    Self::DictionaryUtf8 {
                        dict_heap,
                        dict_offsets,
                        codes,
                        validity,
                    }
                }
                None => Self::Values(patch_values(
                    Self::DictionaryUtf8 {
                        dict_heap,
                        dict_offsets,
                        codes,
                        validity,
                    }
                    .into_values(),
                    patches,
                )),
            },
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => match text_inserts(patches) {
                Some(texts) => patch_text(&heap, &offsets, validity, &texts),
                None => Self::Values(patch_values(
                    Self::Utf8 {
                        heap,
                        offsets,
                        validity,
                    }
                    .into_values(),
                    patches,
                )),
            },
            Self::Values(values) => Self::Values(patch_values(values, patches)),
        }
    }
}

/// `ranges` with the ascending `excluded` positions cut out.
fn subtract_positions(
    ranges: Vec<std::ops::Range<usize>>,
    excluded: &[usize],
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::with_capacity(ranges.len() + excluded.len());
    let mut next = 0;
    for range in ranges {
        let mut cursor = range.start;
        while next < excluded.len() && excluded[next] < range.start {
            next += 1;
        }
        while next < excluded.len() && excluded[next] < range.end {
            if excluded[next] > cursor {
                out.push(cursor..excluded[next]);
            }
            cursor = excluded[next] + 1;
            next += 1;
        }
        if cursor < range.end {
            out.push(cursor..range.end);
        }
    }
    out
}

/// Whether `key` lies beyond the upper bound `hi`.
fn bound_below(hi: &std::ops::Bound<PrimaryKey>, key: &PrimaryKey) -> bool {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    match hi {
        Included(bound) => key > bound,
        Excluded(bound) => key >= bound,
        Unbounded => false,
    }
}

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

/// Rows a direct segment is handed to the decoders in. A slice is the
/// scan's work unit: parallel width comes from how many slices are in
/// flight, not from how many segments there are, and the rows in flight
/// are bounded by width times this whatever the segment size. It matches
/// the executor's largest pass-through batch, so a slice becomes one batch.
pub(super) const DIRECT_SLICE_ROWS: u64 = 131_072;

/// One unit of direct-segment decode work.
#[derive(Clone, Debug)]
pub(super) enum DirectSlice {
    /// A segment decoded as it was before slicing: small enough to be one
    /// slice, or only partly inside the scanned key range.
    Whole(segment::SegmentMeta),
    /// A block-aligned row range of a segment that lies wholly inside the
    /// scanned key range.
    Range {
        segment: segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
    },
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
    /// The rows the current overlay or memtable-only part reads: the
    /// memtable's, or a layered cluster's resolved rows.
    pub(super) overlay_rows: LayerRows,
    pub(super) direct_range: Option<(segment::SegmentMeta, u64, u64)>,
    /// Rows per slice that last fit the budget for the pending direct range.
    pub(super) direct_slice_rows: Option<u64>,
    /// Direct-segment work units not yet decoded, cut from the segments of
    /// the current part as they are reached.
    pub(super) slices: VecDeque<DirectSlice>,
    pub(super) merge: Option<MergedProjectedStream>,
    /// The user columns that carry the table's key (integer, text or
    /// binary parts), when the caller named them; what lets an [`ScanPart::Overlay`] mask the rows
    /// the memtable supersedes from a packed column instead of merging.
    pub(super) overlay_key: Option<Vec<u32>>,
    pub(super) overlay: Option<OverlayState>,
    /// Chunks an overlay slice produced beyond the one the single-chunk
    /// API could hand out, waiting their turn.
    pub(super) pending: VecDeque<ProjectedColumnChunk>,
    /// The side-index request the scan's predicates or a join
    /// gave it; consulted only while the index is switched on.
    pub(super) index_lookup: Option<super::side_index::IndexLookup>,
    /// Further lookups, each naming every row the scan wants by another
    /// column: per segment the one naming the fewest rows is read.
    pub(super) index_alternates: Vec<super::side_index::IndexLookup>,
    /// Whether the scan decodes nothing beyond the columns its filter
    /// reads and selects first only to leave rows unread: the side index
    /// is then held to a smaller share of rows, and a slice where no row
    /// is left unread is read through as a scan with no selector reads it.
    pub(super) filter_only: bool,
    /// The scan predicates' value bounds, consulted against each direct
    /// block's stored extremes on the filter-first path.
    pub(super) value_bounds: Vec<segment::ColumnBounds>,
    /// The scan predicates that read one text column each, asked of the
    /// distinct values a direct block holds on the filter-first path.
    pub(super) text_filters: Vec<segment::TextValueFilter>,
    /// Whether filter-first rounds judge every slice or a sample of them.
    pub(super) prewhere_sample: PrewhereSample,
    /// Rows the reader still wants, when it wants only so many (see
    /// [`ProjectedScanStream::set_row_budget`]).
    pub(super) row_budget: Option<u64>,
    /// Which end of its key range the scan hands out first (see
    /// [`ProjectedScanStream::read_from_end`]).
    pub(super) order: ReadOrder,
    /// Whether the pending direct range is the rest of one being read
    /// forward in pieces, so what was handed out of it is its start.
    pub(super) range_resumes: bool,
}

/// Which end of the scanned key range a stream hands out first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ReadOrder {
    /// In key order.
    #[default]
    Forward,
    /// The last part first, a segment's last rows first.
    EndFirst,
}

/// While a filter keeps nearly every row of the slices it judges, judging
/// costs more than it saves: the predicate columns decode apart from the
/// rest and the selector runs, to keep everything. A sampled scan sends one
/// slice in [`Self::EVERY`] through the selector and decodes the others
/// whole, as if it had none. The first sampled slice the selector restricts
/// puts every later slice of the round back under it, so a table whose
/// filter keeps all of one stretch and little of the next loses a few
/// slices at the boundary and not the rest of the scan.
#[derive(Debug, Default)]
pub(super) struct PrewhereSample {
    /// Set by the reader between rounds.
    on: bool,
    /// Slices offered so far, across rounds, so a scan reading one slice a
    /// round samples as sparsely as a wide one.
    turn: AtomicUsize,
    /// A sampled slice of this round was restricted.
    resumed: AtomicBool,
}

impl PrewhereSample {
    /// While sampling, one slice in this many is judged.
    const EVERY: usize = 8;
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
pub type PrewhereSelect<'a> =
    &'a (dyn Fn(&[DecodedColumn], usize) -> Result<Option<PrewhereRanges>, String> + Sync);

/// The rows a prewhere selector keeps from one chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrewhereRanges {
    /// Surviving row ranges, ascending and disjoint.
    pub ranges: Vec<std::ops::Range<usize>>,
    /// Whether every row inside `ranges` satisfies the selector's whole
    /// predicate. A selector that widens its ranges to decode fewer, larger
    /// regions keeps rows that fail, and must say so here; a chunk decoded
    /// from exact ranges alone is [`ProjectedColumnChunk::prefiltered`].
    pub exact: bool,
    /// The kept rows as one bit per row of the chunk (bit `r % 64` of word
    /// `r / 64`), given instead of `ranges`, which are then empty. A filter
    /// keeping rows scattered through the chunk names them in a few
    /// thousand words rather than tens of thousands of ranges, and a direct
    /// segment read places them from the words; every other read turns
    /// them into ranges first ([`Self::into_ranges`]).
    pub mask: Option<Vec<u64>>,
}

impl PrewhereRanges {
    /// The same selection with the mask, if any, turned into ranges over
    /// the chunk's `rows`.
    #[must_use]
    pub fn into_ranges(self, rows: usize) -> Self {
        match self.mask {
            Some(words) => Self {
                ranges: crate::segment::word_runs(&words, rows),
                exact: self.exact,
                mask: None,
            },
            None => self,
        }
    }
}

impl From<Vec<std::ops::Range<usize>>> for PrewhereRanges {
    /// Ranges that restrict the decode but promise nothing about the rows.
    fn from(ranges: Vec<std::ops::Range<usize>>) -> Self {
        Self {
            ranges,
            exact: false,
            mask: None,
        }
    }
}

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
    type IntoIter = ValidityIter<'validity>;

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

    /// Per-row validity, without a call through a trait object per row.
    #[must_use]
    pub fn iter(&self) -> ValidityIter<'_> {
        match self {
            Self::AllValid(count) => ValidityIter::AllValid(std::iter::repeat_n(true, *count)),
            Self::Bytes(bytes) => ValidityIter::Bytes(bytes.iter().copied()),
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

/// Iterator over a [`ColumnValidity`]'s rows, `true` = non-null.
#[derive(Clone, Debug)]
pub enum ValidityIter<'validity> {
    /// Every row valid.
    AllValid(std::iter::RepeatN<bool>),
    /// One byte per row.
    Bytes(std::iter::Copied<std::slice::Iter<'validity, bool>>),
}

impl Iterator for ValidityIter<'_> {
    type Item = bool;

    #[inline]
    fn next(&mut self) -> Option<bool> {
        match self {
            Self::AllValid(rows) => rows.next(),
            Self::Bytes(rows) => rows.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::AllValid(rows) => rows.size_hint(),
            Self::Bytes(rows) => rows.size_hint(),
        }
    }
}

impl ExactSizeIterator for ValidityIter<'_> {}

/// Moves the first `count` elements out of `values` into a vector sized to
/// exactly `count`, leaving the tail in place.
fn split_prefix<T>(values: &mut Vec<T>, count: usize) -> Vec<T> {
    let rest = values.split_off(count);
    let mut prefix = std::mem::replace(values, rest);
    prefix.shrink_to_fit();
    prefix
}

fn split_validity_prefix(validity: &mut ColumnValidity, count: usize) -> ColumnValidity {
    let rest = validity.split_off(count);
    let prefix = std::mem::replace(validity, rest);
    match prefix {
        ColumnValidity::Bytes(mut bytes) => {
            bytes.shrink_to_fit();
            ColumnValidity::Bytes(bytes)
        }
        all_valid @ ColumnValidity::AllValid(_) => all_valid,
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
    ///
    /// The prefix is right-sized. `Vec::split_off` hands the tail a fresh
    /// exact allocation and leaves the head holding the WHOLE original
    /// capacity, so a 1M-row segment sliced into sixteen 64K-row batches
    /// used to retain sixteen prefixes of 1M, 940K, 875K ... rows each -
    /// about eight times the segment - for as long as those batches lived.
    /// Measured on a two-column 1M-row segment: 118 MB retained for 16 MB
    /// of data, which is what made a plain GROUP BY under the shipped
    /// ceiling fail on its first pull.
    #[must_use]
    pub fn take_prefix(&mut self, count: usize) -> Self {
        let count = count.min(self.len());
        match self {
            Self::Values(values) => Self::Values(split_prefix(values, count)),
            Self::Int64 { values, validity } => Self::Int64 {
                values: split_prefix(values, count),
                validity: split_validity_prefix(validity, count),
            },
            Self::NativeUnits {
                units,
                values,
                validity,
            } => Self::NativeUnits {
                units: *units,
                values: split_prefix(values, count),
                validity: split_validity_prefix(validity, count),
            },
            Self::UInt64 { values, validity } => Self::UInt64 {
                values: split_prefix(values, count),
                validity: split_validity_prefix(validity, count),
            },
            Self::Float64 { bits, validity } => Self::Float64 {
                bits: split_prefix(bits, count),
                validity: split_validity_prefix(validity, count),
            },
            Self::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => Self::DictionaryUtf8 {
                dict_heap: dict_heap.clone(),
                dict_offsets: dict_offsets.clone(),
                codes: split_prefix(codes, count),
                validity: split_validity_prefix(validity, count),
            },
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => {
                let cut = offsets[count];
                let rest_offsets = offsets[count..]
                    .iter()
                    .map(|offset| offset - cut)
                    .collect::<Vec<_>>();
                offsets.truncate(count + 1);
                let mut prefix_offsets = std::mem::replace(offsets, rest_offsets);
                prefix_offsets.shrink_to_fit();
                Self::Utf8 {
                    heap: split_prefix(heap, cut),
                    offsets: prefix_offsets,
                    validity: split_validity_prefix(validity, count),
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

/// The run of a segment's rows a key range covers, and the key blocks it
/// selected and skipped to find them.
struct KeySpan {
    rows: std::ops::Range<usize>,
    /// Key blocks decoded to find the run.
    key_blocks_decoded: usize,
    blocks_read: usize,
    blocks_pruned: usize,
}

/// One bounded column-major projection from an independently visible segment.
pub struct ProjectedColumnChunk {
    columns: Vec<DecodedColumn>,
    row_count: usize,
    stats: ScanStats,
    retained_bytes: usize,
    /// Every row satisfies the scan's prewhere predicate: the chunk holds
    /// only rows an exact selector kept.
    prefiltered: bool,
    /// What each column read for this chunk cost, predicate columns
    /// included; a column read twice appears twice.
    column_decode: Vec<ColumnDecode>,
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

    /// What each column read for this chunk cost: decompressed bytes and
    /// delivered values per column id. Empty for rows that came from the
    /// memtable or the row-merge path.
    #[must_use]
    pub fn column_decode(&self) -> &[ColumnDecode] {
        &self.column_decode
    }

    /// Moves the packed projected columns into a columnar executor.
    #[must_use]
    pub fn into_decoded_columns(self) -> Vec<DecodedColumn> {
        self.columns
    }

    /// Materializes projected columns into per-row values.
    #[must_use]
    /// The decoded columns and the row count, without turning packed
    /// values into one `Value` per cell. A consumer with its own typed
    /// representation wants these, not `into_columns`.
    pub fn take_columns(self) -> (Vec<DecodedColumn>, usize) {
        (self.columns, self.row_count)
    }

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

    /// Whether every row of this chunk is known to satisfy the prewhere
    /// predicate the scan was given, so a consumer need not test it again.
    #[must_use]
    pub const fn prefiltered(&self) -> bool {
        self.prefiltered
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
            if let Some(chunk) = self.pending.pop_front() {
                return Ok(Some(chunk));
            }
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
            } else if self.overlay.is_some() {
                // An overlay segment is decoded slice by slice with the
                // mask, whichever API pulls; the slice's chunks queue up.
                self.fill_direct_slices(1)?;
                let Some(slice) = self.slices.pop_front() else {
                    if !self.advance_part()? {
                        return Ok(None);
                    }
                    continue;
                };
                let mut chunks = self.decode_overlay_slice_bounded(slice, memory_limit, None)?;
                if chunks.is_empty() {
                    continue;
                }
                let first = chunks.remove(0);
                self.pending.extend(chunks);
                return Ok(Some(first));
            } else if let Some(segment) = self.segments.get(self.next_segment).cloned() {
                self.next_segment += 1;
                return match self.decode_column_chunk(segment.clone(), memory_limit) {
                    // A segment the budget cannot hold whole is read in row
                    // slices instead of refused: a compacted table can hold
                    // tens of millions of rows in one segment.
                    // The slices cover only the rows the scan's key range
                    // selects.
                    Err(error @ StoreError::MemoryLimitExceeded { .. })
                        if segment.row_count > 1 =>
                    {
                        let (start_row, end_row) =
                            self.bounded_rows(&segment, memory_limit)?.ok_or(error)?;
                        if start_row == end_row {
                            continue;
                        }
                        self.decode_direct_range_within(segment, start_row, end_row, memory_limit)
                            .map(Some)
                    }
                    other => other.map(Some),
                };
            }
            if !self.advance_part()? {
                return Ok(None);
            }
        }
    }

    /// Activates the next classified scan part, returning `false` at the end.
    #[allow(clippy::too_many_lines)]
    fn advance_part(&mut self) -> Result<bool, StoreError> {
        let part = if self.ends_first() {
            self.parts.pop_back()
        } else {
            self.parts.pop_front()
        };
        let Some(part) = part else {
            return Ok(false);
        };
        self.range_resumes = false;
        self.merge = None;
        self.memtable_cursor = None;
        self.direct_range = None;
        self.direct_slice_rows = None;
        self.slices.clear();
        self.overlay = None;
        match part {
            ScanPart::Direct { mut segments } => {
                if self.ends_first() {
                    segments.reverse();
                }
                self.segments = segments;
                self.next_segment = 0;
            }
            ScanPart::Layered {
                segments,
                lo,
                hi,
                bases,
                rows,
            } => {
                let expanded = self.expand_layered(segments, lo, hi, bases, rows)?;
                if self.ends_first() {
                    // In key order at the back, where the next part is taken.
                    self.parts.extend(expanded);
                } else {
                    for part in expanded.into_iter().rev() {
                        self.parts.push_front(part);
                    }
                }
                return self.advance_part();
            }
            ScanPart::Overlay { segment, rows } => {
                let sparse = if self.overlay_key.is_some() {
                    segment::read_sparse_index(&self.snapshot.directory, &segment).ok()
                } else {
                    None
                };
                let Some(sparse) = sparse else {
                    if rows.is_some() {
                        // A layered cluster checked every base's index
                        // before it expanded; a merge over this base alone
                        // would miss the rows layered over it.
                        return Err(StoreError::FormatLimit(
                            "a layered base segment lost its sparse index".into(),
                        ));
                    }
                    // No key column named, or no index to place slices by:
                    // the merge answers for the whole segment as before.
                    self.parts.push_front(ScanPart::Merge {
                        lo: std::ops::Bound::Included(segment.min_key.clone()),
                        hi: std::ops::Bound::Included(segment.max_key.clone()),
                        segments: vec![segment],
                    });
                    return self.advance_part();
                };
                let layered = rows.is_some();
                let mut rows =
                    rows.unwrap_or_else(|| LayerRows::single(self.snapshot.memtable.clone()));
                // The key's columns are named here, so the memtable is
                // read as arrays: built once for these rows, by this scan
                // or one before it.
                rows.attach_image(&self.snapshot.memtable_image);
                let mut sparse = vec![(segment.file_name.clone(), sparse)];
                let mut segments = vec![segment];
                // The overlay parts that follow under the same rows join
                // this one, so their slices decode in the same rounds.
                while !self.ends_first()
                    && let Some(ScanPart::Overlay {
                        segment: next,
                        rows: next_rows,
                    }) = self.parts.front()
                {
                    let same_rows = match next_rows {
                        Some(next_rows) => layered && next_rows.same_source(&rows),
                        None => !layered,
                    };
                    if !same_rows {
                        break;
                    }
                    let Ok(next_sparse) =
                        segment::read_sparse_index(&self.snapshot.directory, next)
                    else {
                        break;
                    };
                    let Some(ScanPart::Overlay { segment: next, .. }) = self.parts.pop_front()
                    else {
                        break;
                    };
                    sparse.push((next.file_name.clone(), next_sparse));
                    segments.push(next);
                }
                self.overlay_rows = rows;
                self.segments = segments;
                self.next_segment = 0;
                self.overlay = Some(OverlayState { sparse });
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
                        let mut stream = segment::SegmentRowStream::open_headers(
                            &self.snapshot.directory,
                            meta,
                            &self.snapshot.schema,
                        )?;
                        // The merge starts at the part's lower bound; the
                        // blocks before it are passed over, not walked.
                        if let std::ops::Bound::Included(key) | std::ops::Bound::Excluded(key) = &lo
                        {
                            stream.skip_to_key(meta, key)?;
                        }
                        Ok(stream)
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;
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
            ScanPart::MemtableOnly { lo, hi, rows } => {
                let mut rows =
                    rows.unwrap_or_else(|| LayerRows::single(self.snapshot.memtable.clone()));
                if self.overlay_key.is_some() {
                    rows.attach_image(&self.snapshot.memtable_image);
                }
                self.overlay_rows = rows;
                self.segments = Vec::new();
                self.next_segment = 0;
                self.memtable_cursor = Some((lo, hi));
            }
        }
        Ok(true)
    }

    /// The parts a layered cluster is served as: an overlay of each base and
    /// the resolved rows of the gaps around them, in key order. Without an
    /// overlay key, or a base without a sparse index to place slices by, it
    /// is the merge it stands for.
    fn expand_layered(
        &self,
        segments: Vec<segment::SegmentMeta>,
        lo: std::ops::Bound<PrimaryKey>,
        hi: std::ops::Bound<PrimaryKey>,
        bases: Vec<segment::SegmentMeta>,
        mut rows: LayerRows,
    ) -> Result<Vec<ScanPart>, StoreError> {
        let keyed = self.overlay_key.is_some();
        let sparse = keyed
            && bases.iter().all(|base| {
                segment::read_sparse_index(&self.snapshot.directory, base)
                    .is_ok_and(|sparse| !sparse.is_empty())
            });
        // The newer segments' keys, read once for the manifest that names
        // them; their values stay in the segments until a slice asks.
        let indexed = sparse
            && rows.resolve(
                &self.snapshot.manifest.layer_index,
                &self.snapshot.directory,
                &self.snapshot.schema,
            )?;
        if indexed {
            rows.attach_image(&self.snapshot.memtable_image);
        }
        if !indexed {
            pintail_log::log_debug!(
                "store scan merges a layered cluster row by row: {}",
                if !keyed {
                    "the scan names no key columns to mask by (a keyless table, or a key part that is not an integer, text or binary column)"
                } else if !sparse {
                    "a base has no sparse index"
                } else {
                    "its newer segments' keys cannot be indexed"
                }
            );
            return Ok(self
                .snapshot
                .refine_merge_parts(
                    &self.start,
                    &self.end,
                    VecDeque::from([ScanPart::Merge { segments, lo, hi }]),
                )
                .into());
        }
        let has_rows = |lo: &std::ops::Bound<PrimaryKey>,
                        hi: &std::ops::Bound<PrimaryKey>|
         -> Result<bool, StoreError> {
            Ok(bound_range_is_searchable(lo, hi) && rows.range(lo, hi)?.next()?.is_some())
        };
        let mut expanded = Vec::with_capacity(bases.len() * 2 + 1);
        let mut cursor = lo;
        for base in bases {
            let gap_hi = std::ops::Bound::Excluded(base.min_key.clone());
            if has_rows(&cursor, &gap_hi)? {
                expanded.push(ScanPart::MemtableOnly {
                    lo: cursor,
                    hi: gap_hi,
                    rows: Some(rows.clone()),
                });
            }
            cursor = std::ops::Bound::Excluded(base.max_key.clone());
            expanded.push(ScanPart::Overlay {
                segment: base,
                rows: Some(rows.clone()),
            });
        }
        if has_rows(&cursor, &hi)? {
            expanded.push(ScanPart::MemtableOnly {
                lo: cursor,
                hi,
                rows: Some(rows),
            });
        }
        Ok(expanded)
    }

    /// Produces the next chunk of memtable-resident rows for a gap part.
    fn next_memtable_chunk(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        Ok(self.next_memtable_chunks(1, memory_limit)?.pop())
    }

    /// Up to `max_chunks` chunks of memtable-resident rows for a gap part,
    /// in key order, within `memory_limit` together; empty when the part is
    /// drained.
    ///
    /// The rows are walked once, in order, and the chunks built from them
    /// side by side: a table that took a few hundred thousand inserts past
    /// its segments otherwise hands them up a few thousand rows per call,
    /// one call after another, while the scan's other threads wait.
    #[allow(clippy::too_many_lines)]
    fn next_memtable_chunks(
        &mut self,
        max_chunks: usize,
        memory_limit: usize,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        const MAX_MEMTABLE_CHUNK_ROWS: usize = 8 * 1024;
        /// Below this allowance the part is handed up a chunk at a time, as
        /// it always was: under a tight ceiling the operators above need
        /// the room more than the scan needs width, and a round of several
        /// chunks is rows they have to hold before the first is consumed.
        const WIDE_ROUND_BYTES: usize = 64 << 20;
        let max_chunks = if memory_limit < WIDE_ROUND_BYTES {
            1
        } else {
            max_chunks
        };
        let Some((lo, hi)) = self.memtable_cursor.clone() else {
            return Ok(Vec::new());
        };
        if !bound_range_is_searchable(&lo, &hi) {
            self.memtable_cursor = None;
            return Ok(Vec::new());
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
        // Read from the end, the part is handed out whole before anything
        // before it: cutting it to the reader's rows would only make more
        // calls of it.
        let row_budget = self.row_budget.filter(|_| !self.ends_first());
        let max_chunks = if row_budget.is_some() {
            1
        } else {
            max_chunks.max(1)
        };
        let budget_rows = row_budget.map_or(usize::MAX, |rows| {
            usize::try_from(rows).unwrap_or(usize::MAX).max(1)
        });
        let chunk_limit = memory_limit / max_chunks;
        let chunk_rows = if projection.is_empty() {
            MAX_MEMTABLE_CHUNK_ROWS.min(budget_rows)
        } else {
            chunk_limit
                .checked_div(
                    projection
                        .len()
                        .saturating_mul(size_of::<pintail_types::Value>())
                        .saturating_mul(2),
                )
                .unwrap_or(0)
                .clamp(1, MAX_MEMTABLE_CHUNK_ROWS)
                .min(budget_rows)
        };
        // The live rows first, then each projected column built from them in
        // the packed shape a segment of the same type decodes to: a chunk of
        // plain values sends every consumer above it down its row-at-a-time
        // path, for however many rows were inserted past the segments.
        let rows_source = self.overlay_rows.clone();
        let wanted = chunk_rows.saturating_mul(max_chunks);
        let mut live: Vec<SpanRow<'_>> = Vec::new();
        let mut last_key = None;
        let mut admission = self.row_admission();
        // The lookup's column of the memtable's image, to judge its rows.
        let admitted = match &admission {
            Some(admission) if rows_source.has_image() => {
                Some(rows_source.image_column(admission.position(), &self.snapshot.schema)?)
            }
            _ => None,
        };
        let mut cursor = rows_source.range(&lo, &hi)?;
        while let Some((key, row)) = cursor.next()? {
            last_key = Some(key);
            match row {
                SpanRow::Mask => continue,
                SpanRow::Row(stored) => {
                    if admission
                        .as_mut()
                        .is_some_and(|admission| !admission.admits(stored))
                    {
                        continue;
                    }
                }
                SpanRow::Image { row } => {
                    if let (Some(admission), Some(column)) = (admission.as_mut(), &admitted)
                        && !admission.admits_cell(image_cell(column, row as usize))
                    {
                        continue;
                    }
                }
                // A layer row the index lookup would reject is read with
                // the rest and left to the filter above.
                SpanRow::Layer { .. } => {}
            }
            live.push(row);
            if live.len() >= wanted {
                break;
            }
        }
        drop(admission);
        // A walk that ran out of rows drained the part, tombstones and all.
        self.memtable_cursor = match last_key {
            Some(key) if live.len() >= wanted => Some((
                std::ops::Bound::Excluded(rows_source.key_of(key)?),
                hi.clone(),
            )),
            _ => None,
        };
        if live.is_empty() {
            return Ok(Vec::new());
        }
        let schema = &self.snapshot.schema;
        let build = |rows: &[SpanRow<'_>]| -> Result<ProjectedColumnChunk, StoreError> {
            let cells = LiveCells::read(
                &rows_source,
                &self.snapshot.directory,
                schema,
                rows,
                projection.clone(),
                chunk_limit,
            )?;
            let columns = projection
                .iter()
                .enumerate()
                .map(|(column, position)| {
                    let inserts = (0..cells.len())
                        .map(|at| (at, cells.cell(at, column)))
                        .collect::<Vec<_>>();
                    empty_packed_column(schema.columns()[*position].data_type())
                        .interleave_cells(&inserts)
                })
                .collect::<Vec<_>>();
            let retained_bytes = size_of::<ProjectedColumnChunk>()
                .saturating_add(
                    columns
                        .capacity()
                        .saturating_mul(size_of::<DecodedColumn>()),
                )
                .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum());
            if retained_bytes > chunk_limit {
                return Err(StoreError::MemoryLimitExceeded {
                    used: 0,
                    requested: retained_bytes,
                    limit: chunk_limit,
                });
            }
            Ok(ProjectedColumnChunk {
                prefiltered: false,
                columns,
                row_count: rows.len(),
                stats: ScanStats::default(),
                retained_bytes,
                column_decode: Vec::new(),
            })
        };
        let chunks: Result<Vec<ProjectedColumnChunk>, StoreError> = if live.len() <= chunk_rows {
            build(&live).map(|chunk| vec![chunk])
        } else {
            projected_scan_pool()?.install(|| live.par_chunks(chunk_rows).map(build).collect())
        };
        match chunks {
            // Chunks that share the allowance may not fit where one that
            // has it whole does.
            Err(StoreError::MemoryLimitExceeded { .. }) if max_chunks > 1 => {
                self.memtable_cursor = Some((lo, hi));
                self.next_memtable_chunks(1, memory_limit)
            }
            other => other,
        }
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
        if !self.pending.is_empty() {
            return Ok(self.pending.drain(..).collect());
        }
        if self.merge.is_none() && self.memtable_cursor.is_some() {
            let chunks = self.next_memtable_chunks(max_chunks, memory_limit)?;
            if !chunks.is_empty() {
                return Ok(chunks);
            }
            // Drained: on to the next part.
            self.memtable_cursor = None;
            return self.next_column_chunks_inner(max_chunks, memory_limit, prewhere);
        }
        if let Some(chunks) = self.next_range_rows(memory_limit)? {
            return Ok(chunks);
        }
        if self.merge.is_some() || self.direct_range.is_some() {
            return Ok(self.next_column_chunk(memory_limit)?.into_iter().collect());
        }
        // A reader that wants only so many rows takes one slice, cut to
        // them, at a time: a round of slices is rows it would throw away.
        let max_chunks = if self.row_budget.is_some() {
            1
        } else {
            max_chunks
        };
        self.fill_direct_slices(max_chunks.max(1))?;
        if let Some(budget) = self.row_budget {
            self.trim_front_slice(budget, memory_limit)?;
        }
        if self.slices.is_empty() {
            if !self.advance_part()? {
                return Ok(Vec::new());
            }
            return self.next_column_chunks_inner(max_chunks, memory_limit, prewhere);
        }
        let chunk_count = max_chunks.max(1).min(self.slices.len());
        let taken: Vec<DirectSlice> = self.slices.drain(..chunk_count).collect();
        if chunk_count == 1 {
            let slice = taken.into_iter().next().expect("one slice");
            let (segment, start_row, end_row) = match &slice {
                DirectSlice::Whole(segment) => (segment.clone(), 0, segment.row_count),
                DirectSlice::Range {
                    segment,
                    start_row,
                    end_row,
                } => (segment.clone(), *start_row, *end_row),
            };
            if self.overlay.is_some() {
                return self.decode_overlay_slice_bounded(slice, memory_limit, prewhere);
            }
            return match self.decode_slice(&slice, memory_limit, prewhere) {
                Err(error @ StoreError::MemoryLimitExceeded { .. }) if end_row - start_row > 1 => {
                    // A whole segment the range covers only in part keeps
                    // its key bounds when read in slices.
                    let (start_row, end_row) = if matches!(slice, DirectSlice::Whole(_)) {
                        self.bounded_rows(&segment, memory_limit)?.ok_or(error)?
                    } else {
                        (start_row, end_row)
                    };
                    if start_row == end_row {
                        return Ok(Vec::new());
                    }
                    self.decode_direct_range_within(segment, start_row, end_row, memory_limit)
                        .map(|chunk| vec![chunk])
                }
                other => other,
            };
        }
        let per_chunk_limit = memory_limit / chunk_count;
        let decoded: Result<Vec<Vec<ProjectedColumnChunk>>, StoreError> = projected_scan_pool()?
            .install(|| {
                taken
                    .par_iter()
                    .map(|slice| self.decode_slice(slice, per_chunk_limit, prewhere))
                    .collect()
            });
        if matches!(decoded, Err(StoreError::MemoryLimitExceeded { .. })) {
            for slice in taken.into_iter().rev() {
                self.slices.push_front(slice);
            }
            return self.next_column_chunks_inner(chunk_count.div_ceil(2), memory_limit, prewhere);
        }
        decoded.map(|chunks| chunks.into_iter().flatten().collect())
    }

    /// The reader's rows of the pending direct range, when it wants only
    /// so many or reads from the end; `None` leaves the range to the
    /// forward read.
    fn next_range_rows(
        &mut self,
        memory_limit: usize,
    ) -> Result<Option<Vec<ProjectedColumnChunk>>, StoreError> {
        // Read from the end, a direct range is cut from its end, or taken
        // whole: read forward in pieces it would hand out its start first.
        let range_rows = self.row_budget.or(self.ends_first().then_some(u64::MAX));
        if let Some(budget) = range_rows
            && self.merge.is_none()
            && !self.range_resumes
            && let Some((segment, start_row, end_row)) = self.direct_range.take()
        {
            // A direct range decodes any rows of it: the reader's rows, and
            // the rest of the range stays queued.
            let (low, high, rest) = if self.ends_first() {
                let cut = end_row.saturating_sub(budget.max(1)).max(start_row);
                (cut, end_row, (start_row, cut))
            } else {
                let cut = start_row.saturating_add(budget.max(1)).min(end_row);
                (start_row, cut, (cut, end_row))
            };
            match self.decode_column_chunk_rows(&segment, low, high, memory_limit) {
                Ok(chunk) => {
                    if rest.0 < rest.1 {
                        self.direct_range = Some((segment, rest.0, rest.1));
                    }
                    return Ok(Some(vec![chunk]));
                }
                Err(StoreError::MemoryLimitExceeded { .. }) => {
                    self.direct_range = Some((segment, start_row, end_row));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// One round of the scan in which the thread that decodes a slice also
    /// consumes it: `fold` receives each chunk on the thread that decoded
    /// it, while the chunk's bytes are still in that core's cache, and what
    /// it returns is collected. `None` at the end of the stream.
    ///
    /// `fold` is also told where the chunk stands in the round: the
    /// position of its slice among at most `max_chunks`, and its own among
    /// the chunks that slice decoded to. Chunks reach `fold` in whatever
    /// order the pool decodes them; the two positions give the order
    /// [`Self::next_column_chunks`] would have returned them in.
    ///
    /// The round runs on the caller's pool, not the scan's own: a worker
    /// here does the decode and whatever `fold` does with the rows, so one
    /// pool carries the statement. `proceed` is asked before each slice is
    /// decoded; slices it turns away stay queued for the next call. Parts of
    /// the scan that are read one chunk at a time - memtable rows, a merge
    /// of overlapping segments, a bounded direct range - are decoded as
    /// they always are and folded on the calling thread.
    ///
    /// At most as many chunks as the pool has threads are alive at once, so
    /// each slice may take that share of `memory_limit`. A slice that does
    /// not fit its share is read again alone, with the whole of it.
    ///
    /// # Errors
    ///
    /// Returns a precise storage, corruption, schema, or memory-limit error.
    pub fn fold_column_chunks<T: Send>(
        &mut self,
        max_chunks: usize,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
        proceed: &(dyn Fn() -> bool + Sync),
        fold: &(dyn Fn(ProjectedColumnChunk, usize, usize) -> T + Sync),
    ) -> Result<Option<Vec<T>>, StoreError> {
        enum Outcome<T> {
            Folded(Vec<T>),
            Skipped,
            Refused,
        }
        loop {
            let one_at_a_time = !self.pending.is_empty()
                || self.memtable_cursor.is_some()
                || self.merge.is_some()
                || self.direct_range.is_some()
                // A reader that wants only so many rows, or reads from the
                // end, takes its slices the way that path cuts them.
                || self.row_budget.is_some()
                || self.ends_first();
            if one_at_a_time {
                let chunks = self.next_column_chunks_inner(max_chunks, memory_limit, prewhere)?;
                if chunks.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(
                    chunks
                        .into_iter()
                        .enumerate()
                        .map(|(position, chunk)| fold(chunk, position, 0))
                        .collect(),
                ));
            }
            self.fill_direct_slices(max_chunks.max(1))?;
            if !self.slices.is_empty() {
                break;
            }
            if !self.advance_part()? {
                return Ok(None);
            }
        }
        let chunk_count = max_chunks.max(1).min(self.slices.len());
        let taken: Vec<DirectSlice> = self.slices.drain(..chunk_count).collect();
        let alive = chunk_count.min(rayon::current_num_threads().max(1));
        let per_chunk_limit = memory_limit / alive;
        let outcomes = taken
            .par_iter()
            .enumerate()
            .map(|(position, slice)| {
                if !proceed() {
                    return Ok(Outcome::Skipped);
                }
                match self.decode_slice(slice, per_chunk_limit, prewhere) {
                    Ok(chunks) => Ok(Outcome::Folded(
                        chunks
                            .into_iter()
                            .enumerate()
                            .map(|(piece, chunk)| fold(chunk, position, piece))
                            .collect(),
                    )),
                    Err(StoreError::MemoryLimitExceeded { .. }) => Ok(Outcome::Refused),
                    Err(error) => Err(error),
                }
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let mut folded = Vec::with_capacity(chunk_count);
        let mut skipped = Vec::new();
        let mut refused = Vec::new();
        for (position, (slice, outcome)) in taken.into_iter().zip(outcomes).enumerate() {
            match outcome {
                Outcome::Folded(results) => folded.extend(results),
                Outcome::Skipped => skipped.push(slice),
                Outcome::Refused => refused.push((position, slice)),
            }
        }
        for slice in skipped.into_iter().rev() {
            self.slices.push_front(slice);
        }
        for (position, slice) in refused {
            // Alone, with the whole budget, and cut smaller when even that
            // is too little: the path a round of one slice takes. A slice
            // cut smaller leaves its tail queued as the direct range; the
            // tail is read here too, as further pieces of the same slice,
            // so no piece of it is placed after the slices that follow it.
            self.slices.push_front(slice);
            let mut piece = 0;
            loop {
                let chunks = self.next_column_chunks_inner(1, memory_limit, prewhere)?;
                for chunk in chunks {
                    folded.push(fold(chunk, position, piece));
                    piece += 1;
                }
                if self.direct_range.is_none() {
                    break;
                }
            }
        }
        Ok(Some(folded))
    }

    /// Decodes one overlay slice within `memory_limit`, halving it at block
    /// boundaries while it does not fit. The overlay must mask every slice
    /// it decodes, so a slice is never decoded unmasked in pieces; a single
    /// block that does not fit is a memory error.
    fn decode_overlay_slice_bounded(
        &self,
        slice: DirectSlice,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        let mut work = VecDeque::from([slice]);
        let mut chunks: Vec<ProjectedColumnChunk> = Vec::new();
        while let Some(slice) = work.pop_front() {
            // The pieces of one slice share its allowance: what the pieces
            // already decoded retain comes off what the next may take.
            let retained = chunks
                .iter()
                .map(|chunk| chunk.retained_bytes)
                .sum::<usize>();
            let remaining = memory_limit.saturating_sub(retained);
            match self.decode_slice(&slice, remaining, prewhere) {
                Ok(decoded) => chunks.extend(decoded),
                Err(error @ StoreError::MemoryLimitExceeded { .. }) => {
                    // A single block that does not fit is the real answer,
                    // with the request that failed.
                    let (head, tail) = self.split_overlay_slice(&slice).ok_or(error)?;
                    work.push_front(tail);
                    work.push_front(head);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(chunks)
    }

    /// Halves an overlay slice at the block boundary nearest its middle;
    /// `None` when it is a single block.
    fn split_overlay_slice(&self, slice: &DirectSlice) -> Option<(DirectSlice, DirectSlice)> {
        let (segment, start_row, end_row) = match slice {
            DirectSlice::Whole(segment) => (segment, 0, segment.row_count),
            DirectSlice::Range {
                segment,
                start_row,
                end_row,
            } => (segment, *start_row, *end_row),
        };
        let sparse = self.overlay.as_ref()?.sparse_of(segment);
        let middle = start_row + (end_row - start_row) / 2;
        let boundary = sparse
            .iter()
            .map(|(row, _)| *row)
            .filter(|row| *row > start_row && *row < end_row)
            .min_by_key(|row| row.abs_diff(middle))?;
        Some((
            DirectSlice::Range {
                segment: segment.clone(),
                start_row,
                end_row: boundary,
            },
            DirectSlice::Range {
                segment: segment.clone(),
                start_row: boundary,
                end_row,
            },
        ))
    }

    /// Says how many more rows the reader wants, or `None` when it wants
    /// every row. While a count is set a call decodes one work unit cut to
    /// about that many rows - the rows themselves in a direct segment, whole
    /// blocks where the memtable's rows are placed by block - and leaves the
    /// rest queued, instead of decoding a round of whole slices.
    ///
    /// The count is what to read next, not a bound on what the scan holds:
    /// a filter, the memtable's deletes or a superseded version can leave a
    /// unit with fewer rows than it was cut to, and the reader calls again
    /// until it has its rows or the stream ends. Every row is still handed
    /// out, in the same order, whatever counts are set between calls.
    pub fn set_row_budget(&mut self, rows: Option<usize>) {
        self.row_budget = rows.map(|rows| u64::try_from(rows).unwrap_or(u64::MAX));
    }

    /// Cuts the next direct slice to the first `budget` rows of it, leaving
    /// the rest as the slice after. An overlay slice is cut at the block
    /// boundary at or past them, where its key span is known. A segment the
    /// scanned range covers only in part is cut within its located run, or
    /// left whole when the run cannot be located.
    fn trim_front_slice(&mut self, budget: u64, memory_limit: usize) -> Result<(), StoreError> {
        let Some(front) = self.slices.front() else {
            return Ok(());
        };
        let (segment, start_row, end_row) = match front {
            DirectSlice::Range {
                segment,
                start_row,
                end_row,
            } => (segment.clone(), *start_row, *end_row),
            DirectSlice::Whole(segment) => {
                if segment.row_count <= budget || self.start == self.end {
                    return Ok(());
                }
                let Some((start_row, end_row)) = self.bounded_rows(segment, memory_limit)? else {
                    return Ok(());
                };
                (segment.clone(), start_row, end_row)
            }
        };
        let boundaries = self
            .overlay
            .as_ref()
            .map(|overlay| overlay.sparse_of(&segment).iter().map(|(row, _)| *row));
        let cut = if self.ends_first() {
            // The last rows of the slice: from the block boundary at or
            // before them.
            let wanted = end_row.saturating_sub(budget.max(1));
            if wanted <= start_row {
                return Ok(());
            }
            match boundaries {
                Some(rows) => rows.take_while(|row| *row <= wanted).last(),
                None => Some(wanted),
            }
        } else {
            let wanted = start_row.saturating_add(budget.max(1));
            if wanted >= end_row {
                return Ok(());
            }
            match boundaries {
                Some(mut rows) => rows.find(|row| *row >= wanted),
                None => Some(wanted),
            }
        };
        let Some(cut) = cut.filter(|cut| *cut > start_row && *cut < end_row) else {
            return Ok(());
        };
        let head = DirectSlice::Range {
            segment: segment.clone(),
            start_row,
            end_row: cut,
        };
        let tail = DirectSlice::Range {
            segment,
            start_row: cut,
            end_row,
        };
        let (first, second) = if self.ends_first() {
            (tail, head)
        } else {
            (head, tail)
        };
        self.slices[0] = first;
        self.slices.insert(1, second);
        Ok(())
    }

    /// Reads the scanned key range from its end: the last part first, the
    /// last rows of a segment first, each chunk's own rows still in key
    /// order. With a row count set ([`Self::set_row_budget`]) a call then
    /// decodes about that many of the rows nearest the end that are not
    /// yet read. A reader that wants the last rows in key order reads
    /// until it has them and [`Self::at_unit_boundary`] holds; what it
    /// read is then every visible row from some key on. Call before the
    /// first chunk is pulled.
    pub fn read_from_end(&mut self) {
        self.order = ReadOrder::EndFirst;
    }

    fn ends_first(&self) -> bool {
        self.order == ReadOrder::EndFirst
    }

    /// Whether the rows handed out so far, read from the end, are every
    /// row of the scan from some key on. Parts that only read forward - a
    /// row-wise merge, the memtable's own rows, a range too large for its
    /// allowance - are handed out start first, and are whole only when
    /// their last chunk has been.
    #[must_use]
    pub fn at_unit_boundary(&self) -> bool {
        self.merge.is_none()
            && self.memtable_cursor.is_none()
            && self.pending.is_empty()
            && !(self.range_resumes && self.direct_range.is_some())
    }

    fn fill_direct_slices(&mut self, wanted: usize) -> Result<(), StoreError> {
        while self.slices.len() < wanted {
            let Some(segment) = self.segments.get(self.next_segment).cloned() else {
                return Ok(());
            };
            self.next_segment += 1;
            let full_direct = self.start <= segment.min_key && self.end >= segment.max_key;
            if !full_direct || segment.row_count <= DIRECT_SLICE_ROWS {
                self.slices.push_back(DirectSlice::Whole(segment));
                continue;
            }
            let block = u64::try_from(segment::block_rows(
                &self.snapshot.directory,
                &segment,
                &self.snapshot.schema,
            )?)
            .unwrap_or(u64::MAX)
            .max(1);
            let rows = (DIRECT_SLICE_ROWS / block).max(1).saturating_mul(block);
            let first = self.slices.len();
            let mut start_row = 0;
            while start_row < segment.row_count {
                let end_row = start_row.saturating_add(rows).min(segment.row_count);
                self.slices.push_back(DirectSlice::Range {
                    segment: segment.clone(),
                    start_row,
                    end_row,
                });
                start_row = end_row;
            }
            if self.ends_first() {
                // The segment's last slice first.
                self.slices.make_contiguous()[first..].reverse();
            }
        }
        Ok(())
    }

    fn decode_slice(
        &self,
        slice: &DirectSlice,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        let sample = &self.prewhere_sample;
        let Some((predicate_ids, select)) = prewhere.filter(|_| sample.on) else {
            return self.decode_slice_judged(slice, memory_limit, prewhere);
        };
        if !sample.resumed.load(Ordering::Relaxed)
            && !sample
                .turn
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(PrewhereSample::EVERY)
        {
            // Unjudged: the slice decodes whole. An overlay still removes
            // the rows the memtable supersedes; that mask is its own.
            return self.decode_slice_judged(slice, memory_limit, None);
        }
        let select = |columns: &[DecodedColumn], row_count: usize| {
            let kept = select(columns, row_count)?;
            if kept.is_some() {
                sample.resumed.store(true, Ordering::Relaxed);
            }
            Ok(kept)
        };
        self.decode_slice_judged(slice, memory_limit, Some((predicate_ids, &select)))
    }

    fn decode_slice_judged(
        &self,
        slice: &DirectSlice,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        if self.overlay.is_some() {
            return self.decode_overlay_slice(slice, memory_limit, prewhere);
        }
        self.decode_slice_plain(slice, memory_limit, prewhere, self.filter_only)
            .map(|chunk| vec![chunk])
    }

    fn decode_slice_plain(
        &self,
        slice: &DirectSlice,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
        read_through: bool,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        match slice {
            DirectSlice::Whole(segment) => self.decode_column_chunk_maybe_filtered(
                segment.clone(),
                memory_limit,
                prewhere,
                read_through,
            ),
            DirectSlice::Range {
                segment,
                start_row,
                end_row,
            } => self.decode_range_maybe_filtered(
                segment,
                *start_row,
                *end_row,
                memory_limit,
                prewhere,
                read_through,
            ),
        }
    }

    /// The key span a direct slice covers, from the segment's block
    /// boundaries: the first key of its first block up to (excluding) the
    /// first key of the block after it, or the segment's last key.
    fn overlay_slice_span(
        &self,
        slice: &DirectSlice,
    ) -> (std::ops::Bound<PrimaryKey>, std::ops::Bound<PrimaryKey>) {
        use std::ops::Bound::{Excluded, Included};
        match slice {
            DirectSlice::Whole(segment) => (
                Included(segment.min_key.clone()),
                Included(segment.max_key.clone()),
            ),
            DirectSlice::Range {
                segment,
                start_row,
                end_row,
            } => {
                let sparse = self
                    .overlay
                    .as_ref()
                    .map(|state| state.sparse_of(segment))
                    .unwrap_or_default();
                let lo = sparse
                    .iter()
                    .find(|(row, _)| *row == *start_row)
                    .map_or_else(
                        || Included(segment.min_key.clone()),
                        |(_, key)| Included(key.clone()),
                    );
                let hi = sparse.iter().find(|(row, _)| *row == *end_row).map_or_else(
                    || Included(segment.max_key.clone()),
                    |(_, key)| Excluded(key.clone()),
                );
                (lo, hi)
            }
        }
    }

    /// Decodes a slice of an overlay part: the segment's rows minus those
    /// whose key the memtable holds (updated or deleted since the flush),
    /// followed by the memtable's live rows for the slice's key span. The
    /// mask rides the filter-first path as one more predicate column, so a
    /// slice with no memtable rows in its span costs what a direct slice
    /// costs, and one with a few costs one extra packed column.
    fn decode_overlay_slice(
        &self,
        slice: &DirectSlice,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
    ) -> Result<Vec<ProjectedColumnChunk>, StoreError> {
        let Some(key_ids) = self.overlay_key.as_deref() else {
            return self
                .decode_slice_plain(slice, memory_limit, prewhere, self.filter_only)
                .map(|chunk| vec![chunk]);
        };
        let mut memtable = self.overlay_span_rows(slice, key_ids.len())?;
        let key_bytes = memtable.keys.used_bytes();
        if memtable.is_empty() {
            return self
                .decode_slice_plain(slice, memory_limit, prewhere, self.filter_only)
                .map(|chunk| vec![chunk]);
        }
        self.mask_unwanted_live_rows(&mut memtable, memory_limit)?;
        if let Some((predicate_ids, select)) = prewhere.filter(|(ids, _)| !ids.is_empty()) {
            self.mask_unselected_live_rows(&mut memtable, predicate_ids, select, memory_limit)?;
        }
        let live = memtable.live();
        // The overlay's own working set comes out of the slice's allowance
        // before the decode: the memtable keys, the live-row list, and the
        // positions and ranges the mask can produce at worst (one per row).
        let slice_rows = match slice {
            DirectSlice::Whole(segment) => segment.row_count,
            DirectSlice::Range {
                start_row, end_row, ..
            } => end_row - start_row,
        };
        let slice_rows = usize::try_from(slice_rows).unwrap_or(usize::MAX);
        let overhead = memtable
            .len()
            .saturating_mul(size_of::<SpanRow<'_>>())
            .saturating_add(key_bytes)
            .saturating_add(live.len().saturating_mul(size_of::<SpanRow<'_>>()))
            .saturating_add(
                slice_rows.saturating_mul(size_of::<usize>() + size_of::<std::ops::Range<usize>>()),
            );
        let Some(decode_limit) = memory_limit
            .checked_sub(overhead)
            .filter(|limit| *limit > 0)
        else {
            return Err(StoreError::MemoryLimitExceeded {
                used: 0,
                requested: overhead,
                limit: memory_limit,
            });
        };
        let mut ids = prewhere.map_or_else(Vec::new, |(ids, _)| ids.to_vec());
        let key_indices = key_ids
            .iter()
            .map(|key_id| {
                ids.iter().position(|id| id == key_id).unwrap_or_else(|| {
                    ids.push(*key_id);
                    ids.len() - 1
                })
            })
            .collect::<Vec<_>>();
        let caller = prewhere;
        // What the memtable's rows do to the rows the decode keeps. The
        // decode itself is left alone: the selector answers what the scan's
        // own filter answers, so a slice with no filter decodes whole, in
        // the bulk paths, and the edits are applied to the finished chunk -
        // an updated row written over the one it supersedes, a deleted one
        // closed up, an inserted one placed by key. Cutting each superseded
        // row out of the decode instead split it into as many ranges as rows
        // had changed, decoded value by value, and then copied every column
        // again to put the new versions back.
        let edits = std::sync::Mutex::new(None);
        let select = |columns: &[DecodedColumn], row_count: usize| {
            let kept = match caller {
                Some((caller_ids, select)) => select(&columns[..caller_ids.len()], row_count)?
                    .map(|kept| kept.into_ranges(row_count)),
                None => None,
            };
            let key_columns = key_indices
                .iter()
                .map(|index| &columns[*index])
                .collect::<Vec<_>>();
            *edits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(overlay_positions(
                &key_columns,
                row_count,
                kept.as_ref().map(|kept| kept.ranges.as_slice()),
                &memtable,
            ));
            Ok(kept)
        };
        // Never read through unselected: the selector is where the
        // memtable's rows are placed.
        let segment_chunk =
            self.decode_slice_plain(slice, decode_limit, Some((&ids, &select)), false)?;
        let edits = edits
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ok_or_else(|| {
                StoreError::FormatLimit("the memtable overlay could not place its rows".into())
            })?;
        self.apply_overlay_edits(segment_chunk, &edits, &live, decode_limit)
            .map(|chunk| vec![chunk])
    }

    /// The memtable rows of a slice's key span in key order.
    fn overlay_span_rows(
        &self,
        slice: &DirectSlice,
        key_parts: usize,
    ) -> Result<SpanRows<'_>, StoreError> {
        let span = self.overlay_slice_span(slice);
        let mut rows = SpanRows::new();
        if bound_range_is_searchable(&span.0, &span.1) {
            let mut cursor = self.overlay_rows.range(&span.0, &span.1)?;
            while let Some((key, row)) = cursor.next()? {
                let parts = match key {
                    SpanKey::Memtable(key) => {
                        rows.keys.push(key);
                        key.parts().len()
                    }
                    held => {
                        let key = cursor.parts(held).ok_or_else(|| {
                            StoreError::FormatLimit("an overlay row lost its key".into())
                        })?;
                        rows.keys.push_ref(key);
                        key.parts().count()
                    }
                };
                if parts != key_parts {
                    return Err(StoreError::FormatLimit(
                        "the memtable overlay's key has a different number of parts".into(),
                    ));
                }
                rows.rows.push(row);
            }
        }
        Ok(rows)
    }

    /// The segment chunk with the memtable's edits applied to every
    /// projected column: live rows written over the rows they supersede,
    /// deleted rows closed up, and the remaining live rows placed at their
    /// positions in the finished chunk, which stays in key order - a
    /// consumer that takes the first value it meets for a group (as the
    /// source does, in key order) must meet the same one.
    #[allow(clippy::too_many_lines)]
    fn apply_overlay_edits(
        &self,
        segment_chunk: ProjectedColumnChunk,
        edits: &OverlayEdits,
        live: &[SpanRow<'_>],
        decode_limit: usize,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let misplaced =
            || StoreError::FormatLimit("the memtable overlay could not place its rows".into());
        if edits.placements.len() != live.len() {
            return Err(misplaced());
        }
        if edits.deletes.is_empty() && live.is_empty() {
            return Ok(segment_chunk);
        }
        let ProjectedColumnChunk {
            columns,
            row_count,
            stats,
            retained_bytes: _,
            prefiltered,
            column_decode,
        } = segment_chunk;
        let in_chunk = |row: &usize| *row < row_count;
        if !edits.deletes.iter().all(in_chunk)
            || !edits.placements.iter().all(|placement| match placement {
                Placement::Replace(row) => in_chunk(row),
                Placement::Insert(_) => true,
            })
        {
            return Err(misplaced());
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
        let survivors = row_count - edits.deletes.len();
        let surviving = if edits.deletes.is_empty() {
            Vec::new()
        } else {
            subtract_positions(std::iter::once(0..row_count).collect(), &edits.deletes)
        };
        let inserted = edits
            .placements
            .iter()
            .filter(|placement| matches!(placement, Placement::Insert(_)))
            .count();
        // A memtable row's values are its own; a layer row's are read from
        // its segment here, for the projected columns alone, as packed as
        // the chunk they are written into.
        let cells = LiveCells::read(
            &self.overlay_rows,
            &self.snapshot.directory,
            &self.snapshot.schema,
            live,
            projection,
            decode_limit,
        )?;
        let layer_bytes = cells.retained_bytes();
        let columns = columns
            .into_iter()
            .enumerate()
            .map(|(index, column)| {
                let mut replaces = Vec::with_capacity(live.len() - inserted);
                let mut inserts = Vec::with_capacity(inserted);
                for (row, placement) in edits.placements.iter().enumerate() {
                    let cell = cells.cell(row, index);
                    match placement {
                        Placement::Replace(at) => replaces.push((*at, cell)),
                        Placement::Insert(at) => inserts.push((*at, cell)),
                    }
                }
                let mut column = column.replace_cells(&replaces);
                if !edits.deletes.is_empty() {
                    compact_decoded_column(&mut column, &surviving, survivors);
                }
                column.interleave_cells(&inserts)
            })
            .collect::<Vec<_>>();
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<DecodedColumn>()),
            )
            .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum());
        // The columns read from the layer were held beside the chunk while
        // it was edited.
        if retained_bytes.saturating_add(layer_bytes) > decode_limit {
            return Err(StoreError::MemoryLimitExceeded {
                used: 0,
                requested: retained_bytes.saturating_add(layer_bytes),
                limit: decode_limit,
            });
        }
        Ok(ProjectedColumnChunk {
            // A row placed from the memtable was not judged by the filter
            // that judged the segment's.
            prefiltered: prefiltered && live.is_empty(),
            columns,
            row_count: survivors + inserted,
            stats,
            retained_bytes,
            column_decode,
        })
    }

    /// The filter-first path for one row range of a direct segment: the
    /// predicate columns decode for the range alone, the selector picks the
    /// surviving sub-ranges relative to it, and those decode in full at
    /// their absolute positions. Without a selector, or when it keeps
    /// everything, the range decodes whole.
    #[allow(clippy::too_many_lines)]
    fn decode_range_maybe_filtered(
        &self,
        segment: &segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
        read_through: bool,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let Some((predicate_ids, select)) = prewhere.filter(|(ids, _)| !ids.is_empty()) else {
            return self.decode_column_chunk_rows(segment, start_row, end_row, memory_limit);
        };
        if let Some(chunk) = self.decode_by_side_index(
            segment,
            start_row,
            end_row,
            memory_limit,
            predicate_ids,
            select,
        )? {
            return Ok(chunk);
        }
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
        let start = usize::try_from(start_row)
            .map_err(|_| StoreError::FormatLimit("range start exceeds usize".into()))?;
        let end = usize::try_from(end_row)
            .map_err(|_| StoreError::FormatLimit("range end exceeds usize".into()))?;
        let row_count = end.saturating_sub(start);
        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        if let Some((candidates, skipped)) = self.value_candidate_ranges(segment, start, end)? {
            return self.decode_value_candidates(
                segment,
                &candidates,
                skipped,
                predicate_ids,
                &map_projection(predicate_ids)?,
                select,
                usize::from(start_row == 0),
                &scan_budget,
            );
        }
        // Neither the index nor a block's values left a row unread, and
        // the scan decodes no column beyond these: selecting here would
        // only copy the rows the reader's own filter judges as cheaply.
        if read_through {
            return self.decode_column_chunk_rows(segment, start_row, end_row, memory_limit);
        }
        let fetch = segment::read_projected_columns(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            &map_projection(predicate_ids)?,
            start,
            end,
            &scan_budget,
        )?;
        let ranges = select(&fetch.columns, row_count)
            .map_err(StoreError::FormatLimit)?
            .map(|kept| kept.into_ranges(row_count));
        if predicate_ids == self.column_ids {
            return retain_predicate_fetch(
                fetch,
                ranges.as_ref(),
                row_count,
                usize::from(start_row == 0),
                &scan_budget,
            );
        }
        let Some(PrewhereRanges { ranges, exact, .. }) = ranges else {
            return self.project_after_predicates(
                segment,
                predicate_ids,
                fetch,
                None,
                KeptRows::Span(start, end),
                false,
                usize::from(start_row == 0),
                &scan_budget,
            );
        };
        // The selector saw the range's rows from zero; the segment reader
        // wants their positions in the segment.
        let absolute = ranges
            .iter()
            .map(|range| range.start + start..range.end + start)
            .collect::<Vec<_>>();
        self.project_after_predicates(
            segment,
            predicate_ids,
            fetch,
            Some(&ranges),
            KeptRows::Ranges(&absolute),
            exact,
            usize::from(start_row == 0),
            &scan_budget,
        )
    }

    /// The rows of `start..end` of a direct segment that lie in blocks the
    /// scan's value bounds cannot rule out, and how many blocks they rule
    /// out; `None` when no bound applies or none is ruled out.
    ///
    /// A text predicate rules a block out when it accepts none of the
    /// distinct values the block holds, and no NULL where the block has
    /// one.
    ///
    /// A block is ruled out when, for some bound, its stored extremes lie
    /// wholly outside the bound or every row of it is NULL (a NULL satisfies
    /// no range or equality comparison). Every bound comes from a top-level
    /// conjunct of the scan's predicates, so a row in such a block fails the
    /// filter the executor would apply to it anyway.
    ///
    /// Only the direct filter-first path asks: its rows are the segment's
    /// own, unshadowed by any other segment. An overlay's memtable rows are
    /// interleaved after the segment rows are chosen, whatever this skips.
    #[allow(clippy::too_many_lines)]
    fn value_candidate_ranges(
        &self,
        segment: &segment::SegmentMeta,
        start: usize,
        end: usize,
    ) -> Result<Option<ValueCandidates>, StoreError> {
        if (self.value_bounds.is_empty() && self.text_filters.is_empty()) || start >= end {
            return Ok(None);
        }
        // The blocks' row spans, from the first directory that answers, and
        // whether each may still hold a row the predicates keep.
        let mut spans: Option<Vec<(usize, usize)>> = None;
        let mut keep: Vec<bool> = Vec::new();
        // Every column of a segment is cut at the same rows; a directory
        // that disagrees proves nothing.
        let mut aligned = |found: Vec<(usize, usize)>, keep: &mut Vec<bool>| -> bool {
            if let Some(first) = &spans {
                *first == found
            } else {
                *keep = vec![true; found.len()];
                spans = Some(found);
                true
            }
        };
        for bound in &self.value_bounds {
            if bound.lower.is_none() && bound.upper.is_none() {
                continue;
            }
            let Some(extremes) = segment::block_extremes(
                &self.snapshot.directory,
                segment,
                &self.snapshot.schema,
                bound.column_id,
                bound.domain,
            )?
            else {
                continue;
            };
            if !aligned(
                extremes
                    .iter()
                    .map(|block| (block.start, block.end))
                    .collect(),
                &mut keep,
            ) {
                return Ok(None);
            }
            for (flag, block) in keep.iter_mut().zip(extremes.iter()) {
                let ruled_out = match block.range {
                    None => true,
                    Some((least, greatest)) => {
                        bound.lower.is_some_and(|lower| greatest < lower)
                            || bound.upper.is_some_and(|upper| least > upper)
                    }
                };
                if ruled_out {
                    *flag = false;
                }
            }
        }
        for filter in &self.text_filters {
            let Some(held) = segment::block_text_values(
                &self.snapshot.directory,
                segment,
                &self.snapshot.schema,
                filter.column_id,
            )?
            else {
                continue;
            };
            if !aligned(
                held.blocks
                    .iter()
                    .map(|block| (block.start, block.end))
                    .collect(),
                &mut keep,
            ) {
                return Ok(None);
            }
            // The predicate is asked once per distinct value of the
            // segment, never per block or row.
            let mut admitted = [0_u64; 4];
            for (position, value) in held.values.iter().enumerate() {
                if (filter.admits)(Some(value)) {
                    admitted[position / 64] |= 1 << (position % 64);
                }
            }
            let admits_null = (filter.admits)(None);
            for (flag, block) in keep.iter_mut().zip(held.blocks.iter()) {
                let any = block
                    .held
                    .iter()
                    .zip(&admitted)
                    .any(|(held, admitted)| held & admitted != 0);
                let null_passes = admits_null && block.nulls;
                if !any && !null_passes {
                    *flag = false;
                }
            }
        }
        let Some(spans) = spans else {
            return Ok(None);
        };
        let mut candidates: Vec<std::ops::Range<usize>> = Vec::new();
        let mut skipped = 0_usize;
        for ((block_start, block_end), kept) in spans.iter().zip(&keep) {
            let lo = (*block_start).max(start);
            let hi = (*block_end).min(end);
            if lo >= hi {
                continue;
            }
            if !kept {
                skipped += 1;
                continue;
            }
            match candidates.last_mut() {
                Some(last) if last.end == lo => last.end = hi,
                _ => candidates.push(lo..hi),
            }
        }
        if skipped == 0 {
            return Ok(None);
        }
        Ok(Some((candidates, skipped)))
    }

    /// The filter-first read of a direct segment restricted to the
    /// `candidates` rows (absolute, ascending, disjoint) that block value
    /// skipping left: the predicate columns decode for those rows alone,
    /// the selector judges them as one run, and its choice maps back to
    /// segment positions for the rest of the projection.
    #[allow(clippy::too_many_arguments)]
    fn decode_value_candidates(
        &self,
        segment: &segment::SegmentMeta,
        candidates: &[std::ops::Range<usize>],
        skipped: usize,
        predicate_ids: &[u32],
        predicate_projection: &[usize],
        select: PrewhereSelect<'_>,
        segments_read: usize,
        scan_budget: &segment::ScanMemoryBudget<'_>,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let fetch = segment::read_projected_column_ranges(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            predicate_projection,
            candidates,
            scan_budget,
        )?;
        let row_count = candidates
            .iter()
            .map(std::iter::ExactSizeIterator::len)
            .sum::<usize>();
        let ranges = select(&fetch.columns, row_count)
            .map_err(StoreError::FormatLimit)?
            .map(|kept| kept.into_ranges(row_count));
        let mut chunk = if predicate_ids == self.column_ids {
            retain_predicate_fetch(
                fetch,
                ranges.as_ref(),
                row_count,
                segments_read,
                scan_budget,
            )?
        } else if let Some(PrewhereRanges { ranges, exact, .. }) = ranges {
            check_selected_ranges(&ranges, row_count)?;
            let absolute = candidate_positions(&ranges, candidates);
            self.project_after_predicates(
                segment,
                predicate_ids,
                fetch,
                Some(&ranges),
                KeptRows::Ranges(&absolute),
                exact,
                segments_read,
                scan_budget,
            )?
        } else {
            self.project_after_predicates(
                segment,
                predicate_ids,
                fetch,
                None,
                KeptRows::Ranges(candidates),
                false,
                segments_read,
                scan_budget,
            )?
        };
        chunk.stats.blocks_value_skipped += skipped;
        Ok(chunk)
    }

    /// Finishes a filter-first read once the selector has judged the
    /// predicate columns: the projection for the kept rows, with every
    /// projected column the predicate fetch already holds compacted from it
    /// rather than decoded a second time. Only the other columns are read,
    /// at `rows`, the kept rows' positions in the segment.
    ///
    /// A join key handed to a scan as a runtime filter is both a predicate
    /// and an output column, so without this every block of it was decoded
    /// twice - on a scan the filter keeps whole, twice for nothing.
    ///
    /// `kept` is relative to the predicate fetch's rows (`None`: all of
    /// them) and must name the same rows as `rows`.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn project_after_predicates(
        &self,
        segment: &segment::SegmentMeta,
        predicate_ids: &[u32],
        predicate: segment::ProjectedColumnFetch,
        kept: Option<&[std::ops::Range<usize>]>,
        rows: KeptRows<'_>,
        prefiltered: bool,
        segments_read: usize,
        scan_budget: &segment::ScanMemoryBudget<'_>,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let predicate_rows = predicate.columns.first().map(DecodedColumn::len);
        let row_count = match rows {
            KeptRows::Span(start, end) => end.saturating_sub(start),
            KeptRows::Ranges(ranges) => ranges.iter().map(std::iter::ExactSizeIterator::len).sum(),
            KeptRows::Mask(words) => words.iter().map(|word| word.count_ones() as usize).sum(),
        };
        let reuse = self
            .column_ids
            .iter()
            .map(|id| predicate_ids.iter().position(|predicate| predicate == id))
            .collect::<Vec<_>>();
        if let Some(kept) = kept {
            check_selected_ranges(kept, predicate_rows.unwrap_or(0))?;
            let selected = kept
                .iter()
                .map(std::iter::ExactSizeIterator::len)
                .sum::<usize>();
            if selected != row_count {
                return Err(StoreError::FormatLimit(
                    "kept predicate rows disagree with the rows to read".into(),
                ));
            }
        } else if let KeptRows::Mask(words) = rows {
            let fetched = predicate_rows.unwrap_or(0);
            if words.len() != fetched.div_ceil(64)
                || (!fetched.is_multiple_of(64)
                    && words.last().is_some_and(|last| last >> (fetched % 64) != 0))
            {
                return Err(StoreError::FormatLimit(
                    "kept predicate rows disagree with the rows to read".into(),
                ));
            }
        } else if predicate_rows.is_some_and(|rows| rows != row_count) {
            return Err(StoreError::FormatLimit(
                "predicate rows disagree with the rows to read".into(),
            ));
        }
        // Keep only the predicate columns the projection reuses, compacted
        // to the kept rows, and hold the budget for just those while the
        // rest decode: the peak stays what reading them apart cost.
        let mut predicate_columns = predicate
            .columns
            .into_iter()
            .enumerate()
            .map(|(index, mut column)| {
                reuse.contains(&Some(index)).then(|| {
                    if let Some(kept) = kept {
                        compact_decoded_column(&mut column, kept, row_count);
                    } else if let KeptRows::Mask(words) = rows {
                        compact_decoded_column_by_mask(&mut column, words, row_count);
                    }
                    column
                })
            })
            .collect::<Vec<_>>();
        let held = predicate_columns
            .iter()
            .flatten()
            .map(DecodedColumn::retained_bytes)
            .sum::<usize>();
        scan_budget.release(predicate.reserved_bytes);
        scan_budget.reserve(held)?;
        let rest = self
            .column_ids
            .iter()
            .zip(&reuse)
            .filter(|(_, reused)| reused.is_none())
            .map(|(id, _)| {
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
        let rest = if rest.is_empty() {
            None
        } else {
            Some(match rows {
                KeptRows::Span(start, end) => segment::read_projected_columns(
                    &self.snapshot.directory,
                    segment,
                    &self.snapshot.schema,
                    &rest,
                    start,
                    end,
                    scan_budget,
                )?,
                KeptRows::Ranges(ranges) => segment::read_projected_column_ranges(
                    &self.snapshot.directory,
                    segment,
                    &self.snapshot.schema,
                    &rest,
                    ranges,
                    scan_budget,
                )?,
                KeptRows::Mask(words) => segment::read_projected_column_mask(
                    &self.snapshot.directory,
                    segment,
                    &self.snapshot.schema,
                    &rest,
                    words,
                    scan_budget,
                )?,
            })
        };
        let mut stats = ScanStats {
            segments_read,
            blocks_read: predicate.blocks_read,
            blocks_pruned: predicate.blocks_pruned,
            blocks_decoded: predicate.blocks_decoded,
            ..ScanStats::default()
        };
        let mut reserved = held;
        let mut rest_columns = Vec::new().into_iter();
        let mut column_decode = predicate.column_decode;
        if let Some(fetch) = rest {
            stats.blocks_read += fetch.blocks_read;
            stats.blocks_pruned += fetch.blocks_pruned;
            stats.blocks_decoded += fetch.blocks_decoded;
            reserved = reserved.saturating_add(fetch.reserved_bytes);
            rest_columns = fetch.columns.into_iter();
            column_decode.extend(fetch.column_decode);
        }
        let stats = stats.with_decode(&column_decode);
        let mut columns = Vec::with_capacity(self.column_ids.len());
        for (position, reused) in reuse.iter().enumerate() {
            let column = match reused {
                Some(index) => {
                    // A column projected twice takes a copy the second time.
                    let later = reuse[position + 1..].contains(&Some(*index));
                    let slot = &mut predicate_columns[*index];
                    if later { slot.clone() } else { slot.take() }
                }
                None => rest_columns.next(),
            };
            columns.push(column.ok_or_else(|| {
                StoreError::FormatLimit("a projected column is missing from its fetch".into())
            })?);
        }
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<DecodedColumn>()),
            )
            .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum());
        scan_budget.release(reserved);
        scan_budget.reserve(retained_bytes)?;
        Ok(ProjectedColumnChunk {
            columns,
            row_count,
            stats,
            retained_bytes,
            prefiltered,
            column_decode,
        })
    }

    /// Chooses how the next filter-first rounds judge their slices: every
    /// one (`false`, the default), or one in a few with the rest decoded
    /// whole until a judged slice is restricted (`true`). A reader turns
    /// sampling on after a round whose selector restricted nothing and off
    /// after one that did; the answers are the same either way, since a
    /// slice decoded whole is one the selector was allowed to keep whole.
    pub fn sample_prewhere(&mut self, sample: bool) {
        self.prewhere_sample.on = sample;
        *self.prewhere_sample.resumed.get_mut() = false;
    }

    /// Sets the predicates that each read one text column, so the
    /// filter-first path skips the direct blocks holding no value they
    /// accept. Every one must be a top-level conjunct of the scan's filter.
    pub fn set_text_filters(&mut self, filters: Vec<segment::TextValueFilter>) {
        self.text_filters = filters;
    }

    /// Sets the side-index request (see
    /// [`super::side_index`]). Callers set it only while the index is on.
    pub fn set_index_lookup(&mut self, lookup: super::side_index::IndexLookup) {
        self.index_lookup = Some(lookup);
    }

    /// Says the scan decodes nothing beyond the columns its filter reads,
    /// and selects first only for the rows that leaves unread. There is no
    /// second column to save, so a side-index lookup is read by only when
    /// it names very few rows; and where neither a lookup nor a block's
    /// values rule rows out, the slice is decoded whole and unselected,
    /// for the reader's own filter to judge, rather than selected and
    /// copied here first.
    pub fn set_filter_only(&mut self, filter_only: bool) {
        self.filter_only = filter_only;
    }

    /// Whether any side-index request names exact values rather than a
    /// span.
    #[must_use]
    pub fn has_value_index_lookup(&self) -> bool {
        self.index_lookup
            .iter()
            .chain(&self.index_alternates)
            .any(|lookup| matches!(lookup.probe, super::side_index::IndexProbe::Values(_)))
    }

    /// Adds a side-index request beside the one set: both name every row
    /// the scan wants, and each segment is read by whichever names fewer
    /// of its rows. Becomes the request when none is set.
    pub fn add_index_lookup(&mut self, lookup: super::side_index::IndexLookup) {
        if self.index_lookup.is_none() {
            self.index_lookup = Some(lookup);
        } else {
            self.index_alternates.push(lookup);
        }
    }

    /// The side-index lookup as a test of whole rows (the memtable's, or a
    /// layered cluster's resolved ones): the lookup and its column's schema
    /// position. A row it rejects is one the scan does not want, so it is
    /// left out rather than materialized for the filter to drop.
    /// Turns the overlay's live rows the side-index lookup rejects into
    /// masks: such a row still supersedes its segment row, it is only not
    /// interleaved.
    fn mask_unwanted_live_rows<'a>(
        &'a self,
        rows: &mut SpanRows<'a>,
        memory_limit: usize,
    ) -> Result<(), StoreError> {
        let Some(mut admission) = self.row_admission() else {
            return Ok(());
        };
        // A layer row is judged by the one column the lookup reads.
        let live = rows.live();
        let cells = if live
            .iter()
            .any(|row| matches!(row, SpanRow::Layer { .. } | SpanRow::Image { .. }))
        {
            Some(LiveCells::read(
                &self.overlay_rows,
                &self.snapshot.directory,
                &self.snapshot.schema,
                &live,
                vec![admission.position()],
                memory_limit,
            )?)
        } else {
            None
        };
        let mut at = 0;
        for row in &mut rows.rows {
            let wanted = match *row {
                SpanRow::Mask => continue,
                SpanRow::Row(stored) => admission.admits(stored),
                SpanRow::Layer { .. } | SpanRow::Image { .. } => cells
                    .as_ref()
                    .is_none_or(|cells| admission.admits_cell(cells.cell(at, 0))),
            };
            at += 1;
            if !wanted {
                *row = SpanRow::Mask;
            }
        }
        Ok(())
    }

    /// Turns the overlay's live rows the scan's own filter rejects into
    /// masks, as [`Self::mask_unwanted_live_rows`] does for an index lookup:
    /// the predicate columns are built from the rows in packed form and
    /// judged by the selector that judges the segment's, so a filter that
    /// keeps a handful of segment rows no longer has every changed row of
    /// the slice interleaved beside them for the filter above to drop.
    ///
    /// The selector may keep rows that fail (it promises only that a row it
    /// drops fails), which is what the segment rows it keeps are held to.
    fn mask_unselected_live_rows(
        &self,
        rows: &mut SpanRows<'_>,
        predicate_ids: &[u32],
        select: PrewhereSelect<'_>,
        memory_limit: usize,
    ) -> Result<(), StoreError> {
        let live = rows.live();
        if live.is_empty() {
            return Ok(());
        }
        let positions = predicate_ids
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
            .collect::<Result<Vec<_>, StoreError>>()?;
        let cells = LiveCells::read(
            &self.overlay_rows,
            &self.snapshot.directory,
            &self.snapshot.schema,
            &live,
            positions.clone(),
            memory_limit,
        )?;
        let columns = positions
            .iter()
            .enumerate()
            .map(|(column, position)| {
                let inserts = (0..live.len())
                    .map(|at| (at, cells.cell(at, column)))
                    .collect::<Vec<_>>();
                empty_packed_column(self.snapshot.schema.columns()[*position].data_type())
                    .interleave_cells(&inserts)
            })
            .collect::<Vec<_>>();
        let Some(kept) = select(&columns, live.len()).map_err(StoreError::FormatLimit)? else {
            return Ok(());
        };
        let kept = kept.into_ranges(live.len()).ranges;
        check_selected_ranges(&kept, live.len())?;
        let mut kept = kept.into_iter().peekable();
        for (index, row) in rows
            .rows
            .iter_mut()
            .filter(|row| !matches!(row, SpanRow::Mask))
            .enumerate()
        {
            while kept.peek().is_some_and(|range| range.end <= index) {
                kept.next();
            }
            if kept.peek().is_none_or(|range| range.start > index) {
                *row = SpanRow::Mask;
            }
        }
        Ok(())
    }

    fn row_admission(&self) -> Option<super::side_index::RowAdmission<'_>> {
        let lookup = self.index_lookup.as_ref()?;
        let position = self
            .snapshot
            .schema
            .columns()
            .iter()
            .position(|column| column.id() == lookup.column_id)?;
        Some(super::side_index::RowAdmission::new(lookup, position))
    }

    /// Where the first `k` rows of this scan's segments in the order of the
    /// integer column `column_id` end, and whether a NULL may sort among
    /// them: the bound over the segments' postings (see the side index's
    /// order bound), with the memtable's rows counted as possible NULLs
    /// under a nullable column, since nothing indexes them. A scan
    /// restricted to the rows at or before the bound answers the first `k`
    /// exactly only when at least `k` rows come back: superseded and
    /// deleted segment rows count toward the bound but not toward the
    /// answer.
    ///
    /// # Errors
    ///
    /// Returns a storage error reading a segment's postings.
    pub fn side_index_order_bound(
        &self,
        column_id: u32,
        k: usize,
        descending: bool,
    ) -> Result<Option<(i128, bool)>, StoreError> {
        // Every segment the scan's key range overlaps: an open scan has
        // already dealt them out into its parts.
        let segments = self
            .snapshot
            .manifest
            .segments
            .iter()
            .filter(|segment| segment.max_key >= self.start && segment.min_key <= self.end)
            .cloned()
            .collect::<Vec<_>>();
        Ok(super::side_index::order_bound(
            &self.snapshot.directory,
            &segments,
            &self.snapshot.schema,
            column_id,
            k,
            descending,
        )?
        .map(|(bound, nulls)| {
            let nullable = self
                .snapshot
                .schema
                .columns()
                .iter()
                .find(|column| column.id() == column_id)
                .is_some_and(pintail_types::Column::is_nullable);
            // A segment's own NULLs count whatever the schema says now; the
            // memtable's rows can hold one only under a nullable column.
            (
                bound,
                nulls || (nullable && !self.snapshot.memtable.is_empty()),
            )
        }))
    }

    /// The side-index request set so far, if any.
    #[must_use]
    pub const fn index_lookup(&self) -> Option<&super::side_index::IndexLookup> {
        self.index_lookup.as_ref()
    }

    /// The filter-first decode over only the rows the side index names for
    /// the scan's lookup: the predicate columns decode for those rows, the
    /// selector judges them, and the projection decodes for the survivors.
    /// `None` when the index is off, declines the column, or finds too many
    /// rows to be worth reading apart.
    #[allow(clippy::too_many_lines)]
    fn decode_by_side_index(
        &self,
        segment: &segment::SegmentMeta,
        start_row: u64,
        end_row: u64,
        memory_limit: usize,
        predicate_ids: &[u32],
        select: PrewhereSelect<'_>,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        let Some(first) = self.index_lookup.as_ref() else {
            if super::side_index::side_index_trace() {
                pintail_log::log_info!(
                    "side index unasked file={}: the scan has no lookup",
                    segment.file_name
                );
            }
            return Ok(None);
        };
        let (Ok(start), Ok(end)) = (usize::try_from(start_row), usize::try_from(end_row)) else {
            return Ok(None);
        };
        let share = if self.filter_only {
            super::side_index::MAX_FILTER_ONLY_CANDIDATE_SHARE
        } else {
            super::side_index::MAX_CANDIDATE_SHARE
        };
        // Every lookup names a superset of the wanted rows, so the one
        // naming the fewest of this slice's rows is the one to read by.
        let mut chosen: Option<(usize, u32, Vec<std::ops::Range<usize>>)> = None;
        for lookup in std::iter::once(first).chain(&self.index_alternates) {
            let Some(postings) = super::side_index::postings(
                &self.snapshot.directory,
                segment,
                &self.snapshot.schema,
                lookup.column_id,
                &lookup.key,
            )?
            else {
                if super::side_index::side_index_trace() {
                    pintail_log::log_info!(
                        "side index declined file={} column={}: no postings for the column",
                        segment.file_name,
                        lookup.column_id
                    );
                }
                continue;
            };
            let Some(candidates) =
                postings.candidate_ranges_within(&lookup.probe, start, end, share)
            else {
                if super::side_index::side_index_trace() {
                    pintail_log::log_info!(
                        "side index declined file={} column={} rows={start}..{end}: the lookup names too many rows",
                        segment.file_name,
                        lookup.column_id
                    );
                }
                continue;
            };
            let rows = candidates
                .iter()
                .map(std::iter::ExactSizeIterator::len)
                .sum::<usize>();
            if chosen.as_ref().is_none_or(|(fewest, _, _)| rows < *fewest) {
                chosen = Some((rows, lookup.column_id, candidates));
            }
        }
        let Some((rows, column_id, candidates)) = chosen else {
            return Ok(None);
        };
        if super::side_index::side_index_trace() {
            pintail_log::log_info!(
                "side index used file={} column={column_id} rows={start}..{end}: {rows} candidates",
                segment.file_name
            );
        }
        super::side_index::note_useful(&self.snapshot.directory, column_id);
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
        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        let candidate_rows = candidates
            .iter()
            .map(std::iter::ExactSizeIterator::len)
            .sum::<usize>();
        // Even with no candidates the selector runs, over zero rows: an
        // overlay part places its memtable rows from what it sees there.
        let fetch = segment::read_projected_column_ranges(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            &map_projection(predicate_ids)?,
            &candidates,
            &scan_budget,
        )?;
        let selected = select(&fetch.columns, candidate_rows)
            .map_err(StoreError::FormatLimit)?
            .map(|kept| kept.into_ranges(candidate_rows));
        let segments_read = usize::from(start_row == 0);
        let mut chunk = match selected {
            Some(PrewhereRanges { ranges, exact, .. }) => {
                let absolute = super::side_index::absolute_ranges(&candidates, &ranges);
                self.project_after_predicates(
                    segment,
                    predicate_ids,
                    fetch,
                    Some(&ranges),
                    KeptRows::Ranges(&absolute),
                    exact,
                    segments_read,
                    &scan_budget,
                )?
            }
            // The selector judged nothing: the candidates pass only the
            // lookup's own column test.
            None => self.project_after_predicates(
                segment,
                predicate_ids,
                fetch,
                None,
                KeptRows::Ranges(&candidates),
                false,
                segments_read,
                &scan_budget,
            )?,
        };
        chunk.stats.index_slices += 1;
        Ok(Some(chunk))
    }

    /// Routes one segment through the filter-first path when a predicate
    /// selector applies and the segment decodes as a full direct chunk.
    #[allow(clippy::too_many_lines)] // one path per way a segment is left partly unread
    fn decode_column_chunk_maybe_filtered(
        &self,
        segment: segment::SegmentMeta,
        memory_limit: usize,
        prewhere: Option<(&[u32], PrewhereSelect<'_>)>,
        read_through: bool,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let full_direct = self.start <= segment.min_key && self.end >= segment.max_key;
        if let Some((predicate_ids, select)) = prewhere
            && full_direct
            && !predicate_ids.is_empty()
        {
            if let Some(chunk) = self.decode_by_side_index(
                &segment,
                0,
                segment.row_count,
                memory_limit,
                predicate_ids,
                select,
            )? {
                return Ok(chunk);
            }
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
            if let Some((candidates, skipped)) =
                self.value_candidate_ranges(&segment, 0, row_count)?
            {
                return self.decode_value_candidates(
                    &segment,
                    &candidates,
                    skipped,
                    predicate_ids,
                    &predicate_projection,
                    select,
                    1,
                    &scan_budget,
                );
            }
            if read_through {
                return self.decode_column_chunk(segment, memory_limit);
            }
            let fetch = segment::read_projected_columns(
                &self.snapshot.directory,
                &segment,
                &self.snapshot.schema,
                &predicate_projection,
                0,
                row_count,
                &scan_budget,
            )?;
            let ranges = select(&fetch.columns, row_count).map_err(StoreError::FormatLimit)?;
            if predicate_ids == self.column_ids {
                let ranges = ranges.map(|kept| kept.into_ranges(row_count));
                return retain_predicate_fetch(fetch, ranges.as_ref(), row_count, 1, &scan_budget);
            }
            return match ranges {
                // The whole segment was judged, so the mask's rows are the
                // segment's own and place the other columns as they are.
                Some(PrewhereRanges {
                    mask: Some(words),
                    exact,
                    ..
                }) => self.project_after_predicates(
                    &segment,
                    predicate_ids,
                    fetch,
                    None,
                    KeptRows::Mask(&words),
                    exact,
                    1,
                    &scan_budget,
                ),
                Some(PrewhereRanges { ranges, exact, .. }) => self.project_after_predicates(
                    &segment,
                    predicate_ids,
                    fetch,
                    Some(&ranges),
                    KeptRows::Ranges(&ranges),
                    exact,
                    1,
                    &scan_budget,
                ),
                None => self.project_after_predicates(
                    &segment,
                    predicate_ids,
                    fetch,
                    None,
                    KeptRows::Span(0, row_count),
                    false,
                    1,
                    &scan_budget,
                ),
            };
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
        let budget_rows = self
            .row_budget
            .filter(|_| !self.ends_first())
            .map_or(usize::MAX, |rows| {
                usize::try_from(rows).unwrap_or(usize::MAX).max(1)
            });
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
        // A merged row is resolved value by value: a reader that wants a
        // few rows is not handed a chunk of thousands.
        let chunk_rows = chunk_rows.min(budget_rows);
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
            // Every remaining head is at or past the smallest one, so once
            // that is beyond the part's upper bound nothing else qualifies:
            // the streams are left where they are rather than drained.
            if bound_below(&part_hi, &minimum) {
                break;
            }
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
        // The winners are placed straight into the output columns. The
        // fetch below is already column-major and so is the chunk, so
        // turning it into rows and back cost two transposes and one vector
        // allocation per row, for a representation nothing downstream
        // wanted.
        let mut columns = projection
            .iter()
            .map(|_| vec![pintail_types::Value::Null; row_count])
            .collect::<Vec<_>>();
        let mut placed = 0_usize;
        let mut segment_rows = BTreeMap::<usize, Vec<(usize, usize)>>::new();
        for (winner_index, source) in winner_sources.into_iter().enumerate() {
            match source {
                MergedWinnerSource::Segment { .. } if projection.is_empty() => placed += 1,
                MergedWinnerSource::Segment {
                    segment_index,
                    row_index,
                } => segment_rows
                    .entry(segment_index)
                    .or_default()
                    .push((row_index, winner_index)),
                MergedWinnerSource::Memtable(values) => {
                    if values.len() != columns.len() {
                        return Err(StoreError::FormatLimit(
                            "a merged memtable winner has a different width from the projection"
                                .into(),
                        ));
                    }
                    for (column, value) in columns.iter_mut().zip(values) {
                        column[winner_index] = value;
                    }
                    placed += 1;
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
            if fetch.columns.len() != columns.len() {
                return Err(StoreError::FormatLimit(
                    "a merged segment fetch has a different width from the projection".into(),
                ));
            }
            for (column, fetched) in columns.iter_mut().zip(fetch.columns) {
                if fetched.len() != selected.len() {
                    return Err(StoreError::FormatLimit(
                        "projected column length differs from its selected row count".into(),
                    ));
                }
                for ((_, winner_index), value) in selected.iter().zip(fetched) {
                    column[*winner_index] = value;
                }
            }
            scan_budget.release(fetch.reserved_bytes);
            placed += selected.len();
        }
        if placed != row_count {
            return Err(StoreError::FormatLimit(
                "a merged winner was not materialized".into(),
            ));
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
            prefiltered: false,
            columns: columns.into_iter().map(DecodedColumn::Values).collect(),
            row_count,
            stats: ScanStats {
                segments_read: usize::from(first_chunk) * self.segments.len(),
                segments_pruned: usize::from(report_pruned) * self.pruned_segments,
                blocks_decoded,
                ..ScanStats::default()
            },
            retained_bytes,
            column_decode: Vec::new(),
        }))
    }

    /// The physical rows of `segment` this scan's key range selects, for a
    /// decode that reads them in row slices: every row when the range covers
    /// the segment, the located run otherwise, and `None` when the run
    /// cannot be located - a slice decode applies no key bounds, so it must
    /// not be given rows the range excludes.
    fn bounded_rows(
        &self,
        segment: &segment::SegmentMeta,
        memory_limit: usize,
    ) -> Result<Option<(u64, u64)>, StoreError> {
        if self.start <= segment.min_key && self.end >= segment.max_key {
            return Ok(Some((0, segment.row_count)));
        }
        let scan_memory = AtomicUsize::new(0);
        let budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        Ok(self.key_row_span(segment, &budget)?.map(|span| {
            let row = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
            (row(span.rows.start), row(span.rows.end))
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
                        self.range_resumes = true;
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
        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        // A segment read in ranges is still one segment read.
        self.decode_projected_rows(
            segment,
            start..end,
            &projection,
            usize::from(start_row == 0),
            &scan_budget,
        )
    }

    /// Decodes `rows` of a direct segment in every projected column, less
    /// the blocks the scan's value bounds rule out (see
    /// [`Self::value_candidate_ranges`]): their rows fail the scan's filter,
    /// which the executor applies to whatever is returned.
    fn decode_projected_rows(
        &self,
        segment: &segment::SegmentMeta,
        rows: std::ops::Range<usize>,
        projection: &[usize],
        segments_read: usize,
        scan_budget: &segment::ScanMemoryBudget<'_>,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let (fetch, row_count, value_skipped) =
            match self.value_candidate_ranges(segment, rows.start, rows.end)? {
                Some((candidates, skipped)) => (
                    segment::read_projected_column_ranges(
                        &self.snapshot.directory,
                        segment,
                        &self.snapshot.schema,
                        projection,
                        &candidates,
                        scan_budget,
                    )?,
                    candidates
                        .iter()
                        .map(std::iter::ExactSizeIterator::len)
                        .sum::<usize>(),
                    skipped,
                ),
                None => (
                    segment::read_projected_columns(
                        &self.snapshot.directory,
                        segment,
                        &self.snapshot.schema,
                        projection,
                        rows.start,
                        rows.end,
                        scan_budget,
                    )?,
                    rows.len(),
                    0,
                ),
            };
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
            prefiltered: false,
            columns: fetch.columns,
            row_count,
            stats: ScanStats {
                segments_read,
                blocks_decoded: fetch.blocks_decoded,
                blocks_read: fetch.blocks_read,
                blocks_pruned: fetch.blocks_pruned,
                blocks_value_skipped: value_skipped,
                ..ScanStats::default()
            }
            .with_decode(&fetch.column_decode),
            retained_bytes,
            column_decode: fetch.column_decode,
        })
    }

    #[allow(clippy::too_many_lines)]
    /// The rows of a segment the scan's key range covers in part, as one
    /// run: a directly served segment holds each key once, in key order, so
    /// the rows in range are those between the first key at or above the
    /// range's start and the first key past its end. The sparse index names
    /// the blocks that can hold them; only those blocks' key columns, which
    /// the executor named, are read to find the run. `None` without them.
    fn key_row_span(
        &self,
        segment: &segment::SegmentMeta,
        memory: &segment::ScanMemoryBudget<'_>,
    ) -> Result<Option<KeySpan>, StoreError> {
        let Some(key_ids) = self.overlay_key.as_deref() else {
            return Ok(None);
        };
        if !segment.unique_keys
            || key_ids.len() != self.start.parts().len()
            || key_ids.len() != self.end.parts().len()
        {
            return Ok(None);
        }
        let Some(projection) = key_ids
            .iter()
            .map(|id| {
                self.snapshot
                    .schema
                    .columns()
                    .iter()
                    .position(|column| column.id() == *id)
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let row_count = usize::try_from(segment.row_count)
            .map_err(|_| StoreError::FormatLimit("segment row count exceeds usize".into()))?;
        // Each entry is a block's first row and first key. The blocks that
        // can hold the range run from the last one starting at or before its
        // start to the last one starting at or before its end.
        let sparse = segment::read_sparse_index(&self.snapshot.directory, segment)?;
        let block_row = |block: usize| {
            sparse
                .get(block)
                .map_or(Ok(row_count), |(row, _)| usize::try_from(*row))
                .map_err(|_| StoreError::FormatLimit("block row exceeds usize".into()))
        };
        let (first_block, end_block) = if sparse.is_empty() {
            (0, 1)
        } else {
            (
                sparse
                    .partition_point(|(_, key)| *key <= self.start)
                    .saturating_sub(1),
                sparse.partition_point(|(_, key)| *key <= self.end),
            )
        };
        let blocks = sparse.len().max(1);
        if end_block <= first_block {
            return Ok(Some(KeySpan {
                rows: 0..0,
                key_blocks_decoded: 0,
                blocks_read: 0,
                blocks_pruned: blocks,
            }));
        }
        let window = if sparse.is_empty() {
            0..row_count
        } else {
            block_row(first_block)?..block_row(end_block)?
        };
        let fetch = segment::read_projected_columns(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            &projection,
            window.start,
            window.end,
            memory,
        )?;
        let reserved = fetch.reserved_bytes;
        let key_blocks_decoded = fetch.blocks_decoded;
        // Every key column must show its part at every row asked: a column
        // that decoded in a shape with no key part to read has none.
        let key_columns = fetch.columns.iter().collect::<Vec<_>>();
        let readable = window.is_empty()
            || key_columns
                .iter()
                .all(|column| key_part_at(column, 0).is_some());
        let (start, end) = (KeyList::of(&self.start), KeyList::of(&self.end));
        let span = readable.then(|| {
            {
                let compare =
                    |row: usize, bound: &KeyList| compare_row_key(&key_columns, row, bound.get(0));
                // The first window row for which `past` holds; rows are in
                // key order.
                let first_where = |past: &dyn Fn(usize) -> bool| {
                    let (mut low, mut high) = (0, window.len());
                    while low < high {
                        let middle = low + (high - low) / 2;
                        if past(middle) {
                            high = middle;
                        } else {
                            low = middle + 1;
                        }
                    }
                    low
                };
                let first = first_where(&|row| compare(row, &start).is_ge());
                let last = first_where(&|row| compare(row, &end).is_gt());
                KeySpan {
                    rows: window.start + first..window.start + last.max(first),
                    key_blocks_decoded,
                    blocks_read: end_block - first_block,
                    blocks_pruned: blocks - (end_block - first_block),
                }
            }
        });
        drop(fetch);
        memory.release(reserved);
        Ok(span)
    }

    /// One key of a directly served segment, answered from the one block
    /// that can hold it.
    ///
    /// The sparse index names the block; its key columns say whether the
    /// key is there and at which row; the projected columns' blocks give
    /// the row. Each of those is a block already decoded when a recent
    /// lookup landed in it, so a lookup into a block held decodes nothing,
    /// and one into a block not held decodes it once for the lookups after
    /// it - where reading a run of rows decodes the key block to find the
    /// run and then every projected block again, the key's own included.
    ///
    /// `None` leaves the lookup to the general path: without the key's
    /// columns, in a segment that may hold a key twice, before the
    /// segment's first key, or when a block is too large to hold.
    #[allow(clippy::too_many_lines)]
    fn decode_point(
        &self,
        segment: &segment::SegmentMeta,
        memory: &segment::ScanMemoryBudget<'_>,
    ) -> Result<Option<ProjectedColumnChunk>, StoreError> {
        /// Rows in a block worth holding decoded for the next lookup.
        const HELD_BLOCK_ROWS: usize = 1 << 17;
        let Some(key_ids) = self.overlay_key.as_deref() else {
            return Ok(None);
        };
        if !segment.unique_keys || key_ids.len() != self.start.parts().len() {
            return Ok(None);
        }
        let position = |id: &u32| {
            self.snapshot
                .schema
                .columns()
                .iter()
                .position(|column| column.id() == *id)
        };
        let (Some(mut wanted), Some(projection)) = (
            key_ids.iter().map(position).collect::<Option<Vec<_>>>(),
            self.column_ids
                .iter()
                .map(position)
                .collect::<Option<Vec<_>>>(),
        ) else {
            return Ok(None);
        };
        let key_width = wanted.len();
        // Where each projected column's block sits among the blocks read:
        // the key's columns first, then the others, each once.
        let places = projection
            .iter()
            .map(|column| {
                wanted
                    .iter()
                    .position(|held| held == column)
                    .unwrap_or_else(|| {
                        wanted.push(*column);
                        wanted.len() - 1
                    })
            })
            .collect::<Vec<_>>();
        let row_count = usize::try_from(segment.row_count)
            .map_err(|_| StoreError::FormatLimit("segment row count exceeds usize".into()))?;
        let sparse =
            segment::sparse_index_shared(&self.snapshot.directory, segment, &self.snapshot.schema)?;
        // The block that can hold the key is the last one starting at or
        // before it.
        let Some(block) = sparse
            .partition_point(|(_, key)| *key <= self.start)
            .checked_sub(1)
        else {
            return Ok(None);
        };
        let block_row = |block: usize| {
            sparse
                .get(block)
                .map_or(Ok(row_count), |(row, _)| usize::try_from(*row))
                .map_err(|_| StoreError::FormatLimit("block row exceeds usize".into()))
        };
        let window = block_row(block)?..block_row(block + 1)?;
        if window.is_empty() || window.len() > HELD_BLOCK_ROWS {
            return Ok(None);
        }
        let blocks = match segment::point_blocks(
            &self.snapshot.directory,
            segment,
            &self.snapshot.schema,
            &wanted,
            window.clone(),
            memory,
        ) {
            Ok(blocks) => blocks,
            // The general path reads within what the scan may hold.
            Err(StoreError::MemoryLimitExceeded { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        if blocks.columns.len() != wanted.len()
            || blocks
                .columns
                .iter()
                .any(|column| column.len() != window.len())
        {
            return Ok(None);
        }
        let key_columns = blocks.columns[..key_width]
            .iter()
            .map(|column| &**column)
            .collect::<Vec<_>>();
        // Every key column must show its part: a column that decoded in a
        // shape with no key part to read has none.
        if key_columns
            .iter()
            .any(|column| key_part_at(column, 0).is_none())
        {
            return Ok(None);
        }
        let key = KeyList::of(&self.start);
        // The first row at or past the key; rows are in key order.
        let (mut low, mut high) = (0, window.len());
        while low < high {
            let middle = low + (high - low) / 2;
            if compare_row_key(&key_columns, middle, key.get(0)).is_ge() {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        let found = low < window.len() && compare_row_key(&key_columns, low, key.get(0)).is_eq();
        let found_rows = usize::from(found);
        let columns = places
            .iter()
            .map(|place| {
                DecodedColumn::Values(
                    found
                        .then(|| blocks.columns[*place].value_at(low))
                        .flatten()
                        .into_iter()
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        let column_decode = self
            .column_ids
            .iter()
            .zip(&places)
            .map(|(column_id, place)| segment::ColumnDecode {
                column_id: *column_id,
                bytes_decompressed: blocks.bytes_decompressed[*place],
                values_decoded: u64::from(found),
            })
            .collect::<Vec<_>>();
        let retained_bytes = size_of::<ProjectedColumnChunk>()
            .saturating_add(
                columns
                    .capacity()
                    .saturating_mul(size_of::<DecodedColumn>()),
            )
            .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum());
        memory.reserve(retained_bytes)?;
        Ok(Some(ProjectedColumnChunk {
            prefiltered: false,
            columns,
            row_count: found_rows,
            stats: ScanStats {
                segments_read: 1,
                blocks_decoded: blocks.blocks_decoded,
                blocks_read: 1,
                blocks_pruned: sparse.len() - 1,
                ..ScanStats::default()
            }
            .with_decode(&column_decode),
            retained_bytes,
            column_decode,
        }))
    }

    /// A run of a segment's rows decoded as the projected columns.
    fn decode_segment_rows(
        &self,
        segment: &segment::SegmentMeta,
        rows: std::ops::Range<usize>,
        scan_budget: &segment::ScanMemoryBudget<'_>,
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
        self.decode_projected_rows(segment, rows, &projection, 1, scan_budget)
    }

    fn decode_column_chunk(
        &self,
        segment: segment::SegmentMeta,
        memory_limit: usize,
    ) -> Result<ProjectedColumnChunk, StoreError> {
        let covered = self.start <= segment.min_key && self.end >= segment.max_key;
        let scan_memory = AtomicUsize::new(0);
        let span_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        if !covered
            && self.start == self.end
            && let Some(chunk) = self.decode_point(&segment, &span_budget)?
        {
            return Ok(chunk);
        }
        let span = if covered {
            None
        } else {
            self.key_row_span(&segment, &span_budget)?
        };
        if covered || span.is_some() {
            let rows = match &span {
                Some(span) => span.rows.clone(),
                None => {
                    0..usize::try_from(segment.row_count).map_err(|_| {
                        StoreError::FormatLimit("segment row count exceeds usize".into())
                    })?
                }
            };
            let mut chunk = self.decode_segment_rows(&segment, rows, &span_budget)?;
            // A run reports the key blocks its range selected, as the merge
            // path does.
            if let Some(span) = span {
                chunk.stats.blocks_read = span.blocks_read;
                chunk.stats.blocks_pruned = span.blocks_pruned;
                chunk.stats.blocks_decoded += span.key_blocks_decoded;
            }
            return Ok(chunk);
        }
        let mut manifest = self.snapshot.manifest.as_ref().clone();
        manifest.segments = vec![segment];
        let chunk = TableSnapshot {
            instance: self.snapshot.instance,
            memtable: Arc::new(BTreeMap::new()),
            memtable_image: Arc::default(),
            memtable_oldest: None,
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
            prefiltered: false,
            columns,
            row_count,
            stats,
            retained_bytes,
            column_decode: Vec::new(),
        })
    }

    /// Returns the scanned key range.
    /// Names the user columns holding the table's key, in key order; every
    /// part must be an integer, text or binary column, whose stored value
    /// is the key part itself (text and binary compare by their bytes, as
    /// the table's keys do; which rows are one row under a collation was
    /// settled by the source before the key was written). A segment the
    /// memtable overlaps can then be decoded directly, with the superseded
    /// rows masked out by those columns, instead of merged row by row.
    /// Ignored for an absent column or one of another type. Call before the
    /// first chunk is pulled.
    ///
    /// The memtable's live rows are placed among the segment's rows by key,
    /// so the stream stays in key order; a consumer that takes the first
    /// value it meets for a group sees the same row the merge would show.
    pub fn enable_memtable_overlay(&mut self, key_column_ids: &[u32]) {
        let keyed = |id: u32| {
            self.snapshot
                .schema
                .columns()
                .iter()
                .find(|column| column.id() == id)
                .is_some_and(|column| {
                    matches!(
                        column.data_type(),
                        pintail_types::DataType::Utf8
                            | pintail_types::DataType::Binary
                            | pintail_types::DataType::Int8
                            | pintail_types::DataType::Int16
                            | pintail_types::DataType::Int32
                            | pintail_types::DataType::Int64
                            | pintail_types::DataType::UInt8
                            | pintail_types::DataType::UInt16
                            | pintail_types::DataType::UInt32
                            | pintail_types::DataType::UInt64
                    )
                })
        };
        if !key_column_ids.is_empty() && key_column_ids.iter().all(|id| keyed(*id)) {
            self.overlay_key = Some(key_column_ids.to_vec());
        }
    }

    /// The key columns [`Self::enable_memtable_overlay`] accepted, so a
    /// stream rebuilt over a narrower range can be given the same.
    #[must_use]
    pub fn memtable_overlay_key(&self) -> Option<&[u32]> {
        self.overlay_key.as_deref()
    }

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

/// Where a filter-first read finds its kept rows in the segment.
#[derive(Clone, Copy)]
enum KeptRows<'ranges> {
    /// Every row of `start..end`.
    Span(usize, usize),
    /// These ordered, disjoint row ranges.
    Ranges(&'ranges [std::ops::Range<usize>]),
    /// The rows a mask over the whole segment selects, which are also the
    /// predicate fetch's kept rows (the fetch read the whole segment).
    Mask(&'ranges [u64]),
}

/// [`compact_decoded_column`] for the rows a mask selects (bit `r % 64` of
/// `words[r / 64]` for row `r`); `selected` is their count. Packed columns
/// move their values a word at a time, so a run of one row costs a bit
/// scan rather than a range of its own; text and row values take the
/// ranges path.
fn compact_decoded_column_by_mask(column: &mut DecodedColumn, words: &[u64], selected: usize) {
    fn compact<T: Copy>(values: &mut Vec<T>, words: &[u64]) {
        let rows = values.len();
        let mut written = 0;
        for (index, &word) in words.iter().enumerate() {
            let base = index * 64;
            if word == u64::MAX && base + 64 <= rows {
                values.copy_within(base..base + 64, written);
                written += 64;
                continue;
            }
            let mut bits = word;
            while bits != 0 {
                let row = base + bits.trailing_zeros() as usize;
                if row >= rows {
                    break;
                }
                values[written] = values[row];
                written += 1;
                bits &= bits - 1;
            }
        }
        values.truncate(written);
        values.shrink_to_fit();
    }
    let validity = match column {
        DecodedColumn::Int64 { values, validity }
        | DecodedColumn::NativeUnits {
            values, validity, ..
        } => {
            compact(values, words);
            validity
        }
        DecodedColumn::UInt64 { values, validity }
        | DecodedColumn::Float64 {
            bits: values,
            validity,
        } => {
            compact(values, words);
            validity
        }
        DecodedColumn::DictionaryUtf8 {
            codes, validity, ..
        } => {
            compact(codes, words);
            validity
        }
        DecodedColumn::Utf8 { .. } | DecodedColumn::Values(_) => {
            let rows = column.len();
            let ranges = crate::segment::word_runs(words, rows);
            compact_decoded_column(column, &ranges, selected);
            return;
        }
    };
    match validity {
        ColumnValidity::AllValid(count) => *count = selected,
        ColumnValidity::Bytes(bits) => compact(bits, words),
    }
}

/// Keeps only the rows `ranges` names in one decoded column, in place:
/// native values, text arenas and dictionary codes are moved down rather
/// than decoded again. `ranges` must be ordered, disjoint and in bounds, and
/// `selected` their total length.
fn compact_decoded_column(
    column: &mut DecodedColumn,
    ranges: &[std::ops::Range<usize>],
    selected: usize,
) {
    fn compact<T: Copy>(values: &mut Vec<T>, ranges: &[std::ops::Range<usize>]) {
        // A scattered filter keeps runs of a row or two; a memmove call per
        // run cost more than the copy, so short runs move value by value.
        const SHORT_RUN: usize = 16;
        let mut written = 0;
        for range in ranges {
            if range.len() <= SHORT_RUN {
                for row in range.clone() {
                    values[written] = values[row];
                    written += 1;
                }
            } else {
                values.copy_within(range.clone(), written);
                written += range.len();
            }
        }
        values.truncate(written);
        values.shrink_to_fit();
    }
    let validity = match column {
        DecodedColumn::Int64 { values, validity }
        | DecodedColumn::NativeUnits {
            values, validity, ..
        } => {
            compact(values, ranges);
            validity
        }
        DecodedColumn::UInt64 { values, validity }
        | DecodedColumn::Float64 {
            bits: values,
            validity,
        } => {
            compact(values, ranges);
            validity
        }
        DecodedColumn::DictionaryUtf8 {
            codes, validity, ..
        } => {
            compact(codes, ranges);
            validity
        }
        DecodedColumn::Utf8 {
            heap,
            offsets,
            validity,
        } => {
            let mut written_rows = 0;
            let mut written_bytes = 0;
            for range in ranges {
                for row in range.clone() {
                    let start = offsets[row];
                    let end = offsets[row + 1];
                    heap.copy_within(start..end, written_bytes);
                    offsets[written_rows] = written_bytes;
                    written_bytes += end - start;
                    written_rows += 1;
                }
            }
            offsets[written_rows] = written_bytes;
            offsets.truncate(written_rows + 1);
            heap.truncate(written_bytes);
            offsets.shrink_to_fit();
            heap.shrink_to_fit();
            validity
        }
        DecodedColumn::Values(values) => {
            let mut row = 0;
            let mut range_index = 0;
            values.retain(|_| {
                while range_index < ranges.len() && row >= ranges[range_index].end {
                    range_index += 1;
                }
                let keep = ranges
                    .get(range_index)
                    .is_some_and(|range| range.contains(&row));
                row += 1;
                keep
            });
            values.shrink_to_fit();
            return;
        }
    };
    match validity {
        ColumnValidity::AllValid(count) => *count = selected,
        ColumnValidity::Bytes(bits) => compact(bits, ranges),
    }
}

/// Checks a selector's ranges before anything indexes by them: a selector
/// is an API callback.
fn check_selected_ranges(ranges: &[std::ops::Range<usize>], rows: usize) -> Result<(), StoreError> {
    if ranges
        .iter()
        .any(|range| range.start > range.end || range.end > rows)
        || ranges.windows(2).any(|pair| pair[0].end > pair[1].start)
    {
        return Err(StoreError::FormatLimit(
            "invalid predicate row ranges".to_owned(),
        ));
    }
    Ok(())
}

/// Rows of a direct segment that block value skipping leaves to decode
/// (absolute, ascending, disjoint), and how many blocks it skipped.
type ValueCandidates = (Vec<std::ops::Range<usize>>, usize);

/// Maps `ranges` over the rows of `candidates` laid end to end (as a fetch
/// of those row ranges returns them) back to the segment positions they
/// name. `ranges` must be checked against the candidates' row total.
fn candidate_positions(
    ranges: &[std::ops::Range<usize>],
    candidates: &[std::ops::Range<usize>],
) -> Vec<std::ops::Range<usize>> {
    let mut positions: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    let mut candidate = 0_usize;
    // Packed offset of `candidates[candidate].start`.
    let mut base = 0_usize;
    for range in ranges {
        let mut at = range.start;
        while at < range.end {
            while base + candidates[candidate].len() <= at {
                base += candidates[candidate].len();
                candidate += 1;
            }
            let span = &candidates[candidate];
            let until = range.end.min(base + span.len());
            let mapped = span.start + (at - base)..span.start + (until - base);
            match positions.last_mut() {
                Some(last) if last.end == mapped.start => last.end = mapped.end,
                _ => positions.push(mapped),
            }
            at = until;
        }
    }
    positions
}

/// The predicate projection already decoded every output column. Compact
/// those buffers before the prefetch round retains them, preserving native
/// values, text arenas and dictionary codes instead of decoding them again.
fn retain_predicate_fetch(
    mut fetch: segment::ProjectedColumnFetch,
    selection: Option<&PrewhereRanges>,
    rows: usize,
    segments_read: usize,
    memory: &segment::ScanMemoryBudget<'_>,
) -> Result<ProjectedColumnChunk, StoreError> {
    let ranges = selection.map(|selection| selection.ranges.as_slice());
    let selected = ranges.map_or(rows, |ranges| {
        ranges.iter().map(std::iter::ExactSizeIterator::len).sum()
    });
    if let Some(ranges) = ranges {
        check_selected_ranges(ranges, rows)?;
        for column in &mut fetch.columns {
            compact_decoded_column(column, ranges, selected);
        }
    }
    let retained_bytes = size_of::<ProjectedColumnChunk>()
        + fetch.columns.capacity() * size_of::<DecodedColumn>()
        + fetch
            .columns
            .iter()
            .map(DecodedColumn::retained_bytes)
            .sum::<usize>();
    memory.release(fetch.reserved_bytes);
    memory.reserve(retained_bytes)?;
    Ok(ProjectedColumnChunk {
        prefiltered: selection.is_some_and(|selection| selection.exact),
        columns: fetch.columns,
        row_count: selected,
        stats: ScanStats {
            segments_read,
            blocks_read: fetch.blocks_read,
            blocks_pruned: fetch.blocks_pruned,
            blocks_decoded: fetch.blocks_decoded,
            ..ScanStats::default()
        }
        .with_decode(&fetch.column_decode),
        retained_bytes,
        column_decode: fetch.column_decode,
    })
}

#[cfg(test)]
mod overlay_primitive_tests {
    use super::{
        Cell, ColumnValidity, DecodedColumn, OverlayEdits, Placement, SpanRows, compare_row_key,
        empty_packed_column, interleave_values, overlay_positions, searched_overlay_positions,
        subtract_positions, walked_overlay_positions,
    };
    use pintail_types::{KeyPart, PrimaryKey, StoredRow, Value};

    fn packed(values: &[u64]) -> DecodedColumn {
        DecodedColumn::UInt64 {
            values: values.to_vec(),
            validity: ColumnValidity::AllValid(values.len()),
        }
    }

    fn plain(values: &[u64]) -> DecodedColumn {
        DecodedColumn::Values(values.iter().map(|value| Value::UInt64(*value)).collect())
    }

    fn row(id: u64) -> StoredRow {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            vec![Value::UInt64(id)],
            2,
            false,
        )
    }

    /// The edits by the walk and by the lookups, which must agree.
    fn edits(
        key_columns: &[&DecodedColumn],
        row_count: usize,
        kept: Option<&[std::ops::Range<usize>]>,
        memtable: &[(Vec<i128>, Option<&StoredRow>)],
    ) -> OverlayEdits {
        let span = SpanRows::from_rows(memtable);
        let walked = overlay_positions(key_columns, row_count, kept, &span);
        assert_eq!(
            walked,
            searched_overlay_positions(key_columns, row_count, kept, &span)
        );
        walked
    }

    #[test]
    fn the_walk_replaces_deletes_and_places_inserts_in_key_order() {
        let live = [row(4), row(9), row(25), row(45)];
        // Segment keys 2..16 step 2; memtable: 4 updated, 8 deleted, 9
        // inserted between 8 and 10, 25 and 45 inserted past the end.
        let memtable: Vec<(Vec<i128>, Option<&StoredRow>)> = vec![
            (vec![4], Some(&live[0])),
            (vec![8], None),
            (vec![9], Some(&live[1])),
            (vec![25], Some(&live[2])),
            (vec![45], Some(&live[3])),
        ];
        let values = [2_u64, 4, 6, 8, 10, 12, 14, 16];
        for column in [packed(&values), plain(&values)] {
            let found = edits(&[&column], 8, None, &memtable);
            assert_eq!(found.deletes, vec![3], "key 8 is deleted");
            // Output: 2, 4*, 6, 9*, 10, 12, 14, 16, 25*, 45*.
            assert_eq!(
                found.placements,
                vec![
                    Placement::Replace(1),
                    Placement::Insert(3),
                    Placement::Insert(8),
                    Placement::Insert(9),
                ]
            );
        }
        // With only rows 2..6 (keys 6, 8, 10, 12) kept by a predicate, the
        // chunk holds those four. The memtable's live rows are placed
        // whatever the predicate said about the segment rows they replace
        // (the filter runs again above): the output is 4*, 6, 9*, 10, 12,
        // 25*, 45*.
        let kept: Vec<std::ops::Range<usize>> = std::iter::once(2..6_usize).collect();
        let found = edits(&[&packed(&values)], 8, Some(&kept), &memtable);
        assert_eq!(found.deletes, vec![1]);
        assert_eq!(
            found.placements,
            vec![
                Placement::Insert(0),
                Placement::Insert(2),
                Placement::Insert(5),
                Placement::Insert(6),
            ]
        );
        // Every row superseded, nothing live: no placements.
        let all: Vec<(Vec<i128>, Option<&StoredRow>)> = values
            .iter()
            .map(|value| (vec![i128::from(*value)], None))
            .collect();
        let found = edits(&[&packed(&values)], 8, None, &all);
        assert_eq!(found.deletes, (0..8).collect::<Vec<_>>());
        assert!(found.placements.is_empty());
    }

    /// A text key, alone and after an integer part, is placed by its bytes:
    /// upper case before lower, a key before the keys it is a prefix of, a
    /// trailing space significant, in a packed text column and in plain
    /// values alike.
    #[test]
    fn a_text_key_is_placed_by_its_bytes() {
        let texts = ["Ab", "a", "ab", "ab ", "b", "\u{e9}"];
        let plain_text = DecodedColumn::Values(
            texts
                .iter()
                .map(|text| Value::Utf8((*text).to_owned()))
                .collect(),
        );
        let packed_text = empty_packed_column(pintail_types::DataType::Utf8).interleave_cells(
            &texts
                .iter()
                .enumerate()
                .map(|(at, text)| (at, Cell::Text(text.as_bytes())))
                .collect::<Vec<_>>(),
        );
        let live = [row(1), row(2), row(3), row(4)];
        let key = |text: &str| PrimaryKey::new(vec![KeyPart::Utf8(text.to_owned())]).expect("key");
        // "A" before everything, "a" updated, "aB" between "a" and "ab",
        // "ab " deleted, "abc" between "ab " and "b", "z" between "b" and
        // the two-byte key.
        let memtable = vec![
            (key("A"), Some(&live[0])),
            (key("a"), Some(&live[1])),
            (key("aB"), Some(&live[2])),
            (key("ab "), None),
            (key("abc"), Some(&live[3])),
            (key("z"), None),
        ];
        for column in [&plain_text, &packed_text] {
            let span = SpanRows::from_keys(&memtable);
            let walked = walked_overlay_positions(texts.len(), None, &span, |row, next| {
                compare_row_key(&[column], row, span.key(next))
            });
            assert_eq!(
                walked,
                searched_overlay_positions(&[column], texts.len(), None, &span)
            );
            assert_eq!(walked.deletes, vec![3]);
            // Output: A*, Ab, a*, aB*, ab, abc*, b, e-acute.
            assert_eq!(
                walked.placements,
                vec![
                    Placement::Insert(0),
                    Placement::Replace(1),
                    Placement::Insert(3),
                    Placement::Insert(5),
                ]
            );
        }
        // (shelf, code): the text decides within a shelf.
        let shelves = DecodedColumn::Int64 {
            values: vec![1, 1, 1, 2, 2, 2],
            validity: ColumnValidity::AllValid(6),
        };
        let codes = DecodedColumn::Values(
            ["a", "b", "c", "a", "b", "c"]
                .iter()
                .map(|text| Value::Utf8((*text).to_owned()))
                .collect(),
        );
        let key = |shelf: i64, text: &str| {
            PrimaryKey::new(vec![KeyPart::Int64(shelf), KeyPart::Utf8(text.to_owned())])
                .expect("key")
        };
        let memtable = vec![
            (key(1, "b"), None),
            (key(1, "bb"), Some(&live[0])),
            (key(2, "a"), Some(&live[1])),
            (key(2, "d"), Some(&live[2])),
        ];
        let span = SpanRows::from_keys(&memtable);
        let columns = [&shelves, &codes];
        let walked = overlay_positions(&columns, 6, None, &span);
        assert_eq!(walked, searched_overlay_positions(&columns, 6, None, &span));
        assert_eq!(walked.deletes, vec![1]);
        assert_eq!(
            walked.placements,
            vec![
                Placement::Insert(1),
                Placement::Replace(3),
                Placement::Insert(6),
            ]
        );
    }

    #[test]
    fn a_composite_key_compares_part_by_part() {
        let first = DecodedColumn::Int64 {
            values: vec![1, 1, 2, 2, 3],
            validity: ColumnValidity::AllValid(5),
        };
        let second = DecodedColumn::Int64 {
            values: vec![5, 9, 1, 7, 0],
            validity: ColumnValidity::AllValid(5),
        };
        let live = [row(1), row(2), row(3)];
        // (1,9) updated; (2,3) inserted between (2,1) and (2,7); (4,0)
        // inserted past the end; (3,0) deleted.
        let memtable: Vec<(Vec<i128>, Option<&StoredRow>)> = vec![
            (vec![1, 9], Some(&live[0])),
            (vec![2, 3], Some(&live[1])),
            (vec![3, 0], None),
            (vec![4, 0], Some(&live[2])),
        ];
        let found = edits(&[&first, &second], 5, None, &memtable);
        assert_eq!(found.deletes, vec![4]);
        // Output: (1,5), (1,9)*, (2,1), (2,3)*, (2,7), (4,0)*.
        assert_eq!(
            found.placements,
            vec![
                Placement::Replace(1),
                Placement::Insert(3),
                Placement::Insert(5),
            ]
        );
        // Signed keys past the unsigned range and large unsigned keys stay
        // distinct.
        let signed = DecodedColumn::Int64 {
            values: vec![-3, -1, 0],
            validity: ColumnValidity::AllValid(3),
        };
        let memtable: Vec<(Vec<i128>, Option<&StoredRow>)> =
            vec![(vec![-1], None), (vec![i128::from(u64::MAX)], None)];
        assert_eq!(edits(&[&signed], 3, None, &memtable).deletes, vec![1]);
    }

    #[test]
    fn replaced_rows_keep_a_column_packed_and_its_length() {
        let (five, null, text) = (Value::Int64(5), Value::Null, Value::Utf8("x".to_owned()));
        let column = DecodedColumn::Int64 {
            values: vec![10, 20, 30],
            validity: ColumnValidity::AllValid(3),
        };
        match column.clone().replace_rows(&[(1, &five)]) {
            DecodedColumn::Int64 { values, validity } => {
                assert_eq!(values, vec![10, 5, 30]);
                assert_eq!(validity, ColumnValidity::AllValid(3));
            }
            other => panic!("not packed: {other:?}"),
        }
        match column.clone().replace_rows(&[(0, &null), (2, &five)]) {
            DecodedColumn::Int64 { values, validity } => {
                assert_eq!(values, vec![0, 20, 5]);
                assert_eq!(validity, ColumnValidity::Bytes(vec![false, true, true]));
            }
            other => panic!("not packed: {other:?}"),
        }
        assert_eq!(
            column.replace_rows(&[(2, &text)]).into_values(),
            vec![Value::Int64(10), Value::Int64(20), text.clone()]
        );
        let texts = DecodedColumn::Utf8 {
            heap: b"abbccc".to_vec(),
            offsets: vec![0, 1, 3, 6],
            validity: ColumnValidity::AllValid(3),
        };
        let longer = Value::Utf8("longer".to_owned());
        assert_eq!(
            texts
                .replace_rows(&[(1, &longer), (2, &null)])
                .into_values(),
            vec![Value::Utf8("a".to_owned()), longer.clone(), Value::Null]
        );
        let coded = DecodedColumn::DictionaryUtf8 {
            dict_heap: b"ab".to_vec(),
            dict_offsets: vec![0, 1, 2],
            codes: vec![0, 1, 0],
            validity: ColumnValidity::AllValid(3),
        };
        let fresh = Value::Utf8("c".to_owned());
        let known = Value::Utf8("b".to_owned());
        assert_eq!(
            coded
                .replace_rows(&[(0, &known), (2, &fresh)])
                .into_values(),
            vec![known.clone(), known.clone(), fresh.clone()]
        );
    }

    #[test]
    fn subtracting_positions_cuts_every_range_exactly() {
        let all: Vec<std::ops::Range<usize>> = std::iter::once(0..10_usize).collect();
        assert_eq!(subtract_positions(all.clone(), &[]), vec![0..10]);
        assert_eq!(subtract_positions(all.clone(), &[0]), vec![1..10]);
        assert_eq!(subtract_positions(all.clone(), &[9]), vec![0..9]);
        assert_eq!(
            subtract_positions(all.clone(), &[3, 4, 5]),
            vec![0..3, 6..10]
        );
        assert!(subtract_positions(all, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).is_empty());
        let kept = vec![2..5_usize, 8..12];
        assert_eq!(
            subtract_positions(kept, &[0, 3, 6, 8, 11, 20]),
            vec![2..3, 4..5, 9..11]
        );
    }

    #[test]
    fn interleave_keeps_integer_columns_packed_and_falls_back_for_others() {
        let column = DecodedColumn::Int64 {
            values: vec![10, 30],
            validity: ColumnValidity::AllValid(2),
        };
        let (a, b, c) = (Value::Int64(5), Value::Int64(20), Value::Int64(40));
        let merged = column.interleave(&[(0, &a), (2, &b), (4, &c)]);
        match merged {
            DecodedColumn::Int64 { values, validity } => {
                assert_eq!(values, vec![5, 10, 20, 30, 40]);
                assert!(matches!(validity, ColumnValidity::AllValid(5)));
            }
            other => panic!("expected a packed column, got {other:?}"),
        }
        let column = DecodedColumn::Int64 {
            values: vec![1, 2],
            validity: ColumnValidity::Bytes(vec![true, false]),
        };
        let merged = column.interleave(&[(1, &Value::Null)]);
        match merged {
            DecodedColumn::Int64 { values, validity } => {
                assert_eq!(values, vec![1, 0, 2]);
                assert_eq!(
                    (0..3).map(|row| validity.is_valid(row)).collect::<Vec<_>>(),
                    vec![true, false, false]
                );
            }
            other => panic!("expected a packed column, got {other:?}"),
        }
        let column = DecodedColumn::Int64 {
            values: vec![1, 2],
            validity: ColumnValidity::AllValid(2),
        };
        let text = Value::Utf8("x".to_owned());
        let merged = column.interleave(&[(2, &text)]);
        assert_eq!(
            merged.into_values(),
            vec![
                Value::Int64(1),
                Value::Int64(2),
                Value::Utf8("x".to_owned())
            ]
        );
        let column = DecodedColumn::Values(vec![Value::UInt64(2), Value::UInt64(4)]);
        let (a, b, c) = (Value::UInt64(1), Value::UInt64(3), Value::UInt64(5));
        assert_eq!(
            column
                .interleave(&[(0, &a), (2, &b), (4, &c)])
                .into_values(),
            [1_u64, 2, 3, 4, 5].map(Value::UInt64).to_vec()
        );
    }

    /// Text, dictionary text, native units and floats keep their packed
    /// shape when memtable rows interleave into them, and read back exactly
    /// as the plain-value interleave of the same rows does.
    #[test]
    fn packed_columns_stay_packed_under_interleave() {
        let text = |value: &str| Value::Utf8(value.to_owned());
        let check = |column: DecodedColumn, inserts: &[(usize, &Value)], packed: bool| {
            let cells = inserts
                .iter()
                .map(|(at, value)| (*at, super::Cell::Value(value)))
                .collect::<Vec<_>>();
            let expected = interleave_values(column.clone().into_values(), &cells);
            let merged = column.interleave(inserts);
            assert_eq!(!matches!(merged, DecodedColumn::Values(_)), packed);
            assert_eq!(merged.into_values(), expected);
        };
        let (north, east, fresh, null) = (text("north"), text("east"), text("fresh"), Value::Null);
        let dictionary = || DecodedColumn::DictionaryUtf8 {
            dict_heap: b"northsouth".to_vec(),
            dict_offsets: vec![0, 5, 10],
            codes: vec![0, 1, 0, 1],
            validity: ColumnValidity::Bytes(vec![true, true, false, true]),
        };
        let inserts = [(0, &east), (2, &north), (3, &null), (6, &fresh), (7, &east)];
        check(dictionary(), &inserts, true);
        let arena = || DecodedColumn::Utf8 {
            heap: b"abcde".to_vec(),
            offsets: vec![0, 2, 2, 5],
            validity: ColumnValidity::Bytes(vec![true, false, true]),
        };
        check(arena(), &[(1, &east), (4, &null), (5, &fresh)], true);
        check(arena(), &[(1, &Value::Int64(3))], false);
        let units = || DecodedColumn::NativeUnits {
            units: crate::segment::NativeUnits::Decimal { scale: 2 },
            values: vec![150, -7],
            validity: ColumnValidity::AllValid(2),
        };
        let (decimal, loose) = (text("12.30"), text("12.3"));
        check(units(), &[(0, &decimal), (3, &null)], true);
        // Text the units would not regenerate stays text.
        check(units(), &[(1, &loose)], false);
        let floats = DecodedColumn::Float64 {
            bits: vec![1.5_f64.to_bits()],
            validity: ColumnValidity::AllValid(1),
        };
        let half = Value::Float64(pintail_types::Float64::new(0.5));
        check(floats, &[(0, &half), (2, &null)], true);
    }
}

#[cfg(test)]
mod candidate_position_tests {
    use super::candidate_positions;

    #[test]
    fn packed_ranges_map_back_across_candidate_gaps() {
        // Candidates 10..20 and 40..45 pack as rows 0..10 and 10..15.
        let candidates = [10..20, 40..45];
        assert_eq!(
            candidate_positions(&[0..3, 8..12, 14..15], &candidates),
            vec![10..13, 18..20, 40..42, 44..45]
        );
        // Candidates that touch map to one range.
        assert_eq!(
            candidate_positions(std::slice::from_ref(&(0..15)), &[0..5, 5..15]),
            std::iter::once(0..15).collect::<Vec<_>>()
        );
        assert_eq!(candidate_positions(&[], &candidates), Vec::new());
        assert_eq!(
            candidate_positions(std::slice::from_ref(&(4..4)), &candidates),
            Vec::new()
        );
    }
}

#[cfg(test)]
mod mask_compaction_tests {
    use super::{
        ColumnValidity, DecodedColumn, compact_decoded_column, compact_decoded_column_by_mask,
    };
    use pintail_types::Value;

    fn columns(rows: usize) -> Vec<DecodedColumn> {
        let values: Vec<i64> = (0..rows)
            .map(|row| i64::try_from(row).expect("small") * 3 - 7)
            .collect();
        let valid: Vec<bool> = (0..rows).map(|row| row % 5 != 2).collect();
        let mut heap = Vec::new();
        let mut offsets = vec![0];
        for row in 0..rows {
            heap.extend_from_slice(format!("v{row}").as_bytes());
            offsets.push(heap.len());
        }
        vec![
            DecodedColumn::Int64 {
                values: values.clone(),
                validity: ColumnValidity::AllValid(rows),
            },
            DecodedColumn::Int64 {
                values,
                validity: ColumnValidity::Bytes(valid),
            },
            DecodedColumn::Utf8 {
                heap,
                offsets,
                validity: ColumnValidity::AllValid(rows),
            },
            DecodedColumn::Values((0..rows).map(|row| Value::UInt64(row as u64)).collect()),
        ]
    }

    #[test]
    fn a_mask_keeps_the_same_rows_as_its_ranges() {
        for rows in [1_usize, 63, 64, 65, 200, 1_000] {
            let patterns: [&dyn Fn(usize) -> bool; 4] = [
                &|row| row % 3 == 0,
                &|_| true,
                &|row| row % 64 < 40,
                &|row| row + 1 == rows,
            ];
            for keep in patterns {
                let mut words = vec![0_u64; rows.div_ceil(64)];
                let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
                for row in (0..rows).filter(|row| keep(*row)) {
                    words[row / 64] |= 1 << (row % 64);
                    match ranges.last_mut() {
                        Some(last) if last.end == row => last.end = row + 1,
                        _ => ranges.push(row..row + 1),
                    }
                }
                let selected = ranges.iter().map(std::iter::ExactSizeIterator::len).sum();
                for (mut by_mask, mut by_ranges) in columns(rows).into_iter().zip(columns(rows)) {
                    compact_decoded_column_by_mask(&mut by_mask, &words, selected);
                    compact_decoded_column(&mut by_ranges, &ranges, selected);
                    assert_eq!(
                        by_mask.into_values(),
                        by_ranges.into_values(),
                        "rows {rows}"
                    );
                }
            }
        }
    }
}
