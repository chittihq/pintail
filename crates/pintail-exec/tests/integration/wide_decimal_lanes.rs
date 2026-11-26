//! Decimal aggregates declared wider than 18 digits fold in the packed-key
//! fold when their values' units fit 64 bits.
//!
//! A product of two money columns declares 31 digits, and a product of
//! three 47, so a SUM of either used to have no lane: the whole GROUP BY
//! then ran on the general path, a vector of values per row. The lane now
//! takes any decimal column and checks each batch's units: a batch whose
//! units all fit 64 bits folds in the packed cells, and one holding a
//! value past them folds row by row, exactly. The answers are checked
//! against a model computed here from the rows.
//!
//! The ignored bench times the summary shape over a ledger:
//! `cargo test --profile recovery -p pintail-exec --test integration
//! wide_decimal_lanes::bench -- --ignored --nocapture`. `BENCH_ROWS` sets
//! the table size (6M), `BENCH_RUNS` the repeats (7), `BENCH_ONLY` keeps
//! the shapes whose name contains it, and `PINTAIL_DISABLE_PACKED_GROUP=1`
//! runs the same queries down the general path. Run it with
//! `PINTAIL_DISABLE_SETTLED_MEMO=1`, or every repeat is a memo replay.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SIDES: [&str; 3] = ["A", "N", "R"];
const STATES: [&str; 2] = ["F", "O"];
/// Days the rows span from 1992-01-01: through 1998-12-31.
const DAYS: u64 = 2_557;

fn mix(id: u64) -> u64 {
    let mut x = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn small(value: u64, modulus: u64) -> i128 {
    i128::from(value % modulus)
}

/// One row as the model holds it: decimals as their units at scale 2.
struct Entry {
    side: usize,
    state: usize,
    price: i128,
    cut: i128,
    levy: i128,
    /// A wide-declared column; past 64 bits of units from `huge_from` on.
    big: Option<i128>,
    day: u64,
}

fn entry(id: u64, huge_from: u64) -> Entry {
    let h = mix(id);
    let big = (!mix(h ^ 7).is_multiple_of(9)).then(|| {
        let units = small(h >> 3, 2_000_000_000) - 1_000_000_000;
        if id >= huge_from && id.is_multiple_of(97) {
            // 10^20 units: a value no i64 holds.
            units * 100_000_000_000 + 100_000_000_000_000_000_000
        } else {
            units
        }
    });
    Entry {
        side: usize::try_from(h % 3).expect("small"),
        state: usize::try_from((h >> 2) % 2).expect("small"),
        // A quantity of 1..=50 at a price of 900.00..=2099.99.
        price: (small(h >> 8, 50) + 1) * (small(h >> 16, 120_000) + 90_000),
        cut: small(h >> 32, 11),
        levy: small(h >> 40, 9),
        big,
        day: (h >> 48) % DAYS,
    }
}

fn scaled(units: i128, scale: u32) -> String {
    let divisor = 10_i128.pow(scale);
    let magnitude = units.abs();
    let sign = if units < 0 { "-" } else { "" };
    format!(
        "{sign}{}.{:0width$}",
        magnitude / divisor,
        magnitude % divisor,
        width = scale as usize
    )
}

/// Day `day` from 1992-01-01, as a DATE's text.
fn date(day: u64) -> String {
    let z = day + 8_035 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn stored(id: u64, entry: &Entry) -> StoredRow {
    let money = |units: i128| Value::Utf8(scaled(units, 2));
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(SIDES[entry.side].to_owned()),
            Value::Utf8(STATES[entry.state].to_owned()),
            money(entry.price),
            money(entry.cut),
            money(entry.levy),
            entry.big.map_or(Value::Null, money),
            Value::Utf8(date(entry.day)),
        ],
        1,
        false,
    )
}

fn schema() -> TableSchema {
    let money = |id, name, precision, nullable| {
        Column::new(
            id,
            name,
            DataType::Decimal {
                precision,
                scale: 2,
            },
            nullable,
        )
    };
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "side", DataType::Utf8, false),
            Column::new(3, "state", DataType::Utf8, false),
            money(4, "price", 15, false),
            money(5, "cut", 15, false),
            money(6, "levy", 15, false),
            money(7, "big", 30, true),
            Column::new(8, "day", DataType::Date32, false),
        ],
    )
    .expect("schema")
}

