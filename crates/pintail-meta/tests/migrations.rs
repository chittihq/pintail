#[test]
fn opening_a_blank_control_plane_applies_the_initial_schema() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.schema_version().expect("schema version"), 3);
}

#[test]
fn initial_schema_contains_every_control_plane_table() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");
    drop(metadata);

    let connection = rusqlite::Connection::open(database_path).expect("inspect metadata schema");
    let mut query = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("schema query");
    let table_names: Vec<String> = query
        .query_map([], |row| row.get(0))
        .expect("table rows")
        .collect::<Result<_, _>>()
        .expect("table names");

    assert_eq!(
        table_names,
        [
            "api_keys",
            "checkpoints",
            "databases",
            "dlq",
            "poll_chunk_states",
            "poll_states",
            "schema_history",
            "settings",
            "snapshot_chunks",
            "sync_runs",
            "tables",
            "users",
        ]
    );
}

#[test]
fn reopening_an_initialized_control_plane_is_idempotent() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    pintail_meta::MetaStore::open(&database_path).expect("first open");
    let reopened = pintail_meta::MetaStore::open(&database_path).expect("second open");

    assert_eq!(reopened.schema_version().expect("schema version"), 3);
}

#[test]
fn version_one_control_plane_upgrades_polling_state_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version one database");
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply version one schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 3);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name = 'poll_states')",
            [],
            |row| row.get(0),
        )
        .expect("poll state table");
    assert!(exists);
}

#[test]
fn version_two_control_plane_upgrades_polling_checksums_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version two database");
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply version one schema");
    connection
        .execute_batch(include_str!("../migrations/002_polling.sql"))
        .expect("apply version two schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 3);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name = 'poll_chunk_states')",
            [],
            |row| row.get(0),
        )
        .expect("poll chunk state table");
    assert!(exists);
}

#[cfg(unix)]
#[test]
fn control_plane_database_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    for path in [
        database_path.clone(),
        database_path.with_extension("db-wal"),
        database_path.with_extension("db-shm"),
    ] {
        let mode = std::fs::metadata(&path)
            .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{} is not owner-only", path.display());
    }

    drop(metadata);
}
