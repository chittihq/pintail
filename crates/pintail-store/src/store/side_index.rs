//! Secondary side index for one integer or text column of a segment.
//!
//! On unless `PINTAIL_SECONDARY_INDEX=0`. A filter on a column that is not
//! the table's key, whose values scatter across the whole segment, touches
//! nearly every block, so block extremes skip nothing and the filter-first
//! scan decodes and tests every row of its predicate columns. The side index
//! holds the column's `(value, row)` pairs sorted by value, built lazily the
//! first time a scan asks for it (or loaded from the postings section a
//! flush or compaction wrote) and cached, bounded and evictable, while the
//! segment file lives. A scan that knows the only values its rows can hold (an
//! equality, an IN list, a join's key set) asks it for their rows and hands
//! the filter-first decode those rows alone; the scan's own predicates still
//! decide every row, so the index only chooses which rows are looked at.
//!
//! A text column is indexed under the collation a lookup compares with: its
//! postings hold a 64-bit hash of each value's collation key, so every value
//! equal to a probed one under that collation shares the probed hash. A
//! collision only adds candidates the predicates then reject. The persisted
//! section holds the exact values, which no collation's rules change; the
//! hashed postings derive from it once per collation a scan asks for.
//!
//! Postings cover segment rows only. Rows still in the memtable (or resolved
//! from a layered cluster) are tested against the lookup one by one, and a
//! row it rejects is left out before it is materialized; an overlay part
//! still masks the segment row a rejected memtable row supersedes, and masks
//! superseded segment rows among the candidates exactly as among all rows.

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicUsize},
    time::Instant,
};

use pintail_types::{StoredRow, TableSchema, Value};

use super::scan::DecodedColumn;
use crate::{StoreError, segment};

/// Candidates past this share of a slice's rows are not worth the detour:
/// the plain filter-first decode reads the same blocks in one pass.
pub(crate) const MAX_CANDIDATE_SHARE: usize = 4;

/// The share for a scan that decodes nothing beyond the columns its filter
/// reads. The detour then saves no second column, only the filter columns'
/// own decode, and reading a candidate through the index costs several
/// times what decoding a row in place does: a value one row in five holds
/// took three times the instructions through the index that reading the
/// column through did, and the two met near one row in thirteen.
pub(crate) const MAX_FILTER_ONLY_CANDIDATE_SHARE: usize = 32;

thread_local! {
    static THREAD_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the side index is on: the process environment (on unless
/// `PINTAIL_SECONDARY_INDEX=0`), or
/// the calling thread's override. Only the thread that plans a scan asks;
/// the decode follows whatever lookup the scan was given.
#[must_use]
pub fn side_index_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    THREAD_OVERRIDE
        .with(std::cell::Cell::get)
        .unwrap_or_else(|| {
            *ENABLED.get_or_init(|| {
                !std::env::var("PINTAIL_SECONDARY_INDEX").is_ok_and(|value| value.trim() == "0")
            })
        })
}

/// Whether `PINTAIL_SIDE_INDEX_TRACE=1` asks every lookup to log what it
/// found or why it declined: the first thing to read when a filter on an
/// indexed column still reads the table.
#[must_use]
pub fn side_index_trace() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| {
        std::env::var("PINTAIL_SIDE_INDEX_TRACE").is_ok_and(|value| value.trim() == "1")
    })
}

/// Logs `message()` when the trace is on: for the layers above the store,
/// which decide what a scan is asked and have no log of their own.
pub fn side_index_note(message: impl FnOnce() -> String) {
    if side_index_trace() {
        pintail_log::log_info!("side index {}", message());
    }
}

/// Switches the side index on or off for scans planned on this thread, or
/// back to the environment's setting with `None`: what lets a test compare
/// both paths in one process.
pub fn override_side_index(enabled: Option<bool>) {
    THREAD_OVERRIDE.with(|cell| cell.set(enabled));
}

/// The values a scan's rows can hold in one column: an integer column's
/// own values, or a text column's key hashes (see [`TextKeyer::value`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexProbe {
    /// Exactly these values (sorted, deduplicated); NULL is never one.
    Values(Vec<i128>),
    /// Any value in `[lower, upper]`. Integer columns only: key hashes have
    /// no order.
    Span(i128, i128),
}

/// Writes a text value's collation key: two texts equal under the collation
/// must write the same bytes.
pub type TextKeyFn = dyn Fn(&str, &mut Vec<u8>) + Send + Sync;

/// A collation as the side index sees it: an identity for the cache and the
/// function writing a value's key.
#[derive(Clone)]
pub struct TextKeyer {
    id: u32,
    key: Arc<TextKeyFn>,
}

impl TextKeyer {
    /// A keyer named `id`, which must differ between collations whose keys
    /// differ and stay the same for one collation within the process.
    #[must_use]
    pub fn new(id: u32, key: Arc<TextKeyFn>) -> Self {
        Self { id, key }
    }

    /// The postings value of `text`: the hash of its collation key.
    #[must_use]
    pub fn value(&self, text: &str) -> i64 {
        let mut key = Vec::with_capacity(text.len() * 2 + 8);
        (self.key)(text, &mut key);
        Self::value_of_key(&key)
    }

    /// The postings value of a text whose collation key is already written:
    /// what [`Self::value`] answers for any text with this key.
    #[must_use]
    pub fn value_of_key(key: &[u8]) -> i64 {
        xxhash_rust::xxh3::xxh3_64(key).cast_signed()
    }
}

impl PartialEq for TextKeyer {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TextKeyer {}

impl std::fmt::Debug for TextKeyer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextKeyer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a lookup's probe values are.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum IndexKey {
    /// The integer column's own values.
    #[default]
    Integer,
    /// Hashes of text values' keys under one collation.
    Text(TextKeyer),
}

impl IndexKey {
    const fn cache_id(&self) -> u32 {
        match self {
            Self::Integer => 0,
            Self::Text(keyer) => keyer.id.saturating_add(1),
        }
    }
}

/// A side-index request a scan carries: the column, what its values are,
/// and its probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexLookup {
    pub column_id: u32,
    pub key: IndexKey,
    pub probe: IndexProbe,
}

