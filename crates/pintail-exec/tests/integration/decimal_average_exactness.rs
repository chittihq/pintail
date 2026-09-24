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
    run_with_schema(sql, fixture, live, schema())
}

fn run_with_schema(
    sql: &str,
    fixture: &[StoredRow],
    live: usize,
    schema: TableSchema,
) -> Vec<Vec<String>> {
    let row_count = fixture.len() as u64;
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
        .expect("open table");
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
        schema,
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
                    Value::DecimalAverage(average) => average.label,
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

/// The corpus shape that fails: `AVG(total)` and `SUM(total) / COUNT(*)`
/// over the same column in one statement, both rounded to four places.
///
/// `MySQL` defines `AVG` as the sum divided by the count at the same
/// widened scale, so the two projections must agree row for row. When the
/// gate caught this disagreeing, the average was the wrong one and the
/// division was right - so the invariant, not the literal value, is what
/// pins it.
#[test]
fn an_average_agrees_with_the_sum_over_the_count_in_one_statement() {
    let rows = large_fixture();
    for live in [0, rows.len() / 4] {
        let out = run_split(
            "SELECT grp, ROUND(AVG(total), 4) AS avg_total, \
             ROUND(SUM(total) / COUNT(*), 4) AS mean_check FROM orders \
             GROUP BY grp HAVING COUNT(*) >= 2 ORDER BY avg_total DESC, grp LIMIT 20",
            &rows,
            live,
        );
        assert_eq!(out.len(), 20, "the limit keeps twenty groups (live={live})");
        for row in &out {
            assert_eq!(
                row[1], row[2],
                "AVG and SUM/COUNT must agree for group {} (live={live})",
                row[0]
            );
        }
    }
}

/// Realistic magnitudes rather than a single cent: values in the hundreds
/// with two fraction digits, several thousand rows, several hundred
/// groups. A double's error over a sum of this size is around the fourth
/// fraction digit, which is exactly where the corpus query rounds, so if
/// the engine ever reaches its inexact average this is the fixture that
/// should show it.
#[test]
fn an_average_agrees_with_the_sum_over_the_count_on_realistic_values() {
    const GROUPS: u64 = 500;
    const PER_GROUP: u64 = 14;
    let mut rows = Vec::new();
    let mut id = 1_u64;
    // A cheap deterministic spread; the point is varied cents, not entropy.
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    for grp in 1..=GROUPS {
        for _ in 0..PER_GROUP {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let cents = 10_000 + (seed >> 33) % 90_000;
            rows.push(row(id, grp, &format!("{}.{:02}", cents / 100, cents % 100)));
            id += 1;
        }
    }
    for live in [0, rows.len() / 3] {
        let out = run_split(
            "SELECT grp, ROUND(AVG(total), 4) AS avg_total, \
             ROUND(SUM(total) / COUNT(*), 4) AS mean_check FROM orders \
             GROUP BY grp HAVING COUNT(*) >= 2 ORDER BY grp",
            &rows,
            live,
        );
        assert_eq!(out.len(), usize::try_from(GROUPS).expect("small"));
        for row in &out {
            assert_eq!(
                row[1], row[2],
                "AVG and SUM/COUNT must agree for group {} (live={live})",
                row[0]
            );
        }
    }
}

/// Quotients that do not terminate, with the answers taken from `MySQL`
/// 8.4 rather than derived here.
///
/// A group of forty averages exactly, because forty divides a power of
/// ten - so every earlier arm in this file measured a division that never
/// had to round. These group sizes (six, seven, nine, thirteen) force the
/// division to round at the result's sixth fraction digit, which is the
/// only place `AVG` and `SUM(_) / COUNT(*)` could ever part company. In
/// `MySQL` they never do, over eight thousand rows of varied sums; the
/// pairs below are its own readings.
#[test]
fn a_non_terminating_average_rounds_the_way_mysql_rounds() {
    // (rows in the group, the group's exact sum, MySQL's AVG at scale 6,
    //  MySQL's ROUND(AVG, 4)).
    const CASES: [(u64, &str, &str, &str); 4] = [
        (9, "1398.20", "155.355556", "155.3556"),
        (7, "1090.41", "155.772857", "155.7729"),
        (13, "1857.09", "142.853077", "142.8531"),
        (6, "888.38", "148.063333", "148.0633"),
    ];
    for (index, (count, sum, expected_avg, expected_round)) in CASES.iter().enumerate() {
        // `count - 1` rows of a round hundred, then whatever makes the sum.
        let base = 100_u64;
        let filled = (count - 1) * base;
        let cents = (sum.replace('.', "").parse::<u64>().expect("sum")) - filled * 100;
        let mut rows = Vec::new();
        for seat in 0..(count - 1) {
            rows.push(row(seat + 1, 1, &format!("{base}.00")));
        }
        rows.push(row(
            *count,
            1,
            &format!("{}.{:02}", cents / 100, cents % 100),
        ));

        let exact = run_on("SELECT AVG(total) FROM orders", &rows);
        assert_eq!(
            exact,
            vec![vec![(*expected_avg).to_owned()]],
            "case {index}: AVG at its result scale over {count} rows summing to {sum}"
        );
        let rounded = run_on("SELECT ROUND(AVG(total), 4) FROM orders", &rows);
        assert_eq!(
            rounded,
            vec![vec![(*expected_round).to_owned()]],
            "case {index}: ROUND(AVG, 4) over {count} rows summing to {sum}"
        );
    }
}

/// The corpus shape: group sizes that do not divide a power of ten, ordered
/// by the average itself under a `LIMIT`.
///
/// The arms above fix the group size at fourteen and order by the group key.
/// The gate's failing query does neither - its groups are whatever the data
/// made them, and it takes the twenty largest averages, so the ordering reads
/// the very value under test and a top-k path decides which rows survive.
/// Thirty-one and thirty-six rows both force the division to round at the
/// sixth fraction digit, the only place `AVG` and `SUM(_) / COUNT(*)` can
/// part company.
#[test]
fn a_top_k_by_average_agrees_with_the_sum_over_the_count() {
    let mut rows = Vec::new();
    let mut id = 1_u64;
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    // Sizes around the shapes the gate reported, none of them a divisor of a
    // power of ten, so every group's average is a non-terminating quotient.
    for (grp, per_group) in (1..=400_u64).map(|grp| (grp, 29 + (grp % 9))) {
        for _ in 0..per_group {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let cents = 30_000 + (seed >> 33) % 5_000;
            rows.push(row(id, grp, &format!("{}.{:02}", cents / 100, cents % 100)));
            id += 1;
        }
    }
    for live in [0, rows.len() / 4, rows.len() / 2] {
        let out = run_split(
            "SELECT grp, ROUND(AVG(total), 4) AS avg_total, \
             ROUND(SUM(total) / COUNT(*), 4) AS mean_check FROM orders \
             GROUP BY grp HAVING COUNT(*) >= 2 \
             ORDER BY avg_total DESC, grp LIMIT 20",
            &rows,
            live,
        );
        assert_eq!(
            out.len(),
            20,
            "the limit decides the row count (live={live})"
        );
        for row in &out {
            assert_eq!(
                row[1], row[2],
                "AVG and SUM/COUNT must agree for group {} (live={live})",
                row[0]
            );
        }
    }
}

/// `ROUND(AVG(_), 4)` must round the quotient once, not twice.
///
/// `MySQL` displays `AVG` over a `DECIMAL(_,2)` at six fraction digits, but
/// keeps the division's full precision for anything that reads it. One
/// hundred and sixty-one rows summing to 54001.34 average to
/// 335.41204968944..., which shows as 335.412050 and rounds to four places
/// as 335.4120 - because the digits past the sixth place are below a half.
///
/// Rounding to six places first and letting `ROUND` round that again turns
/// the same value into 335.4121: the intermediate 335.412050 is an exact
/// half at the fourth place, and half-up carries it upward. The answer is
/// then a unit in the last place above `MySQL`'s, which is G14's signature.
///
/// The numbers come from a live `MySQL` 8.4: `AVG` reads 335.412050 and
/// `ROUND(AVG(total), 4)` reads 335.4120 over exactly this fixture.
#[test]
fn rounding_an_average_does_not_round_it_twice() {
    let mut rows = vec![row(1, 1, "335.74")];
    rows.extend((2..=161).map(|id| row(id, 1, "335.41")));
    let out = run_on(
        "SELECT grp, ROUND(AVG(total), 4) AS avg_total, \
         ROUND(SUM(total) / COUNT(*), 4) AS mean_check FROM orders GROUP BY grp",
        &rows,
    );
    assert_eq!(out.len(), 1, "one group");
    assert_eq!(
        out[0][1], "335.4120",
        "MySQL rounds the exact quotient once; got {:?}",
        out[0]
    );
    assert_eq!(out[0][1], out[0][2], "AVG and SUM/COUNT must agree");
}

#[test]
fn average_consumers_keep_internal_digits_and_declared_rendering() {
    for sign in ["", "-"] {
        let mut rows = vec![row(1, 1, &format!("{sign}335.74"))];
        rows.extend((2..=161).map(|id| row(id, 1, &format!("{sign}335.41"))));
        for live in [0, 80] {
            let out = run_split(
                "SELECT AVG(total), ROUND(AVG(total), 9), CAST(AVG(total) AS DECIMAL(15,4)), \
                 TRUNCATE(AVG(total), 4), ROUND(AVG(total) + 0, 4), \
                 CAST(AVG(total) AS CHAR), ROUND(-AVG(total), 4) FROM orders",
                &rows,
                live,
            );
            let opposite = if sign.is_empty() { "-" } else { "" };
            assert_eq!(
                out,
                vec![vec![
                    format!("{sign}335.412050"),
                    format!("{sign}335.412050"),
                    format!("{sign}335.4120"),
                    format!("{sign}335.4120"),
                    format!("{sign}335.4120"),
                    format!("{sign}335.412050"),
                    format!("{opposite}335.4120"),
                ]]
            );
        }
    }
}

#[test]
fn materialized_averages_support_predicates_and_outer_aggregates() {
    let rows = vec![row(1, 1, "1.00"), row(2, 2, "1.00"), row(3, 2, "1.00")];
    assert_eq!(
        run_on(
            "SELECT AVG(total), ABS(AVG(total)) FROM orders HAVING AVG(total) > 0",
            &rows
        ),
        vec![vec!["1.000000".to_owned(), "1.000000".to_owned()]]
    );
    assert_eq!(
        run_on(
            "SELECT SUM(a), AVG(a), MIN(a), MAX(a) FROM \
         (SELECT AVG(total) AS a FROM orders GROUP BY grp) AS means",
            &rows
        ),
        vec![vec![
            "2.000000".to_owned(),
            "1.0000000000".to_owned(),
            "1.000000".to_owned(),
            "1.000000".to_owned()
        ]]
    );
    assert_eq!(
        run_on(
            "SELECT a, COUNT(*) FROM (SELECT AVG(total) AS a FROM orders GROUP BY grp) AS means GROUP BY a",
            &rows
        ),
        vec![vec!["1.000000".to_owned(), "2".to_owned()]]
    );
}

#[test]
fn averages_retain_whole_fraction_words_for_each_declared_scale() {
    for (scale, displayed, internal) in [
        (0, "0.3333", "0.333333333000000000"),
        (2, "0.333333", "0.333333333000000000"),
        (6, "0.3333333333", "0.333333333333333333"),
    ] {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "grp", DataType::UInt64, false),
                Column::new(
                    3,
                    "total",
                    DataType::Decimal {
                        precision: 12,
                        scale,
                    },
                    false,
                ),
            ],
        )
        .expect("schema");
        let rows = vec![row(1, 1, "1"), row(2, 1, "0"), row(3, 1, "0")];
        assert_eq!(
            run_with_schema(
                "SELECT AVG(total), CAST(AVG(total) AS DECIMAL(30,18)) FROM orders",
                &rows,
                0,
                schema
            ),
            vec![vec![displayed.to_owned(), internal.to_owned()]]
        );
    }
}

#[test]
fn window_range_bounds_read_materialized_average_text() {
    let rows = vec![row(1, 1, "1.00"), row(2, 2, "2.00"), row(3, 3, "4.00")];
    assert_eq!(
        run_on(
            "SELECT a, SUM(a) OVER (ORDER BY a RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) \
         FROM (SELECT AVG(total) AS a FROM orders GROUP BY grp) AS means",
            &rows
        ),
        vec![
            vec!["1.000000".to_owned(), "1.000000".to_owned()],
            vec!["2.000000".to_owned(), "3.000000".to_owned()],
            vec!["4.000000".to_owned(), "4.000000".to_owned()]
        ]
    );
}

#[test]
fn wide_exact_averages_do_not_expand_unused_internal_digits() {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::UInt64, false),
            Column::new(
                3,
                "total",
                DataType::Decimal {
                    precision: 33,
                    scale: 2,
                },
                false,
            ),
        ],
    )
    .expect("schema");
    let rows = vec![row(1, 1, "1000000000000000000000000000000.01")];
    assert_eq!(
        run_with_schema("SELECT ROUND(AVG(total), 2) FROM orders", &rows, 0, schema),
        vec![vec!["1000000000000000000000000000000.01".to_owned()]]
    );
}
