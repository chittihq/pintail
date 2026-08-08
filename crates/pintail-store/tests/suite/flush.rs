use pintail_store::{StoreError, StoreOptions, TableStore};
use pintail_types::{
    Column, DataType, Float64, KeyPart, PrimaryKey, StoredRow, TableSchema, Value,
};

#[test]
fn flush_publishes_a_versioned_segment_and_reopen_reads_it_without_wal_rows() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = schema();
    let options = StoreOptions {
        block_rows: 2,
        ..StoreOptions::default()
    };

    let segment_path = {
        let mut table =
            TableStore::open(directory.path(), schema.clone(), options).expect("open table");
        table
            .ingest(vec![
                row(1, "alpha", 1),
                row(2, "beta", 2),
                row(3, "gamma", 3),
            ])
            .expect("ingest");
        let flush = table.flush().expect("flush");
        assert_eq!(flush.row_count(), 3);
        flush.segment_path().expect("new segment").to_path_buf()
    };

    let bytes = std::fs::read(&segment_path).expect("segment bytes");
    assert_eq!(&bytes[..5], b"PTSEG");
    assert_eq!(bytes[5], 3, "writer stamps PTSEG v3");
    assert_eq!(
        std::fs::metadata(directory.path().join("table.wal"))
            .expect("WAL metadata")
            .len(),
        6,
        "flushed WAL is truncated to its header"
    );

    let reopened = TableStore::open(directory.path(), schema, options).expect("reopen table");
    assert_eq!(
        reopened.snapshot().scan().expect("scan segment"),
        vec![row(1, "alpha", 1), row(2, "beta", 2), row(3, "gamma", 3)]
    );
}

#[test]
fn snapshots_pin_the_pre_flush_memtable_and_manifest() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table.ingest(vec![row(1, "before", 1)]).expect("ingest");
    let before_flush = table.snapshot();

    table.flush().expect("flush");
    table
        .ingest(vec![row(1, "after", 2)])
        .expect("newer ingest");

    assert_eq!(
        before_flush.scan().expect("old snapshot"),
        vec![row(1, "before", 1)]
    );
    assert_eq!(
        table.snapshot().scan().expect("current snapshot"),
        vec![row(1, "after", 2)]
    );
}

#[test]
fn merge_on_read_uses_max_version_across_segments_and_recovered_wal() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions::default();
    {
        let mut table = TableStore::open(directory.path(), schema(), options).expect("open table");
        table
            .ingest(vec![row(1, "old", 1), row(2, "keep", 1)])
            .expect("first ingest");
        table.flush().expect("first flush");
        table
            .ingest(vec![row(1, "new", 3), row(2, "stale", 0)])
            .expect("second ingest");
        table.flush().expect("second flush");
        table
            .ingest(vec![StoredRow::new(
                key(2),
                vec![Value::UInt64(2), Value::Utf8("deleted".into())],
                4,
                true,
            )])
            .expect("WAL tombstone");
    }

    let reopened = TableStore::open(directory.path(), schema(), options).expect("reopen");
    assert_eq!(
        reopened.snapshot().scan().expect("merged scan"),
        vec![row(1, "new", 3)]
    );
}

#[test]
fn segment_round_trips_every_scalar_type_and_nulls() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = TableSchema::new(
        7,
        vec![
            Column::new(1, "bool", DataType::Boolean, false),
            Column::new(2, "i64", DataType::Int64, false),
            Column::new(3, "u64", DataType::UInt64, false),
            Column::new(4, "f64", DataType::Float64, false),
            Column::new(5, "text", DataType::Utf8, false),
            Column::new(6, "bytes", DataType::Binary, true),
        ],
    )
    .expect("schema");
    let rows = vec![
        StoredRow::new(
            key(1),
            vec![
                Value::Boolean(true),
                Value::Int64(-7),
                Value::UInt64(9),
                Value::Float64(Float64::new(3.5)),
                Value::Utf8("alpha".into()),
                Value::Binary(vec![0, 1, 2]),
            ],
            10,
            false,
        ),
        StoredRow::new(
            key(2),
            vec![
                Value::Boolean(false),
                Value::Int64(i64::MIN),
                Value::UInt64(u64::MAX),
                Value::Float64(Float64::new(-0.0)),
                Value::Utf8("βeta".into()),
                Value::Null,
            ],
            11,
            false,
        ),
    ];
    let mut table =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("open");
    table.ingest(rows.clone()).expect("ingest");
    table.flush().expect("flush");
    assert_eq!(table.snapshot().scan().expect("scan"), rows);
}

