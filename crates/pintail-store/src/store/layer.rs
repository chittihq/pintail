//! The rows over a layered cluster's bases, read where they are stored.
//!
//! The newer segments of a cluster are never turned into rows. Their keys,
//! versions and delete flags are resolved once into a [`LayerIndex`] - one
//! entry per key, naming the segment row that holds its newest version -
//! and a slice of a base reads the entries of its key span as a range of
//! arrays. The values of the rows a slice needs are then read from the
//! newer segments column by column, as packed as a base decodes.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicUsize},
};

use pintail_types::{KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rayon::prelude::*;

use super::projected_scan_pool;
use super::scan::{DecodedColumn, empty_packed_column};
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
    /// A live memtable row, by its place in the memtable's image.
    Image { row: u32 },
}

/// One part of a key as it orders: an integer of either sign by value,
/// then text and bytes by their bytes - the order the table's keys have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum KeyPartRef<'a> {
    Int(i128),
    Text(&'a [u8]),
    Binary(&'a [u8]),
}

impl<'a> KeyPartRef<'a> {
    fn of(part: &'a KeyPart) -> Self {
        match part {
            KeyPart::Int64(value) => Self::Int(i128::from(*value)),
            KeyPart::UInt64(value) => Self::Int(i128::from(*value)),
            KeyPart::Utf8(value) => Self::Text(value.as_bytes()),
            KeyPart::Binary(value) => Self::Binary(value),
        }
    }
}

const PACKED_INT: u8 = 0;
const PACKED_TEXT: u8 = 1;
const PACKED_BINARY: u8 = 2;

fn pack_part(part: KeyPartRef<'_>, heap: &mut Vec<u8>) {
    let (tag, bytes) = match part {
        KeyPartRef::Int(value) => {
            heap.push(PACKED_INT);
            heap.extend_from_slice(&value.to_le_bytes());
            return;
        }
        KeyPartRef::Text(bytes) => (PACKED_TEXT, bytes),
        KeyPartRef::Binary(bytes) => (PACKED_BINARY, bytes),
    };
    heap.push(tag);
    heap.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    heap.extend_from_slice(bytes);
}

/// One key of a [`KeyList`]: its integers side by side while every key of
/// the list is integers, its parts packed one after another otherwise.
#[derive(Clone, Copy, Debug)]
pub(super) enum KeyRef<'a> {
    Ints(&'a [i128]),
    Packed(&'a [u8]),
}

/// The parts of a [`KeyRef`], first to last.
pub(super) enum KeyParts<'a> {
    Ints(std::slice::Iter<'a, i128>),
    Packed(&'a [u8]),
}

impl<'a> Iterator for KeyParts<'a> {
    type Item = KeyPartRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ints(values) => values.next().map(|value| KeyPartRef::Int(*value)),
            Self::Packed(bytes) => {
                let (tag, rest) = bytes.split_first()?;
                if *tag == PACKED_INT {
                    let (value, rest) = rest.split_first_chunk::<16>()?;
                    *bytes = rest;
                    return Some(KeyPartRef::Int(i128::from_le_bytes(*value)));
                }
                let (len, rest) = rest.split_first_chunk::<4>()?;
                let len = u32::from_le_bytes(*len) as usize;
                let (value, rest) = rest.split_at_checked(len)?;
                *bytes = rest;
                Some(if *tag == PACKED_TEXT {
                    KeyPartRef::Text(value)
                } else {
                    KeyPartRef::Binary(value)
                })
            }
        }
    }
}

impl<'a> KeyRef<'a> {
    pub(super) fn parts(self) -> KeyParts<'a> {
        match self {
            Self::Ints(values) => KeyParts::Ints(values.iter()),
            Self::Packed(bytes) => KeyParts::Packed(bytes),
        }
    }

    /// Part by part, a key that is a prefix of another before it: the order
    /// of the table's keys.
    pub(super) fn compare(self, other: KeyRef<'_>) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Ints(left), KeyRef::Ints(right)) => left.cmp(right),
            (left, right) => left.parts().cmp(right.parts()),
        }
    }

    /// This key as a key of the table, its integer parts typed as
    /// `template`'s are.
    fn primary_key(self, template: &PrimaryKey) -> Result<PrimaryKey, StoreError> {
        let unfit = || StoreError::FormatLimit("a layered key does not fit its key type".into());
        let mut like = template.parts().iter();
        let mut parts = Vec::with_capacity(template.parts().len());
        for part in self.parts() {
            parts.push(match (part, like.next().ok_or_else(unfit)?) {
                (KeyPartRef::Int(value), KeyPart::Int64(_)) => {
                    KeyPart::Int64(i64::try_from(value).map_err(|_| unfit())?)
                }
                (KeyPartRef::Int(value), KeyPart::UInt64(_)) => {
                    KeyPart::UInt64(u64::try_from(value).map_err(|_| unfit())?)
                }
                (KeyPartRef::Text(bytes), KeyPart::Utf8(_)) => {
                    KeyPart::Utf8(String::from_utf8(bytes.to_vec()).map_err(|_| unfit())?)
                }
                (KeyPartRef::Binary(bytes), KeyPart::Binary(_)) => KeyPart::Binary(bytes.to_vec()),
                _ => return Err(unfit()),
            });
        }
        if like.next().is_some() {
            return Err(unfit());
        }
        PrimaryKey::new(parts).map_err(|_| unfit())
    }
}

