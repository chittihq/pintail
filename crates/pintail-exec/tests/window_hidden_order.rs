//! A window query may sort its output by a column it does not select, as in
//! `MySQL`: windows keep every source row, so the hidden sort column is still
//! a row-per-row value of the one scope. Binding refused `ORDER BY` on such a
//! column once a window appeared in the select list.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// (id, bucket, amount): bucket order disagrees with id order.
const ROWS: [(u64, i64, i64); 5] = [(1, 3, 10), (2, 1, 20), (3, 2, 30), (4, 1, 40), (5, 3, 50)];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "bucket", DataType::Int64, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, bucket, amount)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            Value::Int64(*bucket),
                            Value::Int64(*amount),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk snapshot");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(3);
    let table_id = TableId::new(5);
    let entry = TableEntry::new(
        table_id,
        "entries",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("pull batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).expect("selected value") {
                        Value::Int64(number) => number.to_string(),
                        Value::UInt64(number) => number.to_string(),
                        Value::Utf8(text) => text.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    rows
}

#[test]
fn a_window_query_sorts_by_an_unselected_column() {
    let rows = run(
        "SELECT id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
         FROM entries ORDER BY bucket, id",
    );
    assert_eq!(
        rows,
        [
            ["2", "30"],
            ["4", "100"],
            ["3", "60"],
            ["1", "10"],
            ["5", "150"],
        ]
    );
}

#[test]
fn a_window_over_a_derived_table_sorts_by_its_unselected_column() {
    let rows = run(
        "WITH x AS (SELECT id AS event_id, bucket AS group_id, amount FROM entries) \
         SELECT event_id, SUM(amount) OVER w AS total FROM x \
         WINDOW w AS (PARTITION BY group_id) ORDER BY group_id, event_id",
    );
    assert_eq!(
        rows,
        [
            ["2", "60"],
            ["4", "60"],
            ["3", "30"],
            ["1", "60"],
            ["5", "60"],
        ]
    );
}
