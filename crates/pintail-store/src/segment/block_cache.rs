//! Blocks of sealed segments held in memory, under one byte budget.
//!
//! A sealed segment file never changes, so a block read from it once can
//! be handed to every later reader. Two forms are held:
//!
//! - a block's **payload**, checked and decompressed but still encoded,
//!   for scans: a scan that finds it skips the read, the checksum and the
//!   decompression, and decodes only the rows it selects;
//! - a block **decoded** into a column, for key lookups, which read one
//!   row of a block and would otherwise decode all of it each time.
//!
//! An entry is named by the segment as it exists on disk (path, length,
//! modification time and schema generation, interned to a number that is
//! never reused), the column and the block's first row. A flush, a
//! compaction or a recopy publishes other files, so their blocks are other
//! entries; a file that is deleted has its entries dropped at once
//! ([`forget_file`]), and anything else that goes stale is unreachable and
//! ages out.
//!
//! The budget is in bytes and process-wide. Entries leave in the order
//! they came unless they were used since: each use buys an entry one more
//! pass, up to three. A payload is admitted the second time it is asked
//! for, so one scan of a large table does not push out what is in steady
//! use; decoded blocks are admitted at once. When the server charges the
//! cache to its memory budget, a query that finds that budget full takes
//! memory back from here before it is refused ([`shrink`]).

use std::{
    collections::{HashMap, VecDeque},
    hash::{BuildHasherDefault, Hash, Hasher},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use super::{Encoding, VerifiedKey};
use crate::store::DecodedColumn;

/// What the cache holds when nothing configures it: enough for the blocks
/// a burst of key lookups touches.
const DEFAULT_LIMIT_BYTES: usize = 32 * 1024 * 1024;

const SHARDS: usize = 16;

/// Recently refused payload keys remembered per shard.
const DOORKEEPER_SLOTS: usize = 4096;

/// The most passes repeated use can buy an entry.
const MAX_CHANCES: u8 = 3;

/// Segment identities remembered before the table of them starts over.
const MAX_SEGMENTS: usize = 65_536;

/// A block's payload, checked against its checksum and decompressed.
pub(super) struct CachedPayload {
    pub(super) row_count: usize,
    pub(super) null_bitmap: Vec<u8>,
    pub(super) null_count: usize,
    pub(super) encoding: Encoding,
    pub(super) bytes: Vec<u8>,
}

impl CachedPayload {
    fn retained_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.null_bitmap.capacity())
            .saturating_add(size_of::<Self>())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Form {
    Payload,
    Decoded,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Key {
    segment: u64,
    column: u32,
    first_row: usize,
    form: Form,
}

/// Mixes a key's few integers. Fixed rather than seeded per process, so
/// the same blocks land in the same places on every run and a count of
/// instructions executed does not move with the seed.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        let mixed = self.0 ^ (self.0 >> 32);
        mixed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (mixed >> 29)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(23) ^ value).wrapping_mul(0xff51_afd7_ed55_8ccd);
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

#[derive(Clone)]
enum Held {
    Payload(Arc<CachedPayload>),
    Decoded(Arc<DecodedColumn>),
}

struct Entry {
    held: Held,
    bytes: usize,
    chances: u8,
}

#[derive(Default)]
struct Shard {
    entries: HashMap<Key, Entry, BuildHasherDefault<KeyHasher>>,
    /// Keys in arrival order; a key evicted by name may linger here and is
    /// passed over when reached.
    order: VecDeque<Key>,
    bytes: usize,
    doorkeeper: Vec<u64>,
}

/// How the server accounts for what the cache holds.
#[derive(Clone, Copy, Debug)]
pub struct BlockCacheAccounting {
    /// Asks for `bytes`; `false` refuses, and the block is not held.
    pub charge: fn(usize) -> bool,
    /// Returns `bytes` charged earlier.
    pub release: fn(usize),
}

/// What the cache has done since the process started.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockCacheStats {
    /// The byte budget; zero means nothing is held.
    pub limit_bytes: usize,
    /// Bytes held now.
    pub held_bytes: usize,
    /// Lookups that found their block.
    pub hits: u64,
    /// Lookups that did not.
    pub misses: u64,
    /// Blocks admitted.
    pub inserted: u64,
    /// Blocks pushed out, forgotten or given back.
    pub evicted: u64,
}

struct Cache {
    shards: Vec<Mutex<Shard>>,
    limit: AtomicUsize,
    held: AtomicUsize,
    accounting: Mutex<Option<BlockCacheAccounting>>,
    segments: Mutex<Segments>,
    hits: AtomicU64,
    misses: AtomicU64,
    inserted: AtomicU64,
    evicted: AtomicU64,
}

#[derive(Default)]
struct Segments {
    ids: HashMap<VerifiedKey, u64>,
    next: u64,
}

/// The setting that names the budget, in mebibytes.
pub const BLOCK_CACHE_SETTING: &str = "PINTAIL_BLOCK_CACHE_MB";

