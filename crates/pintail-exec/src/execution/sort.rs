//! Sort, DISTINCT and set-operation materialization, including the
//! on-disk spill paths used when a query exceeds its memory ceiling.

use crate::collation::Collation;
use std::cmp::Ordering;

use pintail_sql::BoundOrderKey;
use pintail_types::{DataType, Value};

use super::{
    ExecError, MaterializedRows, MemoryTracker, PullOperator, batch_row, columnar_sort,
    compare_decimal_text, estimated_batch_row_bytes, estimated_record_batch_bytes,
    estimated_row_payload_bytes, next_materialized_batch, reserve_vec_elements, rows_to_columns,
};
use crate::{DEFAULT_BATCH_ROWS, RecordBatch, expression::compare_utf8_mysql, spill};

/// Sorted rows served either from memory (the fast path, byte-identical to
/// the pre-spill behavior) or by merging sorted on-disk runs when the input
/// exceeded the query memory ceiling.
pub(super) enum SortedRows {
    Memory(MaterializedRows),
    /// The input's own batches, ordered by reference.
    Columnar(columnar_sort::ColumnarSorted),
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
            Self::Columnar(sorted) => Ok(sorted.next_row()),
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
            Self::Columnar(sorted) => sorted.next_batch(column_types, memory),
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
            // JSON columns dedupe structurally: MySQL's set duplicate
            // handling treats two spellings of one document as one row.
            collation: matches!(data_type, DataType::Json)
                .then_some(pintail_sql::JSON_TEXT_COLLATION),
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
    key_collations: &[Option<String>],
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
            // The projection's own coercibility-ladder collation, so a
            // JSON-text column dedupes case-sensitively (utf8mb4_bin) even
            // when the plan default is case-insensitive.
            collation: key_collations
                .get(index)
                .and_then(|name| name.as_deref())
                .and_then(|name| {
                    if name == pintail_sql::JSON_TEXT_COLLATION {
                        return Some(pintail_sql::JSON_TEXT_COLLATION);
                    }
                    pintail_sql::SUPPORTED_TEXT_COLLATIONS
                        .into_iter()
                        .find(|supported| *supported == name)
                }),
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
        return Ok(SortedRows::Memory(MaterializedRows {
            rows,
            position: 0,
            spilled: None,
        }));
    }
    // Sorted as columns while the input fits beside its operators: its
    // batches are kept whole and only row references move. Past that the
    // rows it holds so far go to the spilling row sort with the rest.
    let threshold = memory.limit() / 2;
    let mut retained = Vec::new();
    let mut reserved = 0_usize;
    while let Some(batch) = input.next_batch(memory)? {
        let bytes = columnar_sort::retained_bytes(&batch, keys.len());
        if reserved.saturating_add(bytes) <= threshold && memory.reserve(bytes).is_ok() {
            reserved = reserved.saturating_add(bytes);
            retained.push(batch);
            continue;
        }
        memory.release(reserved);
        let mut materializer = RowMaterializer::new(keys, memory, collation);
        for held in retained.drain(..).chain(std::iter::once(batch)) {
            materializer.push(&held)?;
        }
        while let Some(batch) = input.next_batch(memory)? {
            materializer.push(&batch)?;
        }
        return sort_rows(materializer.finish(), keys, trim_to, memory, collation);
    }
    Ok(SortedRows::Columnar(columnar_sort::ColumnarSorted::new(
        retained, keys, trim_to, collation,
    )?))
}

/// The spilling row sort's tail: sorts what stayed resident and merges it
/// with the runs it spilled.
fn sort_rows(
    materialized: SpillMaterialization,
    keys: &[BoundOrderKey],
    trim_to: Option<usize>,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<SortedRows, ExecError> {
    let compare =
        |left: &Vec<Value>, right: &Vec<Value>| compare_sort_rows(left, right, keys, collation);
    let SpillMaterialization {
        mut rows,
        runs,
        reserved: rows_reserved,
    } = materialized;
    rows.sort_by(compare);
    if runs.is_empty() {
        if let Some(width) = trim_to {
            for row in &mut rows {
                row.truncate(width);
            }
        }
        return Ok(SortedRows::Memory(MaterializedRows {
            rows,
            position: 0,
            spilled: None,
        }));
    }
    let merge = SpilledMerge::new(runs, &rows, keys.to_vec(), trim_to, collation, memory)?;
    drop(rows);
    memory.release(rows_reserved);
    Ok(SortedRows::Spilled(merge))
}

/// Materializes the sort input, spilling the accumulated rows as a sorted
/// on-disk run whenever the memory ceiling would be exceeded. Queries that
/// fit in memory take exactly the old path and produce no runs.
struct SpillMaterialization {
    rows: Vec<Vec<Value>>,
    runs: Vec<spill::ClosedRun>,
    reserved: usize,
}

/// Materializes sort input as rows, spilling the accumulated rows as a
/// sorted on-disk run whenever the memory ceiling would be exceeded.
struct RowMaterializer<'sort> {
    keys: &'sort [BoundOrderKey],
    memory: &'sort MemoryTracker,
    collation: Collation,
    rows: Vec<Vec<Value>>,
    retained: usize,
    vector_reserved: usize,
    runs: Vec<spill::ClosedRun>,
}