impl IndexLookup {
    /// Whether a row holding `value` in the lookup's column can be one the
    /// scan wants. The probe names every value a wanted row can hold, so a
    /// row it rejects - NULL included, which no probe names - is one the
    /// scan's predicates or join would drop. A value of a kind the lookup
    /// does not describe is kept, left for the predicates to judge.
    #[must_use]
    pub fn admits(&self, value: &Value) -> bool {
        let probed = match (&self.key, value) {
            (_, Value::Null) => return false,
            (IndexKey::Integer, Value::Int64(value)) => i128::from(*value),
            (IndexKey::Integer, Value::UInt64(value)) => i128::from(*value),
            (IndexKey::Text(keyer), Value::Utf8(text)) => i128::from(keyer.value(text)),
            _ => return true,
        };
        match &self.probe {
            IndexProbe::Values(values) => values.binary_search(&probed).is_ok(),
            IndexProbe::Span(lower, upper) => (*lower..=*upper).contains(&probed),
        }
    }
}

/// Distinct texts a [`RowAdmission`] remembers its verdict for; past this
/// it keys each further text as it meets it.
const ADMISSION_MEMO: usize = 4_096;

/// Judges whole rows (the memtable's, or a layered cluster's) against a
/// lookup. A text value's collation key is the costly part, and a text
/// column in the memtable repeats a few values over many rows, so each
/// distinct text is keyed once.
pub(crate) struct RowAdmission<'a> {
    lookup: &'a IndexLookup,
    position: usize,
    texts: HashMap<&'a str, bool>,
    /// The same memo for texts read packed, which no row lends.
    packed_texts: HashMap<Vec<u8>, bool>,
}

impl<'a> RowAdmission<'a> {
    /// Judges the column at schema `position` of each row by `lookup`.
    pub(crate) fn new(lookup: &'a IndexLookup, position: usize) -> Self {
        Self {
            lookup,
            position,
            texts: HashMap::new(),
            packed_texts: HashMap::new(),
        }
    }

    /// The schema position of the column the lookup reads.
    pub(crate) const fn position(&self) -> usize {
        self.position
    }

    /// Whether the scan can want a row holding `value` in that column.
    pub(super) fn admits_cell(&mut self, cell: super::layer::Cell<'_>) -> bool {
        let super::layer::Cell::Text(bytes) = cell else {
            return self.lookup.admits(&cell.to_value());
        };
        if let Some(known) = self.packed_texts.get(bytes) {
            return *known;
        }
        let admitted = self.lookup.admits(&cell.to_value());
        if self.packed_texts.len() < ADMISSION_MEMO {
            self.packed_texts.insert(bytes.to_vec(), admitted);
        }
        admitted
    }

    /// Whether the scan can want `row` (see [`IndexLookup::admits`]).
    pub(crate) fn admits(&mut self, row: &'a StoredRow) -> bool {
        let Some(value) = row.values().get(self.position) else {
            return true;
        };
        if let (IndexKey::Text(_), Value::Utf8(text)) = (&self.lookup.key, value) {
            if let Some(known) = self.texts.get(text.as_str()) {
                return *known;
            }
            let admitted = self.lookup.admits(value);
            if self.texts.len() < ADMISSION_MEMO {
                self.texts.insert(text.as_str(), admitted);
            }
            return admitted;
        }
        self.lookup.admits(value)
    }
}

/// One segment column's non-NULL values sorted with their rows.
pub(crate) struct Postings {
    values: Vec<i64>,
    rows: Vec<u32>,
    /// Rows in the segment, NULLs included.
    row_count: usize,
}

impl Postings {
    fn from_pairs(mut pairs: Vec<(i64, u32)>, row_count: usize) -> Self {
        pairs.sort_unstable();
        let (values, rows) = pairs.into_iter().unzip();
        Self {
            values,
            rows,
            row_count,
        }
    }

    /// Heap bytes the postings hold.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.values.capacity() * size_of::<i64>() + self.rows.capacity() * size_of::<u32>()
    }

    /// The run of postings entries whose value lies in `[lower, upper]`.
    fn entries_between(&self, lower: i128, upper: i128) -> Range<usize> {
        if lower > upper {
            return 0..0;
        }
        let start = self
            .values
            .partition_point(|value| i128::from(*value) < lower);
        let end = self
            .values
            .partition_point(|value| i128::from(*value) <= upper);
        start..end.max(start)
    }

    /// The rows of `[start, end)` whose value the probe admits, coalesced
    /// into ascending ranges; `None` when they are too many to be worth
    /// reading apart from the rest.
    #[cfg(test)]
    pub(crate) fn candidate_ranges(
        &self,
        probe: &IndexProbe,
        start: usize,
        end: usize,
    ) -> Option<Vec<Range<usize>>> {
        self.candidate_ranges_within(probe, start, end, MAX_CANDIDATE_SHARE)
    }

    /// [`Self::candidate_ranges`] with the share of rows past which the
    /// probe declines given as its divisor: one row in `share`.
    pub(crate) fn candidate_ranges_within(
        &self,
        probe: &IndexProbe,
        start: usize,
        end: usize,
        share: usize,
    ) -> Option<Vec<Range<usize>>> {
        let runs = match probe {
            IndexProbe::Values(values) => values
                .iter()
                .map(|value| self.entries_between(*value, *value))
                .filter(|run| !run.is_empty())
                .collect::<Vec<_>>(),
            IndexProbe::Span(lower, upper) => vec![self.entries_between(*lower, *upper)],
        };
        let slice_rows = end.saturating_sub(start);
        let limit = slice_rows / share;
        let in_slice = |row: &u32| {
            let row = *row as usize;
            row >= start && row < end
        };
        let total = runs.iter().map(ExactSizeIterator::len).sum::<usize>();
        // A probe past the share of the whole segment declines at once: a
        // segment read in several slices otherwise walked the probe's every
        // row once per slice to count the ones inside it.
        if total.saturating_mul(share) > self.row_count {
            return None;
        }
        // Counted before anything is collected, and abandoned as soon as the
        // slice's share is passed.
        if total > limit {
            let mut counted = 0_usize;
            for run in &runs {
                for row in &self.rows[run.clone()] {
                    counted += usize::from(in_slice(row));
                    if counted > limit {
                        return None;
                    }
                }
            }
        }
        let mut rows = Vec::with_capacity(total.min(limit.max(1)));
        for run in runs {
            rows.extend(self.rows[run].iter().copied().filter(|row| in_slice(row)));
        }
        rows.sort_unstable();
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for row in rows {
            let row = row as usize;
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => ranges.push(row..row + 1),
            }
        }
        Some(ranges)
    }
}

