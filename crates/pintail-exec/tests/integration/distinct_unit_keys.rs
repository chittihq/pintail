//! `COUNT(DISTINCT)` over DECIMAL, DATE, DATETIME and text columns, and
//! `STDDEV`/`VARIANCE` over a DECIMAL column, folded a column at a time.
//!
//! The distinct counts are checked against a model of the rows: a decimal
//! or temporal value is one value per canonical text, and a text value is
//! one value per collation class - case and accents folded with NO PAD
//! under `utf8mb4_0900_ai_ci`, trailing spaces dropped under `utf8mb4_bin`,
//! both under `latin1_swedish_ci`. The moments are checked against the
//! same aggregate over `column + 0`, which keeps the per-row update.
//!
//! Every check runs twice: over rows the store has packed, and again after
//! writes that stay in the memtable repeat stored values, add new ones and
//! replace rows, so one distinct set takes the same value from a packed
//! batch and from a row.
//!
//! `DISTINCT_KEYS_DUMP=<file>` writes the rows of both states as
//! tab-separated text and prints each query's answer, to load into another
//! server and compare.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 60_000;
const REGIONS: [&str; 3] = ["north", "south", "east"];
const KITES: [&str; 4] = ["kite", "Kite", "kíte", "KITE"];

#[derive(Clone)]
struct Model {
    region: &'static str,
    price: Option<String>,
    seen: Option<String>,
    stamp: Option<String>,
    day: Option<String>,
    label: Option<String>,
    cost: Option<String>,
}

fn mix(id: u64) -> u64 {
    let mut x = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn decimal(units: i64, scale: usize) -> String {
    let divisor = 10_i64.pow(u32::try_from(scale).expect("small"));
    format!(
        "{}{}.{:0scale$}",
        if units < 0 { "-" } else { "" },
        units.abs() / divisor,
        units.abs() % divisor
    )
}

fn date(day: u64) -> String {
    format!(
        "{}-{:02}-{:02}",
        2021 + day / 336,
        1 + (day % 336) / 28,
        1 + day % 28
    )
}

/// The row `seed` describes. Values repeat often: prices over a few
/// thousand cents around zero, seconds over two days, labels over a few
/// thousand stems in four spellings with and without a trailing space.
fn model(seed: u64) -> Model {
    let h = mix(seed);
    let pick = |shift: u32, modulus: u64| (h >> shift) % modulus;
    let null = |every: u64| mix(seed ^ every).is_multiple_of(every);
    let seconds = pick(8, 2 * 86_400);
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds % 86_400 / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    let day = date(900 + seconds / 86_400);
    Model {
        region: REGIONS[usize::try_from(pick(40, 3)).expect("small")],
        price: (!null(17))
            .then(|| decimal(i64::try_from(pick(4, 6_000)).expect("small") - 3_000, 2)),
        seen: (!null(19)).then(|| format!("{day} {clock}")),
        stamp: (!null(23)).then(|| format!("{day} {clock}.{:03}", pick(30, 4) * 250)),
        day: (!null(29)).then(|| date(pick(20, 700))),
        label: (!null(31)).then(|| {
            format!(
                "{}-{:04}{}",
                KITES[usize::try_from(pick(50, 4)).expect("small")],
                pick(12, 3_000),
                if pick(55, 5) == 0 { " " } else { "" }
            )
        }),
        cost: (!null(37)).then(|| {
            decimal(
                i64::try_from(pick(2, 90_000_000_000)).expect("small") - 40_000_000_000,
                3,
            )
        }),
    }
}

fn stored(id: u64, row: &Model, version: u64) -> StoredRow {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::Utf8);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(row.region.to_owned()),
            text(&row.price),
            text(&row.seen),
            text(&row.stamp),
            text(&row.day),
            text(&row.label),
            text(&row.label),
            text(&row.label),
            text(&row.cost),
        ],
        version,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    rows: BTreeMap<u64, Model>,
}

