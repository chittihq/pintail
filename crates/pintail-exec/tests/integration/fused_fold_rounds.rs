//! An aggregate over a scan folds the table in place: each worker decodes a
//! slice and folds its rows into its own partial, instead of the scan
//! queueing batches for a window another core reads back.
//!
//! The workers reach the table's slices in no order, so the answers here
//! are the ones that could show it: a text group's spelling is its first
//! row's in key order, and the table is built so a class's later spellings
//! and a class of its own arrive mid-table, after the rounds have begun.
//! Every statement is checked against totals computed from the rows
//! themselves, over settled segments and again with rows in the memtable -
//! new keys and a replaced row - and under a ceiling small enough that
//! rounds hand batches back.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const AI_CI: &str = "utf8mb4_0900_ai_ci";
const ROWS: u64 = 600_000;
/// Rows before this carry the first spellings; the rest bring a second
/// spelling of each class and a class of their own.
const TURN: u64 = 330_000;

/// One row: `(account, state, kind, day, cents)`.
#[derive(Clone)]
struct Visit {
    account: Option<i64>,
    state: &'static str,
    kind: Option<&'static str>,
    /// `(year, month, day of month)`
    day: (u32, u32, u32),
    cents: Option<i64>,
}

fn visit(id: u64) -> Visit {
    let small = i64::try_from(id).expect("small id");
    let late = id >= TURN;
    Visit {
        account: (!id.is_multiple_of(23)).then_some(small % 5_000 + 100),
        state: match (id % 3, late) {
            (0, true) => "OPEN",
            (1, false) => "closed",
            (1, true) => "Closed",
            (_, false) => "open",
            (_, true) => "held",
        },
        kind: (!id.is_multiple_of(17)).then_some(match (id % 4, late) {
            (0 | 1, false) => "walk",
            (0 | 1, true) => "Walk",
            _ => "ride",
        }),
        day: (
            2021 + u32::try_from(id % 4).expect("small"),
            1 + u32::try_from((id / 7) % 12).expect("small"),
            1 + u32::try_from(id % 28).expect("small"),
        ),
        cents: (!id.is_multiple_of(13)).then_some((small * 7_919) % 900_000 - 200_000),
    }
}

/// The replaced row's new contents: a spelling of its own, so the overlay
/// decides which row a group sees first.
fn replaced(id: u64) -> Visit {
    Visit {
        state: "HELD",
        kind: Some("RIDE"),
        cents: Some(12_345),
        ..visit(id)
    }
}

fn money(cents: i64) -> String {
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

fn stored(id: u64, visit: &Visit, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            visit.account.map_or(Value::Null, Value::Int64),
            Value::Utf8(visit.state.to_owned()),
            visit
                .kind
                .map_or(Value::Null, |kind| Value::Utf8(kind.to_owned())),
            Value::Utf8(format!(
                "{}-{:02}-{:02}",
                visit.day.0, visit.day.1, visit.day.2
            )),
            visit
                .cents
                .map_or(Value::Null, |cents| Value::Utf8(money(cents))),
        ],
        version,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    /// The table's rows by id, as the statements should see them.
    rows: BTreeMap<u64, Visit>,
}

