//! Durable audit trail: every mutating action taken by every dashboard user,
//! scoped to the workspace their session is in. API-key sessions (headless
//! automation against a single database, not a signed-in person) are not
//! logged here — that is a separate concern from "what did each user do."
//!
//! Logging failures do not fail the request that triggered them — the
//! primary action already succeeded (or is about to be reported to the
//! caller) by the time this runs, and losing the audit trail for one event
//! is preferable to losing the underlying work.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

use chrono::Utc;
use serde_json::Value;

use crate::{ApiState, auth::AuthPrincipal, state::random_identifier};

/// Records one audit event in the caller's current workspace. `target` is
/// `(type, id)`, e.g. `("database", "db_abc123")`. A no-op for API-key
/// sessions, which have no workspace to scope into. Errors are logged to
/// stderr rather than propagated, per the module-level rationale above.
pub(crate) fn record(
    state: &ApiState,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) {
    let Some(workspace_id) = principal.workspace_id.clone() else {
        return;
    };
    record_in(state, &workspace_id, principal, action, target, detail);
}

/// Records one audit event in an explicit workspace, for the rare action
/// (creating a workspace, accepting an invite into one) that targets a
/// workspace other than the one the caller's session is currently scoped
/// to.
pub(crate) fn record_in(
    state: &ApiState,
    workspace_id: &str,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) {
    if let Err(error) = try_record(state, workspace_id, principal, action, target, detail) {
        eprintln!("audit log: failed to record '{action}' in {workspace_id}: {error}");
    }
}

fn try_record(
    state: &ApiState,
    workspace_id: &str,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) -> anyhow::Result<()> {
    let metadata = state
        .metadata()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (actor_type, actor_label) = if principal.database_id.is_some() {
        ("api_key", principal.subject.clone())
    } else {
        let label = metadata
            .user_by_id(&principal.subject)?
            .map_or_else(|| principal.subject.clone(), |user| user.email);
        ("user", label)
    };
    let detail_json = detail.map(|value| value.to_string());
    metadata.record_audit_event(&pintail_meta::NewAuditEvent {
        id: &random_identifier("audit_", 16),
        workspace_id,
        actor_type,
        actor_id: &principal.subject,
        actor_label: &actor_label,
        action,
        target_type: target.map(|(kind, _)| kind),
        target_id: target.map(|(_, id)| id),
        detail_json: detail_json.as_deref(),
        created_at: &Utc::now().to_rfc3339(),
        client_ip: principal.client_ip.as_deref(),
    })
}

/// Records one audit event without making the caller wait for its commit,
/// for the one action that arrives at the rate of the workload: a query.
///
/// The event is handed to a single writer, which commits whatever has
/// queued up behind the commit before as one transaction. A row apiece
/// from a thread apiece had every request open the store, wait its turn
/// for the write lock and synchronize the log for one row: under
/// concurrent sessions that waiting was most of what the server did. The
/// row's time is the time the event was queued. A process that dies with
/// events queued loses those rows and nothing else, which is the window
/// the unawaited write already had.
///
/// The queue holds at most [`QUEUE_BYTES`] of events. When the writer
/// falls that far behind - the metadata store locked by another
/// connection, a disk that has stopped keeping up - the caller waits here
/// for room instead of the queue growing: the request is slowed, its
/// audit row is not given up.
pub(crate) async fn record_later(
    state: &ApiState,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) {
    let Some(workspace_id) = principal.workspace_id.clone() else {
        return;
    };
    let Some(queue) = state.audit_queue() else {
        return;
    };
    queue
        .push(Queued {
            id: random_identifier("audit_", 16),
            workspace_id,
            by_key: principal.database_id.is_some(),
            actor_id: principal.subject.clone(),
            action: action.to_owned(),
            target: target.map(|(kind, id)| (kind.to_owned(), id.to_owned())),
            detail_json: detail.map(|value| value.to_string()),
            created_at: Utc::now().to_rfc3339(),
            client_ip: principal.client_ip.clone(),
            room: None,
        })
        .await;
}

