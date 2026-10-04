//! Upkeep of the control-plane file, on a timer of its own.
//!
//! Damage to the metadata file used to surface as whatever read touched the
//! bad page first - a dashboard that could not decode its table list, hours
//! after the fact, with no good copy to go back to. This task looks for
//! damage on purpose (a full check at startup, a quick one every hour),
//! reports it as an error so it reaches Sentry and the dashboard, keeps a
//! short rotation of consistent copies beside the data, and prunes the run
//! history that otherwise grows by one row per replication cycle forever
//! and the audit trail that otherwise grows by one row per query forever.
//!
//! A copy is only taken while the last check was clean: rotating damaged
//! copies in would push out the good ones the moment they are needed.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use chrono::Utc;
use serde::Serialize;

use crate::{ApiState, events::ApiEvent};

/// How often the file is checked and the history pruned.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Default age at which a new copy is taken.
const DEFAULT_BACKUP_HOURS: u64 = 6;
/// Default number of copies kept: two days at the default cadence.
const DEFAULT_BACKUP_KEEP: usize = 8;
/// Successful replication cycles are kept this long; they are one row per
/// cycle and only the recent ones are ever read.
const CYCLE_RETENTION: chrono::Duration = chrono::Duration::days(1);
/// Copies, repairs and failures are kept this long: they are what an
/// operator reads after an incident.
const HISTORY_RETENTION: chrono::Duration = chrono::Duration::days(30);
/// Audit events are kept this many days unless `PINTAIL_AUDIT_RETENTION_DAYS`
/// says otherwise; zero keeps them all.
const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 90;
const AUDIT_RETENTION_VARIABLE: &str = "PINTAIL_AUDIT_RETENTION_DAYS";
/// Most audit events one pruning transaction deletes. A batch this size
/// holds the write lock for tens of milliseconds, so the audit writer and
/// request handlers queued behind it barely notice.
const AUDIT_PRUNE_BATCH: u64 = 10_000;
/// The pause between pruning batches, so writers waiting on the lock are
/// not raced for it by the next batch.
const AUDIT_PRUNE_PAUSE: Duration = Duration::from_millis(20);

const BACKUP_DIRECTORY: &str = "meta-backups";
const BACKUP_PREFIX: &str = "pintail-meta-";

/// What the last check of the metadata file found.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct MetadataHealth {
    /// `unchecked` until the first check completes, then `ok` or `damaged`.
    pub(crate) state: &'static str,
    pub(crate) checked_at: Option<String>,
    /// What the check reported, capped; empty when healthy.
    pub(crate) problems: Vec<String>,
    /// When the newest copy on disk was written.
    pub(crate) last_backup_at: Option<String>,
    /// Why the last attempt to write a copy failed, if it did.
    pub(crate) backup_error: Option<String>,
    /// How long audit events are kept, and what the last pruning pass did.
    pub(crate) audit_retention: AuditRetention,
}

/// The audit log's retention setting and its last pruning pass.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct AuditRetention {
    /// Days an audit event is kept; zero keeps every event.
    pub(crate) days: u32,
    /// When the last pass finished; `None` before the first.
    pub(crate) last_pruned_at: Option<String>,
    /// Events the last pass deleted.
    pub(crate) last_removed: u64,
    /// Transactions the last pass took.
    pub(crate) last_batches: u64,
    /// Why the last pass failed, if it did.
    pub(crate) last_error: Option<String>,
}

/// Days audit events are kept, from `PINTAIL_AUDIT_RETENTION_DAYS`; zero
/// keeps them all. Read once: a value that is not a whole number of days is
/// reported as a warning the first time and the default is kept.
#[must_use]
pub fn audit_retention_days() -> u32 {
    static DAYS: OnceLock<u32> = OnceLock::new();
    *DAYS.get_or_init(|| {
        match parse_audit_retention(std::env::var(AUDIT_RETENTION_VARIABLE).ok().as_deref()) {
            Ok(days) => days,
            Err(rejected) => {
                pintail_log::log_warn!(
                    "{AUDIT_RETENTION_VARIABLE}={rejected:?} is not a whole number of days; \
                     keeping audit events {DEFAULT_AUDIT_RETENTION_DAYS} days"
                );
                DEFAULT_AUDIT_RETENTION_DAYS
            }
        }
    })
}

/// The retention a raw `PINTAIL_AUDIT_RETENTION_DAYS` asks for: the default
/// when unset or blank, the value when it is a whole number of days, and
/// the rejected text otherwise.
fn parse_audit_retention(raw: Option<&str>) -> Result<u32, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(DEFAULT_AUDIT_RETENTION_DAYS),
        Some(value) => value.parse::<u32>().map_err(|_| value.to_owned()),
    }
}

