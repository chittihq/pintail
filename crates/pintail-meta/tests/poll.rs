use pintail_meta::{MetaStore, PollChunkStateUpdate, PollStateUpdate};
use rusqlite::Connection;

#[test]
fn polling_state_advances_atomically_and_preserves_reconcile_time() {
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
        .commit_poll_state(
            "source",
            "events",
            &PollStateUpdate {
                cursor_column: Some("updated_at"),
                cursor_json: Some(r#"{"date":[2026,7,30,1,0,0,0]}"#),
                source_token_json: Some(r#"{"count":2,"max":"2026-07-30 01:00:00"}"#),
                source_count: 2,
                version: 1,
                reconciled: true,
            },
            "2026-07-30T01:00:00Z",
        )
        .expect("first poll checkpoint");
    store
        .commit_poll_state(
            "source",
            "events",
            &PollStateUpdate {
                cursor_column: Some("updated_at"),
                cursor_json: Some(r#"{"date":[2026,7,30,2,0,0,0]}"#),
                source_token_json: Some(r#"{"count":3,"max":"2026-07-30 02:00:00"}"#),
                source_count: 3,
                version: 2,
                reconciled: false,
            },
            "2026-07-30T02:00:00Z",
        )
        .expect("second poll checkpoint");

    let state = store
        .poll_state("source", "events")
        .expect("poll state")
        .expect("poll state row");
    assert_eq!(state.cursor_column.as_deref(), Some("updated_at"));
    assert_eq!(
        state.cursor_json.as_deref(),
        Some(r#"{"date":[2026,7,30,2,0,0,0]}"#)
    );
    assert_eq!(state.source_count, 3);
    assert_eq!(state.version, 2);
    assert_eq!(
        state.last_reconcile_at.as_deref(),
        Some("2026-07-30T01:00:00Z")
    );
    assert_eq!(
        store
            .snapshot_checkpoint("source")
            .expect("database checkpoint")
            .expect("checkpoint")
            .kind,
        "polling"
    );

    let connection = Connection::open(&path).expect("inspect metadata");
    let database: (String, String) = connection
        .query_row(
            "SELECT state, effective_mode FROM databases WHERE id = 'source'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("database state");
    assert_eq!(database, ("polling".to_owned(), "polling".to_owned()));
    let table: (String, String) = connection
        .query_row(
            "SELECT state, last_reconcile_at FROM tables \
             WHERE db_id = 'source' AND name = 'events'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("table state");
    assert_eq!(
        table,
        ("polling".to_owned(), "2026-07-30T01:00:00Z".to_owned())
    );
}

#[test]
fn polling_chunk_fingerprints_replace_atomically_with_the_checkpoint() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    store
        .upsert_database("source", "app", b"mysql://source", "2026-07-30T00:00:00Z")
        .expect("register database");
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
    let poll = PollStateUpdate {
        cursor_column: None,
        cursor_json: None,
        source_token_json: Some(r#"{"count":4,"max":4}"#),
        source_count: 4,
        version: 1,
        reconciled: true,
    };
    let first = [
        PollChunkStateUpdate {
            chunk_id: "0",
            source_count: 2,
            source_checksum: "source-a",
            replica_checksum: "replica-a",
        },
        PollChunkStateUpdate {
            chunk_id: "1",
            source_count: 2,
            source_checksum: "source-b",
            replica_checksum: "replica-b",
        },
    ];
    store
        .commit_poll_state_with_chunks("source", "events", &poll, &first, "2026-07-30T01:00:00Z")
        .expect("first chunk checkpoint");
    assert_eq!(
        store.poll_chunk_states("source", "events").unwrap().len(),
        2
    );

    let replacement = [PollChunkStateUpdate {
        chunk_id: "0",
        source_count: 1,
        source_checksum: "source-new",
        replica_checksum: "replica-new",
    }];
    store
        .commit_poll_state_with_chunks(
            "source",
            "events",
            &PollStateUpdate { version: 2, ..poll },
            &replacement,
            "2026-07-30T02:00:00Z",
        )
        .expect("replacement chunk checkpoint");
    let chunks = store
        .poll_chunk_states("source", "events")
        .expect("read chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk_id, "0");
    assert_eq!(chunks[0].source_count, 1);
    assert_eq!(chunks[0].source_checksum, "source-new");
    assert_eq!(chunks[0].replica_checksum, "replica-new");
    assert_eq!(
        store
            .poll_state("source", "events")
            .unwrap()
            .unwrap()
            .version,
        2
    );
}
