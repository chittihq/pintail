//! Reader-owned immutable table views and the backup artifacts
//! taken from them.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicUsize},
};

use super::scan::{
    ProjectedRow, ProjectedScan, ProjectedScanStream, ScanPart, ScanStats,
    bound_range_is_searchable, columns_to_rows,
};
use super::{
    ProjectedCandidate, ProjectedSource, WAL_FILE, adapt_recovered_row, apply_latest,
    apply_projected_latest, projected_scan_pool, register_pinned_manifest,
};
use rayon::prelude::*;

use pintail_types::{PrimaryKey, StoredRow, TableSchema};

use crate::{
    StoreError,
    manifest::{self, Manifest},
    memtable::Memtable,
    segment,
};

/// A reader-owned immutable table view.
#[derive(Clone)]
pub struct TableSnapshot {
    /// The opening this snapshot came from; see `TableStore::instance`.
    pub(super) instance: u64,
    pub(super) memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    /// The image scans build of `memtable` and share (see
    /// [`super::MemtableImage`]).
    pub(super) memtable_image: Arc<super::MemtableImage>,
    /// No memtable row is older than this; `None` when it is empty. A
    /// bound, not always the oldest row: a replaced version can leave it
    /// lower, which only sends a caller to the exact check.
    pub(super) memtable_oldest: Option<u64>,
    pub(super) manifest: Arc<Manifest>,
    pub(super) directory: PathBuf,
    pub(super) schema: TableSchema,
    /// Bytes the replayed memtable holds, so whoever keeps this snapshot
    /// resident can charge it to a budget.
    pub(super) estimated_bytes: usize,
}

/// One segment's identity and key span for a grouped fold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupedFoldSpan {
    /// The segment's file name. Segments are never rewritten, so this
    /// identifies the bytes a fold was taken over.
    pub file_name: String,
    /// Inclusive span the segment covers.
    pub min_key: PrimaryKey,
    /// Inclusive span the segment covers.
    pub max_key: PrimaryKey,
    /// Whether the memtable holds a key inside the span, which makes a
    /// fold over it correct to use now and wrong to keep.
    pub dirty: bool,
}

/// Immutable files pinned by a reader snapshot for native backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupArtifacts {
    generation: u64,
    manifest: Vec<u8>,
    segments: Vec<BackupSegment>,
}

impl BackupArtifacts {
    /// Returns the pinned manifest generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the encoded storage manifest that references the pinned files.
    #[must_use]
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Returns the immutable segment files referenced by the manifest.
    #[must_use]
    pub fn segments(&self) -> &[BackupSegment] {
        &self.segments
    }
}

/// One immutable storage segment pinned for backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSegment {
    file_name: String,
    path: PathBuf,
}

impl BackupSegment {
    /// Returns the portable segment file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Returns the local path to the pinned segment.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TableSnapshot {
    /// Upper bound on physical input rows, including obsolete versions and
    /// tombstones. Unlike source estimates this includes the pinned WAL tail.
    #[must_use]
    pub fn physical_row_upper_bound(&self) -> u64 {
        self.manifest.segments.iter().fold(
            u64::try_from(self.memtable.len()).unwrap_or(u64::MAX),
            |rows, segment| rows.saturating_add(segment.row_count),
        )
    }

    /// Physical rows whose segment key bounds overlap this range. Includes
    /// every pinned memtable row conservatively, without scanning its values.
    #[must_use]
    pub fn physical_range_row_upper_bound(&self, start: &PrimaryKey, end: &PrimaryKey) -> u64 {
        self.manifest
            .segments
            .iter()
            .filter(|segment| segment.min_key <= *end && segment.max_key >= *start)
            .fold(
                u64::try_from(self.memtable.len()).unwrap_or(u64::MAX),
                |rows, segment| rows.saturating_add(segment.row_count),
            )
    }

    /// Bytes of unflushed rows this snapshot keeps resident: the WAL tail
    /// replayed into memory at open, or the writer's live memtable. Segment
    /// data is read from files and not counted.
    #[must_use]
    pub const fn estimated_memtable_bytes(&self) -> usize {
        self.estimated_bytes
    }

