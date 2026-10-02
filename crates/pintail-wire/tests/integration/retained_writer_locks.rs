//! Freshness between writers, in a process that keeps its tables' writer
//! locks. Replication opens a table's writer for one cycle and closes it; a
//! server that retains the lock keeps proving the table current from its
//! generation between cycles, instead of walking the table's files on every
//! query - and still sees every change the next cycle makes.
//!
//! Retention is process-wide, so these tests have a binary of their own.

use crate::common;

use std::sync::{Mutex, PoisonError};

use common::{Replica, count, row, source_table};
use pintail_meta::MetaStore;
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

/// The only writer of a data directory asks the file system nothing to
/// prove a replica current: its own metadata commits and its own new table
/// directories tell it. Each must still be seen by the very next query.
#[test]
fn the_only_writer_sees_its_metadata_commits_and_new_tables_at_once() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    pintail_store::retain_writer_locks();
    let replica = Replica::seed();
    let engine = replica.engine();
    {
        let mut a = replica.writer("a");
        a.ingest(vec![row(1, "one", 1, false)]).expect("ingest");
    }
    assert_eq!(count(&engine, "a"), Ok(1));
    assert_eq!(count(&engine, "b"), Ok(0));

    // A metadata commit that changes what a query sees: the source no
    // longer has `b`.
    replica.probe(vec![source_table("a")]);
    assert!(count(&engine, "b").is_err(), "the dropped table is gone");
    assert_eq!(count(&engine, "a"), Ok(1));
    replica.probe(vec![source_table("a"), source_table("b")]);
    assert_eq!(count(&engine, "b"), Ok(0), "and back");

    // A table copied in while queries run: a new directory under the
    // tables root, which no listing taken before it has.
    replica.probe(vec![
        source_table("a"),
        source_table("b"),
        source_table("c"),
    ]);
    {
        let mut metadata = MetaStore::open(&replica.metadata_path).expect("metadata");
        metadata
            .upsert_snapshot_table(common::DATABASE, "c", Some(r#"["id"]"#), Some(r#"["id"]"#))
            .expect("table");
        metadata
            .start_snapshot_chunk(common::DATABASE, "c", "all", None, None)
            .expect("chunk");
        metadata
            .complete_snapshot_chunk(common::DATABASE, "c", "all", 0)
            .expect("chunk complete");
        metadata
            .complete_snapshot_table(common::DATABASE, "c")
            .expect("copy complete");
    }
    {
        let mut c = replica.writer("c");
        c.ingest(vec![row(1, "one", 1, false)]).expect("ingest");
    }
    assert_eq!(count(&engine, "c"), Ok(1), "the new table is read");
    // And its later changes are seen: the listing that proves it current
    // was taken after its directory appeared.
    for rows in 2..=4_u64 {
        let mut c = replica.writer("c");
        c.ingest(vec![row(rows, "more", rows, false)])
            .expect("ingest");
        drop(c);
        assert_eq!(count(&engine, "c"), Ok(rows));
    }
    // Nothing above made a query walk a table's files.
    let before = replica_cache_stats();
    for table in ["a", "b", "c"] {
        count(&engine, table).expect("count");
    }
    assert_eq!(delta(before), (0, 0, 0));
}

/// Between changes the only writer proves a replica current from three
/// numbers rather than from every table's generation. Whatever changes -
/// rows, the schema, the set of tables - the query right behind the change
/// sees it, however many queries ran just before on the standing proof.
#[test]
fn a_standing_proof_never_outlives_a_change() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    pintail_store::retain_writer_locks();
    let replica = Replica::seed();
    let engine = replica.engine();
    let mut expected = 0_u64;
    for round in 1..=40_u64 {
        // Queries on the standing proof, of both kinds: prepared afresh
        // and, from the third on, run from a kept plan.
        for _ in 0..4 {
            assert_eq!(count(&engine, "a"), Ok(expected), "round {round}");
        }
        {
            let mut a = replica.writer("a");
            a.ingest(vec![row(round, "row", round, false)])
                .expect("ingest");
            if round % 7 == 0 {
                a.flush().expect("flush");
            }
        }
        expected += 1;
        assert_eq!(
            count(&engine, "a"),
            Ok(expected),
            "the write of round {round}"
        );
        if round % 10 == 0 {
            // A metadata commit between two queries.
            replica.probe(vec![source_table("a")]);
            assert!(count(&engine, "b").is_err(), "round {round}: b dropped");
            replica.probe(vec![source_table("a"), source_table("b")]);
            assert_eq!(count(&engine, "b"), Ok(0), "round {round}: b back");
        }
    }
    // A change made while a query is being answered on another thread is
    // seen by every query that starts after it.
    let reader = {
        let engine = engine.clone();
        std::thread::spawn(move || {
            for _ in 0..2_000 {
                count(&engine, "a").expect("count");
            }
        })
    };
    for id in 100..160_u64 {
        {
            let mut a = replica.writer("a");
            a.ingest(vec![row(id, "row", id, false)]).expect("ingest");
        }
        expected += 1;
        assert_eq!(count(&engine, "a"), Ok(expected), "row {id}");
    }
    reader.join().expect("reader");
}
