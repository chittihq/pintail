//! The rows over a layered cluster's bases, read where they are stored.
//!
//! The newer segments of a cluster are never turned into rows. Their keys,
//! versions and delete flags are resolved once into a [`LayerIndex`] - one
//! entry per key, naming the segment row that holds its newest version -
//! and a slice of a base reads the entries of its key span as a range of
//! arrays. The values of the rows a slice needs are then read from the
//! newer segments column by column, as packed as a base decodes.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::AtomicUsize},
};

use pintail_types::{KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rayon::prelude::*;

use super::projected_scan_pool;
use super::scan::DecodedColumn;
use crate::{StoreError, segment};

/// One value an overlay writes into a decoded chunk: a memtable row's, or
/// one read packed from a newer segment.
#[derive(Clone, Copy, Debug)]
pub(super) enum Cell<'a> {
    Value(&'a Value),
    Null,
    Int(i64),
    UInt(u64),
    /// The bits of a float.
    Bits(u64),
    /// Native units of the column's type.
    Units(segment::NativeUnits, i64),
    Text(&'a [u8]),
}

impl Cell<'_> {
    /// The value a row-wise read of the same cell yields.
    pub(super) fn to_value(self) -> Value {
        match self {
            Self::Value(value) => value.clone(),
            Self::Null => Value::Null,
            Self::Int(value) => Value::Int64(value),
            Self::UInt(value) => Value::UInt64(value),
            Self::Bits(bits) => Value::Float64(pintail_types::Float64::new(f64::from_bits(bits))),
            Self::Units(units, value) => {
                Value::Utf8(units.format(value).expect("stored native units round-trip"))
            }
            Self::Text(bytes) => Value::Utf8(
                String::from_utf8(bytes.to_vec())
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
            ),
        }
    }
}

impl DecodedColumn {
    /// The cell at `row`, borrowed from the column.
    pub(super) fn cell(&self, row: usize) -> Cell<'_> {
        match self {
            Self::Values(values) => values.get(row).map_or(Cell::Null, Cell::Value),
            Self::Int64 { values, validity } => {
                if validity.is_valid(row) {
                    Cell::Int(values[row])
                } else {
                    Cell::Null
                }
            }
            Self::UInt64 { values, validity } => {
                if validity.is_valid(row) {
                    Cell::UInt(values[row])
                } else {
                    Cell::Null
                }
            }
            Self::Float64 { bits, validity } => {
                if validity.is_valid(row) {
                    Cell::Bits(bits[row])
                } else {
                    Cell::Null
                }
            }
            Self::NativeUnits {
                units,
                values,
                validity,
            } => {
                if validity.is_valid(row) {
                    Cell::Units(*units, values[row])
                } else {
                    Cell::Null
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
                    Cell::Text(&dict_heap[dict_offsets[code]..dict_offsets[code + 1]])
                } else {
                    Cell::Null
                }
            }
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => {
                if validity.is_valid(row) {
                    Cell::Text(&heap[offsets[row]..offsets[row + 1]])
                } else {
                    Cell::Null
                }
            }
        }
    }
}

/// Where one row of a slice's key span comes from.
#[derive(Clone, Copy, Debug)]
pub(super) enum SpanRow<'a> {
    /// A row that only supersedes: a delete, or a live row the scan does
    /// not want.
    Mask,
    /// A live memtable row.
    Row(&'a StoredRow),
    /// A live row of a layer's newer segment `segment`, at physical row
    /// `row` of it.
    Layer { segment: u32, row: u32 },
}

/// Appends the integer parts of `key` to `out`; `false` when a part is not
/// an integer.
pub(super) fn push_key_parts(key: &PrimaryKey, out: &mut Vec<i128>) -> bool {
    for part in key.parts() {
        out.push(match part {
            KeyPart::Int64(value) => i128::from(*value),
            KeyPart::UInt64(value) => i128::from(*value),
            _ => return false,
        });
    }
    true
}

/// What identifies a segment's bytes within one table's lineage.
type SegmentIdentity = (String, u64, u64, u64);

fn identity(segments: &[segment::SegmentMeta]) -> Vec<SegmentIdentity> {
    segments
        .iter()
        .map(|meta| {
            (
                meta.file_name.clone(),
                meta.row_count,
                meta.min_version,
                meta.max_version,
            )
        })
        .collect()
}

