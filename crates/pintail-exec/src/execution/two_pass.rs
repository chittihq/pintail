//! Two-pass partitioned aggregation: scatter lanes, dense slots and
//! the string interning used to keep group keys comparable.

use std::collections::HashMap;

use pintail_sql::{AggregateFunction, DatePart};
use pintail_types::{DataType, Value};

use super::aggregate::GroupKeyMap;
use super::aggregate::{
    AggregateState, CompiledAggregate, aggregate_uses_float, decimal_average_scale,
    decimal_units_from_int,
};
use super::join::normalized_collation_text;
use super::{
    ExecError, HASH_ENTRY_OVERHEAD, MaterializedRows, MemoryTracker, PullOperator,
    estimated_row_payload_bytes,
};
use rayon::prelude::*;

use crate::collation::Collation;

use crate::{RecordBatch, expression::mysql_f64};

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
}

/// Whether every aggregate fits a scatter lane, and which kind. `None`
/// keeps the query on the sequential direct path.
/// Partitions per worker thread. See `build_streaming_two_pass_aggregate`.
const PARTITIONS_PER_WORKER: usize = 4;

pub(super) fn two_pass_lanes(
    aggregates: &[CompiledAggregate],
    batch: &RecordBatch,
) -> Option<Vec<TwoPassLane>> {
    if aggregates.len() > 7 {
        // One mask bit per lane plus the key bit.
        return None;
    }
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
                        DataType::Float64 => Some(TwoPassLane::Float { column }),
                        _ => match batch.column(column)?.data_type() {
                            // SUM and exact AVG both ride the packed-units
                            // lane; the per-row apply branches on the
                            // aggregate function.
                            DataType::Decimal { scale, .. }
                                if aggregate.function == AggregateFunction::Sum
                                    || decimal_average_scale(aggregate).is_some() =>
                            {
                                Some(TwoPassLane::DecimalUnits {
                                    column,
                                    scale,
                                    float_output: aggregate_uses_float(aggregate),
                                })
                            }
                            DataType::Decimal { .. } => Some(TwoPassLane::Float { column }),
                            _ => None,
                        },
                    }
                }
                AggregateFunction::Minimum | AggregateFunction::Maximum => {
                    if let DataType::Decimal { scale, .. } = batch.column(column)?.data_type() {
                        return Some(TwoPassLane::ExtremeDecimal { column, scale });
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
        .collect()
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
        Value::Utf8(_) | Value::Binary(_) | Value::Enum { .. } => None,
    }
}

