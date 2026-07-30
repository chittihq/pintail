use pintail_meta::{DatabaseUpdate, MetaStore};

#[test]
fn users_databases_and_table_controls_round_trip() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.user_count().unwrap(), 0);
    metadata
        .create_user(
            "user-1",
            "operator@example.com",
            "$argon2id$test",
            "admin",
            "2026-07-30T00:00:00Z",
        )
        .unwrap();
    assert_eq!(metadata.user_count().unwrap(), 1);
    let user = metadata
        .user_by_email("OPERATOR@example.com")
        .unwrap()
        .expect("case-insensitive user");
    assert_eq!(user.role, "admin");
    metadata
        .touch_user_login("user-1", "2026-07-30T00:01:00Z")
        .unwrap();
    assert_eq!(
        metadata.users().unwrap()[0].last_login_at.as_deref(),
        Some("2026-07-30T00:01:00Z")
    );

    metadata
        .upsert_database("db-1", "app", b"encrypted-v1", "2026-07-30T00:00:00Z")
        .unwrap();
    metadata
        .update_database(
            "db-1",
            &DatabaseUpdate {
                name: "analytics",
                encrypted_dsn: Some(b"encrypted-v2"),
                mode: "polling",
                include_tables: Some("[\"events\"]"),
                exclude_tables: Some("[\"secrets\"]"),
                poll_interval_seconds: 7,
                reconcile_interval_seconds: 420,
                now: "2026-07-30T00:02:00Z",
            },
        )
        .unwrap();
    metadata
        .update_database_probe(
            "db-1",
            "{\"database\":\"analytics\"}",
            "polling",
            "2026-07-30T00:03:00Z",
        )
        .unwrap();
    let database = metadata.database("db-1").unwrap().expect("database");
    assert_eq!(database.name, "analytics");
    assert_eq!(database.encrypted_dsn, b"encrypted-v2");
    assert_eq!(database.poll_interval_seconds, 7);
    assert_eq!(database.effective_mode.as_deref(), Some("polling"));
    assert_eq!(metadata.databases().unwrap().len(), 1);

    metadata
        .upsert_snapshot_table("db-1", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .unwrap();
    metadata
        .set_table_soft_delete_column("db-1", "events", Some("deleted_at"))
        .unwrap();
    assert_eq!(
        metadata.tables("db-1").unwrap()[0]
            .soft_delete_column
            .as_deref(),
        Some("deleted_at")
    );

    metadata
        .set_database_mode("db-1", "paused", "2026-07-30T00:04:00Z")
        .unwrap();
    assert_eq!(
        metadata
            .database("db-1")
            .unwrap()
            .unwrap()
            .effective_mode
            .as_deref(),
        Some("paused")
    );
    metadata
        .set_database_mode("db-1", "auto", "2026-07-30T00:05:00Z")
        .unwrap();
    assert_eq!(
        metadata.database("db-1").unwrap().unwrap().effective_mode,
        None
    );
    assert!(metadata.delete_database("db-1").unwrap());
    assert!(metadata.databases().unwrap().is_empty());
}
