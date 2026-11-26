//! Memtable copy-on-write probe.
//!
//! Fills a memtable to about `--fill-mb` of estimated row bytes (below the
//! flush threshold), then times change-stream batches applied three ways:
//! with no snapshot held, with a fresh snapshot pinned across every batch
//! (the worst case: each batch is the first write after a pin), and with
//! one snapshot pinned across all of them (only the first write after the
//! pin pays for the copy).
//!
//! ```text
//! cargo run --release --example memtable_cow_probe -- --fill-mb 60 --batch 64
//! ```

// Measurement code: ratios and rates are reported as approximate f64.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

struct Args {
    fill_mb: usize,
    batch: usize,
    batches: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        fill_mb: 60,
        batch: 64,
        batches: 200,
    };
    let mut flags = std::env::args().skip(1);
    while let Some(flag) = flags.next() {
        let value = flags.next().and_then(|v| v.parse().ok());
        match flag.as_str() {
            "--fill-mb" => args.fill_mb = value.unwrap_or(args.fill_mb),
            "--batch" => args.batch = value.unwrap_or(args.batch),
            "--batches" => args.batches = value.unwrap_or(args.batches),
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
            Column::new(2, "account_id", DataType::UInt64, false),
            Column::new(3, "state", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "note", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

const STATES: [&str; 4] = ["open", "held", "closed", "void"];

fn row(id: u64, version: u64) -> StoredRow {
    let mixed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(mixed % 5_000_000),
            Value::Utf8(
                STATES[usize::try_from(mixed >> 32).unwrap_or(0) % STATES.len()].to_owned(),
            ),
            Value::Int64(i64::try_from(mixed % 100_000).unwrap_or(0)),
            Value::Utf8(format!("line {id} note text")),
        ],
        version,
        false,
    )
}

#[derive(Clone, Copy)]
enum Mode {
    Unpinned,
    PinEach,
    PinOnce,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Unpinned => "unpinned",
            Self::PinEach => "pin-each",
            Self::PinOnce => "pin-once",
        }
    }
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index]
}

fn main() {
    let args = parse_args();
    let directory = tempfile::tempdir().expect("tempdir");
    let options = StoreOptions {
        // Never flush during the probe: the fill stays in the memtable.
        memtable_bytes: usize::MAX,
        wal_sync: WalSync::Checkpoint,
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    let target = args.fill_mb * 1024 * 1024;
    let mut next_id = 1u64;
    let mut version = 1u64;
    let fill_began = Instant::now();
    while table.metrics().expect("metrics").memtable_bytes() < target {
        let rows = (next_id..next_id + 4096)
            .map(|id| row(id, version))
            .collect();
        table.ingest_cdc(rows).expect("fill");
        next_id += 4096;
        version += 1;
    }
    let fill_ms = fill_began.elapsed().as_secs_f64() * 1e3;
    let filled = next_id - 1;
    println!(
        "memtable cow probe: rows={filled} estimated_bytes={} fill_ms={fill_ms:.1} batch={} batches={}",
        table.metrics().expect("metrics").memtable_bytes(),
        args.batch,
        args.batches
    );
    if args.batches == 0 {
        // The fill alone, for counting what an unpinned ingest costs.
        return;
    }
    println!(
        "{:>9} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "mode", "median_us", "p90_us", "max_us", "min_us", "total_ms"
    );
    // Updates spread over the whole key range, as a stream of changes to
    // existing rows does.
    let mut cursor = 0u64;
    for mode in [Mode::Unpinned, Mode::PinEach, Mode::PinOnce, Mode::Unpinned] {
        let mut times = Vec::with_capacity(args.batches);
        let held_once = matches!(mode, Mode::PinOnce).then(|| table.snapshot());
        for _ in 0..args.batches {
            let rows = (0..args.batch)
                .map(|_| {
                    cursor = cursor.wrapping_add(0x2545_F491_4F6C_DD1D);
                    row(1 + cursor % filled, version)
                })
                .collect::<Vec<_>>();
            let held = matches!(mode, Mode::PinEach).then(|| table.snapshot());
            let began = Instant::now();
            table.ingest_cdc(rows).expect("batch");
            times.push(began.elapsed());
            // Dropped outside the timed span: the copy is the write's cost.
            drop(held);
            version += 1;
        }
        drop(held_once);
        let total: Duration = times.iter().sum();
        times.sort_unstable();
        println!(
            "{:>9} {:>11} {:>11} {:>11} {:>11} {:>11.1}",
            mode.name(),
            percentile(&times, 0.5).as_micros(),
            percentile(&times, 0.9).as_micros(),
            times.last().expect("times").as_micros(),
            times[0].as_micros(),
            total.as_secs_f64() * 1e3,
        );
    }
}
