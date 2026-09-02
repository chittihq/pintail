//! High-cardinality GROUP BY must produce identical results whether the
//! group map fits in memory or spills to sorted on-disk runs: same groups,
//! same aggregate values, with the spill engaging only under a memory
//! ceiling that previously failed the query outright.

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
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "tag", DataType::Utf8, false),
            Column::new(4, "score", DataType::Int64, false),
            Column::new(5, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    // ~30k (grp, tag) groups over 120k rows: a few rows per group,
    // negative scores included, and label collisions so COUNT(DISTINCT)
    // dedups both integer and text keys.
    let grp = i64::try_from(id % 15_000).expect("grp fits i64");
    let score = i64::try_from(id % 1_000).expect("score fits i64") - 500;
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(grp),
            Value::Utf8(format!("t{}", (id / 15_000) % 2)),
            Value::Int64(score),
            Value::Utf8(format!("label-{:07}", (id * 31) % ROWS)),
        ],
        id,
        false,
    )
}

fn run_aggregated(memory_limit: usize) -> Result<Vec<Vec<Value>>, String> {
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
        "SELECT grp, tag, COUNT(*), SUM(score), AVG(score), MIN(label), \
         COUNT(DISTINCT score), COUNT(DISTINCT label) \
         FROM events GROUP BY grp, tag ORDER BY grp, tag",
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
            let row = batch
                .columns()
                .iter()
                .map(|column| {
                    column
                        .value(index)
                        .cloned()
                        .ok_or_else(|| "row outside an output column".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(row);
        }
    }
    Ok(rows)
}

#[test]
fn spilled_aggregation_matches_the_in_memory_groups_exactly() {
    // Roomy ceiling: pure in-memory path.
    let reference = run_aggregated(256 * 1024 * 1024).expect("in-memory aggregation");
    assert_eq!(reference.len(), 30_000);
    // Tight ceiling: the live group map (~30k keys with distinct sets on
    // both integer and text keys) needs well over half the budget, so runs
    // must spill; the scan working set, the downstream ORDER BY (which
    // holds the aggregation output while it runs), and the finished rows
    // still fit. The same query previously failed with
    // MemoryLimitExceeded at this ceiling.
    let spilled = run_aggregated(24 * 1024 * 1024).expect("spilled aggregation");
    assert_eq!(spilled, reference);
}

/// Every ceiling between "spills" and "fits" must produce the same groups.
/// The gate once caught a 5 MiB ceiling refusing a 136-byte reservation the
/// partial-group build made on a budget the batch had already filled; a
/// sweep in unit time is what keeps that class of knife-edge from reaching
/// the gate again. This corpus's single scan batch is 13 MiB, so below that
/// the query is right to refuse; and at 16 MiB the map spills so often that
/// its run files exceed macOS's default descriptor limit (recorded in
/// docs/limitations.md), so the sweep runs the ceilings above both.
#[test]
fn every_ceiling_between_spilling_and_fitting_aggregates_exactly() {
    let reference = run_aggregated(256 * 1024 * 1024).expect("in-memory aggregation");
    let mut failures = Vec::new();
    let mut limit = 20 * 1024 * 1024;
    while limit <= 32 * 1024 * 1024 {
        match run_aggregated(limit) {
            Ok(rows) if rows == reference => {}
            Ok(_) => failures.push(format!("{limit}: wrong groups")),
            Err(error) => failures.push(format!("{limit}: {error}")),
        }
        limit += 6 * 1024 * 1024;
    }
    assert!(
        failures.is_empty(),
        "ceilings that did not aggregate exactly:\n  {}",
        failures.join("\n  ")
    );
}