fn two_pass_key_value(bits: u64, null: bool, data_type: DataType) -> Value {
    if null {
        return Value::Null;
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
    /// Two string columns: `(id_a + 1) << 32 | (id_b + 1)`, with 0 as the
    /// per-column NULL sentinel so `(NULL, x)`, `(x, NULL)` and
    /// `(NULL, NULL)` stay distinct groups.
    TextPair { first: usize, second: usize },
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
#[allow(clippy::too_many_lines)]
pub(super) fn build_streaming_two_pass_aggregate(
    input: &mut PullOperator,
    first: RecordBatch,
    keys: TwoPassKeySource,
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
    // Flush the scatter window at a quarter of the budget (bounded to
    // 1-64 MB) so the scan always keeps its transient headroom.
    let flush_bytes = (memory.limit() / 4).clamp(1 << 20, 64 << 20);
    let scan_floor = input.scan_transient_floor().saturating_mul(2);
    let mut buckets: Vec<TwoPassBucket> =
        (0..partitions).map(|_| TwoPassBucket::default()).collect();
    let mut maps: Vec<GroupKeyMap> = (0..partitions).map(|_| GroupKeyMap::default()).collect();
    let mut bucket_reserved = 0_usize;
    let mut group_reserved = 0_usize;
    let mut flushes = 0_u32;
    let mut intern = matches!(
        keys,
        TwoPassKeySource::Text { .. } | TwoPassKeySource::TextPair { .. }
    )
    .then(|| StringIntern {
        index: HashMap::new(),
        values: Vec::new(),
        collation,
    });
    // Distinct lanes stay on the classic path: dense per-worker partials
    // would replicate each group's distinct set per thread and pay a
    // drain-and-reinsert merge that costs more than the scatter it saves
    // (n4 585ms -> 768ms when measured on 2026-08-02).
    let mut dense = lanes
        .iter()
        .all(|lane| !matches!(lane, TwoPassLane::Distinct { .. }))
        .then(|| dense_slot_count(keys))
        .flatten()
        .map(|slots| {
            let mut table: DenseGroupSlots = Vec::new();
            table.resize_with(slots, || None);
            table
        });
    if let Some(slots) = &dense {
        let slab = slots
            .len()
            .saturating_mul(size_of::<Option<Vec<AggregateState>>>());
        memory.reserve(slab)?;
        group_reserved = group_reserved.saturating_add(slab);
    }

    let mut window: Vec<(RecordBatch, Vec<Vec<u64>>)> = Vec::new();
    let mut window_reserved = 0_usize;
    let mut window_rows = 0_usize;
    // Declared ENUM labels per text key column, captured from the first
    // batch that carries them. The intern table holds label TEXT only, so
    // without this the finalize below rebuilds every group key as a plain
    // string and the declaration index - which is what MySQL orders an
    // ENUM by - is erased exactly here (#251).
    let mut key_enum_labels: [Option<std::sync::Arc<Vec<String>>>; 2] = [None, None];
    let mut key_set_members: [Option<std::sync::Arc<Vec<String>>>; 2] = [None, None];
    let key_columns: [Option<usize>; 2] = match keys {
        TwoPassKeySource::Text { column } => [Some(column), None],
        TwoPassKeySource::TextPair { first, second } => [Some(first), Some(second)],
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => [None, None],
    };
    let mut batch = Some(first);
    loop {
        let Some(current) = batch.take() else {
            break;
        };
        for (slot, key_column) in key_columns.iter().enumerate() {
            if let Some(column) = key_column
                && key_enum_labels[slot].is_none()
                && key_set_members[slot].is_none()
                && let Some(vector) = current.column(*column)
                && let Some((crate::batch::TypedValues::Utf8(strings), _)) = vector.typed()
            {
                key_enum_labels[slot] = strings.declared_enum_labels().cloned();
                key_set_members[slot] = strings.declared_set_members().cloned();
            }
        }
        // String sources prepare their (tiny, per-distinct-value) dictionary
        // translations serially, then scatter rows in parallel from the
        // read-only tables; batches whose strings decoded without codes
        // fall back to the serial scatter below.
        let prepared = match (keys, &mut intern) {
            (TwoPassKeySource::Text { column }, Some(intern)) => {
                prepare_text_translations(&current, &[column], intern, memory)?
            }
            (TwoPassKeySource::TextPair { first, second }, Some(intern)) => {
                prepare_text_translations(&current, &[first, second], intern, memory)?
            }
            _ => Some(Vec::new()),
        };
        if let Some(translations) = prepared {
            let rows = current.visible_row_count();
            let need = rows
                .saturating_mul(scatter_row_bytes)
                .saturating_add(current.estimated_bytes());
            if let Err(error) = memory.reserve(need) {
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                )?;
                window_rows = 0;
                flushes += 1;
                if memory.reserve(need).is_err() {
                    return Err(error);
                }
            }
            window_reserved = window_reserved.saturating_add(need);
            window_rows += rows;
            window.push((current, translations));
            if window_rows.saturating_mul(scatter_row_bytes) >= flush_bytes
                || (scan_floor > 0 && memory.remaining() < scan_floor)
            {
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                )?;
                window_rows = 0;
                flushes += 1;
            }
            batch = input.next_batch(memory)?;
            continue;
        }
        let rows = current.visible_row_count();
        let bytes = rows.saturating_mul(scatter_row_bytes);
        if let Err(error) = memory.reserve(bytes) {
            // Free the scatter window and retry once; a second failure
            // means the group states themselves exceed the budget.
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
            match memory.reserve(bytes) {
                Ok(()) => {}
                Err(_) => return Err(error),
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
            (TwoPassKeySource::TextPair { first, second }, Some(intern)) => {
                two_pass_scatter_string_pair(
                    &current,
                    first,
                    second,
                    lanes,
                    partitions,
                    &mut buckets,
                    intern,
                    memory,
                )?;
            }
            (TwoPassKeySource::Int { column, .. }, _) => {
                two_pass_scatter_batch(&current, column, lanes, partitions, &mut buckets)?;
            }
            (TwoPassKeySource::DateParts { parts }, _) => {
                two_pass_scatter_date_parts(&current, parts, lanes, partitions, &mut buckets)?;
            }
            _ => unreachable!("intern presence follows the key source"),
        }
        drop(current);
        if bucket_reserved >= flush_bytes || (scan_floor > 0 && memory.remaining() < scan_floor) {
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
        }
        batch = input.next_batch(memory)?;
    }
    drain_two_pass_window(
        &mut window,
        keys,
        lanes,
        aggregates,
        partitions,
        &mut maps,
        &mut dense,
        intern.as_ref().map_or(0, |intern| intern.values.len()),
        memory,
        &mut group_reserved,
        &mut window_reserved,
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
    if let Some(slots) = dense.take() {
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            &mut maps,
            memory,
            &mut group_reserved,
        )?;
    }
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        let groups: usize = maps.iter().map(HashMap::len).sum();
        eprintln!(
            "[agg] streaming two-pass: {groups} groups, {} flushes",
            flushes + 1
        );
    }

    // Finalize each partition in parallel; ORDER BY above owns ordering.
    let finalized = maps
        .into_par_iter()
        .map(|map| -> Result<(Vec<Vec<Value>>, usize), ExecError> {
            let interned =
                |id: u64,
                 labels: Option<&std::sync::Arc<Vec<String>>>,
                 members: Option<&std::sync::Arc<Vec<String>>>| {
                    let text = intern
                        .as_ref()
                        .expect("text keys carry an intern table")
                        .values[usize::try_from(id).expect("intern id fits usize")]
                    .clone();
                    // An ENUM group key rebuilds with its declaration index and
                    // a SET key with its member bitmask, so the ORDER BY above
                    // sorts by MySQL's rule; anything undeclared stays a plain
                    // string.
                    let ordinal = if let Some(labels) = labels {
                        // Empty text never matches a slot: a label table
                        // reconstructed from Value::Enum indices keeps its
                        // unseen slots as empty strings, and the empty SET
                        // ("", mask 0) must not inherit a gap's ordinal.
                        (!text.is_empty())
                            .then(|| {
                                labels
                                    .iter()
                                    .position(|declared| declared == &text)
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
                        || Value::Utf8(text.clone()),
                        |index| Value::Enum {
                            index,
                            label: text.clone(),
                        },
                    )
                };
            let mut rows = Vec::with_capacity(map.len());
            let mut payload = 0_usize;
            for ((bits, null), states) in map {
                let mut row = Vec::with_capacity(2 + states.len());
                match keys {
                    TwoPassKeySource::Int { group_type, .. } => {
                        row.push(two_pass_key_value(bits, null, group_type));
                    }
                    TwoPassKeySource::Text { .. } => {
                        row.push(if null {
                            Value::Null
                        } else {
                            interned(
                                bits,
                                key_enum_labels[0].as_ref(),
                                key_set_members[0].as_ref(),
                            )
                        });
                    }
                    TwoPassKeySource::TextPair { .. } => {
                        for (slot, id) in [bits >> 32, bits & 0xFFFF_FFFF].into_iter().enumerate() {
                            row.push(if id == 0 {
                                Value::Null
                            } else {
                                interned(
                                    id - 1,
                                    key_enum_labels[slot].as_ref(),
                                    key_set_members[slot].as_ref(),
                                )
                            });
                        }
                    }
                    TwoPassKeySource::DateParts { parts } => {
                        let count = parts.iter().flatten().count();
                        for index in 0..count {
                            let shift = 20 * (count - 1 - index);
                            let id = (bits >> shift) & 0xF_FFFF;
                            row.push(if id == 0 {
                                Value::Null
                            } else {
                                // Date parts are signed (Int64), like the
                                // scalar and units paths that feed them.
                                Value::Int64(i64::try_from(id - 1).unwrap_or(i64::MAX))
                            });
                        }
                    }
                }
                for state in states {
                    row.push(state.finish(memory)?);
                }
                let bytes = estimated_row_payload_bytes(&row);
                memory.reserve(bytes)?;
                payload = payload.saturating_add(bytes);
                rows.push(row);
            }
            Ok((rows, payload))
        })
        .collect::<Result<Vec<_>, _>>();
    memory.release(group_reserved);
    let finalized = finalized?;
    let mut rows = Vec::new();
    for (partition_rows, _) in finalized {
        rows.extend(partition_rows);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

/// Pass 1 for one batch: extract (key bits, lane bits, null mask) per
/// selected row into the partition buckets. Reservation is the caller\'s.
fn two_pass_scatter_batch(
    batch: &RecordBatch,
    group_column: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let group_values = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    for row in batch.selection().selected_rows() {
        let value = group_values.value(row).ok_or(ExecError::InvalidBatch(
            "grouping row is outside the input batch",
        ))?;
        let (key_bits, key_null) = two_pass_key_bits(value)
            .ok_or(ExecError::InvalidBatch("two-pass key is not scalar"))?;
        scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
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
        let translation = dict_values
            .iter()
            .map(|value| intern.intern(value.as_bytes(), memory))
            .collect::<Result<Vec<_>, _>>()?;
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                translation[usize::try_from(codes[row]).expect("dict code fits usize")]
            };
            scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
        }
    } else {
        let (views, heap) = (strings.views(), strings.heap());
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                views[row].with_bytes(heap, |bytes| intern.intern(bytes, memory))?
            };
            scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
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
            translation: dict_values
                .iter()
                .map(|value| intern.intern(value.as_bytes(), memory))
                .collect::<Result<Vec<_>, _>>()?,
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
        let Some((crate::batch::TypedValues::Utf8(strings), _)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((_, dict_values)) = strings.dictionary() else {
            return Ok(None);
        };
        prepared.push(
            dict_values
                .iter()
                .map(|value| intern.intern(value.as_bytes(), memory))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(Some(prepared))
}

/// Scatters string keys from prepared (read-only) translations: no intern
/// access, so windows of batches scatter in parallel.
fn two_pass_scatter_text_prepared(
    batch: &RecordBatch,
    columns: &[usize],
    translations: &[Vec<u64>],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
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
    let pair = readers.len() == 2;
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            let id = if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                let interned = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                if pair { interned + 1 } else { interned }
            } else {
                if !pair {
                    key_null = true;
                }
                0
            };
            key_bits = if pair { (key_bits << 32) | id } else { id };
        }
        scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
    }
    Ok(())
}

/// Two string group columns: ids pack as `(a+1) << 32 | (b+1)` with 0 as
/// the per-column NULL sentinel (mask bit 7 stays clear).
#[allow(clippy::too_many_arguments)]
fn two_pass_scatter_string_pair(
    batch: &RecordBatch,
    first: usize,
    second: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let (first_reader, first_validity) = string_key_reader(batch, first, intern, memory)?;
    let (second_reader, second_validity) = string_key_reader(batch, second, intern, memory)?;
    for row in batch.selection().selected_rows() {
        let first_id = if first_validity.is_valid(row) {
            first_reader.read(row, intern, memory)? + 1
        } else {
            0
        };
        let second_id = if second_validity.is_valid(row) {
            second_reader.read(row, intern, memory)? + 1
        } else {
            0
        };
        let key_bits = (first_id << 32) | second_id;
        scatter_two_pass_row(batch, row, key_bits, false, lanes, partitions, buckets);
    }
    Ok(())
}

/// Up to two bounded date-part expressions as the group key: values come
/// straight from packed temporal units (no Value cells, no text).
fn two_pass_scatter_date_parts(
    batch: &RecordBatch,
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        for (part, column) in parts.iter().flatten() {
            let id = match crate::expression::evaluate_units_date_part(batch, *column, row, *part) {
                Some(Ok(Value::Int64(value))) => u64::try_from(value).unwrap_or(0) + 1,
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
        scatter_two_pass_row(batch, row, key_bits, false, lanes, partitions, buckets);
    }
    Ok(())
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
        TwoPassLane::DecimalUnits { column, .. } | TwoPassLane::ExtremeDecimal { column, .. } => {
            batch
                .column(*column)
                .and_then(|column| {
                    let (typed, validity) = column.typed()?;
                    validity
                        .is_valid(row)
                        .then(|| typed.units_at(row))
                        .flatten()
                })
                .and_then(|units| i64::try_from(units).ok())
                .map(|units| u64::from_ne_bytes(units.to_ne_bytes()))
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

fn scatter_two_pass_row(
    batch: &RecordBatch,
    row: usize,
    key_bits: u64,
    key_null: bool,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) {
    let lane_count = lanes.len();
    {
        let mut mask = u8::from(key_null) << 7;
        let bucket = &mut buckets[usize::try_from(
            crate::batch::mix64(key_bits ^ u64::from(key_null)) % partitions as u64,
        )
        .expect("partition index fits usize")];
        let lane_base = bucket.lanes.len();
        bucket.lanes.resize(lane_base + lane_count, 0);
        for (lane_index, lane) in lanes.iter().enumerate() {
            match two_pass_lane_bits(batch, row, lane) {
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
    /// The plan's collation. Held here because the table IS the equivalence
    /// relation - two spellings share an id exactly when the collation says
    /// they are equal - so it cannot be decided per call.
    collation: Collation,
}

impl StringIntern {
    fn intern(&mut self, bytes: &[u8], memory: &MemoryTracker) -> Result<u64, ExecError> {
        // Group keys unify through the same sort key used by comparison,
        // hashing, DISTINCT, and joins. Keep the first-seen spelling
        // separately for MySQL-compatible GROUP BY output.
        let value = std::str::from_utf8(bytes)
            .map_err(|_| ExecError::InvalidBatch("string group key is not UTF-8"))?;
        let folded = normalized_collation_text(value, self.collation).into_bytes();
        if let Some(id) = self.index.get(&folded) {
            return Ok(*id);
        }
        let id = u64::try_from(self.values.len()).expect("intern ids fit u64");
        memory.reserve(
            bytes
                .len()
                .saturating_add(folded.len())
                .saturating_add(HASH_ENTRY_OVERHEAD)
                .saturating_add(size_of::<String>() + size_of::<u64>()),
        )?;
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
#[allow(clippy::too_many_arguments)]
fn drain_two_pass_window(
    window: &mut Vec<(RecordBatch, Vec<Vec<u64>>)>,
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    dense: &mut Option<DenseGroupSlots>,
    intern_len: usize,
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    window_reserved: &mut usize,
) -> Result<(), ExecError> {
    if window.is_empty() {
        return Ok(());
    }
    if let Some(slots) = dense.as_mut() {
        if dense_in_bounds(keys, intern_len) {
            // A date-part key indexes its slots directly, and can discover
            // mid-fold that a value has no slot; the text keys cannot.
            if let TwoPassKeySource::DateParts { parts } = keys {
                if dense_date_parts_window(window, parts, lanes, aggregates, slots, memory)? {
                    window.clear();
                    memory.release(*window_reserved);
                    *window_reserved = 0;
                    return Ok(());
                }
                // A year outside the table's window: fall through, unify what
                // the slots hold and finish on the scatter path.
            } else {
                dense_text_window(window, keys, lanes, aggregates, slots, memory)?;
                window.clear();
                memory.release(*window_reserved);
                *window_reserved = 0;
                return Ok(());
            }
        }
        // The intern table outgrew the dense domain: unify what the dense
        // slots hold into the partition maps and continue on the classic
        // scatter path for the rest of the stream.
        let slots = dense.take().expect("checked above");
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            maps,
            memory,
            group_reserved,
        )?;
    }
    let mut sets = window
        .par_iter()
        .map(
            |(batch, translations)| -> Result<Vec<TwoPassBucket>, ExecError> {
                let mut buckets: Vec<TwoPassBucket> =
                    (0..partitions).map(|_| TwoPassBucket::default()).collect();
                match keys {
                    TwoPassKeySource::Int { column, .. } => {
                        two_pass_scatter_batch(batch, column, lanes, partitions, &mut buckets)?;
                    }
                    TwoPassKeySource::DateParts { parts } => {
                        two_pass_scatter_date_parts(batch, parts, lanes, partitions, &mut buckets)?;
                    }
                    TwoPassKeySource::Text { column } => {
                        two_pass_scatter_text_prepared(
                            batch,
                            &[column],
                            translations,
                            lanes,
                            partitions,
                            &mut buckets,
                        )?;
                    }
                    TwoPassKeySource::TextPair { first, second } => {
                        two_pass_scatter_text_prepared(
                            batch,
                            &[first, second],
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
        TwoPassLane::CountStar => state.update(aggregate, &Value::UInt64(1), memory),
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
/// Every other supported part is under 60, so `(value + 1)` fits here.
const DENSE_DATE_SMALL_SIDE: usize = 64;
/// Largest date-part table built. Merging walks every slot whether or not
/// it is occupied, so the table stays small enough that walking it costs
/// less than the buckets it replaces.
const DENSE_DATE_SLOT_CAP: usize = 1 << 16;

/// Ordinals one part can take, or `None` for a part this table cannot hold.
const fn dense_date_side(part: DatePart) -> Option<usize> {
    match part {
        DatePart::Year => Some(DENSE_DATE_YEAR_SIDE),
        DatePart::Month | DatePart::Day | DatePart::Hour | DatePart::Minute | DatePart::Second => {
            Some(DENSE_DATE_SMALL_SIDE)
        }
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

/// Single text column: intern ids 0..=1023 map to slots 1..=1024.
const DENSE_TEXT_CAP: usize = 1024;
/// Text pair: side ids are (intern id + 1) with 0 as NULL, kept < 65.
const DENSE_PAIR_SIDE: usize = 65;

fn dense_slot_count(keys: TwoPassKeySource) -> Option<usize> {
    match keys {
        TwoPassKeySource::Text { .. } => Some(DENSE_TEXT_CAP + 1),
        TwoPassKeySource::TextPair { .. } => Some(DENSE_PAIR_SIDE * DENSE_PAIR_SIDE),
        TwoPassKeySource::DateParts { parts } => dense_date_slot_count(parts),
        TwoPassKeySource::Int { .. } => None,
    }
}

/// Whether every sentinel the current intern table can produce still fits
/// the dense slots.
fn dense_in_bounds(keys: TwoPassKeySource, intern_len: usize) -> bool {
    match keys {
        TwoPassKeySource::Text { .. } => intern_len <= DENSE_TEXT_CAP,
        TwoPassKeySource::TextPair { .. } => intern_len + 1 < DENSE_PAIR_SIDE,
        // Date-part domains are checked per row instead: the table covers a
        // bounded window of years and the fold abandons it when a value
        // falls outside, which no table-wide check can predict.
        TwoPassKeySource::DateParts { .. } => true,
        TwoPassKeySource::Int { .. } => false,
    }
}

fn dense_slot_index(keys: TwoPassKeySource, key_bits: u64, key_null: bool) -> usize {
    match keys {
        TwoPassKeySource::Text { .. } => {
            if key_null {
                0
            } else {
                usize::try_from(key_bits).expect("intern id fits usize") + 1
            }
        }
        TwoPassKeySource::TextPair { .. } => {
            let first = usize::try_from(key_bits >> 32).expect("side id fits usize");
            let second = usize::try_from(key_bits & 0xFFFF_FFFF).expect("side id fits usize");
            first * DENSE_PAIR_SIDE + second
        }
        // The date-part fold indexes its own slots, because unlike text it
        // can fail: a year outside the table's window has no slot at all.
        TwoPassKeySource::DateParts { .. } => unreachable!("date parts index their own slots"),
        TwoPassKeySource::Int { .. } => unreachable!("int keys have no dense table"),
    }
}

/// Inverse of [`dense_slot_index`]: the map key the classic path would use.
fn dense_slot_sentinel(keys: TwoPassKeySource, index: usize) -> (u64, bool) {
    match keys {
        TwoPassKeySource::Text { .. } => {
            if index == 0 {
                (0, true)
            } else {
                (
                    u64::try_from(index - 1).expect("slot index fits u64"),
                    false,
                )
            }
        }
        TwoPassKeySource::TextPair { .. } => {
            let first = u64::try_from(index / DENSE_PAIR_SIDE).expect("slot index fits u64");
            let second = u64::try_from(index % DENSE_PAIR_SIDE).expect("slot index fits u64");
            ((first << 32) | second, false)
        }
        TwoPassKeySource::DateParts { parts } => (dense_date_key(parts, index), false),
        TwoPassKeySource::Int { .. } => unreachable!("int keys have no dense table"),
    }
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
    let pair = readers.len() == 2;
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            let id = if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                let interned = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                if pair { interned + 1 } else { interned }
            } else {
                if !pair {
                    key_null = true;
                }
                0
            };
            key_bits = if pair { (key_bits << 32) | id } else { id };
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

/// Folds one window into the dense slots of a text or text-pair key.
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
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let columns: &[usize] = match keys {
        TwoPassKeySource::Text { column } => &[column],
        TwoPassKeySource::TextPair { first, second } => &[first, second],
        _ => unreachable!("dense slots are text-keyed"),
    };
    let slot_count = slots.len();
    let folded = window
        .par_iter()
        .try_fold(
            || vec![None; slot_count],
            |mut acc, (batch, translations)| {
                two_pass_dense_batch(
                    batch,
                    keys,
                    columns,
                    translations,
                    lanes,
                    aggregates,
                    &mut acc,
                    memory,
                )?;
                Ok(acc)
            },
        )
        .try_reduce(
            || vec![None; slot_count],
            |left, right| merge_dense_slots(left, right, aggregates, memory),
        )?;
    *slots = merge_dense_slots(std::mem::take(slots), folded, aggregates, memory)?;
    Ok(())
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
    let folded = window
        .par_iter()
        .try_fold(
            || vec![None; slot_count],
            |mut acc, (batch, _)| {
                two_pass_dense_date_parts_batch(batch, parts, lanes, aggregates, &mut acc, memory)?;
                Ok(acc)
            },
        )
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
                Some(Ok(Value::Int64(value))) => u64::try_from(value).unwrap_or(0) + 1,
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
#[allow(clippy::too_many_arguments)]
fn fold_dense_into_maps(
    slots: DenseGroupSlots,
    keys: TwoPassKeySource,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    for (index, slot) in slots.into_iter().enumerate() {
        let Some(states) = slot else { continue };
        let (bits, null) = dense_slot_sentinel(keys, index);
        let partition =
            usize::try_from(crate::batch::mix64(bits ^ u64::from(null)) % partitions as u64)
                .expect("partition index fits usize");
        match maps[partition].entry((bits, null)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                memory.reserve(per_group_bytes)?;
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
    Ok(())
}

/// Pass 2 over several scatter outputs at once (one per parallel scatter
/// worker): each partition folds its bucket from EVERY set, so parallel
/// pass 1 needs no cross-worker merging (e13's shape, bounded windows).
#[allow(clippy::too_many_lines)]
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
    let sets_ref: &[Vec<TwoPassBucket>] = sets;
    let added = maps
        .par_iter_mut()
        .enumerate()
        .map(|(partition, map)| -> Result<usize, ExecError> {
            let before = map.len();
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
        .collect::<Result<Vec<_>, _>>()?;
    for set in sets.iter_mut() {
        for bucket in set.iter_mut() {
            bucket.keys.clear();
            bucket.masks.clear();
            bucket.lanes.clear();
        }
    }
    *group_reserved = group_reserved.saturating_add(added.into_iter().sum());
    Ok(())
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
