//! A table under live replication keeps its latest updates in the memtable.
//! The scan must answer as if they were flushed, whichever path serves the
//! rows, and the executor names the key column so the direct overlay can.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 80_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, true),
            Column::new(3, "amount", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64, grp: i64, amount: i64, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Int64(grp), Value::Int64(amount)],
        version,
        deleted,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    /// id -> (grp, amount) after the memtable writes.
    model: std::collections::BTreeMap<u64, (i64, i64)>,
}

impl Fixture {
    fn new(key_columns: bool) -> Self {
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
        let base = |id: u64| {
            (
                i64::try_from(id % 97).expect("grp"),
                i64::try_from(id % 1_000).expect("amount"),
            )
        };
        table
            .bulk_ingest_snapshot(
                (1..=ROWS)
                    .map(|id| {
                        let (grp, amount) = base(id);
                        row(id, grp, amount, 1, false)
                    })
                    .collect(),
            )
            .expect("ingest");
        let mut model = (1..=ROWS)
            .map(|id| (id, base(id)))
            .collect::<std::collections::BTreeMap<_, _>>();
        // Scattered updates over every block, a few deletes, inserts inside
        // and past the segment.
        let mut writes = Vec::new();
        for k in 0..300_u64 {
            let id = 7 + k * 263;
            writes.push(row(
                id,
                500 + i64::try_from(k % 5).expect("grp"),
                -1,
                2,
                false,
            ));
            model.insert(id, (500 + i64::try_from(k % 5).expect("grp"), -1));
        }
        for k in 0..40_u64 {
            let id = 100 + k * 1_999;
            writes.push(row(id, 0, 0, 2, true));
            model.remove(&id);
        }
        for id in [ROWS + 1, ROWS + 5_000] {
            writes.push(row(id, 600, -2, 2, false));
            model.insert(id, (600, -2));
        }
        table.ingest(writes).expect("memtable writes");
        let entry = TableEntry::new(
            TableId::new(1),
            "t",
            schema(),
            TableStatistics::with_row_count(ROWS),
        )
        .expect("entry");
        let entry = if key_columns {
            entry.with_key_columns([1]).expect("key")
        } else {
            entry
        };
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            table,
            catalog,
            model,
        }
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    }
}

fn check(fixture: &Fixture) {
    let count = fixture.model.len() as u64;
    let sum: i64 = fixture.model.values().map(|(_, amount)| amount).sum();
    assert_eq!(
        fixture.run("SELECT COUNT(*), SUM(amount) FROM t"),
        vec![vec![Value::UInt64(count), Value::Int64(sum)]]
    );
    let expected_grp = fixture
        .model
        .iter()
        .filter(|(_, (grp, _))| *grp == 503)
        .map(|(id, (_, amount))| vec![Value::UInt64(*id), Value::Int64(*amount)])
        .collect::<Vec<_>>();
    assert_eq!(
        fixture.run("SELECT id, amount FROM t WHERE grp = 503 ORDER BY id"),
        expected_grp
    );
    // A deleted key, an updated key, an inserted key and an untouched one.
    let mut probe = [100_u64, 7, ROWS + 1, 12_345];
    probe.sort_unstable();
    let expected_probe = probe
        .iter()
        .filter_map(|id| {
            fixture.model.get(id).map(|(grp, amount)| {
                vec![
                    Value::UInt64(*id),
                    Value::Int64(*grp),
                    Value::Int64(*amount),
                ]
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        fixture.run(&format!(
            "SELECT id, grp, amount FROM t WHERE id IN ({}) ORDER BY id",
            probe
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        expected_probe
    );
    // A range straddling updates, a delete and the segment's end.
    let expected_range = fixture
        .model
        .range(79_990..=ROWS + 1)
        .map(|(id, (_, amount))| vec![Value::UInt64(*id), Value::Int64(*amount)])
        .collect::<Vec<_>>();
    assert_eq!(
        fixture.run(&format!(
            "SELECT id, amount FROM t WHERE id BETWEEN 79990 AND {} ORDER BY id",
            ROWS + 1
        )),
        expected_range
    );
}

#[test]
fn memtable_rows_answer_the_same_with_and_without_the_direct_overlay() {
    // The key column is named: the overlay decodes directly.
    check(&Fixture::new(true));
    // No key column named: the merge answers.
    check(&Fixture::new(false));
}
