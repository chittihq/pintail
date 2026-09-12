//! Per-statement counts of value-at-a-time work, for measurement runs.
//!
//! The executor's columns are typed, but several paths turn them into
//! `Value`s a cell at a time. These counters say how much of that one
//! statement did on the thread that ran it; the wire's query trace reports
//! them. Each is bumped once per batch, or once per row on a path that
//! already allocates that row, so they stay on without a switch. Work done
//! on a parallel pool's threads is not counted.

use std::cell::Cell;

/// What one statement's execution materialized on this thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecCounters {
    /// Cells materialized from a typed column into `Value`s.
    pub values_materialized: u64,
    /// Rows the scalar projection path evaluated expression by expression.
    pub rows_projected_scalar: u64,
    /// Rows a sort buffered as a vector of values.
    pub rows_sorted: u64,
    /// Cells copied from rows back into columns.
    pub cells_regathered: u64,
    /// Aggregates answered by merging an insert-only memtable delta onto a
    /// memoized settled result, rather than reading the table.
    pub settled_delta_merges: u64,
}

thread_local! {
    static COUNTERS: Cell<ExecCounters> = const {
        Cell::new(ExecCounters {
            values_materialized: 0,
            rows_projected_scalar: 0,
            rows_sorted: 0,
            cells_regathered: 0,
            settled_delta_merges: 0,
        })
    };
}

/// Adds to this thread's counts.
pub(crate) fn count(update: impl FnOnce(&mut ExecCounters)) {
    COUNTERS.with(|cell| {
        let mut counters = cell.get();
        update(&mut counters);
        cell.set(counters);
    });
}

/// Takes the counts this thread accumulated since the last take.
#[must_use]
pub fn take_exec_counters() -> ExecCounters {
    COUNTERS.with(|cell| cell.replace(ExecCounters::default()))
}