/// Names one segment column's postings. The file name alone is not enough:
/// a table resynchronized into a fresh directory can reuse it, and a type
/// change keeps the file while changing what its values mean, so the
/// segment's identity, versions and schema fingerprint and the column's
/// declared type are part of the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    segment_id: u64,
    row_count: u64,
    versions: (u64, u64),
    schema_fingerprint: u64,
    column_id: u32,
    data_type: Option<pintail_types::DataType>,
    /// Zero for integer postings, else the text keyer's collation.
    key_id: u32,
}

impl CacheKey {
    fn new(
        directory: &Path,
        meta: &segment::SegmentMeta,
        schema: &TableSchema,
        column_id: u32,
        key: &IndexKey,
    ) -> Self {
        Self {
            path: directory.join(&meta.file_name),
            segment_id: meta.id,
            row_count: meta.row_count,
            versions: (meta.min_version, meta.max_version),
            schema_fingerprint: meta.schema_fingerprint,
            column_id,
            data_type: schema
                .columns()
                .iter()
                .find(|column| column.id() == column_id)
                .map(pintail_types::Column::data_type),
            key_id: key.cache_id(),
        }
    }
}

/// One slot per segment column: the first scan to reach it builds, and
/// scans reaching it meanwhile wait for that build rather than repeat it.
type Slot = Arc<OnceLock<Option<Arc<Postings>>>>;

/// The least the cached postings may hold together when nothing says
/// otherwise: the ceiling of a process that was not told its memory.
const DEFAULT_CACHE_BYTES: usize = 256 << 20;

/// The share of the process's memory the cached postings may hold by
/// default: one eighth.
///
/// A fixed 256 MB held the postings of one text column of a table of twelve
/// million rows but not of two. Statements that looked rows up by one and
/// then the other evicted each other's postings every time, and each
/// rebuilt them - a few hundred milliseconds a statement for lookups that
/// take a few once the postings stay.
const DEFAULT_CACHE_MEMORY_SHARE: u64 = 8;

/// The default ceiling the process was given for its memory, when it said.
static DEFAULT_LIMIT: OnceLock<usize> = OnceLock::new();

/// Sizes the postings cache's default ceiling from the memory available to
/// the process: an eighth of it, and never under 256 MB.
/// `PINTAIL_SECONDARY_INDEX_CACHE_MB` still overrides it. Call once, before
/// the first query; later calls change nothing.
pub fn side_index_cache_default(available_memory_bytes: u64) {
    let _ = DEFAULT_LIMIT.set(default_cache_bytes(available_memory_bytes));
}

fn default_cache_bytes(available_memory_bytes: u64) -> usize {
    usize::try_from(available_memory_bytes / DEFAULT_CACHE_MEMORY_SHARE)
        .unwrap_or(usize::MAX)
        .max(DEFAULT_CACHE_BYTES)
}

/// The bytes the cached postings may hold: `PINTAIL_SECONDARY_INDEX_CACHE_MB`
/// when set, else the default sized from the process's memory.
#[must_use]
pub fn side_index_cache_limit() -> usize {
    cache_limit()
}

fn cache_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("PINTAIL_SECONDARY_INDEX_CACHE_MB")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .map_or_else(
                || DEFAULT_LIMIT.get().copied().unwrap_or(DEFAULT_CACHE_BYTES),
                |megabytes| megabytes.saturating_mul(1 << 20),
            )
    })
}

struct CacheEntry {
    slot: Slot,
    /// Heap bytes of the built postings; zero until the build finishes.
    bytes: usize,
    last_used: u64,
}

/// The postings cache, bounded by bytes and evicted least recently used
/// first. An evicted entry stays alive for the scans already holding it and
/// is rebuilt by the next scan that asks.
#[derive(Default)]
struct Cache {
    entries: HashMap<CacheKey, CacheEntry>,
    resident: usize,
    clock: u64,
    evictions: u64,
}

impl Cache {
    fn slot(&mut self, key: CacheKey) -> Slot {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.entry(key).or_insert_with(|| CacheEntry {
            slot: Slot::default(),
            bytes: 0,
            last_used: clock,
        });
        entry.last_used = clock;
        Arc::clone(&entry.slot)
    }

    /// Charges a finished build to its entry, then evicts the least recently
    /// used other entries until the total fits the limit again.
    fn charge(&mut self, key: &CacheKey, bytes: usize, limit: usize) {
        // One segment's postings alone past the limit are not kept, and
        // evict nothing else to make room.
        if bytes > limit {
            if self.entries.remove(key).is_some() {
                self.evictions += 1;
            }
            return;
        }
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        self.resident = self.resident - entry.bytes + bytes;
        entry.bytes = bytes;
        while self.resident > limit {
            let victim = self
                .entries
                .iter()
                .filter(|(candidate, entry)| *candidate != key && entry.bytes > 0)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(candidate, _)| candidate.clone());
            let Some(victim) = victim else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&victim) {
                self.resident -= evicted.bytes;
                self.evictions += 1;
            }
        }
    }

    fn forget(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.resident -= entry.bytes;
        }
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn lock_cache() -> std::sync::MutexGuard<'static, Cache> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The side-index cache as it stands: entries resident, heap bytes they
/// hold, and entries evicted since the process started.
#[must_use]
pub fn side_index_cache_usage() -> (usize, usize, u64) {
    let cache = lock_cache();
    (cache.entries.len(), cache.resident, cache.evictions)
}