/// The newer segments of a layered cluster resolved to one entry per key,
/// in key order: the key's integer parts, the winning version, whether it
/// is a delete, and the segment row that holds it. No value is read.
pub(crate) struct LayerIndex {
    segments: Vec<SegmentIdentity>,
    parts: usize,
    keys: Vec<i128>,
    versions: Vec<u64>,
    sources: Vec<u32>,
    rows: Vec<u32>,
    deleted: Vec<bool>,
}

/// One segment's headers, in key order.
#[derive(Default)]
struct Headers {
    keys: Vec<i128>,
    versions: Vec<u64>,
    sources: Vec<u32>,
    rows: Vec<u32>,
    deleted: Vec<bool>,
}

impl Headers {
    fn len(&self) -> usize {
        self.versions.len()
    }

    fn key(&self, parts: usize, index: usize) -> &[i128] {
        &self.keys[index * parts..(index + 1) * parts]
    }

    fn push_from(&mut self, other: &Self, parts: usize, index: usize) {
        self.keys.extend_from_slice(other.key(parts, index));
        self.versions.push(other.versions[index]);
        self.sources.push(other.sources[index]);
        self.rows.push(other.rows[index]);
        self.deleted.push(other.deleted[index]);
    }

    /// Replaces the last entry with entry `index` of `other`.
    fn replace_last(&mut self, other: &Self, index: usize) {
        let last = self.len() - 1;
        self.versions[last] = other.versions[index];
        self.sources[last] = other.sources[index];
        self.rows[last] = other.rows[index];
        self.deleted[last] = other.deleted[index];
    }
}

/// Bytes a [`LayerIndex`] holds per newer row of a table with a one-part
/// key: the key, the version, the segment, the row and the delete flag.
pub(super) const LAYER_INDEX_ROW_BYTES: u64 = 33;

/// Bytes the key index of one cluster's newer segments may hold, from
/// `PINTAIL_LAYER_INDEX_MB` (512 by default; 0 switches layering off). A
/// cluster whose newer rows would pass it is read by the row-wise merge,
/// which holds nothing.
pub(super) fn layer_index_budget() -> u64 {
    static BUDGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("PINTAIL_LAYER_INDEX_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(512)
            .saturating_mul(1 << 20)
    })
}

/// Blocks of a segment whose headers one task reads.
const PIECE_BLOCKS: usize = 8;

impl LayerIndex {
    pub(super) fn len(&self) -> usize {
        self.versions.len()
    }

    pub(super) fn key(&self, index: usize) -> &[i128] {
        &self.keys[index * self.parts..(index + 1) * self.parts]
    }

    pub(super) fn version(&self, index: usize) -> u64 {
        self.versions[index]
    }

