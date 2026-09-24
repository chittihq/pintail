//! One test binary for the crate's integration tests. Each module was its own
//! binary, and every one of them linked the whole engine; nextest still runs
//! each test in its own process, so the merge shares no state between them.

mod layout;
mod schema;
