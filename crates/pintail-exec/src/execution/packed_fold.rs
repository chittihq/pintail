//! Column-at-a-time folds for the packed aggregate lanes.
//!
//! A packed lane (COUNT(*), exact DECIMAL SUM and AVG, DECIMAL MIN/MAX)
//! reduces every group to an `i128` total and a row count, which is what
//! [`PackedCell`] commits to a group's state. The per-row fold resolved
//! each lane's reader, matched the lane kind and returned a `Result` for
//! every row and lane; here a batch's slots are computed once and each lane
//! then runs one monomorphized loop over its packed column, so the loop body
//! is a load, an add and a store.
//!
//! The accumulator is struct-of-arrays: a row count per slot, a total per
//! lane and slot, and a NULL count per lane and slot that is only allocated
//! once a lane meets a NULL. A lane's row count is the slot's rows minus its
//! NULLs, which is the count [`PackedCell::add`] keeps by incrementing.

use std::ops::Range;

use super::aggregate::{AggregateState, CompiledAggregate, written_zeros};
use super::two_pass::{LaneReader, PackedCell, PackedLane, TwoPassLane};
use super::{ExecError, MemoryTracker};
use crate::RecordBatch;
use crate::array::ValidityMask;
use crate::batch::{DecimalUnits, TypedValues};

/// The physical rows a fold reads, in the order their slots are listed.
#[derive(Clone, Debug)]
pub(super) enum FoldRows<'a> {
    /// A contiguous run of rows, every one selected.
    Span(Range<usize>),
    /// Selected rows, ascending.
    Picked(&'a [u32]),
}

impl FoldRows<'_> {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Span(rows) => rows.len(),
            Self::Picked(rows) => rows.len(),
        }
    }

    /// The listed rows, in order.
    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len()).map(move |index| match self {
            Self::Span(rows) => rows.start + index,
            Self::Picked(rows) => rows[index] as usize,
        })
    }
}

/// The rows of `rows` the batch selects: a span when every one is selected,
/// otherwise the selected rows gathered into `buffer`.
pub(super) fn fold_rows<'a>(
    batch: &RecordBatch,
    rows: Range<usize>,
    buffer: &'a mut Vec<u32>,
) -> FoldRows<'a> {
    let selection = batch.selection();
    if selection.count_in(rows.clone()) == rows.len() {
        return FoldRows::Span(rows);
    }
    buffer.clear();
    buffer.extend(
        selection
            .selected_rows_in(rows)
            .map(|row| u32::try_from(row).expect("batch row fits u32")),
    );
    FoldRows::Picked(buffer)
}

/// One lane's input for a batch, resolved before any row is read.
pub(super) enum LaneInput<'a> {
    /// The lane is not packed; the caller applies it some other way.
    Skip,
    /// COUNT(*): the slot's row count is the lane's.
    Count,
    /// Signed 64-bit units: scaled decimals, integers, or temporal units.
    Units(&'a [i64], &'a ValidityMask),
    /// COUNT of a column: the slot's rows less the rows this marks NULL.
    Presence(&'a ValidityMask),
}

const SUM: u8 = 0;
const MINIMUM: u8 = 1;
const MAXIMUM: u8 = 2;

#[inline]
fn combine<const OP: u8>(total: &mut i128, value: i64) {
    let value = i128::from(value);
    match OP {
        // A total of `i64` units cannot leave `i128` before 2^64 rows, so
        // the wrapping add never wraps; it only spares the overflow check.
        SUM => *total = total.wrapping_add(value),
        MINIMUM => *total = (*total).min(value),
        _ => *total = (*total).max(value),
    }
}

/// A paired cell's sum and row count: the low and the high half of the
/// 128 bits a total takes.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn pair_parts(cell: i128) -> (i64, u64) {
    let bits = cell as u128;
    (
        i64::from_ne_bytes((bits as u64).to_ne_bytes()),
        (bits >> 64) as u64,
    )
}

/// The paired cell holding `sum` and `rows`.
#[inline]
#[allow(clippy::cast_possible_wrap)]
fn pair_of(sum: i64, rows: u64) -> i128 {
    ((u128::from(rows) << 64) | u128::from(u64::from_ne_bytes(sum.to_ne_bytes()))) as i128
}

/// Adds one row of `value` to a paired cell: two 64-bit additions in the
/// one cache line the slot's sum and row count share.
#[inline]
fn pair_add(cell: &mut i128, value: i64) {
    let (sum, rows) = pair_parts(*cell);
    *cell = pair_of(sum.wrapping_add(value), rows.wrapping_add(1));
}

/// Counts one row in a paired cell without adding to its sum.
#[inline]
fn pair_count(cell: &mut i128) {
    let (sum, rows) = pair_parts(*cell);
    *cell = pair_of(sum, rows.wrapping_add(1));
}

/// A bound on what `values` can add to any one sum: the rows times a
/// magnitude no value exceeds. The magnitude is the OR of every value's
/// bits with a negative value's inverted, which is a pass of one
/// instruction a value where a sum of absolute values would carry.
#[inline]
fn magnitude_bound(values: &[i64], rows: usize) -> u128 {
    let bits = values
        .iter()
        .fold(0_i64, |bits, value| bits | (value ^ (value >> 63)));
    // `bits` is not negative, and no value is further from zero than one
    // past it.
    (u128::from(bits.unsigned_abs()) + 1) * rows as u128
}

/// One column's accumulators: the totals of its SUM, MIN and MAX roles
/// (empty for a role no lane takes) and its NULL rows per slot.
struct ColumnTotals<'a> {
    sum: &'a mut [i128],
    minimum: &'a mut [i128],
    maximum: &'a mut [i128],
}

