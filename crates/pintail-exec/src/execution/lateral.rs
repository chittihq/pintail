//! `LATERAL` derived tables: dependent execution whose answer is a table.
//!
//! A lateral derived table reads columns of the relations written to its
//! left, so it has no single result: each row of that input sees the rows
//! the table's query produces for its values. This is the dependent
//! subquery path with rows where a scalar subquery has one value. For each
//! input row the query is cloned, the row's values are written into it as
//! literals, and the result is planned and run - once per DISTINCT tuple of
//! those values, because the statement-local memo answers every later row
//! that substitutes the same tuple.
//!
//! The join to the input is done here as the pairs are produced: an inner
//! join emits each accepted pair, and a join that preserves its input
//! (`LEFT JOIN LATERAL`) emits a row with NULL derived columns for an input
//! row that no derived row is accepted for.
//!
//! Every inner execution runs under what is left of the query's memory
//! ceiling and under the statement's deadline, the output rows and the
//! memo's entries are charged to the query's tracker, and a volatile query
//! (`RAND()`, `UUID()`) is never memoized.

use std::mem::size_of;

use pintail_sql::{BoundColumn, BoundExpr, BoundQuery};
use pintail_types::{DataType, Value};

use super::memo::DependentMemo;
use super::{
    DEPENDENT_SUBQUERY_EXECUTIONS, ExecError, Execution, MemoryScope, MemoryTracker,
    PhysicalPlanner, PullOperator, ScanProvider, batch_row, dependent_subquery_memory_limit,
    estimated_row_payload_bytes, record_dependent_memo, rows_to_columns, subquery_tables,
    substitute_outer_query,
};
use crate::collation::Collation;
use crate::{
    LogicalPlanner, Optimizer, RecordBatch,
    expression::{CompiledExpr, predicate_truth},
};

/// The one slot a lateral table's memo holds: the table's own query.
const QUERY_SLOT: usize = 0;

/// What a lateral join reads and how it joins.
pub(super) struct LateralJoin<'a> {
    /// Columns of the input rows, in batch order.
    pub(super) input_columns: &'a [BoundColumn],
    /// The derived table's query, outer references unresolved.
    pub(super) query: &'a BoundQuery,
    /// The derived table's columns.
    pub(super) columns: &'a [BoundColumn],
    /// Whether an input row with no accepted derived row is kept.
    pub(super) preserve_input: bool,
    /// The ON condition over input and derived columns.
    pub(super) condition: Option<&'a BoundExpr>,
}

/// Runs a lateral join to completion: every input row paired with the rows
/// its values produce.
pub(super) fn execute_lateral_join(
    input: &mut PullOperator,
    join: &LateralJoin<'_>,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut output_columns = join.input_columns.to_vec();
    output_columns.extend_from_slice(join.columns);
    let column_types = output_columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<DataType>>();
    let width = join.columns.len();
    if width == 0 {
        return Err(ExecError::InvalidPhysicalPlan(
            "LATERAL derived table has no columns",
        ));
    }
    let condition = join
        .condition
        .map(|condition| CompiledExpr::compile(condition, &output_columns, collation))
        .transpose()?;
    let mut memo = DependentMemo::for_query(join.query);
    let mut rows = Vec::new();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            memory.check_interruption()?;
            let derived = derived_rows(join, &batch, row, provider, memory, collation, &mut memo)?;
            let left = batch_row(&batch, row)?;
            let mut matched = false;
            for values in derived.chunks(width) {
                let mut candidate = left.clone();
                candidate.extend(values.iter().cloned());
                if let Some(condition) = &condition {
                    let vectors = rows_to_columns(std::slice::from_ref(&candidate), &column_types)?;
                    let pair = RecordBatch::new(1, vectors)?;
                    if !predicate_truth(&condition.evaluate(&pair, 0)?)? {
                        continue;
                    }
                }
                matched = true;
                keep_row(&mut rows, candidate, batch_bytes, memory)?;
            }
            if !matched && join.preserve_input {
                let mut candidate = left;
                candidate.extend(std::iter::repeat_n(Value::Null, width));
                keep_row(&mut rows, candidate, batch_bytes, memory)?;
            }
        }
    }
    record_dependent_memo(memo.finish(memory));
    Ok(rows)
}

/// Charges one output row to the query and keeps it.
fn keep_row(
    rows: &mut Vec<Vec<Value>>,
    row: Vec<Value>,
    batch_bytes: usize,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let row_bytes = estimated_row_payload_bytes(&row);
    memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
    memory.reserve(row_bytes)?;
    rows.push(row);
    Ok(())
}

/// The derived table's rows for one input row, row after row in one
/// vector: from the memo when the row's outer tuple has been seen, and by
/// planning and running the query for it otherwise.
fn derived_rows(
    join: &LateralJoin<'_>,
    batch: &RecordBatch,
    row: usize,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
    memo: &mut DependentMemo,
) -> Result<Vec<Value>, ExecError> {
    let mut query = join.query.clone();
    let mut key = Vec::new();
    substitute_outer_query(&mut query, batch, row, join.input_columns, &mut key)?;
    if let Some(values) = memo.get(QUERY_SLOT, &key) {
        return Ok(values);
    }
    memory.note_subquery("lateral execution", &subquery_tables(&query), 1);
    let memory_limit = dependent_subquery_memory_limit(memory, batch)?;
    DEPENDENT_SUBQUERY_EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let logical = Optimizer::optimize(LogicalPlanner::plan(query));
    let physical = PhysicalPlanner::plan(logical, collation)?;
    let width = join.columns.len();
    if physical.output_fields().len() != width {
        return Err(ExecError::InvalidPhysicalPlan(
            "LATERAL derived table layout does not match its bound columns",
        ));
    }
    let mut execution = Execution::start_with_deadline(
        physical,
        provider,
        memory_limit,
        memory.deadline,
        collation,
    )?;
    let mut values = Vec::new();
    let mut used = size_of::<Vec<Value>>();
    while let Some(batch) = execution.next_batch()? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            for column in 0..width {
                let value = batch
                    .column(column)
                    .and_then(|values| values.value(row))
                    .cloned()
                    .ok_or(ExecError::InvalidBatch(
                        "LATERAL derived table row is missing a column",
                    ))?;
                let bytes = size_of::<Value>().saturating_add(value.heap_bytes());
                let live_bytes = execution
                    .memory()
                    .used()
                    .saturating_add(batch_bytes)
                    .saturating_add(used);
                if live_bytes.saturating_add(bytes) > memory_limit {
                    return Err(ExecError::MemoryLimitExceeded {
                        used: live_bytes,
                        requested: bytes,
                        limit: memory_limit,
                        scope: MemoryScope::Query,
                    });
                }
                used += bytes;
                values.push(value);
            }
        }
    }
    drop(execution);
    memo.insert(memory, QUERY_SLOT, key, &values);
    Ok(values)
}
