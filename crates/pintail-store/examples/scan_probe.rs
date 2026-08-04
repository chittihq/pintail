//! In-engine scan probe: what do the lab's encoding results become once
//! they run through PTSEG's real writer and reader?
//!
//! The lab experiments (e20–e22) decode a serialized buffer into a flat
//! `Vec`. The engine decodes an exact-length bitstream through a 16-byte
//! window, merges nulls, walks visibility, and appends into typed columnar
//! builders. This measures the whole of that, on real segment files, so a
//! microbenchmark win can be checked against what a query actually pays.
//!
//! ```text
//! cargo run --release --example scan_probe -- --rows 20000000
//! ```
//!
//! Reports on-disk segment bytes and scan latency for a first (cold-ish)
//! scan after reopen and for repeated warm scans. The page cache stays
//! warm from writing unless the host is purged between phases, so the
//! "cold" figure is a lower bound on real cold-scan cost, not an upper one.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::Path;
use std::time::{Duration, Instant};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BATCH: usize = 64 * 1024;
const STATUSES: [&str; 5] = ["pending", "paid", "shipped", "delivered", "refunded"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::UInt64, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "day", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// Mirrors benchmark/seed.sql's shape: a cyclic status that defeats zone
/// maps, a wide-domain user id, and a uniform amount.
fn row(id: u64) -> StoredRow {
    let mixed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(mixed % 200_000),
            Value::Utf8(STATUSES[(id % 5) as usize].to_owned()),
            Value::Int64(100 + (mixed % 999_900) as i64),
            Value::Int64(19_000 + (mixed % 1_095) as i64),
        ],
        0,
        false,
    )
}

fn directory_bytes(directory: &Path) -> u64 {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "ptseg")
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

/// Two ways to drain the same scan. `next_column_chunk` stops at decoded
/// columns, which is what a vectorized operator consumes; `next_chunk`
/// additionally transposes them into per-row `Vec<Value>`. Measuring both
/// separates decode cost from row materialization, and only the first is
/// what an encoding change can move.
fn scan(table: &TableStore, columns: &[u32], rows_too: bool) -> (Duration, usize) {
    let snapshot = table.snapshot();
    let start = PrimaryKey::new(vec![KeyPart::UInt64(u64::MIN)]).expect("start");
    let end = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("end");
    let began = Instant::now();
    let mut seen = 0;
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, columns)
        .expect("stream");
    if let Some(stream) = stream.as_mut() {
        if rows_too {
            while let Some(chunk) = stream.next_chunk(256 * 1024 * 1024).expect("chunk") {
                seen += chunk.rows().len();
            }
        } else {
            while let Some(chunk) = stream.next_column_chunk(256 * 1024 * 1024).expect("chunk") {
                seen += chunk.columns().first().map_or(0, |column| column.len());
            }
        }
    }
    (began.elapsed(), seen)
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

fn main() {
    let rows: u64 = std::env::args()
        .skip(1)
        .scan(String::new(), |previous, argument| {
            let value = (previous == "--rows")
                .then(|| argument.parse().ok())
                .flatten();
            *previous = argument;
            Some(value)
        })
        .flatten()
        .next()
        .unwrap_or(20_000_000);

    let directory = tempfile::tempdir().expect("tempdir");
    let options = StoreOptions {
        wal_sync: WalSync::Off,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");

    let began = Instant::now();
    let mut batch = Vec::with_capacity(BATCH);
    for id in 1..=rows {
        batch.push(row(id));
        if batch.len() == BATCH {
            table.ingest(std::mem::take(&mut batch)).expect("ingest");
            batch.reserve(BATCH);
        }
    }
    if !batch.is_empty() {
        table.ingest(batch).expect("ingest tail");
    }
    table.flush().expect("flush");
    table.checkpoint().expect("checkpoint");
    let load = began.elapsed();

    let segment_bytes = directory_bytes(directory.path());
    let metrics = table.metrics().expect("metrics");
    println!("scan probe: {rows} rows");
    println!(
        "load          : {:.1}s  ({:.0} rows/s)",
        load.as_secs_f64(),
        rows as f64 / load.as_secs_f64()
    );
    println!(
        "segments      : {} files, {segment_bytes} B ({:.2} B/row)",
        metrics.segment_count(),
        segment_bytes as f64 / rows as f64,
    );

    // Reopen so nothing of the writer's in-memory state serves the read.
    drop(table);
    let table = TableStore::open(directory.path(), schema(), options).expect("reopen");

    for (label, columns) in [
        ("amount only        ", &[4_u32][..]),
        ("amount + day       ", &[4, 5][..]),
        ("status only (dict) ", &[3][..]),
        ("all five columns   ", &[1, 2, 3, 4, 5][..]),
    ] {
        let (first, scanned) = scan(&table, columns, false);
        let columnar = median((0..3).map(|_| scan(&table, columns, false).0).collect());
        let with_rows = median((0..3).map(|_| scan(&table, columns, true).0).collect());
        println!(
            "{label}: first {:>8.1} ms   columns {:>8.1} ms   +rows {:>8.1} ms   {scanned} values",
            first.as_secs_f64() * 1e3,
            columnar.as_secs_f64() * 1e3,
            with_rows.as_secs_f64() * 1e3,
        );
    }
}
