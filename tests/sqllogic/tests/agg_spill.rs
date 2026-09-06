//! High-cardinality GROUP BY must produce identical results whether the
//! group map fits in memory or spills to sorted on-disk runs: same groups,
//! same aggregate values, with the spill engaging only under a memory
//! ceiling that previously failed the query outright.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::spill::{MERGE_FAN_IN, QuerySpillMetrics};
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
            Column::new(6, "note", DataType::Utf8, false),
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
            // Two spellings per group that the default collation folds to
            // one distinct value each, so the answer is 1 or 2 per group.
            Value::Utf8(if id % 4 < 2 {
                "Note-A".to_owned()
            } else {
                "note-b".to_owned()
            }),
        ],
        id,
        false,
    )
}

fn run_aggregated(memory_limit: usize) -> Result<Vec<Vec<Value>>, String> {
    run_aggregated_with_metrics(memory_limit).map(|(rows, _)| rows)
}

fn run_aggregated_with_metrics(
    memory_limit: usize,
) -> Result<(Vec<Vec<Value>>, QuerySpillMetrics), String> {
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
        // COUNT(DISTINCT note) is the text set that repeats within a group
        // across many runs: a merge that re-normalized its keys counted it
        // once per run.
        "SELECT grp, tag, COUNT(*), SUM(score), AVG(score), MIN(label), \
         COUNT(DISTINCT score), COUNT(DISTINCT label), COUNT(DISTINCT note) \
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
    // Read after the last batch: an operator that has produced its final
    // row must already have let go of its files.
    let metrics = execution.spill_metrics();
    Ok((rows, metrics))
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

/// Every ceiling between "spills" and "fits" must produce the same groups,
/// and hold descriptors bounded by the merge fan-in however many runs it
/// spilled. The gate once caught a 5 MiB ceiling refusing a 136-byte
/// reservation the partial-group build made on a budget the batch had
/// already filled; a sweep in unit time is what keeps that class of
/// knife-edge from reaching the gate again. Below 12 MiB the query still
/// refuses (the scan's ready batches fill the ceiling before the first
/// group lands), so the sweep starts there. The 12 MiB ceiling spills the
/// map dozens of times - it was hundreds before the aggregate ran its
/// rounds in budget-sized waves, and it used to exhaust macOS's default
/// descriptor limit and was skipped for that reason, which left the suite
/// encoding the defect as expected. Now it is the ceiling that proves the
/// bound: one writer while building, fan-in plus one while a pass merges,
/// nothing once the last row is out, at run counts that fall by at least
/// half across the sweep.
#[test]
fn every_ceiling_between_spilling_and_fitting_aggregates_exactly() {
    let reference = run_aggregated(256 * 1024 * 1024).expect("in-memory aggregation");
    let bound = u64::try_from(MERGE_FAN_IN + 1).expect("small");
    let mut failures = Vec::new();
    let mut run_counts = Vec::new();
    let mut limit = 12 * 1024 * 1024;
    while limit <= 30 * 1024 * 1024 {
        match run_aggregated_with_metrics(limit) {
            Ok((rows, metrics)) if rows == reference => {
                if metrics.peak_handles > bound {
                    failures.push(format!(
                        "{limit}: peak {} open spill files exceeds fan-in + 1 = {bound} \
                         (created {})",
                        metrics.peak_handles, metrics.files
                    ));
                }
                if metrics.active_handles != 0 || metrics.active_bytes != 0 {
                    failures.push(format!(
                        "{limit}: {} files and {} bytes still held after the last row",
                        metrics.active_handles, metrics.active_bytes
                    ));
                }
                run_counts.push(metrics.files);
            }
            Ok(_) => failures.push(format!("{limit}: wrong groups")),
            Err(error) => failures.push(format!("{limit}: {error}")),
        }
        limit += 6 * 1024 * 1024;
    }
    assert!(
        failures.is_empty(),
        "ceilings that did not aggregate exactly within the descriptor bound:\n  {}",
        failures.join("\n  ")
    );
    let (first, last) = (run_counts[0], run_counts[run_counts.len() - 1]);
    assert!(
        first > bound && first >= last.saturating_mul(2),
        "the sweep must span very different run counts, got {run_counts:?}"
    );
}

/// Set in the child process that runs under a lowered descriptor limit.
const DESCRIPTOR_PROBE: &str = "PINTAIL_DESCRIPTOR_PROBE";
const DESCRIPTOR_LIMIT: u64 = 48;

/// Handle counters can miss a descriptor something else holds; the kernel
/// cannot. A fresh child process lowers its own soft `RLIMIT_NOFILE` to 48
/// and runs the ceiling that spills more runs than that. The limit is
/// process-wide and shared by every thread, so it is never lowered in the
/// test process itself, where restoring it would not undo interference with
/// the tests running alongside.
#[test]
fn a_spilling_aggregation_completes_under_a_low_descriptor_limit() {
    const NAME: &str = "a_spilling_aggregation_completes_under_a_low_descriptor_limit";
    if std::env::var_os(DESCRIPTOR_PROBE).is_some() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let current = getrlimit(Resource::Nofile);
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(DESCRIPTOR_LIMIT),
                maximum: current.maximum,
            },
        )
        .expect("lower the soft descriptor limit of this process");
        let reference = run_aggregated(256 * 1024 * 1024).expect("in-memory aggregation");
        let (spilled, metrics) =
            run_aggregated_with_metrics(12 * 1024 * 1024).expect("spilled aggregation");
        assert_eq!(spilled, reference);
        assert!(
            metrics.files > DESCRIPTOR_LIMIT,
            "the case must create more runs than the process may hold open, created {}",
            metrics.files
        );
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(DESCRIPTOR_PROBE, "1")
        .output()
        .expect("spawn the probe child");
    assert!(
        output.status.success(),
        "probe child failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
