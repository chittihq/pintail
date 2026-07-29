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
    assert_eq!(bytes[5], 1, "format version starts at one");
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
    bytes[62] ^= 0xff;
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
            assert_eq!(offset, 58);
            assert!(reason.contains("checksum mismatch"), "{reason}");
        }
        other => panic!("unexpected error: {other}"),
    }
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
