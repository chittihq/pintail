//! A columnar fold for the fused inner join-aggregate's commonest shape: a
//! build key that is a unique integer (an auto-increment primary key, dense
//! or with gaps of any width), and
//! aggregates that are counts, integer sums and exact decimal sums or
//! averages of probe columns.
//!
//! The row fold resolved each probe row's group through two indirections -
//! the dense slot, then that bucket's list of group indexes - and updated
//! every aggregate through the general typed-column path, which looks the
//! column up, matches its representation and dispatches on the aggregate
//! function once per row per aggregate. That update was a quarter of the
//! whole profile of a star join grouped by a dimension column. Here a key
//! resolves to its group with one array index, each aggregate folds into
//! plain per-group integers, and the states see one update per group per
//! morsel. Every fold here is exact integer addition, so the states end
//! where the row fold leaves them.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use pintail_sql::AggregateFunction;

use super::aggregate::{
    AggregateState, CompiledAggregate, aggregate_uses_float, decimal_average_scale,
};
use super::join::PartitionedBuild;
use super::morsel::Morsel;
use super::sparse_keys::SparseKeyTable;
use super::{ExecError, MemoryTracker};
use crate::array::ValidityMask;
use crate::batch::{DecimalUnits, TypedValues};
use crate::expression::CompiledExpr;

/// Probe rows resolved before the aggregates fold them: small enough that
/// the row and group buffers stay in L1.
const CHUNK_ROWS: usize = 2_048;

/// Widest decimal-average widening folded as one partial total; the same
/// bound the packed aggregate lanes use.
const AVERAGE_MAX_DIGITS: u8 = 19;

/// Each build key's one group, for a build whose every key names exactly
/// one row: by the key's offset where the keys are dense, through a hash
/// table of the keys where they are spread.
///
/// A slot without a key - and any probe row that matches nothing - resolves
/// to the miss group one past the real ones, so the fold adds every row
/// somewhere and needs no branch to skip the misses. An inner join discards
/// that group; an outer join's unmatched rows are exactly it.
pub(super) struct UniqueKeyGroups {
    keys: GroupKeys,
    miss: u32,
}

enum GroupKeys {
    Direct { minimum: i128, groups: Vec<u32> },
    Sparse(SparseKeyTable),
}

impl UniqueKeyGroups {
    /// Why not, unless the build finalized to a flat table and every
    /// bucket holds one row: a key with several rows folds each of them,
    /// which is the row fold's job.
    pub(super) fn resolve(
        build: &PartitionedBuild,
        dense_group_indexes: &[Option<&[usize]>],
        group_count: usize,
    ) -> Result<Self, &'static str> {
        const TOO_MANY_GROUPS: &str = "more groups than the key table addresses";
        let miss = u32::try_from(group_count)
            .ok()
            .filter(|miss| *miss < u32::MAX - 1)
            .ok_or(TOO_MANY_GROUPS)?;
        let group_of = |bucket: usize| match dense_group_indexes.get(bucket).copied().flatten() {
            None | Some([]) => Ok(miss),
            Some([group]) => u32::try_from(*group)
                .ok()
                .filter(|g| *g < miss)
                .ok_or(TOO_MANY_GROUPS),
            Some(_) => Err("a build key with several rows"),
        };
        if let Some(table) = build.sparse_layout() {
            return Ok(Self {
                keys: GroupKeys::Sparse(table.remapped(group_of)?),
                miss,
            });
        }
        let (minimum, slots) = build
            .dense_layout()
            .ok_or("a build key that is not a packed integer column")?;
        let mut groups = Vec::with_capacity(slots.len());
        for slot in slots {
            groups.push(match slot.checked_sub(1) {
                None => miss,
                Some(bucket) => group_of(bucket as usize)?,
            });
        }
        Ok(Self {
            keys: GroupKeys::Direct { minimum, groups },
            miss,
        })
    }

    /// Bytes the key table holds.
    pub(super) fn bytes(&self) -> usize {
        match &self.keys {
            GroupKeys::Direct { groups, .. } => groups.len().saturating_mul(size_of::<u32>()),
            GroupKeys::Sparse(table) => table.bytes(),
        }
    }

    /// Each value's group into `out`, NULL keys to the miss group.
    #[inline]
    fn resolve_into<T: Copy>(&self, values: &[T], valid: Option<&[bool]>, out: &mut Vec<u32>)
    where
        i128: From<T>,
    {
        out.clear();
        let miss = self.miss;
        match &self.keys {
            GroupKeys::Direct { minimum, groups } => {
                out.extend(values.iter().map(|value| {
                    usize::try_from(i128::from(*value).wrapping_sub(*minimum))
                        .ok()
                        .and_then(|offset| groups.get(offset).copied())
                        .unwrap_or(miss)
                }));
            }
            GroupKeys::Sparse(table) => {
                out.extend(values.iter().map(|value| {
                    table
                        .find(i128::from(*value))
                        .map_or(miss, |group| u32::try_from(group).unwrap_or(miss))
                }));
            }
        }
        self.miss_invalid(valid, out);
    }

    /// Sends the rows `valid` marks NULL to the miss group.
    #[inline]
    fn miss_invalid(&self, valid: Option<&[bool]>, out: &mut [u32]) {
        if let Some(valid) = valid {
            for (group, valid) in out.iter_mut().zip(valid) {
                if !valid {
                    *group = self.miss;
                }
            }
        }
    }

    /// The dense table and its smallest key in the key column's own 64
    /// bits, when that key is one of them. A key at or above it has its
    /// offset by one subtraction in those bits, and one bounds check then
    /// says whether the table holds it, where the 128-bit difference took
    /// a borrow and two tests for every row.
    fn direct<T: TryFrom<i128>>(&self) -> Option<(T, &[u32])> {
        match &self.keys {
            GroupKeys::Direct { minimum, groups } => {
                Some((T::try_from(*minimum).ok()?, groups.as_slice()))
            }
            GroupKeys::Sparse(_) => None,
        }
    }

    /// Each offset's group into `out`: the table's entry, or the miss group
    /// for an offset past its end.
    #[inline]
    fn resolve_offsets(
        &self,
        groups: &[u32],
        offsets: impl Iterator<Item = u64>,
        valid: Option<&[bool]>,
        out: &mut Vec<u32>,
    ) {
        let miss = self.miss;
        out.clear();
        out.extend(offsets.map(|offset| {
            usize::try_from(offset)
                .ok()
                .and_then(|offset| groups.get(offset).copied())
                .unwrap_or(miss)
        }));
        self.miss_invalid(valid, out);
    }
}