/// Totals over every side index built in this process: entries, heap
/// bytes, and build time in microseconds.
#[must_use]
pub fn side_index_totals() -> (usize, usize, u128) {
    let totals = TOTALS.get_or_init(|| Mutex::new((0, 0, 0)));
    *totals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static TOTALS: OnceLock<Mutex<(usize, usize, u128)>> = OnceLock::new();

/// The postings of `column_id` in one segment under `index_key`, built on
/// first use. `None` when the column does not decode as that key needs (a
/// plain integer column, or holding an unsigned value past the signed range;
/// text), which declines the index.
pub(crate) fn postings(
    directory: &Path,
    meta: &segment::SegmentMeta,
    schema: &TableSchema,
    column_id: u32,
    index_key: &IndexKey,
) -> Result<Option<Arc<Postings>>, StoreError> {
    let key = CacheKey::new(directory, meta, schema, column_id, index_key);
    let slot = lock_cache().slot(key.clone());
    if let Some(found) = slot.get() {
        return Ok(found.clone());
    }
    let mut failure = None;
    let mut built_here = false;
    let built = slot.get_or_init(|| {
        let started = Instant::now();
        built_here = true;
        match build(directory, meta, schema, column_id, index_key) {
            Ok(built) => {
                let built = built.map(Arc::new);
                if let Some(postings) = &built {
                    record_build(meta, column_id, postings, started.elapsed());
                }
                built
            }
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    });
    if let Some(error) = failure {
        // A failed build is not remembered as a decline.
        lock_cache().forget(&key);
        return Err(error);
    }
    if built_here {
        // A decline is remembered at the cost of its entry alone.
        let bytes = built
            .as_ref()
            .map_or(1, |postings| postings.heap_bytes().max(1));
        lock_cache().charge(&key, bytes, cache_limit());
    }
    Ok(built.clone())
}

/// Where the first `k` rows in the order of an integer column end, from the
/// postings of `segments`: the smallest value at or below which (the
/// largest at or above which, `descending`) the segments hold at least `k`
/// non-NULL entries, and whether any of them holds a NULL there. `None`
/// when the column is not a plain integer column (an ENUM or SET orders by
/// something else), a segment declines, or they hold fewer than `k`
/// entries together.
///
/// The entries include rows a newer version supersedes or a tombstone
/// deletes, so the rows at or before the bound can number fewer than `k`:
/// a caller restricting a scan to them must see at least `k` come back
/// before trusting the restriction.
pub(crate) fn order_bound(
    directory: &Path,
    segments: &[segment::SegmentMeta],
    schema: &TableSchema,
    column_id: u32,
    k: usize,
    descending: bool,
) -> Result<Option<(i128, bool)>, StoreError> {
    let Some(column) = schema
        .columns()
        .iter()
        .find(|column| column.id() == column_id)
    else {
        return Ok(None);
    };
    if k == 0
        || !is_integer(column.data_type())
        || column.enum_labels().is_some()
        || column.set_members().is_some()
    {
        return Ok(None);
    }
    let mut edge = Vec::new();
    let mut nulls = false;
    for meta in segments {
        let Some(postings) = postings(directory, meta, schema, column_id, &IndexKey::Integer)?
        else {
            return Ok(None);
        };
        nulls |= postings.values.len() < postings.row_count;
        let take = k.min(postings.values.len());
        if descending {
            edge.extend_from_slice(&postings.values[postings.values.len() - take..]);
        } else {
            edge.extend_from_slice(&postings.values[..take]);
        }
    }
    if edge.len() < k {
        return Ok(None);
    }
    let bound = if descending {
        *edge
            .select_nth_unstable_by(k - 1, |left, right| right.cmp(left))
            .1
    } else {
        *edge.select_nth_unstable(k - 1).1
    };
    Ok(Some((i128::from(bound), nulls)))
}

/// Columns, per table directory, whose index a scan found selective enough
/// to use: the ones the next flush or compaction of that table writes
/// postings for, so a restart or a new segment does not rebuild them.
fn useful_columns() -> &'static Mutex<HashMap<PathBuf, BTreeSet<u32>>> {
    static USEFUL: OnceLock<Mutex<HashMap<PathBuf, BTreeSet<u32>>>> = OnceLock::new();
    USEFUL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records that a scan of the table in `directory` used the index on
/// `column_id`.
pub(crate) fn note_useful(directory: &Path, column_id: u32) {
    let mut useful = useful_columns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(columns) = useful.get_mut(directory) {
        columns.insert(column_id);
    } else {
        useful.insert(directory.to_path_buf(), BTreeSet::from([column_id]));
    }
}

/// The integer and text columns, with their schema positions, a segment
/// written to `directory` carries postings for: every column a scan has used
/// the index on, and every column `PINTAIL_SECONDARY_INDEX_COLUMNS` (a comma
/// list of column names) names for every table. None while the index is off.
pub(crate) fn persisted_columns(directory: &Path, schema: &TableSchema) -> Vec<(u32, usize)> {
    static NAMED: OnceLock<Vec<String>> = OnceLock::new();
    if !side_index_enabled() {
        return Vec::new();
    }
    let named = NAMED.get_or_init(|| {
        std::env::var("PINTAIL_SECONDARY_INDEX_COLUMNS")
            .map(|list| {
                list.split(',')
                    .map(|name| name.trim().to_ascii_lowercase())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    });
    let useful = useful_columns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(directory)
        .cloned()
        .unwrap_or_default();
    schema
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, column)| {
            is_integer(column.data_type()) || column.data_type() == pintail_types::DataType::Utf8
        })
        .filter(|(_, column)| {
            useful.contains(&column.id())
                || named
                    .iter()
                    .any(|name| column.name().eq_ignore_ascii_case(name))
        })
        .map(|(position, column)| (column.id(), position))
        .collect()
}

const fn is_integer(data_type: pintail_types::DataType) -> bool {
    use pintail_types::DataType;
    matches!(
        data_type,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    )
}

/// Layout tag of an integer postings section; a reader meeting another
/// declines it.
const POSTINGS_LAYOUT: u8 = 1;
/// Layout tag of a text postings section: exact values, no collation.
const TEXT_POSTINGS_LAYOUT: u8 = 2;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).unwrap_or(0) | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).unwrap_or(0));
}

fn take_varint(bytes: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*position)
            .ok_or_else(|| "postings end inside a number".to_owned())?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("postings number is too long".to_owned())
}

/// The postings section for the column at schema position `index` of
/// `rows`, as a segment writes it: text values as [`encode_text_postings`]
/// lays them out, integers grouped, each group as its value's zigzag delta
/// from the previous group's, its row count, and its rows as ascending
/// deltas. `None` when a value is neither a plain integer the index can hold
/// nor text (or the column mixes them), which leaves the column unindexed.
pub(crate) fn encode_row_postings(rows: &[StoredRow], index: usize) -> Option<Vec<u8>> {
    if rows
        .iter()
        .any(|row| matches!(row.values().get(index), Some(Value::Utf8(_))))
    {
        return encode_text_postings(rows, index);
    }
    let postings = Postings::from_pairs(row_pairs(rows, index)?, rows.len());
    let mut out = Vec::with_capacity(postings.rows.len() * 3 + 16);
    out.push(POSTINGS_LAYOUT);
    put_varint(&mut out, postings.rows.len() as u64);
    let mut previous_value = 0_i64;
    let mut entry = 0_usize;
    while entry < postings.values.len() {
        let value = postings.values[entry];
        let end = entry + postings.values[entry..].partition_point(|other| *other == value);
        let delta = value.wrapping_sub(previous_value);
        put_varint(&mut out, ((delta << 1) ^ (delta >> 63)).cast_unsigned());
        put_varint(&mut out, (end - entry) as u64);
        let mut previous_row = 0_u32;
        for (offset, row) in postings.rows[entry..end].iter().enumerate() {
            let step = if offset == 0 {
                *row
            } else {
                row - previous_row
            };
            put_varint(&mut out, u64::from(step));
            previous_row = *row;
        }
        previous_value = value;
        entry = end;
    }
    Some(out)
}

