use std::{fmt, mem::size_of};

use pintail_types::{DataType, Value};

use crate::array::{StrColumn, ValidityMask};

/// Target row count for pull-based executor batches.
pub const DEFAULT_BATCH_ROWS: usize = 4_096;

/// The text carrier for a packed column, regenerated from the packed units
/// only when a text-shaped consumer first asks. Scan-born native-unit
/// columns whose consumers stay numeric (aggregates, comparisons) never
/// format a single string.
#[derive(Debug)]
pub(crate) struct LazyText {
    kind: TextKind,
    cell: std::sync::OnceLock<StrColumn>,
}

#[derive(Clone, Copy, Debug)]
enum TextKind {
    /// Pre-built at construction (value-born columns keep original text).
    Ready,
    Decimal {
        scale: u8,
    },
    Date,
    DateTime {
        fsp: u8,
    },
}

impl Clone for LazyText {
    fn clone(&self) -> Self {
        let cell = std::sync::OnceLock::new();
        if let Some(built) = self.cell.get() {
            let _ = cell.set(built.clone());
        }
        Self {
            kind: self.kind,
            cell,
        }
    }
}

impl LazyText {
    pub(crate) fn ready(text: StrColumn) -> Self {
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(text);
        Self {
            kind: TextKind::Ready,
            cell,
        }
    }

    pub(crate) const fn decimal(scale: u8) -> Self {
        Self {
            kind: TextKind::Decimal { scale },
            cell: std::sync::OnceLock::new(),
        }
    }

    pub(crate) const fn date() -> Self {
        Self {
            kind: TextKind::Date,
            cell: std::sync::OnceLock::new(),
        }
    }

    pub(crate) const fn datetime(fsp: u8) -> Self {
        Self {
            kind: TextKind::DateTime { fsp },
            cell: std::sync::OnceLock::new(),
        }
    }

    /// The built text, if any consumer has forced it yet.
    fn built(&self) -> Option<&StrColumn> {
        self.cell.get()
    }

    /// Builds (once) and returns the text for `units`-backed rows; null rows
    /// hold empty views. `Ready` carriers were set at construction.
    fn force(&self, units: &[i64], validity: &ValidityMask) -> &StrColumn {
        self.cell.get_or_init(|| {
            let mut column = StrColumn::default();
            for (row, unit) in units.iter().enumerate() {
                if !validity.is_valid(row) {
                    column.push(&[]);
                    continue;
                }
                let text = match self.kind {
                    TextKind::Ready => unreachable!("ready text is set at construction"),
                    TextKind::Decimal { scale } => {
                        pintail_types::format_decimal_scaled(i128::from(*unit), scale)
                    }
                    TextKind::Date => pintail_types::format_date_days(*unit)
                        .expect("stored date units round-trip"),
                    TextKind::DateTime { fsp } => pintail_types::format_datetime_micros(*unit, fsp)
                        .expect("stored datetime units round-trip"),
                };
                column.push(text.as_bytes());
            }
            column
        })
    }
}

/// Packed physical values for homogeneous batches, built once at vector
/// construction so kernels never re-match `Value` per row
/// (docs/decisions.md, "Executor moves to typed packed arrays").
#[derive(Clone, Debug)]
pub(crate) enum TypedValues {
    Int64(Vec<i64>),
    UInt64(Vec<u64>),
    Float64(Vec<f64>),
    Utf8(StrColumn),
    /// Scaled-integer decimals parsed once at vector construction from the
    /// canonical text carrier (docs/decisions.md, "Decimals and dates execute
    /// natively"). `values[i] = decimal * 10^scale`. The scale rides along
    /// for the aggregation kernels that consume it next.
    #[allow(dead_code)] // scale consumed by the upcoming typed SUM/AVG kernels
    Decimal128 {
        values: Vec<i128>,
        scale: u8,
        /// Canonical text carrier, regenerated on demand.
        text: LazyText,
    },
    /// Temporal values parsed once from their canonical text carrier into
    /// comparable integers (days for `Date32`, microseconds for `DateTime64`).
    /// Views are retained so text-shaped consumers keep working; `number_at`
    /// deliberately refuses temporals — `MySQL` numeric coercion of date text
    /// is NOT the epoch integer.
    Temporal {
        units: Vec<i64>,
        text: LazyText,
    },
}

pub(crate) use pintail_types::{parse_date_days, parse_datetime_micros, parse_decimal_scaled};

