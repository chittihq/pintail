//! Two-pass partitioned aggregation: scatter lanes, dense slots and
//! the string interning used to keep group keys comparable.

use std::collections::{HashMap, VecDeque};

use pintail_sql::{AggregateFunction, DatePart};
use pintail_types::{DataType, Value};

use super::aggregate::GroupKeyMap;
use super::aggregate::{
    AggregateGroup, AggregateState, CompiledAggregate, OpenDistinctBits, aggregate_uses_float,
    decimal_average_scale, decimal_integer_total, decimal_units_from_int,
    merge_spilled_aggregate_groups, write_aggregate_spill_run,
};
use super::join::{normalized_group_hash_key, normalized_group_text};
use super::morsel::{Morsel, default_morsel_limit, morsel_plan};
use super::packed_fold::{
    FoldRows, PackedFold, commit_merged, fold_rows, merged_cell, occupied_in,
};
use super::{
    ExecError, HASH_ENTRY_OVERHEAD, MaterializedRows, MemoryTracker, PullOperator,
    estimated_row_payload_bytes,
};
use crate::spill;
use rayon::prelude::*;

use crate::collation::Collation;

use crate::expression::CompiledExpr;
use crate::{RecordBatch, expression::mysql_f64};

/// Finished-row bytes a finalizing partition gathers before charging them.
pub(super) const FINALIZE_CHARGE_SLICE: usize = 256 << 10;

/// Per-aggregate scatter payload for the two-pass partitioned aggregate.
#[derive(Clone, Copy)]
pub(super) enum TwoPassLane {
    /// COUNT(*): every row counts; the lane carries nothing.
    CountStar,
    /// COUNT/SUM/AVG over a float or decimal column: an f64 rides the lane
    /// (matching the sequential path's f64 accumulation for these types).
    Float { column: usize },
    /// COUNT/SUM/AVG over an integer column: exact bits ride the lane and
    /// pass 2 takes the sequential path's exact integer branch.
    Int { column: usize, data_type: DataType },
    /// MIN/MAX over a plain int/uint/float/bool column: exact bits ride the
    /// lane so the retained Value stays exact.
    Exact { column: usize, data_type: DataType },
    /// SUM over a decimal column: i64 scaled units ride the lane and pass 2
    /// accumulates i128 exactly. f64 lanes drift past the 4-decimal
    /// canonical on 500k-row group sums (the Q4 mismatch, 2026-08-02).
    DecimalUnits {
        column: usize,
        scale: u8,
        float_output: bool,
    },
    /// COUNT(DISTINCT `int_col)`: raw key bits ride the lane and pass 2
    /// dedups through the typed i128 set (e16 — this was the shape that
    /// kept Q7 off every typed path).
    Distinct { column: usize, data_type: DataType },
    /// MIN/MAX over a decimal column: i64 scaled units ride the lane;
    /// pass 2 compares units and formats only on replacement.
    ExtremeDecimal { column: usize, scale: u8 },
    /// COUNT over a column no other lane carries - text, a date, a binary
    /// string: only whether the row holds a value rides the lane.
    Present { column: usize },
    /// MIN/MAX over a DATE or DATETIME column: the packed units ride the
    /// lane and pass 2 compares them, formatting only on replacement. A
    /// batch whose column carries no units - one holding a zero date, or
    /// text as it was written - never reaches a lane: it has no bits to
    /// ride, and reading it here would answer its rows as NULL. Such a
    /// batch folds row by row instead (see [`fold_odd_batch`]).
    Temporal { column: usize, data_type: DataType },
}

/// Most digits a decimal can declare and still have every value's scaled
/// units fit the signed 64 bits a lane carries.
const LANE_DECIMAL_DIGITS: u8 = 18;

/// Whether every value of a decimal declared with `precision` digits has
/// scaled units a lane can carry.
pub(super) fn units_fit_a_lane(precision: u8) -> bool {
    precision <= LANE_DECIMAL_DIGITS
}

/// The text a temporal unit of `data_type` spells.
fn temporal_unit_text(units: i128, data_type: DataType) -> Option<String> {
    let units = i64::try_from(units).ok()?;
    match data_type {
        DataType::Date32 => pintail_types::format_date_days(units),
        DataType::DateTime64 { fsp } => pintail_types::format_datetime_micros(units, fsp),
        _ => None,
    }
}

/// The packed units of a DATE or DATETIME column whose text is the units'
/// own canonical spelling, which is what lets a lane or a key carry the
/// units alone and format them back.
pub(super) fn derived_temporal_units(
    batch: &RecordBatch,
    column: usize,
) -> Option<(&[i64], &crate::array::ValidityMask)> {
    let vector = batch.column(column)?;
    if !matches!(
        vector.data_type(),
        DataType::Date32 | DataType::DateTime64 { .. }
    ) {
        return None;
    }
    match vector.typed()? {
        (crate::batch::TypedValues::Temporal { units, text }, validity)
            if text.derived() && units.len() >= batch.row_count() =>
        {
            Some((units.as_slice(), validity))
        }
        _ => None,
    }
}

/// Whether every aggregate fits a scatter lane, and which kind. `None`
/// keeps the query on the sequential direct path.
/// Partitions per worker thread. See `build_streaming_two_pass_aggregate`.
const PARTITIONS_PER_WORKER: usize = 4;

/// A decimal value's exact units at `scale`, for a column with no packed
/// units to read. `None` for NULL, or for a value that does not fit `scale`
/// exactly.
fn decimal_units_at_scale(value: &Value, scale: u8) -> Option<i128> {
    let widen = |units: i128, from: u8| {
        scale
            .checked_sub(from)
            .and_then(|digits| 10_i128.checked_pow(u32::from(digits)))
            .and_then(|factor| units.checked_mul(factor))
    };
    match value {
        Value::Null => None,
        Value::DecimalAverage(quotient) if quotient.count == 1 => {
            widen(quotient.units, quotient.scale)
        }
        Value::Int64(signed) => widen(i128::from(*signed), 0),
        Value::UInt64(unsigned) => widen(i128::from(*unsigned), 0),
        other => other
            .text()
            .and_then(|text| pintail_types::parse_decimal_scaled(text, scale)),
    }
}

pub(super) fn two_pass_lanes(
    aggregates: &[CompiledAggregate],
    batch: &RecordBatch,
) -> Option<Vec<TwoPassLane>> {
    if aggregates.len() > 7 {
        // One mask bit per lane plus the key bit.
        return None;
    }
    aggregate_lanes(aggregates, batch)
}

/// The lane of every aggregate, however many there are: the scatter's mask
/// byte bounds [`two_pass_lanes`], not what a lane can carry.
#[allow(clippy::too_many_lines)] // one arm per aggregate kind
pub(super) fn aggregate_lanes(
    aggregates: &[CompiledAggregate],
    batch: &RecordBatch,
) -> Option<Vec<TwoPassLane>> {
    aggregates
        .iter()
        .map(|aggregate| {
            if aggregate.distinct {
                // COUNT(DISTINCT int_col) rides its own lane; any other
                // distinct shape keeps the query off the two-pass path.
                if aggregate.function != AggregateFunction::Count {
                    return None;
                }
                let column = aggregate.expr.as_ref()?.column_index()?;
                let storage = batch.column(column)?.data_type().storage_type();
                return matches!(storage, DataType::Int64 | DataType::UInt64).then_some(
                    TwoPassLane::Distinct {
                        column,
                        data_type: storage,
                    },
                );
            }
            let Some(expr) = &aggregate.expr else {
                return matches!(aggregate.function, AggregateFunction::Count)
                    .then_some(TwoPassLane::CountStar);
            };
            let column = expr.column_index()?;
            let storage = batch.column(column)?.data_type().storage_type();
            match aggregate.function {
                AggregateFunction::Count | AggregateFunction::Sum | AggregateFunction::Average => {
                    match storage {
                        // Integer inputs stay on the exact integer branch;
                        // the generic state accumulates exact decimal
                        // averages from integer values too.
                        DataType::Int64 | DataType::UInt64 => Some(TwoPassLane::Int {
                            column,
                            data_type: storage,
                        }),
                        // An average the planner typed as an exact decimal
                        // must never accumulate through a float: f64
                        // addition is not associative, so the answer would
                        // move with however the rows were split across
                        // workers and merged. The arm below already asks
                        // this question before it picks an exact lane; this
                        // one did not, so a decimal column whose batch
                        // materialized as `Float64` took the inexact lane
                        // while the plan said otherwise. `None` here is not
                        // a fallback to something worse - it declines the
                        // two-pass lane, and the general path's
                        // `DecimalAverage` accumulates scaled integers.
                        DataType::Float64 => decimal_average_scale(aggregate)
                            .is_none()
                            .then_some(TwoPassLane::Float { column }),
                        _ => match batch.column(column)?.data_type() {
                            // SUM and exact AVG both ride the packed-units
                            // lane; the per-row apply branches on the
                            // aggregate function.
                            DataType::Decimal { precision, scale }
                                if aggregate.function == AggregateFunction::Sum
                                    || decimal_average_scale(aggregate).is_some() =>
                            {
                                // The lane carries i64 units. A column
                                // declared wider can hold a value that does
                                // not fit them, and a lane has no way to
                                // say so: the row would read as NULL.
                                units_fit_a_lane(precision).then_some(TwoPassLane::DecimalUnits {
                                    column,
                                    scale,
                                    float_output: aggregate_uses_float(aggregate),
                                })
                            }
                            DataType::Decimal { .. } => Some(TwoPassLane::Float { column }),
                            // A COUNT reads no value, only its presence.
                            _ => (aggregate.function == AggregateFunction::Count)
                                .then_some(TwoPassLane::Present { column }),
                        },
                    }
                }
                AggregateFunction::Minimum | AggregateFunction::Maximum => {
                    let data_type = batch.column(column)?.data_type();
                    if matches!(data_type, DataType::Date32 | DataType::DateTime64 { .. }) {
                        // Judged on the first batch, like every lane; a
                        // later batch without units folds row by row.
                        return derived_temporal_units(batch, column)
                            .map(|_| TwoPassLane::Temporal { column, data_type });
                    }
                    if let DataType::Decimal { precision, scale } =
                        batch.column(column)?.data_type()
                    {
                        return units_fit_a_lane(precision)
                            .then_some(TwoPassLane::ExtremeDecimal { column, scale });
                    }
                    matches!(
                        storage,
                        DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                    )
                    .then_some(TwoPassLane::Exact {
                        column,
                        data_type: storage,
                    })
                }
                // ANY_VALUE retains one exact value, exactly like MIN/MAX.
                AggregateFunction::AnyValue => matches!(
                    storage,
                    DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                )
                .then_some(TwoPassLane::Exact {
                    column,
                    data_type: storage,
                }),
                // Welford consumes one f64 per row, so the float lane
                // carries everything the moments need. Riding a lane is the
                // point: an aggregate with no lane drops its whole query
                // onto the per-row Value path, which is what costs Q7 its
                // margin against ClickHouse (issue #6).
                AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. } => matches!(
                    storage,
                    DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                )
                .then_some(TwoPassLane::Float { column }),
                // The bit folds need exact integer bits, which the int lane
                // already carries.
                AggregateFunction::BitAnd
                | AggregateFunction::BitOr
                | AggregateFunction::BitXor => {
                    matches!(storage, DataType::Int64 | DataType::UInt64).then_some(
                        TwoPassLane::Int {
                            column,
                            data_type: storage,
                        },
                    )
                }
                AggregateFunction::GroupConcat
                | AggregateFunction::JsonArrayAgg
                | AggregateFunction::JsonObjectAgg => None,
            }
        })
        .collect::<Option<Vec<_>>>()
}

/// One worker's scatter output for one partition: struct-of-arrays rows.
#[derive(Default)]
struct TwoPassBucket {
    /// Group key bits per row.
    keys: Vec<u64>,
    /// Bit 7: key is NULL; bits 0..lanes: lane value is NULL.
    masks: Vec<u8>,
    /// `lanes.len() == keys.len() * lane_count`, row-major.
    lanes: Vec<u64>,
}

fn two_pass_key_bits(value: &Value) -> Option<(u64, bool)> {
    match value {
        Value::Null => Some((0, true)),
        Value::Int64(value) => Some((u64::from_ne_bytes(value.to_ne_bytes()), false)),
        Value::UInt64(value) => Some((*value, false)),
        Value::Float64(value) => Some((value.get().to_bits(), false)),
        Value::Boolean(value) => Some((u64::from(*value), false)),
        // Text-shaped values have no fixed-width lane key.
        Value::Utf8(_) | Value::Binary(_) | Value::Enum { .. } | Value::DecimalAverage(_) => None,
    }
}

pub(super) fn two_pass_key_value(bits: u64, null: bool, data_type: DataType) -> Value {
    if null {
        return Value::Null;
    }
    if matches!(data_type, DataType::Date32 | DataType::DateTime64 { .. }) {
        // A temporal key's bits are its packed units; its value is the
        // text they spell, as the key column itself would hand it out.
        let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
        return temporal_unit_text(units, data_type).map_or(Value::Null, Value::Utf8);
    }
    match data_type.storage_type() {
        DataType::Int64 => Value::Int64(i64::from_ne_bytes(bits.to_ne_bytes())),
        DataType::UInt64 => Value::UInt64(bits),
        DataType::Float64 => Value::float64(f64::from_bits(bits)),
        DataType::Boolean => Value::Boolean(bits != 0),
        _ => Value::Null,
    }
}

/// How the streaming two-pass extracts group-key bits per row.
#[derive(Clone, Copy)]
pub(super) enum TwoPassKeySource {
    /// One int-typed column: key bits are the value's bit pattern.
    Int { column: usize, group_type: DataType },
    /// One string column: key bits are interned string ids (bit 7 of the
    /// mask carries NULL, matching the int scheme).
    Text { column: usize },
    /// Up to two DATE-PART expressions over temporal columns (the Q5
    /// shape, GROUP BY YEAR(d), MONTH(d)): each part value is bounded
    /// (year < 10^4, others < 60), so `(v + 1)` packs into 20 bits per
    /// part with 0 as the per-part NULL sentinel.
    DateParts {
        parts: [Option<(DatePart, usize)>; 2],
    },
}

/// Streaming two-pass partitioned aggregation for one int-typed group
/// column (experiments/RESULTS.md e13/e15 and the 2026-08-02 phase-0
/// profile). Pass 1 scatters (key bits, lane bits) into partition buckets
/// as batches arrive — no `RecordBatch` is retained. Pass 2 folds buckets
/// into per-partition typed hashmaps in parallel whenever the scatter
/// window fills, so memory is bounded by the group states plus one flush
/// window regardless of input size.
/// Declared labels and members per text key column, for rebuilding a
/// group key the way the finalize does.
type KeyDeclarations = [Option<(std::sync::Arc<Vec<String>>, bool)>; 2];
type KeyMembers = [Option<std::sync::Arc<Vec<String>>>; 2];

/// Two label tables rebuilt from observed ordinals, as one: every slot
/// either batch filled. An unfilled slot is an empty string, so a filled
/// one wins; where both filled a slot they hold the same label.
pub(super) fn merge_partial_labels(
    held: std::sync::Arc<Vec<String>>,
    seen: &std::sync::Arc<Vec<String>>,
) -> std::sync::Arc<Vec<String>> {
    let adds = seen.len() > held.len()
        || seen
            .iter()
            .zip(held.iter())
            .any(|(seen, held)| held.is_empty() && !seen.is_empty());
    if !adds {
        return held;
    }
    let mut merged = held.as_ref().clone();
    if merged.len() < seen.len() {
        merged.resize(seen.len(), String::new());
    }
    for (slot, label) in merged.iter_mut().zip(seen.iter()) {
        if slot.is_empty() {
            slot.clone_from(label);
        }
    }
    std::sync::Arc::new(merged)
}

/// One interned text key as a value: an ENUM group key rebuilds with its
/// declaration index and a SET key with its member bitmask, so an ORDER BY
/// above sorts by `MySQL`'s rule; anything undeclared stays a plain string.
fn interned_key_value(
    intern: &StringIntern,
    id: u64,
    labels: Option<&(std::sync::Arc<Vec<String>>, bool)>,
    members: Option<&std::sync::Arc<Vec<String>>>,
) -> Value {
    let text = &intern.values[usize::try_from(id).expect("intern id fits usize")];
    labelled_text_value(text, labels, members)
}

/// A text group key as a value: an ENUM key with its declaration index and
/// a SET key with its member bitmask, anything undeclared a plain string.
pub(super) fn labelled_text_value(
    text: &str,
    labels: Option<&(std::sync::Arc<Vec<String>>, bool)>,
    members: Option<&std::sync::Arc<Vec<String>>>,
) -> Value {
    let ordinal = if let Some((labels, exhaustive)) = labels {
        // An empty label resolves only against a complete table: a gappy
        // reconstruction keeps unseen slots as empty strings, and neither
        // the empty SET ("", mask 0) nor a declared '' member may take a
        // gap's ordinal (see StrColumn::enum_index_of).
        (!text.is_empty() || *exhaustive)
            .then(|| {
                labels
                    .iter()
                    .position(|declared| declared.as_str() == text)
                    .and_then(|position| u64::try_from(position + 1).ok())
            })
            .flatten()
    } else if let Some(members) = members {
        let mut mask = Some(0_u64);
        for member in text.split(',').filter(|member| !member.is_empty()) {
            mask = mask.and_then(|mask| {
                members
                    .iter()
                    .position(|declared| declared == member)
                    .filter(|position| *position < 64)
                    .map(|position| mask | (1_u64 << position))
            });
        }
        mask
    } else {
        None
    };
    ordinal.map_or_else(
        || Value::Utf8(text.to_owned()),
        |index| Value::Enum {
            index,
            label: text.to_owned(),
        },
    )
}

/// The group key values of one map entry, in output order.
fn two_pass_key_values(
    keys: TwoPassKeySource,
    bits: u64,
    null: bool,
    intern: Option<&StringIntern>,
    labels: &KeyDeclarations,
    members: &KeyMembers,
) -> Vec<Value> {
    let intern = || intern.expect("text keys carry an intern table");
    match keys {
        TwoPassKeySource::Int { group_type, .. } => {
            vec![two_pass_key_value(bits, null, group_type)]
        }
        TwoPassKeySource::Text { .. } => vec![if null {
            Value::Null
        } else {
            interned_key_value(intern(), bits, labels[0].as_ref(), members[0].as_ref())
        }],
        TwoPassKeySource::DateParts { parts } => {
            let count = parts.iter().flatten().count();
            (0..count)
                .map(|index| {
                    let shift = 20 * (count - 1 - index);
                    let id = (bits >> shift) & 0xF_FFFF;
                    if id == 0 {
                        Value::Null
                    } else {
                        // Date parts are signed (Int64), like the scalar and
                        // units paths that feed them; a 20-bit id always fits.
                        Value::Int64(i64::try_from(id - 1).expect("20-bit date-part id fits i64"))
                    }
                })
                .collect()
        }
    }
}

/// The partition maps as the buffered path keys its runs: by normalized
/// group values, so a run from here merges with the resident remainder on
/// identical encoded keys.
fn two_pass_groups_map(
    maps: &mut [GroupKeyMap],
    keys: TwoPassKeySource,
    intern: Option<&StringIntern>,
    labels: &KeyDeclarations,
    members: &KeyMembers,
    collation: Collation,
) -> HashMap<Vec<Value>, AggregateGroup> {
    let mut groups = HashMap::with_capacity(maps.iter().map(HashMap::len).sum());
    for map in maps.iter_mut() {
        for ((bits, null), states) in map.drain() {
            let values = two_pass_key_values(keys, bits, null, intern, labels, members);
            let key = values
                .iter()
                .cloned()
                .map(|value| normalized_group_hash_key(value, collation).unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            groups.insert(key, AggregateGroup { values, states });
        }
    }
    groups
}

/// The groups of one dense slot table, keyed as a spilled run keys them.
fn dense_slot_groups(
    slots: DenseGroupSlots,
    keys: TwoPassKeySource,
    intern: Option<&StringIntern>,
    labels: &KeyDeclarations,
    members: &KeyMembers,
    collation: Collation,
) -> HashMap<Vec<Value>, AggregateGroup> {
    let mut groups = HashMap::new();
    for (index, slot) in slots.into_iter().enumerate() {
        let Some(states) = slot else { continue };
        let (bits, null) = dense_slot_sentinel(keys, index);
        let values = two_pass_key_values(keys, bits, null, intern, labels, members);
        let key = values
            .iter()
            .cloned()
            .map(|value| normalized_group_hash_key(value, collation).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        groups.insert(key, AggregateGroup { values, states });
    }
    groups
}

/// Everything the two-pass aggregate holds between flushes, so a spill can
/// take it whole: the partition maps, the dense slots not yet unified into
/// them, and the bytes charged for both.
struct TwoPassState<'a> {
    maps: &'a mut [GroupKeyMap],
    dense: &'a mut Option<DenseGroupSlots>,
    range: &'a mut IntRange,
    pool: &'a mut DensePool,
    group_reserved: &'a mut usize,
    spill_runs: &'a mut Vec<spill::ClosedRun>,
}

/// Writes every group held so far as one closed, sorted run and starts
/// over with empty maps. The path holds its whole state otherwise, and a
/// per-entity DISTINCT count over a large table used to fail at a ceiling
/// smaller than that state instead of going to disk.
#[allow(clippy::too_many_arguments)]
fn two_pass_spill(
    state: &mut TwoPassState<'_>,
    keys: TwoPassKeySource,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    intern: Option<&StringIntern>,
    labels: &KeyDeclarations,
    members: &KeyMembers,
    collation: Collation,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    // The workers' pooled partials go to disk as runs of their own. Merging
    // them into the slots first unions every group's distinct sets in
    // memory, at the moment the query is out of it; the merge of the runs
    // unions them a group at a time instead.
    let partials = std::mem::take(&mut state.pool.partials);
    let mut spill = GroupSpill {
        keys,
        intern,
        labels,
        members,
        collation,
        runs: state.spill_runs,
    };
    if let Some(slots) = state.dense.take() {
        for partial in partials {
            let mut groups = dense_slot_groups(partial, keys, intern, labels, members, collation);
            if !groups.is_empty() {
                spill
                    .runs
                    .push(write_aggregate_spill_run(&mut groups, memory)?);
            }
        }
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            state.maps,
            memory,
            state.group_reserved,
            &mut spill,
        )?;
    }
    memory.release(state.pool.reserved);
    state.pool.reserved = 0;
    if let IntRange::Active(range) = std::mem::replace(state.range, IntRange::Off) {
        fold_int_range_into_maps(
            &range,
            aggregates,
            partitions,
            state.maps,
            memory,
            state.group_reserved,
            &mut spill,
        )?;
    }
    spill.write(state.maps, state.group_reserved, memory)
}

