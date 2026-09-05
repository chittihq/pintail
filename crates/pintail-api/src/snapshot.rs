use std::{
    collections::BTreeSet,
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
use mysql_async::Pool;
use pintail_cdc::{CdcOptions, CdcTarget, run_cdc};
use pintail_meta::{DatabaseRecord, MetaStore, SnapshotChunkStatus, TableRecord};
use pintail_poll::{PollOptions, PollTarget, run_poll_cycle};
use pintail_probe::{ProbeReport, RecommendedMode, probe};
use pintail_snapshot::{
    SnapshotOptions, SnapshotPosition, SnapshotProgress, SnapshotResult, SnapshotTarget,
    TableSnapshotFailure, run_snapshot_with_progress,
};
use pintail_store::{StoreOptions, TableStore};
use serde::{Deserialize, Serialize};

use crate::{ApiState, audit, auth::AuthPrincipal, error::ApiError, events::ApiEvent};

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
    crate::databases::load_database(&state, &principal, &database_id)?;
    let force = payload.is_some_and(|Json(request)| request.force);
    let run_id = begin_snapshot_job(&state, &database_id, force)?;
    audit::record(
        &state,
        &principal,
        "snapshot.start",
        Some(("database", &database_id)),
        Some(serde_json::json!({"force": force})),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedSnapshot {
            run_id,
            state: "snapshotting",
        }),
    ))
}

/// Clears the mirror and starts over with the stored connection.
///
/// The operator's escape hatch when replication state is wedged beyond what
/// a per-table resync repairs: every tracked table, checkpoint, quarantined
/// event and on-disk store is dropped, then a forced snapshot re-probes the
/// source and copies everything fresh, continuing in whatever mode the
/// database is configured for. Nothing about the connection is asked again.
pub(crate) async fn reset(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<(StatusCode, Json<AcceptedSnapshot>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    crate::databases::load_database(&state, &principal, &database_id)?;
    // Hold the job slot through the wipe so no replication cycle is mid-write
    // while the state underneath it disappears.
    state.require_replicated(&database_id, "a factory reset")?;
    state.acquire_job_as(&database_id, "a factory reset")?;
    let wiped = (|| -> Result<(), ApiError> {
        let mut metadata = state.metadata()?;
        let database = metadata
            .database(&database_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("database does not exist"))?;
        if database.mode == "paused" {
            return Err(ApiError::conflict(
                "resume the database before resetting it",
            ));
        }
        metadata
            .reset_database_replication(&database_id, &Utc::now().to_rfc3339())
            .map_err(ApiError::internal)?;
        let tables_dir = state
            .data_dir()?
            .join("databases")
            .join(&database_id)
            .join("tables");
        if tables_dir.exists() {
            std::fs::remove_dir_all(&tables_dir).map_err(ApiError::internal)?;
        }
        Ok(())
    })();
    state.release_job(&database_id);
    wiped?;
    state.publish(ApiEvent::database(
        "database.reset",
        &database_id,
        "replication state cleared; a fresh snapshot follows",
    ));
    let run_id = begin_snapshot_job(&state, &database_id, true)?;
    audit::record(
        &state,
        &principal,
        "database.reset",
        Some(("database", &database_id)),
        None,
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedSnapshot {
            run_id,
            state: "snapshotting",
        }),
    ))
}

