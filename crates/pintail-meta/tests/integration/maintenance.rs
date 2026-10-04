//! The control-plane file's upkeep: checking it, copying it, and bounding
//! the history that grows with every replication cycle.

use pintail_meta::MetaStore;

fn store_with_database(dir: &std::path::Path) -> MetaStore {
    let store = MetaStore::open(&dir.join("pintail-meta.db")).expect("metadata opens");
    store
        .upsert_database("db_1", "source", b"dsn", "2026-01-01T00:00:00+00:00")
        .expect("database row");
    store
}

fn run(store: &MetaStore, id: &str, kind: &str, status: &str, started_at: &str) {
    store
        .start_sync_run(id, "db_1", None, kind, started_at)
        .expect("run starts");
    if status != "running" {
        store
            .finish_sync_run(id, status, 0, 0, 1, None)
            .expect("run finishes");
    }
}

#[test]
fn a_healthy_file_reports_no_problems() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_database(dir.path());
    assert!(
        store
            .integrity_problems(false)
            .expect("quick check")
            .is_empty()
    );
    assert!(
        store
            .integrity_problems(true)
            .expect("full check")
            .is_empty()
    );
}

#[test]
fn a_damaged_page_is_reported_rather_than_found_by_a_read() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("pintail-meta.db");
    {
        let store = store_with_database(dir.path());
        for index in 0..2_000 {
            run(
                &store,
                &format!("run_{index:05}"),
                "cdc",
                "completed",
                "2026-01-01T00:00:00+00:00",
            );
        }
    }
    // Everything into the main file, so the damage below is not shadowed
    // by a newer page image in the WAL.
    let checkpoint = rusqlite::Connection::open(&path).expect("raw open");
    checkpoint
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("checkpoint");
    drop(checkpoint);
    // Overwrite a stretch of pages in the middle of the file: what a torn
    // write or a second writer without the lock leaves behind.
    let mut bytes = std::fs::read(&path).expect("read file");
    let middle = bytes.len() / 2;
    for byte in &mut bytes[middle..middle + 8_192] {
        *byte = 0x5a;
    }
    std::fs::write(&path, bytes).expect("damage file");

    // A file too damaged to check is reported by the failure itself; what
    // must never happen is a clean bill of health.
    let store = MetaStore::open(&path).expect("header still readable");
    if let Ok(problems) = store.integrity_problems(false) {
        assert!(!problems.is_empty(), "the damage must be reported");
    }
}

#[test]
fn a_backup_is_a_complete_readable_copy_and_replaces_the_previous_one() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_database(dir.path());
    run(
        &store,
        "run_a",
        "snapshot",
        "completed",
        "2026-01-02T00:00:00+00:00",
    );
    let target = dir.path().join("backup.db");
    store.backup_into(&target).expect("first copy");
    run(
        &store,
        "run_b",
        "snapshot",
        "completed",
        "2026-01-03T00:00:00+00:00",
    );
    // A copy a crash cut short must not block the next one.
    std::fs::write(dir.path().join("backup.db.partial"), b"torn").expect("stale partial");
    store
        .backup_into(&target)
        .expect("second copy replaces the first");
    assert!(!dir.path().join("backup.db.partial").exists());

    let copy = MetaStore::open(&target).expect("copy opens");
    assert!(copy.integrity_problems(true).expect("check").is_empty());
    let runs = copy.sync_runs(Some("db_1"), 10).expect("runs");
    assert_eq!(runs.len(), 2, "the copy holds everything written before it");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the copy holds encrypted DSNs");
    }
}

#[test]
fn pruning_keeps_copies_failures_and_live_runs_longer_than_routine_cycles() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_database(dir.path());
    let old = "2026-01-01T00:00:00+00:00";
    let recent = "2026-01-20T00:00:00+00:00";
    run(&store, "old_cycle", "cdc", "completed", old);
    run(&store, "old_poll", "polling", "completed", old);
    run(&store, "old_failure", "cdc", "error", old);
    run(&store, "old_copy", "resnapshot", "completed", old);
    run(&store, "old_running", "snapshot", "running", old);
    run(&store, "new_cycle", "cdc", "completed", recent);

    let removed = store
        .prune_sync_runs("2026-01-10T00:00:00+00:00", "2025-12-01T00:00:00+00:00")
        .expect("prune");
    assert_eq!(removed, 2);
    let mut kept = store
        .sync_runs(Some("db_1"), 100)
        .expect("runs")
        .into_iter()
        .map(|run| run.id)
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(
        kept,
        ["new_cycle", "old_copy", "old_failure", "old_running"]
    );

    let removed = store
        .prune_sync_runs("2026-01-10T00:00:00+00:00", "2026-01-10T00:00:00+00:00")
        .expect("prune");
    assert_eq!(removed, 2, "past the history bound only the live run stays");
    let mut kept = store
        .sync_runs(Some("db_1"), 100)
        .expect("runs")
        .into_iter()
        .map(|run| run.id)
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(kept, ["new_cycle", "old_running"]);
}

