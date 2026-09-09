//! Reproducible projected and filter-first scans over invented immutable data.
//! Run with a persistent directory and optional row count (default 524288).
//! Reuse that directory with baseline and candidate binaries; timings are warm
//! page-cache measurements, with output values checked outside the timed loop.
#![allow(clippy::cast_precision_loss)]
use pintail_store::{DecodedColumn, StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use std::{hint::black_box, path::PathBuf, time::Instant};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        (1..=24)
            .map(|id| {
                Column::new(
                    id,
                    format!("field_{id}"),
                    if id == 24 {
                        DataType::Utf8
                    } else {
                        DataType::UInt64
                    },
                    false,
                )
            })
            .collect(),
    )
    .expect("schema")
}
fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}
fn main() {
    let mut args = std::env::args().skip(1);
    let directory = PathBuf::from(args.next().expect("data directory"));
    let rows: u64 = args.next().map_or(524_288, |v| v.parse().expect("rows"));
    let mut table = TableStore::open(
        &directory,
        schema(),
        StoreOptions {
            wal_sync: WalSync::Off,
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("open");
    if table.snapshot().key_bounds().is_none() {
        let data = (0..rows)
            .map(|id| {
                StoredRow::new(
                    key(id),
                    (1_u32..=24)
                        .map(|col| {
                            if col == 24 {
                                Value::Utf8(format!("label-{}", id % 8))
                            } else {
                                Value::UInt64(id * u64::from(col) + 7)
                            }
                        })
                        .collect(),
                    0,
                    false,
                )
            })
            .collect();
        table.bulk_ingest_snapshot(data).expect("seed");
    }
    let snapshot = table.snapshot();
    let (first, last) = snapshot.key_bounds().expect("bounds");
    for (label, columns, predicate, selective) in [
        ("narrow-last", vec![23], vec![], false),
        ("wide", (1..=24).collect(), vec![], false),
        ("text-all", vec![24], vec![24], false),
        ("text-selective", vec![24], vec![24], true),
        ("mixed-selective", vec![23, 24], vec![24], true),
    ] {
        let mut timings = Vec::new();
        let mut decoded = 0;
        for round in 0..9 {
            let began = Instant::now();
            let mut stream = snapshot
                .scan_projected_range_stream(&first, &last, &columns)
                .expect("scan")
                .expect("stream");
            let select = |_: &[DecodedColumn],
                          count: usize|
             -> Result<Option<Vec<std::ops::Range<usize>>>, String> {
                Ok(selective.then(|| {
                    (0..count)
                        .step_by(4096)
                        .map(|start| start..(start + 256).min(count))
                        .collect()
                }))
            };
            let mut count = 0;
            let mut blocks = 0;
            loop {
                let chunks = if predicate.is_empty() {
                    stream.next_column_chunks(1, 256 * 1024 * 1024)
                } else {
                    stream.next_column_chunks_filtered(1, 256 * 1024 * 1024, &predicate, &select)
                }
                .expect("chunks");
                if chunks.is_empty() {
                    break;
                }
                for chunk in chunks {
                    count += chunk.row_count();
                    blocks += chunk.stats().blocks_decoded();
                    black_box(chunk.columns());
                }
            }
            let elapsed = began.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(count as u64, if selective { rows / 16 } else { rows });
            decoded = blocks;
            if round > 1 {
                timings.push(elapsed);
            }
        }
        timings.sort_by(f64::total_cmp);
        println!(
            "{label}: median_ms={:.3} min_ms={:.3} blocks_decoded={decoded}",
            timings[timings.len() / 2],
            timings[0]
        );
    }
}
