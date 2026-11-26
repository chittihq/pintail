//! In-process timing of decimal arithmetic chains in aggregate arguments,
//! over a pricing-summary shape: a few groups, most rows selected, and two
//! sums whose arguments are `+`, `-` and `*` chains over DECIMAL(15,2)
//! columns, the second containing the first. The control drops both
//! computed sums, so the difference is what the chains cost. Ignored: a
//! measurement, not a gate. Run with
//! `cargo test --profile recovery -p pintail-exec --test integration decimal_chain_cost:: -- --ignored --nocapture`.
//! `PINTAIL_BENCH_ROWS` overrides the table size (default two million).

use std::hash::{Hash, Hasher};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const MONEY: DataType = DataType::Decimal {
    precision: 15,
    scale: 2,
};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "flag", DataType::Utf8, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "quantity", MONEY, false),
            Column::new(5, "price", MONEY, false),
            Column::new(6, "discount", MONEY, false),
            Column::new(7, "tax", MONEY, false),
            Column::new(8, "shipped", DataType::Date32, false),
        ],
    )
    .expect("schema")
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

/// A deterministic spread of values: no randomness, so the digest of an
/// answer is the same on every run and every build.
fn row(id: u64) -> StoredRow {
    let cents = |value: u64| Value::Utf8(format!("{}.{:02}", value / 100, value % 100));
    let day = 1 + id % 28;
    let month = 1 + (id / 28) % 12;
    let year = 1992 + (id / 336) % 7;
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(["A", "N", "R"][usize::try_from(id % 3).expect("small")].to_owned()),
            Value::Utf8(["F", "O"][usize::try_from(id % 2).expect("small")].to_owned()),
            cents(100 + (id * 7) % 4_900),
            cents(90_000 + (id * 7_919) % 10_400_000),
            cents((id * 13) % 11),
            cents((id * 17) % 9),
            Value::Utf8(format!("{year}-{month:02}-{day:02}")),
        ],
        id + 1,
        false,
    )
}

impl Fixture {
    fn new(rows: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        let mut start = 0;
        while start < rows {
            let end = (start + 500_000).min(rows);
            table
                .bulk_ingest_snapshot((start..end).map(row).collect())
                .expect("rows");
            start = end;
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "lines",
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

const FILTER: &str = "WHERE shipped <= DATE_SUB('1998-12-01', INTERVAL 90 DAY)";

fn cases() -> [(&'static str, String); 4] {
    [
        (
            "summary",
            format!(
                "SELECT flag, status, SUM(quantity), SUM(price), \
                 SUM(price * (1 - discount)), SUM(price * (1 - discount) * (1 + tax)), \
                 AVG(quantity), AVG(price), AVG(discount), COUNT(*) \
                 FROM lines {FILTER} GROUP BY flag, status ORDER BY flag, status"
            ),
        ),
        (
            "control",
            format!(
                "SELECT flag, status, SUM(quantity), SUM(price), \
                 AVG(quantity), AVG(price), AVG(discount), COUNT(*) \
                 FROM lines {FILTER} GROUP BY flag, status ORDER BY flag, status"
            ),
        ),
        (
            "discounted",
            format!(
                "SELECT flag, status, SUM(price * (1 - discount)) \
                 FROM lines {FILTER} GROUP BY flag, status ORDER BY flag, status"
            ),
        ),
        (
            "charged",
            format!(
                "SELECT flag, status, SUM(price * (1 - discount) * (1 + tax)) \
                 FROM lines {FILTER} GROUP BY flag, status ORDER BY flag, status"
            ),
        ),
    ]
}

/// The two sums answer what a derived table computing the same chains
/// answers, and what the rows' own arithmetic in whole cents gives.
#[test]
fn chained_sums_match_their_exact_value() {
    const ROWS: u64 = 40_000;
    let fixture = Fixture::new(ROWS);
    let mut expected = std::collections::BTreeMap::<(String, String), (i128, i128)>::new();
    for id in 0..ROWS {
        let stored = row(id);
        let values = stored.values();
        let units = |value: &Value| {
            let text = value.text().expect("text");
            pintail_types::parse_decimal_scaled(text, 2).expect("units")
        };
        if values[7].text().expect("date") > "1998-09-02" {
            continue;
        }
        let (price, discount, tax) = (units(&values[4]), units(&values[5]), units(&values[6]));
        let discounted = price * (100 - discount);
        let entry = expected
            .entry((
                values[1].text().expect("flag").to_owned(),
                values[2].text().expect("status").to_owned(),
            ))
            .or_default();
        entry.0 += discounted;
        entry.1 += discounted * (100 + tax);
    }
    let spell = |units: i128, scale: u32| {
        let factor = 10_i128.pow(scale);
        format!(
            "{:?}",
            Value::Utf8(format!(
                "{}.{:0width$}",
                units / factor,
                units % factor,
                width = scale as usize
            ))
        )
    };
    let mut expected = expected
        .into_iter()
        .map(|((flag, status), (discounted, charged))| {
            vec![
                format!("{:?}", Value::Utf8(flag)),
                format!("{:?}", Value::Utf8(status)),
                spell(discounted, 4),
                spell(charged, 6),
            ]
        })
        .collect::<Vec<_>>();
    expected.sort();
    for sql in [
        format!(
            "SELECT flag, status, SUM(price * (1 - discount)), \
             SUM(price * (1 - discount) * (1 + tax)) FROM lines {FILTER} GROUP BY flag, status"
        ),
        format!(
            "SELECT flag, status, SUM(d), SUM(c) FROM (SELECT flag, status, \
             price * (1 - discount) AS d, price * (1 - discount) * (1 + tax) AS c \
             FROM lines {FILTER}) t GROUP BY flag, status"
        ),
    ] {
        assert_eq!(fixture.run(&sql).0, expected, "{sql}");
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn decimal_chain_aggregation_cost() {
    let rows = std::env::var("PINTAIL_BENCH_ROWS")
        .ok()
        .and_then(|rows| rows.parse().ok())
        .unwrap_or(2_000_000);
    let fixture = Fixture::new(rows);
    let cases = cases();
    let mut timings = vec![Vec::new(); cases.len()];
    let mut digests = vec![0_u64; cases.len()];
    for (index, (_, sql)) in cases.iter().enumerate() {
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        fixture.run(sql).0.hash(&mut digest);
        digests[index] = digest.finish();
    }
    // Interleaved: one run of each case per round.
    for _ in 0..9 {
        for (index, (_, sql)) in cases.iter().enumerate() {
            timings[index].push(fixture.run(sql).1);
        }
    }
    for ((name, _), (mut timing, digest)) in cases.iter().zip(timings.into_iter().zip(digests)) {
        timing.sort_by(f64::total_cmp);
        println!(
            "[{rows} rows] {name:<11} median {:>8.1} ms  min {:>8.1} ms  max {:>8.1} ms  digest {digest:016x}",
            timing[timing.len() / 2],
            timing[0],
            timing[timing.len() - 1],
        );
    }
}
