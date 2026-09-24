//! Which databases a restart left mid-snapshot.
//!
//! Only a FORCED re-snapshot moves the database row itself to
//! 'snapshotting'; a first snapshot leaves it in 'created' or 'probed' the
//! whole time. Both strand the database if the process dies, because none
//! of those states is one the supervisor schedules - but the boot recovery
//! needs to tell them apart, since a forced copy must resume forced or the
//! tables it had not reached yet stay believing they are current.

use pintail_meta::MetaStore;

#[test]
fn only_a_forced_copy_marks_the_database_row_snapshotting() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).expect("metadata");
    let now = "2026-09-02T00:00:00Z";
    metadata
        .upsert_database("db-1", "shop", b"secret", now)
        .expect("database");

    assert!(
        metadata
            .databases_left_snapshotting()
            .expect("sweep")
            .is_empty(),
        "a database that never began a forced copy is not mid-snapshot"
    );

    metadata
        .begin_resnapshot("db-1", now)
        .expect("forced copy begins");
    assert_eq!(
        metadata.databases_left_snapshotting().expect("sweep"),
        vec!["db-1".to_owned()],
        "a forced copy in flight is exactly what a restart would strand"
    );

    metadata
        .set_database_replication_state("db-1", "cdc", now)
        .expect("copy completes");
    assert!(
        metadata
            .databases_left_snapshotting()
            .expect("sweep")
            .is_empty(),
        "a completed copy leaves nothing to resume"
    );
}
