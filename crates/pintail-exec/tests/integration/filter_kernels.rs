//! A `WHERE` clause reaches the batch kernels.
//!
//! The filter had two paths: a packed comparison of a column against a
//! literal, and row-at-a-time evaluation for everything else - so a
//! function of a column, arithmetic or a decimal comparison rendered every
//! cell to a value and back, per row. The kernels answer the same
//! predicate a batch at a time, and this pins both halves of that: the
//! rows are the ones the predicate names, and they are reached without
//! materializing a value per row.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters, take_session_division_warnings,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 20_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "total", DataType::Int64, false),
            Column::new(4, "placed_at", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    let day = 1 + (id % 28);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("row-{}", id % 7)),
            Value::Int64(i64::try_from(id % 100).expect("small")),
            Value::Utf8(format!("2026-07-{day:02} 12:00:00")),
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
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        table
            .bulk_ingest_snapshot((0..ROWS).map(row).collect())
            .expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema(),
            TableStatistics::with_row_count(ROWS),
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

    /// The ids `sql` answers, the cells it materialized into values, and the
    /// divisions by zero it reported.
    fn run(&self, sql: &str) -> (Vec<u64>, u64, u64) {
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
        let _ = take_session_division_warnings();
        let mut execution =
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut ids = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                // `value_owned` reads the packed column without materializing
                // every other row's, so the count below is the query's own.
                match batch.column(0).and_then(|column| column.value_owned(row)) {
                    Some(Value::UInt64(id)) => ids.push(id),
                    other => panic!("{sql}: unexpected id {other:?}"),
                }
            }
        }
        ids.sort_unstable();
        (
            ids,
            take_exec_counters().values_materialized,
            take_session_division_warnings(),
        )
    }
}

#[test]
fn a_function_predicate_answers_its_rows_from_the_batch_kernels() {
    let fixture = Fixture::new();
    for (predicate, expected) in [
        (
            "UPPER(name) = 'ROW-3'",
            (0..ROWS).filter(|id| id % 7 == 3).collect::<Vec<_>>(),
        ),
        (
            "total * 2 > 190",
            (0..ROWS).filter(|id| (id % 100) * 2 > 190).collect(),
        ),
        (
            "LENGTH(name) = 5 AND total < 3",
            (0..ROWS).filter(|id| id % 100 < 3).collect(),
        ),
        (
            "DATE(placed_at) = '2026-07-05'",
            (0..ROWS).filter(|id| id % 28 == 4).collect(),
        ),
    ] {
        let sql = format!("SELECT id FROM events WHERE {predicate}");
        let (ids, materialized, warnings) = fixture.run(&sql);
        assert_eq!(ids, expected, "{predicate}");
        assert_eq!(warnings, 0, "{predicate}");
        assert!(
            materialized < ROWS,
            "{predicate}: {materialized} cells materialized for {ROWS} rows - the filter went row \
             by row"
        );
    }
}

/// The scan's own skipping pass evaluates the predicate to choose which
/// rows to read, and the filter evaluates it again over what it read. A
/// warning has to come back once, so the skipping pass leaves any batch
/// that raises one to the filter.
#[test]
fn a_warning_under_a_function_predicate_is_reported_once() {
    let fixture = Fixture::new();
    // Every hundredth row divides by zero, answers NULL, and is filtered
    // out - one warning each, as MySQL reports them.
    let (ids, _, warnings) = fixture.run("SELECT id FROM events WHERE 10 / (total % 100) > 0.05");
    assert_eq!(
        ids,
        (0..ROWS).filter(|id| id % 100 != 0).collect::<Vec<_>>()
    );
    assert_eq!(warnings, ROWS / 100);
}
