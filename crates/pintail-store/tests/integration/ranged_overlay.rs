//! A ranged overlay scan against the same range with nothing to mask.
//!
//! e92 measured the overlay over a WHOLE table at 1.0-2.5x a direct scan.
//! A grouped fold reads one segment's key span at a time, and a span the
//! memtable touches measured forty times a clean one. This asks whether
//! the range is what costs, in the store alone, with no aggregate above it.
use std::time::Instant;

use pintail_store::{StoreOptions, TableSnapshot, TableStore};
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

fn timed(
    snapshot: &TableSnapshot,
    lo: u64,
    hi: u64,
    projection: &[u32],
    overlay: bool,
) -> (f64, usize) {
    let low = PrimaryKey::new(vec![KeyPart::UInt64(lo)]).expect("low");
    let high = PrimaryKey::new(vec![KeyPart::UInt64(hi)]).expect("high");
    let clock = Instant::now();
    let mut seen = 0;
    if let Some(mut stream) = snapshot
        .scan_projected_range_stream(&low, &high, projection)
        .expect("scan")
    {
        if overlay {
            stream.enable_memtable_overlay(&[1]);
        }
        loop {
            let chunks = stream
                .next_column_chunks(projection.len(), 512 << 20)
                .expect("chunks");
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
fn a_ranged_overlay_against_a_clean_range() {
    let dirty_dir = tempfile::tempdir().expect("directory");
    let mut dirty =
        TableStore::open(dirty_dir.path(), schema(), StoreOptions::default()).expect("store");
    for segment in 0..SEGMENTS {
        let first = segment * PER_SEGMENT + 1;
        dirty
            .bulk_ingest_snapshot((first..first + PER_SEGMENT).map(row).collect())
            .expect("segment");
    }
    let newest = (SEGMENTS - 1) * PER_SEGMENT;
    dirty
        .ingest_cdc(
            (0..20_000)
                .map(|n| versioned(newest + n * 7 + 1, SEGMENTS * PER_SEGMENT + n + 1))
                .collect(),
        )
        .expect("live updates");

    let clean_dir = tempfile::tempdir().expect("directory");
    let mut clean =
        TableStore::open(clean_dir.path(), schema(), StoreOptions::default()).expect("store");
    for segment in 0..SEGMENTS {
        let first = segment * PER_SEGMENT + 1;
        clean
            .bulk_ingest_snapshot((first..first + PER_SEGMENT).map(row).collect())
            .expect("segment");
    }

    let with_live = dirty.snapshot();
    let without = clean.snapshot();
    let (lo, hi) = (newest + 1, SEGMENTS * PER_SEGMENT);

    println!();
    println!("one segment's span ({PER_SEGMENT} rows), 20,000 of them superseded in the memtable");
    for (label, projection) in [
        ("key projected (id, status, amount)", &[1_u32, 2, 3][..]),
        ("key NOT projected (status, amount)", &[2, 3][..]),
    ] {
        let _ = timed(&with_live, lo, hi, projection, true);
        let _ = timed(&without, lo, hi, projection, true);
        let mut d = f64::MAX;
        let mut c = f64::MAX;
        for _ in 0..3 {
            d = d.min(timed(&with_live, lo, hi, projection, true).0);
            c = c.min(timed(&without, lo, hi, projection, true).0);
        }
        println!(
            "  {label:>36}: dirty {:7.1} ms, clean {:7.1} ms, {:5.1}x",
            d * 1e3,
            c * 1e3,
            d / c
        );
    }
    // And the same span with the overlay never asked for, which is what the
    // scan falls back to when it cannot mask.
    let mut merged = f64::MAX;
    for _ in 0..3 {
        merged = merged.min(timed(&with_live, lo, hi, &[1, 2, 3], false).0);
    }
    println!(
        "  {:>36}: {:7.1} ms",
        "overlay not enabled (merge path)",
        merged * 1e3
    );
}
