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
    FINALIZE_CHARGE_SLICE, PackedCell, PackedLane, ReadyColumn, ReadyColumns, TwoPassLane,
    aggregate_lanes, derived_temporal_units, labelled_text_value, lane_readers,
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
        if std::env::var_os("PINTAIL_DISABLE_PACKED_GROUP").is_some() {
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
}

impl PackedGroupFold<'_> {
    /// Notes what the text key columns declare, as each batch arrives: a
    /// later batch may be the first to carry the whole declaration.
    fn note_declarations(&mut self, batch: &RecordBatch) {
        for (column, kind) in self.plan.columns.iter().zip(&self.plan.kinds) {
            let KeyKind::Text { slot } = *kind else {
                continue;
            };
            let settled = self.members[slot].is_some()
                || self.labels[slot]
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
            self.labels[slot] = match (self.labels[slot].take(), strings.declared_enum_labels()) {
                (Some((held, _)), Some(seen)) if !exhaustive => {
                    Some((merge_partial_labels(held, seen), false))
                }
                (_, Some(seen)) => Some((Arc::clone(seen), exhaustive)),
                (held, None) => held,
            };
            self.members[slot] = strings.declared_set_members().cloned();
        }
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
                // query holds.
                let mut ids = vec![u64::MAX; entries.len()];
                for row in batch.selection().selected_rows() {
                    if !validity.is_valid(row) {
                        continue;
                    }
                    let code = usize::try_from(codes[row])
                        .map_err(|_| ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                    let slot = ids
                        .get_mut(code)
                        .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                    if *slot == u64::MAX {
                        *slot = intern.intern(&entries[code], self.memory)?;
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
        let keys = self.plan.keys();
        let stride = self.plan.group_stride;
        let totals_at = 1 + keys + self.plan.texts();
        let mut groups = HashMap::with_capacity(self.tables.iter().map(|table| table.groups).sum());
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
    fn take(&mut self, batch: RecordBatch) -> Result<(), ExecError> {
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
        self.window.push(Prepared { batch, text });
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
    };
    let debug = std::env::var_os("PINTAIL_AGG_DEBUG").is_some();
    let started = std::time::Instant::now();
    let mut pulling = std::time::Duration::ZERO;
    let mut next = Some(first);
    while let Some(batch) = next.take() {
        memory.check_interruption()?;
        fold.take(batch)?;
        let pull = std::time::Instant::now();
        next = input.next_batch(memory)?;
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
        note.set(&format!(
            "packed-key fold: {groups} groups resident in {held} bytes over {PARTITIONS} \
             partitions, {} spill runs, {} batches folded row by row",
            fold.spill_runs.len(),
            fold.by_row_batches
        ));
    }
    fold.finish()
}
