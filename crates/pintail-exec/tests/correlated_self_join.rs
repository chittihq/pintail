//! A correlated `[NOT] EXISTS` over the same table under another alias
//! decorrelates into a semi or anti join like any other: `b.id < a.id` and
//! `b.note <=> a.note AND b.id <> a.id` over tens of thousands of rows
//! answer in a join rather than one subquery per outer row.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: i64 = 20_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "note", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

/// Five classes of note, NULL among them, and one row whose note no other
/// row shares.
fn note(id: i64) -> Value {
    match (id, id % 5) {
        (7, _) => Value::Utf8("solo".to_owned()),
        (_, 0) => Value::Null,
        (_, 1) => Value::Utf8("alpha".to_owned()),
        (_, 2) => Value::Utf8("beta".to_owned()),
        (_, 3) => Value::Utf8("gamma".to_owned()),
        _ => Value::Utf8("delta".to_owned()),
    }
}

#[test]
fn a_correlated_exists_over_the_same_table_is_a_join() {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (1..=ROWS)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
                        vec![Value::Int64(id), note(id)],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("rows");
    let snapshot = table.snapshot();
    let (database, table_id) = (DatabaseId::new(1), TableId::new(1));
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        database,
        "app",
        [TableEntry::new(
            table_id,
            "events",
            schema(),
            TableStatistics::with_row_count(ROWS.cast_unsigned()),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key")],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(database, table_id, &snapshot)]).expect("provider");
    let cases = [
        (
            "EXISTS (SELECT 1 FROM events b WHERE b.id < a.id)",
            ROWS - 1,
        ),
        ("NOT EXISTS (SELECT 1 FROM events b WHERE b.id < a.id)", 1),
        (
            "EXISTS (SELECT 1 FROM events b WHERE b.note <=> a.note AND b.id <> a.id)",
            ROWS - 1,
        ),
        (
            "NOT EXISTS (SELECT 1 FROM events b WHERE b.note <=> a.note AND b.id <> a.id)",
            1,
        ),
        (
            "EXISTS (SELECT 1 FROM events b WHERE b.id = a.id + 1)",
            ROWS - 1,
        ),
    ];
    for (predicate, expected) in cases {
        let sql = format!("SELECT COUNT(*) FROM events a WHERE {predicate}");
        let started = Instant::now();
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(&sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let plan = format!("{physical:?}");
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .expect("start");
        let batch = execution.next_batch().expect("pull").expect("one row");
        let count = batch.columns()[0].value(0).expect("count").clone();
        let elapsed = started.elapsed();
        eprintln!("{elapsed:?}: {sql}");
        assert!(
            !plan.contains("ExistsSubquery") && (plan.contains("Semi") || plan.contains("Anti")),
            "{sql} stayed a dependent subquery"
        );
        assert_eq!(count, Value::UInt64(expected.cast_unsigned()), "{sql}");
        assert!(elapsed.as_secs_f64() < 5.0, "{sql} took {elapsed:?}");
    }
}