/// Keys held side by side in one allocation, whatever their parts are.
///
/// While every key is integers of one count they are the integers
/// themselves, `parts` per key, and compare as slices. The first key with a
/// text or binary part (or another count of parts) turns the list into
/// packed keys: each part's kind, length and bytes one after another, with
/// where every key ends. A key vector per row made a span of a hundred
/// thousand changed rows a hundred thousand allocations before the first
/// comparison; a string per text key would do the same.
#[derive(Default)]
pub(super) struct KeyList {
    len: usize,
    parts: usize,
    ints: Vec<i128>,
    packed: bool,
    ends: Vec<usize>,
    heap: Vec<u8>,
}

impl KeyList {
    /// A list of the one key `key`.
    pub(super) fn of(key: &PrimaryKey) -> Self {
        let mut list = Self::default();
        list.push(key);
        list
    }

    pub(super) fn clear(&mut self) {
        self.len = 0;
        self.parts = 0;
        self.ints.clear();
        self.packed = false;
        self.ends.clear();
        self.heap.clear();
    }

    pub(super) fn get(&self, index: usize) -> KeyRef<'_> {
        if self.packed {
            let start = if index == 0 { 0 } else { self.ends[index - 1] };
            KeyRef::Packed(&self.heap[start..self.ends[index]])
        } else {
            KeyRef::Ints(&self.ints[index * self.parts..(index + 1) * self.parts])
        }
    }

    /// The keys as integers, when each is one integer.
    pub(super) fn single_integers(&self) -> Option<&[i128]> {
        (!self.packed && self.parts == 1).then_some(self.ints.as_slice())
    }

    /// Bytes the keys hold.
    pub(super) fn bytes(&self) -> usize {
        self.ints.capacity() * size_of::<i128>()
            + self.ends.capacity() * size_of::<usize>()
            + self.heap.capacity()
    }

    /// Bytes the keys themselves take, whatever was set aside for more.
    pub(super) fn used_bytes(&self) -> usize {
        self.ints.len() * size_of::<i128>() + self.ends.len() * size_of::<usize>() + self.heap.len()
    }

    /// Makes room for the keys of `left` and `right` together.
    fn reserve_for(&mut self, left: &Self, right: &Self) {
        self.ints.reserve(left.ints.len() + right.ints.len());
        self.ends.reserve(left.ends.len() + right.ends.len());
        self.heap.reserve(left.heap.len() + right.heap.len());
    }

    fn pack(&mut self) {
        if self.packed {
            return;
        }
        self.packed = true;
        if self.parts > 0 {
            for key in self.ints.chunks_exact(self.parts) {
                for value in key {
                    pack_part(KeyPartRef::Int(*value), &mut self.heap);
                }
                self.ends.push(self.heap.len());
            }
        }
        self.ints = Vec::new();
    }

    fn push_parts<'part>(
        &mut self,
        count: usize,
        parts: impl Iterator<Item = KeyPartRef<'part>> + Clone,
    ) {
        let integers = parts.clone().all(|part| matches!(part, KeyPartRef::Int(_)));
        if !self.packed && integers && count > 0 && (self.len == 0 || self.parts == count) {
            self.parts = count;
            self.ints.extend(parts.filter_map(|part| match part {
                KeyPartRef::Int(value) => Some(value),
                _ => None,
            }));
        } else {
            self.pack();
            for part in parts {
                pack_part(part, &mut self.heap);
            }
            self.ends.push(self.heap.len());
        }
        self.len += 1;
    }

    pub(super) fn push(&mut self, key: &PrimaryKey) {
        self.push_parts(key.parts().len(), key.parts().iter().map(KeyPartRef::of));
    }

    pub(super) fn push_ref(&mut self, key: KeyRef<'_>) {
        match key {
            KeyRef::Ints(values)
                if !self.packed && !values.is_empty() && self.parts == values.len() =>
            {
                self.ints.extend_from_slice(values);
                self.len += 1;
            }
            KeyRef::Ints(values) => self.push_parts(
                values.len(),
                values.iter().map(|value| KeyPartRef::Int(*value)),
            ),
            KeyRef::Packed(bytes) if self.packed => {
                self.heap.extend_from_slice(bytes);
                self.ends.push(self.heap.len());
                self.len += 1;
            }
            KeyRef::Packed(bytes) => {
                let parts = KeyParts::Packed(bytes);
                self.push_parts(parts.clone().count(), parts);
            }
        }
    }
}

