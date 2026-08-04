use std::{collections::BTreeMap, time::Instant};

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use pintail_backup::{
    BackupSource, S3Destination, SourceSegment, SourceTable, build_s3, create_backup,
    load_manifest, restore_backup, validate_prefix,
};
use pintail_meta::{
    BackupConfigRecord, BackupRecord, NewBackup, NewBackupConfig, RestoredCheckpoint,
    RestoredDatabase, RestoredTable, TableRecord,
};
use pintail_probe::ProbeReport;
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use serde::{Deserialize, Serialize};

use crate::{
    ApiState, auth::AuthPrincipal, error::ApiError, events::ApiEvent, snapshot::table_directory,
};

type EncryptedCredentials = (Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)] // serialization DTO mirrors the API contract
pub(crate) struct BackupConfigResponse {
    configured: bool,
    bucket: String,
    prefix: String,
    endpoint: Option<String>,
    region: String,
    schedule_minutes: u64,
    enabled: bool,
    retain_count: u64,
    verify_restore: bool,
    full_every: u64,
    credentials_configured: bool,
    updated_at: String,
}

#[derive(Deserialize)]
pub(crate) struct BackupConfigRequest {
    bucket: String,
    prefix: String,
    endpoint: Option<String>,
    #[serde(default = "default_region")]
    region: String,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    #[serde(default)]
    clear_credentials: bool,
    #[serde(default = "default_schedule")]
    schedule_minutes: u64,
    #[serde(default = "enabled")]
    enabled: bool,
    /// Restore each completed backup into a scratch directory and record
    /// the checksum-verified outcome on the backup row.
    #[serde(default)]
    verify_restore: bool,
    /// Force a full backup every Nth scheduled run; zero chains
    /// incrementals after the first full indefinitely.
    #[serde(default)]
    full_every: u64,
    /// Completed backups to keep; zero keeps everything.
    #[serde(default)]
    retain_count: u64,
}

#[derive(Deserialize)]
pub(crate) struct StartBackupRequest {
    #[serde(default)]
    full: bool,
}

#[derive(Serialize)]
pub(crate) struct AcceptedBackup {
    id: String,
    kind: String,
    state: &'static str,
}

