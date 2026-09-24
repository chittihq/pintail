//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.

mod common;
mod local_writes;
mod published_generation;
mod replica_cache;
mod retained_writer_locks;
mod shared_queries;
mod shared_query_burst;
mod unique_read_policy;
mod unreadable_table;
