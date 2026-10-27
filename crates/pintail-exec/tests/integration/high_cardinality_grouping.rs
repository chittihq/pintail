//! Grouping where nearly every row is its own group, with several
//! aggregates over nullable integers and datetimes: the shape of a
//! per-pair rollup (one row per visit and visitor) that a report then
//! joins back. Each group is small, so the cost is all per-group
//! bookkeeping.
//!
//! The measurement is `#[ignore]`d:
//! `cargo test --profile recovery -p pintail-exec --test integration high_cardinality_grouping::
//! -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "visit", DataType::UInt64, false),
            Column::new(3, "visitor", DataType::UInt64, false),
            Column::new(4, "present", DataType::Int8, true),
            Column::new(5, "watched", DataType::Int8, true),
            Column::new(6, "entered", DataType::DateTime64 { fsp: 3 }, true),
            Column::new(7, "exited", DataType::DateTime64 { fsp: 3 }, true),
        ],
    )
    .expect("schema")
}

fn seats_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "visit", DataType::UInt64, false),
            Column::new(3, "visitor", DataType::UInt64, false),
        ],
    )
    .expect("seats schema")
}

/// Seats for a handful of visits: every tenth seat names a visitor who
/// never came, so the LEFT JOIN keeps rows that match nothing.
const SEATED_VISITS: u64 = 25;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    seats: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(rows: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        let stored = (0..rows)
            .map(|id| {
                // One visitor in fifty shows up twice for the same visit.
                let pair = id - id / 50;
                let second = id % 60;
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        Value::UInt64(pair / 40),
                        Value::UInt64(7_000_000 + pair % 40 + (pair / 4_000) * 40),
                        if id % 9 == 0 {
                            Value::Null
                        } else {
                            Value::Int64(i64::from(id % 3 == 0))
                        },
                        if id % 11 == 0 {
                            Value::Null
                        } else {
                            Value::Int64(i64::from(id % 2 == 0))
                        },
                        Value::Utf8(format!("2026-03-{:02} 10:{second:02}:00.000", 1 + id % 28)),
                        if id % 13 == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(format!("2026-03-{:02} 11:{second:02}:00.000", 1 + id % 28))
                        },
                    ],
                    id + 1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        table.bulk_ingest_snapshot(stored).expect("rows");
        let seats_directory = directory.path().join("seats");
        let mut seats = TableStore::open(&seats_directory, seats_schema(), StoreOptions::default())
            .expect("seats");
        let seat_rows = (0..SEATED_VISITS * 40)
            .map(|id| {
                let visit = id / 40;
                let visitor = if id % 10 == 9 {
                    1
                } else {
                    7_000_000 + id % 40 + (visit * 40 / 4_000) * 40
                };
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        Value::UInt64(visit),
                        Value::UInt64(visitor),
                    ],
                    id + 1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        seats.bulk_ingest_snapshot(seat_rows).expect("seat rows");
        let seats_entry = TableEntry::new(
            TableId::new(2),
            "seats",
            seats_schema(),
            TableStatistics::with_row_count(SEATED_VISITS * 40),
        )
        .expect("seats entry")
        .with_key_columns([1])
        .expect("seats key");
        let entry = TableEntry::new(
            TableId::new(1),
            "checkins",
            schema(),
            TableStatistics::with_row_count(rows),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        Self {
            _directory: directory,
            table,
            seats,
            catalog: CatalogSnapshot::new([DatabaseEntry::new(
                DatabaseId::new(1),
                "app",
                [entry, seats_entry],
            )
            .expect("database")])
            .expect("catalog"),
        }
    }

    fn run(&self, sql: &str, memory: usize) -> (usize, f64) {
        let (rows, elapsed) = self
            .try_rows(sql, memory)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        (rows.len(), elapsed)
    }

    /// Rows the rollup's aggregate was fed, from a profiled execution: a
    /// profiled node reports what leaves it, filters placed on it included.
    fn aggregate_input_rows(&self, sql: &str) -> u64 {
        let snapshot = self.table.snapshot();
        let seats = self.seats.snapshot();
        let provider = SnapshotScanProvider::new([
            (DatabaseId::new(1), TableId::new(1), &snapshot),
            (DatabaseId::new(1), TableId::new(2), &seats),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(physical, &provider, 1 << 31, None, Collation::default())
                .expect("start");
        while execution.next_batch().expect("batch").is_some() {}
        let profile = execution.profile().expect("a profile");
        profile
            .operators
            .iter()
            .find(|node| node.label.starts_with("Scan app.checkins"))
            .map(|node| node.rows)
            .expect("the rollup scans checkins")
    }

    fn try_rows(&self, sql: &str, memory: usize) -> Result<(Vec<String>, f64), String> {
        let snapshot = self.table.snapshot();
        let seats = self.seats.snapshot();
        let provider = SnapshotScanProvider::new([
            (DatabaseId::new(1), TableId::new(1), &snapshot),
            (DatabaseId::new(1), TableId::new(2), &seats),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let started = std::time::Instant::now();
        let mut execution = Execution::start(physical, &provider, memory, Collation::default())
            .map_err(|error| error.to_string())?;
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
            for row in batch.selection().selected_rows() {
                rows.push(format!(
                    "{:?}",
                    (0..batch.columns().len())
                        .map(|column| batch
                            .column(column)
                            .and_then(|column| column.value(row))
                            .cloned())
                        .collect::<Vec<_>>()
                ));
            }
        }
        rows.sort();
        Ok((rows, started.elapsed().as_secs_f64() * 1000.0))
    }
}

const ROLLUP: &str = "SELECT visit, visitor, MAX(id) AS id, MAX(COALESCE(present, 0)) AS present, \
     MAX(COALESCE(watched, 0)) AS watched, MIN(entered) AS entered, MAX(exited) AS exited \
     FROM checkins GROUP BY visit, visitor";

#[test]
fn the_rollup_answers_one_row_per_pair() {
    let fixture = Fixture::new(5_000);
    let (rows, _) = fixture.run(ROLLUP, 1 << 31);
    let pairs = (0..5_000_u64)
        .map(|id| id - id / 50)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(rows, pairs.len());
}

/// Seats joined to the per-pair rollup, as a report reads it. `$KEY` and
/// `$WHO` are the rollup's keys: plain columns let the seats' keys filter
/// the rollup's input, `visit + 0` and `visitor + 0` are the same answer
/// with nothing to push to.
const SEATED: &str = "SELECT s.id, r.present, r.entered, r.exited FROM seats s \
     LEFT JOIN (SELECT $KEY AS visit, $WHO AS visitor, MAX(COALESCE(present, 0)) AS present, \
     MIN(entered) AS entered, MAX(exited) AS exited FROM checkins GROUP BY $KEY, $WHO) r \
     ON r.visit = s.visit AND r.visitor = s.visitor";

#[test]
fn a_small_join_filters_the_rollup_input_and_keeps_the_answer() {
    let fixture = Fixture::new(500_000);
    let (pushed, _) = fixture
        .try_rows(
            &SEATED.replace("$KEY", "visit").replace("$WHO", "visitor"),
            1 << 31,
        )
        .expect("filtered rollup");
    let (whole, _) = fixture
        .try_rows(
            &SEATED
                .replace("$KEY", "visit + 0")
                .replace("$WHO", "visitor + 0"),
            1 << 31,
        )
        .expect("whole rollup");
    assert_eq!(
        pushed.len(),
        usize::try_from(SEATED_VISITS * 40).expect("seats")
    );
    assert_eq!(pushed, whole);
    assert!(
        pushed.iter().any(|row| row.contains("Null")),
        "seats nobody took stay, unmatched"
    );
    // The seats' keys reach the rollup's input: it is fed only the visits
    // the seats name, where the unfiltered form is fed every row.
    let filtered =
        fixture.aggregate_input_rows(&SEATED.replace("$KEY", "visit").replace("$WHO", "visitor"));
    let unfiltered = fixture.aggregate_input_rows(
        &SEATED
            .replace("$KEY", "visit + 0")
            .replace("$WHO", "visitor + 0"),
    );
    assert!(unfiltered > 400_000, "{unfiltered}");
    assert!(
        filtered <= SEATED_VISITS * 41,
        "{filtered} rows fed to the rollup"
    );
}

#[test]
#[ignore = "measurement, not an assertion"]
fn high_cardinality_rollup_cost() {
    let fixture = Fixture::new(500_000);
    for (label, sql) in [
        ("rollup", ROLLUP),
        (
            "keys only",
            "SELECT visit, visitor, COUNT(*) FROM checkins GROUP BY visit, visitor",
        ),
        (
            "int maxes",
            "SELECT visit, visitor, MAX(id), MAX(COALESCE(present, 0)) FROM checkins GROUP BY visit, visitor",
        ),
        (
            "datetime min/max",
            "SELECT visit, visitor, MIN(entered), MAX(exited) FROM checkins GROUP BY visit, visitor",
        ),
    ] {
        for memory in [1_usize << 31, 64 << 20] {
            let mut timings = (0..3)
                .map(|_| fixture.run(sql, memory).1)
                .collect::<Vec<_>>();
            timings.sort_by(f64::total_cmp);
            println!(
                "[{label:>16}] limit {:>5} MiB  median {:>9.1} ms  min {:>9.1} ms",
                memory >> 20,
                timings[1],
                timings[0]
            );
        }
    }
}