#[derive(Serialize)]
pub(crate) struct BackupResponse {
    id: String,
    database_id: String,
    kind: String,
    parent_id: Option<String>,
    object_prefix: String,
    status: String,
    bytes: u64,
    object_count: u64,
    error: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    verified_at: Option<String>,
    verify_error: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct RestoreRequest {
    backup_id: String,
    name: String,
    /// Roll the restored replica forward from the backup's checkpoint to
    /// the last source transaction at or before this RFC 3339 instant.
    /// Requires `dsn` so the catch-up can read the source binlog.
    #[serde(default)]
    point_in_time: Option<String>,
    /// Source DSN for point-in-time catch-up.
    #[serde(default)]
    dsn: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct RestoreResponse {
    database_id: String,
    backup_id: String,
    state: &'static str,
    restored_bytes: u64,
    restored_objects: u64,
    /// Whether a bounded point-in-time catch-up job was started.
    catching_up: bool,
}

#[derive(Deserialize, Serialize)]
struct ControlPlane {
    database: ControlDatabase,
    tables: Vec<ControlTable>,
    checkpoint: Option<ControlCheckpoint>,
}

#[derive(Deserialize, Serialize)]
struct ControlDatabase {
    name: String,
    effective_mode: String,
    probe_json: String,
}

#[derive(Deserialize, Serialize)]
struct ControlTable {
    name: String,
    primary_key_json: Option<String>,
    cursor_column: Option<String>,
    sort_key_json: Option<String>,
    rows_synced: u64,
    schema_version: u32,
    soft_delete_column: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ControlCheckpoint {
    kind: String,
    gtid_set: Option<String>,
    binlog_file: Option<String>,
    binlog_pos: Option<u64>,
}

pub(crate) async fn get_config(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<Json<BackupConfigResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&database_id)?;
    ensure_database(&state, &database_id)?;
    let config = state
        .metadata()?
        .backup_config(&database_id)
        .map_err(ApiError::internal)?;
    Ok(Json(
        config.map_or_else(BackupConfigResponse::default, Into::into),
    ))
}

pub(crate) async fn put_config(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
    Json(request): Json<BackupConfigRequest>,
) -> Result<Json<BackupConfigResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    ensure_database(&state, &database_id)?;
    validate_prefix(&request.prefix).map_err(bad_request)?;
    if request.schedule_minutes == 0 {
        return Err(ApiError::bad_request(
            "backup schedule must be at least one minute",
        ));
    }
    if request.access_key_id.is_some() != request.secret_access_key.is_some() {
        return Err(ApiError::bad_request(
            "backup access key ID and secret must be provided together",
        ));
    }
    let metadata = state.metadata()?;
    let existing = metadata
        .backup_config(&database_id)
        .map_err(ApiError::internal)?;
    let (access_key, secret) = encrypted_credentials(&state, &request, existing.as_ref())?;
    let now = Utc::now().to_rfc3339();
    metadata
        .upsert_backup_config(&NewBackupConfig {
            database_id: &database_id,
            bucket: request.bucket.trim(),
            prefix: &request.prefix,
            endpoint: request.endpoint.as_deref(),
            region: request.region.trim(),
            encrypted_access_key_id: access_key.as_deref(),
            encrypted_secret_access_key: secret.as_deref(),
            schedule_minutes: request.schedule_minutes,
            enabled: request.enabled,
            retain_count: request.retain_count,
            verify_restore: request.verify_restore,
            full_every: request.full_every,
            now: &now,
        })
        .map_err(bad_request)?;
    let saved = metadata
        .backup_config(&database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("saved backup configuration disappeared"))?;
    Ok(Json(saved.into()))
}

pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<Json<Vec<BackupResponse>>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&database_id)?;
    ensure_database(&state, &database_id)?;
    let backups = state
        .metadata()?
        .backups(&database_id, 100)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(BackupResponse::from)
        .collect();
    Ok(Json(backups))
}

pub(crate) async fn start(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
    payload: Option<Json<StartBackupRequest>>,
) -> Result<(StatusCode, Json<AcceptedBackup>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    let force_full = payload.is_some_and(|Json(request)| request.full);
    let accepted = start_job(&state, &database_id, force_full)?;
    Ok((StatusCode::ACCEPTED, Json(accepted)))
}

pub(crate) fn start_scheduled_if_due(
    state: &ApiState,
    database_id: &str,
) -> Result<bool, ApiError> {
    let metadata = state.metadata()?;
    let Some(config) = metadata
        .backup_config(database_id)
        .map_err(ApiError::internal)?
        .filter(|config| config.enabled)
    else {
        return Ok(false);
    };
    let last_started = metadata
        .backups(database_id, 1)
        .map_err(ApiError::internal)?
        .into_iter()
        .next()
        .and_then(|backup| chrono::DateTime::parse_from_rfc3339(&backup.started_at).ok())
        .map(|started| started.with_timezone(&Utc));
    let due = last_started.is_none_or(|started| {
        Utc::now().signed_duration_since(started).num_minutes()
            >= i64::try_from(config.schedule_minutes).unwrap_or(i64::MAX)
    });
    if !due {
        return Ok(false);
    }
    // Force a full every Nth scheduled run: the completed chain since (and
    // including) the last full reaching the cadence resets it.
    let force_full = config.full_every > 0 && {
        let mut chain = 0_u64;
        let mut saw_full = false;
        for backup in metadata
            .backups(database_id, 1_000)
            .map_err(ApiError::internal)?
        {
            if backup.status != "completed" {
                continue;
            }
            chain += 1;
            if backup.kind == "full" {
                saw_full = true;
                break;
            }
        }
        saw_full && chain >= config.full_every
    };
    drop(metadata);
    start_job(state, database_id, force_full).map(|_| true)
}

