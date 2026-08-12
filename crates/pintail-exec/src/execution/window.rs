//! Window function compilation and evaluation.

use std::cmp::Ordering;

use crate::collation::Collation;

use pintail_sql::{
    BoundColumn, BoundExpr, BoundExprKind, BoundOrderKey, BoundWindow, WindowFunction,
};
use pintail_types::{DataType, Value};

use super::{
    AggregateState, CompiledAggregate, ExecError, MaterializedRows, MemoryTracker, PullOperator,
    compare_decimal_text, compare_sort_values, estimated_row_payload_bytes,
};
use crate::expression::CompiledExpr;

/// One window computation compiled against its input's column layout.
pub(super) struct CompiledWindow {
    function: CompiledWindowFunction,
    partition: Vec<CompiledExpr>,
    /// Order keys with `(ascending, nulls_first, decimal)`.
    order: Vec<(CompiledExpr, bool, bool, bool)>,
    /// Explicit `ROWS`/`RANGE` frame; `None` keeps `MySQL`'s default frame.
    frame: Option<pintail_sql::BoundWindowFrame>,
}

enum CompiledWindowFunction {
    RowNumber,
    Rank,
    DenseRank,
    /// The aggregate plus its compiled argument; `COUNT(*)` compiles a
    /// constant 1 so every row counts.
    Aggregate(CompiledAggregate, CompiledExpr),
    /// `LAG`/`LEAD` with its compiled value expression; the default is
    /// compiled alongside so the edge substitution is a plain lookup.
    Offset {
        lead: bool,
        offset: u64,
        argument: CompiledExpr,
        default: Option<CompiledExpr>,
    },
    NTile(u64),
    Extreme {
        last: bool,
        argument: CompiledExpr,
    },
}