/// splitmix64 finalizer: cheap, well-distributed mixing for local group hashes.
#[inline]
pub(crate) const fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl TypedValues {
    /// The column's text views, built on first use for native-unit columns.
    /// `None` for shapes with no text form.
    pub(crate) fn text_column(&self, validity: &ValidityMask) -> Option<&StrColumn> {
        match self {
            Self::Utf8(column) => Some(column),
            Self::Temporal { units, text } => Some(text.force(units, validity)),
            Self::Decimal128 { values, text, .. } => Some(text.cell.get_or_init(|| {
                let mut column = StrColumn::default();
                let TextKind::Decimal { scale } = text.kind else {
                    unreachable!("decimal carriers regenerate with a scale")
                };
                for (row, value) in values.iter().enumerate() {
                    if validity.is_valid(row) {
                        let formatted = pintail_types::format_decimal_scaled(*value, scale);
                        column.push(formatted.as_bytes());
                    } else {
                        column.push(&[]);
                    }
                }
                column
            })),
            Self::Int64(_) | Self::UInt64(_) | Self::Float64(_) => None,
        }
    }

    /// Returns the number of packed rows.
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::UInt64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Utf8(column) => column.len(),
            Self::Decimal128 { values, .. } => values.len(),
            Self::Temporal { units, .. } => units.len(),
        }
    }

    /// A cheap per-row hash for LOCAL group routing (never crosses batches —
    /// cross-batch merging uses normalized value keys, so any within-batch
    /// consistent function is correct). `None` when the row must fall back to
    /// the `Value` hash path.
    pub(crate) fn group_hash_at(&self, row: usize, validity: &ValidityMask) -> Option<u64> {
        const NULL_SENTINEL: u64 = 0x6b8b_4567_327b_23c6;
        if !validity.is_valid(row) {
            return Some(NULL_SENTINEL);
        }
        Some(match self {
            Self::Int64(values) => mix64(u64::from_ne_bytes(values.get(row)?.to_ne_bytes()) ^ 0x01),
            Self::UInt64(values) => mix64(*values.get(row)? ^ 0x02),
            Self::Float64(values) => mix64(values.get(row)?.to_bits() ^ 0x03),
            Self::Utf8(column) => {
                let (head, tail) = column.views().get(row)?.hash_words();
                mix64(head) ^ mix64(tail ^ 0x04)
            }
            Self::Decimal128 { values, .. } => {
                let bytes = values.get(row)?.to_ne_bytes();
                let low = u64::from_ne_bytes(bytes[..8].try_into().expect("8 bytes"));
                let high = u64::from_ne_bytes(bytes[8..].try_into().expect("8 bytes"));
                mix64(low ^ 0x05) ^ mix64(high)
            }
            Self::Temporal { units, .. } => {
                mix64(u64::from_ne_bytes(units.get(row)?.to_ne_bytes()) ^ 0x06)
            }
        })
    }

    /// The row's numeric value for float-accumulating aggregates, straight
    /// from packed storage. Matches `mysql_f64` semantics bit-for-bit inside
    /// f64's exact integer range: dividing an exactly-represented scaled
    /// integer by an exact power of ten is correctly rounded, the same result
    /// text parsing produces.
    /// One row's scaled/packed integer units, when this vector carries them.
    pub(crate) fn units_at(&self, row: usize) -> Option<i128> {
        match self {
            Self::Decimal128 { values, .. } => values.get(row).copied(),
            Self::Temporal { units, .. } => units.get(row).copied().map(i128::from),
            _ => None,
        }
    }

    /// The decimal scale when this vector carries scaled decimal units.
    pub(crate) fn decimal_scale(&self) -> Option<u8> {
        match self {
            Self::Decimal128 { scale, .. } => Some(*scale),
            _ => None,
        }
    }

    /// Formats ONE row's canonical text from its units without forcing the
    /// whole column's lazy text (phase-0 2026-08-02: whole-column forcing
    /// was the dominant residue of the string-keyed aggregate paths).
    pub(crate) fn format_unit(&self, row: usize) -> Option<String> {
        match self {
            Self::Decimal128 { values, scale, .. } => Some(pintail_types::format_decimal_scaled(
                *values.get(row)?,
                *scale,
            )),
            Self::Temporal { units, text } => {
                let unit = *units.get(row)?;
                match text.kind {
                    TextKind::Date => pintail_types::format_date_days(unit),
                    TextKind::DateTime { fsp } => pintail_types::format_datetime_micros(unit, fsp),
                    TextKind::Ready | TextKind::Decimal { .. } => None,
                }
            }
            _ => None,
        }
    }

    pub(crate) fn number_at(&self, row: usize) -> Option<f64> {
        const POW10: [f64; 19] = [
            1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
            1e16, 1e17, 1e18,
        ];
        match self {
            Self::Int64(values) => {
                let value = *values.get(row)?;
                #[allow(clippy::cast_precision_loss)]
                Some(value as f64)
            }
            Self::UInt64(values) => {
                let value = *values.get(row)?;
                #[allow(clippy::cast_precision_loss)]
                Some(value as f64)
            }
            Self::Float64(values) => values.get(row).copied(),
            Self::Decimal128 { values, scale, .. } => {
                let value = *values.get(row)?;
                #[allow(clippy::cast_precision_loss)]
                Some(value as f64 / POW10[usize::from(*scale).min(18)])
            }
            Self::Utf8(_) | Self::Temporal { .. } => None,
        }
    }
}

