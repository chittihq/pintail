use pintail_meta::{MetaStore, SnapshotCheckpointRecord};
use rusqlite::Connection;

#[test]
fn cdc_checkpoint_updates_position_and_streaming_state_atomically() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    register_source(&store);

    store
        .commit_cdc_checkpoint(
            "source",
            &SnapshotCheckpointRecord {
                kind: "gtid".to_owned(),
                gtid_set: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:1-7".to_owned()),
                binlog_file: Some("mysql-bin.000003".to_owned()),
                binlog_pos: Some(918),
            },
            &["events".to_owned()],
            "2026-07-30T01:00:00Z",
        )
        .expect("commit CDC checkpoint");

    let checkpoint = store
        .snapshot_checkpoint("source")
        .expect("read checkpoint")
        .expect("checkpoint");
    assert_eq!(checkpoint.kind, "gtid");
    assert_eq!(
        checkpoint.gtid_set.as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:1-7")
    );
    assert_eq!(checkpoint.binlog_file.as_deref(), Some("mysql-bin.000003"));
    assert_eq!(checkpoint.binlog_pos, Some(918));
    let connection = Connection::open(&path).expect("inspect metadata");
    let database: (String, String) = connection
        .query_row(
            "SELECT state, effective_mode FROM databases WHERE id = 'source'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("database state");
    assert_eq!(database, ("streaming".to_owned(), "cdc".to_owned()));
    let table: (String, Option<String>) = connection
        .query_row(
            "SELECT state, last_error FROM tables \
             WHERE db_id = 'source' AND name = 'events'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("table state");
    assert_eq!(table, ("streaming".to_owned(), None));
}

#[test]
fn resync_and_dlq_updates_are_durable_and_idempotent() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    register_source(&store);
    store
        .mark_table_needs_resync("source", "events", "cannot decode row")
        .expect("mark table");
    store
        .commit_cdc_checkpoint(
            "source",
            &SnapshotCheckpointRecord {
                kind: "filepos".to_owned(),
                gtid_set: None,
                binlog_file: Some("mysql-bin.000001".to_owned()),
                binlog_pos: Some(124),
            },
            &["events".to_owned()],
            "2026-07-30T01:00:00Z",
        )
        .expect("advance checkpoint without clearing resync");
    store
        .record_dlq(
            "cdc:source:mysql-bin.000001:123",
            "source",
            Some("events"),
            r#"{"position":123}"#,
            "first failure",
            "2026-07-30T01:00:00Z",
        )
        .expect("first DLQ write");
    store
        .record_dlq(
            "cdc:source:mysql-bin.000001:123",
            "source",
            Some("events"),
            r#"{"position":123}"#,
            "replayed failure",
            "2026-07-30T01:01:00Z",
        )
        .expect("idempotent DLQ write");
    store
        .mark_database_needs_resync("source", "source purged binlog")
        .expect("mark database");

    let connection = Connection::open(&path).expect("inspect metadata");
    let database_state: String = connection
        .query_row(
            "SELECT state FROM databases WHERE id = 'source'",
            [],
            |row| row.get(0),
        )
        .expect("database state");
    assert_eq!(database_state, "needs_resync");
    let (table_state, last_error): (String, String) = connection
        .query_row(
            "SELECT state, last_error FROM tables \
             WHERE db_id = 'source' AND name = 'events'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("table state");
    assert_eq!(table_state, "needs_resync");
    assert_eq!(last_error, "source purged binlog");
    let (count, error, created_at): (u64, String, String) = connection
        .query_row(
            "SELECT COUNT(*), MAX(error), MAX(created_at) FROM dlq",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("DLQ row");
    assert_eq!(count, 1);
    assert_eq!(error, "replayed failure");
    assert_eq!(created_at, "2026-07-30T01:00:00Z");
}

#[test]
fn begin_resnapshot_clears_the_old_handoff_and_chunk_journal() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let store = MetaStore::open(&path).expect("metadata");
    register_source(&store);
    store
        .upsert_snapshot_checkpoint(
            "source",
            "filepos",
            None,
            Some("mysql-bin.000001"),
            Some(123),
            "2026-07-30T00:00:00Z",
        )
        .expect("old checkpoint");
    store
        .start_snapshot_chunk("source", "events", "chunk-000", None, None)
        .expect("old chunk");
    store
        .mark_database_needs_resync("source", "purged")
        .expect("mark source");

    store
        .begin_resnapshot("source", "2026-07-30T02:00:00Z")
        .expect("begin fresh snapshot");
    assert!(
        store
            .snapshot_checkpoint("source")
            .expect("checkpoint query")
            .is_none()
    );
    assert!(
        store
            .snapshot_chunks("source", "events")
            .expect("chunk query")
            .is_empty()
    );
    assert!(
        store
            .tables_needing_resync("source")
            .expect("resync query")
            .is_empty()
    );
    let connection = Connection::open(&path).expect("inspect metadata");
    let state: String = connection
        .query_row(
            "SELECT state FROM databases WHERE id = 'source'",
            [],
            |row| row.get(0),
        )
        .expect("database state");
    assert_eq!(state, "snapshotting");
}

#[test]
fn cdc_reconciliation_preserves_stream_mode_and_binlog_checkpoint() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    store
        .upsert_database("source", "app", b"mysql://source", "2026-07-30T00:00:00Z")
        .expect("register database");
    store
        .upsert_snapshot_table("source", "children", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
    store
        .upsert_snapshot_checkpoint(
            "source",
            "filepos",
            None,
            Some("mysql-bin.000007"),
            Some(900),
            "2026-07-30T01:00:00Z",
        )
        .expect("seed checkpoint");
    let streaming_checkpoint = store.snapshot_checkpoint("source").unwrap().unwrap();
    store
        .commit_cdc_checkpoint(
            "source",
            &streaming_checkpoint,
            &["children".to_owned()],
            "2026-07-30T01:00:01Z",
        )
        .expect("mark streaming");
    store
        .commit_cdc_reconciliation("source", "children", 1, 42, "2026-07-30T02:00:00Z")
        .expect("reconcile child");

    let checkpoint = store
        .snapshot_checkpoint("source")
        .unwrap()
        .expect("checkpoint");
    assert_eq!(checkpoint.kind, "filepos");
    assert_eq!(checkpoint.binlog_file.as_deref(), Some("mysql-bin.000007"));
    assert_eq!(checkpoint.binlog_pos, Some(900));
    assert_eq!(
        store
            .poll_state("source", "children")
            .unwrap()
            .unwrap()
            .version,
        42
    );
    let connection = rusqlite::Connection::open(path).expect("inspect mode");
    let mode: (String, String) = connection
        .query_row(
            "SELECT state, effective_mode FROM databases WHERE id = 'source'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("database mode");
    assert_eq!(mode, ("streaming".to_owned(), "cdc".to_owned()));
}

#[test]
fn probing_a_streaming_database_keeps_it_scheduled() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    register_source(&store);

    // Onboarding: the first probe advances 'created' to 'probed'.
    store
        .update_database_probe("source", "{}", "cdc", "2026-07-30T00:30:00Z")
        .expect("first probe");
    let state = |store: &MetaStore| {
        store
            .database("source")
            .expect("read database")
            .expect("database")
            .state
    };
    assert_eq!(state(&store), "probed");

    store
        .commit_cdc_checkpoint(
            "source",
            &SnapshotCheckpointRecord {
                kind: "filepos".to_owned(),
                gtid_set: None,
                binlog_file: Some("mysql-bin.000001".to_owned()),
                binlog_pos: Some(4),
            },
            &["events".to_owned()],
            "2026-07-30T01:00:00Z",
        )
        .expect("reach streaming");
    assert_eq!(state(&store), "streaming");

    // A probe of a live database is an inventory refresh. Writing 'probed'
    // here removed it from the supervisor's schedule - which only picks up
    // streaming/polling/error - so replication stopped silently, for good.
    store
        .update_database_probe("source", "{}", "cdc", "2026-07-30T02:00:00Z")
        .expect("re-probe");
    assert_eq!(state(&store), "streaming");
}

fn register_source(store: &MetaStore) {
    store
        .upsert_database("source", "app", b"mysql://source", "2026-07-30T00:00:00Z")
        .expect("register database");
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
}