/// A text postings section: `u8 layout (2) | varint entry_count | varint
/// group_count`, then per distinct value in ascending byte order its varint
/// length and UTF-8 bytes, a varint row count, and the rows as ascending
/// deltas. Exact values, so the section holds under every collation.
fn encode_text_postings(rows: &[StoredRow], index: usize) -> Option<Vec<u8>> {
    let mut groups: std::collections::BTreeMap<&str, Vec<u32>> = std::collections::BTreeMap::new();
    let mut entries = 0_usize;
    for (row, stored) in rows.iter().enumerate() {
        let row = u32::try_from(row).ok()?;
        match stored.values().get(index)? {
            Value::Null => {}
            Value::Utf8(text) => {
                groups.entry(text.as_str()).or_default().push(row);
                entries += 1;
            }
            _ => return None,
        }
    }
    let heap = groups.keys().map(|text| text.len()).sum::<usize>();
    let mut out = Vec::with_capacity(heap + entries * 3 + 16);
    out.push(TEXT_POSTINGS_LAYOUT);
    put_varint(&mut out, entries as u64);
    put_varint(&mut out, groups.len() as u64);
    for (text, group) in groups {
        put_varint(&mut out, text.len() as u64);
        out.extend_from_slice(text.as_bytes());
        put_varint(&mut out, group.len() as u64);
        let mut previous = 0_u32;
        for (offset, row) in group.iter().enumerate() {
            put_varint(
                &mut out,
                u64::from(if offset == 0 { *row } else { row - previous }),
            );
            previous = *row;
        }
    }
    Some(out)
}

/// Reads a text postings section back as `(value, rows)` groups, holding
/// the values to ascending byte order and every row to `row_count`.
fn decode_text_groups(bytes: &[u8], row_count: usize) -> Result<Vec<(&str, Vec<u32>)>, String> {
    if bytes.first() != Some(&TEXT_POSTINGS_LAYOUT) {
        return Err("unknown postings layout".to_owned());
    }
    let mut position = 1_usize;
    let size =
        |value: u64| usize::try_from(value).map_err(|_| "postings size does not fit".to_owned());
    let entries = size(take_varint(bytes, &mut position)?)?;
    let group_count = size(take_varint(bytes, &mut position)?)?;
    if entries > row_count || group_count > entries {
        return Err("more postings than rows".to_owned());
    }
    let mut groups: Vec<(&str, Vec<u32>)> = Vec::with_capacity(group_count);
    let mut seen = 0_usize;
    for _ in 0..group_count {
        let length = size(take_varint(bytes, &mut position)?)?;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "postings end inside a value".to_owned())?;
        let text = std::str::from_utf8(&bytes[position..end])
            .map_err(|_| "postings value is not UTF-8".to_owned())?;
        position = end;
        if groups
            .last()
            .is_some_and(|(previous, _)| previous.as_bytes() >= text.as_bytes())
        {
            return Err("postings values out of order".to_owned());
        }
        let count = size(take_varint(bytes, &mut position)?)?;
        if count == 0 || count > entries - seen {
            return Err("postings group size is out of range".to_owned());
        }
        let mut rows = Vec::with_capacity(count);
        let mut row = 0_u64;
        for offset in 0..count {
            let step = take_varint(bytes, &mut position)?;
            if offset > 0 && step == 0 {
                return Err("postings rows out of order".to_owned());
            }
            row = row
                .checked_add(step)
                .ok_or_else(|| "postings row overflows".to_owned())?;
            rows.push(
                u32::try_from(row)
                    .ok()
                    .filter(|row| (*row as usize) < row_count)
                    .ok_or_else(|| "postings row is outside the segment".to_owned())?,
            );
        }
        seen += count;
        groups.push((text, rows));
    }
    if seen != entries {
        return Err("postings entry count does not match its groups".to_owned());
    }
    if position != bytes.len() {
        return Err("trailing bytes after postings".to_owned());
    }
    Ok(groups)
}

/// Keyed postings from exact text groups: each distinct value hashed once.
fn keyed_text_postings(
    groups: Vec<(&str, Vec<u32>)>,
    keyer: &TextKeyer,
    row_count: usize,
) -> Postings {
    let entries = groups.iter().map(|(_, rows)| rows.len()).sum::<usize>();
    let mut hashed = groups
        .into_iter()
        .map(|(text, rows)| (keyer.value(text), rows))
        .collect::<Vec<_>>();
    hashed.sort_unstable_by_key(|(value, _)| *value);
    let mut values = Vec::with_capacity(entries);
    let mut all_rows = Vec::with_capacity(entries);
    for (value, rows) in hashed {
        values.extend(std::iter::repeat_n(value, rows.len()));
        all_rows.extend(rows);
    }
    Postings {
        values,
        rows: all_rows,
        row_count,
    }
}

fn row_pairs(rows: &[StoredRow], index: usize) -> Option<Vec<(i64, u32)>> {
    let mut pairs = Vec::with_capacity(rows.len());
    for (row, stored) in rows.iter().enumerate() {
        let row = u32::try_from(row).ok()?;
        match stored.values().get(index)? {
            Value::Null => {}
            Value::Int64(value) => pairs.push((*value, row)),
            Value::UInt64(value) => pairs.push((i64::try_from(*value).ok()?, row)),
            _ => return None,
        }
    }
    Some(pairs)
}

