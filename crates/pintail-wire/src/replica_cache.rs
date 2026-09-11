//! The process-wide cache of loaded replicas.
//!
//! A loaded replica is every table snapshot a database's queries read, and
//! each one costs a manifest read, a WAL replay into a fresh memtable and a
//! segment verification. Before this cache was shared, every wire connection
//! and every HTTP request built its own: sixty-four connections held
//! sixty-four copies of every memtable, none charged to any budget, and one
//! CDC commit to one table reloaded every table of the database on each of
//! those connections.
//!
//! Three properties, in the order they matter under memory pressure: one
//! copy per database per process; a table is reopened only when its own
//! files or schema changed; and the resident memtable bytes come out of the
//! same budget queries execute in, so a replica the process cannot afford is
//! served once and refused a slot rather than counted as free.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use pintail_exec::MemoryBudget;

/// A file's identity as far as change detection is concerned.
pub(crate) type FileStamp = (PathBuf, u64, Option<SystemTime>);

/// The metadata half of a stamp: the store's files, and a signature of the
/// rows a replica load actually reads (`MetaStore::replica_signature`).
///
/// Equality is the signature's alone. The files are how the signature is
/// found cheaply - unchanged files mean an unchanged signature and no read -
/// but they cannot decide staleness: an audit record or an API-key touch
/// moves them on every request without changing what a query sees, and
/// comparing them evicted every warm replica and sent otherwise eligible
/// short queries back to general admission.
#[derive(Clone, Debug, Default)]
pub(crate) struct MetadataStamp {
    /// The metadata store and its write-ahead log.
    pub(crate) files: Vec<FileStamp>,
    /// Signature of the database, table and schema-history rows.
    pub(crate) signature: u64,
}

impl PartialEq for MetadataStamp {
    fn eq(&self, other: &Self) -> bool {
        self.signature == other.signature
    }
}

impl Eq for MetadataStamp {}

/// One table's part of a stamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TableStamp {
    /// The generation this process's writer published for the table while
    /// it held the table's lock: nothing else could have changed the files.
    Published(u64),
    /// The table's files, walked because no writer here holds the table.
    Files(Vec<FileStamp>),
}

/// Everything on disk that can change what a query sees, attributed so a
/// change to one table's files is distinguishable from a metadata write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReplicaStamp {
    /// The metadata store: its files and their semantic signature.
    pub(crate) metadata: MetadataStamp,
    /// Each table's state, keyed by the table's directory name.
    pub(crate) tables: BTreeMap<String, TableStamp>,
}

impl ReplicaStamp {
    /// How many files were inspected, for the setup log line.
    pub(crate) fn files(&self) -> usize {
        self.metadata.files.len()
            + self
                .tables
                .values()
                .map(|table| match table {
                    TableStamp::Published(_) => 0,
                    TableStamp::Files(files) => files.len(),
                })
                .sum::<usize>()
    }

    /// How many tables were answered by their writer's generation.
    pub(crate) fn published(&self) -> usize {
        self.tables
            .values()
            .filter(|table| matches!(table, TableStamp::Published(_)))
            .count()
    }
}

/// One database's replica in one data directory. Two engines that share a
/// process but not a data directory - the integration tests do this - must
/// never see each other's tables.
pub(crate) type CacheKey = (PathBuf, String);

/// What the cache holds for a key, judged against the stamp just taken.
pub(crate) enum Lookup<R> {
    /// Nothing on disk changed since the load.
    Hit(Arc<R>),
    /// Something changed; the caller reloads, reusing whatever did not.
    Stale(Arc<R>, ReplicaStamp),
    Miss,
}

struct Entry<R> {
    stamp: ReplicaStamp,
    replica: Arc<R>,
    /// Bytes taken from the budget for this entry, returned on eviction.
    charged: usize,
    last_used: Instant,
    /// When this entry stops answering even though nothing on disk moved:
    /// a replica holding a table that would not open, which the next load
    /// after this instant tries again.
    retry_at: Option<Instant>,
}

