use std::{
    collections::{BTreeMap, btree_map},
    ops::{Bound, RangeBounds},
    sync::Arc,
};

use pintail_types::{PrimaryKey, StoredRow};

/// Rows a chunk is built with; a chunk that grows past twice this splits
/// into two halves. Small enough that the first write after a snapshot
/// pinned the rows copies little, large enough that the chunk list itself
/// (which that write copies too) stays short.
const CHUNK_ROWS: usize = 256;

type Chunk = BTreeMap<PrimaryKey, StoredRow>;

/// The memtable's rows in key order, as consecutive key ranges ("chunks")
/// each behind its own [`Arc`].
///
/// A snapshot shares the whole set. A write while one is held copies the
/// list of chunks and the one chunk it lands in, not every row: the
/// snapshot keeps the chunks as they were, the memtable the copies.
#[derive(Clone, Default)]
pub(crate) struct MemtableRows {
    /// Each chunk with its lowest key, in key order. No chunk is empty, and
    /// every key of a chunk is below the lowest key of the next.
    chunks: Vec<(PrimaryKey, Arc<Chunk>)>,
    len: usize,
}

/// What [`MemtableRows::insert`] did with a row.
enum Inserted {
    /// The key held a newer version; nothing changed.
    Refused,
    /// The row took a new key.
    Added,
    /// The row replaced this older version of its key.
    Replaced(StoredRow),
}

impl MemtableRows {
    /// Rows in key order with distinct keys, as [`Memtable::take_rows`]
    /// hands them out.
    fn from_sorted(rows: Vec<StoredRow>) -> Self {
        let len = rows.len();
        let mut chunks = Vec::with_capacity(len.div_ceil(CHUNK_ROWS));
        let mut rows = rows.into_iter().peekable();
        while let Some(first) = rows.peek().map(|row| row.key().clone()) {
            let chunk = rows
                .by_ref()
                .take(CHUNK_ROWS)
                .map(|row| (row.key().clone(), row))
                .collect::<Chunk>();
            chunks.push((first, Arc::new(chunk)));
        }
        Self { chunks, len }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many chunks hold the rows.
    #[cfg(test)]
    fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// The place of the chunk that holds `key` or would: the last one
    /// starting at or below it. `None` when every chunk starts above it.
    fn chunk_of(&self, key: &PrimaryKey) -> Option<usize> {
        // A change stream mostly appends above every key it has written:
        // such a key lands in the last chunk, found without a search.
        match self.chunks.last() {
            Some((first, _)) if key >= first => Some(self.chunks.len() - 1),
            _ => self
                .chunks
                .partition_point(|(first, _)| first <= key)
                .checked_sub(1),
        }
    }

    pub(crate) fn get(&self, key: &PrimaryKey) -> Option<&StoredRow> {
        self.chunks[self.chunk_of(key)?].1.get(key)
    }

    /// The rows of `range` in key order.
    pub(crate) fn range(&self, range: impl RangeBounds<PrimaryKey>) -> Range<'_> {
        let bounds = (range.start_bound().cloned(), range.end_bound().cloned());
        // From the chunk holding the range's first key, through the last
        // chunk that starts inside the range.
        let start = match &bounds.0 {
            Bound::Included(key) | Bound::Excluded(key) => self.chunk_of(key).unwrap_or(0),
            Bound::Unbounded => 0,
        };
        let end = match &bounds.1 {
            Bound::Included(key) => self.chunks.partition_point(|(first, _)| first <= key),
            Bound::Excluded(key) => self.chunks.partition_point(|(first, _)| first < key),
            Bound::Unbounded => self.chunks.len(),
        };
        Range {
            chunks: self.chunks[start.min(end)..end].iter(),
            front: None,
            back: None,
            bounds,
        }
    }

