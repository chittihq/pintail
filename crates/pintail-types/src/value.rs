use std::cmp::Ordering;

/// Logical scalar types supported by the storage format.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// Boolean value.
    Boolean,
    /// Signed 8-bit integer.
    Int8,
    /// Signed 16-bit integer.
    Int16,
    /// Signed 32-bit integer.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// Unsigned 8-bit integer.
    UInt8,
    /// Unsigned 16-bit integer.
    UInt16,
    /// Unsigned 32-bit integer.
    UInt32,
    /// Unsigned 64-bit integer.
    UInt64,
    /// IEEE-754 32-bit floating-point value.
    Float32,
    /// IEEE-754 64-bit floating-point value.
    Float64,
    /// Fixed-point decimal carried losslessly as canonical text.
    Decimal {
        /// Total number of decimal digits.
        precision: u8,
        /// Number of digits after the decimal point.
        scale: u8,
    },
    /// `MySQL` calendar date carried as canonical `YYYY-MM-DD` text.
    Date32,
    /// `MySQL` date-time carried as canonical text with the declared precision.
    DateTime64 {
        /// Fractional-second precision in the range `0..=6`.
        fsp: u8,
    },
    /// Signed `MySQL` time interval with the declared fractional precision.
    Time64 {
        /// Fractional-second precision in the range `0..=6`.
        fsp: u8,
    },
    /// `MySQL` four-digit `YEAR` value, or zero.
    Year,
    /// UTF-8 string.
    Utf8,
    /// Arbitrary bytes.
    Binary,
    /// Canonical JSON text.
    Json,
}

impl DataType {
    /// Returns the physical scalar carrier used by the version-one executor
    /// and segment encodings.
    ///
    /// Narrow numeric types preserve their source range while sharing the
    /// corresponding 64-bit carrier. Decimal and temporal values use a
    /// canonical UTF-8 representation so invalid `MySQL` dates can be normalized
    /// before they enter storage.
    #[must_use]
    pub const fn storage_type(self) -> Self {
        match self {
            Self::Boolean => Self::Boolean,
            Self::Int8 | Self::Int16 | Self::Int32 | Self::Int64 => Self::Int64,
            Self::UInt8 | Self::UInt16 | Self::UInt32 | Self::UInt64 | Self::Year => Self::UInt64,
            Self::Float32 | Self::Float64 => Self::Float64,
            Self::Decimal { .. }
            | Self::Date32
            | Self::DateTime64 { .. }
            | Self::Time64 { .. }
            | Self::Utf8
            | Self::Json => Self::Utf8,
            Self::Binary => Self::Binary,
        }
    }

    /// Returns whether the type parameters are valid for Pintail v1.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        match self {
            Self::Decimal { precision, scale } => {
                precision > 0 && precision <= 38 && scale <= precision
            }
            Self::DateTime64 { fsp } | Self::Time64 { fsp } => fsp <= 6,
            _ => true,
        }
    }

    /// Returns whether a physical value type is accepted by this logical
    /// column type.
    #[must_use]
    pub fn accepts(self, physical: Self) -> bool {
        self.storage_type() == physical
    }
}

/// An IEEE-754 value with bitwise equality and total ordering.
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
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
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
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
    /// A `MySQL` ENUM: its declaration index alongside its label.
    ///
    /// `MySQL` orders and compares ENUM by the declaration index and displays
    /// the label. Storing only the label - as this engine used to - makes
    /// `ORDER BY` follow alphabetical order instead, silently.
    ///
    /// Deliberately reports [`DataType::Utf8`], so every site that has not
    /// learned about ENUM treats it as the string it displays as. That keeps
    /// an unaudited path at today's behaviour rather than giving it a new
    /// one; only comparison, which is the defect, changes.
    ///
    /// Derived `Ord` compares `index` before `label`, which is the ordering
    /// `MySQL` uses.
    Enum {
        /// One-based declaration index for an ENUM, or the member bitmask
        /// for a SET - both are what `MySQL` sorts the type by.
        index: u64,
        /// Declared label, and what the value displays as.
        label: String,
    },
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
            // Reports Utf8 on purpose: an ENUM displays as its label, so
            // any path that has not learned about ENUM keeps treating it
            // exactly as it treated the label before.
            Self::Utf8(_) | Self::Enum { .. } => Some(DataType::Utf8),
            Self::Binary(_) => Some(DataType::Binary),
        }
    }

    /// Estimates heap bytes owned by this value.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(value) | Self::Enum { label: value, .. } => value.len(),
            Self::Binary(value) => value.len(),
            _ => 0,
        }
    }

    /// The text an ENUM or string value displays as.
    ///
    /// Lets a caller read the label without matching both variants, which is
    /// how most existing string handling should treat an ENUM.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Utf8(value) | Self::Enum { label: value, .. } => Some(value),
            _ => None,
        }
    }

    /// The declaration index when this is an ENUM.
    #[must_use]
    pub const fn enum_index(&self) -> Option<u64> {
        match self {
            Self::Enum { index, .. } => Some(*index),
            _ => None,
        }
    }
}
