use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead as _, KeyInit as _},
};
use pintail_meta::MetaStore;
use pintail_wire::{DEFAULT_QUERY_MEMORY_LIMIT, ReplicaEngine};
use rand::RngCore as _;
use tokio::sync::broadcast;

use crate::{error::ApiError, events::ApiEvent};

const NONCE_BYTES: usize = 12;
const OAUTH_EXCHANGE_LIFETIME: Duration = Duration::from_secs(60);
const MAX_PENDING_OAUTH_EXCHANGES: usize = 256;

/// How long an operator action waits for a replication cycle to hand the
/// job slot over before the cycle is cut short.
const CYCLE_YIELD_GRACE: Duration = Duration::from_secs(8);
/// How long a reset lets whatever holds the slot finish before cancelling it.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// How long an operator action waits for a job that is not a cycle. Those
/// are real work - a copy, a backup - and are not interrupted for anything
/// short of a reset; the wait only covers the ones that finish in a moment.
const BUSY_PATIENCE: Duration = Duration::from_secs(3);
/// The longest an operator action waits for the slot before it is refused.
const OPERATOR_WAIT: Duration = Duration::from_secs(25);

/// Who holds a database's job slot, which decides who may take it from them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobKind {
    /// A supervisor replication cycle: repeats forever, so it yields to
    /// anything an operator asks for.
    Cycle,
    /// Work the supervisor started by itself (a table repair, a scheduled
    /// backup). Not started while an operator waits.
    Automatic,
    /// Work an operator asked for.
    Operator,
}

/// What an operator action may do to the job in its way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Preempt {
    /// End a replication cycle early; wait briefly for anything else and
    /// then say what is running.
    Yield,
    /// Cancel whatever holds the slot. Only for an action that discards the
    /// state the running job is writing.
    Cancel,
}

/// The line between a job and whoever wants its slot.
#[derive(Clone, Default)]
pub(crate) struct JobSignal {
    stop: pintail_cdc::CycleStop,
    cancelled: Arc<AtomicBool>,
}

impl JobSignal {
    /// Raised when the job should finish at its next safe point.
    pub(crate) fn stop(&self) -> pintail_cdc::CycleStop {
        self.stop.clone()
    }

    fn cancel(&self) {
        self.stop.request();
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Runs `job` until it finishes or the slot is taken from it, in which
    /// case the job is dropped where it stands and `None` comes back. That
    /// leaves what a process stopped at the same point would leave, which
    /// every job here already recovers from.
    pub(crate) async fn until_cancelled<T>(&self, job: impl Future<Output = T>) -> Option<T> {
        let cancelled = async {
            while !self.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            biased;
            () = cancelled => None,
            output = job => Some(output),
        }
    }
}

struct JobSlot {
    claim: String,
    since: Instant,
    kind: JobKind,
    signal: JobSignal,
}

impl JobSlot {
    fn describe(&self) -> String {
        format!(
            "{} has been running for {}s; it holds this database's job slot",
            self.claim,
            self.since.elapsed().as_secs()
        )
    }
}

/// One claim per database: what kind of job holds the slot and since when,
/// so a refusal can say what is actually running instead of leaving the
/// operator to guess whether to retry in seconds or minutes - and how many
/// operator actions are waiting for it, which keeps the supervisor from
/// taking it back first.
#[derive(Default)]
struct JobTable {
    active: std::collections::BTreeMap<String, JobSlot>,
    waiting: std::collections::BTreeMap<String, usize>,
}

/// Counts one waiting operator action for as long as it lives, including
/// when the request it belongs to is dropped mid-wait.
struct OperatorWaiting<'a> {
    state: &'a ApiState,
    database_id: &'a str,
}

impl Drop for OperatorWaiting<'_> {
    fn drop(&mut self) {
        if let Some(inner) = &self.state.inner
            && let Ok(mut jobs) = inner.jobs.lock()
            && let Some(count) = jobs.waiting.get_mut(self.database_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                jobs.waiting.remove(self.database_id);
            }
        }
    }
}

