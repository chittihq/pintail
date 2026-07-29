use std::{error::Error, fmt, io, path::PathBuf};

use pintail_types::SchemaError;

/// A storage-engine operation failure.
#[derive(Debug)]
pub enum StoreError {
    /// A filesystem operation failed.
    Io {
        /// Operation that failed.
        action: String,
        /// Underlying operating-system error.
        source: io::Error,
    },
    /// A batch did not match the table schema.
    Schema(SchemaError),
    /// Another writer already owns this table.
    WriterBusy,
    /// WAL bytes did not satisfy the documented format.
    CorruptWal {
        /// Byte offset at which validation failed.
        offset: u64,
        /// Precise validation failure.
        reason: String,
    },
    /// Immutable segment bytes failed structural or checksum validation.
    CorruptSegment {
        /// Segment that failed validation.
        path: PathBuf,
        /// Byte offset at which validation failed.
        offset: u64,
        /// Precise validation failure.
        reason: String,
    },
    /// The durable manifest failed structural or checksum validation.
    CorruptManifest {
        /// Byte offset at which validation failed.
        offset: u64,
        /// Precise validation failure.
        reason: String,
    },
    /// An on-disk segment was created with another schema version.
    SchemaMismatch {
        /// Version supplied by the caller.
        expected_version: u32,
        /// Version recorded on disk.
        actual_version: u32,
    },
    /// An on-disk schema has different columns for the same version.
    SchemaFingerprintMismatch {
        /// Fingerprint supplied by the caller.
        expected: u64,
        /// Fingerprint recorded on disk.
        actual: u64,
    },
    /// A monotonically increasing sequence number overflowed.
    SequenceOverflow,
    /// A collection could not be represented by the on-disk format.
    FormatLimit(String),
}

impl StoreError {
    pub(crate) fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }

    pub(crate) fn corrupt_wal(offset: usize, reason: impl Into<String>) -> Self {
        Self::CorruptWal {
            offset: offset as u64,
            reason: reason.into(),
        }
    }

    pub(crate) fn corrupt_manifest(offset: usize, reason: impl Into<String>) -> Self {
        Self::CorruptManifest {
            offset: offset as u64,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { action, source } => write!(formatter, "{action}: {source}"),
            Self::Schema(error) => write!(formatter, "row does not match table schema: {error}"),
            Self::WriterBusy => formatter.write_str("another writer already owns this table"),
            Self::CorruptWal { offset, reason } => {
                write!(formatter, "corrupt WAL at byte {offset}: {reason}")
            }
            Self::CorruptSegment {
                path,
                offset,
                reason,
            } => write!(
                formatter,
                "corrupt segment {} at byte {offset}: {reason}",
                path.display()
            ),
            Self::CorruptManifest { offset, reason } => {
                write!(formatter, "corrupt manifest at byte {offset}: {reason}")
            }
            Self::SchemaMismatch {
                expected_version,
                actual_version,
            } => write!(
                formatter,
                "schema version mismatch: expected {expected_version}, found {actual_version}"
            ),
            Self::SchemaFingerprintMismatch { expected, actual } => write!(
                formatter,
                "schema fingerprint mismatch: expected {expected:016x}, found {actual:016x}"
            ),
            Self::SequenceOverflow => formatter.write_str("WAL sequence number overflow"),
            Self::FormatLimit(reason) => write!(formatter, "storage format limit: {reason}"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Schema(source) => Some(source),
            _ => None,
        }
    }
}

impl From<SchemaError> for StoreError {
    fn from(value: SchemaError) -> Self {
        Self::Schema(value)
    }
}
