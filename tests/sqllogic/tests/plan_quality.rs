use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{SnapshotScanProvider, explain_analyze_statement};
use pintail_sql::parse_statement;
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);

#[test]
fn explain_analyze_proves_segment_and_block_pruning() {
    let directory = tempfile::tempdir().expect("temporary table");
    let schema = schema();
    let options = StoreOptions {
        block_rows: 2,
        ..StoreOptions::default()
    };
    let mut table =
        TableStore::open(directory.path(), schema.clone(), options).expect("open table");
    table
        .ingest((1..=4).map(row).collect())
        .expect("first ingest");
    table.flush().expect("first flush");
    table
        .ingest((5..=8).map(row).collect())
        .expect("second ingest");
    table.flush().expect("second flush");
    let snapshot = table.snapshot();

    let entry = TableEntry::new(
        TABLE_ID,
        "events",
        schema,
        TableStatistics::with_row_count(8),
    )
    .expect("table entry")
    .with_key_columns([1])
    .expect("key columns");
    let database = DatabaseEntry::new(DATABASE_ID, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(DATABASE_ID, TABLE_ID, &snapshot)]).expect("provider");
    let statement = parse_statement(
        "EXPLAIN ANALYZE \
         SELECT name FROM events WHERE id BETWEEN 5 AND 6 ORDER BY name",
    )
    .expect("parse");

    let explanation =
        explain_analyze_statement(&statement, &catalog, Some("app"), &provider, 64 * 1024)
            .expect("analyze");

    assert!(explanation.contains("predicates=1"));
    assert!(explanation.contains("actual_segments=1/2"));
    assert!(explanation.contains("actual_blocks=1/2"));
    assert!(explanation.contains("decoded_blocks=5"));
}

#[test]
fn recursive_cte_depth_guard_aborts_non_converging_queries() {
    let directory = tempfile::tempdir().expect("temporary table");
    let schema = schema();
    let table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("open");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TABLE_ID,
        "events",
        schema,
        TableStatistics::with_row_count(0),
    )
    .expect("table entry")
    .with_key_columns([1])
    .expect("key columns");
    let database = DatabaseEntry::new(DATABASE_ID, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(DATABASE_ID, TABLE_ID, &snapshot)]).expect("provider");
    // UNION ALL with a self-copying member never converges; the fixpoint
    // must abort at MySQL's default cte_max_recursion_depth.
    let statement = parse_statement(
        "EXPLAIN ANALYZE WITH RECURSIVE r (n) AS (\
         SELECT 1 UNION ALL SELECT n FROM r) SELECT n FROM r",
    )
    .expect("parse");
    let error = explain_analyze_statement(
        &statement,
        &catalog,
        Some("app"),
        &provider,
        64 * 1024 * 1024,
    )
    .expect_err("depth guard");
    assert!(
        error.to_string().contains("recursive query aborted"),
        "unexpected error: {error}"
    );
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(format!("event-{id:02}"))],
        id,
        false,
    )
}
