//! Aggregates over a computed argument - `SUM(CASE ...)`, `SUM(a * b)`.
//!
//! The fast aggregation paths take a bare column as an argument and nothing
//! else; a computed one sent the whole aggregation to the general path, one
//! row at a time on one thread. The argument is now projected a batch at a
//! time first. `answers_match_the_derived_table_form` pins the answers to
//! the form a user would write to get a column: the same expression
//! projected by a derived table.
//!
//! The measurement is `#[ignore]`d. Run it twice, once with
//! `PINTAIL_DISABLE_ARGUMENT_PROJECTION=1`, and compare both the timings
//! and the digests (which must agree):
//! `cargo test --profile recovery -p pintail-exec --test
//! integration computed_aggregate_arguments:: -- --ignored --nocapture`.

use std::hash::{Hash, Hasher};

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
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(
                3,
                "price",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(4, "qty", DataType::Int64, false),
            Column::new(5, "name", DataType::Utf8, false),
            Column::new(6, "ratio", DataType::Float64, false),
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
    fn new(rows: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        table
            .bulk_ingest_snapshot(
                (0..rows)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Int64(i64::try_from(id % 1_000).expect("small")),
                                if id % 17 == 0 {
                                    Value::Null
                                } else {
                                    Value::Utf8(format!("{}.{:02}", id % 997, id % 100))
                                },
                                Value::Int64(i64::try_from(id % 13).expect("small") - 3),
                                Value::Utf8(format!("name-{:05}", id % 20_000)),
                                Value::float64(
                                    f64::from(u32::try_from(id % 1_000).expect("small")) / 8.0,
                                ),
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
            "orders",
            schema(),
            TableStatistics::with_row_count(rows),
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

    /// The answer as sorted rendered rows, and how long it took.
    fn run(&self, sql: &str) -> (Vec<Vec<String>>, f64) {
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
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 1 << 31, Collation::default()).expect("start");
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
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        rows.sort();
        (rows, elapsed)
    }
}

/// Each case as it is written, beside the derived-table form that hands the
/// aggregate a column.
const CASES: [(&str, &str); 7] = [
    (
        "SELECT grp, SUM(CASE WHEN qty > 2 THEN price ELSE 0 END) FROM orders GROUP BY grp",
        "SELECT grp, SUM(x) FROM (SELECT grp, CASE WHEN qty > 2 THEN price ELSE 0 END AS x FROM orders) t GROUP BY grp",
    ),
    (
        "SELECT grp, SUM(price * qty), COUNT(CASE WHEN qty < 0 THEN 1 END) FROM orders GROUP BY grp",
        "SELECT grp, SUM(x), COUNT(y) FROM (SELECT grp, price * qty AS x, CASE WHEN qty < 0 THEN 1 END AS y FROM orders) t GROUP BY grp",
    ),
    (
        "SELECT grp, AVG(price + 1), MIN(qty * 2), MAX(ratio * 3) FROM orders GROUP BY grp",
        "SELECT grp, AVG(x), MIN(y), MAX(z) FROM (SELECT grp, price + 1 AS x, qty * 2 AS y, ratio * 3 AS z FROM orders) t GROUP BY grp",
    ),
    (
        "SELECT SUM(qty * 3), AVG(price * 2), COUNT(price + 1) FROM orders",
        "SELECT SUM(x), AVG(y), COUNT(z) FROM (SELECT qty * 3 AS x, price * 2 AS y, price + 1 AS z FROM orders) t",
    ),
    (
        "SELECT name, SUM(qty * 2) FROM orders GROUP BY name",
        "SELECT name, SUM(x) FROM (SELECT name, qty * 2 AS x FROM orders) t GROUP BY name",
    ),
    (
        "SELECT grp, SUM(ratio * qty), SUM(qty) FROM orders WHERE id % 3 = 0 GROUP BY grp",
        "SELECT grp, SUM(x), SUM(qty) FROM (SELECT grp, ratio * qty AS x, qty FROM orders WHERE id % 3 = 0) t GROUP BY grp",
    ),
    (
        "SELECT grp, SUM(qty / 0) FROM orders GROUP BY grp",
        "SELECT grp, SUM(x) FROM (SELECT grp, qty / 0 AS x FROM orders) t GROUP BY grp",
    ),
];

#[test]
fn answers_match_the_derived_table_form() {
    let fixture = Fixture::new(30_000);
    for (written, derived) in CASES {
        assert_eq!(fixture.run(written).0, fixture.run(derived).0, "{written}");
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn computed_argument_aggregation_cost() {
    let fixture = Fixture::new(2_000_000);
    let mode = if std::env::var_os("PINTAIL_DISABLE_ARGUMENT_PROJECTION").is_some() {
        "row path"
    } else {
        "projected"
    };
    for (written, _) in CASES {
        let (rows, _) = fixture.run(written);
        let mut timings = (0..5).map(|_| fixture.run(written).1).collect::<Vec<_>>();
        timings.sort_by(f64::total_cmp);
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        rows.hash(&mut digest);
        println!(
            "[{mode}] median {:>8.1} ms  min {:>8.1} ms  digest {:016x}  {written}",
            timings[2],
            timings[0],
            digest.finish()
        );
    }
}

/// A CASE whose narrower branch is an integer keeps that branch's own
/// label (`0`, as `MySQL` renders it) beside its exact units, so the column
/// the projection builds holds values rather than packed decimal units. The
/// grouped decimal lanes read only packed units and answered every such row
/// as NULL: `SUM` and `MAX` over the column came back NULL for every group.
/// The expected sums are computed here from the fixture's own rows.
#[test]
fn a_case_with_an_integer_branch_sums_under_grouping() {
    const ROWS: u64 = 30_000;
    let fixture = Fixture::new(ROWS);
    let mut expected = std::collections::BTreeMap::<i64, i128>::new();
    for id in 0..ROWS {
        let grp = i64::try_from(id % 1_000).expect("small");
        let qty = i64::try_from(id % 13).expect("small") - 3;
        let units = if qty > 2 && id % 17 != 0 {
            i128::from(id % 997) * 100 + i128::from(id % 100)
        } else {
            0
        };
        *expected.entry(grp).or_default() += units;
    }
    let mut expected = expected
        .into_iter()
        .map(|(grp, units)| {
            vec![
                format!("{:?}", Value::Int64(grp)),
                format!(
                    "{:?}",
                    Value::Utf8(format!("{}.{:02}", units / 100, units % 100))
                ),
            ]
        })
        .collect::<Vec<_>>();
    expected.sort();
    for sql in [
        "SELECT grp, SUM(CASE WHEN qty > 2 THEN price ELSE 0 END) FROM orders GROUP BY grp",
        "SELECT grp, SUM(x) FROM (SELECT grp, CASE WHEN qty > 2 THEN price ELSE 0 END AS x FROM orders) t GROUP BY grp",
    ] {
        assert_eq!(fixture.run(sql).0, expected, "{sql}");
    }
}