/// What the cache has done since the process started.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplicaCacheStats {
    /// Databases currently resident.
    pub databases: usize,
    /// Memtable bytes those replicas hold, all charged to the process budget.
    pub resident_bytes: usize,
    /// Lookups answered without touching a table.
    pub hits: u64,
    /// Replica loads, whole or partial.
    pub loads: u64,
    /// Table snapshots opened across every load. A load that reused every
    /// table adds nothing here; that is the number a CDC commit should not
    /// move by more than one.
    pub tables_opened: u64,
    /// Loads the budget could not cover even after evicting everything
    /// else: served once, not cached.
    pub refused: u64,
    /// Table files freshness checks have had to inspect. A table whose
    /// writer is open in this process adds nothing: its published
    /// generation answers instead.
    pub walked_files: u64,
}

/// Loaded replicas keyed by database, bounded in count and in bytes.
pub(crate) struct ReplicaCache<R> {
    entries: Mutex<HashMap<CacheKey, Entry<R>>>,
    /// One lock per database, held by whichever query is reloading it, so a
    /// stamp that moved under twenty concurrent queries is replayed once and
    /// answered twenty times rather than replayed twenty times at once.
    reloads: Mutex<HashMap<CacheKey, Arc<Mutex<()>>>>,
    capacity: usize,
    budget: &'static MemoryBudget,
    hits: AtomicU64,
    loads: AtomicU64,
    tables_opened: AtomicU64,
    refused: AtomicU64,
    walked_files: AtomicU64,
}

impl<R> ReplicaCache<R> {
    /// A cache holding at most `capacity` databases, charging their memtables
    /// to `budget`. A capacity of zero is treated as one: a cache that can
    /// hold nothing would reload every table on every query.
    pub(crate) fn new(capacity: usize, budget: &'static MemoryBudget) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            reloads: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            budget,
            hits: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            tables_opened: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            walked_files: AtomicU64::new(0),
        }
    }

    /// Counts table files a stamp inspected.
    pub(crate) fn record_walk(&self, files: usize) {
        if files > 0 {
            self.walked_files.fetch_add(files as u64, Ordering::Relaxed);
        }
    }

    /// Returns a candidate without disk I/O. It must be revalidated before use.
    pub(crate) fn peek(&self, key: &CacheKey) -> Option<Arc<R>> {
        self.entries
            .lock()
            .ok()?
            .get(key)
            .map(|entry| Arc::clone(&entry.replica))
    }

    /// Judges the cached replica for `key` against `current`, the stamp
    /// just taken from disk.
    pub(crate) fn lookup(&self, key: &CacheKey, current: &ReplicaStamp) -> Lookup<R> {
        let mut entries = self.entries.lock().expect("replica cache lock");
        let Some(entry) = entries.get_mut(key) else {
            return Lookup::Miss;
        };
        let now = Instant::now();
        entry.last_used = now;
        let due = entry.retry_at.is_some_and(|retry_at| now >= retry_at);
        if entry.stamp == *current && !due {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Lookup::Hit(Arc::clone(&entry.replica));
        }
        Lookup::Stale(Arc::clone(&entry.replica), entry.stamp.clone())
    }

    /// Records a load and keeps the replica if the budget allows.
    ///
    /// The previous entry for `key` is released first, so a reload never
    /// double-charges. When the budget cannot cover `resident_bytes`, other
    /// databases go least-recently-used first; when nothing is left to
    /// evict the replica is handed back uncached and counted as refused,
    /// because caching it would mean holding memory the budget said the
    /// process does not have.
    /// `retry_after` bounds how long the entry answers when nothing on disk
    /// moves: a table that would not open may open on a later attempt, so
    /// the replica holding one expires on its own and the load after that
    /// reopens it - and it alone, since every other table's files are
    /// unchanged.
    pub(crate) fn insert(
        &self,
        key: CacheKey,
        stamp: ReplicaStamp,
        replica: Arc<R>,
        resident_bytes: usize,
        tables_opened: usize,
        retry_after: Option<Duration>,
    ) -> bool {
        self.loads.fetch_add(1, Ordering::Relaxed);
        self.tables_opened
            .fetch_add(tables_opened as u64, Ordering::Relaxed);
        let mut entries = self.entries.lock().expect("replica cache lock");
        if let Some(previous) = entries.remove(&key) {
            self.budget.release(previous.charged);
        }
        while entries.len() >= self.capacity && Self::evict_least_recent(&mut entries, self.budget)
        {
        }
        while self.budget.reserve(resident_bytes).is_err() {
            if !Self::evict_least_recent(&mut entries, self.budget) {
                self.refused.fetch_add(1, Ordering::Relaxed);
                pintail_log::log_info!(
                    "replica cache refused db={} resident={resident_bytes}B budget used={}B \
                     limit={}B: served uncached",
                    key.1,
                    self.budget.used(),
                    self.budget.limit()
                );
                return false;
            }
        }
        let now = Instant::now();
        entries.insert(
            key,
            Entry {
                stamp,
                replica,
                charged: resident_bytes,
                last_used: now,
                retry_at: retry_after.map(|after| now + after),
            },
        );
        true
    }

    /// The reload lock for `key`. Hold it across a reload, and look the key
    /// up again once it is held: the query that held it first may have
    /// loaded exactly the replica this one needs.
    pub(crate) fn reload_guard(&self, key: &CacheKey) -> Arc<Mutex<()>> {
        Arc::clone(
            self.reloads
                .lock()
                .expect("replica reload registry lock")
                .entry(key.clone())
                .or_default(),
        )
    }

    /// Drops the replica for `key`; the next read loads afresh.
    pub(crate) fn invalidate(&self, key: &CacheKey) {
        let mut entries = self.entries.lock().expect("replica cache lock");
        if let Some(previous) = entries.remove(key) {
            self.budget.release(previous.charged);
        }
    }

    pub(crate) fn stats(&self) -> ReplicaCacheStats {
        let entries = self.entries.lock().expect("replica cache lock");
        ReplicaCacheStats {
            databases: entries.len(),
            resident_bytes: entries.values().map(|entry| entry.charged).sum(),
            hits: self.hits.load(Ordering::Relaxed),
            loads: self.loads.load(Ordering::Relaxed),
            tables_opened: self.tables_opened.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            walked_files: self.walked_files.load(Ordering::Relaxed),
        }
    }

    fn evict_least_recent(
        entries: &mut HashMap<CacheKey, Entry<R>>,
        budget: &MemoryBudget,
    ) -> bool {
        let Some(key) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        if let Some(evicted) = entries.remove(&key) {
            budget.release(evicted.charged);
        }
        true
    }
}

