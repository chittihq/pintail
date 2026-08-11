use pintail_meta::{
    DatabaseUpdate, GoogleAdmission, MetaStore, NewApiKey, NewBackup, NewBackupConfig, NewInvite,
    RestoredCheckpoint, RestoredDatabase, RestoredTable,
};

#[test]
fn users_databases_and_table_controls_round_trip() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = MetaStore::open(&database_path).expect("metadata store");

    assert_eq!(metadata.user_count().unwrap(), 0);
    metadata
        .create_user(
            "user-1",
            "operator@example.com",
            "$argon2id$test",
            "admin",
            "2026-07-30T00:00:00Z",
        )
        .unwrap();
    assert_eq!(metadata.user_count().unwrap(), 1);
    let user = metadata
        .user_by_email("OPERATOR@example.com")
        .unwrap()
        .expect("case-insensitive user");
    assert_eq!(user.role, "admin");
    metadata
        .touch_user_login("user-1", "2026-07-30T00:01:00Z")
        .unwrap();
    assert_eq!(
        metadata.users().unwrap()[0].last_login_at.as_deref(),
        Some("2026-07-30T00:01:00Z")
    );

    metadata
        .upsert_database("db-1", "app", b"encrypted-v1", "2026-07-30T00:00:00Z")
        .unwrap();
    metadata
        .update_database(
            "db-1",
            &DatabaseUpdate {
                name: "analytics",
                encrypted_dsn: Some(b"encrypted-v2"),
                mode: "polling",
                include_tables: Some("[\"events\"]"),
                exclude_tables: Some("[\"secrets\"]"),
                poll_interval_seconds: 7,
                reconcile_interval_seconds: 420,
                keyless_policy: "auto_resync",
                now: "2026-07-30T00:02:00Z",
            },
        )
        .unwrap();
    metadata
        .update_database_probe(
            "db-1",
            "{\"database\":\"analytics\"}",
            "polling",
            "2026-07-30T00:03:00Z",
        )
        .unwrap();
    let database = metadata.database("db-1").unwrap().expect("database");
    assert_eq!(database.name, "analytics");
    assert_eq!(database.encrypted_dsn, b"encrypted-v2");
    assert_eq!(database.poll_interval_seconds, 7);
    assert_eq!(database.keyless_policy, "auto_resync");
    assert_eq!(database.effective_mode.as_deref(), Some("polling"));
    assert_eq!(metadata.databases().unwrap().len(), 1);

    metadata
        .upsert_snapshot_table("db-1", "events", Some("[\"id\"]"), Some("[\"id\"]"))
        .unwrap();
    metadata
        .set_table_soft_delete_column("db-1", "events", Some("deleted_at"))
        .unwrap();
    assert_eq!(
        metadata.tables("db-1").unwrap()[0]
            .soft_delete_column
            .as_deref(),
        Some("deleted_at")
    );

    metadata
        .set_database_mode("db-1", "paused", "2026-07-30T00:04:00Z")
        .unwrap();
    assert_eq!(
        metadata
            .database("db-1")
            .unwrap()
            .unwrap()
            .effective_mode
            .as_deref(),
        Some("paused")
    );
    metadata
        .set_database_mode("db-1", "auto", "2026-07-30T00:05:00Z")
        .unwrap();
    assert_eq!(
        metadata.database("db-1").unwrap().unwrap().effective_mode,
        None
    );
    metadata
        .set_database_replication_state("db-1", "polling", "2026-07-30T00:06:00Z")
        .unwrap();
    assert_eq!(metadata.database("db-1").unwrap().unwrap().state, "polling");
    assert_eq!(metadata.tables("db-1").unwrap()[0].state, "polling");

    assert!(metadata.delete_database("db-1").unwrap());
    assert!(metadata.databases().unwrap().is_empty());
}