/// How one aggregate folds, decided once per query.
#[derive(Clone, Copy)]
pub(super) enum Lane {
    /// `COUNT(*)`: the group's matched rows.
    CountRows,
    /// `COUNT(column)`: the matched rows where the column is not NULL.
    CountValid { column: usize },
    /// `SUM(column)` of a decimal column, on its scaled units.
    DecimalSum { column: usize, float_output: bool },
    /// `SUM(column)` typed as the column's own integer type: signed when
    /// `signed`, and the same checked addition the row fold does.
    IntegerSum { column: usize, signed: bool },
    /// Exact `AVG(column)` of a decimal column at `result_scale`.
    DecimalAverage { column: usize, result_scale: u8 },
}

/// The lane of every aggregate, or `None` when any of them needs the row
/// fold: a DISTINCT, a build-side argument, or a function without a lane.
pub(super) fn plan_lanes(aggregates: &[CompiledAggregate], left_width: usize) -> Option<Vec<Lane>> {
    aggregates
        .iter()
        .map(|aggregate| {
            if aggregate.distinct {
                return None;
            }
            let column = match &aggregate.expr {
                None => None,
                Some(expression) => Some(expression.column_index().filter(|c| *c < left_width)?),
            };
            match (aggregate.function, column) {
                (AggregateFunction::Count, None) => Some(Lane::CountRows),
                (AggregateFunction::Count, Some(column)) => Some(Lane::CountValid { column }),
                (AggregateFunction::Sum, Some(column)) => Some(match aggregate.sum_carrier {
                    Some(pintail_types::DataType::Int64) => Lane::IntegerSum {
                        column,
                        signed: true,
                    },
                    Some(pintail_types::DataType::UInt64) => Lane::IntegerSum {
                        column,
                        signed: false,
                    },
                    _ => Lane::DecimalSum {
                        column,
                        float_output: aggregate_uses_float(aggregate),
                    },
                }),
                (AggregateFunction::Average, Some(column)) => {
                    decimal_average_scale(aggregate).map(|result_scale| Lane::DecimalAverage {
                        column,
                        result_scale,
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// One lane's column in this morsel's batch.
enum LaneInput<'a> {
    Rows,
    Valid(&'a ValidityMask),
    Units {
        units: Units<'a>,
        validity: &'a ValidityMask,
        scale: u8,
    },
}

/// A decimal column's scaled units: 64-bit as the store decodes them, or
/// 128-bit as a batch rebuilt from row values (memtable rows, a small
/// materialized range) parses them; or an unsigned integer column's values
/// as the units of a scale-zero sum.
#[derive(Clone, Copy)]
enum Units<'a> {
    Narrow(&'a [i64]),
    Unsigned(&'a [u64]),
    Wide(&'a [i128]),
}

/// The probe key column as integers, when it is one.
enum Keys<'a> {
    Signed(&'a [i64]),
    Unsigned(&'a [u64]),
}

impl Keys<'_> {
    /// The groups of `rows` - a contiguous range or picked rows - into `out`.
    fn resolve(
        &self,
        table: &UniqueKeyGroups,
        rows: &Rows<'_>,
        valid: Option<&[bool]>,
        scratch: &mut Vec<i128>,
        out: &mut Vec<u32>,
    ) {
        // A key below the smallest has no offset: past any table's end.
        let signed = |key: i64, minimum: i64| {
            if key >= minimum {
                u64::from_ne_bytes(key.wrapping_sub(minimum).to_ne_bytes())
            } else {
                u64::MAX
            }
        };
        let unsigned = |key: u64, minimum: u64| key.checked_sub(minimum).unwrap_or(u64::MAX);
        match (self, rows, table.direct::<i64>(), table.direct::<u64>()) {
            (Self::Signed(values), Rows::Range(range), Some((minimum, groups)), _) => {
                let offsets = values[range.clone()]
                    .iter()
                    .map(|key| signed(*key, minimum));
                table.resolve_offsets(groups, offsets, valid, out);
            }
            (Self::Unsigned(values), Rows::Range(range), _, Some((minimum, groups))) => {
                let offsets = values[range.clone()]
                    .iter()
                    .map(|key| unsigned(*key, minimum));
                table.resolve_offsets(groups, offsets, valid, out);
            }
            (Self::Signed(values), Rows::Picked(rows), Some((minimum, groups)), _) => {
                let offsets = rows
                    .iter()
                    .map(|row| signed(values[*row as usize], minimum));
                table.resolve_offsets(groups, offsets, valid, out);
            }
            (Self::Unsigned(values), Rows::Picked(rows), _, Some((minimum, groups))) => {
                let offsets = rows
                    .iter()
                    .map(|row| unsigned(values[*row as usize], minimum));
                table.resolve_offsets(groups, offsets, valid, out);
            }
            (Self::Signed(values), Rows::Range(range), ..) => {
                table.resolve_into(&values[range.clone()], valid, out);
            }
            (Self::Unsigned(values), Rows::Range(range), ..) => {
                table.resolve_into(&values[range.clone()], valid, out);
            }
            (Self::Signed(values), Rows::Picked(rows), ..) => {
                scratch.clear();
                scratch.extend(rows.iter().map(|row| i128::from(values[*row as usize])));
                table.resolve_into(scratch, valid, out);
            }
            (Self::Unsigned(values), Rows::Picked(rows), ..) => {
                scratch.clear();
                scratch.extend(rows.iter().map(|row| i128::from(values[*row as usize])));
                table.resolve_into(scratch, valid, out);
            }
        }
    }
}

/// The rows of one chunk.
enum Rows<'a> {
    Range(std::ops::Range<usize>),
    Picked(&'a [u32]),
}

impl Rows<'_> {
    /// Each row's validity under `mask`, or `None` when every row is valid.
    fn validity(&self, mask: &ValidityMask, out: &mut Vec<bool>) -> bool {
        if mask.no_nulls() {
            return false;
        }
        out.clear();
        match self {
            Self::Range(range) => out.extend(range.clone().map(|row| mask.is_valid(row))),
            Self::Picked(rows) => out.extend(rows.iter().map(|row| mask.is_valid(*row as usize))),
        }
        true
    }
}

/// Adds each row's units into its group's total.
#[inline]
fn add_units(totals: &mut [i128], groups: &[u32], units: Units<'_>, rows: &Rows<'_>) {
    match units {
        Units::Narrow(units) => add_unit_values(totals, groups, units, rows),
        Units::Unsigned(units) => add_unit_values(totals, groups, units, rows),
        Units::Wide(units) => add_unit_values(totals, groups, units, rows),
    }
}

#[inline]
fn add_unit_values<T: Copy>(totals: &mut [i128], groups: &[u32], units: &[T], rows: &Rows<'_>)
where
    i128: From<T>,
{
    match rows {
        Rows::Range(range) => {
            for (group, units) in groups.iter().zip(&units[range.clone()]) {
                totals[*group as usize] += i128::from(*units);
            }
        }
        Rows::Picked(rows) => {
            for (group, row) in groups.iter().zip(*rows) {
                totals[*group as usize] += i128::from(units[*row as usize]);
            }
        }
    }
}

/// The scale each unit lane's column carries, fixed by the first morsel
/// that reads it: one lane's totals only add up at one scale, across every
/// morsel and worker that folds into them.
pub(super) struct LaneScales(Vec<AtomicU16>);

/// A lane no morsel has read yet.
const NO_SCALE: u16 = u16::MAX;

impl LaneScales {
    pub(super) fn new(lanes: usize) -> Self {
        Self((0..lanes).map(|_| AtomicU16::new(NO_SCALE)).collect())
    }

    /// Whether `lane` folds at `scale`: the first morsel to ask decides.
    fn agree(&self, lane: usize, scale: u8) -> bool {
        match self.0[lane].compare_exchange(
            NO_SCALE,
            u16::from(scale),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => true,
            Err(existing) => existing == u16::from(scale),
        }
    }

    fn get(&self, lane: usize) -> Option<u8> {
        u8::try_from(self.0[lane].load(Ordering::Relaxed)).ok()
    }
}

/// Running totals of every lane for every group, indexed by the group's
/// position in the plan: the build-side groups, then the miss group, then a
/// sink for the rows a lane's NULL argument leaves out of that lane.
///
/// Nothing here depends on the morsel being folded, so one of these serves
/// a worker for a whole query: its arrays are sized by the build side's
/// groups, which are fixed before the first probe row is read.
pub(super) struct LaneTotals {
    slots: usize,
    /// Probe rows that reached each group.
    hits: Vec<u64>,
    /// Lane after lane, each group's summed units.
    totals: Vec<i128>,
    /// Lane after lane, each group's rows whose argument was NULL.
    null_rows: Vec<u64>,
    chunk_groups: Vec<u32>,
    lane_groups: Vec<u32>,
    picked: Vec<u32>,
    valid: Vec<bool>,
    scratch: Vec<i128>,
}

impl LaneTotals {
    /// Bytes one of these holds for `keys` and `lanes` lanes.
    pub(super) fn bytes(keys: &UniqueKeyGroups, lanes: usize) -> usize {
        let slots = keys.miss as usize + 2;
        slots
            .saturating_mul(size_of::<u64>().saturating_add(
                lanes.saturating_mul(size_of::<i128>().saturating_add(size_of::<u64>())),
            ))
            .saturating_add(
                CHUNK_ROWS.saturating_mul(3 * size_of::<u32>() + size_of::<bool>() + 16),
            )
    }

    pub(super) fn new(keys: &UniqueKeyGroups, lanes: usize) -> Self {
        let slots = keys.miss as usize + 2;
        Self {
            slots,
            hits: vec![0; slots],
            totals: vec![0; slots.saturating_mul(lanes)],
            null_rows: vec![0; slots.saturating_mul(lanes)],
            chunk_groups: Vec::with_capacity(CHUNK_ROWS),
            lane_groups: Vec::with_capacity(CHUNK_ROWS),
            picked: Vec::with_capacity(CHUNK_ROWS),
            valid: Vec::with_capacity(CHUNK_ROWS),
            scratch: Vec::new(),
        }
    }

    /// Adds another worker's totals, group by group.
    pub(super) fn absorb(&mut self, other: &Self) -> Result<(), ExecError> {
        if self.slots != other.slots || self.totals.len() != other.totals.len() {
            return Err(ExecError::InvalidPhysicalPlan(
                "fused join totals of two shapes",
            ));
        }
        for (mine, theirs) in self.hits.iter_mut().zip(&other.hits) {
            *mine = mine
                .checked_add(*theirs)
                .ok_or(ExecError::NumericOverflow)?;
        }
        for (mine, theirs) in self.totals.iter_mut().zip(&other.totals) {
            *mine = mine
                .checked_add(*theirs)
                .ok_or(ExecError::NumericOverflow)?;
        }
        for (mine, theirs) in self.null_rows.iter_mut().zip(&other.null_rows) {
            *mine = mine
                .checked_add(*theirs)
                .ok_or(ExecError::NumericOverflow)?;
        }
        Ok(())
    }

    /// Probe rows folded into `group`.
    pub(super) fn rows(&self, group: usize) -> u64 {
        self.hits[group]
    }

    /// Adds `group`'s totals to its aggregate states.
    pub(super) fn apply(
        &self,
        lanes: &[Lane],
        scales: &LaneScales,
        group: usize,
        states: &mut [AggregateState],
    ) -> Result<(), ExecError> {
        const LOST: ExecError = ExecError::InvalidPhysicalPlan("a fused join lane lost its column");
        let rows = self.hits[group];
        for (lane_index, (lane, state)) in lanes.iter().zip(states.iter_mut()).enumerate() {
            let total = self.totals[lane_index * self.slots + group];
            let valid = rows - self.null_rows[lane_index * self.slots + group];
            match *lane {
                Lane::CountRows => state.add_dense_count(rows)?,
                Lane::CountValid { .. } => state.add_dense_count(valid)?,
                Lane::DecimalSum { float_output, .. } => {
                    if valid > 0 {
                        let scale = scales.get(lane_index).ok_or(LOST)?;
                        state.update_decimal_sum_units(total, scale, float_output)?;
                    }
                }
                Lane::IntegerSum { signed, .. } => {
                    if valid > 0 {
                        // The total, exact in 128 bits, joins the state
                        // as it is: a morsel's total may be outside 64
                        // bits while the group's is not, and a DECIMAL
                        // answer has no range to leave.
                        state.add_integer_exact(total, !signed)?;
                    }
                }
                Lane::DecimalAverage { result_scale, .. } => {
                    if valid > 0 {
                        let scale = scales.get(lane_index).ok_or(LOST)?;
                        state.add_decimal_average_partial(
                            total,
                            result_scale - scale,
                            result_scale,
                            valid,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Folds `morsel` into `kept` through the unique-key table: the build-side
/// groups, then - last - the miss group, which takes the rows that match
/// nothing. With nothing folded, the reason a column of this batch is not
/// in a representation a lane reads - the row fold then takes the morsel,
/// and the profile names the reason.
#[allow(clippy::too_many_lines)]
pub(super) fn fold_morsel(
    morsel: &Morsel<'_>,
    left_key: &CompiledExpr,
    keys: &UniqueKeyGroups,
    lanes: &[Lane],
    scales: &LaneScales,
    kept: &mut LaneTotals,
    memory: &MemoryTracker,
) -> Result<Option<&'static str>, ExecError> {
    let batch = morsel.batch;
    let Some((key_values, key_validity)) = left_key
        .column_index()
        .and_then(|column| batch.column(column))
        .and_then(crate::ColumnVector::typed)
    else {
        return Ok(Some("a probe key that is not one packed column"));
    };
    let key_values = match key_values {
        TypedValues::Int64(values) => Keys::Signed(values),
        TypedValues::UInt64(values) => Keys::Unsigned(values),
        _ => return Ok(Some("a probe key that is not packed integers")),
    };
    let mut inputs = Vec::with_capacity(lanes.len());
    for (lane_index, lane) in lanes.iter().enumerate() {
        let input = match *lane {
            Lane::CountRows => LaneInput::Rows,
            Lane::CountValid { column } => {
                let Some((_, validity)) = batch.column(column).and_then(crate::ColumnVector::typed)
                else {
                    return Ok(Some("a counted column that is not packed"));
                };
                LaneInput::Valid(validity)
            }
            Lane::DecimalSum { column, .. } | Lane::DecimalAverage { column, .. } => {
                let Some(vector) = batch.column(column) else {
                    return Ok(Some("a summed column outside the batch"));
                };
                // Wide units of a column declared with at most eighteen
                // digits each fit 64 bits, so their totals stay exactly as
                // far from overflow as the narrow units' do.
                let narrow_digits = matches!(
                    vector.data_type(),
                    pintail_types::DataType::Decimal { precision, .. } if precision <= 18
                );
                match vector.typed() {
                    Some((
                        TypedValues::Decimal128 {
                            values: DecimalUnits::Narrow(units),
                            scale,
                            ..
                        },
                        validity,
                    )) => LaneInput::Units {
                        units: Units::Narrow(units),
                        validity,
                        scale: *scale,
                    },
                    Some((
                        TypedValues::Decimal128 {
                            values: DecimalUnits::Wide(units),
                            scale,
                            ..
                        },
                        validity,
                    )) if narrow_digits => LaneInput::Units {
                        units: Units::Wide(units),
                        validity,
                        scale: *scale,
                    },
                    Some((TypedValues::Decimal128 { .. }, _)) => {
                        return Ok(Some("wide decimal units of a column past 18 digits"));
                    }
                    Some(_) => return Ok(Some("a summed column that is not packed units")),
                    None => return Ok(Some("a summed column of mixed row values")),
                }
            }
            Lane::IntegerSum { column, signed } => {
                match batch.column(column).and_then(crate::ColumnVector::typed) {
                    Some((TypedValues::Int64(values), validity)) if signed => LaneInput::Units {
                        units: Units::Narrow(values),
                        validity,
                        scale: 0,
                    },
                    Some((TypedValues::UInt64(values), validity)) if !signed => LaneInput::Units {
                        units: Units::Unsigned(values),
                        validity,
                        scale: 0,
                    },
                    Some(_) => return Ok(Some("a summed column of another integer type")),
                    None => return Ok(Some("a summed column of mixed row values")),
                }
            }
        };
        if let (Lane::DecimalAverage { result_scale, .. }, LaneInput::Units { scale, .. }) =
            (lane, &input)
            && result_scale
                .checked_sub(*scale)
                .is_none_or(|digits| digits > AVERAGE_MAX_DIGITS)
        {
            return Ok(Some("an average widened past 19 digits"));
        }
        if let LaneInput::Units { scale, .. } = &input
            && !scales.agree(lane_index, *scale)
        {
            return Ok(Some("a summed column whose scale changed"));
        }
        inputs.push(input);
    }

    let sink = keys.miss + 1;
    let slots = kept.slots;
    if slots != keys.miss as usize + 2 || kept.totals.len() != slots.saturating_mul(lanes.len()) {
        return Err(ExecError::InvalidPhysicalPlan(
            "fused join totals sized for another key table",
        ));
    }
    let mut chunk_groups = std::mem::take(&mut kept.chunk_groups);
    let mut lane_groups = std::mem::take(&mut kept.lane_groups);
    let mut picked = std::mem::take(&mut kept.picked);
    let mut valid = std::mem::take(&mut kept.valid);
    let mut scratch = std::mem::take(&mut kept.scratch);
    let hits = &mut kept.hits;
    let totals = &mut kept.totals;
    let null_rows = &mut kept.null_rows;
    let contiguous = morsel.selected_count() == morsel.rows.len();
    let mut selected = morsel.selected_rows();
    let mut next = morsel.rows.start;
    loop {
        memory.check_interruption()?;
        let rows = if contiguous {
            let end = next.saturating_add(CHUNK_ROWS).min(morsel.rows.end);
            if next >= end {
                break;
            }
            let range = next..end;
            next = end;
            Rows::Range(range)
        } else {
            picked.clear();
            for row in selected.by_ref().take(CHUNK_ROWS) {
                picked.push(u32::try_from(row).map_err(|_| {
                    ExecError::InvalidBatch("a probe batch holds more rows than a join can address")
                })?);
            }
            if picked.is_empty() {
                break;
            }
            Rows::Picked(&picked)
        };
        let key_valid = rows.validity(key_validity, &mut valid);
        key_values.resolve(
            keys,
            &rows,
            key_valid.then_some(valid.as_slice()),
            &mut scratch,
            &mut chunk_groups,
        );
        for group in &chunk_groups {
            hits[*group as usize] += 1;
        }
        for (lane, input) in inputs.iter().enumerate() {
            let lane_slots = lane * slots..(lane + 1) * slots;
            let (validity, units) = match input {
                LaneInput::Rows => continue,
                LaneInput::Valid(validity) => (*validity, None),
                LaneInput::Units {
                    units, validity, ..
                } => (*validity, Some(*units)),
            };
            // A NULL argument's row moves to the sink for this lane alone,
            // and is counted against its group's valid rows; the other
            // lanes still see it.
            let groups = if rows.validity(validity, &mut valid) {
                let nulls = &mut null_rows[lane_slots.clone()];
                lane_groups.clear();
                for (group, valid) in chunk_groups.iter().zip(&valid) {
                    if *valid {
                        lane_groups.push(*group);
                    } else {
                        nulls[*group as usize] += 1;
                        lane_groups.push(sink);
                    }
                }
                &lane_groups
            } else {
                &chunk_groups
            };
            if let Some(units) = units {
                add_units(&mut totals[lane_slots], groups, units, &rows);
            }
        }
    }
    kept.chunk_groups = chunk_groups;
    kept.lane_groups = lane_groups;
    kept.picked = picked;
    kept.valid = valid;
    kept.scratch = scratch;
    Ok(None)
}

/// Per-worker totals kept for a whole query.
///
/// Folding a morsel used to open with a zeroed set of totals and close by
/// turning every group it touched back into aggregate states, which the
/// caller then merged group by group on one thread. All of that is sized by
/// the build side's groups, not by the morsel: with a hundred thousand
/// groups it cost far more than adding the morsel's rows up. A worker now
/// takes a set from here, folds its morsel into it and hands it back, and
/// the sets are added together by group index once, when the probe ends.
pub(super) struct LanePool<'a> {
    keys: &'a UniqueKeyGroups,
    lanes: &'a [Lane],
    scales: LaneScales,
    idle: Mutex<Vec<LaneTotals>>,
    reserved: AtomicUsize,
}

impl<'a> LanePool<'a> {
    /// A pool for `keys` and `lanes`, or why the query keeps per-morsel
    /// totals: a set per worker has to fit an eighth of the ceiling. An
    /// integer sum pools like any other lane: its total is exact in 128
    /// bits and joins the state as it is, so no morsel's share is judged.
    pub(super) fn plan(
        keys: &'a UniqueKeyGroups,
        lanes: &'a [Lane],
        memory: &MemoryTracker,
    ) -> Result<Self, &'static str> {
        let workers = rayon::current_num_threads().max(1);
        if LaneTotals::bytes(keys, lanes.len()).saturating_mul(workers) > memory.limit() / 8 {
            return Err("a set of totals per worker is past an eighth of the memory ceiling");
        }
        Ok(Self {
            keys,
            lanes,
            scales: LaneScales::new(lanes.len()),
            idle: Mutex::new(Vec::new()),
            reserved: AtomicUsize::new(0),
        })
    }

    /// Folds `morsel` into an idle set of totals, opening - and charging
    /// `memory` for - a new one when every set is in use.
    pub(super) fn fold(
        &self,
        morsel: &Morsel<'_>,
        left_key: &CompiledExpr,
        memory: &MemoryTracker,
    ) -> Result<Option<&'static str>, ExecError> {
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let mut kept = if let Some(kept) = idle {
            kept
        } else {
            let bytes = LaneTotals::bytes(self.keys, self.lanes.len());
            memory.reserve(bytes)?;
            self.reserved.fetch_add(bytes, Ordering::Relaxed);
            LaneTotals::new(self.keys, self.lanes.len())
        };
        let declined = fold_morsel(
            morsel,
            left_key,
            self.keys,
            self.lanes,
            &self.scales,
            &mut kept,
            memory,
        )?;
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(kept);
        Ok(declined)
    }

    /// Bytes charged for the sets opened so far.
    pub(super) fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Every worker's totals added into one, and the scales they are in;
    /// `None` when no morsel was folded.
    pub(super) fn finish(self) -> Result<Option<(LaneTotals, LaneScales)>, ExecError> {
        let mut sets = self
            .idle
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .into_iter();
        let Some(mut total) = sets.next() else {
            return Ok(None);
        };
        for set in sets {
            total.absorb(&set)?;
        }
        Ok(Some((total, self.scales)))
    }
}

/// Aggregate states of every group of the plan, group after group, and
/// which groups a probe row reached.
///
/// The row fold used to open with a copy of every build-side group's values
/// and a fresh set of states, and close by turning the groups a row reached
/// into a map under the collation - all sized by the build side's groups,
/// for every morsel. The states alone are kept here, indexed by the group's
/// position in the plan, and a group's values are copied when the fold
/// ends, for the groups that were reached.
pub(super) struct GroupStates {
    width: usize,
    states: Vec<AggregateState>,
    touched: Vec<bool>,
}

impl GroupStates {
    /// Bytes one of these holds for `groups` groups.
    pub(super) fn bytes(groups: usize, aggregates: usize) -> usize {
        groups.saturating_mul(
            aggregates
                .saturating_mul(size_of::<AggregateState>())
                .saturating_add(size_of::<bool>()),
        )
    }

    pub(super) fn new(groups: usize, aggregates: &[CompiledAggregate]) -> Self {
        let mut states = Vec::with_capacity(groups.saturating_mul(aggregates.len()));
        for _ in 0..groups {
            states.extend(aggregates.iter().map(AggregateState::new));
        }
        Self {
            width: aggregates.len(),
            states,
            touched: vec![false; groups],
        }
    }

    pub(super) fn groups(&self) -> usize {
        self.touched.len()
    }

    /// `group`'s states, marked as reached.
    #[inline]
    pub(super) fn reach(&mut self, group: usize) -> &mut [AggregateState] {
        self.touched[group] = true;
        &mut self.states[group * self.width..(group + 1) * self.width]
    }

    /// Merges another set's reached groups into this one's.
    fn absorb(
        &mut self,
        other: Self,
        aggregates: &[CompiledAggregate],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if self.width != other.width || self.touched.len() != other.touched.len() {
            return Err(ExecError::InvalidPhysicalPlan(
                "fused join states of two shapes",
            ));
        }
        let mut theirs = other.states.into_iter();
        for (group, touched) in other.touched.into_iter().enumerate() {
            if !touched {
                theirs.by_ref().take(self.width).for_each(drop);
                continue;
            }
            for ((state, other), aggregate) in self
                .reach(group)
                .iter_mut()
                .zip(theirs.by_ref())
                .zip(aggregates)
            {
                state.merge(aggregate, other, memory)?;
            }
        }
        Ok(())
    }

    /// The reached groups in plan order, each with `values(its index)`.
    pub(super) fn into_reached(
        self,
        mut values: impl FnMut(usize) -> Vec<pintail_types::Value>,
    ) -> Vec<super::aggregate::AggregateGroup> {
        let mut states = self.states.into_iter();
        let mut reached = Vec::new();
        for (group, touched) in self.touched.into_iter().enumerate() {
            let states = states.by_ref().take(self.width).collect::<Vec<_>>();
            if touched {
                reached.push(super::aggregate::AggregateGroup {
                    values: values(group),
                    states,
                });
            }
        }
        reached
    }
}

/// Why a query's row fold opens its states morsel by morsel, when it must.
///
/// States kept per worker see the morsels in whatever order the pool hands
/// them out, and are merged once at the end, so only aggregates whose
/// answer cannot depend on that order are kept that way: counts, exact
/// integer and decimal sums and averages, and the smallest or largest value of a type
/// whose equal values are identical. The others keep today's fixed order -
/// each morsel's states merged in morsel order.
fn order_sensitive(aggregate: &CompiledAggregate) -> Option<&'static str> {
    use pintail_types::DataType;
    match aggregate.function {
        AggregateFunction::Sum | AggregateFunction::Average if aggregate_uses_float(aggregate) => {
            Some("a float sum or average rounds in the order its rows are added")
        }
        // An integer sum's state keeps its exact total, past 64 bits and
        // back, and is judged when the group is finished: it ends the same
        // however its rows were split. (The column fold's integer lane is
        // another matter - it checks each morsel's total - and keeps its
        // totals per morsel.)
        AggregateFunction::Sum => match aggregate.data_type {
            Some(DataType::Decimal { .. } | DataType::Int64 | DataType::UInt64) => None,
            _ => Some("a sum that is neither an integer nor a decimal"),
        },
        // A count, and an average the guard above left: an exact one.
        AggregateFunction::Count | AggregateFunction::Average => None,
        AggregateFunction::Minimum | AggregateFunction::Maximum => match aggregate.input_type {
            Some(
                DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Decimal { .. }
                | DataType::Date32
                | DataType::DateTime64 { .. }
                | DataType::Time64 { .. }
                | DataType::Year,
            ) => None,
            _ => {
                Some("an extreme of text or floats shows whichever of its equal values came first")
            }
        },
        _ => Some("an aggregate that depends on the order of its rows"),
    }
}

/// Per-worker states of the row fold, kept for a whole query as
/// [`LanePool`] keeps the column fold's totals. A set belongs to the pool
/// thread that opened it and only that thread folds into it.
pub(super) struct StatePool<'a> {
    groups: usize,
    aggregates: &'a [CompiledAggregate],
    /// One slot per pool thread, then one for any other caller.
    slots: Vec<Mutex<Option<GroupStates>>>,
    /// Sets that found their slot taken when they came back.
    spare: Mutex<Vec<GroupStates>>,
    reserved: AtomicUsize,
}

impl<'a> StatePool<'a> {
    /// A pool for `groups` groups, or why the query opens states per
    /// morsel: an aggregate whose answer depends on the order its rows are
    /// folded in, or a set per worker past an eighth of the ceiling.
    pub(super) fn plan(
        groups: usize,
        aggregates: &'a [CompiledAggregate],
        memory: &MemoryTracker,
    ) -> Result<Self, &'static str> {
        if let Some(reason) = aggregates.iter().find_map(order_sensitive) {
            return Err(reason);
        }
        let workers = rayon::current_num_threads().max(1);
        if GroupStates::bytes(groups, aggregates.len()).saturating_mul(workers) > memory.limit() / 8
        {
            return Err("a set of states per worker is past an eighth of the memory ceiling");
        }
        Ok(Self {
            groups,
            aggregates,
            slots: (0..=workers).map(|_| Mutex::new(None)).collect(),
            spare: Mutex::new(Vec::new()),
            reserved: AtomicUsize::new(0),
        })
    }

    /// The calling thread's slot.
    fn slot(&self) -> &Mutex<Option<GroupStates>> {
        let last = self.slots.len() - 1;
        &self.slots[rayon::current_thread_index().map_or(last, |index| index.min(last))]
    }

    /// The calling thread's set, or a new one charged to `memory`. A set
    /// is no larger than the states one morsel opened for itself before,
    /// which every round charged for each of its morsels up front; a
    /// ceiling that refuses one set refused that round.
    pub(super) fn take(&self, memory: &MemoryTracker) -> Result<GroupStates, ExecError> {
        let kept = self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(kept) = kept {
            return Ok(kept);
        }
        let bytes = GroupStates::bytes(self.groups, self.aggregates.len());
        memory.reserve(bytes)?;
        self.reserved.fetch_add(bytes, Ordering::Relaxed);
        Ok(GroupStates::new(self.groups, self.aggregates))
    }

    /// Returns the set the calling thread took.
    pub(super) fn give_back(&self, states: GroupStates) {
        let mut slot = self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(states);
        } else {
            drop(slot);
            self.spare
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(states);
        }
    }

    /// Bytes charged for the sets opened so far.
    pub(super) fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Every worker's states merged into one set; `None` when no morsel
    /// folded into the pool.
    pub(super) fn finish(self, memory: &MemoryTracker) -> Result<Option<GroupStates>, ExecError> {
        let mut sets = self
            .slots
            .into_iter()
            .filter_map(|slot| {
                slot.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .chain(
                self.spare
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        let Some(mut total) = sets.next() else {
            return Ok(None);
        };
        for set in sets {
            total.absorb(set, self.aggregates, memory)?;
        }
        Ok(Some(total))
    }
}

#[cfg(test)]
mod tests {
    use super::{GroupKeys, Keys, Rows, UniqueKeyGroups};

    /// A dense table of `len` keys from `minimum`, key `minimum + i` in
    /// group `i % 7`, every fifth slot without a key.
    fn table(minimum: i128, len: usize) -> UniqueKeyGroups {
        let miss = 7;
        UniqueKeyGroups {
            keys: GroupKeys::Direct {
                minimum,
                groups: (0..len)
                    .map(|slot| {
                        if slot % 5 == 4 {
                            miss
                        } else {
                            u32::try_from(slot % 7).expect("small")
                        }
                    })
                    .collect(),
            },
            miss,
        }
    }

    /// What the 128-bit difference answers for one key.
    fn expected(table: &UniqueKeyGroups, key: i128) -> u32 {
        let GroupKeys::Direct { minimum, groups } = &table.keys else {
            unreachable!("built dense");
        };
        usize::try_from(key - minimum)
            .ok()
            .and_then(|offset| groups.get(offset).copied())
            .unwrap_or(table.miss)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn keys_resolve_in_64_bits_as_they_do_in_128() {
        let signed = [
            i64::MIN,
            i64::MIN + 1,
            -1_000_000,
            -41,
            -40,
            -39,
            -1,
            0,
            1,
            58,
            59,
            60,
            1_000_000,
            i64::MAX - 1,
            i64::MAX,
        ];
        let unsigned = [
            0,
            1,
            39,
            40,
            41,
            139,
            140,
            u64::MAX / 2,
            u64::MAX / 2 + 1,
            u64::MAX - 1,
            u64::MAX,
        ];
        // A smallest key inside the column's type takes the 64-bit path;
        // one outside it (a negative key under unsigned probes, a key past
        // the signed range) keeps the 128-bit one. Both must agree with
        // the reference for keys below, inside and beyond the table.
        for minimum in [
            i128::from(i64::MIN),
            -40,
            0,
            40,
            i128::from(i64::MAX) - 50,
            i128::from(i64::MAX) + 10,
            i128::from(u64::MAX) - 50,
        ] {
            let table = table(minimum, 100);
            let valid = (0..signed.len())
                .map(|row| row % 4 != 2)
                .collect::<Vec<_>>();
            let picked = [1_u32, 3, 4, 8, 9, 10];
            let (mut scratch, mut out) = (Vec::new(), Vec::new());
            Keys::Signed(&signed).resolve(
                &table,
                &Rows::Range(0..signed.len()),
                Some(&valid),
                &mut scratch,
                &mut out,
            );
            for ((key, valid), group) in signed.iter().zip(&valid).zip(&out) {
                let want = if *valid {
                    expected(&table, i128::from(*key))
                } else {
                    table.miss
                };
                assert_eq!(*group, want, "signed key {key} from {minimum}");
            }
            Keys::Signed(&signed).resolve(
                &table,
                &Rows::Picked(&picked),
                None,
                &mut scratch,
                &mut out,
            );
            for (row, group) in picked.iter().zip(&out) {
                let key = signed[*row as usize];
                assert_eq!(*group, expected(&table, i128::from(key)), "picked {key}");
            }
            Keys::Unsigned(&unsigned).resolve(
                &table,
                &Rows::Range(0..unsigned.len()),
                None,
                &mut scratch,
                &mut out,
            );
            for (key, group) in unsigned.iter().zip(&out) {
                assert_eq!(
                    *group,
                    expected(&table, i128::from(*key)),
                    "unsigned key {key} from {minimum}"
                );
            }
            Keys::Unsigned(&unsigned).resolve(
                &table,
                &Rows::Picked(&picked),
                None,
                &mut scratch,
                &mut out,
            );
            for (row, group) in picked.iter().zip(&out) {
                let key = unsigned[*row as usize];
                assert_eq!(*group, expected(&table, i128::from(key)), "picked {key}");
            }
        }
        // The path taken: only a smallest key the type holds is direct.
        assert!(table(-40, 10).direct::<i64>().is_some());
        assert!(table(-40, 10).direct::<u64>().is_none());
        assert!(
            table(i128::from(i64::MAX) + 10, 10)
                .direct::<i64>()
                .is_none()
        );
        assert!(
            table(i128::from(i64::MAX) + 10, 10)
                .direct::<u64>()
                .is_some()
        );
    }
}
