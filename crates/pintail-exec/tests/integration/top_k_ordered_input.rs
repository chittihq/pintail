//! A limited sort over input that arrives in the sort's own order leaves
//! the batches after its first few out whole.
//!
//! A table appended in time order, read for the first rows from a moment,
//! hands the sort millions of rows that all order after the k-th it already
//! holds. The sort used to read each of those rows' keys, rank them and
//! test them one by one before leaving every one of them out; the extremes
//! of the batch's packed key column say the same in one pass. The rows are
//! checked against values computed here, and the path by its counter.

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

const ROWS: u64 = 600_000;
const CHUNK: u64 = 100_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "made", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(3, "rank", DataType::Int64, false),
            Column::new(4, "seen", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

/// A moment `seconds` into March 2026.
fn moment(seconds: u64) -> String {
    format!(
        "2026-03-{:02} {:02}:{:02}:{:02}",
        1 + seconds / 86_400,
        seconds / 3_600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Two rows share each moment, so the second key settles every pair; one
/// row in seven has never been seen.
fn row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(moment(id / 2)),
            Value::Int64(i64::try_from(id / 3).expect("small")),
            if id.is_multiple_of(7) {
                Value::Null
            } else {
                Value::Utf8(moment(id / 2))
            },
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
        let options = StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
        for start in (0..ROWS).step_by(usize::try_from(CHUNK).expect("small")) {
            table
                .bulk_ingest_snapshot((start..start + CHUNK).map(row).collect())
                .expect("rows");
        }
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

    /// The ids `sql` answers, in the order it answers them, and the batches
    /// its sort left out whole.
    fn run(&self, sql: &str) -> (Vec<u64>, u64) {
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
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut ids = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                match batch.column(0).and_then(|column| column.value_owned(row)) {
                    Some(Value::UInt64(id)) => ids.push(id),
                    other => panic!("{sql}: unexpected id {other:?}"),
                }
            }
        }
        (ids, take_exec_counters().top_k_batches_passed_over)
    }
}

#[test]
fn input_in_the_sorts_order_is_left_out_by_its_extremes() {
    let fixture = Fixture::new();
    let from = 250_001_u64;
    let mut failures = Vec::new();
    for (sql, expected) in [
        (
            format!(
                "SELECT id, rank, made FROM events WHERE made >= '{}' ORDER BY made, id LIMIT 50",
                moment(from / 2 + 1)
            ),
            (from + 1..from + 51).collect::<Vec<_>>(),
        ),
        (
            "SELECT id FROM events WHERE id >= 1000 ORDER BY rank, id LIMIT 50".to_owned(),
            (1_000..1_050).collect(),
        ),
    ] {
        let (ids, passed_over) = fixture.run(&sql);
        if ids != expected {
            failures.push(format!("{sql}: answered {ids:?}"));
        }
        if passed_over == 0 {
            failures.push(format!("{sql}: no batch was left out by its extremes"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Where the extremes prove nothing the rows decide, as before: input
/// arriving against the sort's direction, and a key column holding NULLs,
/// which order before every value.
#[test]
fn other_input_is_ordered_row_by_row() {
    let fixture = Fixture::new();
    let (ids, _) = fixture.run("SELECT id FROM events ORDER BY made, id LIMIT 50");
    assert_eq!(ids, (0..50).collect::<Vec<_>>());
    let (ids, _) = fixture.run("SELECT id FROM events ORDER BY made DESC, id DESC LIMIT 50");
    assert_eq!(ids, (ROWS - 50..ROWS).rev().collect::<Vec<_>>());
    let (ids, passed_over) = fixture.run("SELECT id FROM events ORDER BY seen, id LIMIT 50");
    assert_eq!(ids, (0..350).step_by(7).collect::<Vec<_>>());
    assert_eq!(passed_over, 0, "a batch holding NULLs was judged whole");
    let (ids, _) = fixture.run("SELECT id FROM events ORDER BY seen DESC, id LIMIT 4");
    let last = ROWS - 1;
    // The last moment's one seen row (its other was never seen, and such
    // rows order last here), the two of the moment before, then the next.
    assert_eq!(ids, vec![last, last - 3, last - 2, last - 5]);
}