/// Shared configuration for Pintail's authenticated HTTP surface.
#[derive(Clone)]
pub struct ApiState {
    inner: Option<Arc<ApiStateInner>>,
    wire_bind: Option<SocketAddr>,
    query_memory_limit: usize,
}

struct ApiStateInner {
    metadata_path: PathBuf,
    data_dir: PathBuf,
    jwt_secret: Vec<u8>,
    dsn_key: [u8; 32],
    events: broadcast::Sender<ApiEvent>,
    /// One claim per database: what kind of job holds the slot and since
    /// when, so the 409 can say what is actually running instead of leaving
    /// the operator to guess whether to retry in seconds or minutes.
    jobs: Mutex<JobTable>,
    /// Last-known copy progress per (database, table), retained so a
    /// dashboard that loads MID-copy (reload, second browser) can seed its
    /// progress bar instead of waiting for the next SSE frame. Entries live
    /// exactly as long as the run: written by `*.progress` events, removed by
    /// the completion/error/interrupted events, all inside `publish`.
    table_progress: Mutex<HashMap<(String, String), TableProgress>>,
    oauth_exchanges: Mutex<HashMap<String, PendingOauthExchange>>,
    metrics: RuntimeMetrics,
    /// One engine held for the process's life, built once here instead of
    /// per request. `ReplicaEngine` carries the process-wide replica cache
    /// as an `Arc` already, but its per-instance metadata-signature memo and
    /// its cached signature-reader connection are NOT shared - a fresh
    /// instance per HTTP request silently lost both on every call, paying a
    /// `MetaStore::open` it did not need to. A handler clones this (an Arc
    /// clone plus two path clones) and calls `with_memory_limit` on the
    /// clone, so per-request memory-limit and cancellation behaviour are
    /// unchanged.
    replica_engine: ReplicaEngine,
    /// API keys validated recently, keyed by the SHA-256 of the presented
    /// secret. A hit skips the metadata read (hash lookup, enabled/expiry
    /// check) that used to run on every authenticated request; cleared
    /// whenever a key is disabled or deleted (`invalidate_api_keys`) so a
    /// revoked key stops working on its next request rather than at the
    /// entry's next natural eviction.
    api_key_cache: Mutex<HashMap<[u8; 32], CachedApiKey>>,
    /// When the supervisor last started a catalog repair per database, so a
    /// repair that cannot succeed is retried on a gap rather than every
    /// cadence (`upstream::repair_catalog_drift`).
    catalog_repairs: Mutex<HashMap<String, std::time::Instant>>,
    /// Audit events of the actions that arrive at the workload's rate,
    /// waiting for the one thread that writes them (`audit::record_later`).
    audit: crate::audit::Queue,
}

#[derive(Clone)]
pub(crate) struct CachedApiKey {
    pub(crate) id: String,
    pub(crate) database_id: String,
    pub(crate) scopes: Vec<String>,
    /// When this entry was validated against metadata. Doubles as the
    /// cache's TTL clock (a request past `max_age` re-validates, which is
    /// also what bounds how stale a disabled-or-expired key can read) and
    /// as the `touch_api_key` throttle - last-used-at moves once per
    /// refresh instead of once per request.
    last_touched: Instant,
}

#[derive(Clone)]
pub(crate) struct TableProgress {
    pub(crate) rows: u64,
    pub(crate) eta_seconds: Option<u64>,
    started: Instant,
    updated: Instant,
}

impl TableProgress {
    pub(crate) fn elapsed_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub(crate) fn age_seconds(&self) -> u64 {
        self.updated.elapsed().as_secs()
    }
}

struct PendingOauthExchange {
    token: String,
    outcome: String,
    expires_at: Instant,
}

pub(crate) struct OauthExchange {
    pub(crate) token: String,
    pub(crate) outcome: String,
}

