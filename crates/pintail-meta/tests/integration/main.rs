//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.

mod activity_scale;
mod cdc;
mod control;
mod copy_complete;
mod interrupted_snapshots;
mod migrations;
mod poll;
mod recovery_failpoints;
mod remove_table;
mod rename_table;
mod replica_signature;
mod schema_history;
mod settings;
mod snapshot;