#[test]
fn api_keys_activity_and_dead_letters_round_trip() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let database_path = data_dir.path().join("pintail-meta.db");
    let metadata = MetaStore::open(&database_path).expect("metadata store");
    metadata
        .upsert_database("db-1", "app", b"encrypted", "2026-07-30T00:00:00Z")
        .unwrap();

    metadata
        .create_api_key(&NewApiKey {
            id: "key-1",
            database_id: "db-1",
            name: "Metabase",
            sha256: &[7; 32],
            mysql_native_password_hash: Some(&[8; 20]),
            caching_sha2_password_hash: Some(&[9; 32]),
            scopes_json: "[\"query\",\"read\"]",
            expires_at: None,
            now: "2026-07-30T00:06:00Z",
        })
        .unwrap();
    let key = metadata
        .api_key_by_sha256(&[7; 32])
        .unwrap()
        .expect("API key");
    assert!(key.enabled);
    assert_eq!(
        key.mysql_native_password_hash.as_deref(),
        Some(&[8; 20][..])
    );
    assert_eq!(
        key.caching_sha2_password_hash.as_deref(),
        Some(&[9; 32][..])
    );
    assert_eq!(metadata.api_keys("db-1").unwrap().len(), 1);
    metadata
        .touch_api_key("key-1", "2026-07-30T00:07:00Z")
        .unwrap();
    metadata.set_api_key_enabled("key-1", false).unwrap();
    assert!(!metadata.api_keys("db-1").unwrap()[0].enabled);

    metadata
        .start_sync_run(
            "run-1",
            "db-1",
            Some("events"),
            "snapshot",
            "2026-07-30T00:08:00Z",
        )
        .unwrap();
    metadata
        .finish_sync_run("run-1", "completed", 10, 2048, 50, None)
        .unwrap();
    let run = &metadata.sync_runs(Some("db-1"), 10).unwrap()[0];
    assert_eq!((run.rows, run.bytes, run.duration_ms), (10, 2048, Some(50)));

    metadata
        .record_dlq(
            "dlq-1",
            "db-1",
            Some("events"),
            "{\"event\":1}",
            "decode failed",
            "2026-07-30T00:09:00Z",
        )
        .unwrap();
    let dlq = metadata.dlq_records(Some("db-1"), 10).unwrap();
    assert_eq!(dlq.len(), 1);
    assert!(metadata.delete_dlq_record(&dlq[0].id).unwrap());
    assert!(metadata.dlq_records(Some("db-1"), 10).unwrap().is_empty());
    assert!(metadata.delete_api_key("key-1").unwrap());
}

#[test]
fn backup_configuration_and_runs_round_trip() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    metadata
        .upsert_database("db-1", "app", b"encrypted", "2026-07-30T00:00:00Z")
        .unwrap();

    metadata
        .upsert_backup_config(&NewBackupConfig {
            database_id: "db-1",
            bucket: "pintail",
            prefix: "team/analytics",
            endpoint: Some("http://127.0.0.1:9000"),
            region: "us-east-1",
            encrypted_access_key_id: Some(b"encrypted-access"),
            encrypted_secret_access_key: Some(b"encrypted-secret"),
            retain_count: 0,
            verify_restore: true,
            full_every: 4,
            schedule_minutes: 60,
            enabled: true,
            now: "2026-07-30T00:01:00Z",
        })
        .unwrap();
    let config = metadata
        .backup_config("db-1")
        .unwrap()
        .expect("backup config");
    assert_eq!(config.prefix, "team/analytics");
    assert_eq!(config.schedule_minutes, 60);
    assert!(config.enabled);
    assert!(config.verify_restore);
    assert_eq!(config.full_every, 4);

    metadata
        .start_backup(&NewBackup {
            id: "backup-1",
            database_id: "db-1",
            kind: "full",
            parent_id: None,
            object_prefix: "team/analytics/db-1/backup-1",
            started_at: "2026-07-30T00:02:00Z",
        })
        .unwrap();
    metadata
        .finish_backup(
            "backup-1",
            "completed",
            4096,
            3,
            None,
            "2026-07-30T00:03:00Z",
        )
        .unwrap();
    metadata
        .start_backup(&NewBackup {
            id: "backup-2",
            database_id: "db-1",
            kind: "incremental",
            parent_id: Some("backup-1"),
            object_prefix: "team/analytics/db-1/backup-2",
            started_at: "2026-07-30T00:04:00Z",
        })
        .unwrap();
    metadata
        .finish_backup(
            "backup-2",
            "error",
            0,
            0,
            Some("destination unavailable"),
            "2026-07-30T00:05:00Z",
        )
        .unwrap();

    let backups = metadata.backups("db-1", 10).unwrap();
    assert_eq!(backups.len(), 2);
    assert_eq!(backups[0].status, "error");
    let latest = metadata
        .latest_completed_backup("db-1")
        .unwrap()
        .expect("latest completed");
    assert_eq!(latest.id, "backup-1");
}