/// Acquires the database job slot, journals a snapshot run, and detaches the
/// worker. Used by the snapshot/resync routes and the supervisor's
/// `auto_resync` keyless-policy repair.
pub(crate) fn begin_snapshot_job(
    state: &ApiState,
    database_id: &str,
    force: bool,
) -> Result<String, ApiError> {
    state.require_replicated(database_id, "a snapshot")?;
    state.acquire_job_as(database_id, "a full snapshot")?;
    let run_id = crate::state::random_identifier("run_", 16);
    let metadata = match state.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            state.release_job(database_id);
            return Err(error);
        }
    };
    let database = match metadata.database(database_id) {
        Ok(Some(database)) => database,
        Ok(None) => {
            state.release_job(database_id);
            return Err(ApiError::not_found("database does not exist"));
        }
        Err(error) => {
            state.release_job(database_id);
            return Err(ApiError::internal(error));
        }
    };
    if database.mode == "paused" {
        state.release_job(database_id);
        return Err(ApiError::conflict(
            "resume the database before starting a snapshot",
        ));
    }
    if let Err(error) = metadata.start_sync_run(
        &run_id,
        database_id,
        None,
        "snapshot",
        &Utc::now().to_rfc3339(),
    ) {
        state.release_job(database_id);
        return Err(ApiError::internal(error));
    }
    let job_state = state.clone();
    let job_database_id = database_id.to_owned();
    let job_run_id = run_id.clone();
    let failure_state = state.clone();
    let failure_database_id = database_id.to_owned();
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
    Ok(run_id)
}

