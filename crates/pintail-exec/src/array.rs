//! Typed columnar arrays for the vectorized executor.
//!
//! This is the foundation of the typed-array migration ratified in
//! `docs/decisions.md` ("Executor moves to typed packed arrays"): physical
//! representations with Flat/Constant/Dictionary forms, validity masks with an
//! all-valid fast path, selection vectors, and 16-byte German-string views.
//! [`pintail_types::Value`] remains the boundary representation only; kernels
//! operate on these arrays. Decimal128/Date32 physical types arrive with the
//! native decimal/date execution work and extend [`ColumnArray`] here.

use pintail_types::Value;

/// Row-visibility bitmap with an all-valid fast path: `None` means every row
/// is valid and kernels take their null-free loop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidityMask {
    words: Option<Box<[u64]>>,
    len: usize,
}

impl ValidityMask {
    /// A mask over `len` rows with every row valid.
    #[must_use]
    pub const fn all_valid(len: usize) -> Self {
        Self { words: None, len }
    }

    /// Builds a mask from per-row validity; collapses to the fast path when
    /// nothing is null.
    #[must_use]
    pub fn from_bools(bits: &[bool]) -> Self {
        if bits.iter().all(|&valid| valid) {
            return Self::all_valid(bits.len());
        }
        let mut words = vec![0u64; bits.len().div_ceil(64)].into_boxed_slice();
        for (row, &valid) in bits.iter().enumerate() {
            if valid {
                words[row / 64] |= 1 << (row % 64);
            }
        }
        Self {
            words: Some(words),
            len: bits.len(),
        }
    }

    /// Number of rows covered by the mask.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the mask covers zero rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The branch kernels take once per batch, not per row.
    #[must_use]
    pub const fn no_nulls(&self) -> bool {
        self.words.is_none()
    }

    /// Per-row validity (slow path; kernels should test [`Self::no_nulls`]
    /// first and use word-wise access in their null-aware loop).
    #[must_use]
    pub fn is_valid(&self, row: usize) -> bool {
        debug_assert!(row < self.len);
        match &self.words {
            None => true,
            Some(words) => words[row / 64] & (1 << (row % 64)) != 0,
        }
    }

    /// Count of valid rows.
    #[must_use]
    pub fn count_valid(&self) -> usize {
        match &self.words {
            None => self.len,
            Some(words) => {
                let mut count: usize = words.iter().map(|word| word.count_ones() as usize).sum();
                // Mask off bits beyond len in the last word.
                let tail = self.len % 64;
                if tail != 0 {
                    let last = words[self.len / 64];
                    count -= (last >> tail).count_ones() as usize;
                    let _ = last;
                }
                count
            }
        }
    }
}

/// Positions selected out of a batch by a filter, in ascending order.
/// `None` denotes identity (all rows selected) with zero indirection cost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionVector {
    positions: Option<Vec<u32>>,
    len: usize,
}

impl SelectionVector {
    /// Identity selection over `len` rows.
    #[must_use]
    pub const fn identity(len: usize) -> Self {
        Self {
            positions: None,
            len,
        }
    }

    /// Explicit selected positions (must be ascending).
    #[must_use]
    pub fn from_positions(positions: Vec<u32>, source_len: usize) -> Self {
        debug_assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        debug_assert!(
            positions
                .last()
                .is_none_or(|&last| (last as usize) < source_len)
        );
        let _ = source_len;
        let len = positions.len();
        Self {
            positions: Some(positions),
            len,
        }
    }

    /// Number of selected rows.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is selected.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether this is the zero-cost identity selection.
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        self.positions.is_none()
    }

    /// The physical row for logical position `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> usize {
        debug_assert!(index < self.len);
        match &self.positions {
            None => index,
            Some(positions) => positions[index] as usize,
        }
    }
}

