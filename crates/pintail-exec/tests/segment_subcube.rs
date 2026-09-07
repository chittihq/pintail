//! Whether a grouped aggregate could be served from per-segment partial
//! states instead of a scan, and whether that survives continuous ingest
//! the way the settled result memo does not. Ignored: a measurement, not a
//! gate. Run with `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --release
//! -p pintail-exec --test segment_subcube -- --ignored --nocapture`.
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

const STATUSES: [&str; 5] = ["new", "open", "held", "done", "void"];
const SEGMENTS: u64 = 10;
const PER_SEGMENT: u64 = 1_000_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(STATUSES[usize::try_from(id % 5).expect("small")].to_owned()),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        1,
        false,
    )
}

/// The partial state a segment would carry for one group: what COUNT and
/// SUM need to be merged without revisiting a row.
#[derive(Clone, Copy, Default)]
struct Partial {
    count: u64,
    sum: i64,
}

/// The sub-cube one immutable segment would persist beside its data.
fn segment_partials(first: u64, last: u64) -> BTreeMap<&'static str, Partial> {
    let mut cube: BTreeMap<&'static str, Partial> = BTreeMap::new();
    for id in first..=last {
        let entry = cube
            .entry(STATUSES[usize::try_from(id % 5).expect("small")])
            .or_default();
        entry.count += 1;
        entry.sum += i64::try_from(id % 1000).expect("small");
    }
    cube
}

#[test]
#[ignore = "a measurement over a large in-process table, not a gate"]
fn a_grouped_aggregate_from_per_segment_partials() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store = TableStore::open(directory.path(), schema(), StoreOptions::default())
        .expect("store");
    let mut cubes = Vec::new();
    for segment in 0..SEGMENTS {
        let first = segment * PER_SEGMENT + 1;
        let last = first + PER_SEGMENT - 1;
        store
            .bulk_ingest_snapshot((first..=last).map(row).collect())
            .expect("ingest");
        cubes.push(segment_partials(first, last));
    }
    let rows = SEGMENTS * PER_SEGMENT;

    let entry = TableEntry::new(
        TableId::new(1),
        "facts",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");

    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let sql = "SELECT status, COUNT(*), SUM(amount) FROM facts GROUP BY status";
    let run_scan = || {
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let clock = Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 512 << 20, Collation::default())
                .expect("execution");
        let mut produced = 0;
        while let Some(batch) = execution.next_batch().expect("pull") {
            produced += batch.visible_row_count();
        }
        (produced, clock.elapsed().as_secs_f64())
    };

    let mut scan_best = f64::MAX;
    let mut groups = 0;
    for _ in 0..5 {
        let (produced, elapsed) = run_scan();
        groups = produced;
        scan_best = scan_best.min(elapsed);
    }

    // What serving the same answer from the segments' own partials costs.
    let mut merge_best = f64::MAX;
    let mut merged_groups = 0;
    for _ in 0..5 {
        let clock = Instant::now();
        let mut merged: BTreeMap<&'static str, Partial> = BTreeMap::new();
        for cube in &cubes {
            for (group, partial) in cube {
                let entry = merged.entry(group).or_default();
                entry.count += partial.count;
                entry.sum += partial.sum;
            }
        }
        merged_groups = merged.len();
        merge_best = merge_best.min(clock.elapsed().as_secs_f64());
    }

    // Continuous ingest is the case the settled result memo cannot serve:
    // one flush adds a segment's partials, and the rows still in the
    // memtable are the only ones that need looking at.
    const MEMTABLE_ROWS: u64 = 50_000;
    let mut live_best = f64::MAX;
    for _ in 0..5 {
        let clock = Instant::now();
        let mut merged: BTreeMap<&'static str, Partial> = BTreeMap::new();
        for cube in &cubes {
            for (group, partial) in cube {
                let entry = merged.entry(group).or_default();
                entry.count += partial.count;
                entry.sum += partial.sum;
            }
        }
        for id in rows + 1..=rows + MEMTABLE_ROWS {
            let entry = merged
                .entry(STATUSES[usize::try_from(id % 5).expect("small")])
                .or_default();
            entry.count += 1;
            entry.sum += i64::try_from(id % 1000).expect("small");
        }
        live_best = live_best.min(clock.elapsed().as_secs_f64());
    }

    println!("{rows} rows in {SEGMENTS} segments, {groups} groups, minimum of 5 runs");
    println!("  scan and aggregate            = {:9.3} ms", scan_best * 1e3);
    println!(
        "  merge {SEGMENTS} segments' partials    = {:9.3} ms  ({merged_groups} groups)",
        merge_best * 1e3
    );
    println!(
        "  merge + {MEMTABLE_ROWS} live memtable rows = {:9.3} ms",
        live_best * 1e3
    );
    println!(
        "  ratio settled                 = {:9.0}x",
        scan_best / merge_best
    );
    println!(
        "  ratio under ingest            = {:9.0}x",
        scan_best / live_best
    );
    assert_eq!(groups, merged_groups);
}
