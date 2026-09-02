//! The replica cache as connections see it: one loaded copy per database
//! for the whole process, a commit to one table reopening one table, and
//! the resident memtables charged to the process budget.
//!
//! This is the seam the load harness measured (`tests/load/results.md`):
//! every wire connection used to load and hold its own copy of every table,
//! so peak RSS scaled with connections rather than with data.

use std::sync::Mutex;

use pintail_meta::MetaStore;
use pintail_sql::parse_statement;
use pintail_wire::{ReplicaCacheStats, ReplicaEngine, replica_cache_stats};
use pintail_write::LocalDatabase;

/// The stats are process-wide, so the tests in this binary take turns.
static SERIAL: Mutex<()> = Mutex::new(());

struct Fixture {
    _directory: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    metadata_path: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let data_dir = directory.path().to_path_buf();
        let metadata_path = data_dir.join("pintail-meta.db");
        let metadata = MetaStore::open(&metadata_path).expect("metadata");
        metadata
            .create_local_database("db-local", "scratch", "2026-09-02T00:00:00Z")
            .expect("create local database");
        drop(metadata);
        std::fs::create_dir_all(data_dir.join("databases").join("db-local").join("tables"))
            .expect("table root");
        LocalDatabase::new(&data_dir, &metadata_path, "db-local")
            .recover()
            .expect("initialize catalog");
        let fixture = Self {
            _directory: directory,
            data_dir,
            metadata_path,
        };
        let engine = fixture.engine();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, body VARCHAR(64) NOT NULL, PRIMARY KEY (id))",
            "CREATE TABLE b (id BIGINT UNSIGNED NOT NULL, body VARCHAR(64) NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a (id, body) VALUES (1, 'one')",
            "INSERT INTO b (id, body) VALUES (1, 'one')",
        ] {
            engine.execute("db-local", sql, 10).expect(sql);
        }
        fixture
    }

    fn engine(&self) -> ReplicaEngine {
        ReplicaEngine::new(&self.data_dir, &self.metadata_path)
    }

    /// A write that reaches the files and the metadata store without going
    /// through any engine - what CDC apply looks like to a reader.
    fn write_behind_the_engines(&self, sql: &str) {
        let statement = parse_statement(sql).expect("parse");
        LocalDatabase::new(&self.data_dir, &self.metadata_path, "db-local")
            .execute(&statement)
            .expect(sql);
    }
}

fn count(engine: &ReplicaEngine, table: &str) -> u64 {
    let output = engine
        .execute("db-local", &format!("SELECT COUNT(*) FROM {table}"), 10)
        .expect("count");
    match output.rows[0][0] {
        pintail_types::Value::UInt64(count) => count,
        ref other => panic!("COUNT(*) is not an unsigned integer: {other:?}"),
    }
}

fn delta(before: ReplicaCacheStats, after: ReplicaCacheStats) -> (u64, u64, u64) {
    (
        after.hits - before.hits,
        after.loads - before.loads,
        after.tables_opened - before.tables_opened,
    )
}

#[test]
fn every_engine_in_the_process_reads_one_loaded_copy() {
    let _serial = SERIAL.lock().expect("serial");
    let fixture = Fixture::new();
    let first = fixture.engine();
    let second = fixture.engine();

    assert_eq!(count(&first, "a"), 1);
    let before = replica_cache_stats();
    // A second engine - a second wire connection, or an HTTP request - must
    // not load its own copy.
    assert_eq!(count(&second, "a"), 1);
    assert_eq!(count(&second, "b"), 1);
    assert_eq!(
        delta(before, replica_cache_stats()),
        (2, 0, 0),
        "the second engine hit the copy the first one loaded"
    );
    let stats = replica_cache_stats();
    assert!(stats.databases >= 1);
    assert!(
        stats.resident_bytes > 0,
        "unflushed rows are resident and must be charged: {stats:?}"
    );
}

#[test]
fn a_commit_to_one_table_reopens_that_table_only() {
    let _serial = SERIAL.lock().expect("serial");
    let fixture = Fixture::new();
    let engine = fixture.engine();
    assert_eq!(count(&engine, "a"), 1);

    fixture.write_behind_the_engines("INSERT INTO a (id, body) VALUES (2, 'two')");
    let before = replica_cache_stats();
    assert_eq!(count(&engine, "a"), 2, "the read sees the commit");
    assert_eq!(count(&engine, "b"), 1);
    assert_eq!(
        delta(before, replica_cache_stats()),
        (1, 1, 1),
        "one reload, and it opened one table - `b` did not change"
    );
}

#[test]
fn a_write_through_the_engine_is_seen_by_every_other_engine() {
    let _serial = SERIAL.lock().expect("serial");
    let fixture = Fixture::new();
    let writer = fixture.engine();
    let reader = fixture.engine();
    assert_eq!(count(&reader, "b"), 1);

    writer
        .execute("db-local", "INSERT INTO b (id, body) VALUES (2, 'two')", 10)
        .expect("insert");
    assert_eq!(
        count(&reader, "b"),
        2,
        "the shared cache was invalidated for everyone, not just the writer"
    );
}
