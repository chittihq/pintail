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
    pub(super) memtable: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    pub(super) manifest: Arc<Manifest>,
    pub(super) directory: PathBuf,
    pub(super) schema: TableSchema,
    /// Bytes the replayed memtable holds, so whoever keeps this snapshot
    /// resident can charge it to a budget.
    pub(super) estimated_bytes: usize,
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

    /// Bytes of unflushed rows this snapshot keeps resident: the WAL tail
    /// replayed into memory at open, or the writer's live memtable. Segment
    /// data is read from files and not counted.
    #[must_use]
    pub const fn estimated_memtable_bytes(&self) -> usize {
        self.estimated_bytes
    }

    /// The snapshot's data identity when every visible row is
    /// segment-resident: `(table directory, manifest generation)` with an
    /// empty memtable. Two snapshots with the same identity see byte-for-
    /// byte identical data, so exactness-preserving caches (the settled
    /// aggregate memo) key on it; any ingest or flush changes it.
    #[must_use]
    pub fn settled_identity(&self) -> Option<(&std::path::Path, u64)> {
        self.memtable
            .is_empty()
            .then(|| (self.directory.as_path(), self.manifest.generation))
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
                memtable: memtable.snapshot(),
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
        let segments = &self.manifest.segments;
        let mut order = (0..segments.len()).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            segments[*left]
                .min_key
                .cmp(&segments[*right].min_key)
                .then_with(|| segments[*left].max_key.cmp(&segments[*right].max_key))
        });
        let mut prunable = vec![false; segments.len()];
        let mut highest_end: Option<&PrimaryKey> = None;
        for (position, index) in order.iter().copied().enumerate() {
            let meta = &segments[index];
            let touches_earlier = highest_end.is_some_and(|end| end >= &meta.min_key);
            let touches_later = order
                .get(position + 1)
                .is_some_and(|next| segments[*next].min_key <= meta.max_key);
            prunable[index] = !touches_earlier && !touches_later;
            if highest_end.is_none_or(|end| end < &meta.max_key) {
                highest_end = Some(&meta.max_key);
            }
        }
        prunable
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
    #[allow(clippy::too_many_lines)]
    pub fn scan_projected_range_stream_pruned(
        &self,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
        bounds: &[crate::segment::ColumnBounds],
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
                });
                needs_visibility_resolution = true;
            }
            let lo_bound = std::ops::Bound::Included(part_lo.clone());
            let hi_bound = std::ops::Bound::Included(part_hi.clone());
            let direct =
                next - index == 1 && all_unique && !memtable_has_rows(&lo_bound, &hi_bound);
            if direct {
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
                parts.push_back(ScanPart::Merge {
                    segments: segments[index..next].to_vec(),
                    lo: std::ops::Bound::Included(part_lo),
                    hi: std::ops::Bound::Included(part_hi.clone()),
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
            });
            needs_visibility_resolution = true;
        }
        if needs_visibility_resolution && candidate_rows < 64 * 1024 {
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
            direct_range: None,
            direct_slice_rows: None,
            merge: None,
        }))
    }

    /// Granule-level refinement of merge clusters (docs/decisions.md,
    /// "Merge-on-read uses granule-level sweep-line classification"): a
    /// base+tail cluster whose dominant segment has unique keys splits into
    /// direct row-ranges of the base outside the overlap span plus one merge
    /// bounded to the actual overlap, located through the base's footer
    /// sparse index. Best effort: any obstacle keeps the coarse part.
    fn refine_merge_parts(
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
