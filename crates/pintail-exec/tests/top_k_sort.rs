//! `ORDER BY ... LIMIT k` keeps the input's batches as columns, cuts them
//! to their first k rows as they arrive, and leaves out rows that order
//! after the k-th before keeping them. Its answer is the first k rows the
//! full sort answers, rows with equal keys included, in the order they
//! arrived - checked here over keys with many ties and NULLs, limits either
//! side of the size at which the kept rows are cut, and offsets.

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
        // Two flushed segments whose keys interleave, then memtable writes.
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
                        .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                        .collect(),
                );
            }
        }
        rows
    }
}

#[test]
fn a_limited_sort_answers_the_first_rows_of_the_full_sort() {
    let fixture = Fixture::new();
    for order in [
        "grp",
        "grp DESC",
        "note, grp DESC",
        "grp DESC, note",
        "seen DESC",
        "seen, grp",
        "-grp",
        "note DESC",
    ] {
        let full = fixture.run(&format!(
            "SELECT id, grp, note, seen FROM t ORDER BY {order}"
        ));
        for (limit, offset) in [
            (1, 0),
            (10, 0),
            (10, 25),
            (5_000, 3),
            (70_000, 0),
            (150_000, 7),
        ] {
            let sql = format!(
                "SELECT id, grp, note, seen FROM t ORDER BY {order} LIMIT {limit} OFFSET {offset}"
            );
            let limited = fixture.run(&sql);
            let expected = full
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(limited.len(), expected.len(), "{sql}");
            assert!(limited == expected, "{sql}: the answers differ");
        }
    }
}
