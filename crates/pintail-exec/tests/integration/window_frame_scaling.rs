//! Window frames over a large partition cost about a pass over it: a RANGE
//! frame over a low-cardinality key, one running to UNBOUNDED FOLLOWING and
//! one whose rows share frames with their peers each answer 100,000 rows in
//! well under a second, and answer exactly what folding each frame does.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: i64 = 100_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "score", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn grp(id: i64) -> i64 {
    id % 3
}

fn score(id: i64) -> i64 {
    id % 7 * 10
}

/// A whole number, or `None` for NULL: an empty frame sums to NULL.
fn number(value: &Value) -> Option<i128> {
    match value {
        Value::Null => None,
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        other => Some(
            other
                .text()
                .and_then(|text| text.split('.').next())
                .and_then(|whole| whole.parse().ok())
                .unwrap_or_else(|| panic!("not a whole number: {other:?}")),
        ),
    }
}

/// Runs `sql`, returning each row's `id` and window value, and the time.
fn run(sql: &str) -> (Vec<(i64, Option<i128>)>, std::time::Duration) {
    let (rows, elapsed) = run_values(sql);
    (
        rows.iter()
            .map(|(id, value)| (*id, number(value)))
            .collect(),
        elapsed,
    )
}

/// Runs `sql`, returning each row's `id` and window value as it came back.
fn run_values(sql: &str) -> (Vec<(i64, Value)>, std::time::Duration) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (1..=ROWS)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
                        vec![
                            Value::Int64(id),
                            Value::Int64(grp(id)),
                            Value::Int64(score(id)),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("rows");
    let snapshot = table.snapshot();
    let (database, table_id) = (DatabaseId::new(1), TableId::new(1));
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        database,
        "app",
        [TableEntry::new(
            table_id,
            "t",
            schema(),
            TableStatistics::with_row_count(ROWS.cast_unsigned()),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key")],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(database, table_id, &snapshot)]).expect("provider");
    let started = Instant::now();
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("pull") {
        for row in batch.selection().selected_rows() {
            let id = number(batch.columns()[0].value(row).expect("id")).expect("id");
            let value = batch.columns()[1].value(row).expect("window value").clone();
            rows.push((i64::try_from(id).expect("id"), value));
        }
    }
    rows.sort_unstable_by_key(|(id, _)| *id);
    (rows, started.elapsed())
}

type Expectation<'a> = Box<dyn Fn(i64) -> Option<i128> + 'a>;

#[test]
fn window_frames_over_a_large_partition_cost_about_one_pass() {
    let ids = 1..=ROWS;
    let sum_where = |keep: &dyn Fn(i64) -> bool| -> i128 {
        (1..=ROWS)
            .filter(|id| keep(*id))
            .map(|id| i128::from(score(id)))
            .sum()
    };
    let group_sums = (0..3)
        .map(|group| sum_where(&|id| grp(id) == group))
        .collect::<Vec<_>>();
    let score_sums = (0..7)
        .map(|step| sum_where(&|id| score(id) == step * 10))
        .collect::<Vec<_>>();
    let cases: [(&str, Expectation<'_>); 5] = [
        (
            "SELECT id, SUM(score) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) FROM t",
            Box::new(|id| (id < ROWS).then(|| sum_where(&|other| other > id))),
        ),
        (
            "SELECT id, SUM(score) OVER (ORDER BY grp RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t",
            Box::new(|id| {
                Some(
                    group_sums[usize::try_from(grp(id)).expect("group")..]
                        .iter()
                        .sum(),
                )
            }),
        ),
        (
            "SELECT id, SUM(score) OVER (ORDER BY grp RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
            Box::new(|id| {
                Some(
                    group_sums[..=usize::try_from(grp(id)).expect("group")]
                        .iter()
                        .sum(),
                )
            }),
        ),
        (
            "SELECT id, COUNT(*) OVER (ORDER BY grp RANGE BETWEEN CURRENT ROW AND CURRENT ROW) FROM t",
            Box::new(|id| {
                Some(
                    i128::try_from((1..=ROWS).filter(|other| grp(*other) == grp(id)).count())
                        .expect("count"),
                )
            }),
        ),
        (
            "SELECT id, SUM(score) OVER (ORDER BY score RANGE BETWEEN 10 PRECEDING AND CURRENT ROW) FROM t",
            Box::new(|id| {
                let step = usize::try_from(score(id) / 10).expect("step");
                Some(score_sums[step.saturating_sub(1)..=step].iter().sum())
            }),
        ),
    ];
    for (sql, expected) in &cases {
        let (rows, elapsed) = run(sql);
        eprintln!("{elapsed:?}: {sql}");
        assert_eq!(rows.len(), ids.clone().count(), "{sql}");
        // The first query's expectation walks every row per row; sample it.
        for (id, value) in rows.iter().step_by(997) {
            assert_eq!(*value, expected(*id), "{sql} at id {id}");
        }
        assert!(elapsed.as_secs_f64() < 2.0, "{sql} took {elapsed:?}");
    }
}

/// A running `GROUP_CONCAT` reads the items folded so far without copying
/// them, and stops joining once the session's byte ceiling is passed: each
/// row pays at most that ceiling, not the whole frame. It copied every
/// collected item for every row, which was quadratic in the partition.
#[test]
fn a_running_group_concat_pays_at_most_the_ceiling_per_row() {
    let sql = "SELECT id, GROUP_CONCAT(score) OVER (ORDER BY id \
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t";
    let (rows, elapsed) = run_values(sql);
    eprintln!("{elapsed:?}: {sql}");
    assert_eq!(rows.len(), usize::try_from(ROWS).expect("rows"));
    let text = |value: &Value| value.text().expect("text").to_owned();
    assert_eq!(text(&rows[0].1), "10");
    assert_eq!(text(&rows[2].1), "10,20,30");
    // The default ceiling is 1,024 bytes; a long prefix is cut there.
    let last = text(&rows[rows.len() - 1].1);
    assert_eq!(last.len(), 1_024);
    assert!(last.starts_with("10,20,30,40,50,60,0,10"));
    // Every row answers a kilobyte, so this takes about two seconds in a
    // test build beside the rest of the suite; copying the frame per row
    // took minutes.
    assert!(elapsed.as_secs_f64() < 10.0, "{sql} took {elapsed:?}");
}
