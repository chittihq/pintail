//! A DECIMAL range with negative bounds, answered against values computed
//! here rather than by the engine.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BALANCES: [&str; 8] = [
    "-12.50", "-499.99", "-500.00", "-0.01", "0.00", "-600.00", "15.25", "-100.00",
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(
                2,
                "balance",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
        ],
    )
    .expect("schema")
}

fn count(sql: &str) -> usize {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot(
            (1_u64..)
                .zip(BALANCES)
                .map(|(id, balance)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(balance.to_owned())],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "accounts",
        schema(),
        TableStatistics::with_row_count(BALANCES.len() as u64),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
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
    let mut rows = 0;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.visible_row_count();
    }
    rows
}

#[test]
fn a_decimal_range_with_negative_bounds_selects_by_value() {
    // -500.00, -499.99, -100.00, -12.50 and -0.01 lie in [-500, -0.01].
    for sql in [
        "SELECT id FROM accounts WHERE balance BETWEEN -500 AND -0.01",
        "SELECT id FROM accounts WHERE balance >= -500 AND balance <= -0.01",
        "SELECT id FROM accounts WHERE balance BETWEEN -500.00 AND -0.01",
    ] {
        assert_eq!(count(sql), 5, "{sql}");
    }
    for (sql, rows) in [
        (
            "SELECT id FROM accounts WHERE balance NOT BETWEEN -500 AND -0.01",
            3,
        ),
        (
            "SELECT id FROM accounts WHERE balance BETWEEN -500 AND 0",
            6,
        ),
        ("SELECT id FROM accounts WHERE balance < 0", 6),
        ("SELECT id FROM accounts WHERE balance > '-100.5'", 5),
        ("SELECT id FROM accounts WHERE balance = -12.50", 1),
        (
            "SELECT id FROM accounts WHERE balance + 0 BETWEEN -500 AND -0.01",
            5,
        ),
    ] {
        assert_eq!(count(sql), rows, "{sql}");
    }
}
