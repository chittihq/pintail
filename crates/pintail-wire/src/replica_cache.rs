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
    time::{Instant, SystemTime},
};

use pintail_exec::MemoryBudget;

/// A file's identity as far as change detection is concerned.
pub(crate) type FileStamp = (PathBuf, u64, Option<SystemTime>);

/// Everything on disk that can change what a query sees, attributed so a
/// change to one table's files is distinguishable from a metadata write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReplicaStamp {
    /// The metadata store and its write-ahead log.
    pub(crate) metadata: Vec<FileStamp>,
    /// Each table's files, keyed by the table's directory name.
    pub(crate) tables: BTreeMap<String, Vec<FileStamp>>,
}

impl ReplicaStamp {
    /// How many files were inspected, for the setup log line.
    pub(crate) fn files(&self) -> usize {
        self.metadata.len() + self.tables.values().map(Vec::len).sum::<usize>()
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
}

/// Loaded replicas keyed by database, bounded in count and in bytes.
pub(crate) struct ReplicaCache<R> {
    entries: Mutex<HashMap<CacheKey, Entry<R>>>,
    capacity: usize,
    budget: &'static MemoryBudget,
    hits: AtomicU64,
    loads: AtomicU64,
    tables_opened: AtomicU64,
    refused: AtomicU64,
}

impl<R> ReplicaCache<R> {
    /// A cache holding at most `capacity` databases, charging their memtables
    /// to `budget`. A capacity of zero is treated as one: a cache that can
    /// hold nothing would reload every table on every query.
    pub(crate) fn new(capacity: usize, budget: &'static MemoryBudget) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            budget,
            hits: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            tables_opened: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }

    /// Judges the cached replica for `key` against `current`, the stamp
    /// just taken from disk.
    pub(crate) fn lookup(&self, key: &CacheKey, current: &ReplicaStamp) -> Lookup<R> {
        let mut entries = self.entries.lock().expect("replica cache lock");
        let Some(entry) = entries.get_mut(key) else {
            return Lookup::Miss;
        };
        entry.last_used = Instant::now();
        if entry.stamp == *current {
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
    pub(crate) fn insert(
        &self,
        key: CacheKey,
        stamp: ReplicaStamp,
        replica: Arc<R>,
        resident_bytes: usize,
        tables_opened: usize,
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
        entries.insert(
            key,
            Entry {
                stamp,
                replica,
                charged: resident_bytes,
                last_used: Instant::now(),
            },
        );
        true
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
        stamp
            .tables
            .insert(table.to_owned(), vec![(PathBuf::from(table), len, None)]);
        stamp
    }

    #[test]
    fn an_unchanged_stamp_is_a_hit_and_a_changed_one_hands_back_the_old_replica() {
        let cache = ReplicaCache::new(4, budget(0));
        cache.insert(key("db"), stamp("t", 1), Arc::new("v1"), 0, 1);
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

    #[test]
    fn the_same_database_in_another_data_directory_is_a_different_replica() {
        let cache = ReplicaCache::new(4, budget(0));
        cache.insert(key("db"), stamp("t", 1), Arc::new("here"), 0, 1);
        let elsewhere = (PathBuf::from("/elsewhere"), "db".to_owned());
        assert!(matches!(
            cache.lookup(&elsewhere, &stamp("t", 1)),
            Lookup::Miss
        ));
    }

    #[test]
    fn the_least_recently_used_database_leaves_when_the_cache_is_full() {
        let cache = ReplicaCache::new(2, budget(0));
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 0, 1);
        cache.insert(key("b"), stamp("t", 1), Arc::new("b"), 0, 1);
        // Touch `a` so `b` is the oldest.
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Hit(_)
        ));
        cache.insert(key("c"), stamp("t", 1), Arc::new("c"), 0, 1);
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
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 400, 1);
        assert_eq!(budget.used(), 400);
        // A reload replaces the charge rather than adding to it.
        cache.insert(key("a"), stamp("t", 2), Arc::new("a2"), 300, 1);
        assert_eq!(budget.used(), 300);
        cache.invalidate(&key("a"));
        assert_eq!(budget.used(), 0);
        assert_eq!(cache.stats().resident_bytes, 0);
    }

    #[test]
    fn a_replica_the_budget_cannot_hold_evicts_others_first_then_goes_uncached() {
        let budget = budget(1_000);
        let cache = ReplicaCache::new(4, budget);
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 600, 1);
        cache.insert(key("b"), stamp("t", 1), Arc::new("b"), 300, 1);
        // 500 more does not fit beside both; `a` is the oldest and leaves.
        assert!(cache.insert(key("c"), stamp("t", 1), Arc::new("c"), 500, 1));
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Miss
        ));
        assert_eq!(budget.used(), 800);
        // Larger than the whole budget: nothing to evict helps.
        assert!(!cache.insert(key("d"), stamp("t", 1), Arc::new("d"), 1_500, 1));
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
        cache.insert(key("a"), stamp("t", 1), Arc::new("a"), 0, 1);
        assert!(matches!(
            cache.lookup(&key("a"), &stamp("t", 1)),
            Lookup::Hit(_)
        ));
    }
}
