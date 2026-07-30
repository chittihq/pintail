#[test]
fn opening_a_blank_control_plane_applies_the_initial_schema() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.schema_version().expect("schema version"), 6);
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

    assert_eq!(reopened.schema_version().expect("schema version"), 6);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 6);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 6);
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

#[test]
fn version_three_control_plane_upgrades_schema_tracking_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version three database");
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply version one schema");
    connection
        .execute_batch(include_str!("../migrations/002_polling.sql"))
        .expect("apply version two schema");
    connection
        .execute_batch(include_str!("../migrations/003_poll_checksums.sql"))
        .expect("apply version three schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 6);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let orphaned_column: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tables') \
             WHERE name = 'orphaned_at')",
            [],
            |row| row.get(0),
        )
        .expect("orphaned column");
    assert!(orphaned_column);
}

#[test]
fn version_four_control_plane_upgrades_api_configuration_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version four database");
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply version one schema");
    connection
        .execute_batch(include_str!("../migrations/002_polling.sql"))
        .expect("apply version two schema");
    connection
        .execute_batch(include_str!("../migrations/003_poll_checksums.sql"))
        .expect("apply version three schema");
    connection
        .execute_batch(include_str!("../migrations/004_schema_tracking.sql"))
        .expect("apply version four schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 6);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let api_scopes: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') \
             WHERE name = 'scopes_json')",
            [],
            |row| row.get(0),
        )
        .expect("API scopes column");
    let cadence: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('databases') \
             WHERE name = 'poll_interval_seconds')",
            [],
            |row| row.get(0),
        )
        .expect("poll cadence column");
    assert!(api_scopes);
    assert!(cadence);
}

#[test]
fn version_five_control_plane_upgrades_wire_auth_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version five database");
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply version one schema");
    connection
        .execute_batch(include_str!("../migrations/002_polling.sql"))
        .expect("apply version two schema");
    connection
        .execute_batch(include_str!("../migrations/003_poll_checksums.sql"))
        .expect("apply version three schema");
    connection
        .execute_batch(include_str!("../migrations/004_schema_tracking.sql"))
        .expect("apply version four schema");
    connection
        .execute_batch(include_str!("../migrations/005_api_control.sql"))
        .expect("apply version five schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 6);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let native_hash: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') \
             WHERE name = 'mysql_native_password_hash')",
            [],
            |row| row.get(0),
        )
        .expect("wire verifier column");
    assert!(native_hash);
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