/// Builds the packed projection for a homogeneous column: one builder chosen
/// by the declared type's physical carrier, `None` when values defeat packing
/// (mixed variants, unparseable decimal text, empty column).
#[allow(clippy::too_many_lines)]
fn build_typed(data_type: DataType, values: &[Value]) -> Option<(TypedValues, ValidityMask)> {
    if values.is_empty() {
        return None;
    }
    let mut validity = Vec::with_capacity(values.len());
    let storage = data_type.storage_type();
    let mut int64 = matches!(storage, DataType::Int64).then(|| Vec::with_capacity(values.len()));
    let mut uint64 = matches!(storage, DataType::UInt64).then(|| Vec::with_capacity(values.len()));
    let mut float64 =
        matches!(storage, DataType::Float64).then(|| Vec::with_capacity(values.len()));
    let mut utf8 = matches!(storage, DataType::Utf8).then(StrColumn::default);
    let decimal_scale = match data_type {
        DataType::Decimal { scale, .. } => Some(scale),
        _ => None,
    };
    let mut decimal = decimal_scale.map(|_| Vec::with_capacity(values.len()));
    // true = microseconds (DateTime64), false = days (Date32)
    let temporal_kind = match data_type {
        DataType::Date32 => Some(false),
        DataType::DateTime64 { .. } => Some(true),
        _ => None,
    };
    let mut temporal = temporal_kind.map(|_| Vec::with_capacity(values.len()));
    for value in values {
        validity.push(!matches!(value, Value::Null));
        match value {
            Value::Null => {
                if let Some(packed) = int64.as_mut() {
                    packed.push(0);
                }
                if let Some(packed) = uint64.as_mut() {
                    packed.push(0);
                }
                if let Some(packed) = float64.as_mut() {
                    packed.push(0.0);
                }
                if let Some(packed) = utf8.as_mut() {
                    packed.push(&[]);
                }
                if let Some(packed) = decimal.as_mut() {
                    packed.push(0);
                }
                if let Some(packed) = temporal.as_mut() {
                    packed.push(0);
                }
            }
            Value::Int64(v) => {
                if let Some(packed) = int64.as_mut() {
                    packed.push(*v);
                }
                uint64 = None;
                float64 = None;
                utf8 = None;
                decimal = None;
                temporal = None;
            }
            Value::UInt64(v) => {
                if let Some(packed) = uint64.as_mut() {
                    packed.push(*v);
                }
                int64 = None;
                float64 = None;
                utf8 = None;
                decimal = None;
                temporal = None;
            }
            Value::Float64(v) => {
                if let Some(packed) = float64.as_mut() {
                    packed.push(v.get());
                }
                int64 = None;
                uint64 = None;
                utf8 = None;
                decimal = None;
                temporal = None;
            }
            Value::Utf8(text) => {
                if let Some(packed) = utf8.as_mut() {
                    packed.push(text.as_bytes());
                }
                if let (Some(packed), Some(scale)) = (decimal.as_mut(), decimal_scale) {
                    match parse_decimal_scaled(text, scale) {
                        Some(scaled) => packed.push(scaled),
                        None => decimal = None,
                    }
                }
                if let (Some(packed), Some(micros)) = (temporal.as_mut(), temporal_kind) {
                    let parsed = if micros {
                        parse_datetime_micros(text)
                    } else {
                        parse_date_days(text)
                    };
                    match parsed {
                        Some(units) => packed.push(units),
                        None => temporal = None,
                    }
                }
                int64 = None;
                uint64 = None;
                float64 = None;
            }
            Value::Boolean(_) | Value::Binary(_) => {
                int64 = None;
                uint64 = None;
                float64 = None;
                utf8 = None;
                decimal = None;
                temporal = None;
            }
        }
    }
    let typed = if let (Some(packed), Some(scale)) = (decimal.take(), decimal_scale) {
        // Decimal outranks the Utf8 carrier: kernels get scaled integers; the
        // text views ride along for lazy row-value materialization.
        utf8.take().map(|text| TypedValues::Decimal128 {
            values: packed,
            scale,
            text: LazyText::ready(text),
        })
    } else if let Some(units) = temporal.take() {
        // temporal is only alive for Date32/DateTime64 columns whose every
        // non-null value parsed; the text views must exist alongside it.
        utf8.take().map(|text| TypedValues::Temporal {
            units,
            text: LazyText::ready(text),
        })
    } else if let Some(packed) = int64 {
        Some(TypedValues::Int64(packed))
    } else if let Some(packed) = uint64 {
        Some(TypedValues::UInt64(packed))
    } else if let Some(packed) = float64 {
        Some(TypedValues::Float64(packed))
    } else {
        utf8.map(TypedValues::Utf8)
    };
    typed.map(|packed| (packed, ValidityMask::from_bools(&validity)))
}

