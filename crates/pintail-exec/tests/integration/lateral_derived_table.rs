//! `LATERAL` derived tables run on the dependent path: the table's query is
//! planned and executed once per distinct tuple of the outer columns it
//! reads, and every later input row carrying the same tuple is answered
//! from the statement's memo.
//!
//! The answers below are `MySQL` 8.4's for this data (the oracle corpus
//! compares the same shapes differentially). What this file adds is the
//! path: inner executions are counted, so a shape that stopped sharing
//! work, or one that silently stopped being evaluated per row, fails here
//! even while its rows stay right. It also pins the two limits the path
//! inherits - the query's memory ceiling and its deadline - and the
//! explicit refusals.

use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    dependent_memo_hits, dependent_subquery_executions, format_physical_plan,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Shelves: (id, zone). Zones repeat (1, 1, 2, NULL, 2, 9), so six rows
/// carry four distinct zone values, one of them NULL and one (9) with no
/// part at all.
const SHELVES: [(u64, Option<i64>); 6] = [
    (1, Some(1)),
    (2, Some(1)),
    (3, Some(2)),
    (4, None),
    (5, Some(2)),
    (6, Some(9)),
];

/// Parts: (id, zone, weight).
const PARTS: [(u64, Option<i64>, i64); 7] = [
    (1, Some(1), 50),
    (2, Some(1), 70),
    (3, Some(1), 90),
    (4, Some(2), 10),
    (5, None, 30),
    (6, Some(2), 40),
    (7, Some(3), 80),
];

fn shelves_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "zone", DataType::Int64, true),
        ],
    )
    .expect("shelves schema")
}

fn parts_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "zone", DataType::Int64, true),
            Column::new(3, "weight", DataType::Int64, false),
        ],
    )
    .expect("parts schema")
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn nullable(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::Int64)
}

struct Fixture {
    _shelves_dir: tempfile::TempDir,
    _parts_dir: tempfile::TempDir,
    shelves: TableStore,
    parts: TableStore,
}

fn fixture() -> Fixture {
    let shelves_dir = tempfile::tempdir().expect("shelves dir");
    let parts_dir = tempfile::tempdir().expect("parts dir");
    let mut shelves = TableStore::open(
        shelves_dir.path(),
        shelves_schema(),
        StoreOptions::default(),
    )
    .expect("open shelves");
    let mut parts = TableStore::open(parts_dir.path(), parts_schema(), StoreOptions::default())
        .expect("open parts");
    shelves
        .ingest(
            SHELVES
                .iter()
                .map(|(id, zone)| stored(*id, vec![Value::UInt64(*id), nullable(*zone)]))
                .collect(),
        )
        .expect("ingest shelves");
    parts
        .ingest(
            PARTS
                .iter()
                .map(|(id, zone, weight)| {
                    stored(
                        *id,
                        vec![Value::UInt64(*id), nullable(*zone), Value::Int64(*weight)],
                    )
                })
                .collect(),
        )
        .expect("ingest parts");
    Fixture {
        _shelves_dir: shelves_dir,
        _parts_dir: parts_dir,
        shelves,
        parts,
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

/// Binds, plans and runs `sql`; the rows, or the first error of any stage.
fn try_run(
    sql: &str,
    memory_limit: usize,
    deadline: Option<Instant>,
) -> Result<Vec<Vec<String>>, String> {
    let fixture = fixture();
    let shelves_snapshot = fixture.shelves.snapshot();
    let parts_snapshot = fixture.parts.snapshot();
    let database_id = DatabaseId::new(3);
    let (shelves_id, parts_id) = (TableId::new(1), TableId::new(2));
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                shelves_id,
                "shelves",
                shelves_schema(),
                TableStatistics::with_row_count(SHELVES.len() as u64),
            )
            .expect("shelves entry"),
            TableEntry::new(
                parts_id,
                "parts",
                parts_schema(),
                TableStatistics::with_row_count(PARTS.len() as u64),
            )
            .expect("parts entry"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, shelves_id, &shelves_snapshot),
        (database_id, parts_id, &parts_snapshot),
    ])
    .expect("provider");
    let statement = parse_statement(sql).map_err(|error| format!("parse: {error}"))?;
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| format!("bind: {error}"))?;
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .map_err(|error| format!("plan: {error}"))?;
    if sql.starts_with("/*explain*/") {
        return Ok(vec![vec![format_physical_plan(&physical)]]);
    }
    let mut execution = Execution::start_with_deadline(
        physical,
        &provider,
        memory_limit,
        deadline,
        Collation::default(),
    )
    .map_err(describe)?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(describe)? {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| render(column.value(row).expect("selected value")))
                    .collect(),
            );
        }
    }
    Ok(rows)
}

