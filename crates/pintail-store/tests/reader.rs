use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
fn reader_only_snapshot_observes_wal_rows_while_writer_remains_open() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema =
        TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema");
    let mut writer = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
        .expect("writer");
    writer
        .ingest(vec![StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
            vec![Value::Utf8("visible".into())],
            1,
            false,
        )])
        .expect("WAL-backed row");

    let reader = TableSnapshot::open(directory.path(), schema).expect("reader-only snapshot");
    let rows = reader.scan().expect("reader scan");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values(), &[Value::Utf8("visible".into())]);
}