#[derive(Default)]
struct RuntimeMetrics {
    queries: AtomicU64,
    query_rows: AtomicU64,
    query_duration_ms: AtomicU64,
    replication_cycles: AtomicU64,
    replication_errors: AtomicU64,
    ingested_rows: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeMetricsSnapshot {
    pub(crate) queries: u64,
    pub(crate) query_rows: u64,
    pub(crate) query_duration_ms: u64,
    pub(crate) replication_cycles: u64,
    pub(crate) replication_errors: u64,
    pub(crate) ingested_rows: u64,
}

impl ApiState {
    /// Builds configured API state.
    ///
    /// # Errors
    ///
    /// Returns an error when the DSN key is not exactly 32 hex-encoded bytes
    /// or the metadata store cannot be opened.
    pub fn new(
        data_dir: impl Into<PathBuf>,
        metadata_path: impl Into<PathBuf>,
        jwt_secret: impl Into<Vec<u8>>,
        dsn_encryption_key: &str,
    ) -> Result<Self> {
        let metadata_path = metadata_path.into();
        MetaStore::open(&metadata_path)?;
        let dsn_key = decode_hex_key(dsn_encryption_key)?;
        let (events, _) = broadcast::channel(256);
        let data_dir = data_dir.into();
        let replica_engine = ReplicaEngine::new(data_dir.clone(), metadata_path.clone());
        Ok(Self {
            inner: Some(Arc::new(ApiStateInner {
                audit: crate::audit::Queue::new(metadata_path.clone()),
                metadata_path,
                data_dir,
                jwt_secret: jwt_secret.into(),
                dsn_key,
                events,
                jobs: Mutex::new(JobTable::default()),
                table_progress: Mutex::new(HashMap::new()),
                oauth_exchanges: Mutex::new(HashMap::new()),
                metrics: RuntimeMetrics::default(),
                replica_engine,
                api_key_cache: Mutex::new(HashMap::new()),
                catalog_repairs: Mutex::new(HashMap::new()),
            })),
            wire_bind: None,
            query_memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        })
    }

    pub(crate) const fn unconfigured() -> Self {
        Self {
            inner: None,
            wire_bind: None,
            query_memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        }
    }

    /// Records the wire endpoint exposed by the process hosting this API.
    #[must_use]
    pub const fn with_wire_bind(mut self, wire_bind: SocketAddr) -> Self {
        self.wire_bind = Some(wire_bind);
        self
    }

    /// Sets the hard byte ceiling used by each HTTP query.
    #[must_use]
    pub const fn with_query_memory_limit(mut self, query_memory_limit: usize) -> Self {
        self.query_memory_limit = query_memory_limit;
        self
    }

    pub(crate) const fn wire_bind(&self) -> Option<SocketAddr> {
        self.wire_bind
    }

    pub(crate) const fn query_memory_limit(&self) -> usize {
        self.query_memory_limit
    }

    pub(crate) fn metadata(&self) -> Result<MetaStore, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        MetaStore::open(&inner.metadata_path).map_err(ApiError::internal)
    }