fn fixture() -> Fixture {
    let label = |id: u32, name: &str, collation: &str| {
        Column::new(id, name, DataType::Utf8, true).with_collation(Some(collation.to_owned()))
    };
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "region", DataType::Utf8, false),
            Column::new(
                3,
                "price",
                DataType::Decimal {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            Column::new(4, "seen", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(5, "stamp", DataType::DateTime64 { fsp: 3 }, true),
            Column::new(6, "day", DataType::Date32, true),
            label(7, "label_ai", "utf8mb4_0900_ai_ci"),
            label(8, "label_bin", "utf8mb4_bin"),
            label(9, "label_latin", "latin1_swedish_ci"),
            Column::new(
                10,
                "cost",
                DataType::Decimal {
                    precision: 14,
                    scale: 3,
                },
                true,
            ),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let rows: BTreeMap<u64, Model> = (0..ROWS).map(|id| (id, model(id))).collect();
    let mut start = 0;
    while start < ROWS {
        let end = (start + 20_000).min(ROWS);
        table
            .bulk_ingest_snapshot((start..end).map(|id| stored(id, &rows[&id], 1)).collect())
            .expect("ingest");
        start = end;
    }
    // Column statistics on first use, as the server's catalog has: they
    // prove the calendar columns hold real dates, so their keys need no
    // copy check and keep the column folds.
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "kites",
        schema,
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_column_statistics(pintail_catalog::LazyColumnStatistics::new(move || {
        snapshot.column_statistics()
    }));
    Fixture {
        _directory: directory,
        table,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
        ])
        .expect("catalog"),
        rows,
    }
}

impl Fixture {
    /// Writes that stay in the memtable: new rows, and replacements of
    /// stored ones, drawn from the same value space so most of what they
    /// hold is already in a packed batch.
    fn write_memtable(&mut self) {
        let mut writes = Vec::new();
        for offset in 0..400_u64 {
            let id = if offset % 2 == 0 {
                ROWS + offset
            } else {
                offset * 97
            };
            let row = model(id ^ 0x5eed_0000);
            writes.push(stored(id, &row, 2));
            self.rows.insert(id, row);
        }
        self.table.ingest(writes).expect("memtable writes");
    }
}

fn run(fixture: &Fixture, sql: &str) -> Vec<Vec<Value>> {
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(plan, &provider, 256 << 20, Collation::default()).expect("execution");
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
    rows
}

/// One class per value `MySQL` counts once under the column's collation,
/// for the spellings the labels use.
fn class(column: &str, text: &str) -> String {
    let folded = || text.to_lowercase().replace('í', "i");
    match column {
        "label_ai" => folded(),
        "label_bin" => text.trim_end_matches(' ').to_owned(),
        "label_latin" => folded().trim_end_matches(' ').to_owned(),
        _ => text.to_owned(),
    }
}

fn value_of<'a>(row: &'a Model, column: &str) -> Option<&'a String> {
    match column {
        "price" => row.price.as_ref(),
        "seen" => row.seen.as_ref(),
        "stamp" => row.stamp.as_ref(),
        "day" => row.day.as_ref(),
        _ => row.label.as_ref(),
    }
}

const COLUMNS: [&str; 7] = [
    "price",
    "seen",
    "stamp",
    "day",
    "label_ai",
    "label_bin",
    "label_latin",
];

fn check_distinct(fixture: &Fixture, state: &str, failures: &mut Vec<String>) {
    let print = std::env::var_os("DISTINCT_KEYS_DUMP").is_some();
    for (filter, keep) in [("", 3_u64), ("WHERE id % 3 <> 1", 1)] {
        let kept = |id: u64| keep == 3 || id % 3 != 1;
        for column in COLUMNS {
            let expected = |region: Option<&str>| {
                fixture
                    .rows
                    .iter()
                    .filter(|(id, row)| kept(**id) && region.is_none_or(|name| row.region == name))
                    .filter_map(|(_, row)| value_of(row, column))
                    .map(|text| class(column, text))
                    .collect::<BTreeSet<_>>()
                    .len() as u64
            };
            let sql = format!("SELECT COUNT(DISTINCT {column}) FROM kites {filter}");
            let answer = run(fixture, &sql);
            if print {
                println!("{state}\t{sql}\t{answer:?}");
            }
            if answer != vec![vec![Value::UInt64(expected(None))]] {
                failures.push(format!("{state}: {sql}: {answer:?} != {}", expected(None)));
            }
            let sql = format!(
                "SELECT region, COUNT(DISTINCT {column}) FROM kites {filter} \
                 GROUP BY region ORDER BY region"
            );
            let answer = run(fixture, &sql);
            if print {
                println!("{state}\t{sql}\t{answer:?}");
            }
            let mut regions = REGIONS;
            regions.sort_unstable();
            let wanted: Vec<Vec<Value>> = regions
                .iter()
                .map(|region| {
                    vec![
                        Value::Utf8((*region).to_owned()),
                        Value::UInt64(expected(Some(region))),
                    ]
                })
                .collect();
            if answer != wanted {
                failures.push(format!("{state}: {sql}: {answer:?} != {wanted:?}"));
            }
        }
    }
}

