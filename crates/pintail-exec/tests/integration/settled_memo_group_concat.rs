//! The settled-aggregate memo keys an answer by its plan signature, and
//! `GROUP_CONCAT`'s answer depends on more than its argument: its separator,
//! its own ORDER BY and the session's `group_concat_max_len`. Keyed without
//! them, a repeat of the same argument under a different ORDER BY or
//! separator was served the first spelling's answer.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: [(u64, i64); 3] = [(1, 3), (2, 1), (3, 2)];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "n", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, n: i64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Int64(n)],
        id,
        false,
    )
}

fn answer(table: &TableStore, catalog: &CatalogSnapshot, sql: &str) -> Value {
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(19), &snapshot)])
        .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let batch = execution
        .next_batch()
        .expect("pull")
        .expect("one aggregate row");
    batch
        .column(0)
        .and_then(|column| column.value(0))
        .cloned()
        .expect("the concatenation")
}

#[test]
fn group_concat_spellings_over_a_settled_table_are_answered_separately() {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, n)| row(*id, *n)).collect())
        .expect("snapshot");
    table.flush().expect("flush");

    let entry = TableEntry::new(
        TableId::new(19),
        "readings",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(DatabaseId::new(15), "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");

    let text = |value: &str| Value::Utf8(value.to_owned());
    for (sql, expected) in [
        ("SELECT GROUP_CONCAT(n) FROM readings", "3,1,2"),
        ("SELECT GROUP_CONCAT(n ORDER BY n DESC) FROM readings", "3,2,1"),
        ("SELECT GROUP_CONCAT(n ORDER BY n) FROM readings", "1,2,3"),
        ("SELECT GROUP_CONCAT(n SEPARATOR ';') FROM readings", "3;1;2"),
    ] {
        assert_eq!(answer(&table, &catalog, sql), text(expected), "{sql}");
    }
}
