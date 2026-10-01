//! GROUP BY over a composite key, or one sparse integer key, folds by a
//! packed key: every key column a fixed-width cell, the rows cut by the
//! key's hash into partitions that each keep their groups' totals inline.
//!
//! The answers are checked against a model computed here from the rows: for
//! keys mixing an integer, text whose spellings differ only by case or
//! accent, a date and a decimal, with NULLs in every key position; with the
//! rows settled, in the memtable, or split between the two under updates
//! and deletes; and under a memory ceiling small enough that the groups go
//! to disk part-way.
//!
//! The ignored bench prints what the two shapes cost on an invented ledger:
//! `cargo test --profile recovery -p pintail-exec --test integration
//! packed_group_keys::bench -- --ignored --nocapture`. `BENCH_ROWS` sets
//! the table size (2M), `BENCH_RUNS` the repeats (9), and
//! `PINTAIL_DISABLE_PACKED_GROUP=1` runs the same queries down the paths
//! they took before.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Spellings and the collation class each belongs to: the default
/// collation ignores case and accents, so a class is one group.
const BRANCHES: [(&str, &str); 9] = [
    ("north", "north"),
    ("North", "north"),
    ("NORTH", "north"),
    ("nórth", "north"),
    ("south", "south"),
    ("Sóuth", "south"),
    ("east", "east"),
    ("west", "west"),
    ("WÉST", "west"),
];

const RATES: [&str; 5] = ["0.25", "-1.50", "12.00", "0.00", "999.99"];

fn mix(id: u64) -> u64 {
    let mut x = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// One row of the ledger as the model holds it.
#[derive(Clone, Debug)]
struct Entry {
    account: Option<i64>,
    branch: Option<usize>,
    day: Option<String>,
    rate: Option<&'static str>,
    qty: Option<i64>,
    fee: Option<i64>,
}

fn pick(seed: u64, modulus: u64) -> usize {
    usize::try_from(seed % modulus).expect("small")
}

/// The row `id` holds at `version`; `accounts` bounds the integer key.
fn entry(id: u64, version: u64, accounts: u64) -> Entry {
    let h = mix(id ^ (version << 48));
    let none = |salt: u64, every: u64| mix(h ^ salt).is_multiple_of(every);
    let day = pick(h >> 8, 40);
    Entry {
        account: (!none(1, 13))
            .then(|| i64::try_from((h % accounts) * 1_000_003).expect("account") - 7_000_000_000),
        branch: (!none(2, 11)).then(|| pick(h >> 16, 9)),
        day: (!none(3, 17)).then(|| format!("2024-{:02}-{:02}", 1 + day / 28, 1 + day % 28)),
        rate: (!none(4, 7)).then(|| RATES[pick(h >> 24, 5)]),
        qty: (!none(5, 5)).then(|| i64::try_from((h >> 30) % 20_001).expect("qty") - 10_000),
        fee: (!none(6, 9)).then(|| i64::try_from((h >> 12) % 5_000_000).expect("fee") - 1_000_000),
    }
}

fn scaled(units: i128, scale: u32) -> String {
    let divisor = 10_i128.pow(scale);
    let magnitude = units.abs();
    let sign = if units < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{magnitude}");
    }
    format!(
        "{sign}{}.{:0width$}",
        magnitude / divisor,
        magnitude % divisor,
        width = scale as usize
    )
}

fn stored(id: u64, entry: &Entry, version: u64, deleted: bool) -> StoredRow {
    let text = |value: Option<String>| value.map_or(Value::Null, Value::Utf8);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            entry.account.map_or(Value::Null, Value::Int64),
            text(entry.branch.map(|branch| BRANCHES[branch].0.to_owned())),
            text(entry.day.clone()),
            text(entry.rate.map(ToOwned::to_owned)),
            entry.qty.map_or(Value::Null, Value::Int64),
            text(entry.fee.map(|fee| scaled(i128::from(fee), 2))),
        ],
        version,
        deleted,
    )
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::Int64, true),
            Column::new(3, "branch", DataType::Utf8, true),
            Column::new(4, "day", DataType::Date32, true),
            Column::new(
                5,
                "rate",
                DataType::Decimal {
                    precision: 6,
                    scale: 2,
                },
                true,
            ),
            Column::new(6, "qty", DataType::Int64, true),
            Column::new(
                7,
                "fee",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
        ],
    )
    .expect("schema")
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Settled,
    MemtableOnly,
    SettledWithWrites,
}

