//! Planner, optimizer, and vectorized executor for Pintail.

mod batch;
mod execution;
mod explain;
mod expression;
mod logical;
mod optimizer;
mod storage;

pub use batch::{
    BatchError, ColumnVector, DEFAULT_BATCH_ROWS, RecordBatch, SelectedRows, SelectionMask,
};
pub use execution::{
    BatchStream, ExecError, Execution, MAX_CROSS_JOIN_ROWS, MemoryTracker, OutputField,
    PhysicalPlan, PhysicalPlanner, ScanProvider,
};
pub use explain::{ExplainError, explain_statement, format_physical_plan};
pub use logical::{LogicalPlan, LogicalPlanner, Scan};
pub use optimizer::Optimizer;
pub use storage::SnapshotScanProvider;
