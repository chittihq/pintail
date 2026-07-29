#[test]
fn setting_is_inserted_once_and_reused() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = pintail_meta::MetaStore::open(&database_path).expect("metadata store");

    let created = metadata
        .get_or_insert_setting("jwt_secret", "first-value")
        .expect("insert setting");
    assert!(created.was_inserted());
    assert_eq!(created.value(), "first-value");

    let reused = metadata
        .get_or_insert_setting("jwt_secret", "replacement-value")
        .expect("reuse setting");
    assert!(!reused.was_inserted());
    assert_eq!(reused.value(), "first-value");
}
