//! A text function over a low-cardinality column reads its dictionary.
//!
//! A column of a few distinct values arrives from storage coded: per-row
//! codes and the distinct values, no per-row bytes. A function of it used
//! to build every row's text and then call itself once per row, so a
//! hundred thousand rows of ten names uppercased a hundred thousand
//! strings. It now answers from the dictionary - and the answer stays
//! coded, so a second function over the first is cheap too.
//!
//! This pins the answers, end to end through a real store, because the
//! saving is worth nothing if a coded column answers differently from an
//! uncoded one.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 20_000;
/// The distinct values of `tag`, one per row in rotation; every fifth row
/// leaves `note` NULL, so the coded path has to carry validity.
const TAGS: [&str; 4] = ["red", "Green", "BLUE", "red-orange"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "tag", DataType::Utf8, false),
            Column::new(3, "note", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    let tag = TAGS[usize::try_from(id % 4).expect("small")];
    let note = if id.is_multiple_of(5) {
        Value::Null
    } else {
        Value::Utf8(format!("note-{}", id % 3))
    };
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(tag.to_owned()), note],
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

    /// Every row `sql` answers, as its values rendered in column order.
    fn run(&self, sql: &str) -> Vec<String> {
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
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                let values = (0..batch.columns().len())
                    .map(|column| {
                        batch
                            .column(column)
                            .and_then(|column| column.value_owned(row))
                            .unwrap_or(Value::Null)
                    })
                    .collect::<Vec<_>>();
                rows.push(format!("{values:?}"));
            }
        }
        rows.sort();
        rows
    }
}

/// A single `COUNT` row, as [`Fixture::run`] renders it.
fn counted(rows: u64) -> Vec<String> {
    vec![format!("[{:?}]", Value::UInt64(rows))]
}

#[test]
fn a_text_function_over_a_coded_column_answers_as_it_reads() {
    let fixture = Fixture::new();
    // UPPER and LOWER fold case; the mixed-case values above make a
    // pass-through implementation visible.
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE UPPER(tag) = 'GREEN'"),
        counted(ROWS / 4)
    );
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE LOWER(tag) = 'blue'"),
        counted(ROWS / 4)
    );
    // A function of a function: the first answer is itself coded.
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE LOWER(UPPER(tag)) LIKE 'red%'"),
        counted(ROWS / 2)
    );
    // LENGTH answers a number per distinct value, gathered by code; only
    // `red` is three bytes long.
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE LENGTH(tag) = 3"),
        counted(ROWS / 4)
    );
}

#[test]
fn a_null_in_a_coded_column_stays_null_through_a_function() {
    let fixture = Fixture::new();
    // Every fifth row's `note` is NULL. A function of it is NULL, which no
    // comparison matches and `IS NULL` counts.
    let nulls = ROWS / 5;
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE UPPER(note) IS NULL"),
        counted(nulls)
    );
    assert_eq!(
        fixture.run("SELECT COUNT(*) FROM events WHERE LENGTH(note) IS NULL"),
        counted(nulls)
    );
    // And the non-null rows still answer: three distinct notes over the
    // four fifths of rows that have one.
    assert_eq!(
        fixture.run("SELECT COUNT(DISTINCT UPPER(note)) FROM events"),
        counted(3)
    );
}