    pub(crate) fn iter(&self) -> Range<'_> {
        self.range(..)
    }

    pub(crate) fn keys(&self) -> impl DoubleEndedIterator<Item = &PrimaryKey> + Clone {
        self.iter().map(|(key, _)| key)
    }

    pub(crate) fn values(&self) -> impl DoubleEndedIterator<Item = &StoredRow> + Clone {
        self.iter().map(|(_, row)| row)
    }

    /// Writes `row` unless its key holds a newer version.
    fn insert(&mut self, row: StoredRow) -> Inserted {
        let index = match self.chunk_of(row.key()) {
            Some(index) => index,
            None if self.chunks.is_empty() => {
                self.chunks.push((row.key().clone(), Arc::default()));
                0
            }
            None => {
                // A new lowest key opens the first chunk.
                self.chunks[0].0 = row.key().clone();
                0
            }
        };
        let chunk = Arc::make_mut(&mut self.chunks[index].1);
        let inserted = match chunk.entry(row.key().clone()) {
            btree_map::Entry::Occupied(mut current) => {
                if current.get().version() > row.version() {
                    return Inserted::Refused;
                }
                Inserted::Replaced(current.insert(row))
            }
            btree_map::Entry::Vacant(slot) => {
                slot.insert(row);
                Inserted::Added
            }
        };
        if matches!(inserted, Inserted::Added) {
            self.len += 1;
            if chunk.len() > 2 * CHUNK_ROWS {
                let upper = split_upper(chunk);
                if let Some(first) = upper.keys().next().cloned() {
                    self.chunks.insert(index + 1, (first, Arc::new(upper)));
                }
            }
        }
        inserted
    }

    /// Every row in key order, moved out where no snapshot shares its chunk.
    fn into_rows(self) -> Vec<StoredRow> {
        let mut rows = Vec::with_capacity(self.len);
        for (_, chunk) in self.chunks {
            match Arc::try_unwrap(chunk) {
                Ok(chunk) => rows.extend(chunk.into_values()),
                Err(shared) => rows.extend(shared.values().cloned()),
            }
        }
        rows
    }
}

/// Moves the upper half of `chunk` out into a chunk of its own.
fn split_upper(chunk: &mut Chunk) -> Chunk {
    match chunk.keys().nth(chunk.len() / 2).cloned() {
        Some(middle) => chunk.split_off(&middle),
        None => Chunk::new(),
    }
}

impl<'a> IntoIterator for &'a MemtableRows {
    type Item = (&'a PrimaryKey, &'a StoredRow);
    type IntoIter = Range<'a>;

    fn into_iter(self) -> Range<'a> {
        self.iter()
    }
}

/// [`MemtableRows::range`]'s rows: each chunk's part of the range in turn,
/// from either end.
#[derive(Clone)]
pub(crate) struct Range<'a> {
    chunks: std::slice::Iter<'a, (PrimaryKey, Arc<Chunk>)>,
    front: Option<btree_map::Range<'a, PrimaryKey, StoredRow>>,
    back: Option<btree_map::Range<'a, PrimaryKey, StoredRow>>,
    bounds: (Bound<PrimaryKey>, Bound<PrimaryKey>),
}

impl<'a> Iterator for Range<'a> {
    type Item = (&'a PrimaryKey, &'a StoredRow);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.front.as_mut().and_then(Iterator::next) {
                return Some(row);
            }
            match self.chunks.next() {
                Some((_, chunk)) => self.front = Some(chunk.range(self.bounds.clone())),
                None => return self.back.as_mut().and_then(Iterator::next),
            }
        }
    }
}