/// A 16-byte string view: length, 4-byte prefix, and either the remaining
/// bytes inline (len <= 12) or an offset into the column heap. Equality
/// resolves on (len, prefix) without touching the heap for most mismatches
/// (experiments/RESULTS.md e07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrView {
    len: u32,
    prefix: [u8; 4],
    tail: [u8; 8],
}

const INLINE_LEN: usize = 12;

impl StrView {
    fn new(bytes: &[u8], heap: &mut Vec<u8>) -> Self {
        let len = u32::try_from(bytes.len()).expect("string length fits u32");
        let mut prefix = [0u8; 4];
        let prefix_len = bytes.len().min(4);
        prefix[..prefix_len].copy_from_slice(&bytes[..prefix_len]);
        let mut tail = [0u8; 8];
        if bytes.len() <= INLINE_LEN {
            if bytes.len() > 4 {
                tail[..bytes.len() - 4].copy_from_slice(&bytes[4..]);
            }
        } else {
            let offset = u64::try_from(heap.len()).expect("heap offset fits u64");
            heap.extend_from_slice(bytes);
            tail = offset.to_le_bytes();
        }
        Self { len, prefix, tail }
    }

    /// Equality against a needle prepared once per batch.
    #[must_use]
    pub fn eq_bytes(&self, needle: &StrView, needle_bytes: &[u8], heap: &[u8]) -> bool {
        if self.len != needle.len || self.prefix != needle.prefix {
            return false;
        }
        if self.len as usize <= INLINE_LEN {
            self.tail == needle.tail
        } else {
            let offset = usize::try_from(u64::from_le_bytes(self.tail)).expect("heap offset");
            &heap[offset..offset + self.len as usize] == needle_bytes
        }
    }
}

impl StrView {
    /// Runs `f` over the string's bytes without allocating, copying at most
    /// 12 inline bytes to the stack.
    pub fn with_bytes<R>(&self, heap: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
        let len = self.len as usize;
        if len <= INLINE_LEN {
            let mut stack = [0u8; INLINE_LEN];
            stack[..4.min(len)].copy_from_slice(&self.prefix[..4.min(len)]);
            if len > 4 {
                stack[4..len].copy_from_slice(&self.tail[..len - 4]);
            }
            f(&stack[..len])
        } else {
            let offset = usize::try_from(u64::from_le_bytes(self.tail)).expect("heap offset");
            f(&heap[offset..offset + len])
        }
    }
}

/// A string column: fixed-stride views plus a shared heap for long strings.
#[derive(Clone, Debug, Default)]
pub struct StrColumn {
    views: Vec<StrView>,
    heap: Vec<u8>,
}

impl StrColumn {
    /// Appends one string.
    pub fn push(&mut self, bytes: &[u8]) {
        let view = StrView::new(bytes, &mut self.heap);
        self.views.push(view);
    }

    /// Number of strings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.views.len()
    }

    /// Whether the column is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// The views for kernel iteration.
    #[must_use]
    pub fn views(&self) -> &[StrView] {
        &self.views
    }

    /// The shared long-string heap.
    #[must_use]
    pub fn heap(&self) -> &[u8] {
        &self.heap
    }

    /// Prepares a comparison needle sharing no storage with the column.
    #[must_use]
    pub fn needle(bytes: &[u8]) -> (StrView, Vec<u8>) {
        let mut heap = Vec::new();
        let view = StrView::new(bytes, &mut heap);
        // For long needles the view's offset points into this private heap,
        // but eq_bytes compares against the caller-provided needle bytes, so
        // the heap itself is unused after construction.
        let _ = &heap;
        (view, bytes.to_vec())
    }
}

