//! Grouping by a packed key: a composite key, or one sparse integer key.
//!
//! A GROUP BY over two or more columns used to key a map by a vector of
//! values per group - a hash and a comparison of every value, a clone and an
//! allocation per new group, a vector of states beside it - and merge every
//! worker's partial map into one on a single thread. A sparse integer key
//! scattered its rows and replayed them into a map of boxed states.
//!
//! Here each key column is one fixed-width cell: an integer or a date's
//! packed units as they are, a decimal's scaled units, and for text the id
//! of its collation class, interned once per distinct spelling. The cells of
//! a row are its key, hashed once. Rows are cut into partitions by that
//! hash, a worker per partition folds them into an open-addressing table
//! whose groups hold their key and their aggregate totals inline, and no
//! partition shares a group with another: the result is the partitions one
//! after the other, with no merge.
//!
//! Text keys with few classes skip the cut and the hash altogether. A
//! state by a kind is a few dozen groups over millions of rows: cutting
//! those rows into partitions and probing a table per row costs several
//! times what indexing an array by the two class ids does. While every key
//! is text carried by dictionary code and the classes met multiply to a
//! small table ([`DENSE_SLOTS`]), rows fold straight into that table, each
//! slot a group in the same words a partition's table holds plus the
//! ordinal of the first row that reached it. Workers fold their own copy
//! and the copies merge by taking the totals together and the spellings of
//! the earlier row, so a group still shows its own first row's spellings.
//! The moment a batch arrives without codes, or the classes outgrow the
//! table, its groups move into the partitions' tables and the fold goes on
//! as above.
//!
//! Only aggregates whose rows reduce to an exact, order-free total fold
//! here ([`PackedLane`]); a query with any other aggregate keeps the path it
//! had, and the profile says which one declined.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use pintail_sql::AggregateFunction;
use pintail_types::{DataType, Value};
use rayon::prelude::*;

use super::aggregate::{
    AggregateGroup, AggregateState, CompiledAggregate, build_local_direct_groups,
    finish_aggregate_groups, merge_partial_group, merge_spilled_aggregate_groups,
    write_aggregate_spill_run,
};
use super::join::{normalized_group_hash_key, normalized_group_text};
use super::morsel::{Morsel, default_morsel_limit, morsel_plan};
use super::two_pass::{
    FINALIZE_CHARGE_SLICE, LaneReader, PackedCell, PackedLane, ReadyColumn, ReadyColumns,
    TwoPassLane, aggregate_lanes, derived_temporal_units, labelled_text_value, lane_readers,
    merge_partial_labels, packed_lane, two_pass_key_value, units_fit_a_lane,
};
use super::{ExecError, HASH_ENTRY_OVERHEAD, MaterializedRows, MemoryTracker, PullOperator};
use crate::batch::{DecimalUnits, TypedValues, mix64};
use crate::collation::Collation;
use crate::{RecordBatch, spill};

/// Partitions the rows are cut into, as a power of two. Enough that a
/// partition's table of a few thousand groups stays in a core's own cache
/// when the query holds a million of them.
const PARTITION_BITS: u32 = 8;
const PARTITIONS: usize = 1 << PARTITION_BITS;

/// Rows gathered before they are cut and folded: enough for every worker
/// to have a morsel, few enough that the cut rows stay a small share of
/// what the groups themselves hold.
const WINDOW_ROWS: usize = 4 * crate::batch::DEFAULT_BATCH_ROWS;

/// Rows a morsel is cut to at most, so a worker's cut rows stay in cache
/// between the pass that writes them and the pass that orders them.
const MORSEL_ROWS: usize = 16_384;

/// A single integer key narrower than this is left to the range fold,
/// which indexes an array by the key and needs no hash.
const SPARSE_KEY_SPAN: i128 = 1 << 18;

/// Key columns a packed key holds: one NULL bit each in the key's mask.
const MAX_KEY_COLUMNS: usize = 16;

/// Aggregates a query folds here: one NULL bit each in the high half of a
/// cut row's second word.
const MAX_LANES: usize = 32;

/// Most slots the dense table of text keys holds: the product of each
/// key's classes plus one for its NULL.
const DENSE_SLOTS: usize = 4_096;

/// A dictionary of at most this many entries is interned whole, used by
/// the batch's rows or not.
const WHOLE_DICTIONARY: usize = 16;

/// Low half of a cut row's second word: the key's NULL bits.
const KEY_NULLS: u64 = 0xFFFF_FFFF;

/// How one key column becomes a cell, and back into a value.
#[derive(Clone, Copy)]
enum KeyKind {
    /// An integer column: the cell is the value's bits.
    Integer(DataType),
    /// A DATE or DATETIME column: the cell is its packed units.
    Temporal(DataType),
    /// A decimal column: the cell is its scaled units.
    Decimal { scale: u8 },
    /// A text column: the cell is the id of the spelling's collation class,
    /// and the group keeps the id of the spelling its first row carried.
    Text { slot: usize },
}

/// What a query's keys and aggregates fold as, decided on its first batch.
pub(super) struct PackedGroupPlan {
    columns: Vec<usize>,
    kinds: Vec<KeyKind>,
    /// The collation of each text key, by text slot.
    text_collations: Vec<Collation>,
    lanes: Vec<TwoPassLane>,
    packed: Vec<PackedLane>,
    /// Where each lane's totals start among a group's cell words.
    cell_offsets: Vec<usize>,
    cell_words: usize,
    /// Where each lane's bits sit in a cut row; `None` for a lane that
    /// reads no bits (a count).
    lane_words: Vec<Option<usize>>,
    /// Words of a cut row: hash, NULL bits, key cells, spellings, lane bits.
    cut_stride: usize,
    /// Words of a group: key NULL bits, key cells, spellings, totals.
    group_stride: usize,
}

impl PackedGroupPlan {
    fn keys(&self) -> usize {
        self.columns.len()
    }

    fn texts(&self) -> usize {
        self.text_collations.len()
    }

    /// The fold for this query, or why it keeps the path it had.
    pub(super) fn of(
        columns: &[usize],
        aggregates: &[CompiledAggregate],
        first: &RecordBatch,
        key_collations: &[Collation],
    ) -> Result<Self, String> {
        // `PINTAIL_DISABLE_PACKED_GROUP` keeps every query on the path it
        // took before, for measuring one against the other.
        if super::switches::packed_group_disabled() {
            return Err("disabled".to_owned());
        }
        if columns.len() > MAX_KEY_COLUMNS || key_collations.len() != columns.len() {
            return Err("more key columns than a packed key holds".to_owned());
        }
        let mut kinds = Vec::with_capacity(columns.len());
        let mut text_collations = Vec::new();
        for (index, column) in columns.iter().enumerate() {
            let kind = key_kind(first, *column, text_collations.len())
                .ok_or_else(|| format!("key column {} has no fixed-width cell", index + 1))?;
            if matches!(kind, KeyKind::Text { .. }) {
                text_collations.push(key_collations[index]);
            }
            kinds.push(kind);
        }
        match kinds.as_slice() {
            [KeyKind::Integer(_)] => {
                if !sparse_integer_key(first, columns[0]) {
                    return Err("one dense integer key is the range fold's".to_owned());
                }
            }
            [_] => return Err("one key column has its own fold".to_owned()),
            _ => {}
        }
        if aggregates.len() > MAX_LANES {
            return Err("more aggregates than a cut row marks NULLs for".to_owned());
        }
        let mut lanes = aggregate_lanes(aggregates, first)
            .ok_or_else(|| "an aggregate has no lane".to_owned())?;
        // A COUNT of a column reads only whether each row holds a value,
        // whatever lane its column's values would ride for a SUM.
        for (lane, aggregate) in lanes.iter_mut().zip(aggregates) {
            if aggregate.function == AggregateFunction::Count
                && !aggregate.distinct
                && let Some(column) = aggregate
                    .expr
                    .as_ref()
                    .and_then(crate::expression::CompiledExpr::column_index)
            {
                *lane = TwoPassLane::Present { column };
            }
        }
        let mut packed = Vec::with_capacity(lanes.len());
        for (index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
            packed.push(
                packed_lane(lane, aggregate).ok_or_else(|| {
                    format!("aggregate {} has no exact order-free total", index + 1)
                })?,
            );
        }
        let mut cell_offsets = Vec::with_capacity(packed.len());
        let mut cell_words = 0;
        let mut lane_words = Vec::with_capacity(packed.len());
        let mut lane_count = 0;
        for lane in &packed {
            cell_offsets.push(cell_words);
            let (cells, bits) = match lane {
                PackedLane::Count | PackedLane::Present => (1, false),
                PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
                    (3, true)
                }
                PackedLane::Minimum { .. } | PackedLane::Maximum { .. } => (2, true),
            };
            cell_words += cells;
            lane_words.push(bits.then(|| {
                lane_count += 1;
                lane_count - 1
            }));
        }
        let keys = columns.len();
        let texts = text_collations.len();
        Ok(Self {
            columns: columns.to_vec(),
            kinds,
            text_collations,
            lanes,
            packed,
            cell_offsets,
            cell_words,
            lane_words: lane_words
                .into_iter()
                .map(|slot| slot.map(|slot| 2 + keys + texts + slot))
                .collect(),
            cut_stride: 2 + keys + texts + lane_count,
            group_stride: 1 + keys + texts + cell_words,
        })
    }

    /// Whether `batch` carries every key column and lane in the packed form
    /// the plan reads. One that does not - a date column holding a zero
    /// date, a decimal wider than a cell - folds row by row instead.
    fn carries(&self, batch: &RecordBatch) -> bool {
        let keys = self.columns.iter().zip(&self.kinds).all(|(column, kind)| {
            match (kind, key_kind(batch, *column, 0)) {
                (KeyKind::Integer(expected), Some(KeyKind::Integer(found)))
                | (KeyKind::Temporal(expected), Some(KeyKind::Temporal(found))) => {
                    *expected == found
                }
                (KeyKind::Decimal { scale }, Some(KeyKind::Decimal { scale: found })) => {
                    *scale == found
                }
                (KeyKind::Text { .. }, Some(KeyKind::Text { .. })) => true,
                _ => false,
            }
        });
        keys && self.lanes.iter().all(|lane| match lane {
            TwoPassLane::Temporal { column, .. } => {
                derived_temporal_units(batch, *column).is_some()
            }
            _ => true,
        })
    }
}

/// The cell kind of `column` as `batch` carries it.
fn key_kind(batch: &RecordBatch, column: usize, slot: usize) -> Option<KeyKind> {
    let vector = batch.column(column)?;
    let data_type = vector.data_type();
    let (typed, _) = vector.typed()?;
    match (data_type, typed) {
        (DataType::Utf8, TypedValues::Utf8(_)) => Some(KeyKind::Text { slot }),
        (DataType::Date32 | DataType::DateTime64 { .. }, _) => {
            derived_temporal_units(batch, column).map(|_| KeyKind::Temporal(data_type))
        }
        (
            DataType::Decimal { precision, .. },
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(values),
                scale,
                text,
            },
        ) if units_fit_a_lane(precision) && text.derived() && values.len() >= batch.row_count() => {
            Some(KeyKind::Decimal { scale: *scale })
        }
        (_, TypedValues::Int64(values))
            if data_type.storage_type() == DataType::Int64 && values.len() >= batch.row_count() =>
        {
            Some(KeyKind::Integer(data_type))
        }
        (_, TypedValues::UInt64(values))
            if data_type.storage_type() == DataType::UInt64
                && values.len() >= batch.row_count() =>
        {
            Some(KeyKind::Integer(data_type))
        }
        _ => None,
    }
}