#[test]
fn scan_reports_the_corrupt_segment_and_block_offset() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let segment_path = {
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
        table.ingest(vec![row(1, "alpha", 1)]).expect("ingest");
        table
            .flush()
            .expect("flush")
            .segment_path()
            .expect("segment")
            .to_path_buf()
    };
    let mut bytes = std::fs::read(&segment_path).expect("read segment");
    bytes[66] ^= 0xff;
    std::fs::write(&segment_path, bytes).expect("corrupt segment");

    let reopened =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("reopen");
    match reopened.snapshot().scan().expect_err("checksum must fail") {
        StoreError::CorruptSegment {
            path,
            offset,
            reason,
        } => {
            assert_eq!(path, segment_path);
            assert_eq!(offset, 43);
            assert!(reason.contains("checksum mismatch"), "{reason}");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn point_and_range_reads_prune_disjoint_corrupt_segments_before_block_decode() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let corrupt_path = {
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
        table.ingest(vec![row(1, "corrupt", 1)]).expect("ingest");
        let path = table
            .flush()
            .expect("first flush")
            .segment_path()
            .expect("first segment")
            .to_path_buf();
        table.ingest(vec![row(100, "target", 2)]).expect("ingest");
        table.flush().expect("second flush");
        path
    };
    let mut bytes = std::fs::read(&corrupt_path).expect("segment bytes");
    bytes[66] ^= 0xff;
    std::fs::write(&corrupt_path, bytes).expect("corrupt block");

    let table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("reopen");
    assert_eq!(
        table.snapshot().get(&key(100)).expect("point lookup"),
        Some(row(100, "target", 2))
    );
    assert_eq!(
        table
            .snapshot()
            .scan_range(&key(100), &key(100))
            .expect("range scan"),
        vec![row(100, "target", 2)]
    );
    assert!(
        table.snapshot().scan().is_err(),
        "a full scan still visits the corrupt segment"
    );
}

#[test]
fn version_filtered_range_reads_prune_disjoint_segment_version_bounds() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table.ingest(vec![row(1, "as-of", 5)]).expect("ingest");
    table.flush().expect("first flush");
    table
        .ingest(vec![row(1, "future", 100)])
        .expect("ingest future");
    let future_path = table
        .flush()
        .expect("future flush")
        .segment_path()
        .expect("future segment")
        .to_path_buf();
    let snapshot = table.snapshot();

    let mut bytes = std::fs::read(&future_path).expect("future segment bytes");
    bytes[66] ^= 0xff;
    std::fs::write(&future_path, bytes).expect("corrupt future block");

    assert_eq!(
        snapshot
            .scan_range_versions(&key(1), &key(1), 0, 10)
            .expect("version-filtered scan"),
        vec![row(1, "as-of", 5)]
    );
    assert!(
        snapshot.scan_range(&key(1), &key(1)).is_err(),
        "the current-version scan still visits the corrupt future segment"
    );
}

#[test]
fn projected_range_scan_prunes_key_blocks_and_decodes_only_requested_columns() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        block_rows: 2,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    table
        .ingest(
            (1..=6)
                .map(|id| row(id, &format!("label-{id}"), id))
                .collect(),
        )
        .expect("ingest");
    table.flush().expect("flush");

    let scan = table
        .snapshot()
        .scan_projected_range(&key(3), &key(4), &[2])
        .expect("projected range");
    assert_eq!(scan.rows().len(), 2);
    assert_eq!(scan.rows()[0].key(), &key(3));
    assert_eq!(scan.rows()[0].values(), [Value::Utf8("label-3".into())]);
    assert_eq!(scan.rows()[1].key(), &key(4));
    assert_eq!(scan.rows()[1].values(), [Value::Utf8("label-4".into())]);
    assert_eq!(scan.stats().segments_read(), 1);
    assert_eq!(scan.stats().segments_pruned(), 0);
    assert_eq!(scan.stats().blocks_pruned(), 2);
    assert_eq!(scan.stats().blocks_read(), 1);
    assert_eq!(
        scan.stats().blocks_decoded(),
        4,
        "one key block plus version, tombstone, and projected label"
    );
}

