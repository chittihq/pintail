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
        first.columns()[0].value_at(0),
        Some(Value::Utf8("value-1".into()))
    );
    assert_eq!(
        first.columns()[0].value_at(49),
        Some(Value::Utf8("value-50".into()))
    );
    assert_eq!(second.rows().len(), 50);
    assert!(
        stream
            .next_chunk(1024 * 1024)
            .expect("stream end")
            .is_none()
    );
    let mut parallel_stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1])
        .expect("open parallel projected stream")
        .expect("parallel non-overlapping snapshot fast path");
    let chunks = parallel_stream
        .next_column_chunks(2, 128 * 1024 * 1024)
        .expect("parallel segment chunks");
    assert_eq!(chunks.len(), 2);
    assert_eq!(
        chunks
            .iter()
            .map(pintail_store::ProjectedColumnChunk::row_count)
            .sum::<usize>(),
        100
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

#[test]
fn unselective_filter_first_scans_report_the_probe_decode() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema");
    let mut writer =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("writer");
    writer
        .bulk_ingest_snapshot(
            (1_u64..=100)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(format!("value-{id}"))],
                        0,
                        false,
                    )
                })
                .collect(),
        )
        .expect("snapshot segment");

    let snapshot = writer.snapshot();
    let (start, end) = snapshot.key_bounds().expect("stream bounds");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1, 2])
        .expect("open projected stream")
        .expect("direct stream");
    let keep_all = |_: &[pintail_store::DecodedColumn], _: usize| Ok(None);
    let chunk = stream
        .next_column_chunks_filtered(1, 64 * 1024 * 1024, &[2], &keep_all)
        .expect("filtered chunk")
        .pop()
        .expect("one segment");

    assert_eq!(chunk.row_count(), 100);
    assert_eq!(chunk.stats().blocks_decoded(), 3);
}

#[test]
fn overlapping_segments_and_wal_rows_stream_last_write_wins_in_chunks() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema =
        TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema");
    let mut writer =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("writer");
    for (start, end, prefix) in [(1_u64, 40_000_u64, "old"), (20_001, 60_000, "new")] {
        writer
            .bulk_ingest_snapshot(
                (start..=end)
                    .map(|key| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                            vec![Value::Utf8(format!("{prefix}-{key}"))],
                            0,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("snapshot segment");
    }
    writer
        .ingest(vec![
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(30_000)]).expect("updated key"),
                vec![Value::Utf8("wal-winner".into())],
                1,
                false,
            ),
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(60_001)]).expect("new key"),
                vec![Value::Utf8("wal-tail".into())],
                1,
                false,
            ),
        ])
        .expect("WAL rows");

    let snapshot = writer.snapshot();
    let (start, end) = snapshot.key_bounds().expect("stream bounds");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1])
        .expect("open merged stream")
        .expect("large overlapping snapshot streams");
    let mut values = Vec::new();
    while let Some(chunk) = stream
        .next_column_chunk(4 * 1024 * 1024)
        .expect("merged chunk")
    {
        assert!(chunk.row_count() <= 8 * 1024);
        values.extend(chunk.into_columns().pop().expect("projected value column"));
    }
    assert_eq!(values.len(), 60_001);
    assert_eq!(values[0], Value::Utf8("old-1".into()));
    assert_eq!(values[20_000], Value::Utf8("new-20001".into()));
    assert_eq!(values[29_999], Value::Utf8("wal-winner".into()));
    assert_eq!(values[60_000], Value::Utf8("wal-tail".into()));
}