/// Whether the batch's keys spread wider than an array indexed by the key
/// would be worth.
fn sparse_integer_key(batch: &RecordBatch, column: usize) -> bool {
    let Some((typed, validity)) = batch.column(column).and_then(crate::ColumnVector::typed) else {
        return false;
    };
    let mut bounds: Option<(i128, i128)> = None;
    let mut widen = |key: i128| {
        bounds = Some(bounds.map_or((key, key), |(low, high)| (low.min(key), high.max(key))));
    };
    for row in batch.selection().selected_rows() {
        if !validity.is_valid(row) {
            continue;
        }
        match typed {
            TypedValues::Int64(values) => widen(i128::from(values[row])),
            TypedValues::UInt64(values) => widen(i128::from(values[row])),
            _ => return false,
        }
    }
    bounds.is_some_and(|(low, high)| high - low >= SPARSE_KEY_SPAN)
}

/// The spellings of one text key: which collation class each belongs to,
/// and the spelling itself for the group that shows it.
struct TextIntern {
    collation: Collation,
    /// Class id by the collation's normalized text.
    classes: HashMap<String, u32>,
    /// `class << 32 | spelling` by the spelling as written.
    exact: HashMap<String, u64>,
    spellings: Vec<String>,
    reserved: usize,
}

impl TextIntern {
    fn new(collation: Collation) -> Self {
        Self {
            collation,
            classes: HashMap::new(),
            exact: HashMap::new(),
            spellings: Vec::new(),
            reserved: 0,
        }
    }

    /// `class << 32 | spelling` for `text`. Two spellings share a class
    /// exactly when the collation holds them equal.
    fn intern(&mut self, text: &str, memory: &MemoryTracker) -> Result<u64, ExecError> {
        if let Some(packed) = self.exact.get(text) {
            return Ok(*packed);
        }
        let folded = normalized_group_text(text, self.collation);
        let bytes = text
            .len()
            .saturating_mul(2)
            .saturating_add(folded.len())
            .saturating_add(HASH_ENTRY_OVERHEAD.saturating_mul(2))
            .saturating_add(size_of::<String>() * 3 + size_of::<u64>() * 2);
        memory.reserve(bytes)?;
        self.reserved = self.reserved.saturating_add(bytes);
        let next_class = u32::try_from(self.classes.len())
            .map_err(|_| ExecError::InvalidBatch("text group keys outgrew their ids"))?;
        let class = *self.classes.entry(folded).or_insert(next_class);
        let spelling = u32::try_from(self.spellings.len())
            .map_err(|_| ExecError::InvalidBatch("text group keys outgrew their ids"))?;
        let packed = (u64::from(class) << 32) | u64::from(spelling);
        self.exact.insert(text.to_owned(), packed);
        self.spellings.push(text.to_owned());
        Ok(packed)
    }

    /// What [`Self::intern`] answers for a spelling it has already met.
    fn known(&self, text: &str) -> Option<u64> {
        self.exact.get(text).copied()
    }
}

/// One text key column of a batch as interned ids.
enum TextCells {
    /// By dictionary code.
    Codes(Vec<u64>),
    /// By row, for a column that decoded without codes.
    Rows(Vec<u64>),
}

/// A batch waiting in the window, with its text keys interned.
struct Prepared {
    batch: RecordBatch,
    text: Vec<TextCells>,
    /// The ordinal of the batch's first row, for a batch a fused round
    /// left: its place in the input, not the place it was handed over in.
    ordinal: Option<u64>,
}

/// One morsel's rows cut by partition: `starts[p]..starts[p + 1]` are the
/// rows of partition `p`, in the order the scan produced them.
struct CutRows {
    words: Vec<u64>,
    starts: Vec<usize>,
}

#[inline]
fn key_hash(nulls: u64, cells: &[u64]) -> u64 {
    let mut hash = mix64(nulls);
    for cell in cells {
        hash = mix64(hash ^ *cell);
    }
    hash
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
const fn partition_of(hash: u64) -> usize {
    (hash >> (64 - PARTITION_BITS)) as usize
}

#[inline]
const fn slot_tag(hash: u64) -> u64 {
    (hash >> 16) & KEY_NULLS
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
const fn slot_start(hash: u64, mask: usize) -> usize {
    (hash as usize) & mask
}

#[inline]
const fn signed(bits: u64) -> i64 {
    i64::from_ne_bytes(bits.to_ne_bytes())
}

#[inline]
const fn unsigned(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

/// A sum's running total, kept in two words of the group.
#[inline]
fn total_of(cell: &[u64]) -> i128 {
    let wide = (u128::from(cell[1]) << 64) | u128::from(cell[0]);
    i128::from_ne_bytes(wide.to_ne_bytes())
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn set_total(cell: &mut [u64], total: i128) {
    let wide = u128::from_ne_bytes(total.to_ne_bytes());
    cell[0] = wide as u64;
    cell[1] = (wide >> 64) as u64;
}

/// Cuts one morsel's selected rows by partition.
#[allow(clippy::too_many_lines)]
fn cut_rows(
    plan: &PackedGroupPlan,
    prepared: &Prepared,
    rows: Range<usize>,
) -> Result<CutRows, ExecError> {
    const LOST: &str = "packed group key lost its packed projection";
    let batch = &prepared.batch;
    let stride = plan.cut_stride;
    let keys = plan.keys();
    let selected: Vec<usize> = batch.selection().selected_rows_in(rows).collect();
    let mut staged = vec![0_u64; selected.len() * stride];
    for (index, (column, kind)) in plan.columns.iter().zip(&plan.kinds).enumerate() {
        let (typed, validity) = batch
            .column(*column)
            .and_then(crate::ColumnVector::typed)
            .ok_or(ExecError::InvalidBatch(LOST))?;
        let at = 2 + index;
        let null = 1_u64 << index;
        macro_rules! fill {
            ($read:expr) => {{
                let read = $read;
                if validity.no_nulls() {
                    for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                        out[at] = read(*row);
                    }
                } else {
                    for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                        if validity.is_valid(*row) {
                            out[at] = read(*row);
                        } else {
                            out[1] |= null;
                        }
                    }
                }
            }};
        }
        match (kind, typed) {
            (KeyKind::Integer(_), TypedValues::Int64(values)) => {
                fill!(|row: usize| unsigned(values[row]));
            }
            (KeyKind::Integer(_), TypedValues::UInt64(values)) => fill!(|row: usize| values[row]),
            (KeyKind::Temporal(_), TypedValues::Temporal { units, .. }) => {
                fill!(|row: usize| unsigned(units[row]));
            }
            (
                KeyKind::Decimal { .. },
                TypedValues::Decimal128 {
                    values: DecimalUnits::Narrow(values),
                    ..
                },
            ) => fill!(|row: usize| unsigned(values[row])),
            (KeyKind::Text { slot }, TypedValues::Utf8(strings)) => {
                let spelling_at = 2 + keys + slot;
                let write = |out: &mut [u64], packed: u64| {
                    out[at] = packed >> 32;
                    out[spelling_at] = packed & KEY_NULLS;
                };
                match (&prepared.text[*slot], strings.dictionary()) {
                    (TextCells::Codes(ids), Some((codes, _))) => {
                        for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                            if validity.is_valid(*row) {
                                let code = usize::try_from(codes[*row])
                                    .map_err(|_| ExecError::InvalidBatch(LOST))?;
                                write(out, *ids.get(code).ok_or(ExecError::InvalidBatch(LOST))?);
                            } else {
                                out[1] |= null;
                            }
                        }
                    }
                    (TextCells::Rows(ids), _) => {
                        for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                            if validity.is_valid(*row) {
                                write(out, ids[*row]);
                            } else {
                                out[1] |= null;
                            }
                        }
                    }
                    (TextCells::Codes(_), None) => return Err(ExecError::InvalidBatch(LOST)),
                }
            }
            _ => return Err(ExecError::InvalidBatch(LOST)),
        }
    }
    let readers = lane_readers(batch, &plan.lanes);
    for (lane, reader) in readers.iter().enumerate() {
        let null = 1_u64 << (32 + lane);
        match plan.lane_words[lane] {
            Some(at) => {
                for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                    match reader.bits(*row) {
                        Some(bits) => out[at] = bits,
                        None => out[1] |= null,
                    }
                }
            }
            None if matches!(plan.packed[lane], PackedLane::Present) => {
                for (out, row) in staged.chunks_exact_mut(stride).zip(&selected) {
                    if reader.bits(*row).is_none() {
                        out[1] |= null;
                    }
                }
            }
            None => {}
        }
    }
    let mut starts = vec![0_usize; PARTITIONS + 1];
    for out in staged.chunks_exact_mut(stride) {
        let hash = key_hash(out[1] & KEY_NULLS, &out[2..2 + keys]);
        out[0] = hash;
        starts[partition_of(hash) + 1] += 1;
    }
    let mut running = 0;
    for start in &mut starts[1..] {
        running += *start;
        *start = running;
    }
    let mut cursor = starts[..PARTITIONS].to_vec();
    let mut words = vec![0_u64; staged.len()];
    for row in staged.chunks_exact(stride) {
        let next = &mut cursor[partition_of(row[0])];
        words[*next * stride..(*next + 1) * stride].copy_from_slice(row);
        *next += 1;
    }
    Ok(CutRows { words, starts })
}

/// One partition's groups: an open-addressing index over rows of words,
/// each row a group's key and its totals.
#[derive(Default)]
struct Table {
    /// `tag << 32 | group + 1`, zero for an empty slot.
    slots: Vec<u64>,
    words: Vec<u64>,
    groups: usize,
    reserved: usize,
}

impl Table {
    /// Doubles the index and places every group again.
    fn grow_slots(
        &mut self,
        plan: &PackedGroupPlan,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let len = (self.slots.len() * 2).max(64);
        let bytes = (len - self.slots.len()) * size_of::<u64>();
        memory.reserve(bytes)?;
        self.reserved += bytes;
        let mut slots = vec![0_u64; len];
        let keys = plan.keys();
        for (group, row) in self.words.chunks_exact(plan.group_stride).enumerate() {
            let hash = key_hash(row[0], &row[1..=keys]);
            let mut at = slot_start(hash, len - 1);
            while slots[at] != 0 {
                at = (at + 1) & (len - 1);
            }
            slots[at] = (slot_tag(hash) << 32) | (group as u64 + 1);
        }
        self.slots = slots;
        Ok(())
    }

