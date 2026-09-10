//! Whether a query that can run in parallel does.
//!
//! A scan and aggregate over a few segments is decoded by the storage scan
//! pool and folded by the executor's pool, both sized to the machine. This
//! reads what the process actually did - the CPU time its threads accrued,
//! and how many of them accrued any - from `/proc`, so it holds however
//! fast or slow the host is. A regression that serializes a stage - a pool
//! of one, a lock held across the fold, a stage that stopped splitting its
//! input - shows as CPU time no greater than wall time and a single busy
//! thread, whatever the query's latency.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 1_200_000;
const SEGMENT_ROWS: u64 = 150_000;
const STATUSES: [&str; 5] = ["new", "open", "held", "done", "void"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % 97).expect("small")),
            Value::Utf8(STATUSES[usize::try_from(id % 5).expect("small")].to_owned()),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        id,
        false,
    )
}

/// CPU clock ticks each live thread of this process has accrued, user plus
/// system, keyed by thread id. Fields 14 and 15 of a task's stat file; the
/// command name can hold spaces and parentheses, so fields are counted from
/// the last ')'.
fn thread_ticks() -> BTreeMap<u32, u64> {
    let mut ticks = BTreeMap::new();
    for entry in std::fs::read_dir("/proc/self/task").expect("task directory") {
        let entry = entry.expect("task entry");
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let accrued = [11, 12]
            .iter()
            .filter_map(|index| fields.get(*index)?.parse::<u64>().ok())
            .sum();
        ticks.insert(tid, accrued);
    }
    ticks
}

#[test]
fn a_parallel_aggregate_keeps_several_cores_busy() {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if cores < 4 {
        eprintln!("skipped: {cores} cores cannot show parallel work");
        return;
    }
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    let mut next = 1;
    while next <= ROWS {
        let end = (next + SEGMENT_ROWS - 1).min(ROWS);
        store
            .bulk_ingest_snapshot((next..=end).map(row).collect())
            .expect("ingest");
        next = end + 1;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "facts",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let run = |threshold: u64| {
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        // A fresh constant each run, so no remembered answer can stand in
        // for the execution being measured.
        let sql = format!(
            "SELECT grp, status, COUNT(*) AS n, SUM(amount) AS total FROM facts \
             WHERE amount >= {threshold} GROUP BY grp, status"
        );
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(&sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution = Execution::start(
            physical,
            &provider,
            1024 * 1024 * 1024,
            Collation::default(),
        )
        .expect("start");
        let mut groups = 0;
        while let Some(batch) = execution.next_batch().expect("batch") {
            groups += batch.visible_row_count();
        }
        assert_eq!(groups, 97 * 5, "every group answers");
    };

    // Warm the page cache and both pools before measuring.
    run(0);
    let before = thread_ticks();
    let started = Instant::now();
    for threshold in 1..=4 {
        run(threshold);
    }
    let wall = started.elapsed().as_secs_f64();
    let after = thread_ticks();

    let deltas = after
        .iter()
        .map(|(tid, ticks)| ticks.saturating_sub(before.get(tid).copied().unwrap_or(0)))
        .collect::<Vec<_>>();
    // Two ticks is 20 ms of CPU: a thread that did real work, not one that
    // woke to find nothing queued.
    let busy = deltas.iter().filter(|ticks| **ticks >= 2).count();
    #[allow(clippy::cast_precision_loss)] // tick counts are far below 2^52
    let cpu = deltas.iter().sum::<u64>() as f64 / 100.0;
    let parallelism = cpu / wall;
    eprintln!(
        "{cores} cores: {busy} busy threads, {cpu:.2}s CPU over {wall:.2}s wall ({parallelism:.1}x)"
    );
    assert!(
        busy >= 3,
        "a parallel scan and aggregate should occupy several threads; {busy} did work",
    );
    assert!(
        parallelism >= 1.5,
        "CPU time should exceed wall time when the work runs in parallel; \
         {cpu:.2}s CPU over {wall:.2}s wall",
    );
}
