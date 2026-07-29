use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[test]
fn checkpointed_rows_are_recovered_from_the_wal_after_reopen() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = account_schema();
    let options = StoreOptions {
        wal_sync: WalSync::Checkpoint,
        ..StoreOptions::default()
    };

    {
        let mut table =
            TableStore::open(directory.path(), schema.clone(), options).expect("open table");
        table
            .ingest(vec![
                account(1, "Ada", 1, false),
                account(2, "Linus", 2, false),
            ])
            .expect("ingest rows");
        table.checkpoint().expect("durable checkpoint");

        assert_eq!(
            table.snapshot().scan().expect("scan memtable"),
            vec![account(1, "Ada", 1, false), account(2, "Linus", 2, false)]
        );
    }

    let reopened = TableStore::open(directory.path(), schema, options).expect("reopen table");
    assert_eq!(
        reopened.snapshot().scan().expect("scan recovered rows"),
        vec![account(1, "Ada", 1, false), account(2, "Linus", 2, false)]
    );
}

#[test]
fn a_torn_final_wal_record_is_discarded_without_losing_prior_batches() {
    use std::io::Write;

    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = account_schema();
    {
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![account(7, "Grace", 1, false)])
            .expect("ingest row");
        table.checkpoint().expect("checkpoint");
    }

    let wal_path = directory.path().join("table.wal");
    let valid_length = std::fs::metadata(&wal_path).expect("WAL metadata").len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .expect("open WAL tail")
        .write_all(&[0xde, 0xad, 0xbe])
        .expect("append torn length");

    let reopened =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("recover table");
    assert_eq!(
        reopened.snapshot().scan().expect("scan"),
        vec![account(7, "Grace", 1, false)]
    );
    assert_eq!(
        std::fs::metadata(wal_path).expect("repaired WAL").len(),
        valid_length
    );
}

#[test]
fn checksum_corruption_reports_the_failing_wal_offset() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    {
        let mut table =
            TableStore::open(directory.path(), account_schema(), StoreOptions::default())
                .expect("open table");
        table
            .ingest(vec![account(9, "Margaret", 1, false)])
            .expect("ingest row");
        table.checkpoint().expect("checkpoint");
    }

    let wal_path = directory.path().join("table.wal");
    let mut wal = std::fs::read(&wal_path).expect("read WAL");
    wal[10] ^= 0x5a;
    std::fs::write(&wal_path, wal).expect("corrupt WAL payload");

    let error = TableStore::open(directory.path(), account_schema(), StoreOptions::default())
        .err()
        .expect("corrupt WAL must fail");
    assert!(
        error
            .to_string()
            .contains("corrupt WAL at byte 6: record checksum mismatch"),
        "{error}"
    );
}

#[test]
fn reopen_verifies_live_segment_footers_before_returning() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let segment_path = {
        let mut table =
            TableStore::open(directory.path(), account_schema(), StoreOptions::default())
                .expect("open table");
        table
            .ingest(vec![account(1, "Ada", 1, false)])
            .expect("ingest");
        table
            .flush()
            .expect("flush")
            .segment_path()
            .expect("segment")
            .to_path_buf()
    };
    let mut bytes = std::fs::read(&segment_path).expect("segment bytes");
    let footer_checksum = bytes.len() - 16;
    bytes[footer_checksum] ^= 0x5a;
    std::fs::write(&segment_path, bytes).expect("corrupt footer");

    let error = TableStore::open(directory.path(), account_schema(), StoreOptions::default())
        .err()
        .expect("corrupt footer must fail during open");
    assert!(
        error.to_string().contains("footer checksum mismatch"),
        "{error}"
    );
}

#[test]
fn reopen_removes_unpublished_segments_left_by_a_crash() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let live_path = {
        let mut table =
            TableStore::open(directory.path(), account_schema(), StoreOptions::default())
                .expect("open table");
        table
            .ingest(vec![account(1, "Ada", 1, false)])
            .expect("ingest");
        table
            .flush()
            .expect("flush")
            .segment_path()
            .expect("segment")
            .to_path_buf()
    };
    let orphan = directory.path().join("segment-99999999999999999999.ptseg");
    std::fs::copy(&live_path, &orphan).expect("simulate pre-manifest segment");

    let reopened = TableStore::open(directory.path(), account_schema(), StoreOptions::default())
        .expect("reopen");
    assert!(!orphan.exists(), "unpublished segment is not live");
    assert_eq!(
        reopened.snapshot().scan().expect("scan"),
        vec![account(1, "Ada", 1, false)]
    );
}

#[test]
fn manifest_checkpoint_wins_if_a_crash_prevents_wal_truncation() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let stale_wal = {
        let mut table =
            TableStore::open(directory.path(), account_schema(), StoreOptions::default())
                .expect("open table");
        table
            .ingest(vec![account(1, "Ada", 1, false)])
            .expect("ingest");
        table.checkpoint().expect("checkpoint");
        let bytes = std::fs::read(directory.path().join("table.wal")).expect("pre-flush WAL");
        table.flush().expect("flush");
        bytes
    };
    std::fs::write(directory.path().join("table.wal"), stale_wal)
        .expect("simulate crash before WAL reset");

    let reopened = TableStore::open(directory.path(), account_schema(), StoreOptions::default())
        .expect("reopen");
    assert_eq!(
        reopened.snapshot().scan().expect("scan"),
        vec![account(1, "Ada", 1, false)]
    );
    assert_eq!(
        std::fs::metadata(directory.path().join("table.wal"))
            .expect("WAL metadata")
            .len(),
        6,
        "recovery removes records already covered by the manifest"
    );
}

fn account_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .expect("account schema")
}

fn account(id: u64, name: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("account key"),
        vec![Value::UInt64(id), Value::Utf8(name.into())],
        version,
        deleted,
    )
}
