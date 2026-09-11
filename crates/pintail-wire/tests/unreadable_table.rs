//! One table that cannot be opened must not take its database down. A
//! source that changes a column's type in place, with no transition
//! replication can apply, leaves the store holding rows written under the
//! old definition; a probe that records the new one used to make the
//! engine refuse every query of the database with a schema-fingerprint
//! mismatch. The table now keeps reading under the definition its rows were
//! written with, and a table that cannot be opened at all refuses only its
//! own reads.

mod common;

use common::{Replica, count, row, source_table, source_table_with};
use pintail_types::DataType;

#[test]
fn a_type_changed_in_place_keeps_its_table_readable_under_the_stored_definition() {
    let replica = Replica::seed();
    let mut a = replica.writer("a");
    let mut b = replica.writer("b");
    a.ingest(vec![row(1, "one", 1, false), row(2, "two", 1, false)])
        .expect("ingest a");
    // Flushed, so the store records the definition its rows were written
    // under.
    a.flush().expect("flush a");
    b.ingest(vec![row(1, "one", 1, false)]).expect("ingest b");
    let engine = replica.engine();
    assert_eq!(count(&engine, "a"), Ok(2));

    // The source changed `a.body` to an integer; nothing replicated the
    // change into the store.
    replica.probe(vec![
        source_table_with("a", DataType::Int64),
        source_table("b"),
    ]);
    assert_eq!(count(&engine, "b"), Ok(1), "the other table answers");
    assert_eq!(
        count(&engine, "a"),
        Ok(2),
        "the changed table reads its stored rows"
    );
    // Rows replicated after the probe still arrive in the stored shape.
    a.ingest(vec![row(3, "three", 2, false)]).expect("ingest a");
    assert_eq!(count(&engine, "a"), Ok(3));
}

#[test]
fn a_table_that_cannot_be_opened_refuses_only_its_own_reads() {
    let replica = Replica::seed();
    let mut a = replica.writer("a");
    a.ingest(vec![row(1, "one", 1, false)]).expect("ingest a");
    a.flush().expect("flush a");
    let mut b = replica.writer("b");
    b.ingest(vec![row(1, "one", 1, false)]).expect("ingest b");
    // First sight of this database, already disagreeing with `a`'s store:
    // there is no earlier definition to fall back on.
    replica.probe(vec![
        source_table_with("a", DataType::Int64),
        source_table("b"),
    ]);
    let engine = replica.engine();
    assert_eq!(count(&engine, "b"), Ok(1), "the rest of the database reads");
    let refused = count(&engine, "a").expect_err("the unopenable table refuses");
    assert!(
        refused.contains("table a cannot be read"),
        "the refusal names the table and why: {refused}"
    );

    // Once the source and the store agree again, the table reads.
    replica.probe(vec![source_table("a"), source_table("b")]);
    assert_eq!(count(&engine, "a"), Ok(1));
}