fn store_with_workspace(dir: &std::path::Path) -> MetaStore {
    let store = MetaStore::open(&dir.join("pintail-meta.db")).expect("metadata opens");
    store
        .create_workspace("ws", "Workspace", "workspace", "2026-01-01T00:00:00+00:00")
        .expect("workspace");
    store
}

fn audit(store: &MetaStore, id: &str, created_at: &str) {
    store
        .record_audit_event(&pintail_meta::NewAuditEvent {
            id,
            workspace_id: "ws",
            actor_type: "user",
            actor_id: "user",
            actor_label: "user@example.com",
            action: "query.run",
            target_type: None,
            target_id: None,
            detail_json: None,
            created_at,
            client_ip: None,
        })
        .expect("audit event");
}

fn audit_ids(store: &MetaStore) -> Vec<String> {
    let mut ids = store
        .audit_log_in_workspace("ws", 100_000)
        .expect("audit log")
        .into_iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Rows written before the bound go, rows from the bound's own second and
/// later stay, whichever UTC spelling they were written with.
#[test]
fn audit_pruning_removes_only_rows_before_the_bound() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_workspace(dir.path());
    audit(&store, "old_offset", "2026-01-01T00:00:00.5+00:00");
    audit(&store, "old_zulu", "2026-01-09T23:59:59Z");
    audit(&store, "edge", "2026-01-10T00:00:00.000001+00:00");
    audit(&store, "edge_zulu", "2026-01-10T00:00:00Z");
    audit(&store, "new", "2026-02-01T00:00:00+00:00");

    let outcome = store
        .prune_audit_log("2026-01-10T00:00:00", 10_000, |_| {
            panic!("one short batch has nothing to give way between")
        })
        .expect("prune");
    assert_eq!(
        outcome,
        pintail_meta::AuditPrune {
            removed: 2,
            batches: 1
        }
    );
    assert_eq!(audit_ids(&store), ["edge", "edge_zulu", "new"]);
}

/// However many rows are due, no transaction deletes more than a batch,
/// and the oldest go first.
#[test]
fn audit_pruning_runs_in_bounded_batches() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_workspace(dir.path());
    for serial in 0..25 {
        audit(
            &store,
            &format!("old_{serial:02}"),
            &format!("2026-01-01T00:00:{serial:02}+00:00"),
        );
    }
    audit(&store, "new", "2026-03-01T00:00:00+00:00");

    let mut batches = Vec::new();
    let outcome = store
        .prune_audit_log("2026-02-01T00:00:00", 10, |removed| {
            let oldest_left = audit_ids(&store)
                .into_iter()
                .find(|id| id.starts_with("old_"));
            batches.push((removed, oldest_left));
        })
        .expect("prune");
    assert_eq!(
        outcome,
        pintail_meta::AuditPrune {
            removed: 25,
            batches: 3
        }
    );
    assert_eq!(
        batches,
        [
            (10, Some("old_10".to_owned())),
            (10, Some("old_20".to_owned()))
        ]
    );
    assert_eq!(audit_ids(&store), ["new"]);

    // A batch that deletes exactly its size is followed by one that finds
    // nothing, not by a pass that stops early.
    for serial in 0..10 {
        audit(
            &store,
            &format!("again_{serial}"),
            "2026-01-01T00:00:00+00:00",
        );
    }
    let outcome = store
        .prune_audit_log("2026-02-01T00:00:00", 10, |_| {})
        .expect("prune");
    assert_eq!(
        outcome,
        pintail_meta::AuditPrune {
            removed: 10,
            batches: 2
        }
    );
}

/// No transaction is held between batches: another connection writing an
/// audit event in that gap commits at once, under a zero busy timeout it
/// would fail, and the pass neither loses its row nor stops.
#[test]
fn an_audit_write_between_pruning_batches_commits() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = store_with_workspace(dir.path());
    for serial in 0..30 {
        audit(
            &store,
            &format!("old_{serial:02}"),
            "2026-01-01T00:00:00+00:00",
        );
    }
    let writer = MetaStore::open(&dir.path().join("pintail-meta.db")).expect("second connection");
    let mut written = 0;
    let outcome = store
        .prune_audit_log("2026-02-01T00:00:00", 10, |_| {
            let holder =
                rusqlite::Connection::open(dir.path().join("pintail-meta.db")).expect("lock probe");
            holder
                .busy_timeout(std::time::Duration::ZERO)
                .expect("no waiting");
            holder
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
                .expect("the write lock is free between batches");
            audit(
                &writer,
                &format!("live_{written}"),
                "2026-03-01T00:00:00+00:00",
            );
            written += 1;
        })
        .expect("prune");
    assert_eq!(outcome.removed, 30);
    assert_eq!(written, 3);
    assert_eq!(audit_ids(&store), ["live_0", "live_1", "live_2"]);
}