fn start_job(
    state: &ApiState,
    database_id: &str,
    force_full: bool,
) -> Result<AcceptedBackup, ApiError> {
    state.acquire_job(database_id)?;
    let metadata = state
        .metadata()
        .inspect_err(|_| state.release_job(database_id))?;
    let configured = metadata
        .backup_config(database_id)
        .inspect_err(|_| state.release_job(database_id))
        .map_err(ApiError::internal)?;
    if configured.is_none() {
        state.release_job(database_id);
        return Err(ApiError::conflict(
            "configure a backup destination before starting a backup",
        ));
    }
    let parent = metadata
        .latest_completed_backup(database_id)
        .inspect_err(|_| state.release_job(database_id))
        .map_err(ApiError::internal)?;
    let kind = if force_full || parent.is_none() {
        "full"
    } else {
        "incremental"
    };
    let parent_id = (kind == "incremental")
        .then(|| parent.as_ref().map(|record| record.id.clone()))
        .flatten();
    let id = crate::state::random_identifier("backup_", 16);
    let config = metadata
        .backup_config(database_id)
        .inspect_err(|_| state.release_job(database_id))
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            state.release_job(database_id);
            ApiError::internal("backup configuration disappeared")
        })?;
    let object_prefix = format!("{}/{database_id}/{id}", config.prefix);
    metadata
        .start_backup(&NewBackup {
            id: &id,
            database_id,
            kind,
            parent_id: parent_id.as_deref(),
            object_prefix: &object_prefix,
            started_at: &Utc::now().to_rfc3339(),
        })
        .inspect_err(|_| state.release_job(database_id))
        .map_err(ApiError::internal)?;

    let job_state = state.clone();
    let job_database = database_id.to_owned();
    let job_id = id.clone();
    let job_kind = kind.to_owned();
    let failure_state = state.clone();
    let failure_database = database_id.to_owned();
    let failure_id = id.clone();
    std::thread::Builder::new()
        .name(format!("pintail-backup-{database_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(complete_backup_job(
                    job_state,
                    job_database,
                    job_id,
                    job_kind,
                )),
                Err(error) => {
                    finish_backup_error(&failure_state, &failure_database, &failure_id, &error);
                }
            }
        })
        .map_err(|error| {
            finish_backup_error(state, database_id, &id, &error);
            ApiError::unavailable(format!("could not start backup worker: {error}"))
        })?;
    Ok(AcceptedBackup {
        id,
        kind: kind.to_owned(),
        state: "running",
    })
}

