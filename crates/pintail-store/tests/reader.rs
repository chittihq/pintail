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

#[test]
fn non_overlapping_snapshot_segments_stream_with_bounded_memory() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema =
        TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema");
    let mut writer = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
        .expect("writer");
    for start in [1_u64, 51] {
        let rows = (start..start + 50)
            .map(|key| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                    vec![Value::Utf8(format!("value-{key}"))],
                    0,
                    false,
                )
            })
            .collect();
        writer
            .bulk_ingest_snapshot(rows)
            .expect("bulk snapshot segment");
    }

    let snapshot = writer.snapshot();
    let (start, end) = snapshot.key_bounds().expect("stream bounds");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1])
        .expect("open projected stream")
        .expect("non-overlapping snapshot fast path");
    assert_eq!(stream.segment_count(), 2);
    let first = stream
        .next_column_chunk(1024 * 1024)
        .expect("first stream chunk")
        .expect("first segment");
    let second = stream
        .next_chunk(1024 * 1024)
        .expect("second stream chunk")
        .expect("second segment");
    assert_eq!(first.row_count(), 50);
    assert_eq!(first.columns().len(), 1);
    assert_eq!(
        first.columns()[0].first(),
        Some(&Value::Utf8("value-1".into()))
    );
    assert_eq!(
        first.columns()[0].last(),
        Some(&Value::Utf8("value-50".into()))
    );
    assert_eq!(second.rows().len(), 50);
    assert!(
        stream
            .next_chunk(1024 * 1024)
            .expect("stream end")
            .is_none()
    );

    writer
        .ingest(vec![StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(101)]).expect("WAL key"),
            vec![Value::Utf8("pending".into())],
            1,
            false,
        )])
        .expect("WAL row");
    let snapshot_with_wal = writer.snapshot();
    let (start, end) = snapshot_with_wal.key_bounds().expect("WAL bounds");
    assert!(
        snapshot_with_wal
            .scan_projected_range_stream(&start, &end, &[1])
            .expect("inspect WAL stream safety")
            .is_none()
    );
}