impl<'sort> RowMaterializer<'sort> {
    const fn new(
        keys: &'sort [BoundOrderKey],
        memory: &'sort MemoryTracker,
        collation: Collation,
    ) -> Self {
        Self {
            keys,
            memory,
            collation,
            rows: Vec::new(),
            retained: 0,
            vector_reserved: 0,
            runs: Vec::new(),
        }
    }

    /// Writes the buffered rows as one sorted run and frees their whole
    /// footprint: the row payloads and the vector's capacity reservation.
    fn spill(&mut self) -> Result<(), ExecError> {
        let (keys, collation) = (self.keys, self.collation);
        self.rows
            .sort_by(|left, right| compare_sort_rows(left, right, keys, collation));
        self.runs.push(write_sorted_run(&self.rows, self.memory)?);
        self.rows = Vec::new();
        self.memory
            .release(self.retained.saturating_add(self.vector_reserved));
        self.retained = 0;
        self.vector_reserved = 0;
        Ok(())
    }

    fn push(&mut self, batch: &RecordBatch) -> Result<(), ExecError> {
        let memory = self.memory;
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        match reserve_vec_elements(&mut self.rows, additional_rows, 0, memory) {
            Ok(reserved) => self.vector_reserved = self.vector_reserved.saturating_add(reserved),
            Err(ExecError::MemoryLimitExceeded { .. }) if !self.rows.is_empty() => {
                self.spill()?;
                self.vector_reserved =
                    reserve_vec_elements(&mut self.rows, additional_rows, 0, memory)?;
            }
            Err(error) => return Err(error),
        }
        for row in batch.selection().selected_rows() {
            let row_bytes =
                estimated_batch_row_bytes(batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            match memory.reserve(row_bytes) {
                Ok(()) => {}
                Err(ExecError::MemoryLimitExceeded { .. }) if !self.rows.is_empty() => {
                    self.spill()?;
                    memory.reserve(row_bytes)?;
                }
                Err(error) => return Err(error),
            }
            self.retained = self.retained.saturating_add(row_bytes);
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "sort row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.rows.push(values);
            crate::counters::count(|counters| {
                counters.rows_sorted = counters.rows_sorted.saturating_add(1);
            });
            // Proactive spill at half the ceiling: upstream operators size
            // their own working sets from the remaining headroom, so a sort
            // that hoards the budget until hard failure starves the scan.
            if self.retained.saturating_add(self.vector_reserved) > memory.limit() / 2
                && self.rows.len() > 1
            {
                self.spill()?;
            }
        }
        Ok(())
    }

    fn finish(self) -> SpillMaterialization {
        SpillMaterialization {
            rows: self.rows,
            runs: self.runs,
            reserved: self.retained.saturating_add(self.vector_reserved),
        }
    }
}

/// Writes sorted rows as one closed run: length-framed binary rows in a
/// self-deleting temp file, read back in write order.
pub(super) fn write_sorted_run(
    rows: &[Vec<Value>],
    memory: &MemoryTracker,
) -> Result<spill::ClosedRun, ExecError> {
    let mut writer = spill::RunWriter::create("pintail-sort-spill-", memory.spill())
        .map_err(|error| ExecError::Source(format!("sort spill create: {error}")))?;
    for row in rows {
        let mut encoder = spill::Encoder::new();
        encoder.values(row);
        writer
            .write(&encoder.finish())
            .map_err(|error| ExecError::Source(format!("sort spill write: {error}")))?;
    }
    writer
        .finish()
        .map_err(|error| ExecError::Source(format!("sort spill flush: {error}")))
}

fn decode_sort_row(payload: &[u8]) -> Result<Vec<Value>, String> {
    spill::Decoder::new(payload).values()
}

/// Merge over sorted spilled runs, at most the merge fan-in of them at
/// once: more runs than that are reduced in passes that copy rows in
/// merged order, so the descriptors a spilling sort holds are bounded by
/// the fan-in and not by input bytes over the memory ceiling. With the
/// fan-in that small a linear minimum scan per row is fine.
pub(super) struct SpilledMerge {
    /// The plan's collation, for the merge comparison.
    collation: Collation,
    merge: spill::RunMerge<Vec<Value>>,
    keys: Vec<BoundOrderKey>,
    trim_to: Option<usize>,
}

impl SpilledMerge {
    /// Closes the resident rows as the final run, reduces to the fan-in
    /// and opens the merge.
    pub(super) fn new(
        mut runs: Vec<spill::ClosedRun>,
        resident: &[Vec<Value>],
        keys: Vec<BoundOrderKey>,
        trim_to: Option<usize>,
        collation: Collation,
        memory: &MemoryTracker,
    ) -> Result<Self, ExecError> {
        if !resident.is_empty() {
            runs.push(write_sorted_run(resident, memory)?);
        }
        let less = |left: &Vec<Value>, right: &Vec<Value>| {
            compare_sort_rows(left, right, &keys, collation) == Ordering::Less
        };
        let runs = spill::reduce_runs(
            runs,
            spill::MERGE_FAN_IN,
            "pintail-sort-merge-",
            memory.spill(),
            decode_sort_row,
            &less,
        )
        .map_err(|error| ExecError::Source(format!("sort spill merge: {error}")))?;
        let merge = spill::RunMerge::open(runs, decode_sort_row)
            .map_err(|error| ExecError::Source(format!("sort spill read: {error}")))?;
        Ok(Self {
            collation,
            merge,
            keys,
            trim_to,
        })
    }

