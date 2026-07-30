use std::{fmt, mem::size_of};

use pintail_types::{DataType, Value};

/// Target row count for pull-based executor batches.
pub const DEFAULT_BATCH_ROWS: usize = 4_096;

/// One typed, nullable, columnar value vector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnVector {
    data_type: DataType,
    values: Vec<Value>,
}

impl ColumnVector {
    /// Constructs and validates a column vector.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::WrongValueType`] when a non-null value does not
    /// match the declared logical type.
    pub fn new(data_type: DataType, values: Vec<Value>) -> Result<Self, BatchError> {
        for (row, value) in values.iter().enumerate() {
            if let Some(actual) = value.data_type()
                && actual != data_type
            {
                return Err(BatchError::WrongValueType {
                    row,
                    expected: data_type,
                    actual,
                });
            }
        }
        Ok(Self { data_type, values })
    }

    /// Returns the logical scalar type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns every physical row value, including rows masked from the
    /// current selection.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Returns one physical row value.
    #[must_use]
    pub fn value(&self, row: usize) -> Option<&Value> {
        self.values.get(row)
    }

    /// Returns the number of physical rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the vector has no physical rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Estimates bytes retained by the vector and its owned scalar payloads.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.values.capacity() * size_of::<Value>()
            + self.values.iter().map(Value::heap_bytes).sum::<usize>()
    }
}

/// Compact bit mask identifying visible physical rows in a batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionMask {
    len: usize,
    words: Vec<u64>,
}

impl SelectionMask {
    /// Selects every row in a mask of `len` rows.
    #[must_use]
    pub fn all(len: usize) -> Self {
        let mut mask = Self {
            len,
            words: vec![u64::MAX; len.div_ceil(64)],
        };
        mask.clear_unused_tail_bits();
        mask
    }

    /// Selects no rows in a mask of `len` rows.
    #[must_use]
    pub fn none(len: usize) -> Self {
        Self {
            len,
            words: vec![0; len.div_ceil(64)],
        }
    }

    /// Returns the number of physical rows represented by the mask.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mask represents no physical rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns whether a physical row is selected.
    #[must_use]
    pub fn is_selected(&self, row: usize) -> bool {
        row < self.len && self.words[row / 64] & (1_u64 << (row % 64)) != 0
    }

    /// Changes whether a physical row is selected.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::RowOutOfBounds`] when `row` is outside the mask.
    pub fn set(&mut self, row: usize, selected: bool) -> Result<(), BatchError> {
        if row >= self.len {
            return Err(BatchError::RowOutOfBounds {
                row,
                row_count: self.len,
            });
        }
        let bit = 1_u64 << (row % 64);
        if selected {
            self.words[row / 64] |= bit;
        } else {
            self.words[row / 64] &= !bit;
        }
        Ok(())
    }

    /// Intersects this mask with another mask of equal length.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::SelectionLength`] when the masks describe
    /// different physical row counts.
    pub fn intersect(&mut self, other: &Self) -> Result<(), BatchError> {
        if self.len != other.len {
            return Err(BatchError::SelectionLength {
                expected: self.len,
                actual: other.len,
            });
        }
        for (word, other_word) in self.words.iter_mut().zip(&other.words) {
            *word &= other_word;
        }
        Ok(())
    }

    /// Returns the number of selected rows.
    #[must_use]
    pub fn count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Iterates selected physical row indexes in ascending order.
    #[must_use]
    pub fn selected_rows(&self) -> SelectedRows<'_> {
        SelectedRows {
            mask: self,
            next: 0,
        }
    }

    fn clear_unused_tail_bits(&mut self) {
        let used_tail_bits = self.len % 64;
        if used_tail_bits == 0 {
            return;
        }
        if let Some(last) = self.words.last_mut() {
            *last &= (1_u64 << used_tail_bits) - 1;
        }
    }
}

/// Iterator over selected physical row indexes.
pub struct SelectedRows<'mask> {
    mask: &'mask SelectionMask,
    next: usize,
}

impl Iterator for SelectedRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.mask.len {
            let row = self.next;
            self.next += 1;
            if self.mask.is_selected(row) {
                return Some(row);
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.mask.len.saturating_sub(self.next);
        (0, Some(remaining))
    }
}

impl std::iter::FusedIterator for SelectedRows<'_> {}

/// A columnar executor batch with a shared row-selection mask.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordBatch {
    row_count: usize,
    columns: Vec<ColumnVector>,
    selection: SelectionMask,
}

impl RecordBatch {
    /// Constructs a batch with every physical row initially selected.
    ///
    /// The explicit row count permits zero-column batches for relational
    /// values such as the one-row input to `SELECT 1`.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::ColumnLength`] when any column length differs
    /// from `row_count`.
    pub fn new(row_count: usize, columns: Vec<ColumnVector>) -> Result<Self, BatchError> {
        for (column, values) in columns.iter().enumerate() {
            if values.len() != row_count {
                return Err(BatchError::ColumnLength {
                    column,
                    expected: row_count,
                    actual: values.len(),
                });
            }
        }
        Ok(Self {
            row_count,
            columns,
            selection: SelectionMask::all(row_count),
        })
    }

