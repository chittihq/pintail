//! Single integration-test harness: one linked binary instead of eleven
//! (the target directory lives on a slow external volume; see Cargo.toml
//! profile notes).

mod suite {
    mod compaction;
    mod crash_fuzz;
    mod database;
    mod encodings;
    mod flush;
    mod ingest;
    mod key_modes;
    mod partitioned_scan;
    mod reader;
    mod recovery;
    mod schema_evolution;
}
