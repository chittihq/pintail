//! A table dropped and created again under its name is stored in the
//! directory the dropped one had, and so is a table copied again from its
//! source. When the second has the first one's shape and as many rows, the
//! manifest describes its first segment exactly as it described the earlier
//! one: same file name, id, row count, versions and schema. Postings a
//! lookup built over the earlier file must not answer for the later one - an
//! equality on a value only the new rows hold found no row at all.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore, override_side_index};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "a", DataType::Int64, true),
            Column::new(3, "b", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn catalog() -> CatalogSnapshot {
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema(),
        TableStatistics::with_row_count(3),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    CatalogSnapshot::new(
        [DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")],
    )
    .expect("catalog")
}

/// The table as a first copy of `rows` leaves it: one segment, every row at
/// the copy's version.
fn copied(directory: &std::path::Path, rows: &[(u64, i64, i64)]) -> TableStore {
    let mut table = TableStore::open(
        directory,
        schema(),
        StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    table
        .bulk_ingest_snapshot(
            rows.iter()
                .map(|&(id, a, b)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Int64(a), Value::Int64(b)],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("copy");
    table
}

fn run(table: &TableStore, sql: &str) -> Vec<Vec<Value>> {
    let catalog = catalog();
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
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

#[test]
fn a_table_created_again_in_its_directory_is_not_answered_from_the_dropped_one() {
    override_side_index(Some(true));
    let root = tempfile::tempdir().expect("directory");
    let directory = root.path().join("table");

    // The first table holds one value of `a`; a lookup reads it.
    let first = copied(&directory, &[(1, 1, 1), (2, 1, 2), (3, 1, 3)]);
    assert_eq!(
        run(&first, "SELECT b FROM t WHERE a = 1 ORDER BY b"),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)]
        ]
    );
    assert_eq!(
        run(&first, "SELECT b FROM t WHERE a = 2"),
        Vec::<Vec<Value>>::new()
    );
    drop(first);
    std::fs::remove_dir_all(&directory).expect("drop the table");
    // File times tick coarsely on some filesystems; the two copies of a
    // real table are a statement's round trips apart at the least.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // The second has the same shape and as many rows, and other values.
    let second = copied(&directory, &[(1, 1, 1), (2, 2, 2), (3, 3, 3)]);
    assert_eq!(
        run(&second, "SELECT b FROM t WHERE a = 2"),
        vec![vec![Value::Int64(2)]]
    );
    assert_eq!(
        run(&second, "SELECT b FROM t WHERE a = 1"),
        vec![vec![Value::Int64(1)]]
    );
    assert_eq!(
        run(&second, "SELECT a FROM t WHERE a IN (2, 3) ORDER BY a"),
        vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
    );
    // What a correlated lookup per outer row does with the same filter.
    assert_eq!(
        run(
            &second,
            "SELECT o.a, o.b FROM t o WHERE o.b = (SELECT MIN(i.b) FROM t i WHERE i.a = o.a) ORDER BY o.a"
        ),
        vec![
            vec![Value::Int64(1), Value::Int64(1)],
            vec![Value::Int64(2), Value::Int64(2)],
            vec![Value::Int64(3), Value::Int64(3)]
        ]
    );
    override_side_index(None);
}
