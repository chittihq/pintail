use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use mysql_async::Pool;
use pintail_meta::DatabaseRecord;
use pintail_poll::{PollOptions, PollTarget, run_cdc_reconciliation, run_poll_cycle};
use pintail_probe::ProbeReport;
use pintail_snapshot::{SnapshotOptions, SnapshotPosition, SnapshotTarget};
use serde::Serialize;

use crate::{ApiState, audit, auth::AuthPrincipal, error::ApiError, events::ApiEvent, snapshot};

#[derive(Serialize)]
pub(crate) struct AcceptedReconcile {
    run_id: String,
    state: &'static str,
    table: String,
}

/// Recopies one table from the source, leaving the rest of the database
/// replicating.
///
/// This used to resnapshot the whole database, on the grounds that a snapshot
/// handoff checkpoint belongs to the database rather than a table - true, but
/// it is not the only way to stop older binlog events overwriting freshly
/// copied rows. The stream already skips events at or before a per-table
/// snapshot fence, which is how a table auto-included mid-stream is made safe,
/// and recording that fence here buys the same protection for one table
/// without stopping the other tables' replication or re-copying them.
///
/// The fence must come from the position captured under THIS snapshot's read
/// lock, not the database's preserved handoff position, which sits behind the
/// data actually copied.
pub(crate) async fn resync(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, table_name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<AcceptedReconcile>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    crate::databases::load_database(&state, &principal, &database_id)?;
    require_table(&state, &database_id, &table_name)?;
    state.acquire_job_as(&database_id, "a table resnapshot")?;

    let run_id = crate::state::random_identifier("run_", 16);
    let metadata = match state.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            state.release_job(&database_id);
            return Err(ApiError::internal(error));
        }
    };
    let started = metadata
        .start_sync_run(
            &run_id,
            &database_id,
            Some(&table_name),
            "resnapshot",
            &Utc::now().to_rfc3339(),
        )
        .and_then(|()| metadata.begin_table_resnapshot(&database_id, &table_name));
    drop(metadata);
    if let Err(error) = started {
        state.release_job(&database_id);
        return Err(ApiError::internal(error));
    }

    let job_state = state.clone();
    let job_database_id = database_id.clone();
    let job_table_name = table_name.clone();
    let job_run_id = run_id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("pintail-resnapshot-{database_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(complete_table_resnapshot_job(
                    job_state,
                    job_database_id,
                    job_table_name,
                    job_run_id,
                )),
                Err(error) => finish_table_resnapshot(
                    &job_state,
                    &job_database_id,
                    &job_table_name,
                    &job_run_id,
                    Err(error.to_string()),
                    0,
                ),
            }
        })
    {
        let message = format!("could not start resnapshot worker: {error}");
        finish_table_resnapshot(
            &state,
            &database_id,
            &table_name,
            &run_id,
            Err(message.clone()),
            0,
        );
        return Err(ApiError::unavailable(message));
    }

    audit::record(
        &state,
        &principal,
        "resnapshot.table",
        Some(("database", &database_id)),
        Some(serde_json::json!({"table": table_name.clone()})),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedReconcile {
            run_id,
            state: "snapshotting",
            table: table_name,
        }),
    ))
}

async fn complete_table_resnapshot_job(
    state: ApiState,
    database_id: String,
    table_name: String,
    run_id: String,
) {
    let started = Instant::now();
    let result = run_table_resnapshot_job(&state, &database_id, &table_name).await;
    finish_table_resnapshot(
        &state,
        &database_id,
        &table_name,
        &run_id,
        result,
        duration_ms(started),
    );
}

