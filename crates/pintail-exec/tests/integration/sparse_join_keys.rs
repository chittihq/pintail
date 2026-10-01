//! Joins on integer keys spread too widely for a direct-address table - ids
//! with long gaps - against answers computed here from the rows themselves:
//! folded into the aggregate above by column and by row, with the row
//! fold's states kept per worker and opened per morsel, and through the
//! join operator with and without a residual. The probe key carries NULLs,
//! negatives and values no build row holds; one build repeats every key and
//! one holds its keys unsigned.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const FACTS: i64 = 120_000;
const DIMS: i64 = 300;
const ZONES: [&str; 5] = ["amber", "basalt", "cobalt", "dune", "ember"];

/// A small number as a key far from its neighbours.
fn spread(number: i64) -> i64 {
    number * 7_919_000_003
}

/// The dimension a fact row points at: below, inside (with gaps) and above
/// the dimension's range, or nothing.
fn fact_dim(id: i64) -> Option<i64> {
    (id % 13 != 0).then_some((id * 37) % 326 - 5)
}

fn fact_cents(id: i64) -> Option<i64> {
    (id % 11 != 0).then(|| (id * 7_919) % 200_000 - 50_000)
}

fn cents_text(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", cents.abs() / 100, cents.abs() % 100)
}

fn dim_present(id: i64) -> bool {
    (1..=DIMS).contains(&id) && id % 7 != 0
}

fn zone(id: i64) -> &'static str {
    ZONES[usize::try_from(id % 5).expect("small")]
}

struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

fn table(
    directory: &std::path::Path,
    name: &str,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
) -> (TableStore, TableSchema, u64) {
    let schema = TableSchema::new(1, columns).expect("schema");
    let mut store = TableStore::open(
        directory.join(name),
        schema.clone(),
        StoreOptions::default(),
    )
    .expect("store");
    let count = u64::try_from(rows.len()).expect("rows");
    store
        .bulk_ingest_snapshot(
            rows.into_iter()
                .enumerate()
                .map(|(index, values)| {
                    let key = u64::try_from(index + 1).expect("key");
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                        values,
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    (store, schema, count)
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let facts = table(
            directory.path(),
            "facts",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "wide", DataType::Int64, true),
                Column::new(
                    3,
                    "amount",
                    DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    true,
                ),
            ],
            (1..=FACTS)
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        fact_dim(id).map_or(Value::Null, |dim| Value::Int64(spread(dim))),
                        fact_cents(id).map_or(Value::Null, |cents| Value::Utf8(cents_text(cents))),
                    ]
                })
                .collect(),
        );
        // One row per key, signed and unsigned.
        let dims = table(
            directory.path(),
            "dims",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "wide", DataType::Int64, false),
                Column::new(3, "uwide", DataType::UInt64, false),
                Column::new(4, "zone", DataType::Utf8, false),
                Column::new(5, "label", DataType::Utf8, false),
            ],
            (1..=DIMS)
                .filter(|id| dim_present(*id))
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        Value::Int64(spread(id)),
                        Value::UInt64(u64::try_from(spread(id)).expect("positive")),
                        Value::Utf8(zone(id).to_owned()),
                        Value::Utf8(format!("label-{id:04}")),
                    ]
                })
                .collect(),
        );
        // Two rows per key, the second far from the first.
        let pairs = table(
            directory.path(),
            "pairs",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "wide", DataType::Int64, false),
                Column::new(3, "zone", DataType::Utf8, false),
            ],
            (1..=2 * DIMS)
                .map(|id| {
                    let dim = (id - 1) % DIMS + 1;
                    vec![
                        Value::Int64(id),
                        Value::Int64(spread(dim)),
                        Value::Utf8(zone(dim).to_owned()),
                    ]
                })
                .collect(),
        );
        let mut stores = Vec::new();
        let mut entries = Vec::new();
        for (index, (name, (store, schema, count))) in
            [("facts", facts), ("dims", dims), ("pairs", pairs)]
                .into_iter()
                .enumerate()
        {
            entries.push(
                TableEntry::new(
                    TableId::new(u64::try_from(index + 1).expect("table")),
                    name,
                    schema,
                    TableStatistics::with_row_count(count),
                )
                .expect("entry")
                .with_key_columns([1])
                .expect("key"),
            );
            stores.push(store);
        }
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            stores,
            catalog,
        }
    }

    fn run(&self, sql: &str) -> (Vec<String>, String) {
        let snapshots: Vec<_> = self.stores.iter().map(TableStore::snapshot).collect();
        let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
            (
                DatabaseId::new(1),
                TableId::new(u64::try_from(i + 1).expect("table")),
                s,
            )
        }))
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(plan, &provider, 1 << 31, None, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            let value = batch
                                .column(column)
                                .and_then(|column| column.value_owned(row))
                                .expect("value");
                            match value {
                                Value::Null => "NULL".to_owned(),
                                Value::Int64(number) => number.to_string(),
                                Value::UInt64(number) => number.to_string(),
                                value => value
                                    .text()
                                    .map_or_else(|| format!("{value:?}"), str::to_owned),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                );
            }
        }
        let profile = execution
            .profile()
            .map(|profile| profile.render())
            .unwrap_or_default();
        (rows, profile)
    }
}

