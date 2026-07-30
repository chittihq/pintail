use std::{collections::BTreeSet, time::Duration};

use chrono::{DateTime, Utc};
use mysql_async::{Opts, Pool};
use pintail_cdc::{CdcOptions, CdcTarget, run_cdc};
use pintail_meta::{DatabaseRecord, TableRecord};
use pintail_poll::{PollOptions, PollTarget, run_poll_cycle};
use pintail_probe::ProbeReport;
use pintail_store::StoreOptions;

use crate::{
    ApiState, backup::start_scheduled_if_due, events::ApiEvent, snapshot::table_directory,
};

const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(5);

/// Starts Pintail's per-database finite replication supervisor.
///
/// Every eligible database receives an independent task on each cadence. A
/// source or storage failure is contained to that database and retried on a
/// later cadence.
#[must_use]
pub fn spawn(
    state: ApiState,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SUPERVISOR_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => supervise_once(&state),
                _ = shutdown.recv() => break,
            }
        }
    })
}

fn supervise_once(state: &ApiState) {
    let databases = match state.metadata().and_then(|metadata| {
        metadata
            .databases()
            .map_err(crate::error::ApiError::internal)
    }) {
        Ok(databases) => databases,
        Err(error) => {
            state.publish(ApiEvent::database(
                "supervisor.error",
                "control-plane",
                error.to_string(),
            ));
            return;
        }
    };
    for database in databases.into_iter().filter(eligible) {
        if state.acquire_job(&database.id).is_err() {
            continue;
        }
        let task_state = state.clone();
        let database_id = database.id.clone();
        let thread_database_id = database_id.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("pintail-supervisor-{database_id}"))
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(supervise_database(task_state, database));
                    }
                    Err(error) => {
                        task_state.record_replication_cycle(0, false);
                        task_state.publish(ApiEvent::database(
                            "supervisor.error",
                            &thread_database_id,
                            error.to_string(),
                        ));
                        task_state.release_job(&thread_database_id);
                    }
                }
            })
        {
            state.record_replication_cycle(0, false);
            state.publish(ApiEvent::database(
                "supervisor.error",
                &database_id,
                error.to_string(),
            ));
            state.release_job(&database_id);
        }
    }
}

fn eligible(database: &DatabaseRecord) -> bool {
    database.mode != "paused"
        && database.probe_json.is_some()
        && database
            .effective_mode
            .as_deref()
            .is_some_and(|mode| matches!(mode, "cdc" | "polling"))
        && matches!(database.state.as_str(), "streaming" | "polling" | "error")
}

async fn supervise_database(state: ApiState, database: DatabaseRecord) {
    let run_id = crate::state::random_identifier("run_", 16);
    let started = std::time::Instant::now();
    let kind = database.effective_mode.as_deref().unwrap_or("replication");
    if let Ok(metadata) = state.metadata() {
        let _ =
            metadata.start_sync_run(&run_id, &database.id, None, kind, &Utc::now().to_rfc3339());
    }
    let result = run_cycle(&state, &database).await;
    match result {
        Ok(rows) => {
            if let Ok(metadata) = state.metadata() {
                let now = Utc::now().to_rfc3339();
                let _ = metadata.finish_sync_run(
                    &run_id,
                    "completed",
                    rows,
                    0,
                    elapsed_ms(started),
                    None,
                );
                let _ = metadata.set_database_replication_state(&database.id, kind, &now);
            }
            state.record_replication_cycle(rows, true);
            if rows > 0 {
                state.publish(ApiEvent::database(
                    "replication.progress",
                    &database.id,
                    format!("{kind} cycle applied {rows} rows or tombstones"),
                ));
            }
        }
        Err(error) => {
            if let Ok(metadata) = state.metadata() {
                let now = Utc::now().to_rfc3339();
                let _ = metadata.finish_sync_run(
                    &run_id,
                    "error",
                    0,
                    0,
                    elapsed_ms(started),
                    Some(&error),
                );
                let _ = metadata.fail_database_job(&database.id, &error, &now);
            }
            state.record_replication_cycle(0, false);
            state.publish(ApiEvent::database("replication.error", &database.id, error));
        }
    }
    state.release_job(&database.id);
    if let Err(error) = start_scheduled_if_due(&state, &database.id) {
        state.publish(ApiEvent::database(
            "backup.schedule.error",
            &database.id,
            error.to_string(),
        ));
    }
}

