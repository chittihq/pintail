use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
fn pinned_reader_snapshot_keeps_its_version_while_new_ingest_replaces_it() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .ingest(vec![row("first", 1, false)])
        .expect("first version");
    let pinned = table.snapshot();

    table
        .ingest(vec![row("second", 2, false)])
        .expect("second version");

    assert_eq!(
        pinned.scan().expect("pinned scan"),
        vec![row("first", 1, false)]
    );
    assert_eq!(
        table.snapshot().scan().expect("current scan"),
        vec![row("second", 2, false)]
    );
}

#[test]
fn stale_versions_do_not_replace_newer_rows_and_tombstones_hide_keys() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");

    table.ingest(vec![row("new", 5, false)]).expect("new row");
    table
        .ingest(vec![row("stale", 4, false)])
        .expect("stale row");
    assert_eq!(
        table.snapshot().scan().expect("scan latest"),
        vec![row("new", 5, false)]
    );

    table
        .ingest(vec![row("deleted", 6, true)])
        .expect("tombstone");
    assert!(table.snapshot().scan().expect("scan tombstone").is_empty());
}

#[test]
fn an_invalid_batch_is_rejected_before_it_reaches_the_wal() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    let invalid = StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Utf8("key".into())]).expect("key"),
        vec![Value::UInt64(17)],
        1,
        false,
    );

    let error = table.ingest(vec![invalid]).expect_err("reject wrong type");
    assert!(error.to_string().contains("column value requires Utf8"));
    table.checkpoint().expect("checkpoint empty WAL");
    drop(table);

    let reopened = TableStore::open(directory.path(), schema(), StoreOptions::default())
        .expect("reopen table");
    assert!(reopened.snapshot().scan().expect("scan").is_empty());
}

#[test]
fn reaching_the_memtable_budget_flushes_and_runs_one_bounded_maintenance_step() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        memtable_bytes: 1,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open table");

    let outcome = table
        .ingest(vec![row("automatically flushed", 1, false)])
        .expect("ingest");
    assert!(
        outcome.should_flush(),
        "batch crossed the configured budget"
    );
    assert_eq!(
        std::fs::metadata(directory.path().join("table.wal"))
            .expect("WAL metadata")
            .len(),
        6
    );
    assert_eq!(
        table.compaction_status().expect("status").segment_count(),
        1
    );
    let metrics = table.metrics().expect("storage metrics");
    assert_eq!(metrics.memtable_bytes(), 0);
    assert_eq!(metrics.segment_count(), 1);
    assert_eq!(metrics.compaction_debt_bytes(), 0);
    assert_eq!(
        table.snapshot().scan().expect("scan"),
        vec![row("automatically flushed", 1, false)]
    );
}

#[test]
fn storage_metrics_track_unflushed_memtable_memory() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");

    assert_eq!(table.metrics().expect("empty metrics").memtable_bytes(), 0);
    table
        .ingest(vec![row("resident", 1, false)])
        .expect("ingest");

    let metrics = table.metrics().expect("resident metrics");
    assert!(metrics.memtable_bytes() > 0);
    assert_eq!(metrics.segment_count(), 0);
    assert_eq!(metrics.compaction_debt_bytes(), 0);
}

#[test]
fn snapshot_bulk_ingest_bypasses_wal_sorts_and_collapses_chunk_duplicates() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    let rows = vec![
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Utf8("z".into())]).expect("z key"),
            vec![Value::Utf8("last".into())],
            0,
            false,
        ),
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Utf8("a".into())]).expect("a key"),
            vec![Value::Utf8("old".into())],
            0,
            false,
        ),
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Utf8("a".into())]).expect("a key"),
            vec![Value::Utf8("new".into())],
            1,
            false,
        ),
    ];

    let outcome = table
        .bulk_ingest_snapshot(rows)
        .expect("bulk snapshot ingest");
    assert_eq!(outcome.row_count(), 2);
    assert!(outcome.segment_path().is_some());
    assert_eq!(
        std::fs::metadata(directory.path().join("table.wal"))
            .expect("WAL metadata")
            .len(),
        6,
        "snapshot rows bypass the WAL"
    );
    let visible = table.snapshot().scan().expect("scan");
    assert_eq!(visible[0].values(), [Value::Utf8("new".into())]);
    assert_eq!(visible[1].values(), [Value::Utf8("last".into())]);
}

#[test]
fn snapshot_logical_types_round_trip_through_version_one_segments() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let logical_schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "signed", DataType::Int8, false),
            Column::new(2, "unsigned", DataType::UInt16, false),
            Column::new(3, "float", DataType::Float32, false),
            Column::new(
                4,
                "decimal",
                DataType::Decimal {
                    precision: 38,
                    scale: 10,
                },
                false,
            ),
            Column::new(5, "date", DataType::Date32, false),
            Column::new(6, "datetime", DataType::DateTime64 { fsp: 6 }, false),
            Column::new(7, "time", DataType::Time64 { fsp: 6 }, false),
            Column::new(8, "json", DataType::Json, false),
        ],
    )
    .expect("logical schema");
    let values = vec![
        Value::Int64(-128),
        Value::UInt64(65_535),
        Value::float64(1.5),
        Value::Utf8("1234567890123456789012345678.1234567890".into()),
        Value::Utf8("1000-01-01".into()),
        Value::Utf8("2026-07-30 12:34:56.123456".into()),
        Value::Utf8("-838:59:59.000001".into()),
        Value::Utf8(r#"{"a":1,"emoji":"🪶"}"#.into()),
    ];
    let row = StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
        values,
        0,
        false,
    );
    let mut table = TableStore::open(
        directory.path(),
        logical_schema.clone(),
        StoreOptions::default(),
    )
    .expect("open logical table");
    table
        .bulk_ingest_snapshot(vec![row.clone()])
        .expect("bulk ingest logical row");
    drop(table);

    let reopened = TableStore::open(directory.path(), logical_schema, StoreOptions::default())
        .expect("reopen logical table");
    assert_eq!(reopened.snapshot().scan().expect("logical scan"), [row]);
}

fn schema() -> TableSchema {
    TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema")
}

fn row(value: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Utf8("key".into())]).expect("key"),
        vec![Value::Utf8(value.into())],
        version,
        deleted,
    )
}