/// Where the partition maps' groups go when the budget cannot take another
/// one: everything a closed run needs to name a group by its key.
struct GroupSpill<'a> {
    keys: TwoPassKeySource,
    intern: Option<&'a StringIntern>,
    labels: &'a KeyDeclarations,
    members: &'a KeyMembers,
    collation: Collation,
    runs: &'a mut Vec<spill::ClosedRun>,
}

impl GroupSpill<'_> {
    /// Writes the maps' groups as one closed, sorted run and gives their
    /// charge back; nothing when the maps are empty.
    fn write(
        &mut self,
        maps: &mut [GroupKeyMap],
        group_reserved: &mut usize,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let mut groups = two_pass_groups_map(
            maps,
            self.keys,
            self.intern,
            self.labels,
            self.members,
            self.collation,
        );
        if groups.is_empty() {
            return Ok(());
        }
        self.runs
            .push(write_aggregate_spill_run(&mut groups, memory)?);
        memory.release(*group_reserved);
        *group_reserved = 0;
        Ok(())
    }
}

/// Clears the key intern table once nothing refers to its ids: the groups
/// have gone to disk, where their keys are written as text, and no window
/// or bucket holds a translated row. The table holds every distinct key
/// seen, so without this a high-cardinality text key fills the ceiling
/// with keys whose groups are already on disk.
fn clear_idle_intern(
    intern: &mut Option<StringIntern>,
    pending_empty: bool,
    maps: &[GroupKeyMap],
    dense_empty: bool,
    memory: &MemoryTracker,
) {
    if pending_empty
        && dense_empty
        && maps.iter().all(HashMap::is_empty)
        && let Some(intern) = intern
    {
        intern.clear(memory);
    }
}

/// Whether rows waiting to be applied should be applied now: what the query
/// holds, plus what applying them may add, passes half its ceiling. The
/// window is otherwise due by its scattered bytes alone, but pending rows
/// cost more than those - each new text key is interned as it arrives - so
/// it could fill the ceiling before it was due, with the maps still empty
/// and nothing for a spill to free.
fn pending_under_pressure(memory: &MemoryTracker, rows: usize, per_row_growth: usize) -> bool {
    memory
        .used()
        .saturating_add(rows.saturating_mul(per_row_growth))
        > memory.limit() / 2
}

/// Whether the groups held so far should go to disk before the next flush
/// applies its window: the query past half the ceiling with groups to
/// spill. The maps' charge is only part of what they hold, since distinct
/// sets reserve through the states, so this looks at the whole query.
///
/// Groups to spill are not only the maps': a query of few groups keeps all
/// of them in the dense slots, the workers' pooled partials or the integer
/// range, and each of those holds distinct sets of its own. Looking at the
/// maps alone, such a query never spilled and its sets met the ceiling.
fn two_pass_under_pressure(
    maps: &[GroupKeyMap],
    dense: Option<&DenseGroupSlots>,
    range: &IntRange,
    pool: &DensePool,
    memory: &MemoryTracker,
) -> bool {
    memory.used() > memory.limit() / 2
        && (maps.iter().any(|map| !map.is_empty())
            || !pool.partials.is_empty()
            || dense.is_some_and(|slots| slots.iter().any(Option::is_some))
            || matches!(range, IntRange::Active(range) if range.rows > 0))
}

/// Spills before a flush when the budget is already under pressure, so
/// the flush applies its rows into empty maps instead of failing on the
/// first small charge a full budget refuses.
#[allow(clippy::too_many_arguments)]
fn two_pass_relieve(
    state: &mut TwoPassState<'_>,
    keys: TwoPassKeySource,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    intern: Option<&StringIntern>,
    labels: &KeyDeclarations,
    members: &KeyMembers,
    collation: Collation,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    if two_pass_under_pressure(
        state.maps,
        state.dense.as_ref(),
        state.range,
        state.pool,
        memory,
    ) {
        two_pass_spill(
            state, keys, aggregates, partitions, intern, labels, members, collation, memory,
        )?;
    }
    Ok(())
}

pub(super) fn build_streaming_two_pass_aggregate(
    input: &mut PullOperator,
    first: RecordBatch,
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<MaterializedRows, ExecError> {
    streaming_two_pass(
        input,
        first,
        keys,
        &[],
        lanes,
        aggregates,
        memory,
        collation,
    )
}

/// The streaming two-pass with the GROUP BY expressions its key bits were
/// derived from, so a batch the lanes cannot carry still finds its groups
/// by evaluating them row by row.
///
/// An integer key source with one expression is keyed by that expression's
/// packed units: a DATE or DATETIME column, or an expression whose kernel
/// yields a column of units or integers, which each batch then carries as
/// one more column at `keys`' index (see [`unit_key_type`]). A date-part
/// source reads its parts off the temporal columns' units, and falls back
/// to the expressions for a batch whose column carries none.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_unit_key_two_pass_aggregate(
    input: &mut PullOperator,
    first: RecordBatch,
    keys: TwoPassKeySource,
    key_exprs: &[CompiledExpr],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<MaterializedRows, ExecError> {
    streaming_two_pass(
        input, first, keys, key_exprs, lanes, aggregates, memory, collation,
    )
}

/// The type an expression key evaluates to, which its column kernels
/// require; `None` for a node that declares none.
fn key_declared_type(key: &CompiledExpr) -> Option<DataType> {
    match key {
        CompiledExpr::Unary { data_type, .. }
        | CompiledExpr::Binary { data_type, .. }
        | CompiledExpr::Scalar { data_type, .. } => *data_type,
        _ => None,
    }
}

/// The type of a computed key column whose packed form identifies each of
/// its values: temporal units that spell their own text, or integers.
fn unit_key_column_type(column: &crate::ColumnVector, rows: usize) -> Option<DataType> {
    use crate::batch::TypedValues;
    let data_type = column.data_type();
    match (data_type, column.typed()?.0) {
        (DataType::Date32 | DataType::DateTime64 { .. }, TypedValues::Temporal { units, text })
            if text.derived() && units.len() >= rows =>
        {
            Some(data_type)
        }
        (_, TypedValues::Int64(values))
            if data_type.storage_type() == DataType::Int64 && values.len() >= rows =>
        {
            Some(data_type)
        }
        (_, TypedValues::UInt64(values))
            if data_type.storage_type() == DataType::UInt64 && values.len() >= rows =>
        {
            Some(data_type)
        }
        _ => None,
    }
}

/// The key type of a GROUP BY the streaming two-pass can key by packed
/// units, judged on the first batch: a DATE or DATETIME column, or an
/// expression with a packed kernel over one. An integer column is not
/// answered here; it has its own route.
pub(super) fn unit_key_type(key: &CompiledExpr, first: &RecordBatch) -> Option<DataType> {
    if key.has_variable_effects() {
        return None;
    }
    if let Some(column) = key.column_index() {
        derived_temporal_units(first, column)?;
        first.column(column).map(crate::ColumnVector::data_type)
    } else {
        let column = key.evaluate_vector_column_quietly(first, key_declared_type(key))?;
        unit_key_column_type(&column, first.row_count())
    }
}

/// A batch as the lanes read it, with a computed key added as its last
/// column. `Err` hands back a batch the lanes cannot carry - a key with no
/// packed kernel answer for it (it raised a warning, or met a value with no
/// units), or a temporal lane's column holding a zero date or text as it
/// was written - which then folds row by row.
fn laned_batch(
    batch: RecordBatch,
    keys: TwoPassKeySource,
    key_exprs: &[CompiledExpr],
    lanes: &[TwoPassLane],
) -> Result<RecordBatch, RecordBatch> {
    let carried = lanes.iter().all(|lane| match lane {
        TwoPassLane::Temporal { column, .. } => derived_temporal_units(&batch, *column).is_some(),
        _ => true,
    });
    if !carried {
        return Err(batch);
    }
    if let TwoPassKeySource::DateParts { parts } = keys
        && !key_exprs.is_empty()
    {
        // The parts are read off the column's units; a batch without them
        // evaluates the expressions instead.
        let units = parts.iter().flatten().all(|(_, column)| {
            matches!(
                batch.column(*column).and_then(crate::ColumnVector::typed),
                Some((crate::batch::TypedValues::Temporal { .. }, _))
            )
        });
        return if units { Ok(batch) } else { Err(batch) };
    }
    let (Some(key), TwoPassKeySource::Int { column, group_type }) = (key_exprs.first(), keys)
    else {
        return Ok(batch);
    };
    if key.column_index().is_some() {
        return if derived_temporal_units(&batch, column).is_some() {
            Ok(batch)
        } else {
            Err(batch)
        };
    }
    if batch.columns().len() != column {
        return Err(batch);
    }
    match key.evaluate_vector_column_quietly(&batch, key_declared_type(key)) {
        Some(computed)
            if unit_key_column_type(&computed, batch.row_count()) == Some(group_type) =>
        {
            batch.with_appended_column(computed)
        }
        _ => Err(batch),
    }
}

/// The bits of a unit key's value, as the lanes would have read them from
/// its column; `None` for a value with no units - a zero date, text that is
/// not the canonical spelling - whose group is then kept by value.
fn unit_key_bits(value: &Value, group_type: DataType) -> Option<(u64, bool)> {
    let units = |units: i64| (u64::from_ne_bytes(units.to_ne_bytes()), false);
    match (group_type, value) {
        (_, Value::Null) => Some((0, true)),
        (DataType::Date32, Value::Utf8(text)) => pintail_types::parse_date_days(text)
            .filter(|days| pintail_types::format_date_days(*days).as_deref() == Some(text))
            .map(units),
        (DataType::DateTime64 { fsp }, Value::Utf8(text)) => {
            pintail_types::parse_datetime_micros(text)
                .filter(|micros| {
                    pintail_types::format_datetime_micros(*micros, fsp).as_deref() == Some(text)
                })
                .map(units)
        }
        (DataType::Date32 | DataType::DateTime64 { .. }, _) => None,
        _ => match (group_type.storage_type(), value) {
            (DataType::Int64, Value::Int64(_))
            | (DataType::UInt64, Value::UInt64(_))
            | (DataType::Float64, Value::Float64(_))
            | (DataType::Boolean, Value::Boolean(_)) => two_pass_key_bits(value),
            _ => None,
        },
    }
}

/// Groups of a unit key whose value has no units, by normalized key.
type OddGroups = HashMap<Vec<Value>, AggregateGroup>;

/// Folds one batch row by row: a batch [`laned_batch`] handed back.
///
/// Every row finds its group the way the lanes key it - by the key's bits,
/// in the partition maps, where a later flush, the dense slots and the
/// range fold all merge into the same state - or, for a key value with no
/// bits, in `odd` by its normalized value. Each lane with bits for the row
/// applies them as pass 2 does; a temporal lane whose column carries no
/// units here updates its state with the row's value instead, which is the
/// general path's update, so a zero date takes its place in a MIN or MAX
/// rather than reading as NULL.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn fold_odd_batch(
    batch: &RecordBatch,
    keys: TwoPassKeySource,
    key_exprs: &[CompiledExpr],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    maps: &mut [GroupKeyMap],
    odd: &mut OddGroups,
    mut intern: Option<&mut StringIntern>,
    collation: Collation,
    memory: &MemoryTracker,
) -> Result<usize, ExecError> {
    let partitions = maps.len();
    // Bytes reserved for the groups kept by value, which no spill frees.
    let mut odd_bytes = 0_usize;
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(size_of::<AggregateGroup>())
        .saturating_add(HASH_ENTRY_OVERHEAD);
    let by_value = lanes
        .iter()
        .map(|lane| match lane {
            TwoPassLane::Temporal { column, .. }
                if derived_temporal_units(batch, *column).is_none() =>
            {
                Some(*column)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let text_columns: &[usize] = match &keys {
        TwoPassKeySource::Text { column } => std::slice::from_ref(column),
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => &[],
    };
    let mut readers = Vec::with_capacity(text_columns.len());
    for column in text_columns {
        let intern = intern
            .as_deref_mut()
            .ok_or(ExecError::InvalidBatch("text keys carry an intern table"))?;
        readers.push(string_key_reader(batch, *column, intern, memory)?);
    }
    for row in batch.selection().selected_rows() {
        let key = match keys {
            TwoPassKeySource::Int { column, group_type } => {
                let computed = key_exprs.first().filter(|key| key.column_index().is_none());
                let value = match computed {
                    Some(key) => key.evaluate(batch, row)?,
                    None => batch
                        .column(column)
                        .and_then(|values| values.value_owned(row))
                        .ok_or(ExecError::InvalidBatch(
                            "grouping row is outside the input batch",
                        ))?,
                };
                let bits = if key_exprs.is_empty() {
                    two_pass_key_bits(&value)
                } else {
                    unit_key_bits(&value, group_type)
                };
                bits.ok_or_else(|| vec![value])
            }
            TwoPassKeySource::Text { .. } => {
                let intern = intern
                    .as_deref_mut()
                    .ok_or(ExecError::InvalidBatch("text keys carry an intern table"))?;
                let mut bits = 0_u64;
                let mut null = false;
                for (reader, validity) in &readers {
                    if validity.is_valid(row) {
                        bits = reader.read(row, intern, memory)?;
                    } else {
                        null = true;
                    }
                }
                Ok((bits, null))
            }
            TwoPassKeySource::DateParts { parts }
                if key_exprs.len() == parts.iter().flatten().count() =>
            {
                // Each part as its expression evaluates it, packed as the
                // units reading packs it; a value the 20-bit id cannot hold
                // keeps its group by value.
                let values = key_exprs
                    .iter()
                    .map(|key| key.evaluate(batch, row))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bits = Some(0_u64);
                for value in &values {
                    let id = match value {
                        Value::Null => Some(0),
                        Value::Int64(part) => u64::try_from(*part).ok().map(|part| part + 1),
                        Value::UInt64(part) => part.checked_add(1),
                        _ => None,
                    }
                    .filter(|id| *id <= 0xF_FFFF);
                    bits = bits.zip(id).map(|(bits, id)| (bits << 20) | id);
                }
                bits.map(|bits| (bits, false)).ok_or(values)
            }
            TwoPassKeySource::DateParts { parts } => {
                Ok((date_parts_key_bits(batch, row, parts)?, false))
            }
        };
        let states = match key {
            Ok((bits, null)) => {
                let partition = usize::try_from(
                    crate::batch::mix64(bits ^ u64::from(null)) % partitions as u64,
                )
                .expect("partition index fits usize");
                match maps[partition].entry((bits, null)) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        memory.reserve(per_group_bytes)?;
                        entry.insert(aggregates.iter().map(AggregateState::new).collect())
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                }
            }
            Err(values) => {
                let normalized = values
                    .iter()
                    .cloned()
                    .map(|value| normalized_group_hash_key(value, collation).unwrap_or(Value::Null))
                    .collect::<Vec<_>>();
                match odd.entry(normalized) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let bytes = per_group_bytes
                            .saturating_add(estimated_row_payload_bytes(&values).saturating_mul(2));
                        memory.reserve(bytes)?;
                        odd_bytes = odd_bytes.saturating_add(bytes);
                        &mut entry
                            .insert(AggregateGroup {
                                values,
                                states: aggregates.iter().map(AggregateState::new).collect(),
                            })
                            .states
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        &mut entry.into_mut().states
                    }
                }
            }
        };
        for (index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
            if let Some(column) = by_value[index] {
                match batch.column(column).and_then(|values| values.value(row)) {
                    Some(Value::Null) | None => {}
                    Some(value) => states[index].update(aggregate, value, memory)?,
                }
            } else if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                apply_two_pass_lane(&mut states[index], lane, aggregate, bits, memory)?;
            }
        }
    }
    Ok(odd_bytes)
}

/// Fewest rows a piece of an oversized batch holds: below this the pieces
/// cost more to schedule than their groups cost to hold.
const SLICE_ROWS_FLOOR: usize = 1_024;