#[test]
fn projected_scan_materializes_user_columns_only_after_version_resolution() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        block_rows: 2,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    table
        .ingest(vec![row(1, "losing-payload", 1)])
        .expect("old ingest");
    table.flush().expect("old flush");
    table
        .ingest(vec![row(1, "winning-payload", 2)])
        .expect("new ingest");
    table.flush().expect("new flush");

    let scan = table
        .snapshot()
        .scan_projected_range(&key(1), &key(1), &[2])
        .expect("late-materialized scan");
    assert_eq!(
        scan.rows()[0].values(),
        [Value::Utf8("winning-payload".into())]
    );
    assert_eq!(
        scan.stats().blocks_decoded(),
        7,
        "both segments decode key/version/tombstone headers, but only the \
         winning segment decodes its projected payload"
    );
}

#[test]
fn projected_scan_enforces_its_retained_memory_budget() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .ingest(
            (1..=8)
                .map(|id| row(id, &format!("label-{id}-{}", "x".repeat(256)), id))
                .collect(),
        )
        .expect("ingest");
    table.flush().expect("flush");
    let snapshot = table.snapshot();

    assert!(matches!(
        snapshot.scan_projected_range_bounded(&key(1), &key(8), &[2], 128),
        Err(StoreError::MemoryLimitExceeded { limit: 128, .. })
    ));
    let scan = snapshot
        .scan_projected_range_bounded(&key(1), &key(8), &[2], 64 * 1024)
        .expect("bounded scan");
    assert_eq!(scan.rows().len(), 8);
    assert!(scan.retained_bytes() <= 64 * 1024);
}

#[test]
fn projected_scan_streams_a_segment_larger_than_its_memory_budget() {
    const MEMORY_LIMIT: usize = 1024 * 1024;
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        block_rows: 4,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    let rows = (1..=16)
        .map(|id| {
            let mut state: u64 = id;
            let label = (0..128 * 1024)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    let offset =
                        u8::try_from((state >> 32) % 90).expect("random character offset fits u8");
                    char::from(b'!' + offset)
                })
                .collect::<String>();
            row(id, &label, id)
        })
        .collect();
    table.ingest(rows).expect("ingest");
    let segment = table
        .flush()
        .expect("flush")
        .segment_path()
        .expect("segment path")
        .to_path_buf();
    assert!(
        std::fs::metadata(segment).expect("segment metadata").len()
            > u64::try_from(MEMORY_LIMIT).expect("memory limit fits u64")
    );

    let scan = table
        .snapshot()
        .scan_projected_range_bounded(&key(1), &key(16), &[1], MEMORY_LIMIT)
        .expect("streamed bounded scan");
    assert_eq!(scan.rows().len(), 16);
    assert!(scan.retained_bytes() <= MEMORY_LIMIT);
}

#[test]
fn projected_memtable_scan_reserves_payloads_before_cloning() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .ingest(vec![row(1, &"x".repeat(16 * 1024), 1)])
        .expect("ingest");
    let snapshot = table.snapshot();

    assert!(matches!(
        snapshot.scan_projected_range_bounded(&key(1), &key(1), &[2], 2 * 1024),
        Err(StoreError::MemoryLimitExceeded { limit: 2_048, .. })
    ));
    assert_eq!(
        snapshot
            .scan_projected_range_bounded(&key(1), &key(1), &[2], 64 * 1024)
            .expect("bounded memtable scan")
            .rows()
            .len(),
        1
    );
}

#[test]
fn projected_memtable_scan_reserves_keys_before_cloning() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    let wide_key = PrimaryKey::new(vec![KeyPart::Utf8("k".repeat(16 * 1024))]).expect("wide key");
    table
        .ingest(vec![StoredRow::new(
            wide_key.clone(),
            vec![Value::UInt64(1), Value::Utf8("small".into())],
            1,
            false,
        )])
        .expect("ingest");
    let snapshot = table.snapshot();

    assert!(matches!(
        snapshot.scan_projected_range_bounded(&wide_key, &wide_key, &[2], 2 * 1024),
        Err(StoreError::MemoryLimitExceeded { limit: 2_048, .. })
    ));
}

fn schema() -> TableSchema {
    TableSchema::new(
        3,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, label: &str, version: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![Value::UInt64(id), Value::Utf8(label.into())],
        version,
        false,
    )
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}
