//! An aggregate during ingestion extends the settled answer it already has.
//!
//! A settled snapshot's aggregate is memoized under the table's identity
//! and the scan's own signature. When rows arrive that are pure inserts
//! above the segment key space, the next aggregate can answer from that
//! entry and the new rows alone, instead of reading the table again.
//!
//! Both sides have to spell the signature the same way. They did not: the
//! settled side led with the store instance and the delta side did not, so
//! the lookup could never hit and the path was dead. Nothing caught it
//! because the other aggregate paths answer the same query correctly, just
//! by reading everything.

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

/// Rows written to segments before the measurement.
const SETTLED: u64 = 4_000;
/// Rows appended afterwards, above every segment key, as pure inserts.
const APPENDED: u64 = 16;

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
            Value::Int64(i64::try_from(id % 10).expect("small")),
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
            .bulk_ingest_snapshot((0..SETTLED).map(row).collect())
            .expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema(),
            TableStatistics::with_row_count(SETTLED),
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

    /// `sql`'s single row, and whether it was answered by extending a
    /// memoized settled result.
    fn run(&self, sql: &str) -> (Vec<Value>, u64) {
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
        (rows, take_exec_counters().settled_delta_merges)
    }
}

#[test]
fn an_aggregate_over_appended_rows_extends_the_settled_answer() {
    let mut fixture = Fixture::new();
    let sql = "SELECT COUNT(*), SUM(amount) FROM events";
    let settled_sum: i64 = (0..SETTLED)
        .map(|id| i64::try_from(id % 10).expect("small"))
        .sum();

    // The settled answer, which fills the memo.
    let (rows, merges) = fixture.run(sql);
    assert_eq!(
        rows,
        vec![Value::UInt64(SETTLED), Value::Int64(settled_sum)]
    );
    assert_eq!(merges, 0, "nothing to extend yet");

    // Pure inserts above every segment key, which is what the delta covers.
    fixture
        .table
        .ingest((SETTLED..SETTLED + APPENDED).map(row).collect())
        .expect("append");
    let appended_sum: i64 = (SETTLED..SETTLED + APPENDED)
        .map(|id| i64::try_from(id % 10).expect("small"))
        .sum();

    let (rows, merges) = fixture.run(sql);
    assert_eq!(
        rows,
        vec![
            Value::UInt64(SETTLED + APPENDED),
            Value::Int64(settled_sum + appended_sum)
        ],
        "the extended answer counts the appended rows"
    );
    assert_eq!(
        merges, 1,
        "the delta found its base entry rather than reading the table again"
    );
}
