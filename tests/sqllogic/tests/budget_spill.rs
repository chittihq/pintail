//! The process-wide memory budget is a spill signal, not a verdict. Every
//! admitted query is entitled to its own ceiling, but their sum is not, and
//! under load the budget refuses reservations the query ceiling would have
//! allowed. A join whose build side is refused by the budget must partition
//! to disk exactly as it does under its own ceiling, and answer the same.
//!
//! Its own test binary: the budget is process-wide, and lowering it beside
//! other tests would refuse their reservations too.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    init_shared_memory_budget, shared_memory_budget,
};
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
            Column::new(3, "label", DataType::Utf8, false),
        ],
    )
    .expect("users schema")
}

fn order_row(id: u64) -> StoredRow {
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
            Value::Int64(i64::try_from(id % 13).expect("weight")),
            Value::Utf8(format!("user-{id:07}-with-a-label-wide-enough-to-matter")),
        ],
        id,
        false,
    )
}

/// The query ceiling is far above what the join needs, so only the shared
/// budget can refuse it.
const QUERY_CEILING: usize = 1024 * 1024 * 1024;

fn run_query(
    sql: &str,
) -> Result<(Vec<Vec<Value>>, pintail_exec::spill::QuerySpillMetrics), String> {
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
    let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
    let mut execution = Execution::start(physical, &provider, QUERY_CEILING, Collation::default())
        .map_err(|error| format!("start: {error}"))?;
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| format!("execute: {error}"))?
    {
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
    Ok((rows, execution.spill_metrics()))
}

const INNER_SQL: &str = "SELECT COUNT(*), SUM(o.amount), SUM(u.weight), MAX(u.label) \
     FROM orders o JOIN users u ON o.user_id = u.id";
const LEFT_SQL: &str = "SELECT COUNT(*), COUNT(u.id), SUM(o.amount), MIN(u.label) \
     FROM orders o LEFT JOIN users u ON o.user_id = u.id";

#[test]
fn a_join_refused_by_the_shared_budget_partitions_instead_of_failing() {
    // Unbounded budget: the resident-map path, and the answer to match.
    init_shared_memory_budget(0);
    let (inner_reference, inner_metrics) = run_query(INNER_SQL).expect("in-memory inner join");
    let (left_reference, _) = run_query(LEFT_SQL).expect("in-memory left join");
    assert_eq!(inner_metrics.files, 0, "the reference must not spill");
    // A budget the build side cannot fit in, under a query ceiling it
    // easily would: every refusal comes from the budget, and each one used
    // to be `server memory limit exceeded` back to the client.
    init_shared_memory_budget(40 * 1024 * 1024);
    let inner = run_query(INNER_SQL);
    let left = run_query(LEFT_SQL);
    init_shared_memory_budget(0);
    let (inner_rows, inner_metrics) = inner.expect("inner join under the budget");
    assert_eq!(inner_rows, inner_reference);
    assert!(
        inner_metrics.files > 0,
        "the budget must have forced a spill"
    );
    let (left_rows, left_metrics) = left.expect("left join under the budget");
    assert_eq!(left_rows, left_reference);
    assert!(
        left_metrics.files > 0,
        "the budget must have forced a spill"
    );
    assert_eq!(
        shared_memory_budget().used(),
        0,
        "every query repaid the budget"
    );
}
