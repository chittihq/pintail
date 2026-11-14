//! The operator that expands rows through a `JSON_TABLE`.
//!
//! Each input row yields the document, the document yields the function's
//! rows, and every one of them leaves beside a copy of the input row. A
//! constant document is the same operator over the one-row input. Nothing
//! is re-planned or re-executed per input row: the document is an ordinary
//! expression over the input's columns.
//!
//! Rows are produced an input batch at a time and handed out in batches,
//! so the operator holds the expansion of one input batch at most. What it
//! holds is reserved against the query's memory ceiling as it is produced,
//! and every reservation is also where the statement's deadline and a
//! cancellation are noticed.

use std::collections::VecDeque;

use pintail_sql::BoundColumn;
use pintail_types::{DataType, Value};

use super::{
    MemoryTracker, PhysicalPlan, PullOperator, ScanProvider, batch_row, build_operator,
    estimated_row_payload_bytes, expression_has_dependent_subquery, rows_to_columns,
};
use crate::{
    DEFAULT_BATCH_ROWS, ExecError, JsonTableApply, RecordBatch,
    collation::Collation,
    expression::{CompiledExpr, JsonTableRows, predicate_truth},
};

/// Builds the operator for a `JSON_TABLE` plan node and its input.
///
/// Kept out of line: operator construction recurses through every plan
/// node, and this node's locals belong to it alone.
#[inline(never)]
#[allow(clippy::boxed_local)] // the caller hands over the node's boxes as they are
pub(super) fn build(
    input: Box<PhysicalPlan>,
    apply: Box<JsonTableApply>,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<(PullOperator, Vec<BoundColumn>), ExecError> {
    let (input, columns) = build_operator(*input, provider, memory, collation)?;
    let (operator, columns) = JsonTableOperator::build(input, columns, *apply, collation)?;
    Ok((PullOperator::JsonTable(Box::new(operator)), columns))
}

pub(super) struct JsonTableOperator {
    input: PullOperator,
    document: CompiledExpr,
    rows: JsonTableRows,
    /// `LEFT JOIN`: an input row left with no row is kept once.
    outer: bool,
    /// The `LEFT JOIN`'s `ON`, compiled against the output layout.
    condition: Option<CompiledExpr>,
    /// Types of the output: the input's columns, then the function's.
    column_types: Vec<DataType>,
    /// How many of them are the function's.
    table_width: usize,
    /// Rows produced and not yet handed out.
    pending: VecDeque<Vec<Value>>,
    /// What `pending` has reserved.
    reserved: usize,
    exhausted: bool,
}

impl JsonTableOperator {
    /// Builds the operator over `input`, whose layout is `input_columns`,
    /// and answers it with the layout it produces.
    fn build(
        input: PullOperator,
        mut input_columns: Vec<BoundColumn>,
        apply: JsonTableApply,
        collation: Collation,
    ) -> Result<(Self, Vec<BoundColumn>), ExecError> {
        let rows = JsonTableRows::compile(&apply.table)?;
        let document = CompiledExpr::compile(&apply.table.document, &input_columns, collation)?;
        let table_width = apply.columns.len();
        input_columns.extend(apply.columns);
        let columns = input_columns;
        let condition = match &apply.condition {
            // A correlated subquery is resolved row by row where a filter
            // evaluates it; this operator has no such step.
            Some(condition) if expression_has_dependent_subquery(condition) => {
                return Err(ExecError::UnsupportedOperator(
                    "JSON_TABLE in a LEFT JOIN whose ON holds a correlated subquery",
                ));
            }
            Some(condition) => Some(CompiledExpr::compile(condition, &columns, collation)?),
            None => None,
        };
        Ok((
            Self {
                input,
                document,
                rows,
                outer: apply.outer,
                condition,
                column_types: columns.iter().map(|column| column.data_type).collect(),
                table_width,
                pending: VecDeque::new(),
                reserved: 0,
                exhausted: false,
            },
            columns,
        ))
    }

    pub(super) fn release(&mut self, memory: &MemoryTracker) {
        memory.release(std::mem::take(&mut self.reserved));
        self.pending.clear();
        self.input.release_reservations(memory);
    }

    pub(super) fn next_batch(
        &mut self,
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        while self.pending.len() < DEFAULT_BATCH_ROWS && !self.exhausted {
            match self.input.next_batch(memory)? {
                Some(batch) => self.expand(&batch, memory)?,
                None => self.exhausted = true,
            }
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        let count = self.pending.len().min(DEFAULT_BATCH_ROWS);
        let chunk = self.pending.drain(..count).collect::<Vec<_>>();
        let columns = rows_to_columns(&chunk, &self.column_types)?;
        let batch = RecordBatch::new(chunk.len(), columns)?;
        let held = chunk
            .iter()
            .map(|row| estimated_row_payload_bytes(row))
            .fold(0_usize, usize::saturating_add)
            .min(self.reserved);
        self.reserved -= held;
        memory.release(held);
        memory.ensure_transient(batch.estimated_bytes())?;
        Ok(Some(batch))
    }

    /// Appends the rows of every selected row of `batch` to `pending`.
    fn expand(&mut self, batch: &RecordBatch, memory: &MemoryTracker) -> Result<(), ExecError> {
        for row in batch.selection().selected_rows() {
            let document = self.document.evaluate(batch, row)?;
            let input = batch_row(batch, row)?;
            let first = self.pending.len();
            let pending = &mut self.pending;
            let reserved = &mut self.reserved;
            self.rows.expand(&document, &mut |produced| {
                let mut values = Vec::with_capacity(input.len() + produced.len());
                values.extend_from_slice(&input);
                values.extend_from_slice(produced);
                let bytes = estimated_row_payload_bytes(&values);
                memory.reserve(bytes)?;
                *reserved = reserved.saturating_add(bytes);
                pending.push_back(values);
                Ok(())
            })?;
            if let Some(condition) = self.condition.take() {
                let kept = self.keep_matching(first, &condition, memory);
                self.condition = Some(condition);
                kept?;
            }
            if self.outer && self.pending.len() == first {
                let mut values = input;
                values.resize(values.len() + self.table_width, Value::Null);
                let bytes = estimated_row_payload_bytes(&values);
                memory.reserve(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
                self.pending.push_back(values);
            }
        }
        Ok(())
    }

    /// Drops the rows of `pending` from `first` on that `condition` does
    /// not hold for: the pairs a `LEFT JOIN`'s `ON` rejects.
    fn keep_matching(
        &mut self,
        first: usize,
        condition: &CompiledExpr,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if self.pending.len() == first {
            return Ok(());
        }
        let candidates = Vec::from(self.pending.split_off(first));
        let mut keep = Vec::with_capacity(candidates.len());
        for chunk in candidates.chunks(DEFAULT_BATCH_ROWS) {
            let columns = rows_to_columns(chunk, &self.column_types)?;
            let batch = RecordBatch::new(chunk.len(), columns)?;
            memory.ensure_transient(batch.estimated_bytes())?;
            for index in 0..chunk.len() {
                keep.push(predicate_truth(&condition.evaluate(&batch, index)?)?);
            }
        }
        for (values, keep) in candidates.into_iter().zip(keep) {
            if keep {
                self.pending.push_back(values);
            } else {
                let bytes = estimated_row_payload_bytes(&values).min(self.reserved);
                self.reserved -= bytes;
                memory.release(bytes);
            }
        }
        Ok(())
    }
}