    pub(super) fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        let less = |left: &Vec<Value>, right: &Vec<Value>| {
            compare_sort_rows(left, right, &self.keys, self.collation) == Ordering::Less
        };
        let Some(head) = self
            .merge
            .next(&less)
            .map_err(|error| ExecError::Source(format!("sort spill read: {error}")))?
        else {
            // Drained: release the run files now rather than when the
            // operator is dropped, which for a streamed sort is much later.
            self.merge = spill::RunMerge::empty(decode_sort_row);
            return Ok(None);
        };
        let mut row = head.key;
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
        // Sized to what the query can still afford. The per-row figure has to
        // come from the function that reserves below, so it is taken from the
        // first row once there is one - a payload-only estimate reads about a
        // tenth of what `estimated_record_batch_bytes` charges, and a cap built
        // on it does not bind.
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut planned = DEFAULT_BATCH_ROWS;
        while rows.len() < planned {
            let Some(row) = self.next_row()? else { break };
            rows.push(row);
            if rows.len() == 1 {
                let per_row = estimated_record_batch_bytes(&rows, column_types.len());
                planned = super::affordable_batch_rows(memory, per_row);
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

pub(super) fn compare_sort_rows(
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
        // An ENUM against a plain string orders by the label text; ENUM
        // pairs fall through to derived Ord below, which compares the
        // declaration index first - MySQL's rule.
        (Value::Enum { label, .. }, Value::Utf8(text)) => {
            order_direction(compare_utf8_mysql(label, text, collation), key.ascending)
        }
        (Value::Utf8(text), Value::Enum { label, .. }) => {
            order_direction(compare_utf8_mysql(text, label, collation), key.ascending)
        }
        (left, right)
            if left.text().is_some()
                && right.text().is_some()
                && !matches!((left, right), (Value::Enum { .. }, Value::Enum { .. })) =>
        {
            // A decimal carrying a narrower label compares as its value.
            let (left, right) = (sort_text(left), sort_text(right));
            let (left, right) = (left.as_ref(), right.as_ref());
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

/// A value's text as ordering reads it: a decimal carrying a narrower label
/// compares as its value at its type's scale.
fn sort_text(value: &Value) -> std::borrow::Cow<'_, str> {
    match value {
        Value::DecimalAverage(average) => std::borrow::Cow::Owned(average.canonical()),
        other => std::borrow::Cow::Borrowed(other.text().expect("guarded text")),
    }
}