pub(crate) async fn restore(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
    Json(request): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<RestoreResponse>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    ensure_database(&state, &database_id)?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "restored database name cannot be empty",
        ));
    }
    let metadata = state.metadata()?;
    let record = metadata
        .backups(&database_id, 1_000)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|backup| backup.id == request.backup_id && backup.status == "completed")
        .ok_or_else(|| ApiError::not_found("completed backup does not exist"))?;
    let config = metadata
        .backup_config(&database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::conflict("backup configuration does not exist"))?;
    let destination = destination(&state, &config)?;
    let store = build_s3(&destination).map_err(bad_request)?;
    let manifest = load_manifest(store.as_ref(), &config.prefix, &database_id, &record.id)
        .await
        .map_err(unavailable)?;
    let control: ControlPlane =
        serde_json::from_value(manifest.control_plane.clone()).map_err(ApiError::internal)?;
    let restored_id = crate::state::random_identifier("db_", 12);
    let target = state.data_dir()?.join("databases").join(&restored_id);
    // Validate the point-in-time request shape before any restore work.
    let catch_up = request
        .point_in_time
        .as_deref()
        .map(|point| {
            let bound = chrono::DateTime::parse_from_rfc3339(point)
                .map_err(|error| ApiError::bad_request(format!("invalid point_in_time: {error}")))?
                .timestamp();
            let bound = u32::try_from(bound)
                .map_err(|_| ApiError::bad_request("point_in_time is outside the binlog era"))?;
            let dsn = request
                .dsn
                .as_deref()
                .ok_or_else(|| ApiError::bad_request("point_in_time requires the source dsn"))?;
            Ok::<_, ApiError>((bound, dsn.trim().to_owned()))
        })
        .transpose()?;
    let restored = restore_backup(store.as_ref(), manifest, &target)
        .await
        .map_err(unavailable)?;
    register_restore(&metadata, &restored_id, name, &control).map_err(ApiError::internal)?;
    state.publish(ApiEvent::database(
        "backup.restored",
        &restored_id,
        format!("restored backup {} side-by-side", record.id),
    ));
    let catching_up = if let Some((bound, dsn)) = catch_up {
        let encrypted = state.encrypt_dsn(&dsn)?;
        metadata
            .upsert_database(&restored_id, name, &encrypted, &Utc::now().to_rfc3339())
            .map_err(ApiError::internal)?;
        spawn_point_in_time_catch_up(state.clone(), restored_id.clone(), bound);
        true
    } else {
        false
    };
    Ok((
        StatusCode::CREATED,
        Json(RestoreResponse {
            database_id: restored_id,
            backup_id: record.id,
            state: "restored",
            restored_bytes: restored.restored_bytes,
            restored_objects: restored.restored_objects,
            catching_up,
        }),
    ))
}

/// Rolls a restored replica forward from its backup checkpoint to the last
/// source transaction at or before the bound, then leaves it paused. Each
/// cycle is the supervisor's own CDC cycle with the stop bound applied;
/// convergence is a cycle that applies nothing.
fn spawn_point_in_time_catch_up(state: crate::ApiState, database_id: String, bound: u32) {
    // The CDC cycle holds non-Send metadata connections across awaits, so
    // the catch-up runs on its own thread with a current-thread runtime —
    // the same shape the supervisor uses for its cycles.
    let _ = std::thread::Builder::new()
        .name(format!("pintail-pitr-{database_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let outcome: Result<(), String> = match runtime {
                Ok(runtime) => runtime.block_on(async {
                    const MAX_CYCLES: usize = 10_000;
                    let database = state
                        .metadata()
                        .map_err(|error| error.to_string())?
                        .database(&database_id)
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "restored database disappeared".to_owned())?;
                    for _ in 0..MAX_CYCLES {
                        let applied =
                            crate::supervisor::run_cycle(&state, &database, Some(bound)).await?;
                        if applied == 0 {
                            return Ok(());
                        }
                    }
                    Err("point-in-time catch-up did not converge".to_owned())
                }),
                Err(error) => Err(error.to_string()),
            };
            match outcome {
                Ok(()) => state.publish(ApiEvent::database(
                    "restore.point_in_time.completed",
                    &database_id,
                    "restored replica rolled forward to the requested instant".to_owned(),
                )),
                Err(error) => state.publish(ApiEvent::database(
                    "restore.point_in_time.error",
                    &database_id,
                    error,
                )),
            }
        });
}

async fn complete_backup_job(
    state: ApiState,
    database_id: String,
    backup_id: String,
    kind: String,
) {
    let started = Instant::now();
    let result = run_backup_job(&state, &database_id, &backup_id, &kind).await;
    match result {
        Ok(summary) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_backup(
                    &backup_id,
                    "completed",
                    summary.uploaded_bytes,
                    summary.uploaded_objects,
                    None,
                    &Utc::now().to_rfc3339(),
                );
            }
            state.publish(ApiEvent::database(
                "backup.completed",
                &database_id,
                format!(
                    "{kind} backup {backup_id} uploaded {} objects in {} ms",
                    summary.uploaded_objects,
                    elapsed_ms(started)
                ),
            ));
            if let Err(error) = apply_backup_retention(&state, &database_id).await {
                state.publish(ApiEvent::database(
                    "backup.retention.error",
                    &database_id,
                    error,
                ));
            }
            verify_backup_if_configured(&state, &database_id, &backup_id).await;
        }
        Err(error) => finish_backup_error(&state, &database_id, &backup_id, &error),
    }
    state.release_job(&database_id);
}