impl ColumnTotals<'_> {
    /// Folds one value into `slot` for every role the column has. With `P`
    /// the sum's cells are paired and the row is counted in the same cell.
    #[inline]
    fn add<const S: bool, const MN: bool, const MX: bool, const P: bool>(
        &mut self,
        slot: usize,
        value: i64,
    ) {
        if S {
            if P {
                pair_add(&mut self.sum[slot], value);
            } else {
                combine::<SUM>(&mut self.sum[slot], value);
            }
        }
        if MN {
            combine::<MINIMUM>(&mut self.minimum[slot], value);
        }
        if MX {
            combine::<MAXIMUM>(&mut self.maximum[slot], value);
        }
    }
}

/// Folds one column's units into every role it has in a single pass,
/// counting NULL rows per slot. With `P` the sum's cells are paired: every
/// row is counted in its cell, a NULL row included.
#[inline]
fn fold_column<const S: bool, const MN: bool, const MX: bool, const P: bool>(
    mut totals: ColumnTotals<'_>,
    nulls: &mut Vec<u64>,
    slot_count: usize,
    slots: &[u32],
    rows: &FoldRows<'_>,
    values: &[i64],
    validity: &ValidityMask,
) {
    match rows {
        FoldRows::Span(span) => {
            let values = &values[span.clone()];
            if validity.no_nulls() {
                for (&slot, &value) in slots.iter().zip(values) {
                    totals.add::<S, MN, MX, P>(slot as usize, value);
                }
            } else {
                let nulls = lane_nulls(nulls, slot_count);
                for ((&slot, &value), row) in slots.iter().zip(values).zip(span.clone()) {
                    if validity.is_valid(row) {
                        totals.add::<S, MN, MX, P>(slot as usize, value);
                    } else {
                        if P {
                            pair_count(&mut totals.sum[slot as usize]);
                        }
                        nulls[slot as usize] += 1;
                    }
                }
            }
        }
        FoldRows::Picked(picked) => {
            if validity.no_nulls() {
                for (&slot, &row) in slots.iter().zip(*picked) {
                    totals.add::<S, MN, MX, P>(slot as usize, values[row as usize]);
                }
            } else {
                let nulls = lane_nulls(nulls, slot_count);
                for (&slot, &row) in slots.iter().zip(*picked) {
                    let row = row as usize;
                    if validity.is_valid(row) {
                        totals.add::<S, MN, MX, P>(slot as usize, values[row]);
                    } else {
                        if P {
                            pair_count(&mut totals.sum[slot as usize]);
                        }
                        nulls[slot as usize] += 1;
                    }
                }
            }
        }
    }
}

/// The packed lanes that read one column: the lane holding each role's
/// totals (SUM and AVG share the sum role), and the lane holding the
/// column's NULL counts.
#[derive(Clone, Copy, Debug, Default)]
struct ColumnRoles {
    sum: Option<usize>,
    minimum: Option<usize>,
    maximum: Option<usize>,
    nulls: usize,
}

fn lane_nulls(nulls: &mut Vec<u64>, slot_count: usize) -> &mut [u64] {
    if nulls.is_empty() {
        *nulls = written_zeros(slot_count);
    }
    nulls
}

/// The starting total of a lane: the identity of its combine.
fn identity(lane: Option<PackedLane>) -> i128 {
    match lane {
        Some(PackedLane::Minimum { .. }) => i128::MAX,
        Some(PackedLane::Maximum { .. }) => i128::MIN,
        _ => 0,
    }
}

/// Whether a lane keeps a total (COUNT(*) needs only the slot's rows).
fn keeps_total(lane: Option<PackedLane>) -> bool {
    matches!(
        lane,
        Some(
            PackedLane::Sum { .. }
                | PackedLane::IntegerSum
                | PackedLane::Average { .. }
                | PackedLane::Minimum { .. }
                | PackedLane::Maximum { .. }
        )
    )
}

/// Per-slot packed totals for every packed lane of an aggregate.
pub(super) struct PackedFold {
    slot_count: usize,
    lanes: Vec<Option<PackedLane>>,
    /// Selected rows per slot; empty while a lane is paired.
    counts: Vec<u64>,
    /// The sum lane whose cells also hold the slot's row count, 64 bits
    /// each: a row then touches one cache line for both, where a count and
    /// a 128-bit total in two arrays are two lines and, past a few thousand
    /// groups, two misses. `None` once a sum might leave 64 bits.
    paired: Option<usize>,
    /// While paired: a bound on the magnitude of every sum in the paired
    /// lane, the rows folded times the largest value each batch held.
    magnitude: u128,
    /// Per lane: one total per slot, or empty for a lane with no total.
    totals: Vec<Vec<i128>>,
    /// Per lane: NULL rows per slot, empty until the lane meets a NULL.
    nulls: Vec<Vec<u64>>,
    /// Per lane: the lane whose `totals` hold its result - itself, or the
    /// first lane of the same role over the same column (SUM beside AVG).
    total_of: Vec<usize>,
    /// Per lane: the lane whose `nulls` count its column's NULL rows.
    nulls_of: Vec<usize>,
    /// The lanes that keep totals, grouped by the column they read.
    columns: Vec<ColumnRoles>,
}

impl PackedFold {
    #[cfg(test)]
    pub(super) fn new(slot_count: usize, lanes: &[Option<PackedLane>]) -> Self {
        Self::over_columns(slot_count, lanes, &(0..lanes.len()).collect::<Vec<_>>())
    }

    /// A fold whose lanes read `columns` (one per lane): the lanes over one
    /// column fold in a single pass, and a second sum of a column - SUM
    /// beside AVG - shares the first one's totals instead of repeating it.
    pub(super) fn sharing(
        slot_count: usize,
        packed: &[Option<PackedLane>],
        lanes: &[TwoPassLane],
    ) -> Self {
        let columns = lanes
            .iter()
            .enumerate()
            .map(|(index, lane)| match lane {
                TwoPassLane::DecimalUnits { column, .. }
                | TwoPassLane::ExtremeDecimal { column, .. }
                | TwoPassLane::Int { column, .. }
                | TwoPassLane::Exact { column, .. }
                | TwoPassLane::Temporal { column, .. } => *column,
                // Never shared: a key past any real column index.
                _ => usize::MAX - index,
            })
            .collect::<Vec<_>>();
        Self::over_columns(slot_count, packed, &columns)
    }

