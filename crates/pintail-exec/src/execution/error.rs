//! Physical planning and execution failures.

use std::fmt;

use pintail_catalog::{DatabaseId, TableId};

use crate::BatchError;

use super::budget::MemoryScope;

/// Physical planning or execution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecError {
    /// A logical operator has no physical implementation yet.
    UnsupportedOperator(&'static str),
    /// A join predicate is not a single cross-input equality yet.
    UnsupportedJoinCondition,
    /// A scalar subquery produced more than one row.
    ScalarSubqueryRows {
        /// Actual result cardinality.
        rows: usize,
    },
    /// A physical plan violates an internal layout invariant.
    InvalidPhysicalPlan(&'static str),
    /// A source returned a malformed batch.
    InvalidBatch(&'static str),
    /// A compiled expression cannot find its stable input column.
    MissingColumn {
        /// Query-visible relation.
        relation: String,
        /// Source column.
        column: String,
    },
    /// An expression operation received an impossible bound type.
    InvalidExpressionType,
    /// A JSON path expression does not parse; `position` is where `MySQL`'s
    /// parser stood when it stopped.
    InvalidJsonPath {
        /// Character offset the error names.
        position: usize,
    },
    /// A value left its type's range; `MySQL`'s message names the expression.
    OutOfRange(String),
    /// Numeric evaluation exceeded the bound result type.
    NumericOverflow,
    /// Binary numeric coercion encountered invalid UTF-8.
    InvalidUtf8Number,
    /// A date/time value or operation is outside the supported `MySQL` range.
    InvalidDateTime,
    /// A recursive CTE did not converge within the iteration cap.
    RecursionDepthExceeded {
        /// Iteration cap (`MySQL`'s `cte_max_recursion_depth` default).
        limit: u64,
    },
    /// The configured statement execution deadline elapsed.
    QueryTimedOut,
    /// The client or caller abandoned the running query.
    QueryCancelled,
    /// A source-specific failure.
    Source(String),
    /// The scan provider was configured twice for one stable table.
    DuplicateSnapshot {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
    },
    /// The table's copy from its source has not completed, so its reader
    /// would answer from a partial or empty store.
    TableNotReady {
        /// The table's name as the source knows it.
        table: String,
    },
    /// The scan provider has no pinned reader for a stable table.
    MissingSnapshot {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
    },
    /// The pinned reader and bound catalog used different schema versions.
    SnapshotSchemaChanged {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
        /// Version used while binding.
        expected: u32,
        /// Version pinned by storage.
        actual: u32,
    },
    /// The hard per-query memory cap would be exceeded.
    MemoryLimitExceeded {
        /// Bytes already reserved.
        used: usize,
        /// Additional transient or persistent bytes requested.
        requested: usize,
        /// The ceiling that was hit.
        limit: usize,
        /// Which ceiling: one query's own, or the process-wide budget.
        scope: MemoryScope,
    },
    /// A batch invariant was violated.
    Batch(BatchError),
}

impl fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOperator(operator) => {
                write!(formatter, "physical operator {operator} is not implemented")
            }
            Self::UnsupportedJoinCondition => formatter
                .write_str("join ON clause has no equality between the two inputs to join on"),
            Self::ScalarSubqueryRows { rows } => {
                write!(formatter, "scalar subquery produced {rows} rows")
            }
            Self::RecursionDepthExceeded { limit } => write!(
                formatter,
                "recursive query aborted after {} iterations (cte_max_recursion_depth = {limit})",
                limit + 1
            ),
            Self::QueryTimedOut => formatter
                .write_str("query execution was interrupted after max_execution_time elapsed"),
            Self::QueryCancelled => formatter.write_str("query execution was cancelled"),
            Self::InvalidPhysicalPlan(message) => {
                write!(formatter, "invalid physical plan: {message}")
            }
            Self::InvalidBatch(message) => write!(formatter, "invalid source batch: {message}"),
            Self::MissingColumn { relation, column } => {
                write!(formatter, "physical input is missing {relation}.{column}")
            }
            Self::InvalidExpressionType => {
                formatter.write_str("bound expression has an invalid physical type")
            }
            Self::InvalidJsonPath { position } => write!(
                formatter,
                "Invalid JSON path expression. The error is around character position {position}."
            ),
            Self::OutOfRange(message) => formatter.write_str(message),
            Self::NumericOverflow => formatter.write_str("numeric expression overflow"),
            Self::InvalidUtf8Number => {
                formatter.write_str("binary value is not valid UTF-8 for numeric coercion")
            }
            Self::InvalidDateTime => formatter.write_str("invalid MySQL date/time value"),
            Self::Source(message) => write!(formatter, "scan source failed: {message}"),
            Self::DuplicateSnapshot {
                database_id,
                table_id,
            } => write!(
                formatter,
                "snapshot provider repeats database {} table {}",
                database_id.get(),
                table_id.get()
            ),
            Self::TableNotReady { table } => write!(
                formatter,
                "table {table} is still being copied from its source; retry once its snapshot completes"
            ),
            Self::MissingSnapshot {
                database_id,
                table_id,
            } => write!(
                formatter,
                "no pinned snapshot for database {} table {}",
                database_id.get(),
                table_id.get()
            ),
            Self::SnapshotSchemaChanged {
                database_id,
                table_id,
                expected,
                actual,
            } => write!(
                formatter,
                "snapshot schema changed for database {} table {}: bound version {expected}, pinned version {actual}",
                database_id.get(),
                table_id.get()
            ),
            Self::MemoryLimitExceeded {
                used,
                requested,
                limit,
                scope,
            } => write!(
                formatter,
                "{} memory limit exceeded: {used} bytes used, {requested} requested, {limit} limit",
                scope.describe()
            ),
            Self::Batch(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Batch(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BatchError> for ExecError {
    fn from(error: BatchError) -> Self {
        Self::Batch(error)
    }
}
