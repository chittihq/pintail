//! Joins whose only link between the inputs is an inequality - `<`, `<=`,
//! `>`, `>=` or `BETWEEN` - sort the right input by it and search it per
//! left row, so a band join reads the rows its bounds reach instead of
//! testing every pair; the full condition still decides each pair.

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
            Column::new(2, "a", DataType::Int64, true),
            Column::new(3, "d", DataType::Date32, true),
        ],
    )
    .expect("schema")
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        Value::Utf8(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// Runs `sql` over `rows` of (id, a, d).
fn run(rows: &[(u64, Option<i64>, Option<&str>)], sql: &str) -> Vec<String> {
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(41),
        "app",
        [TableEntry::new(
            TableId::new(42),
            "t",
            schema(),
            TableStatistics::with_row_count(rows.len() as u64),
        )
        .expect("table entry")],
    )
    .expect("database entry")])
    .expect("catalog");
    let dir = tempfile::tempdir().expect("temporary table");
    let mut table = TableStore::open(dir.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            rows.iter()
                .map(|(id, a, d)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            a.map_or(Value::Null, Value::Int64),
                            d.map_or(Value::Null, |d| Value::Utf8(d.to_owned())),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk rows");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(41), TableId::new(42), &snapshot)])
        .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut out = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        let columns = batch.columns().len();
        for row in batch.selection().selected_rows() {
            out.push(
                (0..columns)
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }
    out
}

const SMALL: [(u64, Option<i64>, Option<&str>); 4] = [
    (1, Some(1), Some("2024-01-01")),
    (2, Some(1), Some("2024-01-05")),
    (3, Some(2), None),
    (4, None, Some("2024-01-03")),
];

#[test]
fn inequality_joins_answer_every_kind() {
    assert_eq!(
        run(
            &SMALL,
            "SELECT x.id, y.id FROM t AS x JOIN t AS y ON x.a < y.a ORDER BY x.id, y.id"
        ),
        ["1,3", "2,3"]
    );
    assert_eq!(
        run(
            &SMALL,
            "SELECT x.id, y.id FROM t AS x JOIN t AS y ON y.id BETWEEN x.id AND x.id + 1 \
             ORDER BY x.id, y.id"
        ),
        ["1,1", "1,2", "2,2", "2,3", "3,3", "3,4", "4,4"]
    );
    assert_eq!(
        run(
            &SMALL,
            "SELECT x.id, y.id FROM t AS x LEFT JOIN t AS y ON x.a > y.a ORDER BY x.id, y.id"
        ),
        ["1,NULL", "2,NULL", "3,1", "3,2", "4,NULL"]
    );
    assert_eq!(
        run(
            &SMALL,
            "SELECT x.id, y.id FROM t AS x JOIN t AS y ON y.d >= x.d AND y.d < '2024-01-05' \
             AND x.id <> y.id ORDER BY x.id, y.id"
        ),
        ["1,4"]
    );
}

#[test]
fn a_band_join_reads_only_the_rows_its_bounds_reach() {
    // 30,000 rows against themselves is 900 million pairs; the band keeps
    // three per row. Testing every pair would not finish in any test budget.
    let rows = (1..=30_000_u64)
        .map(|id| (id, Some(i64::try_from(id).expect("small")), None))
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    assert_eq!(
        run(
            &rows,
            "SELECT COUNT(*) FROM t AS x JOIN t AS y ON y.a BETWEEN x.a AND x.a + 2"
        ),
        ["89997"]
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the band join took {:?}",
        started.elapsed()
    );
}

/// A table of every kind of column the sorted search reads, with NULLs
/// spread over each: `(id, x, lo, hi, amt, t0, t3, s, s2)`.
fn wide_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "x", DataType::Int64, true),
            Column::new(3, "lo", DataType::Int64, true),
            Column::new(4, "hi", DataType::Int64, true),
            Column::new(
                5,
                "amt",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(6, "t0", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(7, "t3", DataType::DateTime64 { fsp: 3 }, true),
            Column::new(8, "s", DataType::Utf8, true),
            Column::new(9, "s2", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

/// What one wide row holds, as the numbers the expectations are computed
/// from: `x`, `hi`, `amt` in hundredths, the minute of `t0` and the ordinal
/// its two texts spell, each `None` where the row stores NULL.
#[derive(Clone, Copy)]
struct Wide {
    id: i64,
    x: Option<i64>,
    hi: Option<i64>,
    cents: Option<i64>,
    minute: Option<i64>,
    label: Option<i64>,
}

fn wide(id: i64) -> Wide {
    Wide {
        id,
        x: (id % 11 != 0).then_some(id),
        hi: (id % 13 != 0).then_some(id + 1),
        cents: (id % 7 != 0).then_some(id * 50),
        minute: (id % 5 != 0).then_some(id),
        label: (id % 9 != 0).then_some(id),
    }
}

fn minute_text(minute: i64) -> String {
    format!(
        "2024-03-{:02} {:02}:{:02}:00",
        1 + minute / 1440,
        (minute / 60) % 24,
        minute % 60
    )
}

/// The label of an ordinal, in a case that alternates so only a
/// case-insensitive comparison orders the labels by their ordinals.
fn label_text(ordinal: i64) -> String {
    if ordinal % 2 == 0 {
        format!("N{ordinal:05}")
    } else {
        format!("n{ordinal:05}")
    }
}

/// Runs `sql` over `rows` wide rows named `w`, returning the first row and
/// what the execution counted.
fn run_wide(rows: i64, sql: &str) -> (String, pintail_exec::ExecCounters) {
    let text = |value: Option<String>| value.map_or(Value::Null, Value::Utf8);
    let integer = |value: Option<i64>| value.map_or(Value::Null, Value::Int64);
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(41),
        "app",
        [TableEntry::new(
            TableId::new(43),
            "w",
            wide_schema(),
            TableStatistics::with_row_count(rows.unsigned_abs()),
        )
        .expect("table entry")],
    )
    .expect("database entry")])
    .expect("catalog");
    let dir = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(dir.path(), wide_schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            (1..=rows)
                .map(|id| {
                    let row = wide(id);
                    let key = id.unsigned_abs();
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                        vec![
                            Value::UInt64(key),
                            integer(row.x),
                            Value::Int64(id - 1),
                            integer(row.hi),
                            text(
                                row.cents
                                    .map(|cents| format!("{}.{:02}", cents / 100, cents % 100)),
                            ),
                            text(row.minute.map(minute_text)),
                            text(
                                row.minute
                                    .map(|minute| format!("{}.500", minute_text(minute))),
                            ),
                            text(row.label.map(label_text)),
                            text(row.label.map(|label| label_text(label + 2))),
                        ],
                        key,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk rows");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(41), TableId::new(43), &snapshot)])
        .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let _ = pintail_exec::take_exec_counters();
    let mut execution =
        Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut out = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            out.push(
                (0..batch.columns().len())
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }
    (
        out.into_iter().next().unwrap_or_default(),
        pintail_exec::take_exec_counters(),
    )
}

const WIDE_ROWS: i64 = 400;

/// `COUNT(*), SUM(y.id)` over the pairs `accepts` keeps, as the engine
/// renders them, with the pair count beside it.
fn expected_pairs(accepts: impl Fn(Wide, Wide) -> bool) -> (String, u64) {
    let (mut count, mut sum) = (0_u64, 0_i64);
    for x in (1..=WIDE_ROWS).map(wide) {
        for y in (1..=WIDE_ROWS).map(wide) {
            if accepts(x, y) {
                count += 1;
                sum += y.id;
            }
        }
    }
    // A sum over no pairs is NULL.
    if count == 0 {
        ("0,NULL".to_owned(), 0)
    } else {
        (format!("{count},{sum}"), count)
    }
}

/// Asserts `condition` answers `accepts` and that the join found its
/// candidates by searching the sorted right side: a search per left row,
/// and no more pairs tested than `tested` allows of the pairs that match.
fn assert_searched(condition: &str, accepts: impl Fn(Wide, Wide) -> bool, tested: u64) {
    let (expected, pairs) = expected_pairs(accepts);
    let (answer, counters) = run_wide(
        WIDE_ROWS,
        &format!("SELECT COUNT(*), SUM(y.id) FROM w AS x JOIN w AS y ON {condition}"),
    );
    assert_eq!(answer, expected, "{condition}");
    assert_eq!(
        counters.range_join_searches,
        WIDE_ROWS.unsigned_abs(),
        "{condition}: every left row searches the sorted side"
    );
    assert!(
        counters.range_join_pairs_tested <= pairs.saturating_mul(tested),
        "{condition}: {} pairs tested for {pairs} matches",
        counters.range_join_pairs_tested
    );
}

#[test]
fn a_decimal_band_searches_the_sorted_side() {
    let cents = |row: Wide| row.cents;
    assert_searched(
        "y.amt >= x.amt AND y.amt < x.amt + 1.25",
        |x, y| matches!((cents(x), cents(y)), (Some(x), Some(y)) if y >= x && y < x + 125),
        1,
    );
    assert_searched(
        "y.amt BETWEEN x.amt - 0.5 AND x.amt + 0.5",
        |x, y| matches!((cents(x), cents(y)), (Some(x), Some(y)) if y >= x - 50 && y <= x + 50),
        1,
    );
    // A decimal against an integer is the same exact comparison.
    assert_searched(
        "y.amt > x.x AND y.amt <= x.x + 2",
        |x, y| matches!((x.x, cents(y)), (Some(x), Some(y)) if y > x * 100 && y <= (x + 2) * 100),
        1,
    );
}

#[test]
fn a_date_time_window_searches_the_sorted_side_across_precisions() {
    let minute = |row: Wide| row.minute;
    // Both sides at one precision, the upper bound computed.
    assert_searched(
        "y.t0 >= x.t0 AND y.t0 < x.t0 + INTERVAL 3 MINUTE",
        |x, y| matches!((minute(x), minute(y)), (Some(x), Some(y)) if y >= x && y < x + 3),
        1,
    );
    // A DATETIME(3) half a second past each minute against a DATETIME(0).
    assert_searched(
        "y.t3 > x.t0 AND y.t3 < x.t0 + INTERVAL 3 MINUTE",
        |x, y| matches!((minute(x), minute(y)), (Some(x), Some(y)) if y >= x && y < x + 3),
        1,
    );
    assert_searched(
        "x.t0 BETWEEN y.t3 - INTERVAL 90 SECOND AND y.t3",
        |x, y| matches!((minute(x), minute(y)), (Some(x), Some(y)) if x == y || x == y - 1),
        1,
    );
}

#[test]
fn a_text_range_searches_the_sorted_side_under_its_collation() {
    // The labels alternate case, so byte order would put every upper-case
    // label first; the comparison's collation orders them by ordinal.
    assert_searched(
        "y.s >= x.s AND y.s <= x.s2",
        |x, y| matches!((x.label, y.label), (Some(x), Some(y)) if y >= x && y <= x + 2),
        1,
    );
    // Two right columns, each bounded once: the pairs are those whose
    // second label is no further on than the first, past the row itself.
    assert_searched(
        "y.s > x.s AND y.s2 <= x.s",
        |x, y| matches!((x.label, y.label), (Some(x), Some(y)) if y > x && y + 2 <= x),
        1,
    );
    assert_searched(
        "y.s >= x.s AND y.s2 <= x.s2",
        |x, y| matches!((x.label, y.label), (Some(x), Some(y)) if y == x),
        1,
    );
}

#[test]
fn a_value_between_two_right_columns_skips_the_rows_its_upper_bound_rejects() {
    // `lo` is never NULL and `hi` sometimes is: the sorted side holds `lo`,
    // and the bound on `hi` picks the few rows of each prefix that reach.
    let accepts =
        |x: Wide, y: Wide| matches!((x.x, y.hi), (Some(x), Some(hi)) if y.id - 1 <= x && x <= hi);
    let (expected, pairs) = expected_pairs(accepts);
    let (answer, counters) = run_wide(
        WIDE_ROWS,
        "SELECT COUNT(*), SUM(y.id) FROM w AS x JOIN w AS y ON x.x BETWEEN y.lo AND y.hi",
    );
    assert_eq!(answer, expected);
    let searched = (1..=WIDE_ROWS).filter(|id| wide(*id).x.is_some()).count() as u64;
    assert_eq!(counters.range_join_second_bounds, searched);
    assert_eq!(
        counters.range_join_pairs_tested, pairs,
        "only matching pairs reach the condition"
    );

    // The same pairs, with the left rows that find none kept.
    let unmatched = (1..=WIDE_ROWS)
        .map(wide)
        .filter(|x| !(1..=WIDE_ROWS).map(wide).any(|y| accepts(*x, y)))
        .count() as u64;
    let (answer, counters) = run_wide(
        WIDE_ROWS,
        "SELECT COUNT(*), COUNT(y.id) FROM w AS x LEFT JOIN w AS y ON x.x BETWEEN y.lo AND y.hi",
    );
    assert_eq!(answer, format!("{},{pairs}", pairs + unmatched));
    assert_eq!(counters.range_join_pairs_tested, pairs);

    // A strict bound on each side, written the other way round.
    let strict =
        |x: Wide, y: Wide| matches!((x.x, y.hi), (Some(x), Some(hi)) if y.id - 1 < x && hi > x);
    let (expected, pairs) = expected_pairs(strict);
    let (answer, counters) = run_wide(
        WIDE_ROWS,
        "SELECT COUNT(*), SUM(y.id) FROM w AS x JOIN w AS y ON y.lo < x.x AND y.hi > x.x",
    );
    assert_eq!(answer, expected);
    assert_eq!(counters.range_join_pairs_tested, pairs);
}

#[test]
fn an_outer_join_condition_with_a_subquery_and_no_equality_searches_by_its_inequalities() {
    // The subquery reads the left row alone: it finds that row's own `x`
    // again and asks whether its `hi` is missing. The inequalities bound
    // the pairs it is resolved for; without them every pair would be.
    let in_range =
        |x: Wide, y: Wide| matches!((x.x, y.x), (Some(x), Some(y)) if y > x && y <= x + 2);
    let accepts = |x: Wide, y: Wide| in_range(x, y) && x.hi.is_some();
    let all = || (1..=WIDE_ROWS).map(wide);
    let candidates = all()
        .map(|x| all().filter(|y| in_range(x, *y)).count() as u64)
        .sum::<u64>();
    let pairs = all()
        .map(|x| all().filter(|y| accepts(x, *y)).count() as u64)
        .sum::<u64>();
    let unmatched = all().filter(|x| !all().any(|y| accepts(*x, y))).count() as u64;
    let (answer, counters) = run_wide(
        WIDE_ROWS,
        "SELECT COUNT(*), COUNT(y.id) FROM w AS x LEFT JOIN w AS y ON y.x > x.x AND y.x <= x.x + 2 \
         AND NOT EXISTS (SELECT 1 FROM w AS z WHERE z.x = x.x AND z.hi IS NULL)",
    );
    assert_eq!(answer, format!("{},{pairs}", pairs + unmatched));
    assert_eq!(counters.range_join_searches, WIDE_ROWS.unsigned_abs());
    assert_eq!(
        counters.range_join_pairs_tested, candidates,
        "the subquery is resolved only for the pairs the inequalities reach"
    );
}
