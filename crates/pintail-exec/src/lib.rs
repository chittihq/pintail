//! Planner, optimizer, and vectorized executor for Pintail.

pub mod array;
mod batch;
pub mod collation;
mod execution;
mod explain;
mod expression;
mod json_order;
mod logical;
mod optimizer;
pub mod spill;
mod storage;

pub use batch::{
    BatchError, ColumnVector, DEFAULT_BATCH_ROWS, RecordBatch, SPILL_SERVE_BATCH_ROWS,
    SelectedRows, SelectionMask,
};
pub use execution::compare_collated_text;
pub use execution::{
    BatchStream, DEFAULT_CTE_MAX_RECURSION_DEPTH, ExecError, Execution, ExecutionCancellation,
    MAX_CROSS_JOIN_ROWS, MemoryTracker, OutputField, PhysicalPlan, PhysicalPlanner, ScanProvider,
    with_execution_cancellation,
};
pub use execution::{
    MemoryBudget, MemoryScope, init_shared_memory_budget, set_session_cte_max_recursion_depth,
    set_session_group_concat_max_len, shared_memory_budget, take_session_group_concat_warnings,
};
pub use execution::{
    dependent_memo_disabled, dependent_memo_hits, dependent_memo_misses,
    dependent_subquery_executions,
};
pub use explain::{
    ExplainError, explain_analyze_statement, explain_analyze_statement_with_deadline,
    explain_statement, format_physical_plan, format_physical_plan_with_stats,
};
pub use logical::{LogicalPlan, LogicalPlanner, Scan};
pub use optimizer::{Optimizer, set_session_time_zone};
pub use storage::{PhysicalScanStats, SnapshotScanProvider};

pub use execution::cancel_query_under_memory_pressure;