/// One typed, nullable, columnar value vector.
///
/// Exactly one of the two representations is populated at construction —
/// row values ([`ColumnVector::new`]) or a packed typed projection
/// ([`ColumnVector::from_typed`], the scan path) — and the other builds
/// lazily on first use, so consumers that stay on one side never pay for
/// the other.
#[derive(Debug)]
pub struct ColumnVector {
    data_type: DataType,
    len: usize,
    /// Lazily-materialized row values: typed-born scan batches whose
    /// consumers stay on packed kernels never allocate a single `Value`.
    values: std::sync::OnceLock<Vec<Value>>,
    /// Lazily-built packed projection: batches whose kernels never touch it
    /// (projections, join intermediates, fallback-only filters) pay nothing.
    typed: std::sync::OnceLock<Option<(TypedValues, ValidityMask)>>,
}

impl Clone for ColumnVector {
    fn clone(&self) -> Self {
        let values = std::sync::OnceLock::new();
        if let Some(built) = self.values.get() {
            let _ = values.set(built.clone());
        }
        let typed = std::sync::OnceLock::new();
        if let Some(built) = self.typed.get() {
            let _ = typed.set(built.clone());
        }
        Self {
            data_type: self.data_type,
            len: self.len,
            values,
            typed,
        }
    }
}

impl PartialEq for ColumnVector {
    fn eq(&self, other: &Self) -> bool {
        // Representations are interchangeable; logical equality compares
        // materialized row values (test-path only, materialization is fine).
        self.data_type == other.data_type && self.values() == other.values()
    }
}

impl Eq for ColumnVector {}

impl ColumnVector {
    /// Constructs and validates a column vector from row values; the packed
    /// typed projection builds lazily on first kernel use.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::WrongValueType`] when a non-null value does not
    /// match the declared logical type.
    pub fn new(data_type: DataType, values: Vec<Value>) -> Result<Self, BatchError> {
        for (row, value) in values.iter().enumerate() {
            if let Some(actual) = value.data_type()
                && !data_type.accepts(actual)
            {
                return Err(BatchError::WrongValueType {
                    row,
                    expected: data_type,
                    actual,
                });
            }
        }
        let len = values.len();
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(values);
        Ok(Self {
            data_type,
            len,
            values: cell,
            typed: std::sync::OnceLock::new(),
        })
    }

    /// Constructs a column vector directly from a packed typed projection
    /// (the scan path); row values materialize lazily on first use.
    pub(crate) fn from_typed(
        data_type: DataType,
        typed: TypedValues,
        validity: ValidityMask,
    ) -> Self {
        let len = typed.len();
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(Some((typed, validity)));
        Self {
            data_type,
            len,
            values: std::sync::OnceLock::new(),
            typed: cell,
        }
    }

    /// The packed projection, when the vector is physically homogeneous.
    /// Built on first use and cached; kernels that never ask never pay.
    pub(crate) fn typed(&self) -> Option<(&TypedValues, &ValidityMask)> {
        self.typed
            .get_or_init(|| {
                let values = self
                    .values
                    .get()
                    .expect("a column vector holds row values or a typed projection");
                build_typed(self.data_type, values)
            })
            .as_ref()
            .map(|(packed, validity)| (packed, validity))
    }

