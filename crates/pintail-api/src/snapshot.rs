use std::{
    collections::{BTreeSet, hash_map::DefaultHasher},
    hash::{Hash as _, Hasher as _},
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use mysql_async::{Opts, Pool};
use pintail_cdc::{CdcOptions, CdcTarget, run_cdc};
use pintail_meta::{DatabaseRecord, SnapshotChunkStatus, TableRecord};
use pintail_poll::{PollOptions, PollTarget, run_poll_cycle};
use pintail_probe::{ProbeReport, RecommendedMode};
use pintail_snapshot::{
    SnapshotOptions, SnapshotProgress, SnapshotResult, SnapshotTarget, run_snapshot_with_progress,
};
use pintail_store::{StoreOptions, TableStore};
use serde::{Deserialize, Serialize};

use crate::{ApiState, auth::AuthPrincipal, error::ApiError, events::ApiEvent};

#[derive(Deserialize)]
pub(crate) struct SnapshotRequest {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
pub(crate) struct AcceptedSnapshot {
    run_id: String,
    state: &'static str,
}

#[derive(Serialize)]
pub(crate) struct SnapshotStatus {
    database_id: String,
    state: String,
    effective_mode: Option<String>,
    tables: Vec<TableSnapshotStatus>,
}

#[derive(Serialize)]
struct TableSnapshotStatus {
    name: String,
    state: String,
    rows: u64,
    completed_chunks: usize,
    total_chunks: usize,
    last_error: Option<String>,
}

pub(crate) async fn start(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
    payload: Option<Json<SnapshotRequest>>,
) -> Result<(StatusCode, Json<AcceptedSnapshot>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    state.acquire_job(&database_id)?;
    let run_id = crate::state::random_identifier("run_", 16);
    let metadata = state.metadata()?;
    let database = metadata
        .database(&database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            state.release_job(&database_id);
            ApiError::not_found("database does not exist")
        })?;
    if database.mode == "paused" {
        state.release_job(&database_id);
        return Err(ApiError::conflict(
            "resume the database before starting a snapshot",
        ));
    }
    if let Err(error) = metadata.start_sync_run(
        &run_id,
        &database_id,
        None,
        "snapshot",
        &Utc::now().to_rfc3339(),
    ) {
        state.release_job(&database_id);
        return Err(ApiError::internal(error));
    }
    let force = payload.is_some_and(|Json(request)| request.force);
    let job_state = state.clone();
    let job_database_id = database_id.clone();
    let job_run_id = run_id.clone();
    let failure_state = state.clone();
    let failure_database_id = database_id.clone();
    let failure_run_id = run_id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("pintail-snapshot-{database_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(complete_snapshot_job(
                    job_state,
                    job_database_id,
                    job_run_id,
                    force,
                )),
                Err(error) => fail_snapshot_job(
                    &job_state,
                    &job_database_id,
                    &job_run_id,
                    &error.to_string(),
                    0,
                ),
            }
        })
    {
        let message = format!("could not start snapshot worker: {error}");
        fail_snapshot_job(
            &failure_state,
            &failure_database_id,
            &failure_run_id,
            &message,
            0,
        );
        return Err(ApiError::unavailable(message));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedSnapshot {
            run_id,
            state: "snapshotting",
        }),
    ))
}

pub(crate) async fn start_forced(
    principal: Extension<AuthPrincipal>,
    state: State<ApiState>,
    database_id: Path<String>,
) -> Result<(StatusCode, Json<AcceptedSnapshot>), ApiError> {
    start(
        principal,
        state,
        database_id,
        Some(Json(SnapshotRequest { force: true })),
    )
    .await
}

async fn complete_snapshot_job(state: ApiState, database_id: String, run_id: String, force: bool) {
    let started = Instant::now();
    match run_snapshot_job(&state, &database_id, &run_id, force).await {
        Ok((rows, bytes, mode)) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(
                    &run_id,
                    "completed",
                    rows,
                    bytes,
                    duration_ms(started),
                    None,
                );
            }
            state.publish(ApiEvent::database(
                "replication.ready",
                &database_id,
                format!("{mode} handoff is ready"),
            ));
        }
        Err(error) => {
            fail_snapshot_job(&state, &database_id, &run_id, &error, duration_ms(started));
        }
    }
    state.release_job(&database_id);
}

