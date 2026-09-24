//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.

mod crash_fuzz;
mod disk_faults;
mod fold_eligibility;
mod mask_cost;
mod merge_output;
mod ranged_overlay;
mod recovery_sequences;
mod supersession_bitmap;
