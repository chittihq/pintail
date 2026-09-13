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

/// Removes the log's last record and returns its payload. The layout is
/// the WAL's own: a six-byte header, then a little-endian `u32` length,
/// that many payload bytes, and an eight-byte checksum, repeated.
fn strip_the_last_record(path: &std::path::Path) -> Vec<u8> {
    const HEADER: usize = 6;
    const CHECKSUM: usize = 8;
    let bytes = std::fs::read(path).expect("read wal");
    let mut position = HEADER;
    let mut last = None;
    while position < bytes.len() {
        let start = position;
        let length = u32::from_le_bytes(
            bytes[position..position + size_of::<u32>()]
                .try_into()
                .expect("length"),
        ) as usize;
        position += size_of::<u32>();
        last = Some((start, bytes[position..position + length].to_vec()));
        position += length + CHECKSUM;
    }
    let (start, payload) = last.expect("the log holds at least one record");
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open wal")
        .set_len(start as u64)
        .expect("truncate");
    payload
}

/// A crash between the first commit's row batch and its commit record used
/// to strand the table for good: recovery dropped the batch but kept its
/// sequence, so the next write started at 2, and a reopen before any flush
/// saw a log starting after sequence 1 with no manifest - the signature of
/// flushed rows whose manifest was lost. Every open from then on refused.
#[test]
fn a_crash_before_the_first_commit_record_leaves_the_table_openable() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table =
            TableStore::open(directory.path(), schema(), transactional()).expect("open");
        table.commit(vec![row(1, "ada")]).expect("commit");
    }
    let payload = strip_the_last_record(&directory.path().join("table.wal"));
    // Confirm what was removed really is the commit record - sequence,
    // the reserved table id marking a commit, and the version - so a
    // layout change fails here instead of quietly cutting a row batch and
    // leaving this test passing on a log it never reproduced.
    assert_eq!(payload.len(), 3 * size_of::<u64>(), "commit record payload");
    assert_eq!(
        u64::from_le_bytes(payload[8..16].try_into().expect("table id")),
        u64::MAX,
        "the removed record must be the commit marker"
    );

    {
        let mut table = TableStore::open(directory.path(), schema(), transactional())
            .expect("reopen after the crash");
        assert!(
            table.snapshot().scan().expect("scan").is_empty(),
            "a batch whose commit record never landed is not visible"
        );
        assert_eq!(
            table.commit(vec![row(2, "grace")]).expect("commit again"),
            1,
            "the lost transaction consumed no version"
        );
    }

    let reopened = TableStore::open(directory.path(), schema(), transactional())
        .expect("the table stays openable: the lost batch consumed no sequence either");
    let rows = reopened.snapshot().scan().expect("scan");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[1], Value::Utf8("grace".to_owned()));
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
