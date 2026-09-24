//! An uncorrelated subquery is answered once per query, however many rows it
//! returns: `id IN (SELECT id FROM users …)` over tens of thousands of users
//! must cost about what reading them costs, not a multiple per member.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema(id: u32) -> TableSchema {
    TableSchema::new(
        id,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn table(directory: &std::path::Path, id: u32, rows: i64) -> TableStore {
    let mut table =
        TableStore::open(directory, schema(id), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(
            (1..=rows)
                .map(|row| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Int64(row)]).expect("key"),
                        vec![Value::Int64(row), Value::Utf8(format!("name-{row}"))],
                        u64::try_from(row).expect("positive"),
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk snapshot");
    table
}

/// Runs `sql` over `users` (`members` rows) and `events` (10 rows), returning
/// the rendered rows and the elapsed time.
fn run(sql: &str, members: i64) -> (Vec<Vec<String>>, std::time::Duration) {
    run_with(sql, members, 10)
}

fn run_with(sql: &str, members: i64, outer: i64) -> (Vec<Vec<String>>, std::time::Duration) {
    let users_dir = tempfile::tempdir().expect("users");
    let events_dir = tempfile::tempdir().expect("events");
    let users = table(users_dir.path(), 1, members);
    let events = table(events_dir.path(), 2, outer);
    let (users_snapshot, events_snapshot) = (users.snapshot(), events.snapshot());
    let database = DatabaseId::new(3);
    let (users_id, events_id) = (TableId::new(5), TableId::new(6));
    let entries = [
        TableEntry::new(
            users_id,
            "users",
            schema(1),
            TableStatistics::with_row_count(members.cast_unsigned()),
        )
        .expect("users entry"),
        TableEntry::new(
            events_id,
            "events",
            schema(2),
            TableStatistics::with_row_count(outer.cast_unsigned()),
        )
        .expect("events entry"),
    ];
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(database, "app", entries).expect("database")])
            .expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database, users_id, &users_snapshot),
        (database, events_id, &events_snapshot),
    ])
    .expect("provider");
    let started = Instant::now();
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("pull batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).expect("value") {
                        Value::Int64(n) => n.to_string(),
                        Value::UInt64(n) => n.to_string(),
                        Value::Boolean(b) => i32::from(*b).to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    (rows, started.elapsed())
}

#[test]
fn an_in_subquery_costs_about_what_reading_its_rows_costs() {
    let sql = "SELECT id, id IN (SELECT id FROM users WHERE id >= 1) FROM events WHERE id = 1";
    let mut timings = Vec::new();
    for members in [10_000, 20_000, 40_000, 80_000] {
        let (rows, elapsed) = run(sql, members);
        assert_eq!(rows, [["1", "1"]], "{members} members");
        timings.push((members, elapsed));
        eprintln!("IN subquery over {members} members: {elapsed:?}");
    }
    let scalar = "SELECT id, (SELECT MAX(id) FROM users) FROM events WHERE id = 1";
    let (rows, elapsed) = run(scalar, 80_000);
    assert_eq!(rows, [["1", "80000"]]);
    eprintln!("scalar subquery over 80000 members: {elapsed:?}");
    // Many outer rows against a large member list: each probe must not pay
    // for every member.
    for sql in [
        "SELECT COUNT(*) FROM events WHERE id IN (SELECT id FROM users)",
        "SELECT SUM(id IN (SELECT id FROM users WHERE id % 2 = 0)) FROM events",
    ] {
        let (rows, elapsed) = run_with(sql, 80_000, 20_000);
        eprintln!("{sql} with 20000 outer rows over 80000 members: {elapsed:?} -> {rows:?}");
        assert!(elapsed.as_secs_f64() < 2.0, "{sql} took {elapsed:?}");
    }
    // Doubling the members should not much more than double the cost.
    let (small, large) = (timings[2].1.as_secs_f64(), timings[3].1.as_secs_f64());
    assert!(
        large < small * 3.0 + 0.05,
        "IN over 80k members took {large:.3}s against {small:.3}s for 40k: worse than linear"
    );
}