#[allow(clippy::too_many_lines)] // linear copy-and-restore sequence
async fn run_table_resnapshot_job(
    state: &ApiState,
    database_id: &str,
    table_name: &str,
) -> Result<u64, String> {
    let metadata = state.metadata().map_err(display)?;
    let database = metadata
        .database(database_id)
        .map_err(display)?
        .ok_or_else(|| "database does not exist".to_owned())?;
    if database.mode == "paused" {
        return Err("resume the database before resnapshotting a table".to_owned());
    }
    let report = decode_probe(&database)?;
    let mut source = report
        .tables
        .iter()
        .find(|source| source.name.eq_ignore_ascii_case(table_name))
        .cloned()
        .ok_or_else(|| format!("table {table_name} is absent from the latest probe"))?;
    let directory = snapshot::table_directory(
        &state
            .data_dir()
            .map_err(display)?
            .join("databases")
            .join(database_id)
            .join("tables"),
        &source.name,
    );
    // The snapshot-to-stream handoff checkpoint belongs to the database, and
    // every other table's stream starts from it. A snapshot run owns that
    // checkpoint - it is written for the database being copied wholesale - so
    // copying one table mid-stream must put back exactly what it found, or the
    // tables this operation was supposed to leave alone fail to start with
    // "polling checkpoint cannot start CDC". Measured: the control-plane gate
    // passes at 138 checks without this and fails four with it.
    let preserved_checkpoint = metadata.snapshot_checkpoint(database_id).map_err(display)?;
    let mut store = snapshot::open_tracked_store(&metadata, database_id, &mut source, directory)?;
    // Drop what is there before recopying, so the snapshot is the table rather
    // than the table merged onto its own stale rows.
    store.reset_for_resnapshot().map_err(display)?;
    let target = SnapshotTarget::new(source, store).map_err(display)?;
    let metadata_path = state.metadata_path().map_err(display)?.to_path_buf();
    drop(metadata);

    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = crate::dsn::source_opts(&dsn)?;
    let pool = Pool::new(options);
    // Progress is published exactly as the full-database snapshot publishes
    // it: without this, a large table sat on a motionless 'snapshotting'
    // badge for minutes and the resnapshot read as unresponsive.
    let progress_state = state.clone();
    let progress_database_id = database_id.to_owned();
    let snapshot = pintail_snapshot::run_snapshot_with_progress(
        &pool,
        &metadata_path,
        database_id,
        &report,
        vec![target],
        SnapshotOptions::default(),
        move |progress| {
            progress_state.publish(crate::snapshot::resnapshot_progress_event(
                &progress_database_id,
                progress,
            ));
        },
    )
    .await
    .map_err(display);
    pool.disconnect().await.map_err(display)?;
    let snapshot = snapshot?;

    let rows = snapshot
        .tables
        .iter()
        .map(|outcome| outcome.rows)
        .sum::<u64>();
    let fence = match &snapshot.captured_position {
        SnapshotPosition::Gtid {
            file: Some(file),
            position: Some(position),
            ..
        }
        | SnapshotPosition::FilePosition { file, position } => Some((file.clone(), *position)),
        SnapshotPosition::Gtid { .. } | SnapshotPosition::Unavailable => None,
    };
    let metadata = state.metadata().map_err(display)?;
    if let Some(checkpoint) = preserved_checkpoint
        && metadata.snapshot_checkpoint(database_id).map_err(display)? != Some(checkpoint.clone())
        // Only gtid and filepos can be restored; a 'polling' checkpoint is not
        // one a CDC stream starts from anyway, so there is nothing to put back.
        && matches!(checkpoint.kind.as_str(), "gtid" | "filepos")
    {
        metadata
            .upsert_snapshot_checkpoint(
                database_id,
                &checkpoint.kind,
                checkpoint.gtid_set.as_deref(),
                checkpoint.binlog_file.as_deref(),
                checkpoint.binlog_pos,
                &Utc::now().to_rfc3339(),
            )
            .map_err(display)?;
    }
    match fence {
        Some((file, position)) => metadata
            .set_setting(
                &pintail_cdc::snapshot_fence_key(database_id, &table_name.to_ascii_lowercase()),
                &format!("{file}:{position}"),
            )
            .map_err(display)?,
        // Without a fence the stream would replay events this snapshot has
        // already copied. Deletes and updates would land again harmlessly on a
        // keyed table, but an append-keyed one would duplicate, so refuse
        // rather than leave that to chance.
        None => {
            return Err(
                "source did not report a binlog position for the snapshot, so the table \
                 cannot be fenced against replaying its own rows"
                    .to_owned(),
            );
        }
    }
    Ok(rows)
}

