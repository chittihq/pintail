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
pub use store::{
    BackupArtifacts, BackupSegment, BulkIngestOutcome, CompactionOutcome, CompactionStatus,
    FlushOutcome, IngestOutcome, ProjectedColumnChunk, ProjectedRow, ProjectedScan,
    ProjectedScanStream, ProjectedValueChunk, ScanStats, StorageMetrics, StoreOptions,
    TableSnapshot, TableStore, WalSync,
};