impl Clone for KeyParts<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Ints(values) => Self::Ints(values.clone()),
            Self::Packed(bytes) => Self::Packed(bytes),
        }
    }
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
/// in key order: the key, the winning version, whether it
/// is a delete, and the segment row that holds it. No value is read.
pub(crate) struct LayerIndex {
    segments: Vec<SegmentIdentity>,
    parts: usize,
    entries: Headers,
}

/// One segment's headers, in key order.
#[derive(Default)]
struct Headers {
    keys: KeyList,
    versions: Vec<u64>,
    sources: Vec<u32>,
    rows: Vec<u32>,
    deleted: Vec<bool>,
}

impl Headers {
    fn len(&self) -> usize {
        self.versions.len()
    }

    fn key(&self, index: usize) -> KeyRef<'_> {
        self.keys.get(index)
    }

    fn push_from(&mut self, other: &Self, index: usize) {
        self.keys.push_ref(other.key(index));
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

/// Bytes a [`LayerIndex`] holds per newer row of a table whose keys are
/// like `sample`: a text or binary part is held packed, at its own length.
pub(super) fn layer_index_row_bytes(sample: &PrimaryKey) -> u64 {
    let key = KeyList::of(sample).used_bytes();
    LAYER_INDEX_ROW_BYTES - 16 + u64::try_from(key).unwrap_or(u64::MAX)
}

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
        self.entries.len()
    }

    pub(super) fn key(&self, index: usize) -> KeyRef<'_> {
        self.entries.key(index)
    }

    pub(super) fn version(&self, index: usize) -> u64 {
        self.entries.versions[index]
    }

    /// The row at `index`: a mask for a delete.
    pub(super) fn row(&self, index: usize) -> SpanRow<'static> {
        if self.entries.deleted[index] {
            SpanRow::Mask
        } else {
            SpanRow::Layer {
                segment: self.entries.sources[index],
                row: self.entries.rows[index],
            }
        }
    }

    /// Bytes this index holds.
    pub(crate) fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.entries.keys.bytes()
            + self.entries.versions.capacity() * size_of::<u64>()
            + (self.entries.sources.capacity() + self.entries.rows.capacity()) * size_of::<u32>()
            + self.entries.deleted.capacity()
    }

    /// The key at `index` as a key of the table, its parts typed as
    /// `template`'s are.
    pub(super) fn primary_key(
        &self,
        index: usize,
        template: &PrimaryKey,
    ) -> Result<PrimaryKey, StoreError> {
        self.key(index).primary_key(template)
    }

    /// The entries inside a key range, as a range of this index.
    pub(super) fn range(
        &self,
        lo: &std::ops::Bound<PrimaryKey>,
        hi: &std::ops::Bound<PrimaryKey>,
    ) -> std::ops::Range<usize> {
        key_range(self.len(), |index| self.key(index), lo, hi)
    }

    /// Reads the headers of `segments` (oldest first) and resolves them to
    /// one entry per key: the greater version wins, and of equal versions
    /// the one met first, in the older segment. `None` when a segment holds
    /// more rows than an entry can name.
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
        Ok(
            Self::read_resolved(directory, schema, segments, parts, 0)?.map(|entries| Self {
                segments: identity(segments),
                parts,
                entries,
            }),
        )
    }

    /// This index with the segments that follow its own in `segments`
    /// resolved over it: what a flush adds to a cluster is one newer
    /// segment, and reading that segment's headers and merging them in once
    /// costs a pass over the entries instead of every header again. `None`
    /// when `segments` does not start with the segments indexed here, or
    /// the new ones cannot be indexed.
    pub(super) fn extend(
        &self,
        directory: &std::path::Path,
        schema: &TableSchema,
        segments: &[segment::SegmentMeta],
    ) -> Result<Option<Self>, StoreError> {
        let wanted = identity(segments);
        let held = self.segments.len();
        if held == 0 || held >= wanted.len() || wanted[..held] != self.segments[..] {
            return Ok(None);
        }
        let Some(newer) =
            Self::read_resolved(directory, schema, &segments[held..], self.parts, held)?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            segments: wanted,
            parts: self.parts,
            entries: merge_headers(&self.entries, &newer),
        }))
    }

    /// The headers of `segments` (oldest first) as one entry per key, each
    /// naming its segment by its place in the list plus `first_source`.
    fn read_resolved(
        directory: &std::path::Path,
        schema: &TableSchema,
        segments: &[segment::SegmentMeta],
        parts: usize,
        first_source: usize,
    ) -> Result<Option<Headers>, StoreError> {
        // (segment, first key, the key the piece stops before).
        let mut pieces: Vec<(usize, PrimaryKey, Option<PrimaryKey>)> = Vec::new();
        for (index, meta) in segments.iter().enumerate() {
            if u32::try_from(meta.row_count).is_err()
                || u32::try_from(index + first_source).is_err()
            {
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
            let source = u32::try_from(*index + first_source).unwrap_or(u32::MAX);
            let mut headers = Headers::default();
            for row in scan.rows {
                if stop.as_ref().is_some_and(|stop| row.key >= *stop) {
                    break;
                }
                if row.key.parts().len() != parts {
                    return Ok(None);
                }
                headers.keys.push(&row.key);
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
        let mut runs = Vec::with_capacity(segments.len());
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
                    if last.is_some_and(|last| run.key(last).compare(piece.key(row)).is_eq()) {
                        if piece.versions[row] > run.versions[run.len() - 1] {
                            run.replace_last(&piece, row);
                        }
                    } else {
                        run.push_from(&piece, row);
                    }
                }
            }
            runs.push(run);
        }
        Ok(Some(projected_scan_pool()?.install(|| merge_runs(runs))))
    }
}

