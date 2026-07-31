//! Planner, optimizer, and vectorized executor for Pintail.

pub mod array;
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
pub use explain::{
    ExplainError, explain_analyze_statement, explain_statement, format_physical_plan,
    format_physical_plan_with_stats,
};
pub use logical::{LogicalPlan, LogicalPlanner, Scan};
pub use optimizer::Optimizer;
pub use storage::{PhysicalScanStats, SnapshotScanProvider};
