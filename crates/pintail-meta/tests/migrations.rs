#[test]
fn opening_a_blank_control_plane_applies_the_initial_schema() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");

    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.schema_version().expect("schema version"), 1);
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

    assert_eq!(reopened.schema_version().expect("schema version"), 1);
}
