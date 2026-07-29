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
