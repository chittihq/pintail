//! A memoized aggregate must belong to the table it was computed over.
//!
//! The settled aggregate memo is a process-global map keyed by the table's
//! directory, the manifest generation and the query's signature. Nothing
//! removes an entry when a table goes away: a table dropped and recreated
//! at the same path starts again at generation zero and walks the same
//! generations, so a second incarnation can present a key an earlier one
//! already answered — and be handed the earlier table's rows.
//!
//! Every other test in this crate opens its store under a fresh temporary
//! directory, which is why none of them can see this: a path used once
//! cannot collide with itself.

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
            Column::new(2, "grp", DataType::UInt64, false),
            Column::new(
                3,
                "total",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
        ],
    )
    .expect("schema")
}

fn row(id: u64, grp: u64, total: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(grp),
            Value::Utf8(total.to_owned()),
        ],
        id,
        false,
    )
}

/// Ingests `fixture` into a store at `directory` and answers `sql` over it.
///
/// The directory is the caller's, not a fresh temporary one, so two calls
/// can put two different tables at the same path — which is the whole
/// point here.
fn answer_at(directory: &std::path::Path, fixture: &[StoredRow], sql: &str) -> Vec<Vec<String>> {
    let mut table =
        TableStore::open(directory, schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(fixture.to_vec())
        .expect("bulk snapshot");
    answer_over(&table, sql)
}

/// Answers `sql` over whatever state the caller has already built.
fn answer_over(table: &TableStore, sql: &str) -> Vec<Vec<String>> {
    let snapshot = table.snapshot();
    let (database_id, table_id) = (DatabaseId::new(15), TableId::new(17));
    let entry = TableEntry::new(
        table_id,
        "orders",
        schema(),
        TableStatistics::with_row_count(64),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key columns");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(database_id, "app", [entry]).expect("database")])
            .expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("execution");
    let mut out = Vec::new();
    while let Some(batch) = execution.next_batch().expect("pull") {
        for row in batch.selection().selected_rows() {
            let mut values = Vec::new();
            for column in 0..batch.columns().len() {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("value");
                values.push(match value {
                    Value::Utf8(text) => text,
                    Value::UInt64(number) => number.to_string(),
                    Value::Int64(number) => number.to_string(),
                    Value::Null => "NULL".to_owned(),
                    other => format!("{other:?}"),
                });
            }
            out.push(values);
        }
    }
    out.sort();
    out
}

const SQL: &str = "SELECT grp, ROUND(AVG(total), 4) AS avg_total, \
                   ROUND(SUM(total) / COUNT(*), 4) AS mean_check FROM orders \
                   GROUP BY grp ORDER BY grp";

/// A table recreated at a path answers from its own rows, not the rows of
/// the table that used to live there.
#[test]
fn a_recreated_table_does_not_answer_from_its_predecessor() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("table");
    std::fs::create_dir(&path).expect("table directory");

    let first: Vec<StoredRow> = (1..=31).map(|id| row(id, 1, "322.85")).collect();
    let before = answer_at(&path, &first, SQL);
    assert_eq!(before.len(), 1, "one group");

    // Drop the table the way a dropped and re-added replica does: the path
    // is reclaimed, and what replaces it starts from an empty manifest.
    std::fs::remove_dir_all(&path).expect("drop table");
    std::fs::create_dir(&path).expect("recreate table directory");

    // The same row count and group, deliberately: the shapes match, so the
    // second table walks the same generations and presents the same key.
    // Only the values differ, and only in a place ROUND(_, 4) can show.
    let second: Vec<StoredRow> = (1..=31)
        .map(|id| row(id, 1, if id == 1 { "322.86" } else { "322.85" }))
        .collect();
    let after = answer_at(&path, &second, SQL);

    assert_ne!(
        after, before,
        "the recreated table's rows differ, so its answer must differ; \
         an identical answer means the memo served the dropped table's rows"
    );
    assert_eq!(
        after[0][1], after[0][2],
        "AVG and SUM/COUNT must agree over the recreated table"
    );
}