/// Reads a postings section back, holding every row to the segment's
/// `row_count` and the groups to ascending order.
fn decode_postings(bytes: &[u8], row_count: usize) -> Result<Postings, String> {
    if bytes.first() != Some(&POSTINGS_LAYOUT) {
        return Err("unknown postings layout".to_owned());
    }
    let mut position = 1_usize;
    let entries = usize::try_from(take_varint(bytes, &mut position)?)
        .map_err(|_| "postings entry count does not fit".to_owned())?;
    if entries > row_count {
        return Err("more postings than rows".to_owned());
    }
    let mut values = Vec::with_capacity(entries);
    let mut rows = Vec::with_capacity(entries);
    let mut previous_value = 0_i64;
    while rows.len() < entries {
        let zigzag = take_varint(bytes, &mut position)?;
        let delta = (zigzag >> 1).cast_signed() ^ -((zigzag & 1).cast_signed());
        let value = previous_value.wrapping_add(delta);
        if !values.is_empty() && value <= previous_value {
            return Err("postings values out of order".to_owned());
        }
        let count = usize::try_from(take_varint(bytes, &mut position)?)
            .map_err(|_| "postings group size does not fit".to_owned())?;
        if count == 0 || count > entries - rows.len() {
            return Err("postings group size is out of range".to_owned());
        }
        let mut row = 0_u64;
        for offset in 0..count {
            let step = take_varint(bytes, &mut position)?;
            if offset > 0 && step == 0 {
                return Err("postings rows out of order".to_owned());
            }
            row = row
                .checked_add(step)
                .ok_or_else(|| "postings row overflows".to_owned())?;
            let row = u32::try_from(row)
                .ok()
                .filter(|row| (*row as usize) < row_count)
                .ok_or_else(|| "postings row is outside the segment".to_owned())?;
            values.push(value);
            rows.push(row);
        }
        previous_value = value;
    }
    if position != bytes.len() {
        return Err("trailing bytes after postings".to_owned());
    }
    Ok(Postings {
        values,
        rows,
        row_count,
    })
}

fn record_build(
    meta: &segment::SegmentMeta,
    column_id: u32,
    postings: &Postings,
    elapsed: std::time::Duration,
) {
    let mut totals = TOTALS
        .get_or_init(|| Mutex::new((0, 0, 0)))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    totals.0 += postings.rows.len();
    totals.1 += postings.heap_bytes();
    totals.2 += elapsed.as_micros();
    pintail_log::log_debug!(
        "side index built file={} column={column_id} entries={} bytes={} build_us={} total_entries={} total_bytes={} total_build_us={}",
        meta.file_name,
        postings.rows.len(),
        postings.heap_bytes(),
        elapsed.as_micros(),
        totals.0,
        totals.1,
        totals.2
    );
}