fn join_line(profile: &str) -> &str {
    profile
        .lines()
        .find(|line| line.trim_start().starts_with("HashJoin"))
        .unwrap_or_else(|| panic!("no join in:\n{profile}"))
}

/// What a group of the star join holds: its rows, its rows with an amount,
/// and that amount's sum, smallest and largest.
#[derive(Clone, Copy, Default)]
struct Totals {
    rows: u64,
    valid: u64,
    cents: i64,
    least: Option<i64>,
    most: Option<i64>,
}

impl Totals {
    fn add(&mut self, cents: Option<i64>, times: u64) {
        self.rows += times;
        if let Some(cents) = cents {
            self.valid += times;
            self.cents += cents * i64::try_from(times).expect("small");
            self.least = Some(self.least.map_or(cents, |least| least.min(cents)));
            self.most = Some(self.most.map_or(cents, |most| most.max(cents)));
        }
    }

    fn row(&self, label: &str) -> String {
        let text = |cents: Option<i64>| cents.map_or_else(|| "NULL".to_owned(), cents_text);
        format!(
            "{label}|{}|{}|{}|{}|{}",
            self.rows,
            self.valid,
            text((self.valid > 0).then_some(self.cents)),
            text(self.least),
            text(self.most),
        )
    }
}

/// The star join's groups by `label`, each match counted `times` times;
/// with `outer`, the rows matching nothing under `NULL`, which sorts first.
fn expected(label: impl Fn(i64) -> String, times: u64, outer: bool) -> Vec<String> {
    let mut groups = BTreeMap::<String, Totals>::new();
    let mut unmatched = Totals::default();
    for id in 1..=FACTS {
        match fact_dim(id).filter(|dim| dim_present(*dim)) {
            Some(dim) => groups
                .entry(label(dim))
                .or_default()
                .add(fact_cents(id), times),
            None => unmatched.add(fact_cents(id), 1),
        }
    }
    let unmatched = (outer && unmatched.rows > 0).then(|| unmatched.row("NULL"));
    unmatched
        .into_iter()
        .chain(groups.iter().map(|(label, totals)| totals.row(label)))
        .collect()
}

const MEASURES: &str = "COUNT(*), COUNT(f.amount), SUM(f.amount), MIN(f.amount), MAX(f.amount)";
const LANES: &str = "COUNT(*), COUNT(f.amount), SUM(f.amount)";

/// The first three measures of each expected row.
fn lanes_only(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|row| row.split('|').take(4).collect::<Vec<_>>().join("|"))
        .collect()
}

/// A unique spread key folds a column at a time, inner and outer.
#[test]
fn a_unique_spread_key_takes_the_column_fold() {
    let fixture = Fixture::new();
    for (join, outer) in [("JOIN", false), ("LEFT JOIN", true)] {
        let (rows, profile) = fixture.run(&format!(
            "SELECT d.zone, {LANES} FROM facts f {join} dims d ON f.wide = d.wide \
             GROUP BY d.zone ORDER BY d.zone"
        ));
        assert_eq!(
            rows,
            lanes_only(expected(|dim| zone(dim).to_owned(), 1, outer)),
            "{join}"
        );
        let line = join_line(&profile);
        assert!(
            line.contains("column fold on") && !line.contains("row fold"),
            "{profile}"
        );
    }
}

/// Aggregates without a lane take the row fold; none of them depends on
/// the order of its rows, so a worker keeps its states for the whole probe,
/// with five groups and with a group per dimension row.
#[test]
fn the_row_fold_keeps_its_states_per_worker() {
    let fixture = Fixture::new();
    for (join, outer) in [("JOIN", false), ("LEFT JOIN", true)] {
        for (column, label, kept) in [
            (
                "zone",
                (|dim| zone(dim).to_owned()) as fn(i64) -> String,
                "states kept per worker",
            ),
            (
                "label",
                |dim| format!("label-{dim:04}"),
                "states kept per worker",
            ),
        ] {
            let (rows, profile) = fixture.run(&format!(
                "SELECT d.{column}, {MEASURES} FROM facts f {join} dims d ON f.wide = d.wide \
                 GROUP BY d.{column} ORDER BY d.{column}"
            ));
            assert_eq!(rows, expected(label, 1, outer), "{join} by {column}");
            let line = join_line(&profile);
            assert!(
                line.contains("row fold: an aggregate without a column lane")
                    && line.contains(kept),
                "{profile}"
            );
        }
    }
}

