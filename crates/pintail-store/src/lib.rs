//! Pintail's from-scratch columnar storage engine.
//!
//! [`TableStore`] is the external seam. It validates typed batches, records
//! them in a checksummed WAL before making them visible, exposes immutable
//! reader snapshots, and reconstructs its memtable after a restart.

mod codec;
mod error;
mod manifest;
mod memtable;
mod segment;
mod store;
mod wal;

pub use error::StoreError;
pub use store::{
    CompactionOutcome, CompactionStatus, FlushOutcome, IngestOutcome, ProjectedRow, ProjectedScan,
    ScanStats, StorageMetrics, StoreOptions, TableSnapshot, TableStore, WalSync,
};