fn finish_table_resnapshot(
    state: &ApiState,
    database_id: &str,
    table_name: &str,
    run_id: &str,
    result: Result<u64, String>,
    elapsed_ms: u64,
) {
    match result {
        Ok(rows) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(run_id, "completed", rows, 0, elapsed_ms, None);
                let state_name = metadata
                    .database(database_id)
                    .ok()
                    .flatten()
                    .and_then(|database| database.effective_mode)
                    .map_or("streaming", |mode| {
                        if mode == "polling" {
                            "polling"
                        } else {
                            "streaming"
                        }
                    });
                // Not swallowed: if the table never leaves 'needs_resync'
                // the copy might as well not have happened, and the
                // operator watching the row is entitled to know the
                // difference between a resnapshot that failed and one that
                // succeeded without clearing the flag.
                if let Err(failure) =
                    metadata.finish_table_resnapshot(database_id, table_name, state_name)
                {
                    state.publish(ApiEvent {
                        kind: "resnapshot.error".to_owned(),
                        database_id: Some(database_id.to_owned()),
                        table: Some(table_name.to_owned()),
                        message: format!("{table_name} recopied but stayed flagged: {failure}"),
                        rows: Some(rows),
                        bytes: Some(0),
                        eta_seconds: None,
                        at: Utc::now().to_rfc3339(),
                    });
                }
            }
            state.publish(ApiEvent {
                kind: "resnapshot.completed".to_owned(),
                database_id: Some(database_id.to_owned()),
                table: Some(table_name.to_owned()),
                message: format!("{table_name} resnapshot completed"),
                rows: Some(rows),
                bytes: Some(0),
                eta_seconds: None,
                at: Utc::now().to_rfc3339(),
            });
        }
        Err(error) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(run_id, "error", 0, 0, elapsed_ms, Some(&error));
                // Back to needs_resync, not to streaming: the store was
                // emptied before the copy, so a failed resnapshot leaves the
                // table incomplete and it must not be read as current.
                let _ = metadata.mark_table_needs_resync(database_id, table_name, &error);
            }
            state.publish(ApiEvent {
                kind: "resnapshot.error".to_owned(),
                database_id: Some(database_id.to_owned()),
                table: Some(table_name.to_owned()),
                message: error,
                rows: None,
                bytes: None,
                eta_seconds: None,
                at: Utc::now().to_rfc3339(),
            });
        }
    }
    state.release_job(database_id);
}

pub(crate) async fn reconcile(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, table_name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<AcceptedReconcile>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    crate::databases::load_database(&state, &principal, &database_id)?;
    require_table(&state, &database_id, &table_name)?;
    state.acquire_job_as(&database_id, "a table reconciliation")?;

    let run_id = crate::state::random_identifier("run_", 16);
    if let Err(error) = state.metadata()?.start_sync_run(
        &run_id,
        &database_id,
        Some(&table_name),
        "reconcile",
        &Utc::now().to_rfc3339(),
    ) {
        state.release_job(&database_id);
        return Err(ApiError::internal(error));
    }

    let job_state = state.clone();
    let job_database_id = database_id.clone();
    let job_table_name = table_name.clone();
    let job_run_id = run_id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("pintail-reconcile-{database_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(complete_reconcile_job(
                    job_state,
                    job_database_id,
                    job_table_name,
                    job_run_id,
                )),
                Err(error) => finish_reconcile(
                    &job_state,
                    &job_database_id,
                    &job_table_name,
                    &job_run_id,
                    Err(error.to_string()),
                    0,
                ),
            }
        })
    {
        let message = format!("could not start reconcile worker: {error}");
        finish_reconcile(
            &state,
            &database_id,
            &table_name,
            &run_id,
            Err(message.clone()),
            0,
        );
        return Err(ApiError::unavailable(message));
    }

    audit::record(
        &state,
        &principal,
        "reconcile.start",
        Some(("database", &database_id)),
        Some(serde_json::json!({"table": table_name.clone()})),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedReconcile {
            run_id,
            state: "reconciling",
            table: table_name,
        }),
    ))
}

async fn complete_reconcile_job(
    state: ApiState,
    database_id: String,
    table_name: String,
    run_id: String,
) {
    let started = Instant::now();
    let result = run_reconcile_job(&state, &database_id, &table_name).await;
    finish_reconcile(
        &state,
        &database_id,
        &table_name,
        &run_id,
        result,
        duration_ms(started),
    );
}

