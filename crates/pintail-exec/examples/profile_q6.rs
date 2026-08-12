//! Local profiling harness for the benchmark's heavy aggregate queries.
//!
//! Phase-zero instrument for the Q6 gap (RESULTS.md, Codex review 2026-08-02):
//! builds the benchmark's orders table from seed.sql's exact formulas as real
//! PTSEG segments on local disk, then runs the production SQL through the
//! full planner + executor stack so the wall time can be attributed with a
//! sampling profiler.
//!
//! Usage:
//!   cargo run --release --example `profile_q6` `[q3|q5|q6|q7]` `[rows]`
//!   `PROFILE_LOOP_SECONDS=20` cargo run --release --example `profile_q6` q6
//!     (loops the query so `sample <pid>` can attach)
//!
//! The data directory (`target/profile_q6_data`/<rows>) persists across runs;
//! delete it to force a re-ingest.

use pintail_exec::collation::Collation;
use std::time::Instant;

use pintail_catalog::TableStatistics;
use pintail_catalog::{CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId};
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SEGMENT_ROWS: u64 = 100_000;
const STATUSES: [&str; 5] = ["pending", "processing", "shipped", "delivered", "cancelled"];
const REGIONS: [&str; 8] = [
    "north",
    "south",
    "east",
    "west",
    "central",
    "northeast",
    "southeast",
    "northwest",
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::UInt32, false),
            Column::new(
                3,
                "total_amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
            Column::new(4, "status", DataType::Utf8, false),
            Column::new(5, "region", DataType::Utf8, false),
            Column::new(6, "order_date", DataType::Date32, false),
        ],
    )
    .expect("orders schema")
}

/// seed.sql's exact deterministic formulas for `generated_id` = `id`.
fn row(id: u64) -> StoredRow {
    let user_id = 1 + (id * 17) % 100_000;
    let quantity = 1 + id % 20;
    let unit_price_cents = 1_000 + (id * 7919) % 99_000;
    let total_cents = quantity * unit_price_cents;
    let date_days = (id * 7) % 1825;
    // 2020-01-01 is day 18_262 since the epoch.
    let epoch_days = 18_262 + i64::try_from(date_days).expect("day offset");
    let date = chrono::NaiveDate::from_num_days_from_ce_opt(
        i32::try_from(epoch_days).expect("date fits i32") + 719_163,
    )
    .expect("valid order date");
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("primary key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(user_id),
            Value::Utf8(format!("{}.{:02}", total_cents / 100, total_cents % 100)),
            Value::Utf8(STATUSES[usize::try_from(id % 5).expect("status index")].to_owned()),
            Value::Utf8(REGIONS[usize::try_from(id % 8).expect("region index")].to_owned()),
            Value::Utf8(date.format("%Y-%m-%d").to_string()),
        ],
        id,
        false,
    )
}

const USER_REGIONS: [&str; 8] = [
    "us-east", "us-west", "eu-west", "eu-east", "ap-south", "ap-east", "sa-east", "af-south",
];

fn users_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "region", DataType::Utf8, false),
        ],
    )
    .expect("users schema")
}

/// seed.sql's users formulas for n = `id` (`1..=100_000`).
fn user_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("primary key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(USER_REGIONS[usize::try_from(id % 8).expect("region index")].to_owned()),
        ],
        id,
        false,
    )
}