/// An event waiting for the writer, owning what its request lent it.
struct Queued {
    id: String,
    workspace_id: String,
    /// Whether an API key acted, rather than a signed-in user.
    by_key: bool,
    actor_id: String,
    action: String,
    target: Option<(String, String)>,
    detail_json: Option<String>,
    created_at: String,
    client_ip: Option<String>,
    /// The share of the queue this event holds, given back when the event
    /// is dropped: after the writer has stored it or reported it lost.
    room: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Queued {
    /// What the event holds against the queue's bound: the text it owns,
    /// and never less than [`EVENT_FLOOR`], so the bound on bytes is a
    /// bound on the number of events too.
    fn charge(&self) -> usize {
        let owned = [
            self.id.len(),
            self.workspace_id.len(),
            self.actor_id.len(),
            self.action.len(),
            self.target
                .as_ref()
                .map_or(0, |(kind, id)| kind.len() + id.len()),
            self.detail_json.as_ref().map_or(0, String::len),
            self.created_at.len(),
            self.client_ip.as_ref().map_or(0, String::len),
        ]
        .iter()
        .sum::<usize>();
        owned
            .saturating_add(std::mem::size_of::<Self>())
            .max(EVENT_FLOOR)
    }
}

/// Most events one commit carries; the rest wait for the next.
const BATCH_EVENTS: usize = 512;

/// Bytes of events the queue holds before a caller waits for room. A
/// query's statement text is in its event, and a request body is at most
/// a couple of megabytes, so this is a dozen of the largest statements or
/// tens of thousands of ordinary ones - far more than one commit drains,
/// so a writer that is keeping up never makes anyone wait.
const QUEUE_BYTES: usize = 32 << 20;

/// The least an event is charged, so that the bound on bytes also bounds
/// the number of events: at most `QUEUE_BYTES / EVENT_FLOOR` of them.
const EVENT_FLOOR: usize = 1024;

/// How the writer treats a commit refused because another connection held
/// the store's lock past its busy timeout.
#[derive(Clone, Copy, Debug)]
struct Contention {
    /// Attempts at the whole batch before it is given up. Each attempt has
    /// already waited out the busy timeout, so this is how many timeouts a
    /// batch outlasts.
    attempts: u32,
    /// The pause between attempts.
    pause: std::time::Duration,
}

/// Three busy timeouts - about fifteen seconds of a store nobody can
/// write - before a batch is reported lost. Until then the queue fills and
/// callers wait, so events are dropped only once the store has been
/// unwritable for that long, and then counted and logged, a batch at a
/// time.
const CONTENTION: Contention = Contention {
    attempts: 3,
    pause: std::time::Duration::from_millis(100),
};

/// What the queue has been given and what its writer has done with it.
#[derive(Default)]
struct Counts {
    queued: AtomicU64,
    written: AtomicU64,
    commits: AtomicU64,
    /// Events that found the queue full and waited for room.
    waited: AtomicU64,
    /// Events finished without being stored.
    lost: AtomicU64,
    /// Writes the writer tried: a whole batch, or an event alone.
    attempts: AtomicU64,
}

/// A snapshot of the queue's counters.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QueueCounts {
    /// Events handed to the queue.
    pub(crate) queued: u64,
    /// Events the writer has finished with: stored, or reported as lost.
    pub(crate) written: u64,
    /// Commits the writer made for them.
    pub(crate) commits: u64,
    /// Events that waited for room in a full queue.
    pub(crate) waited: u64,
    /// Events finished without being stored.
    pub(crate) lost: u64,
    /// Writes the writer tried.
    pub(crate) attempts: u64,
    /// Bytes the queued events hold against the bound.
    pub(crate) held_bytes: usize,
}

/// The events waiting to be written, and the one thread that writes them.
pub(crate) struct Queue {
    metadata_path: PathBuf,
    /// The writer's inbox; the writer is started by the first event.
    sender: OnceLock<mpsc::Sender<Queued>>,
    /// Room left in the queue, in bytes; an event takes its charge before
    /// it is sent and gives it back when it is dropped.
    room: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    contention: Contention,
    counts: Arc<Counts>,
}

impl Queue {
    pub(crate) fn new(metadata_path: PathBuf) -> Self {
        Self::bounded(metadata_path, QUEUE_BYTES, CONTENTION)
    }