/// Restores a just-completed backup into a scratch directory when the
/// configuration asks for validation — a full download with every object
/// checksummed — recording the outcome on the backup row. Failures never
/// fail the backup itself.
async fn verify_backup_if_configured(state: &ApiState, database_id: &str, backup_id: &str) {
    let outcome = run_backup_verification(state, database_id, backup_id).await;
    match outcome {
        Ok(false) => {}
        Ok(true) => {
            if let Ok(metadata) = state.metadata() {
                let _ =
                    metadata.record_backup_verification(backup_id, None, &Utc::now().to_rfc3339());
            }
            state.publish(ApiEvent::database(
                "backup.verified",
                database_id,
                format!("backup {backup_id} restore-validated"),
            ));
        }
        Err(error) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.record_backup_verification(
                    backup_id,
                    Some(&error),
                    &Utc::now().to_rfc3339(),
                );
            }
            state.publish(ApiEvent::database(
                "backup.verify.error",
                database_id,
                error,
            ));
        }
    }
}

/// Returns Ok(false) when validation is not configured, Ok(true) on a
/// checksum-clean scratch restore.
async fn run_backup_verification(
    state: &ApiState,
    database_id: &str,
    backup_id: &str,
) -> Result<bool, String> {
    fn display(error: impl std::fmt::Display) -> String {
        error.to_string()
    }
    let metadata = state.metadata().map_err(display)?;
    let Some(config) = metadata
        .backup_config(database_id)
        .map_err(display)?
        .filter(|config| config.verify_restore)
    else {
        return Ok(false);
    };
    let destination = destination(state, &config).map_err(display)?;
    let store = build_s3(&destination).map_err(display)?;
    let manifest = load_manifest(store.as_ref(), &config.prefix, database_id, backup_id)
        .await
        .map_err(display)?;
    let scratch = state
        .data_dir()
        .map_err(display)?
        .join("verify")
        .join(backup_id);
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch).map_err(display)?;
    }
    let result = restore_backup(store.as_ref(), manifest, &scratch)
        .await
        .map_err(display);
    let _cleanup = std::fs::remove_dir_all(&scratch);
    result.map(|_| true)
}