fn fail_snapshot_job(
    state: &ApiState,
    database_id: &str,
    run_id: &str,
    error: &str,
    elapsed_ms: u64,
) {
    if let Ok(metadata) = state.metadata() {
        let now = Utc::now().to_rfc3339();
        let _ = metadata.finish_sync_run(run_id, "error", 0, 0, elapsed_ms, Some(error));
        let _ = metadata.fail_database_job(database_id, error, &now);
    }
    state.publish(ApiEvent::database("replication.error", database_id, error));
    state.release_job(database_id);
}

pub(crate) async fn status(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<Json<SnapshotStatus>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&database_id)?;
    let metadata = state.metadata()?;
    let database = metadata
        .database(&database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))?;
    let tables = metadata
        .tables(&database_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|table| table_snapshot_status(&metadata, table))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(SnapshotStatus {
        database_id,
        state: database.state,
        effective_mode: database.effective_mode,
        tables,
    }))
}

async fn run_snapshot_job(
    state: &ApiState,
    database_id: &str,
    run_id: &str,
    force: bool,
) -> Result<(u64, u64, &'static str), String> {
    let metadata = state.metadata().map_err(display)?;
    let database = metadata
        .database(database_id)
        .map_err(display)?
        .ok_or_else(|| "database does not exist".to_owned())?;
    let report: ProbeReport = serde_json::from_str(
        database
            .probe_json
            .as_deref()
            .ok_or_else(|| "probe the database before starting a snapshot".to_owned())?,
    )
    .map_err(display)?;
    let sources = selected_sources(&database, &report)?;
    let data_dir = state.data_dir().map_err(display)?.to_path_buf();
    let metadata_path = state.metadata_path().map_err(display)?.to_path_buf();
    let root = data_dir.join("databases").join(database_id).join("tables");
    std::fs::create_dir_all(&root).map_err(display)?;
    let mut targets = Vec::with_capacity(sources.len());
    if force {
        metadata
            .begin_resnapshot(database_id, &Utc::now().to_rfc3339())
            .map_err(display)?;
    }
    for source in sources {
        let mut store = TableStore::open(
            table_directory(&root, &source.name),
            source.table_schema().map_err(display)?,
            StoreOptions::default(),
        )
        .map_err(display)?;
        if force {
            store.reset_for_resnapshot().map_err(display)?;
        }
        targets.push(SnapshotTarget::new(source, store).map_err(display)?);
    }
    drop(metadata);
    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = Opts::from_url(&dsn).map_err(display)?;
    let pool = Pool::new(options);
    let bytes = Arc::new(AtomicU64::new(0));
    let progress_state = state.clone();
    let progress_bytes = Arc::clone(&bytes);
    let progress_database_id = database_id.to_owned();
    let result = run_snapshot_with_progress(
        &pool,
        &metadata_path,
        database_id,
        &report,
        targets,
        SnapshotOptions::default(),
        move |progress| {
            progress_bytes.fetch_add(progress.bytes, Ordering::Relaxed);
            progress_state.publish(snapshot_event(&progress_database_id, progress));
        },
    )
    .await
    .map_err(display)?;
    let rows = result.tables.iter().map(|table| table.rows).sum();
    let mode = handoff_replication(
        &pool,
        &metadata_path,
        database_id,
        &database,
        &report,
        result,
        root,
    )
    .await?;
    pool.disconnect().await.map_err(display)?;
    state
        .metadata()
        .map_err(display)?
        .set_database_replication_state(database_id, mode, &Utc::now().to_rfc3339())
        .map_err(display)?;
    state.publish(ApiEvent::database(
        "snapshot.completed",
        database_id,
        format!("snapshot run {run_id} completed with {rows} rows"),
    ));
    Ok((rows, bytes.load(Ordering::Relaxed), mode))
}