#[allow(clippy::too_many_lines)]
async fn run_cycle(state: &ApiState, database: &DatabaseRecord) -> Result<u64, String> {
    let report: ProbeReport = serde_json::from_str(
        database
            .probe_json
            .as_deref()
            .ok_or_else(|| "database has not been probed".to_owned())?,
    )
    .map_err(display)?;
    let metadata = state.metadata().map_err(display)?;
    let records = metadata.tables(&database.id).map_err(display)?;
    let metadata_path = state.metadata_path().map_err(display)?.to_path_buf();
    let root = state
        .data_dir()
        .map_err(display)?
        .join("databases")
        .join(&database.id)
        .join("tables");
    let targets = open_targets(&metadata_path, &database.id, &root, &report, &records)?;
    drop(metadata);

    let dsn = state
        .decrypt_dsn(&database.encrypted_dsn)
        .map_err(display)?;
    let options = Opts::from_url(&dsn).map_err(display)?;
    let pool = Pool::new(options);
    let result = match database.effective_mode.as_deref() {
        Some("cdc") => {
            let includes = decode_names(database.include_tables.as_deref())?;
            let excludes = decode_names(database.exclude_tables.as_deref())?;
            run_cdc(
                &pool,
                &metadata_path,
                &database.id,
                &report,
                targets,
                CdcOptions {
                    blocking: false,
                    new_table_root: Some(root),
                    new_table_includes: includes,
                    new_table_excludes: excludes,
                    ..CdcOptions::default()
                },
            )
            .await
            .map(|result| u64::try_from(result.mutations).unwrap_or(u64::MAX))
            .map_err(display)
        }
        Some("polling") => {
            let poll_targets = targets
                .into_iter()
                .map(|target| {
                    let source = target.source().clone();
                    PollTarget::new(source, target.into_store())
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(display)?;
            let cursor_overrides = records
                .iter()
                .filter_map(|table| {
                    table
                        .cursor_column
                        .as_ref()
                        .map(|cursor| (table.name.to_ascii_lowercase(), cursor.clone()))
                })
                .collect();
            let soft_delete_columns = records
                .iter()
                .filter_map(|table| {
                    table
                        .soft_delete_column
                        .as_ref()
                        .map(|column| (table.name.to_ascii_lowercase(), column.clone()))
                })
                .collect();
            run_poll_cycle(
                &pool,
                &metadata_path,
                &database.id,
                &report,
                poll_targets,
                PollOptions {
                    reconcile: reconciliation_due(database, &records),
                    cursor_overrides,
                    soft_delete_columns,
                    ..PollOptions::default()
                },
            )
            .await
            .map(|result| {
                result
                    .tables
                    .into_iter()
                    .map(|table| table.ingested.saturating_add(table.tombstones))
                    .fold(0_u64, |total, rows| {
                        total.saturating_add(u64::try_from(rows).unwrap_or(u64::MAX))
                    })
            })
            .map_err(display)
        }
        _ => Err("database has no active replication mode".to_owned()),
    };
    let disconnect = pool.disconnect().await.map_err(display);
    match (result, disconnect) {
        (Ok(rows), Ok(())) => Ok(rows),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn open_targets(
    metadata_path: &std::path::Path,
    database_id: &str,
    root: &std::path::Path,
    report: &ProbeReport,
    records: &[TableRecord],
) -> Result<Vec<CdcTarget>, String> {
    let tracked = records
        .iter()
        .map(|table| table.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    report
        .tables
        .iter()
        .filter(|source| tracked.contains(&source.name.to_ascii_lowercase()))
        .cloned()
        .map(|source| {
            let directory = table_directory(root, &source.name);
            CdcTarget::open_tracked(
                metadata_path,
                database_id,
                source,
                directory,
                StoreOptions::default(),
            )
            .map_err(display)
        })
        .collect()
}

fn reconciliation_due(database: &DatabaseRecord, records: &[TableRecord]) -> bool {
    records.iter().any(|table| {
        table
            .last_reconcile_at
            .as_deref()
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .is_none_or(|last| {
                Utc::now()
                    .signed_duration_since(last.with_timezone(&Utc))
                    .num_seconds()
                    >= i64::try_from(database.reconcile_interval_seconds).unwrap_or(i64::MAX)
            })
    })
}

fn decode_names(encoded: Option<&str>) -> Result<BTreeSet<String>, String> {
    encoded.map_or_else(
        || Ok(BTreeSet::new()),
        |encoded| {
            serde_json::from_str::<Vec<String>>(encoded)
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

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