    fn over_columns(slot_count: usize, lanes: &[Option<PackedLane>], sources: &[usize]) -> Self {
        let mut total_of = (0..lanes.len()).collect::<Vec<_>>();
        let mut nulls_of = total_of.clone();
        let mut columns: Vec<(usize, ColumnRoles)> = Vec::new();
        for (index, lane) in lanes.iter().enumerate() {
            if !keeps_total(*lane) {
                continue;
            }
            let source = sources[index];
            let position = columns
                .iter()
                .position(|(column, _)| *column == source)
                .unwrap_or_else(|| {
                    columns.push((
                        source,
                        ColumnRoles {
                            nulls: index,
                            ..ColumnRoles::default()
                        },
                    ));
                    columns.len() - 1
                });
            let roles = &mut columns[position].1;
            nulls_of[index] = roles.nulls;
            let role = match lane {
                Some(PackedLane::Minimum { .. }) => &mut roles.minimum,
                Some(PackedLane::Maximum { .. }) => &mut roles.maximum,
                _ => &mut roles.sum,
            };
            total_of[index] = *role.get_or_insert(index);
        }
        // The first column with a sum: its totals start at zero, which is
        // also a paired cell of no rows.
        let paired = columns.iter().find_map(|(_, roles)| roles.sum);
        Self {
            slot_count,
            lanes: lanes.to_vec(),
            counts: if paired.is_some() {
                Vec::new()
            } else {
                written_zeros(slot_count)
            },
            paired,
            magnitude: 0,
            totals: lanes
                .iter()
                .enumerate()
                .map(|(index, lane)| {
                    if keeps_total(*lane) && total_of[index] == index {
                        filled(slot_count, identity(*lane))
                    } else {
                        Vec::new()
                    }
                })
                .collect(),
            nulls: lanes.iter().map(|_| Vec::new()).collect(),
            total_of,
            nulls_of,
            columns: columns.into_iter().map(|(_, roles)| roles).collect(),
        }
    }

    /// The most a fold over `slot_count` slots holds, NULL counts included.
    pub(super) fn bytes(slot_count: usize, lanes: &[Option<PackedLane>]) -> usize {
        let per_slot = lanes.iter().fold(size_of::<u64>(), |bytes, lane| {
            let total = if keeps_total(*lane) {
                size_of::<i128>()
            } else {
                0
            };
            bytes.saturating_add(total + size_of::<u64>())
        });
        slot_count.saturating_mul(per_slot)
    }

    /// Whether any row reached `slot`.
    pub(super) fn occupied(&self, slot: usize) -> bool {
        self.rows_at(slot) > 0
    }

    /// Whether a sum lane's cells hold the row counts too.
    #[cfg(test)]
    pub(super) const fn is_paired(&self) -> bool {
        self.paired.is_some()
    }

    /// Selected rows that reached `slot`.
    #[inline]
    fn rows_at(&self, slot: usize) -> u64 {
        match self.paired {
            Some(lane) => pair_parts(self.totals[lane][slot]).1,
            None => self.counts[slot],
        }
    }

    /// The total lane `owner` keeps for `slot`, or `None` for a lane that
    /// keeps none.
    #[inline]
    fn total_at(&self, owner: usize, slot: usize) -> Option<i128> {
        let cell = *self.totals[owner].get(slot)?;
        Some(if self.paired == Some(owner) {
            i128::from(pair_parts(cell).0)
        } else {
            cell
        })
    }

    /// Splits the paired cells into a row count and a 128-bit total each,
    /// in place: the counts take the array every unpaired fold has, and
    /// each total is its own low half widened.
    fn unpair(&mut self) {
        let Some(lane) = self.paired.take() else {
            return;
        };
        let mut counts = written_zeros::<u64>(self.slot_count);
        for (cell, count) in self.totals[lane].iter_mut().zip(&mut counts) {
            let (sum, rows) = pair_parts(*cell);
            *count = rows;
            *cell = i128::from(sum);
        }
        self.counts = counts;
    }

    /// Takes `bound` more of the room the paired sums have in 64 bits, or
    /// unpairs when it is not there.
    fn take_headroom(&mut self, bound: u128) {
        if self.paired.is_none() {
            return;
        }
        let magnitude = self.magnitude.saturating_add(bound);
        if magnitude > i64::MAX.unsigned_abs().into() {
            self.unpair();
        } else {
            self.magnitude = magnitude;
        }
    }

