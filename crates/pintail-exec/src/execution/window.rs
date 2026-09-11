//! Window function compilation and evaluation.

use std::cmp::Ordering;
use std::collections::VecDeque;

use crate::RecordBatch;
use crate::batch::ColumnVector;
use crate::collation::Collation;

use pintail_sql::{
    AggregateFunction, BoundColumn, BoundExpr, BoundExprKind, BoundOrderKey, BoundWindow,
    WindowFunction,
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
    /// The declared type of each of [`Self::key_exprs`], where one is known.
    key_types: Vec<Option<DataType>>,
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
        let mut key_types = window
            .partition_by
            .iter()
            .map(|expr| expr.data_type)
            .chain(window.order_by.iter().map(|key| key.expr.data_type))
            .collect::<Vec<_>>();
        match &window.function {
            WindowFunction::Offset { expr, default, .. } => {
                key_types.push(expr.data_type);
                if let Some(default) = default {
                    key_types.push(default.data_type);
                }
            }
            WindowFunction::Extreme { expr, .. } => key_types.push(expr.data_type),
            WindowFunction::Aggregate(aggregate) => key_types.push(
                aggregate
                    .expr
                    .as_ref()
                    .map_or(Some(DataType::Int64), |expr| expr.data_type),
            ),
            WindowFunction::RowNumber
            | WindowFunction::Rank
            | WindowFunction::DenseRank
            | WindowFunction::NTile(_) => {}
        }
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
            key_types,
        })
    }

    /// The expressions each row's keys hold, in the order
    /// [`compute_window_column`] reads them: partition, order, then the
    /// function's arguments.
    fn key_exprs(&self) -> Vec<&CompiledExpr> {
        let mut exprs = self
            .partition
            .iter()
            .chain(self.order.iter().map(|(expr, _, _, _)| expr))
            .collect::<Vec<_>>();
        match &self.function {
            CompiledWindowFunction::Aggregate(_, argument)
            | CompiledWindowFunction::Extreme { argument, .. } => exprs.push(argument),
            CompiledWindowFunction::Offset {
                argument, default, ..
            } => {
                exprs.push(argument);
                exprs.extend(default);
            }
            CompiledWindowFunction::RowNumber
            | CompiledWindowFunction::Rank
            | CompiledWindowFunction::DenseRank
            | CompiledWindowFunction::NTile(_) => {}
        }
        exprs
    }

    /// The sort keys ordering this window's rows: its partition keys, then
    /// its order keys, by their positions among [`Self::key_exprs`].
    fn sort_keys(&self) -> Vec<BoundOrderKey> {
        let partition = self.partition.len();
        (0..partition)
            .map(|index| window_order_key(index, true, true, false))
            .chain(self.order.iter().enumerate().map(
                |(index, (_, ascending, nulls_first, decimal))| {
                    window_order_key(partition + index, *ascending, *nulls_first, *decimal)
                },
            ))
            .collect()
    }
}

