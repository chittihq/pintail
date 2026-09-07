//! `AVG` over a `DECIMAL` column has to be exact, the way `MySQL`'s is.
//!
//! The engine has two representations for a running average: an exact one
//! that accumulates scaled integer units, and an `f64` one. Which is used
//! is decided from the aggregate's planned result type, so a decimal input
//! whose result type is not decimal falls onto `f64` — and `f64` addition
//! is not associative, so the answer then depends on how the rows were
//! split across workers and merged, and moves between runs of the same
//! query on the same data.
//!
//! The fixture puts the exact answer where `f64` cannot hold it: one group
//! of two hundred rows, a single cent among them, whose true average is
//! 0.000050 to the six decimal places `MySQL` gives `AVG` over a
//! `DECIMAL(_,2)`. In binary floating point 0.01/200 lands just under, so
//! the inexact path renders a different value here while agreeing
//! everywhere less delicate.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Rows in the delicate group, chosen so one cent spread across them is
/// exactly half of the last place `AVG`'s result scale keeps.
const GROUP_ROWS: u64 = 200;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::UInt64, false),
            Column::new(
                3,
                "total",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
        ],
    )
    .expect("schema")
}

fn row(id: u64, grp: u64, total: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(grp),
            Value::Utf8(total.to_owned()),
        ],
        id,
        false,
    )
}

/// Group 1: one cent among `GROUP_ROWS` rows, exact average 0.000050.
/// Group 2: a plain average with nothing delicate about it, so a failure
/// in group 1 cannot be dismissed as the fixture being strange.
fn rows() -> Vec<StoredRow> {
    let mut rows = Vec::new();
    for id in 1..=GROUP_ROWS {
        rows.push(row(id, 1, if id == 1 { "0.01" } else { "0.00" }));
    }
    for (offset, total) in ["10.00", "20.00", "30.01"].iter().enumerate() {
        let id = GROUP_ROWS + 1 + offset as u64;
        rows.push(row(id, 2, total));
    }
    rows
}

fn run(sql: &str) -> Vec<Vec<String>> {
    run_on(sql, &rows())
}

fn run_on(sql: &str, fixture: &[StoredRow]) -> Vec<Vec<String>> {
    run_split(sql, fixture, 0)
}

/// `live` rows are applied the way replication applies them - into the
/// memtable, on top of a segment - so the scan meets both a stamped
/// segment and live rows rather than one uniform source. The in-process
/// harness otherwise never exercises that path, and a value's typed
/// representation can differ between the two.
fn run_split(sql: &str, fixture: &[StoredRow], live: usize) -> Vec<Vec<String>> {
    let row_count = fixture.len() as u64;
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    let split = fixture.len().saturating_sub(live);
    let (stamped, streamed) = fixture.split_at(split);
    table
        .bulk_ingest_snapshot(stamped.to_vec())
        .expect("bulk snapshot");
    if !streamed.is_empty() {
        table.ingest_cdc(streamed.to_vec()).expect("cdc ingest");
    }
    let snapshot = table.snapshot();
    let (database_id, table_id) = (DatabaseId::new(15), TableId::new(17));
    let entry = TableEntry::new(
        table_id,
        "orders",
        schema(),
        TableStatistics::with_row_count(row_count),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key columns");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(database_id, "app", [entry]).expect("database")])
            .expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind query");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut out = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            let mut values = Vec::new();
            for column in 0..batch.columns().len() {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value");
                values.push(match value {
                    Value::Utf8(text) => text,
                    Value::UInt64(number) => number.to_string(),
                    Value::Int64(number) => number.to_string(),
                    Value::Null => "NULL".to_owned(),
                    other => format!("{other:?}"),
                });
            }
            out.push(values);
        }
    }
    out.sort();
    out
}

#[test]
fn a_grouped_decimal_average_is_exact_to_its_result_scale() {
    // MySQL widens AVG over DECIMAL(_, 2) to six fraction digits, so the
    // one cent spread over two hundred rows reads 0.000050 and not
    // whatever the nearest double happens to be.
    assert_eq!(
        run("SELECT grp, AVG(total) FROM orders GROUP BY grp"),
        vec![
            vec!["1".to_owned(), "0.000050".to_owned()],
            vec!["2".to_owned(), "20.003333".to_owned()],
        ]
    );
}