/// Cuts `batch` into pieces of at most `slice_rows` visible rows each. A
/// piece is the batch itself under a selection narrowed to a run of its
/// rows, so the columns are shared and nothing is copied; each carries the
/// part of `bytes` its rows are of the whole.
fn slice_batch(
    batch: &RecordBatch,
    slice_rows: usize,
    bytes: usize,
    pieces: &mut VecDeque<(RecordBatch, usize)>,
) {
    let total = batch.visible_row_count().max(1);
    let mut cut = |rows: std::ops::Range<usize>, held: usize| {
        let mut piece = batch.clone();
        piece.selection_mut().keep_only(rows);
        pieces.push_back((piece, bytes.saturating_mul(held) / total));
    };
    let (mut start, mut held, mut position) = (0_usize, 0_usize, 0_usize);
    while position < batch.row_count() {
        let end = position.saturating_add(slice_rows).min(batch.row_count());
        let rows = batch.selection().count_in(position..end);
        if held > 0 && held + rows > slice_rows {
            cut(start..position, held);
            start = position;
            held = 0;
        }
        held += rows;
        position = end;
    }
    if held > 0 {
        cut(start..batch.row_count(), held);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn streaming_two_pass(
    input: &mut PullOperator,
    first: RecordBatch,
    keys: TwoPassKeySource,
    key_exprs: &[CompiledExpr],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<MaterializedRows, ExecError> {
    // Several partitions per worker, not one. The count is usually read as
    // "how many threads share this map", which argues for one each - but the
    // partition that matters here is the cache, not the scheduler. Every
    // update is a random probe into its partition's map, so the cost is set
    // by whether that map fits a core's private cache. Splitting finer keeps
    // each map small enough that it does, and the extra partitions cost only
    // one more (empty) bucket per batch.
    //
    // Measured on the 20M-row benchmark, high-cardinality aggregation (100k
    // groups) against partitions per worker: 1x 674ms, 2x 527ms, 4x 486ms,
    // 8x 500ms, 16x 527ms. Low-cardinality shapes are indifferent, having
    // too few groups to miss either way. The floor is broad rather than
    // sharp - 2x and 8x sit within 4% of 4x - so this multiplier is a
    // region, not a tuned constant, and it does not need refitting per host.
    let partitions = std::thread::available_parallelism()
        .map_or(8, usize::from)
        .saturating_mul(PARTITIONS_PER_WORKER);
    let lane_count = lanes.len();
    let scatter_row_bytes = size_of::<u64>() * (1 + lane_count) + 1;
    // The scatter window is sized so that one flush of it, every row a new
    // group with its own distinct entry, fits in half the ceiling. A flush
    // applies its rows into the group maps and their distinct sets, which
    // grow by an order of magnitude more than the scattered bytes, and a
    // flush that runs out part-way cannot be replayed: the spill valve runs
    // BEFORE a flush, at half the ceiling, and the window guarantees the
    // other half is enough.
    let per_row_growth = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(HASH_ENTRY_OVERHEAD)
        .saturating_add(128);
    let flush_bytes = ((memory.limit() / 2) / per_row_growth.max(1))
        .saturating_mul(scatter_row_bytes)
        .clamp(128 << 10, 64 << 20);
    let scan_floor = input.scan_transient_floor().saturating_mul(2);
    // An input that reports no floor - a join, a subquery - still needs room
    // to produce its next batch. The largest batch it has produced stands in
    // for the floor it does not report, so relief runs before the buffered
    // windows leave that batch nowhere to go.
    let mut observed_floor = 0_usize;
    let mut buckets: Vec<TwoPassBucket> =
        (0..partitions).map(|_| TwoPassBucket::default()).collect();
    let mut maps: Vec<GroupKeyMap> = (0..partitions).map(|_| GroupKeyMap::default()).collect();
    let mut bucket_reserved = 0_usize;
    let mut group_reserved = 0_usize;
    let mut spill_runs: Vec<spill::ClosedRun> = Vec::new();
    let mut flushes = 0_u32;
    let mut intern = matches!(keys, TwoPassKeySource::Text { .. }).then(|| StringIntern {
        index: HashMap::new(),
        values: Vec::new(),
        reserved: 0,
        collation,
    });
    // Distinct lanes take the dense slots too: each worker's partial holds
    // its own copy of a group's distinct set, and the partials merge by
    // unioning those sets - word by word for bitmaps - rather than by
    // replaying every member, which is what once made this path lose to
    // the scatter.
    let mut dense = dense_slot_count(keys).map(|slots| {
        let mut table: DenseGroupSlots = Vec::new();
        table.resize_with(slots, || None);
        table
    });
    if let Some(slots) = &dense {
        let slab = dense_reservation(keys, slots.len(), aggregates.len());
        if memory.reserve(slab).is_ok() {
            group_reserved = group_reserved.saturating_add(slab);
        } else {
            dense = None;
        }
    }

    let mut range = IntRange::default();
    let mut dense_pool = DensePool::default();
    let mut window: Vec<(RecordBatch, Vec<Vec<u64>>)> = Vec::new();
    let mut window_reserved = 0_usize;
    let mut window_rows = 0_usize;
    // Declared ENUM labels per text key column, captured from the first
    // batch that carries them. The intern table holds label TEXT only, so
    // without this the finalize below rebuilds every group key as a plain
    // string and the declaration index - which is what MySQL orders an
    // ENUM by - is erased exactly here (#251).
    let mut key_enum_labels: [Option<(std::sync::Arc<Vec<String>>, bool)>; 2] = [None, None];
    let mut key_set_members: [Option<std::sync::Arc<Vec<String>>>; 2] = [None, None];
    let key_columns: [Option<usize>; 2] = match keys {
        TwoPassKeySource::Text { column } => [Some(column), None],
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => [None, None],
    };
    let mut odd = OddGroups::new();
    let mut odd_reserved = 0_usize;
    let mut odd_batches = 0_usize;
    let mut batch = Some(first);
    // A batch larger than one flush of the window, cut into pieces the
    // window takes one at a time, each with its share of the batch's bytes.
    // The input is not pulled again until the last piece is taken.
    let mut sliced: VecDeque<(RecordBatch, usize)> = VecDeque::new();
    let slice_rows = (flush_bytes / scatter_row_bytes.max(1)).max(SLICE_ROWS_FLOOR);
    loop {
        let slice = sliced.pop_front();
        let fresh = slice.is_none();
        let Some((current, held_bytes)) = slice.or_else(|| batch.take().map(|batch| (batch, 0)))
        else {
            break;
        };
        for (slot, key_column) in key_columns.iter().enumerate() {
            // A complete declaration settles the slot. A table rebuilt from
            // the ordinals one batch held does not: the next batch may hold
            // labels this one lacked, or be the first to carry the catalog's
            // own declaration, and a key resolved against the partial table
            // alone would come out as plain text and sort alphabetically
            // beside its ordinal-sorted neighbours.
            let settled = key_set_members[slot].is_some()
                || key_enum_labels[slot]
                    .as_ref()
                    .is_some_and(|(_, exhaustive)| *exhaustive);
            if let Some(column) = key_column
                && !settled
                && let Some(vector) = current.column(*column)
                && let Some((crate::batch::TypedValues::Utf8(strings), _)) = vector.typed()
            {
                let exhaustive = strings.enum_labels_exhaustive();
                match (key_enum_labels[slot].take(), strings.declared_enum_labels()) {
                    (Some((held, _)), Some(seen)) if !exhaustive => {
                        key_enum_labels[slot] = Some((merge_partial_labels(held, seen), false));
                    }
                    (_, Some(seen)) => {
                        key_enum_labels[slot] = Some((std::sync::Arc::clone(seen), exhaustive));
                    }
                    (held, None) => key_enum_labels[slot] = held,
                }
                key_set_members[slot] = strings.declared_set_members().cloned();
            }
        }
        // A piece of a batch already cut carries its lanes.
        let carried = if fresh {
            laned_batch(current, keys, key_exprs, lanes)
        } else {
            Ok(current)
        };
        let current = match carried {
            Ok(current) => current,
            Err(current) => {
                // No lane can carry this batch: its rows go straight to
                // their groups. Relief first, as before any flush, so the
                // groups it adds do not meet a full budget.
                two_pass_relieve(
                    &mut TwoPassState {
                        maps: &mut maps,
                        dense: &mut dense,
                        range: &mut range,
                        pool: &mut dense_pool,
                        group_reserved: &mut group_reserved,
                        spill_runs: &mut spill_runs,
                    },
                    keys,
                    aggregates,
                    partitions,
                    intern.as_ref(),
                    &key_enum_labels,
                    &key_set_members,
                    collation,
                    memory,
                )?;
                let used_before = memory.used();
                let kept = fold_odd_batch(
                    &current,
                    keys,
                    key_exprs,
                    lanes,
                    aggregates,
                    &mut maps,
                    &mut odd,
                    intern.as_mut(),
                    collation,
                    memory,
                );
                // Whatever was reserved is held whether or not the fold
                // finished: the map entries and what their states grew by
                // belong to the maps, the rest to the groups kept by value.
                let grown = memory.used().saturating_sub(used_before);
                let kept_bytes = *kept.as_ref().unwrap_or(&0);
                odd_reserved = odd_reserved.saturating_add(kept_bytes);
                group_reserved = group_reserved.saturating_add(grown.saturating_sub(kept_bytes));
                kept?;
                odd_batches += 1;
                batch = input.next_batch(memory)?;
                continue;
            }
        };
        let (current, held_bytes) = if fresh {
            let bytes = current.estimated_bytes();
            if current.visible_row_count() > slice_rows {
                // Applied whole, a batch this large adds more groups and
                // distinct entries in one flush than the half of the ceiling
                // the window is sized to leave free, and a flush that runs
                // out part-way cannot be replayed. Its pieces are flushed,
                // and their groups spilled, one window at a time.
                slice_batch(&current, slice_rows, bytes, &mut sliced);
                continue;
            }
            (current, bytes)
        } else {
            (current, held_bytes)
        };
        // String sources prepare their (tiny, per-distinct-value) dictionary
        // translations serially, then scatter rows in parallel from the
        // read-only tables; batches whose strings decoded without codes
        // fall back to the serial scatter below.
        let prepared = match (keys, &mut intern) {
            (TwoPassKeySource::Text { column }, Some(intern)) => {
                prepare_text_translations(&current, &[column], intern, memory)?
            }
            _ => Some(Vec::new()),
        };
        observed_floor = observed_floor.max(current.estimated_bytes().saturating_mul(2));
        let floor = scan_floor.max(observed_floor);
        if let Some(translations) = prepared {
            let rows = current.visible_row_count();
            let need = rows
                .saturating_mul(scatter_row_bytes)
                .saturating_add(held_bytes);
            if memory.reserve(need).is_err() {
                two_pass_relieve(
                    &mut TwoPassState {
                        maps: &mut maps,
                        dense: &mut dense,
                        range: &mut range,
                        pool: &mut dense_pool,
                        group_reserved: &mut group_reserved,
                        spill_runs: &mut spill_runs,
                    },
                    keys,
                    aggregates,
                    partitions,
                    intern.as_ref(),
                    &key_enum_labels,
                    &key_set_members,
                    collation,
                    memory,
                )?;
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    &mut range,
                    &mut dense_pool,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                    &mut GroupSpill {
                        keys,
                        intern: intern.as_ref(),
                        labels: &key_enum_labels,
                        members: &key_set_members,
                        collation,
                        runs: &mut spill_runs,
                    },
                )?;
                window_rows = 0;
                flushes += 1;
                if memory.reserve(need).is_err() {
                    // The window is gone; the groups themselves are what
                    // fill the budget, so they go to disk.
                    two_pass_spill(
                        &mut TwoPassState {
                            maps: &mut maps,
                            dense: &mut dense,
                            range: &mut range,
                            pool: &mut dense_pool,
                            group_reserved: &mut group_reserved,
                            spill_runs: &mut spill_runs,
                        },
                        keys,
                        aggregates,
                        partitions,
                        intern.as_ref(),
                        &key_enum_labels,
                        &key_set_members,
                        collation,
                        memory,
                    )?;
                    memory.reserve(need)?;
                }
            }
            window_reserved = window_reserved.saturating_add(need);
            window_rows += rows;
            window.push((current, translations));
            if window_rows.saturating_mul(scatter_row_bytes) >= flush_bytes
                || (floor > 0 && memory.remaining() < floor)
                || pending_under_pressure(memory, window_rows, per_row_growth)
            {
                two_pass_relieve(
                    &mut TwoPassState {
                        maps: &mut maps,
                        dense: &mut dense,
                        range: &mut range,
                        pool: &mut dense_pool,
                        group_reserved: &mut group_reserved,
                        spill_runs: &mut spill_runs,
                    },
                    keys,
                    aggregates,
                    partitions,
                    intern.as_ref(),
                    &key_enum_labels,
                    &key_set_members,
                    collation,
                    memory,
                )?;
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    &mut range,
                    &mut dense_pool,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                    &mut GroupSpill {
                        keys,
                        intern: intern.as_ref(),
                        labels: &key_enum_labels,
                        members: &key_set_members,
                        collation,
                        runs: &mut spill_runs,
                    },
                )?;
                window_rows = 0;
                flushes += 1;
                if two_pass_under_pressure(&maps, dense.as_ref(), &range, &dense_pool, memory) {
                    two_pass_spill(
                        &mut TwoPassState {
                            maps: &mut maps,
                            dense: &mut dense,
                            range: &mut range,
                            pool: &mut dense_pool,
                            group_reserved: &mut group_reserved,
                            spill_runs: &mut spill_runs,
                        },
                        keys,
                        aggregates,
                        partitions,
                        intern.as_ref(),
                        &key_enum_labels,
                        &key_set_members,
                        collation,
                        memory,
                    )?;
                    clear_idle_intern(
                        &mut intern,
                        window.is_empty() && bucket_reserved == 0,
                        &maps,
                        dense.is_none(),
                        memory,
                    );
                }
            }
            if sliced.is_empty() {
                batch = input.next_batch(memory)?;
            }
            continue;
        }
        let rows = current.visible_row_count();
        let bytes = rows.saturating_mul(scatter_row_bytes);
        if memory.reserve(bytes).is_err() {
            // Free the scatter window and retry; a second failure means the
            // group states themselves fill the budget, so they go to disk.
            two_pass_relieve(
                &mut TwoPassState {
                    maps: &mut maps,
                    dense: &mut dense,
                    range: &mut range,
                    pool: &mut dense_pool,
                    group_reserved: &mut group_reserved,
                    spill_runs: &mut spill_runs,
                },
                keys,
                aggregates,
                partitions,
                intern.as_ref(),
                &key_enum_labels,
                &key_set_members,
                collation,
                memory,
            )?;
            two_pass_flush(
                &mut buckets,
                &mut maps,
                lanes,
                aggregates,
                memory,
                &mut group_reserved,
            )?;
            memory.release(bucket_reserved);
            bucket_reserved = 0;
            flushes += 1;
            if memory.reserve(bytes).is_err() {
                two_pass_spill(
                    &mut TwoPassState {
                        maps: &mut maps,
                        dense: &mut dense,
                        range: &mut range,
                        pool: &mut dense_pool,
                        group_reserved: &mut group_reserved,
                        spill_runs: &mut spill_runs,
                    },
                    keys,
                    aggregates,
                    partitions,
                    intern.as_ref(),
                    &key_enum_labels,
                    &key_set_members,
                    collation,
                    memory,
                )?;
                memory.reserve(bytes)?;
            }
        }
        bucket_reserved = bucket_reserved.saturating_add(bytes);
        match (keys, &mut intern) {
            (TwoPassKeySource::Text { column }, Some(intern)) => two_pass_scatter_strings(
                &current,
                column,
                lanes,
                partitions,
                &mut buckets,
                intern,
                memory,
            )?,
            (TwoPassKeySource::Int { column, .. }, _) => {
                two_pass_scatter_batch(
                    &Morsel::whole(&current),
                    column,
                    lanes,
                    partitions,
                    &mut buckets,
                )?;
            }
            (TwoPassKeySource::DateParts { parts }, _) => {
                two_pass_scatter_date_parts(
                    &Morsel::whole(&current),
                    parts,
                    lanes,
                    partitions,
                    &mut buckets,
                )?;
            }
            _ => unreachable!("intern presence follows the key source"),
        }
        drop(current);
        if bucket_reserved >= flush_bytes
            || (floor > 0 && memory.remaining() < floor)
            || pending_under_pressure(
                memory,
                bucket_reserved / scatter_row_bytes.max(1),
                per_row_growth,
            )
        {
            two_pass_relieve(
                &mut TwoPassState {
                    maps: &mut maps,
                    dense: &mut dense,
                    range: &mut range,
                    pool: &mut dense_pool,
                    group_reserved: &mut group_reserved,
                    spill_runs: &mut spill_runs,
                },
                keys,
                aggregates,
                partitions,
                intern.as_ref(),
                &key_enum_labels,
                &key_set_members,
                collation,
                memory,
            )?;
            two_pass_flush(
                &mut buckets,
                &mut maps,
                lanes,
                aggregates,
                memory,
                &mut group_reserved,
            )?;
            memory.release(bucket_reserved);
            bucket_reserved = 0;
            flushes += 1;
            if two_pass_under_pressure(&maps, dense.as_ref(), &range, &dense_pool, memory) {
                two_pass_spill(
                    &mut TwoPassState {
                        maps: &mut maps,
                        dense: &mut dense,
                        range: &mut range,
                        pool: &mut dense_pool,
                        group_reserved: &mut group_reserved,
                        spill_runs: &mut spill_runs,
                    },
                    keys,
                    aggregates,
                    partitions,
                    intern.as_ref(),
                    &key_enum_labels,
                    &key_set_members,
                    collation,
                    memory,
                )?;
                clear_idle_intern(
                    &mut intern,
                    window.is_empty() && bucket_reserved == 0,
                    &maps,
                    dense.is_none(),
                    memory,
                );
            }
        }
        if sliced.is_empty() {
            batch = input.next_batch(memory)?;
        }
    }
    two_pass_relieve(
        &mut TwoPassState {
            maps: &mut maps,
            dense: &mut dense,
            range: &mut range,
            pool: &mut dense_pool,
            group_reserved: &mut group_reserved,
            spill_runs: &mut spill_runs,
        },
        keys,
        aggregates,
        partitions,
        intern.as_ref(),
        &key_enum_labels,
        &key_set_members,
        collation,
        memory,
    )?;
    drain_two_pass_window(
        &mut window,
        keys,
        lanes,
        aggregates,
        partitions,
        &mut maps,
        &mut dense,
        &mut range,
        &mut dense_pool,
        intern.as_ref().map_or(0, |intern| intern.values.len()),
        memory,
        &mut group_reserved,
        &mut window_reserved,
        &mut GroupSpill {
            keys,
            intern: intern.as_ref(),
            labels: &key_enum_labels,
            members: &key_set_members,
            collation,
            runs: &mut spill_runs,
        },
    )?;
    two_pass_relieve(
        &mut TwoPassState {
            maps: &mut maps,
            dense: &mut dense,
            range: &mut range,
            pool: &mut dense_pool,
            group_reserved: &mut group_reserved,
            spill_runs: &mut spill_runs,
        },
        keys,
        aggregates,
        partitions,
        intern.as_ref(),
        &key_enum_labels,
        &key_set_members,
        collation,
        memory,
    )?;
    two_pass_flush(
        &mut buckets,
        &mut maps,
        lanes,
        aggregates,
        memory,
        &mut group_reserved,
    )?;
    memory.release(bucket_reserved);
    settle_dense(
        &mut dense,
        &mut dense_pool,
        aggregates,
        memory,
        &mut group_reserved,
    )?;
    if let Some(slots) = dense.take() {
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            &mut maps,
            memory,
            &mut group_reserved,
            &mut GroupSpill {
                keys,
                intern: intern.as_ref(),
                labels: &key_enum_labels,
                members: &key_set_members,
                collation,
                runs: &mut spill_runs,
            },
        )?;
    }
    if odd_batches > 0 {
        // Otherwise invisible: the answer is the same, only slower.
        super::ProfileNote::of(input).set(&format!(
            "two-pass: {odd_batches} batches without packed units folded row by row"
        ));
    }
    if let IntRange::Active(active) = std::mem::replace(&mut range, IntRange::Off) {
        if let TwoPassKeySource::Int { group_type, .. } = keys
            && spill_runs.is_empty()
            && odd.is_empty()
            && maps.iter().all(HashMap::is_empty)
        {
            // Every group is in the range fold: finish them straight from
            // its slots, in key order, without building a map entry each.
            let ready = finish_int_range(&active, group_type, aggregates, memory);
            memory.release(active.reserved);
            memory.release(group_reserved);
            if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
                eprintln!(
                    "[agg] streaming two-pass: range fold of {} slots, {} flushes",
                    active.slot_count,
                    flushes + 1
                );
            }
            return Ok(MaterializedRows {
                rows: Vec::new(),
                position: 0,
                spilled: None,
                ready: Some(ready?),
            });
        }
        fold_int_range_into_maps(
            &active,
            aggregates,
            partitions,
            &mut maps,
            memory,
            &mut group_reserved,
            &mut GroupSpill {
                keys,
                intern: intern.as_ref(),
                labels: &key_enum_labels,
                members: &key_set_members,
                collation,
                runs: &mut spill_runs,
            },
        )?;
    }
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        let groups: usize = maps.iter().map(HashMap::len).sum();
        eprintln!(
            "[agg] streaming two-pass: {groups} groups, {} flushes, {} spill runs, \
             {odd_batches} batches row by row, {} groups kept by value",
            flushes + 1,
            spill_runs.len(),
            odd.len()
        );
    }
    if !spill_runs.is_empty() {
        // Groups went to disk along the way: the remainder joins them and
        // the shared merge combines each group once, in run order.
        let mut resident = two_pass_groups_map(
            &mut maps,
            keys,
            intern.as_ref(),
            &key_enum_labels,
            &key_set_members,
            collation,
        );
        // A key kept by value has no bits, so no run holds its group.
        resident.extend(odd);
        memory.release(group_reserved);
        memory.release(odd_reserved);
        return merge_spilled_aggregate_groups(spill_runs, resident, memory);
    }

    // Finalize each partition in parallel; ORDER BY above owns ordering.
    let intern_ref = intern.as_ref();
    let finalized = maps
        .into_par_iter()
        .map(|map| -> Result<(Vec<Vec<Value>>, usize), ExecError> {
            let mut rows = Vec::with_capacity(map.len());
            let mut payload = 0_usize;
            // Charged in slices rather than per row: every partition's
            // worker charging each of a hundred thousand rows on the shared
            // counters cost more than finishing the rows. A slice bounds
            // how far a partition runs past the ceiling before it refuses.
            let mut uncharged = 0_usize;
            for ((bits, null), states) in map {
                let mut row = two_pass_key_values(
                    keys,
                    bits,
                    null,
                    intern_ref,
                    &key_enum_labels,
                    &key_set_members,
                );
                row.reserve(states.len());
                for state in states {
                    row.push(state.finish(memory)?);
                }
                let bytes = estimated_row_payload_bytes(&row);
                uncharged = uncharged.saturating_add(bytes);
                if uncharged >= FINALIZE_CHARGE_SLICE {
                    memory.reserve(uncharged)?;
                    uncharged = 0;
                }
                payload = payload.saturating_add(bytes);
                rows.push(row);
            }
            memory.reserve(uncharged)?;
            Ok((rows, payload))
        })
        .collect::<Result<Vec<_>, _>>();
    memory.release(group_reserved);
    memory.release(odd_reserved);
    let finalized = finalized?;
    let mut rows = Vec::new();
    for (partition_rows, _) in finalized {
        rows.extend(partition_rows);
    }
    for (_, group) in odd {
        let mut row = group.values;
        row.reserve(group.states.len());
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows {
        rows,
        position: 0,
        spilled: None,
        ready: None,
    })
}

/// Pass 1 for one batch: extract (key bits, lane bits, null mask) per
/// selected row into the partition buckets. Reservation is the caller\'s.
fn two_pass_scatter_batch(
    morsel: &Morsel<'_>,
    group_column: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let batch = morsel.batch;
    let group_values = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    let readers = lane_readers(batch, lanes);
    // Packed integer keys read their bits straight from storage; the cell
    // path below would materialize the whole key column to return them.
    let packed_key = match group_values.typed() {
        Some((crate::batch::TypedValues::Int64(values), validity)) => {
            Some((PackedInts::Signed(values.as_slice()), validity))
        }
        Some((crate::batch::TypedValues::UInt64(values), validity)) => {
            Some((PackedInts::Unsigned(values.as_slice()), validity))
        }
        _ => derived_temporal_units(batch, group_column)
            .map(|(units, validity)| (PackedInts::Signed(units), validity)),
    };
    if let Some((values, validity)) = packed_key {
        for row in morsel.selected_rows() {
            let (key_bits, key_null) = if validity.is_valid(row) {
                (values.bits(row), false)
            } else {
                (0, true)
            };
            scatter_two_pass_row(&readers, row, key_bits, key_null, partitions, buckets);
        }
        return Ok(());
    }
    for row in morsel.selected_rows() {
        let value = group_values.value(row).ok_or(ExecError::InvalidBatch(
            "grouping row is outside the input batch",
        ))?;
        let (key_bits, key_null) = two_pass_key_bits(value)
            .ok_or(ExecError::InvalidBatch("two-pass key is not scalar"))?;
        scatter_two_pass_row(&readers, row, key_bits, key_null, partitions, buckets);
    }
    Ok(())
}

/// String-keyed scatter: group keys are interned string ids — dictionary
/// codes translate per batch (one intern per distinct entry), degraded
/// plain-text chunks intern per row. No Value cell is ever built.
fn two_pass_scatter_strings(
    batch: &RecordBatch,
    group_column: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let vector = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
        return Err(ExecError::InvalidBatch(
            "string two-pass key column lost its typed projection",
        ));
    };
    if let Some((codes, dict_values)) = strings.dictionary() {
        let readers = lane_readers(batch, lanes);
        let translation =
            intern_carried_entries(batch, codes, validity, dict_values, intern, memory)?;
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                translation[usize::try_from(codes[row]).expect("dict code fits usize")]
            };
            scatter_two_pass_row(&readers, row, key_bits, key_null, partitions, buckets);
        }
    } else {
        let (views, heap) = (strings.views(), strings.heap());
        let readers = lane_readers(batch, lanes);
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                views[row].with_bytes(heap, |bytes| intern.intern(bytes, memory))?
            };
            scatter_two_pass_row(&readers, row, key_bits, key_null, partitions, buckets);
        }
    }
    Ok(())
}

/// One string column's per-batch key extractor: dictionary translation
/// when codes survive, per-row view interning otherwise.
enum StringKeyReader<'a> {
    Dict {
        codes: &'a [u32],
        translation: Vec<u64>,
    },
    Plain {
        views: &'a [crate::array::StrView],
        heap: &'a [u8],
    },
}

