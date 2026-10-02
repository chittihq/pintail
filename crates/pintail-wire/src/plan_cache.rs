//! Statements kept prepared between executions.
//!
//! Parsing, binding and planning a small statement costs more than running
//! it: a key lookup spends most of its time before the first block is read.
//! A statement sent again - a prepared statement executed with the values
//! it had before, a text statement repeated - is prepared the same way
//! every time, so the preparation is kept and the next execution starts
//! from it.
//!
//! What is kept is the plan, never rows: every execution still runs. So the
//! one question is whether a kept plan is the plan a fresh preparation
//! would have made, and the answer is the key. Preparation reads the
//! statement text, the loaded replica (its catalog, its column facts and,
//! for a join, its column statistics) and the session settings installed on
//! the thread - which is exactly what [`SharedQueryKey`] names for sharing
//! one execution between identical requests, so the two use one key and an
//! input added to one is added to the other:
//!
//! - the replica by its load number, taken fresh on every load: a schema
//!   change, a commit or a local write puts the same text on another key.
//!   The load also carries the database, so two databases - two users'
//!   scopes - never share an entry;
//! - the exact statement text, literals, hints and `LIMIT` included. A
//!   plan can depend on a literal's value (its type, a folded constant, a
//!   pruning bound), so nothing is lifted out of the text;
//! - the row ceiling and every session setting preparation or execution
//!   reads: `sql_mode` as the flags it parses to, the time zone, the pinned
//!   timestamp and the session date, the connection and client character
//!   sets, the connection collation, and the caps and precision settings.
//!
//! Only a statement whose answer cannot depend on anything else is kept:
//! one [`pintail_sql::is_repeatable_statement`] accepts, so nothing that
//! reads the clock, a user or system variable or the connection, and one
//! whose preparation raised no warning, so an execution from the kept plan
//! owes the client no warning that only preparing would have raised.
//!
//! A plan is kept the second time its statement is prepared, not the
//! first: a connection sending statements that never repeat - every key
//! spelled into its own text - would otherwise pay for keeping each plan
//! and for evicting another, and gain nothing.
//!
//! The cache is bounded by entries and by bytes; the least recently used
//! entries leave first. A plan's size is estimated from its statement's
//! length, which it grows with: the bound is on the order of memory held,
//! not an exact count.

use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::shared_query::SharedQueryKey;

/// Entries kept by default: `PINTAIL_PLAN_CACHE_ENTRIES` overrides it, and
/// zero turns the cache off.
pub(crate) const DEFAULT_ENTRIES: usize = 4096;

/// Estimated bytes kept by default: `PINTAIL_PLAN_CACHE_BYTES` overrides it,
/// and zero turns the cache off.
pub(crate) const DEFAULT_BYTES: usize = 64 * 1024 * 1024;

/// A plan's estimated size per byte of its statement's text: the operators,
/// expressions and result metadata a statement binds to are several times
/// the size of the text that names them.
const PLAN_BYTES_PER_STATEMENT_BYTE: usize = 48;

/// What every entry holds whatever its statement: the key's fixed part and
/// the plan's own.
const ENTRY_OVERHEAD_BYTES: usize = 2048;

/// Above this many entries the cache is split, so connections looking up
/// different statements do not wait on one lock.
const SHARDED_FROM_ENTRIES: usize = 256;
const SHARDS: usize = 16;

/// How many once-prepared statements are remembered while they wait to be
/// seen again.
const SEEN_ONCE_SLOTS: usize = 4096;

/// The estimated bytes an entry for `sql` holds.
pub(crate) fn estimated_bytes(sql: &str) -> usize {
    ENTRY_OVERHEAD_BYTES + sql.len().saturating_mul(PLAN_BYTES_PER_STATEMENT_BYTE + 1)
}

struct Slot<V> {
    value: Arc<V>,
    bytes: usize,
    /// When the entry was last answered from, on the cache's own clock.
    used: AtomicU64,
}

struct Shard<V> {
    entries: HashMap<SharedQueryKey, Slot<V>>,
    bytes: usize,
}

/// What the cache has done since it was made.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlanCacheStats {
    /// Executions that started from a kept plan.
    pub hits: u64,
    /// Lookups that found nothing.
    pub misses: u64,
    /// Plans kept.
    pub inserted: u64,
    /// Plans dropped to stay inside the bounds, or because their replica
    /// was replaced.
    pub evicted: u64,
    /// Plans held now.
    pub entries: u64,
    /// Estimated bytes held now.
    pub bytes: u64,
}

/// Prepared statements by everything their preparation read.
pub(crate) struct PlanCache<V> {
    shards: Box<[RwLock<Shard<V>>]>,
    hasher: RandomState,
    entries_per_shard: usize,
    bytes_per_shard: usize,
    /// Statements prepared once and not kept yet, by a hash of their key:
    /// a plan is kept when its statement is seen here a second time.
    seen: Box<[AtomicU64]>,
    clock: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    inserted: AtomicU64,
    evicted: AtomicU64,
}