fn describe(error: ExecError) -> String {
    match error {
        ExecError::MemoryLimitExceeded { .. } => format!("memory: {error}"),
        ExecError::QueryTimedOut => format!("timeout: {error}"),
        other => format!("execute: {other}"),
    }
}

fn run(sql: &str) -> Vec<Vec<String>> {
    try_run(sql, 64 * 1024 * 1024, None).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn rows(expected: &[&[&str]]) -> Vec<Vec<String>> {
    expected
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect()
}

#[test]
fn an_inner_lateral_keeps_only_the_rows_it_finds_something_for() {
    assert_eq!(
        run("SELECT s.id, d.id FROM shelves s, \
             LATERAL (SELECT p.id FROM parts p WHERE p.zone = s.zone) d \
             ORDER BY s.id, d.id"),
        rows(&[
            &["1", "1"],
            &["1", "2"],
            &["1", "3"],
            &["2", "1"],
            &["2", "2"],
            &["2", "3"],
            &["3", "4"],
            &["3", "6"],
            &["5", "4"],
            &["5", "6"],
        ])
    );
}

#[test]
fn a_left_lateral_keeps_every_row_and_tests_its_condition_per_pair() {
    assert_eq!(
        run("SELECT s.id, d.id FROM shelves s LEFT JOIN \
             LATERAL (SELECT p.id, p.weight FROM parts p WHERE p.zone = s.zone) d \
             ON d.weight > 60 ORDER BY s.id, d.id"),
        rows(&[
            &["1", "2"],
            &["1", "3"],
            &["2", "2"],
            &["2", "3"],
            &["3", "NULL"],
            &["4", "NULL"],
            &["5", "NULL"],
            &["6", "NULL"],
        ])
    );
}

#[test]
fn top_n_per_group_orders_and_cuts_inside_each_group() {
    assert_eq!(
        run("SELECT s.id, d.id, d.weight FROM shelves s, \
             LATERAL (SELECT p.id, p.weight FROM parts p WHERE p.zone = s.zone \
                      ORDER BY p.weight DESC LIMIT 2) d \
             ORDER BY s.id, d.weight DESC"),
        rows(&[
            &["1", "3", "90"],
            &["1", "2", "70"],
            &["2", "3", "90"],
            &["2", "2", "70"],
            &["3", "6", "40"],
            &["3", "4", "10"],
            &["5", "6", "40"],
            &["5", "4", "10"],
        ])
    );
}

#[test]
fn an_ungrouped_aggregate_answers_null_and_unmatched_keys_too() {
    assert_eq!(
        run("SELECT s.id, d.c, d.w FROM shelves s, \
             LATERAL (SELECT COUNT(*) AS c, SUM(p.weight) AS w FROM parts p \
                      WHERE p.zone = s.zone) d ORDER BY s.id"),
        rows(&[
            &["1", "3", "210"],
            &["2", "3", "210"],
            &["3", "2", "50"],
            &["4", "0", "NULL"],
            &["5", "2", "50"],
            &["6", "0", "NULL"],
        ])
    );
    // The NULL-safe comparison finds the NULL-zoned part.
    assert_eq!(
        run("SELECT s.id, d.c FROM shelves s, \
             LATERAL (SELECT COUNT(*) AS c FROM parts p WHERE p.zone <=> s.zone) d \
             WHERE s.id = 4"),
        rows(&[&["4", "1"]])
    );
}

#[test]
fn chained_laterals_read_the_tables_before_them() {
    assert_eq!(
        run("SELECT s.id, d.id, e.heavier FROM shelves s, \
             LATERAL (SELECT p.id, p.weight FROM parts p WHERE p.zone = s.zone \
                      ORDER BY p.weight LIMIT 1) d, \
             LATERAL (SELECT COUNT(*) AS heavier FROM parts q \
                      WHERE q.weight > d.weight AND q.zone = s.zone) e \
             ORDER BY s.id"),
        rows(&[
            &["1", "1", "2"],
            &["2", "1", "2"],
            &["3", "4", "1"],
            &["5", "4", "1"],
        ])
    );
}

/// Six shelves carry four distinct zones: the query runs four times, and
/// the two shelves repeating a zone are answered from the memo.
#[test]
fn the_query_runs_once_per_distinct_outer_tuple() {
    let executions = dependent_subquery_executions();
    let hits = dependent_memo_hits();
    let result = run("SELECT s.id, d.c FROM shelves s, \
                      LATERAL (SELECT COUNT(*) AS c FROM parts p WHERE p.zone = s.zone) d \
                      ORDER BY s.id");
    assert_eq!(result.len(), SHELVES.len());
    assert_eq!(dependent_subquery_executions() - executions, 4);
    assert_eq!(dependent_memo_hits() - hits, 2);
}

/// A predicate on the table to the left is applied before the lateral
/// table is evaluated, so rows it drops cost no execution.
#[test]
fn a_filter_on_the_left_table_runs_before_the_lateral_table() {
    let executions = dependent_subquery_executions();
    let result = run("SELECT s.id, d.c FROM shelves s, \
                      LATERAL (SELECT COUNT(*) AS c FROM parts p WHERE p.zone = s.zone) d \
                      WHERE s.id = 3");
    assert_eq!(result, rows(&[&["3", "2"]]));
    assert_eq!(dependent_subquery_executions() - executions, 1);
}

/// A volatile query is never shared: one execution per input row.
#[test]
fn a_volatile_lateral_query_is_never_memoized() {
    let executions = dependent_subquery_executions();
    let result = run("SELECT s.id, d.c FROM shelves s, \
                      LATERAL (SELECT COUNT(*) AS c FROM parts p \
                               WHERE p.zone = s.zone AND RAND() >= 0) d");
    assert_eq!(result.len(), SHELVES.len());
    assert_eq!(
        dependent_subquery_executions() - executions,
        SHELVES.len() as u64
    );
}

/// A table function is as volatile as the document it reads: a query over
/// one whose document calls `RAND()` runs for every row, in a `LATERAL`
/// table and in a dependent subquery alike. Six shelves carry four
/// distinct zones, so a memoized query would run four times.
#[test]
fn a_volatile_table_function_document_is_never_memoized() {
    let executions = dependent_subquery_executions();
    let result = run("SELECT s.id, d.r FROM shelves s, \
                      LATERAL (SELECT jt.r FROM JSON_TABLE(JSON_ARRAY(RAND()), '$[*]' \
                               COLUMNS (r DOUBLE PATH '$')) jt \
                               WHERE s.zone IS NULL OR s.zone > 0) d");
    assert_eq!(result.len(), SHELVES.len());
    assert_eq!(
        dependent_subquery_executions() - executions,
        SHELVES.len() as u64
    );
    let distinct: std::collections::BTreeSet<&String> = result.iter().map(|row| &row[1]).collect();
    assert_eq!(distinct.len(), SHELVES.len(), "{result:?}");

    let result = run(
        "SELECT s.id, (SELECT jt.r FROM JSON_TABLE(JSON_ARRAY(RAND()), '$[*]' \
                               COLUMNS (r DOUBLE PATH '$')) jt \
                               WHERE s.zone IS NULL OR s.zone > 0) FROM shelves s",
    );
    let distinct: std::collections::BTreeSet<&String> = result.iter().map(|row| &row[1]).collect();
    assert_eq!(distinct.len(), SHELVES.len(), "{result:?}");

    // The same table over a constant document is shared as before.
    let executions = dependent_subquery_executions();
    let result = run("SELECT s.id, d.r FROM shelves s, \
                      LATERAL (SELECT jt.r FROM JSON_TABLE('[7]', '$[*]' \
                               COLUMNS (r INT PATH '$')) jt \
                               WHERE s.zone IS NULL OR s.zone > 0) d");
    assert_eq!(result.len(), SHELVES.len());
    assert_eq!(dependent_subquery_executions() - executions, 4);
}

/// A table that reads nothing to its left is an ordinary derived table.
#[test]
fn an_uncorrelated_lateral_is_an_ordinary_derived_table() {
    let executions = dependent_subquery_executions();
    let result = run("SELECT s.id, d.c FROM shelves s, \
                      LATERAL (SELECT COUNT(*) AS c FROM parts) d ORDER BY s.id");
    assert_eq!(result.len(), SHELVES.len());
    assert!(result.iter().all(|row| row[1] == "7"));
    assert_eq!(dependent_subquery_executions() - executions, 0);
    let plan = run("/*explain*/ SELECT s.id, d.c FROM shelves s, \
                    LATERAL (SELECT COUNT(*) AS c FROM parts) d");
    assert!(!plan[0][0].contains("Lateral"), "{}", plan[0][0]);
    let plan = run("/*explain*/ SELECT s.id, d.c FROM shelves s LEFT JOIN \
                    LATERAL (SELECT COUNT(*) AS c FROM parts p WHERE p.zone = s.zone) d ON TRUE");
    assert!(plan[0][0].contains("Lateral kind=Left"), "{}", plan[0][0]);
}

#[test]
fn the_memory_ceiling_and_the_deadline_cover_the_lateral_path() {
    let sql = "SELECT s.id, d.id FROM shelves s, \
               LATERAL (SELECT p.id FROM parts p WHERE p.zone = s.zone) d";
    let error = try_run(sql, 256, None).expect_err("a 256-byte ceiling cannot hold the join");
    assert!(error.starts_with("memory:"), "{error}");
    let past = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("an instant one second ago");
    let error = try_run(sql, 64 * 1024 * 1024, Some(past)).expect_err("the deadline has passed");
    assert!(error.starts_with("timeout:"), "{error}");
}

#[test]
fn unsupported_positions_are_refused_by_name() {
    for sql in [
        "SELECT s.id FROM shelves s JOIN (parts q JOIN \
         LATERAL (SELECT p.id FROM parts p WHERE p.zone = s.zone) d ON TRUE) ON TRUE",
        "SELECT s.id FROM shelves s LEFT JOIN \
         LATERAL (SELECT p.id FROM parts p WHERE p.zone = s.zone) d ON TRUE \
         RIGHT JOIN parts q ON q.zone = s.zone",
        "SELECT s.id FROM shelves s LEFT JOIN \
         LATERAL (SELECT p.id, p.weight FROM parts p WHERE p.zone = s.zone) d \
         ON d.weight > (SELECT AVG(q.weight) FROM parts q WHERE q.zone = s.zone)",
    ] {
        let error = try_run(sql, 64 * 1024 * 1024, None).expect_err("refused");
        assert!(error.contains("LATERAL"), "{sql}: {error}");
    }
    // Without the keyword, a derived table cannot see the table beside it.
    let error = try_run(
        "SELECT s.id FROM shelves s, (SELECT p.id FROM parts p WHERE p.zone = s.zone) d",
        64 * 1024 * 1024,
        None,
    )
    .expect_err("no lateral, no outer reference");
    assert!(error.starts_with("bind:"), "{error}");
}
