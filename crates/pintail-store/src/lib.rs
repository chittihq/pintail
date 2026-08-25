//! Pintail's from-scratch columnar storage engine.
//!
//! [`DatabaseStore`] is the production seam: it multiplexes table batches
//! through one checksummed database WAL before making them visible. Each table
//! retains independent manifests, immutable segments, and reader snapshots.
//! [`TableStore`] provides the same behavior as a standalone compatibility
//! handle for one physical table.

mod codec;
mod database;
mod error;
mod manifest;
mod memtable;
mod segment;
mod store;
mod wal;

pub use database::DatabaseStore;
pub use error::StoreError;
pub use segment::{
    BoundDomain, ColumnBounds, ColumnSma, NativeUnits, SegmentSmas, SmaExtremes, SmaSum,
};
pub use store::{
    BackupArtifacts, BackupSegment, BulkIngestOutcome, ColumnValidity, CompactionOutcome,
    CompactionStatus, DecodedColumn, FlushOutcome, IngestOutcome, PrewhereSelect,
    ProjectedColumnChunk, ProjectedRow, ProjectedScan, ProjectedScanStream, ProjectedValueChunk,
    ScanStats, StorageMetrics, StoreOptions, TableSnapshot, TableStore, WalSync,
    projected_scan_width,
};

/// The stable on-disk directory for one table inside a database's `tables`
/// root.
///
/// The name is derived, not stored: a readable prefix for humans plus a
/// hash of the lowercased table name for uniqueness and case-insensitive
/// identity. Every caller MUST agree byte-for-byte, because a different
/// answer silently addresses a different (empty) table rather than
/// failing - which is why this lives here, below every reader and writer,
/// instead of being copied into each.
#[must_use]
pub fn table_directory(root: &std::path::Path, table: &str) -> std::path::PathBuf {
    use std::hash::{Hash as _, Hasher as _};

    let safe = table
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    table.to_ascii_lowercase().hash(&mut hasher);
    root.join(format!("table-{safe}-{:016x}", hasher.finish()))
}