    /// Returns the logical scalar type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns every physical row value, including rows masked from the
    /// current selection, materializing them from the packed projection on
    /// first use.
    ///
    /// # Panics
    ///
    /// Panics if neither representation is populated, which construction
    /// makes impossible.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        self.values.get_or_init(|| {
            let (typed, validity) = self
                .typed
                .get()
                .and_then(Option::as_ref)
                .expect("a column vector holds row values or a typed projection");
            materialize_values(typed, validity)
        })
    }

    /// Returns one physical row value.
    #[must_use]
    pub fn value(&self, row: usize) -> Option<&Value> {
        self.values().get(row)
    }

    /// Returns the number of physical rows.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the vector has no physical rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Estimates bytes retained by the vector and its owned scalar payloads.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let typed_bytes = match self.typed.get().and_then(Option::as_ref) {
            None => 0,
            Some((TypedValues::Int64(packed), _)) => packed.capacity() * size_of::<i64>(),
            Some((TypedValues::UInt64(packed), _)) => packed.capacity() * size_of::<u64>(),
            Some((TypedValues::Float64(packed), _)) => packed.capacity() * size_of::<f64>(),
            Some((TypedValues::Utf8(packed), _)) => packed.len() * 16 + packed.heap().len(),
            Some((
                TypedValues::Decimal128 {
                    values: packed,
                    text,
                    ..
                },
                _,
            )) => {
                packed.capacity() * size_of::<i128>()
                    + text
                        .built()
                        .map_or(0, |text| text.len() * 16 + text.heap().len())
            }
            Some((TypedValues::Temporal { units, text }, _)) => {
                units.capacity() * size_of::<i64>()
                    + text
                        .built()
                        .map_or(0, |text| text.len() * 16 + text.heap().len())
            }
        };
        let value_bytes = self.values.get().map_or(0, |values| {
            values.capacity() * size_of::<Value>()
                + values.iter().map(Value::heap_bytes).sum::<usize>()
        });
        size_of::<Self>() + value_bytes + typed_bytes
    }
}

/// Materializes row values from a packed projection: the reverse of
/// [`build_typed`], used when a typed-born scan column meets a row-shaped
/// consumer (projection output, sorts, merges).
fn materialize_values(typed: &TypedValues, validity: &ValidityMask) -> Vec<Value> {
    let len = typed.len();
    let mut values = Vec::with_capacity(len);
    for row in 0..len {
        if !validity.is_valid(row) {
            values.push(Value::Null);
            continue;
        }
        values.push(match typed {
            TypedValues::Int64(packed) => Value::Int64(packed[row]),
            TypedValues::UInt64(packed) => Value::UInt64(packed[row]),
            TypedValues::Float64(packed) => {
                Value::Float64(pintail_types::Float64::new(packed[row]))
            }
            TypedValues::Utf8(column) => Value::Utf8(str_column_string(column, row)),
            TypedValues::Decimal128 { .. } | TypedValues::Temporal { .. } => {
                Value::Utf8(str_column_string(
                    typed
                        .text_column(validity)
                        .expect("decimal and temporal columns carry text"),
                    row,
                ))
            }
        });
    }
    values
}