#[test]
fn an_ungrouped_decimal_average_is_exact_to_its_result_scale() {
    assert_eq!(
        run("SELECT AVG(total) FROM orders WHERE grp = 1"),
        vec![vec!["0.000050".to_owned()]]
    );
}

/// Many groups, each with its own delicate average, ordered by that
/// average and cut to a limit: the shape a report takes, and the shape
/// that puts the aggregate on the partitioned path where per-worker
/// partial states are merged rather than folded in one place.
///
/// Group `g` holds `g` cents spread over `GROUP_ROWS` rows, so its exact
/// average is `g * 0.000050` — every one of them sitting exactly on the
/// place `ROUND(_, 4)` decides, and every one of them a different value,
/// so an ordering built on inexact averages would also mis-rank them.
#[test]
fn a_report_shaped_decimal_average_is_exact_across_many_groups() {
    let rows = large_fixture();
    let out = run_on(
        "SELECT grp, ROUND(AVG(total), 4) AS avg_total FROM orders GROUP BY grp \
         HAVING COUNT(*) >= 2 ORDER BY avg_total DESC, grp LIMIT 20",
        &rows,
    );
    // The twenty widest averages, descending: groups LARGE_GROUPS down.
    let expected = (0..20)
        .map(|offset| {
            let grp = LARGE_GROUPS - offset;
            let units = u128::from(grp) * 50; // scale 6
            vec![grp.to_string(), round_units_to_four(units)]
        })
        .collect::<Vec<_>>();
    assert_eq!(out.len(), 20, "the limit keeps twenty groups");
    // `run_on` sorts for stability; compare as sets of rows.
    let mut expected_sorted = expected;
    expected_sorted.sort();
    assert_eq!(out, expected_sorted);
}

/// Half of `0.000001` rounds away from zero, as `MySQL` does.
fn round_units_to_four(units: u128) -> String {
    let quarters = units / 100 + u128::from(units % 100 >= 50);
    format!("{}.{:04}", quarters / 10_000, quarters % 10_000)
}

// Capped at `GROUP_ROWS` so group `g` can hold exactly `g` cents.
const LARGE_GROUPS: u64 = GROUP_ROWS;

fn large_fixture() -> Vec<StoredRow> {
    let mut rows = Vec::new();
    let mut id = 1_u64;
    for grp in 1..=LARGE_GROUPS {
        for seat in 0..GROUP_ROWS {
            // `grp` cents, one per row, then zeros.
            let total = if seat < grp { "0.01" } else { "0.00" };
            rows.push(row(id, grp, total));
            id += 1;
        }
    }
    rows
}

#[test]
fn rounding_a_decimal_average_agrees_with_the_exact_value() {
    // ROUND at the place the average's last digit decides: exact 0.000050
    // rounds away from zero to 0.0001, and a double a hair under it does
    // not.
    assert_eq!(
        run("SELECT ROUND(AVG(total), 4) FROM orders WHERE grp = 1"),
        vec![vec!["0.0001".to_owned()]]
    );
}

/// The same report, with a quarter of every group's rows arriving the way
/// replication delivers them instead of in the stamped snapshot.
#[test]
fn a_report_shaped_decimal_average_is_exact_over_live_rows() {
    let rows = large_fixture();
    let live = rows.len() / 4;
    let out = run_split(
        "SELECT grp, ROUND(AVG(total), 4) AS avg_total FROM orders GROUP BY grp \
         HAVING COUNT(*) >= 2 ORDER BY avg_total DESC, grp LIMIT 20",
        &rows,
        live,
    );
    let mut expected = (0..20)
        .map(|offset| {
            let grp = LARGE_GROUPS - offset;
            vec![grp.to_string(), round_units_to_four(u128::from(grp) * 50)]
        })
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(out, expected);
}
