#[test]
fn secrets_are_generated_once_and_reused_without_being_shown_again() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("make data directory initially permissive");
    }

    let first_boot = pintail::secrets::load_or_create(data_dir.path()).expect("first boot");
    assert!(first_boot.is_first_boot());
    assert_eq!(first_boot.secrets().dsn_encryption_key().len(), 64);
    let persisted =
        std::fs::read_to_string(data_dir.path().join("secrets.toml")).expect("secrets file");
    assert!(!persisted.contains("jwt"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(
            data_dir.path().join("secrets.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("make existing secrets file initially permissive");
        std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("make existing data directory initially permissive");
    }

    let restart = pintail::secrets::load_or_create(data_dir.path()).expect("restart");
    assert!(!restart.is_first_boot());
    assert_eq!(restart.secrets(), first_boot.secrets());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(data_dir.path())
            .expect("data directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        let secrets_mode = std::fs::metadata(data_dir.path().join("secrets.toml"))
            .expect("secrets file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(secrets_mode, 0o600);
    }
}

#[test]
fn concurrent_boots_share_one_durably_published_secret() {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let barrier = Arc::new(Barrier::new(3));
    let mut boots = Vec::new();

    for _ in 0..2 {
        let data_dir = data_dir.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        boots.push(thread::spawn(move || {
            barrier.wait();
            pintail::secrets::load_or_create(&data_dir).expect("concurrent boot")
        }));
    }

    barrier.wait();
    let first = boots.remove(0).join().expect("first boot thread");
    let second = boots.remove(0).join().expect("second boot thread");

    assert_ne!(first.is_first_boot(), second.is_first_boot());
    assert_eq!(first.secrets(), second.secrets());
    assert!(!data_dir.path().join(".secrets.toml.tmp").exists());
}
