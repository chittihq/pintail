//! `column IN (constants)` over a non-key integer column bounds the scan
//! by the least and greatest constant, so segments whose values lie
//! outside that span are skipped as a range predicate's are. A NULL in the
//! list matches nothing and does not widen the span; a constant the column
//! compares with only by conversion leaves the column unbounded. Answers
//! are those of the same test written with OR.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    /// Eight segments of a thousand rows; `grp` is `id / 100`, so each
    /// segment holds ten consecutive groups.
    fn new() -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "grp", DataType::Int64, true),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        for segment in 0..8_u64 {
            let rows = (segment * 1_000..(segment + 1) * 1_000)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            if id % 37 == 0 {
                                Value::Null
                            } else {
                                Value::Int64(i64::try_from(id / 100).expect("group"))
                            },
                        ],
                        id + 1,
                        false,
                    )
                })
                .collect();
            table.bulk_ingest_snapshot(rows).expect("ingest");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "items",
            schema,
            TableStatistics::with_row_count(8_000),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str) -> (Vec<String>, PhysicalScanStats) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
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
                let values: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect();
                rows.push(format!("{values:?}"));
            }
        }
        rows.sort();
        let stats = provider
            .scan_stats(DatabaseId::new(1), TableId::new(1))
            .unwrap_or_default();
        (rows, stats)
    }
}

#[test]
fn a_list_of_constants_skips_segments_outside_its_span() {
    let fixture = Fixture::new();
    let (listed, stats) = fixture.run("SELECT id FROM items WHERE grp IN (75, 71, NULL)");
    let (either, _) = fixture.run("SELECT id FROM items WHERE grp = 71 OR grp = 75");
    assert_eq!(listed, either);
    assert_ne!(listed, [] as [std::string::String; 0]);
    assert!(
        stats.segments_pruned >= 7,
        "only the segment holding groups 70 to 79 is read: {stats:?}"
    );
}

#[test]
fn a_list_with_a_converted_constant_keeps_its_answer() {
    let fixture = Fixture::new();
    for (listed, either) in [
        (
            "SELECT id FROM items WHERE grp IN (71, 'x')",
            "SELECT id FROM items WHERE grp = 71 OR grp = 'x'",
        ),
        (
            "SELECT id FROM items WHERE grp IN (3, 1.5, 64)",
            "SELECT id FROM items WHERE grp = 3 OR grp = 1.5 OR grp = 64",
        ),
        (
            "SELECT id FROM items WHERE grp IN (NULL)",
            "SELECT id FROM items WHERE grp = NULL",
        ),
    ] {
        assert_eq!(fixture.run(listed).0, fixture.run(either).0, "{listed}");
    }
}