    /// The row at `index`: a mask for a delete.
    pub(super) fn row(&self, index: usize) -> SpanRow<'static> {
        if self.deleted[index] {
            SpanRow::Mask
        } else {
            SpanRow::Layer {
                segment: self.sources[index],
                row: self.rows[index],
            }
        }
    }

    /// Bytes this index holds.
    pub(crate) fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.keys.capacity() * size_of::<i128>()
            + self.versions.capacity() * size_of::<u64>()
            + (self.sources.capacity() + self.rows.capacity()) * size_of::<u32>()
            + self.deleted.capacity()
    }

    /// The key at `index` as a key of the table, its parts typed as
    /// `template`'s are.
    pub(super) fn primary_key(
        &self,
        index: usize,
        template: &PrimaryKey,
    ) -> Result<PrimaryKey, StoreError> {
        let unfit = || StoreError::FormatLimit("a layered key does not fit its key type".into());
        let parts = self
            .key(index)
            .iter()
            .zip(template.parts())
            .map(|(value, part)| match part {
                KeyPart::Int64(_) => i64::try_from(*value)
                    .map(KeyPart::Int64)
                    .map_err(|_| unfit()),
                KeyPart::UInt64(_) => u64::try_from(*value)
                    .map(KeyPart::UInt64)
                    .map_err(|_| unfit()),
                _ => Err(unfit()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if parts.len() != self.parts {
            return Err(unfit());
        }
        PrimaryKey::new(parts).map_err(|_| unfit())
    }

    /// The entries inside a key range, as a range of this index.
    pub(super) fn range(
        &self,
        lo: &std::ops::Bound<PrimaryKey>,
        hi: &std::ops::Bound<PrimaryKey>,
    ) -> Result<std::ops::Range<usize>, StoreError> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let parts_of = |key: &PrimaryKey| {
            let mut parts = Vec::with_capacity(key.parts().len());
            if push_key_parts(key, &mut parts) {
                Ok(parts)
            } else {
                Err(StoreError::FormatLimit(
                    "a layered scan needs integer key bounds".into(),
                ))
            }
        };
        // The first entry at or above a key, and the first above it.
        let at_or_above =
            |parts: &[i128]| partition_point(self.len(), |index| self.key(index) < parts);
        let above = |parts: &[i128]| partition_point(self.len(), |index| self.key(index) <= parts);
        let start = match lo {
            Included(key) => at_or_above(&parts_of(key)?),
            Excluded(key) => above(&parts_of(key)?),
            Unbounded => 0,
        };
        let end = match hi {
            Included(key) => above(&parts_of(key)?),
            Excluded(key) => at_or_above(&parts_of(key)?),
            Unbounded => self.len(),
        };
        Ok(start..end.max(start))
    }

    /// Reads the headers of `segments` (oldest first) and resolves them to
    /// one entry per key: the greater version wins, and of equal versions
    /// the one met first, in the older segment. `None` when a key part is
    /// not an integer or a segment holds more rows than an entry can name.
    ///
    /// The headers are read a few blocks per task, so a table whose newer
    /// rows sit in one large segment still resolves as wide as the machine.
    pub(super) fn build(
        directory: &std::path::Path,
        schema: &TableSchema,
        segments: &[segment::SegmentMeta],
    ) -> Result<Option<Self>, StoreError> {
        let Some(parts) = segments.first().map(|meta| meta.min_key.parts().len()) else {
            return Ok(None);
        };
        if parts == 0 {
            return Ok(None);
        }
        // (segment, first key, the key the piece stops before).
        let mut pieces: Vec<(usize, PrimaryKey, Option<PrimaryKey>)> = Vec::new();
        for (index, meta) in segments.iter().enumerate() {
            if u32::try_from(meta.row_count).is_err() || u32::try_from(index).is_err() {
                return Ok(None);
            }
            let sparse = segment::read_sparse_index(directory, meta)?;
            let mut starts = sparse
                .iter()
                .step_by(PIECE_BLOCKS)
                .skip(1)
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>();
            starts.insert(0, meta.min_key.clone());
            starts.dedup();
            for (piece, start) in starts.iter().enumerate() {
                pieces.push((index, start.clone(), starts.get(piece + 1).cloned()));
            }
        }
        let read = |(index, start, stop): &(usize, PrimaryKey, Option<PrimaryKey>)| {
            let meta = &segments[*index];
            let memory = AtomicUsize::new(0);
            let budget = segment::ScanMemoryBudget::new(&memory, usize::MAX);
            let end = stop.as_ref().unwrap_or(&meta.max_key);
            let scan =
                segment::read_row_headers_range(directory, meta, schema, start, end, &budget)?;
            let source = u32::try_from(*index).unwrap_or(u32::MAX);
            let mut headers = Headers::default();
            for row in scan.rows {
                if stop.as_ref().is_some_and(|stop| row.key >= *stop) {
                    break;
                }
                if row.key.parts().len() != parts || !push_key_parts(&row.key, &mut headers.keys) {
                    return Ok(None);
                }
                let Ok(physical) = u32::try_from(row.physical_index) else {
                    return Ok(None);
                };
                headers.versions.push(row.version);
                headers.sources.push(source);
                headers.rows.push(physical);
                headers.deleted.push(row.deleted);
            }
            Ok(Some(headers))
        };
        let read: Result<Vec<Option<Headers>>, StoreError> =
            projected_scan_pool()?.install(|| pieces.par_iter().map(read).collect());
        let mut resolved = Headers::default();
        let mut next_piece = read?.into_iter();
        for index in 0..segments.len() {
            // This segment's pieces, in key order, as one run with a key's
            // versions collapsed to the newest.
            let mut run = Headers::default();
            for _ in pieces.iter().filter(|(segment, _, _)| *segment == index) {
                let Some(Some(piece)) = next_piece.next() else {
                    return Ok(None);
                };
                for row in 0..piece.len() {
                    let last = run.len().checked_sub(1);
                    if last.is_some_and(|last| run.key(parts, last) == piece.key(parts, row)) {
                        if piece.versions[row] > run.versions[run.len() - 1] {
                            run.replace_last(&piece, row);
                        }
                    } else {
                        run.push_from(&piece, parts, row);
                    }
                }
            }
            resolved = if resolved.len() == 0 {
                run
            } else {
                merge_headers(&resolved, &run, parts)
            };
        }
        Ok(Some(Self {
            segments: identity(segments),
            parts,
            keys: resolved.keys,
            versions: resolved.versions,
            sources: resolved.sources,
            rows: resolved.rows,
            deleted: resolved.deleted,
        }))
    }
}

/// `older` and `newer`, each one entry per key in key order, as one: of a
/// key both hold, the greater version, and `older`'s when they are equal.
fn merge_headers(older: &Headers, newer: &Headers, parts: usize) -> Headers {
    let mut out = Headers::default();
    out.keys.reserve(older.keys.len() + newer.keys.len());
    let (mut left, mut right) = (0, 0);
    while left < older.len() && right < newer.len() {
        match older.key(parts, left).cmp(newer.key(parts, right)) {
            std::cmp::Ordering::Less => {
                out.push_from(older, parts, left);
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push_from(newer, parts, right);
                right += 1;
            }
            std::cmp::Ordering::Equal => {
                if newer.versions[right] > older.versions[left] {
                    out.push_from(newer, parts, right);
                } else {
                    out.push_from(older, parts, left);
                }
                left += 1;
                right += 1;
            }
        }
    }
    while left < older.len() {
        out.push_from(older, parts, left);
        left += 1;
    }
    while right < newer.len() {
        out.push_from(newer, parts, right);
        right += 1;
    }
    out
}

/// The first index in `0..len` for which `before` is false.
fn partition_point(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, len);
    while low < high {
        let middle = low + (high - low) / 2;
        if before(middle) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

/// The layer index a manifest's scans last resolved, kept with the manifest
/// so the scans that follow read it instead of the headers again. It holds
/// keys and row numbers only - a few tens of bytes per newer row - and goes
/// when the manifest that names those segments does.
#[derive(Default)]
pub(crate) struct LayerIndexSlot(Arc<Mutex<Option<Arc<LayerIndex>>>>);

/// A manifest is copied to make the one that follows it, whose segments may
/// differ: the copy starts with nothing kept, and the index goes with the
/// last reader of the manifest it was resolved for.
impl Clone for LayerIndexSlot {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for LayerIndexSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "LayerIndexSlot({} bytes)", self.bytes())
    }
}

