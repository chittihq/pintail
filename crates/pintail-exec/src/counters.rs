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
    /// Distinct inputs evaluated by a repeated temporal column kernel.
    pub temporal_values_evaluated: u64,
    /// Rows a sort buffered as a vector of values.
    pub rows_sorted: u64,
    /// Cells copied from rows back into columns.
    pub cells_regathered: u64,
    /// Aggregates answered by merging an insert-only memtable delta onto a
    /// memoized settled result, rather than reading the table.
    pub settled_delta_merges: u64,
    /// Segment spans a grouped aggregate folded because no cached fold
    /// could be reused.
    pub grouped_spans_folded: u64,
    /// Segment spans a grouped aggregate took from the fold cache.
    pub grouped_spans_reused: u64,
    /// Text sort keys prepared as collation weight keys, once per row.
    pub sort_keys_prepared: u64,
    /// Text sort keys left on the comparator because the query ceiling had
    /// no room to hold weight keys for every row.
    pub sort_keys_unprepared: u64,
    /// Hash indexes a dependent `EXISTS` or scalar subquery built so outer
    /// rows look their correlation keys up instead of each executing the
    /// inner query.
    pub dependent_index_builds: u64,
    /// Outer rows a dependent subquery index answered.
    pub dependent_index_probes: u64,
    /// Driving rows a key lookup join turned into values to join them.
    pub lookup_rows_joined: u64,
    /// Rows an `IN (subquery)` conjunct tested by looking their value up by
    /// the subquery table's key, in place of building the set.
    pub membership_rows_looked_up: u64,
    /// `IN (subquery)` tests of a constant answered by asking the subquery
    /// about that constant alone.
    pub membership_point_queries: u64,
    /// Row constructor `IN (subquery)` tests whose members were read once
    /// and compared with each outer row, in place of a subquery per row.
    pub row_members_expanded: u64,
    /// Windows an integer-range fold took whole against the range it
    /// already had, checking each key as its slot was computed.
    pub range_windows_in_range: u64,
    /// Windows an integer-range fold read key bounds for ahead of folding:
    /// the first one, and any whose keys left the range.
    pub range_windows_bounded: u64,
    /// Rounds of a scan an aggregate folded in place: each worker decoded a
    /// slice of the table and folded it itself.
    pub fused_rounds: u64,
    /// Batches those rounds folded whole.
    pub fused_batches: u64,
    /// Stored `TIMESTAMP` texts a session-zone kernel read as canonical
    /// text, without the general time zone conversion.
    pub session_texts_read: u64,
    /// Batches whose packed calendar column the copy check passed whole.
    pub calendar_copies_packed: u64,
}

thread_local! {
    static COUNTERS: Cell<ExecCounters> = const {
        Cell::new(ExecCounters {
            values_materialized: 0,
            rows_projected_scalar: 0,
            temporal_values_evaluated: 0,
            rows_sorted: 0,
            cells_regathered: 0,
            settled_delta_merges: 0,
            grouped_spans_folded: 0,
            grouped_spans_reused: 0,
            sort_keys_prepared: 0,
            sort_keys_unprepared: 0,
            dependent_index_builds: 0,
            dependent_index_probes: 0,
            lookup_rows_joined: 0,
            membership_rows_looked_up: 0,
            membership_point_queries: 0,
            row_members_expanded: 0,
            range_windows_in_range: 0,
            range_windows_bounded: 0,
            fused_rounds: 0,
            fused_batches: 0,
            session_texts_read: 0,
            calendar_copies_packed: 0,
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
