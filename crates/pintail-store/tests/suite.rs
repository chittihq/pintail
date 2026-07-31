//! Consolidated integration-test harness: one linked binary for the suites
//! that tolerate sharing a process. `compaction`, `schema_evolution`, and `crash_fuzz` stay
//! separate binaries: their drop-then-reopen loops intermittently hit
//! `WriterBusy` when 50+ tests share one process (under investigation — the
//! writer flock should be free the moment the store drops).

mod suite {
    mod database;
    mod encodings;
    mod flush;
    mod ingest;
    mod key_modes;
    mod native_units;
    mod partitioned_scan;
    mod reader;
    mod recovery;
}
