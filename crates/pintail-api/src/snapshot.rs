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

use crate::{
    ApiState, audit,
    auth::AuthPrincipal,
    error::ApiError,
    events::ApiEvent,
    state::{JobSignal, Preempt},
};

/// How often the supervisor retries the fresh copy a reset is waiting on.
const RESET_RETRY_GAP: std::time::Duration = std::time::Duration::from_secs(15);

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
    /// What holds the database's job slot right now, so a page can say why
    /// an action is waiting instead of showing a spinner.
    job: Option<RunningJob>,
    tables: Vec<TableSnapshotStatus>,
}

#[derive(Serialize)]
struct RunningJob {
    claim: String,
    seconds: u64,
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
    let run_id = begin_operator_snapshot(&state, &database_id, force).await?;
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
///
/// It has to work on exactly the databases where nothing else does, so it
/// waits for nothing: whatever holds the job slot - a cycle that never
/// ends, a copy against a source that stopped answering - is cancelled,
/// and the slot is kept from the wipe through to the snapshot that follows,
/// so nothing can start on the emptied mirror in between. The intent is
/// recorded with the wipe; if the snapshot cannot start or the process
/// stops, the supervisor finishes the reset.
pub(crate) async fn reset(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<(StatusCode, Json<AcceptedSnapshot>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    let database = crate::databases::load_database(&state, &principal, &database_id)?;
    state.require_replicated(&database_id, "a factory reset")?;
    // Refused before anything is interrupted or cleared.
    if database.mode == "paused" {
        return Err(ApiError::conflict(
            "resume the database before resetting it",
        ));
    }
    let signal = state
        .acquire_operator_job(&database_id, "a factory reset", Preempt::Cancel)
        .await?;
    if let Err(error) = wipe_mirror(&state, &database_id) {
        state.release_job(&database_id);
        return Err(error);
    }
    state.publish(ApiEvent::database(
        "database.reset",
        &database_id,
        "replication state cleared; a fresh snapshot follows",
    ));
    audit::record(
        &state,
        &principal,
        "database.reset",
        Some(("database", &database_id)),
        None,
    );
    state.relabel_job(&database_id, "a full snapshot");
    let run_id = start_snapshot_holding(&state, &database_id, true, signal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedSnapshot {
            run_id,
            state: "snapshotting",
        }),
    ))
}

/// Drops everything mirrored for one database: the control-plane rows, with
/// the pending-reset mark written in the same transaction, then the stores.
/// Safe to repeat, which is how an interrupted reset is finished.
fn wipe_mirror(state: &ApiState, database_id: &str) -> Result<(), ApiError> {
    let mut metadata = state.metadata()?;
    metadata
        .reset_database_replication(database_id, &Utc::now().to_rfc3339())
        .map_err(ApiError::internal)?;
    pintail_failpoint::hit("reset.after_metadata_wipe").map_err(ApiError::internal)?;
    remove_stores(state, database_id)
}

fn remove_stores(state: &ApiState, database_id: &str) -> Result<(), ApiError> {
    let tables_dir = state
        .data_dir()?
        .join("databases")
        .join(database_id)
        .join("tables");
    if tables_dir.exists() {
        std::fs::remove_dir_all(&tables_dir).map_err(ApiError::internal)?;
        pintail_store::publish_changes_under(&tables_dir);
    }
    Ok(())
}

