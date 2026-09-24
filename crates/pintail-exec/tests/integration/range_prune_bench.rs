//! In-process timing of a range filter on a timestamp column, over a table
//! that has taken updates: a base loaded in key order, then a newer segment
//! of changed rows lying across all of it - the shape a replicated table
//! keeps while its source is written to. Ignored: a measurement, not a
//! gate. Run with
//! `cargo test --release -p pintail-exec --test range_prune_bench -- --ignored --nocapture`.
//! `PINTAIL_BENCH_ROWS` overrides the table size.
use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const CHUNK: u64 = 1_000_000;
/// Seconds between consecutive rows' timestamps: 20 million rows span about
/// fifteen months.
const STEP_SECONDS: u64 = 2;
const STATES: [&str; 4] = ["queued", "sent", "delivered", "failed"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "created_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(3, "state", DataType::Utf8, false),
            Column::new(4, "carrier", DataType::Int64, false),
            Column::new(5, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// Civil date from days since 1970-01-01.
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// 2024-01-01 00:00:00 plus `seconds`, as the column's text.
fn timestamp(seconds: u64) -> String {
    let total = 1_704_067_200 + seconds;
    let (year, month, day) = civil(total / 86_400);
    let within = total % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        within / 3600,
        within / 60 % 60,
        within % 60
    )
}

fn row(id: u64, version: u64, state: usize) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(timestamp(id * STEP_SECONDS)),
            Value::Utf8(STATES[state].to_owned()),
            Value::Int64(i64::try_from(id % 40).expect("small")),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        version,
        false,
    )
}

fn run(catalog: &CatalogSnapshot, store: &TableStore, sql: &str) -> (usize, Duration) {
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let clock = Instant::now();
    let bound = Binder::new(catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = Execution::start(
        physical,
        &provider,
        4 * 1024 * 1024 * 1024,
        Collation::default(),
    )
    .expect("start");
    let mut rows = 0;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.visible_row_count();
    }
    (rows, clock.elapsed())
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn a_timestamp_range_over_an_updated_table() {
    let rows = std::env::var("PINTAIL_BENCH_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000_000_u64);
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut store = TableStore::open(directory.path(), schema(), options).expect("store");
    let load = Instant::now();
    let mut next = 1;
    while next <= rows {
        let end = (next + CHUNK - 1).min(rows);
        store
            .bulk_ingest_snapshot((next..=end).map(|id| row(id, 1, 0)).collect())
            .expect("ingest");
        next = end + 1;
    }
    // One row in a hundred changes state, across the whole key range.
    let mut id = 1;
    while id <= rows {
        let end = (id + CHUNK * 100).min(rows + 1);
        store
            .ingest((id..end).step_by(100).map(|id| row(id, 2, 2)).collect())
            .expect("updates");
        id = end;
    }
    store.flush().expect("flush");
    eprintln!(
        "{rows} rows loaded and updated in {:.1}s",
        load.elapsed().as_secs_f64()
    );

    let entry = TableEntry::new(
        TableId::new(1),
        "reports",
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

    let span_days = rows * STEP_SECONDS / 86_400;
    let shapes: [(&str, u64, &str); 3] = [
        (
            "one day, count and sum",
            1,
            "SELECT COUNT(*) AS n, SUM(amount) AS total FROM reports \
             WHERE created_at >= '{from}' AND created_at < '{to}'",
        ),
        (
            "one day, latest fifty rows",
            1,
            "SELECT id, state, amount FROM reports \
             WHERE created_at >= '{from}' AND created_at < '{to}' \
             ORDER BY created_at DESC LIMIT 50",
        ),
        (
            "one week, per state",
            7,
            "SELECT state, COUNT(*) AS n FROM reports \
             WHERE created_at >= '{from}' AND created_at < '{to}' GROUP BY state",
        ),
    ];
    for (name, days, template) in shapes {
        let mut timings = Vec::new();
        for run_index in 0..6_u64 {
            // A different window every run, so nothing remembered answers.
            let start_day = (run_index * 37 + 11) % span_days.saturating_sub(days).max(1);
            let sql = template
                .replace("{from}", &timestamp(start_day * 86_400))
                .replace("{to}", &timestamp((start_day + days) * 86_400));
            let (_, elapsed) = run(&catalog, &store, &sql);
            if run_index > 0 {
                timings.push(elapsed);
            }
        }
        timings.sort();
        eprintln!(
            "{name:<28} min {:>8.1} ms  median {:>8.1} ms",
            timings[0].as_secs_f64() * 1e3,
            timings[timings.len() / 2].as_secs_f64() * 1e3
        );
    }
}
