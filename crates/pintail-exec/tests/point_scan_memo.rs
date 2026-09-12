//! An aggregate over `key IN (...)` memoizes under the keys it read.
//!
//! A point scan reads one run per key range rather than one scan of the
//! table, so the stream serving it is several scans in sequence. It used to
//! decline to identify itself to the settled aggregate memo at all - the
//! first run's identity would have named the whole query with a fraction of
//! it, and answering a later query from those rows is worse than reading
//! the table again.
//!
//! It identifies itself now, and this pins what the memo depends on: two
//! point aggregates over the same settled snapshot, differing only in which
//! keys they ask for, must not answer from each other's entry. Given a
//! single constant identity, the second of these returns the first's sum.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 2_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id).expect("small")),
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

    fn run(&self, sql: &str) -> Vec<Value> {
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
                rows = (0..batch.columns().len())
                    .map(|column| {
                        batch
                            .column(column)
                            .and_then(|column| column.value_owned(row))
                            .unwrap_or(Value::Null)
                    })
                    .collect();
            }
        }
        rows
    }
}

#[test]
fn point_aggregates_sharing_a_first_run_keep_different_answers() {
    let fixture = Fixture::new();
    // Every one of these opens the SAME first run - the single key 1 - and
    // differs only in the runs after it. That is the collision the first
    // run's identity alone would cause, and nothing else would: a run's own
    // key bounds are part of its signature, so queries whose first runs
    // differ are told apart even by a broken identity.
    //
    // The keys are spread wider than the gap at which two of them share one
    // read, so these are several runs rather than one span. Closer together
    // they collapse into a single run and never reach this stream at all.
    let cases = [
        ("id IN (1, 500)", 2_u64, 501_i64),
        ("id IN (1, 1000)", 2, 1001),
        ("id IN (1, 1500)", 2, 1501),
        ("id IN (1, 500, 1000)", 3, 1501),
    ];
    for (predicate, count, sum) in cases {
        let sql = format!("SELECT COUNT(*), SUM(amount) FROM events WHERE {predicate}");
        assert_eq!(
            fixture.run(&sql),
            vec![Value::UInt64(count), Value::Int64(sum)],
            "{sql}"
        );
    }
    // Again, in a different order: whichever entry each query filled, it
    // still has to answer its own question.
    for (predicate, count, sum) in cases.into_iter().rev() {
        let sql = format!("SELECT COUNT(*), SUM(amount) FROM events WHERE {predicate}");
        assert_eq!(
            fixture.run(&sql),
            vec![Value::UInt64(count), Value::Int64(sum)],
            "{sql} on the second pass"
        );
    }
}

/// The same keys asked twice answer the same thing, which is the case the
/// memo exists to make cheap.
#[test]
fn the_same_point_aggregate_answers_the_same_twice() {
    let fixture = Fixture::new();
    let sql = "SELECT COUNT(*), SUM(amount) FROM events WHERE id IN (100, 900, 1700)";
    let first = fixture.run(sql);
    assert_eq!(first, vec![Value::UInt64(3), Value::Int64(2700)]);
    assert_eq!(fixture.run(sql), first, "a memoized replay agrees");
}
