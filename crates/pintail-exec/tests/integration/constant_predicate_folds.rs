//! Constant predicates must fold the way `MySQL` folds them - found by the
//! differential grammar fuzzer against a live 8.4:
//!
//! 1. A constant-false WHERE returns the EMPTY SET, not an error: the old
//!    fold replaced the filter with a column-less `Empty` node, and any
//!    projection above it died with "physical input is missing <column>".
//! 2. A constant-true disjunct absorbs the whole OR before row evaluation:
//!    `x OR TRUE` is TRUE even where `x` would error row-wise (unsigned
//!    subtraction underflow here), and `x AND FALSE` is FALSE the same way.

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
            Column::new(2, "score", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64, score: i64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Int64(score)],
        id,
        false,
    )
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(
            (1..=4u64)
                .map(|id| row(id, 10 * i64::try_from(id).expect("small id")))
                .collect(),
        )
        .expect("bulk snapshot");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(15);
    let table_id = TableId::new(17);
    let entry = TableEntry::new(
        table_id,
        "events",
        schema(),
        TableStatistics::with_row_count(4),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
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
        let columns = batch.columns().len();
        for selected in batch.selection().selected_rows() {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(selected))
                    .cloned()
                    .expect("selected value");
                values.push(render(&value));
            }
            rows.push(values);
        }
    }
    rows
}

#[test]
fn constant_false_where_returns_empty_set() {
    assert_eq!(
        run("SELECT id FROM events WHERE 'zz' IS NULL"),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn constant_false_where_survives_grouping() {
    // The fuzzer's original shape: GROUP BY over a constant-false filter
    // died with "physical input is missing e.active" when the fold
    // dropped the input's columns.
    assert_eq!(
        run("SELECT score, COUNT(*), SUM(2 * 99) FROM events WHERE 'zz' IS NULL GROUP BY score"),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn constant_true_disjunct_absorbs_erroring_side() {
    // id is UNSIGNED: `id - 100` underflows on every row, but MySQL never
    // evaluates it because the right disjunct is constant TRUE.
    assert_eq!(
        run("SELECT id FROM events WHERE (id - 100) < 0 OR 'user-03' IS NOT NULL ORDER BY id"),
        vec![
            vec!["1".to_owned()],
            vec!["2".to_owned()],
            vec!["3".to_owned()],
            vec!["4".to_owned()],
        ]
    );
}

#[test]
fn constant_false_conjunct_absorbs_erroring_side() {
    assert_eq!(
        run("SELECT id FROM events WHERE (id - 100) < 0 AND 'zz' IS NULL"),
        Vec::<Vec<String>>::new()
    );
}
