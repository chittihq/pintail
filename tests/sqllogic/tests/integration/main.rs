//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.
//! `cargo test` does not: it runs a binary's tests as threads of one
//! process. A test that lowers the process-wide memory budget therefore
//! lives in its own binary (`budget_spill.rs`), or it refuses the other
//! tests' reservations and counts theirs as its own.

mod agg_spill;
mod date_prune;
mod datetime_prune_fires;
mod join_spill;
mod memtable_ceiling;
mod report_shapes;
mod sort_spill;
mod two_pass_spill;