pub(crate) async fn run_reconcile_job(
    state: &ApiState,
    database_id: &str,
    table_name: &str,
) -> Result<u64, String> {
    let metadata = state.metadata().map_err(display)?;
    let database = metadata
        .database(database_id)
        .map_err(display)?
        .ok_or_else(|| "database does not exist".to_owned())?;
    if database.mode == "paused" {
        return Err("resume the database before reconciling a table".to_owned());
    }
    let report = decode_probe(&database)?;
    let source = report
        .tables
        .iter()
        .find(|source| source.name.eq_ignore_ascii_case(table_name))
        .cloned()
        .ok_or_else(|| format!("table {table_name} is absent from the latest probe"))?;
    let directory = snapshot::table_directory(
        &state
            .data_dir()
            .map_err(display)?
            .join("databases")
            .join(database_id)
            .join("tables"),
        &source.name,
    );
    let mut source = source;
    let store = snapshot::open_tracked_store(&metadata, database_id, &mut source, directory)?;
    let target = PollTarget::new(source, store).map_err(display)?;
    let metadata_path = state.metadata_path().map_err(display)?.to_path_buf();
    drop(metadata);

    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = crate::dsn::source_opts(&dsn)?;
    let pool = Pool::new(options);
    let mode = effective_mode(&database);
    let rows = match mode {
        "cdc" => run_cdc_reconciliation(
            &pool,
            &metadata_path,
            database_id,
            &report,
            vec![target],
            10_000,
        )
        .await
        .map_err(display)?
        .tables
        .into_iter()
        .map(|outcome| outcome.ingested + outcome.tombstones)
        .sum::<usize>(),
        "polling" => run_poll_cycle(
            &pool,
            &metadata_path,
            database_id,
            &report,
            vec![target],
            PollOptions {
                force: true,
                reconcile: true,
                ..PollOptions::default()
            },
        )
        .await
        .map_err(display)?
        .tables
        .into_iter()
        .map(|outcome| outcome.ingested + outcome.tombstones)
        .sum::<usize>(),
        _ => return Err("database has no active replication mode".to_owned()),
    };
    pool.disconnect().await.map_err(display)?;
    u64::try_from(rows).map_err(display)
}

fn finish_reconcile(
    state: &ApiState,
    database_id: &str,
    table_name: &str,
    run_id: &str,
    result: Result<u64, String>,
    elapsed_ms: u64,
) {
    match result {
        Ok(rows) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(run_id, "completed", rows, 0, elapsed_ms, None);
            }
            state.publish(ApiEvent {
                kind: "reconcile.completed".to_owned(),
                database_id: Some(database_id.to_owned()),
                table: Some(table_name.to_owned()),
                message: format!("{table_name} reconciliation completed"),
                rows: Some(rows),
                bytes: Some(0),
                eta_seconds: None,
                at: Utc::now().to_rfc3339(),
            });
        }
        Err(error) => {
            if let Ok(metadata) = state.metadata() {
                let _ = metadata.finish_sync_run(run_id, "error", 0, 0, elapsed_ms, Some(&error));
            }
            state.publish(ApiEvent {
                kind: "reconcile.error".to_owned(),
                database_id: Some(database_id.to_owned()),
                table: Some(table_name.to_owned()),
                message: error,
                rows: None,
                bytes: None,
                eta_seconds: None,
                at: Utc::now().to_rfc3339(),
            });
        }
    }
    state.release_job(database_id);
}

fn require_table(state: &ApiState, database_id: &str, table_name: &str) -> Result<(), ApiError> {
    let metadata = state.metadata()?;
    if metadata
        .database(database_id)
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found("database does not exist"));
    }
    if metadata
        .tables(database_id)
        .map_err(ApiError::internal)?
        .iter()
        .any(|table| table.name.eq_ignore_ascii_case(table_name))
    {
        Ok(())
    } else {
        Err(ApiError::not_found("table does not exist"))
    }
}

fn decode_probe(database: &DatabaseRecord) -> Result<ProbeReport, String> {
    serde_json::from_str(
        database
            .probe_json
            .as_deref()
            .ok_or_else(|| "probe the database before reconciling a table".to_owned())?,
    )
    .map_err(display)
}

fn effective_mode(database: &DatabaseRecord) -> &str {
    database
        .effective_mode
        .as_deref()
        .unwrap_or(database.mode.as_str())
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
