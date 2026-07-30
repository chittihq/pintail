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