fn fixture() -> Fixture {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::Int64, true),
            Column::new(3, "state", DataType::Utf8, false).with_collation(Some(AI_CI.to_owned())),
            Column::new(4, "kind", DataType::Utf8, true).with_collation(Some(AI_CI.to_owned())),
            Column::new(5, "day", DataType::Date32, false),
            Column::new(
                6,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let mut start = 0;
    while start < ROWS {
        let end = (start + 40_000).min(ROWS);
        table
            .bulk_ingest_snapshot((start..end).map(|id| stored(id, &visit(id), 1)).collect())
            .expect("ingest");
        start = end;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "visits",
        schema,
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    Fixture {
        _directory: directory,
        table,
        catalog,
        rows: (0..ROWS).map(|id| (id, visit(id))).collect(),
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

/// The statement's rows, sorted, under a query ceiling of `limit` bytes.
fn run(fixture: &Fixture, sql: &str, limit: usize) -> Vec<Vec<String>> {
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(plan, &provider, limit, Collation::default()).expect("execution");
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
                    .map(|column| render(column.value(row).expect("value")))
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows.sort();
    rows
}

/// Per group, its key columns as its first row in key order spells them,
/// its row count and the sum of its amounts.
fn expected<K: Ord>(
    fixture: &Fixture,
    keep: impl Fn(u64) -> bool,
    class: impl Fn(&Visit) -> K,
    shown: impl Fn(&Visit) -> Vec<String>,
) -> Vec<Vec<String>> {
    let mut groups = BTreeMap::<K, (Vec<String>, u64, Option<i64>)>::new();
    for (id, visit) in &fixture.rows {
        if !keep(*id) {
            continue;
        }
        let group = groups
            .entry(class(visit))
            .or_insert_with(|| (shown(visit), 0, None));
        group.1 += 1;
        if let Some(cents) = visit.cents {
            group.2 = Some(group.2.unwrap_or(0) + cents);
        }
    }
    let mut rows = groups
        .into_values()
        .map(|(mut shown, count, sum)| {
            shown.push(count.to_string());
            shown.push(sum.map_or_else(|| "NULL".to_owned(), money));
            shown
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn text(value: Option<&str>) -> String {
    value.unwrap_or("NULL").to_owned()
}

/// Whether this process folds in place at all: the switch that turns the
/// path off is honoured by the tests that count its rounds.
fn fused() -> bool {
    std::env::var_os("PINTAIL_DISABLE_FUSED_FOLD").is_none()
}

/// Every shape the rounds fold, each against the rows themselves, over
/// the rows from `from` on: a statement of its own each time, so none is
/// answered from what an earlier one left.
#[allow(clippy::too_many_lines)] // one statement per shape, each with its expectation
fn check_shapes(fixture: &Fixture, limit: usize, counted: bool, from: u64) {
    type Keep = Box<dyn Fn(u64) -> bool>;
    // Whether the rounds' workers answer the filter themselves. A
    // predicate no batch kernel answers is the query's own thread's: the
    // rounds hand every batch back, and the answer is the same.
    let filters: [(String, Keep, bool); 4] = [
        (
            format!("WHERE id >= {from}"),
            Box::new(move |id| id >= from),
            true,
        ),
        (
            format!("WHERE id >= {from} AND account > 1000"),
            Box::new(move |id| id >= from && visit(id).account.is_some_and(|key| key > 1_000)),
            true,
        ),
        (
            format!("WHERE id >= {from} AND id % 3 <> 1"),
            Box::new(move |id| id >= from && id % 3 != 1),
            false,
        ),
        (
            format!("WHERE id >= {}", 100_000 + from),
            Box::new(move |id| id >= 100_000 + from),
            true,
        ),
    ];
    for (filter, keep, on_workers) in &filters {
        // An integer key: the range fold.
        let _ = pintail_exec::take_exec_counters();
        assert_eq!(
            run(
                fixture,
                &format!(
                    "SELECT account, COUNT(*), SUM(amount) FROM visits {filter} GROUP BY account"
                ),
                limit
            ),
            expected(
                fixture,
                keep,
                |visit| visit.account,
                |visit| vec![
                    visit
                        .account
                        .map_or_else(|| "NULL".to_owned(), |key| key.to_string())
                ],
            ),
            "integer key {filter}"
        );
        let range = pintail_exec::take_exec_counters();
        // One text key: the dense slots, by collation class.
        assert_eq!(
            run(
                fixture,
                &format!("SELECT state, COUNT(*), SUM(amount) FROM visits {filter} GROUP BY state"),
                limit
            ),
            expected(
                fixture,
                keep,
                |visit| visit.state.to_lowercase(),
                |visit| vec![visit.state.to_owned()],
            ),
            "text key {filter}"
        );
        let dense = pintail_exec::take_exec_counters();
        // Two text keys: each group shows its own first row.
        assert_eq!(
            run(
                fixture,
                &format!(
                    "SELECT state, kind, COUNT(*), SUM(amount) FROM visits {filter} \
                     GROUP BY state, kind"
                ),
                limit
            ),
            expected(
                fixture,
                keep,
                |visit| (
                    visit.state.to_lowercase(),
                    visit.kind.map(str::to_lowercase)
                ),
                |visit| vec![visit.state.to_owned(), text(visit.kind)],
            ),
            "text pair {filter}"
        );
        let pair = pintail_exec::take_exec_counters();
        // Date parts: the dense date slots.
        assert_eq!(
            run(
                fixture,
                &format!(
                    "SELECT YEAR(day) AS y, MONTH(day) AS m, COUNT(*), SUM(amount) FROM visits \
                     {filter} GROUP BY y, m"
                ),
                limit
            ),
            expected(
                fixture,
                keep,
                |visit| (visit.day.0, visit.day.1),
                |visit| vec![visit.day.0.to_string(), visit.day.1.to_string()],
            ),
            "date parts {filter}"
        );
        let dates = pintail_exec::take_exec_counters();
        if counted && *on_workers && fused() {
            for (shape, counters) in [
                ("integer key", range),
                ("text key", dense),
                ("text pair", pair),
                ("date parts", dates),
            ] {
                assert!(
                    counters.fused_rounds > 0 && counters.fused_batches > 0,
                    "{shape} {filter}: {counters:?}"
                );
            }
        }
    }
}

#[test]
fn settled_segments_fold_in_place_to_the_rows_own_totals() {
    let fixture = fixture();
    // More than once: which worker reaches which slice differs by run.
    for turn in 0..3 {
        check_shapes(&fixture, 256 << 20, true, turn);
    }
}

#[test]
fn memtable_rows_and_a_replaced_row_are_seen_by_the_rounds() {
    let mut fixture = fixture();
    let mut fresh = (ROWS..ROWS + 3_000)
        .map(|id| (id, visit(id)))
        .collect::<Vec<_>>();
    // The first row of the table, replaced: the groups it led now begin
    // at their next row, and the groups it joins see it first.
    fresh.push((0, replaced(0)));
    fresh.push((TURN + 2, replaced(TURN + 2)));
    fixture
        .table
        .ingest(
            fresh
                .iter()
                .map(|(id, visit)| stored(*id, visit, 2))
                .collect(),
        )
        .expect("memtable rows");
    fixture.rows.extend(fresh);
    for turn in 0..2 {
        check_shapes(&fixture, 256 << 20, false, turn);
    }
}

#[test]
fn a_small_ceiling_still_answers() {
    let fixture = fixture();
    check_shapes(&fixture, 24 << 20, false, 0);
}

const READINGS: u64 = 200_000;

/// The amount of lane `lane` (1 to 7) in row `id`, in cents.
fn reading_cents(id: u64, lane: u64) -> i64 {
    i64::try_from(id * lane % 10_000).expect("small")
}

/// A table for a date-part key over a wide domain: seven amounts, a
/// timestamp whose year and second take every value the rows give them.
fn readings() -> (tempfile::TempDir, TableStore, CatalogSnapshot) {
    let decimal = DataType::Decimal {
        precision: 12,
        scale: 2,
    };
    let mut columns = vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "ts", DataType::DateTime64 { fsp: 0 }, false),
    ];
    for (offset, name) in ["a", "b", "c", "d", "e", "f", "g"].into_iter().enumerate() {
        columns.push(Column::new(
            3 + u32::try_from(offset).expect("small"),
            name,
            decimal,
            false,
        ));
    }
    let schema = TableSchema::new(1, columns).expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let row = |id: u64| {
        let mut values = vec![
            Value::UInt64(id),
            Value::Utf8(format!("202{}-03-04 05:06:{:02}", id % 4, id % 60)),
        ];
        values.extend((1..=7).map(|lane| Value::Utf8(money(reading_cents(id, lane)))));
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            values,
            1,
            false,
        )
    };
    let mut start = 0;
    while start < READINGS {
        let end = (start + 20_000).min(READINGS);
        table
            .bulk_ingest_snapshot((start..end).map(row).collect())
            .expect("ingest");
        start = end;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "readings",
        schema,
        TableStatistics::with_row_count(READINGS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    (directory, table, catalog)
}

/// Each worker of a date-part round keeps totals over every slot of the
/// key's domain until the rounds stop. Those totals are charged before
/// they are made: under a ceiling they do not fit, the rounds stand aside
/// and the driver's own path answers the same rows.
#[test]
fn date_part_rounds_charge_their_workers_totals() {
    let (directory, table, catalog) = readings();
    let fixture = Fixture {
        _directory: directory,
        table,
        catalog,
        rows: BTreeMap::new(),
    };
    let mut totals = BTreeMap::<(u64, u64), [i64; 7]>::new();
    for id in 0..READINGS {
        let group = totals.entry((2020 + id % 4, id % 60)).or_default();
        for (lane, total) in (1..).zip(group.iter_mut()) {
            *total += reading_cents(id, lane);
        }
    }
    let mut expected = totals
        .into_iter()
        .map(|((year, second), sums)| {
            let mut row = vec![year.to_string(), second.to_string()];
            row.extend(sums.into_iter().map(money));
            row
        })
        .collect::<Vec<_>>();
    expected.sort();
    let sql = "SELECT YEAR(ts) AS y, SECOND(ts) AS s, SUM(a), SUM(b), SUM(c), SUM(d), \
               SUM(e), SUM(f), SUM(g) FROM readings GROUP BY y, s";
    // Every seat's totals: a row count, and a total and a NULL count per
    // lane, over 257 years by 61 seconds.
    let seats_bytes = (rayon::current_num_threads() + 1) * 257 * 61 * (8 + 7 * (16 + 8));

    let _ = pintail_exec::take_exec_counters();
    assert_eq!(run(&fixture, sql, 256 << 20), expected, "roomy ceiling");
    let roomy = pintail_exec::take_exec_counters();
    if fused() {
        assert!(roomy.fused_rounds > 0, "{roomy:?}");
    }

    // A statement of its own, or the first one's settled answer is reused.
    let tight = 20 << 20;
    let tight_sql = sql.replace("FROM readings", "FROM readings WHERE id < 1000000");
    assert_eq!(run(&fixture, &tight_sql, tight), expected, "tight ceiling");
    let counters = pintail_exec::take_exec_counters();
    if seats_bytes > tight {
        assert_eq!(
            counters.fused_rounds, 0,
            "{seats_bytes} bytes of totals under a {tight}-byte ceiling: {counters:?}"
        );
    }
}
