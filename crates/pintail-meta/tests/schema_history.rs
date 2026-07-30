use pintail_meta::MetaStore;
use rusqlite::Connection;

#[test]
fn schema_generations_are_idempotent_and_dropped_tables_become_orphans() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    store
        .upsert_database("source", "app", b"mysql://source", "2026-07-30T00:00:00Z")
        .expect("register database");
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
    store
        .record_schema_history(
            "source",
            "events",
            2,
            Some("ALTER TABLE events ADD COLUMN note TEXT"),
            r#"[{"id":1,"name":"id"},{"id":2,"name":"note"}]"#,
            "2026-07-30T01:00:00Z",
        )
        .expect("record schema");
    store
        .record_schema_history(
            "source",
            "events",
            2,
            Some("ALTER TABLE events ADD COLUMN note TEXT"),
            r#"[{"id":1,"name":"id"},{"id":2,"name":"note"}]"#,
            "2026-07-30T01:00:00Z",
        )
        .expect("replay schema");
    let history = store
        .schema_history("source", "events")
        .expect("schema history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, 2);

    store
        .mark_table_orphaned(
            "source",
            "events",
            "DROP TABLE events",
            "2026-07-30T02:00:00Z",
        )
        .expect("mark orphan");
    drop(store);
    let connection = Connection::open(path).expect("inspect metadata");
    let row: (String, String, String, i64) = connection
        .query_row(
            "SELECT state, orphaned_at, last_error, schema_version \
             FROM tables WHERE db_id = 'source' AND name = 'events'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("table metadata");
    assert_eq!(
        row,
        (
            "excluded".to_owned(),
            "2026-07-30T02:00:00Z".to_owned(),
            "DROP TABLE events".to_owned(),
            2,
        )
    );
}