impl LayerIndexSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<LayerIndex>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bytes the kept index holds.
    pub(crate) fn bytes(&self) -> usize {
        self.lock().as_ref().map_or(0, |index| index.bytes())
    }

    /// The index of `segments`, read from their headers unless the one kept
    /// is theirs. `None` when they cannot be indexed (see
    /// [`LayerIndex::build`]).
    pub(super) fn resolve(
        &self,
        directory: &std::path::Path,
        schema: &TableSchema,
        segments: &[segment::SegmentMeta],
    ) -> Result<Option<Arc<LayerIndex>>, StoreError> {
        let wanted = identity(segments);
        if let Some(kept) = self.lock().as_ref()
            && kept.segments == wanted
        {
            return Ok(Some(Arc::clone(kept)));
        }
        let started = std::time::Instant::now();
        let Some(index) = LayerIndex::build(directory, schema, segments)? else {
            return Ok(None);
        };
        pintail_log::log_debug!(
            "store scan indexed the keys of {} newer rows in {} segments over their bases: {} bytes, {} ms",
            index.len(),
            segments.len(),
            index.bytes(),
            started.elapsed().as_millis()
        );
        let index = Arc::new(index);
        *self.lock() = Some(Arc::clone(&index));
        Ok(Some(index))
    }
}

/// The rows an overlay reads over a segment, one per key in key order: the
/// memtable's, and under them a layered cluster's newer segments. Of a key
/// both hold the greater version wins, and the segments' on a tie.
#[derive(Clone)]
pub(super) struct LayerRows {
    memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    layer: Option<Layer>,
}

/// A layered cluster's newer segments, oldest first, and their index once
/// a scan has reached the cluster.
#[derive(Clone)]
pub(super) struct Layer {
    segments: Arc<Vec<segment::SegmentMeta>>,
    index: Option<Arc<LayerIndex>>,
}

impl LayerRows {
    /// The memtable's rows alone.
    pub(super) fn single(memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>) -> Self {
        Self {
            memtable,
            layer: None,
        }
    }

