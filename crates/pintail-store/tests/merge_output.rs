//! A scan that has to merge, timed against the same rows with nothing to
//! merge. Ignored: a measurement, not a gate. Run with `cargo test
//! --release -p pintail-store --test merge_output -- --ignored
//! --nocapture`.
//!
//! Both stores are built before either is measured, and the two arms
//! alternate round by round. The first version of this measurement built
//! and timed the merging store, then built and timed the direct one, so
//! the two arms met different allocator and page-cache states and the
//! ratio carried whatever that difference was worth. A number this large
//! should not rest on the order the arms happen to run in.
use std::time::Instant;

use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BASE_ROWS: u64 = 2_000_000;
const CHANGED: u64 = 20_000;
const ROUNDS: usize = 5;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, false),
            Column::new(3, "note", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("status-{}", id % 5)),
            Value::Utf8(format!("a note for row {id} that is not tiny")),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        version,
        false,
    )
}

/// One full projected scan, returning its duration and the rows it saw.
fn timed_scan(snapshot: &TableSnapshot) -> (f64, usize) {
    let low = PrimaryKey::new(vec![KeyPart::UInt64(0)]).expect("low");
    let high = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("high");
    let clock = Instant::now();
    let mut seen = 0;
    if let Some(mut stream) = snapshot
        .scan_projected_range_stream(&low, &high, &[1, 2, 3, 4])
        .expect("scan")
    {
        loop {
            let chunks = stream.next_column_chunks(4, 512 << 20).expect("chunks");
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                seen += chunk.row_count();
            }
        }
    }
    (clock.elapsed().as_secs_f64(), seen)
}

#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn a_merging_scan_reads_every_column() {
    // Both stores first, neither measured yet.
    let merged_dir = tempfile::tempdir().expect("directory");
    let mut merged =
        TableStore::open(merged_dir.path(), schema(), StoreOptions::default()).expect("store");
    merged
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    // A scattered update, then a flush, so the scan meets two overlapping
    // segments and has to merge rather than take the overlay.
    let stride = BASE_ROWS / CHANGED;
    merged
        .bulk_ingest_snapshot((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("tail");

    let plain_dir = tempfile::tempdir().expect("directory");
    let mut plain =
        TableStore::open(plain_dir.path(), schema(), StoreOptions::default()).expect("store");
    plain
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");

    let merging = merged.snapshot();
    let direct = plain.snapshot();

    // One warm round of each, discarded, then the arms alternate so a
    // drift in either direction is shared rather than attributed to one.
    let _ = timed_scan(&merging);
    let _ = timed_scan(&direct);
    let mut merging_best = f64::MAX;
    let mut direct_best = f64::MAX;
    for round in 0..ROUNDS {
        // Alternating which arm goes first inside the round as well, so
        // neither is always the one that runs on a colder cache.
        if round % 2 == 0 {
            let (elapsed, seen) = timed_scan(&merging);
            assert_eq!(seen as u64, BASE_ROWS);
            merging_best = merging_best.min(elapsed);
            let (elapsed, seen) = timed_scan(&direct);
            assert_eq!(seen as u64, BASE_ROWS);
            direct_best = direct_best.min(elapsed);
        } else {
            let (elapsed, seen) = timed_scan(&direct);
            assert_eq!(seen as u64, BASE_ROWS);
            direct_best = direct_best.min(elapsed);
            let (elapsed, seen) = timed_scan(&merging);
            assert_eq!(seen as u64, BASE_ROWS);
            merging_best = merging_best.min(elapsed);
        }
    }

    println!(
        "{BASE_ROWS} rows, four columns, {CHANGED} changed ({:.1}%), \
         {ROUNDS} alternating rounds after a discarded warm-up",
        f64::from(u32::try_from(CHANGED).expect("small")) * 100.0
            / f64::from(u32::try_from(BASE_ROWS).expect("small"))
    );
    println!(
        "  merging scan (two segments) = {:8.1} ms",
        merging_best * 1e3
    );
    println!(
        "  direct scan  (one segment)  = {:8.1} ms",
        direct_best * 1e3
    );
    println!(
        "  the merge costs             = {:8.1}x",
        merging_best / direct_best
    );
}
