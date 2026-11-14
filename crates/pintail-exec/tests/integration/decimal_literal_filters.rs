//! A DECIMAL column compared with a decimal literal is answered from the
//! column's packed units.
//!
//! `amount > 998.50`, `amount BETWEEN 777.00 AND 778.50`, `amount = 777.25`
//! and `amount IN (777.25, 3.50)` name their rows by one integer comparison
//! per value: the literal at the column's scale is an integer of the same
//! units the column stores. Each of them used to leave the packed path for
//! the row path, which spelled every value as text to compare it. The
//! expectations are computed here, in units, not by the engine; the path is
//! pinned by the values the query materialized.

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

const ROWS: u64 = 40_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(
                2,
                "amount",
                DataType::Decimal {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
    .expect("schema")
}

/// The row's amount in hundredths, on both sides of zero; every eleventh
/// row has none.
fn units(id: u64) -> Option<i64> {
    (!id.is_multiple_of(11)).then(|| i64::try_from(id * 7919 % 100_000).expect("small") - 50_000)
}

fn spell(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let magnitude = units.unsigned_abs();
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            units(id).map_or(Value::Null, |units| Value::Utf8(spell(units))),
        ],
        id + 1,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        Self::of(&schema(), (0..ROWS).map(row).collect())
    }

    /// The table `ledger` under `schema`, holding `rows`.
    fn of(schema: &TableSchema, rows: Vec<StoredRow>) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        let count = u64::try_from(rows.len()).expect("a count");
        table.bulk_ingest_snapshot(rows).expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "ledger",
            schema.clone(),
            TableStatistics::with_row_count(count),
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

    /// The first column of every row `sql` answers, and the cells the query
    /// materialized into values on the way.
    fn run(&self, sql: &str) -> (Vec<u64>, u64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let _ = take_exec_counters();
        let mut execution =
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut answers = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                match batch.column(0).and_then(|column| column.value_owned(row)) {
                    Some(Value::UInt64(value)) => answers.push(value),
                    Some(Value::Int64(value)) => {
                        answers.push(u64::try_from(value).expect("a count"));
                    }
                    other => panic!("{sql}: unexpected answer {other:?}"),
                }
            }
        }
        answers.sort_unstable();
        (answers, take_exec_counters().values_materialized)
    }
}

/// Each predicate with the test of a row's units that names the same rows.
#[allow(clippy::type_complexity)]
fn shapes() -> Vec<(&'static str, Box<dyn Fn(i64) -> bool>)> {
    vec![
        ("amount > 498.50", Box::new(|units| units > 49_850)),
        ("amount >= 498.5", Box::new(|units| units >= 49_850)),
        ("amount < -498.50", Box::new(|units| units < -49_850)),
        ("-498.50 >= amount", Box::new(|units| units <= -49_850)),
        ("amount <= 1.5", Box::new(|units| units <= 150)),
        (
            "amount BETWEEN 277.00 AND 278.50",
            Box::new(|units| (27_700..=27_850).contains(&units)),
        ),
        (
            "amount BETWEEN -1.25 AND 1",
            Box::new(|units| (-125..=100).contains(&units)),
        ),
        (
            "amount >= 277.00 AND amount <= 278.50",
            Box::new(|units| (27_700..=27_850).contains(&units)),
        ),
        ("amount = 277.25", Box::new(|units| units == 27_725)),
        ("amount = -3.5", Box::new(|units| units == -350)),
        ("amount <> 277.25", Box::new(|units| units != 27_725)),
        (
            "amount IN (277.25, -3.50, 12)",
            Box::new(|units| [27_725, -350, 1_200].contains(&units)),
        ),
        // Digits past the column's scale: no stored value equals the
        // literal, and an ordering falls between two stored values.
        ("amount = 277.255", Box::new(|_| false)),
        ("amount > 277.255", Box::new(|units| units > 27_725)),
        ("amount >= 277.255", Box::new(|units| units > 27_725)),
        ("amount < 277.255", Box::new(|units| units <= 27_725)),
        ("amount <= -277.255", Box::new(|units| units < -27_725)),
        ("amount > -277.255", Box::new(|units| units >= -27_725)),
        ("amount > 277.250", Box::new(|units| units > 27_725)),
        // Whole numbers, which already compared packed.
        ("amount > 498", Box::new(|units| units > 49_800)),
        ("amount = 12", Box::new(|units| units == 1_200)),
    ]
}

#[test]
fn a_decimal_literal_names_its_rows_from_the_packed_units() {
    let fixture = Fixture::new();
    // Every shape is run before any is judged, so one report names them all.
    let mut failures = Vec::new();
    for (predicate, keeps) in shapes() {
        let expected = (0..ROWS)
            .filter(|id| units(*id).is_some_and(&keeps))
            .collect::<Vec<_>>();
        let count = vec![u64::try_from(expected.len()).expect("a count")];
        for (sql, expected) in [
            (
                format!("SELECT id FROM ledger WHERE {predicate}"),
                &expected,
            ),
            (
                format!("SELECT COUNT(*) FROM ledger WHERE {predicate}"),
                &count,
            ),
        ] {
            let (answers, materialized) = fixture.run(&sql);
            if answers != *expected {
                failures.push(format!(
                    "{sql}: {} rows where {} were expected",
                    answers.len(),
                    expected.len()
                ));
            }
            if materialized >= ROWS {
                failures.push(format!(
                    "{sql}: {materialized} cells materialized for {ROWS} rows - the filter went \
                     row by row"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A decimal literal compares as a decimal, digit for digit. Read as a
/// double it cannot tell these amounts apart - they differ in the
/// nineteenth digit - nor these whole numbers, past the fifty-third bit.
/// The expectations are `MySQL` 8.4's answers for the same rows.
#[test]
fn a_decimal_literal_compares_exactly_past_a_doubles_digits() {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(
                2,
                "amount",
                DataType::Decimal {
                    precision: 20,
                    scale: 2,
                },
                true,
            ),
            Column::new(3, "units", DataType::Int64, false),
        ],
    )
    .expect("schema");
    let rows = [
        (1, Some("12345678901234567.88"), 9_007_199_254_740_992_i64),
        (2, Some("12345678901234567.89"), 9_007_199_254_740_993),
        (3, Some("12345678901234567.90"), 9_007_199_254_740_994),
        (4, None, 5),
    ]
    .into_iter()
    .map(|(id, amount, units)| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            vec![
                Value::UInt64(id),
                amount.map_or(Value::Null, |amount| Value::Utf8(amount.to_owned())),
                Value::Int64(units),
            ],
            id,
            false,
        )
    })
    .collect();
    let fixture = Fixture::of(&schema, rows);
    for (predicate, expected) in [
        ("amount > 12345678901234567.89", vec![3]),
        ("amount >= 12345678901234567.89", vec![2, 3]),
        ("amount = 12345678901234567.89", vec![2]),
        ("amount <> 12345678901234567.89", vec![1, 3]),
        ("amount < 12345678901234567.89", vec![1]),
        (
            "amount BETWEEN 12345678901234567.89 AND 12345678901234567.90",
            vec![2, 3],
        ),
        ("amount IN (12345678901234567.89, 1.5)", vec![2]),
        ("units > 9007199254740992.5", vec![2, 3]),
        ("units = 9007199254740993.0", vec![2]),
        ("units < 9007199254740993.5", vec![1, 2, 4]),
        ("units <= 5.0", vec![4]),
    ] {
        let (ids, _) = fixture.run(&format!("SELECT id FROM ledger WHERE {predicate}"));
        assert_eq!(ids, expected, "{predicate}");
    }
}