/// An aggregate that shows whichever of its equal values came first keeps
/// its states per morsel, and the profile says why.
#[test]
fn an_order_dependent_aggregate_opens_its_states_per_morsel() {
    let fixture = Fixture::new();
    let (rows, profile) = fixture.run(
        "SELECT d.zone, COUNT(*), MAX(d.label) FROM facts f JOIN dims d ON f.wide = d.wide \
         GROUP BY d.zone ORDER BY d.zone",
    );
    let mut most = BTreeMap::<&str, (u64, String)>::new();
    for dim in (1..=FACTS).filter_map(|id| fact_dim(id).filter(|dim| dim_present(*dim))) {
        let entry = most.entry(zone(dim)).or_default();
        entry.0 += 1;
        entry.1 = std::mem::take(&mut entry.1).max(format!("label-{dim:04}"));
    }
    let expected: Vec<String> = most
        .iter()
        .map(|(zone, (rows, label))| format!("{zone}|{rows}|{label}"))
        .collect();
    assert_eq!(rows, expected);
    assert!(
        join_line(&profile).contains("states opened per morsel: an extreme of text"),
        "{profile}"
    );
}

/// A key naming two build rows folds each probe row into both.
#[test]
fn a_repeated_spread_key_folds_every_matching_row() {
    let fixture = Fixture::new();
    let (rows, profile) = fixture.run(&format!(
        "SELECT d.zone, {LANES} FROM facts f JOIN pairs d ON f.wide = d.wide \
         GROUP BY d.zone ORDER BY d.zone"
    ));
    let mut groups = BTreeMap::<&str, Totals>::new();
    for id in 1..=FACTS {
        if let Some(dim) = fact_dim(id).filter(|dim| (1..=DIMS).contains(dim)) {
            groups.entry(zone(dim)).or_default().add(fact_cents(id), 2);
        }
    }
    let expected = lanes_only(
        groups
            .iter()
            .map(|(zone, totals)| totals.row(zone))
            .collect(),
    );
    assert_eq!(rows, expected);
    assert!(
        join_line(&profile).contains("row fold: a build key with several rows"),
        "{profile}"
    );
}

/// Unsigned build keys against a signed probe column: a negative probe key
/// matches nothing, whatever its bits.
#[test]
fn unsigned_build_keys_match_a_signed_probe_by_value() {
    let fixture = Fixture::new();
    for (join, outer) in [("JOIN", false), ("LEFT JOIN", true)] {
        let (rows, _) = fixture.run(&format!(
            "SELECT d.zone, {LANES} FROM facts f {join} dims d ON f.wide = d.uwide \
             GROUP BY d.zone ORDER BY d.zone"
        ));
        assert_eq!(
            rows,
            lanes_only(expected(|dim| zone(dim).to_owned(), 1, outer)),
            "{join}"
        );
    }
}

/// The join operator on its own, and beneath a residual: pairs in probe
/// order, a repeated key's rows in the order the build read them, an outer
/// join's unmatched rows padded.
#[test]
fn the_join_operator_pairs_rows_through_the_spread_keys() {
    let fixture = Fixture::new();
    let mut inner = Vec::new();
    let mut outer = Vec::new();
    for id in 1..=3_000 {
        match fact_dim(id).filter(|dim| (1..=DIMS).contains(dim)) {
            Some(dim) => {
                for row in [format!("{id}|{dim}"), format!("{id}|{}", dim + DIMS)] {
                    inner.push(row.clone());
                    outer.push(row);
                }
            }
            None => outer.push(format!("{id}|NULL")),
        }
    }
    for residual in ["", " AND f.id + d.id > 0"] {
        let (rows, _) = fixture.run(&format!(
            "SELECT f.id, d.id FROM facts f JOIN pairs d ON f.wide = d.wide{residual} \
             WHERE f.id <= 3000 ORDER BY f.id, d.id"
        ));
        assert_eq!(rows, inner, "inner{residual}");
        let (rows, _) = fixture.run(&format!(
            "SELECT f.id, d.id FROM facts f LEFT JOIN pairs d ON f.wide = d.wide{residual} \
             WHERE f.id <= 3000 ORDER BY f.id, d.id"
        ));
        assert_eq!(rows, outer, "outer{residual}");
    }
    let matched = (1..=FACTS)
        .filter(|id| fact_dim(*id).is_some_and(dim_present))
        .count();
    for (sql, expected) in [
        (
            "SELECT COUNT(*) FROM facts f WHERE f.wide IN (SELECT wide FROM dims)",
            matched,
        ),
        (
            "SELECT COUNT(*) FROM facts f WHERE NOT EXISTS \
             (SELECT 1 FROM dims d WHERE d.wide = f.wide)",
            usize::try_from(FACTS).expect("rows") - matched,
        ),
    ] {
        let (rows, _) = fixture.run(sql);
        assert_eq!(rows, [expected.to_string()], "{sql}");
    }
}
