#[test]
fn opening_a_blank_control_plane_applies_the_initial_schema() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.schema_version().expect("schema version"), 19);
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
            "audit_log",
            "backup_configs",
            "backups",
            "checkpoints",
            "databases",
            "dlq",
            "invites",
            "poll_chunk_states",
            "poll_states",
            "schema_history",
            "settings",
            "snapshot_chunks",
            "sync_runs",
            "tables",
            "users",
            "workspace_members",
            "workspaces",
        ]
    );
}

#[test]
fn reopening_an_initialized_control_plane_is_idempotent() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    pintail_meta::MetaStore::open(&database_path).expect("first open");
    let reopened = pintail_meta::MetaStore::open(&database_path).expect("second open");

    assert_eq!(reopened.schema_version().expect("schema version"), 19);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
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
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
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

#[test]
fn version_six_control_plane_upgrades_backup_state_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version six database");
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
    connection
        .execute_batch(include_str!("../migrations/006_wire_auth.sql"))
        .expect("apply version six schema");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let backup_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name IN ('backup_configs', 'backups')",
            [],
            |row| row.get(0),
        )
        .expect("backup tables");
    assert_eq!(backup_tables, 2);
}

#[test]
fn version_seven_adds_restored_table_state_without_losing_children() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version seven database");
    for migration in [
        include_str!("../migrations/001_initial.sql"),
        include_str!("../migrations/002_polling.sql"),
        include_str!("../migrations/003_poll_checksums.sql"),
        include_str!("../migrations/004_schema_tracking.sql"),
        include_str!("../migrations/005_api_control.sql"),
        include_str!("../migrations/006_wire_auth.sql"),
        include_str!("../migrations/007_backups.sql"),
    ] {
        connection
            .execute_batch(migration)
            .expect("apply historical migration");
    }
    connection
        .execute(
            "INSERT INTO databases (
               id, name, mysql_dsn_encrypted, mode, state, created_at, updated_at
             ) VALUES ('db', 'app', X'', 'paused', 'restored', 'now', 'now')",
            [],
        )
        .expect("seed database");
    connection
        .execute(
            "INSERT INTO tables (db_id, name, state) VALUES ('db', 'events', 'excluded')",
            [],
        )
        .expect("seed table");
    connection
        .execute(
            "INSERT INTO snapshot_chunks (db_id, table_name, chunk_id, status)
             VALUES ('db', 'events', 'chunk', 'completed')",
            [],
        )
        .expect("seed child row");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    connection
        .execute(
            "UPDATE tables SET state = 'restored' WHERE db_id = 'db' AND name = 'events'",
            [],
        )
        .expect("restored table state is accepted");
    let chunks: u64 = connection
        .query_row("SELECT COUNT(*) FROM snapshot_chunks", [], |row| row.get(0))
        .expect("retained child rows");
    assert_eq!(chunks, 1);
}

#[test]
fn version_twelve_control_plane_gains_caching_sha2_verifiers_in_place() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version twelve database");
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
    ] {
        connection
            .execute_batch(migration)
            .expect("apply historical migration");
    }
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
    drop(upgraded);
    let connection = rusqlite::Connection::open(database_path).expect("inspect upgrade");
    let caching_sha2: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') \
             WHERE name = 'caching_sha2_password_hash')",
            [],
            |row| row.get(0),
        )
        .expect("caching_sha2 verifier column");
    assert!(caching_sha2);
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

#[test]
fn version_seventeen_widens_table_states_without_losing_rows() {
    // Migration 18 rebuilds `tables` to widen a CHECK constraint, which is
    // the migration shape that silently loses rows if the copy is wrong.
    // Seed a v17 control plane with a table row and its child rows, then
    // prove the upgrade preserved them and accepts the two new states.
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let connection = rusqlite::Connection::open(&database_path).expect("version seventeen");
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
    ] {
        connection
            .execute_batch(migration)
            .expect("apply historical migration");
    }
    connection
        .execute(
            "INSERT INTO databases (\
               id, name, mysql_dsn_encrypted, mode, state, created_at, updated_at\
             ) VALUES ('db-1', 'shop', X'00', 'auto', 'streaming', 'now', 'now')",
            [],
        )
        .expect("seed database");
    connection
        .execute(
            "INSERT INTO tables (db_id, name, state, pk_json, rows_synced, schema_version) \
             VALUES ('db-1', 'orders', 'streaming', '[\"id\"]', 42, 3)",
            [],
        )
        .expect("seed table");
    drop(connection);

    let upgraded = pintail_meta::MetaStore::open(&database_path).expect("upgrade metadata");
    assert_eq!(upgraded.schema_version().expect("schema version"), 19);
    let carried = upgraded.tables("db-1").expect("tables survive the rebuild");
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].name, "orders");
    assert_eq!(carried[0].state, "streaming");
    assert_eq!(carried[0].rows_synced, 42, "counters survive the copy");
    assert_eq!(carried[0].schema_version, 3);
    drop(upgraded);

    let connection = rusqlite::Connection::open(&database_path).expect("inspect upgrade");
    for state in ["creating", "ready"] {
        connection
            .execute(
                "INSERT INTO tables (db_id, name, state, schema_version) \
                 VALUES ('db-1', ?1, ?2, 1)",
                (format!("local_{state}"), state),
            )
            .unwrap_or_else(|error| panic!("state {state} must be accepted: {error}"));
    }
    // The widened CHECK still refuses a state that means nothing.
    assert!(
        connection
            .execute(
                "INSERT INTO tables (db_id, name, state, schema_version) \
                 VALUES ('db-1', 'nonsense', 'wat', 1)",
                [],
            )
            .is_err()
    );
}