#[test]
fn restored_database_is_registered_side_by_side_without_source_credentials() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    metadata
        .upsert_database("source", "app", b"encrypted", "2026-07-30T00:00:00Z")
        .unwrap();
    let tables = [RestoredTable {
        name: "events",
        primary_key_json: Some("[\"id\"]"),
        cursor_column: Some("updated_at"),
        sort_key_json: Some("[\"id\"]"),
        rows_synced: 42,
        schema_version: 3,
        soft_delete_column: Some("deleted_at"),
    }];
    metadata
        .register_restored_database(&RestoredDatabase {
            id: "restored",
            name: "app recovery",
            probe_json: "{\"database\":\"app\"}",
            effective_mode: "cdc",
            tables: &tables,
            checkpoint: Some(RestoredCheckpoint {
                kind: "gtid",
                gtid_set: Some("server:1-9"),
                binlog_file: None,
                binlog_pos: None,
            }),
            now: "2026-07-30T01:00:00Z",
        })
        .unwrap();

    let source = metadata.database("source").unwrap().expect("source");
    let restored = metadata.database("restored").unwrap().expect("restore");
    assert_eq!(source.encrypted_dsn, b"encrypted");
    assert!(restored.encrypted_dsn.is_empty());
    assert_eq!(restored.mode, "paused");
    assert_eq!(restored.state, "restored");
    assert_eq!(restored.effective_mode.as_deref(), Some("cdc"));
    let table = &metadata.tables("restored").unwrap()[0];
    assert_eq!(table.state, "restored");
    assert_eq!(table.rows_synced, 42);
    assert_eq!(table.schema_version, 3);
    assert_eq!(
        metadata
            .snapshot_checkpoint("restored")
            .unwrap()
            .expect("checkpoint")
            .gtid_set
            .as_deref(),
        Some("server:1-9")
    );
}

/// A Google admission is all-or-nothing.
///
/// The three writes it replaces used to run separately, and a failure between
/// creating the user and granting the membership left an account that could
/// never sign in again: present enough that every later attempt skipped the
/// invite path, without the workspace that path exists to grant. Production
/// produced exactly that state, and it was unrecoverable through the UI.
#[test]
fn google_admission_grants_user_membership_and_invite_together() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    let now = "2026-08-10T00:00:00Z";

    metadata
        .create_workspace("ws-1", "Workspace", "workspace", now)
        .expect("workspace");
    // invites.created_by is a foreign key onto users.
    metadata
        .create_user(
            "user-0",
            "admin@example.com",
            "$argon2id$test",
            "admin",
            now,
        )
        .expect("inviting user");
    metadata
        .create_invite(&NewInvite {
            id: "inv-1",
            token_hash: b"hash",
            workspace_id: "ws-1",
            email: "invited@example.com",
            role: "operator",
            created_by: "user-0",
            created_at: now,
            expires_at: "2026-09-10T00:00:00Z",
        })
        .expect("invite");

    metadata
        .admit_invited_google_user(&GoogleAdmission {
            user_id: "usr-1",
            email: "invited@example.com",
            google_subject: "google-subject-1",
            workspace_id: "ws-1",
            invite_id: "inv-1",
            role: "operator",
            now,
        })
        .expect("admission");

    // The membership is the part whose absence made the account unusable.
    let memberships = metadata.workspaces_for_user("usr-1").expect("memberships");
    assert_eq!(
        memberships.len(),
        1,
        "admitted user must belong to a workspace"
    );
    assert_eq!(memberships[0].1, "operator");

    let invite = metadata
        .invites_by_email("invited@example.com")
        .expect("invites")
        .into_iter()
        .find(|invite| invite.id == "inv-1")
        .expect("invite still present");
    assert!(invite.accepted_at.is_some(), "invite must be consumed");
}