/// Finishes a reset whose snapshot never completed: the source was
/// unreachable when it was asked for, the copy failed, or the process
/// stopped between the wipe and the copy. Until that snapshot lands the
/// database has no tables and no position, so nothing else can repair it.
/// Called by the supervisor on a gap; `false` when there was nothing to do.
pub(crate) fn resume_pending_reset(state: &ApiState, database: &DatabaseRecord) -> bool {
    let Ok(metadata) = state.metadata() else {
        return false;
    };
    if !metadata.reset_pending(&database.id).unwrap_or(false) {
        return false;
    }
    if database.mode == "paused" || database.kind == "local" {
        return true;
    }
    let due = state
        .with_catalog_repairs(|clock| {
            let key = format!("reset\u{0}{}", database.id);
            let due = clock
                .get(&key)
                .is_none_or(|last| last.elapsed() >= RESET_RETRY_GAP);
            if due {
                clock.insert(key, Instant::now());
            }
            due
        })
        .unwrap_or(false);
    if !due {
        return true;
    }
    let Ok(signal) = state.acquire_automatic_job(&database.id, "a full snapshot") else {
        return true;
    };
    // A stop between the control-plane wipe and the file removal leaves
    // stores nothing tracks; with no table recorded they are all leftovers.
    let untracked = metadata
        .tables(&database.id)
        .is_ok_and(|tables| tables.is_empty());
    drop(metadata);
    if untracked && let Err(error) = remove_stores(state, &database.id) {
        state.release_job(&database.id);
        state.publish(ApiEvent::database(
            "reset.resume_failed",
            &database.id,
            format!("the interrupted reset could not clear its stores: {error}"),
        ));
        return true;
    }
    match start_snapshot_holding(state, &database.id, true, signal) {
        Ok(run_id) => state.publish(ApiEvent::database(
            "reset.resumed",
            &database.id,
            format!("the reset's fresh copy is starting again as snapshot {run_id}"),
        )),
        Err(error) => state.publish(ApiEvent::database(
            "reset.resume_failed",
            &database.id,
            format!(
                "the reset's fresh copy could not start and retries in {}s: {error}",
                RESET_RETRY_GAP.as_secs()
            ),
        )),
    }
    true
}

/// Starts a snapshot for the supervisor: claims the job slot at once, and
/// not at all while an operator action waits for it.
pub(crate) fn begin_snapshot_job(
    state: &ApiState,
    database_id: &str,
    force: bool,
) -> Result<String, ApiError> {
    state.require_replicated(database_id, "a snapshot")?;
    let signal = state.acquire_automatic_job(database_id, "a full snapshot")?;
    start_snapshot_holding(state, database_id, force, signal)
}

/// Starts a snapshot an operator asked for, behind at most the replication
/// cycle that is running.
pub(crate) async fn begin_operator_snapshot(
    state: &ApiState,
    database_id: &str,
    force: bool,
) -> Result<String, ApiError> {
    state.require_replicated(database_id, "a snapshot")?;
    let signal = state
        .acquire_operator_job(database_id, "a full snapshot", Preempt::Yield)
        .await?;
    start_snapshot_holding(state, database_id, force, signal)
}

/// Journals a snapshot run and detaches the worker, on a job slot the
/// caller already holds. The slot is released here on every refusal and by
/// the worker when it ends.
fn start_snapshot_holding(
    state: &ApiState,
    database_id: &str,
    force: bool,
    signal: JobSignal,
) -> Result<String, ApiError> {
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
    drop(metadata);
    let job_state = state.clone();
    let job_database_id = database_id.to_owned();
    let job_run_id = run_id.clone();
    if let Err(error) =
        std::thread::Builder::new()
            .name(format!("pintail-snapshot-{database_id}"))
            .spawn(move || {
                let started = Instant::now();
                let outcome =
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => {
                            let outcome = runtime.block_on(signal.until_cancelled(
                                run_snapshot_job(&job_state, &job_database_id, &job_run_id, force),
                            ));
                            // Whatever the copy left running ends with its runtime,
                            // before the slot passes to the next job.
                            drop(runtime);
                            outcome
                        }
                        Err(error) => Some(Err(error.to_string())),
                    };
                finish_snapshot_job(
                    &job_state,
                    &job_database_id,
                    &job_run_id,
                    outcome,
                    duration_ms(started),
                );
            })
    {
        let message = format!("could not start snapshot worker: {error}");
        finish_snapshot_job(state, database_id, &run_id, Some(Err(message.clone())), 0);
        return Err(ApiError::unavailable(message));
    }
    Ok(run_id)
}