/// Runs of one entry per key, oldest first, as one: a key several hold
/// resolves to its greatest version, and to the oldest run's among equals.
/// The runs are merged as a tree, its two halves at once, so every entry is
/// copied once per level of the tree rather than once per run that follows
/// its own.
fn merge_runs(mut runs: Vec<Headers>) -> Headers {
    match runs.len() {
        0 => Headers::default(),
        1 => runs.pop().unwrap_or_default(),
        count => {
            let newer = runs.split_off(count / 2);
            let (older, newer) = rayon::join(|| merge_runs(runs), || merge_runs(newer));
            merge_headers(&older, &newer)
        }
    }
}

/// `older` and `newer`, each one entry per key in key order, as one: of a
/// key both hold, the greater version, and `older`'s when they are equal.
fn merge_headers(older: &Headers, newer: &Headers) -> Headers {
    let mut out = Headers::default();
    out.keys.reserve_for(&older.keys, &newer.keys);
    let (mut left, mut right) = (0, 0);
    while left < older.len() && right < newer.len() {
        match older.key(left).compare(newer.key(right)) {
            std::cmp::Ordering::Less => {
                out.push_from(older, left);
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push_from(newer, right);
                right += 1;
            }
            std::cmp::Ordering::Equal => {
                if newer.versions[right] > older.versions[left] {
                    out.push_from(newer, right);
                } else {
                    out.push_from(older, left);
                }
                left += 1;
                right += 1;
            }
        }
    }
    while left < older.len() {
        out.push_from(older, left);
        left += 1;
    }
    while right < newer.len() {
        out.push_from(newer, right);
        right += 1;
    }
    out
}

