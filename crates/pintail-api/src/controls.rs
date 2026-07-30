use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use mysql_async::{Opts, Pool};
use pintail_meta::DatabaseRecord;
use pintail_poll::{PollOptions, PollTarget, run_cdc_reconciliation, run_poll_cycle};
use pintail_probe::ProbeReport;
use pintail_store::{StoreOptions, TableStore};
use serde::Serialize;

use crate::{
    ApiState,
    auth::AuthPrincipal,
    error::ApiError,
    events::ApiEvent,
    snapshot::{self, AcceptedSnapshot},
};

#[derive(Serialize)]
pub(crate) struct AcceptedReconcile {
    run_id: String,
    state: &'static str,
    table: String,
}

/// Starts a safe full-database resnapshot from a table action.
///
/// Snapshot handoff checkpoints belong to the database, not an individual
/// table. Reusing the database-wide snapshot path prevents older binlog events
/// from overwriting a freshly captured table.
pub(crate) async fn resync(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, table_name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<AcceptedSnapshot>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    require_table(&state, &database_id, &table_name)?;
    snapshot::start_forced(Extension(principal), State(state), Path(database_id)).await
}

pub(crate) async fn reconcile(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, table_name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<AcceptedReconcile>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    require_table(&state, &database_id, &table_name)?;
    state.acquire_job(&database_id)?;

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
    let store = TableStore::open(
        directory,
        source.table_schema().map_err(display)?,
        StoreOptions::default(),
    )
    .map_err(display)?;
    let target = PollTarget::new(source, store).map_err(display)?;
    let metadata_path = state.metadata_path().map_err(display)?.to_path_buf();
    drop(metadata);

    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = Opts::from_url(&dsn).map_err(display)?;
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