type SnapshotOutcome = Result<(u64, u64, &'static str, Vec<TableSnapshotFailure>), String>;

/// Journals how a snapshot ended and releases its job slot, once.
/// `None` is a snapshot whose slot was taken by a reset.
fn finish_snapshot_job(
    state: &ApiState,
    database_id: &str,
    run_id: &str,
    outcome: Option<SnapshotOutcome>,
    elapsed_ms: u64,
) {
    match outcome {
        Some(Ok((rows, bytes, mode, failed))) => {
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
                    run_id,
                    "completed",
                    rows,
                    bytes,
                    elapsed_ms,
                    partial.as_deref(),
                );
                // The fresh copy a reset promised has landed.
                let _ = metadata.finish_database_reset(database_id);
            }
            if let Some(partial) = &partial {
                state.publish(ApiEvent::database("snapshot.partial", database_id, partial));
            }
            state.publish(ApiEvent::database(
                "replication.ready",
                database_id,
                format!("{mode} handoff is ready"),
            ));
        }
        Some(Err(error)) => {
            if let Ok(metadata) = state.metadata() {
                let now = Utc::now().to_rfc3339();
                let _ = metadata.finish_sync_run(run_id, "error", 0, 0, elapsed_ms, Some(&error));
                let _ = metadata.fail_database_job(database_id, &error, &now);
            }
            state.publish(ApiEvent::database("replication.error", database_id, error));
        }
        None => {
            let reason = "cancelled: a reset took this database's job slot";
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(run_id, "error", 0, 0, elapsed_ms, Some(reason));
            }
            state.publish(ApiEvent::database(
                "snapshot.cancelled",
                database_id,
                reason,
            ));
        }
    }
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
    let job = state
        .job_holder(&database_id)
        .map(|(claim, seconds)| RunningJob { claim, seconds });
    Ok(Json(SnapshotStatus {
        database_id,
        state: database.state,
        effective_mode: database.effective_mode,
        job,
        tables,
    }))
}

