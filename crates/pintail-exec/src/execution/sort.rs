//! Sort, DISTINCT and set-operation materialization, including the
//! on-disk spill paths used when a query exceeds its memory ceiling.

use crate::collation::Collation;
use std::cmp::Ordering;

use pintail_sql::BoundOrderKey;
use pintail_types::{DataType, Value};

use super::{
    ExecError, MaterializedRows, MemoryTracker, PullOperator, batch_row, compare_decimal_text,
    estimated_batch_row_bytes, estimated_record_batch_bytes, estimated_row_payload_bytes,
    next_materialized_batch, reserve_vec_elements, rows_to_columns,
};
use crate::{DEFAULT_BATCH_ROWS, RecordBatch, expression::compare_utf8_mysql, spill};

/// Sorted rows served either from memory (the fast path, byte-identical to
/// the pre-spill behavior) or by merging sorted on-disk runs when the input
/// exceeded the query memory ceiling.
pub(super) enum SortedRows {
    Memory(MaterializedRows),
    Spilled(SpilledMerge),
}

impl SortedRows {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        match self {
            Self::Memory(rows) => {
                let row = rows.rows.get(rows.position).cloned();
                rows.position = rows.position.saturating_add(usize::from(row.is_some()));
                Ok(row)
            }
            Self::Spilled(merge) => merge.next_row(),
        }
    }

    pub(super) fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        match self {
            Self::Memory(rows) => next_materialized_batch(rows, column_types, memory),
            Self::Spilled(merge) => merge.next_batch(column_types, memory),
        }
    }
}

/// Blocking standalone DISTINCT implemented as an external sort followed by
/// adjacent-row elimination. The all-column ordering uses the same collation
/// and exact DECIMAL comparator as ORDER BY, grouping, and set semantics.
pub(super) struct DistinctRows {
    sorted: SortedRows,
    keys: Vec<BoundOrderKey>,
    /// The plan's collation: DISTINCT decides which rows are the same row.
    collation: Collation,
    last: Option<Vec<Value>>,
    last_reserved: usize,
}

impl DistinctRows {
    pub(super) fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            let Some(mut batch) = self.sorted.next_batch(column_types, memory)? else {
                memory.release(self.last_reserved);
                self.last_reserved = 0;
                self.last = None;
                return Ok(None);
            };
            let batch_bytes = batch.estimated_bytes();
            for row in batch.selection().selected_rows().collect::<Vec<_>>() {
                let values = batch_row(&batch, row)?;
                if self.last.as_ref().is_some_and(|last| {
                    compare_sort_rows(last, &values, &self.keys, self.collation).is_eq()
                }) {
                    batch.selection_mut().set(row, false)?;
                    continue;
                }
                let bytes = estimated_row_payload_bytes(&values);
                memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                if bytes > self.last_reserved {
                    memory.reserve(bytes - self.last_reserved)?;
                } else {
                    memory.release(self.last_reserved - bytes);
                }
                self.last_reserved = bytes;
                self.last = Some(values);
            }
            if batch.visible_row_count() > 0 {
                return Ok(Some(batch));
            }
        }
    }
}

/// External sort-merge state for INTERSECT and EXCEPT. Both inputs use the
/// shared full-row comparator, so spilling preserves exact DECIMAL and `MySQL`
/// collation equivalence instead of introducing a second key definition.
pub(super) struct SetOpRows {
    /// The plan's collation: set operations compare whole rows.
    collation: Collation,
    left: SortedRowCursor,
    right: SortedRowCursor,
    keys: Vec<BoundOrderKey>,
    keep_matching: bool,
    all: bool,
    pending: Option<(Vec<Value>, u64)>,
    pending_reserved: usize,
}

struct SortedRowCursor {
    /// The plan's collation, for grouping equal rows.
    collation: Collation,
    rows: SortedRows,
    head: Option<Vec<Value>>,
    exhausted: bool,
}

impl SortedRowCursor {
    fn new(rows: SortedRows, collation: Collation) -> Self {
        Self {
            rows,
            collation,
            head: None,
            exhausted: false,
        }
    }

    fn ensure_head(&mut self) -> Result<(), ExecError> {
        if self.head.is_none() && !self.exhausted {
            self.head = self.rows.next_row()?;
            self.exhausted = self.head.is_none();
        }
        Ok(())
    }

