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

/// The widest DECIMAL the store keeps as native scaled integers (it fits an
/// `i64`); wider columns are stored as canonical text.
pub const NATIVE_DECIMAL_MAX_PRECISION: u8 = 18;

/// The scaled sum and count retained by a finished decimal average.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct DecimalQuotient {
    /// Canonical text at the declared result scale.
    pub label: String,
    /// Sum in units of the declared result scale.
    pub units: i128,
    /// Number of non-null inputs.
    pub count: u64,
    /// Declared result scale, also used to render the average.
    pub scale: u8,
}

impl DecimalQuotient {
    /// The value rendered at its scale, whatever the label shows: an
    /// average's canonical text, or the exact value of a decimal whose label
    /// keeps a narrower scale than its type. Keys and comparisons read this;
    /// clients see the label.
    #[must_use]
    pub fn canonical(&self) -> String {
        if self.count == 0 {
            return self.label.clone();
        }
        crate::div_decimal_round_half_up(self.units, i128::from(self.count)).map_or_else(
            || self.label.clone(),
            |units| crate::format_decimal_scaled(units, self.scale),
        )
    }
}

/// A nullable scalar value stored in a table row.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
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
    /// A decimal average displays its declared scale while exact arithmetic
    /// and rounding can read its quotient. Boxing keeps scalar rows compact.
    DecimalAverage(Box<DecimalQuotient>),
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
    /// Scalar ordering compares `index` before `label`, which is the ordering
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
            Self::Utf8(_) | Self::Enum { .. } | Self::DecimalAverage(_) => Some(DataType::Utf8),
            Self::Binary(_) => Some(DataType::Binary),
        }
    }

    /// Estimates heap bytes owned by this value.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(value) | Self::Enum { label: value, .. } => value.len(),
            Self::DecimalAverage(value) => value.label.len() + size_of::<DecimalQuotient>(),
            Self::Binary(value) => value.len(),
            _ => 0,
        }
    }

    /// The text a string, ENUM, or decimal average displays as.
    ///
    /// Lets a caller read the label without matching both variants, which is
    /// how most existing string handling should treat an ENUM.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Utf8(value) | Self::Enum { label: value, .. } => Some(value),
            Self::DecimalAverage(value) => Some(&value.label),
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

// The quotient is execution precision, not scalar identity. Ordinary value
// consumers keep the same equality, hashing and ordering as the display text.
#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ValueKey<'a> {
    Null,
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    Float64(Float64),
    Utf8(&'a str),
    Binary(&'a [u8]),
    Enum(u64, &'a str),
}

impl Value {
    fn key(&self) -> ValueKey<'_> {
        match self {
            Self::Null => ValueKey::Null,
            Self::Boolean(value) => ValueKey::Boolean(*value),
            Self::Int64(value) => ValueKey::Int64(*value),
            Self::UInt64(value) => ValueKey::UInt64(*value),
            Self::Float64(value) => ValueKey::Float64(*value),
            Self::Utf8(value) => ValueKey::Utf8(value),
            Self::DecimalAverage(value) => ValueKey::Utf8(&value.label),
            Self::Binary(value) => ValueKey::Binary(value),
            Self::Enum { index, label } => ValueKey::Enum(*index, label),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Value {}
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}
impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::{DecimalQuotient, Value};
    use std::hash::{Hash, Hasher};

    #[test]
    fn decimal_precision_does_not_change_scalar_identity_or_layout() {
        let average = Value::DecimalAverage(Box::new(DecimalQuotient {
            label: "1.0000".to_owned(),
            units: 20_000,
            count: 2,
            scale: 4,
        }));
        let text = Value::Utf8("1.0000".to_owned());
        assert_eq!(average, text);
        assert_eq!(average.cmp(&text), std::cmp::Ordering::Equal);
        let hash = |value: &Value| {
            let mut state = std::collections::hash_map::DefaultHasher::new();
            value.hash(&mut state);
            state.finish()
        };
        assert_eq!(hash(&average), hash(&text));
        assert_eq!(size_of::<Value>(), 32);
        assert_eq!(average.heap_bytes(), size_of::<DecimalQuotient>() + 6);
    }
}