impl CompiledWindow {
    pub(super) fn compile(
        window: &BoundWindow,
        columns: &[BoundColumn],
        collation: Collation,
    ) -> Result<Self, ExecError> {
        let function = match &window.function {
            WindowFunction::Offset {
                lead,
                expr,
                offset,
                default,
            } => CompiledWindowFunction::Offset {
                lead: *lead,
                offset: *offset,
                argument: CompiledExpr::compile(expr, columns, collation)?,
                default: default
                    .as_ref()
                    .map(|value| CompiledExpr::compile(value, columns, collation))
                    .transpose()?,
            },
            WindowFunction::NTile(buckets) => CompiledWindowFunction::NTile(*buckets),
            WindowFunction::Extreme { last, expr } => CompiledWindowFunction::Extreme {
                last: *last,
                argument: CompiledExpr::compile(expr, columns, collation)?,
            },
            WindowFunction::RowNumber => CompiledWindowFunction::RowNumber,
            WindowFunction::Rank => CompiledWindowFunction::Rank,
            WindowFunction::DenseRank => CompiledWindowFunction::DenseRank,
            WindowFunction::Aggregate(aggregate) => {
                let argument = match &aggregate.expr {
                    Some(expr) => CompiledExpr::compile(expr, columns, collation)?,
                    None => CompiledExpr::compile(
                        &BoundExpr {
                            kind: BoundExprKind::Literal(Value::Int64(1)),
                            data_type: Some(DataType::Int64),
                            nullable: false,
                        },
                        columns,
                        collation,
                    )?,
                };
                CompiledWindowFunction::Aggregate(
                    CompiledAggregate::compile(aggregate, columns, collation)?,
                    argument,
                )
            }
        };
        Ok(Self {
            function,
            partition: window
                .partition_by
                .iter()
                .map(|expr| CompiledExpr::compile(expr, columns, collation))
                .collect::<Result<Vec<_>, _>>()?,
            order: window
                .order_by
                .iter()
                .map(|key| {
                    Ok::<_, ExecError>((
                        CompiledExpr::compile(&key.expr, columns, collation)?,
                        key.ascending,
                        key.nulls_first,
                        matches!(key.expr.data_type, Some(DataType::Decimal { .. })),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
            frame: window.frame,
        })
    }
}

/// Materializes the input, computes every window over its partitions, and
/// returns rows with the window results appended as trailing columns.
pub(super) fn build_window(
    input: &mut PullOperator,
    windows: &[CompiledWindow],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<MaterializedRows, ExecError> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut keys: Vec<Vec<Vec<Value>>> = windows.iter().map(|_| Vec::new()).collect();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            memory.ensure_transient(batch_bytes)?;
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "window row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            memory.reserve(estimated_row_payload_bytes(&values))?;
            for (index, window) in windows.iter().enumerate() {
                let mut row_keys =
                    Vec::with_capacity(window.partition.len() + window.order.len() + 1);
                for expr in &window.partition {
                    row_keys.push(expr.evaluate(&batch, row)?);
                }
                for (expr, _, _, _) in &window.order {
                    row_keys.push(expr.evaluate(&batch, row)?);
                }
                match &window.function {
                    CompiledWindowFunction::Aggregate(_, argument)
                    | CompiledWindowFunction::Extreme { argument, .. } => {
                        row_keys.push(argument.evaluate(&batch, row)?);
                    }
                    // LAG/LEAD carry the value and, when present, the edge
                    // default, so both are read positionally later.
                    CompiledWindowFunction::Offset {
                        argument, default, ..
                    } => {
                        row_keys.push(argument.evaluate(&batch, row)?);
                        if let Some(default) = default {
                            row_keys.push(default.evaluate(&batch, row)?);
                        }
                    }
                    CompiledWindowFunction::RowNumber
                    | CompiledWindowFunction::Rank
                    | CompiledWindowFunction::DenseRank
                    | CompiledWindowFunction::NTile(_) => {}
                }
                memory.reserve(estimated_row_payload_bytes(&row_keys))?;
                keys[index].push(row_keys);
            }
            rows.push(values);
        }
    }
    let row_count = rows.len();
    for (index, window) in windows.iter().enumerate() {
        let result = compute_window_column(window, &keys[index], row_count, memory, collation)?;
        for (row, value) in rows.iter_mut().zip(&result) {
            memory.reserve(value.heap_bytes().saturating_add(size_of::<Value>()))?;
            row.push(value.clone());
        }
    }
    Ok(MaterializedRows { rows, position: 0 })
}

enum NumericRangeTarget {
    NegativeInfinity,
    ExactDecimal { units: i128, scale: u8 },
    Value(Value),
    PositiveInfinity,
}

#[allow(clippy::cast_precision_loss)] // Float ordering keys are approximate by definition.
fn numeric_range_target(
    current: &Value,
    offset_units: i128,
    offset_scale: u8,
    add: bool,
    decimal: bool,
) -> Result<NumericRangeTarget, ExecError> {
    let overflow = || {
        if add {
            NumericRangeTarget::PositiveInfinity
        } else {
            NumericRangeTarget::NegativeInfinity
        }
    };
    let target = match current {
        Value::Int64(value) => 10_i128
            .checked_pow(u32::from(offset_scale))
            .and_then(|factor| i128::from(*value).checked_mul(factor))
            .and_then(|value| {
                if add {
                    value.checked_add(offset_units)
                } else {
                    value.checked_sub(offset_units)
                }
            })
            .map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale: offset_scale,
            }),
        Value::UInt64(value) => 10_i128
            .checked_pow(u32::from(offset_scale))
            .and_then(|factor| i128::from(*value).checked_mul(factor))
            .and_then(|value| {
                if add {
                    value.checked_add(offset_units)
                } else {
                    value.checked_sub(offset_units)
                }
            })
            .map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale: offset_scale,
            }),
        Value::Float64(value) => {
            let offset = offset_units as f64 / 10_f64.powi(i32::from(offset_scale));
            let value = if add {
                value.get() + offset
            } else {
                value.get() - offset
            };
            if value.is_finite() {
                NumericRangeTarget::Value(Value::float64(value))
            } else {
                overflow()
            }
        }
        Value::Utf8(text) if decimal => {
            let current_scale = text
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len());
            let current_scale =
                u8::try_from(current_scale).map_err(|_| ExecError::NumericOverflow)?;
            let scale = current_scale.max(offset_scale);
            let units = pintail_types::parse_decimal_scaled(text, current_scale)
                .ok_or(ExecError::InvalidExpressionType)?;
            let rescale = |units: i128, from: u8| {
                10_i128
                    .checked_pow(u32::from(scale - from))
                    .and_then(|factor| units.checked_mul(factor))
            };
            let offset = rescale(units, current_scale)
                .zip(rescale(offset_units, offset_scale))
                .and_then(|(units, offset)| {
                    if add {
                        units.checked_add(offset)
                    } else {
                        units.checked_sub(offset)
                    }
                });
            offset.map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale,
            })
        }
        _ => return Err(ExecError::InvalidExpressionType),
    };
    Ok(target)
}