/// Physical column representations for one batch.
///
/// `Flat` variants are packed vectors; `Constant` holds one value for the
/// whole batch; `Dictionary` keeps codes plus a (much smaller) values array
/// so low-cardinality columns execute on codes.
#[derive(Clone, Debug)]
pub enum ColumnArray {
    Boolean {
        values: Vec<bool>,
        validity: ValidityMask,
    },
    Int64 {
        values: Vec<i64>,
        validity: ValidityMask,
    },
    UInt64 {
        values: Vec<u64>,
        validity: ValidityMask,
    },
    Float64 {
        values: Vec<f64>,
        validity: ValidityMask,
    },
    Utf8 {
        values: StrColumn,
        validity: ValidityMask,
    },
    Binary {
        values: Vec<Vec<u8>>,
        validity: ValidityMask,
    },
    Constant {
        value: Value,
        len: usize,
    },
    Dictionary {
        codes: Vec<u32>,
        values: Box<ColumnArray>,
        validity: ValidityMask,
    },
}

impl ColumnArray {
    /// Number of rows in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Boolean { values, .. } => values.len(),
            Self::Int64 { values, .. } => values.len(),
            Self::UInt64 { values, .. } => values.len(),
            Self::Float64 { values, .. } => values.len(),
            Self::Utf8 { values, .. } => values.len(),
            Self::Binary { values, .. } => values.len(),
            Self::Constant { len, .. } => *len,
            Self::Dictionary { codes, .. } => codes.len(),
        }
    }

    /// Whether the batch has zero rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Builds the tightest homogeneous representation from boundary values.
    ///
    /// Mixed-type batches (beyond NULL) fall back to `Binary`-free encoding
    /// via `Utf8` never — they return `None` and the caller keeps row values;
    /// the migration converts call sites incrementally.
    #[must_use]
    pub fn from_values(values: &[Value]) -> Option<Self> {
        #[derive(PartialEq, Eq, Clone, Copy)]
        enum Kind {
            Unknown,
            Boolean,
            Int64,
            UInt64,
            Float64,
            Utf8,
            Binary,
        }
        let mut kind = Kind::Unknown;
        for value in values {
            let observed = match value {
                Value::Null => continue,
                Value::Boolean(_) => Kind::Boolean,
                Value::Int64(_) => Kind::Int64,
                Value::UInt64(_) => Kind::UInt64,
                Value::Float64(_) => Kind::Float64,
                Value::Utf8(_) => Kind::Utf8,
                Value::Binary(_) => Kind::Binary,
            };
            if kind == Kind::Unknown {
                kind = observed;
            } else if kind != observed {
                return None;
            }
        }
        let validity = ValidityMask::from_bools(
            &values
                .iter()
                .map(|v| !matches!(v, Value::Null))
                .collect::<Vec<_>>(),
        );
        Some(match kind {
            Kind::Unknown => Self::Constant {
                value: Value::Null,
                len: values.len(),
            },
            Kind::Boolean => Self::Boolean {
                values: values
                    .iter()
                    .map(|v| matches!(v, Value::Boolean(true)))
                    .collect(),
                validity,
            },
            Kind::Int64 => Self::Int64 {
                values: values
                    .iter()
                    .map(|v| if let Value::Int64(i) = v { *i } else { 0 })
                    .collect(),
                validity,
            },
            Kind::UInt64 => Self::UInt64 {
                values: values
                    .iter()
                    .map(|v| if let Value::UInt64(u) = v { *u } else { 0 })
                    .collect(),
                validity,
            },
            Kind::Float64 => Self::Float64 {
                values: values
                    .iter()
                    .map(|v| {
                        if let Value::Float64(f) = v {
                            f.get()
                        } else {
                            0.0
                        }
                    })
                    .collect(),
                validity,
            },
            Kind::Utf8 => {
                let mut column = StrColumn::default();
                for value in values {
                    match value {
                        Value::Utf8(text) => column.push(text.as_bytes()),
                        _ => column.push(&[]),
                    }
                }
                Self::Utf8 {
                    values: column,
                    validity,
                }
            }
            Kind::Binary => Self::Binary {
                values: values
                    .iter()
                    .map(|v| {
                        if let Value::Binary(b) = v {
                            b.clone()
                        } else {
                            Vec::new()
                        }
                    })
                    .collect(),
                validity,
            },
        })
    }

    /// Boundary conversion back to a row value.
    #[must_use]
    pub fn value_at(&self, row: usize) -> Value {
        match self {
            Self::Boolean { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                Value::Boolean(values[row])
            }
            Self::Int64 { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                Value::Int64(values[row])
            }
            Self::UInt64 { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                Value::UInt64(values[row])
            }
            Self::Float64 { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                Value::float64(values[row])
            }
            Self::Utf8 { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                values.views()[row].with_bytes(values.heap(), |bytes| {
                    Value::Utf8(String::from_utf8_lossy(bytes).into_owned())
                })
            }
            Self::Binary { values, validity } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                Value::Binary(values[row].clone())
            }
            Self::Constant { value, .. } => value.clone(),
            Self::Dictionary {
                codes,
                values,
                validity,
            } => {
                if !validity.is_valid(row) {
                    return Value::Null;
                }
                values.value_at(codes[row] as usize)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_all_valid_fast_path_and_counts() {
        let mask = ValidityMask::from_bools(&[true, true, true]);
        assert!(mask.no_nulls());
        assert_eq!(mask.count_valid(), 3);
        let mixed = ValidityMask::from_bools(&[true, false, true]);
        assert!(!mixed.no_nulls());
        assert!(mixed.is_valid(0));
        assert!(!mixed.is_valid(1));
        assert_eq!(mixed.count_valid(), 2);
    }

    #[test]
    fn selection_identity_and_positions() {
        let identity = SelectionVector::identity(4);
        assert!(identity.is_identity());
        assert_eq!(identity.get(3), 3);
        let picked = SelectionVector::from_positions(vec![1, 3], 4);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked.get(0), 1);
        assert_eq!(picked.get(1), 3);
    }

    #[test]
    fn string_views_inline_and_heap_round_trip() {
        let mut column = StrColumn::default();
        column.push(b"IN");
        column.push(b"shipped");
        column.push(b"a-very-long-string-beyond-inline");
        assert!(column.heap().len() >= 32);
        let texts: Vec<String> = column
            .views()
            .iter()
            .map(|view| view.with_bytes(column.heap(), |b| String::from_utf8_lossy(b).into_owned()))
            .collect();
        assert_eq!(texts, ["IN", "shipped", "a-very-long-string-beyond-inline"]);
        let (needle, needle_bytes) = StrColumn::needle(b"shipped");
        let matches: Vec<bool> = column
            .views()
            .iter()
            .map(|view| view.eq_bytes(&needle, &needle_bytes, column.heap()))
            .collect();
        assert_eq!(matches, [false, true, false]);
    }

    #[test]
    fn from_values_round_trips_via_value_at() {
        let values = vec![Value::Int64(5), Value::Null, Value::Int64(-9)];
        let array = ColumnArray::from_values(&values).expect("homogeneous");
        assert_eq!(array.len(), 3);
        assert_eq!(array.value_at(0), Value::Int64(5));
        assert_eq!(array.value_at(1), Value::Null);
        assert_eq!(array.value_at(2), Value::Int64(-9));
    }

    #[test]
    fn dictionary_resolves_through_codes() {
        let mut dict_values = StrColumn::default();
        dict_values.push(b"pending");
        dict_values.push(b"shipped");
        let dictionary = ColumnArray::Dictionary {
            codes: vec![0, 1, 1, 0],
            values: Box::new(ColumnArray::Utf8 {
                values: dict_values,
                validity: ValidityMask::all_valid(2),
            }),
            validity: ValidityMask::all_valid(4),
        };
        assert_eq!(dictionary.value_at(2), Value::Utf8("shipped".into()));
        assert_eq!(dictionary.len(), 4);
    }

    #[test]
    fn mixed_types_refuse_array_form() {
        assert!(ColumnArray::from_values(&[Value::Int64(1), Value::Utf8("x".into())]).is_none());
    }
}