/// Prunes completed backups beyond the configured retention, keeping every
/// ancestor a retained incremental depends on and every object a retained
/// manifest still references. Children delete before parents so the
/// `parent_id` foreign key never blocks the sweep.
async fn apply_backup_retention(state: &ApiState, database_id: &str) -> Result<(), String> {
    fn display(error: impl std::fmt::Display) -> String {
        error.to_string()
    }
    let metadata = state.metadata().map_err(display)?;
    let Some(config) = metadata.backup_config(database_id).map_err(display)? else {
        return Ok(());
    };
    if config.retain_count == 0 {
        return Ok(());
    }
    let mut completed: Vec<_> = metadata
        .backups(database_id, 100_000)
        .map_err(display)?
        .into_iter()
        .filter(|backup| backup.status == "completed")
        .collect();
    // backups() already orders newest first.
    let retain = usize::try_from(config.retain_count).unwrap_or(usize::MAX);
    if completed.len() <= retain {
        return Ok(());
    }
    let by_id: std::collections::HashMap<String, Option<String>> = completed
        .iter()
        .map(|backup| (backup.id.clone(), backup.parent_id.clone()))
        .collect();
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    for backup in completed.iter().take(retain) {
        let mut cursor = Some(backup.id.clone());
        while let Some(id) = cursor {
            if !keep.insert(id.clone()) {
                break;
            }
            cursor = by_id.get(&id).cloned().flatten();
        }
    }
    completed.retain(|backup| !keep.contains(&backup.id));
    if completed.is_empty() {
        return Ok(());
    }
    let destination = destination(state, &config).map_err(|error| error.to_string())?;
    let store = pintail_backup::build_s3(&destination).map_err(display)?;
    let mut retained_keys = std::collections::HashSet::new();
    for id in &keep {
        let manifest =
            pintail_backup::load_manifest(store.as_ref(), &config.prefix, database_id, id)
                .await
                .map_err(display)?;
        retained_keys.extend(pintail_backup::manifest_object_keys(&manifest));
    }
    // Newest pruned first: a pruned child always deletes before its pruned
    // parent.
    let mut pruned = 0_usize;
    for backup in &completed {
        pintail_backup::delete_backup(
            store.as_ref(),
            &config.prefix,
            database_id,
            &backup.id,
            &retained_keys,
        )
        .await
        .map_err(display)?;
        metadata.delete_backup_record(&backup.id).map_err(display)?;
        pruned += 1;
    }
    state.publish(ApiEvent::database(
        "backup.retention",
        database_id,
        format!(
            "retention pruned {pruned} backups, keeping {} completed",
            keep.len()
        ),
    ));
    Ok(())
}

async fn run_backup_job(
    state: &ApiState,
    database_id: &str,
    backup_id: &str,
    kind: &str,
) -> Result<pintail_backup::BackupSummary, ApiError> {
    let metadata = state.metadata()?;
    let database = metadata
        .database(database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))?;
    let report: ProbeReport = serde_json::from_str(
        database
            .probe_json
            .as_deref()
            .ok_or_else(|| ApiError::conflict("database has not been probed"))?,
    )
    .map_err(ApiError::internal)?;
    let records = metadata.tables(database_id).map_err(ApiError::internal)?;
    let checkpoint = metadata
        .snapshot_checkpoint(database_id)
        .map_err(ApiError::internal)?;
    let control = control_plane(&database, &records, checkpoint.as_ref())?;
    let config = metadata
        .backup_config(database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::conflict("backup configuration does not exist"))?;
    let destination = destination(state, &config)?;
    let object_store = build_s3(&destination).map_err(bad_request)?;
    let parent_record = (kind == "incremental")
        .then(|| {
            metadata
                .latest_completed_backup(database_id)
                .map_err(ApiError::internal)
        })
        .transpose()?
        .flatten();
    let parent = if let Some(parent) = &parent_record {
        Some(
            load_manifest(
                object_store.as_ref(),
                &config.prefix,
                database_id,
                &parent.id,
            )
            .await
            .map_err(unavailable)?,
        )
    } else {
        None
    };
    drop(metadata);

    let (tables, _pins) = pinned_tables(state, database_id, &report, &records)?;
    let source = BackupSource {
        database_id: database_id.to_owned(),
        backup_id: backup_id.to_owned(),
        parent_id: parent_record.map(|record| record.id),
        control_plane: serde_json::to_value(control).map_err(ApiError::internal)?,
        tables,
    };
    let (_, summary) = create_backup(object_store, &config.prefix, source, parent.as_ref())
        .await
        .map_err(unavailable)?;
    Ok(summary)
}

