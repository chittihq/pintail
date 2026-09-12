//! The same `ORDER BY` over the same snapshot answers the same rows in the
//! same order every time it runs. Ties decide by arrival, so this holds only
//! while the scan hands its batches to the sort in one fixed order - a probe
//! for the nondeterminism that makes a limited sort disagree with the full
//! sort it is supposed to be a prefix of.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: i64 = 200_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "grp", DataType::Int64, true),
            Column::new(3, "note", DataType::Utf8, true),
            Column::new(4, "seen", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

fn row(id: i64, version: u64) -> StoredRow {
    let spread = (id * 7_919) % 1_009;
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            if spread % 17 == 0 {
                Value::Null
            } else {
                Value::Int64(spread % 50)
            },
            if spread % 23 == 0 {
                Value::Null
            } else {
                Value::Utf8(
                    ["b", "B", "a", "\u{e9}", "e"][usize::try_from(spread % 5).expect("slot")]
                        .to_owned(),
                )
            },
            if spread % 29 == 0 {
                Value::Null
            } else {
                Value::Utf8(format!("2026-01-{:02} 10:00:00", spread % 28 + 1))
            },
        ],
        version,
        false,
    )
}

struct Fixture {
    table: TableStore,
    catalog: CatalogSnapshot,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(
            directory.path(),
            schema(),
            StoreOptions {
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .expect("table");
        for parity in [1, 0] {
            table
                .ingest(
                    (1..=ROWS)
                        .filter(|id| id % 2 == parity)
                        .map(|id| row(id, 1))
                        .collect(),
                )
                .expect("segment");
            table.flush().expect("flush");
        }
        table
            .ingest((1..=500).map(|id| row(id * 397, 2)).collect())
            .expect("memtable");
        let entry = TableEntry::new(
            TableId::new(1),
            "t",
            schema(),
            TableStatistics::with_row_count(ROWS.cast_unsigned()),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        Self {
            table,
            catalog,
            _directory: directory,
        }
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(plan, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value_owned(row).unwrap_or(Value::Null))
                        .collect(),
                );
            }
        }
        rows
    }
}

#[test]
fn repeated_runs_of_one_ordering_answer_the_same_rows() {
    let fixture = Fixture::new();
    for sql in [
        "SELECT id, grp, note, seen FROM t ORDER BY grp",
        "SELECT id, grp, note, seen FROM t ORDER BY grp LIMIT 70000",
    ] {
        let first = fixture.run(sql);
        for attempt in 1..8 {
            let again = fixture.run(sql);
            assert_eq!(again.len(), first.len(), "{sql}: run {attempt} row count");
            let differs = again
                .iter()
                .zip(&first)
                .position(|(left, right)| left != right);
            assert!(
                differs.is_none(),
                "{sql}: run {attempt} differs from run 0 at row {differs:?}"
            );
        }
    }
}