    /// Makes room for one more group's words, growing by half.
    fn reserve_group(&mut self, stride: usize, memory: &MemoryTracker) -> Result<(), ExecError> {
        let capacity = self.words.capacity();
        if self.words.len() + stride <= capacity {
            return Ok(());
        }
        let wanted = (capacity + capacity / 2).max(64 * stride);
        let bytes = (wanted - capacity) * size_of::<u64>();
        memory.reserve(bytes)?;
        self.reserved += bytes;
        self.words.reserve_exact(wanted - self.words.len());
        Ok(())
    }

    /// Takes a whole group, key and totals, that no group here shares a
    /// key with.
    fn adopt(
        &mut self,
        plan: &PackedGroupPlan,
        group: &[u64],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if self.groups * 2 >= self.slots.len() {
            self.grow_slots(plan, memory)?;
        }
        if self.groups >= u32::MAX as usize - 1 {
            return Err(ExecError::InvalidBatch("a partition outgrew its group ids"));
        }
        self.reserve_group(plan.group_stride, memory)?;
        let hash = key_hash(group[0], &group[1..=plan.keys()]);
        let mask = self.slots.len() - 1;
        let mut at = slot_start(hash, mask);
        while self.slots[at] != 0 {
            at = (at + 1) & mask;
        }
        self.words.extend_from_slice(group);
        self.slots[at] = (slot_tag(hash) << 32) | (self.groups as u64 + 1);
        self.groups += 1;
        Ok(())
    }