/// The bound below which audit events are pruned at `now`, in the form
/// [`pintail_meta::MetaStore::prune_audit_log`] compares; `None` when
/// `days` keeps everything or reaches back past the calendar.
fn audit_cutoff(now: chrono::DateTime<Utc>, days: u32) -> Option<String> {
    if days == 0 {
        return None;
    }
    let cutoff = now.checked_sub_signed(chrono::Duration::try_days(i64::from(days))?)?;
    Some(cutoff.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// Prunes audit events older than `days` before `now`, `batch_rows` per
/// transaction, calling `between` after each full batch. `None` when
/// `days` is zero and nothing is pruned.
pub(crate) fn prune_audit_log(
    metadata: &pintail_meta::MetaStore,
    now: chrono::DateTime<Utc>,
    days: u32,
    batch_rows: u64,
    between: impl FnMut(u64),
) -> Option<anyhow::Result<pintail_meta::AgePrune>> {
    let cutoff = audit_cutoff(now, days)?;
    Some(metadata.prune_audit_log(&cutoff, batch_rows, between))
}

fn health_cell() -> &'static Mutex<MetadataHealth> {
    static HEALTH: OnceLock<Mutex<MetadataHealth>> = OnceLock::new();
    HEALTH.get_or_init(|| {
        Mutex::new(MetadataHealth {
            state: "unchecked",
            ..MetadataHealth::default()
        })
    })
}

/// The metadata file's health as of the last check.
pub(crate) fn current() -> MetadataHealth {
    let mut health = health_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    health.audit_retention.days = audit_retention_days();
    health
}

fn update(change: impl FnOnce(&mut MetadataHealth)) {
    change(
        &mut health_cell()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Runs the upkeep until `shutdown` fires. The first pass runs at once and
/// checks thoroughly; later passes use the quick check.
pub(crate) async fn run(state: ApiState, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut thorough = true;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let pass_state = state.clone();
                // Every step is synchronous SQLite and file work, and a full
                // check of a large file takes seconds: keep it off the
                // runtime's worker threads.
                let _ = tokio::task::spawn_blocking(move || upkeep(&pass_state, thorough)).await;
                thorough = false;
            }
            _ = shutdown.recv() => break,
        }
    }
}

fn upkeep(state: &ApiState, thorough: bool) {
    let healthy = check(state, thorough);
    prune(state);
    if healthy {
        backup_if_due(state);
    }
}

/// Checks the file and records the outcome; true when it is healthy.
fn check(state: &ApiState, thorough: bool) -> bool {
    let outcome = state
        .metadata()
        .map_err(|error| error.to_string())
        .and_then(|metadata| {
            metadata
                .integrity_problems(thorough)
                .map_err(|error| format!("{error:#}"))
        });
    let problems = match outcome {
        Ok(problems) => problems,
        // A check that cannot run is not a clean check. The file may simply
        // be busy, so this is reported but not called damage.
        Err(error) => {
            pintail_log::log_error!("metadata integrity check could not run: {error}");
            return false;
        }
    };
    let now = Utc::now().to_rfc3339();
    let damaged = !problems.is_empty();
    let newly_damaged = damaged && current().state != "damaged";
    if damaged {
        // Error level, so it reaches Sentry. Once per transition, with the
        // first findings; the dashboard carries the rest.
        if newly_damaged {
            pintail_log::log_error!(
                "metadata damaged: {} problem(s) found, first: {}",
                problems.len(),
                problems.first().map_or("", String::as_str)
            );
            state.publish(ApiEvent::database(
                "metadata.damaged",
                "control-plane",
                format!(
                    "the control-plane metadata file failed its integrity check \
                     ({} problem(s)); copies in {BACKUP_DIRECTORY}/ predate the damage",
                    problems.len()
                ),
            ));
        }
    } else if current().state == "damaged" {
        pintail_log::log_info!("metadata integrity check is clean again");
    }
    update(|health| {
        health.state = if damaged { "damaged" } else { "ok" };
        health.checked_at = Some(now);
        health.problems = problems.into_iter().take(20).collect();
    });
    !damaged
}