fn pinned_tables(
    state: &ApiState,
    database_id: &str,
    report: &ProbeReport,
    records: &[TableRecord],
) -> Result<(Vec<SourceTable>, Vec<TableSnapshot>), ApiError> {
    let by_name = records
        .iter()
        .map(|record| (record.name.to_ascii_lowercase(), record))
        .collect::<BTreeMap<_, _>>();
    let root = state
        .data_dir()?
        .join("databases")
        .join(database_id)
        .join("tables");
    let mut tables = Vec::new();
    let mut pins = Vec::new();
    for source in &report.tables {
        if !by_name.contains_key(&source.name.to_ascii_lowercase()) {
            continue;
        }
        let directory = table_directory(&root, &source.name);
        let directory_name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ApiError::internal("table directory is not valid UTF-8"))?
            .to_owned();
        let mut store = TableStore::open(
            &directory,
            source.table_schema().map_err(ApiError::internal)?,
            StoreOptions::default(),
        )
        .map_err(unavailable)?;
        store.flush().map_err(unavailable)?;
        let snapshot = store.snapshot();
        let artifacts = snapshot.backup_artifacts().map_err(ApiError::internal)?;
        let segments = artifacts
            .segments()
            .iter()
            .map(|segment| SourceSegment {
                file_name: segment.file_name().to_owned(),
                path: segment.path().to_path_buf(),
            })
            .collect();
        tables.push(SourceTable {
            name: source.name.clone(),
            directory_name,
            manifest: artifacts.manifest().to_vec(),
            segments,
        });
        pins.push(snapshot);
    }
    Ok((tables, pins))
}

fn control_plane(
    database: &pintail_meta::DatabaseRecord,
    tables: &[TableRecord],
    checkpoint: Option<&pintail_meta::SnapshotCheckpointRecord>,
) -> Result<ControlPlane, ApiError> {
    Ok(ControlPlane {
        database: ControlDatabase {
            name: database.name.clone(),
            effective_mode: database
                .effective_mode
                .clone()
                .filter(|mode| matches!(mode.as_str(), "cdc" | "polling"))
                .ok_or_else(|| ApiError::conflict("database is not ready for backup"))?,
            probe_json: database
                .probe_json
                .clone()
                .ok_or_else(|| ApiError::conflict("database has not been probed"))?,
        },
        tables: tables
            .iter()
            .map(|table| ControlTable {
                name: table.name.clone(),
                primary_key_json: table.primary_key_json.clone(),
                cursor_column: table.cursor_column.clone(),
                sort_key_json: table.sort_key_json.clone(),
                rows_synced: table.rows_synced,
                schema_version: table.schema_version,
                soft_delete_column: table.soft_delete_column.clone(),
            })
            .collect(),
        checkpoint: checkpoint.map(|checkpoint| ControlCheckpoint {
            kind: checkpoint.kind.clone(),
            gtid_set: checkpoint.gtid_set.clone(),
            binlog_file: checkpoint.binlog_file.clone(),
            binlog_pos: checkpoint.binlog_pos,
        }),
    })
}

fn register_restore(
    metadata: &pintail_meta::MetaStore,
    database_id: &str,
    name: &str,
    control: &ControlPlane,
) -> anyhow::Result<()> {
    let tables = control
        .tables
        .iter()
        .map(|table| RestoredTable {
            name: &table.name,
            primary_key_json: table.primary_key_json.as_deref(),
            cursor_column: table.cursor_column.as_deref(),
            sort_key_json: table.sort_key_json.as_deref(),
            rows_synced: table.rows_synced,
            schema_version: table.schema_version,
            soft_delete_column: table.soft_delete_column.as_deref(),
        })
        .collect::<Vec<_>>();
    let checkpoint = control
        .checkpoint
        .as_ref()
        .map(|checkpoint| RestoredCheckpoint {
            kind: &checkpoint.kind,
            gtid_set: checkpoint.gtid_set.as_deref(),
            binlog_file: checkpoint.binlog_file.as_deref(),
            binlog_pos: checkpoint.binlog_pos,
        });
    metadata.register_restored_database(&RestoredDatabase {
        id: database_id,
        name,
        probe_json: &control.database.probe_json,
        effective_mode: &control.database.effective_mode,
        tables: &tables,
        checkpoint,
        now: &Utc::now().to_rfc3339(),
    })
}

