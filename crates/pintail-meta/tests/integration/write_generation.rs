//! The write generation: how a process that is the only writer of its
//! metadata store learns the store was written without looking at a file.

use pintail_meta::{MetaStore, journal_commits, write_generation};

const NOW: &str = "2026-10-02T00:00:00Z";

#[test]
fn every_committed_write_moves_the_generation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("open");
    let opened = write_generation();

    store
        .upsert_database("db", "source", b"dsn", NOW)
        .expect("create");
    let created = write_generation();
    assert!(created > opened, "an autocommitted write");
    store
        .upsert_snapshot_table("db", "a", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .expect("table");

    store
        .record_schema_history("db", "a", 1, Some("CREATE TABLE a"), "[]", NOW)
        .expect("history");
    let recorded = write_generation();
    assert!(recorded > created, "a write inside a transaction");

    // A second connection in the process is counted the same way, and what
    // it committed is there to read once the generation has moved.
    let mut other = MetaStore::open(&path).expect("second connection");
    other
        .record_schema_history("db", "a", 2, Some("ALTER TABLE a"), "[]", NOW)
        .expect("history");
    assert!(write_generation() > recorded);
    assert_eq!(store.schema_history("db", "a").expect("read").len(), 2);

    // A connection handed back and taken again still counts.
    drop(other);
    let before = write_generation();
    let mut reused = MetaStore::open(&path).expect("reused connection");
    reused
        .record_schema_history("db", "a", 3, Some("ALTER TABLE a"), "[]", NOW)
        .expect("history");
    assert!(write_generation() > before);
}

/// Counting commits takes over the hook that checkpoints the write-ahead
/// log, so the count has to checkpoint it too: the log must not grow with
/// every commit for as long as the process lives.
#[test]
fn the_write_ahead_log_is_still_checkpointed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("open");
    store
        .upsert_database("db", "source", b"dsn", NOW)
        .expect("create");
    store
        .upsert_snapshot_table("db", "a", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .expect("table");
    // About a hundred pages a commit: left alone, four hundred commits
    // would leave a log of forty thousand frames.
    let columns = format!("[\"{}\"]", "x".repeat(400_000));
    for version in 1..=400 {
        store
            .record_schema_history("db", "a", version, None, &columns, NOW)
            .expect("history");
    }
    let mut log = path.clone().into_os_string();
    log.push("-wal");
    let log_bytes = std::fs::metadata(&log).expect("the log").len();
    assert!(
        log_bytes < 64 * 1024 * 1024,
        "the log holds {log_bytes} bytes: it was never checkpointed"
    );
}

/// A commit that wrote only journal rows - an audit event, a sync run, an
/// API key's last use - is durable and readable like any other, and leaves
/// the generation where it was: nothing a reader decides by has changed.
#[test]
fn a_journal_only_commit_leaves_the_generation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("pintail-meta.db");
    let store = MetaStore::open(&path).expect("open");
    store
        .upsert_database("db", "source", b"dsn", NOW)
        .expect("database");
    store
        .create_workspace("ws", "Workspace", "workspace", NOW)
        .expect("workspace");
    store
        .create_api_key(&pintail_meta::NewApiKey {
            id: "key",
            database_id: "db",
            name: "reader",
            sha256: &[7; 32],
            mysql_native_password_hash: None,
            caching_sha2_password_hash: None,
            scopes_json: "[\"query\"]",
            expires_at: None,
            now: NOW,
        })
        .expect("key");

    // Other tests in this process may commit while this one looks, so one
    // undisturbed round is the evidence; a journal write that moved the
    // generation would disturb every round.
    let mut undisturbed = false;
    let mut rounds = 0;
    for round in 0..64 {
        rounds += 1;
        let before = write_generation();
        let journal_before = journal_commits();
        let run = format!("run-{round}");
        let event = format!("audit-{round}");
        store
            .start_sync_run(&run, "db", None, "cdc", NOW)
            .expect("start");
        store
            .finish_sync_run(&run, "completed", 0, 0, 1, None)
            .expect("finish");
        store.touch_api_key("key", NOW).expect("touch");
        store
            .record_audit_event(&pintail_meta::NewAuditEvent {
                id: &event,
                workspace_id: "ws",
                actor_type: "user",
                actor_id: "user",
                actor_label: "user@example.com",
                action: "query.run",
                target_type: None,
                target_id: None,
                detail_json: None,
                created_at: NOW,
                client_ip: None,
            })
            .expect("audit");
        assert!(
            journal_commits() >= journal_before + 4,
            "each of the four writes is counted as a journal commit"
        );
        if write_generation() == before {
            undisturbed = true;
            break;
        }
    }
    assert!(undisturbed, "journal writes moved the generation");

    // The rows are there, one of each per round, read through another
    // connection.
    let reader = MetaStore::open(&path).expect("second connection");
    let runs = reader.sync_runs(Some("db"), 100).expect("runs");
    assert_eq!(runs.len(), rounds);
    assert!(runs.iter().all(|run| run.status == "completed"));
    assert_eq!(
        reader
            .audit_log_in_workspace("ws", 100)
            .expect("audit")
            .len(),
        rounds
    );

    // And the next write that is not a journal row moves it again.
    let before = write_generation();
    store.set_setting("k", "v").expect("setting");
    assert!(write_generation() > before);
}