type Model = BTreeMap<u64, Entry>;

struct Ledger {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    model: Model,
}

fn ledger(layout: Layout, rows: u64, accounts: u64) -> Ledger {
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
    let mut model = Model::new();
    let mut base = Vec::new();
    for id in 1..=rows {
        let entry = entry(id, 1, accounts);
        base.push(stored(id, &entry, 1, false));
        model.insert(id, entry);
    }
    match layout {
        Layout::Settled | Layout::SettledWithWrites => {
            for chunk in base.chunks(100_000) {
                table.bulk_ingest_snapshot(chunk.to_vec()).expect("ingest");
            }
        }
        Layout::MemtableOnly => {
            table.ingest(base).expect("memtable rows");
        }
    }
    if matches!(layout, Layout::SettledWithWrites) {
        let mut writes = Vec::new();
        for id in (1..=rows).step_by(7) {
            let entry = entry(id, 3, accounts);
            writes.push(stored(id, &entry, 3, false));
            model.insert(id, entry);
        }
        for id in (3..=rows).step_by(31) {
            if let Some(entry) = model.remove(&id) {
                writes.push(stored(id, &entry, 4, true));
            }
        }
        for id in rows + 1..=rows + 500 {
            let entry = entry(id, 3, accounts);
            writes.push(stored(id, &entry, 3, false));
            model.insert(id, entry);
        }
        table.ingest(writes).expect("memtable writes");
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "ledger",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry");
    Ledger {
        _directory: directory,
        table,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
        ])
        .expect("catalog"),
        model,
    }
}

struct Answer {
    rows: Vec<Vec<String>>,
    groups: usize,
    notes: String,
    peak_reserved: usize,
}

fn run(table: &TableStore, catalog: &CatalogSnapshot, sql: &str, memory: usize) -> Answer {
    pull(table, catalog, sql, memory, true)
}

