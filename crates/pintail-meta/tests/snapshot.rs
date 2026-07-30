use pintail_meta::{MetaStore, SnapshotCheckpointRecord, SnapshotChunkStatus};

fn registered_store() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().expect("temporary metadata directory");
    let store =
        MetaStore::open(&directory.path().join("pintail-meta.db")).expect("open metadata store");
    store
        .upsert_database("source", "app", b"encrypted", "2026-07-30T00:00:00Z")
        .expect("register source");
    (directory, store)
}

#[test]
fn resumed_snapshot_preserves_the_original_handoff_checkpoint() {
    let (_directory, store) = registered_store();
    store
        .insert_snapshot_checkpoint_if_absent(
            "source",
            "filepos",
            None,
            Some("mysql-bin.000001"),
            Some(123),
            "2026-07-30T00:00:01Z",
        )
        .expect("first checkpoint");
    store
        .insert_snapshot_checkpoint_if_absent(
            "source",
            "filepos",
            None,
            Some("mysql-bin.000002"),
            Some(456),
            "2026-07-30T00:00:02Z",
        )
        .expect("resume checkpoint");

    assert_eq!(
        store.snapshot_checkpoint("source").expect("checkpoint"),
        Some(SnapshotCheckpointRecord {
            kind: "filepos".to_owned(),
            gtid_set: None,
            binlog_file: Some("mysql-bin.000001".to_owned()),
            binlog_pos: Some(123),
        })
    );
}

#[test]
fn chunk_completion_is_durable_and_advances_progress_once() {
    let (directory, mut store) = registered_store();
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
    store
        .start_snapshot_chunk(
            "source",
            "events",
            "chunk-000",
            None,
            Some("[{\"uint\":2}]"),
        )
        .expect("start chunk");
    store
        .complete_snapshot_chunk("source", "events", "chunk-000", 2)
        .expect("complete chunk");
    store
        .start_snapshot_chunk("source", "events", "chunk-000", None, None)
        .expect("idempotent restart");
    store
        .complete_snapshot_chunk("source", "events", "chunk-000", 999)
        .expect("idempotent completion");

    let chunks = store
        .snapshot_chunks("source", "events")
        .expect("snapshot chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].status, SnapshotChunkStatus::Completed);
    assert_eq!(chunks[0].rows, 2);
    assert_eq!(
        store
            .completed_snapshot_chunks("source", "events")
            .expect("completed chunks")
            .into_iter()
            .collect::<Vec<_>>(),
        ["chunk-000"]
    );
    drop(store);
    let connection = rusqlite::Connection::open(directory.path().join("pintail-meta.db"))
        .expect("inspect progress");
    let rows_synced: u64 = connection
        .query_row(
            "SELECT rows_synced FROM tables WHERE db_id = 'source' AND name = 'events'",
            [],
            |row| row.get(0),
        )
        .expect("rows synced");
    assert_eq!(rows_synced, 2);
}
