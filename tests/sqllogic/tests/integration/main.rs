//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.

mod agg_spill;
mod budget_spill;
mod date_prune;
mod datetime_prune_fires;
mod join_spill;
mod report_shapes;
mod sort_spill;
mod two_pass_spill;