/// A rolled-back admission leaves no user behind.
///
/// Scope, stated precisely because an earlier version of this comment claimed
/// more than the test shows: it creates no invite, so it proves only that the
/// user insert is undone - not that an invite survives. Invite survival is
/// covered by the consumed- and revoked-invite tests below.
#[test]
fn a_failed_google_admission_leaves_no_user_behind() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    let now = "2026-08-10T00:00:00Z";

    metadata
        .create_workspace("ws-1", "Workspace", "workspace", now)
        .expect("workspace");

    // A workspace that does not exist fails the membership insert, which is
    // the second of the three writes - precisely where production broke.
    let failed = metadata.admit_invited_google_user(&GoogleAdmission {
        user_id: "usr-2",
        email: "invited@example.com",
        google_subject: "google-subject-2",
        workspace_id: "ws-missing",
        invite_id: "inv-missing",
        role: "operator",
        now,
    });
    assert!(
        failed.is_err(),
        "admission into a missing workspace must fail"
    );

    // The user must not survive the failure: an orphaned row is what made the
    // original bug permanent.
    assert!(
        metadata
            .workspaces_for_user("usr-2")
            .expect("lookup")
            .is_empty(),
        "a rolled-back admission must leave no membership"
    );
    assert!(
        metadata
            .user_by_email("invited@example.com")
            .expect("lookup")
            .is_none(),
        "a rolled-back admission must leave no user"
    );
}

/// Two admissions differing only by identity, against the same invite.
fn same_invite_admission<'a>(
    user_id: &'a str,
    subject: &'a str,
    now: &'a str,
) -> GoogleAdmission<'a> {
    GoogleAdmission {
        user_id,
        email: "invited@example.com",
        google_subject: subject,
        workspace_id: "ws-1",
        invite_id: "inv-1",
        role: "operator",
        now,
    }
}

/// An already-consumed invite admits nobody.
///
/// The first version of this guard checked `accepted_at IS NULL` but ignored
/// the affected-row count, so a second admission updated zero rows while the
/// user and membership committed anyway - letting an account in against an
/// invite that no longer authorized it. Codex review caught it; this pins it.
#[test]
fn a_consumed_invite_cannot_admit_a_second_user() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    let now = "2026-08-10T00:00:00Z";

    metadata
        .create_workspace("ws-1", "Workspace", "workspace", now)
        .expect("workspace");
    metadata
        .create_user(
            "user-0",
            "admin@example.com",
            "$argon2id$test",
            "admin",
            now,
        )
        .expect("inviting user");
    metadata
        .create_invite(&NewInvite {
            id: "inv-1",
            token_hash: b"hash",
            workspace_id: "ws-1",
            email: "invited@example.com",
            role: "operator",
            created_by: "user-0",
            created_at: now,
            expires_at: "2026-09-10T00:00:00Z",
        })
        .expect("invite");

    metadata
        .admit_invited_google_user(&same_invite_admission("usr-1", "subject-1", now))
        .expect("first admission");

    // Same invite, a different identity. It must be refused and leave nothing
    // behind, or one invite would admit an unbounded number of accounts.
    let second =
        metadata.admit_invited_google_user(&same_invite_admission("usr-2", "subject-2", now));
    assert!(
        second.is_err(),
        "a consumed invite must not admit a second user"
    );
    assert!(
        metadata
            .workspaces_for_user("usr-2")
            .expect("lookup")
            .is_empty(),
        "the refused admission must leave no membership"
    );
}

