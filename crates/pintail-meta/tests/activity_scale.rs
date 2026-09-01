//! How the activity queries behave as `sync_runs` and `dlq` grow.
//!
//! A replication cycle writes one `sync_runs` row every supervisor cadence -
//! 17,280 rows a day at the 5-second default - a dead letter arrives per
//! undecodable event, and nothing prunes either. The dashboard reads both
//! newest-first, so without an index on the sort column `SQLite` scans and
//! sorts the whole table to return ten rows, and every dashboard load gets
//! slower for the life of the deployment.
//!
//! The regression test asserts the QUERY PLAN rather than a duration: a
//! wall-clock threshold is flaky on a shared machine and says nothing about
//! why it regressed, while "this read must not scan the table" is exactly
//! the property that broke and stays true at any row count. The timing
//! harness below is kept, ignored, for measuring the difference.

use pintail_meta::MetaStore;

/// The reads the dashboard issues, and the table each must not scan.
const ACTIVITY_READS: [(&str, &str, usize); 6] = [
    (
        "SELECT id FROM sync_runs WHERE (?1 IS NULL OR db_id = ?1) \
         ORDER BY started_at DESC, id LIMIT ?2",
        "sync_runs",
        2,
    ),
    (
        "SELECT id FROM sync_runs WHERE db_id = ?2 \
           AND db_id IN (SELECT id FROM databases WHERE workspace_id = ?1) \
         ORDER BY started_at DESC, id LIMIT ?3",
        "sync_runs",
        3,
    ),
    (
        "SELECT id FROM sync_runs INDEXED BY idx_sync_runs_started \
         WHERE db_id IN (SELECT id FROM databases WHERE workspace_id = ?1) \
           AND (?2 IS NULL OR db_id = ?2) \
         ORDER BY started_at DESC, id LIMIT ?3",
        "sync_runs",
        3,
    ),
    (
        "SELECT id FROM dlq WHERE (?1 IS NULL OR db_id = ?1) \
         ORDER BY created_at DESC, id LIMIT ?2",
        "dlq",
        2,
    ),
    (
        "SELECT id FROM dlq WHERE db_id = ?2 \
           AND db_id IN (SELECT id FROM databases WHERE workspace_id = ?1) \
         ORDER BY created_at DESC, id LIMIT ?3",
        "dlq",
        3,
    ),
    (
        "SELECT id FROM dlq INDEXED BY idx_dlq_created \
         WHERE db_id IN (SELECT id FROM databases WHERE workspace_id = ?1) \
           AND (?2 IS NULL OR db_id = ?2) \
         ORDER BY created_at DESC, id LIMIT ?3",
        "dlq",
        3,
    ),
];

#[test]
fn the_activity_reads_never_scan_a_table_that_grows_forever() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let path = directory.path().join("pintail-meta.db");
    drop(MetaStore::open(&path).expect("metadata"));

    let connection = rusqlite::Connection::open(&path).expect("inspect schema");
    for (sql, table, parameters) in ACTIVITY_READS {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|error| panic!("prepare plan for {sql}: {error}"));
        // Bound to concrete values, so the plan is the one the dashboard's
        // scoped read actually gets.
        let bindings = (0..parameters)
            .map(|_| String::from("db-1"))
            .collect::<Vec<_>>();
        let plan = statement
            .query_map(rusqlite::params_from_iter(bindings), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("plan")
            .join(" | ");

        // The read must reach these rows through an index. SQLite still
        // calls an ordered index walk a SCAN, and that is fine: the `?1 IS
        // NULL OR` shape cannot seek, but walking the index in sort order
        // stops at the LIMIT instead of reading the whole table.
        assert!(
            plan.contains("USING INDEX") || plan.contains("USING COVERING INDEX"),
            "this read reaches {table} without an index, and {table} grows \
             forever:\n  {sql}\n  plan: {plan}"
        );
        // Sorting the WHOLE result is the other half of the cost. A bounded
        // "LAST TERM OF ORDER BY" sort is the tiebreak within one timestamp
        // and does not grow with the table.
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "this read sorts all of {table} rather than walking an index in \
             order:\n  {sql}\n  plan: {plan}"
        );
    }
}

/// Seeds roughly seventeen days of uptime and times the reads. Ignored by
/// default: the seeding dominates, and the assertion above is the one that
/// catches a regression.
#[test]
#[ignore = "measurement: cargo test -p pintail-meta --test activity_scale -- --ignored --nocapture"]
fn activity_read_timings_over_a_large_history() {
    use std::time::Instant;

    const ROWS: usize = 300_000;
    let directory = tempfile::tempdir().expect("temporary data directory");
    let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).expect("metadata");
    metadata
        .upsert_database("db-1", "shop", b"secret", "2026-09-01T00:00:00Z")
        .expect("database");
    metadata
        .upsert_database("db-2", "other", b"secret", "2026-09-01T00:00:00Z")
        .expect("second database");
    metadata
        .create_workspace("ws-1", "Team", "team", "2026-09-01T00:00:00Z")
        .expect("workspace");
    for database in ["db-1", "db-2"] {
        metadata
            .set_database_workspace(database, "ws-1")
            .expect("assign workspace");
    }
    for index in 0..ROWS {
        let database = if index % 2 == 0 { "db-1" } else { "db-2" };
        let started = format!(
            "2026-09-01T{:02}:{:02}:{:02}Z",
            index / 3600 % 24,
            index / 60 % 60,
            index % 60
        );
        metadata
            .start_sync_run(&format!("run_{index:08}"), database, None, "cdc", &started)
            .expect("seed sync run");
    }

    let started = Instant::now();
    assert_eq!(
        metadata.sync_runs(Some("db-1"), 10).expect("runs").len(),
        10
    );
    let one_database = started.elapsed();
    let started = Instant::now();
    assert_eq!(metadata.sync_runs(None, 10).expect("runs").len(), 10);
    let every_database = started.elapsed();

    // What the dashboard actually calls: the workspace-scoped feed.
    let started = Instant::now();
    assert_eq!(
        metadata
            .sync_runs_in_workspace("ws-1", Some("db-1"), 10)
            .expect("runs")
            .len(),
        10
    );
    let workspace_scoped = started.elapsed();
    let started = Instant::now();
    assert_eq!(
        metadata
            .sync_runs_in_workspace("ws-1", None, 10)
            .expect("runs")
            .len(),
        10
    );
    let workspace_all = started.elapsed();

    println!("rows={ROWS}");
    println!("  sync_runs(db-1, 10):                  {one_database:?}");
    println!("  sync_runs(all,  10):                  {every_database:?}");
    println!("  sync_runs_in_workspace(ws, db-1, 10): {workspace_scoped:?}");
    println!("  sync_runs_in_workspace(ws, all,  10): {workspace_all:?}");
}
