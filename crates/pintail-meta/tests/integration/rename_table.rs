//! Renaming a tracked table moves every row the metadata store keys by its
//! name, in one transaction, and refuses a name already tracked.
use pintail_meta::MetaStore;

const NOW: &str = "2026-09-07T00:00:00Z";

#[test]
fn a_rename_moves_the_table_its_history_and_its_fence() {
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
    meta.complete_snapshot_chunk("db", "events", "all", 7)
        .unwrap();
    meta.set_setting("cdc_snapshot_fence:db:events", "binlog.000003:1234")
        .unwrap();

    meta.rename_table("db", "events", "archived_events")
        .unwrap();

    let names = meta
        .tables("db")
        .unwrap()
        .into_iter()
        .map(|table| table.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["archived_events", "other"]);
    assert!(meta.schema_history("db", "events").unwrap().is_empty());
    let history = meta.schema_history("db", "archived_events").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, 2);
    assert_eq!(meta.setting("cdc_snapshot_fence:db:events").unwrap(), None);
    assert_eq!(
        meta.setting("cdc_snapshot_fence:db:archived_events")
            .unwrap(),
        Some("binlog.000003:1234".to_owned())
    );

    // The old name is gone, the new one is taken.
    assert!(meta.rename_table("db", "events", "elsewhere").is_err());
    assert!(meta.rename_table("db", "other", "archived_events").is_err());
    // Case-insensitive like the source: a spelling variant is the same name.
    assert!(meta.rename_table("db", "other", "Archived_Events").is_err());
}