    fn bounded(metadata_path: PathBuf, capacity: usize, contention: Contention) -> Self {
        let capacity = capacity.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        Self {
            metadata_path,
            sender: OnceLock::new(),
            room: Arc::new(tokio::sync::Semaphore::new(capacity)),
            capacity,
            contention,
            counts: Arc::new(Counts::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> QueueCounts {
        QueueCounts {
            queued: self.counts.queued.load(Ordering::Acquire),
            written: self.counts.written.load(Ordering::Acquire),
            commits: self.counts.commits.load(Ordering::Acquire),
            waited: self.counts.waited.load(Ordering::Acquire),
            lost: self.counts.lost.load(Ordering::Acquire),
            attempts: self.counts.attempts.load(Ordering::Acquire),
            held_bytes: self.capacity - self.room.available_permits(),
        }
    }

    async fn push(&self, mut event: Queued) {
        self.counts.queued.fetch_add(1, Ordering::AcqRel);
        // An event larger than the whole queue waits for all of it rather
        // than for room that can never be.
        let charge = u32::try_from(event.charge().min(self.capacity)).unwrap_or(u32::MAX);
        let room = match Arc::clone(&self.room).try_acquire_many_owned(charge) {
            Ok(room) => Some(room),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                self.counts.waited.fetch_add(1, Ordering::AcqRel);
                Arc::clone(&self.room).acquire_many_owned(charge).await.ok()
            }
            // The semaphore is never closed.
            Err(tokio::sync::TryAcquireError::Closed) => None,
        };
        event.room = room;
        let sender = self.sender.get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Queued>();
            let path = self.metadata_path.clone();
            let counts = Arc::clone(&self.counts);
            let contention = self.contention;
            // A writer that cannot be started drops its inbox, and every
            // event then takes the path below.
            let _ = std::thread::Builder::new()
                .name("pintail-audit".to_owned())
                .spawn(move || {
                    while let Ok(first) = receiver.recv() {
                        let mut batch = vec![first];
                        while batch.len() < BATCH_EVENTS
                            && let Ok(next) = receiver.try_recv()
                        {
                            batch.push(next);
                        }
                        write_batch(&path, &batch, &counts, contention);
                        // Dropping the batch gives its room back.
                    }
                });
            sender
        });
        if let Err(mpsc::SendError(event)) = sender.send(event) {
            // No writer: this thread writes the row itself.
            write_batch(&self.metadata_path, &[event], &self.counts, self.contention);
        }
    }
}

