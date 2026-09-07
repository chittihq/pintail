//! A scan that has to merge, timed end to end. Ignored: a measurement, not
//! a gate. Run with `cargo test --release -p pintail-store --test
//! merge_output -- --ignored --nocapture`.
use std::time::Instant;

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BASE_ROWS: u64 = 2_000_000;
const CHANGED: u64 = 20_000;

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

#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn a_merging_scan_reads_every_column() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    // A scattered update, then a flush, so the scan meets two overlapping
    // segments and has to merge rather than take the overlay.
    let stride = BASE_ROWS / CHANGED;
    store
        .bulk_ingest_snapshot((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("tail");

    let snapshot = store.snapshot();
    let mut best = f64::MAX;
    let mut rows_seen = 0;
    for _ in 0..5 {
        let clock = Instant::now();
        let low = PrimaryKey::new(vec![KeyPart::UInt64(0)]).expect("low");
        let high = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("high");
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
        rows_seen = seen;
        best = best.min(clock.elapsed().as_secs_f64());
    }
    // The same rows and the same columns with nothing to merge: one
    // segment, so the scan takes the direct path and its packed columns.
    let plain = tempfile::tempdir().expect("directory");
    let mut single =
        TableStore::open(plain.path(), schema(), StoreOptions::default()).expect("store");
    single
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    let flat = single.snapshot();
    let mut direct = f64::MAX;
    for _ in 0..5 {
        let clock = Instant::now();
        let low = PrimaryKey::new(vec![KeyPart::UInt64(0)]).expect("low");
        let high = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("high");
        let mut seen = 0;
        if let Some(mut stream) = flat
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
        assert_eq!(seen as u64, BASE_ROWS);
        direct = direct.min(clock.elapsed().as_secs_f64());
    }

    println!(
        "{BASE_ROWS} rows, four columns, {CHANGED} changed ({:.1}%)",
        f64::from(u32::try_from(CHANGED).expect("small")) * 100.0
            / f64::from(u32::try_from(BASE_ROWS).expect("small"))
    );
    println!("  merging scan (two segments) = {:8.1} ms", best * 1e3);
    println!("  direct scan  (one segment)  = {:8.1} ms", direct * 1e3);
    println!("  the merge costs             = {:8.1}x", best / direct);
    assert_eq!(rows_seen as u64, BASE_ROWS);
}
