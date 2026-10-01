//! `DATE()` of a zero value depends on what holds it. Under `NO_ZERO_DATE`
//! `MySQL` answers NULL for a zero `TIMESTAMP` column and keeps the zero
//! date of a `DATETIME` column; without the mode both stay the zero date.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, ParseMode, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "seen", DataType::DateTime64 { fsp: 0 }, true).with_timestamp(true),
            Column::new(3, "noted", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

fn run(sql_mode: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    let rows = [
        (1_u64, "0000-00-00 00:00:00", "0000-00-00 00:00:00"),
        (2, "2024-02-29 11:59:59", "2024-02-29 11:59:59"),
    ];
    store
        .bulk_ingest_snapshot(
            rows.into_iter()
                .map(|(id, seen, noted)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(seen.to_owned()),
                            Value::Utf8(noted.to_owned()),
                        ],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "visits",
        schema(),
        TableStatistics::with_row_count(2),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    pintail_sql::with_parse_mode(ParseMode::from_sql_mode(sql_mode), || {
        let sql = "SELECT id, DATE(seen), DATE(noted) FROM visits ORDER BY id";
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .expect("start");
        let mut answer = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                answer.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| match column.value_owned(row).expect("value") {
                            Value::UInt64(n) => n.to_string(),
                            Value::Utf8(text) => text,
                            Value::Null => "NULL".to_owned(),
                            other => format!("{other:?}"),
                        })
                        .collect(),
                );
            }
        }
        answer
    })
}

#[test]
fn date_of_a_zero_timestamp_is_null_only_under_no_zero_date() {
    assert_eq!(
        run("NO_ZERO_DATE"),
        [
            ["1", "NULL", "0000-00-00"],
            ["2", "2024-02-29", "2024-02-29"],
        ]
    );
    assert_eq!(
        run("ALLOW_INVALID_DATES"),
        [
            ["1", "0000-00-00", "0000-00-00"],
            ["2", "2024-02-29", "2024-02-29"],
        ]
    );
}