/// Retains every tracked table a fresh probe no longer finds, the way a
/// DROP the stream reads retains it: kept, never streamed, removable by an
/// operator. A table renamed while the process was down - or while its copy
/// was cut short - has no DROP for the stream to read, and its old name
/// otherwise sat in `snapshotting` for good, since the snapshot copies only
/// what the probe lists.
fn retire_tables_the_source_dropped(
    metadata: &pintail_meta::MetaStore,
    database_id: &str,
    report: &ProbeReport,
) -> Result<(), String> {
    let now = Utc::now().to_rfc3339();
    for table in metadata.tables(database_id).map_err(display)? {
        let listed = report
            .tables
            .iter()
            .any(|source| source.name.eq_ignore_ascii_case(&table.name));
        if table.orphaned_at.is_none() && !listed {
            metadata
                .mark_table_orphaned(
                    database_id,
                    &table.name,
                    "the source no longer has this table",
                    &now,
                )
                .map_err(display)?;
        }
    }
    Ok(())
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
        retire_tables_the_source_dropped(&metadata, database_id, &refreshed)?;
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
    let handed_off = !force && has_handed_off(&metadata, &database).map_err(display)?;
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
    // A full repair replaces the affected rows too. Clear only dead letters
    // belonging to successful copies, before CDC resumes and can create new
    // ones. Failed copies must retain their diagnostic evidence.
    let metadata = state.metadata().map_err(display)?;
    for target in &result.targets {
        let name = &target.source().name;
        if !failed.iter().any(|failure| &failure.table == name) {
            metadata
                .clear_dlq_for_table(database_id, name)
                .map_err(display)?;
        }
    }
    drop(metadata);
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

/// Whether a database finished its first snapshot and handed off to
/// replication.
///
/// The stored checkpoint alone does not say so: the first snapshot persists
/// its position as soon as it captures it, so that a copy cut short by a
/// restart resumes against the same position. A database whose first copy
/// was interrupted therefore holds a checkpoint while still 'probed' - and
/// reading that as "already replicating" finished the resumed copy table by
/// table without ever handing off, leaving the database outside the
/// supervisor's schedule with every table reporting streaming and no change
/// ever applied. Only a lifecycle state past onboarding means the handoff
/// happened.
pub(crate) fn has_handed_off(
    metadata: &MetaStore,
    database: &DatabaseRecord,
) -> anyhow::Result<bool> {
    if matches!(
        database.state.as_str(),
        "created" | "probed" | "snapshotting"
    ) {
        return Ok(false);
    }
    Ok(metadata.snapshot_checkpoint(&database.id)?.is_some())
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
    let opened = match attempted {
        Err(message)
            if wipe_on_schema_mismatch
                && (message.contains("schema fingerprint mismatch")
                    || message.contains("schema version mismatch")
                    // The store's own refusal to re-read a segment under a
                    // changed column type.
                    || message.contains("changed physical type")
                    // A source table that gained or lost its key, or was
                    // replaced by one with other columns, under the same
                    // name: the store's layout cannot read the new shape at
                    // all, and a recopy was the only way forward.
                    || message.contains("key mode cannot change")
                    || message.contains("is absent from schema version")
                    // Every in-place refusal the probe makes, under one
                    // marker: adoptable here because the branch below deletes
                    // the store before rebuilding it.
                    || message.contains(pintail_probe::IN_PLACE_REFUSAL)) =>
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
            pintail_store::publish_changes_under(&directory);
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
    };
    // The copy about to run fills this shape; recording it as the first
    // generation keeps a later re-probe from redefining it (see
    // `freeze_first_generation`).
    if history.is_empty() && opened.is_ok() {
        pintail_cdc::freeze_first_generation(metadata, database_id, source).map_err(display)?;
    }
    opened
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
    pintail_cdc::evolve_tracked_schema(
        metadata,
        database_id,
        &source.name,
        &mut store,
        &adopted.columns,
        adopted
            .table_schema_with_version(version)
            .map_err(display)?,
        None,
    )
    .map_err(display)?
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

#[cfg(test)]
mod tests {
    use super::{fence_table_after_copy, has_handed_off, summarize_names};
    use pintail_meta::MetaStore;
    use pintail_snapshot::SnapshotPosition;

    fn fence(metadata: &MetaStore) -> Option<String> {
        metadata
            .setting(&pintail_cdc::snapshot_fence_key("db-1", "orders"))
            .expect("setting")
    }

    #[test]
    fn a_copied_table_is_fenced_at_the_position_its_copy_reflects() {
        let directory = tempfile::tempdir().expect("metadata directory");
        let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).expect("open");
        fence_table_after_copy(
            &metadata,
            "db-1",
            "Orders",
            &SnapshotPosition::FilePosition {
                file: "mysql-bin.000012".to_owned(),
                position: 4_096,
            },
            true,
        )
        .expect("fenced");
        assert_eq!(fence(&metadata).as_deref(), Some("mysql-bin.000012:4096"));
        fence_table_after_copy(
            &metadata,
            "db-1",
            "orders",
            &SnapshotPosition::Gtid {
                set: "aaaa:1-9".to_owned(),
                file: Some("mysql-bin.000013".to_owned()),
                position: Some(8),
            },
            true,
        )
        .expect("fenced again");
        assert_eq!(fence(&metadata).as_deref(), Some("mysql-bin.000013:8"));
    }

    #[test]
    fn a_cdc_copy_without_a_position_is_refused_and_a_polling_copy_is_not() {
        let directory = tempfile::tempdir().expect("metadata directory");
        let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).expect("open");
        let unplaced = SnapshotPosition::Gtid {
            set: "aaaa:1-9".to_owned(),
            file: None,
            position: None,
        };
        let refused = fence_table_after_copy(&metadata, "db-1", "orders", &unplaced, true)
            .expect_err("a CDC table cannot be fenced without a position");
        assert!(refused.contains("cannot be fenced"), "{refused}");
        fence_table_after_copy(
            &metadata,
            "db-1",
            "orders",
            &SnapshotPosition::Unavailable,
            false,
        )
        .expect("a polling database has no stream to fence against");
        assert_eq!(fence(&metadata), None);
    }

    #[test]
    fn an_interrupted_first_copy_has_not_handed_off() {
        let directory = tempfile::tempdir().expect("metadata directory");
        let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).expect("open");
        metadata
            .upsert_database("db-1", "app", b"mysql://source", "2026-07-30T00:00:00Z")
            .expect("register database");
        metadata
            .update_database_probe("db-1", "{}", "cdc", "2026-07-30T00:00:01Z")
            .expect("probe");
        let database = |metadata: &MetaStore| {
            metadata
                .database("db-1")
                .expect("read database")
                .expect("database")
        };
        // The first copy persists its position before any table is done.
        metadata
            .insert_snapshot_checkpoint_if_absent(
                "db-1",
                "filepos",
                None,
                Some("mysql-bin.000001"),
                Some(4),
                "2026-07-30T00:00:02Z",
            )
            .expect("checkpoint");
        assert!(
            !has_handed_off(&metadata, &database(&metadata)).expect("judged"),
            "a checkpoint captured by an unfinished first copy is not a handoff"
        );
        metadata
            .set_database_replication_state("db-1", "cdc", "2026-07-30T00:00:03Z")
            .expect("handoff");
        assert!(has_handed_off(&metadata, &database(&metadata)).expect("judged"));
    }

    #[test]
    fn long_name_lists_are_summarized() {
        assert_eq!(summarize_names(["a", "b"].into_iter()), "a, b");
        let many = (0..15).map(|index| format!("t{index}")).collect::<Vec<_>>();
        assert_eq!(
            summarize_names(many.iter().map(String::as_str)),
            "t0, t1, t2, t3, t4, t5, t6, t7, t8, t9, t10, t11 and 3 more"
        );
    }
}

