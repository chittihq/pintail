//! Ordering comparisons, BETWEEN and IN over a low-cardinality text column
//! answer by collation, as the row path does: under the default
//! case-insensitive collation 'Delivered' equals 'delivered' and sorts
//! between 'cancelled' and 'pending'.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const LABELS: [&str; 5] = ["pending", "processing", "shipped", "Delivered", "cancelled"];
const ROWS: u64 = 1_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn count(store: &TableStore, sql: &str) -> u64 {
    let entry = TableEntry::new(
        TableId::new(1),
        "orders",
        schema(),
        TableStatistics::with_row_count(ROWS),
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
    u64::try_from(rows).expect("row count")
}

#[test]
fn text_ranges_and_membership_answer_by_collation() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot(
            (1..=ROWS)
                .map(|id| {
                    // Every tenth row is NULL; the rest cycle the labels.
                    let status = if id % 10 == 0 {
                        Value::Null
                    } else {
                        Value::Utf8(LABELS[usize::try_from(id % 5).expect("small")].to_owned())
                    };
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), status],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    // Each label covers 200 rows, and the NULLs take 100 of the rows whose
    // id is a multiple of five: label index 0, 'pending'.
    let per_label = |label: &str| if label == "pending" { 100 } else { 200 };
    let rows = |labels: &[&str]| labels.iter().map(|label| per_label(label)).sum::<u64>();
    for (sql, expected) in [
        (
            "SELECT id FROM orders WHERE status >= 'delivered'",
            rows(&["Delivered", "pending", "processing", "shipped"]),
        ),
        (
            "SELECT id FROM orders WHERE status > 'delivered'",
            rows(&["pending", "processing", "shipped"]),
        ),
        (
            "SELECT id FROM orders WHERE status < 'PENDING'",
            rows(&["cancelled", "Delivered"]),
        ),
        (
            "SELECT id FROM orders WHERE status <= 'pending'",
            rows(&["cancelled", "Delivered", "pending"]),
        ),
        (
            "SELECT id FROM orders WHERE status BETWEEN 'Delivered' AND 'processing'",
            rows(&["Delivered", "pending", "processing"]),
        ),
        ("SELECT id FROM orders WHERE status BETWEEN 'q' AND 'a'", 0),
        (
            "SELECT id FROM orders WHERE status IN ('SHIPPED', 'cancelled')",
            rows(&["shipped", "cancelled"]),
        ),
        (
            "SELECT id FROM orders WHERE status NOT IN ('shipped')",
            rows(&["pending", "processing", "Delivered", "cancelled"]),
        ),
    ] {
        assert_eq!(count(&store, sql), expected, "{sql}");
    }
}