/// A revoked invite admits nobody, even if it was valid when read.
///
/// The caller checks revocation before opening the transaction, so only a
/// re-check inside it closes the window where an admin revokes an invite
/// while a sign-in is in flight.
#[test]
fn a_revoked_invite_cannot_admit_a_user() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    let now = "2026-08-10T00:00:00Z";

    metadata
        .create_workspace("ws-1", "Workspace", "workspace", now)
        .expect("workspace");
    metadata
        .create_user(
            "user-0",
            "admin@example.com",
            "$argon2id$test",
            "admin",
            now,
        )
        .expect("inviting user");
    metadata
        .create_invite(&NewInvite {
            id: "inv-1",
            token_hash: b"hash",
            workspace_id: "ws-1",
            email: "invited@example.com",
            role: "operator",
            created_by: "user-0",
            created_at: now,
            expires_at: "2026-09-10T00:00:00Z",
        })
        .expect("invite");
    metadata.revoke_invite("inv-1", now).expect("revoke");

    let refused = metadata.admit_invited_google_user(&GoogleAdmission {
        user_id: "usr-1",
        email: "invited@example.com",
        google_subject: "subject-1",
        workspace_id: "ws-1",
        invite_id: "inv-1",
        role: "operator",
        now,
    });
    assert!(refused.is_err(), "a revoked invite must not admit a user");
    assert!(
        metadata
            .user_by_email("invited@example.com")
            .expect("lookup")
            .is_none(),
        "the refused admission must leave no user"
    );
}

/// An invite stored with different capitalization is still found, and still
/// claimable, by the lowercased address Google reports.
///
/// The lookup and the compare-and-set have to agree on collation. When only
/// the lookup was case-insensitive, an invite could be found and then fail to
/// claim, which aborts the admission entirely - and when neither was, an
/// invite that is plainly visible in the UI refuses its holder as "not
/// invited". Both are indistinguishable from never having been invited.
#[test]
fn an_invite_is_matched_and_claimed_regardless_of_address_case() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let metadata =
        MetaStore::open(&data_dir.path().join("pintail-meta.db")).expect("metadata store");
    let now = "2026-08-10T00:00:00Z";

    metadata
        .create_workspace("ws-1", "Workspace", "workspace", now)
        .expect("workspace");
    metadata
        .create_user(
            "user-0",
            "admin@example.com",
            "$argon2id$test",
            "admin",
            now,
        )
        .expect("inviting user");
    metadata
        .create_invite(&NewInvite {
            id: "inv-1",
            token_hash: b"hash",
            workspace_id: "ws-1",
            email: "Jayashri_S@Example.COM",
            role: "viewer",
            created_by: "user-0",
            created_at: now,
            expires_at: "2026-09-10T00:00:00Z",
        })
        .expect("invite");

    let found = metadata
        .invites_by_email("jayashri_s@example.com")
        .expect("invites");
    assert_eq!(found.len(), 1, "a differently-cased invite must be found");

    metadata
        .admit_invited_google_user(&GoogleAdmission {
            user_id: "usr-1",
            email: "jayashri_s@example.com",
            google_subject: "google-subject-1",
            workspace_id: "ws-1",
            invite_id: "inv-1",
            role: "viewer",
            now,
        })
        .expect("admission must survive a case difference");

    assert_eq!(
        metadata
            .workspaces_for_user("usr-1")
            .expect("memberships")
            .len(),
        1,
    );
    assert!(
        metadata
            .invites_by_email("jayashri_s@example.com")
            .expect("invites")
            .into_iter()
            .find(|invite| invite.id == "inv-1")
            .expect("invite present")
            .accepted_at
            .is_some(),
        "the invite must be consumed, not merely matched",
    );
}
