//! Removing a table the source dropped forgets every row the metadata store
//! keys by its name, and leaves the other tables alone.
use pintail_meta::MetaStore;

const NOW: &str = "2026-09-25T00:00:00Z";

#[test]
fn removing_a_table_forgets_its_row_history_chunks_and_fence() {
    let directory = tempfile::tempdir().unwrap();
    let mut meta = MetaStore::open(&directory.path().join("meta.db")).unwrap();
    meta.create_local_database("db", "scratch", NOW).unwrap();
    meta.upsert_snapshot_table("db", "events", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .unwrap();
    meta.upsert_snapshot_table("db", "other", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .unwrap();
    meta.record_schema_history(
        "db",
        "events",
        2,
        Some("ALTER TABLE events ADD COLUMN note TEXT"),
        r#"[{"id":1,"name":"id"},{"id":2,"name":"note"}]"#,
        NOW,
    )
    .unwrap();
    meta.start_snapshot_chunk("db", "events", "all", None, None)
        .unwrap();
    meta.set_setting("cdc_snapshot_fence:db:events", "binlog.000003:1234")
        .unwrap();
    meta.set_setting("cdc_snapshot_fence:db:other", "binlog.000003:99")
        .unwrap();

    // Any spelling finds the row; the stored spelling comes back.
    assert_eq!(
        meta.remove_table("db", "EVENTS").unwrap().as_deref(),
        Some("events")
    );

    let names = meta
        .tables("db")
        .unwrap()
        .into_iter()
        .map(|table| table.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["other"]);
    assert!(meta.schema_history("db", "events").unwrap().is_empty());
    assert_eq!(meta.setting("cdc_snapshot_fence:db:events").unwrap(), None);
    assert_eq!(
        meta.setting("cdc_snapshot_fence:db:other").unwrap(),
        Some("binlog.000003:99".to_owned())
    );
    // A second removal finds nothing.
    assert_eq!(meta.remove_table("db", "events").unwrap(), None);
}
