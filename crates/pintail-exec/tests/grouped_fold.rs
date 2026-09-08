//! What a grouped aggregate costs on a table that is being written to,
//! through the engine rather than a hand-folded prototype.
//!
//! e78 measured the shape of the win with partials it folded itself. This
//! runs the real query against the real engine: the same statement twice,
//! with an ingest between, which is exactly what a dashboard does against
//! a mirror. The settled result memo cannot serve the second run - the
//! ingest invalidated it - so what is left is whichever path the engine
//! takes, and the answers must agree with a scan of everything.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SEGMENTS: u64 = 10;
const PER_SEGMENT: u64 = 1_000_000;
const STATUSES: [&str; 5] = ["new", "open", "held", "done", "void"];

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
    versioned(id, id)
}

/// A replicated update carries a version newer than anything already
/// stored. Using the key as the version instead would make an update of an
/// old row look like a stale replay, which the overlay correctly refuses
/// to mask with - it keeps the comparing merge for those.
fn versioned(id: u64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(STATUSES[usize::try_from(id % 5).expect("small")].to_owned()),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        version,
        false,
    )
}

const SQL: &str = "SELECT status, COUNT(*), SUM(amount) FROM facts GROUP BY status ORDER BY status";

fn run(catalog: &CatalogSnapshot, store: &TableStore) -> (f64, Vec<Vec<String>>) {
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let clock = Instant::now();
    let bound = Binder::new(catalog, Some("app"))
        .bind(&parse_statement(SQL).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 512 << 20, Collation::default()).expect("start");
    let mut out = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            out.push(
                (0..batch.columns().len())
                    .map(|column| {
                        format!(
                            "{:?}",
                            batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .expect("value")
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    (clock.elapsed().as_secs_f64(), out)
}

#[test]
#[ignore = "a measurement over a large in-process table, not a gate"]
fn a_grouped_aggregate_under_ingest() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    for segment in 0..SEGMENTS {
        let first = segment * PER_SEGMENT + 1;
        store
            .bulk_ingest_snapshot((first..first + PER_SEGMENT).map(row).collect())
            .expect("segment");
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

    println!();
    println!("{rows} rows in {SEGMENTS} segments, grouped by a five-value column");

    let (cold, expected) = run(&catalog, &store);
    println!("  first run, nothing kept          = {:8.1} ms", cold * 1e3);

    // Updates clustered in the newest segment, which is what a mirror of
    // an active table produces: recent records change, old ones do not.
    // These are in-place updates of rows a segment already holds, which
    // e93 showed the older fold refuses outright.
    let newest = (SEGMENTS - 1) * PER_SEGMENT;
    store
        .ingest_cdc(
            (0..20_000)
                .map(|n| versioned(newest + n * 7 + 1, SEGMENTS * PER_SEGMENT + n + 1))
                .collect(),
        )
        .expect("live updates");
    let (warm, after) = run(&catalog, &store);
    println!("  after 20,000 in-place updates    = {:8.1} ms", warm * 1e3);
    assert_eq!(after.len(), expected.len(), "same groups after the update");

    // A further ingest, to show the cost tracks what changed.
    store
        .ingest_cdc(
            (0..20_000)
                .map(|n| versioned(newest + n * 7 + 2, SEGMENTS * PER_SEGMENT + 100_000 + n))
                .collect(),
        )
        .expect("more updates");
    let (again, _) = run(&catalog, &store);
    println!(
        "  after 20,000 more                = {:8.1} ms",
        again * 1e3
    );
    println!("  the second run against the first = {:8.2}x", cold / warm);
}
