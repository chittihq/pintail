//! A query's rows as execution produced them.
//!
//! Rows used to be collected as values before anything encoded them: every
//! cell of every column materialized, then cloned into a row. They are now
//! kept as the column batches execution produced, and the wire encodes
//! straight from those. Rows of values are built only for a caller that
//! reads them as values - the HTTP API, a multi-statement merge, tests -
//! and only once.

use std::sync::OnceLock;

use pintail_exec::RecordBatch;
use pintail_types::Value;

/// The rows of one result.
#[derive(Clone, Debug, Default)]
pub struct ResultRows {
    chunks: Vec<Chunk>,
    len: usize,
    /// Every row as values, built on the first read as values. Once built
    /// it is the rows: a caller that changes them changes these.
    values: OnceLock<Vec<Vec<Value>>>,
}

/// One run of rows, in result order.
#[derive(Clone, Debug)]
enum Chunk {
    /// A batch whose selected rows are result rows.
    Batch(RecordBatch),
    /// Rows already held as values.
    Rows(Vec<Vec<Value>>),
}

/// Where an encoder reads a run of rows from.
pub(crate) enum RowSource<'rows> {
    /// A batch's selected rows, cell by cell.
    Batch(&'rows RecordBatch),
    /// Rows of values.
    Values(&'rows [Vec<Value>]),
}

impl ResultRows {
    /// Appends a batch's selected rows.
    ///
    /// A batch keeps every row of its columns alive, selected or not, so a
    /// batch whose selection has dropped most of its rows gives up its
    /// selected ones as values instead: what the result holds stays in
    /// proportion to the rows it returns.
    pub(crate) fn push_batch(&mut self, batch: RecordBatch) {
        let selected = batch.visible_row_count();
        if selected == 0 {
            return;
        }
        self.len += selected;
        if selected.saturating_mul(2) < batch.row_count() {
            let rows = batch
                .selection()
                .selected_rows()
                .map(|row| {
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value_owned(row).unwrap_or(Value::Null))
                        .collect()
                })
                .collect();
            self.chunks.push(Chunk::Rows(rows));
        } else {
            self.chunks.push(Chunk::Batch(batch));
        }
    }

    /// How many rows the result holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.get().map_or(self.len, Vec::len)
    }

    /// Whether the result holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The runs of rows in order, for an encoder: the rows as values once
    /// they have been read as values, the batches before that.
    pub(crate) fn sources(&self) -> Vec<RowSource<'_>> {
        if let Some(values) = self.values.get() {
            return vec![RowSource::Values(values)];
        }
        self.chunks
            .iter()
            .map(|chunk| match chunk {
                Chunk::Batch(batch) => RowSource::Batch(batch),
                Chunk::Rows(rows) => RowSource::Values(rows),
            })
            .collect()
    }

    /// Every row as values.
    #[must_use]
    pub fn into_values(mut self) -> Vec<Vec<Value>> {
        self.materialize();
        self.values.take().unwrap_or_default()
    }

    fn materialize(&self) -> &Vec<Vec<Value>> {
        self.values.get_or_init(|| {
            let mut rows = Vec::with_capacity(self.len);
            for chunk in &self.chunks {
                match chunk {
                    Chunk::Rows(values) => rows.extend(values.iter().cloned()),
                    Chunk::Batch(batch) => {
                        for row in batch.selection().selected_rows() {
                            rows.push(
                                batch
                                    .columns()
                                    .iter()
                                    .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                                    .collect(),
                            );
                        }
                    }
                }
            }
            rows
        })
    }
}

impl From<Vec<Vec<Value>>> for ResultRows {
    fn from(rows: Vec<Vec<Value>>) -> Self {
        Self {
            len: rows.len(),
            chunks: vec![Chunk::Rows(rows)],
            values: OnceLock::new(),
        }
    }
}

impl FromIterator<Vec<Value>> for ResultRows {
    fn from_iter<I: IntoIterator<Item = Vec<Value>>>(rows: I) -> Self {
        rows.into_iter().collect::<Vec<_>>().into()
    }
}

impl std::ops::Deref for ResultRows {
    type Target = [Vec<Value>];

    fn deref(&self) -> &Self::Target {
        self.materialize()
    }
}

impl std::ops::DerefMut for ResultRows {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.materialize();
        // The values are now the rows; the batches would only go stale.
        self.chunks.clear();
        self.values.get_mut().expect("materialized above")
    }
}

impl PartialEq for ResultRows {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl PartialEq<Vec<Vec<Value>>> for ResultRows {
    fn eq(&self, other: &Vec<Vec<Value>>) -> bool {
        **self == **other
    }
}

impl<'rows> IntoIterator for &'rows ResultRows {
    type Item = &'rows Vec<Value>;
    type IntoIter = std::slice::Iter<'rows, Vec<Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use pintail_exec::{ColumnVector, RecordBatch, SelectionMask};
    use pintail_types::{DataType, Value};

    use super::{ResultRows, RowSource};

    fn batch(ids: &[u64], selected: &[usize]) -> RecordBatch {
        let column = ColumnVector::new(
            DataType::UInt64,
            ids.iter().copied().map(Value::UInt64).collect(),
        )
        .expect("column");
        let mut batch = RecordBatch::new(ids.len(), vec![column]).expect("batch");
        let mut selection = SelectionMask::none(ids.len());
        for row in selected {
            selection.set(*row, true).expect("row");
        }
        batch.set_selection(selection).expect("selection");
        batch
    }

    #[test]
    fn selected_rows_read_back_in_order_as_values() {
        let mut rows = ResultRows::default();
        rows.push_batch(batch(&[1, 2, 3, 4], &[0, 1, 3]));
        rows.push_batch(batch(&[5, 6], &[0, 1]));
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows,
            [1, 2, 4, 5, 6].map(|id| vec![Value::UInt64(id)]).to_vec()
        );
    }

    #[test]
    fn a_mostly_filtered_batch_keeps_only_its_selected_rows() {
        let mut rows = ResultRows::default();
        rows.push_batch(batch(&[1, 2, 3, 4, 5], &[4]));
        rows.push_batch(batch(&[6, 7], &[0, 1]));
        let kinds = rows
            .sources()
            .iter()
            .map(|source| matches!(source, RowSource::Batch(_)))
            .collect::<Vec<_>>();
        assert_eq!(kinds, [false, true], "the sparse batch became values");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn rows_changed_as_values_are_what_an_encoder_reads() {
        let mut rows = ResultRows::default();
        rows.push_batch(batch(&[1, 2], &[0, 1]));
        rows[0][0] = Value::UInt64(9);
        let sources = rows.sources();
        let [RowSource::Values(values)] = sources.as_slice() else {
            panic!("the changed rows are the only source");
        };
        assert_eq!(values[0], vec![Value::UInt64(9)]);
    }
}
