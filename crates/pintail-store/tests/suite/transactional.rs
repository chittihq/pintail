use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, name: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(name.to_owned())],
        0,
        false,
    )
}

fn transactional() -> StoreOptions {
    StoreOptions {
        transactional: true,
        wal_sync: WalSync::Off,
        ..StoreOptions::default()
    }
}

#[test]
fn committed_transactions_survive_reopen_with_their_versions() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table =
            TableStore::open(directory.path(), schema(), transactional()).expect("open");
        assert_eq!(table.commit(vec![row(1, "ada")]).expect("commit one"), 1);
        assert_eq!(
            table
                .commit(vec![row(2, "grace"), row(3, "edsger")])
                .expect("commit two"),
            2
        );
        assert_eq!(table.commit_version(), 2);
    }
    let reopened = TableStore::open(directory.path(), schema(), transactional()).expect("reopen");
    assert_eq!(reopened.commit_version(), 2);
    let rows = reopened.snapshot().scan().expect("scan");
    assert_eq!(rows.len(), 3);
    // Rows carry their commit version.
    assert_eq!(rows[0].version(), 1);
    assert_eq!(rows[1].version(), 2);
    assert_eq!(rows[2].version(), 2);
}

#[test]
fn uncommitted_wal_rows_vanish_on_transactional_reopen() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table =
            TableStore::open(directory.path(), schema(), transactional()).expect("open");
        table.commit(vec![row(1, "ada")]).expect("commit");
    }
    {
        // A crash between the row batch and its commit record: model it by
        // appending a batch through the non-transactional path, which
        // writes no commit marker.
        let mut plain = TableStore::open(
            directory.path(),
            schema(),
            StoreOptions {
                wal_sync: WalSync::Always,
                ..StoreOptions::default()
            },
        )
        .expect("open plain");
        plain.ingest(vec![row(9, "phantom")]).expect("ingest");
    }
    let reopened = TableStore::open(directory.path(), schema(), transactional()).expect("reopen");
    let rows = reopened.snapshot().scan().expect("scan");
    assert_eq!(rows.len(), 1, "uncommitted tail row must vanish");
    assert_eq!(rows[0].values()[1], Value::Utf8("ada".to_owned()));
    assert_eq!(reopened.commit_version(), 1);

    // The tail is physically gone: another reopen finds a clean log.
    drop(reopened);
    let again = TableStore::open(directory.path(), schema(), transactional()).expect("again");
    assert_eq!(again.snapshot().scan().expect("scan").len(), 1);
}

#[test]
fn a_torn_tail_after_a_commit_keeps_the_committed_prefix() {
    use std::io::Write as _;
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table =
            TableStore::open(directory.path(), schema(), transactional()).expect("open");
        table.commit(vec![row(1, "ada")]).expect("commit");
    }
    let wal_path = directory.path().join("table.wal");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .expect("open wal");
    file.write_all(&[0xAB; 11]).expect("torn garbage");
    drop(file);

    let reopened = TableStore::open(directory.path(), schema(), transactional()).expect("reopen");
    assert_eq!(reopened.snapshot().scan().expect("scan").len(), 1);
    assert_eq!(reopened.commit_version(), 1);
}

#[test]
fn commit_versions_survive_flush_and_restart() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table =
            TableStore::open(directory.path(), schema(), transactional()).expect("open");
        table.commit(vec![row(1, "ada")]).expect("commit");
        table.commit(vec![row(2, "grace")]).expect("commit");
        table.flush().expect("flush");
    }
    let mut reopened =
        TableStore::open(directory.path(), schema(), transactional()).expect("reopen");
    assert_eq!(
        reopened.commit_version(),
        2,
        "flushed commit version persists in the manifest"
    );
    assert_eq!(reopened.commit(vec![row(3, "edsger")]).expect("next"), 3);
}

#[test]
fn commit_requires_a_transactional_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    assert!(table.commit(vec![row(1, "ada")]).is_err());
}