/// The budget the environment asks for, in bytes.
#[must_use]
pub fn environment_limit() -> Option<usize> {
    std::env::var(BLOCK_CACHE_SETTING)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .map(|mebibytes| mebibytes.saturating_mul(1024 * 1024))
}

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Cache {
        shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
        limit: AtomicUsize::new(environment_limit().unwrap_or(DEFAULT_LIMIT_BYTES)),
        held: AtomicUsize::new(0),
        accounting: Mutex::new(None),
        segments: Mutex::new(Segments::default()),
        hits: AtomicU64::new(0),
        misses: AtomicU64::new(0),
        inserted: AtomicU64::new(0),
        evicted: AtomicU64::new(0),
    })
}

/// A segment's place in the cache: every block of one file as it exists on
/// disk is looked up under one of these.
#[derive(Clone, Copy, Debug)]
pub(super) struct SegmentSlot(u64);

/// The slot of the segment `key` names, or `None` while the cache holds
/// nothing.
pub(super) fn segment_slot(key: &VerifiedKey) -> Option<SegmentSlot> {
    let cache = cache();
    if cache.limit.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let mut segments = cache
        .segments
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(id) = segments.ids.get(key) {
        return Some(SegmentSlot(*id));
    }
    if segments.ids.len() >= MAX_SEGMENTS {
        // Numbers are never reused, so blocks under a forgotten number are
        // unreachable and age out.
        segments.ids.clear();
    }
    segments.next += 1;
    let id = segments.next;
    segments.ids.insert(key.clone(), id);
    Some(SegmentSlot(id))
}

impl Cache {
    fn shard(&self, key: &Key) -> (&Mutex<Shard>, u64) {
        let mut hasher = KeyHasher::default();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        let shard = usize::try_from((hash >> 48) % SHARDS as u64).unwrap_or(0);
        (&self.shards[shard], hash)
    }