/// The entries of `len` sorted keys that lie inside a key range.
fn key_range<'keys>(
    len: usize,
    key: impl Fn(usize) -> KeyRef<'keys>,
    lo: &std::ops::Bound<PrimaryKey>,
    hi: &std::ops::Bound<PrimaryKey>,
) -> std::ops::Range<usize> {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    // The first entry at or above a key, and the first above it.
    let at_or_above = |bound: &PrimaryKey| {
        let bound = KeyList::of(bound);
        partition_point(len, |index| key(index).compare(bound.get(0)).is_lt())
    };
    let above = |bound: &PrimaryKey| {
        let bound = KeyList::of(bound);
        partition_point(len, |index| key(index).compare(bound.get(0)).is_le())
    };
    let start = match lo {
        Included(bound) => at_or_above(bound),
        Excluded(bound) => above(bound),
        Unbounded => 0,
    };
    let end = match hi {
        Included(bound) => above(bound),
        Excluded(bound) => at_or_above(bound),
        Unbounded => len,
    };
    start..end.max(start)
}

/// Rows of the memtable one chunk of an image column holds.
const IMAGE_CHUNK_ROWS: usize = 16 * 1024;

/// The memtable's keys, versions and delete flags as arrays in key order.
struct ImageKeys {
    keys: KeyList,
    versions: Vec<u64>,
    deleted: Vec<bool>,
}

impl ImageKeys {
    /// `None` for a memtable of more rows than an image row can name.
    fn build(memtable: &BTreeMap<PrimaryKey, StoredRow>) -> Option<Self> {
        if memtable.is_empty() || u32::try_from(memtable.len()).is_err() {
            return None;
        }
        let mut image = Self {
            keys: KeyList::default(),
            versions: Vec::with_capacity(memtable.len()),
            deleted: Vec::with_capacity(memtable.len()),
        };
        for (key, row) in memtable {
            image.keys.push(key);
            image.versions.push(row.version());
            image.deleted.push(row.is_deleted());
        }
        Some(image)
    }

    fn key(&self, index: usize) -> KeyRef<'_> {
        self.keys.get(index)
    }

    fn row(&self, index: usize) -> SpanRow<'static> {
        if self.deleted[index] {
            SpanRow::Mask
        } else {
            SpanRow::Image {
                row: u32::try_from(index).unwrap_or(u32::MAX),
            }
        }
    }
}

/// One schema column of every memtable row, packed, in chunks of
/// [`IMAGE_CHUNK_ROWS`] rows.
type ImageColumn = Arc<Vec<DecodedColumn>>;

/// The cell of memtable row `row` in an image column.
pub(super) fn image_cell(column: &[DecodedColumn], row: usize) -> Cell<'_> {
    column
        .get(row / IMAGE_CHUNK_ROWS)
        .map_or(Cell::Null, |chunk| chunk.cell(row % IMAGE_CHUNK_ROWS))
}

/// One state of the memtable as arrays: its keys in key order, and each
/// column a scan has asked for, packed in the shape a segment of that type
/// decodes to. Built by the first scan to need it and read by every scan of
/// the same state after it; the memtable hands out a new one once a write
/// changes its rows.
///
/// A scan otherwise walks the memtable's map for every slice it overlays,
/// turns each key into integers to compare, and parses each changed value
/// from its row form into the chunk - the same work again for every query,
/// though the rows are the same.
#[derive(Default)]
pub(crate) struct MemtableImage {
    keys: OnceLock<Option<ImageKeys>>,
    columns: Mutex<HashMap<usize, Arc<OnceLock<ImageColumn>>>>,
}

impl MemtableImage {
    /// Whether a scan has built any of it.
    pub(crate) fn is_built(&self) -> bool {
        self.keys.get().is_some()
    }

    fn keys(&self, memtable: &BTreeMap<PrimaryKey, StoredRow>) -> Option<&ImageKeys> {
        self.keys
            .get_or_init(|| ImageKeys::build(memtable))
            .as_ref()
    }

