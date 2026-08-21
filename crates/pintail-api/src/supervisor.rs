use std::{collections::BTreeSet, path::Path, time::Duration};

use chrono::{DateTime, Utc};
use mysql_async::Pool;
use pintail_cdc::{CdcOptions, CdcTarget, run_cdc};
use pintail_meta::{DatabaseRecord, MetaStore, TableRecord};
use pintail_poll::{PollOptions, PollTarget, run_cdc_reconciliation, run_poll_cycle};
use pintail_probe::ProbeReport;
use pintail_store::StoreOptions;

use crate::{
    ApiState, backup::start_scheduled_if_due, events::ApiEvent, snapshot::table_directory,
};

const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(5);

/// The supervision cadence: five seconds in production, overridable through
/// `PINTAIL_SUPERVISOR_INTERVAL_MS` so test harnesses stop paying multiples
/// of five seconds for every adoption, auto-include, and re-probe wait.
/// Clamped to 100ms so a typo cannot turn supervision into a busy loop.
fn supervisor_interval() -> Duration {
    std::env::var("PINTAIL_SUPERVISOR_INTERVAL_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(SUPERVISOR_INTERVAL, |ms| Duration::from_millis(ms.max(100)))
}

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
        // No job survives a restart, so any table still marked
        // 'snapshotting' holds a PARTIAL copy from a process that died
        // mid-resnapshot. Left alone it answered queries as a healthy
        // 'streaming' table with a fraction of the source's rows -
        // quarantine each one visibly before the first cadence runs.
        if let Ok(metadata) = state.metadata() {
            match metadata.quarantine_interrupted_snapshots(
                "a table copy was interrupted by a restart; resync to repair the partial data",
            ) {
                Ok(interrupted) => {
                    for (database_id, table) in interrupted {
                        state.publish(ApiEvent::database(
                            "resnapshot.interrupted",
                            &database_id,
                            format!(
                                "{table} was mid-copy when the process stopped; \
                                 flagged needs_resync so the partial data never \
                                 answers as healthy"
                            ),
                        ));
                    }
                }
                Err(error) => {
                    state.publish(ApiEvent::database(
                        "supervisor.error",
                        "control-plane",
                        format!("interrupted-snapshot sweep failed: {error}"),
                    ));
                }
            }
        }
        let mut interval = tokio::time::interval(supervisor_interval());
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
        // auto_resync keyless policy: flagged tables are repaired with a
        // forced snapshot (the safe operator-resync path) instead of the
        // regular cycle this cadence; the snapshot worker owns the job slot.
        if database.keyless_policy == "auto_resync"
            && let Ok(metadata) = state.metadata()
            && let Ok(flagged) = metadata.tables_needing_resync(&database.id)
            && !flagged.is_empty()
        {
            // A begin failure means the job is already active or the
            // control plane hiccupped; the next cadence retries.
            if let Ok(run_id) = crate::snapshot::begin_snapshot_job(state, &database.id, true) {
                state.publish(ApiEvent::database(
                    "resync.auto",
                    &database.id,
                    format!(
                        "auto_resync policy repairing {} flagged table(s) via snapshot {run_id}",
                        flagged.len()
                    ),
                ));
            }
            continue;
        }
        // A polling cycle overwrites the source checkpoint with a polling
        // one, and CDC can never start from that - so a database switched
        // from polling back to cdc has no position to resume from and every
        // cycle would fail with "polling checkpoint cannot start CDC". The
        // only honest handoff is a fresh snapshot: resuming from any older
        // binlog position would skip the interval polling covered, and there
        // is no per-table shortcut because every table shares the position.
        // (This previously worked only by accident, when the old
        // whole-database resync endpoint happened to run right after a mode
        // switch and rebuilt the checkpoint as a side effect.)
        if database.effective_mode.as_deref() == Some("cdc")
            && let Ok(metadata) = state.metadata()
            && let Ok(Some(checkpoint)) = metadata.snapshot_checkpoint(&database.id)
            && checkpoint.kind == "polling"
        {
            // A begin failure means the job is already active or the
            // control plane hiccupped; the next cadence retries. Said out
            // loud rather than swallowed: the e2e gate twice watched a
            // database sit in exactly this state for its whole two-minute
            // adoption window with nothing in the log to say why - the
            // handoff was retrying against a held job lock every cadence,
            // and silence here is indistinguishable from not trying.
            match crate::snapshot::begin_snapshot_job(state, &database.id, true) {
                Ok(run_id) => {
                    state.publish(ApiEvent::database(
                        "resync.auto",
                        &database.id,
                        format!("rebuilding the CDC handoff after polling via snapshot {run_id}"),
                    ));
                }
                Err(error) => {
                    state.publish(ApiEvent::database(
                        "resync.retry",
                        &database.id,
                        format!("the CDC handoff rebuild will retry next cadence: {error}"),
                    ));
                }
            }
            continue;
        }
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
    // effective_mode is unset when the operator resumes to 'auto': the mode
    // change deliberately clears it for recomputation. Requiring it here
    // meant a pause followed by resume-to-auto unscheduled the database
    // FOREVER - the badge kept saying streaming while no cycle ever ran
    // again. An unset mode with a probe in hand is recomputed by the cycle.
    let mode_known = match database.effective_mode.as_deref() {
        Some(mode) => matches!(mode, "cdc" | "polling"),
        None => database.mode == "auto",
    };
    // state stays 'paused' after a resume to 'auto' - the pause wrote it and
    // the auto arm deliberately defers recomputation. That combination is a
    // database WAITING for its first cycle, not a stopped one; requiring a
    // live state here was the other half of the resume-to-auto trap.
    let state_live = matches!(database.state.as_str(), "streaming" | "polling" | "error")
        || (database.state == "paused" && database.mode != "paused");
    database.mode != "paused" && database.probe_json.is_some() && mode_known && state_live
}

