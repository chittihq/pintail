//! Proves temporal pruning actually FIRES - not merely that it is exact -
//! on the production shape reported as #244: a large table whose DATETIME
//! column is clustered by insert order (an activity log's createdAt), read
//! with a selective time range.
//!
//! The earlier 4M-row measurement recorded `blocks_pruned=0` and nearly sent
//! the roadmap toward rebuilding pruning; the query it measured was a bare
//! COUNT with no predicate, which prunes nothing by definition, and its
//! doubled seed data had no time clustering for bounds to bite on. This
//! pins the real behavior: chronological segments + a range predicate must
//! skip most of the table, and the skipped scan must stay byte-exact.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);
/// Rows per ingested batch; each batch flushes as its own segment.
const BATCH: u64 = 4_000;
const BATCHES: u64 = 8;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "created_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// One row per minute from 2026-01-01 00:00:00, in id order - insert order
/// IS time order, the activity-log shape.
fn row(id: u64) -> StoredRow {
    let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .expect("date")
        .and_hms_opt(0, 0, 0)
        .expect("time");
    let created = start + chrono::Duration::minutes(i64::try_from(id).expect("small id"));
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(created.format("%Y-%m-%d %H:%M:%S").to_string()),
            Value::Int64(i64::try_from(id % 977).expect("amount")),
        ],
        id,
        false,
    )
}

fn run_query(table: &TableStore, sql: &str) -> (Vec<Vec<Value>>, PhysicalScanStats) {
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TABLE_ID,
        "activity",
        schema(),
        TableStatistics::with_row_count(BATCH * BATCHES),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let database = DatabaseEntry::new(DATABASE_ID, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(DATABASE_ID, TABLE_ID, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for index in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(index).cloned().expect("value"))
                    .collect::<Vec<_>>(),
            );
        }
    }
    let stats = provider
        .scan_stats(DATABASE_ID, TABLE_ID)
        .expect("scan recorded stats");
    (rows, stats)
}

fn ingest_chronological(table: &mut TableStore) {
    for batch in 0..BATCHES {
        let rows = (batch * BATCH..(batch + 1) * BATCH).map(row).collect();
        table.bulk_ingest_snapshot(rows).expect("ingest batch");
    }
}

#[test]
fn a_selective_datetime_range_prunes_most_of_a_chronological_table() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    ingest_chronological(&mut table);

    // The last ~6 hours of a 22-day log: one segment's worth of rows.
    let (rows, stats) = run_query(
        &table,
        "SELECT COUNT(*), SUM(amount) FROM activity \
         WHERE created_at >= '2026-01-22 00:00:00'",
    );
    let expected = u64::try_from(
        (0..BATCH * BATCHES)
            .filter(|id| id * 60 >= 21 * 86_400)
            .count(),
    )
    .expect("count");
    assert_eq!(rows[0][0], Value::UInt64(expected));
    assert!(
        stats.segments_pruned + stats.blocks_pruned > 0,
        "a selective range over chronological segments must skip storage; \
         stats: {stats:?}"
    );
    // Most of the table must be skipped, not a token block: at least half
    // the segments hold rows strictly before the range.
    assert!(
        stats.segments_pruned >= (usize::try_from(BATCHES).expect("small")) / 2,
        "expected at least half the segments pruned; stats: {stats:?}"
    );
}

#[test]
fn an_unselective_range_prunes_nothing_and_stays_exact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    ingest_chronological(&mut table);

    let (rows, _) = run_query(
        &table,
        "SELECT COUNT(*) FROM activity WHERE created_at >= '2026-01-01 00:00:00'",
    );
    assert_eq!(rows[0][0], Value::UInt64(BATCH * BATCHES));
}