/// One string view copied out as an owned `String`. Views were built from
/// validated UTF-8, so the lossy fallback never fires.
fn str_column_string(column: &StrColumn, row: usize) -> String {
    column.views()[row].with_bytes(column.heap(), |bytes| {
        String::from_utf8_lossy(bytes).into_owned()
    })
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

        let narrow =
            ColumnVector::new(DataType::Int8, vec![Value::Int64(-128)]).expect("narrow carrier");
        assert_eq!(narrow.data_type(), DataType::Int8);
        let decimal = ColumnVector::new(
            DataType::Decimal {
                precision: 10,
                scale: 2,
            },
            vec![Value::Utf8("123.45".to_owned())],
        )
        .expect("decimal carrier");
        assert_eq!(
            decimal.data_type(),
            DataType::Decimal {
                precision: 10,
                scale: 2
            }
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

#[cfg(test)]
mod typed_projection_tests {
    use super::*;

    #[test]
    fn decimal_columns_pack_to_scaled_i128() {
        let vector = ColumnVector::new(
            DataType::Decimal {
                precision: 18,
                scale: 4,
            },
            vec![
                Value::Utf8("123.4500".into()),
                Value::Null,
                Value::Utf8("-0.0001".into()),
                Value::Utf8("7".into()),
            ],
        )
        .expect("decimal vector");
        let (typed, validity) = vector.typed().expect("typed projection");
        let TypedValues::Decimal128 { values, scale, .. } = typed else {
            panic!("expected decimal projection, got {typed:?}");
        };
        assert_eq!(*scale, 4);
        assert_eq!(values, &[1_234_500, 0, -1, 70_000]);
        assert!(!validity.is_valid(1));
        assert_eq!(validity.count_valid(), 3);
    }

    #[test]
    fn unparseable_decimal_text_falls_back_to_utf8_views() {
        let vector = ColumnVector::new(
            DataType::Decimal {
                precision: 10,
                scale: 2,
            },
            vec![
                Value::Utf8("12.34".into()),
                Value::Utf8("not-a-number".into()),
            ],
        )
        .expect("vector");
        let (typed, _) = vector.typed().expect("typed projection");
        assert!(matches!(typed, TypedValues::Utf8(_)));
    }

    #[test]
    fn parse_decimal_rejects_precision_loss_and_garbage() {
        assert_eq!(parse_decimal_scaled("10.5", 2), Some(1050));
        assert_eq!(parse_decimal_scaled("10.505", 2), None);
        assert_eq!(parse_decimal_scaled("10.500", 2), Some(1050));
        assert_eq!(parse_decimal_scaled("", 2), None);
        assert_eq!(parse_decimal_scaled(".", 2), None);
        assert_eq!(parse_decimal_scaled("-3", 0), Some(-3));
        assert_eq!(parse_decimal_scaled("1e5", 2), None);
    }
}

#[cfg(test)]
mod temporal_tests {
    use super::*;

    #[test]
    fn date_parsing_matches_known_epochs() {
        assert_eq!(parse_date_days("1970-01-01"), Some(0));
        assert_eq!(parse_date_days("1969-12-31"), Some(-1));
        assert_eq!(parse_date_days("2000-02-29"), Some(11_016));
        assert_eq!(parse_date_days("2023-01-01"), Some(19_358));
        assert_eq!(parse_date_days("2023-1-1"), None);
        assert_eq!(parse_date_days("not-a-date"), None);
    }

    #[test]
    fn impossible_calendar_dates_are_rejected() {
        assert_eq!(parse_date_days("2023-02-29"), None);
        assert_eq!(parse_date_days("2023-02-30"), None);
        assert_eq!(parse_date_days("2023-02-31"), None);
        assert_eq!(parse_date_days("2024-02-29"), Some(19_782)); // leap year
        assert_eq!(parse_date_days("1900-02-29"), None); // century, not leap
        assert_eq!(parse_date_days("2000-02-29"), Some(11_016)); // 400-year leap
        assert_eq!(parse_date_days("2023-04-31"), None);
        assert_eq!(parse_date_days("2023-06-31"), None);
        assert_eq!(parse_date_days("2023-00-10"), None);
        assert_eq!(parse_date_days("2023-05-00"), None);
        assert_eq!(parse_datetime_micros("2023-02-31 00:00:00"), None);
        assert_eq!(parse_datetime_micros("2023-01-01 00:00:00."), None);
    }

    #[test]
    fn datetime_parsing_and_fraction_padding() {
        assert_eq!(parse_datetime_micros("1970-01-01 00:00:00"), Some(0));
        assert_eq!(
            parse_datetime_micros("1970-01-01 00:00:01.5"),
            Some(1_500_000)
        );
        assert_eq!(
            parse_datetime_micros("2023-01-01 00:00:00"),
            Some(19_358_i64 * 86_400 * 1_000_000)
        );
        assert_eq!(parse_datetime_micros("2023-01-01T00:00:00"), None);
    }

    #[test]
    fn date_columns_pack_to_temporal_units_with_views() {
        let vector = ColumnVector::new(
            DataType::Date32,
            vec![
                Value::Utf8("2023-01-01".into()),
                Value::Null,
                Value::Utf8("1970-01-01".into()),
            ],
        )
        .expect("date vector");
        let (typed, validity) = vector.typed().expect("typed projection");
        let TypedValues::Temporal { units, text } = typed else {
            panic!("expected temporal projection, got {typed:?}");
        };
        assert_eq!(units, &[19_358, 0, 0]);
        let text = text.built().expect("value-born text is ready");
        assert_eq!(text.len(), 3);
        assert!(!validity.is_valid(1));
    }
}
