//! What a filtered scan pays for its predicate, and whether it pays twice.
//!
//! `#[ignore]`: measurement, not assertion. Run with
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p pintail-exec
//! --test integration filter_once_cost:: -- --ignored --nocapture`.
//!
//! A scan whose predicate columns are a strict subset of its projection
//! decodes the predicate columns first, evaluates the predicates to choose
//! row ranges, and the Filter above used to evaluate them again on what
//! survived. `FILTER_ONCE_PROFILE=1` beside `PINTAIL_PROFILE=1` prints each
//! run's operator profile.
//!
//! What it found, over 2,000,000 rows on a 16-core host, timing the Filter
//! and the prewhere pass directly: the second evaluation is worth something
//! only where the predicate is expensive and the scan's ranges are exact.
//! `name < 'name-002500'` spent 26ms of a 115ms query re-comparing the 5%
//! of rows the scan had already kept; passing exact chunks untested took
//! the median to 94ms (-18%, interleaved in one process). Every other case
//! stayed within noise, for one of three reasons:
//!
//! - A key-range predicate is cheap: the Filter over a million surviving
//!   ids costs 1ms of an 11ms `COUNT(*)`, and 2ms of a 90ms grouped sum
//!   at 95% survival. Marking a segment whose statistics prove every row
//!   passes would save that and no more.
//! - Survivors spread in runs closer together than the scan's range merge
//!   (`v = 7` with `v = id % 1000`) merge into near-full coverage, the scan
//!   declines, and there was only ever one evaluation.
//! - The aggregate dominates: a keyless `SUM(x)` over a scattered 50% or
//!   90% selection spends 180-300ms in the aggregate against 7ms in the
//!   Filter.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 2_000_000;
const RUNS: usize = 9;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "region", DataType::Utf8, false),
            Column::new(3, "x", DataType::Int64, false),
            Column::new(4, "v", DataType::Int64, true),
            Column::new(5, "name", DataType::Utf8, false),
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
                        let v = if id % 17 == 0 {
                            Value::Null
                        } else {
                            Value::Int64(i64::try_from(id % 1_000).expect("small"))
                        };
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Utf8(format!("region-{}", id % 12)),
                                Value::Int64(i64::try_from(id % 997).expect("small")),
                                v,
                                Value::Utf8(format!("name-{:06}", id % 50_000)),
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

    fn run(&self, sql: &str) -> (Vec<Vec<Value>>, f64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value_owned(row).expect("value"))
                        .collect(),
                );
            }
        }
        if std::env::var_os("FILTER_ONCE_PROFILE").is_some()
            && let Some(profile) = execution.profile()
        {
            println!("{}", profile.render());
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        rows.sort_by(|left: &Vec<Value>, right| format!("{left:?}").cmp(&format!("{right:?}")));
        (rows, elapsed)
    }
}

pub const CASES: [(&str, &str); 10] = [
    (
        "count key half",
        "SELECT COUNT(*) FROM events WHERE id >= 1000000",
    ),
    (
        "group key half",
        "SELECT region, SUM(x) FROM events WHERE id >= 1000000 GROUP BY region",
    ),
    (
        "group key 95%",
        "SELECT region, SUM(x) FROM events WHERE id >= 100000 GROUP BY region",
    ),
    (
        "sum key tail 5%",
        "SELECT SUM(x) FROM events WHERE id >= 1900000",
    ),
    ("sum selective", "SELECT SUM(x) FROM events WHERE v = 7"),
    (
        "group selective",
        "SELECT region, SUM(x) FROM events WHERE v = 7 GROUP BY region",
    ),
    (
        "sum 50% scattered",
        "SELECT SUM(x) FROM events WHERE v < 500",
    ),
    (
        "sum non-selective",
        "SELECT SUM(x) FROM events WHERE v < 950",
    ),
    (
        "text equal",
        "SELECT region, COUNT(*) FROM events WHERE name = 'name-000123' GROUP BY region",
    ),
    (
        "text range",
        "SELECT SUM(x) FROM events WHERE name < 'name-002500'",
    ),
];

#[test]
#[ignore = "measurement, not an assertion"]
fn filtered_scan_cost() {
    let fixture = Fixture::new();
    for (label, sql) in CASES {
        let (answer, _) = fixture.run(sql);
        let mut times = (0..RUNS).map(|_| fixture.run(sql).1).collect::<Vec<_>>();
        times.sort_by(f64::total_cmp);
        println!(
            "{label:<20} median {:>8.2}ms  min {:>8.2}ms  rows {}",
            times[RUNS / 2],
            times[0],
            answer.len()
        );
    }
}
