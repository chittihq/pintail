//! Exact packed prefiltering agrees with the general expression path.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
#[allow(clippy::too_many_lines)] // one full scan comparison over generated segment rows
fn packed_conjunction_preserves_nulls_duplicates_and_unsigned_ranges() {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "serial", DataType::UInt64, false),
            Column::new(2, "code", DataType::UInt64, true),
            Column::new(3, "bucket", DataType::Int64, true),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let rows = (0_u64..150_000)
        .map(|id| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(id),
                    if id % 11 == 0 {
                        Value::Null
                    } else {
                        Value::UInt64(u64::MAX - 4096 + id % 4096)
                    },
                    if id % 17 == 0 {
                        Value::Null
                    } else {
                        Value::Int64(i64::try_from(id % 50).expect("small") - 25)
                    },
                ],
                1,
                false,
            )
        })
        .collect();
    table.bulk_ingest_snapshot(rows).expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "items",
        schema,
        TableStatistics::with_row_count(150_000),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key columns");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "test", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let run = |predicate: &str| {
        let sql = format!("SELECT code, bucket FROM items WHERE {predicate} ORDER BY code, bucket");
        let bound = Binder::new(&catalog, Some("test"))
            .bind(&parse_statement(&sql).expect("parse"))
            .expect("bind");
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(plan, &provider, 64 << 20, Collation::default()).expect("start");
        let mut output = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                output.push((
                    batch
                        .column(0)
                        .expect("code")
                        .value(row)
                        .expect("value")
                        .clone(),
                    batch
                        .column(1)
                        .expect("bucket")
                        .value(row)
                        .expect("value")
                        .clone(),
                ));
            }
        }
        output
    };
    for (packed, general) in [
        (
            "code >= 18446744073709547520 AND bucket = 2",
            "COALESCE(code, code) >= 18446744073709547520 AND bucket + 0 = 2",
        ),
        (
            "code >= -1 AND bucket < 0",
            "COALESCE(code, code) >= -1 AND bucket + 0 < 0",
        ),
        (
            "code < 18446744073709547520 AND bucket = 2",
            "COALESCE(code, code) < 18446744073709547520 AND bucket + 0 = 2",
        ),
    ] {
        assert_eq!(run(packed), run(general), "{packed}");
    }
}
