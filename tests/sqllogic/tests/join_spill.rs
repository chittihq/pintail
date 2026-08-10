//! Hash joins must produce identical results whether the build side fits
//! in memory or grace-partitions to disk, including LEFT JOIN null
//! extension, unmatched probe rows, and NULL join keys.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const ORDERS_ID: TableId = TableId::new(1);
const USERS_ID: TableId = TableId::new(2);
const USERS: u64 = 120_000;
const ORDERS: u64 = 120_000;

fn orders_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::UInt64, true),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("orders schema")
}

fn users_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "weight", DataType::Int64, false),
        ],
    )
    .expect("users schema")
}

fn order_row(id: u64) -> StoredRow {
    // Every 97th order has a NULL user; user ids run past the users table
    // so a slice of orders never matches.
    let user = if id.is_multiple_of(97) {
        Value::Null
    } else {
        Value::UInt64(id % (USERS + 5_000) + 1)
    };
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            user,
            Value::Int64(i64::try_from(id % 1_009).expect("amount") - 300),
        ],
        id,
        false,
    )
}

fn user_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % 613).expect("weight")),
        ],
        id,
        false,
    )
}

fn run_query(memory_limit: usize, sql: &str) -> Vec<Vec<Value>> {
    let orders_dir = tempfile::tempdir().expect("orders dir");
    let users_dir = tempfile::tempdir().expect("users dir");
    let mut orders = TableStore::open(orders_dir.path(), orders_schema(), StoreOptions::default())
        .expect("open orders");
    orders
        .ingest((1..=ORDERS).map(order_row).collect())
        .expect("ingest orders");
    let mut users = TableStore::open(users_dir.path(), users_schema(), StoreOptions::default())
        .expect("open users");
    users
        .ingest((1..=USERS).map(user_row).collect())
        .expect("ingest users");
    let orders_snapshot = orders.snapshot();
    let users_snapshot = users.snapshot();
    let orders_entry = TableEntry::new(
        ORDERS_ID,
        "orders",
        orders_schema(),
        TableStatistics::with_row_count(ORDERS),
    )
    .expect("orders entry")
    .with_key_columns([1])
    .expect("orders key");
    let users_entry = TableEntry::new(
        USERS_ID,
        "users",
        users_schema(),
        TableStatistics::with_row_count(USERS),
    )
    .expect("users entry")
    .with_key_columns([1])
    .expect("users key");
    let database =
        DatabaseEntry::new(DATABASE_ID, "app", [orders_entry, users_entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (DATABASE_ID, ORDERS_ID, &orders_snapshot),
        (DATABASE_ID, USERS_ID, &users_snapshot),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical = PhysicalPlanner::plan(logical).expect("plan");
    let mut execution = Execution::start(physical, &provider, memory_limit).expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for index in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(index).cloned().expect("value"))
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

const INNER_SQL: &str = "SELECT COUNT(*), SUM(o.amount), SUM(u.weight) \
     FROM orders o JOIN users u ON o.user_id = u.id";
const LEFT_SQL: &str = "SELECT COUNT(*), COUNT(u.id), SUM(o.amount) \
     FROM orders o LEFT JOIN users u ON o.user_id = u.id";

#[test]
fn grace_partitioned_joins_match_in_memory_joins_exactly() {
    // Roomy ceiling: the resident-map path.
    let inner_reference = run_query(256 * 1024 * 1024, INNER_SQL);
    let left_reference = run_query(256 * 1024 * 1024, LEFT_SQL);
    // Tight ceiling: the 60k-user build side exceeds half the budget, so
    // the join partitions to disk; results must match byte for byte. The
    // same queries previously failed with MemoryLimitExceeded here.
    let inner_spilled = run_query(32 * 1024 * 1024, INNER_SQL);
    assert_eq!(inner_spilled, inner_reference);
    let left_spilled = run_query(32 * 1024 * 1024, LEFT_SQL);
    assert_eq!(left_spilled, left_reference);
}
