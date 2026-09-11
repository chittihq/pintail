//! A join on `a <=> b` matches NULL with NULL, and runs as a hash join on
//! that key rather than testing every pair. Each query here is compared
//! with the same condition written `(a <=> b) = TRUE`, which no hash key
//! reads, so its answer comes from the pair-by-pair loop.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn database() -> DatabaseId {
    DatabaseId::new(1)
}

fn schema(id: u32) -> TableSchema {
    TableSchema::new(
        id,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "n", DataType::Int64, true),
            Column::new(3, "note", DataType::Utf8, true),
            Column::new(4, "k", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn row(id: i64, seed: i64) -> StoredRow {
    let spread = (id * 7_919 + seed) % 97;
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            if spread % 5 == 0 {
                Value::Null
            } else {
                Value::Int64(spread % 13)
            },
            if spread % 7 == 0 {
                Value::Null
            } else {
                Value::Utf8(
                    ["x", "X", "y", "\u{e9}", "e"][usize::try_from(spread % 5).expect("slot")]
                        .to_owned(),
                )
            },
            if spread % 3 == 0 {
                Value::Null
            } else {
                Value::Int64(spread % 4)
            },
        ],
        1,
        false,
    )
}

struct Fixture {
    snapshots: Vec<TableSnapshot>,
    catalog: CatalogSnapshot,
    _stores: Vec<(TableStore, tempfile::TempDir)>,
}

impl Fixture {
    fn new() -> Self {
        let mut stores = Vec::new();
        let mut entries = Vec::new();
        for (id, name, rows, seed) in [(1_u32, "a", 400_i64, 0_i64), (2, "b", 300, 11)] {
            let directory = tempfile::tempdir().expect("directory");
            let mut table = TableStore::open(
                directory.path(),
                schema(id),
                StoreOptions {
                    background_compaction: false,
                    ..StoreOptions::default()
                },
            )
            .expect("table");
            table
                .ingest((1..=rows).map(|key| row(key, seed)).collect())
                .expect("rows");
            entries.push(
                TableEntry::new(
                    TableId::new(id.into()),
                    name,
                    schema(id),
                    TableStatistics::with_row_count(rows.cast_unsigned()),
                )
                .expect("entry")
                .with_key_columns([1])
                .expect("key"),
            );
            stores.push((table, directory));
        }
        let catalog =
            CatalogSnapshot::new([DatabaseEntry::new(database(), "app", entries).expect("app")])
                .expect("catalog");
        Self {
            snapshots: stores.iter().map(|(table, _)| table.snapshot()).collect(),
            catalog,
            _stores: stores,
        }
    }

    fn run(&self, sql: &str) -> (Vec<String>, String) {
        let provider = SnapshotScanProvider::new([
            (database(), TableId::new(1), &self.snapshots[0]),
            (database(), TableId::new(2), &self.snapshots[1]),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let plan = format!("{physical:?}");
        let mut execution = Execution::start(physical, &provider, 1 << 30, Collation::default())
            .unwrap_or_else(|error| panic!("start {sql}: {error}"));
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                let values = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect::<Vec<_>>();
                rows.push(format!("{values:?}"));
            }
        }
        rows.sort();
        (rows, plan)
    }
}

#[test]
fn a_null_safe_join_answers_as_the_pair_by_pair_loop_does() {
    let fixture = Fixture::new();
    for template in [
        "SELECT a.id, b.id FROM a JOIN b ON {a.n <=> b.n}",
        "SELECT a.id, b.id FROM a JOIN b ON {b.note <=> a.note} AND b.id < a.id",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON {a.note <=> b.note} AND b.id <= 30 \
         WHERE a.id <= 60",
        "SELECT a.id, b.id FROM a JOIN b ON {a.n <=> b.n} AND a.k = b.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON {a.n <=> b.n} AND {a.k <=> b.k}",
        "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE {b.n <=> a.n} AND b.id <> a.id)",
        "SELECT a.id FROM a WHERE NOT EXISTS \
         (SELECT 1 FROM b WHERE {b.note <=> a.note} AND {b.k <=> a.n} AND b.id <> a.id)",
        "SELECT a.id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE {b.k <=> a.n})",
    ] {
        let hashed = template.replace(['{', '}'], "");
        let looped = template.replace('{', "(").replace('}', ") = TRUE");
        let (fast, plan) = fixture.run(&hashed);
        let (reference, _) = fixture.run(&looped);
        assert!(plan.contains("HashJoin"), "{hashed}: {plan}");
        assert_eq!(fast, reference, "{hashed}");
        assert!(!fast.is_empty(), "{hashed}");
    }
}