    fn take_group(&mut self, keys: &[BoundOrderKey]) -> Result<(Vec<Value>, u64), ExecError> {
        self.ensure_head()?;
        let first = self
            .head
            .take()
            .ok_or(ExecError::InvalidPhysicalPlan("set input group is empty"))?;
        let mut count = 1_u64;
        loop {
            let Some(next) = self.rows.next_row()? else {
                self.exhausted = true;
                break;
            };
            if compare_sort_rows(&first, &next, keys, self.collation).is_eq() {
                count = count.saturating_add(1);
            } else {
                self.head = Some(next);
                break;
            }
        }
        Ok((first, count))
    }
}

impl SetOpRows {
    fn next_group(
        &mut self,
        memory: &MemoryTracker,
    ) -> Result<Option<(Vec<Value>, u64)>, ExecError> {
        loop {
            memory.check_interruption()?;
            self.left.ensure_head()?;
            self.right.ensure_head()?;
            let ordering = match (self.left.head.as_ref(), self.right.head.as_ref()) {
                (None, _) => return Ok(None),
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => {
                    compare_sort_rows(left, right, &self.keys, self.collation)
                }
            };
            match ordering {
                Ordering::Less => {
                    let (row, left_count) = self.left.take_group(&self.keys)?;
                    if !self.keep_matching {
                        return Ok(Some((row, if self.all { left_count } else { 1 })));
                    }
                }
                Ordering::Greater => {
                    let _ = self.right.take_group(&self.keys)?;
                }
                Ordering::Equal => {
                    let (row, left_count) = self.left.take_group(&self.keys)?;
                    let (_, right_count) = self.right.take_group(&self.keys)?;
                    let output_count = if self.keep_matching {
                        if self.all {
                            left_count.min(right_count)
                        } else {
                            1
                        }
                    } else if self.all {
                        left_count.saturating_sub(right_count)
                    } else {
                        0
                    };
                    if output_count > 0 {
                        return Ok(Some((row, output_count)));
                    }
                }
            }
        }
    }

