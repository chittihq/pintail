use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
fn nullable_additions_are_metadata_only_for_old_segments_and_wal_rows() {
    for flush_old_rows in [false, true] {
        let directory = tempfile::tempdir().expect("temporary table directory");
        {
            let mut table =
                TableStore::open(directory.path(), schema_v1(), StoreOptions::default())
                    .expect("open v1");
            table.ingest(vec![row_v1(1, "old", 1)]).expect("v1 ingest");
            if flush_old_rows {
                table.flush().expect("v1 flush");
            } else {
                table.checkpoint().expect("v1 checkpoint");
            }
        }

        {
            let mut table = TableStore::open(
                directory.path(),
                schema_v2_nullable(),
                StoreOptions::default(),
            )
            .expect("upgrade to v2");
            assert_eq!(
                table.snapshot().scan().expect("scan old row"),
                vec![row_v2(1, "old", Value::Null, 1)]
            );
            table
                .ingest(vec![row_v2(2, "new", Value::Boolean(true), 2)])
                .expect("v2 ingest");
            table.flush().expect("v2 flush");
        }

        let reopened = TableStore::open(
            directory.path(),
            schema_v2_nullable(),
            StoreOptions::default(),
        )
        .expect("reopen v2");
        assert_eq!(
            reopened.snapshot().scan().expect("scan mixed schemas"),
            vec![
                row_v2(1, "old", Value::Null, 1),
                row_v2(2, "new", Value::Boolean(true), 2)
            ]
        );
    }
}

#[test]
fn required_additions_and_physical_type_changes_are_rejected() {
    for incompatible in [schema_v2_required(), schema_v2_type_change()] {
        let directory = tempfile::tempdir().expect("temporary table directory");
        {
            let mut table =
                TableStore::open(directory.path(), schema_v1(), StoreOptions::default())
                    .expect("open v1");
            table.ingest(vec![row_v1(1, "old", 1)]).expect("v1 ingest");
            table.flush().expect("v1 flush");
        }

        let error = TableStore::open(directory.path(), incompatible, StoreOptions::default())
            .err()
            .expect("incompatible evolution must fail");
        assert!(
            error.to_string().contains("incompatible schema evolution"),
            "{error}"
        );
    }
}

#[test]
fn dropped_columns_remain_readable_until_compaction_rewrites_them() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    for version in 1..=4 {
        let mut table = TableStore::open(directory.path(), schema_v1(), StoreOptions::default())
            .expect("open v1");
        table
            .ingest(vec![row_v1(1, "discarded", version)])
            .expect("v1 ingest");
        table.flush().expect("v1 flush");
    }

    let schema = TableSchema::new(2, vec![Column::new(1, "id", DataType::UInt64, false)])
        .expect("dropped-column schema");
    let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
        .expect("open after drop");
    assert_eq!(
        table.snapshot().scan().expect("scan old bytes"),
        vec![StoredRow::new(key(1), vec![Value::UInt64(1)], 4, false)]
    );

    let output = table
        .compact()
        .expect("rewrite dropped column")
        .output_path()
        .expect("compacted segment")
        .to_path_buf();
    let bytes = std::fs::read(output).expect("compacted bytes");
    assert_eq!(
        u32::from_le_bytes(bytes[26..30].try_into().expect("columns")),
        4
    );
    drop(table);
    TableStore::open(directory.path(), schema, StoreOptions::default())
        .expect("reopen rewritten schema");
}

fn schema_v1() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("v1 schema")
}

fn schema_v2_nullable() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
            Column::new(3, "active", DataType::Boolean, true),
        ],
    )
    .expect("v2 nullable schema")
}

fn schema_v2_required() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
            Column::new(3, "active", DataType::Boolean, false),
        ],
    )
    .expect("v2 required schema")
}

fn schema_v2_type_change() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("v2 changed schema")
}

fn row_v1(id: u64, label: &str, version: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![Value::UInt64(id), Value::Utf8(label.into())],
        version,
        false,
    )
}

fn row_v2(id: u64, label: &str, active: Value, version: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![Value::UInt64(id), Value::Utf8(label.into()), active],
        version,
        false,
    )
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}
