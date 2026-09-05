//! The copy marker: which tables a restart may hand straight back to
//! replication, and which it must copy. Table state cannot say - a snapshot
//! walk, a resync and a failed job all rewrite it - so the marker follows
//! the copy itself: set when a copy or resync reaches its end, cleared when
//! one begins or the table is flagged for one.

use pintail_meta::MetaStore;

fn store_with_database() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().expect("temporary metadata directory");
    let store =
        MetaStore::open(&directory.path().join("pintail-meta.db")).expect("open metadata store");
    store
        .upsert_database("db-1", "shop", b"secret", "2026-09-05T00:00:00Z")
        .expect("database");
    (directory, store)
}

fn table_state(store: &MetaStore, name: &str) -> (String, bool) {
    let table = store
        .tables("db-1")
        .expect("tables")
        .into_iter()
        .find(|table| table.name == name)
        .expect("tracked table");
    (table.state, table.copy_complete)
}

#[test]
fn the_marker_follows_the_copy_not_the_state() {
    let (_directory, mut store) = store_with_database();
    store
        .upsert_snapshot_table("db-1", "orders", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register");
    assert_eq!(
        table_state(&store, "orders"),
        ("snapshotting".to_owned(), false)
    );

    store
        .complete_snapshot_table("db-1", "orders")
        .expect("copy completes");
    assert_eq!(table_state(&store, "orders"), ("pending".to_owned(), true));

    store
        .mark_table_needs_resync("db-1", "orders", "row image is wider than the schema")
        .expect("quarantine");
    assert_eq!(
        table_state(&store, "orders"),
        ("needs_resync".to_owned(), false)
    );

    store
        .begin_table_resnapshot("db-1", "orders")
        .expect("resync begins");
    assert_eq!(
        table_state(&store, "orders"),
        ("snapshotting".to_owned(), false)
    );

    store
        .finish_table_resnapshot("db-1", "orders", "streaming")
        .expect("resync finishes");
    assert_eq!(
        table_state(&store, "orders"),
        ("streaming".to_owned(), true)
    );

    store
        .fail_snapshot_table("db-1", "orders", "the source went away")
        .expect("copy fails");
    assert_eq!(table_state(&store, "orders"), ("error".to_owned(), false));
    let _ = &mut store;
}

#[test]
fn a_restart_hands_complete_tables_back_and_copies_only_the_rest() {
    let (_directory, mut store) = store_with_database();
    for name in ["activity_log", "orders", "payment", "users"] {
        store
            .upsert_snapshot_table("db-1", name, Some("[\"id\"]"), Some("[\"id\"]"))
            .expect("register");
    }
    // Three tables copied to the end; the walk a restart cut short had
    // moved them through pending, error and snapshotting respectively.
    for name in ["activity_log", "orders", "users"] {
        store
            .complete_snapshot_table("db-1", name)
            .expect("copy completes");
    }
    store
        .fail_snapshot_table(
            "db-1",
            "orders",
            "snapshot storage failed: direct snapshot ingest requires an empty memtable",
        )
        .expect("the walk refused a live table");
    // A failure marks the table incomplete; the walk's refusal is the one
    // failure that proves the opposite, and the migration backfill covers
    // that text for stores written before the marker existed. Here the
    // marker is authoritative, so restore it the way the backfill would.
    store
        .upsert_snapshot_table("db-1", "users", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("the walk re-registered a complete table");
    store
        .start_snapshot_chunk("db-1", "users", "chunk-000000000000000009", None, None)
        .expect("a chunk the walk left running");
    // Payment never finished: its resync had reset the journal.
    store
        .begin_table_resnapshot("db-1", "payment")
        .expect("resync begins");

    assert_eq!(
        store
            .tables_without_complete_copy("db-1")
            .expect("incomplete")
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["orders".to_owned(), "payment".to_owned()],
        "a failed copy and a mid-copy table are the ones to copy"
    );

    let restored = store
        .restore_complete_tables("db-1", "streaming")
        .expect("restore");
    assert_eq!(
        restored,
        vec!["activity_log".to_owned(), "users".to_owned()]
    );
    assert_eq!(
        table_state(&store, "activity_log"),
        ("streaming".to_owned(), true)
    );
    assert_eq!(table_state(&store, "users"), ("streaming".to_owned(), true));
    assert_eq!(
        table_state(&store, "payment"),
        ("snapshotting".to_owned(), false)
    );
    assert_eq!(table_state(&store, "orders"), ("error".to_owned(), false));
    assert!(
        store
            .snapshot_chunks("db-1", "users")
            .expect("journal")
            .iter()
            .all(|chunk| chunk.status == pintail_meta::SnapshotChunkStatus::Completed),
        "the chunk the walk left running is dropped with the restore"
    );
    let _ = &mut store;
}

/// A store written before the marker existed: the backfill reads the copy
/// out of the states a walk, a handoff and a failed job leave behind.
#[test]
fn upgrading_a_store_backfills_the_marker_from_the_old_states() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let path = directory.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&path).expect("old store");
    for migration in [
        include_str!("../migrations/001_initial.sql"),
        include_str!("../migrations/002_polling.sql"),
        include_str!("../migrations/003_poll_checksums.sql"),
        include_str!("../migrations/004_schema_tracking.sql"),
        include_str!("../migrations/005_api_control.sql"),
        include_str!("../migrations/006_wire_auth.sql"),
        include_str!("../migrations/007_backups.sql"),
        include_str!("../migrations/008_restored_tables.sql"),
        include_str!("../migrations/009_backup_retention.sql"),
        include_str!("../migrations/010_keyless_policy.sql"),
        include_str!("../migrations/011_backup_verification.sql"),
        include_str!("../migrations/012_backup_full_cadence.sql"),
        include_str!("../migrations/013_caching_sha2.sql"),
        include_str!("../migrations/014_database_kind.sql"),
        include_str!("../migrations/015_workspaces.sql"),
        include_str!("../migrations/016_oauth_invites_audit.sql"),
        include_str!("../migrations/017_audit_client_ip.sql"),
        include_str!("../migrations/018_local_tables.sql"),
        include_str!("../migrations/019_activity_indexes.sql"),
    ] {
        connection
            .execute_batch(migration)
            .expect("apply an old migration");
    }
    connection
        .execute(
            "INSERT INTO databases (id, name, mysql_dsn_encrypted, mode, state, created_at, updated_at) \
             VALUES ('db-1', 'shop', X'00', 'auto', 'streaming', 't', 't')",
            [],
        )
        .expect("database row");
    let walk_error = "snapshot storage failed: storage format limit: \
                      direct snapshot ingest requires an empty memtable";
    for (name, state, rows, error) in [
        ("streaming_table", "streaming", 10, None),
        ("pending_table", "pending", 10, None),
        ("refused_table", "error", 124, Some(walk_error)),
        ("job_failed_table", "error", 50, Some(walk_error)),
        ("never_copied", "error", 0, Some(walk_error)),
        ("mid_copy", "snapshotting", 0, None),
        ("other_error", "error", 7, Some("the source went away")),
    ] {
        connection
            .execute(
                "INSERT INTO tables (db_id, name, state, rows_synced, last_error, schema_version) \
                 VALUES ('db-1', ?1, ?2, ?3, ?4, 1)",
                rusqlite::params![name, state, rows, error],
            )
            .expect("table row");
    }
    drop(connection);

    let upgraded = MetaStore::open(&path).expect("upgrade");
    assert_eq!(upgraded.schema_version().expect("version"), 20);
    let marked = upgraded
        .tables("db-1")
        .expect("tables")
        .into_iter()
        .filter(|table| table.copy_complete)
        .map(|table| table.name)
        .collect::<Vec<_>>();
    assert_eq!(
        marked,
        vec![
            "job_failed_table".to_owned(),
            "pending_table".to_owned(),
            "refused_table".to_owned(),
            "streaming_table".to_owned(),
        ]
    );
}