    pub(crate) fn jwt_secret(&self) -> Result<&[u8], ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.jwt_secret.as_slice())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn data_dir(&self) -> Result<&Path, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.data_dir.as_path())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn metadata_path(&self) -> Result<&Path, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.metadata_path.as_path())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    /// The queue of audit events waiting for their writer.
    pub(crate) fn audit_queue(&self) -> Option<&crate::audit::Queue> {
        self.inner.as_ref().map(|inner| &inner.audit)
    }

    /// The process-wide query engine. Cloning is cheap (an `Arc` clone of
    /// the replica cache, admission gate, signature memo and signature
    /// reader, plus two `PathBuf` clones) and shares all of it with every
    /// other request, which is the point: a fresh `ReplicaEngine::new` per
    /// request started that memo and reader over from empty.
    pub(crate) fn replica_engine(&self) -> Result<ReplicaEngine, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.replica_engine.clone())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    /// A validated API key from the cache, keyed by the SHA-256 of the
    /// presented secret, when it was cached less than `max_age` ago -
    /// bounding how long a disabled-but-not-yet-invalidated entry (a crash
    /// between the metadata write and the cache clear, say) could still
    /// authenticate.
    pub(crate) fn cached_api_key(
        &self,
        digest: &[u8; 32],
        max_age: Duration,
    ) -> Option<CachedApiKey> {
        let inner = self.inner.as_ref()?;
        let cache = inner.api_key_cache.lock().ok()?;
        let cached = cache.get(digest)?;
        (cached.last_touched.elapsed() < max_age).then(|| cached.clone())
    }

    pub(crate) fn cache_api_key(
        &self,
        digest: [u8; 32],
        id: String,
        database_id: String,
        scopes: Vec<String>,
    ) {
        if let Some(inner) = &self.inner
            && let Ok(mut cache) = inner.api_key_cache.lock()
        {
            cache.insert(
                digest,
                CachedApiKey {
                    id,
                    database_id,
                    scopes,
                    last_touched: Instant::now(),
                },
            );
        }
    }

    /// Drops every cached entry for one key id. Called on disable/delete so
    /// a revoked key stops authenticating on its very next request instead
    /// of waiting out the cache's max age.
    pub(crate) fn invalidate_api_key(&self, id: &str) {
        if let Some(inner) = &self.inner
            && let Ok(mut cache) = inner.api_key_cache.lock()
        {
            cache.retain(|_, cached| cached.id != id);
        }
    }

    pub(crate) fn encrypt_dsn(&self, dsn: &str) -> Result<Vec<u8>, ApiError> {
        self.encrypt_secret(dsn)
    }

    pub(crate) fn encrypt_secret(&self, secret: &str) -> Result<Vec<u8>, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(&inner.dsn_key).map_err(ApiError::internal)?;
        let mut nonce = [0_u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let nonce_array = Nonce::try_from(nonce.as_slice()).map_err(ApiError::internal)?;
        let encrypted = cipher
            .encrypt(&nonce_array, secret.as_bytes())
            .map_err(ApiError::internal)?;
        let mut encoded = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        encoded.extend_from_slice(&nonce);
        encoded.extend_from_slice(&encrypted);
        Ok(encoded)
    }

    pub(crate) fn decrypt_dsn(&self, encrypted: &[u8]) -> Result<String, ApiError> {
        self.decrypt_secret(encrypted)
    }

    pub(crate) fn decrypt_secret(&self, encrypted: &[u8]) -> Result<String, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let (nonce, ciphertext) = encrypted
            .split_at_checked(NONCE_BYTES)
            .ok_or_else(|| ApiError::internal("encrypted secret is truncated"))?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(&inner.dsn_key).map_err(ApiError::internal)?;
        let nonce = Nonce::try_from(nonce).map_err(ApiError::internal)?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(ApiError::internal)?;
        String::from_utf8(plaintext).map_err(ApiError::internal)
    }

    pub(crate) fn subscribe(&self) -> Result<broadcast::Receiver<ApiEvent>, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.events.subscribe())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn publish(&self, event: ApiEvent) {
        // Every event is logged here rather than at each call site, so an
        // event kind added later is logged without anyone remembering to.
        //
        // This is also the only place they are guaranteed to survive: the
        // broadcast send below drops the event when nothing is subscribed, and
        // a supervisor failing at 3am has no dashboard open. Replication
        // errors used to exist solely as an SSE frame nobody received plus a
        // control-plane row written with `let _ =`, so `docker logs` showed
        // the two startup lines and nothing else.
        let level = if event.kind.contains("error") || event.kind.contains("failed") {
            pintail_log::ERROR
        } else if event.kind.ends_with(".progress") {
            // Emitted per replication cycle, which is every poll interval on
            // every database. Useful when watching a specific problem, ruinous
            // as a default.
            pintail_log::DEBUG
        } else {
            pintail_log::INFO
        };
        if pintail_log::enabled(level) {
            let scope = event.database_id.as_deref().unwrap_or("-");
            let table = event
                .table
                .as_deref()
                .map(|name| format!(" table={name}"))
                .unwrap_or_default();
            let counts = match (event.rows, event.bytes) {
                (Some(rows), Some(bytes)) => format!(" rows={rows} bytes={bytes}"),
                (Some(rows), None) => format!(" rows={rows}"),
                (None, Some(bytes)) => format!(" bytes={bytes}"),
                (None, None) => String::new(),
            };
            // At the level worked out above: `emit` logs at info, and the
            // exporter forwards errors only, so no replication error ever
            // reached it - a source that forced a full recopy left no trace
            // outside the container log.
            pintail_log::emit_at(
                level,
                &format!("{} db={scope}{table}{counts} {}", event.kind, event.message),
            );
        }
        self.retain_progress(&event);
        if let Some(inner) = &self.inner {
            let _ = inner.events.send(event);
        }
    }

    /// Mirrors the dashboard's own SSE bookkeeping so the two can never
    /// disagree: the same frames that move its live bar move this map.
    fn retain_progress(&self, event: &ApiEvent) {
        let Some(inner) = &self.inner else { return };
        let Some(database_id) = event.database_id.as_deref() else {
            return;
        };
        let Ok(mut map) = inner.table_progress.lock() else {
            return;
        };
        if event.kind.ends_with(".progress") {
            if let Some(table) = event.table.as_deref() {
                let key = (database_id.to_owned(), table.to_owned());
                let started = map.get(&key).map_or_else(Instant::now, |kept| kept.started);
                map.insert(
                    key,
                    TableProgress {
                        rows: event.rows.unwrap_or(0),
                        eta_seconds: event.eta_seconds,
                        started,
                        updated: Instant::now(),
                    },
                );
            }
        } else if matches!(
            event.kind.as_str(),
            "resnapshot.completed"
                | "snapshot.completed"
                | "resnapshot.error"
                | "resnapshot.interrupted"
        ) {
            match event.table.as_deref() {
                Some(table) => {
                    map.remove(&(database_id.to_owned(), table.to_owned()));
                }
                // A database-level completion ends every table's copy.
                None => map.retain(|(kept_database, _), _| kept_database != database_id),
            }
        }
    }

    pub(crate) fn table_progress(&self, database_id: &str, table: &str) -> Option<TableProgress> {
        let inner = self.inner.as_ref()?;
        let map = inner.table_progress.lock().ok()?;
        map.get(&(database_id.to_owned(), table.to_owned()))
            .cloned()
    }

    /// Claims the slot for one replication cycle. Refused while an operator
    /// action waits for it: a cycle that restarts the moment the last one
    /// ends would otherwise keep every such action out for good.
    pub(crate) fn acquire_job(&self, database_id: &str) -> Result<JobSignal, ApiError> {
        self.claim_job(database_id, "a replication cycle", JobKind::Cycle)
    }

    /// Claims the slot for work the supervisor starts by itself, which
    /// gives way to a waiting operator the way a cycle does.
    pub(crate) fn acquire_automatic_job(
        &self,
        database_id: &str,
        claim: &str,
    ) -> Result<JobSignal, ApiError> {
        self.claim_job(database_id, claim, JobKind::Automatic)
    }

    /// Claims the database's one job slot under a human-readable name. On
    /// conflict the refusal names the running job and its age - "retry in a
    /// moment" and "a snapshot has been copying for four minutes" demand
    /// different operator responses, and the bare message distinguished
    /// neither.
    /// Refuses a source operation against a LOCAL database.
    ///
    /// Local databases have no DSN, no probe and no source to talk to, so
    /// probing, snapshotting, resnapshotting, reconciling and dead-letter
    /// retries are meaningless against them rather than merely unsupported
    /// (`docs/design/writable-mode.md`). Backups are deliberately NOT
    /// guarded: they read the manifest objects a local database has like any
    /// other, which is why this is a per-entry-point check and not one
    /// inside the job-slot claim.
    ///
    /// # Errors
    ///
    /// Returns a conflict for a local database, and an internal error when
    /// the control plane cannot be read.
    pub(crate) fn require_replicated(
        &self,
        database_id: &str,
        operation: &str,
    ) -> Result<(), ApiError> {
        if self
            .metadata()?
            .is_local_database(database_id)
            .map_err(ApiError::internal)?
        {
            return Err(ApiError::conflict(format!(
                "{operation} needs a replicated source; this is a local database"
            )));
        }
        Ok(())
    }

    fn claim_job(
        &self,
        database_id: &str,
        claim: &str,
        kind: JobKind,
    ) -> Result<JobSignal, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let mut jobs = inner.jobs.lock().map_err(ApiError::internal)?;
        if kind != JobKind::Operator && jobs.waiting.contains_key(database_id) {
            return Err(ApiError::conflict(
                "an operator action is waiting for this database's job slot",
            ));
        }
        match jobs.active.entry(database_id.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let signal = JobSignal::default();
                entry.insert(JobSlot {
                    claim: claim.to_owned(),
                    since: Instant::now(),
                    kind,
                    signal: signal.clone(),
                });
                Ok(signal)
            }
            std::collections::btree_map::Entry::Occupied(entry) => Err(ApiError::conflict(
                format!("{} - retry when it completes", entry.get().describe()),
            )),
        }
    }

    /// Claims the slot for an operator's action, ahead of the supervisor.
    ///
    /// The supervisor takes the slot for every replication cycle and takes
    /// it again as soon as it can, and a cycle on a source written faster
    /// than it is applied does not end at all. An immediate claim therefore
    /// lost to the supervisor for as long as that lasted - minutes of
    /// refusals for a reset, a resnapshot or an added table, on exactly the
    /// databases that needed them. So the claim is queued instead: while it
    /// waits the supervisor starts nothing new for this database, a running
    /// cycle is asked to return at its next commit and is cut short if it
    /// does not, and the action is admitted behind at most that one cycle.
    ///
    /// # Errors
    ///
    /// Returns a conflict naming the job that holds the slot when that job
    /// is not one to interrupt, or did not let go in time.
    pub(crate) async fn acquire_operator_job(
        &self,
        database_id: &str,
        claim: &str,
        preempt: Preempt,
    ) -> Result<JobSignal, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        {
            let mut jobs = inner.jobs.lock().map_err(ApiError::internal)?;
            *jobs.waiting.entry(database_id.to_owned()).or_default() += 1;
        }
        let _waiting = OperatorWaiting {
            state: self,
            database_id,
        };
        let asked = Instant::now();
        loop {
            {
                let mut jobs = inner.jobs.lock().map_err(ApiError::internal)?;
                let Some(slot) = jobs.active.get(database_id) else {
                    let signal = JobSignal::default();
                    jobs.active.insert(
                        database_id.to_owned(),
                        JobSlot {
                            claim: claim.to_owned(),
                            since: Instant::now(),
                            kind: JobKind::Operator,
                            signal: signal.clone(),
                        },
                    );
                    return Ok(signal);
                };
                let waited = asked.elapsed();
                match (preempt, slot.kind) {
                    (Preempt::Cancel, _) => {
                        slot.signal.stop.request();
                        if waited >= CANCEL_GRACE {
                            slot.signal.cancel();
                        }
                    }
                    (Preempt::Yield, JobKind::Cycle) => {
                        slot.signal.stop.request();
                        if waited >= CYCLE_YIELD_GRACE {
                            slot.signal.cancel();
                        }
                    }
                    (Preempt::Yield, JobKind::Automatic | JobKind::Operator) => {
                        if waited >= BUSY_PATIENCE {
                            return Err(ApiError::conflict(format!(
                                "{} - retry when it completes",
                                slot.describe()
                            )));
                        }
                    }
                }
                if waited >= OPERATOR_WAIT {
                    return Err(ApiError::conflict(format!(
                        "{} and did not stop within {}s of being asked to - retry shortly",
                        slot.describe(),
                        OPERATOR_WAIT.as_secs()
                    )));
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Renames the claim a held slot carries, when one action hands the
    /// slot to the next step of the same work.
    pub(crate) fn relabel_job(&self, database_id: &str, claim: &str) {
        if let Some(inner) = &self.inner
            && let Ok(mut jobs) = inner.jobs.lock()
            && let Some(slot) = jobs.active.get_mut(database_id)
        {
            claim.clone_into(&mut slot.claim);
        }
    }

    /// What holds the database's job slot, if anything, for status output.
    pub(crate) fn job_holder(&self, database_id: &str) -> Option<(String, u64)> {
        let inner = self.inner.as_ref()?;
        let jobs = inner.jobs.lock().ok()?;
        let slot = jobs.active.get(database_id)?;
        Some((slot.claim.clone(), slot.since.elapsed().as_secs()))
    }

    /// Runs `action` over the per-database catalog-repair clock; `None` when
    /// the API is unconfigured or the lock is poisoned.
    pub(crate) fn with_catalog_repairs<T>(
        &self,
        action: impl FnOnce(&mut HashMap<String, std::time::Instant>) -> T,
    ) -> Option<T> {
        let inner = self.inner.as_ref()?;
        let mut repairs = inner.catalog_repairs.lock().ok()?;
        Some(action(&mut repairs))
    }

    pub(crate) fn release_job(&self, database_id: &str) {
        if let Some(inner) = &self.inner
            && let Ok(mut jobs) = inner.jobs.lock()
        {
            jobs.active.remove(database_id);
        }
    }

    /// Stores a session token behind a short-lived, one-time browser exchange
    /// code. The callback URL carries only the opaque code, never the JWT.
    pub(crate) fn create_oauth_exchange(
        &self,
        token: String,
        outcome: &str,
    ) -> Result<String, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let now = Instant::now();
        let mut exchanges = inner.oauth_exchanges.lock().map_err(ApiError::internal)?;
        exchanges.retain(|_, exchange| exchange.expires_at > now);
        if exchanges.len() >= MAX_PENDING_OAUTH_EXCHANGES {
            return Err(ApiError::unavailable(
                "too many sign-in exchanges are pending; try again shortly",
            ));
        }
        let code = random_identifier("oauth_", 24);
        exchanges.insert(
            code.clone(),
            PendingOauthExchange {
                token,
                outcome: outcome.to_owned(),
                expires_at: now + OAUTH_EXCHANGE_LIFETIME,
            },
        );
        Ok(code)
    }

    /// Consumes a browser exchange exactly once.
    pub(crate) fn consume_oauth_exchange(&self, code: &str) -> Result<OauthExchange, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let now = Instant::now();
        let mut exchanges = inner.oauth_exchanges.lock().map_err(ApiError::internal)?;
        exchanges.retain(|_, exchange| exchange.expires_at > now);
        exchanges
            .remove(code)
            .map(|exchange| OauthExchange {
                token: exchange.token,
                outcome: exchange.outcome,
            })
            .ok_or_else(|| ApiError::bad_request("sign-in exchange code is invalid or expired"))
    }

    pub(crate) fn record_query(&self, duration_ms: u64, rows: u64) {
        if let Some(inner) = &self.inner {
            inner.metrics.queries.fetch_add(1, Ordering::Relaxed);
            inner.metrics.query_rows.fetch_add(rows, Ordering::Relaxed);
            inner
                .metrics
                .query_duration_ms
                .fetch_add(duration_ms, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_replication_cycle(&self, rows: u64, succeeded: bool) {
        if let Some(inner) = &self.inner {
            inner
                .metrics
                .replication_cycles
                .fetch_add(1, Ordering::Relaxed);
            inner
                .metrics
                .ingested_rows
                .fetch_add(rows, Ordering::Relaxed);
            if !succeeded {
                inner
                    .metrics
                    .replication_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn runtime_metrics(&self) -> RuntimeMetricsSnapshot {
        self.inner
            .as_ref()
            .map_or_else(RuntimeMetricsSnapshot::default, |inner| {
                RuntimeMetricsSnapshot {
                    queries: inner.metrics.queries.load(Ordering::Relaxed),
                    query_rows: inner.metrics.query_rows.load(Ordering::Relaxed),
                    query_duration_ms: inner.metrics.query_duration_ms.load(Ordering::Relaxed),
                    replication_cycles: inner.metrics.replication_cycles.load(Ordering::Relaxed),
                    replication_errors: inner.metrics.replication_errors.load(Ordering::Relaxed),
                    ingested_rows: inner.metrics.ingested_rows.load(Ordering::Relaxed),
                }
            })
    }
}

pub(crate) fn random_identifier(prefix: &str, bytes: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    let mut output = String::with_capacity(prefix.len() + bytes * 2);
    output.push_str(prefix);
    for byte in random {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex_key(encoded: &str) -> Result<[u8; 32]> {
    if encoded.len() != 64 {
        bail!("DSN encryption key must contain 64 hexadecimal characters");
    }
    let mut decoded = [0_u8; 32];
    for (index, output) in decoded.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&encoded[offset..offset + 2], 16)
            .with_context(|| format!("DSN encryption key has invalid hex at byte {index}"))?;
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::ApiState;
    use std::time::Duration;

    fn state() -> ApiState {
        let data = tempfile::tempdir().expect("temporary API state");
        ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"jwt-secret",
            &"11".repeat(32),
        )
        .expect("API state")
    }

    #[test]
    fn cached_api_key_is_visible_only_within_its_max_age() {
        let state = state();
        let digest = [7_u8; 32];
        assert!(
            state
                .cached_api_key(&digest, Duration::from_secs(30))
                .is_none()
        );
        state.cache_api_key(
            digest,
            "key_1".to_owned(),
            "db_1".to_owned(),
            vec!["query".to_owned()],
        );
        let cached = state
            .cached_api_key(&digest, Duration::from_secs(30))
            .expect("cached within max age");
        assert_eq!(cached.id, "key_1");
        assert_eq!(cached.database_id, "db_1");
        assert_eq!(cached.scopes, vec!["query".to_owned()]);
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            state
                .cached_api_key(&digest, Duration::from_millis(1))
                .is_none(),
            "an entry older than max_age must miss, so a re-validated enabled/expiry check runs"
        );
    }

    #[test]
    fn invalidating_an_api_key_drops_every_cache_entry_for_its_id() {
        let state = state();
        let revoked = [1_u8; 32];
        let other = [2_u8; 32];
        state.cache_api_key(revoked, "key_revoked".to_owned(), "db".to_owned(), vec![]);
        state.cache_api_key(other, "key_other".to_owned(), "db".to_owned(), vec![]);
        state.invalidate_api_key("key_revoked");
        assert!(
            state
                .cached_api_key(&revoked, Duration::from_secs(30))
                .is_none()
        );
        assert!(
            state
                .cached_api_key(&other, Duration::from_secs(30))
                .is_some()
        );
    }

    #[test]
    fn dsn_encryption_is_randomized_and_authenticated() {
        let data = tempfile::tempdir().expect("temporary API state");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"jwt-secret",
            &"11".repeat(32),
        )
        .expect("API state");
        let first = state.encrypt_dsn("mysql://source/app").unwrap();
        let second = state.encrypt_dsn("mysql://source/app").unwrap();
        assert_ne!(first, second);
        assert_eq!(state.decrypt_dsn(&first).unwrap(), "mysql://source/app");
        let mut corrupt = first;
        *corrupt.last_mut().expect("ciphertext") ^= 1;
        assert!(state.decrypt_dsn(&corrupt).is_err());
    }

    #[test]
    fn oauth_exchange_codes_are_one_time() {
        let data = tempfile::tempdir().expect("temporary API state");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"jwt-secret",
            &"11".repeat(32),
        )
        .expect("API state");
        let code = state
            .create_oauth_exchange("session-token".to_owned(), "linked")
            .expect("create exchange");
        let exchange = state
            .consume_oauth_exchange(&code)
            .expect("consume exchange");
        assert_eq!(exchange.token, "session-token");
        assert_eq!(exchange.outcome, "linked");
        assert!(state.consume_oauth_exchange(&code).is_err());
    }
}
