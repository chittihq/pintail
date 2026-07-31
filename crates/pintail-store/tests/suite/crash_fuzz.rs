use std::{
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use pintail_store::{DatabaseStore, StoreOptions, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rand::{Rng, SeedableRng, rngs::StdRng};

const WORKER_ENV: &str = "PINTAIL_CRASH_FUZZ_WORKER";
const DIRECTORY_ENV: &str = "PINTAIL_CRASH_FUZZ_DIRECTORY";
const ACK_ADDRESS_ENV: &str = "PINTAIL_CRASH_FUZZ_ACK_ADDRESS";
const ITERATIONS: usize = 100;
const USERS: u64 = 17;
const ORDERS: u64 = 29;

#[test]
fn short_kill9_crash_fuzz_matches_the_acknowledged_commit_oracle() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let executable = std::env::current_exe().expect("current test executable");
    let mut random = StdRng::seed_from_u64(0x0050_494e_5441_494c);
    let mut recovered_version = 0_u64;

    for iteration in 0..ITERATIONS {
        let ack_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind acknowledgement pipe");
        let ack_address = ack_listener
            .local_addr()
            .expect("acknowledgement pipe address");
        let mut child = Command::new(&executable)
            .args([
                "--ignored",
                "--exact",
                "suite::crash_fuzz::crash_fuzz_worker",
                "--test-threads=1",
            ])
            .env(WORKER_ENV, "1")
            .env(DIRECTORY_ENV, directory.path())
            .env(ACK_ADDRESS_ENV, ack_address.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crash worker");
        let acknowledged = Arc::new(AtomicU64::new(recovered_version));
        let reader_acknowledged = Arc::clone(&acknowledged);
        let (mut ack_stream, _) = ack_listener.accept().expect("accept acknowledgement pipe");
        let ack_reader = std::thread::spawn(move || {
            loop {
                let mut bytes = [0_u8; size_of::<u64>()];
                match ack_stream.read_exact(&mut bytes) {
                    Ok(()) => {
                        reader_acknowledged.store(u64::from_le_bytes(bytes), Ordering::Release);
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
                    Err(error) => panic!("read worker acknowledgement: {error}"),
                }
            }
        });
        std::thread::sleep(Duration::from_millis(random.random_range(8..40)));
        child.kill().expect("kill crash worker");
        child.wait().expect("reap crash worker");
        ack_reader.join().expect("join acknowledgement reader");

        let acknowledged_version = acknowledged.load(Ordering::Acquire);
        let database = DatabaseStore::open(directory.path(), schemas(), options())
            .unwrap_or_else(|error| panic!("iteration {iteration} failed to reopen: {error}"));
        let actual = database_state(&database)
            .unwrap_or_else(|error| panic!("iteration {iteration} failed to scan: {error}"));
        let acknowledged = expected_state(acknowledged_version);
        let in_flight = expected_state(acknowledged_version.saturating_add(1));
        assert!(
            actual == acknowledged || actual == in_flight,
            "iteration {iteration} recovered {actual:?}, outside exact database oracle \
             {acknowledged:?} or {in_flight:?}"
        );
        let actual_version = actual
            .iter()
            .filter_map(|row| row.as_ref().map(StoredRow::version))
            .max()
            .unwrap_or(0);
        assert!(
            actual_version >= recovered_version,
            "iteration {iteration} regressed from {recovered_version} to {actual_version}"
        );
        recovered_version = actual_version;
        drop(database);
    }

    assert!(
        recovered_version > 0,
        "crash workers made no durable progress"
    );
}

#[test]
#[ignore = "spawned and killed by the crash-fuzz parent"]
fn crash_fuzz_worker() {
    if std::env::var_os(WORKER_ENV).is_none() {
        return;
    }
    let directory = std::env::var_os(DIRECTORY_ENV).expect("worker directory");
    let ack_address = std::env::var(ACK_ADDRESS_ENV).expect("acknowledgement pipe address");
    let mut ack_stream = TcpStream::connect(ack_address).expect("connect acknowledgement pipe");
    let mut database = DatabaseStore::open(&directory, schemas(), options()).expect("worker open");
    let mut version = database_state(&database)
        .expect("worker initial scan")
        .iter()
        .filter_map(|row| row.as_ref().map(StoredRow::version))
        .max()
        .unwrap_or(0);

    loop {
        version += 1;
        let table_id = table_for_version(version);
        database
            .ingest(table_id, vec![versioned_row(table_id, version)])
            .expect("worker ingest");
        acknowledge_version(&mut ack_stream, version);
        database.flush(table_id).expect("worker flush");
        database.compact(table_id).expect("worker compact");
        std::thread::yield_now();
    }
}

fn acknowledge_version(stream: &mut TcpStream, version: u64) {
    stream
        .write_all(&version.to_le_bytes())
        .and_then(|()| stream.flush())
        .expect("send crash-fuzz acknowledgement");
}

fn options() -> StoreOptions {
    StoreOptions {
        wal_sync: WalSync::Always,
        compaction_fan_in: 4,
        ..StoreOptions::default()
    }
}

fn schemas() -> Vec<(u64, TableSchema)> {
    vec![(USERS, schema()), (ORDERS, schema())]
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn table_for_version(version: u64) -> u64 {
    if version % 2 == 1 { USERS } else { ORDERS }
}

fn versioned_row(table_id: u64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
        vec![
            Value::UInt64(table_id),
            Value::Utf8(format!("table-{table_id}-version-{version}")),
        ],
        version,
        false,
    )
}

fn expected_state(version: u64) -> [Option<StoredRow>; 2] {
    [latest_row(USERS, version), latest_row(ORDERS, version)]
}

fn latest_row(table_id: u64, version: u64) -> Option<StoredRow> {
    let wants_odd = table_id == USERS;
    let latest = if (version % 2 == 1) == wants_odd {
        version
    } else {
        version.saturating_sub(1)
    };
    (latest > 0).then(|| versioned_row(table_id, latest))
}

fn database_state(database: &DatabaseStore) -> Result<[Option<StoredRow>; 2], String> {
    let users = database
        .snapshot(USERS)
        .and_then(|snapshot| snapshot.scan())
        .map_err(|error| error.to_string())?;
    let orders = database
        .snapshot(ORDERS)
        .and_then(|snapshot| snapshot.scan())
        .map_err(|error| error.to_string())?;
    if users.len() > 1 || orders.len() > 1 {
        return Err(format!(
            "single-key tables returned users={users:?}, orders={orders:?}"
        ));
    }
    Ok([users.into_iter().next(), orders.into_iter().next()])
}