    /// The column at schema `position`, built from the rows on first use.
    /// One thread builds it while the others that want it wait; different
    /// columns build side by side.
    fn column(
        &self,
        position: usize,
        data_type: pintail_types::DataType,
        memtable: &BTreeMap<PrimaryKey, StoredRow>,
    ) -> ImageColumn {
        let slot = Arc::clone(
            self.columns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(position)
                .or_default(),
        );
        Arc::clone(slot.get_or_init(|| {
            let mut chunks = Vec::with_capacity(memtable.len().div_ceil(IMAGE_CHUNK_ROWS));
            let mut cells = Vec::with_capacity(IMAGE_CHUNK_ROWS.min(memtable.len()));
            for row in memtable.values() {
                cells.push((
                    cells.len(),
                    row.values().get(position).map_or(Cell::Null, Cell::Value),
                ));
                if cells.len() == IMAGE_CHUNK_ROWS {
                    chunks.push(empty_packed_column(data_type).interleave_cells(&cells));
                    cells.clear();
                }
            }
            if !cells.is_empty() {
                chunks.push(empty_packed_column(data_type).interleave_cells(&cells));
            }
            Arc::new(chunks)
        }))
    }
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

/// A manifest is copied to make the one that follows it. A flush adds one
/// segment over the ones indexed, so the copy keeps the index and the next
/// scan extends it; a publication that takes segments away starts its
/// manifest with [`Self::default`] instead.
impl Clone for LayerIndexSlot {
    fn clone(&self) -> Self {
        Self(Arc::new(Mutex::new(self.lock().clone())))
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
        let kept = self.lock().clone();
        if let Some(kept) = &kept
            && kept.segments == wanted
        {
            return Ok(Some(Arc::clone(kept)));
        }
        let started = std::time::Instant::now();
        let extended = match &kept {
            Some(kept) => kept.extend(directory, schema, segments)?,
            None => None,
        };
        let index = if let Some(index) = extended {
            index
        } else {
            let Some(index) = LayerIndex::build(directory, schema, segments)? else {
                return Ok(None);
            };
            index
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
    /// The memtable's image, once a scan that names the key's columns has
    /// reached these rows; without it they are read from the map.
    image: Option<Arc<MemtableImage>>,
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
            image: None,
            layer: None,
        }
    }

    /// Reads the memtable through `image` from here on.
    pub(super) fn attach_image(&mut self, image: &Arc<MemtableImage>) {
        if self.image.is_none() && !self.memtable.is_empty() && image.keys(&self.memtable).is_some()
        {
            self.image = Some(Arc::clone(image));
        }
    }

    fn image_keys(&self) -> Option<&ImageKeys> {
        self.image.as_ref()?.keys.get()?.as_ref()
    }

    /// Whether the memtable is read through its image.
    pub(super) fn has_image(&self) -> bool {
        self.image.is_some()
    }

    /// The memtable's column at schema `position`, packed.
    pub(super) fn image_column(
        &self,
        position: usize,
        schema: &TableSchema,
    ) -> Result<Arc<Vec<DecodedColumn>>, StoreError> {
        let missing =
            || StoreError::FormatLimit("a memtable image was read before it was built".into());
        let column = schema.columns().get(position).ok_or_else(missing)?;
        Ok(self.image.as_ref().ok_or_else(missing)?.column(
            position,
            column.data_type(),
            &self.memtable,
        ))
    }

    /// The key a cursor of these rows stopped at, as a key of the table.
    pub(super) fn key_of(&self, key: SpanKey<'_>) -> Result<PrimaryKey, StoreError> {
        match key {
            SpanKey::Memtable(key) => Ok(key.clone()),
            SpanKey::Index(entry) => self.index_key(entry),
            SpanKey::Image(row) => {
                let missing = || {
                    StoreError::FormatLimit("a memtable image was read before it was built".into())
                };
                let template = self.memtable.keys().next().ok_or_else(missing)?;
                self.image_keys()
                    .ok_or_else(missing)?
                    .key(row)
                    .primary_key(template)
            }
        }
    }

