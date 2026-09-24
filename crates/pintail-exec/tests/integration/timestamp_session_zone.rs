//! A source `TIMESTAMP` column reads in the session's time zone, as `MySQL`
//! converts it on retrieval: what a query displays, filters on, computes and
//! aggregates are the session's values. Stored values are UTC; a `DATETIME`
//! column is never converted.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "ts", DataType::DateTime64 { fsp: 6 }, true).with_timestamp(true),
            Column::new(3, "dt", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

/// Rows and whether each output column still reports as a `TIMESTAMP`.
fn run(sql: &str, zone: Option<&str>) -> (Vec<Vec<String>>, Vec<bool>) {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    let text =
        |value: Option<&str>| value.map_or(Value::Null, |value| Value::Utf8(value.to_owned()));
    store
        .bulk_ingest_snapshot(
            [
                (
                    1_u64,
                    Some("2024-03-10 06:59:59.500000"),
                    Some("2024-03-10 06:59:59"),
                ),
                (
                    2,
                    Some("2024-03-10 07:00:00.000000"),
                    Some("2024-03-10 07:00:00"),
                ),
                (3, None, None),
            ]
            .into_iter()
            .map(|(id, ts, dt)| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![Value::UInt64(id), text(ts), text(dt)],
                    id,
                    false,
                )
            })
            .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema(),
        TableStatistics::with_row_count(3),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    assert!(pintail_exec::set_session_time_zone(zone), "zone {zone:?}");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let timestamps = execution
        .output_fields()
        .iter()
        .map(|field| field.timestamp)
        .collect();
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).expect("value") {
                        Value::UInt64(n) => n.to_string(),
                        Value::Int64(n) => n.to_string(),
                        Value::Utf8(text) => text.clone(),
                        Value::Null => "NULL".to_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    assert!(pintail_exec::set_session_time_zone(None));
    (rows, timestamps)
}

#[test]
fn a_timestamp_column_reads_in_the_session_time_zone() {
    let new_york = Some("America/New_York");
    let (rows, timestamps) = run("SELECT id, ts, dt FROM t ORDER BY id", new_york);
    assert_eq!(
        rows,
        [
            ["1", "2024-03-10 01:59:59.500000", "2024-03-10 06:59:59"],
            ["2", "2024-03-10 03:00:00.000000", "2024-03-10 07:00:00"],
            ["3", "NULL", "NULL"],
        ]
    );
    assert_eq!(timestamps, [false, true, false]);
    assert_eq!(
        run(
            "SELECT id, HOUR(ts) FROM t WHERE id < 3 ORDER BY id",
            new_york
        )
        .0,
        [["1", "1"], ["2", "3"]]
    );
    assert_eq!(
        run(
            "SELECT id FROM t WHERE ts < '2024-03-10 03:00:00'",
            new_york
        )
        .0,
        [["1"]]
    );
    assert_eq!(
        run("SELECT MAX(ts), MIN(ts) FROM t", new_york).0,
        [["2024-03-10 03:00:00.000000", "2024-03-10 01:59:59.500000"]]
    );
    assert_eq!(
        run("SELECT ts FROM t WHERE id = 1", Some("+05:30")).0,
        [["2024-03-10 12:29:59.500000"]]
    );
    for zone in [Some("+00:00"), None] {
        let (rows, timestamps) = run("SELECT ts FROM t WHERE id = 1", zone);
        assert_eq!(rows, [["2024-03-10 06:59:59.500000"]], "{zone:?}");
        assert_eq!(timestamps, [true]);
    }
}