    /// Folds one cut row into its group, creating the group if it is new.
    #[inline]
    fn fold(
        &mut self,
        plan: &PackedGroupPlan,
        row: &[u64],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let stride = plan.group_stride;
        let keys = plan.keys();
        let texts = plan.texts();
        if self.groups * 2 >= self.slots.len() {
            self.grow_slots(plan, memory)?;
        }
        let hash = row[0];
        let nulls = row[1] & KEY_NULLS;
        let cells = &row[2..2 + keys];
        let tag = slot_tag(hash);
        let mask = self.slots.len() - 1;
        let mut at = slot_start(hash, mask);
        let group = loop {
            let slot = self.slots[at];
            if slot == 0 {
                let group = self.groups;
                if group >= u32::MAX as usize - 1 {
                    return Err(ExecError::InvalidBatch("a partition outgrew its group ids"));
                }
                self.reserve_group(stride, memory)?;
                self.words.push(nulls);
                self.words.extend_from_slice(&row[2..2 + keys + texts]);
                self.words.resize(self.words.len() + plan.cell_words, 0);
                self.slots[at] = (tag << 32) | (group as u64 + 1);
                self.groups += 1;
                break group;
            }
            if slot >> 32 == tag {
                let group = slot_start(slot & KEY_NULLS, usize::MAX) - 1;
                let held = &self.words[group * stride..group * stride + 1 + keys];
                if held[0] == nulls && held[1..] == *cells {
                    break group;
                }
            }
            at = (at + 1) & mask;
        };
        let base = group * stride + 1 + keys + texts;
        let totals = &mut self.words[base..base + plan.cell_words];
        let lane_nulls = row[1] >> 32;
        for (lane, packed) in plan.packed.iter().enumerate() {
            let cell = &mut totals[plan.cell_offsets[lane]..];
            let null = lane_nulls & (1 << lane) != 0;
            match packed {
                PackedLane::Count => cell[0] += 1,
                PackedLane::Present => cell[0] += u64::from(!null),
                PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
                    if !null {
                        let units = i128::from(signed(row[plan.lane_words[lane].unwrap_or(0)]));
                        let total = total_of(cell)
                            .checked_add(units)
                            .ok_or(ExecError::NumericOverflow)?;
                        set_total(cell, total);
                        cell[2] += 1;
                    }
                }
                PackedLane::Minimum { .. } => {
                    if !null {
                        let bits = row[plan.lane_words[lane].unwrap_or(0)];
                        if cell[1] == 0 || signed(bits) < signed(cell[0]) {
                            cell[0] = bits;
                        }
                        cell[1] += 1;
                    }
                }
                PackedLane::Maximum { .. } => {
                    if !null {
                        let bits = row[plan.lane_words[lane].unwrap_or(0)];
                        if cell[1] == 0 || signed(bits) > signed(cell[0]) {
                            cell[0] = bits;
                        }
                        cell[1] += 1;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The groups of text keys with few classes, indexed by class instead of
/// hashed: slot `sum(share(key) * weight(key))`, a key's share its class id
/// plus one, or zero for NULL.
struct DenseGroups {
    /// Slots along each key: the classes met so far and one for NULL.
    dims: Vec<usize>,
    /// A slot's words: the ordinal of its first row plus one (zero while
    /// the slot is empty), then the group as a partition's table holds it.
    words: Vec<u64>,
    /// Rows folded so far, which numbers the next window's rows.
    rows: u64,
    reserved: usize,
}

/// The slots `dims` multiply to, when they fit the dense table.
fn dense_slots(dims: &[usize]) -> Option<usize> {
    dims.iter()
        .try_fold(1_usize, |slots, dim| slots.checked_mul(*dim))
        .filter(|slots| *slots <= DENSE_SLOTS)
}

/// The weight of each key's share in a slot's index, last key fastest.
fn dense_weights(dims: &[usize]) -> Vec<usize> {
    let mut weights = vec![1_usize; dims.len()];
    for index in (0..dims.len().saturating_sub(1)).rev() {
        weights[index] = weights[index + 1] * dims[index + 1];
    }
    weights
}

/// The rows of a morsel a fold reads: all of them, where the loops run over
/// the columns' own slices, or the ones the selection keeps.
enum Picked {
    Span(Range<usize>),
    Rows(Vec<usize>),
}

impl Picked {
    fn len(&self) -> usize {
        match self {
            Self::Span(span) => span.len(),
            Self::Rows(rows) => rows.len(),
        }
    }

    #[inline]
    fn row(&self, index: usize) -> usize {
        match self {
            Self::Span(span) => span.start + index,
            Self::Rows(rows) => rows[index],
        }
    }
}

/// Folds one morsel's selected rows into `words`, a dense table laid out
/// by `dims`. `first_ordinal` is the ordinal of the batch's row zero.
///
/// A pass per key adds its share to each row's slot; a pass counts the
/// rows of each slot; a pass per aggregate adds into totals held by slot
/// alone. Only then are the table's words touched, once per group met.
#[allow(clippy::too_many_lines)]
fn fold_dense_rows(
    plan: &PackedGroupPlan,
    prepared: &Prepared,
    rows: Range<usize>,
    first_ordinal: u64,
    dims: &[usize],
    words: &mut [u64],
) -> Result<(), ExecError> {
    const LOST: &str = "packed group key lost its dictionary codes";
    /// A share no slot reaches: a dictionary code no row was interned for.
    const NO_SHARE: u32 = u32::MAX;
    let batch = &prepared.batch;
    let selection = batch.selection();
    let picked = if selection.count_in(rows.clone()) == rows.len() {
        Picked::Span(rows)
    } else {
        Picked::Rows(selection.selected_rows_in(rows).collect())
    };
    let keys = plan.keys();
    let stride = 1 + plan.group_stride;
    let table_slots = words.len() / stride;
    let weights = dense_weights(dims);
    let mut slots: Vec<u32> = Vec::new();
    let mut columns = Vec::with_capacity(keys);
    for (index, column) in plan.columns.iter().enumerate() {
        let Some((TypedValues::Utf8(strings), validity)) =
            batch.column(*column).and_then(crate::ColumnVector::typed)
        else {
            return Err(ExecError::InvalidBatch(LOST));
        };
        let (Some((codes, _)), TextCells::Codes(ids)) =
            (strings.dictionary(), &prepared.text[index])
        else {
            return Err(ExecError::InvalidBatch(LOST));
        };
        let shares = ids
            .iter()
            .map(|packed| {
                if *packed == u64::MAX {
                    NO_SHARE
                } else {
                    u32::try_from((slot_start(packed >> 32, usize::MAX) + 1) * weights[index])
                        .unwrap_or(NO_SHARE)
                }
            })
            .collect::<Vec<_>>();
        let share = |code: u32| shares.get(code as usize).copied().unwrap_or(NO_SHARE);
        match &picked {
            Picked::Span(span) if validity.no_nulls() => {
                let codes = codes
                    .get(span.clone())
                    .ok_or(ExecError::InvalidBatch(LOST))?;
                if index == 0 {
                    slots.extend(codes.iter().map(|code| share(*code)));
                } else {
                    for (slot, code) in slots.iter_mut().zip(codes) {
                        *slot = slot.saturating_add(share(*code));
                    }
                }
            }
            _ => {
                if index == 0 {
                    slots.resize(picked.len(), 0);
                }
                for (at, slot) in slots.iter_mut().enumerate() {
                    let row = picked.row(at);
                    if validity.is_valid(row) {
                        *slot = slot.saturating_add(share(codes[row]));
                    }
                }
            }
        }
        columns.push((codes, validity, ids));
    }
    // Rows by slot; a slot past the table is a code that was never interned.
    let mut hits = vec![0_u64; table_slots];
    for slot in &slots {
        let Some(hit) = hits.get_mut(*slot as usize) else {
            return Err(ExecError::InvalidBatch(LOST));
        };
        *hit += 1;
    }
    // A group shows the key and spellings of its earliest row. Rows reach
    // this morsel in scan order, so a slot's first row here is its earliest
    // here, and it gives the group its spellings when the slot is empty or
    // holds a later row: a worker's table that folded a later batch of the
    // scan first. A slot whose row is earlier than this morsel's first is
    // not looked at, so a fold in scan order searches only for new groups,
    // and the search ends once every slot that may move has met its row.
    let earliest = if slots.is_empty() {
        u64::MAX
    } else {
        first_ordinal + picked.row(0) as u64 + 1
    };
    let movable = |ordinal: u64| ordinal == 0 || ordinal > earliest;
    let mut unplaced = hits
        .iter()
        .zip(words.chunks_exact(stride))
        .filter(|(hit, group)| **hit > 0 && movable(group[0]))
        .count();
    let mut met = vec![false; if unplaced == 0 { 0 } else { table_slots }];
    for (at, slot) in slots.iter().enumerate() {
        if unplaced == 0 {
            break;
        }
        let from = *slot as usize * stride;
        let group = &mut words[from..from + stride];
        if !movable(group[0]) || std::mem::replace(&mut met[*slot as usize], true) {
            continue;
        }
        unplaced -= 1;
        let row = picked.row(at);
        let ordinal = first_ordinal + row as u64 + 1;
        if group[0] != 0 && group[0] < ordinal {
            continue;
        }
        group[0] = ordinal;
        for (index, (codes, validity, ids)) in columns.iter().enumerate() {
            if validity.is_valid(row) {
                let interned = ids[codes[row] as usize];
                group[2 + index] = interned >> 32;
                group[2 + keys + index] = interned & KEY_NULLS;
            } else {
                group[1] |= 1 << index;
            }
        }
    }
    let totals_at = 2 + keys + plan.texts();
    let readers = lane_readers(batch, &plan.lanes);
    for (lane, (packed, reader)) in plan.packed.iter().zip(&readers).enumerate() {
        let at = totals_at + plan.cell_offsets[lane];
        match packed {
            PackedLane::Count => {
                for (group, hit) in words.chunks_exact_mut(stride).zip(&hits) {
                    group[at] += *hit;
                }
            }
            PackedLane::Present => {
                let mut present = vec![0_u64; table_slots];
                for (index, slot) in slots.iter().enumerate() {
                    present[*slot as usize] += u64::from(reader.bits(picked.row(index)).is_some());
                }
                for (group, present) in words.chunks_exact_mut(stride).zip(&present) {
                    group[at] += *present;
                }
            }
            PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
                // A morsel's units cannot overflow the wide total they are
                // gathered in; the group's own total is checked below.
                let mut sums = vec![0_i128; table_slots];
                // The packed values of the morsel's rows, when it is every
                // row and none is NULL.
                let span: Option<&[i64]> = match (&picked, reader) {
                    (
                        Picked::Span(span),
                        LaneReader::Units(DecimalUnits::Narrow(values), validity),
                    ) if validity.no_nulls() => values.get(span.clone()),
                    (Picked::Span(span), LaneReader::Int64(values, validity))
                        if validity.no_nulls() =>
                    {
                        values.get(span.clone())
                    }
                    _ => None,
                };
                let counted = if let Some(values) = span {
                    for (slot, value) in slots.iter().zip(values) {
                        sums[*slot as usize] += i128::from(*value);
                    }
                    None
                } else {
                    let mut counted = vec![0_u64; table_slots];
                    for (index, slot) in slots.iter().enumerate() {
                        if let Some(bits) = reader.bits(picked.row(index)) {
                            sums[*slot as usize] += i128::from(signed(bits));
                            counted[*slot as usize] += 1;
                        }
                    }
                    Some(counted)
                };
                let valued = counted.as_ref().unwrap_or(&hits);
                for ((group, sum), rows) in words.chunks_exact_mut(stride).zip(&sums).zip(valued) {
                    if *rows > 0 {
                        let cell = &mut group[at..at + 3];
                        let total = total_of(cell)
                            .checked_add(*sum)
                            .ok_or(ExecError::NumericOverflow)?;
                        set_total(cell, total);
                        cell[2] += *rows;
                    }
                }
            }
            PackedLane::Minimum { .. } | PackedLane::Maximum { .. } => {
                let least = matches!(packed, PackedLane::Minimum { .. });
                for (index, slot) in slots.iter().enumerate() {
                    if let Some(bits) = reader.bits(picked.row(index)) {
                        let cell = &mut words[*slot as usize * stride + at..];
                        let better = if least {
                            signed(bits) < signed(cell[0])
                        } else {
                            signed(bits) > signed(cell[0])
                        };
                        if cell[1] == 0 || better {
                            cell[0] = bits;
                        }
                        cell[1] += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Merges one dense slot into another holding the same group: the totals
/// together, and the key and spellings of whichever saw its first row
/// earlier. An empty `into` takes `from` whole.
fn merge_dense_slot(
    plan: &PackedGroupPlan,
    into: &mut [u64],
    from: &[u64],
) -> Result<(), ExecError> {
    if from[0] == 0 {
        return Ok(());
    }
    if into[0] == 0 {
        into.copy_from_slice(from);
        return Ok(());
    }
    let totals_at = 2 + plan.keys() + plan.texts();
    if from[0] < into[0] {
        into[..totals_at].copy_from_slice(&from[..totals_at]);
    }
    for (lane, packed) in plan.packed.iter().enumerate() {
        let at = totals_at + plan.cell_offsets[lane];
        let (cell, other) = (&mut into[at..], &from[at..]);
        match packed {
            PackedLane::Count | PackedLane::Present => cell[0] += other[0],
            PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
                let total = total_of(cell)
                    .checked_add(total_of(other))
                    .ok_or(ExecError::NumericOverflow)?;
                set_total(cell, total);
                cell[2] += other[2];
            }
            PackedLane::Minimum { .. } => {
                if other[1] > 0 && (cell[1] == 0 || signed(other[0]) < signed(cell[0])) {
                    cell[0] = other[0];
                }
                cell[1] += other[1];
            }
            PackedLane::Maximum { .. } => {
                if other[1] > 0 && (cell[1] == 0 || signed(other[0]) > signed(cell[0])) {
                    cell[0] = other[0];
                }
                cell[1] += other[1];
            }
        }
    }
    Ok(())
}

/// Merges two dense tables of one layout; an empty vector is no table.
fn merge_dense(
    plan: &PackedGroupPlan,
    mut into: Vec<u64>,
    from: Vec<u64>,
) -> Result<Vec<u64>, ExecError> {
    if into.is_empty() {
        return Ok(from);
    }
    let stride = 1 + plan.group_stride;
    for (into, from) in into.chunks_exact_mut(stride).zip(from.chunks_exact(stride)) {
        merge_dense_slot(plan, into, from)?;
    }
    Ok(into)
}

/// A group's totals for one lane, as the cell every packed fold commits.
fn packed_cell(lane: PackedLane, cell: &[u64]) -> PackedCell {
    match lane {
        PackedLane::Count | PackedLane::Present => PackedCell {
            total: 0,
            rows: cell[0],
        },
        PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
            PackedCell {
                total: total_of(cell),
                rows: cell[2],
            }
        }
        PackedLane::Minimum { .. } | PackedLane::Maximum { .. } => PackedCell {
            total: i128::from(signed(cell[0])),
            rows: cell[1],
        },
    }
}

type Labels = Option<(Arc<Vec<String>>, bool)>;
type Members = Option<Arc<Vec<String>>>;

/// What turns a group's key cells back into the values the query answers.
struct KeyOutput<'a> {
    plan: &'a PackedGroupPlan,
    interns: &'a [TextIntern],
    labels: &'a [Labels],
    members: &'a [Members],
}

impl KeyOutput<'_> {
    /// One key column of a group as a value.
    fn value(&self, index: usize, group: &[u64]) -> Value {
        if group[0] & (1 << index) != 0 {
            return Value::Null;
        }
        let cell = group[1 + index];
        match self.plan.kinds[index] {
            KeyKind::Integer(data_type) | KeyKind::Temporal(data_type) => {
                two_pass_key_value(cell, false, data_type)
            }
            KeyKind::Decimal { scale } => Value::Utf8(pintail_types::format_decimal_scaled(
                i128::from(signed(cell)),
                scale,
            )),
            KeyKind::Text { slot } => {
                let spelling = slot_start(group[1 + self.plan.keys() + slot], usize::MAX);
                labelled_text_value(
                    &self.interns[slot].spellings[spelling],
                    self.labels[slot].as_ref(),
                    self.members[slot].as_ref(),
                )
            }
        }
    }
}

/// Merges `folded`, a dense table of `len` words in the layout of the
/// table's present dimensions, into what earlier folds left.
fn absorb_dense(
    plan: &PackedGroupPlan,
    dense: &mut DenseGroups,
    folded: &[u64],
    len: usize,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let stride = 1 + plan.group_stride;
    // Earlier windows' groups, laid out again if a key met new classes.
    if dense.words.len() != len {
        let bytes = len * size_of::<u64>();
        memory.reserve(bytes)?;
        let weights = dense_weights(&dense.dims);
        let mut words = vec![0_u64; len];
        for group in dense
            .words
            .chunks_exact(stride)
            .filter(|group| group[0] != 0)
        {
            let slot: usize = (0..plan.keys())
                .map(|index| {
                    if group[1] & (1 << index) == 0 {
                        (slot_start(group[2 + index], usize::MAX) + 1) * weights[index]
                    } else {
                        0
                    }
                })
                .sum();
            words[slot * stride..(slot + 1) * stride].copy_from_slice(group);
        }
        memory.release(dense.reserved);
        dense.reserved = bytes;
        dense.words = words;
    }
    for (into, from) in dense
        .words
        .chunks_exact_mut(stride)
        .zip(folded.chunks_exact(stride))
    {
        merge_dense_slot(plan, into, from)?;
    }
    Ok(())
}

/// Notes what the text key columns of `batch` declare into `labels` and
/// `members`. What several batches declare is joined whatever order they
/// are noted in.
fn note_text_declarations(
    plan: &PackedGroupPlan,
    labels: &mut [Labels],
    members: &mut [Members],
    batch: &RecordBatch,
) {
    for (column, kind) in plan.columns.iter().zip(&plan.kinds) {
        let KeyKind::Text { slot } = *kind else {
            continue;
        };
        let settled = members[slot].is_some()
            || labels[slot]
                .as_ref()
                .is_some_and(|(_, exhaustive)| *exhaustive);
        if settled {
            continue;
        }
        let Some((TypedValues::Utf8(strings), _)) =
            batch.column(*column).and_then(crate::ColumnVector::typed)
        else {
            continue;
        };
        let exhaustive = strings.enum_labels_exhaustive();
        labels[slot] = match (labels[slot].take(), strings.declared_enum_labels()) {
            (Some((held, _)), Some(seen)) if !exhaustive => {
                Some((merge_partial_labels(held, seen), false))
            }
            (_, Some(seen)) => Some((Arc::clone(seen), exhaustive)),
            (held, None) => held,
        };
        members[slot] = strings.declared_set_members().cloned();
    }
}

/// The batch's text keys as [`PackedGroupFold::intern_text`] would answer them,
/// when that would add nothing to the intern tables; `None` otherwise,
/// and for a column that decoded without dictionary codes.
fn known_text(
    plan: &PackedGroupPlan,
    interns: &[TextIntern],
    batch: &RecordBatch,
) -> Result<Option<Vec<TextCells>>, ExecError> {
    let mut text = Vec::with_capacity(plan.texts());
    for (column, kind) in plan.columns.iter().zip(&plan.kinds) {
        let KeyKind::Text { slot } = *kind else {
            continue;
        };
        let Some((TypedValues::Utf8(strings), validity)) =
            batch.column(*column).and_then(crate::ColumnVector::typed)
        else {
            return Err(ExecError::InvalidBatch(
                "text group key lost its typed projection",
            ));
        };
        let Some((codes, entries)) = strings.dictionary() else {
            return Ok(None);
        };
        let intern = &interns[slot];
        let mut ids = vec![u64::MAX; entries.len()];
        if entries.len() <= WHOLE_DICTIONARY {
            for (id, entry) in ids.iter_mut().zip(entries.iter()) {
                let Some(known) = intern.known(entry) else {
                    return Ok(None);
                };
                *id = known;
            }
        } else {
            for row in batch.selection().selected_rows() {
                if !validity.is_valid(row) {
                    continue;
                }
                let code = usize::try_from(codes[row])
                    .map_err(|_| ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                let Some(id) = ids.get_mut(code) else {
                    return Err(ExecError::InvalidBatch("dictionary code is out of bounds"));
                };
                if *id == u64::MAX {
                    let Some(known) = intern.known(&entries[code]) else {
                        return Ok(None);
                    };
                    *id = known;
                }
            }
        }
        text.push(TextCells::Codes(ids));
    }
    Ok(Some(text))
}

/// Everything the fold holds between windows.
struct PackedGroupFold<'a> {
    plan: &'a PackedGroupPlan,
    aggregates: &'a [CompiledAggregate],
    key_collations: &'a [Collation],
    memory: &'a MemoryTracker,
    tables: Vec<Table>,
    interns: Vec<TextIntern>,
    /// Declared ENUM labels and SET members per text key, from the batches
    /// that carry them.
    labels: Vec<Labels>,
    members: Vec<Members>,
    window: Vec<Prepared>,
    window_rows: usize,
    window_reserved: usize,
    /// Groups of the batches no cell could carry, by normalized key.
    by_row: HashMap<Vec<Value>, AggregateGroup>,
    by_row_reserved: usize,
    by_row_batches: usize,
    spill_runs: Vec<spill::ClosedRun>,
    /// The dense table, while every batch has kept to it.
    dense: Option<DenseGroups>,
    /// Rows the dense table folded, for the profile.
    dense_rows: u64,
    /// The first place in the input no fused round has handed over: the
    /// next call's rounds number their batches from it, after every row
    /// numbered so far.
    fused_floor: u64,
    /// The groups the dense table moved to the hashed tables after fused
    /// rounds, by key: the ordinal of each one's first row and where it
    /// went. A batch a round left may hold an earlier row of one of them.
    poured: HashMap<Vec<u64>, PouredGroup>,
}

/// Where a group the dense table moved to the hashed tables went, and the
/// ordinal of its first row plus one.
struct PouredGroup {
    ordinal: u64,
    partition: usize,
    group: usize,
}

/// Gives each group the dense table moved to the hashed tables the
/// spellings of `held`'s rows that come before its first row. `held` is a
/// batch a fused round left, numbered from `first_ordinal`; the rows of
/// such batches reach the hashed tables in order, but after the rounds
/// folded later rows into the groups that moved.
fn claim_poured(
    plan: &PackedGroupPlan,
    poured: &mut HashMap<Vec<u64>, PouredGroup>,
    tables: &mut [Table],
    held: &Prepared,
    first_ordinal: u64,
) -> Result<(), ExecError> {
    const LOST: &str = "packed group key lost its text cells";
    let keys = plan.keys();
    let mut columns = Vec::with_capacity(keys);
    for (index, column) in plan.columns.iter().enumerate() {
        let Some((TypedValues::Utf8(strings), validity)) = held
            .batch
            .column(*column)
            .and_then(crate::ColumnVector::typed)
        else {
            return Err(ExecError::InvalidBatch(LOST));
        };
        columns.push((
            strings.dictionary().map(|(codes, _)| codes),
            validity,
            &held.text[index],
        ));
    }
    let mut key = vec![0_u64; 1 + keys];
    let mut spellings = vec![0_u64; keys];
    for row in held.batch.selection().selected_rows() {
        key.fill(0);
        for (index, (codes, validity, cells)) in columns.iter().enumerate() {
            if !validity.is_valid(row) {
                key[0] |= 1 << index;
                continue;
            }
            let packed = match (cells, codes) {
                (TextCells::Codes(ids), Some(codes)) => ids.get(codes[row] as usize).copied(),
                (TextCells::Rows(ids), _) => ids.get(row).copied(),
                (TextCells::Codes(_), None) => None,
            }
            .ok_or(ExecError::InvalidBatch(LOST))?;
            key[1 + index] = packed >> 32;
            spellings[index] = packed & KEY_NULLS;
        }
        let ordinal = first_ordinal + row as u64 + 1;
        let Some(moved) = poured.get_mut(&key) else {
            continue;
        };
        if moved.ordinal <= ordinal {
            continue;
        }
        moved.ordinal = ordinal;
        let at = moved.group * plan.group_stride + 1 + keys;
        let words = &mut tables[moved.partition].words;
        for (index, spelling) in spellings.iter().enumerate() {
            if key[0] & (1 << index) == 0 {
                words[at + index] = *spelling;
            }
        }
    }
    Ok(())
}

/// A fused round numbers a batch's rows from its place in the input: the
/// place, then the row within the batch, whose rows fit these bits.
const FUSED_ROW_BITS: u32 = 18;

/// What a call to [`PackedGroupFold::fold_in_place`] left.
enum InPlace {
    /// The input does not fold in place, or the dense table cannot take
    /// it: the windows take the input from here on.
    Off,
    /// The input is exhausted.
    Done,
    /// Batches for the windows, each with the ordinal of its first row,
    /// and whether the round that left them took at least as many.
    Left(Vec<(u64, RecordBatch)>, bool),
    /// Nothing left and more to read: the query is past half its ceiling.
    Paused,
}

impl PackedGroupFold<'_> {
    /// Folds the input into the dense table in fused rounds: each worker
    /// decodes a slice of the table and folds its rows into a copy of the
    /// dense table it keeps for the rounds' length, and the copies merge
    /// into the table when the rounds stop.
    ///
    /// Rows are numbered by their batch's place in the input, so a group
    /// still shows the key spellings of its first row in input order. A
    /// round adds nothing to the intern tables: a batch that brings a
    /// spelling, one no cell can carry and one the ceiling had no room for
    /// are left for the windows, with their ordinals.
    #[allow(clippy::too_many_lines)]
    fn fold_in_place(&mut self, input: &mut PullOperator) -> Result<InPlace, ExecError> {
        let plan = self.plan;
        let memory = self.memory;
        let Some(dense) = self.dense.as_ref() else {
            return Ok(InPlace::Off);
        };
        let dims = self
            .interns
            .iter()
            .map(|intern| intern.classes.len() + 1)
            .collect::<Vec<_>>();
        let Some(slots) = dense_slots(&dims) else {
            return Ok(InPlace::Off);
        };
        let stride = 1 + plan.group_stride;
        let len = slots * stride;
        let workers = rayon::current_num_threads().max(1);
        // A copy of the table per pool thread and one for a caller outside
        // the pool.
        let bytes = (workers + 1) * len * size_of::<u64>();
        if memory.reserve(bytes).is_err() {
            return Ok(InPlace::Off);
        }
        // Rows the windows numbered - before the rounds, between them while
        // the query was past half its ceiling, and the batches the last
        // call's rounds left - all come before this call's rounds' rows.
        let base = dense.rows;
        let floor = self.fused_floor;
        let next_floor = std::sync::atomic::AtomicU64::new(floor);
        let mut seats: Vec<std::sync::Mutex<Vec<u64>>> = Vec::new();
        seats.resize_with(workers + 1, || std::sync::Mutex::new(Vec::new()));
        let poisoned = || ExecError::InvalidBatch("dense group seat poisoned");
        let left = std::sync::Mutex::new(Vec::<(u64, RecordBatch)>::new());
        let declared = std::sync::Mutex::new((
            std::mem::take(&mut self.labels),
            std::mem::take(&mut self.members),
        ));
        let folded_rows = std::sync::atomic::AtomicU64::new(0);
        let last_ordinal = std::sync::atomic::AtomicU64::new(0);
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        let ordinal_of = |order: u64| {
            let since = order.checked_sub(floor)?;
            (since.leading_zeros() > FUSED_ROW_BITS)
                .then(|| base.checked_add(since << FUSED_ROW_BITS))
                .flatten()
        };
        let interns = self.interns.as_slice();
        let mut outcome = Ok(InPlace::Paused);
        while memory.used() <= memory.limit() / 2 {
            let taken = std::sync::atomic::AtomicUsize::new(0);
            let round = input.fold_round(memory, usize::MAX, &|batch, order| {
                next_floor.fetch_max(order.saturating_add(1), relaxed);
                let leave = |batch: RecordBatch| {
                    left.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((order, batch));
                    Ok(None)
                };
                let Some(first_ordinal) = ordinal_of(order) else {
                    return Ok(Some(batch));
                };
                if batch.row_count() >= 1 << FUSED_ROW_BITS || !plan.carries(&batch) {
                    return leave(batch);
                }
                let Some(text) = known_text(plan, interns, &batch)? else {
                    return leave(batch);
                };
                {
                    let mut declared = declared.lock().map_err(|_| poisoned())?;
                    let (labels, members) = &mut *declared;
                    note_text_declarations(plan, labels, members, &batch);
                }
                let prepared = Prepared {
                    batch,
                    text,
                    ordinal: Some(first_ordinal),
                };
                let seat =
                    rayon::current_thread_index().map_or(workers, |index| index.min(workers));
                let mut words = seats[seat].lock().map_err(|_| poisoned())?;
                if words.is_empty() {
                    *words = vec![0_u64; len];
                }
                let rows = prepared.batch.row_count();
                fold_dense_rows(plan, &prepared, 0..rows, first_ordinal, &dims, &mut words)?;
                taken.fetch_add(1, relaxed);
                folded_rows.fetch_add(prepared.batch.visible_row_count() as u64, relaxed);
                last_ordinal.fetch_max(first_ordinal.saturating_add(rows as u64), relaxed);
                Ok(None)
            });
            let returned = match round {
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
                Ok(super::FoldedRound::Unavailable) => {
                    outcome = Ok(InPlace::Off);
                    break;
                }
                Ok(super::FoldedRound::Done) => {
                    outcome = Ok(InPlace::Done);
                    break;
                }
                Ok(super::FoldedRound::Round { returned }) => returned,
            };
            let taken = taken.into_inner();
            crate::counters::count(|counters| {
                counters.fused_rounds += 1;
                counters.fused_batches += taken as u64;
            });
            let mut left = left
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (order, _) in &returned {
                next_floor.fetch_max(order.saturating_add(1), relaxed);
            }
            left.extend(returned);
            if !left.is_empty() {
                let mut left = std::mem::take(&mut *left);
                left.sort_by_key(|(order, _)| *order);
                // A place past what an ordinal can number ends the rounds:
                // the windows number their rows after every row so far.
                let numbered = left.iter().all(|(order, _)| ordinal_of(*order).is_some());
                let worthwhile = numbered && left.len() <= taken;
                outcome = Ok(InPlace::Left(
                    left.into_iter()
                        .map(|(order, batch)| {
                            let ordinal = if numbered { ordinal_of(order) } else { None };
                            (ordinal.unwrap_or(u64::MAX), batch)
                        })
                        .collect(),
                    worthwhile,
                ));
                break;
            }
        }
        let (labels, members) = declared
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.labels = labels;
        self.members = members;
        let merged: Result<(), ExecError> = (|| {
            let Some(dense) = self.dense.as_mut() else {
                return Ok(());
            };
            dense.dims.clone_from(&dims);
            for seat in seats {
                let words = seat
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !words.is_empty() {
                    absorb_dense(plan, dense, &words, len, memory)?;
                }
            }
            // The windows number their rows after every row folded here.
            dense.rows = dense.rows.max(last_ordinal.into_inner());
            Ok(())
        })();
        self.dense_rows += folded_rows.into_inner();
        self.fused_floor = next_floor.into_inner();
        memory.release(bytes);
        merged?;
        outcome
    }

    /// Folds the window into the dense table: every worker into a copy of
    /// its own, the copies merged, and the result merged into what earlier
    /// windows left.
    fn flush_dense(&mut self) -> Result<(), ExecError> {
        self.relieve()?;
        let Some(dense) = self.dense.as_mut() else {
            return Ok(());
        };
        let plan = self.plan;
        let stride = 1 + plan.group_stride;
        let slots = dense_slots(&dense.dims).ok_or(ExecError::InvalidBatch(
            "the dense group table outgrew its slots",
        ))?;
        let len = slots * stride;
        let window = &self.window;
        // A batch a fused round left keeps its own ordinal, and the rows
        // after it - in this window and the next - are numbered past it.
        let mut first_ordinals = Vec::with_capacity(window.len());
        let mut ordinal = dense.rows;
        let mut rows = 0_u64;
        for held in window {
            let first = held.ordinal.unwrap_or(ordinal);
            first_ordinals.push(first);
            let count = held.batch.row_count() as u64;
            ordinal = ordinal.max(first.saturating_add(count));
            rows += count;
        }
        let pieces = morsel_plan(
            window.iter().map(|held| held.batch.row_count()),
            default_morsel_limit(),
        );
        let dims = dense.dims.as_slice();
        let folded = pieces
            .par_iter()
            .try_fold(Vec::new, |mut words: Vec<u64>, (index, rows)| {
                if words.is_empty() {
                    words = vec![0_u64; len];
                }
                fold_dense_rows(
                    plan,
                    &window[*index],
                    rows.clone(),
                    first_ordinals[*index],
                    dims,
                    &mut words,
                )?;
                Ok::<_, ExecError>(words)
            })
            .try_reduce(Vec::new, |into, from| merge_dense(plan, into, from))?;
        absorb_dense(plan, dense, &folded, len, self.memory)?;
        self.dense_rows += rows;
        dense.rows = ordinal;
        Ok(())
    }

    /// Moves the dense table's groups into the partitions' tables, where
    /// every other step of the fold expects to find a group.
    fn pour_dense(&mut self) -> Result<(), ExecError> {
        let Some(dense) = self.dense.as_mut() else {
            return Ok(());
        };
        let plan = self.plan;
        let words = std::mem::take(&mut dense.words);
        self.memory.release(dense.reserved);
        dense.reserved = 0;
        // After fused rounds, batches they left may still come, each with
        // rows earlier than ones the rounds folded into these groups.
        let remember = self.fused_floor > 0;
        for slot in words.chunks_exact(1 + plan.group_stride) {
            if slot[0] == 0 {
                continue;
            }
            let group = &slot[1..];
            let hash = key_hash(group[0], &group[1..=plan.keys()]);
            let partition = partition_of(hash);
            let table = &mut self.tables[partition];
            table.adopt(plan, group, self.memory)?;
            if remember {
                self.poured.insert(
                    group[..=plan.keys()].to_vec(),
                    PouredGroup {
                        ordinal: slot[0],
                        partition,
                        group: table.groups - 1,
                    },
                );
            }
        }
        Ok(())
    }

    /// Whether the batch about to join the window keeps to the dense
    /// table: its text keys by dictionary code, and the classes met so far
    /// few enough. When it does not, what the table holds moves to the
    /// partitions' tables and the fold is the hashed one from here on.
    fn settle_dense(&mut self, text: &[TextCells]) -> Result<(), ExecError> {
        let Some(dense) = self.dense.as_mut() else {
            return Ok(());
        };
        let dims = self
            .interns
            .iter()
            .map(|intern| intern.classes.len() + 1)
            .collect::<Vec<_>>();
        let coded = text
            .iter()
            .all(|cells| matches!(cells, TextCells::Codes(_)));
        if coded && dense_slots(&dims).is_some() {
            dense.dims = dims;
            return Ok(());
        }
        self.flush()?;
        self.pour_dense()?;
        self.dense = None;
        Ok(())
    }

    /// Notes what the text key columns declare, as each batch arrives: a
    /// later batch may be the first to carry the whole declaration.
    fn note_declarations(&mut self, batch: &RecordBatch) {
        note_text_declarations(self.plan, &mut self.labels, &mut self.members, batch);
    }

    /// Interns the batch's text keys: once per dictionary entry, or once
    /// per row for a column that decoded without codes.
    fn intern_text(&mut self, batch: &RecordBatch) -> Result<Vec<TextCells>, ExecError> {
        let mut text = Vec::with_capacity(self.plan.texts());
        for (column, kind) in self.plan.columns.iter().zip(&self.plan.kinds) {
            let KeyKind::Text { slot } = *kind else {
                continue;
            };
            let Some((TypedValues::Utf8(strings), validity)) =
                batch.column(*column).and_then(crate::ColumnVector::typed)
            else {
                return Err(ExecError::InvalidBatch(
                    "text group key lost its typed projection",
                ));
            };
            let intern = &mut self.interns[slot];
            let cells = if let Some((codes, entries)) = strings.dictionary() {
                // Only the entries a selected row carries, as the rows meet
                // them: a dictionary also lists spellings no row of this
                // query holds. A short dictionary is interned whole instead:
                // walking every row to learn which of a handful of entries
                // it uses costs more than a few classes no row reaches.
                let mut ids = vec![u64::MAX; entries.len()];
                if entries.len() <= WHOLE_DICTIONARY {
                    for (id, entry) in ids.iter_mut().zip(entries.iter()) {
                        *id = intern.intern(entry, self.memory)?;
                    }
                } else {
                    for row in batch.selection().selected_rows() {
                        if !validity.is_valid(row) {
                            continue;
                        }
                        let code = usize::try_from(codes[row]).map_err(|_| {
                            ExecError::InvalidBatch("dictionary code is out of bounds")
                        })?;
                        // Not `ok_or`: the error would be built and dropped
                        // for every row that has none.
                        let Some(slot) = ids.get_mut(code) else {
                            return Err(ExecError::InvalidBatch(
                                "dictionary code is out of bounds",
                            ));
                        };
                        if *slot == u64::MAX {
                            *slot = intern.intern(&entries[code], self.memory)?;
                        }
                    }
                }
                TextCells::Codes(ids)
            } else {
                let (views, heap) = (strings.views(), strings.heap());
                let mut ids = vec![0_u64; batch.row_count()];
                for row in batch.selection().selected_rows() {
                    if validity.is_valid(row) {
                        ids[row] = views[row].with_bytes(heap, |bytes| {
                            let spelling = std::str::from_utf8(bytes).map_err(|_| {
                                ExecError::InvalidBatch("string group key is not UTF-8")
                            })?;
                            intern.intern(spelling, self.memory)
                        })?;
                    }
                }
                TextCells::Rows(ids)
            };
            text.push(cells);
        }
        Ok(text)
    }

    fn holds_groups(&self) -> bool {
        self.tables.iter().any(|table| table.groups > 0)
            || self.dense.as_ref().is_some_and(|dense| {
                dense
                    .words
                    .chunks_exact(1 + self.plan.group_stride)
                    .any(|slot| slot[0] != 0)
            })
    }

    fn key_output(&self) -> KeyOutput<'_> {
        KeyOutput {
            plan: self.plan,
            interns: &self.interns,
            labels: &self.labels,
            members: &self.members,
        }
    }

    /// Every group the tables hold, as the general path keys and holds
    /// them, leaving the tables empty: the form a spilled run is written
    /// in, and the one a group folded row by row merges with.
    fn drain_groups(&mut self) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
        self.pour_dense()?;
        let keys = self.plan.keys();
        let stride = self.plan.group_stride;
        let totals_at = 1 + keys + self.plan.texts();
        let mut groups = HashMap::with_capacity(self.tables.iter().map(|table| table.groups).sum());
        // The tables are drained, so the groups that moved have no place.
        self.poured = HashMap::new();
        let tables = std::mem::take(&mut self.tables);
        let output = self.key_output();
        for table in tables {
            for group in table.words.chunks_exact(stride) {
                let values = (0..keys)
                    .map(|index| output.value(index, group))
                    .collect::<Vec<_>>();
                let key = values
                    .iter()
                    .cloned()
                    .zip(self.key_collations)
                    .map(|(value, collation)| {
                        normalized_group_hash_key(value, *collation).unwrap_or(Value::Null)
                    })
                    .collect::<Vec<_>>();
                let mut states = Vec::with_capacity(self.aggregates.len());
                for (lane, aggregate) in self.aggregates.iter().enumerate() {
                    let mut state = AggregateState::new(aggregate);
                    let packed = self.plan.packed[lane];
                    packed_cell(packed, &group[totals_at + self.plan.cell_offsets[lane]..])
                        .commit(packed, &mut state, aggregate, self.memory)?;
                    states.push(state);
                }
                groups.insert(key, AggregateGroup { values, states });
            }
            self.memory.release(table.reserved);
        }
        self.tables = (0..PARTITIONS).map(|_| Table::default()).collect();
        Ok(groups)
    }

    /// Writes every group held as one sorted run and starts over with
    /// empty tables.
    fn spill(&mut self) -> Result<(), ExecError> {
        if !self.holds_groups() {
            return Ok(());
        }
        let mut groups = self.drain_groups()?;
        self.spill_runs
            .push(write_aggregate_spill_run(&mut groups, self.memory)?);
        Ok(())
    }

    /// Spills when the query is past half its ceiling, so the rows about to
    /// fold meet room for the groups they add.
    fn relieve(&mut self) -> Result<(), ExecError> {
        if self.memory.used() > self.memory.limit() / 2 {
            self.spill()?;
        }
        Ok(())
    }

    /// Folds one round of morsels: cut by partition on every worker, then
    /// one worker per partition into its table.
    fn fold_round(&mut self, pieces: &[(usize, Range<usize>)]) -> Result<(), ExecError> {
        self.relieve()?;
        let rows: usize = pieces
            .iter()
            .map(|(index, rows)| self.window[*index].batch.selection().count_in(rows.clone()))
            .sum();
        // The cut rows and the staging they are ordered from.
        let bytes = rows
            .saturating_mul(self.plan.cut_stride)
            .saturating_mul(2 * size_of::<u64>());
        if self.memory.reserve(bytes).is_err() {
            self.spill()?;
            self.memory.reserve(bytes)?;
        }
        let plan = self.plan;
        let window = &self.window;
        let memory = self.memory;
        let cut = pieces
            .par_iter()
            .map(|(index, rows)| cut_rows(plan, &window[*index], rows.clone()))
            .collect::<Result<Vec<_>, _>>();
        let folded = cut.and_then(|cut| {
            let stride = plan.cut_stride;
            self.tables
                .par_iter_mut()
                .enumerate()
                .try_for_each(|(partition, table)| {
                    for morsel in &cut {
                        let rows = &morsel.words[morsel.starts[partition] * stride
                            ..morsel.starts[partition + 1] * stride];
                        for row in rows.chunks_exact(stride) {
                            table.fold(plan, row, memory)?;
                        }
                    }
                    Ok(())
                })
        });
        self.memory.release(bytes);
        folded
    }

    /// Folds the window's batches and lets them go.
    fn flush(&mut self) -> Result<(), ExecError> {
        if self.window.is_empty() {
            return Ok(());
        }
        if self.dense.is_some() {
            self.flush_dense()?;
            // A spill on the way in may have ended nothing: the window is
            // folded either way.
            self.window.clear();
            self.window_rows = 0;
            self.memory.release(self.window_reserved);
            self.window_reserved = 0;
            return Ok(());
        }
        if !self.poured.is_empty() {
            for held in &self.window {
                if let Some(first_ordinal) = held.ordinal {
                    claim_poured(
                        self.plan,
                        &mut self.poured,
                        &mut self.tables,
                        held,
                        first_ordinal,
                    )?;
                }
            }
        }
        let physical: usize = self.window.iter().map(|held| held.batch.row_count()).sum();
        let pieces = morsel_plan(
            self.window.iter().map(|held| held.batch.row_count()),
            default_morsel_limit().max(physical.div_ceil(MORSEL_ROWS)),
        );
        // A round's rows are bounded by what every one of them being a new
        // group would add, against a share of the ceiling: a fold that runs
        // out part-way cannot be replayed.
        let per_row =
            (self.plan.group_stride * 2 + self.plan.cut_stride * 2 + 4) * size_of::<u64>();
        let round_rows = (self.memory.limit() / 8 / per_row).max(4_096);
        let mut start = 0;
        while start < pieces.len() {
            let mut end = start;
            let mut rows = 0;
            while end < pieces.len() && (end == start || rows + pieces[end].1.len() <= round_rows) {
                rows += pieces[end].1.len();
                end += 1;
            }
            self.memory.check_interruption()?;
            self.fold_round(&pieces[start..end])?;
            start = end;
        }
        self.window.clear();
        self.window_rows = 0;
        self.memory.release(self.window_reserved);
        self.window_reserved = 0;
        Ok(())
    }

    /// Takes one batch: into the window, or row by row when no cell can
    /// carry it.
    fn take(&mut self, batch: RecordBatch, ordinal: Option<u64>) -> Result<(), ExecError> {
        if !self.plan.carries(&batch) {
            let before = self.memory.used();
            let folded = self.fold_by_row(&batch);
            self.by_row_reserved = self
                .by_row_reserved
                .saturating_add(self.memory.used().saturating_sub(before));
            self.by_row_batches += 1;
            return folded;
        }
        self.note_declarations(&batch);
        let text = self.intern_text(&batch)?;
        self.settle_dense(&text)?;
        let bytes = batch.estimated_bytes();
        if self.memory.reserve(bytes).is_err() {
            self.flush()?;
            if self.memory.reserve(bytes).is_err() {
                self.spill()?;
                self.memory.reserve(bytes)?;
            }
        }
        self.window_reserved = self.window_reserved.saturating_add(bytes);
        self.window_rows += batch.visible_row_count();
        self.window.push(Prepared {
            batch,
            text,
            ordinal,
        });
        if self.window_rows >= WINDOW_ROWS || self.memory.used() > self.memory.limit() / 2 {
            self.flush()?;
        }
        Ok(())
    }

    /// The general path's fold of one batch, into groups kept by value.
    fn fold_by_row(&mut self, batch: &RecordBatch) -> Result<(), ExecError> {
        let partial = build_local_direct_groups(
            &Morsel::whole(batch),
            &self.plan.columns,
            self.aggregates,
            self.memory,
            self.key_collations,
        )?;
        for (key, group) in partial {
            merge_partial_group(
                &mut self.by_row,
                key,
                group,
                self.aggregates,
                0,
                self.memory,
            )
            .map_err(|(error, _)| error)?;
        }
        Ok(())
    }

    /// The finished groups of every table as columns, a partition at a
    /// time, each table's bytes handed back as its groups are finished.
    fn finish_columns(&mut self) -> Result<MaterializedRows, ExecError> {
        self.pour_dense()?;
        let plan = self.plan;
        let keys = plan.keys();
        let stride = plan.group_stride;
        let totals_at = 1 + keys + plan.texts();
        let aggregates = self.aggregates;
        let memory = self.memory;
        let tables = std::mem::take(&mut self.tables);
        let output = self.key_output();
        let fresh = |capacity: usize| {
            (0..keys)
                .map(|_| ReadyColumn::Values(Vec::with_capacity(capacity)))
                .chain(plan.packed.iter().map(|lane| match lane {
                    PackedLane::Sum {
                        scale,
                        float_output: false,
                    } => ReadyColumn::Decimal {
                        units: Vec::with_capacity(capacity),
                        scale: *scale,
                    },
                    _ => ReadyColumn::Values(Vec::with_capacity(capacity)),
                }))
                .collect::<Vec<_>>()
        };
        let pieces = tables
            .into_par_iter()
            .map(|table| -> Result<(usize, Vec<ReadyColumn>), ExecError> {
                let mut columns = fresh(table.groups);
                let mut uncharged = 0_usize;
                for group in table.words.chunks_exact(stride) {
                    for (index, column) in columns.iter_mut().enumerate() {
                        if index < keys {
                            let value = output.value(index, group);
                            uncharged =
                                uncharged.saturating_add(size_of::<Value>() + value.heap_bytes());
                            if let ReadyColumn::Values(values) = column {
                                values.push(value);
                            }
                            continue;
                        }
                        let lane = index - keys;
                        let packed = plan.packed[lane];
                        let cell =
                            packed_cell(packed, &group[totals_at + plan.cell_offsets[lane]..]);
                        match column {
                            ReadyColumn::Decimal { units, .. } => {
                                units.push((cell.rows > 0).then_some(cell.total));
                                uncharged = uncharged.saturating_add(size_of::<Option<i128>>());
                            }
                            ReadyColumn::Values(values) => {
                                let mut state = AggregateState::new(&aggregates[lane]);
                                cell.commit(packed, &mut state, &aggregates[lane], memory)?;
                                let value = state.finish(memory)?;
                                uncharged = uncharged
                                    .saturating_add(size_of::<Value>() + value.heap_bytes());
                                values.push(value);
                            }
                        }
                    }
                    if uncharged >= FINALIZE_CHARGE_SLICE {
                        memory.reserve(uncharged)?;
                        uncharged = 0;
                    }
                }
                memory.reserve(uncharged)?;
                let (groups, reserved) = (table.groups, table.reserved);
                drop(table);
                memory.release(reserved);
                Ok((groups, columns))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let len = pieces.iter().map(|(groups, _)| groups).sum();
        let mut columns = fresh(len);
        for (_, piece) in pieces {
            for (into, from) in columns.iter_mut().zip(piece) {
                match (into, from) {
                    (ReadyColumn::Values(into), ReadyColumn::Values(from)) => into.extend(from),
                    (
                        ReadyColumn::Decimal { units: into, .. },
                        ReadyColumn::Decimal { units: from, .. },
                    ) => into.extend(from),
                    _ => unreachable!("every partition lays its columns out alike"),
                }
            }
        }
        for intern in &mut self.interns {
            memory.release(intern.reserved);
            intern.reserved = 0;
        }
        Ok(MaterializedRows {
            rows: Vec::new(),
            position: 0,
            spilled: None,
            ready: (len > 0).then(|| ReadyColumns::from_columns(len, columns)),
        })
    }

    fn finish(&mut self) -> Result<MaterializedRows, ExecError> {
        self.flush()?;
        if self.spill_runs.is_empty() && self.by_row.is_empty() {
            return self.finish_columns();
        }
        // Groups went to disk, or were folded by value: what the tables
        // still hold joins them in the general path's form.
        let before = self.memory.used();
        let mut groups = self.drain_groups()?;
        for (key, group) in std::mem::take(&mut self.by_row) {
            merge_partial_group(&mut groups, key, group, self.aggregates, 0, self.memory)
                .map_err(|(error, _)| error)?;
        }
        self.memory.release(self.by_row_reserved);
        self.by_row_reserved = 0;
        if self.spill_runs.is_empty() {
            return finish_aggregate_groups(groups.into_values(), self.memory);
        }
        self.memory
            .release(self.memory.used().saturating_sub(before));
        merge_spilled_aggregate_groups(std::mem::take(&mut self.spill_runs), groups, self.memory)
    }
}

/// Runs the packed-key fold over `first` and everything `input` yields.
#[allow(clippy::too_many_lines)] // the pull loop, then the profile's note
pub(super) fn build_packed_group_aggregate(
    input: &mut PullOperator,
    first: RecordBatch,
    plan: &PackedGroupPlan,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    key_collations: &[Collation],
) -> Result<MaterializedRows, ExecError> {
    let mut fold = PackedGroupFold {
        plan,
        aggregates,
        key_collations,
        memory,
        tables: (0..PARTITIONS).map(|_| Table::default()).collect(),
        interns: plan
            .text_collations
            .iter()
            .map(|collation| TextIntern::new(*collation))
            .collect(),
        labels: vec![None; plan.texts()],
        members: vec![None; plan.texts()],
        window: Vec::new(),
        window_rows: 0,
        window_reserved: 0,
        by_row: HashMap::new(),
        by_row_reserved: 0,
        by_row_batches: 0,
        spill_runs: Vec::new(),
        dense: (plan.keys() >= 2 && plan.texts() == plan.keys()).then(|| DenseGroups {
            dims: vec![1; plan.keys()],
            words: Vec::new(),
            rows: 0,
            reserved: 0,
        }),
        dense_rows: 0,
        fused_floor: 0,
        poured: HashMap::new(),
    };
    let debug = std::env::var_os("PINTAIL_AGG_DEBUG").is_some();
    let started = std::time::Instant::now();
    let mut pulling = std::time::Duration::ZERO;
    // While the dense table takes the input, it is folded in place: the
    // window's batches first, then rounds in which each worker decodes a
    // slice and folds it itself. What a round leaves comes back through
    // the window, numbered by its place in the input.
    let mut fuse = !super::switches::fused_fold_disabled();
    let mut left = std::collections::VecDeque::<(u64, RecordBatch)>::new();
    let mut strikes = 0_u8;
    let mut drained = false;
    let mut next = Some((None, first));
    while let Some((ordinal, batch)) = next.take() {
        memory.check_interruption()?;
        fold.take(batch, ordinal)?;
        let pull = std::time::Instant::now();
        if fuse && left.is_empty() && fold.dense.is_some() {
            fold.flush()?;
            match fold.fold_in_place(input)? {
                InPlace::Off => fuse = false,
                InPlace::Done => drained = true,
                InPlace::Left(batches, worthwhile) => {
                    // Rounds that keep handing back more than they take
                    // - a predicate only the query's thread answers, keys
                    // that keep bringing spellings - are a detour.
                    strikes = if worthwhile { 0 } else { strikes + 1 };
                    fuse = strikes < 3;
                    left.extend(batches);
                }
                InPlace::Paused => {}
            }
        }
        next = match left.pop_front() {
            // An ordinal past what the rounds can number is the window's
            // own to give.
            Some((ordinal, batch)) => Some(((ordinal != u64::MAX).then_some(ordinal), batch)),
            None if drained => None,
            None => input.next_batch(memory)?.map(|batch| (None, batch)),
        };
        pulling += pull.elapsed();
    }
    fold.flush()?;
    if debug {
        let folded = started.elapsed();
        let groups: usize = fold.tables.iter().map(|table| table.groups).sum();
        let finish = std::time::Instant::now();
        let rows = fold.finish();
        eprintln!(
            "[agg] packed-key fold: {groups} groups, {} spill runs; input {:.1} ms, cut and \
             fold {:.1} ms, finish {:.1} ms",
            fold.spill_runs.len(),
            pulling.as_secs_f64() * 1e3,
            folded.saturating_sub(pulling).as_secs_f64() * 1e3,
            finish.elapsed().as_secs_f64() * 1e3
        );
        return rows;
    }
    let note = super::ProfileNote::of(input);
    if note.is_active() {
        let groups: usize = fold.tables.iter().map(|table| table.groups).sum();
        let held: usize = fold.tables.iter().map(|table| table.reserved).sum();
        let dense = fold.dense.as_ref().map_or_else(String::new, |dense| {
            format!(
                ", {} groups in a dense table of {} slots by text class",
                dense
                    .words
                    .chunks_exact(1 + plan.group_stride)
                    .filter(|slot| slot[0] != 0)
                    .count(),
                dense.words.len() / (1 + plan.group_stride)
            )
        });
        note.set(&format!(
            "packed-key fold: {groups} groups resident in {held} bytes over {PARTITIONS} \
             partitions{dense}, {} rows folded by text class, {} spill runs, {} batches \
             folded row by row",
            fold.dense_rows,
            fold.spill_runs.len(),
            fold.by_row_batches
        ));
    }
    fold.finish()
}

#[cfg(test)]
mod first_row_tests {
    //! A group of two text keys shows the spellings of its earliest row in
    //! scan order, whatever order its rows are folded in.

    use super::*;
    use crate::ColumnVector;
    use crate::array::{StrColumn, ValidityMask};

    fn collation() -> Collation {
        Collation::from_mysql_name("utf8mb4_0900_ai_ci").expect("collation")
    }

    fn coded(values: &[&str]) -> ColumnVector {
        let mut distinct: Vec<&str> = Vec::new();
        let codes = values
            .iter()
            .map(|text| {
                let code = distinct
                    .iter()
                    .position(|held| held == text)
                    .unwrap_or_else(|| {
                        distinct.push(text);
                        distinct.len() - 1
                    });
                u32::try_from(code).expect("small")
            })
            .collect::<Vec<_>>();
        let mut heap = Vec::new();
        let mut offsets = vec![0];
        for text in &distinct {
            heap.extend_from_slice(text.as_bytes());
            offsets.push(heap.len());
        }
        let validity = ValidityMask::from_bools(&vec![true; values.len()]);
        let column = StrColumn::from_dictionary(&heap, &offsets, codes, validity.clone());
        ColumnVector::from_typed(DataType::Utf8, TypedValues::Utf8(column), validity)
    }

    fn plain(values: &[&str]) -> ColumnVector {
        ColumnVector::new(
            DataType::Utf8,
            values
                .iter()
                .map(|text| Value::Utf8((*text).to_owned()))
                .collect(),
        )
        .expect("column")
    }

    fn batch(pairs: &[(&str, &str)], code: bool) -> RecordBatch {
        let (first, second): (Vec<&str>, Vec<&str>) = pairs.iter().copied().unzip();
        let column = if code { coded } else { plain };
        RecordBatch::new(pairs.len(), vec![column(&first), column(&second)]).expect("batch")
    }

    fn count_star() -> CompiledAggregate {
        CompiledAggregate {
            function: AggregateFunction::Count,
            expr: None,
            input_type: None,
            binary_width: None,
            distinct: false,
            data_type: Some(DataType::Int64),
            sum_carrier: None,
            separator: ",".to_owned(),
            order_within: Vec::new(),
            collation: Collation::default(),
        }
    }

    /// The plan, and interns that hold every spelling the tests use: the
    /// class of `Open` first met as `Open`, of `a` as `a`.
    fn setup(
        aggregates: &[CompiledAggregate],
        memory: &MemoryTracker,
    ) -> (PackedGroupPlan, Vec<TextIntern>) {
        let collations = [collation(), collation()];
        let plan = PackedGroupPlan::of(
            &[0, 1],
            aggregates,
            &batch(&[("Open", "a")], true),
            &collations,
        )
        .expect("plan");
        let mut interns = vec![TextIntern::new(collation()), TextIntern::new(collation())];
        for text in ["Open", "OPEN"] {
            interns[0].intern(text, memory).expect("intern");
        }
        for text in ["a", "A"] {
            interns[1].intern(text, memory).expect("intern");
        }
        (plan, interns)
    }

    fn prepared(
        plan: &PackedGroupPlan,
        interns: &[TextIntern],
        pairs: &[(&str, &str)],
        ordinal: Option<u64>,
    ) -> Prepared {
        let batch = batch(pairs, true);
        let text = known_text(plan, interns, &batch)
            .expect("text")
            .expect("known");
        Prepared {
            batch,
            text,
            ordinal,
        }
    }

    /// The one group a dense table holds: its ordinal word, its spellings
    /// and its count.
    fn only_group(
        plan: &PackedGroupPlan,
        interns: &[TextIntern],
        words: &[u64],
    ) -> (u64, String, String, u64) {
        let keys = plan.keys();
        let mut groups = words
            .chunks_exact(1 + plan.group_stride)
            .filter(|group| group[0] != 0);
        let group = groups.next().expect("a group");
        assert!(groups.next().is_none(), "one group");
        let spelling = |index: usize| {
            interns[index].spellings[slot_start(group[2 + keys + index], usize::MAX)].clone()
        };
        let totals_at = 2 + keys + plan.texts();
        (group[0], spelling(0), spelling(1), group[totals_at])
    }

    /// A worker's seat folds the batches it is handed in whatever order the
    /// round hands them: a later batch first, then an earlier one. The
    /// group keeps the earlier batch's spellings and both batches' rows.
    #[test]
    fn a_seat_takes_an_earlier_batchs_spellings_after_a_later_one() {
        let memory = MemoryTracker::new(usize::MAX);
        let aggregates = [count_star()];
        let (plan, interns) = setup(&aggregates, &memory);
        let dims = [2, 2];
        let mut words = vec![0_u64; 4 * (1 + plan.group_stride)];
        let later = prepared(&plan, &interns, &[("OPEN", "A")], None);
        let earlier = prepared(&plan, &interns, &[("Open", "a")], None);
        fold_dense_rows(&plan, &later, 0..1, 100, &dims, &mut words).expect("later");
        fold_dense_rows(&plan, &earlier, 0..1, 10, &dims, &mut words).expect("earlier");
        assert_eq!(
            only_group(&plan, &interns, &words),
            (11, "Open".to_owned(), "a".to_owned(), 2)
        );
        // The other way round, the earlier batch's spellings stay.
        let mut words = vec![0_u64; 4 * (1 + plan.group_stride)];
        fold_dense_rows(&plan, &earlier, 0..1, 10, &dims, &mut words).expect("earlier");
        fold_dense_rows(&plan, &later, 0..1, 100, &dims, &mut words).expect("later");
        assert_eq!(
            only_group(&plan, &interns, &words),
            (11, "Open".to_owned(), "a".to_owned(), 2)
        );
    }

    fn fold<'a>(
        plan: &'a PackedGroupPlan,
        interns: Vec<TextIntern>,
        aggregates: &'a [CompiledAggregate],
        collations: &'a [Collation],
        memory: &'a MemoryTracker,
    ) -> PackedGroupFold<'a> {
        PackedGroupFold {
            plan,
            aggregates,
            key_collations: collations,
            memory,
            tables: (0..PARTITIONS).map(|_| Table::default()).collect(),
            interns,
            labels: vec![None; plan.texts()],
            members: vec![None; plan.texts()],
            window: Vec::new(),
            window_rows: 0,
            window_reserved: 0,
            by_row: HashMap::new(),
            by_row_reserved: 0,
            by_row_batches: 0,
            spill_runs: Vec::new(),
            dense: Some(DenseGroups {
                dims: vec![2, 2],
                words: Vec::new(),
                rows: 1,
                reserved: 0,
            }),
            dense_rows: 0,
            fused_floor: 0,
            poured: HashMap::new(),
        }
    }

    /// A batch a fused round left carries its own ordinal, past the rows
    /// the windows numbered. Once its window is folded, a batch pulled
    /// after it is numbered after it too, and cannot take its group.
    #[test]
    fn a_pulled_batch_is_numbered_after_a_left_batch() {
        let memory = MemoryTracker::new(usize::MAX);
        let aggregates = [count_star()];
        let collations = [collation(), collation()];
        let (plan, interns) = setup(&aggregates, &memory);
        let left = prepared(&plan, &interns, &[("Open", "a")], Some(1 << 42));
        let pulled = prepared(&plan, &interns, &[("OPEN", "A")], None);
        let mut fold = fold(&plan, interns, &aggregates, &collations, &memory);
        fold.window.push(left);
        fold.flush_dense().expect("left");
        fold.window.clear();
        assert!(
            fold.dense.as_ref().expect("dense").rows > 1 << 42,
            "the cursor passes the left batch's rows"
        );
        fold.window.push(pulled);
        fold.flush_dense().expect("pulled");
        let dense = fold.dense.as_ref().expect("dense");
        let (_, first, second, count) = only_group(&plan, &fold.interns, &dense.words);
        assert_eq!((first.as_str(), second.as_str(), count), ("Open", "a", 2));
    }

    /// A batch a fused round left that decodes without dictionary codes
    /// moves the dense table's groups to the hashed tables. Its rows are
    /// earlier than rows the rounds already folded into those groups, so
    /// the group takes its spellings.
    #[test]
    fn a_left_batch_that_ends_the_dense_table_keeps_its_earlier_spellings() {
        let memory = MemoryTracker::new(usize::MAX);
        let aggregates = [count_star()];
        let collations = [collation(), collation()];
        let (plan, interns) = setup(&aggregates, &memory);
        let folded = prepared(&plan, &interns, &[("OPEN", "A")], Some(1 << 40));
        let mut fold = fold(&plan, interns, &aggregates, &collations, &memory);
        // As after a fused round handed over its first place.
        fold.fused_floor = 1;
        fold.window.push(folded);
        fold.flush_dense().expect("folded");
        fold.window.clear();
        fold.take(batch(&[("Open", "a")], false), Some(1 << 30))
            .expect("take");
        fold.flush().expect("flush");
        assert!(fold.dense.is_none(), "the hashed tables hold the groups");
        let output = fold.key_output();
        let groups = fold
            .tables
            .iter()
            .flat_map(|table| table.words.chunks_exact(plan.group_stride))
            .map(|group| {
                (
                    output.value(0, group),
                    output.value(1, group),
                    group[1 + plan.keys() + plan.texts()],
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            groups,
            vec![(
                Value::Utf8("Open".to_owned()),
                Value::Utf8("a".to_owned()),
                2
            )]
        );
    }
}
