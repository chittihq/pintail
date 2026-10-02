//! The write generation: how a process that is the only writer of its
//! metadata store learns the store was written without looking at a file.

use pintail_meta::{MetaStore, write_generation};

const NOW: &str = "2026-10-02T00:00:00Z";

#[test]
fn every_committed_write_moves_the_generation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("open");
    let opened = write_generation();

    store
        .upsert_database("db", "source", b"dsn", NOW)
        .expect("create");
    let created = write_generation();
    assert!(created > opened, "an autocommitted write");
    store
        .upsert_snapshot_table("db", "a", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .expect("table");

    store
        .record_schema_history("db", "a", 1, Some("CREATE TABLE a"), "[]", NOW)
        .expect("history");
    let recorded = write_generation();
    assert!(recorded > created, "a write inside a transaction");

    // A second connection in the process is counted the same way, and what
    // it committed is there to read once the generation has moved.
    let mut other = MetaStore::open(&path).expect("second connection");
    other
        .record_schema_history("db", "a", 2, Some("ALTER TABLE a"), "[]", NOW)
        .expect("history");
    assert!(write_generation() > recorded);
    assert_eq!(store.schema_history("db", "a").expect("read").len(), 2);

    // A connection handed back and taken again still counts.
    drop(other);
    let before = write_generation();
    let mut reused = MetaStore::open(&path).expect("reused connection");
    reused
        .record_schema_history("db", "a", 3, Some("ALTER TABLE a"), "[]", NOW)
        .expect("history");
    assert!(write_generation() > before);
}

/// Counting commits takes over the hook that checkpoints the write-ahead
/// log, so the count has to checkpoint it too: the log must not grow with
/// every commit for as long as the process lives.
#[test]
fn the_write_ahead_log_is_still_checkpointed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("open");
    store
        .upsert_database("db", "source", b"dsn", NOW)
        .expect("create");
    store
        .upsert_snapshot_table("db", "a", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .expect("table");
    // About a hundred pages a commit: left alone, four hundred commits
    // would leave a log of forty thousand frames.
    let columns = format!("[\"{}\"]", "x".repeat(400_000));
    for version in 1..=400 {
        store
            .record_schema_history("db", "a", version, None, &columns, NOW)
            .expect("history");
    }
    let mut log = path.clone().into_os_string();
    log.push("-wal");
    let log_bytes = std::fs::metadata(&log).expect("the log").len();
    assert!(
        log_bytes < 64 * 1024 * 1024,
        "the log holds {log_bytes} bytes: it was never checkpointed"
    );
}
