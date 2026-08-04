//! Storage scale probe.
//!
//! Loads rows into a real store and reports how segment count, manifest size,
//! on-disk bytes, and scan latency grow with the table. A run of a few million
//! rows finishes in minutes and shows the shape of the curve, which is what
//! terabyte-scale claims have to be extrapolated from.
//!
//! ```text
//! cargo run --release --example scale_probe -- --rows 2000000 --mode append
//! cargo run --release --example scale_probe -- --rows 2000000 --mode churn
//! ```
//!
//! `append` models CDC over an auto-increment primary key: every batch covers
//! a strictly higher key range. `churn` rewrites earlier keys, which is what
//! an update-heavy source table looks like.

// Measurement code: ratios and rates are reported as approximate f64.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::Path;
use std::time::{Duration, Instant};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BATCH_ROWS: usize = 4_096;
const CHECKPOINTS: u64 = 10;

struct Args {
    rows: u64,
    memtable_mb: usize,
    churn: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        rows: 1_000_000,
        memtable_mb: 64,
        churn: false,
    };
    let mut flags = std::env::args().skip(1);
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--rows" => {
                args.rows = flags
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.rows)
            }
            "--memtable-mb" => {
                args.memtable_mb = flags
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.memtable_mb);
            }
            "--mode" => args.churn = flags.next().as_deref() == Some("churn"),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "customer_id", DataType::UInt64, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "note", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

const STATUSES: [&str; 4] = ["pending", "shipped", "delivered", "refunded"];

fn row(id: u64) -> StoredRow {
    // Deterministic spread without a random dependency in the hot loop.
    let mixed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(mixed % 5_000_000),
            Value::Utf8(
                STATUSES[usize::try_from(mixed >> 32).unwrap_or(0) % STATUSES.len()].to_owned(),
            ),
            Value::Int64(i64::try_from(mixed % 100_000).unwrap_or(0)),
            Value::Utf8(format!("order line {id} note text")),
        ],
        0,
        false,
    )
}

fn directory_bytes(directory: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
        .sum()
}

fn manifest_bytes(directory: &Path) -> u64 {
    std::fs::metadata(directory.join("manifest.ptm")).map_or(0, |meta| meta.len())
}

fn report(directory: &Path, table: &TableStore, rows: u64, elapsed: Duration) {
    let metrics = table.metrics().expect("metrics");
    let disk = directory_bytes(directory);
    println!(
        "{:>12} {:>10} {:>14} {:>12} {:>12} {:>11.0}",
        rows,
        metrics.segment_count(),
        manifest_bytes(directory),
        disk,
        disk.checked_div(rows.max(1)).unwrap_or(0),
        rows as f64 / elapsed.as_secs_f64(),
    );
}

fn time_scan(table: &TableStore, columns: &[u32]) -> (Duration, usize) {
    let snapshot = table.snapshot();
    let start = PrimaryKey::new(vec![KeyPart::UInt64(u64::MIN)]).expect("start");
    let end = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("end");
    let began = Instant::now();
    let mut rows = 0;
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, columns)
        .expect("stream");
    if let Some(stream) = stream.as_mut() {
        while let Some(chunk) = stream.next_chunk(256 * 1024 * 1024).expect("chunk") {
            rows += chunk.rows().len();
        }
    }
    (began.elapsed(), rows)
}

fn main() {
    let args = parse_args();
    let directory = tempfile::tempdir().expect("tempdir");
    let options = StoreOptions {
        memtable_bytes: args.memtable_mb * 1024 * 1024,
        wal_sync: WalSync::Checkpoint,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");

    println!(
        "scale probe: rows={} memtable={}MB mode={}",
        args.rows,
        args.memtable_mb,
        if args.churn { "churn" } else { "append" }
    );
    println!(
        "{:>12} {:>10} {:>14} {:>12} {:>12} {:>11}",
        "rows", "segments", "manifest_b", "disk_b", "b/row", "rows/s"
    );

    let began = Instant::now();
    let checkpoint_every = (args.rows / CHECKPOINTS).max(1);
    let mut next_checkpoint = checkpoint_every;
    let mut written = 0u64;
    let mut batch = Vec::with_capacity(BATCH_ROWS);
    for index in 0..args.rows {
        // Churn rewrites a key from the first half of what already exists,
        // producing overlapping segments and tombstone-free updates.
        let key = if args.churn && index % 2 == 1 && written > 2 {
            1 + (index.wrapping_mul(0x2545_F491_4F6C_DD1D) % (written / 2).max(1))
        } else {
            index + 1
        };
        batch.push(row(key));
        if batch.len() == BATCH_ROWS {
            table
                .ingest_cdc(std::mem::take(&mut batch))
                .expect("ingest");
            batch.reserve(BATCH_ROWS);
        }
        written = index + 1;
        if written >= next_checkpoint {
            if !batch.is_empty() {
                table
                    .ingest_cdc(std::mem::take(&mut batch))
                    .expect("ingest tail");
                batch.reserve(BATCH_ROWS);
            }
            report(directory.path(), &table, written, began.elapsed());
            next_checkpoint += checkpoint_every;
        }
    }
    if !batch.is_empty() {
        table.ingest_cdc(batch).expect("ingest final");
    }
    table.checkpoint().expect("checkpoint");
    report(directory.path(), &table, args.rows, began.elapsed());

    let ingest = began.elapsed();
    let (wide, wide_rows) = time_scan(&table, &[1, 2, 3, 4, 5]);
    let (narrow, _) = time_scan(&table, &[1, 4]);

    println!();
    println!("ingest        : {:.1}s", ingest.as_secs_f64());
    println!(
        "scan 5 cols   : {:.3}s over {wide_rows} rows",
        wide.as_secs_f64()
    );
    println!("scan 2 cols   : {:.3}s", narrow.as_secs_f64());
    let metrics = table.metrics().expect("metrics");
    println!(
        "segments      : {} ({:.0} rows/segment)",
        metrics.segment_count(),
        args.rows as f64 / metrics.segment_count().max(1) as f64
    );
    println!(
        "extrapolated  : {:.0} segments and {:.1}MB manifest at 1e9 rows",
        metrics.segment_count() as f64 * (1e9 / args.rows as f64),
        manifest_bytes(directory.path()) as f64 * (1e9 / args.rows as f64) / 1e6,
    );
}