fn query_sql(name: &str) -> &'static str {
    match name {
        "q1" => "SELECT COUNT(*) AS total_orders FROM orders",
        "q2" | "n1" => "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'delivered'",
        "q4" => {
            "SELECT region, status, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total \
             FROM orders GROUP BY region, status ORDER BY total DESC, region, status LIMIT 20"
        }
        "q3" => {
            "SELECT status, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt \
             FROM orders GROUP BY status ORDER BY cnt DESC"
        }
        "q5" => {
            "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
             ROUND(SUM(total_amount), 2) AS revenue FROM orders \
             WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' \
             GROUP BY yr, mo ORDER BY yr, mo"
        }
        "q6" => {
            "SELECT user_id, COUNT(*) AS order_count, ROUND(SUM(total_amount), 2) AS total_spent \
             FROM orders GROUP BY user_id ORDER BY total_spent DESC, user_id LIMIT 10"
        }
        "q8" => {
            "SELECT u.region, COUNT(*) AS cnt, ROUND(SUM(o.total_amount), 2) AS total \
             FROM orders o JOIN users u ON o.user_id = u.id \
             GROUP BY u.region ORDER BY total DESC"
        }
        "q7" => {
            "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
             ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
             ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
             FROM orders WHERE order_date BETWEEN '2022-01-01' AND '2023-12-31' \
             GROUP BY region ORDER BY total DESC"
        }
        "n2" => {
            "SELECT region, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt \
             FROM orders GROUP BY region ORDER BY cnt DESC"
        }
        "n3" => {
            "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
             ROUND(SUM(total_amount), 2) AS revenue FROM orders \
             WHERE order_date >= '2022-01-01' AND order_date < '2023-01-01' \
             GROUP BY yr, mo ORDER BY yr, mo"
        }
        "n4" => {
            "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
             ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
             ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
             FROM orders WHERE order_date BETWEEN '2021-01-01' AND '2022-12-31' \
             GROUP BY region ORDER BY total DESC"
        }
        other => panic!("unknown query {other}: expected q1..q8 or n1..n4"),
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    let mut args = std::env::args().skip(1);
    let query = args.next().unwrap_or_else(|| "q6".to_owned());
    let rows: u64 = args
        .next()
        .map_or(20_000_000, |rows| rows.parse().expect("row count"));
    let sql = query_sql(&query);

    let data_dir = std::path::PathBuf::from(format!("target/profile_q6_data/{rows}"));
    let fresh = !data_dir.exists()
        || std::fs::read_dir(&data_dir).map_or(true, |mut entries| entries.next().is_none());
    std::fs::create_dir_all(&data_dir).expect("data directory");
    let mut table =
        TableStore::open(&data_dir, schema(), StoreOptions::default()).expect("open table");
    if fresh {
        let started = Instant::now();
        let mut id = 1_u64;
        while id <= rows {
            let end = (id + SEGMENT_ROWS).min(rows + 1);
            table
                .bulk_ingest_snapshot((id..end).map(row).collect())
                .expect("bulk segment");
            id = end;
        }
        println!(
            "ingested {rows} rows in {:.1}s ({} segments)",
            started.elapsed().as_secs_f64(),
            rows.div_ceil(SEGMENT_ROWS)
        );
    } else {
        println!("reusing existing data at {}", data_dir.display());
    }

    let users_dir = data_dir.join("users");
    let users_fresh = !users_dir.exists();
    std::fs::create_dir_all(&users_dir).expect("users directory");
    let mut users_table = TableStore::open(&users_dir, users_schema(), StoreOptions::default())
        .expect("open users table");
    if users_fresh {
        for start in (1_u64..=100_000).step_by(25_000) {
            users_table
                .bulk_ingest_snapshot((start..start + 25_000).map(user_row).collect())
                .expect("users segment");
        }
    }
    let users_snapshot = users_table.snapshot();
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let users_id = TableId::new(2);
    let entry = TableEntry::new(
        table_id,
        "orders",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("table entry");
    let users_entry = TableEntry::new(
        users_id,
        "users",
        users_schema(),
        TableStatistics::with_row_count(100_000),
    )
    .expect("users entry");
    let database =
        DatabaseEntry::new(database_id, "app", [entry, users_entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, table_id, &snapshot),
        (database_id, users_id, &users_snapshot),
    ])
    .expect("provider");

    let run = || {
        let statement = parse_statement(sql).expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let mut execution = Execution::start(
            physical,
            &provider,
            4 * 1024 * 1024 * 1024,
            Collation::default(),
        )
        .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().expect("cell"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    };

    println!("query {query}: {sql}");
    let loop_seconds: u64 = std::env::var("PROFILE_LOOP_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(0);
    if loop_seconds > 0 {
        println!(
            "pid {} looping for {loop_seconds}s — attach `sample` now",
            std::process::id()
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(loop_seconds);
        let mut iterations = 0_u32;
        while Instant::now() < deadline {
            let started = Instant::now();
            let result = run();
            iterations += 1;
            println!(
                "  iteration {iterations}: {:.0} ms, {} rows",
                started.elapsed().as_secs_f64() * 1e3,
                result.len()
            );
        }
        return;
    }
    if std::env::var_os("PROFILE_PRINT_ROWS").is_some() {
        for row in run() {
            println!(
                "ROW {}",
                row.iter()
                    .map(|value| format!("{value:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            );
        }
        return;
    }
    for iteration in 1..=3 {
        let started = Instant::now();
        let result = run();
        println!(
            "iteration {iteration}: {:.0} ms, {} rows, first={:?}",
            started.elapsed().as_secs_f64() * 1e3,
            result.len(),
            result.first().map(|row| row
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join(", "))
        );
    }
}
