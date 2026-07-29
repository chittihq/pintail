use std::{
    process::{Command, Stdio},
    time::Duration,
};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rand::{Rng, SeedableRng, rngs::StdRng};

const WORKER_ENV: &str = "PINTAIL_CRASH_FUZZ_WORKER";
const DIRECTORY_ENV: &str = "PINTAIL_CRASH_FUZZ_DIRECTORY";
const ITERATIONS: usize = 100;

#[test]
fn short_kill9_crash_fuzz_reopens_to_a_valid_monotonic_prefix() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let executable = std::env::current_exe().expect("current test executable");
    let mut random = StdRng::seed_from_u64(0x0050_494e_5441_494c);
    let mut recovered_version = 0;

    for iteration in 0..ITERATIONS {
        let mut child = Command::new(&executable)
            .args([
                "--ignored",
                "--exact",
                "crash_fuzz_worker",
                "--test-threads=1",
            ])
            .env(WORKER_ENV, "1")
            .env(DIRECTORY_ENV, directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crash worker");
        std::thread::sleep(Duration::from_millis(random.random_range(8..40)));
        child.kill().expect("kill crash worker");
        child.wait().expect("reap crash worker");

        let table = TableStore::open(directory.path(), schema(), options())
            .unwrap_or_else(|error| panic!("iteration {iteration} failed to reopen: {error}"));
        let rows = table
            .snapshot()
            .scan()
            .unwrap_or_else(|error| panic!("iteration {iteration} failed to scan: {error}"));
        assert!(rows.len() <= 1, "single-key worker returned {rows:?}");
        if let Some(row) = rows.first() {
            assert!(
                row.version() >= recovered_version,
                "iteration {iteration} regressed from {recovered_version} to {}",
                row.version()
            );
            assert_eq!(
                row.values(),
                [
                    Value::UInt64(1),
                    Value::Utf8(format!("version-{}", row.version()))
                ],
                "iteration {iteration} recovered a non-atomic row"
            );
            recovered_version = row.version();
        }
        drop(table);
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
    let mut table = TableStore::open(directory, schema(), options()).expect("worker open");
    let mut version = table
        .snapshot()
        .scan()
        .expect("worker initial scan")
        .first()
        .map_or(0, StoredRow::version);

    loop {
        version += 1;
        table
            .ingest(vec![StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
                vec![Value::UInt64(1), Value::Utf8(format!("version-{version}"))],
                version,
                false,
            )])
            .expect("worker ingest");
        if version % 2 == 0 {
            table.flush().expect("worker flush");
        }
        if table
            .compaction_status()
            .expect("worker compaction status")
            .eligible_segments()
            > 0
        {
            table.compact().expect("worker compact");
        }
        std::thread::yield_now();
    }
}

fn options() -> StoreOptions {
    StoreOptions {
        wal_sync: WalSync::Always,
        compaction_fan_in: 4,
        ..StoreOptions::default()
    }
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
