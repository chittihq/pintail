//! A projection of computed expressions under an aggregate is evaluated on
//! the workers of a fused round, beside the scan that decoded each batch,
//! instead of on the query's own thread while the scan is pulled.
//!
//! Each case runs twice: as written, and with one more aggregate over
//! `RAND()`, whose argument keeps the whole projection on the query's
//! thread. The extra aggregate answers TRUE for every group, so the other
//! columns are the pulled path's answers to the same expressions, and the
//! counter says which path each took. Warnings are compared the same way:
//! a batch that raises one is left to the query's thread whole, so the
//! count is the pulled path's.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters, take_session_division_warnings,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS_PER_SEGMENT: u64 = 400;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "flag", DataType::Utf8, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(
                4,
                "price",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
            Column::new(
                5,
                "rate",
                DataType::Decimal {
                    precision: 4,
                    scale: 2,
                },
                false,
            ),
            Column::new(6, "qty", DataType::Int64, false),
            Column::new(7, "divisor", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    zero_divisors: u64,
}

impl Fixture {
    fn new() -> Self {
        // Several slices a thread, so a round has batches on every worker.
        let segments = u64::try_from(rayon::current_num_threads().max(1) * 6).expect("small");
        let directory = tempfile::tempdir().expect("directory");
        let options = StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
        let mut zero_divisors = 0;
        for segment in 0..segments {
            let rows = (segment * ROWS_PER_SEGMENT..(segment + 1) * ROWS_PER_SEGMENT)
                .map(|id| {
                    // Only the second segment divides by zero, so most
                    // batches raise nothing and one raises warnings.
                    let divisor = if segment == 1 && id % 10 == 0 {
                        zero_divisors += 1;
                        0
                    } else {
                        i64::try_from(id % 7 + 1).expect("small")
                    };
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(
                                ["A", "N", "R"][usize::try_from(id % 3).expect("small")].into(),
                            ),
                            Value::Utf8(["F", "O"][usize::try_from(id % 2).expect("small")].into()),
                            Value::Utf8(format!("{}.{:02}", id % 9_973, id % 100)),
                            Value::Utf8(format!("0.{:02}", id % 11)),
                            Value::Int64(i64::try_from(id % 50).expect("small") + 1),
                            Value::Int64(divisor),
                        ],
                        1,
                        false,
                    )
                })
                .collect();
            table.ingest(rows).expect("ingest");
            table.flush().expect("flush");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "items",
            schema(),
            TableStatistics::with_row_count(segments * ROWS_PER_SEGMENT),
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
            zero_divisors,
        }
    }

    /// The rendered rows in the order the query gives them, its profile, the batches
    /// projected on workers, and the divisions by zero reported.
    fn run(&self, sql: &str) -> (Vec<Vec<String>>, String, pintail_exec::ExecCounters, u64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let _ = take_exec_counters();
        let _ = take_session_division_warnings();
        let mut execution =
            Execution::start_profiled(physical, &provider, 512 << 20, None, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            format!(
                                "{:?}",
                                batch
                                    .column(column)
                                    .and_then(|column| column.value(row))
                                    .cloned()
                                    .expect("selected value")
                            )
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        let profile = execution
            .profile()
            .map(|profile| profile.render())
            .unwrap_or_default();
        drop(execution);
        (
            rows,
            profile,
            take_exec_counters(),
            take_session_division_warnings(),
        )
    }
}

/// Grouped and ungrouped aggregates over computed arguments, each with the
/// select list the pulled reference appends an aggregate to.
const CASES: [(&str, &str); 4] = [
    (
        "SELECT flag, status, SUM(price), SUM(price * (1 - rate)), SUM(price * (1 - rate) * (1 + rate)), AVG(qty * 2), COUNT(*)",
        "FROM items GROUP BY flag, status ORDER BY flag, status",
    ),
    (
        "SELECT SUM(price * rate), SUM(qty * 3 + 1)",
        "FROM items WHERE qty < 40",
    ),
    (
        "SELECT flag, status, SUM(qty / divisor), COUNT(*)",
        "FROM items GROUP BY flag, status ORDER BY flag, status",
    ),
    ("SELECT SUM(qty / divisor), MAX(price - rate)", "FROM items"),
];

#[test]
fn computed_projections_fold_on_workers_with_the_pulled_answers() {
    if std::env::var_os("PINTAIL_DISABLE_FUSED_FOLD").is_some() {
        return;
    }
    let fixture = Fixture::new();
    for (select, rest) in CASES {
        let fused = format!("{select} {rest}");
        let pulled = format!("{select}, MAX(RAND() < 2) {rest}");
        let (fused_rows, profile, fused_counters, fused_warnings) = fixture.run(&fused);
        let (pulled_rows, _, pulled_counters, pulled_warnings) = fixture.run(&pulled);
        assert!(!fused_rows.is_empty(), "{fused}");
        let pulled_rows = pulled_rows
            .into_iter()
            .map(|mut row| {
                assert_eq!(row.pop().as_deref(), Some("Boolean(true)"), "{pulled}");
                row
            })
            .collect::<Vec<_>>();
        assert_eq!(fused_rows, pulled_rows, "{fused}");
        assert!(
            fused_counters.projected_batches_on_workers > 0,
            "{fused}: nothing was projected on a worker\n{fused_counters:?}\n{profile}"
        );
        assert_eq!(
            pulled_counters.projected_batches_on_workers, 0,
            "{pulled}: RAND() was projected on a worker"
        );
        assert_eq!(fused_warnings, pulled_warnings, "{fused}");
        if select.contains("divisor") {
            assert_eq!(fused_warnings, fixture.zero_divisors, "{fused}");
        }
    }
}
