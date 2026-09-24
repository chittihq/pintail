//! A DECIMAL stored as text keeps its rows when the column's scale widens:
//! the stored text renders at the new scale, as a table rebuilt by the
//! ALTER does, both when read directly and when a grouped fold reads it.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const FRACTIONS: [&str; 3] = ["-99.9950", "1.1234", "0.0000"];

fn schema(version: u32, precision: u8, scale: u8) -> TableSchema {
    TableSchema::new(
        version,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "frac", DataType::Decimal { precision, scale }, true),
        ],
    )
    .expect("schema")
}

fn rows(store: &TableStore, schema: &TableSchema, sql: &str) -> Vec<Vec<String>> {
    let entry = TableEntry::new(
        TableId::new(1),
        "bounds",
        schema.clone(),
        TableStatistics::with_row_count(FRACTIONS.len() as u64),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
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
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).expect("value") {
                        Value::UInt64(n) => n.to_string(),
                        Value::Utf8(text) => text.clone(),
                        Value::Null => "NULL".to_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    rows
}

#[test]
fn a_widened_decimal_scale_renders_the_stored_rows_at_the_new_scale() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store = TableStore::open(directory.path(), schema(1, 22, 4), StoreOptions::default())
        .expect("store");
    store
        .bulk_ingest_snapshot(
            (1_u64..)
                .zip(FRACTIONS)
                .map(|(id, frac)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(frac.to_owned())],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let widened = schema(2, 24, 5);
    store.evolve_schema(widened.clone()).expect("evolve");
    let expected = [["1", "-99.99500"], ["2", "1.12340"], ["3", "0.00000"]];
    assert_eq!(
        rows(&store, &widened, "SELECT id, frac FROM bounds ORDER BY id"),
        expected
    );
    assert_eq!(
        rows(
            &store,
            &widened,
            "SELECT id, MAX(frac) FROM bounds GROUP BY id ORDER BY id"
        ),
        expected
    );
    assert_eq!(
        rows(
            &store,
            &widened,
            "SELECT id FROM bounds WHERE frac = 1.1234"
        ),
        [["2"]]
    );
}
