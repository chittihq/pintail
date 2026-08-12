//! The sort operator must produce identical results whether its input fits
//! in memory or spills to sorted on-disk runs: same rows, same order, with
//! the spill engaging only under a memory ceiling that previously failed
//! the query outright.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);
const ROWS: u64 = 120_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
            Column::new(3, "score", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    // A shuffled, collision-heavy sort key: descending scores with ties,
    // and labels that reverse the id order inside each tie group.
    let score = i64::try_from(id % 97).expect("score fits i64");
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("label-{:07}", ROWS - id)),
            Value::Int64(score),
        ],
        id,
        false,
    )
}

fn run_sorted(memory_limit: usize) -> Result<Vec<(i64, String, u64)>, String> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table.ingest((1..=ROWS).map(row).collect()).expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TABLE_ID,
        "events",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let database = DatabaseEntry::new(DATABASE_ID, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(DATABASE_ID, TABLE_ID, &snapshot)]).expect("provider");

    let statement = parse_statement(
        "SELECT score, label, id FROM events ORDER BY score DESC, label ASC, id DESC",
    )
    .map_err(|error| format!("parse: {error}"))?;
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| format!("bind: {error}"))?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical = PhysicalPlanner::plan(logical, Collation::default())
        .map_err(|error| format!("plan: {error}"))?;
    let mut execution = Execution::start(physical, &provider, memory_limit, Collation::default())
        .map_err(|error| format!("start: {error}"))?;
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| format!("execute: {error}"))?
    {
        for index in batch.selection().selected_rows() {
            let score = match batch.columns()[0].value(index) {
                Some(Value::Int64(score)) => *score,
                other => return Err(format!("unexpected score {other:?}")),
            };
            let label = match batch.columns()[1].value(index) {
                Some(Value::Utf8(label)) => label.clone(),
                other => return Err(format!("unexpected label {other:?}")),
            };
            let id = match batch.columns()[2].value(index) {
                Some(Value::UInt64(id)) => *id,
                other => return Err(format!("unexpected id {other:?}")),
            };
            rows.push((score, label, id));
        }
    }
    Ok(rows)
}

#[test]
fn spilled_sort_matches_the_in_memory_order_exactly() {
    // Roomy ceiling: pure in-memory path.
    let reference = run_sorted(256 * 1024 * 1024).expect("in-memory sort");
    assert_eq!(reference.len() as u64, ROWS);
    // Tight ceiling: the sort retention alone (~12MB of row payloads)
    // exceeds it, so runs must spill; the scan working set still fits.
    // The same query previously failed with MemoryLimitExceeded.
    let spilled = run_sorted(8 * 1024 * 1024).expect("spilled sort");
    assert_eq!(spilled.len(), reference.len());
    assert_eq!(spilled, reference);
}