fn destination(state: &ApiState, config: &BackupConfigRecord) -> Result<S3Destination, ApiError> {
    let access_key_id = config
        .encrypted_access_key_id
        .as_deref()
        .map(|secret| state.decrypt_secret(secret))
        .transpose()?;
    let secret_access_key = config
        .encrypted_secret_access_key
        .as_deref()
        .map(|secret| state.decrypt_secret(secret))
        .transpose()?;
    Ok(S3Destination {
        bucket: config.bucket.clone(),
        prefix: config.prefix.clone(),
        endpoint: config.endpoint.clone(),
        region: config.region.clone(),
        access_key_id,
        secret_access_key,
    })
}

fn encrypted_credentials(
    state: &ApiState,
    request: &BackupConfigRequest,
    existing: Option<&BackupConfigRecord>,
) -> Result<EncryptedCredentials, ApiError> {
    if request.clear_credentials {
        return Ok((None, None));
    }
    if let (Some(access_key), Some(secret)) = (
        request.access_key_id.as_deref(),
        request.secret_access_key.as_deref(),
    ) {
        return Ok((
            Some(state.encrypt_secret(access_key)?),
            Some(state.encrypt_secret(secret)?),
        ));
    }
    Ok(existing.map_or((None, None), |config| {
        (
            config.encrypted_access_key_id.clone(),
            config.encrypted_secret_access_key.clone(),
        )
    }))
}

fn ensure_database(state: &ApiState, database_id: &str) -> Result<(), ApiError> {
    if state
        .metadata()?
        .database(database_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        Ok(())
    } else {
        Err(ApiError::not_found("database does not exist"))
    }
}

fn finish_backup_error(
    state: &ApiState,
    database_id: &str,
    backup_id: &str,
    error: &impl std::fmt::Display,
) {
    let message = error.to_string();
    if let Ok(metadata) = state.metadata() {
        let _ = metadata.finish_backup(
            backup_id,
            "error",
            0,
            0,
            Some(&message),
            &Utc::now().to_rfc3339(),
        );
    }
    state.publish(ApiEvent::database("backup.error", database_id, message));
    state.release_job(database_id);
}

impl From<BackupConfigRecord> for BackupConfigResponse {
    fn from(config: BackupConfigRecord) -> Self {
        Self {
            configured: true,
            credentials_configured: config.encrypted_access_key_id.is_some(),
            bucket: config.bucket,
            prefix: config.prefix,
            endpoint: config.endpoint,
            region: config.region,
            schedule_minutes: config.schedule_minutes,
            enabled: config.enabled,
            retain_count: config.retain_count,
            verify_restore: config.verify_restore,
            full_every: config.full_every,
            updated_at: config.updated_at,
        }
    }
}

impl Default for BackupConfigResponse {
    fn default() -> Self {
        Self {
            configured: false,
            bucket: String::new(),
            prefix: "pintail".to_owned(),
            endpoint: None,
            region: default_region(),
            schedule_minutes: default_schedule(),
            enabled: false,
            retain_count: 0,
            verify_restore: false,
            full_every: 0,
            credentials_configured: false,
            updated_at: String::new(),
        }
    }
}

impl From<BackupRecord> for BackupResponse {
    fn from(backup: BackupRecord) -> Self {
        Self {
            id: backup.id,
            database_id: backup.database_id,
            kind: backup.kind,
            parent_id: backup.parent_id,
            object_prefix: backup.object_prefix,
            status: backup.status,
            bytes: backup.bytes,
            object_count: backup.object_count,
            error: backup.error,
            started_at: backup.started_at,
            completed_at: backup.completed_at,
            verified_at: backup.verified_at,
            verify_error: backup.verify_error,
        }
    }
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

const fn default_schedule() -> u64 {
    1_440
}

const fn enabled() -> bool {
    true
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn bad_request(error: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(error.to_string())
}

fn unavailable(error: impl std::fmt::Display) -> ApiError {
    ApiError::unavailable(error.to_string())
}
