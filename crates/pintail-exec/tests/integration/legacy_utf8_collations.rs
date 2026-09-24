//! A legacy `utf8` (utf8mb3) column holds only characters of the basic
//! plane, and over those its `general_ci` and `bin` collations weigh every
//! character as their utf8mb4 twins do. Such a column therefore compares,
//! groups and dedupes as the twin instead of being refused. Both twins pad
//! trailing spaces: `'a'` and `'a '` are one value under each.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: [(u64, &str); 4] = [(1, "A"), (2, "a"), (3, "a "), (4, "B")];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "g", DataType::Utf8, true)
                .with_collation(Some("utf8mb3_general_ci".to_owned())),
            Column::new(3, "b", DataType::Utf8, true).with_collation(Some("utf8_bin".to_owned())),
        ],
    )
    .expect("schema")
}

fn run(sql: &str) -> Vec<String> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, text)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            Value::Utf8((*text).to_owned()),
                            Value::Utf8((*text).to_owned()),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(table_id, "t", schema(), TableStatistics::with_row_count(4))
        .expect("entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row) {
                        Some(Value::Int64(number)) => number.to_string(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(Value::Utf8(text)) => text.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
    }
    rows
}

#[test]
fn a_utf8mb3_general_ci_column_compares_as_general_ci() {
    assert_eq!(run("SELECT COUNT(DISTINCT g) FROM t"), ["2"]);
    assert_eq!(run("SELECT COUNT(*) FROM t WHERE g = 'a'"), ["3"]);
    assert_eq!(
        run("SELECT COUNT(*) FROM t GROUP BY g ORDER BY g"),
        ["3", "1"]
    );
}

#[test]
fn a_utf8_bin_column_compares_as_utf8mb4_bin() {
    assert_eq!(run("SELECT COUNT(DISTINCT b) FROM t"), ["3"]);
    assert_eq!(run("SELECT COUNT(*) FROM t WHERE b = 'a'"), ["2"]);
    assert_eq!(run("SELECT id FROM t ORDER BY b, id"), ["1", "4", "2", "3"]);
}