/// Keeps small inputs on the memory path and stages larger inputs for one
/// partition at a time. An ordinal survives every sort so independent windows
/// append their values to the same input row and preserve encounter-order ties.
pub(super) fn build_window(
    input: &mut PullOperator,
    windows: &[CompiledWindow],
    column_types: &[DataType],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<super::SortedRows, ExecError> {
    let input_types = &column_types[..column_types.len() - windows.len()];
    let mut pending = match over_batches(input, windows, column_types, memory, collation)? {
        Ok(answered) => return Ok(answered),
        Err(pending) => pending.into_iter(),
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut reserved = 0;
    let mut writer = None;
    let mut ordinal = 0_u64;
    while let Some(batch) = match pending.next() {
        Some(batch) => Some(batch),
        None => input.next_batch(memory)?,
    } {
        for row in batch.selection().selected_rows() {
            let values = super::batch_row(&batch, row)?;
            let bytes = estimated_row_payload_bytes(&values);
            if writer.is_none() && reserved + bytes > memory.limit() / 8 {
                let mut run = window_writer(memory)?;
                for (index, mut buffered) in rows.drain(..).enumerate() {
                    buffered.push(Value::UInt64(index as u64));
                    write_window_row(&mut run, &buffered)?;
                }
                rows = Vec::new();
                memory.release(reserved);
                reserved = 0;
                writer = Some(run);
            }
            if let Some(run) = &mut writer {
                let mut values = values;
                values.push(Value::UInt64(ordinal));
                memory.ensure_transient(
                    batch.estimated_bytes() + estimated_row_payload_bytes(&values),
                )?;
                write_window_row(run, &values)?;
            } else {
                reserved += super::reserve_vec_elements(&mut rows, 1, 0, memory)?;
                memory.reserve(bytes)?;
                reserved += bytes;
                rows.push(values);
            }
            ordinal += 1;
        }
    }
    let writer = if let Some(writer) = writer {
        writer
    } else {
        let mut source = PullOperator::Rows {
            rows,
            cursor: 0,
            column_types: input_types.to_vec(),
        };
        let before = memory.used();
        match build_memory_window(&mut source, windows, memory, collation) {
            Ok(output) => {
                drop(source);
                memory.release(reserved);
                return Ok(super::SortedRows::Memory(output));
            }
            Err(ExecError::MemoryLimitExceeded { .. }) => {
                // Frame values can grow far beyond their input. The raw
                // rows remain replayable if the memory attempt fails.
                memory.release(memory.used().saturating_sub(before));
                let PullOperator::Rows { rows, .. } = source else {
                    unreachable!()
                };
                let mut writer = window_writer(memory)?;
                for (index, mut row) in rows.into_iter().enumerate() {
                    row.push(Value::UInt64(index as u64));
                    write_window_row(&mut writer, &row)?;
                }
                memory.release(reserved);
                writer
            }
            Err(error) => return Err(error),
        }
    };
    let mut run = writer.finish().map_err(window_io)?;
    for (index, window) in windows.iter().enumerate() {
        run = evaluate_spilled_window(
            run,
            window,
            input_types,
            input_types.len() + index,
            memory,
            collation,
        )?;
    }
    let width = column_types.len();
    let keys = vec![window_order_key(width, true, true, false)];
    let mut sorter = WindowSorter::new(keys, collation);
    let mut reader = run.open().map_err(window_io)?;
    while let Some(payload) = reader.next().map_err(window_io)? {
        sorter.push(
            crate::spill::Decoder::new(payload)
                .values()
                .map_err(ExecError::Source)?,
            memory,
        )?;
    }
    sorter
        .finish(Some(width), memory)
        .map(super::SortedRows::Spilled)
}

/// The windows answered over the input's own batches, when the input ends
/// within the memory path's share of the ceiling and every key column
/// builds; otherwise `Err` with the batches read so far, which the row
/// path continues from.
fn over_batches(
    input: &mut PullOperator,
    windows: &[CompiledWindow],
    column_types: &[DataType],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Result<super::SortedRows, Vec<RecordBatch>>, ExecError> {
    let mut kept = Vec::new();
    let mut kept_bytes = 0_usize;
    while let Some(batch) = input.next_batch(memory)? {
        let bytes = batch.estimated_bytes();
        if kept_bytes.saturating_add(bytes) > memory.limit() / 8 || memory.reserve(bytes).is_err() {
            memory.release(kept_bytes);
            kept.push(batch);
            return Ok(Err(kept));
        }
        kept_bytes += bytes;
        kept.push(batch);
    }
    let before = memory.used();
    match columnar_window(&kept, windows, column_types, memory, collation) {
        Ok(Some(batches)) => Ok(Ok(super::SortedRows::Batches { batches, next: 0 })),
        // Keys the columnar path declined, or frames that outgrew the
        // ceiling: the rows go the row path's way.
        Ok(None) | Err(ExecError::MemoryLimitExceeded { .. }) => {
            memory.release(
                memory
                    .used()
                    .saturating_sub(before)
                    .saturating_add(kept_bytes),
            );
            Ok(Err(kept))
        }
        Err(error) => Err(error),
    }
}

/// The memory path over the input's own batches. Each window's keys are
/// evaluated a column at a time, the rows are ordered by the columnar
/// sort's packed keys - the row sort's exact order, ties in arrival order,
/// as the row path orders them - and each window's values join the batches
/// as a trailing column. The row path copied every input cell into a row
/// and ordered the rows by comparing values. `None` when a key column
/// cannot be built from the values its expression gives; the row path
/// answers then.
fn columnar_window(
    batches: &[RecordBatch],
    windows: &[CompiledWindow],
    column_types: &[DataType],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Option<VecDeque<RecordBatch>>, ExecError> {
    let input_width = column_types.len() - windows.len();
    // Each batch's selected rows take consecutive indexes, in arrival order.
    let mut offsets = Vec::with_capacity(batches.len());
    let mut row_count = 0_usize;
    for batch in batches {
        offsets.push(row_count);
        row_count += batch.visible_row_count();
    }
    let mut results = Vec::with_capacity(windows.len());
    for window in windows {
        let held = memory.used();
        let mut key_batches = Vec::with_capacity(batches.len());
        let mut keys = Vec::with_capacity(row_count);
        for batch in batches {
            let mut columns = Vec::with_capacity(window.key_types.len());
            for (expr, data_type) in window.key_exprs().into_iter().zip(&window.key_types) {
                let Some(column) = key_column(expr, *data_type, batch)? else {
                    return Ok(None);
                };
                columns.push(column);
            }
            let mut key_batch = RecordBatch::new(batch.row_count(), columns)?;
            key_batch.set_selection(batch.selection().clone())?;
            memory.reserve(key_batch.estimated_bytes())?;
            for row in batch.selection().selected_rows() {
                let values = key_batch
                    .columns()
                    .iter()
                    .map(|column| {
                        column.value_owned(row).ok_or(ExecError::InvalidBatch(
                            "window row is outside an input column",
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                memory.reserve(estimated_row_payload_bytes(&values))?;
                keys.push(values);
            }
            key_batches.push(key_batch);
        }
        let order = columnar_order(window, key_batches, &offsets, memory, collation)?;
        results.push(compute_window_column(
            window, &keys, row_count, &order, memory, collation,
        )?);
        drop(keys);
        memory.release(memory.used().saturating_sub(held));
    }
    let mut output = VecDeque::with_capacity(batches.len());
    for (batch, offset) in batches.iter().zip(offsets) {
        let mut columns = batch.columns().to_vec();
        for (index, result) in results.iter().enumerate() {
            let mut values = vec![Value::Null; batch.row_count()];
            for (position, row) in batch.selection().selected_rows().enumerate() {
                values[row] = result[offset + position].clone();
            }
            memory.reserve(estimated_row_payload_bytes(&values))?;
            columns.push(ColumnVector::new(
                column_types[input_width + index],
                values,
            )?);
        }
        let mut answered = RecordBatch::new(batch.row_count(), columns)?;
        answered.set_selection(batch.selection().clone())?;
        output.push_back(answered);
    }
    Ok(Some(output))
}

/// One window key over `batch` as a column: from the batch kernels where
/// they answer, else row by row over the selected rows. `None` when the
/// values do not fit the key's declared type.
fn key_column(
    expr: &CompiledExpr,
    data_type: Option<DataType>,
    batch: &RecordBatch,
) -> Result<Option<ColumnVector>, ExecError> {
    if let Some(column) = expr.evaluate_column(batch, None) {
        return Ok(Some(column));
    }
    let values = (0..batch.row_count())
        .map(|row| {
            if batch.selection().is_selected(row) {
                expr.evaluate(batch, row)
            } else {
                Ok(Value::Null)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let data_type = data_type
        .or_else(|| values.iter().find_map(Value::data_type))
        .unwrap_or(DataType::Utf8);
    Ok(ColumnVector::new(data_type, values).ok())
}

/// The window's rows in (partition, order) order, as indexes of the rows
/// `offsets` number: the columnar sort over the key batches, whose stable
/// order keeps rows with equal keys in the order they arrived.
fn columnar_order(
    window: &CompiledWindow,
    key_batches: Vec<RecordBatch>,
    offsets: &[usize],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<usize>, ExecError> {
    // Each row's index among its batch's selected rows.
    let positions = key_batches
        .iter()
        .map(|batch| {
            let mut positions = vec![0_usize; batch.row_count()];
            for (position, row) in batch.selection().selected_rows().enumerate() {
                positions[row] = position;
            }
            positions
        })
        .collect::<Vec<_>>();
    let rows = key_batches
        .iter()
        .map(RecordBatch::visible_row_count)
        .sum::<usize>();
    memory.reserve(rows.saturating_mul(size_of::<usize>() + size_of::<(u32, u32)>()))?;
    Ok(super::columnar_sort::ColumnarSorted::new(
        key_batches,
        &window.sort_keys(),
        None,
        collation,
        None,
    )?
    .into_order()
    .into_iter()
    .map(|(batch, row)| offsets[batch as usize] + positions[batch as usize][row as usize])
    .collect())
}

// Result::map_err passes ownership of its error to this adapter.
#[allow(clippy::needless_pass_by_value)]
fn window_io(error: std::io::Error) -> ExecError {
    ExecError::Source(format!("window spill: {error}"))
}

fn window_writer(memory: &MemoryTracker) -> Result<crate::spill::RunWriter, ExecError> {
    crate::spill::RunWriter::create("pintail-window-", memory.spill()).map_err(window_io)
}

fn write_window_row(writer: &mut crate::spill::RunWriter, row: &[Value]) -> Result<(), ExecError> {
    let mut encoder = crate::spill::Encoder::new();
    encoder.values(row);
    writer.write(&encoder.finish()).map_err(window_io)
}

fn window_order_key(
    index: usize,
    ascending: bool,
    nulls_first: bool,
    decimal: bool,
) -> BoundOrderKey {
    BoundOrderKey {
        index,
        ascending,
        nulls_first,
        decimal,
        collation: None,
    }
}

struct WindowSorter {
    rows: Vec<Vec<Value>>,
    runs: Vec<crate::spill::ClosedRun>,
    reserved: usize,
    keys: Vec<BoundOrderKey>,
    collation: Collation,
}

impl WindowSorter {
    fn new(keys: Vec<BoundOrderKey>, collation: Collation) -> Self {
        Self {
            rows: Vec::new(),
            runs: Vec::new(),
            reserved: 0,
            keys,
            collation,
        }
    }

    fn flush(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        if self.rows.is_empty() {
            return Ok(());
        }
        self.rows.sort_by(|left, right| {
            super::sort::compare_sort_rows(left, right, &self.keys, self.collation)
        });
        self.runs
            .push(super::sort::write_sorted_run(&self.rows, memory)?);
        self.rows = Vec::new();
        memory.release(self.reserved);
        self.reserved = 0;
        Ok(())
    }

    fn push(&mut self, row: Vec<Value>, memory: &MemoryTracker) -> Result<(), ExecError> {
        let bytes = estimated_row_payload_bytes(&row);
        if self.reserved + bytes > memory.limit() / 8 {
            self.flush(memory)?;
        }
        self.reserved += super::reserve_vec_elements(&mut self.rows, 1, 0, memory)?;
        memory.reserve(bytes)?;
        self.reserved += bytes;
        self.rows.push(row);
        Ok(())
    }

    fn finish(
        mut self,
        trim: Option<usize>,
        memory: &MemoryTracker,
    ) -> Result<super::sort::SpilledMerge, ExecError> {
        self.flush(memory)?;
        super::sort::SpilledMerge::new(self.runs, &[], self.keys, trim, self.collation, memory)
    }
}

fn evaluate_spilled_window(
    run: crate::spill::ClosedRun,
    window: &CompiledWindow,
    input_types: &[DataType],
    width: usize,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<crate::spill::ClosedRun, ExecError> {
    let key_start = width + 1;
    let mut order = (0..window.partition.len())
        .map(|index| window_order_key(key_start + index, true, true, false))
        .collect::<Vec<_>>();
    order.extend(window.order.iter().enumerate().map(|(index, key)| {
        window_order_key(
            key_start + window.partition.len() + index,
            key.1,
            key.2,
            key.3,
        )
    }));
    order.push(window_order_key(width, true, true, false));
    let mut sorter = WindowSorter::new(order, collation);
    let mut reader = run.open().map_err(window_io)?;
    while let Some(payload) = reader.next().map_err(window_io)? {
        memory.check_interruption()?;
        let mut row = crate::spill::Decoder::new(payload)
            .values()
            .map_err(ExecError::Source)?;
        let batch = crate::RecordBatch::new(
            1,
            super::rows_to_columns(&[row[..input_types.len()].to_vec()], input_types)?,
        )?;
        for expr in &window.partition {
            row.push(expr.evaluate(&batch, 0)?);
        }
        for (expr, _, _, _) in &window.order {
            row.push(expr.evaluate(&batch, 0)?);
        }
        match &window.function {
            CompiledWindowFunction::Aggregate(_, argument)
            | CompiledWindowFunction::Extreme { argument, .. } => {
                row.push(argument.evaluate(&batch, 0)?);
            }
            CompiledWindowFunction::Offset {
                argument, default, ..
            } => {
                row.push(argument.evaluate(&batch, 0)?);
                if let Some(default) = default {
                    row.push(default.evaluate(&batch, 0)?);
                }
            }
            _ => {}
        }
        memory.ensure_transient(batch.estimated_bytes() + estimated_row_payload_bytes(&row))?;
        sorter.push(row, memory)?;
    }
    drop(reader);
    drop(run);
    let mut merged = sorter.finish(None, memory)?;
    let mut writer = window_writer(memory)?;
    let mut partition: Vec<Vec<Value>> = Vec::new();
    let mut reserved = 0;
    while let Some(row) = merged.next_row()? {
        let same = partition.first().is_none_or(|first| {
            (0..window.partition.len()).all(|index| {
                compare_sort_values(
                    &first[key_start + index],
                    &row[key_start + index],
                    window_order_key(0, true, true, false),
                    collation,
                )
                .is_eq()
            })
        });
        if !same {
            finish_window_partition(
                &mut partition,
                window,
                key_start,
                &mut writer,
                memory,
                collation,
            )?;
            partition = Vec::new();
            memory.release(reserved);
            reserved = 0;
        }
        let bytes = estimated_row_payload_bytes(&row);
        reserved += super::reserve_vec_elements(&mut partition, 1, 0, memory)?;
        memory.reserve(bytes)?;
        reserved += bytes;
        partition.push(row);
    }
    finish_window_partition(
        &mut partition,
        window,
        key_start,
        &mut writer,
        memory,
        collation,
    )?;
    memory.release(reserved);
    writer.finish().map_err(window_io)
}

fn finish_window_partition(
    rows: &mut Vec<Vec<Value>>,
    window: &CompiledWindow,
    key_start: usize,
    writer: &mut crate::spill::RunWriter,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<(), ExecError> {
    if rows.is_empty() {
        return Ok(());
    }
    let before = memory.used();
    let key_bytes = rows
        .iter()
        .map(|row| estimated_row_payload_bytes(&row[key_start..]))
        .sum::<usize>();
    memory.reserve(key_bytes.saturating_add(rows.len().saturating_mul(size_of::<Vec<Value>>())))?;
    let keys = rows
        .iter()
        .map(|row| row[key_start..].to_vec())
        .collect::<Vec<_>>();
    memory.reserve(rows.len() * size_of::<Value>())?;
    let order = window_order(window, &keys, memory, collation)?;
    let results = compute_window_column(window, &keys, rows.len(), &order, memory, collation)?;
    for (mut row, value) in rows.drain(..).zip(results) {
        row.truncate(key_start);
        let ordinal = row.pop().expect("window ordinal");
        row.push(value);
        row.push(ordinal);
        memory.ensure_transient(estimated_row_payload_bytes(&row))?;
        write_window_row(writer, &row)?;
    }
    memory.release(memory.used().saturating_sub(before));
    Ok(())
}

/// Materializes the input, computes every window over its partitions, and
/// returns rows with the window results appended as trailing columns.
fn build_memory_window(
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
        let order = window_order(window, &keys[index], memory, collation)?;
        let result =
            compute_window_column(window, &keys[index], row_count, &order, memory, collation)?;
        for (row, value) in rows.iter_mut().zip(&result) {
            memory.reserve(value.heap_bytes().saturating_add(size_of::<Value>()))?;
            row.push(value.clone());
        }
    }
    Ok(MaterializedRows {
        rows,
        position: 0,
        spilled: None,
    })
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
        value if decimal && value.text().is_some() => {
            let text = value.text().expect("guarded text");
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
                // An integer key compares with the target in units, without
                // rendering either side as text.
                let integer = match candidate {
                    Value::Boolean(value) => Some(i128::from(*value)),
                    Value::Int64(value) => Some(i128::from(*value)),
                    Value::UInt64(value) => Some(i128::from(*value)),
                    _ => None,
                }
                .and_then(|integer| {
                    10_i128
                        .checked_pow(u32::from(*scale))
                        .and_then(|factor| integer.checked_mul(factor))
                });
                let ordering = if let Some(integer) = integer {
                    integer.cmp(units)
                } else {
                    let candidate = match candidate {
                        Value::Boolean(value) => i8::from(*value).to_string(),
                        Value::Int64(value) => value.to_string(),
                        Value::UInt64(value) => value.to_string(),
                        value if *decimal && value.text().is_some() => {
                            value.text().expect("guarded text").to_owned()
                        }
                        _ => return Ordering::Equal,
                    };
                    compare_decimal_text(
                        &candidate,
                        &pintail_types::format_decimal_scaled(*units, *scale),
                    )
                    .unwrap_or(Ordering::Equal)
                };
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
                    collation: None,
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

/// The rows of `keys` in (partition, order) order, rows with equal keys in
/// the order they arrived.
fn window_order(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<usize>, ExecError> {
    let partition_len = window.partition.len();
    let order_key = |ascending: bool, nulls_first: bool, decimal: bool| BoundOrderKey {
        index: 0,
        ascending,
        nulls_first,
        decimal,
        collation: None,
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
    let mut order = (0..keys.len()).collect::<Vec<_>>();
    memory.reserve(keys.len().saturating_mul(size_of::<usize>()))?;
    order.sort_by(|left, right| compare_rows(*left, *right));
    Ok(order)
}

/// Computes one window's value per row, given the rows in (partition,
/// order) order: walks each partition assigning ranks or aggregate frames
/// (whole partition without ORDER BY; running frame including the current
/// row's peers with it — `MySQL`'s default frames).
#[allow(clippy::too_many_lines)]
fn compute_window_column(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    row_count: usize,
    order: &[usize],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Value>, ExecError> {
    let partition_len = window.partition.len();
    let order_key = |ascending: bool, nulls_first: bool, decimal: bool| BoundOrderKey {
        index: 0,
        ascending,
        nulls_first,
        decimal,
        collation: None,
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

    let mut results = vec![Value::Null; row_count];
    let mut start = 0;
    while start < row_count {
        let mut end = start + 1;
        while end < row_count && same_partition(order[start], order[end]) {
            end += 1;
        }
        let partition = &order[start..end];
        // Each row's peer group, found once per partition: a RANGE bound at
        // CURRENT ROW reads it for every row, and walking the group each
        // time made a low-cardinality ordering key quadratic.
        let (peer_firsts, peer_ends) = if window.frame.is_some() {
            peer_groups(partition, &same_peers)
        } else {
            (Vec::new(), Vec::new())
        };
        let peer_bytes = peer_firsts.len().saturating_mul(2 * size_of::<usize>());
        memory.reserve(peer_bytes)?;
        let peer_start = |from: usize| peer_firsts[from];
        let peer_end = |from: usize| peer_ends[from];
        // A RANGE frame's extent follows from the row's ordering value alone,
        // so the rows of one peer group share it. Resolving an offset bound
        // per row repeated two searches for every peer.
        let range_extent =
            std::cell::Cell::new(None::<(usize, pintail_sql::BoundWindowFrame, (usize, usize))>);
        let frame_extent = |frame: pintail_sql::BoundWindowFrame,
                            index: usize|
         -> Result<(usize, usize), ExecError> {
            use pintail_sql::{BoundFrameBound as Edge, BoundFrameOffset as Offset};
            if frame.range
                && let Some((peer, cached, extent)) = range_extent.get()
                && cached == frame
                && peer == peer_start(index)
            {
                return Ok(extent);
            }
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
            if frame.range {
                range_extent.set(Some((peer_start(index), frame, (start, end))));
            }
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
                    // A frame running to UNBOUNDED FOLLOWING is the mirror
                    // image: its start only moves back as the rows do, so it
                    // folds once from the partition's end, for aggregates
                    // whose answer does not depend on the order rows arrive in.
                    let trailing = !running
                        && matches!(frame.end, Edge::UnboundedFollowing)
                        && order_insensitive(aggregate);
                    let mut state = AggregateState::new(aggregate);
                    let mut accumulated = if trailing { partition.len() } else { 0 };
                    // A row whose frame is the previous row's shares its
                    // value: under RANGE every peer has the same frame.
                    let mut previous: Option<(usize, usize, Value)> = None;
                    let indexes: Vec<usize> = if trailing {
                        (0..partition.len()).rev().collect()
                    } else {
                        (0..partition.len()).collect()
                    };
                    for index in indexes {
                        // Under RANGE, CURRENT ROW covers the whole peer
                        // group rather than the single row: the frame is
                        // defined over the ordering key's values, and peers
                        // share one value.
                        let (start, end) = frame_extent(frame, index)?;
                        let value = match &previous {
                            Some((first, last, value)) if (*first, *last) == (start, end) => {
                                value.clone()
                            }
                            _ if running => {
                                while accumulated < end {
                                    state.update(
                                        aggregate,
                                        &keys[partition[accumulated]][argument_position],
                                        memory,
                                    )?;
                                    accumulated += 1;
                                }
                                state.clone().finish(memory)?
                            }
                            _ if trailing && start <= accumulated && end == partition.len() => {
                                while accumulated > start {
                                    accumulated -= 1;
                                    state.update(
                                        aggregate,
                                        &keys[partition[accumulated]][argument_position],
                                        memory,
                                    )?;
                                }
                                state.clone().finish(memory)?
                            }
                            _ => {
                                let mut framed = AggregateState::new(aggregate);
                                for row in partition.iter().take(end).skip(start) {
                                    framed.update(
                                        aggregate,
                                        &keys[*row][argument_position],
                                        memory,
                                    )?;
                                }
                                framed.finish(memory)?
                            }
                        };
                        previous = Some((start, end, value.clone()));
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
        memory.release(peer_bytes);
        start = end;
    }
    Ok(results)
}

/// The first position and the end of each row's peer group, by position in
/// the sorted partition.
fn peer_groups(
    partition: &[usize],
    same_peers: &impl Fn(usize, usize) -> bool,
) -> (Vec<usize>, Vec<usize>) {
    let len = partition.len();
    let mut firsts = vec![0; len];
    for (index, pair) in partition.windows(2).enumerate() {
        firsts[index + 1] = if same_peers(pair[0], pair[1]) {
            firsts[index]
        } else {
            index + 1
        };
    }
    let mut ends = vec![len; len];
    for (index, pair) in partition.windows(2).enumerate().rev() {
        ends[index] = if same_peers(pair[0], pair[1]) {
            ends[index + 1]
        } else {
            index + 1
        };
    }
    (firsts, ends)
}

/// Whether folding a frame's rows in reverse gives the answer folding them
/// forward does. Counts, bit folds, exact sums and averages, and extremes of
/// anything but text do not depend on arrival order; a float sum rounds by
/// it, a text extreme keeps the first of values that compare equal, and a
/// concatenation reads back in it.
fn order_insensitive(aggregate: &CompiledAggregate) -> bool {
    let float = |data_type: Option<DataType>| {
        matches!(
            data_type,
            None | Some(DataType::Float32 | DataType::Float64)
        )
    };
    match aggregate.function {
        AggregateFunction::Count
        | AggregateFunction::BitAnd
        | AggregateFunction::BitOr
        | AggregateFunction::BitXor => true,
        AggregateFunction::Sum | AggregateFunction::Average => {
            !float(aggregate.input_type) && !float(aggregate.data_type)
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum => {
            !float(aggregate.input_type)
                && !matches!(
                    aggregate.input_type,
                    Some(DataType::Utf8 | DataType::Json | DataType::Binary)
                )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, parse_statement};
    use pintail_types::{Column, DataType, TableSchema, Value};

    use super::{CompiledWindow, build_memory_window, columnar_window};
    use crate::RecordBatch;
    use crate::batch::{ColumnVector, SelectionMask};
    use crate::collation::Collation;
    use crate::execution::{MemoryTracker, PhysicalPlan, PhysicalPlanner, PullOperator};
    use crate::{LogicalPlanner, Optimizer};

    const ROWS: usize = 3_000;

    fn schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "grp", DataType::Int64, true),
                Column::new(3, "note", DataType::Utf8, true),
                Column::new(
                    4,
                    "amount",
                    DataType::Decimal {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                Column::new(5, "seen", DataType::DateTime64 { fsp: 0 }, true),
            ],
        )
        .expect("schema")
    }

    /// Column `id` of row `row`, with NULLs, ties and text that folds under
    /// the collation.
    fn cell(id: u32, row: usize) -> Value {
        let spread = (row * 7_919) % 1_009;
        let null = |every: usize| spread.is_multiple_of(every);
        match id {
            1 => Value::Int64(i64::try_from(row).expect("small")),
            2 if null(13) => Value::Null,
            2 => Value::Int64(i64::try_from(spread % 7).expect("small")),
            3 if null(11) => Value::Null,
            3 => Value::Utf8(["b", "B", "a", "\u{e9}", "e"][spread % 5].to_owned()),
            4 if null(17) => Value::Null,
            4 => Value::Utf8(format!(
                "{}.{:02}",
                i64::try_from(spread % 40).expect("small") - 20,
                spread % 100
            )),
            5 if null(19) => Value::Null,
            5 => Value::Utf8(format!("2026-01-{:02} 10:00:00", spread % 28 + 1)),
            _ => unreachable!("five columns"),
        }
    }

    /// The window node of `sql`'s plan, its windows compiled against its
    /// input, and that input as batches of `batch_rows` rows with every
    /// seventh row unselected.
    fn window_input(
        sql: &str,
        batch_rows: usize,
    ) -> (Vec<CompiledWindow>, Vec<DataType>, Vec<RecordBatch>) {
        let table = TableEntry::new(
            TableId::new(1),
            "t",
            schema(),
            TableStatistics::with_row_count(ROWS as u64),
        )
        .expect("table");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database")
            ])
            .expect("catalog");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let mut plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let (scan, windows, outputs) = loop {
            match plan {
                PhysicalPlan::Window {
                    input,
                    windows,
                    outputs,
                } => match *input {
                    PhysicalPlan::Scan(scan) => break (scan, windows, outputs),
                    other => panic!("window over {other:?}"),
                },
                PhysicalPlan::Project { input, .. } | PhysicalPlan::Sort { input, .. } => {
                    plan = *input;
                }
                other => panic!("no window in {other:?}"),
            }
        };
        let columns = scan
            .projected_column_ids
            .iter()
            .map(|id| {
                scan.table
                    .columns
                    .iter()
                    .find(|column| column.column_id == *id)
                    .cloned()
                    .expect("column")
            })
            .collect::<Vec<_>>();
        let compiled = windows
            .iter()
            .map(|window| CompiledWindow::compile(window, &columns, Collation::default()))
            .collect::<Result<Vec<_>, _>>()
            .expect("compile");
        let types = columns
            .iter()
            .chain(&outputs)
            .map(|column| column.data_type)
            .collect::<Vec<_>>();
        let batches = (0..ROWS)
            .step_by(batch_rows)
            .map(|first| {
                let rows = first..(first + batch_rows).min(ROWS);
                let mut batch = RecordBatch::new(
                    rows.len(),
                    columns
                        .iter()
                        .map(|column| {
                            ColumnVector::new(
                                column.data_type,
                                rows.clone()
                                    .map(|row| cell(column.column_id, row))
                                    .collect(),
                            )
                            .expect("column")
                        })
                        .collect(),
                )
                .expect("batch");
                let mut selection = SelectionMask::all(rows.len());
                for (position, row) in rows.enumerate() {
                    if row % 7 == 3 {
                        selection.set(position, false).expect("row");
                    }
                }
                batch.set_selection(selection).expect("selection");
                batch
            })
            .collect();
        (compiled, types, batches)
    }

    fn rows_of(batch: &RecordBatch) -> Vec<Vec<Value>> {
        batch
            .selection()
            .selected_rows()
            .map(|row| {
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn windows_over_batches_answer_as_the_row_path_does() {
        let memory = MemoryTracker::new(usize::MAX);
        for window in [
            "ROW_NUMBER() OVER (ORDER BY amount, id)",
            "ROW_NUMBER() OVER (PARTITION BY note ORDER BY seen DESC, id)",
            "RANK() OVER (PARTITION BY grp ORDER BY note)",
            "DENSE_RANK() OVER (ORDER BY amount DESC)",
            "SUM(amount) OVER (PARTITION BY grp ORDER BY id)",
            "SUM(amount) OVER (PARTITION BY note)",
            "COUNT(*) OVER (PARTITION BY grp, note ORDER BY seen)",
            "AVG(amount) OVER (ORDER BY id ROWS BETWEEN 3 PRECEDING AND 2 FOLLOWING)",
            "MAX(note) OVER (PARTITION BY grp ORDER BY amount \
             RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
            "LAG(note, 2, 'none') OVER (PARTITION BY grp ORDER BY id)",
            "LEAD(amount) OVER (ORDER BY seen, id)",
            "FIRST_VALUE(id) OVER (PARTITION BY note ORDER BY amount)",
            "LAST_VALUE(id) OVER (PARTITION BY grp ORDER BY note)",
            "NTILE(7) OVER (PARTITION BY note ORDER BY id)",
            "SUM(grp) OVER (ORDER BY amount RANGE BETWEEN 2 PRECEDING AND 1 FOLLOWING)",
            "MIN(id + grp) OVER (PARTITION BY grp % 3 ORDER BY amount * 2)",
        ] {
            let sql = format!(
                "SELECT id, {window} AS w, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY id) AS r \
                 FROM t"
            );
            for batch_rows in [ROWS, 700] {
                let (windows, types, batches) = window_input(&sql, batch_rows);
                let input_types = types[..types.len() - windows.len()].to_vec();
                let mut rows = PullOperator::Rows {
                    rows: batches.iter().flat_map(rows_of).collect(),
                    cursor: 0,
                    column_types: input_types,
                };
                let expected =
                    build_memory_window(&mut rows, &windows, &memory, Collation::default())
                        .expect("row path")
                        .rows;
                let answered =
                    columnar_window(&batches, &windows, &types, &memory, Collation::default())
                        .expect("columnar path")
                        .expect("key columns build");
                let actual = answered.iter().flat_map(rows_of).collect::<Vec<_>>();
                assert_eq!(actual.len(), expected.len(), "{sql}");
                assert!(
                    actual == expected,
                    "{sql} over batches of {batch_rows}: the answers differ"
                );
            }
        }
    }
}
