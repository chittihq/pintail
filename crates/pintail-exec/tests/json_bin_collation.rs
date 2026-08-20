//! JSON string results collate `utf8mb4_bin` (measured against `MySQL` 8.4):
//! grouping and comparing them is case-sensitive, losing only to a real
//! column's collation. This is the in-process repro for the oracle family -
//! and for the case-703 regression, where the bin-collated filter died with
//! "bound expression has an invalid physical type".

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: [(u64, Option<&str>); 5] = [
    (1, Some(r#"{"tags":["premium"],"score":1}"#)),
    (2, Some(r#"{"tags":["PREMIUM"],"score":2}"#)),
    (3, Some(r#"{"tags":["premium"],"score":3}"#)),
    (4, None),
    (5, Some(r#"{"tags":[],"score":0}"#)),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "meta", DataType::Json, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64, meta: Option<&str>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            meta.map_or(Value::Null, |text| Value::Utf8(text.to_owned())),
        ],
        id,
        false,
    )
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, meta)| row(*id, *meta)).collect())
        .expect("bulk snapshot");
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
        for row in batch.selection().selected_rows() {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value");
                values.push(match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Boolean(flag) => if flag { "1" } else { "0" }.to_owned(),
                    Value::Utf8(text) | Value::Enum { label: text, .. } => text,
                    Value::UInt64(number) => number.to_string(),
                    Value::Int64(number) => number.to_string(),
                    other => format!("{other:?}"),
                });
            }
            rows.push(values);
        }
    }
    rows
}

#[test]
fn a_bin_collated_json_filter_executes() {
    // The case-703 regression: this exact shape died in execution once the
    // comparison's collation resolved to utf8mb4_bin.
    let rows = run("SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' = 'premium'");
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn json_grouping_is_case_sensitive() {
    // Ordered by count, not by the alias: ordering by an alias of a
    // bin-collated group key is a known gap (the GroupKey reference is
    // opaque to collation resolution) tracked separately; what THIS test
    // pins is the case-sensitive grouping itself.
    let rows = run("SELECT meta->>'$.tags[0]' AS t, COUNT(*) FROM orders \
         WHERE meta IS NOT NULL AND meta->>'$.tags[0]' IS NOT NULL \
         GROUP BY meta->>'$.tags[0]' ORDER BY COUNT(*), t");
    assert_eq!(
        rows,
        vec![
            vec!["PREMIUM".to_owned(), "1".to_owned()],
            vec!["premium".to_owned(), "2".to_owned()],
        ]
    );
}

#[test]
fn json_literal_comparison_is_case_sensitive() {
    let rows = run(
        "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\"k\":\"A\"}','$.k')) = 'a', \
                JSON_UNQUOTE(JSON_EXTRACT('{\"k\":\"A\"}','$.k')) = 'A'",
    );
    assert_eq!(rows, vec![vec!["0".to_owned(), "1".to_owned()]]);
}

#[test]
fn probe_projection_only() {
    let rows = run("SELECT meta->>'$.tags[0]' FROM orders ORDER BY id");
    assert_eq!(rows.len(), 5);
}

#[test]
fn probe_filter_without_json() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE id > 2");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn probe_explicit_unquote_filter() {
    let rows = run(
        "SELECT COUNT(*) FROM orders WHERE JSON_UNQUOTE(JSON_EXTRACT(meta,'$.tags[0]')) = 'premium'",
    );
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn probe_is_not_null_filter() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' IS NOT NULL");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn probe_extract_no_unquote_filter() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE JSON_EXTRACT(meta,'$.tags[0]') IS NOT NULL");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}
