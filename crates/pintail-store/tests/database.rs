use pintail_store::{DatabaseStore, StoreError, StoreOptions, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const USERS: u64 = 17;
const ORDERS: u64 = 29;

#[test]
fn one_database_wal_sequences_and_recovers_multiple_tables() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let options = StoreOptions {
        wal_sync: WalSync::Always,
        ..StoreOptions::default()
    };
    let mut database =
        DatabaseStore::open(directory.path(), schemas(), options).expect("open database");

    let first = database
        .ingest(USERS, vec![row("alice", 1)])
        .expect("ingest user");
    let second = database
        .ingest(ORDERS, vec![row("order-1", 1)])
        .expect("ingest order");
    let third = database
        .ingest(USERS, vec![row("alice-updated", 2)])
        .expect("update user");
    assert_eq!(
        (first.sequence(), second.sequence(), third.sequence()),
        (1, 2, 3)
    );

    assert!(directory.path().join("database.wal").is_file());
    assert!(
        !directory
            .path()
            .join("tables")
            .join(USERS.to_string())
            .join("table.wal")
            .exists()
    );
    drop(database);

    let database =
        DatabaseStore::open(directory.path(), schemas(), options).expect("recover database");
    assert_eq!(
        database
            .snapshot(USERS)
            .expect("users")
            .scan()
            .expect("scan"),
        vec![row("alice-updated", 2)]
    );
    assert_eq!(
        database
            .snapshot(ORDERS)
            .expect("orders")
            .scan()
            .expect("scan"),
        vec![row("order-1", 1)]
    );
}

#[test]
fn flushing_one_table_preserves_other_table_records_until_all_are_published() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let options = StoreOptions {
        wal_sync: WalSync::Always,
        ..StoreOptions::default()
    };
    let mut database =
        DatabaseStore::open(directory.path(), schemas(), options).expect("open database");
    database
        .ingest(USERS, vec![row("published", 1)])
        .expect("ingest user");
    database
        .ingest(ORDERS, vec![row("still-in-wal", 1)])
        .expect("ingest order");

    database.flush(USERS).expect("flush users");
    assert!(
        std::fs::metadata(directory.path().join("database.wal"))
            .expect("WAL metadata")
            .len()
            > 6,
        "an unflushed table keeps the shared WAL live"
    );
    drop(database);

    let mut recovered =
        DatabaseStore::open(directory.path(), schemas(), options).expect("recover database");
    assert_eq!(
        recovered
            .snapshot(USERS)
            .expect("users")
            .scan()
            .expect("scan"),
        vec![row("published", 1)]
    );
    assert_eq!(
        recovered
            .snapshot(ORDERS)
            .expect("orders")
            .scan()
            .expect("scan"),
        vec![row("still-in-wal", 1)]
    );

    recovered.flush(ORDERS).expect("flush orders");
    assert_eq!(
        std::fs::metadata(directory.path().join("database.wal"))
            .expect("WAL metadata")
            .len(),
        6,
        "the WAL is redundant only after every memtable is published"
    );
}

#[test]
fn recovery_rejects_an_unregistered_table_still_present_in_the_wal() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let options = StoreOptions {
        wal_sync: WalSync::Always,
        ..StoreOptions::default()
    };
    let mut database = DatabaseStore::open(directory.path(), vec![(USERS, schema())], options)
        .expect("open database");
    database
        .ingest(USERS, vec![row("must-not-be-lost", 1)])
        .expect("ingest");
    drop(database);

    let Err(error) = DatabaseStore::open(directory.path(), vec![(ORDERS, schema())], options)
    else {
        panic!("unknown WAL table must fail recovery");
    };
    assert!(matches!(
        error,
        StoreError::UnknownTable { table_id: USERS }
    ));
}

fn schemas() -> Vec<(u64, TableSchema)> {
    vec![(USERS, schema()), (ORDERS, schema())]
}

fn schema() -> TableSchema {
    TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema")
}

fn row(value: &str, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Utf8("key".into())]).expect("key"),
        vec![Value::Utf8(value.into())],
        version,
        false,
    )
}