    /// The rows of `segments` (oldest first) under the memtable's.
    pub(super) fn layered(
        segments: Vec<segment::SegmentMeta>,
        memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    ) -> Self {
        Self {
            memtable,
            layer: (!segments.is_empty()).then(|| Layer {
                segments: Arc::new(segments),
                index: None,
            }),
        }
    }

    /// Whether `other` reads the very same rows.
    pub(super) fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.memtable, &other.memtable)
            && match (&self.layer, &other.layer) {
                (Some(left), Some(right)) => Arc::ptr_eq(&left.segments, &right.segments),
                (None, None) => true,
                _ => false,
            }
    }

    /// Resolves the layer's index, from `slot` when it holds it; `false`
    /// when the newer segments cannot be indexed.
    pub(super) fn resolve(
        &mut self,
        slot: &LayerIndexSlot,
        directory: &std::path::Path,
        schema: &TableSchema,
    ) -> Result<bool, StoreError> {
        let Some(layer) = self.layer.as_mut() else {
            return Ok(true);
        };
        if layer.index.is_some() {
            return Ok(true);
        }
        layer.index = slot.resolve(directory, schema, &layer.segments)?;
        Ok(layer.index.is_some())
    }

    /// The newer segment a [`SpanRow::Layer`] names.
    pub(super) fn segment(&self, segment: u32) -> Option<&segment::SegmentMeta> {
        self.layer.as_ref()?.segments.get(segment as usize)
    }

    /// How many newer segments lie under the memtable.
    pub(super) fn segment_count(&self) -> usize {
        self.layer.as_ref().map_or(0, |layer| layer.segments.len())
    }

    /// A key the layer's index holds, as a key of the table.
    pub(super) fn index_key(&self, index: usize) -> Result<PrimaryKey, StoreError> {
        let unresolved = || StoreError::FormatLimit("a layer was read before it resolved".into());
        let layer = self.layer.as_ref().ok_or_else(unresolved)?;
        let template = &layer.segments.first().ok_or_else(unresolved)?.min_key;
        layer
            .index
            .as_ref()
            .ok_or_else(unresolved)?
            .primary_key(index, template)
    }

    /// The rows of a searchable key range, in key order.
    pub(super) fn range(
        &self,
        lo: &std::ops::Bound<PrimaryKey>,
        hi: &std::ops::Bound<PrimaryKey>,
    ) -> Result<LayerCursor<'_>, StoreError> {
        let index = match &self.layer {
            None => None,
            Some(layer) => {
                let index = layer.index.as_deref().ok_or_else(|| {
                    StoreError::FormatLimit("a layer was read before it resolved".into())
                })?;
                Some((index, index.range(lo, hi)?))
            }
        };
        Ok(LayerCursor {
            memtable: self.memtable.range((lo.clone(), hi.clone())).peekable(),
            index,
            scratch: Vec::new(),
        })
    }
}

/// Which side a cursor's row came from, with its key.
#[derive(Clone, Copy)]
pub(super) enum SpanKey<'a> {
    Memtable(&'a PrimaryKey),
    /// An entry of the layer's index.
    Index(usize),
}

/// [`LayerRows::range`]'s walk.
pub(super) struct LayerCursor<'a> {
    memtable: std::iter::Peekable<std::collections::btree_map::Range<'a, PrimaryKey, StoredRow>>,
    index: Option<(&'a LayerIndex, std::ops::Range<usize>)>,
    scratch: Vec<i128>,
}

impl<'a> LayerCursor<'a> {
    fn memtable_row(row: &'a StoredRow) -> SpanRow<'a> {
        if row.is_deleted() {
            SpanRow::Mask
        } else {
            SpanRow::Row(row)
        }
    }

    /// The parts of index entry `entry`'s key.
    pub(super) fn index_key(&self, entry: usize) -> &'a [i128] {
        self.index
            .as_ref()
            .map_or(&[], |(index, _)| (*index).key(entry))
    }