// The frame a RANGE bound resolves to depends on the window, the sorted keys,
// the row it is measured from, the offset, its direction, and how text compares.
// Bundling them into a struct would move the same values behind one more name.
#[allow(clippy::too_many_arguments)]
fn numeric_range_bound(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    current: usize,
    offset: (i128, u8),
    preceding: bool,
    upper: bool,
    collation: Collation,
) -> Result<usize, ExecError> {
    let Some((_, ascending, _, decimal)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let current_value = &keys[partition[current]][key_position];
    let target = numeric_range_target(
        current_value,
        offset.0,
        offset.1,
        if *ascending { !preceding } else { preceding },
        *decimal,
    )?;
    range_bound_for_target(window, keys, partition, &target, upper, collation)
}

// The frame a RANGE bound resolves to depends on the window, the sorted keys,
// the row it is measured from, the offset, its direction, and how text compares.
// Bundling them into a struct would move the same values behind one more name.
#[allow(clippy::too_many_arguments)]
fn temporal_range_bound(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    current: usize,
    interval: (u64, pintail_sql::IntervalUnit),
    preceding: bool,
    upper: bool,
    collation: Collation,
) -> Result<usize, ExecError> {
    let Some((_, ascending, _, _)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let current_value = &keys[partition[current]][key_position];
    let target = crate::expression::shift_temporal_value(
        current_value,
        interval.0,
        interval.1,
        if *ascending { !preceding } else { preceding },
    )?;
    range_bound_for_target(
        window,
        keys,
        partition,
        &NumericRangeTarget::Value(target),
        upper,
        collation,
    )
}

// The frame a RANGE bound resolves to depends on the window, the sorted keys,
// the row it is measured from, the offset, its direction, and how text compares.
// Bundling them into a struct would move the same values behind one more name.
#[allow(clippy::too_many_arguments)]
fn range_bound_for_target(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    target: &NumericRangeTarget,
    upper: bool,
    collation: Collation,
) -> Result<usize, ExecError> {
    let Some((_, ascending, nulls_first, decimal)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let compare = |candidate: &Value| {
        if matches!(candidate, Value::Null) {
            return if *nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let natural = match &target {
            NumericRangeTarget::NegativeInfinity => Ordering::Greater,
            NumericRangeTarget::PositiveInfinity => Ordering::Less,
            NumericRangeTarget::ExactDecimal { units, scale } => {
                let candidate = match candidate {
                    Value::Boolean(value) => i8::from(*value).to_string(),
                    Value::Int64(value) => value.to_string(),
                    Value::UInt64(value) => value.to_string(),
                    Value::Utf8(value) if *decimal => value.clone(),
                    _ => return Ordering::Equal,
                };
                let ordering = compare_decimal_text(
                    &candidate,
                    &pintail_types::format_decimal_scaled(*units, *scale),
                )
                .unwrap_or(Ordering::Equal);
                if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
            NumericRangeTarget::Value(target) => compare_sort_values(
                candidate,
                target,
                BoundOrderKey {
                    index: 0,
                    ascending: *ascending,
                    nulls_first: *nulls_first,
                    decimal: *decimal,
                },
                collation,
            ),
        };
        match target {
            NumericRangeTarget::Value(_) | NumericRangeTarget::ExactDecimal { .. } => natural,
            _ if *ascending => natural,
            _ => natural.reverse(),
        }
    };
    let mut low = 0;
    let mut high = partition.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let ordering = compare(&keys[partition[middle]][key_position]);
        let before_boundary = if upper {
            ordering != Ordering::Greater
        } else {
            ordering == Ordering::Less
        };
        if before_boundary {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// Computes one window's value per row: sorts a permutation by
/// (partition, order) keys, then walks each partition assigning ranks or
/// aggregate frames (whole partition without ORDER BY; running frame
/// including the current row's peers with it — `MySQL`'s default frames).
#[allow(clippy::too_many_lines)]
fn compute_window_column(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    row_count: usize,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Value>, ExecError> {
    let partition_len = window.partition.len();
    let order_key = |ascending: bool, nulls_first: bool, decimal: bool| BoundOrderKey {
        index: 0,
        ascending,
        nulls_first,
        decimal,
    };
    let compare_rows = |left: usize, right: usize| {
        let left_keys = &keys[left];
        let right_keys = &keys[right];
        for position in 0..partition_len {
            let ordering = compare_sort_values(
                &left_keys[position],
                &right_keys[position],
                order_key(true, true, false),
                collation,
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        for (position, (_, ascending, nulls_first, decimal)) in window.order.iter().enumerate() {
            let ordering = compare_sort_values(
                &left_keys[partition_len + position],
                &right_keys[partition_len + position],
                order_key(*ascending, *nulls_first, *decimal),
                collation,
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    };
    let same_partition = |left: usize, right: usize| {
        (0..partition_len).all(|position| {
            compare_sort_values(
                &keys[left][position],
                &keys[right][position],
                order_key(true, true, false),
                collation,
            ) == Ordering::Equal
        })
    };
    let same_peers = |left: usize, right: usize| {
        window.order.iter().enumerate().all(|(position, key)| {
            compare_sort_values(
                &keys[left][partition_len + position],
                &keys[right][partition_len + position],
                order_key(key.1, key.2, key.3),
                collation,
            ) == Ordering::Equal
        })
    };

    let mut order = (0..row_count).collect::<Vec<_>>();
    memory.reserve(row_count.saturating_mul(size_of::<usize>()))?;
    order.sort_by(|left, right| compare_rows(*left, *right));

    let mut results = vec![Value::Null; row_count];
    let mut start = 0;
    while start < row_count {
        let mut end = start + 1;
        while end < row_count && same_partition(order[start], order[end]) {
            end += 1;
        }
        let partition = &order[start..end];
        let peer_start = |from: usize| {
            let mut first = from;
            while first > 0 && same_peers(partition[first - 1], partition[from]) {
                first -= 1;
            }
            first
        };
        let peer_end = |from: usize| {
            let mut last = from + 1;
            while last < partition.len() && same_peers(partition[from], partition[last]) {
                last += 1;
            }
            last
        };
        let frame_extent = |frame: pintail_sql::BoundWindowFrame,
                            index: usize|
         -> Result<(usize, usize), ExecError> {
            use pintail_sql::{BoundFrameBound as Edge, BoundFrameOffset as Offset};
            let len = partition.len();
            let row_offset = |offset: Offset| match offset {
                Offset::Rows(value) => Ok(usize::try_from(value).unwrap_or(usize::MAX)),
                _ => Err(ExecError::InvalidPhysicalPlan(
                    "non-row offset reached a ROWS frame",
                )),
            };
            let range_bound = |offset: Offset, preceding: bool, upper: bool| match offset {
                Offset::Numeric { units, scale } => numeric_range_bound(
                    window,
                    keys,
                    partition,
                    index,
                    (units, scale),
                    preceding,
                    upper,
                    collation,
                ),
                Offset::Interval { value, unit } => temporal_range_bound(
                    window,
                    keys,
                    partition,
                    index,
                    (value, unit),
                    preceding,
                    upper,
                    collation,
                ),
                Offset::Rows(_) => Err(ExecError::InvalidPhysicalPlan(
                    "row offset reached a RANGE frame",
                )),
            };
            let current_is_null = window
                .order
                .first()
                .is_some_and(|_| matches!(keys[partition[index]][partition_len], Value::Null));
            let start = match frame.start {
                Edge::UnboundedPreceding => 0,
                Edge::Preceding(_) | Edge::Following(_) if frame.range && current_is_null => {
                    peer_start(index)
                }
                Edge::Preceding(offset) if frame.range => range_bound(offset, true, false)?,
                Edge::Following(offset) if frame.range => range_bound(offset, false, false)?,
                Edge::Preceding(offset) => index.saturating_sub(row_offset(offset)?),
                Edge::CurrentRow if frame.range => peer_start(index),
                Edge::CurrentRow => index,
                Edge::Following(offset) => index.saturating_add(row_offset(offset)?).min(len),
                Edge::UnboundedFollowing => len,
            };
            let end = match frame.end {
                Edge::UnboundedPreceding => 0,
                Edge::Preceding(_) | Edge::Following(_) if frame.range && current_is_null => {
                    peer_end(index)
                }
                Edge::Preceding(offset) if frame.range => range_bound(offset, true, true)?,
                Edge::Following(offset) if frame.range => range_bound(offset, false, true)?,
                Edge::Preceding(offset) => index
                    .checked_sub(row_offset(offset)?)
                    .map_or(0, |row| row + 1),
                Edge::CurrentRow if frame.range => peer_end(index),
                Edge::CurrentRow => index + 1,
                Edge::Following(offset) => index
                    .saturating_add(row_offset(offset)?)
                    .saturating_add(1)
                    .min(len),
                Edge::UnboundedFollowing => len,
            };
            Ok((start, end))
        };
        match &window.function {
            CompiledWindowFunction::RowNumber
            | CompiledWindowFunction::Rank
            | CompiledWindowFunction::DenseRank => {
                let mut rank = 0_u64;
                let mut dense = 0_u64;
                for (position, row) in partition.iter().enumerate() {
                    let number = u64::try_from(position + 1).unwrap_or(u64::MAX);
                    if position == 0 || !same_peers(partition[position - 1], *row) {
                        rank = number;
                        dense += 1;
                    }
                    results[*row] = Value::UInt64(match window.function {
                        CompiledWindowFunction::RowNumber => number,
                        CompiledWindowFunction::Rank => rank,
                        _ => dense,
                    });
                }
            }
            CompiledWindowFunction::Offset {
                lead,
                offset,
                default,
                ..
            } => {
                let value_position = partition_len + window.order.len();
                let offset = usize::try_from(*offset).unwrap_or(usize::MAX);
                for (index, row) in partition.iter().enumerate() {
                    let source = if *lead {
                        index.checked_add(offset)
                    } else {
                        index.checked_sub(offset)
                    };
                    let value = match source.filter(|source| *source < partition.len()) {
                        Some(source) => keys[partition[source]][value_position].clone(),
                        // Past the partition edge MySQL substitutes the
                        // default, evaluated on the current row, and NULL
                        // when none was given.
                        None if default.is_some() => keys[*row][value_position + 1].clone(),
                        None => Value::Null,
                    };
                    memory.reserve(value.heap_bytes())?;
                    results[*row] = value;
                }
            }
            CompiledWindowFunction::NTile(buckets) => {
                // MySQL gives the larger buckets to the earlier positions:
                // the first (len % buckets) buckets take one extra row.
                // More buckets than rows means every row is its own bucket
                // and the rest are empty; capping at the row count keeps
                // NTILE(18446744073709551615) from walking 2^64 of them.
                let buckets = usize::try_from(*buckets)
                    .unwrap_or(usize::MAX)
                    .max(1)
                    .min(partition.len().max(1));
                let base = partition.len() / buckets;
                let wide = partition.len() % buckets;
                let mut assigned = 0;
                for bucket in 0..buckets {
                    let size = base + usize::from(bucket < wide);
                    for row in partition.iter().skip(assigned).take(size) {
                        results[*row] = Value::UInt64(bucket as u64 + 1);
                    }
                    assigned += size;
                }
            }
            CompiledWindowFunction::Extreme { last, .. } => {
                let value_position = partition_len + window.order.len();
                if let Some(frame) = window.frame {
                    // An explicit frame governs which row is read. Binding a
                    // frame and then ignoring it would answer the default
                    // frame's question under the caller's syntax.
                    for index in 0..partition.len() {
                        let (start, end) = frame_extent(frame, index)?;
                        // An empty frame has no value to read.
                        let value = if start >= end {
                            Value::Null
                        } else {
                            let source = if *last { end - 1 } else { start };
                            keys[partition[source]][value_position].clone()
                        };
                        memory.reserve(value.heap_bytes())?;
                        results[partition[index]] = value;
                    }
                } else if !*last || window.order.is_empty() {
                    // FIRST_VALUE reads the partition's first row; without
                    // ORDER BY the frame is the whole partition, so
                    // LAST_VALUE reads its last.
                    let source = if *last {
                        *partition.last().expect("partitions are non-empty")
                    } else {
                        partition[0]
                    };
                    let value = keys[source][value_position].clone();
                    for row in partition {
                        memory.reserve(value.heap_bytes())?;
                        results[*row] = value.clone();
                    }
                } else {
                    // Under MySQL's default frame LAST_VALUE is the last row
                    // of the CURRENT PEER GROUP, not of the partition. This
                    // surprises people, and matching it is the whole point of
                    // pinning it against the oracle.
                    let mut group_start = 0;
                    while group_start < partition.len() {
                        let mut group_end = group_start + 1;
                        while group_end < partition.len()
                            && same_peers(partition[group_start], partition[group_end])
                        {
                            group_end += 1;
                        }
                        let value = keys[partition[group_end - 1]][value_position].clone();
                        for row in &partition[group_start..group_end] {
                            memory.reserve(value.heap_bytes())?;
                            results[*row] = value.clone();
                        }
                        group_start = group_end;
                    }
                }
            }
            CompiledWindowFunction::Aggregate(aggregate, _) => {
                let argument_position = partition_len + window.order.len();
                if let Some(frame) = window.frame {
                    use pintail_sql::BoundFrameBound as Edge;
                    // A frame anchored at UNBOUNDED PRECEDING accumulates
                    // once across the partition; anything else is a sliding
                    // window recomputed over its own width. MIN/MAX cannot be
                    // un-accumulated, so a bounded start has no cheaper form
                    // without a monotonic deque per aggregate kind.
                    // The incremental path needs the frame end to advance
                    // monotonically, which holds for both ROWS and RANGE when
                    // the start is anchored — peer-group ends are also
                    // non-decreasing across a sorted partition.
                    let running = matches!(frame.start, Edge::UnboundedPreceding);
                    let mut state = AggregateState::new(aggregate);
                    let mut accumulated = 0_usize;
                    for index in 0..partition.len() {
                        // Under RANGE, CURRENT ROW covers the whole peer
                        // group rather than the single row: the frame is
                        // defined over the ordering key's values, and peers
                        // share one value.
                        let (start, end) = frame_extent(frame, index)?;
                        let value = if running {
                            while accumulated < end {
                                state.update(
                                    aggregate,
                                    &keys[partition[accumulated]][argument_position],
                                    memory,
                                )?;
                                accumulated += 1;
                            }
                            state.clone().finish(memory)?
                        } else {
                            let mut framed = AggregateState::new(aggregate);
                            for row in partition.iter().take(end).skip(start) {
                                framed.update(aggregate, &keys[*row][argument_position], memory)?;
                            }
                            framed.finish(memory)?
                        };
                        memory.reserve(value.heap_bytes())?;
                        results[partition[index]] = value;
                    }
                } else if window.order.is_empty() {
                    // Whole-partition frame.
                    let mut state = AggregateState::new(aggregate);
                    for row in partition {
                        state.update(aggregate, &keys[*row][argument_position], memory)?;
                    }
                    let value = state.finish(memory)?;
                    for row in partition {
                        memory.reserve(value.heap_bytes())?;
                        results[*row] = value.clone();
                    }
                } else {
                    // Running frame including the current row's peers.
                    let mut state = AggregateState::new(aggregate);
                    let mut group_start = 0;
                    while group_start < partition.len() {
                        let mut group_end = group_start + 1;
                        while group_end < partition.len()
                            && same_peers(partition[group_start], partition[group_end])
                        {
                            group_end += 1;
                        }
                        for row in &partition[group_start..group_end] {
                            state.update(aggregate, &keys[*row][argument_position], memory)?;
                        }
                        let value = state.clone().finish(memory)?;
                        for row in &partition[group_start..group_end] {
                            memory.reserve(value.heap_bytes())?;
                            results[*row] = value.clone();
                        }
                        group_start = group_end;
                    }
                }
            }
        }
        start = end;
    }
    Ok(results)
}