impl StringKeyReader<'_> {
    fn read(
        &self,
        row: usize,
        intern: &mut StringIntern,
        memory: &MemoryTracker,
    ) -> Result<u64, ExecError> {
        match self {
            Self::Dict { codes, translation } => {
                Ok(translation[usize::try_from(codes[row]).expect("dict code fits usize")])
            }
            Self::Plain { views, heap } => {
                views[row].with_bytes(heap, |bytes| intern.intern(bytes, memory))
            }
        }
    }
}

fn string_key_reader<'a>(
    batch: &'a RecordBatch,
    column: usize,
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(StringKeyReader<'a>, &'a crate::array::ValidityMask), ExecError> {
    let vector = batch.column(column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
        return Err(ExecError::InvalidBatch(
            "string two-pass key column lost its typed projection",
        ));
    };
    let reader = if let Some((codes, dict_values)) = strings.dictionary() {
        StringKeyReader::Dict {
            codes,
            translation: intern_carried_entries(
                batch,
                codes,
                validity,
                dict_values,
                intern,
                memory,
            )?,
        }
    } else {
        StringKeyReader::Plain {
            views: strings.views(),
            heap: strings.heap(),
        }
    };
    Ok((reader, validity))
}

/// Resolves this batch's dictionary translations against the global
/// intern table — the only step that needs `&mut intern`, and it costs one
/// intern per DISTINCT value. Returns `None` when any key column decoded
/// without codes (plain views need per-row interning, so those batches
/// stay on the serial path).
fn prepare_text_translations(
    batch: &RecordBatch,
    columns: &[usize],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<Option<Vec<Vec<u64>>>, ExecError> {
    let mut prepared = Vec::with_capacity(columns.len());
    for column in columns {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, dict_values)) = strings.dictionary() else {
            return Ok(None);
        };
        prepared.push(intern_carried_entries(
            batch,
            codes,
            validity,
            dict_values,
            intern,
            memory,
        )?);
    }
    Ok(Some(prepared))
}

/// The id of a dictionary code no selected row carries. No row reads it;
/// one below the maximum so the dense tables' `id + 1` still has a value
/// to reject.
const NO_INTERN_ID: u64 = u64::MAX - 1;

/// Interns the dictionary entries `batch`'s selected rows carry, in the
/// order those rows first show them, and returns each code's id.
///
/// The intern keeps the first spelling it meets of every set the collation
/// calls equal, and that spelling is the one the group displays. Interning
/// the dictionary front to back let an entry no selected row carries - a
/// row the filter dropped - name the group: `GROUP BY name` over rows
/// holding only `Å` answered `A`, a spelling absent from the result's
/// input. A code no selected row carries keeps [`NO_INTERN_ID`].
fn intern_carried_entries(
    batch: &RecordBatch,
    codes: &[u32],
    validity: &crate::array::ValidityMask,
    dict_values: &[String],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<Vec<u64>, ExecError> {
    let mut translation = vec![NO_INTERN_ID; dict_values.len()];
    let mut missing = dict_values.len();
    for row in batch.selection().selected_rows() {
        if missing == 0 {
            // Every entry has its id: the usual end, a few rows in.
            break;
        }
        if !validity.is_valid(row) {
            continue;
        }
        let code = usize::try_from(codes[row]).expect("dict code fits usize");
        let slot = translation
            .get_mut(code)
            .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
        if *slot == NO_INTERN_ID {
            *slot = intern.intern(dict_values[code].as_bytes(), memory)?;
            missing -= 1;
        }
    }
    Ok(translation)
}

/// Scatters string keys from prepared (read-only) translations: no intern
/// access, so windows of batches scatter in parallel.
fn two_pass_scatter_text_prepared(
    morsel: &Morsel<'_>,
    columns: &[usize],
    translations: &[Vec<u64>],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let batch = morsel.batch;
    let mut readers = Vec::with_capacity(columns.len());
    for (column, translation) in columns.iter().zip(translations) {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, _)) = strings.dictionary() else {
            return Err(ExecError::InvalidBatch(
                "prepared text scatter requires dictionary codes",
            ));
        };
        readers.push((codes, validity, translation));
    }
    let values = lane_readers(batch, lanes);
    for row in morsel.selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                key_bits = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
            } else {
                key_null = true;
            }
        }
        scatter_two_pass_row(&values, row, key_bits, key_null, partitions, buckets);
    }
    Ok(())
}

/// Up to two bounded date-part expressions as the group key: values come
/// straight from packed temporal units (no Value cells, no text).
fn two_pass_scatter_date_parts(
    morsel: &Morsel<'_>,
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let batch = morsel.batch;
    let readers = lane_readers(batch, lanes);
    for row in morsel.selected_rows() {
        let key_bits = date_parts_key_bits(batch, row, parts)?;
        scatter_two_pass_row(&readers, row, key_bits, false, partitions, buckets);
    }
    Ok(())
}

/// One row's date-part key: each part's `(value + 1)` in 20 bits, 0 for a
/// NULL part.
fn date_parts_key_bits(
    batch: &RecordBatch,
    row: usize,
    parts: [Option<(DatePart, usize)>; 2],
) -> Result<u64, ExecError> {
    let mut key_bits = 0_u64;
    for (part, column) in parts.iter().flatten() {
        let id = match crate::expression::evaluate_units_date_part(batch, *column, row, *part) {
            // Ids are 20-bit lane slots (value + 1, 0 = NULL); a value
            // the lane cannot carry - negative, or past the mask - must
            // refuse rather than collide with a real slot.
            Some(Ok(Value::Int64(value))) => match u64::try_from(value) {
                Ok(value) if value < 0xF_FFFF => value + 1,
                _ => {
                    return Err(ExecError::InvalidBatch(
                        "date-part group key does not fit its 20-bit lane",
                    ));
                }
            },
            Some(Ok(Value::Null)) => 0,
            Some(Err(error)) => return Err(error),
            _ => {
                return Err(ExecError::InvalidBatch(
                    "date-part group key column lost its packed units",
                ));
            }
        };
        debug_assert!(id < 1 << 20, "date part value fits 20 bits");
        key_bits = (key_bits << 20) | id;
    }
    Ok(key_bits)
}

#[inline]
/// Extracts one lane's scatter bits for one row; `None` is the NULL mark.
/// Shared by the scatter path (which buffers the bits) and the dense direct
/// path (which applies them immediately).
fn two_pass_lane_bits(batch: &RecordBatch, row: usize, lane: &TwoPassLane) -> Option<u64> {
    match lane {
        TwoPassLane::CountStar => Some(0),
        TwoPassLane::Float { column } => batch
            .column(*column)
            .and_then(|column| {
                let (typed, validity) = column.typed()?;
                validity
                    .is_valid(row)
                    .then(|| typed.number_at(row))
                    .flatten()
            })
            .or_else(|| {
                batch
                    .column(*column)
                    .and_then(|column| match column.value(row) {
                        Some(Value::Null) | None => None,
                        Some(value) => mysql_f64(value).ok(),
                    })
            })
            .map(f64::to_bits),
        TwoPassLane::DecimalUnits { column, scale, .. }
        | TwoPassLane::ExtremeDecimal { column, scale } => {
            batch
                .column(*column)
                .and_then(|column| match column.typed() {
                    Some((typed, validity)) => validity
                        .is_valid(row)
                        .then(|| typed.units_at(row))
                        .flatten(),
                    // A column a projection built from values has no packed
                    // units: a CASE whose narrower branch keeps its own
                    // label is one. Reading only packed units answered
                    // every such row as NULL, so a grouped SUM over it
                    // came back NULL.
                    None => column
                        .value(row)
                        .and_then(|value| decimal_units_at_scale(value, *scale)),
                })
                .and_then(|units| i64::try_from(units).ok())
                .map(|units| u64::from_ne_bytes(units.to_ne_bytes()))
        }
        TwoPassLane::Present { column } => {
            let vector = batch.column(*column)?;
            let present = match vector.typed() {
                Some((_, validity)) => validity.is_valid(row),
                None => !matches!(vector.value(row), Some(Value::Null) | None),
            };
            present.then_some(0)
        }
        TwoPassLane::Temporal { column, .. } => {
            let (units, validity) = derived_temporal_units(batch, *column)?;
            validity
                .is_valid(row)
                .then(|| u64::from_ne_bytes(units[row].to_ne_bytes()))
        }
        TwoPassLane::Int { column, .. }
        | TwoPassLane::Exact { column, .. }
        | TwoPassLane::Distinct { column, .. } => {
            match batch.column(*column).and_then(|column| column.value(row)) {
                Some(Value::Int64(value)) => Some(u64::from_ne_bytes(value.to_ne_bytes())),
                Some(Value::UInt64(value)) => Some(*value),
                Some(Value::Float64(value)) => Some(value.get().to_bits()),
                Some(Value::Boolean(value)) => Some(u64::from(*value)),
                _ => None,
            }
        }
    }
}

/// A packed integer key column, signed or unsigned.
#[derive(Clone, Copy)]
enum PackedInts<'a> {
    Signed(&'a [i64]),
    Unsigned(&'a [u64]),
}

impl PackedInts<'_> {
    /// The bits `two_pass_key_bits` gives this row's `Int64`/`UInt64` cell.
    #[inline]
    fn bits(self, row: usize) -> u64 {
        match self {
            Self::Signed(values) => u64::from_ne_bytes(values[row].to_ne_bytes()),
            Self::Unsigned(values) => values[row],
        }
    }
}

/// One lane's input column, resolved once per batch. The row loop then
/// reads packed storage directly: resolving per row looked the column up
/// and matched its representation for every row and lane, and the integer
/// lanes read through `ColumnVector::value`, which materialized the whole
/// column as `Value` cells to hand back one integer.
pub(super) enum LaneReader<'a> {
    CountStar,
    /// Scaled decimal units from a packed decimal column.
    Units(
        &'a crate::batch::DecimalUnits,
        &'a crate::array::ValidityMask,
    ),
    Int64(&'a [i64], &'a crate::array::ValidityMask),
    UInt64(&'a [u64], &'a crate::array::ValidityMask),
    /// Whether the row holds a value, for a COUNT of a column.
    Presence(&'a crate::array::ValidityMask),
    /// Any other representation takes the per-row reader.
    Row(&'a RecordBatch, &'a TwoPassLane),
}

impl LaneReader<'_> {
    /// The same bits [`two_pass_lane_bits`] returns for this row.
    #[inline]
    pub(super) fn bits(&self, row: usize) -> Option<u64> {
        match self {
            Self::CountStar => Some(0),
            Self::Units(values, validity) => validity
                .is_valid(row)
                .then(|| values.get(row))
                .flatten()
                .and_then(|units| i64::try_from(units).ok())
                .map(|units| u64::from_ne_bytes(units.to_ne_bytes())),
            Self::Int64(values, validity) => validity
                .is_valid(row)
                .then(|| u64::from_ne_bytes(values[row].to_ne_bytes())),
            Self::UInt64(values, validity) => validity.is_valid(row).then(|| values[row]),
            Self::Presence(validity) => validity.is_valid(row).then_some(0),
            Self::Row(batch, lane) => two_pass_lane_bits(batch, row, lane),
        }
    }
}

pub(super) fn lane_readers<'a>(
    batch: &'a RecordBatch,
    lanes: &'a [TwoPassLane],
) -> Vec<LaneReader<'a>> {
    use crate::batch::TypedValues;
    lanes
        .iter()
        .map(|lane| {
            let typed = |column: usize| batch.column(column).and_then(crate::ColumnVector::typed);
            match lane {
                TwoPassLane::CountStar => LaneReader::CountStar,
                TwoPassLane::DecimalUnits { column, .. }
                | TwoPassLane::ExtremeDecimal { column, .. } => match typed(*column) {
                    Some((TypedValues::Decimal128 { values, .. }, validity)) => {
                        LaneReader::Units(values, validity)
                    }
                    _ => LaneReader::Row(batch, lane),
                },
                // A packed integer vector materializes exactly these values
                // (`Int64`/`UInt64` cells, NULL where invalid); floats are
                // normalized on the way into a cell, so they keep the row
                // reader.
                TwoPassLane::Int { column, .. }
                | TwoPassLane::Exact { column, .. }
                | TwoPassLane::Distinct { column, .. } => match typed(*column) {
                    Some((TypedValues::Int64(values), validity)) => {
                        LaneReader::Int64(values, validity)
                    }
                    Some((TypedValues::UInt64(values), validity)) => {
                        LaneReader::UInt64(values, validity)
                    }
                    _ => LaneReader::Row(batch, lane),
                },
                TwoPassLane::Present { column } => match typed(*column) {
                    Some((_, validity)) => LaneReader::Presence(validity),
                    None => LaneReader::Row(batch, lane),
                },
                TwoPassLane::Temporal { column, .. } => {
                    match derived_temporal_units(batch, *column) {
                        Some((units, validity)) => LaneReader::Int64(units, validity),
                        None => LaneReader::Row(batch, lane),
                    }
                }
                TwoPassLane::Float { .. } => LaneReader::Row(batch, lane),
            }
        })
        .collect()
}

fn scatter_two_pass_row(
    readers: &[LaneReader<'_>],
    row: usize,
    key_bits: u64,
    key_null: bool,
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) {
    let lane_count = readers.len();
    {
        let mut mask = u8::from(key_null) << 7;
        let bucket = &mut buckets[usize::try_from(
            crate::batch::mix64(key_bits ^ u64::from(key_null)) % partitions as u64,
        )
        .expect("partition index fits usize")];
        let lane_base = bucket.lanes.len();
        bucket.lanes.resize(lane_base + lane_count, 0);
        for (lane_index, reader) in readers.iter().enumerate() {
            match reader.bits(row) {
                Some(bits) => bucket.lanes[lane_base + lane_index] = bits,
                None => mask |= 1 << lane_index,
            }
        }
        bucket.keys.push(key_bits);
        bucket.masks.push(mask);
    }
}

/// Global string-key intern table for string-keyed two-pass grouping:
/// dictionary code spaces are per chunk, so keys unify through this table.
#[derive(Default)]
struct StringIntern {
    index: HashMap<Vec<u8>, u64>,
    values: Vec<String>,
    /// Bytes reserved for the entries, handed back when the table is cleared.
    reserved: usize,
    /// The plan's collation. Held here because the table IS the equivalence
    /// relation - two spellings share an id exactly when the collation says
    /// they are equal - so it cannot be decided per call.
    collation: Collation,
}

impl StringIntern {
    fn clear(&mut self, memory: &MemoryTracker) {
        self.index = HashMap::new();
        self.values = Vec::new();
        memory.release(self.reserved);
        self.reserved = 0;
    }

    fn intern(&mut self, bytes: &[u8], memory: &MemoryTracker) -> Result<u64, ExecError> {
        // Group keys unify through the same sort key used by comparison,
        // hashing, DISTINCT, and joins. Keep the first-seen spelling
        // separately for MySQL-compatible GROUP BY output.
        let value = std::str::from_utf8(bytes)
            .map_err(|_| ExecError::InvalidBatch("string group key is not UTF-8"))?;
        let folded = normalized_group_text(value, self.collation).into_bytes();
        if let Some(id) = self.index.get(&folded) {
            return Ok(*id);
        }
        let id = u64::try_from(self.values.len()).expect("intern ids fit u64");
        let entry = bytes
            .len()
            .saturating_add(folded.len())
            .saturating_add(HASH_ENTRY_OVERHEAD)
            .saturating_add(size_of::<String>() + size_of::<u64>());
        memory.reserve(entry)?;
        self.reserved = self.reserved.saturating_add(entry);
        self.index.insert(folded, id);
        self.values.push(value.to_owned());
        Ok(id)
    }
}

/// Pass 2: fold every partition\'s scattered rows into its typed group
/// map, in parallel, then clear the buckets (keeping capacity).
/// Scatters a bounded window of batches in parallel (one bucket set per
/// batch — no cross-worker sharing) and folds every set in one pass-2
/// flush. Only int-keyed sources scatter in parallel: string sources
/// share the intern table and stay on the serial path.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn drain_two_pass_window(
    window: &mut Vec<(RecordBatch, Vec<Vec<u64>>)>,
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    dense: &mut Option<DenseGroupSlots>,
    range: &mut IntRange,
    pool: &mut DensePool,
    intern_len: usize,
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    window_reserved: &mut usize,
    spill: &mut GroupSpill<'_>,
) -> Result<(), ExecError> {
    if window.is_empty() {
        return Ok(());
    }
    if let Some(slots) = dense.as_mut() {
        if dense_in_bounds(keys, intern_len) && dense_integer_window_in_bounds(window, keys) {
            // What the slots' and the partials' states reserve while they
            // fold - their distinct sets - belongs to the groups. Charged
            // and never counted, it stayed charged after a spill had
            // written those sets out, and the next window met a ceiling
            // nothing could lower.
            let before = memory.used();
            let pooled = pool.reserved;
            // A date-part key indexes its slots directly, and can discover
            // mid-fold that a value has no slot; the text keys cannot.
            let folded = if let TwoPassKeySource::DateParts { parts } = keys {
                // On a year outside the table's window: fall through, unify
                // what the slots hold and finish on the scatter path.
                dense_date_parts_window(window, parts, lanes, aggregates, slots, memory)
            } else {
                dense_text_window(window, keys, lanes, aggregates, slots, pool, memory)
            };
            *group_reserved = group_reserved.saturating_add(
                memory
                    .used()
                    .saturating_sub(before)
                    .saturating_sub(pool.reserved.saturating_sub(pooled)),
            );
            if folded? {
                window.clear();
                memory.release(*window_reserved);
                *window_reserved = 0;
                return Ok(());
            }
        }
        // The intern table outgrew the dense domain: unify what the dense
        // slots hold into the partition maps and continue on the classic
        // scatter path for the rest of the stream.
        settle_dense(dense, pool, aggregates, memory, group_reserved)?;
        let slots = dense.take().expect("checked above");
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            maps,
            memory,
            group_reserved,
            spill,
        )?;
    }
    if let TwoPassKeySource::Int { column, group_type } = keys
        && (matches!(
            group_type.storage_type(),
            DataType::Int64 | DataType::UInt64
        ) || matches!(group_type, DataType::Date32 | DataType::DateTime64 { .. }))
        && fold_int_range_window(
            window,
            column,
            lanes,
            aggregates,
            partitions,
            maps,
            range,
            memory,
            group_reserved,
            spill,
        )?
    {
        window.clear();
        memory.release(*window_reserved);
        *window_reserved = 0;
        return Ok(());
    }
    // Row-range morsels rather than whole batches: the window's width then
    // comes from the pool, and a window of one or two batches - the tail of
    // a scan, or a small table - no longer scatters on one or two threads.
    let morsels: Vec<(Morsel<'_>, &Vec<Vec<u64>>)> = morsel_plan(
        window.iter().map(|(batch, _)| batch.row_count()),
        default_morsel_limit(),
    )
    .into_iter()
    .map(|(index, rows)| {
        let (batch, translations) = &window[index];
        (Morsel { batch, rows }, translations)
    })
    .collect();
    let mut sets = morsels
        .par_iter()
        .map(
            |(morsel, translations)| -> Result<Vec<TwoPassBucket>, ExecError> {
                // Sized for an even spread up front: growing every bucket
                // from empty by doubling copied each morsel's scatter output
                // about once more. A skewed key still grows the buckets it
                // lands in, as before.
                let expected = morsel.selected_count().div_ceil(partitions);
                let expected = expected.saturating_add(expected / 8);
                let mut buckets: Vec<TwoPassBucket> = (0..partitions)
                    .map(|_| TwoPassBucket {
                        keys: Vec::with_capacity(expected),
                        masks: Vec::with_capacity(expected),
                        lanes: Vec::with_capacity(expected.saturating_mul(lanes.len())),
                    })
                    .collect();
                match keys {
                    TwoPassKeySource::Int { column, .. } => {
                        two_pass_scatter_batch(morsel, column, lanes, partitions, &mut buckets)?;
                    }
                    TwoPassKeySource::DateParts { parts } => {
                        two_pass_scatter_date_parts(
                            morsel,
                            parts,
                            lanes,
                            partitions,
                            &mut buckets,
                        )?;
                    }
                    TwoPassKeySource::Text { column } => {
                        two_pass_scatter_text_prepared(
                            morsel,
                            &[column],
                            translations,
                            lanes,
                            partitions,
                            &mut buckets,
                        )?;
                    }
                }
                Ok(buckets)
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    drop(morsels);
    window.clear();
    let outcome = two_pass_flush_sets(&mut sets, maps, lanes, aggregates, memory, group_reserved);
    memory.release(*window_reserved);
    *window_reserved = 0;
    outcome
}

fn two_pass_flush(
    buckets: &mut [TwoPassBucket],
    maps: &mut [GroupKeyMap],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let set: Vec<TwoPassBucket> = buckets.iter_mut().map(std::mem::take).collect();
    let mut sets = [set];
    let outcome = two_pass_flush_sets(&mut sets, maps, lanes, aggregates, memory, group_reserved);
    let [set] = sets;
    for (destination, bucket) in buckets.iter_mut().zip(set) {
        *destination = bucket;
    }
    outcome
}

/// Most key slots the integer-range fold takes. Every worker holds a fold
/// this wide, so the bound is what keeps those folds inside a core's share
/// of the last-level cache: past it, the random updates miss to memory and
/// the scatter's partitioned maps are the better shape.
const RANGE_SLOT_CAP: usize = 1 << 18;

/// Rows a range fold computes slots for at a time, so the slot buffer stays
/// in the first-level cache while each lane reads it.
const RANGE_FOLD_ROWS: usize = 4_096;

/// Packed totals for an integer group key, indexed by the key's offset
/// from the smallest key seen. Slot 0 is the NULL group and slot `1 + k -
/// base` holds key `k`.
///
/// A high-cardinality grouped SUM over an integer key - one per customer,
/// per product - spent most of its time scattering every row into partition
/// buckets and probing a map per partition. When the keys sit in a range not
/// much wider than the rows that fill it, which is how surrogate and foreign
/// keys look, each worker folds its rows straight into an array of that
/// range instead: no buffer, no hash, one indexed add per row and lane.
struct IntRangeFold {
    base: i128,
    /// Key slots, the NULL slot excluded.
    span: usize,
    /// Whether the key column is signed, which is how its bits are spelled.
    signed: bool,
    /// Key slots plus the NULL slot.
    slot_count: usize,
    /// One fold per worker, kept across windows and combined per slot only
    /// when the groups are committed: merging them whole after every window
    /// was a serial pass over every slot of every partial.
    folds: Vec<PackedFold>,
    /// Rows folded so far, for the density bound.
    rows: usize,
    reserved: usize,
}

impl IntRangeFold {
    /// The map key the scatter would have built for `slot`.
    fn key_bits(&self, slot: usize) -> (u64, bool) {
        if slot == 0 {
            return (0, true);
        }
        let key = self.base + i128::try_from(slot - 1).expect("slot fits i128");
        let bits = if self.signed {
            u64::from_ne_bytes(
                i64::try_from(key)
                    .expect("signed key in range")
                    .to_ne_bytes(),
            )
        } else {
            u64::try_from(key).expect("unsigned key in range")
        };
        (bits, false)
    }
}

/// Where the integer-range fold stands for a query.
#[derive(Default)]
enum IntRange {
    /// No window has been offered yet.
    #[default]
    Untried,
    Active(Box<IntRangeFold>),
    /// The key or the lanes do not fit; the scatter takes every window.
    Off,
}

/// A packed integer key column of one batch.
fn int_key_column(
    batch: &RecordBatch,
    column: usize,
) -> Option<(PackedInts<'_>, &crate::array::ValidityMask)> {
    match batch.column(column)?.typed()? {
        (crate::batch::TypedValues::Int64(values), validity)
            if values.len() >= batch.row_count() =>
        {
            Some((PackedInts::Signed(values.as_slice()), validity))
        }
        (crate::batch::TypedValues::UInt64(values), validity)
            if values.len() >= batch.row_count() =>
        {
            Some((PackedInts::Unsigned(values.as_slice()), validity))
        }
        // A DATE or DATETIME key: its units are the key, a day number or a
        // microsecond count, and as dense as the days the rows cover.
        _ => derived_temporal_units(batch, column)
            .map(|(units, validity)| (PackedInts::Signed(units), validity)),
    }
}

/// The smallest and largest non-NULL key among `rows`.
fn key_bounds(
    keys: PackedInts<'_>,
    validity: &crate::array::ValidityMask,
    rows: &FoldRows<'_>,
) -> Option<(i128, i128)> {
    fn widen<T: Copy + Ord + Into<i128>>(
        values: &[T],
        validity: &crate::array::ValidityMask,
        rows: &FoldRows<'_>,
    ) -> Option<(i128, i128)> {
        match rows {
            FoldRows::Span(span) if validity.no_nulls() => {
                let values = &values[span.clone()];
                let low = values.iter().copied().min()?;
                let high = values.iter().copied().max()?;
                Some((low.into(), high.into()))
            }
            FoldRows::Span(span) => span
                .clone()
                .filter(|row| validity.is_valid(*row))
                .map(|row| values[row].into())
                .fold(None, |bounds, key: i128| {
                    Some(bounds.map_or((key, key), |(low, high): (i128, i128)| {
                        (low.min(key), high.max(key))
                    }))
                }),
            FoldRows::Picked(picked) => picked
                .iter()
                .map(|row| *row as usize)
                .filter(|row| validity.is_valid(*row))
                .map(|row| values[row].into())
                .fold(None, |bounds, key: i128| {
                    Some(bounds.map_or((key, key), |(low, high): (i128, i128)| {
                        (low.min(key), high.max(key))
                    }))
                }),
        }
    }
    match keys {
        PackedInts::Signed(values) => widen(values, validity, rows),
        PackedInts::Unsigned(values) => widen(values, validity, rows),
    }
}

/// Each listed row's slot: `1 + key - base`, or 0 for a NULL key. Every
/// key was checked against the range before the fold began.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn range_slots(
    keys: PackedInts<'_>,
    validity: &crate::array::ValidityMask,
    rows: &FoldRows<'_>,
    base: i128,
    slots: &mut Vec<u32>,
) {
    slots.clear();
    // Offsets are below RANGE_SLOT_CAP, so the wrapping difference in the
    // key's own width is the offset, and it fits u32.
    macro_rules! fill {
        ($values:expr, $base:expr) => {{
            let values = $values;
            let base = $base;
            match rows {
                FoldRows::Span(span) if validity.no_nulls() => slots.extend(
                    values[span.clone()]
                        .iter()
                        .map(|key| (key.wrapping_sub(base) as u32) + 1),
                ),
                FoldRows::Span(span) => slots.extend(span.clone().map(|row| {
                    if validity.is_valid(row) {
                        (values[row].wrapping_sub(base) as u32) + 1
                    } else {
                        0
                    }
                })),
                FoldRows::Picked(picked) => slots.extend(picked.iter().map(|row| {
                    let row = *row as usize;
                    if validity.is_valid(row) {
                        (values[row].wrapping_sub(base) as u32) + 1
                    } else {
                        0
                    }
                })),
            }
        }};
    }
    match keys {
        PackedInts::Signed(values) => fill!(values, base as i64),
        PackedInts::Unsigned(values) => fill!(values, base as u64),
    }
}