/// Runs `sql`; `keep` renders the rows, otherwise they are only counted.
fn pull(
    table: &TableStore,
    catalog: &CatalogSnapshot,
    sql: &str,
    memory: usize,
    keep: bool,
) -> Answer {
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(plan, &provider, memory, None, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    let mut groups = 0;
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        groups += batch.visible_row_count();
        if !keep {
            continue;
        }
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
    Answer {
        rows,
        groups,
        notes: profile
            .operators
            .iter()
            .filter_map(|operator| operator.note.clone())
            .collect::<Vec<_>>()
            .join("; "),
        peak_reserved: profile
            .operators
            .iter()
            .map(|operator| operator.peak_reserved)
            .max()
            .unwrap_or(0),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Account,
    Branch,
    Day,
    Rate,
}

impl Key {
    const fn column(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Branch => "branch",
            Self::Day => "day",
            Self::Rate => "rate",
        }
    }

    /// The key as the model groups it: a text key by its collation class.
    fn of(self, entry: &Entry) -> String {
        match self {
            Self::Account => entry.account.map(|account| account.to_string()),
            Self::Branch => entry.branch.map(|branch| BRANCHES[branch].1.to_owned()),
            Self::Day => entry.day.clone(),
            Self::Rate => entry.rate.map(ToOwned::to_owned),
        }
        .unwrap_or_else(|| "NULL".to_owned())
    }
}

#[derive(Default)]
struct Totals {
    rows: u64,
    quantities: u64,
    quantity: i128,
    least: Option<i64>,
    most: Option<i64>,
    fees: u64,
    fee: i128,
    /// The spelling each text key shows: the one its first row carries.
    shown: Vec<String>,
}

const AGGREGATES: &str =
    "COUNT(*), COUNT(qty), SUM(qty), MIN(qty), MAX(qty), AVG(qty), SUM(fee), COUNT(fee)";

fn expected(model: &Model, keys: &[Key]) -> BTreeMap<Vec<String>, Totals> {
    let mut groups = BTreeMap::<Vec<String>, Totals>::new();
    for entry in model.values() {
        let key = keys.iter().map(|key| key.of(entry)).collect::<Vec<_>>();
        let totals = groups.entry(key).or_insert_with(|| Totals {
            shown: keys
                .iter()
                .filter(|key| **key == Key::Branch)
                .map(|_| {
                    entry
                        .branch
                        .map_or_else(|| "NULL".to_owned(), |branch| BRANCHES[branch].0.to_owned())
                })
                .collect(),
            ..Totals::default()
        });
        totals.rows += 1;
        if let Some(qty) = entry.qty {
            totals.quantities += 1;
            totals.quantity += i128::from(qty);
            totals.least = Some(totals.least.map_or(qty, |least| least.min(qty)));
            totals.most = Some(totals.most.map_or(qty, |most| most.max(qty)));
        }
        if let Some(fee) = entry.fee {
            totals.fees += 1;
            totals.fee += i128::from(fee);
        }
    }
    groups
}

fn rendered(totals: &Totals) -> Vec<String> {
    let optional = |value: Option<String>| value.unwrap_or_else(|| "NULL".to_owned());
    let average = (totals.quantities > 0).then(|| {
        let count = i128::from(totals.quantities);
        let numerator = totals.quantity * 10_000;
        // Half away from zero, at the four places an integer average adds.
        let magnitude = (2 * numerator.abs() + count) / (2 * count);
        scaled(if numerator < 0 { -magnitude } else { magnitude }, 4)
    });
    vec![
        totals.rows.to_string(),
        totals.quantities.to_string(),
        optional((totals.quantities > 0).then(|| totals.quantity.to_string())),
        optional(totals.least.map(|least| least.to_string())),
        optional(totals.most.map(|most| most.to_string())),
        optional(average),
        optional((totals.fees > 0).then(|| scaled(totals.fee, 2))),
        totals.fees.to_string(),
    ]
}

fn class_of(spelling: &str) -> String {
    BRANCHES
        .iter()
        .find(|(written, _)| *written == spelling)
        .map_or_else(|| spelling.to_owned(), |(_, class)| (*class).to_owned())
}

/// Runs the grouped query over `keys` and checks every group against the
/// model. Returns the answer for what a caller checks beyond that.
fn check(ledger: &Ledger, keys: &[Key], memory: usize, context: &str) -> Answer {
    let list = keys
        .iter()
        .map(|key| key.column())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {list}, {AGGREGATES} FROM ledger GROUP BY {list}");
    let answer = run(&ledger.table, &ledger.catalog, &sql, memory);
    let mut actual = answer
        .rows
        .iter()
        .map(|row| {
            let mut row = row.clone();
            for (cell, key) in row.iter_mut().zip(keys) {
                if *key == Key::Branch {
                    *cell = class_of(cell);
                }
            }
            row
        })
        .collect::<Vec<_>>();
    actual.sort();
    let mut wanted = expected(&ledger.model, keys)
        .iter()
        .map(|(key, totals)| {
            let mut row = key.clone();
            row.extend(rendered(totals));
            row
        })
        .collect::<Vec<_>>();
    wanted.sort();
    assert_eq!(
        actual.len(),
        wanted.len(),
        "{context}: group count for {sql} ({})",
        answer.notes
    );
    for (actual, wanted) in actual.iter().zip(&wanted) {
        assert_eq!(actual, wanted, "{context}: {sql} ({})", answer.notes);
    }
    answer
}

const SHAPES: [&[Key]; 6] = [
    &[Key::Account, Key::Branch],
    &[Key::Branch, Key::Account],
    &[Key::Day, Key::Account],
    &[Key::Rate, Key::Branch],
    &[Key::Account, Key::Branch, Key::Day],
    &[Key::Rate, Key::Day, Key::Branch, Key::Account],
];

fn enabled() -> bool {
    std::env::var_os("PINTAIL_DISABLE_PACKED_GROUP").is_none()
}

#[test]
fn composite_keys_answer_exactly_wherever_the_rows_live() {
    for layout in [
        Layout::Settled,
        Layout::MemtableOnly,
        Layout::SettledWithWrites,
    ] {
        for (rows, accounts) in [(40_u64, 5_u64), (30_000, 700)] {
            let ledger = ledger(layout, rows, accounts);
            for keys in SHAPES {
                let context = format!("{layout:?} at {rows} rows");
                let answer = check(&ledger, keys, 256 << 20, &context);
                if enabled() && matches!(layout, Layout::Settled) {
                    assert!(
                        answer.notes.contains("packed-key fold:"),
                        "{context}: {keys:?} left the packed-key fold: {}",
                        answer.notes
                    );
                }
            }
        }
    }
}

#[test]
fn a_sparse_integer_key_answers_exactly() {
    for layout in [Layout::Settled, Layout::SettledWithWrites] {
        let ledger = ledger(layout, 30_000, 9_000);
        let answer = check(&ledger, &[Key::Account], 256 << 20, &format!("{layout:?}"));
        if enabled() {
            assert!(
                answer.notes.contains("packed-key fold:"),
                "a sparse key left the packed-key fold: {}",
                answer.notes
            );
        }
    }
}

#[test]
fn a_group_shows_the_spelling_of_its_first_row() {
    let ledger = ledger(Layout::Settled, 30_000, 50);
    let keys = [Key::Account, Key::Branch];
    let sql = "SELECT account, branch, COUNT(*) FROM ledger GROUP BY account, branch";
    let answer = run(&ledger.table, &ledger.catalog, sql, 256 << 20);
    let wanted = expected(&ledger.model, &keys);
    let mut spellings = 0;
    for row in &answer.rows {
        let key = vec![row[0].clone(), class_of(&row[1])];
        let totals = wanted.get(&key).expect("a group of the model");
        assert_eq!(
            row[1], totals.shown[0],
            "group {key:?} shows another row's spelling"
        );
        spellings += usize::from(row[1] != key[1]);
    }
    assert_eq!(answer.rows.len(), wanted.len());
    assert!(
        spellings > 0,
        "no group showed a spelling other than its class's"
    );
}

#[test]
fn groups_past_the_ceiling_go_to_disk_and_answer_exactly() {
    let ledger = ledger(Layout::SettledWithWrites, 250_000, 60_000);
    for keys in [SHAPES[0], SHAPES[4]] {
        let roomy = check(&ledger, keys, 1 << 30, "roomy");
        let tight = check(&ledger, keys, 40 << 20, "tight");
        assert_eq!(roomy.rows.len(), tight.rows.len());
        if enabled() {
            assert!(
                tight.notes.contains("packed-key fold:") && !tight.notes.contains(", 0 spill runs"),
                "the tight run did not spill: {}",
                tight.notes
            );
        }
    }
}

/// Writes the ledger's rows and this engine's answers where a live `MySQL`
/// can load the one and be asked for the other: `PACKED_GROUP_DUMP` names
/// the file prefix, one `rows.<layout>` file of tab-separated rows per
/// layout, and each answer is printed as `PAIR<tab>layout<tab>sql<tab>rows`
/// with the rows sorted, cells joined by `,` and rows by `|`.
#[test]
#[ignore = "dumps rows and answers for a comparison against a live server"]
fn dump_for_a_live_pair() {
    use std::fmt::Write as _;
    let Some(prefix) = std::env::var_os("PACKED_GROUP_DUMP") else {
        return;
    };
    let prefix = prefix.to_string_lossy().into_owned();
    for layout in [Layout::Settled, Layout::SettledWithWrites] {
        let ledger = ledger(layout, 30_000, 700);
        let mut rows = String::new();
        for (id, entry) in &ledger.model {
            let cell = |value: Option<String>| value.unwrap_or_else(|| "\\N".to_owned());
            writeln!(
                rows,
                "{id}\t{}\t{}\t{}\t{}\t{}\t{}",
                cell(entry.account.map(|account| account.to_string())),
                cell(entry.branch.map(|branch| BRANCHES[branch].0.to_owned())),
                cell(entry.day.clone()),
                cell(entry.rate.map(ToOwned::to_owned)),
                cell(entry.qty.map(|qty| qty.to_string())),
                cell(entry.fee.map(|fee| scaled(i128::from(fee), 2))),
            )
            .expect("row");
        }
        std::fs::write(format!("{prefix}.{layout:?}"), rows).expect("dump");
        let mut shapes = SHAPES.to_vec();
        shapes.push(&[Key::Account]);
        for keys in shapes {
            let list = keys
                .iter()
                .map(|key| key.column())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT {list}, {AGGREGATES} FROM ledger GROUP BY {list}");
            let mut answer = run(&ledger.table, &ledger.catalog, &sql, 256 << 20)
                .rows
                .iter()
                .map(|row| row.join(","))
                .collect::<Vec<_>>();
            answer.sort();
            println!("PAIR\t{layout:?}\t{sql}\t{}", answer.join("|"));
        }
    }
}

mod bench {
    use super::{
        CatalogSnapshot, Column, DataType, DatabaseEntry, DatabaseId, KeyPart, PrimaryKey,
        StoreOptions, StoredRow, TableEntry, TableId, TableSchema, TableStatistics, TableStore,
        Value, mix, pull,
    };

    const REGIONS: [&str; 8] = [
        "north", "south", "east", "west", "upper", "lower", "inner", "outer",
    ];

    fn schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "qty", DataType::Int64, true),
                Column::new(3, "grp", DataType::Int64, false),
                Column::new(4, "region", DataType::Utf8, false),
            ],
        )
        .expect("schema")
    }

    fn fixture(rows: u64, groups: u64) -> (tempfile::TempDir, TableStore, CatalogSnapshot) {
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
        let row = |id: u64| {
            let h = mix(id);
            let qty = i64::try_from(h % 100_000).expect("small") - 20_000;
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(id),
                    if mix(id ^ 0x11).is_multiple_of(17) {
                        Value::Null
                    } else {
                        Value::Int64(qty)
                    },
                    Value::Int64(i64::try_from((id % groups) * 1_000_003).expect("small")),
                    Value::Utf8(REGIONS[usize::try_from((h >> 40) % 8).expect("small")].to_owned()),
                ],
                1,
                false,
            )
        };
        let mut start = 0;
        while start < rows {
            let end = (start + 100_000).min(rows);
            table
                .bulk_ingest_snapshot((start..end).map(row).collect())
                .expect("ingest");
            start = end;
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "ledger",
            schema(),
            TableStatistics::with_row_count(rows),
        )
        .expect("entry");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        (directory, table, catalog)
    }

    /// User and system time this process has used, in milliseconds.
    fn process_cpu_ms() -> f64 {
        let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
            return 0.0;
        };
        let fields = stat
            .rsplit_once(") ")
            .map(|(_, rest)| rest.split_whitespace().collect::<Vec<_>>())
            .unwrap_or_default();
        let ticks = |index: usize| {
            fields
                .get(index)
                .and_then(|field| field.parse::<u32>().ok())
                .unwrap_or(0)
        };
        // Clock ticks are a hundredth of a second on the hosts this runs on.
        f64::from(ticks(11) + ticks(12)) * 10.0
    }

    #[test]
    #[ignore = "bench: prints the cost of grouping by a sparse or composite key"]
    fn sparse_and_composite_keys() {
        let rows: u64 = std::env::var("BENCH_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_000_000);
        let runs: usize = std::env::var("BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(9);
        println!("shape\tgroups\tmedian_ms\tmin_ms\tcpu_ms\tpeak_mib\tbytes_per_group\tpath");
        for groups in [100_000_u64, 1_000_000] {
            let (_directory, table, catalog) = fixture(rows, groups);
            for (shape, sql) in [
                (
                    "sparse integer",
                    "SELECT grp, COUNT(*), SUM(qty) FROM ledger GROUP BY grp",
                ),
                (
                    "integer + text",
                    "SELECT grp, region, COUNT(*), SUM(qty) FROM ledger GROUP BY grp, region",
                ),
            ] {
                let _ = pull(&table, &catalog, sql, 8 << 30, false);
                let before = process_cpu_ms();
                let mut times = Vec::new();
                let mut last = None;
                for _ in 0..runs {
                    let started = std::time::Instant::now();
                    last = Some(pull(&table, &catalog, sql, 8 << 30, false));
                    times.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                #[allow(clippy::cast_precision_loss)]
                let cpu = (process_cpu_ms() - before) / runs as f64;
                times.sort_by(f64::total_cmp);
                let answer = last.expect("a run");
                #[allow(clippy::cast_precision_loss)]
                let peak = answer.peak_reserved as f64;
                #[allow(clippy::cast_precision_loss)]
                let per_group = peak / answer.groups.max(1) as f64;
                println!(
                    "{shape}\t{}\t{:.1}\t{:.1}\t{cpu:.0}\t{:.1}\t{per_group:.0}\t{}",
                    answer.groups,
                    times[times.len() / 2],
                    times[0],
                    peak / 1_048_576.0,
                    answer.notes
                );
            }
        }
    }
}