fn prune(state: &ApiState) {
    let now = Utc::now();
    let cycles_since = (now - CYCLE_RETENTION).to_rfc3339();
    let history_since = (now - HISTORY_RETENTION).to_rfc3339();
    match state
        .metadata()
        .map_err(|error| error.to_string())
        .and_then(|metadata| {
            metadata
                .prune_sync_runs(&cycles_since, &history_since)
                .map_err(|error| format!("{error:#}"))
        }) {
        Ok(0) => {}
        Ok(removed) => pintail_log::log_info!("pruned {removed} sync run(s) past retention"),
        Err(error) => pintail_log::log_error!("sync run pruning failed: {error}"),
    }
    prune_audit(state, now);
}

/// One pass over the audit log, between the batches of which the lock is
/// left free for [`AUDIT_PRUNE_PAUSE`].
fn prune_audit(state: &ApiState, now: chrono::DateTime<Utc>) {
    let days = audit_retention_days();
    let outcome = match state.metadata() {
        Ok(metadata) => prune_audit_log(&metadata, now, days, AUDIT_PRUNE_BATCH, |_| {
            std::thread::sleep(AUDIT_PRUNE_PAUSE);
        })
        .map(|outcome| outcome.map_err(|error| format!("{error:#}"))),
        Err(error) => Some(Err(error.to_string())),
    };
    let Some(outcome) = outcome else {
        return;
    };
    match &outcome {
        Ok(pruned) if pruned.removed > 0 => pintail_log::log_info!(
            "pruned {} audit event(s) older than {days} day(s) in {} batch(es)",
            pruned.removed,
            pruned.batches
        ),
        Ok(_) => {}
        Err(error) => pintail_log::log_error!("audit log pruning failed: {error}"),
    }
    update(|health| {
        let retention = &mut health.audit_retention;
        retention.last_pruned_at = Some(Utc::now().to_rfc3339());
        match outcome {
            Ok(pruned) => {
                retention.last_removed = pruned.removed;
                retention.last_batches = pruned.batches;
                retention.last_error = None;
            }
            Err(error) => {
                retention.last_removed = 0;
                retention.last_batches = 0;
                retention.last_error = Some(error);
            }
        }
    });
}

fn backup_if_due(state: &ApiState) {
    let Ok(data_dir) = state.data_dir() else {
        return;
    };
    let directory = data_dir.join(BACKUP_DIRECTORY);
    let period = Duration::from_secs(
        env_number("PINTAIL_META_BACKUP_HOURS", DEFAULT_BACKUP_HOURS).saturating_mul(3600),
    );
    let keep = env_number("PINTAIL_META_BACKUP_KEEP", DEFAULT_BACKUP_KEEP);
    if period.is_zero() || keep == 0 {
        return;
    }
    let existing = backups(&directory);
    let newest = existing
        .last()
        .and_then(|path| path.metadata().ok())
        .and_then(|metadata| metadata.modified().ok());
    let due = newest.is_none_or(|written| {
        SystemTime::now()
            .duration_since(written)
            .is_ok_and(|age| age >= period)
    });
    if !due {
        update(|health| health.last_backup_at = newest.map(rfc3339));
        return;
    }
    match write_backup(state, &directory) {
        Ok(path) => {
            pintail_log::log_info!("metadata backup written to {}", path.display());
            for stale in backups(&directory).iter().rev().skip(keep) {
                if let Err(error) = std::fs::remove_file(stale) {
                    pintail_log::log_error!(
                        "could not remove old metadata backup {}: {error}",
                        stale.display()
                    );
                }
            }
            update(|health| {
                health.last_backup_at = Some(Utc::now().to_rfc3339());
                health.backup_error = None;
            });
        }
        Err(error) => {
            pintail_log::log_error!("metadata backup failed: {error}");
            update(|health| {
                health.last_backup_at = newest.map(rfc3339);
                health.backup_error = Some(error);
            });
        }
    }
}

