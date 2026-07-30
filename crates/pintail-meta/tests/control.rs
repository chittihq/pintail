use pintail_meta::{DatabaseUpdate, MetaStore, NewApiKey};

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
    metadata
        .set_database_replication_state("db-1", "polling", "2026-07-30T00:06:00Z")
        .unwrap();
    assert_eq!(metadata.database("db-1").unwrap().unwrap().state, "polling");
    assert_eq!(metadata.tables("db-1").unwrap()[0].state, "polling");

    assert!(metadata.delete_database("db-1").unwrap());
    assert!(metadata.databases().unwrap().is_empty());
}

#[test]
fn api_keys_activity_and_dead_letters_round_trip() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = MetaStore::open(&database_path).expect("metadata store");
    metadata
        .upsert_database("db-1", "app", b"encrypted", "2026-07-30T00:00:00Z")
        .unwrap();

    metadata
        .create_api_key(&NewApiKey {
            id: "key-1",
            database_id: "db-1",
            name: "Metabase",
            sha256: &[7; 32],
            scopes_json: "[\"query\",\"read\"]",
            expires_at: None,
            now: "2026-07-30T00:06:00Z",
        })
        .unwrap();
    let key = metadata
        .api_key_by_sha256(&[7; 32])
        .unwrap()
        .expect("API key");
    assert!(key.enabled);
    assert_eq!(metadata.api_keys("db-1").unwrap().len(), 1);
    metadata
        .touch_api_key("key-1", "2026-07-30T00:07:00Z")
        .unwrap();
    metadata.set_api_key_enabled("key-1", false).unwrap();
    assert!(!metadata.api_keys("db-1").unwrap()[0].enabled);

    metadata
        .start_sync_run(
            "run-1",
            "db-1",
            Some("events"),
            "snapshot",
            "2026-07-30T00:08:00Z",
        )
        .unwrap();
    metadata
        .finish_sync_run("run-1", "completed", 10, 2048, 50, None)
        .unwrap();
    let run = &metadata.sync_runs(Some("db-1"), 10).unwrap()[0];
    assert_eq!((run.rows, run.bytes, run.duration_ms), (10, 2048, Some(50)));

    metadata
        .record_dlq(
            "dlq-1",
            "db-1",
            Some("events"),
            "{\"event\":1}",
            "decode failed",
            "2026-07-30T00:09:00Z",
        )
        .unwrap();
    let dlq = metadata.dlq_records(Some("db-1"), 10).unwrap();
    assert_eq!(dlq.len(), 1);
    assert!(metadata.delete_dlq_record(&dlq[0].id).unwrap());
    assert!(metadata.dlq_records(Some("db-1"), 10).unwrap().is_empty());
    assert!(metadata.delete_api_key("key-1").unwrap());
}
