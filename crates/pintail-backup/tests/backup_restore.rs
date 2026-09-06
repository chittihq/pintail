use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt as _, memory::InMemory, path::Path};
use pintail_backup::{
    BackupSource, SourceSegment, SourceTable, create_backup, load_manifest, restore_backup,
    validate_prefix,
};
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn full_and_incremental_backups_restore_with_verified_objects() {
    let local = tempdir().expect("tempdir");
    let first_segment = local.path().join("segment-1.pts");
    let second_segment = local.path().join("segment-2.pts");
    std::fs::write(&first_segment, b"first immutable segment").expect("first segment");
    std::fs::write(&second_segment, b"second immutable segment").expect("second segment");
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    let full_source = source(
        "backup-full",
        None,
        vec![SourceSegment {
            file_name: "segment-1.pts".into(),
            path: first_segment.clone(),
        }],
    );
    let (full, full_summary) = create_backup(store.clone(), "safe/prefix", full_source, None)
        .await
        .expect("full backup");
    assert_eq!(full_summary.uploaded_objects, 3);
    assert_eq!(full_summary.reused_segments, 0);

    let incremental_source = source(
        "backup-incremental",
        Some("backup-full"),
        vec![
            SourceSegment {
                file_name: "segment-1.pts".into(),
                path: first_segment,
            },
            SourceSegment {
                file_name: "segment-2.pts".into(),
                path: second_segment,
            },
        ],
    );
    let (incremental, summary) = create_backup(
        store.clone(),
        "safe/prefix",
        incremental_source,
        Some(&full),
    )
    .await
    .expect("incremental backup");
    assert_eq!(summary.uploaded_objects, 3);
    assert_eq!(summary.reused_segments, 1);
    assert_eq!(
        incremental.tables[0].segments[0].source_backup_id,
        "backup-full"
    );

    let loaded = load_manifest(
        store.as_ref(),
        "safe/prefix",
        "source-db",
        "backup-incremental",
    )
    .await
    .expect("load manifest");
    assert_eq!(loaded, incremental);

    let destination = local.path().join("restored-db");
    let restored = restore_backup(store.as_ref(), loaded, &destination)
        .await
        .expect("restore");
    assert_eq!(restored.restored_objects, 3);
    assert_eq!(
        std::fs::read(destination.join("tables/table-orders/segment-1.pts"))
            .expect("restored first"),
        b"first immutable segment"
    );
    assert_eq!(
        std::fs::read(destination.join("tables/table-orders/segment-2.pts"))
            .expect("restored second"),
        b"second immutable segment"
    );
}

#[tokio::test]
async fn restore_rejects_a_corrupt_object_and_leaves_no_database() {
    let local = tempdir().expect("tempdir");
    let segment = local.path().join("segment-1.pts");
    std::fs::write(&segment, b"original").expect("segment");
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let (manifest, _) = create_backup(
        store.clone(),
        "safe/prefix",
        source(
            "backup-full",
            None,
            vec![SourceSegment {
                file_name: "segment-1.pts".into(),
                path: segment,
            }],
        ),
        None,
    )
    .await
    .expect("backup");
    let segment_key = &manifest.tables[0].segments[0].key;
    store
        .put(&Path::parse(segment_key).expect("key"), "corrupt".into())
        .await
        .expect("corrupt object");

    let destination = local.path().join("restored-db");
    let error = restore_backup(store.as_ref(), manifest, &destination)
        .await
        .expect_err("checksum failure");
    assert!(error.to_string().contains("unexpected size"));
    assert!(!destination.exists());
}

#[test]
fn prefix_validation_is_an_accident_guard() {
    for invalid in ["", "/absolute", "trailing/", "safe/../broad", "./safe"] {
        assert!(validate_prefix(invalid).is_err(), "{invalid}");
    }
    validate_prefix("pintail/production").expect("safe prefix");
}

#[tokio::test]
async fn multipart_tail_reuse_and_same_size_corruption() {
    use pintail_backup::{
        TransferOptions, create_backup_with_options, restore_backup_with_options,
    };

    let local = tempdir().expect("tempdir");
    let options = TransferOptions { concurrency: 2 };
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // Cross several part boundaries and exercise a short final part.
    let data: Vec<_> = (0..(17 * 1024 * 1024 + 13))
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let path = local.path().join("large.pts");
    std::fs::write(&path, &data).unwrap();
    let segments = vec![SourceSegment {
        file_name: "large.pts".into(),
        path,
    }];
    let (base, _) = create_backup_with_options(
        store.clone(),
        "test",
        source("base", None, segments.clone()),
        None,
        options,
    )
    .await
    .unwrap();
    let (delta, summary) = create_backup_with_options(
        store.clone(),
        "test",
        source("delta", Some("base"), segments),
        Some(&base),
        options,
    )
    .await
    .unwrap();
    assert_eq!(summary.reused_segments, 1);
    let destination = local.path().join("good");
    restore_backup_with_options(store.as_ref(), delta.clone(), &destination, options)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(destination.join("tables/table-orders/large.pts")).unwrap(),
        data
    );

    let mut corrupted = data;
    corrupted[9 * 1024 * 1024] ^= 1;
    store
        .put(
            &Path::from(delta.tables[0].segments[0].key.as_str()),
            corrupted.into(),
        )
        .await
        .unwrap();
    let destination = local.path().join("corrupt");
    let error = restore_backup_with_options(store.as_ref(), delta, &destination, options)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256"));
    assert!(!destination.exists());
    assert!(!local.path().join(".restore-corrupt-delta.tmp").exists());
}

#[tokio::test]
async fn failed_parallel_backup_does_not_publish_and_restore_rejects_duplicate_targets() {
    let local = tempdir().expect("tempdir");
    let valid = local.path().join("valid.pts");
    std::fs::write(&valid, b"payload").unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let segment = SourceSegment {
        file_name: "valid.pts".into(),
        path: valid,
    };
    let invalid = SourceSegment {
        file_name: "missing.pts".into(),
        path: local.path().join("missing.pts"),
    };
    assert!(
        create_backup(
            store.clone(),
            "test",
            source("failed", None, vec![segment.clone(), invalid]),
            None
        )
        .await
        .is_err()
    );
    assert!(
        load_manifest(store.as_ref(), "test", "source-db", "failed")
            .await
            .is_err()
    );
    let (mut manifest, _) = create_backup(
        store.clone(),
        "test",
        source("good", None, vec![segment]),
        None,
    )
    .await
    .unwrap();
    let duplicate = manifest.tables[0].segments[0].clone();
    manifest.tables[0].segments.push(duplicate);
    let destination = local.path().join("duplicate");
    let error = restore_backup(store.as_ref(), manifest, &destination)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("duplicate restore path"));
    assert!(!destination.exists());
}

fn source(backup_id: &str, parent_id: Option<&str>, segments: Vec<SourceSegment>) -> BackupSource {
    BackupSource {
        database_id: "source-db".into(),
        backup_id: backup_id.into(),
        parent_id: parent_id.map(str::to_owned),
        control_plane: json!({
            "database": {"name": "Source"},
            "tables": [{"name": "orders"}],
            "checkpoint": {"mode": "cdc", "position": 42}
        }),
        tables: vec![SourceTable {
            name: "orders".into(),
            directory_name: "table-orders".into(),
            manifest: b"manifest".to_vec(),
            segments,
        }],
    }
}
