//! Metadata rollback at the durability boundaries named in the recovery plan.
#![cfg(feature = "failpoints")]
use pintail_meta::{MetaStore, PollChunkStateUpdate, PollStateUpdate, SnapshotCheckpointRecord};

#[test]
fn failed_metadata_commit_preserves_the_previous_generation_and_can_retry() {
    for case in ["cdc", "poll"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "metadata_fault_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("PINTAIL_FAILPOINT", "meta.before_commit@2=error")
            .env("PINTAIL_META_FAULT_CASE", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("failpoint meta.before_commit hit 2: error")
        );
    }
}

#[test]
#[ignore = "child process with isolated failpoint configuration"]
fn metadata_fault_worker() {
    let Some(case) = std::env::var("PINTAIL_META_FAULT_CASE").ok() else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let mut store = MetaStore::open(&directory.path().join("meta.db")).unwrap();
    store
        .upsert_database("source", "app", b"unused", "2026-09-05T00:00:00Z")
        .unwrap();
    store
        .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .unwrap();
    let now = "2026-09-05T00:00:00Z";
    if case == "cdc" {
        let first = SnapshotCheckpointRecord {
            kind: "filepos".into(),
            gtid_set: None,
            binlog_file: Some("mysql-bin.000001".into()),
            binlog_pos: Some(100),
        };
        let next = SnapshotCheckpointRecord {
            binlog_pos: Some(200),
            ..first.clone()
        };
        store
            .commit_cdc_checkpoint("source", &first, &["events".into()], now)
            .unwrap();
        assert!(
            store
                .commit_cdc_checkpoint("source", &next, &["events".into()], now)
                .is_err()
        );
        assert_eq!(store.snapshot_checkpoint("source").unwrap().unwrap(), first);
        store
            .commit_cdc_checkpoint("source", &next, &["events".into()], now)
            .unwrap();
        assert_eq!(store.snapshot_checkpoint("source").unwrap().unwrap(), next);
    } else {
        let first = PollStateUpdate {
            cursor_column: None,
            cursor_json: None,
            source_token_json: Some("{}"),
            source_count: 1,
            version: 1,
            reconciled: true,
        };
        let next = PollStateUpdate {
            version: 2,
            ..first
        };
        let a = [PollChunkStateUpdate {
            chunk_id: "a",
            source_count: 1,
            source_checksum: "old",
            replica_checksum: "old",
        }];
        let b = [PollChunkStateUpdate {
            chunk_id: "b",
            source_count: 1,
            source_checksum: "new",
            replica_checksum: "new",
        }];
        store
            .commit_poll_state_with_chunks("source", "events", &first, &a, now)
            .unwrap();
        assert!(
            store
                .commit_poll_state_with_chunks("source", "events", &next, &b, now)
                .is_err()
        );
        assert_eq!(
            store
                .poll_state("source", "events")
                .unwrap()
                .unwrap()
                .version,
            1
        );
        let chunks = store.poll_chunk_states("source", "events").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_id, "a");
        assert_eq!(chunks[0].source_checksum, "old");
        store
            .commit_poll_state_with_chunks("source", "events", &next, &b, now)
            .unwrap();
        assert_eq!(
            store
                .poll_state("source", "events")
                .unwrap()
                .unwrap()
                .version,
            2
        );
        let chunks = store.poll_chunk_states("source", "events").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_id, "b");
    }
}
