//! Date-range scans must return identical rows whether or not SMA segment
//! pruning engages, and pruning must disable itself the moment overlapping
//! row versions make it unsound.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);
/// Rows per ingested batch; each batch flushes as its own segment.
const BATCH: u64 = 4_000;
const BATCHES: u64 = 6;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "day", DataType::Date32, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, day_offset: u64, version: u64) -> StoredRow {
    let day =
        chrono::NaiveDate::from_ymd_opt(2021, 1, 1).expect("date") + chrono::Days::new(day_offset);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(day.format("%Y-%m-%d").to_string()),
            Value::Int64(i64::try_from(id % 977).expect("amount")),
        ],
        version,
        false,
    )
}

fn run_query(table: &TableStore, sql: &str) -> Vec<Vec<Value>> {
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TABLE_ID,
        "events",
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
    let physical = PhysicalPlanner::plan(logical).expect("plan");
    let mut execution = Execution::start(physical, &provider, 256 * 1024 * 1024).expect("start");
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
    rows
}

/// Chronological batches: each segment covers a distinct id range and a
/// distinct slice of days, so date bounds prune most segments.
fn ingest_chronological(table: &mut TableStore) {
    for batch in 0..BATCHES {
        let rows = (0..BATCH)
            .map(|offset| {
                let id = batch * BATCH + offset + 1;
                // ~66 days per batch, strictly increasing with id.
                row(id, id / 60, id)
            })
            .collect();
        table.ingest(rows).expect("ingest batch");
    }
}

const RANGE_SQL: &str = "SELECT id, day, amount FROM events \
     WHERE day >= '2021-03-01' AND day < '2021-06-01' ORDER BY id";
const BETWEEN_SQL: &str = "SELECT COUNT(*), MIN(day), MAX(day) FROM events \
     WHERE day BETWEEN '2021-02-01' AND '2021-04-15'";

#[test]
fn pruned_date_scans_match_full_scans_exactly() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    ingest_chronological(&mut table);

    // Reference rows computed engine-free from the generator.
    let expected: Vec<u64> = (1..=BATCH * BATCHES)
        .filter(|id| {
            let day = chrono::NaiveDate::from_ymd_opt(2021, 1, 1).expect("date")
                + chrono::Days::new(id / 60);
            let lo = chrono::NaiveDate::from_ymd_opt(2021, 3, 1).expect("date");
            let hi = chrono::NaiveDate::from_ymd_opt(2021, 6, 1).expect("date");
            day >= lo && day < hi
        })
        .collect();
    let rows = run_query(&table, RANGE_SQL);
    assert_eq!(rows.len(), expected.len());
    assert_eq!(
        rows.iter()
            .map(|row| match row[0] {
                Value::UInt64(id) => id,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect::<Vec<_>>(),
        expected
    );

    let aggregate = run_query(&table, BETWEEN_SQL);
    assert_eq!(aggregate.len(), 1);
    assert_eq!(aggregate[0][1], Value::Utf8("2021-02-01".to_owned()));
    assert_eq!(aggregate[0][2], Value::Utf8("2021-04-15".to_owned()));
}

#[test]
fn overlapping_row_versions_keep_date_scans_exact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    ingest_chronological(&mut table);
    // A late segment rewrites two early keys onto in-range dates, and one
    // in-range key onto an out-of-range date. Its key range overlaps every
    // earlier segment, so value pruning must disable itself; the winning
    // versions must still decide the results.
    let moved_in_a = row(5, 70, BATCH * BATCHES + 1); // 2021-03-12
    let moved_in_b = row(BATCH * BATCHES, 75, BATCH * BATCHES + 2); // 2021-03-17
    let mut moved_out = row(4000, 0, BATCH * BATCHES + 3); // 2021-01-01
    let _ = &mut moved_out;
    table
        .ingest(vec![moved_in_a, moved_in_b, moved_out])
        .expect("ingest overlapping versions");

    // Filtered MIN over a value-born date column: previously errored
    // NumericOverflow (LazyText Ready refused format_unit).
    let probe_min = run_query(
        &table,
        "SELECT MIN(day) FROM events WHERE day BETWEEN '2021-02-01' AND '2021-04-15'",
    );
    assert_eq!(probe_min, vec![vec![Value::Utf8("2021-02-01".to_owned())]]);
    let rows = run_query(&table, RANGE_SQL);
    let ids: Vec<u64> = rows
        .iter()
        .map(|row| match row[0] {
            Value::UInt64(id) => id,
            ref other => panic!("unexpected id {other:?}"),
        })
        .collect();
    assert!(ids.contains(&5), "rewritten key 5 must enter the range");
    assert!(
        ids.contains(&(BATCH * BATCHES)),
        "rewritten last key must enter the range"
    );
    assert!(
        !ids.contains(&4000),
        "key 4000 moved out of the range and must vanish"
    );
    // The moved-in rows carry their new dates.
    let day_of = |target: u64| {
        rows.iter()
            .find(|row| row[0] == Value::UInt64(target))
            .map(|row| row[1].clone())
    };
    assert_eq!(day_of(5), Some(Value::Utf8("2021-03-12".to_owned())));
}
