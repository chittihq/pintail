//! Freshness under an open writer. A table whose writer is open in this
//! process - every table under replication - is proven current by the
//! generation the writer publishes after each change, not by walking its
//! files on every query. These tests hold writers the way replication does
//! and check both halves: every kind of change is seen by the next query,
//! and a query with nothing changed touches no table file at all.

mod common;

use std::sync::{Mutex, PoisonError};

use common::{Replica, bodies, count, row};
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
fn an_open_writer_proves_its_table_current_without_a_file_walk() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let replica = Replica::seed();
    let mut a = replica.writer("a");
    let mut b = replica.writer("b");
    a.ingest(vec![row(1, "one", 1, false)]).expect("ingest a");
    b.ingest(vec![row(1, "one", 1, false)]).expect("ingest b");
    let engine = replica.engine();
    assert_eq!(count(&engine, "a"), Ok(1));

    let before = replica_cache_stats();
    assert_eq!(count(&engine, "a"), Ok(1));
    assert_eq!(count(&engine, "b"), Ok(1));
    assert_eq!(bodies(&engine, "a"), ["one"]);
    assert_eq!(
        delta(before),
        (0, 0, 0),
        "nothing changed: no reload, and no table file inspected"
    );

    // An apply to one table reopens that table and nothing else.
    let before = replica_cache_stats();
    a.ingest(vec![row(2, "two", 2, false)]).expect("ingest a");
    assert_eq!(count(&engine, "a"), Ok(2));
    assert_eq!(count(&engine, "b"), Ok(1));
    assert_eq!(delta(before), (1, 1, 0));
}

#[test]
fn every_kind_of_change_under_an_open_writer_reaches_the_next_query() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let replica = Replica::seed();
    let mut a = replica.writer("a");
    let engine = replica.engine();
    assert_eq!(count(&engine, "a"), Ok(0));

    a.ingest(vec![row(1, "one", 1, false), row(2, "two", 1, false)])
        .expect("ingest");
    assert_eq!(bodies(&engine, "a"), ["one", "two"], "an apply");

    a.flush().expect("flush");
    assert_eq!(bodies(&engine, "a"), ["one", "two"], "a flush moves rows");

    a.ingest(vec![row(1, "uno", 2, false), row(2, "", 2, true)])
        .expect("update and delete");
    assert_eq!(bodies(&engine, "a"), ["uno"], "an update and a delete");

    a.flush().expect("flush");
    let compacted = a.compact().expect("compact");
    assert!(
        compacted.input_segments() >= 2,
        "the test must exercise a merge"
    );
    assert_eq!(bodies(&engine, "a"), ["uno"], "a compaction");
    a.reclaim_obsolete_segments().expect("reclaim");
    assert_eq!(bodies(&engine, "a"), ["uno"], "reclaimed inputs");

    a.reset_for_resnapshot().expect("reset");
    assert_eq!(count(&engine, "a"), Ok(0), "a reset before a recopy");
}

#[test]
fn a_table_without_an_open_writer_is_walked_and_still_current() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let replica = Replica::seed();
    let engine = replica.engine();
    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(1, "one", 1, false)]).expect("ingest");
        assert_eq!(count(&engine, "a"), Ok(1));
    }
    // The writer closed: the generation no longer vouches for the files,
    // so they are walked - and a change made by a writer that is no
    // longer open is still seen.
    let before = replica_cache_stats();
    assert_eq!(count(&engine, "a"), Ok(1));
    assert!(delta(before).2 > 0, "the closed table's files were walked");

    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(2, "two", 2, false)]).expect("ingest");
    }
    assert_eq!(count(&engine, "a"), Ok(2));

    // Reopened and written again: the next writer's generations never
    // repeat the previous one's.
    let mut a = replica.writer("a");
    assert_eq!(count(&engine, "a"), Ok(2));
    a.ingest(vec![row(3, "three", 3, false)]).expect("ingest");
    assert_eq!(count(&engine, "a"), Ok(3));
}