/// Default bound on resident databases. Sized for a multi-tenant instance
/// rather than a single mirror: the cost of one more is its memtables,
/// which the budget bounds, so this mostly caps the stamp bookkeeping.
const DEFAULT_CAPACITY: usize = 32;

/// `PINTAIL_REPLICA_CACHE_DATABASES` overrides the resident-database bound.
pub(crate) fn default_capacity() -> usize {
    std::env::var("PINTAIL_REPLICA_CACHE_DATABASES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(limit: usize) -> &'static MemoryBudget {
        Box::leak(Box::new(MemoryBudget::new(limit)))
    }

    fn key(name: &str) -> CacheKey {
        (PathBuf::from("/data"), name.to_owned())
    }

    fn stamp(table: &str, len: u64) -> ReplicaStamp {
        let mut stamp = ReplicaStamp::default();
        stamp.tables.insert(
            table.to_owned(),
            TableStamp::Files(vec![(PathBuf::from(table), len, None)]),
        );
        stamp
    }

    #[test]
    fn an_unchanged_stamp_is_a_hit_and_a_changed_one_hands_back_the_old_replica() {
        let cache = ReplicaCache::new(4, budget(0));
        cache.insert(key("db"), stamp("t", 1), Arc::new("v1"), 0, 1, None);
        assert!(matches!(
            cache.lookup(&key("db"), &stamp("t", 1)),
            Lookup::Hit(replica) if *replica == "v1"
        ));
        match cache.lookup(&key("db"), &stamp("t", 2)) {
            Lookup::Stale(replica, previous) => {
                assert_eq!(*replica, "v1");
                assert_eq!(previous, stamp("t", 1));
            }
            _ => panic!("a changed stamp must hand back the previous replica"),
        }
        assert!(matches!(
            cache.lookup(&key("other"), &stamp("t", 1)),
            Lookup::Miss
        ));
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.loads, stats.tables_opened), (1, 1, 1));
    }

    /// A replica holding a table that would not open answers until its
    /// retry falls due, and is reloaded after that with nothing on disk
    /// having moved - where it used to be dropped, reloading every table on
    /// every query for as long as the table stayed shut.
    #[test]
    fn a_replica_with_a_retry_answers_until_it_falls_due() {
        let cache = ReplicaCache::new(4, budget(0));
        cache.insert(
            key("db"),
            stamp("t", 1),
            Arc::new("shut"),
            0,
            1,
            Some(Duration::from_millis(40)),
        );
        assert!(matches!(
            cache.lookup(&key("db"), &stamp("t", 1)),
            Lookup::Hit(replica) if *replica == "shut"
        ));
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            matches!(
                cache.lookup(&key("db"), &stamp("t", 1)),
                Lookup::Stale(replica, _) if *replica == "shut"
            ),
            "past the retry the load runs again, reusing what did not move"
        );
    }

    #[test]
    fn the_same_database_in_another_data_directory_is_a_different_replica() {
        let cache = ReplicaCache::new(4, budget(0));
        cache.insert(key("db"), stamp("t", 1), Arc::new("here"), 0, 1, None);
        let elsewhere = (PathBuf::from("/elsewhere"), "db".to_owned());
        assert!(matches!(
            cache.lookup(&elsewhere, &stamp("t", 1)),
            Lookup::Miss
        ));
    }

    #[test]
    fn the_least_recently_used_database_leaves_when_the_cache_is_full() {
        let cache = ReplicaCache::new(2, budget(0));
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 0, 1, None);
        cache.insert(key("b"), stamp("t", 1), Arc::new("b"), 0, 1, None);
        // Touch `a` so `b` is the oldest.
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Hit(_)
        ));
        cache.insert(key("c"), stamp("t", 1), Arc::new("c"), 0, 1, None);
        assert!(matches!(
            cache.lookup(&key("b"), &stamp("t", 1)),
            Lookup::Miss
        ));
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Hit(_)
        ));
        assert_eq!(cache.stats().databases, 2);
    }

    #[test]
    fn resident_bytes_are_charged_released_and_never_double_counted() {
        let budget = budget(1_000);
        let cache = ReplicaCache::new(4, budget);
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 400, 1, None);
        assert_eq!(budget.used(), 400);
        // A reload replaces the charge rather than adding to it.
        cache.insert(key("a"), stamp("t", 2), Arc::new("a2"), 300, 1, None);
        assert_eq!(budget.used(), 300);
        cache.invalidate(&key("a"));
        assert_eq!(budget.used(), 0);
        assert_eq!(cache.stats().resident_bytes, 0);
    }

    #[test]
    fn a_replica_the_budget_cannot_hold_evicts_others_first_then_goes_uncached() {
        let budget = budget(1_000);
        let cache = ReplicaCache::new(4, budget);
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 600, 1, None);
        cache.insert(key("b"), stamp("t", 1), Arc::new("b"), 300, 1, None);
        // 500 more does not fit beside both; `a` is the oldest and leaves.
        assert!(cache.insert(key("c"), stamp("t", 1), Arc::new("c"), 500, 1, None));
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Miss
        ));
        assert_eq!(budget.used(), 800);
        // Larger than the whole budget: nothing to evict helps.
        assert!(!cache.insert(key("d"), stamp("t", 1), Arc::new("d"), 1_500, 1, None));
        let stats = cache.stats();
        assert_eq!(stats.refused, 1);
        assert_eq!(stats.databases, 0, "eviction ran before the refusal");
        assert_eq!(
            budget.used(),
            0,
            "a refused replica must not leave a charge behind"
        );
    }

    #[test]
    fn a_zero_capacity_still_holds_one_database() {
        let cache = ReplicaCache::new(0, budget(0));
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 0, 1, None);
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Hit(_)
        ));
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    #[test]
    fn one_reload_lock_per_database() {
        let cache: ReplicaCache<()> =
            ReplicaCache::new(4, Box::leak(Box::new(MemoryBudget::new(0))));
        let here = (PathBuf::from("/data"), "db".to_owned());
        let there = (PathBuf::from("/data"), "other".to_owned());
        assert!(Arc::ptr_eq(
            &cache.reload_guard(&here),
            &cache.reload_guard(&here)
        ));
        assert!(!Arc::ptr_eq(
            &cache.reload_guard(&here),
            &cache.reload_guard(&there)
        ));
    }
}
