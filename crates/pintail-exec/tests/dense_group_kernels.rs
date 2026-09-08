//! Dense keys and packed lanes agree with expression-keyed aggregation,
//! including the transition to a domain that no longer fits the slot table.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[allow(clippy::too_many_lines)] // one generated-table comparison shared by the three physical key types
fn compare_dense_with_general(key_type: DataType, expression: &str) {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "bucket", key_type, true),
            Column::new(3, "delta", DataType::Int64, true),
            Column::new(4, "weight", DataType::UInt64, true),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    // The first segment fills several dense windows. The second introduces
    // more than 1024 distinct keys, then revisits the original groups.
    for (start, end) in [(0_u64, 650_000_u64), (650_000, 720_000)] {
        table
            .bulk_ingest_snapshot(
                (start..end)
                    .map(|id| {
                        let ordinal = if id < 650_000 { id % 5 } else { id % 1300 };
                        let key = if id % 17 == 0 {
                            Value::Null
                        } else {
                            match key_type {
                                DataType::Utf8 => Value::Utf8(format!("bucket-{ordinal}")),
                                DataType::Int64 => {
                                    Value::Int64(i64::try_from(ordinal).expect("small"))
                                }
                                DataType::UInt64 => Value::UInt64(ordinal),
                                _ => unreachable!(),
                            }
                        };
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                key,
                                if id % 11 == 0 {
                                    Value::Null
                                } else {
                                    Value::Int64(i64::try_from(id % 19).expect("small") - 9)
                                },
                                if id % 13 == 0 {
                                    Value::Null
                                } else {
                                    Value::UInt64(id % 23)
                                },
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest");
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "items",
        schema,
        TableStatistics::with_row_count(720_000),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "test", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let run = |key: &str| {
        let sql = format!(
            "SELECT {key} AS k, COUNT(*), SUM(delta), SUM(weight) FROM items GROUP BY k ORDER BY k"
        );
        let bound = Binder::new(&catalog, Some("test"))
            .bind(&parse_statement(&sql).expect("parse"))
            .expect("bind");
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(plan, &provider, 512 << 20, Collation::default()).expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..4)
                        .map(|column| {
                            batch
                                .column(column)
                                .expect("column")
                                .value(row)
                                .expect("value")
                                .clone()
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    };
    let dense = run("bucket");
    let general = run(expression);
    assert_eq!(dense.len(), general.len());
    for (left, right) in dense.iter().zip(&general) {
        assert_eq!(left, right);
    }
}

#[test]
fn dense_text_lanes_and_dictionary_overflow_match_general() {
    compare_dense_with_general(DataType::Utf8, "CONCAT(bucket, '')");
}

#[test]
fn dense_signed_keys_and_range_overflow_match_general() {
    compare_dense_with_general(DataType::Int64, "bucket + 0");
}

#[test]
fn dense_unsigned_keys_and_range_overflow_match_general() {
    compare_dense_with_general(DataType::UInt64, "bucket + 0");
}
