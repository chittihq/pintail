//! `MySQL` orders and compares an ENUM by its declared ordinal, never by the
//! label text. The fixture's labels are deliberately declared in an order
//! where alphabetical and declaration order disagree everywhere, so a path
//! that falls back to text comparison cannot pass by luck.
//!
//! Ten shapes pin the split `MySQL` actually implements (confirmed
//! differentially against 8.4): SORTING follows the declared ordinal -
//! ORDER BY, grouped tie-breaks, DISTINCT, `TopK`, window order - while
//! COMPARISON (ranges, BETWEEN, MIN/MAX) treats the value as its label
//! string. The expectations are `MySQL` 8.4's answers for this data
//! (mirrored differentially by the e2e corpus cases over the same shapes).

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Declaration order: pending(1), processing(2), shipped(3), delivered(4),
/// cancelled(5). Alphabetical order is cancelled, delivered, pending,
/// processing, shipped - no two adjacent labels agree between the orders.
const LABELS: [&str; 5] = ["pending", "processing", "shipped", "delivered", "cancelled"];

/// One row per (id, status): ids 1..=12. Counts by status: pending 2,
/// processing 3, shipped 1, delivered 3, cancelled 3 - the processing/
/// delivered/cancelled tie at 3 is what exposes tie-break order.
const ROWS: [(u64, &str); 12] = [
    (1, "delivered"),
    (2, "pending"),
    (3, "processing"),
    (4, "cancelled"),
    (5, "shipped"),
    (6, "processing"),
    (7, "delivered"),
    (8, "cancelled"),
    (9, "pending"),
    (10, "processing"),
    (11, "delivered"),
    (12, "cancelled"),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, true)
                .with_enum_labels(Some(LABELS.iter().map(ToString::to_string).collect())),
        ],
    )
    .expect("schema")
}

fn row(id: u64, status: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(status.to_owned())],
        id,
        false,
    )
}

fn execute_rows(
    sql: &str,
    catalog: &CatalogSnapshot,
    provider: &SnapshotScanProvider<'_>,
) -> Vec<Vec<String>> {
    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(catalog, Some("app"))
        .bind(&statement)
        .expect("bind query");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        let columns = batch.columns().len();
        for row in batch.selection().selected_rows() {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value");
                values.push(render(&value));
            }
            rows.push(values);
        }
    }
    rows
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
        // debug variant marker
    }
}

fn fixture() -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, status)| row(*id, status)).collect())
        .expect("bulk snapshot");
    (directory, table)
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let (_directory, table) = fixture();
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(15);
    let table_id = TableId::new(17);
    let entry = TableEntry::new(
        table_id,
        "orders",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    execute_rows(sql, &catalog, &provider)
}

fn column(rows: &[Vec<String>], index: usize) -> Vec<String> {
    rows.iter().map(|row| row[index].clone()).collect()
}

#[test]
fn v1_order_by_a_bare_enum_ascends_by_ordinal() {
    let rows = run("SELECT status FROM orders ORDER BY status, id");
    assert_eq!(
        column(&rows, 0),
        [
            "pending",
            "pending",
            "processing",
            "processing",
            "processing",
            "shipped",
            "delivered",
            "delivered",
            "delivered",
            "cancelled",
            "cancelled",
            "cancelled",
        ]
    );
}

#[test]
fn v2_order_by_a_bare_enum_descends_by_ordinal() {
    let rows = run("SELECT status FROM orders ORDER BY status DESC, id");
    assert_eq!(
        column(&rows, 0),
        [
            "cancelled",
            "cancelled",
            "cancelled",
            "delivered",
            "delivered",
            "delivered",
            "shipped",
            "processing",
            "processing",
            "processing",
            "pending",
            "pending",
        ]
    );
}

#[test]
fn v3_a_grouped_tie_breaks_on_the_enum_ordinal() {
    // The customer's shape: counts tie at 3 for processing, delivered and
    // cancelled, so the second key decides the order - by ordinal, that is
    // processing(2), delivered(4), cancelled(5).
    let rows = run("SELECT status, COUNT(*) AS c FROM orders \
         GROUP BY status ORDER BY COALESCE(COUNT(*), 0) DESC, status");
    assert_eq!(
        rows,
        [
            ["processing", "3"],
            ["delivered", "3"],
            ["cancelled", "3"],
            ["pending", "2"],
            ["shipped", "1"],
        ]
    );
}

#[test]
fn v4_min_and_max_compare_as_strings() {
    // MySQL's documented quirk, confirmed differentially: MIN/MAX over an
    // ENUM compare the LABELS lexically, not the ordinals.
    let rows = run("SELECT MIN(status), MAX(status) FROM orders");
    assert_eq!(rows, [["cancelled", "shipped"]]);
}

#[test]
fn v5_a_greater_than_range_compares_labels() {
    // MySQL compares an ENUM to a string constant AS A STRING (confirmed
    // differentially): > 'processing' keeps only 'shipped' - 1 row.
    let rows = run("SELECT COUNT(*) FROM orders WHERE status > 'processing'");
    assert_eq!(rows, [["1"]]);
}

#[test]
fn v6_a_less_than_range_compares_labels() {
    // < 'delivered' lexically keeps only 'cancelled' - 3 rows.
    let rows = run("SELECT COUNT(*) FROM orders WHERE status < 'delivered'");
    assert_eq!(rows, [["3"]]);
}

#[test]
fn v7_between_compares_labels() {
    // BETWEEN 'processing' AND 'delivered' lexically is an empty interval
    // ('processing' > 'delivered'), so no rows qualify. Confirmed against
    // MySQL 8.4: ENUM constants in BETWEEN compare as strings.
    let rows = run("SELECT COUNT(*) FROM orders WHERE status BETWEEN 'processing' AND 'delivered'");
    assert_eq!(rows, [["0"]]);
}

#[test]
fn v8_distinct_orders_by_ordinal() {
    let rows = run("SELECT DISTINCT status FROM orders ORDER BY status");
    assert_eq!(
        column(&rows, 0),
        ["pending", "processing", "shipped", "delivered", "cancelled"]
    );
}

#[test]
fn v9_a_topk_limit_keeps_the_lowest_ordinals() {
    // LIMIT routes through the TopK path rather than the full sort.
    let rows = run("SELECT status FROM orders ORDER BY status, id LIMIT 3");
    assert_eq!(column(&rows, 0), ["pending", "pending", "processing"]);
}

#[test]
fn v10_a_window_order_walks_the_ordinal() {
    // ROW_NUMBER over ORDER BY status must hand rank 1 to a pending row and
    // rank 12 to a cancelled row.
    let rows = run(
        "SELECT status, ROW_NUMBER() OVER (ORDER BY status, id) AS r \
         FROM orders ORDER BY r",
    );
    assert_eq!(rows[0], ["pending", "1"]);
    assert_eq!(rows[11], ["cancelled", "12"]);
    assert_eq!(
        column(&rows, 0),
        [
            "pending",
            "pending",
            "processing",
            "processing",
            "processing",
            "shipped",
            "delivered",
            "delivered",
            "delivered",
            "cancelled",
            "cancelled",
            "cancelled",
        ]
    );
}
