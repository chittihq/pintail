use std::cmp::Ordering;

/// Logical scalar types supported by the storage format.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DataType {
    /// Boolean value.
    Boolean,
    /// Signed 64-bit integer.
    Int64,
    /// Unsigned 64-bit integer.
    UInt64,
    /// IEEE-754 64-bit floating-point value.
    Float64,
    /// UTF-8 string.
    Utf8,
    /// Arbitrary bytes.
    Binary,
}

/// An IEEE-754 value with bitwise equality and total ordering.
#[derive(Clone, Copy, Debug)]
pub struct Float64(u64);

impl Float64 {
    /// Wraps a floating-point value without changing its bits.
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value.to_bits())
    }

    /// Returns the wrapped value.
    #[must_use]
    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Returns the original IEEE-754 bits.
    #[must_use]
    pub fn to_bits(self) -> u64 {
        self.0
    }
}

impl PartialEq for Float64 {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Float64 {}

impl PartialOrd for Float64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Float64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.get().total_cmp(&other.get())
    }
}

impl std::hash::Hash for Float64 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// A nullable scalar value stored in a table row.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// Boolean value.
    Boolean(bool),
    /// Signed 64-bit integer.
    Int64(i64),
    /// Unsigned 64-bit integer.
    UInt64(u64),
    /// IEEE-754 64-bit floating-point value.
    Float64(Float64),
    /// UTF-8 string.
    Utf8(String),
    /// Arbitrary bytes.
    Binary(Vec<u8>),
}

impl Value {
    /// Constructs a floating-point value.
    #[must_use]
    pub fn float64(value: f64) -> Self {
        Self::Float64(Float64::new(value))
    }

    /// Returns this value's logical type, or `None` for `NULL`.
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Boolean(_) => Some(DataType::Boolean),
            Self::Int64(_) => Some(DataType::Int64),
            Self::UInt64(_) => Some(DataType::UInt64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Utf8(_) => Some(DataType::Utf8),
            Self::Binary(_) => Some(DataType::Binary),
        }
    }

    /// Estimates heap bytes owned by this value.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(value) => value.len(),
            Self::Binary(value) => value.len(),
            _ => 0,
        }
    }
}