    /// Returns the number of physical rows before selection.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the typed column vectors.
    #[must_use]
    pub fn columns(&self) -> &[ColumnVector] {
        &self.columns
    }

    /// Returns a typed column vector by physical index.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&ColumnVector> {
        self.columns.get(index)
    }

    /// Returns the shared selection mask.
    #[must_use]
    pub const fn selection(&self) -> &SelectionMask {
        &self.selection
    }

    /// Returns mutable access to the shared selection mask.
    pub const fn selection_mut(&mut self) -> &mut SelectionMask {
        &mut self.selection
    }

    /// Replaces the shared selection mask.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::SelectionLength`] when the mask does not
    /// describe this batch's physical row count.
    pub fn set_selection(&mut self, selection: SelectionMask) -> Result<(), BatchError> {
        if selection.len() != self.row_count {
            return Err(BatchError::SelectionLength {
                expected: self.row_count,
                actual: selection.len(),
            });
        }
        self.selection = selection;
        Ok(())
    }

    /// Returns the number of currently visible rows.
    #[must_use]
    pub fn visible_row_count(&self) -> usize {
        self.selection.count()
    }

    /// Estimates bytes retained by the batch, its columns, and its selection
    /// mask.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .columns
                .iter()
                .map(ColumnVector::estimated_bytes)
                .sum::<usize>()
            + self.selection.words.capacity() * size_of::<u64>()
    }
}

/// Column-vector or selection-mask invariant violation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchError {
    /// A value does not match its vector's declared type.
    WrongValueType {
        /// Physical row index.
        row: usize,
        /// Vector type.
        expected: DataType,
        /// Value type.
        actual: DataType,
    },
    /// A column does not match its batch's physical row count.
    ColumnLength {
        /// Physical column index.
        column: usize,
        /// Batch row count.
        expected: usize,
        /// Column row count.
        actual: usize,
    },
    /// A selection mask does not match the expected row count.
    SelectionLength {
        /// Required row count.
        expected: usize,
        /// Supplied row count.
        actual: usize,
    },
    /// A requested physical row is outside the batch.
    RowOutOfBounds {
        /// Requested row.
        row: usize,
        /// Available physical rows.
        row_count: usize,
    },
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongValueType {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row} has type {actual:?}; vector requires {expected:?}"
            ),
            Self::ColumnLength {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column} has {actual} rows; batch requires {expected}"
            ),
            Self::SelectionLength { expected, actual } => write!(
                formatter,
                "selection has {actual} rows; batch requires {expected}"
            ),
            Self::RowOutOfBounds { row, row_count } => {
                write!(formatter, "row {row} is outside batch length {row_count}")
            }
        }
    }
}

impl std::error::Error for BatchError {}

#[cfg(test)]
mod tests {
    use pintail_types::{DataType, Value};

    use super::{BatchError, ColumnVector, RecordBatch, SelectionMask};

    #[test]
    fn validates_vector_types_and_batch_lengths() {
        let error = ColumnVector::new(
            DataType::Int64,
            vec![Value::Int64(1), Value::Utf8("wrong".to_owned())],
        )
        .expect_err("wrong type");
        assert_eq!(
            error,
            BatchError::WrongValueType {
                row: 1,
                expected: DataType::Int64,
                actual: DataType::Utf8
            }
        );

        let column = ColumnVector::new(DataType::Int64, vec![Value::Int64(1)]).expect("column");
        assert_eq!(
            RecordBatch::new(2, vec![column]),
            Err(BatchError::ColumnLength {
                column: 0,
                expected: 2,
                actual: 1
            })
        );
    }

    #[test]
    fn permits_nullable_and_zero_column_batches() {
        let column = ColumnVector::new(
            DataType::Utf8,
            vec![Value::Utf8("value".to_owned()), Value::Null],
        )
        .expect("nullable vector");
        let batch = RecordBatch::new(2, vec![column]).expect("batch");
        assert_eq!(batch.visible_row_count(), 2);

        let one_row = RecordBatch::new(1, Vec::new()).expect("one row");
        assert_eq!(one_row.row_count(), 1);
        assert!(one_row.columns().is_empty());
    }

    #[test]
    fn selection_masks_cover_word_boundaries_without_tail_bits() {
        let mut mask = SelectionMask::all(65);
        assert_eq!(mask.count(), 65);
        mask.set(0, false).expect("row zero");
        mask.set(64, false).expect("row 64");
        assert_eq!(mask.count(), 63);
        assert_eq!(
            mask.selected_rows().collect::<Vec<_>>(),
            (1..64).collect::<Vec<_>>()
        );
        assert_eq!(
            mask.set(65, true),
            Err(BatchError::RowOutOfBounds {
                row: 65,
                row_count: 65
            })
        );
    }

    #[test]
    fn intersects_equal_length_masks() {
        let mut left = SelectionMask::all(5);
        left.set(1, false).expect("row");
        let mut right = SelectionMask::none(5);
        right.set(1, true).expect("row");
        right.set(2, true).expect("row");
        left.intersect(&right).expect("equal masks");
        assert_eq!(left.selected_rows().collect::<Vec<_>>(), [2]);

        assert_eq!(
            left.intersect(&SelectionMask::all(4)),
            Err(BatchError::SelectionLength {
                expected: 5,
                actual: 4
            })
        );
    }
}
