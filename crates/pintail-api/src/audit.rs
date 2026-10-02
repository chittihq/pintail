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
pub(crate) fn record_later(
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
    queue.push(Queued {
        id: random_identifier("audit_", 16),
        workspace_id,
        by_key: principal.database_id.is_some(),
        actor_id: principal.subject.clone(),
        action: action.to_owned(),
        target: target.map(|(kind, id)| (kind.to_owned(), id.to_owned())),
        detail_json: detail.map(|value| value.to_string()),
        created_at: Utc::now().to_rfc3339(),
        client_ip: principal.client_ip.clone(),
    });
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
}

/// Most events one commit carries; the rest wait for the next.
const BATCH_EVENTS: usize = 512;

/// What the queue has been given and what its writer has done with it.
#[derive(Default)]
struct Counts {
    queued: AtomicU64,
    written: AtomicU64,
    commits: AtomicU64,
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
}

/// The events waiting to be written, and the one thread that writes them.
pub(crate) struct Queue {
    metadata_path: PathBuf,
    /// The writer's inbox; the writer is started by the first event.
    sender: OnceLock<mpsc::Sender<Queued>>,
    counts: Arc<Counts>,
}

impl Queue {
    pub(crate) fn new(metadata_path: PathBuf) -> Self {
        Self {
            metadata_path,
            sender: OnceLock::new(),
            counts: Arc::new(Counts::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> QueueCounts {
        QueueCounts {
            queued: self.counts.queued.load(Ordering::Acquire),
            written: self.counts.written.load(Ordering::Acquire),
            commits: self.counts.commits.load(Ordering::Acquire),
        }
    }

    fn push(&self, event: Queued) {
        self.counts.queued.fetch_add(1, Ordering::AcqRel);
        let sender = self.sender.get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Queued>();
            let path = self.metadata_path.clone();
            let counts = Arc::clone(&self.counts);
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
                        write_batch(&path, &batch, &counts);
                    }
                });
            sender
        });
        if let Err(mpsc::SendError(event)) = sender.send(event) {
            // No writer: this thread writes the row itself.
            write_batch(&self.metadata_path, &[event], &self.counts);
        }
    }
}

/// Stores `batch` as one commit. If that fails - one event names a
/// workspace deleted since, say - each event is tried alone, so the others
/// are kept, and what cannot be stored is reported and dropped.
fn write_batch(metadata_path: &Path, batch: &[Queued], counts: &Counts) {
    let finished = u64::try_from(batch.len()).unwrap_or(u64::MAX);
    let metadata = match pintail_meta::MetaStore::open(metadata_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!(
                "audit log: failed to record {} event(s): {error}",
                batch.len()
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
    if metadata.record_audit_events(&rows).is_ok() {
        counts.commits.fetch_add(1, Ordering::AcqRel);
    } else {
        for row in &rows {
            match metadata.record_audit_event(row) {
                Ok(()) => {
                    counts.commits.fetch_add(1, Ordering::AcqRel);
                }
                Err(error) => eprintln!(
                    "audit log: failed to record '{}' in {}: {error}",
                    row.action, row.workspace_id
                ),
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
        write_batch(node.state.metadata_path().expect("path"), &batch, &counts);
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
        write_batch(node.state.metadata_path().expect("path"), &batch, &counts);
        assert_eq!(query_rows(&node).len(), 9);
        assert_eq!(counts.written.load(Ordering::Acquire), 10);
        assert_eq!(counts.commits.load(Ordering::Acquire), 9);
    }

    /// Events queued from several threads at once are all written, by the
    /// one writer, and an API-key session still records nothing.
    #[tokio::test]
    async fn queued_events_are_all_written() {
        let node = Node::new().await;
        let principal = session(&node);
        std::thread::scope(|scope| {
            for thread in 0..4 {
                let state = &node.state;
                let principal = &principal;
                scope.spawn(move || {
                    for serial in 0..50 {
                        record_later(
                            state,
                            principal,
                            "query.run",
                            Some(("database", "db_example")),
                            Some(serde_json::json!({"sql": "SELECT 1", "rows": thread * 50 + serial})),
                        );
                    }
                });
            }
        });
        let mut by_key = session(&node);
        by_key.workspace_id = None;
        by_key.database_id = Some("db_example".to_owned());
        record_later(&node.state, &by_key, "query.run", None, None);

        let queue = node.state.audit_queue().expect("queue");
        let deadline = Instant::now() + Duration::from_secs(20);
        while queue.counts().written < 200 {
            assert!(Instant::now() < deadline, "the writer never caught up");
            std::thread::sleep(Duration::from_millis(5));
        }
        let counts = queue.counts();
        assert_eq!(counts.queued, 200);
        assert!((1..=200).contains(&counts.commits), "{counts:?}");
        let rows = query_rows(&node);
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|row| {
            row.actor_label == "admin@example.com" && row.client_ip.as_deref() == Some("192.0.2.7")
        }));
    }
}
