//! The replica signature covers the rows a replica load reads and nothing
//! else: audit, settings and workspace writes leave it alone, while schema
//! history, table state and database mode move it.
use pintail_meta::{MetaStore, NewAuditEvent};

const NOW: &str = "2026-09-06T00:00:00Z";

#[test]
fn bookkeeping_writes_leave_the_signature_and_semantic_writes_move_it() {
    let workspace = tempfile::tempdir().expect("metadata workspace");
    let path = workspace.path().join("pintail-meta.db");
    let mut store = MetaStore::open(&path).expect("metadata");
    store
        .upsert_database("source", "app", b"mysql://source", NOW)
        .expect("register database");
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .expect("register table");
    let warm = store.replica_signature("source").expect("signature");
    assert_eq!(
        warm,
        store.replica_signature("source").expect("signature"),
        "the signature is a pure function of the rows"
    );

    store.set_setting("probe.cadence", "5").expect("setting");
    store
        .create_workspace("ws", "Workspace", "ws", NOW)
        .expect("workspace");
    store
        .record_audit_event(&NewAuditEvent {
            id: "evt-1",
            workspace_id: "ws",
            actor_type: "user",
            actor_id: "usr_1",
            actor_label: "operator",
            action: "query.execute",
            target_type: Some("database"),
            target_id: Some("source"),
            detail_json: None,
            created_at: NOW,
            client_ip: None,
        })
        .expect("audit");
    store
        .upsert_database("other", "other", b"mysql://other", NOW)
        .expect("another database");
    assert_eq!(
        warm,
        store.replica_signature("source").expect("signature"),
        "audit, settings, workspaces and other databases are outside the signature"
    );

    store
        .record_schema_history(
            "source",
            "events",
            2,
            Some("ALTER TABLE events ADD COLUMN note TEXT"),
            r#"[{"id":1,"name":"id"},{"id":2,"name":"note"}]"#,
            "2026-09-06T01:00:00Z",
        )
        .expect("schema history");
    let evolved = store.replica_signature("source").expect("signature");
    assert_ne!(warm, evolved, "a schema generation moves the signature");

    store
        .mark_table_needs_resync("source", "events", "drift")
        .expect("flag table");
    let flagged = store.replica_signature("source").expect("signature");
    assert_ne!(evolved, flagged, "a table state change moves the signature");

    store
        .set_database_mode("source", "paused", NOW)
        .expect("mode");
    let paused = store.replica_signature("source").expect("signature");
    assert_ne!(
        flagged, paused,
        "a database mode change moves the signature"
    );
}