    fn shard_limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed) / SHARDS
    }

    fn get(&self, key: &Key) -> Option<Held> {
        let (shard, _) = self.shard(key);
        let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
        let found = shard.entries.get_mut(key).map(|entry| {
            entry.chances = (entry.chances + 1).min(MAX_CHANCES);
            entry.held.clone()
        });
        drop(shard);
        let counter = if found.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// Whether a payload asked for under `key` should be kept this time:
    /// yes when it was asked for recently and refused.
    fn admits(&self, key: &Key) -> bool {
        let (shard, hash) = self.shard(key);
        let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
        if shard.doorkeeper.is_empty() {
            shard.doorkeeper = vec![0; DOORKEEPER_SLOTS];
        }
        let slot = usize::try_from(hash % DOORKEEPER_SLOTS as u64).unwrap_or(0);
        // Zero marks an empty slot.
        let mark = hash | 1;
        if shard.doorkeeper[slot] == mark {
            return true;
        }
        shard.doorkeeper[slot] = mark;
        false
    }

    fn insert(&self, key: Key, held: Held, bytes: usize) {
        let limit = self.shard_limit();
        // One block may not take most of a shard.
        if bytes > limit / 2 {
            return;
        }
        let accounting = *self
            .accounting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (shard, _) = self.shard(&key);
        let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
        if shard.entries.contains_key(&key) {
            return;
        }
        let mut freed = 0_usize;
        while shard.bytes.saturating_add(bytes) > limit {
            let Some(freed_here) = evict_one(&mut shard) else {
                break;
            };
            self.evicted.fetch_add(1, Ordering::Relaxed);
            freed += freed_here;
        }
        // What was pushed out is returned before the newcomer is charged,
        // so making room never asks for more than the newcomer's bytes.
        if freed > 0 {
            self.held.fetch_sub(freed, Ordering::Relaxed);
            if let Some(accounting) = accounting {
                (accounting.release)(freed);
            }
        }
        if shard.bytes.saturating_add(bytes) > limit {
            return;
        }
        if let Some(accounting) = accounting
            && !(accounting.charge)(bytes)
        {
            return;
        }
        shard.bytes += bytes;
        shard.order.push_back(key);
        shard.entries.insert(
            key,
            Entry {
                held,
                bytes,
                chances: 0,
            },
        );
        self.held.fetch_add(bytes, Ordering::Relaxed);
        self.inserted.fetch_add(1, Ordering::Relaxed);
    }

    /// Drops entries until `wanted` bytes are freed or nothing is left,
    /// or only those `keep` refuses when it is given. Returns bytes freed.
    fn release_entries(&self, wanted: usize, doomed: Option<&dyn Fn(&Key) -> bool>) -> usize {
        let accounting = *self
            .accounting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut freed = 0_usize;
        for shard in &self.shards {
            if freed >= wanted {
                break;
            }
            let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
            let mut freed_here = 0_usize;
            if let Some(doomed) = doomed {
                let keys = shard
                    .entries
                    .keys()
                    .filter(|key| doomed(key))
                    .copied()
                    .collect::<Vec<_>>();
                for key in keys {
                    if let Some(entry) = shard.entries.remove(&key) {
                        freed_here += entry.bytes;
                        self.evicted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                while freed + freed_here < wanted {
                    // Under pressure use buys nothing: take the oldest.
                    let Some(key) = shard.order.pop_front() else {
                        break;
                    };
                    if let Some(entry) = shard.entries.remove(&key) {
                        freed_here += entry.bytes;
                        self.evicted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            shard.bytes -= freed_here;
            drop(shard);
            if freed_here > 0 {
                self.held.fetch_sub(freed_here, Ordering::Relaxed);
                if let Some(accounting) = accounting {
                    (accounting.release)(freed_here);
                }
            }
            freed += freed_here;
        }
        freed
    }
}

/// Removes the entry longest held without a use since its last pass.
/// Returns its bytes, or `None` when the shard is empty.
fn evict_one(shard: &mut Shard) -> Option<usize> {
    loop {
        let key = shard.order.pop_front()?;
        let Some(entry) = shard.entries.get_mut(&key) else {
            continue;
        };
        if entry.chances > 0 {
            entry.chances -= 1;
            shard.order.push_back(key);
            continue;
        }
        let bytes = entry.bytes;
        shard.entries.remove(&key);
        shard.bytes -= bytes;
        return Some(bytes);
    }
}

const fn key(slot: SegmentSlot, column: u32, first_row: usize, form: Form) -> Key {
    Key {
        segment: slot.0,
        column,
        first_row,
        form,
    }
}

/// The payload held for one block.
pub(super) fn payload(
    slot: SegmentSlot,
    column: u32,
    first_row: usize,
) -> Option<Arc<CachedPayload>> {
    match cache().get(&key(slot, column, first_row, Form::Payload)) {
        Some(Held::Payload(payload)) => Some(payload),
        _ => None,
    }
}

/// Whether a payload just read for this block should be handed to
/// [`hold_payload`].
pub(super) fn admits_payload(slot: SegmentSlot, column: u32, first_row: usize) -> bool {
    cache().admits(&key(slot, column, first_row, Form::Payload))
}

/// Holds one block's payload.
pub(super) fn hold_payload(
    slot: SegmentSlot,
    column: u32,
    first_row: usize,
    payload: CachedPayload,
) {
    let bytes = payload.retained_bytes();
    cache().insert(
        key(slot, column, first_row, Form::Payload),
        Held::Payload(Arc::new(payload)),
        bytes,
    );
}

/// The decoded column held for one block.
pub(super) fn decoded(
    slot: SegmentSlot,
    column: u32,
    first_row: usize,
) -> Option<Arc<DecodedColumn>> {
    match cache().get(&key(slot, column, first_row, Form::Decoded)) {
        Some(Held::Decoded(column)) => Some(column),
        _ => None,
    }
}

/// Holds one block decoded.
pub(super) fn hold_decoded(
    slot: SegmentSlot,
    column: u32,
    first_row: usize,
    block: Arc<DecodedColumn>,
) {
    let bytes = block.retained_bytes().saturating_add(size_of::<Entry>());
    cache().insert(
        key(slot, column, first_row, Form::Decoded),
        Held::Decoded(block),
        bytes,
    );
}

/// Drops every block of the segment file at `path`: called when the file
/// is deleted, so its blocks do not wait to age out.
pub(crate) fn forget_file(path: &Path) {
    let cache = cache();
    let doomed = {
        let mut segments = cache
            .segments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut doomed = Vec::new();
        segments.ids.retain(|key, id| {
            if key.0 == path {
                doomed.push(*id);
                false
            } else {
                true
            }
        });
        doomed
    };
    if !doomed.is_empty() && cache.held.load(Ordering::Relaxed) > 0 {
        cache.release_entries(usize::MAX, Some(&|key| doomed.contains(&key.segment)));
    }
}

/// Sets the byte budget (zero holds nothing) and how held bytes are
/// accounted. Everything held is dropped first, under the accounting it
/// was charged to.
pub fn configure_block_cache(limit_bytes: usize, accounting: Option<BlockCacheAccounting>) {
    let cache = cache();
    cache.release_entries(usize::MAX, None);
    *cache
        .accounting
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = accounting;
    cache.limit.store(limit_bytes, Ordering::Relaxed);
}

/// Gives back at least `bytes` of what the cache holds, oldest first, or
/// everything when it holds less. Returns the bytes freed.
#[must_use]
pub fn shrink_block_cache(bytes: usize) -> usize {
    let cache = cache();
    if cache.held.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    cache.release_entries(bytes, None)
}

/// The cache's budget, what it holds and what it has done.
#[must_use]
pub fn block_cache_stats() -> BlockCacheStats {
    let cache = cache();
    BlockCacheStats {
        limit_bytes: cache.limit.load(Ordering::Relaxed),
        held_bytes: cache.held.load(Ordering::Relaxed),
        hits: cache.hits.load(Ordering::Relaxed),
        misses: cache.misses.load(Ordering::Relaxed),
        inserted: cache.inserted.load(Ordering::Relaxed),
        evicted: cache.evicted.load(Ordering::Relaxed),
    }
}