async fn complete_snapshot_job(state: ApiState, database_id: String, run_id: String, force: bool) {
    let started = Instant::now();
    match run_snapshot_job(&state, &database_id, &run_id, force).await {
        Ok((rows, bytes, mode, failed)) => {
            let partial = (!failed.is_empty()).then(|| {
                format!(
                    "{} table(s) could not be copied and are flagged for resync: {}",
                    failed.len(),
                    failed
                        .iter()
                        .map(|failure| format!("{} ({})", failure.table, failure.error))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            });
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(
                    &run_id,
                    "completed",
                    rows,
                    bytes,
                    duration_ms(started),
                    partial.as_deref(),
                );
            }
            if let Some(partial) = &partial {
                state.publish(ApiEvent::database(
                    "snapshot.partial",
                    &database_id,
                    partial,
                ));
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
    crate::databases::load_database(&state, &principal, &database_id)?;
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

#[allow(clippy::too_many_lines)]
async fn run_snapshot_job(
    state: &ApiState,
    database_id: &str,
    run_id: &str,
    force: bool,
) -> Result<(u64, u64, &'static str, Vec<TableSnapshotFailure>), String> {
    let mut metadata = state.metadata().map_err(display)?;
    let database = metadata
        .database(database_id)
        .map_err(display)?
        .ok_or_else(|| "database does not exist".to_owned())?;
    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = crate::dsn::source_opts(&dsn)?;
    let pool = Pool::new(options);
    // A forced snapshot runs MID-STREAM, so the stored probe can be older
    // than the source: a table created since it was taken is absent from it.
    // Snapshotting the stale list and then handing the stream a position
    // captured AFTER that CREATE TABLE loses the statement outright - the
    // table is never copied and never auto-included, the stream looks
    // healthy, and nothing errors. Re-probing first is what keeps the
    // snapshot and the position it hands over describing the same source.
    let report: ProbeReport = if force {
        let refreshed = probe(&pool, &database.name).await.map_err(display)?;
        let encoded = serde_json::to_string(&refreshed).map_err(display)?;
        // Probe JSON only: this must not disturb the lifecycle state, which
        // is what removed a live database from the supervisor's schedule.
        metadata
            .refresh_database_probe_json(database_id, &encoded, &Utc::now().to_rfc3339())
            .map_err(display)?;
        refreshed
    } else {
        serde_json::from_str(
            database
                .probe_json
                .as_deref()
                .ok_or_else(|| "probe the database before starting a snapshot".to_owned())?,
        )
        .map_err(display)?
    };
    let sources = selected_sources(&database, &report)?;
    // A database that already handed off to replication keeps its copied
    // tables live. Without `force`, a snapshot here copies only the tables
    // whose copy never reached its end - a restart's leftovers - and any
    // table the source added since; walking the complete ones re-read the
    // whole source and turned them all pending while it did.
    let handed_off = !force
        && metadata
            .snapshot_checkpoint(database_id)
            .map_err(display)?
            .is_some();
    let sources = if handed_off {
        let incomplete = metadata
            .tables_without_complete_copy(database_id)
            .map_err(display)?
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let tracked = metadata
            .tables(database_id)
            .map_err(display)?
            .into_iter()
            .map(|table| table.name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let total = sources.len();
        let selected = sources
            .into_iter()
            .filter(|source| {
                let name = source.name.to_ascii_lowercase();
                incomplete.contains(&name) || !tracked.contains(&name)
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            state.publish(ApiEvent::database(
                "snapshot.nothing_to_copy",
                database_id,
                format!("every one of the {total} tables holds a complete copy; nothing to do"),
            ));
            pool.disconnect().await.map_err(display)?;
            return Ok((0, 0, effective_mode(&database, &report), Vec::new()));
        }
        state.publish(ApiEvent::database(
            "snapshot.partial_copy",
            database_id,
            format!(
                "copying {} of {total} tables whose copy is incomplete: {}; the rest stay live",
                selected.len(),
                summarize_names(selected.iter().map(|source| source.name.as_str()))
            ),
        ));
        selected
    } else {
        sources
    };
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
        let mut source = source;
        let directory = table_directory(&root, &source.name);
        // A forced snapshot recopies everything, so a store whose schema no
        // longer matches the source is rebuilt; a resumable first snapshot
        // must not wipe half-copied chunks, so it stays strict.
        let mut store =
            open_tracked_store(&mut metadata, database_id, &mut source, directory, force)?;
        if force {
            store.reset_for_resnapshot().map_err(display)?;
        }
        targets.push(SnapshotTarget::new(source, store).map_err(display)?);
    }
    drop(metadata);
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
    let failed = result.failed.clone();
    if handed_off {
        // The database is already replicating: finish each copied table the
        // way a table resync does - fence it against replaying its own rows,
        // hand it back to the live state, drop its dead letters - and leave
        // the handoff alone.
        let mode = effective_mode(&database, &report);
        let table_state = if mode == "polling" {
            "polling"
        } else {
            "streaming"
        };
        let metadata = state.metadata().map_err(display)?;
        for target in &result.targets {
            let name = target.source().name.clone();
            if failed.iter().any(|failure| failure.table == name) {
                continue;
            }
            fence_table_after_copy(
                &metadata,
                database_id,
                &name,
                &result.captured_position,
                mode == "cdc",
            )?;
            metadata
                .finish_table_resnapshot(database_id, &name, table_state)
                .map_err(display)?;
            metadata
                .clear_dlq_for_table(database_id, &name)
                .map_err(display)?;
        }
        pool.disconnect().await.map_err(display)?;
        state.publish(ApiEvent::database(
            "snapshot.completed",
            database_id,
            format!("snapshot run {run_id} copied {rows} rows into the live database"),
        ));
        return Ok((rows, bytes.load(Ordering::Relaxed), mode, failed));
    }
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
    Ok((rows, bytes.load(Ordering::Relaxed), mode, failed))
}

/// Up to a dozen names, then a count of the rest.
pub(crate) fn summarize_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names = names.collect::<Vec<_>>();
    if names.len() <= 12 {
        return names.join(", ");
    }
    format!("{} and {} more", names[..12].join(", "), names.len() - 12)
}

/// Records where a freshly copied table's data stands in the stream, so CDC
/// does not replay the rows the copy already holds.
///
/// Without a fence the stream would replay events the copy already holds.
/// Deletes and updates would land again harmlessly on a keyed table, but an
/// append-keyed one would duplicate, so a CDC database whose source reported
/// no position refuses the copy rather than leave that to chance. A polling
/// database has no stream to replay, and its source may not write a binlog
/// at all.
///
/// # Errors
///
/// Returns the metadata error, or the refusal.
pub(crate) fn fence_table_after_copy(
    metadata: &MetaStore,
    database_id: &str,
    table_name: &str,
    captured: &SnapshotPosition,
    cdc: bool,
) -> Result<(), String> {
    let fence = match captured {
        SnapshotPosition::Gtid {
            file: Some(file),
            position: Some(position),
            ..
        }
        | SnapshotPosition::FilePosition { file, position } => Some((file.clone(), *position)),
        SnapshotPosition::Gtid { .. } | SnapshotPosition::Unavailable => None,
    };
    match fence {
        Some((file, position)) => metadata
            .set_setting(
                &pintail_cdc::snapshot_fence_key(database_id, &table_name.to_ascii_lowercase()),
                &format!("{file}:{position}"),
            )
            .map_err(display),
        None if cdc => Err(
            "source did not report a binlog position for the snapshot, so the table \
             cannot be fenced against replaying its own rows"
                .to_owned(),
        ),
        None => Ok(()),
    }
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

pub(crate) fn effective_mode(database: &DatabaseRecord, report: &ProbeReport) -> &'static str {
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

/// Progress for a single-table resnapshot, distinguishable from a full
/// snapshot's chunks in the activity feed.
pub(crate) fn resnapshot_progress_event(database_id: &str, progress: SnapshotProgress) -> ApiEvent {
    ApiEvent {
        kind: "resnapshot.progress".to_owned(),
        database_id: Some(database_id.to_owned()),
        table: Some(progress.table),
        message: format!("chunk {} is durable", progress.chunk_id),
        rows: Some(progress.rows),
        bytes: Some(progress.bytes),
        eta_seconds: progress.eta_seconds,
        at: Utc::now().to_rfc3339(),
    }
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

/// Opens a table store at its durable schema-history version: live DDL can
/// evolve a store past the probe's version-1 shape, and opening it with a
/// stale schema fails with a version mismatch (the resync path did exactly
/// that — found by the e2e control-plane gate, 2026-08-03). Mirrors
/// `CdcTarget::open_tracked`.
pub(crate) fn open_tracked_store(
    metadata: &mut pintail_meta::MetaStore,
    database_id: &str,
    source: &mut pintail_probe::SourceTable,
    directory: std::path::PathBuf,
    wipe_on_schema_mismatch: bool,
) -> Result<pintail_store::TableStore, String> {
    let history = metadata
        .schema_history(database_id, &source.name)
        .map_err(display)?;
    let attempted =
        open_store_with_history(metadata, database_id, source, directory.clone(), &history);
    match attempted {
        Err(message)
            if wipe_on_schema_mismatch
                && (message.contains("schema fingerprint mismatch")
                    || message.contains("schema version mismatch")
                    // stabilize's in-place refusals: adoptable here because
                    // the branch below deletes the store before rebuilding.
                    || message.contains("changed physical type")
                    || message.contains("physical key changed")) =>
        {
            // The store on disk was built from a shape this control plane has
            // no usable record of - schema history is only written by DDL
            // events, so a source migrated while nothing was streaming leaves
            // the durable store and the fresh probe disagreeing with no
            // history row to bridge them. The caller is recopying the table
            // wholesale, so the data carries no information worth keeping:
            // rebuild the store around the source's current shape instead of
            // refusing forever.
            std::fs::remove_dir_all(&directory).map_err(display)?;
            let version = match history.last() {
                None => 1,
                Some(record) => {
                    let stored: Vec<pintail_probe::SourceColumn> =
                        serde_json::from_str(&record.columns_json).map_err(display)?;
                    let mut previous = source.clone();
                    previous.columns = stored;
                    // Stable-ID continuity is a property of LIVE evolution;
                    // this store was just deleted, so a column whose physical
                    // type changed (which stabilize rightly refuses to adopt
                    // in place) simply takes a fresh identity - refusing here
                    // left "column X changed physical type" looping forever
                    // on the exact path that exists to repair it.
                    if let Ok(adopted) =
                        pintail_probe::stabilize_source_table(&previous, source.clone())
                    {
                        source.columns = adopted.columns;
                    }
                    let version = record
                        .version
                        .checked_add(1)
                        .ok_or_else(|| "table schema version exceeds UInt32".to_owned())?;
                    let columns_json = serde_json::to_string(&source.columns).map_err(display)?;
                    metadata
                        .record_schema_history(
                            database_id,
                            &source.name,
                            version,
                            None,
                            &columns_json,
                            &Utc::now().to_rfc3339(),
                        )
                        .map_err(display)?;
                    version
                }
            };
            TableStore::open(
                directory,
                source.table_schema_with_version(version).map_err(display)?,
                StoreOptions::default(),
            )
            .map_err(display)
        }
        other => other,
    }
}

fn open_store_with_history(
    metadata: &mut pintail_meta::MetaStore,
    database_id: &str,
    source: &mut pintail_probe::SourceTable,
    directory: std::path::PathBuf,
    history: &[pintail_meta::SchemaHistoryRecord],
) -> Result<pintail_store::TableStore, String> {
    let Some(record) = history.last() else {
        return TableStore::open(
            directory,
            source.table_schema_with_version(1).map_err(display)?,
            StoreOptions::default(),
        )
        .map_err(display);
    };
    let stored: Vec<pintail_probe::SourceColumn> =
        serde_json::from_str(&record.columns_json).map_err(display)?;
    if columns_equivalent(&stored, &source.columns) {
        source.columns = stored;
        return TableStore::open(
            directory,
            source
                .table_schema_with_version(record.version)
                .map_err(display)?,
            StoreOptions::default(),
        )
        .map_err(display);
    }
    // The source's schema moved while nothing was streaming - a migration
    // during downtime, or DDL whose binlog was purged before it replayed.
    // The history's shape can no longer read the source (its SELECT dies on
    // the source's own "Unknown column"), and since every retry read the
    // same stale history, the copy stayed impossible until someone deleted
    // the mirror. The probe in hand describes the source as it IS, so adopt
    // it as the next schema version and let the copy rewrite every row.
    let mut previous = source.clone();
    previous.columns = stored;
    let adopted = pintail_probe::stabilize_source_table(&previous, source.clone())?;
    let version = record
        .version
        .checked_add(1)
        .ok_or_else(|| "table schema version exceeds UInt32".to_owned())?;
    let mut store = TableStore::open(
        directory,
        previous
            .table_schema_with_version(record.version)
            .map_err(display)?,
        StoreOptions::default(),
    )
    .map_err(display)?;
    store
        .evolve_schema(
            adopted
                .table_schema_with_version(version)
                .map_err(display)?,
        )
        .map_err(display)?;
    let columns_json = serde_json::to_string(&adopted.columns).map_err(display)?;
    metadata
        .record_schema_history(
            database_id,
            &source.name,
            version,
            None,
            &columns_json,
            &Utc::now().to_rfc3339(),
        )
        .map_err(display)?;
    source.columns = adopted.columns;
    Ok(store)
}

/// Same table shape, ignoring the stable IDs the probe cannot know.
fn columns_equivalent(
    stored: &[pintail_probe::SourceColumn],
    fresh: &[pintail_probe::SourceColumn],
) -> bool {
    stored.len() == fresh.len()
        && stored.iter().zip(fresh).all(|(left, right)| {
            left.name.eq_ignore_ascii_case(&right.name)
                && left.mysql_column_type == right.mysql_column_type
                && left.pintail_type == right.pintail_type
                && left.nullable == right.nullable
        })
}

pub(crate) fn table_directory(root: &FsPath, table: &str) -> PathBuf {
    pintail_store::table_directory(root, table)
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn display(error: impl std::fmt::Display) -> String {
    // Alternate form: an `anyhow` chain prints every cause, so "failed to
    // create private metadata database" arrives with the OS error behind it.
    format!("{error:#}")
}