/// The job-slot behaviour behind Reset mirror, Resnapshot and adding
/// tables: none of them may lose the slot to the supervisor for good.
#[cfg(test)]
mod operator_admission_tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        time::{Duration, Instant},
    };

    use axum::http::StatusCode;

    use super::resume_pending_reset;
    use crate::{ApiState, state::Preempt, test_support::Node};

    /// Tracks one table with a file in its store, and returns the store.
    fn seed(node: &Node, database_id: &str) -> PathBuf {
        node.metadata()
            .upsert_snapshot_table(database_id, "orders", None, None)
            .expect("tracked table");
        let store = node
            .state
            .data_dir()
            .expect("data directory")
            .join("databases")
            .join(database_id)
            .join("tables")
            .join("orders");
        std::fs::create_dir_all(&store).expect("store directory");
        std::fs::write(store.join("part"), b"rows").expect("store file");
        store
    }

    /// A supervisor whose cycles follow one another with no gap: each takes
    /// the slot the moment the last one lets go.
    struct TightCycles {
        done: Arc<AtomicBool>,
        cycles: Arc<AtomicU32>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TightCycles {
        fn start(state: &ApiState, database_id: &str) -> Self {
            let done = Arc::new(AtomicBool::new(false));
            let cycles = Arc::new(AtomicU32::new(0));
            let (state, id) = (state.clone(), database_id.to_owned());
            let (thread_done, thread_cycles) = (Arc::clone(&done), Arc::clone(&cycles));
            let thread = std::thread::spawn(move || {
                while !thread_done.load(Ordering::Acquire) {
                    if state.acquire_job(&id).is_ok() {
                        thread_cycles.fetch_add(1, Ordering::AcqRel);
                        std::thread::sleep(Duration::from_millis(40));
                        state.release_job(&id);
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
            let cycles_seen = Arc::clone(&cycles);
            let started = Instant::now();
            while cycles_seen.load(Ordering::Acquire) < 3 {
                assert!(started.elapsed() < Duration::from_secs(10), "no cycle ran");
                std::thread::sleep(Duration::from_millis(5));
            }
            Self {
                done,
                cycles,
                thread: Some(thread),
            }
        }
    }

    impl Drop for TightCycles {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Holds the slot as a job that never finishes by itself and only lets
    /// go when it is cancelled: a cycle on a source that stopped answering,
    /// or a copy with hours left.
    fn hold_until_cancelled(state: &ApiState, database_id: &str, operator: bool) {
        let (state, id) = (state.clone(), database_id.to_owned());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let signal = if operator {
                runtime
                    .block_on(state.acquire_operator_job(&id, "a full snapshot", Preempt::Yield))
                    .expect("operator claim")
            } else {
                state.acquire_job(&id).expect("cycle claim")
            };
            ready_tx.send(()).expect("ready");
            runtime.block_on(signal.until_cancelled(std::future::pending::<()>()));
            state.release_job(&id);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the holder claimed the slot");
    }

    async fn wait_until_not_snapshotting(state: &ApiState, database_id: &str) {
        let started = Instant::now();
        while state
            .job_holder(database_id)
            .is_some_and(|(claim, _)| claim == "a full snapshot")
        {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the snapshot against an unreachable source never ended"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reset_and_resnapshot_are_admitted_while_cycles_retake_the_slot() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        let store = seed(&node, &id);
        let cycles = TightCycles::start(&node.state, &id);

        let asked = Instant::now();
        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/snapshot"),
                Some(&node.admin),
                Some(r#"{"force":true}"#),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(
            asked.elapsed() < Duration::from_secs(3),
            "the resnapshot waited {:?} behind 40ms cycles",
            asked.elapsed()
        );
        wait_until_not_snapshotting(&node.state, &id).await;

        let asked = Instant::now();
        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/reset"),
                Some(&node.admin),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(
            asked.elapsed() < Duration::from_secs(3),
            "the reset waited {:?} behind 40ms cycles",
            asked.elapsed()
        );
        assert!(node.metadata().tables(&id).expect("tables").is_empty());
        assert!(!store.exists(), "the store outlived the reset");
        assert!(cycles.cycles.load(Ordering::Acquire) >= 3);
    }

    #[tokio::test]
    async fn reset_cancels_a_cycle_that_never_ends() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        seed(&node, &id);
        hold_until_cancelled(&node.state, &id, false);

        let asked = Instant::now();
        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/reset"),
                Some(&node.admin),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "{:?}",
            asked.elapsed()
        );
        assert!(node.metadata().tables(&id).expect("tables").is_empty());
    }

    #[tokio::test]
    async fn a_running_snapshot_refuses_a_second_one_by_name_and_yields_to_a_reset() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        seed(&node, &id);
        hold_until_cancelled(&node.state, &id, true);

        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/snapshot"),
                Some(&node.admin),
                Some(r#"{"force":true}"#),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let refusal = body["error"].as_str().expect("refusal");
        assert!(
            refusal.starts_with("a full snapshot has been running for")
                && refusal.contains("job slot"),
            "{refusal}"
        );
        assert_eq!(node.metadata().tables(&id).expect("tables").len(), 1);

        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/reset"),
                Some(&node.admin),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(node.metadata().tables(&id).expect("tables").is_empty());
    }

    #[tokio::test]
    async fn a_reset_that_cannot_copy_stays_owed_and_is_resumed() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        seed(&node, &id);
        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/reset"),
                Some(&node.admin),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        // The source in the stored connection does not exist, so the copy
        // fails; the reset must not be forgotten with it.
        wait_until_not_snapshotting(&node.state, &id).await;
        assert!(node.metadata().reset_pending(&id).expect("mark"));
        let database = node.metadata().database(&id).expect("read").expect("row");
        assert!(resume_pending_reset(&node.state, &database));
        wait_until_not_snapshotting(&node.state, &id).await;
        assert!(node.metadata().reset_pending(&id).expect("mark"));
    }

    #[tokio::test]
    async fn a_reset_stopped_after_its_control_plane_wipe_is_finished_by_the_supervisor() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        let store = seed(&node, &id);
        // What a process killed between the two halves of the wipe leaves.
        node.metadata()
            .reset_database_replication(&id, "2026-01-01T00:00:00Z")
            .expect("control-plane wipe");
        assert!(store.exists());

        let database = node.metadata().database(&id).expect("read").expect("row");
        assert!(resume_pending_reset(&node.state, &database));
        assert!(!store.exists(), "the untracked store survived the resume");
        wait_until_not_snapshotting(&node.state, &id).await;
        // A database with nothing owed is left to the ordinary cycle.
        node.metadata().finish_database_reset(&id).expect("clear");
        assert!(!resume_pending_reset(&node.state, &database));
    }

    #[tokio::test]
    async fn reset_refusals_change_nothing() {
        let node = Node::new().await;
        let id = node.database(&node.admin, "shop").await;
        let store = seed(&node, &id);
        let path = format!("/api/databases/{id}/reset");
        let intact = |why: &str| {
            assert_eq!(
                node.metadata().tables(&id).expect("tables").len(),
                1,
                "{why}"
            );
            assert!(store.exists(), "{why}");
            assert!(!node.metadata().reset_pending(&id).expect("mark"), "{why}");
            assert!(node.state.job_holder(&id).is_none(), "{why}");
        };

        let (status, _) = node.call("POST", &path, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        intact("anonymous");

        let (_, viewer) = node.member(&node.first_workspace, "viewer");
        let (status, _) = node.call("POST", &path, Some(&viewer), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        intact("viewer");

        // Another workspace's administrator must not learn the database
        // exists, let alone clear it.
        let (_, outsider) = node.workspace(&node.admin, "Second").await;
        let (status, body) = node.call("POST", &path, Some(&outsider), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        intact("another workspace");

        let (_, secret) = node.api_key(&node.admin, &id).await;
        let (status, _) = node.call("POST", &path, Some(&secret), None).await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
            "{status}"
        );
        intact("API key");

        let (status, body) = node
            .call(
                "POST",
                &format!("/api/databases/{id}/mode"),
                Some(&node.admin),
                Some(r#"{"mode":"paused"}"#),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = node.call("POST", &path, Some(&node.admin), None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "resume the database before resetting it");
        intact("paused");
    }
}
