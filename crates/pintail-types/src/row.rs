use crate::Value;

/// A typed component of a composite primary or unique key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum KeyPart {
    /// Signed integer key component.
    Int64(i64),
    /// Unsigned integer key component.
    UInt64(u64),
    /// UTF-8 key component.
    Utf8(String),
    /// Binary key component.
    Binary(Vec<u8>),
}

impl KeyPart {
    /// Estimates heap bytes owned by this key component.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(value) => value.len(),
            Self::Binary(value) => value.len(),
            Self::Int64(_) | Self::UInt64(_) => 0,
        }
    }
}

/// A non-empty, lexicographically ordered composite key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PrimaryKey(Vec<KeyPart>);

impl PrimaryKey {
    /// Constructs a primary key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key has no components.
    pub fn new(parts: Vec<KeyPart>) -> Result<Self, crate::SchemaError> {
        if parts.is_empty() {
            return Err(crate::SchemaError::EmptyPrimaryKey);
        }
        Ok(Self(parts))
    }

    /// Returns the key components in sort order.
    #[must_use]
    pub fn parts(&self) -> &[KeyPart] {
        &self.0
    }

    /// Estimates heap bytes owned by this key.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.0.iter().map(KeyPart::heap_bytes).sum()
    }
}

/// A versioned row accepted by the storage engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRow {
    key: PrimaryKey,
    values: Vec<Value>,
    version: u64,
    deleted: bool,
}

impl StoredRow {
    /// Constructs a versioned row.
    #[must_use]
    pub fn new(key: PrimaryKey, values: Vec<Value>, version: u64, deleted: bool) -> Self {
        Self {
            key,
            values,
            version,
            deleted,
        }
    }

    /// Returns the row key.
    #[must_use]
    pub fn key(&self) -> &PrimaryKey {
        &self.key
    }

    /// Returns values in schema column order.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Returns the source version used for last-write-wins resolution.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Returns whether this row is a tombstone.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// Estimates bytes retained by the in-memory representation.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.key.heap_bytes()
            + self.values.iter().map(Value::heap_bytes).sum::<usize>()
            + self.values.len() * size_of::<Value>()
    }
}