/// Folds one morsel into a worker's range fold.
fn fold_range_morsel(
    morsel: &Morsel<'_>,
    column: usize,
    lanes: &[TwoPassLane],
    base: i128,
    fold: &mut PackedFold,
) -> Result<(), ExecError> {
    let batch = morsel.batch;
    let (keys, validity) = int_key_column(batch, column).ok_or(ExecError::InvalidBatch(
        "range fold key lost its packed projection",
    ))?;
    let inputs = fold.resolve(batch, lanes);
    let readers = inputs.is_none().then(|| lane_readers(batch, lanes));
    let mut selected = Vec::new();
    let mut slots = Vec::with_capacity(RANGE_FOLD_ROWS);
    let mut start = morsel.rows.start;
    while start < morsel.rows.end {
        let end = start.saturating_add(RANGE_FOLD_ROWS).min(morsel.rows.end);
        let rows = fold_rows(batch, start..end, &mut selected);
        range_slots(keys, validity, &rows, base, &mut slots);
        match (&inputs, &readers) {
            (Some(inputs), _) => fold.fold(inputs, &slots, &rows),
            (None, Some(readers)) => {
                for (row, slot) in batch.selection().selected_rows_in(start..end).zip(&slots) {
                    fold.add_row(*slot as usize, readers, row);
                }
            }
            (None, None) => unreachable!("readers stand in for missing inputs"),
        }
        start = end;
    }
    Ok(())
}

/// Commits the range fold's groups into the partition maps, where the
/// scatter's groups and a spill expect them, and hands its slab back.
///
/// A group costs several times more in a map than in the fold's slots, and
/// the slab stays charged until the last slot is read. The fold is given up
/// exactly when the budget is short, so the maps may not have room for all
/// of its groups: the ones already committed then go to disk as a run and
/// the rest follow into the emptied maps. A slot is committed once, whole,
/// and the merge of runs combines a key that several runs hold.
fn fold_int_range_into_maps(
    range: &IntRangeFold,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    spill: &mut GroupSpill<'_>,
) -> Result<(), ExecError> {
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    let outcome = (|| {
        for slot in 0..range.slot_count {
            if !occupied_in(&range.folds, slot) {
                continue;
            }
            let (bits, null) = range.key_bits(slot);
            let partition =
                usize::try_from(crate::batch::mix64(bits ^ u64::from(null)) % partitions as u64)
                    .expect("partition index fits usize");
            if !maps[partition].contains_key(&(bits, null)) {
                if memory.reserve(per_group_bytes).is_err() {
                    spill.write(maps, group_reserved, memory)?;
                    memory.reserve(per_group_bytes)?;
                }
                *group_reserved = group_reserved.saturating_add(per_group_bytes);
            }
            let states = maps[partition]
                .entry((bits, null))
                .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
            commit_merged(&range.folds, slot, states, aggregates, memory)?;
        }
        Ok(())
    })();
    memory.release(range.reserved);
    outcome
}

/// One finished column of a grouped aggregate's result.
pub(super) enum ReadyColumn {
    /// Finished values, built into a column of the plan's type.
    Values(Vec<Value>),
    /// An exact decimal SUM's totals as scaled units, `None` where a group
    /// summed no value. Served as a packed decimal column whose text is
    /// rendered from the units - the text the finished value spells - so
    /// what reads it next (a rounding, a sort) works on the units instead
    /// of formatting every total only to parse it back.
    Decimal { units: Vec<Option<i128>>, scale: u8 },
}

impl ReadyColumn {
    fn value(&self, row: usize) -> Value {
        match self {
            Self::Values(values) => values[row].clone(),
            Self::Decimal { units, scale } => units[row].map_or(Value::Null, |units| {
                Value::Utf8(pintail_types::format_decimal_scaled(units, *scale))
            }),
        }
    }

    fn row_bytes(&self) -> usize {
        match self {
            Self::Values(values) => values.first().map_or(0, |value| {
                size_of::<Value>().saturating_add(value.heap_bytes())
            }),
            Self::Decimal { .. } => size_of::<Option<i128>>() + 1,
        }
    }
}

/// A grouped aggregate's finished groups held as columns: the batches the
/// operator serves are cut straight from them, with no row of cells built.
pub(super) struct ReadyColumns {
    len: usize,
    columns: Vec<ReadyColumn>,
    /// A settled-memo answer served from the batches its entry keeps, in
    /// place of `columns`.
    settled: Option<std::sync::Arc<SettledRows>>,
}

impl ReadyColumns {
    /// Finished groups a fold built a column at a time.
    pub(super) const fn from_columns(len: usize, columns: Vec<ReadyColumn>) -> Self {
        Self {
            len,
            columns,
            settled: None,
        }
    }