async fn supervise_database(state: ApiState, database: DatabaseRecord) {
    let run_id = crate::state::random_identifier("run_", 16);
    let started = std::time::Instant::now();
    // Derived exactly as run_cycle derives it, so the success path's
    // replication-state write (which insists on cdc|polling) re-persists the
    // mode that 'auto' cleared instead of being silently rejected.
    let kind = database
        .effective_mode
        .clone()
        .filter(|mode| matches!(mode.as_str(), "cdc" | "polling"))
        .or_else(|| {
            let report: ProbeReport = serde_json::from_str(database.probe_json.as_deref()?).ok()?;
            Some(crate::snapshot::effective_mode(&database, &report).to_owned())
        })
        .unwrap_or_else(|| "replication".to_owned());
    let kind = kind.as_str();
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
    // A table CDC quarantined (needs_resync) stays frozen until recopied;
    // under binlog_row_metadata=MINIMAL that legitimately happens when the
    // stream falls more than one hidden ALTER behind, so the repair cannot
    // wait for an operator.
    crate::controls::auto_resync_quarantined(&state, &database.id);
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
    let options = crate::dsn::source_opts(&dsn)?;
    let pool = Pool::new(options);
    // Recompute the mode the way the snapshot handoff would when 'auto'
    // cleared it; a successful cycle re-persists it via the replication
    // state below, so this heals the record too.
    let effective = database
        .effective_mode
        .as_deref()
        .filter(|mode| matches!(*mode, "cdc" | "polling"))
        .unwrap_or_else(|| crate::snapshot::effective_mode(database, &report));
    let result = match effective {
        "cdc" => {
            let includes = decode_names(database.include_tables.as_deref())?;
            let excludes = decode_names(database.exclude_tables.as_deref())?;
            let streamed = run_cdc(
                &pool,
                &metadata_path,
                &database.id,
                &report,
                targets,
                CdcOptions {
                    blocking: false,
                    new_table_root: Some(root.clone()),
                    new_table_includes: includes,
                    new_table_excludes: excludes,
                    ..CdcOptions::default()
                },
            )
            .await
            .map(|outcome| u64::try_from(outcome.mutations).unwrap_or(u64::MAX))
            .map_err(display);
            let result = streamed;

            // MySQL executes `ON DELETE/UPDATE CASCADE` inside InnoDB without
            // writing row events, so those child rows are invisible to any CDC
            // reader and survive in the replica forever. The probe flags the
            // affected tables; this is what actually repairs them, on the same
            // cadence polling mode uses for its reconciler.
            if let Ok(due) = cascade_reconciliation_due(&metadata_path, database, &report)
                && !due.is_empty()
            {
                let names = due.join(", ");
                match open_targets(&metadata_path, &database.id, &root, &report, &records) {
                    Ok(all) => {
                        let cascade = all
                            .into_iter()
                            .filter(|target| {
                                due.iter()
                                    .any(|name| name.eq_ignore_ascii_case(&target.source().name))
                            })
                            .map(|target| {
                                let source = target.source().clone();
                                PollTarget::new(source, target.into_store())
                            })
                            .collect::<Result<Vec<_>, _>>();
                        let cascade = match cascade {
                            Ok(cascade) => cascade,
                            Err(error) => {
                                state.publish(ApiEvent::database(
                                    "replication.cascade-reconcile.error",
                                    &database.id,
                                    format!("could not build cascade targets: {error}"),
                                ));
                                Vec::new()
                            }
                        };
                        match run_cdc_reconciliation(
                            &pool,
                            &metadata_path,
                            &database.id,
                            &report,
                            cascade,
                            10_000,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                let repaired: usize =
                                    outcome.tables.iter().map(|table| table.tombstones).sum();
                                state.publish(ApiEvent::database(
                                        "replication.cascade-reconcile",
                                        &database.id,
                                        format!(
                                            "reconciled cascade-affected tables ({names}); {repaired} rows tombstoned"
                                        ),
                                    ));
                            }
                            // A failed repair must not fail the CDC cycle:
                            // streaming is still correct for everything
                            // cascades do not touch.
                            Err(error) => state.publish(ApiEvent::database(
                                "replication.cascade-reconcile.error",
                                &database.id,
                                format!("cascade reconciliation failed for {names}: {error}"),
                            )),
                        }
                    }
                    Err(error) => state.publish(ApiEvent::database(
                        "replication.cascade-reconcile.error",
                        &database.id,
                        format!("could not open cascade targets: {error}"),
                    )),
                }
            }
            result
        }
        "polling" => {
            // A tracked table can vanish from the source between probes (DROP
            // or RENAME that CDC has no handler for). Polling validates every
            // target against the probe report, so one ghost record would fail
            // the whole cycle forever; skip those tables instead and say so.
            let (present, absent): (Vec<_>, Vec<_>) = targets.into_iter().partition(|target| {
                report
                    .tables
                    .iter()
                    .any(|table| table.name.eq_ignore_ascii_case(&target.source().name))
            });
            if !absent.is_empty() {
                let names = absent
                    .iter()
                    .map(|target| target.source().name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                state.publish(ApiEvent::database(
                    "replication.skip",
                    &database.id,
                    format!("polling skipped tables absent from the source: {names}"),
                ));
            }
            let poll_targets = present
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

/// Tables the probe flagged as cascade-affected whose last reconcile is older
/// than the database's interval. Returns names, because the caller reopens
/// targets rather than holding stores across the CDC run.
fn cascade_reconciliation_due(
    metadata_path: &Path,
    database: &DatabaseRecord,
    report: &ProbeReport,
) -> Result<Vec<String>, String> {
    let flagged = report
        .tables
        .iter()
        .filter(|table| table.requires_reconciliation)
        .map(|table| table.name.clone())
        .collect::<Vec<_>>();
    if flagged.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = MetaStore::open(metadata_path).map_err(display)?;
    let records = metadata.tables(&database.id).map_err(display)?;
    Ok(records
        .into_iter()
        .filter(|record| {
            flagged
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&record.name))
                && record
                    .last_reconcile_at
                    .as_deref()
                    .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
                    .is_none_or(|last| {
                        Utc::now()
                            .signed_duration_since(last.with_timezone(&Utc))
                            .num_seconds()
                            >= i64::try_from(database.reconcile_interval_seconds)
                                .unwrap_or(i64::MAX)
                    })
        })
        .map(|record| record.name)
        .collect())
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