impl DoubleEndedIterator for Range<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.back.as_mut().and_then(DoubleEndedIterator::next_back) {
                return Some(row);
            }
            match self.chunks.next_back() {
                Some((_, chunk)) => self.back = Some(chunk.range(self.bounds.clone())),
                None => {
                    return self.front.as_mut().and_then(DoubleEndedIterator::next_back);
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Memtable {
    rows: Arc<MemtableRows>,
    estimated_bytes: usize,
    /// The lowest version applied since the memtable was last cleared: no
    /// row it holds is older. Replacing a row can leave this below the
    /// oldest one still held, never above it.
    oldest_version: Option<u64>,
    /// The highest version applied since the memtable was last cleared.
    newest_version: Option<u64>,
    /// The image scans build of the rows as they are now; replaced when a
    /// write changes rows a scan has read through it or may yet.
    image: Arc<crate::store::MemtableImage>,
    /// Moves with every change to the rows - an applied row or a clear -
    /// and never goes back, so two moments of one memtable that report the
    /// same count hold the same rows.
    changes: u64,
}

impl Memtable {
    pub(crate) fn apply(&mut self, row: &StoredRow) -> bool {
        self.insert(row.clone())
    }

    /// [`Self::apply`] for a row the caller is done with: the memtable
    /// keeps the row itself rather than a copy of it.
    pub(crate) fn insert(&mut self, row: StoredRow) -> bool {
        let version = row.version();
        let bytes = row.estimated_bytes();
        match Arc::make_mut(&mut self.rows).insert(row) {
            Inserted::Refused => return false,
            Inserted::Replaced(previous) => {
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(previous.estimated_bytes());
            }
            Inserted::Added => {}
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(bytes);
        if Arc::strong_count(&self.image) > 1 || self.image.is_built() {
            self.image = Arc::default();
        }
        self.changes = self.changes.wrapping_add(1);
        self.oldest_version = Some(
            self.oldest_version
                .map_or(version, |oldest| oldest.min(version)),
        );
        self.newest_version = Some(
            self.newest_version
                .map_or(version, |newest| newest.max(version)),
        );
        true
    }

    /// The row the memtable holds for `key`.
    pub(crate) fn get(&self, key: &PrimaryKey) -> Option<&StoredRow> {
        self.rows.get(key)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows in key order, for a flush. They are moved out when no scan
    /// shares them, and the memtable then answers as empty until it is
    /// cleared or the rows are handed back with [`Self::restore`]; shared
    /// rows are copied and left in place.
    pub(crate) fn take_rows(&mut self) -> Vec<StoredRow> {
        match Arc::get_mut(&mut self.rows) {
            Some(rows) => std::mem::take(rows).into_rows(),
            None => self.rows.values().cloned().collect(),
        }
    }

    /// Puts back rows [`Self::take_rows`] moved out, after a flush that did
    /// not finish. Nothing else about the memtable changed in between.
    pub(crate) fn restore(&mut self, rows: Vec<StoredRow>) {
        if self.rows.is_empty() {
            self.rows = Arc::new(MemtableRows::from_sorted(rows));
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<MemtableRows> {
        Arc::clone(&self.rows)
    }

    /// The image of the rows [`Self::snapshot`] hands out now.
    pub(crate) fn image(&self) -> Arc<crate::store::MemtableImage> {
        Arc::clone(&self.image)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub(crate) fn oldest_version(&self) -> Option<u64> {
        self.oldest_version
    }

    pub(crate) fn newest_version(&self) -> Option<u64> {
        self.newest_version
    }

    /// How many times the rows have changed; see `changes`.
    pub(crate) fn changes(&self) -> u64 {
        self.changes
    }

    pub(crate) fn clear(&mut self) {
        self.changes = self.changes.wrapping_add(1);
        self.rows = Arc::new(MemtableRows::default());
        self.image = Arc::default();
        self.estimated_bytes = 0;
        self.oldest_version = None;
        self.newest_version = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{CHUNK_ROWS, Memtable, MemtableRows};
    use pintail_types::{KeyPart, PrimaryKey, StoredRow, Value};
    use std::collections::BTreeMap;
    use std::ops::{Bound, RangeBounds};

    fn key(id: u64) -> PrimaryKey {
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
    }

    fn row(id: u64, version: u64, deleted: bool) -> StoredRow {
        StoredRow::new(
            key(id),
            vec![Value::UInt64(id), Value::Utf8(format!("v{version}"))],
            version,
            deleted,
        )
    }

    /// A small deterministic sequence standing in for a random one.
    fn mixed(index: u64, modulus: u64) -> u64 {
        index.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) % modulus
    }

    fn pairs(rows: &MemtableRows) -> Vec<(u64, u64, bool)> {
        rows.iter()
            .map(|(key, row)| {
                assert_eq!(key, row.key());
                let [KeyPart::UInt64(id)] = key.parts() else {
                    panic!("unexpected key")
                };
                (*id, row.version(), row.is_deleted())
            })
            .collect()
    }

    fn model_pairs(model: &BTreeMap<u64, (u64, bool)>) -> Vec<(u64, u64, bool)> {
        model
            .iter()
            .map(|(id, (version, deleted))| (*id, *version, *deleted))
            .collect()
    }

    /// Applies one write to the model the way the memtable must: a newer
    /// or equal version replaces, an older one is refused.
    fn model_apply(
        model: &mut BTreeMap<u64, (u64, bool)>,
        id: u64,
        version: u64,
        deleted: bool,
    ) -> bool {
        match model.get(&id) {
            Some((current, _)) if *current > version => false,
            _ => {
                model.insert(id, (version, deleted));
                true
            }
        }
    }

    #[test]
    fn chunked_rows_match_an_ordered_map_through_splits_and_snapshots() {
        let mut memtable = Memtable::default();
        let mut model = BTreeMap::new();
        let mut held = Vec::new();
        for index in 0..20_000u64 {
            // Keys descending, ascending and scattered, versions mostly
            // rising with some stale replays and deletes.
            let id = match index % 3 {
                0 => 1_000_000 - index,
                1 => 1_000_000 + index,
                _ => mixed(index, 2_000_000),
            };
            let version = if index % 11 == 0 { index / 2 } else { index };
            let deleted = index % 7 == 0;
            let applied = memtable.insert(row(id, version, deleted));
            assert_eq!(applied, model_apply(&mut model, id, version, deleted));
            if index % 2_500 == 0 {
                held.push((memtable.snapshot(), model_pairs(&model)));
            }
        }
        assert!(memtable.snapshot().chunk_count() > model.len() / (2 * CHUNK_ROWS));
        assert_eq!(memtable.snapshot().len(), model.len());
        assert_eq!(pairs(&memtable.snapshot()), model_pairs(&model));
        // Every snapshot still sees the rows as they were when it was taken.
        for (snapshot, expected) in &held {
            assert_eq!(snapshot.len(), expected.len());
            assert_eq!(&pairs(snapshot), expected);
        }
        let rows = memtable.snapshot();
        for probe in [0, 1, 999_999, 1_000_000, 1_000_001, 1_500_000, 3_000_000] {
            assert_eq!(
                rows.get(&key(probe)).map(StoredRow::version),
                model.get(&probe).map(|(version, _)| *version),
            );
        }
        for (lo, hi) in [
            (Bound::Unbounded, Bound::Unbounded),
            (
                Bound::Included(key(990_000)),
                Bound::Excluded(key(1_010_000)),
            ),
            (
                Bound::Excluded(key(990_000)),
                Bound::Included(key(1_010_000)),
            ),
            (Bound::Included(key(5)), Bound::Included(key(5))),
            (Bound::Unbounded, Bound::Included(key(1_000_000))),
            (Bound::Excluded(key(1_500_000)), Bound::Unbounded),
            (Bound::Included(key(3_000_000)), Bound::Unbounded),
        ] {
            let in_range =
                |id: &u64| (lo.as_ref().map(key_id), hi.as_ref().map(key_id)).contains(id);
            let expected = model_pairs(&model)
                .into_iter()
                .filter(|(id, ..)| in_range(id))
                .collect::<Vec<_>>();
            let forward = rows
                .range((lo.clone(), hi.clone()))
                .map(|(key, row)| (key_id(key), row.version(), row.is_deleted()))
                .collect::<Vec<_>>();
            assert_eq!(forward, expected);
            let mut backward = rows
                .range((lo.clone(), hi.clone()))
                .rev()
                .map(|(key, row)| (key_id(key), row.version(), row.is_deleted()))
                .collect::<Vec<_>>();
            backward.reverse();
            assert_eq!(backward, expected);
            // Taken from both ends at once, the walk meets in the middle.
            let mut both = rows.range((lo.clone(), hi.clone()));
            let (mut front, mut back) = (Vec::new(), Vec::new());
            while let Some((key, _)) = both.next() {
                front.push(key_id(key));
                let Some((key, _)) = both.next_back() else {
                    break;
                };
                back.push(key_id(key));
            }
            back.reverse();
            front.extend(back);
            assert_eq!(
                front,
                expected.iter().map(|(id, ..)| *id).collect::<Vec<_>>()
            );
        }
        assert_eq!(rows.keys().next().map(key_id), model.keys().next().copied());
        assert_eq!(
            rows.keys().next_back().map(key_id),
            model.keys().next_back().copied()
        );
    }

    fn key_id(key: &PrimaryKey) -> u64 {
        let [KeyPart::UInt64(id)] = key.parts() else {
            panic!("unexpected key")
        };
        *id
    }

    #[test]
    fn a_snapshot_taken_mid_batch_sees_exactly_the_rows_before_it() {
        let mut memtable = Memtable::default();
        let mut model = BTreeMap::new();
        for id in 0..10_000 {
            memtable.insert(row(id * 2, 1, false));
            model_apply(&mut model, id * 2, 1, false);
        }
        let before = pairs(&memtable.snapshot());
        let mut snapshot = None;
        // A batch that adds keys between and beyond the held ones, replaces
        // some, deletes some and replays a stale version, with the snapshot
        // pinned partway through it.
        for id in 0..3_000u64 {
            if id == 1_500 {
                snapshot = Some((memtable.snapshot(), model_pairs(&model)));
            }
            let target = mixed(id, 25_000);
            memtable.insert(row(target, 2, id % 5 == 0));
            model_apply(&mut model, target, 2, id % 5 == 0);
        }
        assert!(!memtable.insert(row(4, 0, true)));
        let (snapshot, at_pin) = snapshot.expect("pinned mid-batch");
        assert_ne!(at_pin, before);
        assert_eq!(pairs(&snapshot), at_pin);
        assert_eq!(snapshot.len(), at_pin.len());
        assert_eq!(pairs(&memtable.snapshot()), model_pairs(&model));
    }

    #[test]
    fn a_write_after_a_snapshot_copies_one_chunk() {
        let mut memtable = Memtable::default();
        for id in 0..20 * CHUNK_ROWS as u64 {
            memtable.insert(row(id, 1, false));
        }
        let snapshot = memtable.snapshot();
        memtable.insert(row(7, 2, false));
        let unshared = memtable
            .rows
            .chunks
            .iter()
            .filter(|(_, chunk)| {
                !snapshot
                    .chunks
                    .iter()
                    .any(|(_, held)| std::sync::Arc::ptr_eq(chunk, held))
            })
            .count();
        assert_eq!(unshared, 1);
        // Ascending keys leave chunks of 256 to 512 rows.
        assert!(memtable.rows.chunks.len() >= 10);
        assert_eq!(snapshot.get(&key(7)).map(StoredRow::version), Some(1));
        assert_eq!(memtable.get(&key(7)).map(StoredRow::version), Some(2));
    }

    #[test]
    fn taken_rows_restore_to_the_same_rows() {
        let mut memtable = Memtable::default();
        for index in 0..5_000u64 {
            memtable.insert(row(mixed(index, 100_000), index, index % 3 == 0));
        }
        let before = pairs(&memtable.snapshot());
        let taken = memtable.take_rows();
        assert!(memtable.is_empty());
        assert_eq!(taken.len(), before.len());
        memtable.restore(taken);
        assert_eq!(pairs(&memtable.snapshot()), before);
        // With a snapshot held the rows are copied and stay in place.
        let held = memtable.snapshot();
        let copied = memtable.take_rows();
        assert_eq!(copied.len(), before.len());
        assert_eq!(pairs(&memtable.snapshot()), before);
        assert_eq!(pairs(&held), before);
    }
}