    /// A settled-memo entry's rows, served as the batches it keeps.
    pub(super) fn settled(entry: std::sync::Arc<SettledRows>) -> Self {
        Self {
            len: entry.rows.len(),
            columns: Vec::new(),
            settled: Some(entry),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Bytes a served row costs, for sizing and charging its batch.
    pub(super) fn row_bytes(&self) -> usize {
        if let Some(entry) = &self.settled {
            return entry.row_bytes;
        }
        self.columns
            .iter()
            .map(ReadyColumn::row_bytes)
            .sum::<usize>()
            .max(1)
    }

    /// Rows `rows` as a batch of the plan's column types.
    pub(super) fn batch(
        &self,
        rows: std::ops::Range<usize>,
        column_types: &[DataType],
    ) -> Result<RecordBatch, ExecError> {
        if let Some(entry) = &self.settled {
            return entry.batch(rows, column_types);
        }
        // As cells the rows would serve the layout's columns from their
        // front, so too short is the only mismatch.
        if self.columns.len() < column_types.len() {
            return Err(ExecError::InvalidBatch(
                "finished groups do not match the aggregate's layout",
            ));
        }
        let columns = self
            .columns
            .iter()
            .zip(column_types)
            .map(|(column, data_type)| match column {
                ReadyColumn::Decimal { units, scale }
                    if matches!(
                        data_type,
                        DataType::Decimal { scale: declared, .. } if declared == scale
                    ) =>
                {
                    let units = &units[rows.clone()];
                    let validity = crate::array::ValidityMask::from_bools(
                        &units.iter().map(Option::is_some).collect::<Vec<_>>(),
                    );
                    Ok(crate::ColumnVector::from_typed(
                        *data_type,
                        crate::batch::TypedValues::Decimal128 {
                            // 64 bits a total where every one fits them,
                            // the full width where one does not.
                            values: units
                                .iter()
                                .map(|units| i64::try_from(units.unwrap_or(0)).ok())
                                .collect::<Option<Vec<_>>>()
                                .map_or_else(
                                    || {
                                        crate::batch::DecimalUnits::Wide(
                                            units.iter().map(|units| units.unwrap_or(0)).collect(),
                                        )
                                    },
                                    crate::batch::DecimalUnits::Narrow,
                                ),
                            scale: *scale,
                            text: crate::batch::LazyText::decimal(*scale),
                        },
                        validity,
                    ))
                }
                _ => crate::ColumnVector::new(
                    *data_type,
                    rows.clone().map(|row| column.value(row)).collect(),
                )
                .map_err(ExecError::from),
            })
            .collect::<Result<Vec<_>, _>>()?;
        RecordBatch::new(rows.len(), columns).map_err(ExecError::from)
    }

    /// Every row as cells, for a consumer that keeps rows.
    pub(super) fn into_rows(self) -> Vec<Vec<Value>> {
        if let Some(entry) = self.settled {
            return std::sync::Arc::try_unwrap(entry)
                .map_or_else(|shared| shared.rows.clone(), |entry| entry.rows);
        }
        (0..self.len)
            .map(|row| {
                self.columns
                    .iter()
                    .map(|column| column.value(row))
                    .collect()
            })
            .collect()
    }
}

/// One settled-memo answer: the finished rows, and the batches a replay
/// serves cut from them once.
///
/// A replay used to clone every row out of the memo and rebuild its columns
/// from the cells, parsing each decimal back from its text, on every hit.
/// The batches are built on the first replay and shared after it: a column
/// clone shares its packed values, so a hit costs a handful of reference
/// counts whatever the number of groups.
pub(super) struct SettledRows {
    rows: Vec<Vec<Value>>,
    /// What a served row is charged, as the cells would be.
    row_bytes: usize,
    /// Every row's payload, reserved again by each replay.
    payload: usize,
    /// The column types and full-size batches of the first replay.
    served: std::sync::OnceLock<(Vec<DataType>, Vec<RecordBatch>)>,
}

impl SettledRows {
    pub(super) fn new(rows: Vec<Vec<Value>>) -> Self {
        let payload = rows
            .iter()
            .map(|row| super::estimated_row_payload_bytes(row))
            .sum();
        let row_bytes = rows.first().map_or(1, |row| {
            super::estimated_record_batch_bytes(std::slice::from_ref(row), row.len()).max(1)
        });
        Self {
            rows,
            row_bytes,
            payload,
            served: std::sync::OnceLock::new(),
        }
    }

    pub(super) fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    pub(super) const fn payload(&self) -> usize {
        self.payload
    }

    /// Rows `rows` as a batch: the kept batch when `rows` is one of the
    /// full-size cuts and the types are the ones it was built with,
    /// otherwise built from the cells as before.
    fn batch(
        &self,
        rows: std::ops::Range<usize>,
        column_types: &[DataType],
    ) -> Result<RecordBatch, ExecError> {
        let step = crate::batch::DEFAULT_BATCH_ROWS;
        let full =
            rows.start.is_multiple_of(step) && rows.end == (rows.start + step).min(self.rows.len());
        if full {
            let built = self.served.get_or_init(|| {
                let batches = self
                    .rows
                    .chunks(step)
                    .map(|chunk| {
                        super::rows_to_columns(chunk, column_types).and_then(|columns| {
                            RecordBatch::new(chunk.len(), columns).map_err(ExecError::from)
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap_or_default();
                (column_types.to_vec(), batches)
            });
            if built.0 == column_types
                && let Some(batch) = built.1.get(rows.start / step)
            {
                return Ok(batch.clone());
            }
        }
        let cells = &self.rows[rows.clone()];
        let columns = super::rows_to_columns(cells, column_types)?;
        RecordBatch::new(cells.len(), columns).map_err(ExecError::from)
    }
}

/// Finishes every group of a range fold, in key order, when no other path
/// holds groups of the same query. An exact decimal SUM keeps its totals as
/// units; every other lane finishes through its state, as the maps would.
fn finish_int_range(
    range: &IntRangeFold,
    group_type: DataType,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<ReadyColumns, ExecError> {
    let slot_count = range.slot_count;
    let chunk = slot_count
        .div_ceil(rayon::current_num_threads().saturating_mul(4))
        .max(1_024);
    // The lanes kept as units, at the scale they total at: an exact decimal
    // SUM answered as a decimal.
    let unit_lanes = (0..aggregates.len())
        .map(|index| match merged_cell(&range.folds, index, 0) {
            Some((
                PackedLane::Sum {
                    scale,
                    float_output: false,
                },
                _,
            )) => Some(scale),
            _ => None,
        })
        .collect::<Vec<_>>();
    let pieces = (0..slot_count.div_ceil(chunk))
        .into_par_iter()
        .map(|piece| -> Result<(usize, Vec<ReadyColumn>), ExecError> {
            let mut columns = std::iter::once(ReadyColumn::Values(Vec::new()))
                .chain(unit_lanes.iter().map(|scale| match scale {
                    Some(scale) => ReadyColumn::Decimal {
                        units: Vec::new(),
                        scale: *scale,
                    },
                    None => ReadyColumn::Values(Vec::new()),
                }))
                .collect::<Vec<_>>();
            let mut groups = 0_usize;
            let mut uncharged = 0_usize;
            for slot in piece * chunk..((piece + 1) * chunk).min(slot_count) {
                if !occupied_in(&range.folds, slot) {
                    continue;
                }
                groups += 1;
                let (bits, null) = range.key_bits(slot);
                let key = two_pass_key_value(bits, null, group_type);
                uncharged = uncharged.saturating_add(size_of::<Value>() + key.heap_bytes());
                if let ReadyColumn::Values(values) = &mut columns[0] {
                    values.push(key);
                }
                for (index, (column, aggregate)) in
                    columns[1..].iter_mut().zip(aggregates).enumerate()
                {
                    match column {
                        ReadyColumn::Decimal { units, .. } => {
                            let cell = merged_cell(&range.folds, index, slot)
                                .map(|(_, cell)| cell)
                                .unwrap_or_default();
                            units.push((cell.rows > 0).then_some(cell.total));
                            uncharged = uncharged.saturating_add(size_of::<Option<i128>>());
                        }
                        ReadyColumn::Values(values) => {
                            let mut state = AggregateState::new(aggregate);
                            if let Some((lane, cell)) = merged_cell(&range.folds, index, slot) {
                                cell.commit(lane, &mut state, aggregate, memory)?;
                            }
                            let value = state.finish(memory)?;
                            uncharged =
                                uncharged.saturating_add(size_of::<Value>() + value.heap_bytes());
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
            Ok((groups, columns))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut len = 0;
    let mut columns: Option<Vec<ReadyColumn>> = None;
    for (groups, piece) in pieces {
        len += groups;
        match &mut columns {
            None => columns = Some(piece),
            Some(columns) => {
                for (into, from) in columns.iter_mut().zip(piece) {
                    match (into, from) {
                        (ReadyColumn::Values(into), ReadyColumn::Values(from)) => {
                            into.extend(from);
                        }
                        (
                            ReadyColumn::Decimal { units: into, .. },
                            ReadyColumn::Decimal { units: from, .. },
                        ) => into.extend(from),
                        _ => unreachable!("every piece lays its columns out alike"),
                    }
                }
            }
        }
    }
    Ok(ReadyColumns {
        len,
        columns: columns.unwrap_or_default(),
        settled: None,
    })
}

/// Folds one window through the integer-range fold. `false`, with the fold
/// committed to the maps and switched off, when the key or the lanes do not
/// fit it; the caller then scatters the window.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn fold_int_range_window(
    window: &[(RecordBatch, Vec<Vec<u64>>)],
    column: usize,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    range: &mut IntRange,
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    spill: &mut GroupSpill<'_>,
) -> Result<bool, ExecError> {
    let mut give_up = |range: &mut IntRange,
                       maps: &mut [GroupKeyMap],
                       group_reserved: &mut usize|
     -> Result<bool, ExecError> {
        if let IntRange::Active(active) = std::mem::replace(range, IntRange::Off) {
            fold_int_range_into_maps(
                &active,
                aggregates,
                partitions,
                maps,
                memory,
                group_reserved,
                spill,
            )?;
        }
        Ok(false)
    };
    if matches!(range, IntRange::Off) {
        return Ok(false);
    }
    let packed = lanes
        .iter()
        .zip(aggregates)
        .map(|(lane, aggregate)| packed_lane(lane, aggregate))
        .collect::<Vec<_>>();
    if packed.iter().any(Option::is_none) {
        return give_up(range, maps, group_reserved);
    }
    let morsels: Vec<Morsel<'_>> = morsel_plan(
        window.iter().map(|(batch, _)| batch.row_count()),
        default_morsel_limit(),
    )
    .into_iter()
    .map(|(index, rows)| Morsel {
        batch: &window[index].0,
        rows,
    })
    .collect();
    // The window's key bounds and signedness, read in parallel.
    let bounds = morsels
        .par_iter()
        .map(|morsel| {
            let (keys, validity) = int_key_column(morsel.batch, column)?;
            let mut selected = Vec::new();
            let rows = fold_rows(morsel.batch, morsel.rows.clone(), &mut selected);
            let signed = matches!(keys, PackedInts::Signed(_));
            Some((signed, key_bounds(keys, validity, &rows)))
        })
        .collect::<Option<Vec<_>>>();
    let Some(bounds) = bounds else {
        return give_up(range, maps, group_reserved);
    };
    let signed = match range {
        IntRange::Active(active) => active.signed,
        _ => bounds.first().is_none_or(|(signed, _)| *signed),
    };
    if bounds.iter().any(|(each, _)| *each != signed) {
        return give_up(range, maps, group_reserved);
    }
    let mut low_high = bounds
        .iter()
        .filter_map(|(_, bounds)| *bounds)
        .reduce(|(low, high), (other_low, other_high)| (low.min(other_low), high.max(other_high)));
    if let IntRange::Active(active) = range
        && active.span > 0
    {
        let (low, high) = (
            active.base,
            active.base + i128::try_from(active.span - 1).expect("span fits i128"),
        );
        low_high = Some(low_high.map_or((low, high), |(other_low, other_high)| {
            (low.min(other_low), high.max(other_high))
        }));
    }
    let window_rows: usize = window
        .iter()
        .map(|(batch, _)| batch.visible_row_count())
        .sum();
    let rows_seen = match range {
        IntRange::Active(active) => active.rows,
        _ => 0,
    }
    .saturating_add(window_rows);
    let (base, span) = match low_high {
        Some((low, high)) => match usize::try_from(high - low + 1) {
            Ok(span) => (low, span),
            Err(_) => return give_up(range, maps, group_reserved),
        },
        None => (0, 0),
    };
    // Dense enough to beat a hash table: no wider than the rows that fill
    // it (a small floor lets a short input in), and within the cache bound.
    if span > RANGE_SLOT_CAP || span > rows_seen.max(4_096) {
        return give_up(range, maps, group_reserved);
    }
    let slot_count = span + 1;
    let workers = rayon::current_num_threads().max(1);
    let fold_bytes = PackedFold::bytes(slot_count, &packed);
    let needed = fold_bytes.saturating_mul(workers + 1);
    let current = match range {
        IntRange::Active(active) => active.reserved,
        _ => 0,
    };
    let rebase = match range {
        IntRange::Active(active) => active.base != base || active.span != span,
        _ => true,
    };
    if rebase {
        if memory.reserve(needed).is_err() {
            return give_up(range, maps, group_reserved);
        }
        let mut folds = Vec::new();
        let mut rows = 0;
        if let IntRange::Active(old) = std::mem::replace(range, IntRange::Off) {
            // An old range of no keys holds only the NULL slot.
            let shift = if old.span == 0 {
                0
            } else {
                usize::try_from(old.base - base).expect("old range inside the new one")
            };
            let mut fold = PackedFold::sharing(slot_count, &packed, lanes);
            for partial in &old.folds {
                fold.merge_from(partial, |slot| if slot == 0 { 0 } else { slot + shift });
            }
            folds.push(fold);
            rows = old.rows;
            memory.release(old.reserved);
        }
        *range = IntRange::Active(Box::new(IntRangeFold {
            base,
            span,
            signed,
            slot_count,
            folds,
            rows,
            reserved: needed,
        }));
    } else {
        debug_assert_eq!(current, needed);
    }
    let IntRange::Active(active) = range else {
        unreachable!("the range was just made active");
    };
    let pool = std::sync::Mutex::new(std::mem::take(&mut active.folds));
    let fresh = || PackedFold::sharing(slot_count, &packed, lanes);
    morsels.par_iter().try_for_each(|morsel| {
        let taken = pool
            .lock()
            .map_err(|_| ExecError::InvalidBatch("range fold pool poisoned"))?
            .pop();
        let mut fold = taken.unwrap_or_else(fresh);
        let outcome = fold_range_morsel(morsel, column, lanes, base, &mut fold);
        pool.lock()
            .map_err(|_| ExecError::InvalidBatch("range fold pool poisoned"))?
            .push(fold);
        outcome
    })?;
    active.folds = pool
        .into_inner()
        .map_err(|_| ExecError::InvalidBatch("range fold pool poisoned"))?;
    active.rows = rows_seen;
    Ok(true)
}

/// A lane whose rows reduce to one integer total and a row count, so a
/// group's rows can be summed in a plain cell and applied to its
/// [`AggregateState`] once. Applying every row to the state costs a call,
/// an enum match and a carrier rebuild per row, on state that is often
/// out of cache: that was most of a high-cardinality grouped SUM's time.
#[derive(Clone, Copy)]
pub(super) enum PackedLane {
    Count,
    /// COUNT of a column: the slot's rows less the column's NULLs.
    Present,
    Sum {
        scale: u8,
        float_output: bool,
    },
    /// SUM of a signed integer column answered as a signed integer.
    IntegerSum,
    Average {
        /// Places between the input scale and the result scale.
        digits: u8,
        result_scale: u8,
    },
    Minimum {
        text: UnitText,
    },
    Maximum {
        text: UnitText,
    },
}

/// What a packed extreme's units are, which is how the retained value is
/// built from them once a group's extreme is known.
#[derive(Clone, Copy)]
pub(super) enum UnitText {
    Decimal {
        scale: u8,
    },
    /// A DATE's day number or a DATETIME's microseconds.
    Temporal(DataType),
    /// A signed integer, retained as the integer it is.
    Integer,
}

/// Largest rescale a packed average takes: an `i64` unit times `10^19`
/// still fits `i128`, so no row the per-row path would widen successfully
/// can fail here, and no row it would refuse can be taken.
const PACKED_AVERAGE_MAX_DIGITS: u8 = 19;

pub(super) fn packed_lane(lane: &TwoPassLane, aggregate: &CompiledAggregate) -> Option<PackedLane> {
    match *lane {
        TwoPassLane::CountStar => Some(PackedLane::Count),
        TwoPassLane::DecimalUnits {
            scale,
            float_output,
            ..
        } => match decimal_average_scale(aggregate) {
            None => Some(PackedLane::Sum {
                scale,
                float_output,
            }),
            Some(result_scale) => result_scale
                .checked_sub(scale)
                .filter(|digits| *digits <= PACKED_AVERAGE_MAX_DIGITS)
                .map(|digits| PackedLane::Average {
                    digits,
                    result_scale,
                }),
        },
        TwoPassLane::ExtremeDecimal { scale, .. } => {
            packed_extreme(aggregate, UnitText::Decimal { scale })
        }
        TwoPassLane::Present { .. } => Some(PackedLane::Present),
        TwoPassLane::Temporal { data_type, .. } => {
            packed_extreme(aggregate, UnitText::Temporal(data_type))
        }
        // A signed integer column's SUM and AVG are exact decimal totals of
        // its values, and its MIN and MAX compare the values themselves:
        // all three reduce to one total per group, as a decimal column's
        // do. The unsigned reading and the bit folds keep the per-row lane.
        TwoPassLane::Int {
            data_type: DataType::Int64,
            ..
        } if !aggregate.distinct => match (aggregate.function, aggregate.sum_carrier) {
            (AggregateFunction::Count, _) => Some(PackedLane::Present),
            // An integer SUM answered as a DECIMAL is that decimal's
            // scale-0 units: it rides the decimal lane, and its finished
            // totals stay units for what reads them next.
            (AggregateFunction::Sum, Some(DataType::Int64)) if decimal_integer_total(aggregate) => {
                Some(PackedLane::Sum {
                    scale: 0,
                    float_output: false,
                })
            }
            // A sum the plan types as an integer joins the integer state
            // the per-row update keeps, refused past its type when the
            // group is finished.
            (AggregateFunction::Sum, Some(DataType::Int64)) => Some(PackedLane::IntegerSum),
            (AggregateFunction::Average, _) => decimal_average_scale(aggregate)
                .filter(|digits| *digits <= PACKED_AVERAGE_MAX_DIGITS)
                .map(|result_scale| PackedLane::Average {
                    digits: result_scale,
                    result_scale,
                }),
            _ => None,
        },
        TwoPassLane::Exact {
            data_type: DataType::Int64,
            ..
        } => packed_extreme(aggregate, UnitText::Integer),
        TwoPassLane::Float { .. }
        | TwoPassLane::Int { .. }
        | TwoPassLane::Exact { .. }
        | TwoPassLane::Distinct { .. } => None,
    }
}

fn packed_extreme(aggregate: &CompiledAggregate, text: UnitText) -> Option<PackedLane> {
    match aggregate.function {
        AggregateFunction::Minimum => Some(PackedLane::Minimum { text }),
        AggregateFunction::Maximum => Some(PackedLane::Maximum { text }),
        _ => None,
    }
}

/// One group's running total for one packed lane.
#[derive(Clone, Copy, Default)]
pub(super) struct PackedCell {
    pub(super) total: i128,
    pub(super) rows: u64,
}

impl PackedCell {
    /// Folds one non-NULL row's lane bits in. Totals of `i64` units cannot
    /// leave `i128` before `2^63` rows, which no window holds.
    #[inline]
    fn add(&mut self, lane: PackedLane, bits: u64) -> Result<(), ExecError> {
        let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
        match lane {
            PackedLane::Count | PackedLane::Present => {}
            PackedLane::Sum { .. } | PackedLane::IntegerSum | PackedLane::Average { .. } => {
                self.total = self
                    .total
                    .checked_add(units)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            PackedLane::Minimum { .. } => {
                if self.rows == 0 || units < self.total {
                    self.total = units;
                }
            }
            PackedLane::Maximum { .. } => {
                if self.rows == 0 || units > self.total {
                    self.total = units;
                }
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Applies the cell to the group's state: the same state the per-row
    /// path leaves, since every packed lane is an exact, order-free fold.
    pub(super) fn commit(
        &self,
        lane: PackedLane,
        state: &mut AggregateState,
        aggregate: &CompiledAggregate,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if self.rows == 0 {
            return Ok(());
        }
        match lane {
            PackedLane::Count | PackedLane::Present => state.add_dense_count(self.rows),
            PackedLane::Sum {
                scale,
                float_output,
            } => state.update_decimal_sum_units(self.total, scale, float_output),
            // The cell's total may be outside 64 bits while the group's is
            // not: the state keeps it exact and judges the group's.
            PackedLane::IntegerSum => state.add_integer_exact(self.total, false),
            PackedLane::Average {
                digits,
                result_scale,
            } => state.add_decimal_average_partial(self.total, digits, result_scale, self.rows),
            PackedLane::Minimum { text } | PackedLane::Maximum { text } => {
                let units = self.total;
                match text {
                    UnitText::Decimal { scale } => state.update_extreme_units(
                        aggregate,
                        units,
                        || Some(pintail_types::format_decimal_scaled(units, scale)),
                        memory,
                    ),
                    UnitText::Temporal(data_type) => state.update_extreme_units(
                        aggregate,
                        units,
                        || temporal_unit_text(units, data_type),
                        memory,
                    ),
                    UnitText::Integer => {
                        // What the per-row lane hands the state: the value
                        // and its double as the comparison hint.
                        let value = i64::try_from(units).map_err(|_| ExecError::NumericOverflow)?;
                        #[allow(clippy::cast_precision_loss)]
                        let number = value as f64;
                        state.update_with_number(
                            aggregate,
                            &Value::Int64(value),
                            Some(number),
                            memory,
                        )
                    }
                }
            }
        }
    }
}

/// Applies one lane's scattered bits to one aggregate state. Shared by
/// pass-2 flush (bits re-read from buckets) and the dense direct path
/// (bits applied straight from the batch).
fn apply_two_pass_lane(
    state: &mut AggregateState,
    lane: &TwoPassLane,
    aggregate: &CompiledAggregate,
    bits: u64,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    match lane {
        TwoPassLane::CountStar | TwoPassLane::Present { .. } => {
            state.update(aggregate, &Value::UInt64(1), memory)
        }
        TwoPassLane::Temporal { data_type, .. } => {
            let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
            state.update_extreme_units(
                aggregate,
                units,
                || temporal_unit_text(units, *data_type),
                memory,
            )
        }
        TwoPassLane::DecimalUnits {
            scale,
            float_output,
            ..
        } => {
            let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
            if let Some(result_scale) = decimal_average_scale(aggregate) {
                let rescaled = (*scale <= result_scale)
                    .then(|| decimal_units_from_int(units, result_scale - *scale))
                    .flatten()
                    .ok_or(ExecError::NumericOverflow)?;
                return state.update_decimal_average_units(rescaled, result_scale);
            }
            state.update_decimal_sum_units(units, *scale, *float_output)
        }
        TwoPassLane::ExtremeDecimal { scale, .. } => {
            let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
            state.update_extreme_units(
                aggregate,
                units,
                || Some(pintail_types::format_decimal_scaled(units, *scale)),
                memory,
            )
        }
        TwoPassLane::Distinct { data_type, .. } => {
            let key = if *data_type == DataType::Int64 {
                i128::from(i64::from_ne_bytes(bits.to_ne_bytes()))
            } else {
                i128::from(bits)
            };
            state.update_distinct_count_int(key, memory)
        }
        TwoPassLane::Float { .. } => {
            let number = f64::from_bits(bits);
            state.update_with_number(aggregate, &Value::float64(number), Some(number), memory)
        }
        TwoPassLane::Int { data_type, .. } => {
            let value = two_pass_key_value(bits, false, *data_type);
            // number=None keeps integer sums on the exact integer branch,
            // as sequential does.
            state.update_with_number(aggregate, &value, None, memory)
        }
        TwoPassLane::Exact { data_type, .. } => {
            let value = two_pass_key_value(bits, false, *data_type);
            let number = match &value {
                Value::Int64(v) =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(*v as f64)
                }
                Value::UInt64(v) =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(*v as f64)
                }
                Value::Float64(v) => Some(v.get()),
                _ => None,
            };
            state.update_with_number(aggregate, &value, number, memory)
        }
    }
}

/// Dense slot table for small text-keyed group domains: intern ids are
/// dense small integers, so the whole scatter/flush round trip (buffer 17
/// bytes per row, re-read, hash-probe) collapses into direct indexing.
/// Slot 0 is the NULL group for single-column keys; pairs pack their
/// NULL-encoded side ids directly.
/// Year the date-part dense domain starts at. Years are stored as an offset
/// so a year fits beside a second part inside the slot cap; the other parts
/// are already bounded (month 12, day 31, hour 23, minute and second 59).
/// Ordinal 0 is NULL for every part, matching the `(value + 1)` packing the
/// scatter uses.
const DENSE_DATE_YEAR_BASE: u64 = 1900;
/// Year ordinals run 1..=256, so 1900 through 2155.
const DENSE_DATE_YEAR_SIDE: usize = 257;
/// The other parts by their own bound, `(value + 1)` ids plus the NULL
/// ordinal: month up to 12, day up to 31, hour up to 23, minute and second
/// up to 59. One side for all of them made a YEAR/MONTH table four and a
/// half times the slots it can use, and every partial of a window is
/// allocated, committed and merged across all of them.
const DENSE_DATE_MONTH_SIDE: usize = 14;
const DENSE_DATE_DAY_SIDE: usize = 33;
const DENSE_DATE_HOUR_SIDE: usize = 25;
const DENSE_DATE_SIXTY_SIDE: usize = 61;
/// Largest date-part table built. Merging walks every slot whether or not
/// it is occupied, so the table stays small enough that walking it costs
/// less than the buckets it replaces.
const DENSE_DATE_SLOT_CAP: usize = 1 << 16;

/// Ordinals one part can take, or `None` for a part this table cannot hold.
const fn dense_date_side(part: DatePart) -> Option<usize> {
    match part {
        DatePart::Year => Some(DENSE_DATE_YEAR_SIDE),
        DatePart::Month => Some(DENSE_DATE_MONTH_SIDE),
        DatePart::Day => Some(DENSE_DATE_DAY_SIDE),
        DatePart::Hour => Some(DENSE_DATE_HOUR_SIDE),
        DatePart::Minute | DatePart::Second => Some(DENSE_DATE_SIXTY_SIDE),
        _ => None,
    }
}

/// One part's dense ordinal from its packed `(value + 1)` id, or `None` when
/// the value falls outside the domain the table covers - a year before 1900
/// or after 2155. The caller then abandons the dense table for the classic
/// scatter rather than folding distinct groups together.
fn dense_date_ordinal(part: DatePart, id: u64) -> Option<usize> {
    if id == 0 {
        return Some(0);
    }
    let side = dense_date_side(part)?;
    let ordinal = match part {
        DatePart::Year => usize::try_from(id.checked_sub(DENSE_DATE_YEAR_BASE)?).ok()?,
        _ => usize::try_from(id).ok()?,
    };
    (ordinal >= 1 && ordinal < side).then_some(ordinal)
}

/// The packed `(value + 1)` id a dense ordinal came from.
fn dense_date_id(part: DatePart, ordinal: usize) -> u64 {
    if ordinal == 0 {
        return 0;
    }
    match part {
        DatePart::Year => DENSE_DATE_YEAR_BASE.saturating_add(ordinal as u64),
        _ => ordinal as u64,
    }
}

/// Slots a date-part key needs, or `None` when any part is unsupported or
/// the product exceeds the cap.
fn dense_date_slot_count(parts: [Option<(DatePart, usize)>; 2]) -> Option<usize> {
    let mut slots = 1_usize;
    let mut present = 0_usize;
    for (part, _) in parts.iter().flatten() {
        slots = slots.checked_mul(dense_date_side(*part)?)?;
        present += 1;
    }
    (present > 0 && slots <= DENSE_DATE_SLOT_CAP).then_some(slots)
}

/// Mixed-radix slot for a packed date-part key, or `None` when a part falls
/// outside the dense domain.
fn dense_date_slot(parts: [Option<(DatePart, usize)>; 2], key_bits: u64) -> Option<usize> {
    let present = parts.iter().flatten().count();
    let mut slot = 0_usize;
    for (index, (part, _)) in parts.iter().flatten().enumerate() {
        let shift = 20 * (present - 1 - index);
        let id = (key_bits >> shift) & 0xF_FFFF;
        slot = slot
            .checked_mul(dense_date_side(*part)?)?
            .checked_add(dense_date_ordinal(*part, id)?)?;
    }
    Some(slot)
}

/// The inverse of [`dense_date_slot`], rebuilding the packed key a slot
/// stands for so folded groups keep the identity the scatter path gives them.
fn dense_date_key(parts: [Option<(DatePart, usize)>; 2], slot: usize) -> u64 {
    let present: Vec<DatePart> = parts.iter().flatten().map(|(part, _)| *part).collect();
    let mut ids = vec![0_u64; present.len()];
    let mut rest = slot;
    for (index, part) in present.iter().enumerate().rev() {
        let side = dense_date_side(*part).expect("dense table exists for these parts");
        ids[index] = dense_date_id(*part, rest % side);
        rest /= side;
    }
    ids.into_iter().fold(0_u64, |bits, id| (bits << 20) | id)
}

/// Why a dense date-part fold stopped: a real failure, or a value outside
/// the table's domain, which is recoverable by falling back to the scatter.
enum DenseFold {
    Exec(ExecError),
    OutOfDomain,
}

type DenseGroupSlots = Vec<Option<Vec<AggregateState>>>;

/// Worker partials of the dense text slots, kept from one window to the
/// next and merged into the slots only when something reads them.
///
/// A partial made fresh for every batch started every group's states over:
/// each COUNT(DISTINCT) set grew from empty through its integer set and its
/// bitmap's widenings once per batch, and every partial merged into the
/// slots once per window.
#[derive(Default)]
struct DensePool {
    partials: Vec<DenseGroupSlots>,
    /// Bytes charged for the partials, handed back when they merge.
    reserved: usize,
}

/// Merges the pool's partials into the dense slots, which every reader of
/// the slots must see whole. What the merged states reserve on the way - a
/// distinct set growing to the union of two - is added to `group_reserved`,
/// so a spill hands it back with the groups.
fn settle_dense(
    dense: &mut Option<DenseGroupSlots>,
    pool: &mut DensePool,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let partials = std::mem::take(&mut pool.partials);
    let before = memory.used();
    let outcome = (|| {
        if let Some(slots) = dense.as_mut() {
            for partial in partials {
                *slots = merge_dense_slots(std::mem::take(slots), partial, aggregates, memory)?;
            }
        }
        Ok(())
    })();
    *group_reserved = group_reserved.saturating_add(memory.used().saturating_sub(before));
    memory.release(pool.reserved);
    pool.reserved = 0;
    outcome
}

/// Whether worker partials may live across batches and windows: only when
/// every lane's merge is order-free. Which rows a pooled partial sees
/// depends on scheduling, and a floating-point total depends on the order
/// its parts are added in, as does where a checked integer sum overflows,
/// so those lanes keep one partial per chunk, merged in chunk order.
fn poolable(lanes: &[TwoPassLane]) -> bool {
    lanes.iter().all(|lane| {
        matches!(
            lane,
            TwoPassLane::CountStar
                | TwoPassLane::DecimalUnits { .. }
                | TwoPassLane::ExtremeDecimal { .. }
                | TwoPassLane::Distinct { .. }
                | TwoPassLane::Exact { .. }
                | TwoPassLane::Present { .. }
                | TwoPassLane::Temporal { .. }
        )
    })
}

/// The persistent slab also bounds the states allocated lazily in its
/// occupied slots. Worker slabs have their own temporary reservation.
fn dense_reservation(keys: TwoPassKeySource, slots: usize, lanes: usize) -> usize {
    let state_bytes = if matches!(keys, TwoPassKeySource::DateParts { .. }) {
        0
    } else {
        lanes.saturating_mul(size_of::<AggregateState>())
    };
    slots.saturating_mul(size_of::<Option<Vec<AggregateState>>>() + state_bytes)
}

/// Single text column: intern ids 0..=1023 map to slots 1..=1024.
const DENSE_TEXT_CAP: usize = 1024;

fn dense_slot_count(keys: TwoPassKeySource) -> Option<usize> {
    match keys {
        TwoPassKeySource::DateParts { parts } => dense_date_slot_count(parts),
        TwoPassKeySource::Text { .. }
        | TwoPassKeySource::Int {
            group_type: DataType::Int64 | DataType::UInt64,
            ..
        } => Some(DENSE_TEXT_CAP + 1),
        TwoPassKeySource::Int { .. } => None,
    }
}

/// Whether every sentinel the current intern table can produce still fits
/// the dense slots.
fn dense_in_bounds(keys: TwoPassKeySource, intern_len: usize) -> bool {
    match keys {
        TwoPassKeySource::Text { .. } => intern_len <= DENSE_TEXT_CAP,
        // Date-part domains are checked per row instead: the table covers a
        // bounded window of years and the fold abandons it when a value
        // falls outside, which no table-wide check can predict.
        TwoPassKeySource::DateParts { .. } | TwoPassKeySource::Int { .. } => true,
    }
}

fn dense_slot_index(keys: TwoPassKeySource, key_bits: u64, key_null: bool) -> usize {
    match keys {
        TwoPassKeySource::Text { .. } | TwoPassKeySource::Int { .. } => {
            if key_null {
                0
            } else {
                usize::try_from(key_bits).expect("intern id fits usize") + 1
            }
        }
        // The date-part fold indexes its own slots, because unlike text it
        // can fail: a year outside the table's window has no slot at all.
        TwoPassKeySource::DateParts { .. } => unreachable!("date parts index their own slots"),
    }
}

/// Inverse of [`dense_slot_index`]: the map key the classic path would use.
fn dense_slot_sentinel(keys: TwoPassKeySource, index: usize) -> (u64, bool) {
    match keys {
        TwoPassKeySource::Text { .. } | TwoPassKeySource::Int { .. } => {
            if index == 0 {
                (0, true)
            } else {
                (
                    u64::try_from(index - 1).expect("slot index fits u64"),
                    false,
                )
            }
        }
        TwoPassKeySource::DateParts { parts } => (dense_date_key(parts, index), false),
    }
}

/// Integer domains use slots 1..=1024 for nonnegative values and slot 0
/// for NULL. Validate a window before updating any partial, so an outlier
/// replays the entire window through scatter without duplicating its prefix.
fn dense_integer_window_in_bounds(
    window: &[(RecordBatch, Vec<Vec<u64>>)],
    keys: TwoPassKeySource,
) -> bool {
    let TwoPassKeySource::Int { column, .. } = keys else {
        return true;
    };
    window.iter().all(|(batch, _)| {
        let Some((typed, validity)) = batch.column(column).and_then(crate::ColumnVector::typed)
        else {
            return false;
        };
        batch.selection().selected_rows().all(|row| {
            !validity.is_valid(row)
                || typed
                    .int_key_at(row)
                    .is_some_and(|key| (0..1024).contains(&key))
        })
    })
}

/// Packed count and exact integer SUM lanes resolve columns and dispatch
/// once per batch. The same dense states are used by the generic lanes and
/// by the map/spill merge; no parallel aggregate representation is retained.
fn dense_packed_lanes(
    batch: &RecordBatch,
    slot_for: impl Fn(usize) -> usize,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
) -> Result<bool, ExecError> {
    use crate::batch::TypedValues;
    if !lanes.iter().zip(aggregates).all(|(lane, aggregate)| {
        matches!(lane, TwoPassLane::CountStar)
            || matches!((lane, aggregate.function, aggregate.sum_carrier),
                (TwoPassLane::Int { column, data_type }, AggregateFunction::Sum, Some(output))
                    if output == *data_type && batch.column(*column).and_then(crate::ColumnVector::typed)
                        .is_some_and(|(typed, _)| matches!(typed, TypedValues::Int64(_) | TypedValues::UInt64(_))))
    }) { return Ok(false); }
    for row in batch.selection().selected_rows() {
        slots[slot_for(row)]
            .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
    }
    for (lane_index, lane) in lanes.iter().enumerate() {
        match lane {
            TwoPassLane::CountStar => {
                for row in batch.selection().selected_rows() {
                    slots[slot_for(row)].as_mut().expect("initialized slot")[lane_index]
                        .add_dense_count(1)?;
                }
            }
            TwoPassLane::Int { column, .. } => {
                let (typed, validity) = batch
                    .column(*column)
                    .and_then(crate::ColumnVector::typed)
                    .expect("checked packed lane");
                // Monomorphized reads keep the physical type dispatch outside
                // the row loop and preserve unsigned values above i64::MAX.
                macro_rules! fold {
                    ($values:expr, $update:ident) => {
                        for row in batch.selection().selected_rows() {
                            if validity.is_valid(row) {
                                slots[slot_for(row)].as_mut().expect("initialized slot")
                                    [lane_index]
                                    .$update($values[row])?;
                            }
                        }
                    };
                }
                match typed {
                    TypedValues::Int64(values) => fold!(values, add_dense_signed),
                    TypedValues::UInt64(values) => fold!(values, add_dense_unsigned),
                    _ => unreachable!("checked packed lane"),
                }
            }
            _ => unreachable!("checked packed lanes"),
        }
    }
    Ok(true)
}

/// Dense pass over one batch: same key readers as
/// [`two_pass_scatter_text_prepared`], same lane extraction and state
/// updates as scatter + flush — minus the buffering between them.
#[allow(clippy::too_many_arguments)]
fn two_pass_dense_batch(
    batch: &RecordBatch,
    keys: TwoPassKeySource,
    columns: &[usize],
    translations: &[Vec<u64>],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let mut readers = Vec::with_capacity(columns.len());
    for (column, translation) in columns.iter().zip(translations) {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, _)) = strings.dictionary() else {
            return Err(ExecError::InvalidBatch(
                "prepared text scatter requires dictionary codes",
            ));
        };
        readers.push((codes, validity, translation));
    }
    if let TwoPassKeySource::Int { column, .. } = keys {
        let (typed, validity) = batch
            .column(column)
            .and_then(crate::ColumnVector::typed)
            .ok_or(ExecError::InvalidBatch(
                "dense integer key lost its packed projection",
            ))?;
        let slot_for = |row| {
            if validity.is_valid(row) {
                usize::try_from(typed.int_key_at(row).expect("checked integer key"))
                    .expect("checked dense domain")
                    + 1
            } else {
                0
            }
        };
        if dense_packed_lanes(batch, slot_for, lanes, aggregates, slots)? {
            return Ok(());
        }
        for row in batch.selection().selected_rows() {
            let states = slots[slot_for(row)]
                .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
            for (index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
                if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                    apply_two_pass_lane(&mut states[index], lane, aggregate, bits, memory)?;
                }
            }
        }
        return Ok(());
    }
    if let [(codes, validity, translation)] = readers.as_slice() {
        let slot_for = |row| {
            if validity.is_valid(row) {
                usize::try_from(translation[codes[row] as usize]).expect("intern id fits usize") + 1
            } else {
                0
            }
        };
        if dense_packed_lanes(batch, slot_for, lanes, aggregates, slots)? {
            return Ok(());
        }
    }
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                key_bits = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
            } else {
                key_null = true;
            }
        }
        let states = slots[dense_slot_index(keys, key_bits, key_null)]
            .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
        for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
            if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                apply_two_pass_lane(&mut states[lane_index], lane, aggregate, bits, memory)?;
            }
        }
    }
    Ok(())
}

/// One chunk of a dense window when every lane is packed: rows sum into
/// plain cells per slot, and the slots' states see each lane once at the
/// end of the chunk. A batch whose key columns lost their dictionary codes
/// takes the per-row fold instead.
#[allow(clippy::too_many_arguments)]
fn dense_packed_chunk(
    chunk: &[(RecordBatch, Vec<Vec<u64>>)],
    keys: TwoPassKeySource,
    columns: &[usize],
    lanes: &[TwoPassLane],
    packed: &[Option<PackedLane>],
    aggregates: &[CompiledAggregate],
    acc: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let unpacked = packed.iter().any(Option::is_none);
    let mut fold = PackedFold::sharing(acc.len(), packed, lanes);
    let mut selected = Vec::new();
    for (batch, translations) in chunk {
        // The selected rows are listed once; the slots, the packed fold and
        // the unpacked lanes all walk that list.
        let rows = fold_rows(batch, 0..batch.row_count(), &mut selected);
        let Some(slots) = dense_row_slots(batch, keys, columns, translations, &rows)? else {
            two_pass_dense_batch(
                batch,
                keys,
                columns,
                translations,
                lanes,
                aggregates,
                acc,
                memory,
            )?;
            continue;
        };
        // Packed lanes fold a column at a time; a batch whose decimal
        // column carries no 64-bit units folds the same totals row by row.
        if let Some(inputs) = fold.resolve(batch, lanes) {
            fold.fold(&inputs, &slots, &rows);
        } else {
            let readers = lane_readers(batch, lanes);
            for (row, slot) in rows.iter().zip(&slots) {
                fold.add_row(*slot as usize, &readers, row);
            }
        }
        if unpacked {
            apply_unpacked_lanes(batch, &rows, &slots, lanes, packed, aggregates, acc, memory)?;
        }
    }
    for (slot, entry) in acc.iter_mut().enumerate() {
        if !fold.occupied(slot) {
            continue;
        }
        let states =
            entry.get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
        fold.commit_slot(slot, states, aggregates, memory)?;
    }
    Ok(())
}

/// Each selected row's dense slot, in selection order, for an integer
/// or text key - the slots [`two_pass_dense_batch`] folds into.
/// `None` when a text key column carries no dictionary codes.
fn dense_row_slots(
    batch: &RecordBatch,
    keys: TwoPassKeySource,
    columns: &[usize],
    translations: &[Vec<u64>],
    selected: &FoldRows<'_>,
) -> Result<Option<Vec<u32>>, ExecError> {
    let mut slots = Vec::with_capacity(batch.visible_row_count());
    if let TwoPassKeySource::Int { column, .. } = keys {
        let (typed, validity) = batch
            .column(column)
            .and_then(crate::ColumnVector::typed)
            .ok_or(ExecError::InvalidBatch(
                "dense integer key lost its packed projection",
            ))?;
        for row in batch.selection().selected_rows() {
            let slot = if validity.is_valid(row) {
                usize::try_from(typed.int_key_at(row).expect("checked integer key"))
                    .expect("checked dense domain")
                    + 1
            } else {
                0
            };
            slots.push(u32::try_from(slot).expect("dense slot fits u32"));
        }
        return Ok(Some(slots));
    }
    let mut readers = Vec::with_capacity(columns.len());
    for (column, translation) in columns.iter().zip(translations) {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, _)) = strings.dictionary() else {
            return Ok(None);
        };
        readers.push((codes, validity, translation));
    }
    // A row's slot is its intern id + 1, and 0 for NULL - `dense_slot_index`
    // spelled as one lookup. The slots are tabled per dictionary code once
    // per batch, so the row loop is a lookup; a code with no translation
    // saturates to a slot past the table and fails below.
    let tables = readers
        .iter()
        .map(|(_, _, translation)| {
            translation
                .iter()
                .map(|id| u32::try_from(id + 1).unwrap_or(u32::MAX))
                .collect::<Vec<u32>>()
        })
        .collect::<Vec<_>>();
    let share = |table: &[u32], code: u32| table.get(code as usize).copied().unwrap_or(u32::MAX);
    let rows = batch.row_count();
    let no_nulls = readers
        .iter()
        .all(|(codes, validity, _)| validity.no_nulls() && codes.len() >= rows);
    match (readers.as_slice(), tables.as_slice()) {
        // One NOT NULL key: a lookup per listed row, whole span or filtered.
        ([(codes, ..)], [table]) if no_nulls => match selected {
            FoldRows::Span(span) => {
                slots.extend(codes[span.clone()].iter().map(|code| share(table, *code)));
            }
            FoldRows::Picked(picked) => {
                slots.extend(picked.iter().map(|row| share(table, codes[*row as usize])));
            }
        },
        _ => {
            for row in batch.selection().selected_rows() {
                let mut slot = 0_u32;
                for ((codes, validity, _), table) in readers.iter().zip(&tables) {
                    if validity.is_valid(row) {
                        slot = slot.saturating_add(share(table, codes[row]));
                    }
                }
                slots.push(slot);
            }
        }
    }
    let slot_count = dense_slot_count(keys).expect("text keys have dense slots");
    if slots.iter().any(|slot| *slot as usize >= slot_count) {
        return Err(ExecError::InvalidBatch("dictionary code is out of bounds"));
    }
    Ok(Some(slots))
}

/// Folds one window into the dense slots of a text key.
///
/// One partial per rayon worker (fold), merged pairwise (reduce): batches of
/// the window aggregate in parallel with no per-row buffering and no hashing.
/// Transient partials are bounded by worker count x slot table, under the
/// scatter window's own reservation.
fn dense_text_window(
    window: &[(RecordBatch, Vec<Vec<u64>>)],
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
    pool: &mut DensePool,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let columns: &[usize] = match keys {
        TwoPassKeySource::Text { column } => &[column],
        TwoPassKeySource::Int { .. } => &[],
        TwoPassKeySource::DateParts { .. } => unreachable!("dense slots are text or integer keyed"),
    };
    let slot_count = slots.len();
    // Several chunks per thread, not one: a slab per chunk bounds the
    // partials, but one chunk per thread leaves the pool with nothing to
    // steal, so the window ends when its slowest chunk does. Measured on
    // the benchmark replica, where a batch's cost varies with its groups.
    let chunk_size = window
        .len()
        .div_ceil(rayon::current_num_threads().saturating_mul(4))
        .max(1);
    let packed = lanes
        .iter()
        .zip(aggregates)
        .map(|(lane, aggregate)| packed_lane(lane, aggregate))
        .collect::<Vec<_>>();
    let any_packed = packed.iter().any(Option::is_some);
    let fold_chunk = |chunk: &[(RecordBatch, Vec<Vec<u64>>)], acc: &mut DenseGroupSlots| {
        if any_packed {
            return dense_packed_chunk(
                chunk, keys, columns, lanes, &packed, aggregates, acc, memory,
            );
        }
        for (batch, translations) in chunk {
            two_pass_dense_batch(
                batch,
                keys,
                columns,
                translations,
                lanes,
                aggregates,
                acc,
                memory,
            )?;
        }
        Ok(())
    };
    let partial_bytes = |partials: usize| {
        partials.saturating_mul(slot_count).saturating_mul(
            size_of::<Option<Vec<AggregateState>>>()
                + aggregates.len() * size_of::<AggregateState>(),
        )
    };
    if poolable(lanes) {
        // One partial per worker at most, charged once for the pool's life.
        if pool.reserved == 0 {
            let bytes = partial_bytes(rayon::current_num_threads().max(1));
            if memory.reserve(bytes).is_err() {
                return Ok(false);
            }
            pool.reserved = bytes;
        }
        let shared = std::sync::Mutex::new(std::mem::take(&mut pool.partials));
        let poisoned = || ExecError::InvalidBatch("dense partial pool poisoned");
        let outcome = window.par_chunks(chunk_size).try_for_each(|chunk| {
            let taken = shared.lock().map_err(|_| poisoned())?.pop();
            let mut acc = taken.unwrap_or_else(|| vec![None; slot_count]);
            let folded = fold_chunk(chunk, &mut acc);
            shared.lock().map_err(|_| poisoned())?.push(acc);
            folded
        });
        pool.partials = shared.into_inner().map_err(|_| poisoned())?;
        outcome?;
        return Ok(true);
    }
    let workers = window.len().div_ceil(chunk_size);
    let partial_bytes = partial_bytes(workers);
    if memory.reserve(partial_bytes).is_err() {
        return Ok(false);
    }
    let outcome = (|| {
        let partials = window
            .par_chunks(chunk_size)
            .map(|chunk| {
                let mut acc = vec![None; slot_count];
                fold_chunk(chunk, &mut acc)?;
                Ok(acc)
            })
            .collect::<Result<Vec<_>, ExecError>>()?;
        for partial in partials {
            *slots = merge_dense_slots(std::mem::take(slots), partial, aggregates, memory)?;
        }
        Ok(true)
    })();
    memory.release(partial_bytes);
    outcome
}

/// Folds one window into the dense date-part slots, or reports that a value
/// fell outside the table's domain so the caller can fall back.
fn dense_date_parts_window(
    window: &[(RecordBatch, Vec<Vec<u64>>)],
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let slot_count = slots.len();
    let packed = lanes
        .iter()
        .zip(aggregates)
        .map(|(lane, aggregate)| packed_lane(lane, aggregate))
        .collect::<Vec<_>>();
    let any_packed = packed.iter().any(Option::is_some);
    let folded = window
        .par_iter()
        .try_fold(
            || {
                (
                    vec![None; slot_count],
                    any_packed.then(|| PackedFold::sharing(slot_count, &packed, lanes)),
                )
            },
            |(mut acc, mut fold), (batch, _)| {
                if let Some(fold) = &mut fold {
                    dense_date_parts_packed_batch(
                        batch, parts, lanes, &packed, aggregates, &mut acc, fold, memory,
                    )?;
                } else {
                    two_pass_dense_date_parts_batch(
                        batch, parts, lanes, aggregates, &mut acc, memory,
                    )?;
                }
                Ok((acc, fold))
            },
        )
        .map(|folded| {
            let (mut acc, fold) = folded?;
            if let Some(fold) = fold {
                for (slot, entry) in acc.iter_mut().enumerate() {
                    if fold.occupied(slot) {
                        let states = entry.get_or_insert_with(|| {
                            aggregates.iter().map(AggregateState::new).collect()
                        });
                        fold.commit_slot(slot, states, aggregates, memory)
                            .map_err(DenseFold::Exec)?;
                    }
                }
            }
            Ok(acc)
        })
        .try_reduce(
            || vec![None; slot_count],
            |left, right| {
                merge_dense_slots(left, right, aggregates, memory).map_err(DenseFold::Exec)
            },
        );
    match folded {
        Ok(folded) => {
            *slots = merge_dense_slots(std::mem::take(slots), folded, aggregates, memory)?;
            Ok(true)
        }
        Err(DenseFold::Exec(error)) => Err(error),
        Err(DenseFold::OutOfDomain) => Ok(false),
    }
}

/// Each selected row's date-part slot, in selection order, or
/// [`DenseFold::OutOfDomain`] when a value has no slot.
///
/// Two calendar parts of one DATE column - the YEAR/MONTH grouping - are
/// tabled over the batch's span of days: one civil conversion per distinct
/// day instead of one per row, then a lookup per row.
fn date_part_slots(
    batch: &RecordBatch,
    parts: [Option<(DatePart, usize)>; 2],
) -> Result<Vec<u32>, DenseFold> {
    const NO_SLOT: u32 = u32::MAX;
    let slot_of = |key_bits: u64| -> Result<u32, DenseFold> {
        dense_date_slot(parts, key_bits)
            .and_then(|slot| u32::try_from(slot).ok())
            .ok_or(DenseFold::OutOfDomain)
    };
    let mut slots = Vec::with_capacity(batch.visible_row_count());
    let mut present = parts.iter().flatten();
    let column = present.next().map(|(_, column)| *column);
    if let Some(column) = column
        && parts.iter().flatten().all(|(part, each)| {
            *each == column && matches!(part, DatePart::Year | DatePart::Month | DatePart::Day)
        })
        && let Some(vector) = batch.column(column)
        && vector.data_type() == DataType::Date32
        && let Some((crate::batch::TypedValues::Temporal { units, .. }, validity)) = vector.typed()
        && units.len() >= batch.row_count()
    {
        let pick = |part: DatePart, year: i64, month: i64, day: i64| -> u64 {
            let value = match part {
                DatePart::Year => year,
                DatePart::Month => month,
                _ => day,
            };
            // Matches evaluate_units_date_part: out-of-range clamps to 0,
            // then the scatter packing adds one.
            u64::try_from(value).unwrap_or(0) + 1
        };
        let key_of = |day_units: i64| {
            let (year, month, day) = pintail_types::civil_from_days(day_units);
            parts.iter().flatten().fold(0_u64, |bits, (part, _)| {
                (bits << 20) | pick(*part, year, month, day)
            })
        };
        let null_slot = slot_of(0)?;
        let bounds = batch
            .selection()
            .selected_rows()
            .filter(|row| validity.is_valid(*row))
            .map(|row| units[row])
            .fold(None, |bounds: Option<(i64, i64)>, day| {
                Some(bounds.map_or((day, day), |(low, high)| (low.min(day), high.max(day))))
            });
        let span = bounds.and_then(|(low, high)| usize::try_from(high.checked_sub(low)?).ok());
        if let (Some((low, _)), Some(span)) = (bounds, span)
            && span < DENSE_DATE_SLOT_CAP
        {
            let table = (0..=span)
                .map(|offset| {
                    let day = low + i64::try_from(offset).expect("span fits i64");
                    slot_of(key_of(day)).unwrap_or(NO_SLOT)
                })
                .collect::<Vec<u32>>();
            for row in batch.selection().selected_rows() {
                let slot = if validity.is_valid(row) {
                    let offset = usize::try_from(units[row] - low).expect("inside the bounds");
                    table[offset]
                } else {
                    null_slot
                };
                if slot == NO_SLOT {
                    return Err(DenseFold::OutOfDomain);
                }
                slots.push(slot);
            }
            return Ok(slots);
        }
        for row in batch.selection().selected_rows() {
            let key_bits = if validity.is_valid(row) {
                key_of(units[row])
            } else {
                0
            };
            slots.push(slot_of(key_bits)?);
        }
        return Ok(slots);
    }
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        for (part, column) in parts.iter().flatten() {
            let id = match crate::expression::evaluate_units_date_part(batch, *column, row, *part) {
                // Same 20-bit lane contract as the sparse encoder.
                Some(Ok(Value::Int64(value))) => match u64::try_from(value) {
                    Ok(value) if value < 0xF_FFFF => value + 1,
                    _ => {
                        return Err(DenseFold::Exec(ExecError::InvalidBatch(
                            "date-part group key does not fit its 20-bit lane",
                        )));
                    }
                },
                Some(Ok(Value::Null)) => 0,
                Some(Err(error)) => return Err(DenseFold::Exec(error)),
                _ => {
                    return Err(DenseFold::Exec(ExecError::InvalidBatch(
                        "date-part group key column lost its packed units",
                    )));
                }
            };
            key_bits = (key_bits << 20) | id;
        }
        slots.push(slot_of(key_bits)?);
    }
    Ok(slots)
}

/// [`two_pass_dense_date_parts_batch`] with the packed lanes folded a column
/// at a time into `fold`; the other lanes apply per row as before.
#[allow(clippy::too_many_arguments)]
fn dense_date_parts_packed_batch(
    batch: &RecordBatch,
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    packed: &[Option<PackedLane>],
    aggregates: &[CompiledAggregate],
    acc: &mut DenseGroupSlots,
    fold: &mut PackedFold,
    memory: &MemoryTracker,
) -> Result<(), DenseFold> {
    let slots = date_part_slots(batch, parts)?;
    let mut selected = Vec::new();
    let rows = fold_rows(batch, 0..batch.row_count(), &mut selected);
    if let Some(inputs) = fold.resolve(batch, lanes) {
        fold.fold(&inputs, &slots, &rows);
    } else {
        let readers = lane_readers(batch, lanes);
        for (row, slot) in rows.iter().zip(&slots) {
            fold.add_row(*slot as usize, &readers, row);
        }
    }
    if packed.iter().any(Option::is_none) {
        apply_unpacked_lanes(batch, &rows, &slots, lanes, packed, aggregates, acc, memory)
            .map_err(DenseFold::Exec)?;
    }
    Ok(())
}

/// Applies the lanes that are not packed to the dense slots' states, one
/// lane at a time: each lane's reader and kind resolve once per batch, and
/// a COUNT(DISTINCT) over a packed integer column inserts straight from it.
/// Every lane still sees its rows in row order, so each state ends exactly
/// where the row-at-a-time loop left it.
#[allow(clippy::too_many_arguments)]
fn apply_unpacked_lanes(
    batch: &RecordBatch,
    rows: &FoldRows<'_>,
    slots: &[u32],
    lanes: &[TwoPassLane],
    packed: &[Option<PackedLane>],
    aggregates: &[CompiledAggregate],
    acc: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    for slot in slots {
        acc[*slot as usize]
            .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
    }
    let readers = lane_readers(batch, lanes);
    for (index, ((lane, aggregate), reader)) in
        lanes.iter().zip(aggregates).zip(&readers).enumerate()
    {
        if packed[index].is_some() {
            continue;
        }
        // The key a distinct lane dedups on, from its column's bits: the
        // conversion `apply_two_pass_lane` makes.
        let signed = matches!(
            lane,
            TwoPassLane::Distinct {
                data_type: DataType::Int64,
                ..
            }
        );
        macro_rules! distinct_from {
            ($values:expr, $validity:expr, $bits:expr) => {
                insert_distinct_keys(
                    acc,
                    index,
                    rows,
                    slots,
                    $values,
                    $validity,
                    |value| {
                        let bits: u64 = $bits(value);
                        if signed {
                            i128::from(i64::from_ne_bytes(bits.to_ne_bytes()))
                        } else {
                            i128::from(bits)
                        }
                    },
                    memory,
                )?
            };
        }
        match (lane, reader) {
            (TwoPassLane::Distinct { .. }, LaneReader::Int64(values, validity)) => {
                distinct_from!(values, validity, |value: i64| u64::from_ne_bytes(
                    value.to_ne_bytes()
                ));
            }
            (TwoPassLane::Distinct { .. }, LaneReader::UInt64(values, validity)) => {
                distinct_from!(values, validity, |value: u64| value);
            }
            _ => {
                for (row, slot) in rows.iter().zip(slots) {
                    if let Some(bits) = reader.bits(row) {
                        let states = acc[*slot as usize].as_mut().expect("states made above");
                        apply_two_pass_lane(&mut states[index], lane, aggregate, bits, memory)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Inserts one batch's keys into a COUNT(DISTINCT) lane's group states.
///
/// A group whose distinct set is a bitmap takes its keys as unchecked bit
/// sets and is recounted once after the batch (see `OpenDistinctBits`): the
/// per-key test, count and branch on whether the key was new each waited on
/// a bitmap word that a low-cardinality key's few groups, each spanning the
/// whole key range, keep out of the first-level cache. A recount reads the
/// whole bitmap, so the bitmaps open only while recounting all of them costs
/// no more than a few words per row of the batch. Every other key, and
/// every key outside its bitmap's window, inserts one at a time after the
/// recount; a set's members and count do not depend on the order its keys
/// arrive in.
#[allow(clippy::too_many_arguments)]
fn insert_distinct_keys<T: Copy>(
    acc: &mut DenseGroupSlots,
    index: usize,
    rows: &FoldRows<'_>,
    slots: &[u32],
    values: &[T],
    validity: &crate::array::ValidityMask,
    key: impl Fn(T) -> i128,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let mut open = if acc.len() <= rows.len() {
        acc.iter_mut()
            .map(|entry| {
                entry
                    .as_mut()
                    .and_then(|states| states[index].open_distinct_bits())
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let words = open
        .iter()
        .flatten()
        .fold(0_usize, |words, bits| words.saturating_add(bits.words()));
    if words > rows.len().saturating_mul(4) || open.iter().all(Option::is_none) {
        drop(open);
        for (row, slot) in rows.iter().zip(slots) {
            if validity.is_valid(row) {
                acc[*slot as usize].as_mut().expect("states made above")[index]
                    .update_distinct_count_int(key(values[row]), memory)?;
            }
        }
        return Ok(());
    }
    let mut misses = Vec::new();
    set_distinct_keys(&mut open, rows, slots, values, validity, key, &mut misses);
    let touched = open
        .iter()
        .enumerate()
        .filter(|(_, bits)| bits.as_ref().is_some_and(|bits| bits.touched))
        .map(|(slot, _)| slot)
        .collect::<Vec<_>>();
    drop(open);
    for slot in touched {
        acc[slot].as_mut().expect("opened above")[index].settle_distinct_bits()?;
    }
    for (slot, key) in misses {
        acc[slot as usize].as_mut().expect("states made above")[index]
            .update_distinct_count_int(key, memory)?;
    }
    Ok(())
}

/// Sets each listed row's distinct key in its slot's opened bitmap, and
/// lists the keys with no open bitmap or outside its window in `misses`.
/// The loops are spelled per row shape and per validity so the body is a
/// load, a subtraction and a bit set.
#[inline]
fn set_distinct_keys<T: Copy>(
    open: &mut [Option<OpenDistinctBits<'_>>],
    rows: &FoldRows<'_>,
    slots: &[u32],
    values: &[T],
    validity: &crate::array::ValidityMask,
    key: impl Fn(T) -> i128,
    misses: &mut Vec<(u32, i128)>,
) {
    let mut set = |slot: u32, value: T| {
        let key = key(value);
        if !open[slot as usize]
            .as_mut()
            .is_some_and(|bits| bits.set(key))
        {
            misses.push((slot, key));
        }
    };
    match (rows, validity.no_nulls()) {
        (FoldRows::Span(span), true) => {
            for (&slot, &value) in slots.iter().zip(&values[span.clone()]) {
                set(slot, value);
            }
        }
        (FoldRows::Picked(picked), true) => {
            for (&slot, &row) in slots.iter().zip(*picked) {
                set(slot, values[row as usize]);
            }
        }
        _ => {
            for (slot, row) in slots.iter().zip(rows.iter()) {
                if validity.is_valid(row) {
                    set(*slot, values[row]);
                }
            }
        }
    }
}

/// Dense pass over one batch for a date-part key: the same part extraction
/// the scatter does, applied straight into slots instead of buffered into
/// buckets. Returns [`DenseFold::OutOfDomain`] when a value has no slot, so
/// the caller can fall back rather than merge distinct groups together.
#[allow(clippy::too_many_arguments)]
fn two_pass_dense_date_parts_batch(
    batch: &RecordBatch,
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<(), DenseFold> {
    // The Q5 shape - two calendar parts over ONE Date32 column - pays for
    // two civil conversions and two column resolutions per row on the
    // generic path below, when a single conversion yields year, month and
    // day together and the column never changes within a batch. This front
    // handles exactly that shape; everything else falls through unchanged.
    if let [
        Some((first_part, first_column)),
        Some((second_part, second_column)),
    ] = parts
        && first_column == second_column
        && matches!(first_part, DatePart::Year | DatePart::Month | DatePart::Day)
        && matches!(
            second_part,
            DatePart::Year | DatePart::Month | DatePart::Day
        )
        && let Some(vector) = batch.column(first_column)
        && vector.data_type() == DataType::Date32
        && let Some((crate::batch::TypedValues::Temporal { units, .. }, validity)) = vector.typed()
    {
        let pick = |part: DatePart, year: i64, month: i64, day: i64| -> u64 {
            let value = match part {
                DatePart::Year => year,
                DatePart::Month => month,
                _ => day,
            };
            // Matches evaluate_units_date_part: out-of-range clamps to 0,
            // then the scatter packing adds one.
            u64::try_from(value).unwrap_or(0) + 1
        };
        for row in batch.selection().selected_rows() {
            let key_bits = if validity.is_valid(row) {
                let day_units = *units
                    .get(row)
                    .ok_or(DenseFold::Exec(ExecError::InvalidBatch(
                        "date-part group key column ended before its rows",
                    )))?;
                let (year, month, day) = pintail_types::civil_from_days(day_units);
                (pick(first_part, year, month, day) << 20) | pick(second_part, year, month, day)
            } else {
                0
            };
            let Some(slot) = dense_date_slot(parts, key_bits) else {
                return Err(DenseFold::OutOfDomain);
            };
            let states = slots[slot]
                .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
            for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
                if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                    apply_two_pass_lane(&mut states[lane_index], lane, aggregate, bits, memory)
                        .map_err(DenseFold::Exec)?;
                }
            }
        }
        return Ok(());
    }
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        for (part, column) in parts.iter().flatten() {
            let id = match crate::expression::evaluate_units_date_part(batch, *column, row, *part) {
                // Same 20-bit lane contract as the sparse encoder above.
                Some(Ok(Value::Int64(value))) => match u64::try_from(value) {
                    Ok(value) if value < 0xF_FFFF => value + 1,
                    _ => {
                        return Err(DenseFold::Exec(ExecError::InvalidBatch(
                            "date-part group key does not fit its 20-bit lane",
                        )));
                    }
                },
                Some(Ok(Value::Null)) => 0,
                Some(Err(error)) => return Err(DenseFold::Exec(error)),
                _ => {
                    return Err(DenseFold::Exec(ExecError::InvalidBatch(
                        "date-part group key column lost its packed units",
                    )));
                }
            };
            key_bits = (key_bits << 20) | id;
        }
        let Some(slot) = dense_date_slot(parts, key_bits) else {
            return Err(DenseFold::OutOfDomain);
        };
        let states =
            slots[slot].get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
        for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
            if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                apply_two_pass_lane(&mut states[lane_index], lane, aggregate, bits, memory)
                    .map_err(DenseFold::Exec)?;
            }
        }
    }
    Ok(())
}

/// Merges one dense partial into another (per-batch fold outputs).
fn merge_dense_slots(
    mut into: DenseGroupSlots,
    from: DenseGroupSlots,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<DenseGroupSlots, ExecError> {
    for (target, source) in into.iter_mut().zip(from) {
        let Some(source) = source else { continue };
        match target {
            None => *target = Some(source),
            Some(states) => {
                for ((state, other), aggregate) in states.iter_mut().zip(source).zip(aggregates) {
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
    }
    Ok(into)
}

/// Folds dense slots into the partition maps (dense overflow, mixed
/// serial-scatter flows, and the final pass share this): map collisions
/// merge state-by-state, so dense and classic results always unify.
///
/// A group moving into a map is charged again while the slab that already
/// paid for it stays charged, until the last slot has moved. When the budget
/// refuses that second charge nothing is committed further: the maps' groups
/// go to disk as one run and the slots not yet moved as another, straight
/// from the slots, and the whole charge - maps, slab and what the states
/// grew by - is given back, since no group it paid for is left in memory.
#[allow(clippy::too_many_arguments)]
fn fold_dense_into_maps(
    mut slots: DenseGroupSlots,
    keys: TwoPassKeySource,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    spill: &mut GroupSpill<'_>,
) -> Result<(), ExecError> {
    let slab = dense_reservation(keys, slots.len(), aggregates.len());
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    for index in 0..slots.len() {
        let Some(states) = slots[index].take() else {
            continue;
        };
        let (bits, null) = dense_slot_sentinel(keys, index);
        let partition =
            usize::try_from(crate::batch::mix64(bits ^ u64::from(null)) % partitions as u64)
                .expect("partition index fits usize");
        match maps[partition].entry((bits, null)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                if memory.reserve(per_group_bytes).is_err() {
                    slots[index] = Some(states);
                    spill.write(maps, group_reserved, memory)?;
                    let mut rest = dense_slot_groups(
                        slots,
                        keys,
                        spill.intern,
                        spill.labels,
                        spill.members,
                        spill.collation,
                    );
                    spill
                        .runs
                        .push(write_aggregate_spill_run(&mut rest, memory)?);
                    memory.release(*group_reserved);
                    *group_reserved = 0;
                    return Ok(());
                }
                *group_reserved = group_reserved.saturating_add(per_group_bytes);
                entry.insert(states);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                for ((state, other), aggregate) in
                    entry.get_mut().iter_mut().zip(states).zip(aggregates)
                {
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
    }
    memory.release(slab);
    *group_reserved = group_reserved.saturating_sub(slab);
    Ok(())
}

/// Pass 2 over several scatter outputs at once (one per parallel scatter
/// worker): each partition folds its bucket from EVERY set, so parallel
/// pass 1 needs no cross-worker merging (e13's shape, bounded windows).
#[allow(clippy::too_many_lines)]
/// Pass 2 for one partition when some lanes are packed: those lanes sum
/// into a compact per-partition table of plain cells and reach each group's
/// states once, at the end; the rest apply per row as before. Returns
/// `false`, having touched nothing, when the table's worst case does not
/// fit the query's budget, so the caller takes the per-row path.
#[allow(clippy::too_many_arguments)]
fn flush_partition_packed(
    sets: &[Vec<TwoPassBucket>],
    partition: usize,
    map: &mut GroupKeyMap,
    lanes: &[TwoPassLane],
    packed: &[Option<PackedLane>],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let lane_count = lanes.len();
    let rows = sets
        .iter()
        .map(|set| set[partition].keys.len())
        .sum::<usize>();
    if rows == 0 {
        return Ok(true);
    }
    // Worst case every row is its own group: an index entry plus a cell per
    // lane. Charged for the table's lifetime, handed back before returning.
    let bound = rows.saturating_mul(
        size_of::<((u64, bool), u32)>()
            .saturating_add(HASH_ENTRY_OVERHEAD)
            .saturating_add(lane_count.saturating_mul(size_of::<PackedCell>())),
    );
    if memory.reserve(bound).is_err() {
        return Ok(false);
    }
    let outcome = (|| {
        let mut index: HashMap<
            (u64, bool),
            u32,
            std::hash::BuildHasherDefault<super::aggregate::GroupKeyHasher>,
        > = HashMap::default();
        let mut cells: Vec<PackedCell> = Vec::new();
        let unpacked = packed.iter().any(Option::is_none);
        for set in sets {
            let bucket = &set[partition];
            for (row, (key, mask)) in bucket.keys.iter().zip(&bucket.masks).enumerate() {
                let key_null = mask & (1 << 7) != 0;
                let next = u32::try_from(index.len()).map_err(|_| ExecError::NumericOverflow)?;
                let slot = *index.entry((*key, key_null)).or_insert_with(|| {
                    cells.resize(cells.len() + lane_count, PackedCell::default());
                    next
                }) as usize;
                let lane_bits = &bucket.lanes[row * lane_count..(row + 1) * lane_count];
                let group_cells = &mut cells[slot * lane_count..(slot + 1) * lane_count];
                for (lane_index, lane) in packed.iter().enumerate() {
                    if let Some(lane) = lane
                        && mask & (1 << lane_index) == 0
                    {
                        group_cells[lane_index].add(*lane, lane_bits[lane_index])?;
                    }
                }
                if unpacked {
                    let states = map
                        .entry((*key, key_null))
                        .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
                    for (lane_index, ((lane, aggregate), packed)) in
                        lanes.iter().zip(aggregates).zip(packed).enumerate()
                    {
                        if packed.is_some() || mask & (1 << lane_index) != 0 {
                            continue;
                        }
                        apply_two_pass_lane(
                            &mut states[lane_index],
                            lane,
                            aggregate,
                            lane_bits[lane_index],
                            memory,
                        )?;
                    }
                }
            }
        }
        for (key, slot) in &index {
            let states = map
                .entry(*key)
                .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
            let slot = *slot as usize;
            for (lane_index, (lane, aggregate)) in packed.iter().zip(aggregates).enumerate() {
                if let Some(lane) = lane {
                    cells[slot * lane_count + lane_index].commit(
                        *lane,
                        &mut states[lane_index],
                        aggregate,
                        memory,
                    )?;
                }
            }
        }
        Ok(true)
    })();
    memory.release(bound);
    outcome
}

fn two_pass_flush_sets(
    sets: &mut [Vec<TwoPassBucket>],
    maps: &mut [GroupKeyMap],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let lane_count = lanes.len();
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    // Everything reserved while the rows are applied belongs to the maps:
    // the entries charged below and the distinct sets the states grow
    // through their own reservations. Measured as a difference so a spill
    // hands all of it back; charging the entries alone left the sets on
    // the books forever and a second flush ran into a ceiling the first
    // spill had supposedly freed.
    let used_before = memory.used();
    let sets_ref: &[Vec<TwoPassBucket>] = sets;
    let packed = lanes
        .iter()
        .zip(aggregates)
        .map(|(lane, aggregate)| packed_lane(lane, aggregate))
        .collect::<Vec<_>>();
    let any_packed = packed.iter().any(Option::is_some);
    let added = maps
        .par_iter_mut()
        .enumerate()
        .map(|(partition, map)| -> Result<usize, ExecError> {
            let before = map.len();
            if any_packed
                && flush_partition_packed(
                    sets_ref, partition, map, lanes, &packed, aggregates, memory,
                )?
            {
                let new_groups = map.len().saturating_sub(before);
                let bytes = new_groups.saturating_mul(per_group_bytes);
                memory.reserve(bytes)?;
                return Ok(bytes);
            }
            for set in sets_ref {
                let bucket = &set[partition];
                for (row, (key, mask)) in bucket.keys.iter().zip(&bucket.masks).enumerate() {
                    let key_null = mask & (1 << 7) != 0;
                    let states = map
                        .entry((*key, key_null))
                        .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
                    for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate()
                    {
                        if mask & (1 << lane_index) != 0 {
                            continue;
                        }
                        let bits = bucket.lanes[row * lane_count + lane_index];
                        apply_two_pass_lane(
                            &mut states[lane_index],
                            lane,
                            aggregate,
                            bits,
                            memory,
                        )?;
                    }
                }
            }
            let new_groups = map.len().saturating_sub(before);
            let bytes = new_groups.saturating_mul(per_group_bytes);
            memory.reserve(bytes)?;
            Ok(bytes)
        })
        .collect::<Vec<Result<usize, ExecError>>>();
    // The rows are in the maps whether or not their charge went through;
    // clearing the buckets first is what keeps a refused charge from
    // applying them a second time on the next flush.
    for set in sets.iter_mut() {
        for bucket in set.iter_mut() {
            bucket.keys.clear();
            bucket.masks.clear();
            bucket.lanes.clear();
        }
    }
    *group_reserved = group_reserved.saturating_add(memory.used().saturating_sub(used_before));
    let failure = added.into_iter().find_map(Result::err);
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod dense_date_tests {
    use super::{
        DENSE_DATE_SLOT_CAP, DatePart, dense_date_key, dense_date_slot, dense_date_slot_count,
    };

    const YEAR_MONTH: [Option<(DatePart, usize)>; 2] =
        [Some((DatePart::Year, 0)), Some((DatePart::Month, 1))];

    fn pack(year: u64, month: u64) -> u64 {
        // The scatter packs each part as (value + 1) in 20 bits, NULL as 0.
        ((year + 1) << 20) | (month + 1)
    }

    #[test]
    fn slots_round_trip_to_the_key_the_scatter_would_have_built() {
        // The slot index and its inverse are a matched pair: a mismatch would
        // silently attribute a group's rows to another group's key.
        assert!(dense_date_slot_count(YEAR_MONTH).is_some_and(|n| n <= DENSE_DATE_SLOT_CAP));
        for year in [1900, 1970, 2023, 2024, 2155] {
            for month in 1..=12 {
                let bits = pack(year, month);
                let slot = dense_date_slot(YEAR_MONTH, bits).expect("inside the dense domain");
                assert_eq!(dense_date_key(YEAR_MONTH, slot), bits, "{year}-{month}");
            }
        }
    }

    #[test]
    fn distinct_keys_never_share_a_slot() {
        let mut seen = std::collections::HashMap::new();
        for year in 1900..=2155_u64 {
            for month in 1..=12 {
                let bits = pack(year, month);
                let slot = dense_date_slot(YEAR_MONTH, bits).expect("inside the dense domain");
                assert_eq!(seen.insert(slot, bits), None, "slot {slot} reused");
            }
        }
    }

    #[test]
    fn values_outside_the_window_report_no_slot() {
        // Out of domain must be None rather than a wrapped slot: MySQL dates
        // reach year 9999, and folding those onto an in-range slot would
        // merge unrelated groups.
        for year in [0, 1, 999, 1899, 2156, 9999] {
            assert_eq!(
                dense_date_slot(YEAR_MONTH, pack(year, 6)),
                None,
                "year {year}"
            );
        }
    }

    #[test]
    fn nulls_take_the_zero_ordinal_and_round_trip() {
        for bits in [0, pack(2023, 5) & !0xF_FFFF, 1] {
            if let Some(slot) = dense_date_slot(YEAR_MONTH, bits) {
                assert_eq!(dense_date_key(YEAR_MONTH, slot), bits);
            }
        }
    }
}
