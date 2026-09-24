//! The settled-aggregate memo is process-wide, so its key has to carry
//! everything outside the plan text that changes the answer.
//! `div_precision_increment` is one of those: it decides how many fraction
//! digits AVG adds, so the same query over the same settled table is a
//! different answer in a session that raised it. Keyed without it, whichever
//! session ran first decided the scale for every other one.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: [(u64, i64); 4] = [(1, 1), (2, 2), (3, 3), (4, 4)];

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

fn average(table: &TableStore, catalog: &CatalogSnapshot) -> String {
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
        .expect("provider");
    let statement = parse_statement("SELECT AVG(n) FROM readings").expect("parse");
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
    let value = batch
        .column(0)
        .and_then(|column| column.value(0))
        .cloned()
        .expect("the average");
    match value {
        Value::DecimalAverage(quotient) => quotient.label.clone(),
        other => panic!("AVG over an exact-numeric column is a decimal, got {other:?}"),
    }
}

#[test]
fn a_session_that_widens_division_does_not_read_another_session_s_average() {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, n)| row(*id, *n)).collect())
        .expect("snapshot");
    table.flush().expect("flush");

    let entry = TableEntry::new(
        TableId::new(17),
        "readings",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(DatabaseId::new(15), "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");

    // The first session settles the memo at the default four digits.
    pintail_sql::set_session_div_precision_increment(Some(4));
    assert_eq!(average(&table, &catalog), "2.5000");
    assert_eq!(
        average(&table, &catalog),
        "2.5000",
        "the memo replays its own session's answer"
    );

    // A second session asks for six. The answer is the same number at a
    // different scale, and it must not come back at the first one's.
    pintail_sql::set_session_div_precision_increment(Some(6));
    assert_eq!(average(&table, &catalog), "2.500000");

    pintail_sql::set_session_div_precision_increment(None);
}