fn write_backup(state: &ApiState, directory: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let target = directory.join(format!(
        "{BACKUP_PREFIX}{}.db",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    state
        .metadata()
        .map_err(|error| error.to_string())?
        .backup_into(&target)
        .map_err(|error| format!("{error:#}"))?;
    Ok(target)
}

/// Completed copies in `directory`, oldest first. The timestamped names sort
/// chronologically; partial copies are not counted.
fn backups(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(BACKUP_PREFIX)
                        && Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("db"))
                })
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<Utc>::from(time).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};

    use super::{
        DEFAULT_AUDIT_RETENTION_DAYS, audit_cutoff, backups, parse_audit_retention, prune_audit_log,
    };
    use crate::test_support::Node;

    #[test]
    fn audit_retention_reads_whole_days_and_rejects_the_rest() {
        assert_eq!(
            parse_audit_retention(None),
            Ok(DEFAULT_AUDIT_RETENTION_DAYS)
        );
        assert_eq!(parse_audit_retention(Some("  ")), Ok(90));
        assert_eq!(parse_audit_retention(Some("30")), Ok(30));
        assert_eq!(parse_audit_retention(Some(" 0 ")), Ok(0));
        for bad in ["-1", "7d", "1.5", "ninety", "99999999999"] {
            assert_eq!(parse_audit_retention(Some(bad)), Err(bad.to_owned()));
        }
    }

    #[test]
    fn the_audit_cutoff_is_whole_seconds_days_back_and_absent_at_zero() {
        let now = Utc
            .with_ymd_and_hms(2026, 10, 4, 12, 30, 15)
            .single()
            .expect("time");
        assert_eq!(
            audit_cutoff(now, 90).as_deref(),
            Some("2026-07-06T12:30:15")
        );
        assert_eq!(audit_cutoff(now, 0), None);
        assert_eq!(audit_cutoff(now, u32::MAX), None, "before the calendar");
    }

    fn audit_at(node: &Node, id: &str, created_at: &str) {
        node.metadata()
            .record_audit_event(&pintail_meta::NewAuditEvent {
                id,
                workspace_id: &node.first_workspace,
                actor_type: "user",
                actor_id: &node.admin_id,
                actor_label: "admin@example.com",
                action: "query.run",
                target_type: None,
                target_id: None,
                detail_json: None,
                created_at,
                client_ip: None,
            })
            .expect("audit event");
    }

    fn query_ids(node: &Node) -> Vec<String> {
        let mut ids = node
            .metadata()
            .audit_log_in_workspace(&node.first_workspace, 10_000)
            .expect("audit log")
            .into_iter()
            .filter(|event| event.action == "query.run")
            .map(|event| event.id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// At an injected `now`, events older than the retention go and newer
    /// ones stay; a retention of zero leaves every event alone.
    #[tokio::test]
    async fn audit_events_past_retention_are_pruned_and_zero_keeps_all() {
        let node = Node::new().await;
        let now = Utc
            .with_ymd_and_hms(2026, 10, 4, 0, 0, 0)
            .single()
            .expect("time");
        audit_at(&node, "a_year_old", "2025-10-04T00:00:00+00:00");
        audit_at(&node, "ninety_one_days", "2026-07-05T00:00:00.5+00:00");
        audit_at(&node, "eighty_nine_days", "2026-07-07T00:00:00+00:00");
        audit_at(&node, "today", "2026-10-03T23:59:59+00:00");

        let metadata = node.metadata();
        assert!(
            prune_audit_log(&metadata, now, 0, 10_000, |_| {}).is_none(),
            "zero keeps everything"
        );
        assert_eq!(query_ids(&node).len(), 4);

        let pruned = prune_audit_log(&metadata, now, 90, 10_000, |_| {})
            .expect("retention on")
            .expect("prune");
        assert_eq!((pruned.removed, pruned.batches), (2, 1));
        assert_eq!(query_ids(&node), ["eighty_nine_days", "today"]);
    }

    /// A pass over more events than a batch deletes them a batch per
    /// transaction.
    #[tokio::test]
    async fn audit_pruning_takes_a_transaction_per_batch() {
        let node = Node::new().await;
        for serial in 0..45 {
            audit_at(
                &node,
                &format!("old_{serial:02}"),
                "2026-01-01T00:00:00+00:00",
            );
        }
        let now = Utc
            .with_ymd_and_hms(2026, 10, 4, 0, 0, 0)
            .single()
            .expect("time");
        let mut sizes = Vec::new();
        let pruned = prune_audit_log(&node.metadata(), now, 90, 20, |removed| {
            sizes.push(removed);
        })
        .expect("retention on")
        .expect("prune");
        assert_eq!((pruned.removed, pruned.batches), (45, 3));
        assert_eq!(sizes, [20, 20]);
        assert!(query_ids(&node).is_empty());
    }

    #[test]
    fn only_completed_copies_count_and_they_sort_oldest_first() {
        let dir = tempfile::tempdir().expect("temporary directory");
        for name in [
            "pintail-meta-20260102T000000Z.db",
            "pintail-meta-20260101T000000Z.db",
            "pintail-meta-20260103T000000Z.db.partial",
            "unrelated.db",
        ] {
            std::fs::write(dir.path().join(name), b"").expect("write");
        }
        let names = backups(dir.path())
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("named")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "pintail-meta-20260101T000000Z.db",
                "pintail-meta-20260102T000000Z.db"
            ]
        );
    }
}