impl<V> PlanCache<V> {
    /// A cache holding at most `entries` plans of at most `bytes` estimated
    /// bytes together.
    pub(crate) fn new(entries: usize, bytes: usize) -> Self {
        let shards = if entries >= SHARDED_FROM_ENTRIES {
            SHARDS
        } else {
            1
        };
        Self {
            shards: (0..shards)
                .map(|_| {
                    RwLock::new(Shard {
                        entries: HashMap::new(),
                        bytes: 0,
                    })
                })
                .collect(),
            hasher: RandomState::new(),
            seen: (0..SEEN_ONCE_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            entries_per_shard: entries / shards,
            bytes_per_shard: bytes / shards,
            clock: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserted: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
        }
    }

    fn shard(&self, key: &SharedQueryKey) -> &RwLock<Shard<V>> {
        // The high bits: the map inside the shard uses the low ones.
        let hash = self.hasher.hash_one(key);
        let index = usize::try_from(hash >> 48).unwrap_or(0) % self.shards.len();
        &self.shards[index]
    }

    /// The plan kept for `key`. Finding one is not yet a hit: the caller
    /// has still to prove the replica it was kept against current, and
    /// reports the execution it then starts with [`Self::used`].
    pub(crate) fn get(&self, key: &SharedQueryKey) -> Option<Arc<V>> {
        let shard = self
            .shard(key)
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = shard.entries.get(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        slot.used.store(
            self.clock.fetch_add(1, Ordering::Relaxed) + 1,
            Ordering::Relaxed,
        );
        Some(Arc::clone(&slot.value))
    }

    /// Counts one execution started from a kept plan.
    pub(crate) fn used(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether `key`'s statement was prepared before, recording that it now
    /// has been. Remembered in a fixed table that later statements write
    /// over, so a statement repeated only after thousands of others is
    /// taken as new again - and kept one preparation later, nothing worse.
    pub(crate) fn seen_before(&self, key: &SharedQueryKey) -> bool {
        // Never zero, which is an empty slot.
        let hash = self.hasher.hash_one(key) | 1;
        let slot = usize::try_from(hash >> 16).unwrap_or(0) % self.seen.len();
        self.seen[slot].swap(hash, Ordering::Relaxed) == hash
    }

    /// Keeps `value` for `key`, dropping the least recently used entries
    /// when a bound is passed. A plan too large for the cache is not kept.
    pub(crate) fn insert(&self, key: SharedQueryKey, value: V, bytes: usize) {
        if bytes > self.bytes_per_shard || self.entries_per_shard == 0 {
            return;
        }
        let mut shard = self
            .shard(&key)
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = Slot {
            value: Arc::new(value),
            bytes,
            used: AtomicU64::new(self.clock.fetch_add(1, Ordering::Relaxed) + 1),
        };
        if let Some(previous) = shard.entries.insert(key, slot) {
            shard.bytes -= previous.bytes;
        }
        shard.bytes += bytes;
        self.inserted.fetch_add(1, Ordering::Relaxed);
        if shard.entries.len() > self.entries_per_shard || shard.bytes > self.bytes_per_shard {
            self.evict(&mut shard);
        }
    }

    /// Drops the least recently used entries until the shard is an eighth
    /// under both bounds, so a full cache does not evict on every insert.
    fn evict(&self, shard: &mut Shard<V>) {
        let entries_target = self.entries_per_shard - self.entries_per_shard / 8;
        let bytes_target = self.bytes_per_shard - self.bytes_per_shard / 8;
        let mut ages = shard
            .entries
            .iter()
            .map(|(key, slot)| (slot.used.load(Ordering::Relaxed), key.clone()))
            .collect::<Vec<_>>();
        ages.sort_unstable_by_key(|(used, _)| *used);
        // The newest entry - the one whose insert brought the shard here -
        // always stays.
        ages.pop();
        for (_, key) in ages {
            if shard.entries.len() <= entries_target && shard.bytes <= bytes_target {
                break;
            }
            if let Some(slot) = shard.entries.remove(&key) {
                shard.bytes -= slot.bytes;
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Drops every plan prepared against the replica load `replica`: that
    /// load has been replaced, and nothing will ask for its plans again.
    pub(crate) fn forget_replica(&self, replica: u64) {
        for shard in &self.shards {
            let mut shard = shard
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = shard.entries.len();
            let mut released = 0;
            shard.entries.retain(|key, slot| {
                let keep = key.replica != replica;
                if !keep {
                    released += slot.bytes;
                }
                keep
            });
            shard.bytes -= released;
            self.evicted
                .fetch_add((before - shard.entries.len()) as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn stats(&self) -> PlanCacheStats {
        let (entries, bytes) = self.shards.iter().fold((0, 0), |(entries, bytes), shard| {
            let shard = shard
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (entries + shard.entries.len(), bytes + shard.bytes)
        });
        PlanCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserted: self.inserted.load(Ordering::Relaxed),
            evicted: self.evicted.load(Ordering::Relaxed),
            entries: entries as u64,
            bytes: bytes as u64,
        }
    }
}

/// The bounds a process has when nothing overrides them: entries, then
/// estimated bytes.
#[must_use]
pub const fn default_bounds() -> (usize, usize) {
    (DEFAULT_ENTRIES, DEFAULT_BYTES)
}

/// The configured bounds: entries, then estimated bytes. Either at zero, or
/// `PINTAIL_PLAN_CACHE=0`, turns the cache off.
#[must_use]
pub fn configured_bounds() -> Option<(usize, usize)> {
    let setting = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(default)
    };
    if matches!(
        std::env::var("PINTAIL_PLAN_CACHE")
            .unwrap_or_default()
            .trim(),
        "0" | "false" | "off"
    ) {
        return None;
    }
    let entries = setting("PINTAIL_PLAN_CACHE_ENTRIES", DEFAULT_ENTRIES);
    let bytes = setting("PINTAIL_PLAN_CACHE_BYTES", DEFAULT_BYTES);
    (entries > 0 && bytes > 0).then_some((entries, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(replica: u64, sql: &str) -> SharedQueryKey {
        SharedQueryKey::for_current_session(replica, sql, 100)
    }

    #[test]
    fn a_kept_plan_is_found_by_everything_in_its_key_and_nothing_less() {
        let cache = PlanCache::new(8, 1 << 20);
        cache.insert(key(1, "SELECT 1"), "one", 100);
        assert_eq!(cache.get(&key(1, "SELECT 1")).as_deref(), Some(&"one"));
        cache.used();
        // Another load, another text, another row ceiling: each is another
        // statement.
        assert!(cache.get(&key(2, "SELECT 1")).is_none());
        assert!(cache.get(&key(1, "SELECT 2")).is_none());
        assert!(
            cache
                .get(&SharedQueryKey::for_current_session(1, "SELECT 1", 99))
                .is_none()
        );
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.inserted), (1, 3, 1));
    }

    #[test]
    fn the_least_recently_used_plans_leave_when_the_entry_bound_is_passed() {
        let cache = PlanCache::new(8, 1 << 20);
        for index in 0..8 {
            cache.insert(key(1, &format!("SELECT {index}")), index, 100);
        }
        // The oldest by insertion, made the most recent by use.
        assert!(cache.get(&key(1, "SELECT 0")).is_some());
        cache.insert(key(1, "SELECT 8"), 8, 100);
        let stats = cache.stats();
        assert!(stats.entries <= 8, "{stats:?}");
        assert!(stats.evicted >= 1);
        assert!(cache.get(&key(1, "SELECT 0")).is_some(), "used last, kept");
        assert!(cache.get(&key(1, "SELECT 8")).is_some(), "newest, kept");
        assert!(cache.get(&key(1, "SELECT 1")).is_none(), "oldest, gone");
    }

    #[test]
    fn the_byte_bound_holds_and_a_plan_larger_than_the_cache_is_not_kept() {
        let cache = PlanCache::new(64, 1000);
        for index in 0..20 {
            cache.insert(key(1, &format!("SELECT {index}")), index, 300);
            assert!(cache.stats().bytes <= 1000, "{:?}", cache.stats());
        }
        assert!(cache.stats().entries <= 3);
        cache.insert(key(1, "SELECT 'large'"), 0, 1001);
        assert!(cache.get(&key(1, "SELECT 'large'")).is_none());
        // Replacing an entry charges it once.
        let cache = PlanCache::new(64, 1000);
        cache.insert(key(1, "SELECT 1"), 1, 400);
        cache.insert(key(1, "SELECT 1"), 2, 400);
        assert_eq!(cache.stats().bytes, 400);
        assert_eq!(cache.get(&key(1, "SELECT 1")).as_deref(), Some(&2));
    }

    #[test]
    fn a_replaced_replicas_plans_are_dropped_and_no_others() {
        let cache = PlanCache::new(1024, 1 << 24);
        for index in 0..100 {
            cache.insert(key(1, &format!("SELECT {index}")), index, 100);
            cache.insert(key(2, &format!("SELECT {index}")), index, 100);
        }
        cache.forget_replica(1);
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.bytes), (100, 10_000));
        assert!(cache.get(&key(1, "SELECT 5")).is_none());
        assert!(cache.get(&key(2, "SELECT 5")).is_some());
    }

    #[test]
    fn a_statement_is_seen_before_only_once_it_has_been_seen() {
        let cache = PlanCache::<u8>::new(8, 1 << 20);
        assert!(!cache.seen_before(&key(1, "SELECT 1")));
        assert!(cache.seen_before(&key(1, "SELECT 1")));
        assert!(!cache.seen_before(&key(2, "SELECT 1")), "another replica");
        assert!(!cache.seen_before(&key(1, "SELECT 2")), "another statement");
        assert!(cache.seen_before(&key(1, "SELECT 2")));
    }

    #[test]
    fn a_zero_bound_keeps_nothing() {
        let cache = PlanCache::new(0, 1 << 20);
        cache.insert(key(1, "SELECT 1"), 1, 100);
        assert!(cache.get(&key(1, "SELECT 1")).is_none());
    }
}
