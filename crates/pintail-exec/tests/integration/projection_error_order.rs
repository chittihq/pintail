//! Which error a projection raises when two of its expressions fail on
//! different rows: the earlier row's, wherever its expression stands in
//! the select list, as a row-at-a-time evaluation raises it. Answering one
//! expression over the whole batch before the next raised the leftmost
//! expression's error instead.

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
            Column::new(2, "a", DataType::Utf8, false),
            Column::new(3, "b", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn outcome(sql: &str) -> String {
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
    let row = |id: u64, a: &str, b: &str| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            vec![
                Value::UInt64(id),
                Value::Utf8(a.to_owned()),
                Value::Utf8(b.to_owned()),
            ],
            1,
            false,
        )
    };
    table
        .bulk_ingest_snapshot(vec![
            row(1, "{\"x\": 1}", "{\"y\": 2}"),
            row(2, "{\"x\": 1}", "oops-b"),
            row(3, "oops-a", "{\"y\": 2}"),
        ])
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema(),
        TableStatistics::with_row_count(3),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = match Binder::new(&catalog, Some("app")).bind(&parse_statement(sql).expect("parse"))
    {
        Ok(bound) => bound,
        Err(error) => return format!("bind error: {error}"),
    };
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = match Execution::start(plan, &provider, 64 << 20, Collation::default()) {
        Ok(execution) => execution,
        Err(error) => return format!("start error: {error}"),
    };
    let mut rows = 0;
    loop {
        match execution.next_batch() {
            Ok(Some(batch)) => rows += batch.selection().selected_rows().count(),
            Ok(None) => return format!("{rows} rows, no error"),
            Err(error) => return format!("error: {error}"),
        }
    }
}

/// Row 2 holds the text `b` cannot read as JSON and row 3 the one `a`
/// cannot; the product overflows on the one row its comparison names.
#[test]
fn the_earlier_row_decides_which_error_a_projection_raises() {
    const DOCUMENT: &str = "error: bound expression has an invalid physical type";
    const OVERFLOW: &str = "error: numeric expression overflow";
    for (sql, expected) in [
        (
            "SELECT JSON_LENGTH(b), (id = 3) * 9223372036854775807 * 3 FROM t",
            DOCUMENT,
        ),
        (
            "SELECT (id = 3) * 9223372036854775807 * 3, JSON_LENGTH(b) FROM t",
            DOCUMENT,
        ),
        (
            "SELECT JSON_LENGTH(a), (id = 2) * 9223372036854775807 * 3 FROM t",
            OVERFLOW,
        ),
        (
            "SELECT (id = 2) * 9223372036854775807 * 3, JSON_LENGTH(a) FROM t",
            OVERFLOW,
        ),
    ] {
        assert_eq!(outcome(sql), expected, "{sql}");
    }
}
