//! Which projections still fall to the row path, and what that costs.
//!
//! `#[ignore]`: measurement, not assertion. Run with
//! `cargo test --profile recovery -p pintail-exec --test integration scalar_fallback_cost::
//! -- --ignored --nocapture`.
//!
//! A projection whose expression has no batch kernel builds a `Value` per
//! row per column and hands back a value-backed column. `rows_projected_scalar`
//! counts exactly those rows, and unlike `values_materialized` it is
//! incremented on the thread that pulls, so it is readable here.
//!
//! The point is to find where the row path is still reached rather than to
//! assume it. A column reading zero took a kernel; a column reading the row
//! count did not, and its milliseconds are what a kernel there would be
//! worth.
//!
//! What it found, over 200,000 rows: of twenty-nine shapes, twenty-eight
//! take a kernel and one does not. Arithmetic, comparison, decimals, every
//! text function tried, date parts, `DATE_FORMAT`, `DATE_ADD`, `CASE`,
//! `COALESCE`, casts, nesting, mixed text and number, `REGEXP`, `HEX`,
//! `MD5`, `LPAD`, `FIELD` and `JSON_OBJECT` are all vectorized. The
//! exception is `JSON_EXTRACT`, which projects all 200,000 rows through the
//! row path and is the slowest projection here at 63ms.
//!
//! So "retire `Value` from the fallback" is not the work it sounds like:
//! the fallback is nearly unreachable from a projection already, and what
//! is left of it is one function rather than a path. Three more -
//! `EXPORT_SET`, `WEIGHT_STRING`, `SOUNDEX` - do not bind at all, which is
//! a gap of a different kind and not a row-path cost.
//!
//! Nor is the one exception work to do. `JSON_EXTRACT` over a column of
//! documents costs 95ms, and wrapped in `JSON_UNQUOTE` 119ms, because every
//! row holds a different document and each is parsed once. A kernel cannot
//! remove a parse per row, only move it: `parses_a_document` in
//! `expression/vector/functions.rs` records the measurement that settled
//! this already, 123ms adapted against 75ms read row by row. The row path
//! is the faster option here, and the numbers below agree with the ones
//! that put it there.
//!
//! The constant-document case is the odd one, at 64ms to parse the same
//! small document 200,000 times. Folding it would take that to nothing,
//! but `JSON_EXTRACT('{...}', '$.a')` with both arguments constant is a
//! shape queries do not really have, so it is noted rather than chased.
//!
//! The other thing visible here is that the expensive kernels are the text
//! ones: `CONCAT` 47ms, `MD5` 56ms, `LPAD` 52ms, `FIELD` 69ms, against 4ms
//! for a bare column and 5ms for nested integer arithmetic. Text is where
//! projection time goes, exactly as it is where join and grouping time
//! goes.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider, take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 200_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, false),
            Column::new(
                3,
                "price",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
            Column::new(4, "name", DataType::Utf8, false),
            Column::new(5, "seen", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(6, "meta", DataType::Json, false),
        ],
    )
    .expect("schema")
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        table
            .bulk_ingest_snapshot(
                (0..ROWS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Int64(i64::try_from(id % 1_000).expect("small")),
                                Value::Utf8(format!("{}.{:02}", id % 10_000, id % 100)),
                                Value::Utf8(format!("name-{:06}", id % 50_000)),
                                Value::Utf8(format!(
                                    "2026-{:02}-{:02} 10:00:00",
                                    id % 12 + 1,
                                    id % 28 + 1
                                )),
                                Value::Utf8(format!(
                                    "{{\"score\":{},\"tag\":\"t{}\"}}",
                                    id % 100,
                                    id % 7
                                )),
                            ],
                            id + 1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema(),
            TableStatistics::with_row_count(ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog"),
        }
    }

    /// Rows out, rows projected through the row path, and elapsed time.
    fn measure(&self, sql: &str) -> Option<(usize, u64, f64)> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        // An expression this version cannot bind is not a row-path case; it
        // is a gap, and saying which is the point of the measurement.
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .ok()?;
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .ok()?;
        let _ = take_exec_counters();
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = 0;
        loop {
            match execution.next_batch() {
                Ok(Some(batch)) => rows += batch.selection().selected_rows().count(),
                Ok(None) => break,
                Err(_) => return None,
            }
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        Some((rows, take_exec_counters().rows_projected_scalar, elapsed))
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn which_projections_still_reach_the_row_path() {
    let fixture = Fixture::new();
    let cases = [
        ("bare column", "SELECT amount FROM events"),
        ("integer arithmetic", "SELECT amount + 1 FROM events"),
        ("integer comparison", "SELECT amount > 5 FROM events"),
        ("decimal arithmetic", "SELECT price + 1 FROM events"),
        ("decimal rounding", "SELECT ROUND(price, 1) FROM events"),
        ("text upper", "SELECT UPPER(name) FROM events"),
        ("text concat", "SELECT CONCAT(name, 'x') FROM events"),
        ("text substring", "SELECT SUBSTRING(name, 2, 4) FROM events"),
        ("text length", "SELECT LENGTH(name) FROM events"),
        ("text replace", "SELECT REPLACE(name, 'a', 'b') FROM events"),
        ("date part", "SELECT YEAR(seen) FROM events"),
        (
            "date format",
            "SELECT DATE_FORMAT(seen, '%Y-%m') FROM events",
        ),
        (
            "date add",
            "SELECT DATE_ADD(seen, INTERVAL 1 DAY) FROM events",
        ),
        (
            "case",
            "SELECT CASE WHEN amount > 5 THEN 1 ELSE 0 END FROM events",
        ),
        ("coalesce", "SELECT COALESCE(name, 'x') FROM events"),
        ("cast to char", "SELECT CAST(amount AS CHAR) FROM events"),
        (
            "cast to decimal",
            "SELECT CAST(amount AS DECIMAL(12,2)) FROM events",
        ),
        (
            "nested arithmetic",
            "SELECT (amount + 1) * 2 - 3 FROM events",
        ),
        (
            "mixed text and number",
            "SELECT CONCAT(name, amount) FROM events",
        ),
        // The exotic end: if the row path is reachable at all, it is here.
        ("regexp", "SELECT name REGEXP '^n' FROM events"),
        // A constant document and a column one are different questions: the
        // first is one parse repeated, the second is a parse per row, and
        // only the second is the shape a query actually has.
        (
            "json extract, constant doc",
            "SELECT JSON_EXTRACT('{\"a\":1}', '$.a') FROM events",
        ),
        (
            "json extract, column doc",
            "SELECT JSON_EXTRACT(meta, '$.score') FROM events",
        ),
        (
            "json unquote, column doc",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(meta, '$.tag')) FROM events",
        ),
        ("json object", "SELECT JSON_OBJECT('k', amount) FROM events"),
        ("hex", "SELECT HEX(name) FROM events"),
        ("md5", "SELECT MD5(name) FROM events"),
        ("lpad", "SELECT LPAD(name, 20, '-') FROM events"),
        ("field", "SELECT FIELD(name, 'a', 'b') FROM events"),
        (
            "export_set",
            "SELECT EXPORT_SET(amount, 'y', 'n') FROM events",
        ),
        ("weight_string", "SELECT WEIGHT_STRING(name) FROM events"),
        ("soundex", "SELECT SOUNDEX(name) FROM events"),
    ];
    println!(
        "\n{:<26} {:>10} {:>14} {:>9}  path",
        "expression", "rows", "row-path rows", "ms"
    );
    let mut fallbacks: Vec<(&str, f64)> = Vec::new();
    for (name, sql) in cases {
        let Some((rows, scalar, ms)) = fixture.measure(sql) else {
            println!(
                "{name:<26} {dash:>10} {dash:>14} {dash:>9}  unsupported",
                dash = "-"
            );
            continue;
        };
        let path = if scalar == 0 { "kernel" } else { "ROW PATH" };
        if scalar > 0 {
            fallbacks.push((name, ms));
        }
        println!("{name:<26} {rows:>10} {scalar:>14} {ms:>9.1}  {path}");
    }
    fallbacks.sort_by(|left, right| right.1.total_cmp(&left.1));
    println!("\nrow path, slowest first:");
    for (name, ms) in fallbacks {
        println!("  {name:<26} {ms:>8.1}ms");
    }
    println!();
}
