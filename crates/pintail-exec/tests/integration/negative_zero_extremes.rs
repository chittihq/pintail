//! `MIN`, `MAX` and `ANY_VALUE` over a DOUBLE return the value they saw, and a
//! negative zero is a value with a sign: a group holding only `-0` answers
//! `-0`, as MySQL does. Grouping and DISTINCT are the places the two zeros
//! are one value; carrying an extreme through the grouped lanes under the
//! grouping key's normalized bits turned every such `-0` into `0`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{
    Column, DataType, Float64, KeyPart, PrimaryKey, StoredRow, TableSchema, Value,
};

/// (id, k, reading): group 1 holds only negative zeros, group 2 both zeros.
const ROWS: [(u64, u64, f64); 4] = [(1, 1, -0.0), (2, 1, -0.0), (3, 2, -0.0), (4, 2, 0.0)];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::UInt64, false),
            Column::new(3, "reading", DataType::Float64, false),
        ],
    )
    .expect("schema")
}

fn run(sql: &str) -> Vec<Vec<Value>> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    store
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, k, reading)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            Value::UInt64(*k),
                            Value::Float64(Float64::new(*reading)),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk ingest");
    let snapshot = store.snapshot();
    let database_id = DatabaseId::new(21);
    let table_id = TableId::new(22);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [TableEntry::new(
            table_id,
            "readings",
            schema(),
            TableStatistics::with_row_count(ROWS.len() as u64),
        )
        .expect("table entry")],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind query");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                    .collect(),
            );
        }
    }
    rows
}

fn negative_zero(value: &Value) -> bool {
    matches!(value, Value::Float64(number) if number.get() == 0.0 && number.get().is_sign_negative())
}

#[test]
fn grouped_extremes_of_negative_zeros_keep_the_sign() {
    let rows = run("SELECT k, MIN(reading), MAX(reading), ANY_VALUE(reading) \
         FROM readings GROUP BY k ORDER BY k");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::UInt64(1));
    for value in &rows[0][1..] {
        assert!(negative_zero(value), "group 1 answered {value:?}");
    }
}

#[test]
fn grouped_distinct_counts_the_two_zeros_once() {
    let rows = run("SELECT k, COUNT(DISTINCT reading) FROM readings GROUP BY k ORDER BY k");
    let counts = rows
        .iter()
        .map(|row| match row[1] {
            Value::Int64(count) => u64::try_from(count).expect("non-negative count"),
            Value::UInt64(count) => count,
            ref other => panic!("count answered {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, [1, 1]);
}