    /// The rows of `segments` (oldest first) under the memtable's.
    pub(super) fn layered(
        segments: Vec<segment::SegmentMeta>,
        memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    ) -> Self {
        Self {
            memtable,
            image: None,
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
    fn index_key(&self, index: usize) -> Result<PrimaryKey, StoreError> {
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
                Some((index, index.range(lo, hi)))
            }
        };
        let memtable = match self.image_keys() {
            Some(image) => MemtableSide::Image(
                image,
                key_range(image.versions.len(), |row| image.key(row), lo, hi),
            ),
            None => MemtableSide::Map(self.memtable.range((lo.clone(), hi.clone())).peekable()),
        };
        Ok(LayerCursor {
            memtable,
            index,
            scratch: KeyList::default(),
        })
    }
}

/// Which side a cursor's row came from, with its key.
#[derive(Clone, Copy)]
pub(super) enum SpanKey<'a> {
    Memtable(&'a PrimaryKey),
    /// An entry of the layer's index.
    Index(usize),
    /// A row of the memtable's image.
    Image(usize),
}

/// The memtable's rows of a range: walked through its map, or a range of
/// its image.
enum MemtableSide<'a> {
    Map(std::iter::Peekable<std::collections::btree_map::Range<'a, PrimaryKey, StoredRow>>),
    Image(&'a ImageKeys, std::ops::Range<usize>),
}

/// [`LayerRows::range`]'s walk.
pub(super) struct LayerCursor<'a> {
    memtable: MemtableSide<'a>,
    index: Option<(&'a LayerIndex, std::ops::Range<usize>)>,
    scratch: KeyList,
}

impl<'a> LayerCursor<'a> {
    fn memtable_row(row: &'a StoredRow) -> SpanRow<'a> {
        if row.is_deleted() {
            SpanRow::Mask
        } else {
            SpanRow::Row(row)
        }
    }

    /// A key held in arrays: an index entry's or an image row's. A key
    /// still in the memtable's map is not.
    pub(super) fn parts(&self, key: SpanKey<'a>) -> Option<KeyRef<'a>> {
        match (key, &self.memtable) {
            (SpanKey::Index(entry), _) => self.index.as_ref().map(|(index, _)| (*index).key(entry)),
            (SpanKey::Image(row), MemtableSide::Image(image, _)) => Some((*image).key(row)),
            _ => None,
        }
    }

    fn advance(memtable: &mut MemtableSide<'a>) {
        match memtable {
            MemtableSide::Map(rows) => {
                rows.next();
            }
            MemtableSide::Image(_, range) => {
                range.next();
            }
        }
    }

    /// The next row in key order and where its key is.
    pub(super) fn next(&mut self) -> Result<Option<(SpanKey<'a>, SpanRow<'a>)>, StoreError> {
        // The memtable's next row: where its key is, its version, the row.
        let head = match &mut self.memtable {
            MemtableSide::Map(rows) => rows.peek().map(|&(key, row)| {
                (
                    SpanKey::Memtable(key),
                    row.version(),
                    Self::memtable_row(row),
                )
            }),
            MemtableSide::Image(image, range) => (range.start < range.end).then(|| {
                (
                    SpanKey::Image(range.start),
                    image.versions[range.start],
                    image.row(range.start),
                )
            }),
        };
        let Some((index, range)) = self.index.as_mut() else {
            Self::advance(&mut self.memtable);
            return Ok(head.map(|(key, _, row)| (key, row)));
        };
        let index: &'a LayerIndex = index;
        let Some((key, version, row)) = head else {
            return Ok(range
                .next()
                .map(|entry| (SpanKey::Index(entry), index.row(entry))));
        };
        if range.start >= range.end {
            Self::advance(&mut self.memtable);
            return Ok(Some((key, row)));
        }
        let entry = range.start;
        let order = match (key, &self.memtable) {
            (SpanKey::Image(at), MemtableSide::Image(image, _)) => {
                image.key(at).compare(index.key(entry))
            }
            (SpanKey::Memtable(primary), _) => {
                self.scratch.clear();
                self.scratch.push(primary);
                self.scratch.get(0).compare(index.key(entry))
            }
            _ => {
                return Err(StoreError::FormatLimit(
                    "a memtable row lost its key".into(),
                ));
            }
        };
        Ok(Some(match order {
            std::cmp::Ordering::Less => {
                Self::advance(&mut self.memtable);
                (key, row)
            }
            std::cmp::Ordering::Greater => {
                range.next();
                (SpanKey::Index(entry), index.row(entry))
            }
            std::cmp::Ordering::Equal => {
                Self::advance(&mut self.memtable);
                range.next();
                if version > index.version(entry) {
                    (key, row)
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
    /// The memtable image's columns of `positions`, when a row is read
    /// through it.
    image: Vec<Arc<Vec<DecodedColumn>>>,
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
        let image = if rows.iter().any(|row| matches!(row, SpanRow::Image { .. })) {
            positions
                .iter()
                .map(|position| layer.image_column(*position, schema))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        Ok(Self {
            rows,
            positions,
            columns,
            slots,
            image,
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
            SpanRow::Image { row } => self
                .image
                .get(column)
                .map_or(Cell::Null, |cells| image_cell(cells, row as usize)),
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
