//! `NO_UNSIGNED_SUBTRACTION` makes a subtraction signed: `CAST(0 AS
//! UNSIGNED) - 1` is -1, and an unsigned operand past the signed range is
//! out of range rather than wrapped. Without the mode an unsigned result
//! cannot go negative.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, ParseMode, parse_statement, with_parse_mode};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "u", DataType::UInt64, false),
        ],
    )
    .expect("schema")
}

fn run(sql: &str, mode: ParseMode) -> Result<Vec<Vec<String>>, String> {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot(
            [(1_u64, 0_u64), (2, u64::MAX)]
                .into_iter()
                .map(|(id, u)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::UInt64(u)],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "amounts",
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
    with_parse_mode(mode, || {
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .map_err(|error| error.to_string())?;
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .map_err(|error| error.to_string())?;
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .map_err(|error| error.to_string())?;
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| match column.value(row).expect("value") {
                            Value::Int64(n) => n.to_string(),
                            Value::UInt64(n) => n.to_string(),
                            other => format!("{other:?}"),
                        })
                        .collect(),
                );
            }
        }
        Ok(rows)
    })
}

#[test]
fn no_unsigned_subtraction_makes_a_subtraction_signed() {
    let signed = ParseMode {
        no_unsigned_subtraction: true,
        ..ParseMode::default()
    };
    assert_eq!(
        run("SELECT CAST(0 AS UNSIGNED) - 1", signed),
        Ok(vec![vec!["-1".to_owned()]])
    );
    assert_eq!(
        run("SELECT u - 1 FROM amounts WHERE id = 1", signed),
        Ok(vec![vec!["-1".to_owned()]])
    );
    assert!(run("SELECT u - 1 FROM amounts WHERE id = 2", signed).is_err());
    // Additions stay unsigned under the mode.
    assert_eq!(
        run("SELECT u + 1 FROM amounts WHERE id = 1", signed),
        Ok(vec![vec!["1".to_owned()]])
    );
    let plain = ParseMode::default();
    assert!(run("SELECT CAST(0 AS UNSIGNED) - 1", plain).is_err());
    assert!(run("SELECT u - 1 FROM amounts WHERE id = 1", plain).is_err());
}
