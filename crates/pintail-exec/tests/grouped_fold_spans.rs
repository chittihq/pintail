//! A memtable row marks the segment it lands in as dirty.
//!
//! The grouped fold caches one folded result per segment and reuses it
//! while that segment is clean. A memtable row inside a segment's key
//! range supersedes a row that fold counted, so the segment has to be
//! marked and re-read; miss the mark and the query answers from a fold
//! that no longer describes the table.
//!
//! Locating the span used to be a scan of every span for every memtable
//! row. It is a binary search now, which is only sound because the spans
//! are disjoint and sorted - so this pins the assignment itself: a key
//! inside a span, a key in the gap between two, and a key past the last.

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

/// Three segments, with a gap in the key space between each pair so a row
/// can land outside every span without being past the last one.
const SEGMENTS: [(u64, u64); 3] = [(0, 100), (200, 300), (400, 500)];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Utf8, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, amount: i64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(if id.is_multiple_of(2) { "even" } else { "odd" }.to_owned()),
            Value::Int64(amount),
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
        for (first, last) in SEGMENTS {
            table
                .bulk_ingest_snapshot((first..=last).map(|id| row(id, 1)).collect())
                .expect("segment");
        }
        let rows = SEGMENTS
            .iter()
            .map(|(first, last)| last - first + 1)
            .sum::<u64>();
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema(),
            TableStatistics::with_row_count(rows),
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

    /// `SUM(amount)` over the whole table.
    fn total(&self) -> i64 {
        self.measured().0
    }

    /// `SUM(amount)` over the whole table, with the spans the grouped fold
    /// read and the spans it took from its cache.
    fn measured(&self) -> (i64, u64, u64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let sql = "SELECT grp, SUM(amount) FROM events GROUP BY grp";
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 128 * 1024 * 1024, Collation::default())
                .expect("execution");
        let _ = take_exec_counters();
        let mut total = 0;
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                match batch.column(1).and_then(|column| column.value_owned(row)) {
                    Some(Value::Int64(sum)) => total += sum,
                    other => panic!("unexpected sum {other:?}"),
                }
            }
        }
        let counters = take_exec_counters();
        (
            total,
            counters.grouped_spans_folded,
            counters.grouped_spans_reused,
        )
    }
}

#[test]
fn a_memtable_row_inside_a_span_invalidates_that_span_s_fold() {
    let mut fixture = Fixture::new();
    let settled: i64 = SEGMENTS
        .iter()
        .map(|(first, last)| i64::try_from(last - first + 1).expect("small"))
        .sum();
    // The first run folds every span and caches each one.
    assert_eq!(fixture.total(), settled);

    // A key inside the middle segment: an update, so the fold that counted
    // its old value is stale.
    fixture.table.ingest(vec![row(250, 101)]).expect("update");
    let (total, folded, reused) = fixture.measured();
    assert_eq!(
        total,
        settled + 100,
        "the span holding the updated key was re-read, not reused"
    );
    // The counters are what say the answer above came through the fold
    // rather than a general scan that would be right either way.
    assert!(
        folded >= 1,
        "the grouped fold ran: {folded} folded, {reused} reused"
    );

    // A key in the gap between two segments, and one past the last: both
    // are outside every span and contribute on their own.
    fixture.table.ingest(vec![row(150, 7)]).expect("gap");
    fixture.table.ingest(vec![row(900, 9)]).expect("above");
    assert_eq!(
        fixture.total(),
        settled + 100 + 7 + 9,
        "rows outside every span are counted once each"
    );
}