    /// The next row in key order and where its key is.
    pub(super) fn next(&mut self) -> Result<Option<(SpanKey<'a>, SpanRow<'a>)>, StoreError> {
        let Some((index, range)) = self.index.as_mut() else {
            return Ok(self
                .memtable
                .next()
                .map(|(key, row)| (SpanKey::Memtable(key), Self::memtable_row(row))));
        };
        let index: &'a LayerIndex = index;
        let Some(&(key, row)) = self.memtable.peek() else {
            return Ok(range
                .next()
                .map(|entry| (SpanKey::Index(entry), index.row(entry))));
        };
        if range.start >= range.end {
            self.memtable.next();
            return Ok(Some((SpanKey::Memtable(key), Self::memtable_row(row))));
        }
        self.scratch.clear();
        if !push_key_parts(key, &mut self.scratch) {
            return Err(StoreError::FormatLimit(
                "the memtable overlay needs integer key parts".into(),
            ));
        }
        let entry = range.start;
        Ok(Some(match self.scratch.as_slice().cmp(index.key(entry)) {
            std::cmp::Ordering::Less => {
                self.memtable.next();
                (SpanKey::Memtable(key), Self::memtable_row(row))
            }
            std::cmp::Ordering::Greater => {
                range.next();
                (SpanKey::Index(entry), index.row(entry))
            }
            std::cmp::Ordering::Equal => {
                self.memtable.next();
                range.next();
                if row.version() > index.version(entry) {
                    (SpanKey::Memtable(key), Self::memtable_row(row))
                } else {
                    (SpanKey::Index(entry), index.row(entry))
                }
            }
        }))
    }
}

/// The values of a slice's live rows, wherever each row is: a memtable
/// row's are its own, a layer row's are read from its segment for the
/// columns asked, packed.
pub(super) struct LiveCells<'a> {
    rows: &'a [SpanRow<'a>],
    positions: Vec<usize>,
    /// Per newer segment, the columns of `positions` for its live rows.
    columns: Vec<Vec<DecodedColumn>>,
    /// Per live row, its row in its segment's columns.
    slots: Vec<u32>,
}

impl<'a> LiveCells<'a> {
    /// Reads schema columns `positions` of the layer rows among `rows`
    /// (none of them a mask), within `memory_limit`.
    pub(super) fn read(
        layer: &LayerRows,
        directory: &std::path::Path,
        schema: &TableSchema,
        rows: &'a [SpanRow<'a>],
        positions: Vec<usize>,
        memory_limit: usize,
    ) -> Result<Self, StoreError> {
        let misplaced =
            || StoreError::FormatLimit("a layer row is out of its segment's order".into());
        let mut wanted: Vec<Vec<std::ops::Range<usize>>> = vec![Vec::new(); layer.segment_count()];
        let mut counts = vec![0_u32; layer.segment_count()];
        let mut slots = Vec::with_capacity(rows.len());
        for row in rows {
            let SpanRow::Layer { segment, row } = row else {
                slots.push(0);
                continue;
            };
            let ranges = wanted.get_mut(*segment as usize).ok_or_else(misplaced)?;
            let row = *row as usize;
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end += 1,
                Some(last) if last.end > row => return Err(misplaced()),
                _ => ranges.push(row..row + 1),
            }
            slots.push(counts[*segment as usize]);
            counts[*segment as usize] += 1;
        }
        let mut columns = Vec::with_capacity(wanted.len());
        let memory = AtomicUsize::new(0);
        let budget = segment::ScanMemoryBudget::new(&memory, memory_limit);
        for (index, ranges) in wanted.iter().enumerate() {
            if ranges.is_empty() || positions.is_empty() {
                columns.push(Vec::new());
                continue;
            }
            let meta = layer
                .segment(u32::try_from(index).unwrap_or(u32::MAX))
                .ok_or_else(misplaced)?;
            let fetch = segment::read_projected_column_ranges(
                directory, meta, schema, &positions, ranges, &budget,
            )?;
            columns.push(fetch.columns);
        }
        Ok(Self {
            rows,
            positions,
            columns,
            slots,
        })
    }

    /// The live rows these cells are of.
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The cell of live row `row` in the `column`th of the positions read.
    pub(super) fn cell(&self, row: usize, column: usize) -> Cell<'_> {
        match self.rows[row] {
            SpanRow::Mask => Cell::Null,
            SpanRow::Row(stored) => stored
                .values()
                .get(self.positions[column])
                .map_or(Cell::Null, Cell::Value),
            SpanRow::Layer { segment, .. } => {
                self.columns[segment as usize][column].cell(self.slots[row] as usize)
            }
        }
    }

    /// Bytes the columns read from the layer hold.
    pub(super) fn retained_bytes(&self) -> usize {
        self.columns
            .iter()
            .flatten()
            .map(DecodedColumn::retained_bytes)
            .sum()
    }
}
