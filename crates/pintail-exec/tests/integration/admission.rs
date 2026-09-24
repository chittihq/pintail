use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider, collation::Collation,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
fn physical_bounds_admit_small_reads_on_large_replicas() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .unwrap();
    let mut store =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).unwrap();
    // Separate segments make key-range pruning observable without execution.
    for start in [0_u64, 300_000] {
        store
            .bulk_ingest_snapshot(
                (start..start + 5000)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
                            vec![Value::UInt64(id), Value::Utf8("sample".into())],
                            id + 1,
                            false,
                        )
                    })
                    .collect(),
            )
            .unwrap();
    }
    let snapshot = store.snapshot();
    let db = DatabaseId::new(1);
    let entries = [(1, "items"), (2, "other")].map(|(id, name)| {
        TableEntry::new(
            TableId::new(id),
            name,
            schema.clone(),
            TableStatistics::with_estimated_row_count(1_000_000),
        )
        .unwrap()
        .with_key_columns(vec![1])
        .unwrap()
    });
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(db, "app", entries).unwrap()]).unwrap();
    let provider = SnapshotScanProvider::new([
        (db, TableId::new(1), &snapshot),
        (db, TableId::new(2), &snapshot),
    ])
    .unwrap();
    let classify = |sql: &str| {
        let statement = parse_statement(sql).unwrap();
        let bound = Binder::new(&catalog, Some("app")).bind(&statement).unwrap();
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .unwrap();
        provider.admission_cost(&physical)
    };
    for sql in [
        "SELECT id FROM items WHERE id = 42",
        "SELECT COUNT(*) FROM items WHERE id < 100",
        "SELECT label FROM items LIMIT 100",
        "SELECT id FROM items ORDER BY id LIMIT 100",
        "SELECT items.id FROM items JOIN other ON items.id = other.id LIMIT 100",
    ] {
        assert!(classify(sql).is_some(), "must be short: {sql}");
    }
    for sql in [
        "SELECT COUNT(*) FROM items",
        "SELECT items.id FROM items JOIN other ON items.id = other.id",
        "SELECT label FROM items ORDER BY label LIMIT 100",
        "SELECT id FROM items LIMIT 1001",
        "SELECT ROW_NUMBER() OVER () FROM items LIMIT 100",
        "SELECT (SELECT COUNT(*) FROM other) FROM items LIMIT 1",
        "SELECT GROUP_CONCAT(label) FROM items WHERE id < 100",
    ] {
        assert!(classify(sql).is_none(), "must be general: {sql}");
    }
    let point = classify("SELECT id FROM items WHERE id = 42").unwrap();
    assert_eq!(point.rows, 5000, "only the overlapping segment is counted");
    assert_eq!(
        classify("SELECT id FROM items LIMIT 100").unwrap().rows,
        10000,
        "a result limit is not an input selectivity estimate"
    );
}
