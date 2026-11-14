//! A table copied by a release that spelled a JSON double from 1e15 up
//! positionally keeps that text until it is copied again, next to rows
//! stored since with the exponent the source prints. The two spellings are
//! one number: they compare equal and fall into one group. Only the text
//! differs, and reading the document back as text shows the stored one.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Rows 1 and 3 hold the positional spelling an earlier release stored for
/// a double from 1e15 up; rows 2 and 4 hold the same numbers as they are
/// stored now. Row 5 is another number.
const ROWS: [(u64, Option<&str>); 5] = [
    (1, Some(r#"{"v": 4350000000000000.0}"#)),
    (2, Some(r#"{"v": 4.35e15}"#)),
    (3, Some("[1000000000000000.0]")),
    (4, Some("[1e15]")),
    (5, Some(r#"{"v": 4.36e15}"#)),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "doc", DataType::Json, true),
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
        "docs",
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
fn both_spellings_of_a_double_compare_equal() {
    let rows = run(
        "SELECT a.id, b.id FROM docs a JOIN docs b ON a.doc = b.doc \
         WHERE a.id < b.id ORDER BY a.id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_owned(), "2".to_owned()],
            vec!["3".to_owned(), "4".to_owned()],
        ]
    );
    let rows = run("SELECT id FROM docs WHERE doc->'$.v' = 4.35e15 ORDER BY id");
    assert_eq!(rows, vec![vec!["1".to_owned()], vec!["2".to_owned()]]);
}

#[test]
fn both_spellings_of_a_double_group_and_deduplicate_together() {
    let rows = run("SELECT COUNT(*) FROM docs GROUP BY doc ORDER BY MIN(id)");
    assert_eq!(
        rows,
        vec![
            vec!["2".to_owned()],
            vec!["2".to_owned()],
            vec!["1".to_owned()]
        ]
    );
    let rows = run("SELECT COUNT(DISTINCT doc) FROM docs");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
    let rows = run("SELECT COUNT(*) FROM (SELECT DISTINCT doc FROM docs) d");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

/// The documented residual: the text of a row is the text that was stored.
#[test]
fn the_text_of_a_document_is_the_spelling_it_was_stored_with() {
    let rows = run("SELECT CAST(doc AS CHAR) FROM docs WHERE id <= 2 ORDER BY id");
    assert_eq!(
        rows,
        vec![
            vec![r#"{"v": 4350000000000000.0}"#.to_owned()],
            vec![r#"{"v": 4.35e15}"#.to_owned()],
        ]
    );
}
