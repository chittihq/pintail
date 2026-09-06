//! The streaming two-pass aggregate must answer exactly whether its group
//! maps fit in memory or spill to sorted runs, on each key source it
//! takes: an integer key, an interned text key, and date parts.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::spill::QuerySpillMetrics;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);
const ROWS: u64 = 240_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "member", DataType::UInt64, false),
            Column::new(3, "task", DataType::Int64, false),
            Column::new(4, "score", DataType::Int64, false),
            Column::new(5, "tag", DataType::Utf8, false),
            Column::new(6, "done_at", DataType::DateTime64 { fsp: 0 }, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    // Sixty thousand members with a few tasks each, repeated so distinct
    // counts differ from plain counts; forty thousand tags; dates across
    // two years.
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(id % 60_000),
            Value::Int64(i64::try_from((id * 7) % 11).expect("small")),
            Value::Int64(i64::try_from(id % 1_000).expect("small") - 500),
            Value::Utf8(format!("tag-{:05}", (id * 13) % 40_000)),
            Value::Utf8(format!(
                "202{}-{:02}-{:02} 10:00:00",
                5 + id % 2,
                1 + id % 12,
                1 + id % 28
            )),
        ],
        id,
        false,
    )
}

fn run(sql: &str, memory_limit: usize) -> Result<(Vec<String>, QuerySpillMetrics), String> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    // Ten segments rather than one: a scan retains the segments it has
    // prefetched, and one segment of every row would fill a tight ceiling
    // by itself before the aggregate held a single group.
    for segment in 0..10 {
        let start = segment * (ROWS / 10) + 1;
        table
            .bulk_ingest_snapshot((start..start + ROWS / 10).map(row).collect())
            .expect("ingest");
    }
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
    let statement = parse_statement(sql).map_err(|error| format!("parse: {error}"))?;
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
            let values: Vec<_> = batch
                .columns()
                .iter()
                .map(|column| column.value(index).expect("value"))
                .collect();
            rows.push(format!("{values:?}"));
        }
    }
    rows.sort();
    Ok((rows, execution.spill_metrics()))
}

/// Set in the child that runs with the settled memo off, so every ceiling
/// executes rather than replaying the first run.
const CHILD: &str = "PINTAIL_TWO_PASS_SPILL_CHILD";

const SHAPES: &[(&str, &str)] = &[
    (
        "integer key with a distinct count",
        "SELECT member, COUNT(*), COUNT(DISTINCT task), SUM(score), MAX(score) FROM events \
         GROUP BY member",
    ),
    (
        "interned text key",
        "SELECT tag, COUNT(*), SUM(score), MIN(score) FROM events GROUP BY tag",
    ),
    (
        "date parts",
        "SELECT YEAR(done_at), MONTH(done_at), COUNT(*), SUM(score) FROM events \
         GROUP BY YEAR(done_at), MONTH(done_at)",
    ),
];

#[test]
fn a_spilled_two_pass_aggregation_matches_the_in_memory_groups_exactly() {
    const NAME: &str = "a_spilled_two_pass_aggregation_matches_the_in_memory_groups_exactly";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("PINTAIL_DISABLE_SETTLED_MEMO", "1")
            .output()
            .expect("spawn the child");
        assert!(
            output.status.success(),
            "child failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut failures = Vec::new();
    let mut spilled = false;
    for (name, sql) in SHAPES {
        let (reference, metrics) = run(sql, 256 * 1024 * 1024).expect("in-memory aggregation");
        assert!(reference.len() > 1, "{name}: the shape must group");
        assert_eq!(metrics.files, 0, "{name}: the reference must not spill");
        for limit in [24 * 1024 * 1024, 32 * 1024 * 1024] {
            match run(sql, limit) {
                Ok((rows, metrics)) => {
                    if rows != reference {
                        failures.push(format!("{name} at {limit}: wrong groups"));
                    }
                    if metrics.files > 0 {
                        spilled = true;
                    }
                    if metrics.peak_handles > 17 || metrics.active_handles != 0 {
                        failures.push(format!(
                            "{name} at {limit}: held {} files open, {} after the last row",
                            metrics.peak_handles, metrics.active_handles
                        ));
                    }
                }
                Err(error) => failures.push(format!("{name} at {limit}: {error}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "two-pass shapes that did not hold:\n  {}",
        failures.join("\n  ")
    );
    assert!(spilled, "no shape spilled at the tight ceilings");
}