struct Ledger {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    entries: Vec<Entry>,
}

fn ledger(rows: u64, huge_from: u64, settled: bool) -> Ledger {
    let directory = tempfile::tempdir().expect("directory");
    let mut table = TableStore::open(
        directory.path(),
        schema(),
        StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    let entries = (1..=rows)
        .map(|id| entry(id, huge_from))
        .collect::<Vec<_>>();
    let stored = entries
        .iter()
        .zip(1..)
        .map(|(entry, id)| stored(id, entry))
        .collect::<Vec<_>>();
    if settled {
        for chunk in stored.chunks(500_000) {
            table.bulk_ingest_snapshot(chunk.to_vec()).expect("ingest");
        }
    } else {
        table.ingest(stored).expect("memtable rows");
    }
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "ledger",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry")
    .with_column_statistics(pintail_catalog::LazyColumnStatistics::new(move || {
        snapshot.column_statistics()
    }));
    Ledger {
        _directory: directory,
        table,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
        ])
        .expect("catalog"),
        entries,
    }
}

struct Answer {
    rows: Vec<Vec<String>>,
    notes: String,
}

fn run(ledger: &Ledger, sql: &str) -> Answer {
    let snapshot = ledger.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&ledger.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(plan, &provider, 1 << 30, None, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row) {
                        Some(Value::Null) | None => "NULL".to_owned(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(Value::Int64(number)) => number.to_string(),
                        Some(Value::DecimalAverage(average)) => average.canonical(),
                        Some(other) => other
                            .text()
                            .map_or_else(|| format!("{other:?}"), ToOwned::to_owned),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    let profile = execution.profile().expect("profile");
    if std::env::var_os("BENCH_PROFILE").is_some() {
        eprintln!("{}", profile.render());
    }
    Answer {
        rows,
        notes: profile
            .operators
            .iter()
            .filter_map(|operator| operator.note.clone())
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// An exact average of `total` units over `count` rows, `digits` places
/// finer, rounded half away from zero.
fn average(total: i128, count: i128, digits: u32, scale: u32) -> String {
    let numerator = total * 10_i128.pow(digits);
    let magnitude = (2 * numerator.abs() + count) / (2 * count);
    scaled(if numerator < 0 { -magnitude } else { magnitude }, scale)
}

const SUMMARY: &str = "SELECT side, state, SUM(price), SUM(price * (1 - cut)), \
     SUM(price * (1 - cut) * (1 + levy)), AVG(price), AVG(cut), \
     MIN(price * (1 - cut)), MAX(price * (1 - cut)), COUNT(*) \
     FROM ledger WHERE day <= '1998-09-02' GROUP BY side, state ORDER BY side, state";

/// The summary as the model computes it. The units of `price * (1 - cut)`
/// are at scale 4, and with `(1 + levy)` at scale 6: the scales a product
/// adds, which is what MySQL answers them at.
fn summary(entries: &[Entry]) -> Vec<Vec<String>> {
    #[derive(Default)]
    struct Totals {
        rows: i128,
        price: i128,
        discounted: i128,
        charged: i128,
        cut: i128,
        least: Option<i128>,
        most: Option<i128>,
    }
    let last = (0..DAYS)
        .find(|day| date(*day) == "1998-09-02")
        .expect("in range");
    let mut groups = BTreeMap::<(usize, usize), Totals>::new();
    for entry in entries.iter().filter(|entry| entry.day <= last) {
        let totals = groups.entry((entry.side, entry.state)).or_default();
        let discounted = entry.price * (100 - entry.cut);
        totals.rows += 1;
        totals.price += entry.price;
        totals.discounted += discounted;
        totals.charged += discounted * (100 + entry.levy);
        totals.cut += entry.cut;
        totals.least = Some(
            totals
                .least
                .map_or(discounted, |least| least.min(discounted)),
        );
        totals.most = Some(totals.most.map_or(discounted, |most| most.max(discounted)));
    }
    groups
        .iter()
        .map(|((side, state), totals)| {
            vec![
                SIDES[*side].to_owned(),
                STATES[*state].to_owned(),
                scaled(totals.price, 2),
                scaled(totals.discounted, 4),
                scaled(totals.charged, 6),
                average(totals.price, totals.rows, 4, 6),
                average(totals.cut, totals.rows, 4, 6),
                scaled(totals.least.expect("a row"), 4),
                scaled(totals.most.expect("a row"), 4),
                totals.rows.to_string(),
            ]
        })
        .collect()
}

#[test]
fn products_of_money_columns_fold_in_packed_cells() {
    for settled in [true, false] {
        let ledger = ledger(150_000, u64::MAX, settled);
        let answer = run(&ledger, SUMMARY);
        assert_eq!(answer.rows, summary(&ledger.entries), "settled {settled}");
        if settled {
            assert!(
                answer.notes.contains("packed-key fold:")
                    && answer.notes.contains(", 0 batches folded row by row"),
                "the summary left the packed cells: {}",
                answer.notes
            );
        }
    }
}

const WIDE: &str = "SELECT side, state, SUM(big), MIN(big), MAX(big), AVG(big), COUNT(big) \
     FROM ledger GROUP BY side, state ORDER BY side, state";

fn wide(entries: &[Entry]) -> Vec<Vec<String>> {
    let mut groups = BTreeMap::<(usize, usize), Vec<i128>>::new();
    for entry in entries {
        let values = groups.entry((entry.side, entry.state)).or_default();
        values.extend(entry.big);
    }
    groups
        .iter()
        .map(|((side, state), values)| {
            let total: i128 = values.iter().sum();
            let count = i128::try_from(values.len()).expect("small");
            vec![
                SIDES[*side].to_owned(),
                STATES[*state].to_owned(),
                scaled(total, 2),
                scaled(*values.iter().min().expect("a value"), 2),
                scaled(*values.iter().max().expect("a value"), 2),
                average(total, count, 4, 6),
                values.len().to_string(),
            ]
        })
        .collect()
}

/// A wide-declared column whose later batches hold values past 64 bits of
/// units: the batches that fit fold in the cells, the others row by row,
/// and the answer is the exact one either way. One whose first batch holds
/// such a value keeps the general path.
#[test]
fn units_past_sixty_four_bits_fold_row_by_row() {
    let rows = 200_000;
    let later = ledger(rows, rows - 20_000, true);
    let answer = run(&later, WIDE);
    assert_eq!(answer.rows, wide(&later.entries));
    assert!(
        answer.notes.contains("packed-key fold:")
            && !answer.notes.contains(", 0 batches folded row by row"),
        "the batches past 64 bits were not folded row by row: {}",
        answer.notes
    );

    let ledger = ledger(rows, 0, true);
    let answer = run(&ledger, WIDE);
    assert_eq!(answer.rows, wide(&ledger.entries));
    assert!(
        answer
            .notes
            .contains("a decimal aggregate's units do not fit a lane"),
        "a first batch past 64 bits did not decline the cells: {}",
        answer.notes
    );
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn bench() {
    let env = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let rows = env("BENCH_ROWS", 6_000_000);
    let runs = env("BENCH_RUNS", 7);
    let load = std::time::Instant::now();
    let ledger = ledger(rows, u64::MAX, true);
    eprintln!("{rows} rows loaded in {:.1}s", load.elapsed().as_secs_f64());
    // The summary, then its parts: what the columns alone cost, and what
    // each product adds.
    let shape = |aggregates: &str| {
        format!(
            "SELECT side, state, {aggregates} FROM ledger WHERE day <= '1998-09-02' \
             GROUP BY side, state ORDER BY side, state"
        )
    };
    let shapes = [
        ("summary", SUMMARY.to_owned()),
        (
            "columns only",
            shape("SUM(price), AVG(price), AVG(cut), COUNT(*)"),
        ),
        ("one product", shape("SUM(price * (1 - cut))")),
        (
            "three factors",
            shape("SUM(price * (1 - cut) * (1 + levy))"),
        ),
    ];
    let only = std::env::var("BENCH_ONLY").unwrap_or_default();
    for (name, sql) in shapes.iter().filter(|(name, _)| name.contains(&only)) {
        let mut times = Vec::new();
        let mut notes = String::new();
        for _ in 0..runs {
            let clock = std::time::Instant::now();
            let answer = run(&ledger, sql);
            times.push(clock.elapsed().as_secs_f64() * 1e3);
            notes = answer.notes;
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "{name}: median {:.1} ms, min {:.1} ms, max {:.1} ms over {runs} runs\n  {notes}",
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        );
    }
}
