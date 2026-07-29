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
    assert_eq!(
        table.snapshot().scan().expect("scan"),
        vec![row("automatically flushed", 1, false)]
    );
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
