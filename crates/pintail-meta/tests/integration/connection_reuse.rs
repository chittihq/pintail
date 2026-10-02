//! A store dropped leaves its connection for the next one opened on the
//! same file. What a store reads must not depend on which connection it
//! was given: writes made through one are seen through the next, a file
//! put in the place of another is read as itself, and stores open together
//! each have a connection of their own. (A file set back to an older schema
//! behind a kept connection is the upgrade test in `copy_complete`.)

use pintail_meta::MetaStore;

#[test]
fn a_reopened_store_reads_what_the_last_one_wrote() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("meta.db");
    for round in 0..12 {
        let store = MetaStore::open(&path).expect("open");
        if round > 0 {
            assert_eq!(
                store.setting("round").expect("read"),
                Some((round - 1).to_string())
            );
        }
        store
            .set_setting("round", &round.to_string())
            .expect("write");
    }
}

#[test]
fn stores_open_together_each_have_a_connection() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("meta.db");
    let stores = (0..9)
        .map(|_| MetaStore::open(&path).expect("open"))
        .collect::<Vec<_>>();
    for (index, store) in stores.iter().enumerate() {
        store
            .set_setting(&format!("key-{index}"), "set")
            .expect("write");
    }
    drop(stores);
    let store = MetaStore::open(&path).expect("open");
    for index in 0..9 {
        assert_eq!(
            store.setting(&format!("key-{index}")).expect("read"),
            Some("set".to_owned())
        );
    }
}

#[test]
fn a_file_put_in_the_place_of_another_is_opened_afresh() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("meta.db");
    let other = directory.path().join("other.db");
    MetaStore::open(&path)
        .expect("open")
        .set_setting("which", "first")
        .expect("write");
    MetaStore::open(&other)
        .expect("open")
        .set_setting("which", "second")
        .expect("write");
    // The second file takes the first one's path while a connection to the
    // first is still kept for reuse.
    let moved = directory.path().join("moved.db");
    let store = MetaStore::open(&other).expect("open");
    store.backup_into(&moved).expect("copy");
    drop(store);
    std::fs::rename(&moved, &path).expect("replace");
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    assert_eq!(
        MetaStore::open(&path)
            .expect("open")
            .setting("which")
            .expect("read"),
        Some("second".to_owned())
    );
}
