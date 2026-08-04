//! Consolidated integration-test harness: one linked binary instead of one
//! per file. Reopen loops under concurrent process spawns transiently see
//! the old writer flock as held (spawned children briefly keep inherited
//! file descriptions alive); `lock_writer`'s bounded retry absorbs that, so
//! sharing a process is safe again. `crash_fuzz` stays separate: it spawns
//! and kill -9s real workers of its own binary by design.

mod suite {
    mod compaction;
    mod database;
    mod direct_scan;
    mod encodings;
    mod flush;
    mod ingest;
    mod key_modes;
    mod native_units;
    mod partitioned_scan;
    mod polling_noop;
    mod reader;
    mod recovery;
    mod schema_evolution;
    mod transactional;
    mod value_pruning;
}