    /// How many immutable segments this view reads.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.manifest.segments.len()
    }

    /// Bytes a full read of this view touches: its segment files as they
    /// are on disk plus the rows it holds in memory. One `stat` per
    /// segment, so a caller asking repeatedly should keep the answer.
    #[must_use]
    pub fn stored_bytes(&self) -> u64 {
        self.manifest.segments.iter().fold(
            u64::try_from(self.estimated_bytes).unwrap_or(u64::MAX),
            |bytes, segment| {
                let length = std::fs::metadata(self.directory.join(&segment.file_name))
                    .map_or(0, |metadata| metadata.len());
                bytes.saturating_add(length)
            },
        )
    }

    /// This snapshot's opening; see `TableStore::instance`.
    ///
    /// Anything caching a result against the table's directory has to carry
    /// this too, or a table recreated at that path is answered from its
    /// predecessor's bytes.
    #[must_use]
    pub const fn instance(&self) -> u64 {
        self.instance
    }

    /// The snapshot's data identity when every visible row is
    /// segment-resident: `(table directory, opening, manifest generation)`
    /// with an empty memtable. Two snapshots with the same identity see
    /// byte-for-byte identical data, so exactness-preserving caches (the
    /// settled aggregate memo) key on it; any ingest or flush changes it.
    ///
    /// The opening is part of it because the other two are not enough: a
    /// directory can be reclaimed, and the table that replaces it starts
    /// from an empty manifest and walks the same generations.
    #[must_use]
    pub fn settled_identity(&self) -> Option<(&std::path::Path, u64, u64)> {
        self.memtable.is_empty().then(|| {
            (
                self.directory.as_path(),
                self.instance,
                self.manifest.generation,
            )
        })
    }

    /// Per-segment SMAs plus residual memtable rows, when the fold is
    /// provably exact under merge-on-read (WS3-B, docs/decisions.md):
    /// every segment carries SMAs and zero tombstones, segment key ranges
    /// are pairwise disjoint (no cross-segment overlays), and every
    /// memtable row is a pure insert above the whole segment key space.
    /// Any tombstone, overlap, or update returns `None` — MIN/MAX cannot
    /// be delta-adjusted under deletes, so the fold never tries.
    #[must_use]
    pub fn sma_fold_state(&self) -> Option<(Vec<&crate::segment::SegmentSmas>, Vec<&StoredRow>)> {
        let mut segments: Vec<&crate::segment::SegmentMeta> =
            self.manifest.segments.iter().collect();
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        for pair in segments.windows(2) {
            if pair[1].min_key <= pair[0].max_key {
                return None;
            }
        }
        let mut smas = Vec::with_capacity(segments.len());
        for meta in &segments {
            let sma = meta.smas.as_ref()?;
            if sma.tombstones != 0 {
                return None;
            }
            smas.push(sma);
        }
        let max_segment_key = segments.last().map(|meta| &meta.max_key);
        let mut rows = Vec::with_capacity(self.memtable.len());
        for row in self.memtable.values() {
            if row.is_deleted() || max_segment_key.is_some_and(|max| row.key() <= max) {
                return None;
            }
            rows.push(row);
        }
        Some((smas, rows))
    }

    /// How many rows - versions and tombstones - the memtable holds. Every
    /// row [`Self::sma_fold_state`] and [`Self::insert_only_delta`] return
    /// is one of these, so a caller that bounds those rows can decline
    /// before either walks the memtable.
    #[must_use]
    pub fn memtable_len(&self) -> usize {
        self.memtable.len()
    }

    /// The table directory these segments live in; with a segment's file
    /// name it identifies bytes that are never rewritten.
    #[must_use]
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }

    /// Per-segment identity and key span for a grouped fold, plus the
    /// memtable rows that fall outside every segment.
    ///
    /// A segment is a file that is never rewritten, so an aggregate folded
    /// over one segment's key span stays true for as long as that file is
    /// in the manifest - which is what lets a caller keep the fold and
    /// reuse it. `dirty` marks a span the memtable holds a key inside: a
    /// scan of that span sees the memtable's version, so its fold is
    /// correct but must not be kept, because the next write changes it.
    ///
    /// `None` unless the segments are key-disjoint. Overlapping segments
    /// would put the same row in two spans, and a fold over each would
    /// count it twice. `None` too past `outside_limit` rows outside every
    /// span: the caller clones each one, and stops walking there.
    #[must_use]
    pub fn grouped_fold_spans(
        &self,
        outside_limit: usize,
    ) -> Option<(Vec<GroupedFoldSpan>, Vec<&StoredRow>)> {
        let mut segments: Vec<&crate::segment::SegmentMeta> =
            self.manifest.segments.iter().collect();
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        for pair in segments.windows(2) {
            if pair[1].min_key <= pair[0].max_key {
                return None;
            }
        }
        let mut spans = segments
            .iter()
            .map(|meta| GroupedFoldSpan {
                file_name: meta.file_name.clone(),
                min_key: meta.min_key.clone(),
                max_key: meta.max_key.clone(),
                dirty: false,
            })
            .collect::<Vec<_>>();
        // A span is dirty when the memtable holds any key inside it, which
        // one ordered lookup per span answers; only the gaps between spans
        // are walked, for the rows no segment covers. Walking every memtable
        // row instead made each scan open pay for the whole memtable - for a
        // table taking updates, every scan of it, folded or not.
        for span in &mut spans {
            span.dirty = self
                .memtable
                .range(span.min_key.clone()..=span.max_key.clone())
                .next()
                .is_some();
        }
        let mut outside = Vec::new();
        for gap in 0..=spans.len() {
            let lo = gap
                .checked_sub(1)
                .map_or(std::ops::Bound::Unbounded, |before| {
                    std::ops::Bound::Excluded(spans[before].max_key.clone())
                });
            let hi = spans.get(gap).map_or(std::ops::Bound::Unbounded, |after| {
                std::ops::Bound::Excluded(after.min_key.clone())
            });
            if !bound_range_is_searchable(&lo, &hi) {
                continue;
            }
            // A tombstone outside every span supersedes nothing.
            for (_, row) in self.memtable.range((lo, hi)) {
                if !row.is_deleted() {
                    if outside.len() == outside_limit {
                        return None;
                    }
                    outside.push(row);
                }
            }
        }
        Some((spans, outside))
    }

    /// The segment-resident identity plus the memtable rows, when every
    /// memtable row is a pure insert above the segment key space (no
    /// tombstones, no updates of segment rows). The delta-maintained
    /// aggregate memo merges these rows onto the generation-keyed result;
    /// any overlap or delete makes the merge unsound and returns `None`.
    #[must_use]
    pub fn insert_only_delta(&self) -> Option<(&std::path::Path, u64, Vec<&StoredRow>)> {
        if self.memtable.is_empty() {
            return None;
        }
        let max_segment_key = self
            .manifest
            .segments
            .iter()
            .map(|meta| &meta.max_key)
            .max();
        let mut rows = Vec::with_capacity(self.memtable.len());
        for row in self.memtable.values() {
            if row.is_deleted() || max_segment_key.is_some_and(|max| row.key() <= max) {
                return None;
            }
            rows.push(row);
        }
        Some((self.directory.as_path(), self.manifest.generation, rows))
    }

    /// Opens a reader-only snapshot without claiming the table writer lock.
    ///
    /// The reader pins one durable manifest and merges complete WAL records
    /// newer than that manifest. A concurrent manifest publication causes a
    /// bounded retry, so a reader cannot combine an old segment set with a
    /// newly truncated WAL.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing table directory, corrupt manifest,
    /// segment, or WAL, incompatible schema, or repeated concurrent manifest
    /// replacement.
    pub fn open(directory: impl AsRef<Path>, schema: TableSchema) -> Result<Self, StoreError> {
        let directory = std::fs::canonicalize(directory.as_ref())
            .map_err(|error| StoreError::io("canonicalize table reader directory", error))?;
        for _ in 0..8 {
            let manifest = Arc::new(manifest::load(&directory, &schema)?);
            register_pinned_manifest(&directory, &manifest);
            let recovery = crate::wal::recover_read_only(&directory.join(WAL_FILE))?;
            let latest = manifest::load(&directory, &schema)?;
            if manifest.generation != latest.generation
                || manifest.epoch != latest.epoch
                || manifest.flushed_sequence != latest.flushed_sequence
            {
                continue;
            }
            let mut memtable = Memtable::default();
            for batch in recovery.batches {
                if batch.table_id != 0 || batch.sequence <= manifest.flushed_sequence {
                    continue;
                }
                for row in batch.rows {
                    let row = adapt_recovered_row(&schema, &batch.columns, &row)?;
                    memtable.apply(&row);
                }
            }
            let verification = manifest
                .segments
                .iter()
                .try_for_each(|meta| segment::verify(&directory, meta, &schema));
            if let Err(error) = verification {
                let current = manifest::load(&directory, &schema)?;
                if current.generation != manifest.generation || current.epoch != manifest.epoch {
                    continue;
                }
                return Err(error);
            }
            let estimated_bytes = memtable.estimated_bytes();
            return Ok(Self {
                instance: super::STORE_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                memtable: memtable.snapshot(),
                memtable_image: memtable.image(),
                memtable_oldest: memtable.oldest_version(),
                manifest,
                directory,
                schema,
                estimated_bytes,
            });
        }
        Err(StoreError::FormatLimit(
            "table manifest changed during eight reader-open attempts".to_owned(),
        ))
    }

    /// A view of no rows under `schema`, for a table whose store cannot be
    /// opened: it keeps the table's place in a catalog while every read of
    /// it is refused.
    #[must_use]
    pub fn empty(directory: impl Into<PathBuf>, schema: TableSchema) -> Self {
        Self {
            instance: super::STORE_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            memtable: Arc::new(BTreeMap::new()),
            memtable_image: Arc::default(),
            memtable_oldest: None,
            manifest: Arc::new(Manifest::empty(&schema)),
            directory: directory.into(),
            schema,
            estimated_bytes: 0,
        }
    }

    /// Returns the catalog schema pinned with this reader snapshot.
    #[must_use]
    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Captures the encoded manifest and immutable segment paths pinned by
    /// this reader. The caller must retain this snapshot while reading the
    /// returned paths so compaction cannot reclaim them.
    ///
    /// # Errors
    ///
    /// Returns an error if the pinned manifest cannot be encoded.
    pub fn backup_artifacts(&self) -> Result<BackupArtifacts, StoreError> {
        let segments = self
            .manifest
            .segments
            .iter()
            .map(|segment| BackupSegment {
                file_name: segment.file_name.clone(),
                path: self.directory.join(&segment.file_name),
            })
            .collect();
        Ok(BackupArtifacts {
            generation: self.manifest.generation,
            manifest: manifest::encode(&self.manifest)?,
            segments,
        })
    }

    /// Returns the minimum and maximum retained storage keys in this snapshot.
    ///
    /// Bounds can include tombstoned keys; they are intended for safe scan
    /// planning rather than visible-row cardinality.
    #[must_use]
    pub fn key_bounds(&self) -> Option<(PrimaryKey, PrimaryKey)> {
        let segment_minimum = self
            .manifest
            .segments
            .iter()
            .map(|segment| &segment.min_key)
            .min();
        let segment_maximum = self
            .manifest
            .segments
            .iter()
            .map(|segment| &segment.max_key)
            .max();
        let memtable_minimum = self.memtable.keys().next();
        let memtable_maximum = self.memtable.keys().next_back();
        let minimum = segment_minimum
            .into_iter()
            .chain(memtable_minimum)
            .min()?
            .clone();
        let maximum = segment_maximum
            .into_iter()
            .chain(memtable_maximum)
            .max()?
            .clone();
        Some((minimum, maximum))
    }

    /// The highest row version this snapshot holds, across segments and
    /// the memtable, or `None` for an empty table. A repair that must win
    /// last-write-wins against every current row stamps itself above it.
    #[must_use]
    pub fn max_row_version(&self) -> Option<u64> {
        let segments = self
            .manifest
            .segments
            .iter()
            .map(|segment| segment.max_version)
            .max();
        let memtable = self.memtable.values().map(StoredRow::version).max();
        segments.into_iter().chain(memtable).max()
    }

    /// Returns visible rows in primary-key order, excluding tombstones.
    ///
    /// # Errors
    ///
    pub fn scan(&self) -> Result<Vec<StoredRow>, StoreError> {
        if self.memtable.is_empty()
            && let [segment_meta] = self.manifest.segments.as_slice()
            && segment_meta.unique_keys
        {
            return Ok(segment::read(&self.directory, segment_meta, &self.schema)?
                .into_iter()
                .filter(|row| !row.is_deleted())
                .collect());
        }
        let mut latest = BTreeMap::new();
        for segment_meta in &self.manifest.segments {
            for row in segment::read(&self.directory, segment_meta, &self.schema)? {
                apply_latest(&mut latest, row);
            }
        }
        for row in self.memtable.values() {
            apply_latest(&mut latest, row.clone());
        }
        Ok(latest
            .into_values()
            .filter(|row| !row.is_deleted())
            .collect())
    }

    /// Returns one visible primary/unique key using footer range and bloom
    /// pruning before any segment block is decoded.
    ///
    /// # Errors
    ///
    /// Returns a precise segment corruption or filesystem error.
    pub fn get(&self, key: &PrimaryKey) -> Result<Option<StoredRow>, StoreError> {
        let column_ids = self
            .schema
            .columns()
            .iter()
            .map(pintail_types::Column::id)
            .collect::<Vec<_>>();
        let scan = self.scan_projected_range(key, key, &column_ids)?;
        Ok(scan
            .rows
            .into_iter()
            .next()
            .map(|row| StoredRow::new(row.key, row.values, row.version, false)))
    }

    /// Returns visible rows in one inclusive key range, pruning disjoint
    /// segments by footer key bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, corrupt segment, or filesystem
    /// failure.
    pub fn scan_range(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
    ) -> Result<Vec<StoredRow>, StoreError> {
        self.scan_range_versions(start, end, 0, u64::MAX)
    }

    /// Returns latest retained rows in inclusive key and source-version
    /// ranges, pruning segments whose complete version bounds are disjoint.
    ///
    /// This is a retained-version filter, not a historical snapshot API:
    /// memtable insertion and compaction may already have collapsed older
    /// versions of a key.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, corrupt segment, or filesystem
    /// failure.
    pub fn scan_range_versions(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        min_version: u64,
        max_version: u64,
    ) -> Result<Vec<StoredRow>, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        if min_version > max_version {
            return Err(StoreError::FormatLimit(
                "scan version range start follows its end".into(),
            ));
        }
        let mut latest = BTreeMap::new();
        for segment_meta in &self.manifest.segments {
            if segment_meta.max_version < min_version
                || segment_meta.min_version > max_version
                || !segment::overlaps_key_range(segment_meta, start, end)
            {
                continue;
            }
            for row in segment::read(&self.directory, segment_meta, &self.schema)? {
                if row.version() >= min_version
                    && row.version() <= max_version
                    && row.key() >= start
                    && row.key() <= end
                {
                    apply_latest(&mut latest, row);
                }
            }
        }
        for (_, row) in self.memtable.range(start.clone()..=end.clone()) {
            if row.version() >= min_version && row.version() <= max_version {
                apply_latest(&mut latest, row.clone());
            }
        }
        Ok(latest
            .into_values()
            .filter(|row| !row.is_deleted())
            .collect())
    }

    /// Reports, per live segment, whether skipping it on scan-predicate
    /// statistics alone is sound.
    ///
    /// Skipping is safe only for a segment whose key range no other live
    /// segment touches. Where ranges overlap, the skipped segment may hold
    /// the winning version of a key whose older, predicate-matching version
    /// survives in a segment that is still read, which would emit a stale
    /// row. Deciding this per segment rather than for the whole manifest
    /// matters at scale: a large table under continuous replication almost
    /// always has some overlap somewhere, and a single overlapping pair used
    /// to disable pruning for every other segment.
    fn value_prunable_segments(&self) -> Vec<bool> {
        // Dropping a segment whose every live row fails the predicate is
        // safe exactly when nothing it would have SHADOWED is still read.
        // A segment whose overlapping neighbours are all newer shadows
        // nothing: each of its rows is either current - and fails the
        // predicate - or stale behind a newer version that decides for
        // itself, since merge-on-read keeps the highest version. A segment
        // overlapping an OLDER one is the other case: its rows may be the
        // newer versions of rows the older one still holds, and dropping
        // them would bring those versions back.
        //
        // The rule this replaces allowed no overlap at all, which switched
        // value pruning off for a table's whole base as soon as updates put
        // a newer segment over it - on a table taking updates, always - and
        // turned a range filter on a timestamp column into a scan of every
        // segment.
        //
        // The memtable is one more reader under the same rule: it usually
        // holds only newer versions, but a replay can leave an older one
        // there, and dropping the segment that shadows it would surface it.
        // The memtable keeps that bound as it applies rows: taking the
        // minimum here walked every memtable row on every filtered scan.
        let memtable_oldest = self.memtable_oldest;
        let memtable_shadowed = |meta: &crate::segment::SegmentMeta| {
            memtable_oldest.is_some_and(|oldest| oldest <= meta.max_version)
                && self
                    .memtable
                    .range(meta.min_key.clone()..=meta.max_key.clone())
                    .any(|(_, row)| row.version() <= meta.max_version)
        };
        let segments = &self.manifest.segments;
        segments
            .iter()
            .enumerate()
            .map(|(index, meta)| {
                segments.iter().enumerate().all(|(other_index, other)| {
                    other_index == index
                        || other.max_key < meta.min_key
                        || other.min_key > meta.max_key
                        || other.min_version > meta.max_version
                }) && !memtable_shadowed(meta)
            })
            .collect()
    }

    /// Scans an inclusive key range while decoding only requested user
    /// columns after segment and key-block pruning.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate/unknown column ID,
    /// incompatible schema, corrupt block, or filesystem failure.
    pub fn scan_projected_range(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<ProjectedScan, StoreError> {
        self.scan_projected_range_bounded(start, end, column_ids, usize::MAX)
    }

    /// Opens a bounded pull scan, using a direct segment path when possible
    /// and a block-wise last-write-wins merge otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt point-lookup bloom filter.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_stream(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<Option<ProjectedScanStream>, StoreError> {
        self.scan_projected_range_stream_pruned(start, end, column_ids, &[])
    }

    /// [`Self::scan_projected_range_stream`] with scan-predicate value
    /// bounds: segments whose statistics prove every row fails a bound are
    /// skipped without decoding. Value pruning engages only on manifests
    /// whose segments have pairwise-disjoint key ranges and no tombstones —
    /// under overlapping row versions a skipped segment could hide the
    /// winning version of another segment's key.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt point-lookup bloom filter.
    ///
    /// Returns `None` for a small range whose memtable rows need row-wise
    /// visibility resolution: materializing it is cheaper than merging.
    pub fn scan_projected_range_stream_pruned(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        bounds: &[crate::segment::ColumnBounds],
    ) -> Result<Option<ProjectedScanStream>, StoreError> {
        self.projected_range_stream(start, end, column_ids, bounds, true)
    }

    /// [`Self::scan_projected_range_stream_pruned`] that streams a small
    /// range too, for a caller whose budget cannot hold it materialized.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt point-lookup bloom filter.
    ///
    /// # Panics
    ///
    /// Never: only the small-range shortcut declines to stream.
    pub fn scan_projected_range_stream_unbuffered(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        bounds: &[crate::segment::ColumnBounds],
    ) -> Result<ProjectedScanStream, StoreError> {
        Ok(self
            .projected_range_stream(start, end, column_ids, bounds, false)?
            .expect("a scan that may not materialize always streams"))
    }

    #[allow(clippy::too_many_lines)]
    fn projected_range_stream(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        bounds: &[crate::segment::ColumnBounds],
        materialize_small: bool,
    ) -> Result<Option<ProjectedScanStream>, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for id in column_ids {
            if !seen.insert(*id) {
                return Err(StoreError::FormatLimit(format!(
                    "projection repeats column id {id}"
                )));
            }
            if !self
                .schema
                .columns()
                .iter()
                .any(|column| column.id() == *id)
            {
                return Err(StoreError::FormatLimit(format!(
                    "unknown projected column id {id}"
                )));
            }
        }
        let mut segments = Vec::new();
        let mut pruned_segments = 0;
        let prunable = if bounds.is_empty() {
            Vec::new()
        } else {
            self.value_prunable_segments()
        };
        for (index, meta) in self.manifest.segments.iter().enumerate() {
            let overlaps = segment::overlaps_key_range(meta, start, end);
            let point_might_match = start != end
                || segment::might_contain_key(&self.directory, meta, &self.schema, start)?;
            let value_disjoint = prunable.get(index).copied().unwrap_or(false)
                && segment::sma_disjoint(meta, bounds);
            if !overlaps || !point_might_match || value_disjoint {
                pruned_segments += 1;
            } else {
                segments.push(meta.clone());
            }
        }
        segments.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        let candidate_rows = segments
            .iter()
            .map(|segment| segment.row_count)
            .sum::<u64>()
            .saturating_add(u64::try_from(self.memtable.len()).unwrap_or(u64::MAX));

        // Partition [start, end] into contiguous parts by a sweep over the
        // sorted segment key ranges: clusters of overlapping segments merge
        // only within their own bounds; everything between clusters is served
        // directly or from the memtable alone (docs/decisions.md,
        // "Merge-on-read uses granule-level sweep-line classification").
        let memtable_has_rows = |lo: &std::ops::Bound<PrimaryKey>,
                                 hi: &std::ops::Bound<PrimaryKey>| {
            bound_range_is_searchable(lo, hi)
                && self
                    .memtable
                    .range((lo.clone(), hi.clone()))
                    .next()
                    .is_some()
        };
        let mut parts = std::collections::VecDeque::new();
        let mut needs_visibility_resolution = false;
        let mut cursor = std::ops::Bound::Included(start.clone());
        let mut index = 0;
        while index < segments.len() {
            let mut next = index + 1;
            let mut cluster_max = segments[index].max_key.clone();
            let mut all_unique = segments[index].unique_keys;
            while next < segments.len() && segments[next].min_key <= cluster_max {
                if segments[next].max_key > cluster_max {
                    cluster_max = segments[next].max_key.clone();
                }
                all_unique &= segments[next].unique_keys;
                next += 1;
            }
            let part_lo = segments[index].min_key.clone().max(start.clone());
            let part_hi = cluster_max.min(end.clone());
            let gap_hi = std::ops::Bound::Excluded(part_lo.clone());
            if memtable_has_rows(&cursor, &gap_hi) {
                parts.push_back(ScanPart::MemtableOnly {
                    lo: cursor.clone(),
                    hi: gap_hi,
                    rows: None,
                });
                needs_visibility_resolution = true;
            }
            let lo_bound = std::ops::Bound::Included(part_lo.clone());
            let hi_bound = std::ops::Bound::Included(part_hi.clone());
            let single_unique = next - index == 1 && all_unique;
            let direct = single_unique && !memtable_has_rows(&lo_bound, &hi_bound);
            // A whole unique segment the memtable overlaps decodes directly
            // with the superseded rows masked out; it needs no row-wise
            // visibility resolution. The mask takes the memtable's version
            // as the winner without comparing, so every memtable row in the
            // span has to be at least as new as anything in the segment; a
            // stale replay inside the span keeps the merge, which compares.
            // The memtable's own bound answers that for every row at once;
            // only a bound below the segment sends the open to the rows,
            // which it otherwise walked - every changed row of the span,
            // one after another, before the first slice could decode.
            let overlay = single_unique
                && !direct
                && *start <= segments[index].min_key
                && *end >= segments[index].max_key
                && (self
                    .memtable_oldest
                    .is_some_and(|oldest| oldest >= segments[index].max_version)
                    || self
                        .memtable
                        .range((lo_bound.clone(), hi_bound.clone()))
                        .all(|(_, row)| row.version() >= segments[index].max_version));
            // A point lookup whose key the memtable holds newer than
            // anything in the cluster is answered by that row alone: no
            // segment version can win. Merging instead decodes the block the
            // key falls in row by row, whole, which grows with the table.
            let memtable_point = !direct
                && start == end
                && self.memtable.get(start).is_some_and(|row| {
                    segments[index..next]
                        .iter()
                        .all(|meta| row.version() > meta.max_version)
                });
            if memtable_point {
                needs_visibility_resolution = true;
                parts.push_back(ScanPart::MemtableOnly {
                    lo: lo_bound,
                    hi: hi_bound,
                    rows: None,
                });
            } else if overlay {
                parts.push_back(ScanPart::Overlay {
                    segment: segments[index].clone(),
                    rows: None,
                });
            } else if direct {
                // Coalesce runs of direct clusters so parallel prefetch keeps
                // its full width across them.
                if let Some(ScanPart::Direct { segments: previous }) = parts.back_mut() {
                    previous.extend_from_slice(&segments[index..next]);
                } else {
                    parts.push_back(ScanPart::Direct {
                        segments: segments[index..next].to_vec(),
                    });
                }
            } else {
                needs_visibility_resolution = true;
                let lo = std::ops::Bound::Included(part_lo);
                let hi = std::ops::Bound::Included(part_hi.clone());
                let cluster = segments[index..next].to_vec();
                parts.push_back(match self.layer_cluster(&cluster, &lo, &hi, start, end) {
                    Some((bases, rows)) => ScanPart::Layered {
                        segments: cluster,
                        lo,
                        hi,
                        bases,
                        rows,
                    },
                    None => ScanPart::Merge {
                        segments: cluster,
                        lo,
                        hi,
                    },
                });
            }
            cursor = std::ops::Bound::Excluded(part_hi);
            index = next;
        }
        let scan_end = std::ops::Bound::Included(end.clone());
        if memtable_has_rows(&cursor, &scan_end) {
            parts.push_back(ScanPart::MemtableOnly {
                lo: cursor,
                hi: scan_end,
                rows: None,
            });
            needs_visibility_resolution = true;
        }
        if materialize_small && needs_visibility_resolution && candidate_rows < 64 * 1024 {
            return Ok(None);
        }
        let parts = self.refine_merge_parts(start, end, parts);
        Ok(Some(ProjectedScanStream {
            snapshot: self.clone(),
            candidate_segments: segments.len(),
            segments: Vec::new(),
            start: start.clone(),
            end: end.clone(),
            column_ids: column_ids.to_vec(),
            next_segment: 0,
            pruned_segments,
            reported_pruned: false,
            parts,
            memtable_cursor: None,
            overlay_rows: super::layer::LayerRows::single(self.memtable.clone()),
            direct_range: None,
            direct_slice_rows: None,
            slices: std::collections::VecDeque::new(),
            merge: None,
            overlay_key: None,
            overlay: None,
            pending: std::collections::VecDeque::new(),
            index_lookup: None,
            value_bounds: bounds.to_vec(),
            prewhere_sample: super::scan::PrewhereSample::default(),
        }))
    }

    /// Splits a merge cluster into large base segments and the newer
    /// segments over them, when that is sound (see [`ScanPart::Layered`]);
    /// `None` keeps the row-wise merge.
    ///
    /// Sound: every base has unique keys, no two bases overlap, each lies
    /// wholly inside the scanned range, and every row that can share a key
    /// with a base - in a segment whose key span overlaps it, or in the
    /// memtable - is at least as new as anything that base holds, so the
    /// newest such row for a key always wins over the base's. A base is
    /// judged against what overlaps it, not against the whole cluster: a
    /// merge that folded the changes into the bases of one key range leaves
    /// output newer than the bases of every other range, which it never
    /// touches, and both are bases.
    ///
    /// However many rows the newer segments hold, up to what their key
    /// index may hold (see [`layer_index_budget`]): they are read where
    /// they are stored - their keys once per manifest, their values column
    /// by column for the rows a slice needs - so what a scan pays grows
    /// with the rows that changed and nothing is kept as rows. (A bound of
    /// a million newer rows stood here while they were read into a map of
    /// rows: a table of twenty million rows with a tenth of them changed
    /// crossed it and fell to the row-wise merge, tens of seconds a query.)
    #[allow(clippy::type_complexity)]
    fn layer_cluster(
        &self,
        cluster: &[segment::SegmentMeta],
        lo: &std::ops::Bound<PrimaryKey>,
        hi: &std::ops::Bound<PrimaryKey>,
        start: &PrimaryKey,
        end: &PrimaryKey,
    ) -> Option<(Vec<segment::SegmentMeta>, super::layer::LayerRows)> {
        let mut by_age = cluster.iter().collect::<Vec<_>>();
        by_age.sort_by_key(|meta| (meta.min_version, meta.max_version));
        let overlap = |left: &segment::SegmentMeta, right: &segment::SegmentMeta| {
            left.min_key <= right.max_key && left.max_key >= right.min_key
        };
        // Oldest first, so of two segments of one version that overlap the
        // first is the base and the other lies over it.
        let mut bases: Vec<segment::SegmentMeta> = Vec::new();
        let mut newer: Vec<segment::SegmentMeta> = Vec::new();
        for (index, meta) in by_age.iter().enumerate() {
            let under_everything = by_age.iter().enumerate().all(|(other_index, other)| {
                other_index == index
                    || !overlap(meta, other)
                    || other.min_version >= meta.max_version
            });
            if meta.unique_keys
                && *start <= meta.min_key
                && meta.max_key <= *end
                && under_everything
                && bases.iter().all(|base| !overlap(base, meta))
            {
                bases.push((*meta).clone());
            } else {
                newer.push((*meta).clone());
            }
        }
        // A segment left out of the bases must be at least as new as every
        // base it overlaps; one that is not would lose to the base it is
        // read over.
        let sound = newer.iter().all(|meta| {
            bases
                .iter()
                .all(|base| !overlap(base, meta) || meta.min_version >= base.max_version)
        });
        let newer_rows = newer.iter().map(|meta| meta.row_count).sum::<u64>();
        if bases.is_empty() || !sound {
            log_unlayered_cluster(&by_age);
            return None;
        }
        if newer_rows.saturating_mul(super::layer::LAYER_INDEX_ROW_BYTES)
            > super::layer::layer_index_budget()
        {
            pintail_log::log_debug!(
                "store scan merges a cluster row by row: the key index of its {newer_rows} newer rows would pass its budget"
            );
            return None;
        }
        let base_version = bases.iter().map(|base| base.max_version).max().unwrap_or(0);
        // The memtable must not hold a row older than the bases; its own
        // bound says so without a walk of its rows.
        if bound_range_is_searchable(lo, hi)
            && self
                .memtable_oldest
                .is_some_and(|oldest| oldest < base_version)
            && self
                .memtable
                .range((lo.clone(), hi.clone()))
                .any(|(_, row)| row.version() < base_version)
        {
            pintail_log::log_debug!(
                "store scan merges a cluster row by row: a memtable row is older than its bases"
            );
            return None;
        }
        bases.sort_by(|left, right| left.min_key.cmp(&right.min_key));
        Some((
            bases,
            super::layer::LayerRows::layered(newer, self.memtable.clone()),
        ))
    }

    /// Granule-level refinement of merge clusters (docs/decisions.md,
    /// "Merge-on-read uses granule-level sweep-line classification"): a
    /// base+tail cluster whose dominant segment has unique keys splits into
    /// direct row-ranges of the base outside the overlap span plus one merge
    /// bounded to the actual overlap, located through the base's footer
    /// sparse index. Best effort: any obstacle keeps the coarse part.
    pub(super) fn refine_merge_parts(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        parts: std::collections::VecDeque<ScanPart>,
    ) -> std::collections::VecDeque<ScanPart> {
        use std::ops::Bound::{Excluded, Included};
        let mut refined = std::collections::VecDeque::with_capacity(parts.len());
        for part in parts {
            let ScanPart::Merge { segments, lo, hi } = part else {
                refined.push_back(part);
                continue;
            };
            if segments.len() != 2 {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let (base_index, tail_index) = if segments[0].row_count >= segments[1].row_count {
                (0, 1)
            } else {
                (1, 0)
            };
            let base = &segments[base_index];
            let tail = &segments[tail_index];
            let refinable = base.unique_keys
                && base.row_count >= tail.row_count.saturating_mul(4)
                && *start <= base.min_key
                && *end >= base.max_key;
            if !refinable {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            // Overlap span: tail keys plus memtable keys inside the part.
            let mut overlap_lo = tail.min_key.clone();
            let mut overlap_hi = tail.max_key.clone();
            if bound_range_is_searchable(&lo, &hi) {
                if let Some((first, _)) = self.memtable.range((lo.clone(), hi.clone())).next()
                    && *first < overlap_lo
                {
                    overlap_lo = first.clone();
                }
                if let Some((last, _)) = self.memtable.range((lo.clone(), hi.clone())).next_back()
                    && *last > overlap_hi
                {
                    overlap_hi = last.clone();
                }
            }
            let Ok(sparse) = segment::read_sparse_index(&self.directory, base) else {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            };
            if sparse.len() < 2 {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let prefix_granules = sparse.partition_point(|(_, key)| *key < overlap_lo);
            let suffix_start = sparse.partition_point(|(_, key)| *key <= overlap_hi);
            let prefix_rows = prefix_granules
                .checked_sub(1)
                .map_or(0, |granule| sparse[granule].0);
            let suffix_rows = if suffix_start < sparse.len() {
                base.row_count - sparse[suffix_start].0
            } else {
                0
            };
            // Refining only pays when a meaningful share of the base skips
            // the merge entirely.
            if (prefix_rows + suffix_rows).saturating_mul(4) < base.row_count {
                refined.push_back(ScanPart::Merge { segments, lo, hi });
                continue;
            }
            let merge_lo = if prefix_granules >= 1 {
                Included(sparse[prefix_granules - 1].1.clone())
            } else {
                lo.clone()
            };
            let merge_hi = if suffix_start < sparse.len() {
                Excluded(sparse[suffix_start].1.clone())
            } else {
                hi.clone()
            };
            if prefix_rows > 0 {
                refined.push_back(ScanPart::DirectRange {
                    segment: base.clone(),
                    start_row: 0,
                    end_row: prefix_rows,
                });
            }
            refined.push_back(ScanPart::Merge {
                segments: segments.clone(),
                lo: merge_lo,
                hi: merge_hi,
            });
            if suffix_rows > 0 {
                refined.push_back(ScanPart::DirectRange {
                    segment: base.clone(),
                    start_row: sparse[suffix_start].0,
                    end_row: base.row_count,
                });
            }
        }
        refined
    }

    /// Scans a projected range while enforcing a caller-owned memory budget
    /// over candidate, winner, and late-materialized row state.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::scan_projected_range`], plus
    /// [`StoreError::MemoryLimitExceeded`] before retained scan state crosses
    /// `memory_limit`.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_bounded(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        memory_limit: usize,
    ) -> Result<ProjectedScan, StoreError> {
        self.scan_projected_range_bounded_pruned(start, end, column_ids, memory_limit, &[])
    }

    /// [`Self::scan_projected_range_bounded`] with scan-predicate value
    /// bounds; see [`Self::scan_projected_range_stream_pruned`] for the
    /// pruning contract.
    ///
    /// # Errors
    ///
    /// Returns an error for a reversed range, duplicate or unknown columns,
    /// or a corrupt segment.
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_bounded_pruned(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        memory_limit: usize,
        bounds: &[crate::segment::ColumnBounds],
    ) -> Result<ProjectedScan, StoreError> {
        if start > end {
            return Err(StoreError::FormatLimit(
                "scan range start follows its end".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let projection = column_ids
            .iter()
            .map(|id| {
                if !seen.insert(*id) {
                    return Err(StoreError::FormatLimit(format!(
                        "projection repeats column id {id}"
                    )));
                }
                self.schema
                    .columns()
                    .iter()
                    .position(|column| column.id() == *id)
                    .ok_or_else(|| {
                        StoreError::FormatLimit(format!("unknown projected column id {id}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let scan_memory = AtomicUsize::new(0);
        let scan_budget = segment::ScanMemoryBudget::new(&scan_memory, memory_limit);
        let scan_pool = projected_scan_pool()?;
        let prunable = if bounds.is_empty() {
            Vec::new()
        } else {
            self.value_prunable_segments()
        };
        let segment_scans = scan_pool.install(|| {
            self.manifest
                .segments
                .par_iter()
                .enumerate()
                .map(|(segment_index, segment_meta)| {
                    let overlaps = segment::overlaps_key_range(segment_meta, start, end);
                    let point_might_match = start != end
                        || segment::might_contain_key(
                            &self.directory,
                            segment_meta,
                            &self.schema,
                            start,
                        )?;
                    let value_disjoint = prunable.get(segment_index).copied().unwrap_or(false)
                        && segment::sma_disjoint(segment_meta, bounds);
                    if !overlaps || !point_might_match || value_disjoint {
                        return Ok((
                            ScanStats {
                                segments_pruned: 1,
                                ..ScanStats::default()
                            },
                            Vec::new(),
                        ));
                    }
                    let scan = segment::read_row_headers_range(
                        &self.directory,
                        segment_meta,
                        &self.schema,
                        start,
                        end,
                        &scan_budget,
                    )?;
                    let stats = ScanStats {
                        segments_read: 1,
                        blocks_pruned: scan.stats.pruned,
                        blocks_read: scan.stats.read,
                        blocks_decoded: scan.stats.decoded,
                        ..ScanStats::default()
                    };
                    let scan_reserved = scan.reserved_bytes;
                    let candidates = scan
                        .rows
                        .into_iter()
                        .map(|row| {
                            let candidate = ProjectedCandidate {
                                key: row.key,
                                version: row.version,
                                deleted: row.deleted,
                                source: ProjectedSource::Segment {
                                    segment_index,
                                    row_index: row.physical_index,
                                },
                            };
                            scan_budget.reserve(candidate.estimated_bytes())?;
                            Ok(candidate)
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    scan_budget.release(scan_reserved);
                    Ok((stats, candidates))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })?;

        let mut stats = ScanStats::default();
        let mut latest = BTreeMap::new();
        for (segment_stats, candidates) in segment_scans {
            stats.add(segment_stats);
            for candidate in candidates {
                apply_projected_latest(&mut latest, candidate);
            }
        }
        for (_, row) in self.memtable.range(start.clone()..=end.clone()) {
            let candidate_bytes = ProjectedCandidate::estimated_bytes_for_key(row.key());
            scan_budget.reserve(candidate_bytes)?;
            let candidate = ProjectedCandidate {
                key: row.key().clone(),
                version: row.version(),
                deleted: row.is_deleted(),
                source: ProjectedSource::Memtable,
            };
            apply_projected_latest(&mut latest, candidate);
        }

        let mut winners = latest
            .into_values()
            .filter(|row| !row.deleted)
            .map(|candidate| (candidate, None))
            .collect::<Vec<_>>();
        let mut segment_rows = BTreeMap::<usize, Vec<(usize, usize)>>::new();
        for (winner_index, (candidate, values)) in winners.iter_mut().enumerate() {
            match candidate.source {
                ProjectedSource::Segment {
                    segment_index,
                    row_index,
                } => segment_rows
                    .entry(segment_index)
                    .or_default()
                    .push((row_index, winner_index)),
                ProjectedSource::Memtable => {
                    let row = self.memtable.get(&candidate.key).ok_or_else(|| {
                        StoreError::FormatLimit(
                            "winning memtable row disappeared from pinned snapshot".into(),
                        )
                    })?;
                    let projected_bytes = size_of::<Vec<pintail_types::Value>>()
                        .saturating_add(
                            projection
                                .len()
                                .saturating_mul(size_of::<pintail_types::Value>()),
                        )
                        .saturating_add(
                            projection
                                .iter()
                                .map(|index| row.values()[*index].heap_bytes())
                                .fold(0_usize, usize::saturating_add),
                        );
                    scan_budget.reserve(projected_bytes)?;
                    *values = Some(
                        projection
                            .iter()
                            .map(|index| row.values()[*index].clone())
                            .collect(),
                    );
                }
            }
        }
        let segment_fetches = scan_pool.install(|| {
            segment_rows
                .into_iter()
                .collect::<Vec<_>>()
                .into_par_iter()
                .map(|(segment_index, mut selected)| {
                    selected.sort_unstable_by_key(|(row_index, _)| *row_index);
                    let row_indices = selected
                        .iter()
                        .map(|(row_index, _)| *row_index)
                        .collect::<Vec<_>>();
                    let fetch = segment::read_projected_rows(
                        &self.directory,
                        &self.manifest.segments[segment_index],
                        &self.schema,
                        &projection,
                        &row_indices,
                        &scan_budget,
                    )?;
                    let fetched_bytes = fetch
                        .columns
                        .iter()
                        .map(|values| {
                            size_of::<Vec<pintail_types::Value>>()
                                + values.len() * size_of::<pintail_types::Value>()
                                + values
                                    .iter()
                                    .map(pintail_types::Value::heap_bytes)
                                    .sum::<usize>()
                        })
                        .sum();
                    let values = columns_to_rows(fetch.columns, selected.len())?;
                    scan_budget.release(fetch.reserved_bytes);
                    scan_budget.reserve(fetched_bytes)?;
                    Ok((selected, values, fetch.blocks_decoded))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })?;
        for (selected, values, blocks_decoded) in segment_fetches {
            stats.blocks_decoded += blocks_decoded;
            for ((_, winner_index), values) in selected.into_iter().zip(values) {
                winners[winner_index].1 = Some(values);
            }
        }
        let rows = winners
            .into_iter()
            .map(|(row, values)| {
                Ok(ProjectedRow {
                    key: row.key,
                    values: values.ok_or_else(|| {
                        StoreError::FormatLimit("projected winner was not late-materialized".into())
                    })?,
                    version: row.version,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let retained_bytes = size_of::<ProjectedScan>()
            + rows.capacity() * size_of::<ProjectedRow>()
            + rows
                .iter()
                .map(|row| {
                    row.estimated_bytes()
                        .saturating_sub(size_of::<ProjectedRow>())
                })
                .sum::<usize>();
        Ok(ProjectedScan {
            rows,
            stats,
            retained_bytes,
        })
    }
}

/// Says, at debug, why a merge cluster is read row by row: the shape of its
/// segments, oldest first, is what decides whether it can be layered.
fn log_unlayered_cluster(by_age: &[&segment::SegmentMeta]) {
    if !pintail_log::enabled(pintail_log::DEBUG) {
        return;
    }
    let shape = by_age
        .iter()
        .map(|meta| {
            format!(
                "{} rows v{}..={} unique={}",
                meta.row_count, meta.min_version, meta.max_version, meta.unique_keys
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    pintail_log::log_debug!(
        "store scan merges a cluster row by row: no sound, cheap split into bases and newer rows [{shape}]"
    );
}