    /// Each lane's packed input in `batch`, or `None` when a packed lane's
    /// column carries no 64-bit units (the caller folds such a batch row by
    /// row through [`Self::add_row`]).
    pub(super) fn resolve<'a>(
        &self,
        batch: &'a RecordBatch,
        lanes: &[TwoPassLane],
    ) -> Option<Vec<LaneInput<'a>>> {
        lanes
            .iter()
            .zip(&self.lanes)
            .map(|(lane, packed)| match (packed, lane) {
                (None, _) => Some(LaneInput::Skip),
                (Some(PackedLane::Count), _) => Some(LaneInput::Count),
                (
                    Some(PackedLane::Present),
                    TwoPassLane::Present { column } | TwoPassLane::Int { column, .. },
                ) => batch
                    .column(*column)
                    .and_then(crate::ColumnVector::typed)
                    .map(|(_, validity)| LaneInput::Presence(validity)),
                (Some(_), TwoPassLane::Int { column, .. } | TwoPassLane::Exact { column, .. }) => {
                    match batch.column(*column).and_then(crate::ColumnVector::typed) {
                        Some((TypedValues::Int64(values), validity))
                            if values.len() >= batch.row_count() =>
                        {
                            Some(LaneInput::Units(values, validity))
                        }
                        _ => None,
                    }
                }
                // Units only where the column's text is the units' own
                // spelling; any other batch never reaches a lane.
                (Some(_), TwoPassLane::Temporal { column, .. }) => {
                    match batch.column(*column).and_then(crate::ColumnVector::typed) {
                        Some((TypedValues::Temporal { units, text }, validity))
                            if text.derived() && units.len() >= batch.row_count() =>
                        {
                            Some(LaneInput::Units(units, validity))
                        }
                        _ => None,
                    }
                }
                (
                    Some(_),
                    TwoPassLane::DecimalUnits { column, .. }
                    | TwoPassLane::ExtremeDecimal { column, .. },
                ) => match batch.column(*column).and_then(crate::ColumnVector::typed) {
                    Some((
                        TypedValues::Decimal128 {
                            values: DecimalUnits::Narrow(values),
                            ..
                        },
                        validity,
                    )) if values.len() >= batch.row_count() => {
                        Some(LaneInput::Units(values, validity))
                    }
                    _ => None,
                },
                (Some(_), _) => None,
            })
            .collect()
    }

    /// Folds the rows `rows` lists, the i-th into `slots[i]`.
    pub(super) fn fold(&mut self, inputs: &[LaneInput<'_>], slots: &[u32], rows: &FoldRows<'_>) {
        debug_assert_eq!(slots.len(), rows.len());
        if let Some(lane) = self.paired
            && let LaneInput::Units(values, _) = &inputs[self.nulls_of[lane]]
        {
            // The values between the first and the last listed row bound
            // the listed ones.
            let read = match rows {
                FoldRows::Span(span) => &values[span.clone()],
                FoldRows::Picked(picked) => match (picked.first(), picked.last()) {
                    (Some(first), Some(last)) => &values[*first as usize..=*last as usize],
                    _ => &values[..0],
                },
            };
            self.take_headroom(magnitude_bound(read, rows.len()));
        }
        // A paired fold counts each row in the pass that adds its value.
        let mut counted = false;
        if self.paired.is_none() {
            counted = true;
            for &slot in slots {
                self.counts[slot as usize] += 1;
            }
        }
        for (index, input) in inputs.iter().enumerate() {
            let LaneInput::Presence(validity) = input else {
                continue;
            };
            if validity.no_nulls() {
                continue;
            }
            let slot_count = self.slot_count;
            let nulls = lane_nulls(&mut self.nulls[index], slot_count);
            for (row, &slot) in rows.iter().zip(slots) {
                if !validity.is_valid(row) {
                    nulls[slot as usize] += 1;
                }
            }
        }
        for roles in &self.columns {
            let LaneInput::Units(values, validity) = &inputs[roles.nulls] else {
                continue;
            };
            let mut take = |lane: Option<usize>| {
                lane.map_or_else(Vec::new, |lane| std::mem::take(&mut self.totals[lane]))
            };
            let (mut sum, mut minimum, mut maximum) =
                (take(roles.sum), take(roles.minimum), take(roles.maximum));
            let mut nulls = std::mem::take(&mut self.nulls[roles.nulls]);
            let totals = ColumnTotals {
                sum: &mut sum,
                minimum: &mut minimum,
                maximum: &mut maximum,
            };
            let slot_count = self.slot_count;
            let pairs = roles.sum.is_some() && roles.sum == self.paired;
            counted |= pairs;
            macro_rules! fold {
                ($s:literal, $mn:literal, $mx:literal, $p:literal) => {
                    fold_column::<$s, $mn, $mx, $p>(
                        totals, &mut nulls, slot_count, slots, rows, values, validity,
                    )
                };
            }
            match (
                roles.sum.is_some(),
                roles.minimum.is_some(),
                roles.maximum.is_some(),
                pairs,
            ) {
                (true, true, true, true) => fold!(true, true, true, true),
                (true, true, false, true) => fold!(true, true, false, true),
                (true, false, true, true) => fold!(true, false, true, true),
                (true, false, false, true) => fold!(true, false, false, true),
                (true, true, true, false) => fold!(true, true, true, false),
                (true, true, false, false) => fold!(true, true, false, false),
                (true, false, true, false) => fold!(true, false, true, false),
                (true, false, false, false) => fold!(true, false, false, false),
                (false, true, true, _) => fold!(false, true, true, false),
                (false, true, false, _) => fold!(false, true, false, false),
                (false, false, true, _) => fold!(false, false, true, false),
                (false, false, false, _) => {}
            }
            for (lane, totals) in [roles.sum, roles.minimum, roles.maximum]
                .into_iter()
                .zip([sum, minimum, maximum])
            {
                if let Some(lane) = lane {
                    self.totals[lane] = totals;
                }
            }
            self.nulls[roles.nulls] = nulls;
        }
        if !counted && let Some(lane) = self.paired {
            // The paired column carried no units in this batch.
            let cells = &mut self.totals[lane];
            for &slot in slots {
                pair_count(&mut cells[slot as usize]);
            }
        }
    }

    /// Folds one row through the per-row readers: the same arithmetic as
    /// [`Self::fold`], for a batch whose columns carry no packed units.
    pub(super) fn add_row(&mut self, slot: usize, readers: &[LaneReader<'_>], row: usize) {
        if let Some(lane) = self.paired {
            let bound = readers[lane].bits(row).map_or(0, |bits| {
                u128::from(i64::from_ne_bytes(bits.to_ne_bytes()).unsigned_abs())
            });
            self.take_headroom(bound);
        }
        match self.paired {
            Some(lane) => pair_count(&mut self.totals[lane][slot]),
            None => self.counts[slot] += 1,
        }
        for (index, reader) in readers.iter().enumerate() {
            let lane = self.lanes[index];
            if matches!(lane, Some(PackedLane::Present)) {
                if reader.bits(row).is_none() {
                    let slot_count = self.slot_count;
                    lane_nulls(&mut self.nulls[index], slot_count)[slot] += 1;
                }
                continue;
            }
            if !keeps_total(lane) || self.total_of[index] != index {
                continue;
            }
            // A lane sharing another's totals is skipped above; one that
            // owns its totals but not its column's NULL counts skips NULLs.
            if let Some(bits) = reader.bits(row) {
                let value = i64::from_ne_bytes(bits.to_ne_bytes());
                let total = &mut self.totals[index][slot];
                if self.paired == Some(index) {
                    // The row is already counted; the sum is the low half.
                    let (sum, rows) = pair_parts(*total);
                    *total = pair_of(sum.wrapping_add(value), rows);
                    continue;
                }
                match lane {
                    Some(PackedLane::Minimum { .. }) => combine::<MINIMUM>(total, value),
                    Some(PackedLane::Maximum { .. }) => combine::<MAXIMUM>(total, value),
                    _ => combine::<SUM>(total, value),
                }
            } else if self.nulls_of[index] == index {
                let slot_count = self.slot_count;
                lane_nulls(&mut self.nulls[index], slot_count)[slot] += 1;
            }
        }
    }

    /// Adds `other` in, its slot `s` landing on `place(s)`.
    pub(super) fn merge_from(&mut self, other: &Self, place: impl Fn(usize) -> usize) {
        // The sums stay paired only while both sides' bounds fit 64 bits
        // together; an unpaired side's totals may already be past them.
        if other.paired.is_some() && other.paired == self.paired {
            self.take_headroom(other.magnitude);
        } else {
            self.unpair();
        }
        for slot in 0..other.slot_count {
            let count = other.rows_at(slot);
            if count == 0 {
                continue;
            }
            let target = place(slot);
            if self.paired.is_none() {
                self.counts[target] += count;
            }
            for index in 0..self.lanes.len() {
                if let Some(value) = other.total_at(index, slot) {
                    let total = &mut self.totals[index][target];
                    if self.paired == Some(index) {
                        let (sum, rows) = pair_parts(*total);
                        // Inside 64 bits: the bound above covers both.
                        let value = i64::try_from(value).unwrap_or_default();
                        *total = pair_of(sum.wrapping_add(value), rows.wrapping_add(count));
                    } else {
                        match self.lanes[index] {
                            Some(PackedLane::Minimum { .. }) => *total = (*total).min(value),
                            Some(PackedLane::Maximum { .. }) => *total = (*total).max(value),
                            _ => *total = total.wrapping_add(value),
                        }
                    }
                }
                if let Some(&nulls) = other.nulls[index].get(slot)
                    && nulls > 0
                {
                    let slot_count = self.slot_count;
                    lane_nulls(&mut self.nulls[index], slot_count)[target] += nulls;
                }
            }
        }
    }

    /// Applies `slot`'s totals to its group's states, once per lane.
    pub(super) fn commit_slot(
        &self,
        slot: usize,
        states: &mut [AggregateState],
        aggregates: &[CompiledAggregate],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        commit_merged(std::slice::from_ref(self), slot, states, aggregates, memory)
    }
}

/// Whether any of `folds`, all over the same slots, reached `slot`.
pub(super) fn occupied_in(folds: &[PackedFold], slot: usize) -> bool {
    folds.iter().any(|fold| fold.occupied(slot))
}

/// Applies `slot`'s totals across `folds` - worker partials over the same
/// slots and lanes - to its group's states, once per lane: the partials
/// combine per slot here instead of being merged whole first.
pub(super) fn commit_merged(
    folds: &[PackedFold],
    slot: usize,
    states: &mut [AggregateState],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    for (index, (state, aggregate)) in states.iter_mut().zip(aggregates).enumerate() {
        if let Some((lane, cell)) = merged_cell(folds, index, slot) {
            cell.commit(lane, state, aggregate, memory)?;
        }
    }
    Ok(())
}

/// Lane `index`'s total and row count for `slot` across `folds`, or `None`
/// for a lane that is not packed.
pub(super) fn merged_cell(
    folds: &[PackedFold],
    index: usize,
    slot: usize,
) -> Option<(PackedLane, PackedCell)> {
    let lane = folds.first()?.lanes[index]?;
    let count: u64 = folds.iter().map(|fold| fold.rows_at(slot)).sum();
    let mut total = identity(Some(lane));
    let mut nulls = 0_u64;
    for fold in folds {
        if let Some(value) = fold.total_at(fold.total_of[index], slot) {
            match lane {
                PackedLane::Minimum { .. } => total = total.min(value),
                PackedLane::Maximum { .. } => total = total.max(value),
                _ => total = total.wrapping_add(value),
            }
        }
        nulls += fold.nulls[fold.nulls_of[index]]
            .get(slot)
            .copied()
            .unwrap_or(0);
    }
    Some((
        lane,
        PackedCell {
            total,
            rows: count - nulls,
        },
    ))
}

/// `len` copies of `value`, written as they are made (see [`written_zeros`]:
/// a zero identity would otherwise come back as untouched zero pages).
fn filled(len: usize, value: i128) -> Vec<i128> {
    let mut values = written_zeros(len);
    if value != 0 {
        values.fill(value);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::super::two_pass::UnitText;
    use super::{FoldRows, LaneInput, PackedFold, PackedLane, merged_cell};
    use crate::array::ValidityMask;

    const LANES: [Option<PackedLane>; 4] = [
        Some(PackedLane::Count),
        Some(PackedLane::Sum {
            scale: 2,
            float_output: false,
        }),
        Some(PackedLane::Minimum {
            text: UnitText::Decimal { scale: 2 },
        }),
        Some(PackedLane::Maximum {
            text: UnitText::Decimal { scale: 2 },
        }),
    ];

    /// Invented column values: signed units, every seventh row NULL.
    fn column(rows: usize) -> (Vec<i64>, ValidityMask) {
        let values = (0..rows)
            .map(|row| {
                let row = i64::try_from(row).expect("small");
                (row * 7_919) % 20_011 - 10_000
            })
            .collect::<Vec<_>>();
        let valid = (0..rows).map(|row| row % 7 != 3).collect::<Vec<_>>();
        (values, ValidityMask::from_bools(&valid))
    }

    fn slots_of(rows: impl Iterator<Item = usize>, slot_count: usize) -> Vec<u32> {
        rows.map(|row| u32::try_from((row * 31) % slot_count).expect("small"))
            .collect()
    }

    /// Per slot: rows, then (total, non-NULL rows) for SUM, MIN and MAX.
    fn reference(
        rows: &[usize],
        slots: &[u32],
        values: &[i64],
        valid: &ValidityMask,
        slot_count: usize,
    ) -> Vec<(u64, [(i128, u64); 3])> {
        let mut expected = vec![(0, [(0, 0), (i128::MAX, 0), (i128::MIN, 0)]); slot_count];
        for (&row, &slot) in rows.iter().zip(slots) {
            let entry = &mut expected[slot as usize];
            entry.0 += 1;
            if valid.is_valid(row) {
                let value = i128::from(values[row]);
                entry.1[0] = (entry.1[0].0 + value, entry.1[0].1 + 1);
                entry.1[1] = (entry.1[1].0.min(value), entry.1[1].1 + 1);
                entry.1[2] = (entry.1[2].0.max(value), entry.1[2].1 + 1);
            }
        }
        expected
    }

    fn check(folds: &[PackedFold], expected: &[(u64, [(i128, u64); 3])]) {
        for (slot, (rows, lanes)) in expected.iter().enumerate() {
            let (_, count) = merged_cell(folds, 0, slot).expect("count lane");
            assert_eq!(count.rows, *rows, "slot {slot} rows");
            for (index, (total, non_null)) in lanes.iter().enumerate() {
                let (_, cell) = merged_cell(folds, index + 1, slot).expect("packed lane");
                assert_eq!(cell.rows, *non_null, "slot {slot} lane {index} rows");
                if *non_null > 0 {
                    assert_eq!(cell.total, *total, "slot {slot} lane {index} total");
                }
            }
        }
    }

    #[test]
    fn spans_picked_rows_and_partials_agree_with_a_row_by_row_reference() {
        let rows = 10_000;
        let slot_count = 13;
        let (values, valid) = column(rows);
        let inputs = [
            LaneInput::Count,
            LaneInput::Units(&values, &valid),
            LaneInput::Units(&values, &valid),
            LaneInput::Units(&values, &valid),
        ];
        // A whole span into one fold.
        let all = (0..rows).collect::<Vec<_>>();
        let slots = slots_of(all.iter().copied(), slot_count);
        let mut fold = PackedFold::new(slot_count, &LANES);
        fold.fold(&inputs, &slots, &FoldRows::Span(0..rows));
        check(
            std::slice::from_ref(&fold),
            &reference(&all, &slots, &values, &valid, slot_count),
        );
        // Every third row picked, split across two partials.
        let picked = (0..rows).filter(|row| row % 3 == 0).collect::<Vec<_>>();
        let half = picked.len() / 2;
        let mut partials = Vec::new();
        for part in [&picked[..half], &picked[half..]] {
            let indices = part
                .iter()
                .map(|row| u32::try_from(*row).expect("small"))
                .collect::<Vec<_>>();
            let part_slots = slots_of(part.iter().copied(), slot_count);
            let mut fold = PackedFold::new(slot_count, &LANES);
            fold.fold(&inputs, &part_slots, &FoldRows::Picked(&indices));
            partials.push(fold);
        }
        let picked_slots = slots_of(picked.iter().copied(), slot_count);
        check(
            &partials,
            &reference(&picked, &picked_slots, &values, &valid, slot_count),
        );
    }

    #[test]
    fn lanes_over_one_column_fold_once_and_agree_with_separate_lanes() {
        use super::super::two_pass::TwoPassLane;
        let rows = 9_000;
        let slot_count = 11;
        let (values, valid) = column(rows);
        // COUNT(*), SUM, AVG, MIN, MAX of one column.
        let packed = [
            Some(PackedLane::Count),
            LANES[1],
            Some(PackedLane::Average {
                digits: 4,
                result_scale: 6,
            }),
            LANES[2],
            LANES[3],
        ];
        let sum = TwoPassLane::DecimalUnits {
            column: 3,
            scale: 2,
            float_output: false,
        };
        let extreme = TwoPassLane::ExtremeDecimal {
            column: 3,
            scale: 2,
        };
        let lanes = [TwoPassLane::CountStar, sum, sum, extreme, extreme];
        let inputs = [
            LaneInput::Count,
            LaneInput::Units(&values, &valid),
            LaneInput::Units(&values, &valid),
            LaneInput::Units(&values, &valid),
            LaneInput::Units(&values, &valid),
        ];
        let listed = (0..rows)
            .filter(|row| row % 5 != 1)
            .map(|row| u32::try_from(row).expect("small"))
            .collect::<Vec<_>>();
        let listed_slots = slots_of(listed.iter().map(|row| *row as usize), slot_count);
        let span_slots = slots_of(0..rows, slot_count);
        let mut shared = PackedFold::sharing(slot_count, &packed, &lanes);
        let mut separate = PackedFold::new(slot_count, &packed);
        for fold in [&mut shared, &mut separate] {
            fold.fold(&inputs, &listed_slots, &FoldRows::Picked(&listed));
            fold.fold(&inputs, &span_slots, &FoldRows::Span(0..rows));
        }
        let mut merged = PackedFold::sharing(slot_count, &packed, &lanes);
        merged.merge_from(&shared, |slot| slot);
        for slot in 0..slot_count {
            for index in 0..packed.len() {
                let (_, expected) =
                    merged_cell(std::slice::from_ref(&separate), index, slot).expect("packed");
                for folds in [std::slice::from_ref(&shared), std::slice::from_ref(&merged)] {
                    let (_, cell) = merged_cell(folds, index, slot).expect("packed");
                    assert_eq!(cell.rows, expected.rows, "slot {slot} lane {index} rows");
                    assert_eq!(cell.total, expected.total, "slot {slot} lane {index} total");
                }
            }
        }
    }

    const COUNT_AND_SUM: [Option<PackedLane>; 2] = [LANES[0], LANES[1]];

    /// Per slot: rows, the exact total and the non-NULL rows.
    fn totals_of(
        rows: &[usize],
        slots: &[u32],
        values: &[i64],
        valid: &ValidityMask,
        slot_count: usize,
    ) -> Vec<(u64, i128, u64)> {
        let mut expected = vec![(0, 0, 0); slot_count];
        for (&row, &slot) in rows.iter().zip(slots) {
            let entry = &mut expected[slot as usize];
            entry.0 += 1;
            if valid.is_valid(row) {
                entry.1 += i128::from(values[row]);
                entry.2 += 1;
            }
        }
        expected
    }

    fn check_totals(folds: &[PackedFold], expected: &[(u64, i128, u64)]) {
        for (slot, (rows, total, non_null)) in expected.iter().enumerate() {
            let (_, count) = merged_cell(folds, 0, slot).expect("count lane");
            let (_, sum) = merged_cell(folds, 1, slot).expect("sum lane");
            assert_eq!(count.rows, *rows, "slot {slot} rows");
            assert_eq!(sum.rows, *non_null, "slot {slot} summed rows");
            assert_eq!(sum.total, *total, "slot {slot} total");
            assert_eq!(
                super::occupied_in(folds, slot),
                *rows > 0,
                "slot {slot} occupied"
            );
        }
    }

    #[test]
    fn a_count_beside_a_sum_shares_its_cells_and_other_shapes_do_not() {
        // COUNT(*) with SUM, and SUM alone, pair; lanes with no sum keep
        // their own count array.
        assert!(PackedFold::new(8, &COUNT_AND_SUM).is_paired());
        assert!(PackedFold::new(8, &[LANES[1]]).is_paired());
        assert!(PackedFold::new(8, &LANES).is_paired());
        assert!(!PackedFold::new(8, &[LANES[0]]).is_paired());
        assert!(!PackedFold::new(8, &[LANES[0], LANES[2], LANES[3]]).is_paired());
        assert!(!PackedFold::new(8, &[LANES[0], Some(PackedLane::Present)]).is_paired());
    }

    #[test]
    fn paired_sums_stay_exact_for_null_negative_sparse_and_unordered_rows() {
        // Sparse slots (most of 5,000 never reached), slots met in no
        // order, negative values, NULL values, and one group taking half
        // of the rows.
        let rows = 40_000;
        let slot_count = 5_000;
        let (values, valid) = column(rows);
        let slots = (0..rows)
            .map(|row| {
                if row % 2 == 0 {
                    4_999
                } else {
                    u32::try_from((row * 7_907) % 311 * 16).expect("small")
                }
            })
            .collect::<Vec<_>>();
        let all = (0..rows).collect::<Vec<_>>();
        let inputs = [LaneInput::Count, LaneInput::Units(&values, &valid)];
        // Three workers taking the chunks out of order.
        let mut partials = (0..3)
            .map(|_| PackedFold::new(slot_count, &COUNT_AND_SUM))
            .collect::<Vec<_>>();
        let chunks = (0..rows).step_by(1_000).collect::<Vec<_>>();
        for (turn, start) in chunks.iter().rev().enumerate() {
            let span = *start..start + 1_000;
            partials[turn % 3].fold(&inputs, &slots[span.clone()], &FoldRows::Span(span));
        }
        assert!(partials.iter().all(PackedFold::is_paired));
        let expected = totals_of(&all, &slots, &values, &valid, slot_count);
        check_totals(&partials, &expected);
        // Merged whole, and shifted as a re-based range shifts them.
        let mut merged = PackedFold::new(slot_count + 10, &COUNT_AND_SUM);
        for partial in &partials {
            merged.merge_from(partial, |slot| slot + 10);
        }
        assert!(merged.is_paired());
        let mut shifted = vec![(0, 0, 0); 10];
        shifted.extend(expected);
        check_totals(std::slice::from_ref(&merged), &shifted);
    }

    #[test]
    fn a_sum_that_may_leave_64_bits_widens_and_stays_exact() {
        let slot_count = 6;
        let valid = ValidityMask::all_valid(8);
        let small = [5_i64, -7, 11, -13, 17, -19, 23, -29];
        let slots = [0_u32, 1, 2, 3, 0, 1, 2, 5];
        let rows = (0..8).collect::<Vec<_>>();
        for huge in [i64::MAX, i64::MIN, i64::MAX / 2 + 1] {
            let large = [huge; 8];
            let mut fold = PackedFold::new(slot_count, &COUNT_AND_SUM);
            let mut expected = vec![(0, 0, 0); slot_count];
            let add = |values: &[i64], expected: &mut [(u64, i128, u64)]| {
                for (slot, total) in totals_of(&rows, &slots, values, &valid, slot_count)
                    .into_iter()
                    .enumerate()
                {
                    expected[slot].0 += total.0;
                    expected[slot].1 += total.1;
                    expected[slot].2 += total.2;
                }
            };
            // Small values keep the pairing; the first batch whose values
            // could carry a sum past 64 bits ends it, before it is folded.
            fold.fold(
                &[LaneInput::Count, LaneInput::Units(&small, &valid)],
                &slots,
                &FoldRows::Span(0..8),
            );
            add(&small, &mut expected);
            assert!(fold.is_paired(), "{huge}");
            check_totals(std::slice::from_ref(&fold), &expected);
            for _ in 0..3 {
                fold.fold(
                    &[LaneInput::Count, LaneInput::Units(&large, &valid)],
                    &slots,
                    &FoldRows::Span(0..8),
                );
                add(&large, &mut expected);
            }
            assert!(!fold.is_paired(), "{huge}");
            // Slot 0 holds six of the huge values: past 64 bits.
            assert!(i64::try_from(expected[0].1).is_err(), "{huge}");
            check_totals(std::slice::from_ref(&fold), &expected);
            // A paired partial beside the widened one, and the two merged.
            let mut other = PackedFold::new(slot_count, &COUNT_AND_SUM);
            other.fold(
                &[LaneInput::Count, LaneInput::Units(&small, &valid)],
                &slots,
                &FoldRows::Span(0..8),
            );
            add(&small, &mut expected);
            assert!(other.is_paired());
            let folds = [fold, other];
            check_totals(&folds, &expected);
            let mut merged = PackedFold::new(slot_count, &COUNT_AND_SUM);
            merged.merge_from(&folds[1], |slot| slot);
            assert!(merged.is_paired());
            merged.merge_from(&folds[0], |slot| slot);
            assert!(!merged.is_paired());
            check_totals(std::slice::from_ref(&merged), &expected);
        }
    }

    #[test]
    fn paired_partials_whose_bounds_do_not_fit_together_merge_widened() {
        // Each partial's sums fit 64 bits; the two together may not.
        let valid = ValidityMask::all_valid(4);
        let values = [i64::MAX / 8; 4];
        let slots = [1_u32, 1, 1, 1];
        let inputs = [LaneInput::Count, LaneInput::Units(&values, &valid)];
        let partial = || {
            let mut fold = PackedFold::new(3, &COUNT_AND_SUM);
            fold.fold(&inputs, &slots, &FoldRows::Span(0..4));
            assert!(fold.is_paired());
            fold
        };
        let folds = [partial(), partial(), partial()];
        let mut merged = PackedFold::new(3, &COUNT_AND_SUM);
        for fold in &folds {
            merged.merge_from(fold, |slot| slot);
        }
        assert!(!merged.is_paired());
        let total = i128::from(i64::MAX / 8) * 12;
        assert!(i64::try_from(total).is_err());
        let expected = [(0, 0, 0), (12, total, 12), (0, 0, 0)];
        check_totals(std::slice::from_ref(&merged), &expected);
        check_totals(&folds, &expected);
    }

    #[test]
    fn rows_folded_one_at_a_time_pair_and_widen_like_columns() {
        use super::super::two_pass::LaneReader;
        let valid = ValidityMask::from_bools(&[true, true, false, true, true, true]);
        let values = [9_i64, -4, 100, i64::MAX, i64::MAX, -6];
        let slots = [2_u32, 0, 2, 2, 2, 0];
        let readers = [LaneReader::CountStar, LaneReader::Int64(&values, &valid)];
        let mut fold = PackedFold::new(3, &COUNT_AND_SUM);
        for (row, slot) in slots.iter().enumerate().take(3) {
            fold.add_row(*slot as usize, &readers, row);
        }
        assert!(fold.is_paired());
        for (row, slot) in slots.iter().enumerate().skip(3) {
            fold.add_row(*slot as usize, &readers, row);
        }
        assert!(!fold.is_paired());
        let rows = (0..6).collect::<Vec<_>>();
        check_totals(
            std::slice::from_ref(&fold),
            &totals_of(&rows, &slots, &values, &valid, 3),
        );
    }

    /// Kernel measurement, ignored by default:
    /// `cargo test --release -p pintail-exec --lib packed_fold::tests::kernel -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    fn kernel_against_row_at_a_time() {
        let rows = 1 << 22;
        let (values, _) = column(rows);
        let valid = ValidityMask::all_valid(rows);
        let lanes = [LANES[0], LANES[1]];
        for slot_count in [5, 1_025, 100_001] {
            let slots = slots_of(0..rows, slot_count);
            let inputs = [LaneInput::Count, LaneInput::Units(&values, &valid)];
            let mut best_fold = f64::MAX;
            let mut best_rows = f64::MAX;
            for _ in 0..5 {
                let mut fold = PackedFold::new(slot_count, &lanes);
                let started = std::time::Instant::now();
                for start in (0..rows).step_by(4_096) {
                    let end = (start + 4_096).min(rows);
                    fold.fold(&inputs, &slots[start..end], &FoldRows::Span(start..end));
                }
                best_fold = best_fold.min(started.elapsed().as_secs_f64());
                std::hint::black_box(&fold);
                // The shape this replaced: per row and lane, a reader match,
                // a lane match and a checked add returning a Result.
                let mut cells = vec![(0_i128, 0_u64); slot_count * 2];
                let started = std::time::Instant::now();
                for (row, slot) in slots.iter().enumerate() {
                    for (index, lane) in lanes.iter().enumerate() {
                        let bits = match index {
                            0 => Some(0),
                            _ => valid.is_valid(row).then_some(values[row]),
                        };
                        let cell = &mut cells[*slot as usize * 2 + index];
                        if let Some(bits) = std::hint::black_box(bits) {
                            let added: Result<(), ()> = match lane {
                                Some(PackedLane::Count) => Ok(()),
                                _ => cell
                                    .0
                                    .checked_add(i128::from(bits))
                                    .map(|total| cell.0 = total)
                                    .ok_or(()),
                            };
                            added.expect("no overflow");
                            cell.1 += 1;
                        }
                    }
                }
                best_rows = best_rows.min(started.elapsed().as_secs_f64());
                std::hint::black_box(&cells);
            }
            #[allow(clippy::cast_precision_loss)]
            let per_row = |seconds: f64| seconds * 1e9 / rows as f64;
            eprintln!(
                "{slot_count} slots: fold {:.2} ns/row, row-at-a-time {:.2} ns/row",
                per_row(best_fold),
                per_row(best_rows)
            );
        }
    }
}
