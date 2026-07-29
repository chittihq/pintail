#[test]
fn secrets_are_generated_once_and_reused_without_being_shown_again() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");

    let first_boot = pintail::secrets::load_or_create(data_dir.path()).expect("first boot");
    assert!(first_boot.is_first_boot());
    assert_eq!(first_boot.secrets().dsn_encryption_key().len(), 64);
    let persisted =
        std::fs::read_to_string(data_dir.path().join("secrets.toml")).expect("secrets file");
    assert!(!persisted.contains("jwt"));

    let restart = pintail::secrets::load_or_create(data_dir.path()).expect("restart");
    assert!(!restart.is_first_boot());
    assert_eq!(restart.secrets(), first_boot.secrets());
}