/// Stores `batch` as one commit.
///
/// A commit refused because another connection held the lock is a failure
/// of the moment, which every event would meet alike: the whole batch is
/// tried again, up to `contention.attempts` times, and then reported lost.
/// Any other failure - one event names a workspace deleted since, say - is
/// one event's fault, so each event is tried alone, the others are kept,
/// and what cannot be stored is reported and dropped. If the lock is lost
/// to another connection partway through that, the events not yet tried
/// are reported lost rather than each waiting out a busy timeout of its
/// own.
fn write_batch(metadata_path: &Path, batch: &[Queued], counts: &Counts, contention: Contention) {
    let finished = u64::try_from(batch.len()).unwrap_or(u64::MAX);
    let lose = |count: u64, why: &dyn std::fmt::Display| {
        eprintln!("audit log: {count} event(s) lost: {why}");
        counts.lost.fetch_add(count, Ordering::AcqRel);
    };
    let metadata = match pintail_meta::MetaStore::open(metadata_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            lose(
                finished,
                &format!("failed to open the metadata store: {error}"),
            );
            counts.written.fetch_add(finished, Ordering::AcqRel);
            return;
        }
    };
    // The label is the actor's email as it is when the row is written; one
    // read per actor in the batch.
    let mut labels = HashMap::<&str, String>::new();
    for event in batch {
        if event.by_key || labels.contains_key(event.actor_id.as_str()) {
            continue;
        }
        let label = metadata
            .user_by_id(&event.actor_id)
            .ok()
            .flatten()
            .map_or_else(|| event.actor_id.clone(), |user| user.email);
        labels.insert(&event.actor_id, label);
    }
    let rows = batch
        .iter()
        .map(|event| pintail_meta::NewAuditEvent {
            id: &event.id,
            workspace_id: &event.workspace_id,
            actor_type: if event.by_key { "api_key" } else { "user" },
            actor_id: &event.actor_id,
            actor_label: labels
                .get(event.actor_id.as_str())
                .map_or(event.actor_id.as_str(), String::as_str),
            action: &event.action,
            target_type: event.target.as_ref().map(|(kind, _)| kind.as_str()),
            target_id: event.target.as_ref().map(|(_, id)| id.as_str()),
            detail_json: event.detail_json.as_deref(),
            created_at: &event.created_at,
            client_ip: event.client_ip.as_deref(),
        })
        .collect::<Vec<_>>();
    let mut attempt = 0;
    let failure = loop {
        attempt += 1;
        counts.attempts.fetch_add(1, Ordering::AcqRel);
        match metadata.record_audit_events(&rows) {
            Ok(()) => {
                counts.commits.fetch_add(1, Ordering::AcqRel);
                counts.written.fetch_add(finished, Ordering::AcqRel);
                return;
            }
            Err(error) if pintail_meta::is_lock_contention(&error) => {
                if attempt >= contention.attempts.max(1) {
                    lose(
                        finished,
                        &format!(
                            "the metadata store stayed locked through {attempt} attempts: {error}"
                        ),
                    );
                    counts.written.fetch_add(finished, Ordering::AcqRel);
                    return;
                }
                std::thread::sleep(contention.pause);
            }
            Err(error) => break error,
        }
    };
    if rows.len() > 1 {
        eprintln!(
            "audit log: a batch of {} event(s) was refused ({failure}); storing each alone",
            rows.len()
        );
    }
    for (index, row) in rows.iter().enumerate() {
        counts.attempts.fetch_add(1, Ordering::AcqRel);
        match metadata.record_audit_event(row) {
            Ok(()) => {
                counts.commits.fetch_add(1, Ordering::AcqRel);
            }
            Err(error) if pintail_meta::is_lock_contention(&error) => {
                let left = u64::try_from(rows.len() - index).unwrap_or(u64::MAX);
                lose(left, &format!("the metadata store was locked: {error}"));
                break;
            }
            Err(error) => {
                counts.lost.fetch_add(1, Ordering::AcqRel);
                eprintln!(
                    "audit log: failed to record '{}' in {}: {error}",
                    row.action, row.workspace_id
                );
            }
        }
    }
    counts.written.fetch_add(finished, Ordering::AcqRel);
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_support::Node;

    fn session(node: &Node) -> AuthPrincipal {
        AuthPrincipal {
            subject: node.admin_id.clone(),
            role: "admin".to_owned(),
            database_id: None,
            workspace_id: Some(node.first_workspace.clone()),
            scopes: vec!["*".to_owned()],
            client_ip: Some("192.0.2.7".to_owned()),
        }
    }

    fn queued(node: &Node, workspace_id: &str, serial: usize) -> Queued {
        Queued {
            id: format!("audit_direct_{serial}"),
            workspace_id: workspace_id.to_owned(),
            by_key: false,
            actor_id: node.admin_id.clone(),
            action: "query.run".to_owned(),
            target: Some(("database".to_owned(), "db_example".to_owned())),
            detail_json: Some(format!("{{\"serial\":{serial}}}")),
            created_at: format!("2026-10-03T00:00:{:02}Z", serial % 60),
            client_ip: None,
            room: None,
        }
    }

    fn query_rows(node: &Node) -> Vec<pintail_meta::AuditEventRecord> {
        node.metadata()
            .audit_log_in_workspace(&node.first_workspace, 10_000)
            .expect("audit log")
            .into_iter()
            .filter(|row| row.action == "query.run")
            .collect()
    }

    /// However many events a batch holds, the writer makes one commit for
    /// it, and each row carries the actor's email and the time it was
    /// queued.
    #[tokio::test]
    async fn a_batch_is_one_commit() {
        let node = Node::new().await;
        let counts = Counts::default();
        let batch = (0..100)
            .map(|serial| queued(&node, &node.first_workspace, serial))
            .collect::<Vec<_>>();
        write_batch(
            node.state.metadata_path().expect("path"),
            &batch,
            &counts,
            CONTENTION,
        );
        assert_eq!(counts.commits.load(Ordering::Acquire), 1);
        assert_eq!(counts.written.load(Ordering::Acquire), 100);
        let rows = query_rows(&node);
        assert_eq!(rows.len(), 100);
        assert!(
            rows.iter()
                .all(|row| row.actor_label == "admin@example.com")
        );
        assert!(
            rows.iter()
                .any(|row| row.created_at == "2026-10-03T00:00:07Z")
        );
    }

    /// One event that cannot be stored does not take the batch with it.
    #[tokio::test]
    async fn an_event_that_cannot_be_stored_costs_only_itself() {
        let node = Node::new().await;
        let counts = Counts::default();
        let mut batch = (0..10)
            .map(|serial| queued(&node, &node.first_workspace, serial))
            .collect::<Vec<_>>();
        batch[4].workspace_id = "ws_that_was_deleted".to_owned();
        write_batch(
            node.state.metadata_path().expect("path"),
            &batch,
            &counts,
            CONTENTION,
        );
        assert_eq!(query_rows(&node).len(), 9);
        assert_eq!(counts.written.load(Ordering::Acquire), 10);
        assert_eq!(counts.commits.load(Ordering::Acquire), 9);
    }

    /// Events queued from several tasks at once are all written, by the
    /// one writer, and an API-key session still records nothing.
    #[tokio::test]
    async fn queued_events_are_all_written() {
        let node = Node::new().await;
        let principal = session(&node);
        futures_util::future::join_all((0..200).map(|serial| {
            record_later(
                &node.state,
                &principal,
                "query.run",
                Some(("database", "db_example")),
                Some(serde_json::json!({"sql": "SELECT 1", "rows": serial})),
            )
        }))
        .await;
        let mut by_key = session(&node);
        by_key.workspace_id = None;
        by_key.database_id = Some("db_example".to_owned());
        record_later(&node.state, &by_key, "query.run", None, None).await;

        let queue = node.state.audit_queue().expect("queue");
        let deadline = Instant::now() + Duration::from_secs(20);
        while queue.counts().written < 200 {
            assert!(Instant::now() < deadline, "the writer never caught up");
            std::thread::sleep(Duration::from_millis(5));
        }
        let counts = queue.counts();
        assert_eq!(counts.queued, 200);
        assert_eq!((counts.waited, counts.lost), (0, 0), "{counts:?}");
        assert!((1..=200).contains(&counts.commits), "{counts:?}");
        let rows = query_rows(&node);
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|row| {
            row.actor_label == "admin@example.com" && row.client_ip.as_deref() == Some("192.0.2.7")
        }));
        assert_eq!(queue.counts().held_bytes, 0);
    }

    /// Another connection holding the store's write lock, the way a long
    /// metadata transaction does.
    fn hold_the_lock(node: &Node) -> rusqlite::Connection {
        let holder =
            rusqlite::Connection::open(node.state.metadata_path().expect("path")).expect("open");
        holder.execute_batch("BEGIN IMMEDIATE").expect("lock");
        holder
    }

    /// While the writer cannot commit, the queue fills to its bound and no
    /// further: the next caller waits for room, holding its own event, and
    /// when the lock is released every event is stored - none was dropped
    /// to make room.
    #[tokio::test]
    async fn a_full_queue_makes_the_caller_wait_and_loses_nothing() {
        let node = Node::new().await;
        let principal = session(&node);
        let capacity = 8 * EVENT_FLOOR;
        let queue = Queue::bounded(
            node.state.metadata_path().expect("path").to_owned(),
            capacity,
            CONTENTION,
        );
        let holder = hold_the_lock(&node);
        let event = |serial: usize| {
            let mut event = queued(&node, &node.first_workspace, serial);
            event.actor_id.clone_from(&principal.subject);
            event
        };
        let all = futures_util::future::join_all((0..20).map(|serial| queue.push(event(serial))));
        tokio::pin!(all);
        let deadline = Instant::now() + Duration::from_secs(20);
        while queue.counts().waited == 0 {
            assert!(Instant::now() < deadline, "nothing ever waited");
            let pending = tokio::time::timeout(Duration::from_millis(10), &mut all).await;
            assert!(pending.is_err(), "every push returned under a held lock");
            assert!(
                queue.counts().held_bytes <= capacity,
                "{:?}",
                queue.counts()
            );
        }
        let counts = queue.counts();
        assert_eq!(counts.written, 0, "{counts:?}");
        assert!(counts.held_bytes <= capacity, "{counts:?}");
        holder.execute_batch("ROLLBACK").expect("release");
        all.await;
        while queue.counts().written < 20 {
            assert!(Instant::now() < deadline, "the writer never caught up");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let counts = queue.counts();
        assert_eq!((counts.queued, counts.lost), (20, 0), "{counts:?}");
        assert!(counts.waited >= 12, "{counts:?}");
        assert_eq!(query_rows(&node).len(), 20);
        assert_eq!(queue.counts().held_bytes, 0);
    }

    /// A batch refused because another connection holds the lock is tried
    /// again whole, a bounded number of times, and then reported lost and
    /// counted - not retried an event at a time, each waiting out a busy
    /// timeout of its own.
    #[tokio::test]
    async fn a_locked_store_costs_a_batch_its_attempts_not_one_per_event() {
        let node = Node::new().await;
        let holder = hold_the_lock(&node);
        let counts = Counts::default();
        let batch = (0..50)
            .map(|serial| queued(&node, &node.first_workspace, serial))
            .collect::<Vec<_>>();
        write_batch(
            node.state.metadata_path().expect("path"),
            &batch,
            &counts,
            Contention {
                attempts: 2,
                pause: Duration::ZERO,
            },
        );
        holder.execute_batch("ROLLBACK").expect("release");
        assert_eq!(counts.attempts.load(Ordering::Acquire), 2);
        assert_eq!(counts.commits.load(Ordering::Acquire), 0);
        assert_eq!(counts.lost.load(Ordering::Acquire), 50);
        assert_eq!(counts.written.load(Ordering::Acquire), 50);
        assert!(query_rows(&node).is_empty());
    }

    /// The audit writer keeps committing while the retention pass prunes:
    /// a batch written in the gap between two pruning transactions commits
    /// on its first attempt, and batches written from another thread for
    /// the whole pass all commit too. Nothing is lost and no new row is
    /// pruned.
    #[tokio::test]
    async fn the_audit_writer_commits_during_a_prune() {
        let node = Node::new().await;
        let path = node.state.metadata_path().expect("path").to_owned();
        let old = (0..3_000)
            .map(|serial| {
                let mut event = queued(&node, &node.first_workspace, serial);
                event.id = format!("audit_old_{serial}");
                event.created_at = "2025-01-01T00:00:00+00:00".to_owned();
                event
            })
            .collect::<Vec<_>>();
        write_batch(&path, &old, &Counts::default(), CONTENTION);
        let now = Utc::now();
        // The test node is not shared across threads; the writer thread
        // builds its events from these.
        let (workspace_id, actor_id) = (node.first_workspace.clone(), node.admin_id.clone());
        let fresh = |serial: usize| Queued {
            id: format!("audit_new_{serial}"),
            workspace_id: workspace_id.clone(),
            by_key: false,
            actor_id: actor_id.clone(),
            action: "query.run".to_owned(),
            target: None,
            detail_json: None,
            created_at: now.to_rfc3339(),
            client_ip: None,
            room: None,
        };

        let between = Counts::default();
        let beside = Counts::default();
        let mut gaps = 0;
        let pruned = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for serial in 0..20 {
                    write_batch(&path, &[fresh(10_000 + serial)], &beside, CONTENTION);
                }
            });
            let pruned = crate::metadata_health::prune_aged(
                &node.metadata(),
                crate::metadata_health::AUDIT,
                now,
                90,
                100,
                |_| {
                    let attempts = between.attempts.load(Ordering::Acquire);
                    write_batch(&path, &[fresh(gaps)], &between, CONTENTION);
                    assert_eq!(
                        between.attempts.load(Ordering::Acquire),
                        attempts + 1,
                        "the gap between batches holds no lock"
                    );
                    gaps += 1;
                },
            );
            writer.join().expect("writer thread");
            pruned
        })
        .expect("retention on")
        .expect("prune");

        assert_eq!(pruned.removed, 3_000);
        assert_eq!(pruned.batches, 31);
        assert_eq!(gaps, 30);
        for counts in [&between, &beside] {
            assert_eq!(counts.lost.load(Ordering::Acquire), 0);
        }
        assert_eq!(between.commits.load(Ordering::Acquire), 30);
        assert_eq!(beside.commits.load(Ordering::Acquire), 20);
        let rows = query_rows(&node);
        assert_eq!(rows.len(), 50);
        assert!(rows.iter().all(|row| row.id.starts_with("audit_new_")));
    }
}
