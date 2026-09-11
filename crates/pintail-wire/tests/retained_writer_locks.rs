//! Freshness between writers, in a process that keeps its tables' writer
//! locks. Replication opens a table's writer for one cycle and closes it; a
//! server that retains the lock keeps proving the table current from its
//! generation between cycles, instead of walking the table's files on every
//! query - and still sees every change the next cycle makes.
//!
//! Retention is process-wide, so these tests have a binary of their own.

mod common;

use std::sync::{Mutex, PoisonError};

use common::{Replica, count, row};
use pintail_wire::{ReplicaCacheStats, replica_cache_stats};

/// The cache counters are process-wide, so the tests here take turns.
static SERIAL: Mutex<()> = Mutex::new(());

/// (loads, tables opened, table files walked) since `before`.
fn delta(before: ReplicaCacheStats) -> (u64, u64, u64) {
    let after = replica_cache_stats();
    (
        after.loads - before.loads,
        after.tables_opened - before.tables_opened,
        after.walked_files - before.walked_files,
    )
}

#[test]
fn a_table_between_writers_is_proven_current_without_a_file_walk() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    pintail_store::retain_writer_locks();
    let replica = Replica::seed();
    let engine = replica.engine();
    {
        // One replication cycle: open, apply, close.
        let mut a = replica.writer("a");
        a.ingest(vec![row(1, "one", 1, false)]).expect("ingest");
    }
    assert_eq!(count(&engine, "a"), Ok(1));

    let before = replica_cache_stats();
    assert_eq!(count(&engine, "a"), Ok(1));
    assert_eq!(count(&engine, "b"), Ok(0));
    assert_eq!(
        delta(before),
        (0, 0, 0),
        "between writers: no reload and no table file inspected"
    );

    // A cycle that applies nothing opens and closes the writer without
    // moving the generation, so the replica stays loaded.
    let before = replica_cache_stats();
    drop(replica.writer("a"));
    assert_eq!(count(&engine, "a"), Ok(1));
    assert_eq!(delta(before), (0, 0, 0), "an empty cycle changes nothing");

    // A cycle that applies a change is seen by the next query.
    let before = replica_cache_stats();
    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(2, "two", 2, false)]).expect("ingest");
        a.flush().expect("flush");
    }
    assert_eq!(count(&engine, "a"), Ok(2));
    assert_eq!(delta(before), (1, 1, 0), "the changed table alone reopens");
}

#[test]
fn a_table_directory_recreated_underneath_is_read_afresh() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    pintail_store::retain_writer_locks();
    let replica = Replica::seed();
    let engine = replica.engine();
    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(1, "one", 1, false), row(2, "two", 1, false)])
            .expect("ingest");
    }
    assert_eq!(count(&engine, "a"), Ok(2));

    // A recopy removes the table's directory and builds it again: the lease
    // on the old directory must not vouch for the new one.
    let root = replica
        .data_dir
        .join("databases")
        .join(common::DATABASE)
        .join("tables");
    let directory = pintail_wire::table_directory(&root, "a");
    std::fs::remove_dir_all(&directory).expect("remove");
    pintail_store::publish_changes_under(&directory);
    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(7, "seven", 3, false)]).expect("ingest");
    }
    assert_eq!(count(&engine, "a"), Ok(1));
}