fn check_moments(fixture: &Fixture, state: &str, failures: &mut Vec<String>) {
    let print = std::env::var_os("DISTINCT_KEYS_DUMP").is_some();
    for filter in ["WHERE id % 5 <> 2", "WHERE id % 2 = 0", "WHERE id < 0"] {
        for function in ["STDDEV", "STDDEV_SAMP", "VARIANCE", "VAR_SAMP"] {
            for column in ["cost", "price"] {
                for (key, group) in [("", ""), ("region, ", " GROUP BY region ORDER BY region")] {
                    let folded =
                        format!("SELECT {key}{function}({column}) FROM kites {filter}{group}");
                    let per_row =
                        format!("SELECT {key}{function}({column} + 0) FROM kites {filter}{group}");
                    let left = run(fixture, &folded);
                    let right = run(fixture, &per_row);
                    if print {
                        println!("{state}\t{folded}\t{left:?}");
                    }
                    if format!("{left:?}") != format!("{right:?}") {
                        failures.push(format!("{state}: {folded}: {left:?} != {right:?}"));
                    }
                }
            }
        }
    }
}

fn dump(fixture: &Fixture, state: &str) {
    let Some(path) = std::env::var_os("DISTINCT_KEYS_DUMP") else {
        return;
    };
    let cell = |value: &Option<String>| value.clone().unwrap_or_else(|| "\\N".to_owned());
    let mut out = String::new();
    for (id, row) in &fixture.rows {
        let label = cell(&row.label);
        writeln!(
            out,
            "{id}\t{}\t{}\t{}\t{}\t{}\t{label}\t{label}\t{label}\t{}",
            row.region,
            cell(&row.price),
            cell(&row.seen),
            cell(&row.stamp),
            cell(&row.day),
            cell(&row.cost),
        )
        .expect("a string takes every write");
    }
    let mut path = std::path::PathBuf::from(path);
    path.set_extension(state);
    std::fs::write(path, out).expect("dump");
}

#[test]
fn distinct_counts_and_decimal_moments_fold_exactly() {
    let mut fixture = fixture();
    let mut failures = Vec::new();
    dump(&fixture, "packed");
    check_distinct(&fixture, "packed", &mut failures);
    check_moments(&fixture, "packed", &mut failures);
    fixture.write_memtable();
    dump(&fixture, "memtable");
    check_distinct(&fixture, "memtable", &mut failures);
    check_moments(&fixture, "memtable", &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The folds are the ones that ran: a regression to the per-row update
/// would still answer correctly, and only the profile shows it.
#[test]
fn the_column_folds_take_these_shapes() {
    let fixture = fixture();
    for aggregate in [
        "COUNT(DISTINCT price)",
        "COUNT(DISTINCT seen)",
        "COUNT(DISTINCT stamp)",
        "COUNT(DISTINCT day)",
        "COUNT(DISTINCT label_ai)",
        "COUNT(DISTINCT label_latin)",
        "STDDEV(cost)",
        "VAR_SAMP(price)",
    ] {
        let sql = format!("SELECT {aggregate} FROM kites WHERE id % 7 <> 3");
        let snapshot = fixture.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&fixture.catalog, Some("app"))
            .bind(&parse_statement(&sql).expect("parse"))
            .expect("bind");
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(plan, &provider, 256 << 20, None, Collation::default())
                .expect("execution");
        while execution.next_batch().expect("batch").is_some() {}
        let notes = execution
            .profile()
            .map(|profile| {
                profile
                    .operators
                    .iter()
                    .filter_map(|operator| operator.note.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        assert!(
            notes.contains("ungrouped column fold") && notes.contains(", 0 per row"),
            "{sql}: {notes}"
        );
    }
}