async fn handoff_replication(
    pool: &Pool,
    metadata_path: &FsPath,
    database_id: &str,
    database: &DatabaseRecord,
    report: &ProbeReport,
    result: SnapshotResult,
    root: PathBuf,
) -> Result<&'static str, String> {
    let mode = effective_mode(database, report);
    match mode {
        "cdc" => {
            let new_table_includes = decode_name_set(database.include_tables.as_deref())?;
            let new_table_excludes = decode_name_set(database.exclude_tables.as_deref())?;
            let targets = result
                .targets
                .into_iter()
                .map(|target| {
                    let source = target.source().clone();
                    CdcTarget::new(source, target.into_store())
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(display)?;
            run_cdc(
                pool,
                metadata_path,
                database_id,
                report,
                targets,
                CdcOptions {
                    blocking: false,
                    new_table_root: Some(root),
                    new_table_includes,
                    new_table_excludes,
                    ..CdcOptions::default()
                },
            )
            .await
            .map_err(display)?;
        }
        "polling" => {
            let targets = result
                .targets
                .into_iter()
                .map(|target| {
                    let source = target.source().clone();
                    PollTarget::new(source, target.into_store())
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(display)?;
            run_poll_cycle(
                pool,
                metadata_path,
                database_id,
                report,
                targets,
                PollOptions {
                    force: true,
                    ..PollOptions::default()
                },
            )
            .await
            .map_err(display)?;
        }
        _ => return Err("unsupported replication mode".to_owned()),
    }
    Ok(mode)
}

fn selected_sources(
    database: &DatabaseRecord,
    report: &ProbeReport,
) -> Result<Vec<pintail_probe::SourceTable>, String> {
    let includes = decode_name_set(database.include_tables.as_deref())?;
    let excludes = decode_name_set(database.exclude_tables.as_deref())?;
    let selected = report
        .tables
        .iter()
        .filter(|table| {
            let name = table.name.to_ascii_lowercase();
            (includes.is_empty() || includes.contains(&name)) && !excludes.contains(&name)
        })
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        Err("table selection is empty".to_owned())
    } else {
        Ok(selected)
    }
}

fn decode_name_set(value: Option<&str>) -> Result<BTreeSet<String>, String> {
    value.map_or_else(
        || Ok(BTreeSet::new()),
        |value| {
            serde_json::from_str::<Vec<String>>(value)
                .map(|names| {
                    names
                        .into_iter()
                        .map(|name| name.to_ascii_lowercase())
                        .collect()
                })
                .map_err(display)
        },
    )
}

fn effective_mode(database: &DatabaseRecord, report: &ProbeReport) -> &'static str {
    match database.mode.as_str() {
        "cdc" => "cdc",
        "polling" => "polling",
        _ => match report.capabilities.recommended_mode {
            RecommendedMode::Cdc => "cdc",
            RecommendedMode::Polling => "polling",
        },
    }
}

fn table_snapshot_status(
    metadata: &pintail_meta::MetaStore,
    table: TableRecord,
) -> Result<TableSnapshotStatus, ApiError> {
    let chunks = metadata
        .snapshot_chunks(&table.database_id, &table.name)
        .map_err(ApiError::internal)?;
    let completed_chunks = chunks
        .iter()
        .filter(|chunk| chunk.status == SnapshotChunkStatus::Completed)
        .count();
    Ok(TableSnapshotStatus {
        name: table.name,
        state: table.state,
        rows: table.rows_synced,
        completed_chunks,
        total_chunks: chunks.len(),
        last_error: table.last_error,
    })
}

fn snapshot_event(database_id: &str, progress: SnapshotProgress) -> ApiEvent {
    ApiEvent {
        kind: "snapshot.progress".to_owned(),
        database_id: Some(database_id.to_owned()),
        table: Some(progress.table),
        message: format!("chunk {} is durable", progress.chunk_id),
        rows: Some(progress.rows),
        bytes: Some(progress.bytes),
        eta_seconds: progress.eta_seconds,
        at: Utc::now().to_rfc3339(),
    }
}

pub(crate) fn table_directory(root: &FsPath, table: &str) -> PathBuf {
    let safe = table
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let mut hasher = DefaultHasher::new();
    table.to_ascii_lowercase().hash(&mut hasher);
    root.join(format!("table-{safe}-{:016x}", hasher.finish()))
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