fn build(
    directory: &Path,
    meta: &segment::SegmentMeta,
    schema: &TableSchema,
    column_id: u32,
    index_key: &IndexKey,
) -> Result<Option<Postings>, StoreError> {
    let Some(position) = schema
        .columns()
        .iter()
        .position(|column| column.id() == column_id)
    else {
        return Ok(None);
    };
    let Ok(row_count) = usize::try_from(meta.row_count) else {
        return Ok(None);
    };
    if u32::try_from(row_count).is_err() {
        return Ok(None);
    }
    // Postings a flush or compaction wrote describe the file under the schema
    // it was written with; after a schema change the decode below applies
    // the column's evolution instead. Damaged postings are an index lost,
    // not an answer lost: the build below reads the column itself.
    if meta.schema_fingerprint == segment::schema_fingerprint(schema) {
        match segment::read_postings_section(directory, meta, column_id) {
            Ok(Some(bytes)) => match decode_section(&bytes, row_count, index_key) {
                Ok(Some(postings)) => return Ok(Some(postings)),
                // A section of the other kind: the column changed type.
                Ok(None) => {}
                Err(reason) => pintail_log::log_error!(
                    "side index postings unreadable file={} column={column_id}: {reason}",
                    meta.file_name
                ),
            },
            Ok(None) => {}
            Err(error) => pintail_log::log_error!(
                "side index postings unreadable file={} column={column_id}: {error}",
                meta.file_name
            ),
        }
    }
    let used = AtomicUsize::new(0);
    let budget = segment::ScanMemoryBudget::new(&used, usize::MAX);
    let fetch = segment::read_projected_columns(
        directory,
        meta,
        schema,
        &[position],
        0,
        row_count,
        &budget,
    )?;
    if let IndexKey::Text(keyer) = index_key {
        return Ok(fetch
            .columns
            .first()
            .and_then(|column| text_pairs(column, keyer))
            .map(|pairs| Postings::from_pairs(pairs, row_count)));
    }
    let mut pairs = Vec::with_capacity(row_count);
    match fetch.columns.first() {
        Some(DecodedColumn::Int64 { values, validity }) => {
            for (row, value) in values.iter().enumerate() {
                if validity.is_valid(row) {
                    pairs.push((*value, u32::try_from(row).unwrap_or(u32::MAX)));
                }
            }
        }
        Some(DecodedColumn::UInt64 { values, validity }) => {
            for (row, value) in values.iter().enumerate() {
                if validity.is_valid(row) {
                    let Ok(value) = i64::try_from(*value) else {
                        return Ok(None);
                    };
                    pairs.push((value, u32::try_from(row).unwrap_or(u32::MAX)));
                }
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(Postings::from_pairs(pairs, row_count)))
}

/// A persisted section's postings under `index_key`; `None` for a section of
/// the other kind.
fn decode_section(
    bytes: &[u8],
    row_count: usize,
    index_key: &IndexKey,
) -> Result<Option<Postings>, String> {
    match (bytes.first(), index_key) {
        (Some(&POSTINGS_LAYOUT), IndexKey::Integer) => decode_postings(bytes, row_count).map(Some),
        (Some(&TEXT_POSTINGS_LAYOUT), IndexKey::Text(keyer)) => Ok(Some(keyed_text_postings(
            decode_text_groups(bytes, row_count)?,
            keyer,
            row_count,
        ))),
        (Some(&(POSTINGS_LAYOUT | TEXT_POSTINGS_LAYOUT)), _) => Ok(None),
        _ => Err("unknown postings layout".to_owned()),
    }
}

/// `(key hash, row)` for every non-NULL row of a decoded text column, each
/// distinct value hashed once. `None` when the column is not text.
fn text_pairs(column: &DecodedColumn, keyer: &TextKeyer) -> Option<Vec<(i64, u32)>> {
    let row_id = |row: usize| u32::try_from(row).ok();
    let mut pairs = Vec::with_capacity(column.len());
    match column {
        DecodedColumn::DictionaryUtf8 {
            dict_heap,
            dict_offsets,
            codes,
            validity,
        } => {
            let hashed = dict_offsets
                .windows(2)
                .map(|bounds| {
                    std::str::from_utf8(dict_heap.get(bounds[0]..bounds[1])?)
                        .ok()
                        .map(|text| keyer.value(text))
                })
                .collect::<Option<Vec<_>>>()?;
            for (row, code) in codes.iter().enumerate() {
                if validity.is_valid(row) {
                    pairs.push((*hashed.get(*code as usize)?, row_id(row)?));
                }
            }
        }
        DecodedColumn::Utf8 {
            heap,
            offsets,
            validity,
        } => {
            let mut hashed: HashMap<&[u8], i64> = HashMap::new();
            for row in 0..validity.len() {
                if !validity.is_valid(row) {
                    continue;
                }
                let bytes = heap.get(*offsets.get(row)?..*offsets.get(row + 1)?)?;
                let value = if let Some(value) = hashed.get(bytes) {
                    *value
                } else {
                    let value = keyer.value(std::str::from_utf8(bytes).ok()?);
                    hashed.insert(bytes, value);
                    value
                };
                pairs.push((value, row_id(row)?));
            }
        }
        DecodedColumn::Values(values) => {
            let mut hashed: HashMap<&str, i64> = HashMap::new();
            for (row, value) in values.iter().enumerate() {
                match value {
                    Value::Null => {}
                    Value::Utf8(text) => {
                        let value = *hashed
                            .entry(text.as_str())
                            .or_insert_with(|| keyer.value(text));
                        pairs.push((value, row_id(row)?));
                    }
                    _ => return None,
                }
            }
        }
        _ => return None,
    }
    Some(pairs)
}

/// Maps ranges over the concatenated candidate rows back to segment rows.
pub(crate) fn absolute_ranges(
    candidates: &[Range<usize>],
    relative: &[Range<usize>],
) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut push = |range: Range<usize>| match out.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => out.push(range),
    };
    let mut candidate = 0_usize;
    let mut offset = 0_usize;
    for range in relative {
        let mut position = range.start;
        while position < range.end && candidate < candidates.len() {
            let span = &candidates[candidate];
            let span_end = offset + span.len();
            if position >= span_end {
                offset = span_end;
                candidate += 1;
                continue;
            }
            let take_end = range.end.min(span_end);
            push(span.start + (position - offset)..span.start + (take_end - offset));
            position = take_end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_ranges_find_scattered_values_and_decline_common_ones() {
        let pairs = (0..1_000_u32)
            .map(|row| (i64::from(row % 97), row))
            .collect::<Vec<_>>();
        let postings = Postings::from_pairs(pairs, 1_000);
        let ranges = postings
            .candidate_ranges(&IndexProbe::Values(vec![5, 6]), 0, 1_000)
            .expect("selective");
        let rows = ranges.iter().flat_map(Clone::clone).collect::<Vec<_>>();
        let expected = (0..1_000_usize)
            .filter(|row| matches!(row % 97, 5 | 6))
            .collect::<Vec<_>>();
        assert_eq!(rows, expected);
        assert_eq!(ranges.len(), expected.len() / 2);
        let bounded = postings
            .candidate_ranges(&IndexProbe::Span(5, 5), 100, 300)
            .expect("selective");
        assert_eq!(bounded, vec![102..103, 199..200, 296..297]);
        assert!(
            postings
                .candidate_ranges(&IndexProbe::Span(0, 90), 0, 1_000)
                .is_none()
        );
        // Five values of 97 are one row in twenty: few enough beside a
        // second column to decode, too many for a filter's columns alone.
        let five = IndexProbe::Span(5, 9);
        assert!(postings.candidate_ranges(&five, 0, 1_000).is_some());
        assert!(
            postings
                .candidate_ranges_within(&five, 0, 1_000, MAX_FILTER_ONLY_CANDIDATE_SHARE)
                .is_none()
        );
    }

    #[test]
    fn the_default_cache_is_an_eighth_of_memory_and_never_under_its_floor() {
        assert_eq!(default_cache_bytes(0), 256 << 20);
        assert_eq!(default_cache_bytes(1 << 30), 256 << 20);
        assert_eq!(default_cache_bytes(16 << 30), 2 << 30);
        assert_eq!(default_cache_bytes(64 << 30), 8 << 30);
    }

    #[test]
    fn cache_evicts_least_recently_used_postings_past_its_limit() {
        let key = |column_id| CacheKey {
            path: PathBuf::from("segment"),
            segment_id: 1,
            row_count: 10,
            versions: (1, 2),
            schema_fingerprint: 3,
            column_id,
            data_type: None,
            key_id: 0,
        };
        let mut cache = Cache::default();
        for column in 0..3 {
            let _ = cache.slot(key(column));
            cache.charge(&key(column), 40, 100);
        }
        // The third build pushed the total to 120: the oldest went.
        assert!(!cache.entries.contains_key(&key(0)));
        assert_eq!(
            (cache.entries.len(), cache.resident, cache.evictions),
            (2, 80, 1)
        );
        // Touching column 1 makes column 2 the older one.
        let _ = cache.slot(key(1));
        let _ = cache.slot(key(3));
        cache.charge(&key(3), 40, 100);
        assert!(cache.entries.contains_key(&key(1)));
        assert!(!cache.entries.contains_key(&key(2)));
        // Postings larger than the whole limit are not kept at all.
        let _ = cache.slot(key(4));
        cache.charge(&key(4), 200, 100);
        assert!(!cache.entries.contains_key(&key(4)));
        assert_eq!((cache.entries.len(), cache.resident), (2, 80));
        // A different type for the same column is a different entry.
        let typed = CacheKey {
            data_type: Some(pintail_types::DataType::Int64),
            ..key(5)
        };
        assert_ne!(typed, key(5));
    }

    fn owner_rows(count: u64) -> Vec<StoredRow> {
        use pintail_types::{KeyPart, PrimaryKey};
        (0..count)
            .map(|id| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        if id % 11 == 0 {
                            Value::Null
                        } else {
                            // Scattered, negative and extreme values.
                            Value::Int64(match id % 5 {
                                0 => i64::MIN + i64::try_from(id % 3).expect("small"),
                                1 => i64::MAX - i64::try_from(id % 3).expect("small"),
                                _ => i64::try_from(id * 7_919 % 257).expect("small") - 128,
                            })
                        },
                    ],
                    1,
                    id % 13 == 0,
                )
            })
            .collect()
    }

    fn owner_schema() -> TableSchema {
        use pintail_types::{Column, DataType};
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "owner", DataType::Int64, true),
            ],
        )
        .expect("schema")
    }

    #[test]
    fn postings_sections_round_trip_and_refuse_damage() {
        let rows = owner_rows(2_000);
        let section = encode_row_postings(&rows, 1).expect("integer column");
        let built = Postings::from_pairs(row_pairs(&rows, 1).expect("pairs"), rows.len());
        let decoded = decode_postings(&section, rows.len()).expect("decode");
        assert_eq!(decoded.values, built.values);
        assert_eq!(decoded.rows, built.rows);
        // Well under the twelve bytes a resident entry takes.
        assert!(section.len() < built.rows.len() * 4, "{}", section.len());
        assert!(decode_postings(&section, 1_000).is_err());
        assert!(decode_postings(&section[..section.len() - 1], rows.len()).is_err());
        let mut longer = section.clone();
        longer.push(0);
        assert!(decode_postings(&longer, rows.len()).is_err());
        let empty = encode_row_postings(&owner_rows(0), 1).expect("empty");
        assert!(decode_postings(&empty, 0).expect("empty").rows.is_empty());
        // A text value among integers leaves the column unindexed.
        let mut text = owner_rows(3);
        text.push(StoredRow::new(
            pintail_types::PrimaryKey::new(vec![pintail_types::KeyPart::UInt64(9)]).expect("key"),
            vec![Value::UInt64(9), Value::Utf8("9".into())],
            1,
            false,
        ));
        assert!(encode_row_postings(&text, 1).is_none());
    }

    #[test]
    fn segments_carry_postings_for_useful_columns_and_old_ones_build_them() {
        override_side_index(Some(true));
        let schema = owner_schema();
        let rows = owner_rows(3_000);
        let directory = tempfile::tempdir().expect("temporary directory");
        let write = |id| {
            segment::write(
                directory.path(),
                id,
                &schema,
                &rows,
                256,
                segment::Compression::Lz4,
                true,
            )
            .expect("write segment")
        };
        let plain = write(1);
        assert!(
            segment::read_postings_section(directory.path(), &plain, 2)
                .expect("read")
                .is_none()
        );
        note_useful(directory.path(), 2);
        let indexed = write(2);
        let section = segment::read_postings_section(directory.path(), &indexed, 2)
            .expect("read")
            .expect("persisted postings");
        assert!(section.len() < rows.len() * 4);
        let persisted = postings(directory.path(), &indexed, &schema, 2, &IndexKey::Integer)
            .expect("postings")
            .expect("integer column");
        let rebuilt = postings(directory.path(), &plain, &schema, 2, &IndexKey::Integer)
            .expect("postings")
            .expect("integer column");
        assert_eq!(persisted.values, rebuilt.values);
        assert_eq!(persisted.rows, rebuilt.rows);
        // The segment itself still reads as before.
        segment::verify(directory.path(), &indexed, &schema).expect("verify");
        assert_eq!(
            segment::read(directory.path(), &indexed, &schema).expect("rows"),
            rows
        );
        override_side_index(None);
    }

    /// A keyer folding ASCII case and trailing spaces, standing in for a
    /// case-insensitive PAD SPACE collation.
    fn folding_keyer() -> TextKeyer {
        TextKeyer::new(
            7,
            Arc::new(|text: &str, out: &mut Vec<u8>| {
                out.extend(
                    text.trim_end_matches(' ')
                        .bytes()
                        .map(|b| b.to_ascii_lowercase()),
                );
            }),
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn text_postings_persist_exact_values_and_key_them_per_collation() {
        use pintail_types::{Column, DataType, KeyPart, PrimaryKey};
        override_side_index(Some(true));
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "label", DataType::Utf8, true),
            ],
        )
        .expect("schema");
        let spellings = [
            "Alpha",
            "alpha",
            "ALPHA  ",
            "beta",
            "Beta ",
            "\u{e9}t\u{e9}",
        ];
        let rows = (0..3_000_u64)
            .map(|id| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        if id % 7 == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(format!(
                                "{}{}",
                                spellings[usize::try_from(id % 6).expect("small")],
                                id % 40
                            ))
                        },
                    ],
                    1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        let section = encode_row_postings(&rows, 1).expect("text column");
        assert_eq!(section[0], TEXT_POSTINGS_LAYOUT);
        let groups = decode_text_groups(&section, rows.len()).expect("decode");
        assert_eq!(
            groups.iter().map(|(_, rows)| rows.len()).sum::<usize>(),
            rows.iter()
                .filter(|row| row.values()[1] != Value::Null)
                .count()
        );
        assert!(decode_text_groups(&section, 100).is_err());
        assert!(decode_text_groups(&section[..section.len() - 1], rows.len()).is_err());
        // An integer lookup finds a text section of the other kind.
        assert!(
            decode_section(&section, rows.len(), &IndexKey::Integer)
                .expect("kind")
                .is_none()
        );

        let directory = tempfile::tempdir().expect("temporary directory");
        let write = |id| {
            segment::write(
                directory.path(),
                id,
                &schema,
                &rows,
                256,
                segment::Compression::Lz4,
                true,
            )
            .expect("write segment")
        };
        let plain = write(1);
        note_useful(directory.path(), 2);
        let indexed = write(2);
        assert!(
            segment::read_postings_section(directory.path(), &indexed, 2)
                .expect("read")
                .is_some()
        );
        let keyer = folding_keyer();
        let key = IndexKey::Text(keyer.clone());
        let loaded = postings(directory.path(), &indexed, &schema, 2, &key)
            .expect("postings")
            .expect("text column");
        let built = postings(directory.path(), &plain, &schema, 2, &key)
            .expect("postings")
            .expect("text column");
        let probe = IndexProbe::Values(vec![i128::from(keyer.value("alpha5"))]);
        let expected = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                matches!(&row.values()[1], Value::Utf8(text)
                    if text.trim_end_matches(' ').eq_ignore_ascii_case("alpha5"))
            })
            .map(|(row, _)| row)
            .collect::<Vec<_>>();
        for found in [&loaded, &built] {
            let rows = found
                .candidate_ranges(&probe, 0, 3_000)
                .expect("selective")
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            assert_eq!(rows, expected);
        }
        assert!(!expected.is_empty());
        override_side_index(None);
    }

    #[test]
    fn absolute_ranges_split_across_candidate_spans() {
        let candidates = vec![10..13, 20..21, 30..34];
        assert_eq!(
            absolute_ranges(&candidates, &[1..5, 6..8]),
            vec![11..13, 20..21, 30..31, 32..34]
        );
        let whole = [0..8, 8..8];
        assert_eq!(
            absolute_ranges(&candidates, &whole),
            vec![10..13, 20..21, 30..34]
        );
        assert!(absolute_ranges(&candidates, &[]).is_empty());
    }
}