    pub(super) fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        let mut rows = Vec::with_capacity(DEFAULT_BATCH_ROWS);
        while rows.len() < DEFAULT_BATCH_ROWS {
            if self.pending.is_none() {
                let Some((row, count)) = self.next_group(memory)? else {
                    break;
                };
                let bytes = estimated_row_payload_bytes(&row);
                memory.reserve(bytes)?;
                self.pending_reserved = bytes;
                self.pending = Some((row, count));
            }
            let (row, count) = self.pending.as_mut().expect("initialized above");
            rows.push(row.clone());
            *count -= 1;
            if *count == 0 {
                self.pending = None;
                memory.release(self.pending_reserved);
                self.pending_reserved = 0;
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        memory.ensure_transient(estimated_record_batch_bytes(&rows, column_types.len()))?;
        let columns = rows_to_columns(&rows, column_types)?;
        Ok(Some(RecordBatch::new(rows.len(), columns)?))
    }
}

pub(super) fn build_set_operation(
    left: &mut PullOperator,
    right: &mut PullOperator,
    column_types: &[DataType],
    keep_matching: bool,
    all: bool,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<SetOpRows, ExecError> {
    let keys = column_types
        .iter()
        .enumerate()
        .map(|(index, data_type)| BoundOrderKey {
            index,
            ascending: true,
            nulls_first: true,
            decimal: matches!(data_type, DataType::Decimal { .. }),
            collation: None,
        })
        .collect::<Vec<_>>();
    let left = build_sort(left, &keys, None, None, memory, collation)?;
    let right = build_sort(right, &keys, None, None, memory, collation)?;
    Ok(SetOpRows {
        collation,
        left: SortedRowCursor::new(left, collation),
        right: SortedRowCursor::new(right, collation),
        keys,
        keep_matching,
        all,
        pending: None,
        pending_reserved: 0,
    })
}

pub(super) fn build_distinct(
    input: &mut PullOperator,
    column_types: &[DataType],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<DistinctRows, ExecError> {
    let keys = column_types
        .iter()
        .enumerate()
        .map(|(index, data_type)| BoundOrderKey {
            index,
            ascending: true,
            nulls_first: true,
            decimal: matches!(data_type, DataType::Decimal { .. }),
            collation: None,
        })
        .collect::<Vec<_>>();
    let sorted = build_sort(input, &keys, None, None, memory, collation)?;
    Ok(DistinctRows {
        sorted,
        keys,
        collation,
        last: None,
        last_reserved: 0,
    })
}

pub(super) fn build_sort(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    top_k: Option<usize>,
    trim_to: Option<usize>,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<SortedRows, ExecError> {
    let compare =
        |left: &Vec<Value>, right: &Vec<Value>| compare_sort_rows(left, right, keys, collation);
    if let Some(top_k) = top_k {
        // Top-k retains at most k rows and cannot exceed the ceiling by
        // materializing its input; the in-memory path is unchanged.
        let mut rows = materialize_top_k(input, top_k, keys, compare, memory, collation)?;
        rows.sort_by(compare);
        if let Some(width) = trim_to {
            for row in &mut rows {
                row.truncate(width);
            }
        }
        return Ok(SortedRows::Memory(MaterializedRows { rows, position: 0 }));
    }
    let SpillMaterialization {
        mut rows,
        runs,
        reserved: rows_reserved,
    } = materialize_with_spill(input, keys, memory, collation)?;
    rows.sort_by(compare);
    if runs.is_empty() {
        if let Some(width) = trim_to {
            for row in &mut rows {
                row.truncate(width);
            }
        }
        return Ok(SortedRows::Memory(MaterializedRows { rows, position: 0 }));
    }
    let mut merge = SpilledMerge::new(runs, keys.to_vec(), trim_to, collation)?;
    merge.push_final_run(&rows, memory)?;
    memory.release(rows_reserved);
    Ok(SortedRows::Spilled(merge))
}

/// Materializes the sort input, spilling the accumulated rows as a sorted
/// on-disk run whenever the memory ceiling would be exceeded. Queries that
/// fit in memory take exactly the old path and produce no runs.
struct SpillMaterialization {
    rows: Vec<Vec<Value>>,
    runs: Vec<SpilledRun>,
    reserved: usize,
}

fn materialize_with_spill(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<SpillMaterialization, ExecError> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut retained = 0_usize;
    let mut vector_reserved = 0_usize;
    let mut runs: Vec<SpilledRun> = Vec::new();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        match reserve_vec_elements(&mut rows, additional_rows, 0, memory) {
            Ok(reserved) => vector_reserved = vector_reserved.saturating_add(reserved),
            Err(ExecError::MemoryLimitExceeded { .. }) if !rows.is_empty() => {
                rows.sort_by(|left, right| compare_sort_rows(left, right, keys, collation));
                runs.push(SpilledRun::write(&rows, memory)?);
                rows = Vec::new();
                memory.release(retained.saturating_add(vector_reserved));
                retained = 0;
                vector_reserved = 0;
                vector_reserved = vector_reserved.saturating_add(reserve_vec_elements(
                    &mut rows,
                    additional_rows,
                    0,
                    memory,
                )?);
            }
            Err(error) => return Err(error),
        }
        for row in batch.selection().selected_rows() {
            let row_bytes =
                estimated_batch_row_bytes(&batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            match memory.reserve(row_bytes) {
                Ok(()) => {}
                Err(ExecError::MemoryLimitExceeded { .. }) if !rows.is_empty() => {
                    // Spill the buffered rows as one sorted run and retry;
                    // releasing both the row payloads and the vector's
                    // capacity reservation frees the sort's whole footprint.
                    rows.sort_by(|left, right| compare_sort_rows(left, right, keys, collation));
                    runs.push(SpilledRun::write(&rows, memory)?);
                    rows = Vec::new();
                    memory.release(retained.saturating_add(vector_reserved));
                    retained = 0;
                    vector_reserved = 0;
                    memory.reserve(row_bytes)?;
                }
                Err(error) => return Err(error),
            }
            retained = retained.saturating_add(row_bytes);
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "sort row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
            // Proactive spill at half the ceiling: upstream operators size
            // their own working sets from the remaining headroom, so a sort
            // that hoards the budget until hard failure starves the scan.
            if retained.saturating_add(vector_reserved) > memory.limit() / 2 && rows.len() > 1 {
                rows.sort_by(|left, right| compare_sort_rows(left, right, keys, collation));
                runs.push(SpilledRun::write(&rows, memory)?);
                rows = Vec::new();
                memory.release(retained.saturating_add(vector_reserved));
                retained = 0;
                vector_reserved = 0;
            }
        }
    }
    Ok(SpillMaterialization {
        rows,
        runs,
        reserved: retained.saturating_add(vector_reserved),
    })
}

/// One sorted run on disk: length-framed binary rows in a self-deleting
/// temp file, streamed back in write order.
struct SpilledRun {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
    _path: tempfile::TempPath,
    _reservation: spill::SpillReservation,
}

impl SpilledRun {
    fn write(rows: &[Vec<Value>], memory: &MemoryTracker) -> Result<Self, ExecError> {
        let file = spill::spill_file("pintail-sort-spill-", memory.spill())
            .map_err(|error| ExecError::Source(format!("sort spill create: {error}")))?;
        let (file, path, mut reservation) = file.into_parts();
        let mut writer = std::io::BufWriter::new(file);
        let mut encoder = spill::Encoder::new();
        for row in rows {
            encoder.values(row);
            let payload = std::mem::replace(&mut encoder, spill::Encoder::new()).finish();
            spill::write_record_quota(&mut writer, &payload, &mut reservation)
                .map_err(|error| ExecError::Source(format!("sort spill write: {error}")))?;
        }
        let mut file = writer
            .into_inner()
            .map_err(|error| ExecError::Source(format!("sort spill flush: {error}")))?;
        std::io::Seek::rewind(&mut file)
            .map_err(|error| ExecError::Source(format!("sort spill rewind: {error}")))?;
        Ok(Self {
            reader: std::io::BufReader::new(file),
            payload: Vec::new(),
            _path: path,
            _reservation: reservation,
        })
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("sort spill read: {error}")))?
        {
            return Ok(None);
        }
        spill::Decoder::new(&self.payload)
            .values()
            .map(Some)
            .map_err(|error| ExecError::Source(format!("sort spill decode: {error}")))
    }
}

/// K-way merge over sorted spilled runs; run count is bounded by
/// input-bytes / memory-ceiling, so a linear minimum scan per row is fine.
pub(super) struct SpilledMerge {
    /// The plan's collation, for the merge comparison.
    collation: Collation,
    runs: Vec<SpilledRun>,
    heads: Vec<Option<Vec<Value>>>,
    keys: Vec<BoundOrderKey>,
    trim_to: Option<usize>,
}

impl SpilledMerge {
    fn new(
        runs: Vec<SpilledRun>,
        keys: Vec<BoundOrderKey>,
        trim_to: Option<usize>,
        collation: Collation,
    ) -> Result<Self, ExecError> {
        let mut merge = Self {
            heads: Vec::with_capacity(runs.len()),
            runs,
            keys,
            trim_to,
            collation,
        };
        for index in 0..merge.runs.len() {
            let head = merge.runs[index].next_row()?;
            merge.heads.push(head);
        }
        Ok(merge)
    }

    fn push_final_run(
        &mut self,
        rows: &[Vec<Value>],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut run = SpilledRun::write(rows, memory)?;
        let head = run.next_row()?;
        self.runs.push(run);
        self.heads.push(head);
        Ok(())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        let mut best: Option<usize> = None;
        for (index, head) in self.heads.iter().enumerate() {
            let Some(candidate) = head else { continue };
            let better = match best {
                None => true,
                Some(current) => {
                    let current_head = self.heads[current]
                        .as_ref()
                        .expect("best head is always occupied");
                    compare_sort_rows(candidate, current_head, &self.keys, self.collation)
                        == Ordering::Less
                }
            };
            if better {
                best = Some(index);
            }
        }
        let Some(winner) = best else { return Ok(None) };
        let replacement = self.runs[winner].next_row()?;
        let mut row = std::mem::replace(&mut self.heads[winner], replacement)
            .expect("winner head is occupied");
        if let Some(width) = self.trim_to {
            row.truncate(width);
        }
        Ok(Some(row))
    }

    fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        let mut rows = Vec::with_capacity(DEFAULT_BATCH_ROWS);
        while rows.len() < DEFAULT_BATCH_ROWS {
            let Some(row) = self.next_row()? else { break };
            rows.push(row);
        }
        if rows.is_empty() {
            return Ok(None);
        }
        memory.ensure_transient(estimated_record_batch_bytes(&rows, column_types.len()))?;
        let columns = rows_to_columns(&rows, column_types)?;
        Ok(Some(RecordBatch::new(rows.len(), columns)?))
    }
}

fn materialize_top_k(
    input: &mut PullOperator,
    top_k: usize,
    keys: &[BoundOrderKey],
    compare: impl Copy + FnMut(&Vec<Value>, &Vec<Value>) -> Ordering,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Vec<Value>>, ExecError> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    // Threshold prefilter (experiments/RESULTS.md e03): once k rows are
    // retained, their current worst acts as a cutoff — rows comparing
    // STRICTLY worse on the sort keys can never enter the top k and are
    // skipped before any column values are cloned. Rows tying the threshold
    // are kept, so the candidate set stays a superset and selection
    // semantics are unchanged.
    let mut threshold: Option<Vec<Value>> = None;
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        reserve_vec_elements(&mut rows, additional_rows, 0, memory)?;
        for row in batch.selection().selected_rows() {
            if let Some(threshold_row) = &threshold {
                let mut ordering = Ordering::Equal;
                for key in keys {
                    let candidate = batch
                        .column(key.index)
                        .and_then(|column| column.value(row))
                        .unwrap_or(&Value::Null);
                    let retained = threshold_row.get(key.index).unwrap_or(&Value::Null);
                    let key_ordering = compare_sort_values(candidate, retained, *key, collation);
                    if key_ordering != Ordering::Equal {
                        ordering = key_ordering;
                        break;
                    }
                }
                if ordering == Ordering::Greater {
                    continue;
                }
            }
            let row_bytes =
                estimated_batch_row_bytes(&batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            memory.reserve(row_bytes)?;
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "top-K row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
        if rows.len() > top_k {
            rows.select_nth_unstable_by(top_k, compare);
            let released = rows[top_k..]
                .iter()
                .map(|row| estimated_row_payload_bytes(row))
                .sum::<usize>();
            rows.truncate(top_k);
            let old_capacity = rows.capacity();
            rows.shrink_to_fit();
            memory.release(
                released.saturating_add(
                    old_capacity
                        .saturating_sub(rows.capacity())
                        .saturating_mul(size_of::<Vec<Value>>()),
                ),
            );
            let mut compare = compare;
            threshold = rows
                .iter()
                .max_by(|left, right| compare(left, right))
                .cloned();
        }
    }
    Ok(rows)
}

fn compare_sort_rows(
    left: &[Value],
    right: &[Value],
    keys: &[BoundOrderKey],
    collation: Collation,
) -> Ordering {
    for key in keys {
        let ordering = compare_sort_values(
            left.get(key.index).unwrap_or(&Value::Null),
            right.get(key.index).unwrap_or(&Value::Null),
            *key,
            collation,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

pub(super) fn compare_sort_values(
    left: &Value,
    right: &Value,
    key: BoundOrderKey,
    collation: Collation,
) -> Ordering {
    // The KEY's collation wins where it has one: `ORDER BY general_ci_column,
    // ai_ci_column` orders each column by its own rules, which is what MySQL
    // does. The passed collation is the plan's fallback, used for keys that
    // order no text and for the operator's internal keys.
    let collation = key
        .collation
        .and_then(Collation::from_mysql_name)
        .unwrap_or(collation);
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if key.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if key.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Value::Utf8(left), Value::Utf8(right)) => {
            // Canonical decimal text orders numerically; lexical ordering
            // would put "9.00" after "10.00". Unparseable text (shouldn't
            // happen for decimal-typed keys) falls back to text order.
            let ordering = if key.decimal {
                compare_decimal_text(left, right)
                    .unwrap_or_else(|_| compare_utf8_mysql(left, right, collation))
            } else {
                compare_utf8_mysql(left, right, collation)
            };
            order_direction(ordering, key.ascending)
        }
        _ => order_direction(left.cmp(right), key.ascending),
    }
}

fn order_direction(ordering: Ordering, ascending: bool) -> Ordering {
    if ascending {
        ordering
    } else {
        ordering.reverse()
    }
}
