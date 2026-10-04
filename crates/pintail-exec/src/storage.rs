use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use pintail_catalog::{DatabaseId, TableId};

use crate::collation::Collation;
use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_store::{
    DecodedColumn, ProjectedColumnChunk, ProjectedRow, ProjectedScanStream, ScanStats, StoreError,
    TableSnapshot,
};
use pintail_types::{KeyPart, PrimaryKey, Value};
use rayon::prelude::*;

use crate::execution::MemoryScope;
use crate::{
    BatchStream, ColumnVector, DEFAULT_BATCH_ROWS, ExecError, RecordBatch, Scan, ScanProvider,
    array::{StrColumn, ValidityMask},
    batch::{LazyText, TypedValues, parse_date_days, parse_datetime_micros, parse_decimal_scaled},
};

/// Storage scan provider backed by reader-pinned table snapshots.
pub struct SnapshotScanProvider<'snapshot> {
    /// The plan's collation, used by prewhere: pushing a text predicate into
    /// the scan must decide equality the same way the operators above it do,
    /// or a row filtered here would have survived there.
    collation: Collation,
    snapshots: BTreeMap<(DatabaseId, TableId), &'snapshot TableSnapshot>,
    unique_visibility: BTreeMap<(DatabaseId, TableId), Vec<Vec<u32>>>,
    /// Tables that cannot answer: their copy from the source has not
    /// completed, or, with a reason, their store could not be opened. They
    /// stay in the catalog so metadata queries see them, but opening a scan
    /// fails with [`ExecError::TableNotReady`] or
    /// [`ExecError::TableUnreadable`] rather than answering from a partial
    /// or absent store.
    not_ready: BTreeMap<(DatabaseId, TableId), (String, Option<String>)>,
    stats: Arc<Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>>,
}

/// One table's snapshot, owned, for an operator that opens scans of that
/// table while the query runs. Scans open exactly as the provider that
/// handed it out would open them.
struct OwnedTableProvider {
    key: (DatabaseId, TableId),
    snapshot: TableSnapshot,
    collation: Collation,
    unique_visibility: Option<Vec<Vec<u32>>>,
    stats: Arc<Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>>,
}

impl ScanProvider for OwnedTableProvider {
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        let provider = SnapshotScanProvider {
            collation: self.collation,
            snapshots: BTreeMap::from([(self.key, &self.snapshot)]),
            unique_visibility: self
                .unique_visibility
                .iter()
                .map(|keys| (self.key, keys.clone()))
                .collect(),
            not_ready: BTreeMap::new(),
            stats: Arc::clone(&self.stats),
        };
        provider.open_scan(scan, memory_limit)
    }
}

/// Actual storage work accumulated for one table during query execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysicalScanStats {
    /// Manifest segments rejected before block inspection.
    pub segments_pruned: usize,
    /// Manifest segments whose block metadata was inspected.
    pub segments_read: usize,
    /// Logical key blocks rejected by typed zone maps.
    pub blocks_pruned: usize,
    /// Logical key blocks selected for row-header decoding.
    pub blocks_read: usize,
    /// Encoded system and projected-value blocks decoded.
    pub blocks_decoded: usize,
    /// Bytes the decoded blocks decompressed to.
    pub bytes_decompressed: u64,
    /// Column values the reads delivered, predicate columns included.
    pub values_decoded: u64,
    /// Row blocks skipped because their stored minimum and maximum rule
    /// out every row for a range or equality predicate (each also counts in
    /// `blocks_pruned` once per projected column).
    pub blocks_value_skipped: usize,
    /// Slices of segments read through the side index, decoding only the
    /// rows a lookup named.
    pub index_slices: usize,
}

impl PhysicalScanStats {
    /// Returns the number of manifest segments considered.
    #[must_use]
    pub const fn segments_total(self) -> usize {
        self.segments_pruned + self.segments_read
    }

    /// Returns the number of logical primary-key blocks considered.
    #[must_use]
    pub const fn blocks_total(self) -> usize {
        self.blocks_pruned + self.blocks_read
    }

    fn add(&mut self, other: Self) {
        self.segments_pruned += other.segments_pruned;
        self.segments_read += other.segments_read;
        self.blocks_pruned += other.blocks_pruned;
        self.blocks_read += other.blocks_read;
        self.blocks_decoded += other.blocks_decoded;
        self.bytes_decompressed = self
            .bytes_decompressed
            .saturating_add(other.bytes_decompressed);
        self.values_decoded = self.values_decoded.saturating_add(other.values_decoded);
        self.blocks_value_skipped += other.blocks_value_skipped;
        self.index_slices += other.index_slices;
    }
}

impl From<ScanStats> for PhysicalScanStats {
    fn from(stats: ScanStats) -> Self {
        Self {
            segments_pruned: stats.segments_pruned(),
            segments_read: stats.segments_read(),
            blocks_pruned: stats.blocks_pruned(),
            blocks_read: stats.blocks_read(),
            blocks_decoded: stats.blocks_decoded(),
            bytes_decompressed: stats.bytes_decompressed(),
            values_decoded: stats.values_decoded(),
            blocks_value_skipped: stats.blocks_value_skipped(),
            index_slices: stats.index_slices(),
        }
    }
}

impl<'snapshot> SnapshotScanProvider<'snapshot> {
    pub(crate) fn scan_admission_cost(&self, scan: &Scan) -> Option<(crate::AdmissionCost, bool)> {
        let key = (scan.table.database_id, scan.table.table_id);
        if self.not_ready.contains_key(&key) || self.unique_visibility.contains_key(&key) {
            return None;
        }
        let snapshot = self.snapshots.get(&key)?;
        if snapshot.schema().version() != scan.table.schema_version {
            return None;
        }
        let Some((start, end)) = storage_key_range(scan, snapshot) else {
            return Some((crate::AdmissionCost::default(), true));
        };
        let rows = snapshot.physical_range_row_upper_bound(&start, &end);
        let width = scan
            .projected_column_ids
            .iter()
            .try_fold(0_u64, |width, id| {
                let column = snapshot
                    .schema()
                    .columns()
                    .iter()
                    .find(|column| column.id() == *id)?;
                let bytes = match column.data_type().storage_type() {
                    pintail_types::DataType::Boolean => 1,
                    pintail_types::DataType::Int64
                    | pintail_types::DataType::UInt64
                    | pintail_types::DataType::Float64 => 8,
                    _ => return None,
                };
                Some(width.saturating_add(bytes + 1))
            });
        // Even COUNT(*) has to read visibility and key information.
        let bytes = width.map_or(u64::MAX, |width| {
            rows.saturating_mul(width.saturating_add(16))
        });
        Some((crate::AdmissionCost { rows, bytes }, start == end))
    }

    /// Sets the collation the plan resolved, so a predicate pushed into the
    /// scan decides equality the way the operators above it do.
    #[must_use]
    pub fn with_collation(mut self, collation: Collation) -> Self {
        self.collation = collation;
        self
    }

    /// Indexes pinned snapshots by stable catalog identity.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::DuplicateSnapshot`] when the same database and
    /// table identity occurs more than once.
    pub fn new(
        snapshots: impl IntoIterator<Item = (DatabaseId, TableId, &'snapshot TableSnapshot)>,
    ) -> Result<Self, ExecError> {
        let mut indexed = BTreeMap::new();
        for (database_id, table_id, snapshot) in snapshots {
            if indexed.insert((database_id, table_id), snapshot).is_some() {
                return Err(ExecError::DuplicateSnapshot {
                    database_id,
                    table_id,
                });
            }
        }
        Ok(Self {
            collation: Collation::default(),
            snapshots: indexed,
            unique_visibility: BTreeMap::new(),
            not_ready: BTreeMap::new(),
            stats: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Marks one table as still being copied: every scan of it fails with
    /// [`ExecError::TableNotReady`] naming `table` until the provider is
    /// rebuilt without the mark.
    pub fn mark_not_ready(&mut self, database_id: DatabaseId, table_id: TableId, table: String) {
        self.not_ready
            .insert((database_id, table_id), (table, None));
    }

    /// Marks one table whose store could not be opened: every scan of it
    /// fails with [`ExecError::TableUnreadable`] naming `table` and
    /// `reason`.
    pub fn mark_unreadable(
        &mut self,
        database_id: DatabaseId,
        table_id: TableId,
        table: String,
        reason: String,
    ) {
        self.not_ready
            .insert((database_id, table_id), (table, Some(reason)));
    }

    /// Opts one table into higher-version visibility for transient secondary
    /// UNIQUE collisions.
    ///
    /// Each inner vector is one non-empty unique constraint expressed as
    /// stable column IDs.
    ///
    /// # Errors
    ///
    /// Returns an error when the table snapshot or a configured column is
    /// absent.
    pub fn enable_unique_visibility_policy(
        &mut self,
        database_id: DatabaseId,
        table_id: TableId,
        unique_keys: Vec<Vec<u32>>,
    ) -> Result<(), ExecError> {
        let key = (database_id, table_id);
        let snapshot = self.snapshots.get(&key).ok_or(ExecError::MissingSnapshot {
            database_id,
            table_id,
        })?;
        if unique_keys.iter().any(Vec::is_empty) {
            return Err(ExecError::InvalidPhysicalPlan(
                "unique visibility constraints cannot be empty",
            ));
        }
        for column_id in unique_keys.iter().flatten() {
            if !snapshot
                .schema()
                .columns()
                .iter()
                .any(|column| column.id() == *column_id)
            {
                return Err(ExecError::InvalidPhysicalPlan(
                    "unique visibility references an unknown stable column ID",
                ));
            }
        }
        self.unique_visibility.insert(key, unique_keys);
        Ok(())
    }

    /// Returns physical work accumulated for one stable table identity.
    #[must_use]
    pub fn scan_stats(
        &self,
        database_id: DatabaseId,
        table_id: TableId,
    ) -> Option<PhysicalScanStats> {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(database_id, table_id))
            .copied()
    }

    fn record_stats(&self, key: (DatabaseId, TableId), stats: PhysicalScanStats) {
        let mut all = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        all.entry(key)
            .and_modify(|current| current.add(stats))
            .or_insert(stats);
    }
}

impl ScanProvider for SnapshotScanProvider<'_> {
    fn table_provider(
        &self,
        database_id: DatabaseId,
        table_id: TableId,
    ) -> Option<Box<dyn ScanProvider + Send + Sync>> {
        let key = (database_id, table_id);
        if self.not_ready.contains_key(&key) {
            return None;
        }
        Some(Box::new(OwnedTableProvider {
            key,
            snapshot: (*self.snapshots.get(&key)?).clone(),
            collation: self.collation,
            unique_visibility: self.unique_visibility.get(&key).cloned(),
            stats: Arc::clone(&self.stats),
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        let key = (scan.table.database_id, scan.table.table_id);
        if let Some((table, reason)) = self.not_ready.get(&key) {
            return Err(match reason {
                None => ExecError::TableNotReady {
                    table: table.clone(),
                },
                Some(reason) => ExecError::TableUnreadable {
                    detail: format!("table {table} cannot be read: {reason}"),
                },
            });
        }
        let snapshot = self.snapshots.get(&key).ok_or(ExecError::MissingSnapshot {
            database_id: key.0,
            table_id: key.1,
        })?;
        if snapshot.schema().version() != scan.table.schema_version {
            return Err(ExecError::SnapshotSchemaChanged {
                database_id: key.0,
                table_id: key.1,
                expected: scan.table.schema_version,
                actual: snapshot.schema().version(),
            });
        }

        let output_positions = scan
            .projected_column_ids
            .iter()
            .map(|id| {
                snapshot
                    .schema()
                    .columns()
                    .iter()
                    .position(|column| column.id() == *id)
                    .ok_or(ExecError::InvalidPhysicalPlan(
                        "snapshot schema is missing a projected stable column ID",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let types = output_positions
            .iter()
            .map(|position| snapshot.schema().columns()[*position].data_type())
            .collect::<Vec<_>>();
        // Parallel to `types`: an ENUM column's declared labels, so the
        // materialized value can carry the declaration index MySQL orders
        // by. Shared rather than cloned per batch - the list is per column
        // and every batch of that column wants the same one.
        let enum_labels = output_positions
            .iter()
            .map(|position| {
                snapshot.schema().columns()[*position]
                    .enum_labels()
                    .map(|labels| Arc::new(labels.to_vec()))
            })
            .collect::<Vec<_>>();
        // Parallel to `enum_labels`: a SET column's declared members, so a
        // materialized value carries the member bitmask MySQL sorts by.
        let set_members = output_positions
            .iter()
            .map(|position| {
                snapshot.schema().columns()[*position]
                    .set_members()
                    .map(|members| Arc::new(members.to_vec()))
            })
            .collect::<Vec<_>>();
        let stream_overhead = std::mem::size_of::<SnapshotStream>()
            .saturating_add(types.capacity() * std::mem::size_of::<pintail_types::DataType>());
        if stream_overhead > memory_limit {
            return Err(ExecError::MemoryLimitExceeded {
                used: 0,
                requested: stream_overhead,
                limit: memory_limit,
                scope: MemoryScope::Query,
            });
        }

        let Some((start, end)) = storage_key_range(scan, snapshot) else {
            self.record_stats(key, PhysicalScanStats::default());
            return Ok(Box::new(SnapshotStream {
                stats: Arc::clone(&self.stats),
                stats_key: key,
                rows: VecDeque::new(),
                columns: Vec::new(),
                column_rows: 0,
                ready: VecDeque::new(),
                last_prefiltered: false,
                column_decode: BTreeMap::new(),
                value_skipped_blocks: 0,
                prefetched: VecDeque::new(),
                stream: None,
                prewhere: None,
                adopt_filter: None,
                key_position: None,
                started: true,
                types,
                enum_labels,
                set_members,
                retained_bytes: stream_overhead,
                remaining: None,
                filtered: FilteredLimit::default(),
                budget_round: 0,
                settled: None,
                sma: None,
                grouped: None,
                delta: None,
                fold_order: 0,
            }));
        };
        let unique_keys = self.unique_visibility.get(&key);
        // Bounded residual so the eager projection clone stays cheap for
        // scans that never fold; unique-key visibility changes merge-on-read
        // semantics, so those tables decline. Both the streamed and the
        // materialized scan paths carry the same fold input.
        #[allow(clippy::items_after_statements)]
        const SMA_RESIDUAL_ROW_CAP: usize = 16_384;
        // Every residual row is a memtable row, so a larger memtable
        // declines before the store walks it.
        let sma = (scan.predicates.is_empty()
            && scan.limit.is_none()
            && unique_keys.is_none()
            && snapshot.memtable_len() <= SMA_RESIDUAL_ROW_CAP)
            .then(|| snapshot.sma_fold_state())
            .flatten()
            .map(|(smas, rows)| crate::execution::SmaFoldInput {
                column_ids: scan.projected_column_ids.clone(),
                segments: smas.into_iter().cloned().collect(),
                rows: rows
                    .iter()
                    .map(|row| {
                        output_positions
                            .iter()
                            .map(|position| row.values()[*position].clone())
                            .collect()
                    })
                    .collect(),
            });
        // The grouped fold wants the same quiet scan the SMA fold does -
        // no predicates, no limit, no unique-key visibility - but tolerates
        // a memtable that supersedes segment rows, because it re-reads the
        // spans those rows fall in rather than trusting a statistic.
        let grouped = (scan.predicates.is_empty() && scan.limit.is_none() && unique_keys.is_none())
            // Bounded like the SMA residual and the delta beside it: the
            // rows outside every span are cloned into the projection here,
            // and without a bound a large memtable pays that clone on every
            // scan open, including the scans that never aggregate.
            .then(|| snapshot.grouped_fold_spans(SMA_RESIDUAL_ROW_CAP))
            .flatten()
            .map(|(spans, outside)| crate::execution::GroupedFoldInput {
                snapshot: (*snapshot).clone(),
                directory: snapshot.directory().to_path_buf(),
                spans,
                column_ids: scan.projected_column_ids.clone(),
                key_column_ids: scan.table.key_column_ids.clone(),
                types: types.clone(),
                enum_labels: enum_labels.clone(),
                set_members: set_members.clone(),
                outside: outside
                    .iter()
                    .map(|row| {
                        output_positions
                            .iter()
                            .map(|position| row.values()[*position].clone())
                            .collect()
                    })
                    .collect(),
            });
        // Bounded so merging never costs more than it saves. Built here,
        // beside the SMA and grouped inputs, because BOTH scan paths below
        // return it: the materialized path used to hardcode `None`, so the
        // delta-maintained memo was dead for every scan that took it.
        #[allow(clippy::items_after_statements)]
        const DELTA_ROW_CAP: usize = 4096;
        let delta = (scan.predicates.is_empty()
            && scan.limit.is_none()
            && unique_keys.is_none()
            && snapshot.memtable_len() <= DELTA_ROW_CAP)
            .then(|| snapshot.insert_only_delta())
            .flatten()
            .map(
                |(directory, generation, rows)| crate::execution::InsertOnlyDelta {
                    directory: directory.to_path_buf(),
                    generation,
                    scan: scan_signature(snapshot.instance(), scan),
                    types: types.clone(),
                    rows: rows
                        .iter()
                        .map(|row| {
                            output_positions
                                .iter()
                                .map(|position| row.values()[*position].clone())
                                .collect()
                        })
                        .collect(),
                },
            );

        let mut physical_column_ids = scan.projected_column_ids.clone();
        if let Some(unique_keys) = unique_keys {
            for column_id in unique_keys.iter().flatten() {
                if !physical_column_ids.contains(column_id) {
                    physical_column_ids.push(*column_id);
                }
            }
        }
        let value_bounds = sma_column_bounds(&scan.predicates);
        // Every range streams unless unique-key visibility needs all of its
        // rows at once. A range under 64K rows with memtable rows used to be
        // materialized whole instead: every segment row's key decoded into
        // an ordered map to pick the newest version, then every value built
        // as a row and turned back into columns. That took about 12 ms for
        // 20,000 rows, where the stream decodes the segment by column, masks
        // the rows the memtable supersedes and appends the memtable's own.
        let streamed = if unique_keys.is_none() {
            Some(
                snapshot
                    .scan_projected_range_stream_unbuffered(
                        &start,
                        &end,
                        &physical_column_ids,
                        &value_bounds,
                    )
                    .map_err(|error| ExecError::Source(error.to_string()))?,
            )
        } else {
            None
        };
        let mut projected = None;
        if streamed.is_none() {
            match snapshot.scan_projected_range_bounded_pruned(
                &start,
                &end,
                &physical_column_ids,
                memory_limit - stream_overhead,
                &value_bounds,
            ) {
                Ok(rows) => projected = Some(rows),
                Err(StoreError::MemoryLimitExceeded {
                    used, requested, ..
                }) => {
                    return Err(ExecError::MemoryLimitExceeded {
                        used: used.saturating_add(stream_overhead),
                        requested,
                        limit: memory_limit,
                        scope: MemoryScope::Query,
                    });
                }
                Err(other) => return Err(ExecError::Source(other.to_string())),
            }
        }
        if let Some(mut stream) = streamed {
            self.record_stats(
                key,
                PhysicalScanStats {
                    segments_pruned: stream.pruned_segment_count(),
                    segments_read: stream.segment_count(),
                    ..PhysicalScanStats::default()
                },
            );
            let key_position = match scan.table.key_column_ids.as_slice() {
                [key_id] => scan.projected_column_ids.iter().position(|id| id == key_id),
                _ => None,
            };
            // A key of integer, text or binary columns lets a segment the
            // memtable overlaps be decoded directly with the superseded rows
            // masked by those columns, instead of merged row by row.
            stream.enable_memtable_overlay(&scan.table.key_column_ids);
            let text_filters = text_value_filters(scan, snapshot, self.collation);
            let prewhere =
                build_prewhere_spec(scan, snapshot, self.collation, !text_filters.is_empty());
            if let Some(spec) = &prewhere {
                stream.set_text_filters(text_filters);
                stream.set_filter_only(spec.filter_only);
            }
            pintail_store::side_index_note(|| {
                format!(
                    "scan table={} predicates={} filter_first={} lookup={:?}",
                    scan.table.table_name,
                    scan.predicates.len(),
                    prewhere.is_some(),
                    predicate_index_lookup(scan, snapshot, self.collation)
                        .map(|lookup| lookup.column_id)
                )
            });
            if prewhere.is_some()
                && pintail_store::side_index_enabled()
                && let Some(lookup) = predicate_index_lookup(scan, snapshot, self.collation)
            {
                stream.set_index_lookup(lookup);
            }
            let limit = scan
                .limit
                .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
            // The last rows in key order: the stream hands out the end of
            // the range first, and the sort above orders what arrives.
            let from_end = scan.from_end && limit.is_some();
            if from_end {
                stream.read_from_end();
            }
            return Ok(Box::new(SnapshotStream {
                stats: Arc::clone(&self.stats),
                stats_key: key,
                rows: VecDeque::new(),
                columns: Vec::new(),
                column_rows: 0,
                ready: VecDeque::new(),
                last_prefiltered: false,
                column_decode: BTreeMap::new(),
                value_skipped_blocks: 0,
                prefetched: VecDeque::new(),
                stream: Some(stream),
                prewhere,
                adopt_filter: build_adopt_filter(scan, self.collation),
                key_position,
                started: false,
                types,
                enum_labels,
                set_members,
                retained_bytes: stream_overhead,
                remaining: (scan.predicates.is_empty() && !from_end)
                    .then_some(limit)
                    .flatten(),
                filtered: FilteredLimit {
                    remaining: (!scan.predicates.is_empty() || from_end)
                        .then_some(limit)
                        .flatten(),
                    // With no predicates there is no Filter to disagree
                    // with the count.
                    armed: from_end && scan.predicates.is_empty(),
                    counts_unjudged: scan.predicates.is_empty(),
                    from_end,
                },
                budget_round: 0,
                // A filtered aggregate over a settled snapshot is just as
                // much a pure function of the data version as a bare one —
                // the predicates and limit simply join the memo key (issue
                // #6: Q2/Q5/Q7 were excluded for no sound reason).
                settled: snapshot
                    .settled_identity()
                    .map(|(directory, instance, generation)| {
                        (
                            directory.to_path_buf(),
                            generation,
                            scan_signature(instance, scan),
                        )
                    }),
                delta,
                sma,
                grouped,
                fold_order: 0,
            }));
        }
        let Some(projected) = projected else {
            return Err(ExecError::InvalidPhysicalPlan(
                "scan opened neither a stream nor a row set",
            ));
        };
        self.record_stats(key, projected.stats().into());
        let mut rows = projected.into_rows();
        if let Some(unique_keys) = unique_keys {
            apply_unique_visibility(&mut rows, &physical_column_ids, unique_keys);
            let positions = scan
                .projected_column_ids
                .iter()
                .map(|column_id| {
                    physical_column_ids
                        .iter()
                        .position(|candidate| candidate == column_id)
                        .expect("output column is included in the physical projection")
                })
                .collect::<Vec<_>>();
            rows = rows
                .into_iter()
                .map(|row| row.project_values(&positions))
                .collect();
        }
        if scan.predicates.is_empty()
            && let Some(limit) = scan.limit
        {
            // The rows are in key order. A limit from the end keeps the
            // last ones, as the stream would hand them out first.
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            if scan.from_end {
                rows.drain(..rows.len().saturating_sub(limit));
            } else {
                rows.truncate(limit);
            }
            rows.shrink_to_fit();
        }
        let rows = rows
            .into_iter()
            .map(ProjectedRow::into_values)
            .collect::<Vec<_>>();
        let retained_bytes =
            projected_values_retained_bytes(rows.capacity(), &rows).saturating_add(stream_overhead);
        Ok(Box::new(SnapshotStream {
            stats: Arc::clone(&self.stats),
            stats_key: key,
            rows: rows.into(),
            columns: Vec::new(),
            column_rows: 0,
            ready: VecDeque::new(),
            last_prefiltered: false,
            column_decode: BTreeMap::new(),
            value_skipped_blocks: 0,
            prefetched: VecDeque::new(),
            stream: None,
            prewhere: None,
            adopt_filter: None,
            key_position: None,
            started: true,
            types,
            enum_labels,
            set_members,
            retained_bytes,
            remaining: None,
            filtered: FilteredLimit::default(),
            budget_round: 0,
            settled: None,
            delta,
            sma,
            grouped,
            fold_order: 0,
        }))
    }
}

fn apply_unique_visibility(
    rows: &mut Vec<ProjectedRow>,
    physical_column_ids: &[u32],
    unique_keys: &[Vec<u32>],
) {
    let mut hidden = BTreeSet::new();
    for unique_key in unique_keys {
        let positions = unique_key
            .iter()
            .map(|column_id| {
                physical_column_ids
                    .iter()
                    .position(|candidate| candidate == column_id)
                    .expect("unique column is included in the physical projection")
            })
            .collect::<Vec<_>>();
        let mut winners = BTreeMap::<Vec<Value>, (u64, PrimaryKey)>::new();
        for row in rows.iter() {
            let values = positions
                .iter()
                .map(|position| normalize_unique_value(&row.values()[*position]))
                .collect::<Vec<_>>();
            if values.iter().any(|value| value == &Value::Null) {
                continue;
            }
            let candidate = (row.version(), row.key().clone());
            match winners.get_mut(&values) {
                Some(winner) if candidate > *winner => {
                    hidden.insert(winner.1.clone());
                    *winner = candidate;
                }
                Some(_) => {
                    hidden.insert(row.key().clone());
                }
                None => {
                    winners.insert(values, candidate);
                }
            }
        }
    }
    rows.retain(|row| !hidden.contains(row.key()));
}

fn normalize_unique_value(value: &Value) -> Value {
    match value {
        Value::Utf8(value) => Value::Utf8(value.to_lowercase()),
        value => value.clone(),
    }
}

fn storage_key_range(scan: &Scan, snapshot: &TableSnapshot) -> Option<(PrimaryKey, PrimaryKey)> {
    let (minimum, maximum) = snapshot.key_bounds()?;
    let ([minimum_part], [maximum_part]) = (minimum.parts(), maximum.parts()) else {
        return Some((minimum, maximum));
    };
    let [key_column_id] = scan.table.key_column_ids.as_slice() else {
        return Some((minimum, maximum));
    };
    let Some(key_column) = snapshot
        .schema()
        .columns()
        .iter()
        .find(|column| column.id() == *key_column_id)
    else {
        return Some((minimum, maximum));
    };
    if !matches!(
        key_column.data_type().storage_type(),
        pintail_types::DataType::Int64 | pintail_types::DataType::UInt64
    ) {
        return Some((minimum, maximum));
    }
    if !key_part_matches_column(minimum_part, key_column.data_type())
        || !key_part_matches_column(maximum_part, key_column.data_type())
    {
        return Some((minimum, maximum));
    }

    let mut lower = minimum_part.clone();
    let mut upper = maximum_part.clone();
    for predicate in &scan.predicates {
        apply_key_predicate(predicate, scan, key_column.id(), &mut lower, &mut upper);
    }
    if lower > upper {
        return None;
    }
    Some((
        PrimaryKey::new(vec![lower]).expect("one-part lower storage key"),
        PrimaryKey::new(vec![upper]).expect("one-part upper storage key"),
    ))
}

fn apply_key_predicate(
    predicate: &BoundExpr,
    scan: &Scan,
    key_column_id: u32,
    lower: &mut KeyPart,
    upper: &mut KeyPart,
) {
    match &predicate.kind {
        BoundExprKind::Binary { op, left, right } => {
            if let Some(value) = key_literal(right, lower)
                && is_scan_key(left, scan, key_column_id)
            {
                apply_comparison(*op, value, lower, upper);
            } else if let Some(value) = key_literal(left, lower)
                && is_scan_key(right, scan, key_column_id)
                && let Some(op) = reverse_comparison(*op)
            {
                apply_comparison(op, value, lower, upper);
            }
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::Between { negated: false },
            args,
        } if args.len() == 3 && is_scan_key(&args[0], scan, key_column_id) => {
            if let Some(value) = key_literal(&args[1], lower) {
                tighten_lower(lower, value);
            }
            if let Some(value) = key_literal(&args[2], upper) {
                tighten_upper(upper, value);
            }
        }
        // `key IN (constants)` lies between its least and greatest key; a
        // NULL in the list matches no row. A constant the key cannot hold
        // exactly compares by conversion, so it leaves the range alone, as
        // does a list of NULLs, which the filter above answers.
        BoundExprKind::Scalar {
            function: ScalarFunction::InList { negated: false },
            args,
        } if args.len() > 1 && is_scan_key(&args[0], scan, key_column_id) => {
            let mut listed = Vec::with_capacity(args.len() - 1);
            for argument in &args[1..] {
                match (&argument.kind, key_literal(argument, lower)) {
                    (_, Some(value)) => listed.push(value),
                    (BoundExprKind::Literal(Value::Null), None) => {}
                    _ => return,
                }
            }
            if let (Some(least), Some(greatest)) = (listed.iter().min(), listed.iter().max()) {
                tighten_lower(lower, least.clone());
                tighten_upper(upper, greatest.clone());
            }
        }
        _ => {}
    }
}

fn is_scan_key(expression: &BoundExpr, scan: &Scan, key_column_id: u32) -> bool {
    matches!(
        &expression.kind,
        BoundExprKind::Column(column)
            if column.database_id == scan.table.database_id
                && column.table_id == scan.table.table_id
                && column.column_id == key_column_id
    )
}

fn key_literal(expression: &BoundExpr, key_type: &KeyPart) -> Option<KeyPart> {
    let BoundExprKind::Literal(value) = &expression.kind else {
        return None;
    };
    match (value, key_type) {
        (Value::Int64(value), KeyPart::Int64(_)) => Some(KeyPart::Int64(*value)),
        (Value::UInt64(value), KeyPart::UInt64(_)) => Some(KeyPart::UInt64(*value)),
        (Value::Int64(value), KeyPart::UInt64(_)) => {
            u64::try_from(*value).ok().map(KeyPart::UInt64)
        }
        (Value::UInt64(value), KeyPart::Int64(_)) => i64::try_from(*value).ok().map(KeyPart::Int64),
        (
            Value::Null
            | Value::Boolean(_)
            | Value::Int64(_)
            | Value::UInt64(_)
            | Value::Float64(_)
            | Value::Utf8(_)
            | Value::Binary(_)
            | Value::Enum { .. }
            | Value::DecimalAverage(_),
            _,
        ) => None,
    }
}

fn key_part_matches_column(key: &KeyPart, data_type: pintail_types::DataType) -> bool {
    matches!(
        (key, data_type.storage_type()),
        (KeyPart::Int64(_), pintail_types::DataType::Int64)
            | (KeyPart::UInt64(_), pintail_types::DataType::UInt64)
            | (KeyPart::Utf8(_), pintail_types::DataType::Utf8)
            | (KeyPart::Binary(_), pintail_types::DataType::Binary)
    )
}

fn apply_comparison(op: BinaryOp, value: KeyPart, lower: &mut KeyPart, upper: &mut KeyPart) {
    match op {
        BinaryOp::Equal => {
            tighten_lower(lower, value.clone());
            tighten_upper(upper, value);
        }
        BinaryOp::GreaterOrEqual => tighten_lower(lower, value),
        BinaryOp::Greater => {
            if let Some(value) = successor(value) {
                tighten_lower(lower, value);
            }
        }
        BinaryOp::LessOrEqual => tighten_upper(upper, value),
        BinaryOp::Less => {
            if let Some(value) = predecessor(&value) {
                tighten_upper(upper, value);
            }
        }
        BinaryOp::NotEqual
        | BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::IntegerDivide
        | BinaryOp::Modulo
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Xor => {}
    }
}

const fn reverse_comparison(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Equal => Some(BinaryOp::Equal),
        BinaryOp::Less => Some(BinaryOp::Greater),
        BinaryOp::LessOrEqual => Some(BinaryOp::GreaterOrEqual),
        BinaryOp::Greater => Some(BinaryOp::Less),
        BinaryOp::GreaterOrEqual => Some(BinaryOp::LessOrEqual),
        _ => None,
    }
}

fn tighten_lower(lower: &mut KeyPart, value: KeyPart) {
    if value > *lower {
        *lower = value;
    }
}

fn tighten_upper(upper: &mut KeyPart, value: KeyPart) {
    if value < *upper {
        *upper = value;
    }
}

fn successor(value: KeyPart) -> Option<KeyPart> {
    match value {
        KeyPart::Int64(value) => value.checked_add(1).map(KeyPart::Int64),
        KeyPart::UInt64(value) => value.checked_add(1).map(KeyPart::UInt64),
        KeyPart::Utf8(mut value) => {
            value.push('\0');
            Some(KeyPart::Utf8(value))
        }
        KeyPart::Binary(mut value) => {
            value.push(0);
            Some(KeyPart::Binary(value))
        }
    }
}

fn predecessor(value: &KeyPart) -> Option<KeyPart> {
    match value {
        KeyPart::Int64(value) => value.checked_sub(1).map(KeyPart::Int64),
        KeyPart::UInt64(value) => value.checked_sub(1).map(KeyPart::UInt64),
        KeyPart::Utf8(_) | KeyPart::Binary(_) => None,
    }
}

/// Compiled scan predicates for filter-first chunk decoding: evaluated over
/// the predicate columns alone, before the rest of the projection decodes.
struct PrewhereSpec {
    predicate_ids: Vec<u32>,
    predicates: Vec<crate::expression::CompiledExpr>,
    data_types: Vec<pintail_types::DataType>,
    /// Parallel to `data_types`. Without it a predicate would compare an
    /// ENUM as text while the projection compares it by index - the same
    /// column answering two different questions in one query.
    enum_labels: Vec<Option<Arc<Vec<String>>>>,
    /// Parallel to `enum_labels`: a SET column's declared members, whose
    /// positions are the bits of the mask `MySQL` sorts a SET by.
    set_members: Vec<Option<Arc<Vec<String>>>>,
    /// The collation the predicates were compiled under. A Filter above the
    /// scan trusts a prefiltered chunk only when it compares text the same
    /// way.
    collation: Collation,
    /// Whether `predicates` are all of the scan's predicates. A spec that
    /// left some out (a wide column's test, deferred until the narrow ones
    /// have chosen rows) never marks a chunk as passing them all.
    complete: bool,
    /// An integer span a join proved every useful row's column lies in,
    /// applied beside the predicates. It is not a scan predicate: rows it
    /// drops could match nothing above, so it never makes a chunk exact.
    runtime_range: Option<RuntimeRange>,
    /// A join's probe keys, tested against the integer predicate column at
    /// this index: rows it rejects are not decoded. The join's own key
    /// filter still tests every row, so this only narrows the decode.
    membership: Option<(usize, crate::execution::IntegerMembership)>,
    /// Whether the scan projects nothing beyond these predicate columns and
    /// the spec exists only for the rows the store can leave unread: those
    /// the side index does not name, and blocks holding no value a text
    /// predicate accepts. Where neither applies the store reads the columns
    /// through unselected, as it did before there was a spec.
    filter_only: bool,
}

/// A join key span pushed into a scan on a column that is not the table's
/// key: rows outside it skip decoding every other projected column.
#[derive(Clone, Copy)]
struct RuntimeRange {
    /// Position of the constrained column among the spec's predicate
    /// columns.
    index: usize,
    lower: i128,
    upper: i128,
}

/// A scan's predicates compiled over its whole projection, exactly as the
/// Filters above it compile them, and answered by the packed kernels alone.
struct AdoptFilter {
    predicates: Vec<crate::expression::CompiledExpr>,
    collation: Collation,
}

impl AdoptFilter {
    /// Narrows `batch` to the rows every predicate keeps and reports whether
    /// it did. A predicate the kernels do not answer (or that fails) leaves
    /// the batch untouched and unmarked, and the Filters above decide its
    /// rows - and raise its error - as they always have.
    fn apply(&self, batch: &mut RecordBatch) -> bool {
        let mut combined: Option<crate::SelectionMask> = None;
        for predicate in &self.predicates {
            let Ok(Some(mask)) = predicate.evaluate_filter_mask(batch) else {
                return false;
            };
            combined = match combined {
                None => Some(mask),
                Some(mut existing) => {
                    if existing.intersect(&mask).is_err() {
                        return false;
                    }
                    Some(existing)
                }
            };
        }
        combined.is_some_and(|mask| batch.selection_mut().intersect(&mask).is_ok())
    }
}

/// The adopt-time filter for a scan with predicates, compiled over the
/// projection the way the plan compiles the Filters it stacks on the scan.
fn build_adopt_filter(scan: &Scan, collation: Collation) -> Option<AdoptFilter> {
    if scan.predicates.is_empty() {
        return None;
    }
    let columns = scan
        .projected_column_ids
        .iter()
        .map(|id| {
            scan.table
                .columns
                .iter()
                .find(|column| column.column_id == *id)
                .cloned()
        })
        .collect::<Option<Vec<_>>>()?;
    let predicates = scan
        .predicates
        .iter()
        .map(|predicate| crate::expression::CompiledExpr::compile(predicate, &columns, collation))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(AdoptFilter {
        predicates: crate::expression::pair_column_ranges(predicates),
        collation,
    })
}

/// Column types whose values are wide enough that decoding them for every
/// row costs more than the narrow predicates beside them: a test on one of
/// these waits until the narrow tests have chosen rows.
fn is_wide_prewhere_type(data_type: pintail_types::DataType) -> bool {
    matches!(
        data_type,
        pintail_types::DataType::Json | pintail_types::DataType::Binary
    )
}

struct SnapshotStream {
    /// Shared with the provider that opened this stream. A scan's block
    /// counters are only known once chunks are pulled, long after `open_scan`
    /// returned, so the stream folds each chunk's stats in as it goes.
    stats: Arc<Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>>,
    stats_key: (DatabaseId, TableId),
    rows: VecDeque<Vec<Value>>,
    /// Batches adopted ahead of time in the worker pool (no-LIMIT scans).
    ready: VecDeque<(RecordBatch, bool)>,
    /// Whether the batch last returned came from a chunk whose every row
    /// the prewhere predicates already accepted.
    last_prefiltered: bool,
    /// Per column id, the bytes this scan's blocks decompressed to and the
    /// values its reads delivered, for the profile.
    column_decode: BTreeMap<u32, (u64, u64)>,
    /// Row blocks this scan left undecoded because their stored extremes
    /// rule out the scan's range predicates.
    value_skipped_blocks: usize,
    columns: Vec<DecodedColumn>,
    column_rows: usize,
    prewhere: Option<PrewhereSpec>,
    /// The scan's predicates over the full projection, tested on the
    /// worker that adopts each chunk. A batch every one of them answered
    /// leaves with its selection already narrowed and marked prefiltered,
    /// so the Filters above pass it untested instead of testing it on the
    /// single thread that pulls batches.
    adopt_filter: Option<AdoptFilter>,
    /// Projected position of the table's single primary-key column, when
    /// projected — the only column a probe-side restriction can prune on.
    key_position: Option<usize>,
    /// Whether any batch was pulled; restrictions are ignored afterwards.
    started: bool,
    prefetched: VecDeque<ProjectedColumnChunk>,
    stream: Option<ProjectedScanStream>,
    types: Vec<pintail_types::DataType>,
    /// Parallel to `types`: declared ENUM labels per projected column.
    enum_labels: Vec<Option<Arc<Vec<String>>>>,
    /// Parallel to `enum_labels`: a SET column's declared members, whose
    /// positions are the bits of the mask `MySQL` sorts a SET by.
    set_members: Vec<Option<Arc<Vec<String>>>>,
    retained_bytes: usize,
    remaining: Option<usize>,
    /// The limit a scan counts against the rows that pass its predicates.
    filtered: FilteredLimit,
    /// Bounded fetches made so far: each asks for more rows than the last,
    /// so a scan whose rows mostly fail its filter is not read a few rows
    /// at a time.
    budget_round: u32,
    /// `(table directory, manifest generation, scan signature)` over a
    /// settled snapshot (empty memtable) — the settled aggregate memo key.
    /// The signature covers projection, predicates and limit, so different
    /// scans of the same generation never share an entry.
    settled: Option<(std::path::PathBuf, u64, String)>,
    /// Insert-only memtable rows above the segment identity, when the memo
    /// can merge them (bare scan, bounded delta).
    delta: Option<crate::execution::InsertOnlyDelta>,
    /// Per-segment SMAs + residual memtable rows when the bare-aggregate
    /// fold is provably exact (WS3-B); `None` otherwise.
    sma: Option<crate::execution::SmaFoldInput>,
    grouped: Option<crate::execution::GroupedFoldInput>,
    /// Slices the fused rounds have been through, which is where the next
    /// round's batches stand in the scan's order.
    fold_order: u64,
}

/// A limit over a scan that filters its rows, or reads them end first:
/// counted against the batches the stream adopts whole, not against the
/// rows of one decoded chunk.
#[derive(Clone, Copy, Debug, Default)]
struct FilteredLimit {
    /// Rows passing every one of the scan's predicates that the limit
    /// still wants. Only batches this stream judged itself count, and
    /// only once the plan says its Filters trust that judgement
    /// ([`BatchStream::stop_after_filtered_rows`]).
    remaining: Option<usize>,
    armed: bool,
    /// Whether a batch nothing judged counts too: the scan has no
    /// predicates, so every row passes them.
    counts_unjudged: bool,
    /// Whether the rows wanted are the last in key order, read end first.
    from_end: bool,
}

/// Why a judged chunk decodes whole.
enum Unrestricted {
    /// The predicates kept nearly every row.
    Dense,
    /// Nothing answered the predicates over the packed columns.
    Unanswered,
}

/// A fused round's batch order packs the slice's place in the scan above
/// the chunk's place in its slice and the batch's in its chunk.
const ORDER_SLICE_SHIFT: u32 = 24;
const ORDER_PIECE_SHIFT: u32 = 12;
const ORDER_PART_MAX: usize = (1 << ORDER_PIECE_SHIFT) - 1;

/// Direct-segment slices a prefetch round asks for per scan thread.
const SLICES_PER_SCAN_THREAD: usize = 4;
/// Below this much remaining budget a prefetch round takes one slice.
const TIGHT_CEILING_BYTES: usize = 64 * 1024 * 1024;
/// The fewest rows a bounded fetch asks of a scan that filters them.
const FILTERED_FETCH_ROWS: usize = 1024;
/// A bounded scan that would ask for more rows than this in one fetch
/// reads on unbounded, a round of slices at a time.
const BOUNDED_FETCH_CEILING_ROWS: usize = 262_144;

/// Rows a bounded scan asks for in its next fetch: what it still wants on
/// the first, four times more on each fetch after it, and from the start
/// several times what it wants when a filter stands between the rows read
/// and the rows kept. `None` once that passes the ceiling.
fn bounded_fetch_rows(wanted: usize, filtered: bool, round: u32) -> Option<usize> {
    let base = if filtered {
        wanted.saturating_mul(4).max(FILTERED_FETCH_ROWS)
    } else {
        wanted.max(1)
    };
    let rows = base.checked_shl(round.saturating_mul(2).min(usize::BITS - 1))?;
    (rows >> round.saturating_mul(2).min(usize::BITS - 1) == base
        && rows <= BOUNDED_FETCH_CEILING_ROWS)
        .then_some(rows)
}

impl SnapshotStream {
    /// Narrows a not-yet-started streamed scan to the rows whose integer
    /// column at `position` lies in `[min, max]`, the span of the keys a
    /// join can match. The column is decoded first, alone or beside the
    /// scan's own predicate columns, and the rest of the projection only
    /// for the rows inside the span. A column that is not the table's key
    /// cannot bound the key range, so without this every row of the table
    /// was decoded and then handed to the join to be thrown away.
    fn restrict_value_range(&mut self, position: usize, min: &Value, max: &Value) {
        let (Some(lower), Some(upper)) = (integer_bound(min), integer_bound(max)) else {
            return;
        };
        if lower > upper {
            return;
        }
        let Some((spec, index)) = self.filter_first_column(position) else {
            return;
        };
        let column_id = spec.predicate_ids[index];
        let (lower, upper) = match spec.runtime_range {
            Some(existing) if existing.index == index => {
                (lower.max(existing.lower), upper.min(existing.upper))
            }
            _ => (lower, upper),
        };
        spec.runtime_range = Some(RuntimeRange {
            index,
            lower,
            upper,
        });
        if pintail_store::side_index_enabled()
            && let Some(stream) = &mut self.stream
            && stream.index_lookup().is_none()
        {
            stream.set_index_lookup(pintail_store::IndexLookup {
                column_id,
                key: pintail_store::IndexKey::Integer,
                probe: pintail_store::IndexProbe::Span(lower, upper),
            });
        }
    }

    /// Hands the side index the exact integer values a join's build side
    /// can use in the projected column at `position`. Replaces a span or a
    /// predicate lookup with more values: fewer values name fewer rows.
    fn restrict_value_set(&mut self, position: usize, values: &[i128]) {
        if self.started || !pintail_store::side_index_enabled() {
            return;
        }
        let Some(data_type) = self.types.get(position).copied() else {
            return;
        };
        if !is_integer_type(data_type) || self.prewhere.is_none() {
            return;
        }
        let Some(stream) = &mut self.stream else {
            return;
        };
        let Some(column_id) = stream.column_ids().get(position).copied() else {
            return;
        };
        let mut values = values.to_vec();
        values.sort_unstable();
        values.dedup();
        let better = match stream.index_lookup().map(|lookup| &lookup.probe) {
            Some(pintail_store::IndexProbe::Values(existing)) => existing.len() > values.len(),
            _ => true,
        };
        if better {
            stream.set_index_lookup(pintail_store::IndexLookup {
                column_id,
                key: pintail_store::IndexKey::Integer,
                probe: pintail_store::IndexProbe::Values(values),
            });
        }
    }

    /// Hands the side index the text values a join can use in the projected
    /// text column at `position`, as hashes of their collation weight bytes.
    /// A scan with no predicates of its own gets a spec that decodes this
    /// column first, for the rows the index names alone.
    fn restrict_text_set(&mut self, position: usize, collation: Collation, weights: &[&[u8]]) {
        pintail_store::side_index_note(|| {
            format!(
                "text keys offered position={position} keys={} started={} type={:?} filter_first={}",
                weights.len(),
                self.started,
                self.types.get(position),
                self.prewhere.is_some()
            )
        });
        if self.started || !pintail_store::side_index_enabled() {
            return;
        }
        if self.types.get(position).copied() != Some(pintail_types::DataType::Utf8)
            || self.enum_labels.get(position).is_some_and(Option::is_some)
            || self.set_members.get(position).is_some_and(Option::is_some)
        {
            return;
        }
        let Some(keyer) = text_keyer(collation) else {
            return;
        };
        let Some(stream) = &mut self.stream else {
            return;
        };
        let Some(column_id) = stream.column_ids().get(position).copied() else {
            return;
        };
        let mut values = weights
            .iter()
            .map(|weights| i128::from(pintail_store::TextKeyer::value_of_key(weights)))
            .collect::<Vec<_>>();
        values.sort_unstable();
        values.dedup();
        if self.prewhere.is_none() {
            if stream.column_ids().len() < 2 {
                // Nothing to decode second.
                return;
            }
            self.prewhere = Some(PrewhereSpec {
                predicate_ids: vec![column_id],
                predicates: Vec::new(),
                data_types: vec![pintail_types::DataType::Utf8],
                enum_labels: vec![None],
                set_members: vec![None],
                collation,
                complete: true,
                runtime_range: None,
                membership: None,
                filter_only: false,
            });
        }
        // The keys choose rows before the scan's other work, whatever it
        // projects.
        if let Some(spec) = &mut self.prewhere {
            spec.filter_only = false;
        }
        stream.set_filter_only(false);
        // Beside a lookup the scan already has, not instead of it: which
        // names fewer rows - a label test or these keys - is the segment's
        // to say.
        stream.add_index_lookup(pintail_store::IndexLookup {
            column_id,
            key: pintail_store::IndexKey::Text(keyer),
            probe: pintail_store::IndexProbe::Values(values),
        });
    }

    /// Makes the integer column at `position` one the filter-first decode
    /// reads, creating a spec with no predicates of its own when the scan
    /// had none, and answers the spec and the column's index in it.
    fn filter_first_column(&mut self, position: usize) -> Option<(&mut PrewhereSpec, usize)> {
        let stream = self.stream.as_ref()?;
        let data_type = self.types.get(position).copied()?;
        let column_id = stream.column_ids().get(position).copied()?;
        if !is_integer_type(data_type) {
            return None;
        }
        // What a join restricts is tested by the selector, so the selector
        // has to run on every slice from here on.
        if let Some(stream) = self.stream.as_mut() {
            stream.set_filter_only(false);
        }
        let spec = self.prewhere.get_or_insert_with(|| PrewhereSpec {
            predicate_ids: Vec::new(),
            predicates: Vec::new(),
            data_types: Vec::new(),
            enum_labels: Vec::new(),
            set_members: Vec::new(),
            collation: Collation::default(),
            complete: true,
            runtime_range: None,
            membership: None,
            filter_only: false,
        });
        spec.filter_only = false;
        let index = if let Some(index) = spec.predicate_ids.iter().position(|id| *id == column_id) {
            index
        } else {
            // Appended, not sorted in: the compiled predicates address
            // their columns by position in this list.
            spec.predicate_ids.push(column_id);
            spec.data_types.push(data_type);
            spec.enum_labels.push(None);
            spec.set_members.push(None);
            spec.predicate_ids.len() - 1
        };
        Some((spec, index))
    }

    /// Folds one chunk's counters into the provider's per-table totals,
    /// and its per-column decode cost into this scan's own tally.
    fn accumulate(&mut self, chunk: &ProjectedColumnChunk) {
        self.accumulate_stats(chunk.stats(), chunk.column_decode());
    }

    fn accumulate_stats(&mut self, stats: ScanStats, decode: &[pintail_store::ColumnDecode]) {
        self.value_skipped_blocks += stats.blocks_value_skipped();
        for column in decode {
            let tally = self.column_decode.entry(column.column_id).or_default();
            tally.0 = tally.0.saturating_add(column.bytes_decompressed);
            tally.1 = tally.1.saturating_add(column.values_decoded);
        }
        let mut all = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        all.entry(self.stats_key)
            .or_default()
            .add(PhysicalScanStats {
                blocks_read: stats.blocks_read(),
                blocks_pruned: stats.blocks_pruned(),
                blocks_decoded: stats.blocks_decoded(),
                bytes_decompressed: stats.bytes_decompressed(),
                values_decoded: stats.values_decoded(),
                blocks_value_skipped: stats.blocks_value_skipped(),
                index_slices: stats.index_slices(),
                ..PhysicalScanStats::default()
            });
    }

    /// Rows to plan for, given what the query has left to spend. Quoting a
    /// fixed `DEFAULT_BATCH_ROWS` makes a tight ceiling fail outright — for a
    /// five-column table that estimate alone is ~263 KB — when the honest
    /// answer is to hand back a smaller batch. At least one row is always
    /// planned so a budget below a single row fails on the real reservation
    /// with a truthful number rather than silently yielding nothing.
    fn planned_batch_rows(&self, budget: usize) -> usize {
        planned_scan_rows(&self.types, budget)
    }
}

/// Share of what a query has left that a scan's next batch may plan for;
/// the store's decode chunk takes up to half of the budget less the batch.
///
/// Planning used to divide the whole budget by a one-row batch's bound,
/// which counted each column's vector header once per row. That header is
/// a fixed cost, and how big it happened to be was all that kept the batch
/// to a tenth of the budget: when it shrank, the batch claimed a third and
/// starved the decode chunk, and a scan under a tight ceiling failed on a
/// chunk it could have taken. The split is now stated, not inherited.
const SCAN_BATCH_SHARE: usize = 8;

/// Rows the scan plans its next batch at: as many as fit its share of
/// `budget`, with each column's fixed cost counted once, never fewer than
/// one and never more than [`DEFAULT_BATCH_ROWS`].
fn planned_scan_rows(types: &[pintail_types::DataType], budget: usize) -> usize {
    let fixed = batch_memory_upper_bound(types, 0);
    let per_row = batch_memory_upper_bound(types, 1)
        .saturating_sub(fixed)
        .max(1);
    let share = (budget / SCAN_BATCH_SHARE).saturating_sub(fixed);
    DEFAULT_BATCH_ROWS.min((share / per_row).max(1))
}

/// What a scan is, for the keys the settled aggregate memo and its
/// delta-maintained extension are held under.
///
/// One constructor because the two are compared against each other: the
/// delta looks the settled entry up to extend it, so a signature spelled
/// differently on either side is not a slower path but a dead one. The
/// store instance leads it. A directory can be reclaimed and its successor
/// walks the same generations, so the path and generation alone would let
/// one table be answered from another's rows.
fn scan_signature(instance: u64, scan: &Scan) -> String {
    format!(
        "i{instance}|{:?}|{:?}|{:?}|{}",
        scan.projected_column_ids, scan.predicates, scan.limit, scan.from_end
    )
}

impl BatchStream for SnapshotStream {
    fn prefilter_collation(&self) -> Option<Collation> {
        self.prewhere
            .as_ref()
            .map(|spec| spec.collation)
            .or_else(|| self.adopt_filter.as_ref().map(|filter| filter.collation))
    }

    fn last_batch_prefiltered(&self) -> bool {
        self.last_prefiltered
    }

    fn stop_after_filtered_rows(&mut self) {
        self.filtered.armed =
            self.filtered.armed || (!self.started && self.filtered.remaining.is_some());
    }

    fn decode_note(&self) -> Option<String> {
        use std::fmt::Write as _;
        if self.column_decode.is_empty() && self.value_skipped_blocks == 0 {
            return None;
        }
        let (bytes, values) = self
            .column_decode
            .values()
            .fold((0_u64, 0_u64), |(bytes, values), (b, v)| {
                (bytes.saturating_add(*b), values.saturating_add(*v))
            });
        let mut note = format!("decompressed={bytes}B values={values}");
        for (id, (bytes, values)) in &self.column_decode {
            let _ = write!(note, " c{id}={bytes}B/{values}");
        }
        if self.value_skipped_blocks > 0 {
            let _ = write!(note, " value_skipped_blocks={}", self.value_skipped_blocks);
        }
        Some(note)
    }

    fn settled_identity(&self) -> Option<(std::path::PathBuf, u64, String)> {
        self.settled.clone()
    }

    fn sma_fold_input(&self) -> Option<crate::execution::SmaFoldInput> {
        self.sma.clone()
    }

    fn grouped_fold_input(&self) -> Option<crate::execution::GroupedFoldInput> {
        self.grouped.clone()
    }

    fn insert_only_delta(&self) -> Option<crate::execution::InsertOnlyDelta> {
        self.delta.clone()
    }

    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        self.started = true;
        while self.rows.is_empty()
            && self.column_rows == 0
            && self.ready.is_empty()
            && self.remaining != Some(0)
            && let Some(stream) = &mut self.stream
        {
            // Reserve headroom for the batch this pull will actually build,
            // not for a full-size one: subtracting the maximum leaves a zero
            // chunk budget under a tight ceiling, which the store then refuses.
            let planned_rows = planned_scan_rows(&self.types, available_memory);
            let batch_overhead = batch_memory_upper_bound(&self.types, planned_rows);
            if self.prefetched.is_empty() {
                // A scan under a limit reads what the limit still wants,
                // not a round of slices: ten rows of a table used to cost
                // every block of its first slice in every column.
                let wanted = self
                    .remaining
                    .or(self.filtered.remaining.filter(|_| self.filtered.armed));
                let fetch_rows = wanted.and_then(|wanted| {
                    bounded_fetch_rows(
                        wanted,
                        self.prewhere.is_some() || self.adopt_filter.is_some(),
                        self.budget_round,
                    )
                });
                if fetch_rows.is_some() {
                    self.budget_round = self.budget_round.saturating_add(1);
                }
                stream.set_row_budget(fetch_rows);
                // One segment per scan-pool thread. These chunks decode inside
                // that pool, so a width below its thread count leaves threads
                // idle for the whole scan - the fixed eight this replaced used
                // half of a sixteen-thread pool, and four of them whenever a
                // query projected more than two columns. Widening it is worth
                // 5-13% per query across the analytical benchmark.
                //
                // Memory does not need a narrower width to stay safe: the
                // store splits the chunk budget across the batch it decodes
                // and halves the width and retries when a share proves too
                // small, so a wide projection under a tight ceiling still
                // lands rather than failing.
                // Several slices per scan thread: a slice is a bounded work
                // unit, and one per thread made a prefetch round short
                // enough that the scan and the consumer took turns idling.
                // The budget below still bounds what a round decodes.
                // Under a tight ceiling the scan takes one slice at a time,
                // as it took one segment before slicing: the operators
                // above need the room more than the scan needs width.
                let prefetch_width =
                    if fetch_rows.is_some() || available_memory < TIGHT_CEILING_BYTES {
                        1
                    } else {
                        pintail_store::projected_scan_width().saturating_mul(SLICES_PER_SCAN_THREAD)
                    };
                // Half of what is left, not all of it: the prefetch is one
                // scan's working set and the operators above it reserve
                // against the same ceiling. A scan that took the whole
                // remainder handed the aggregate a budget already spent,
                // which is what made a query under a tight ceiling fail on
                // a few hundred bytes with an empty group map.
                let chunk_budget = (available_memory / 2).saturating_sub(batch_overhead);
                let (chunks, abandon_prewhere) = if let Some(spec) = &self.prewhere {
                    let judged = AtomicUsize::new(0);
                    let dense = AtomicUsize::new(0);
                    let unanswered = AtomicUsize::new(0);
                    let exact_ranges =
                        spec.predicate_ids.len() > 1 && spec.predicate_ids == stream.column_ids();
                    let select = |columns: &[DecodedColumn], row_count: usize| {
                        judged.fetch_add(1, Ordering::Relaxed);
                        Ok(
                            match prewhere_ranges(spec, columns, row_count, exact_ranges)? {
                                Ok(ranges) => Some(ranges),
                                Err(Unrestricted::Dense) => {
                                    dense.fetch_add(1, Ordering::Relaxed);
                                    None
                                }
                                Err(Unrestricted::Unanswered) => {
                                    unanswered.fetch_add(1, Ordering::Relaxed);
                                    None
                                }
                            },
                        )
                    };
                    let chunks = stream
                        .next_column_chunks_filtered(
                            prefetch_width,
                            chunk_budget,
                            &spec.predicate_ids,
                            &select,
                        )
                        .map_err(|error| ExecError::Source(error.to_string()))?;
                    let judged = judged.load(Ordering::Relaxed);
                    let dense = dense.load(Ordering::Relaxed);
                    let unanswered = unanswered.load(Ordering::Relaxed);
                    if judged > 0 {
                        // Every judged chunk kept nearly all of its rows
                        // and no block was ruled out by its extremes: this
                        // stretch of the table decodes whole. The share a
                        // filter keeps belongs to the stretch, not to the
                        // table, so one chunk in a few is still judged and
                        // the selection returns where rows start failing.
                        // A side-index lookup names its rows before any
                        // chunk is judged and is never sampled.
                        let skipped = chunks
                            .iter()
                            .any(|chunk| chunk.stats().blocks_value_skipped() > 0);
                        stream.sample_prewhere(
                            dense + unanswered == judged
                                && !skipped
                                && stream.index_lookup().is_none(),
                        );
                    }
                    // A lookup of exact values chose the rows the selector
                    // saw: no answer about those says nothing about the
                    // rows the lookup left out.
                    (
                        chunks,
                        judged > 0 && unanswered == judged && !stream.has_value_index_lookup(),
                    )
                } else {
                    (
                        stream
                            .next_column_chunks(prefetch_width, chunk_budget)
                            .map_err(|error| ExecError::Source(error.to_string()))?,
                        false,
                    )
                };
                if abandon_prewhere {
                    // No judged chunk's predicates could be answered over
                    // the packed columns at all. That is a property of the
                    // predicates, not of the rows, so later segments would
                    // only repeat it. A round that kept nearly every row
                    // says nothing of the next one and does not end here:
                    // a table whose early segments all pass a filter and
                    // whose recent ones mostly fail it used to decode the
                    // recent ones whole for the sake of the early ones.
                    self.prewhere = None;
                }
                if chunks.is_empty() {
                    self.stream = None;
                    break;
                }
                for chunk in chunks {
                    self.retained_bytes = self.retained_bytes.saturating_add(
                        chunk
                            .retained_bytes()
                            .saturating_sub(std::mem::size_of_val(&chunk)),
                    );
                    self.accumulate(&chunk);
                    self.prefetched.push_back(chunk);
                }
            }
            if self.remaining.is_none() {
                // No LIMIT: adopt every prefetched chunk into ready batches
                // on the worker pool — slicing and typed adoption (decimal
                // and temporal parsing included) run in parallel per chunk
                // instead of serially on this thread.
                let chunks = std::mem::take(&mut self.prefetched);
                let mut released = 0_usize;
                let adopted = chunks
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_par_iter()
                    .map(|chunk| {
                        let prefiltered = chunk.prefiltered();
                        adopt_chunk(chunk, &self.types, &self.enum_labels, &self.set_members).map(
                            |(batches, bytes)| {
                                let batches = batches
                                    .into_iter()
                                    .map(|mut batch| {
                                        let passed = prefiltered
                                            || self
                                                .adopt_filter
                                                .as_ref()
                                                .is_some_and(|filter| filter.apply(&mut batch));
                                        (batch, passed)
                                    })
                                    .collect::<Vec<_>>();
                                (batches, bytes)
                            },
                        )
                    })
                    .collect::<Result<Vec<_>, ExecError>>()?;
                for (batches, chunk_bytes) in adopted {
                    released = released.saturating_add(chunk_bytes);
                    for (batch, prefiltered) in batches {
                        if (prefiltered || self.filtered.counts_unjudged)
                            && let Some(wanted) = &mut self.filtered.remaining
                        {
                            *wanted = wanted.saturating_sub(batch.visible_row_count());
                        }
                        self.retained_bytes =
                            self.retained_bytes.saturating_add(batch.estimated_bytes());
                        self.ready.push_back((batch, prefiltered));
                    }
                }
                self.retained_bytes = self.retained_bytes.saturating_sub(released);
                // Read from the end, only whole parts make the rows in
                // hand every row from some key on.
                if self.filtered.armed
                    && self.filtered.remaining == Some(0)
                    && (!self.filtered.from_end
                        || self
                            .stream
                            .as_ref()
                            .is_none_or(ProjectedScanStream::at_unit_boundary))
                {
                    // The batches in hand hold every row the limit above
                    // can take: each counted row passed all of the scan's
                    // predicates, and the Filters pass those untested.
                    self.stream = None;
                    break;
                }
                if self.ready.is_empty() {
                    // Every chunk of the round was empty (a slice whose rows
                    // the memtable all superseded, a predicate nothing met):
                    // the stream has more parts, so fetch the next round
                    // rather than read the silence as the end.
                    continue;
                }
                break;
            }
            let chunk = self
                .prefetched
                .pop_front()
                .expect("non-empty prefetch batch");
            self.column_rows = chunk.row_count();
            self.columns = chunk.into_decoded_columns();
        }
        if let Some((batch, prefiltered)) = self.ready.pop_front() {
            self.retained_bytes = self.retained_bytes.saturating_sub(batch.estimated_bytes());
            self.last_prefiltered = prefiltered;
            return Ok(Some(batch));
        }
        self.last_prefiltered = false;
        if self.rows.is_empty() && self.column_rows == 0 {
            return Ok(None);
        }
        let buffered_rows = if self.column_rows > 0 {
            self.column_rows
        } else {
            self.rows.len()
        };
        // Cap by what the caller can actually afford, matching the number
        // quoted by next_batch_memory_upper_bound — otherwise the estimate
        // promises a small batch and the pull delivers a large one.
        let row_count = buffered_rows
            .min(self.planned_batch_rows(available_memory))
            .min(self.remaining.unwrap_or(usize::MAX));
        let columns = if self.column_rows > 0 {
            if self.columns.len() != self.types.len() {
                return Err(ExecError::InvalidBatch(
                    "stored column count differs from its snapshot schema",
                ));
            }
            let before: usize = self.columns.iter().map(DecodedColumn::retained_bytes).sum();
            let taken = self
                .columns
                .iter_mut()
                .map(|column| column.take_prefix(row_count))
                .collect::<Vec<_>>();
            let after: usize = self.columns.iter().map(DecodedColumn::retained_bytes).sum();
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(before.saturating_sub(after));
            self.column_rows = self.column_rows.saturating_sub(row_count);
            if taken.iter().any(|column| column.len() != row_count) {
                return Err(ExecError::InvalidBatch(
                    "stored column ended before its segment rows",
                ));
            }
            self.types
                .iter()
                .copied()
                .zip(taken)
                .zip(self.enum_labels.iter().zip(self.set_members.iter()))
                .map(|((data_type, column), (labels, members))| {
                    column_vector_from_decoded(data_type, column, labels.as_ref(), members.as_ref())
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut output = self
                .types
                .iter()
                .map(|_| Vec::with_capacity(row_count))
                .collect::<Vec<_>>();
            for _ in 0..row_count {
                let values = self.rows.pop_front().expect("row count bounded above");
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_sub(projected_value_payload_bytes(&values));
                if values.len() != self.types.len() {
                    return Err(ExecError::InvalidBatch(
                        "stored row is shorter than its snapshot schema",
                    ));
                }
                for (position, value) in values.into_iter().enumerate() {
                    output[position].push(value);
                }
            }
            self.types
                .iter()
                .copied()
                .zip(output)
                .zip(self.enum_labels.iter().zip(self.set_members.iter()))
                .map(|((data_type, values), (labels, members))| {
                    // Row-shaped values come from the memtable (CDC rows not
                    // yet flushed), where an ENUM is stored as its bare
                    // label. Reattach the declaration index exactly as the
                    // columnar decode path does, or a scan straddling
                    // settled segments and fresh rows mixes ordinal-ordered
                    // and text-ordered values in one sort (#256).
                    column_vector_from_values(data_type, values, labels.as_ref(), members.as_ref())
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        if let Some(remaining) = &mut self.remaining {
            *remaining = remaining.saturating_sub(row_count);
            if *remaining == 0 {
                self.retained_bytes = self.retained_bytes.saturating_sub(
                    self.rows
                        .iter()
                        .map(|values| projected_value_payload_bytes(values))
                        .sum(),
                );
                self.rows.clear();
                self.retained_bytes =
                    self.retained_bytes
                        .saturating_sub(projected_columns_retained_bytes(
                            self.columns.capacity(),
                            &self.columns,
                        ));
                self.columns.clear();
                self.column_rows = 0;
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_sub(prefetched_retained_bytes(&self.prefetched));
                self.prefetched.clear();
                self.stream = None;
            }
        }
        if self.rows.is_empty() {
            let capacity = self.rows.capacity();
            self.rows.shrink_to_fit();
            self.retained_bytes = self.retained_bytes.saturating_sub(
                capacity.saturating_sub(self.rows.capacity()) * std::mem::size_of::<Vec<Value>>(),
            );
        }
        if self.column_rows == 0 && !self.columns.is_empty() {
            self.retained_bytes =
                self.retained_bytes
                    .saturating_sub(projected_columns_retained_bytes(
                        self.columns.capacity(),
                        &self.columns,
                    ));
            self.columns.clear();
            self.columns.shrink_to_fit();
        }
        Ok(Some(RecordBatch::new(row_count, columns)?))
    }

    #[allow(clippy::too_many_lines)] // one round, as `next_batch` reads one
    fn fold_round(
        &mut self,
        available_memory: usize,
        max_batches: usize,
        fold: crate::ScanBatchFold<'_>,
    ) -> Result<crate::FoldedRound, ExecError> {
        // A LIMIT hands its rows out one batch at a time, and rows already
        // buffered for `next_batch` are that path's to deliver.
        if self.remaining.is_some()
            || !self.rows.is_empty()
            || self.column_rows != 0
            || !self.prefetched.is_empty()
        {
            return Ok(crate::FoldedRound::Unavailable);
        }
        self.started = true;
        if !self.ready.is_empty() {
            // Batches a pulled round adopted and nobody has taken yet: they
            // are this round, folded where the pool finds room for them.
            let take = self.ready.len().min(max_batches.max(1));
            let ready: Vec<(RecordBatch, bool)> = self.ready.drain(..take).collect();
            let bytes: usize = ready.iter().map(|(batch, _)| batch.estimated_bytes()).sum();
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
            let base = self.fold_order;
            self.fold_order = base.saturating_add(ready.len() as u64);
            let returned = ready
                .into_par_iter()
                .enumerate()
                .map(|(position, (batch, prefiltered))| {
                    let order = (base + position as u64) << ORDER_SLICE_SHIFT;
                    let place = crate::ScanBatchPlace { prefiltered, order };
                    fold(batch, place).map(|left| left.map(|batch| (order, batch)))
                })
                .collect::<Result<Vec<_>, ExecError>>()?;
            return Ok(crate::FoldedRound::Round {
                returned: returned.into_iter().flatten().collect(),
            });
        }
        let Some(stream) = &mut self.stream else {
            return Ok(crate::FoldedRound::Done);
        };
        let planned_rows = planned_scan_rows(&self.types, available_memory);
        let batch_overhead = batch_memory_upper_bound(&self.types, planned_rows);
        // The round's width comes from the pool that runs it, which is the
        // caller's: its workers decode and fold, and the scan's own pool
        // has no part in the statement.
        let width = if available_memory < TIGHT_CEILING_BYTES {
            1
        } else {
            rayon::current_num_threads()
                .max(1)
                .saturating_mul(SLICES_PER_SCAN_THREAD)
        };
        // A slice is one batch, or close to it.
        let width = width.min(max_batches.max(1));
        let chunk_budget = (available_memory / 2).saturating_sub(batch_overhead);
        let failed: Mutex<Option<ExecError>> = Mutex::new(None);
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let types = &self.types;
        let enum_labels = &self.enum_labels;
        let set_members = &self.set_members;
        let adopt_filter = self.adopt_filter.as_ref();
        let fail = |error: ExecError| {
            stopped.store(true, Ordering::Relaxed);
            let mut slot = failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.get_or_insert(error);
        };
        // One slice's chunk, on the thread that decoded it: adopted into a
        // batch, tested against the scan's predicates and folded while its
        // columns are still in this core's cache.
        let base = self.fold_order;
        self.fold_order = base.saturating_add(width as u64);
        let fold_chunk = |chunk: ProjectedColumnChunk, slice: usize, piece: usize| {
            let stats = chunk.stats();
            let decode = chunk.column_decode().to_vec();
            let prefiltered = chunk.prefiltered();
            let mut returned = Vec::new();
            match adopt_chunk(chunk, types, enum_labels, set_members) {
                Ok((batches, _)) => {
                    for (cut, mut batch) in batches.into_iter().enumerate() {
                        if stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        let passed = prefiltered
                            || adopt_filter.is_some_and(|filter| filter.apply(&mut batch));
                        // The slice's place in the scan, then the chunk's
                        // in the slice, then the batch's in the chunk.
                        let order = ((base + slice as u64) << ORDER_SLICE_SHIFT)
                            | ((piece.min(ORDER_PART_MAX) as u64) << ORDER_PIECE_SHIFT)
                            | cut.min(ORDER_PART_MAX) as u64;
                        let place = crate::ScanBatchPlace {
                            prefiltered: passed,
                            order,
                        };
                        match fold(batch, place) {
                            Ok(None) => {}
                            Ok(Some(batch)) => returned.push((order, batch)),
                            Err(error) => fail(error),
                        }
                    }
                }
                Err(error) => fail(error),
            }
            (stats, decode, returned)
        };
        let proceed = || !stopped.load(Ordering::Relaxed);
        let (folded, abandon_prewhere) = if let Some(spec) = &self.prewhere {
            let judged = AtomicUsize::new(0);
            let dense = AtomicUsize::new(0);
            let unanswered = AtomicUsize::new(0);
            let exact_ranges =
                spec.predicate_ids.len() > 1 && spec.predicate_ids == stream.column_ids();
            let select = |columns: &[DecodedColumn], row_count: usize| {
                judged.fetch_add(1, Ordering::Relaxed);
                Ok(
                    match prewhere_ranges(spec, columns, row_count, exact_ranges)? {
                        Ok(ranges) => Some(ranges),
                        Err(Unrestricted::Dense) => {
                            dense.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                        Err(Unrestricted::Unanswered) => {
                            unanswered.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    },
                )
            };
            let folded = stream
                .fold_column_chunks(
                    width,
                    chunk_budget,
                    Some((&spec.predicate_ids, &select)),
                    &proceed,
                    &fold_chunk,
                )
                .map_err(|error| ExecError::Source(error.to_string()))?;
            let judged = judged.load(Ordering::Relaxed);
            let dense = dense.load(Ordering::Relaxed);
            let unanswered = unanswered.load(Ordering::Relaxed);
            if judged > 0 {
                // The same reading of a round as `next_batch` makes: a
                // stretch that decodes whole is sampled from here on.
                let skipped = folded.as_ref().is_some_and(|folded| {
                    folded
                        .iter()
                        .any(|(stats, _, _)| stats.blocks_value_skipped() > 0)
                });
                stream.sample_prewhere(
                    dense + unanswered == judged && !skipped && stream.index_lookup().is_none(),
                );
            }
            (
                folded,
                judged > 0 && unanswered == judged && !stream.has_value_index_lookup(),
            )
        } else {
            (
                stream
                    .fold_column_chunks(width, chunk_budget, None, &proceed, &fold_chunk)
                    .map_err(|error| ExecError::Source(error.to_string()))?,
                false,
            )
        };
        if abandon_prewhere {
            self.prewhere = None;
        }
        let Some(folded) = folded else {
            self.stream = None;
            return Ok(crate::FoldedRound::Done);
        };
        let mut returned = Vec::new();
        for (stats, decode, batches) in folded {
            self.accumulate_stats(stats, &decode);
            returned.extend(batches);
        }
        if let Some(error) = failed
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(error);
        }
        Ok(crate::FoldedRound::Round { returned })
    }

    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn next_batch_memory_upper_bound(&self, budget: usize) -> usize {
        let planned = self.planned_batch_rows(budget);
        let row_count = self.rows.len().max(self.column_rows).min(planned);
        if row_count == 0 {
            return self
                .stream
                .as_ref()
                .map_or(0, |_| batch_memory_upper_bound(&self.types, planned));
        }
        batch_memory_upper_bound(&self.types, row_count)
    }

    fn restrict_integer_membership(
        &mut self,
        position: usize,
        member: &crate::execution::IntegerMembership,
    ) {
        if self.started {
            return;
        }
        if let Some((spec, index)) = self.filter_first_column(position) {
            spec.membership = Some((index, Arc::clone(member)));
        }
    }

    fn restrict_key_position_set(&mut self, position: usize, values: &[i128]) {
        if self.key_position != Some(position) {
            self.restrict_value_set(position, values);
        }
    }

    fn restrict_key_position_text_set(
        &mut self,
        position: usize,
        collation: Collation,
        weights: &[&[u8]],
    ) {
        self.restrict_text_set(position, collation, weights);
    }

    fn restrict_order_limit(
        &mut self,
        position: usize,
        k: usize,
        descending: bool,
        nulls_first: bool,
    ) -> bool {
        // The key column's order is the scan's own; a started scan can no
        // longer be narrowed.
        if self.started
            || !pintail_store::side_index_enabled()
            || self.key_position == Some(position)
        {
            return false;
        }
        let Some(stream) = &self.stream else {
            return false;
        };
        let Some(column_id) = stream.column_ids().get(position).copied() else {
            return false;
        };
        let Ok(Some((bound, nulls))) = stream.side_index_order_bound(column_id, k, descending)
        else {
            return false;
        };
        // A NULL sorting first would belong before every restricted row.
        if nulls && nulls_first {
            return false;
        }
        let value = |bound: i128| {
            i64::try_from(bound).map_or_else(
                |_| u64::try_from(bound).map_or(Value::Null, Value::UInt64),
                Value::Int64,
            )
        };
        let (min, max) = if descending {
            (value(bound), Value::UInt64(u64::MAX))
        } else {
            (Value::Int64(i64::MIN), value(bound))
        };
        if matches!(min, Value::Null) || matches!(max, Value::Null) {
            return false;
        }
        self.restrict_value_range(position, &min, &max);
        // Applied only when the scan took the span as a filter-first range.
        self.prewhere
            .as_ref()
            .is_some_and(|spec| spec.runtime_range.is_some())
    }

    fn restrict_key_position_range(&mut self, position: usize, min: &Value, max: &Value) {
        if self.started {
            return;
        }
        if self.key_position != Some(position) {
            self.restrict_value_range(position, min, max);
            return;
        }
        let Some(stream) = &self.stream else {
            return;
        };
        let (start, end) = stream.key_range();
        let ([start_part], [end_part]) = (start.parts(), end.parts()) else {
            return;
        };
        let Some(min_part) = key_part_for_bound(min, start_part) else {
            return;
        };
        let Some(max_part) = key_part_for_bound(max, start_part) else {
            return;
        };
        let new_start = if min_part > *start_part {
            min_part
        } else {
            start_part.clone()
        };
        let new_end = if max_part < *end_part {
            max_part
        } else {
            end_part.clone()
        };
        if new_start == *start_part && new_end == *end_part {
            return;
        }
        if new_start > new_end {
            // The build side proves no probe row can match.
            self.stream = None;
            return;
        }
        let column_ids = stream.column_ids().to_vec();
        let new_start = PrimaryKey::new(vec![new_start]).expect("one-part key");
        let new_end = PrimaryKey::new(vec![new_end]).expect("one-part key");
        if let Ok(Some(mut rebuilt)) =
            stream
                .snapshot()
                .scan_projected_range_stream(&new_start, &new_end, &column_ids)
        {
            // The narrower range covers its end segments only in part. The
            // stream locates the rows it selects there by the key columns
            // the overlay names; a rebuild without them fell back to
            // merging each such segment whole from its row headers, which
            // a tight ceiling cannot hold and cannot slice.
            if let Some(key_ids) = stream.memtable_overlay_key() {
                rebuilt.enable_memtable_overlay(key_ids);
            }
            self.stream = Some(rebuilt);
        }
        // On decline or error the original stream stays: best-effort pruning.
    }
}

/// Derives per-column value bounds from a scan's predicate conjuncts for
/// SMA segment pruning: `col <op> literal` comparisons and non-negated
/// `BETWEEN` over integer, unsigned, date, and datetime columns. Anything
/// else contributes no bound (never unsound — pruning only tightens).
#[allow(clippy::too_many_lines)] // one linear conjunct-shape walk
fn sma_column_bounds(predicates: &[BoundExpr]) -> Vec<pintail_store::ColumnBounds> {
    use pintail_store::{BoundDomain, ColumnBounds, NativeUnits};

    fn column_domain(column: &pintail_sql::BoundColumn) -> Option<BoundDomain> {
        match column.data_type {
            pintail_types::DataType::Int64
            | pintail_types::DataType::Int32
            | pintail_types::DataType::Int16
            | pintail_types::DataType::Int8 => Some(BoundDomain::Int),
            pintail_types::DataType::UInt64
            | pintail_types::DataType::UInt32
            | pintail_types::DataType::UInt16
            | pintail_types::DataType::UInt8 => Some(BoundDomain::UInt),
            pintail_types::DataType::Date32 => Some(BoundDomain::Temporal(NativeUnits::Date)),
            pintail_types::DataType::DateTime64 { fsp } => {
                Some(BoundDomain::Temporal(NativeUnits::DateTime { fsp }))
            }
            _ => None,
        }
    }

    fn literal_units(domain: BoundDomain, value: &Value) -> Option<i128> {
        match (domain, value) {
            (BoundDomain::Int | BoundDomain::UInt, Value::Int64(value)) => Some(i128::from(*value)),
            (BoundDomain::Int | BoundDomain::UInt, Value::UInt64(value)) => {
                Some(i128::from(*value))
            }
            (BoundDomain::Temporal(NativeUnits::Date), Value::Utf8(text)) => {
                pintail_types::parse_date_days(text).map(i128::from)
            }
            // A DATE value - DATE(...), CURDATE() or an interval over one -
            // compares with a DATETIME column as its midnight.
            (BoundDomain::Temporal(NativeUnits::DateTime { .. }), Value::Utf8(text)) => {
                pintail_types::parse_datetime_micros(text)
                    .or_else(|| pintail_types::parse_date_days(text)?.checked_mul(86_400_000_000))
                    .map(i128::from)
            }
            _ => None,
        }
    }

    // A DATETIME column widened to more fraction digits keeps every value
    // exactly, so a bound on the widened reading bounds the column.
    fn widened_datetime(expr: &BoundExpr) -> &BoundExpr {
        if let BoundExprKind::Scalar {
            function: ScalarFunction::Cast(pintail_types::DataType::DateTime64 { fsp: wider }),
            args,
        } = &expr.kind
            && let [inner] = args.as_slice()
            && let BoundExprKind::Column(column) = &inner.kind
            && matches!(column.data_type, pintail_types::DataType::DateTime64 { fsp } if fsp <= *wider)
        {
            return inner;
        }
        expr
    }

    let mut bounds: Vec<ColumnBounds> = Vec::new();
    let mut apply =
        |column: &pintail_sql::BoundColumn, lower: Option<i128>, upper: Option<i128>| {
            let Some(domain) = column_domain(column) else {
                return;
            };
            let entry = bounds
                .iter_mut()
                .find(|bound| bound.column_id == column.column_id && bound.domain == domain);
            let entry = if let Some(entry) = entry {
                entry
            } else {
                bounds.push(ColumnBounds {
                    column_id: column.column_id,
                    domain,
                    lower: None,
                    upper: None,
                });
                bounds.last_mut().expect("just pushed")
            };
            if let Some(lower) = lower {
                entry.lower = Some(entry.lower.map_or(lower, |existing| existing.max(lower)));
            }
            if let Some(upper) = upper {
                entry.upper = Some(entry.upper.map_or(upper, |existing| existing.min(upper)));
            }
        };

    for predicate in predicates {
        match &predicate.kind {
            BoundExprKind::Binary { op, left, right } => {
                let (left, right) = (widened_datetime(left), widened_datetime(right));
                let (column, literal, op) = match (&left.kind, &right.kind) {
                    (BoundExprKind::Column(column), BoundExprKind::Literal(value)) => {
                        (column, value, *op)
                    }
                    (BoundExprKind::Literal(value), BoundExprKind::Column(column)) => {
                        let flipped = match op {
                            BinaryOp::Less => BinaryOp::Greater,
                            BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
                            BinaryOp::Greater => BinaryOp::Less,
                            BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
                            other => *other,
                        };
                        (column, value, flipped)
                    }
                    _ => continue,
                };
                let Some(domain) = column_domain(column) else {
                    continue;
                };
                let Some(units) = literal_units(domain, literal) else {
                    continue;
                };
                match op {
                    BinaryOp::Equal => apply(column, Some(units), Some(units)),
                    BinaryOp::Less => apply(column, None, Some(units - 1)),
                    BinaryOp::LessOrEqual => apply(column, None, Some(units)),
                    // Past a bare day only its midnight is certain to be
                    // excluded, and only when the text reads as a
                    // DATETIME; the bound keeps midnight either way.
                    BinaryOp::Greater
                        if matches!(
                            domain,
                            BoundDomain::Temporal(NativeUnits::DateTime { .. })
                        ) && matches!(literal, Value::Utf8(text) if text.len() == 10) =>
                    {
                        apply(column, Some(units), None);
                    }
                    BinaryOp::Greater => apply(column, Some(units + 1), None),
                    BinaryOp::GreaterOrEqual => apply(column, Some(units), None),
                    _ => {}
                }
            }
            BoundExprKind::Scalar {
                function: ScalarFunction::Between { negated: false },
                args,
            } => {
                let [subject, low, high] = args.as_slice() else {
                    continue;
                };
                let BoundExprKind::Column(column) = &subject.kind else {
                    continue;
                };
                let Some(domain) = column_domain(column) else {
                    continue;
                };
                let (BoundExprKind::Literal(low), BoundExprKind::Literal(high)) =
                    (&low.kind, &high.kind)
                else {
                    continue;
                };
                if let (Some(low), Some(high)) =
                    (literal_units(domain, low), literal_units(domain, high))
                {
                    apply(column, Some(low), Some(high));
                }
            }
            // `column IN (constants)` holds only between the least and the
            // greatest of them; a NULL in the list matches no row. A
            // constant outside the column's domain leaves the column
            // unbounded, since it compares by conversion.
            BoundExprKind::Scalar {
                function: ScalarFunction::InList { negated: false },
                args,
            } if args.len() > 1 => {
                let BoundExprKind::Column(column) = &args[0].kind else {
                    continue;
                };
                let Some(domain) = column_domain(column) else {
                    continue;
                };
                let mut listed = Vec::with_capacity(args.len() - 1);
                for argument in &args[1..] {
                    let units = match &argument.kind {
                        BoundExprKind::Literal(Value::Null) => continue,
                        BoundExprKind::Literal(value) => literal_units(domain, value),
                        _ => None,
                    };
                    let Some(units) = units else {
                        listed.clear();
                        break;
                    };
                    listed.push(units);
                }
                if let (Some(least), Some(greatest)) = (listed.iter().min(), listed.iter().max()) {
                    apply(column, Some(*least), Some(*greatest));
                }
            }
            _ => {}
        }
    }
    bounds
}

/// Converts a probe-side bound into the key-part shape of the scanned
/// table's primary key, refusing any type mismatch (a mismatched bound
/// cannot prune safely).
fn key_part_for_bound(value: &Value, template: &KeyPart) -> Option<KeyPart> {
    match (value, template) {
        (Value::Int64(value), KeyPart::Int64(_)) => Some(KeyPart::Int64(*value)),
        (Value::UInt64(value), KeyPart::UInt64(_)) => Some(KeyPart::UInt64(*value)),
        (Value::Utf8(value), KeyPart::Utf8(_)) => Some(KeyPart::Utf8(value.clone())),
        _ => None,
    }
}

fn projected_values_retained_bytes(capacity: usize, rows: &[Vec<Value>]) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<Vec<Value>>())
        .saturating_add(
            rows.iter()
                .map(|values| projected_value_payload_bytes(values))
                .sum(),
        )
}

fn projected_value_payload_bytes(values: &[Value]) -> usize {
    std::mem::size_of_val(values).saturating_add(values.iter().map(Value::heap_bytes).sum())
}

fn projected_columns_retained_bytes(outer_capacity: usize, columns: &[DecodedColumn]) -> usize {
    outer_capacity
        .saturating_mul(std::mem::size_of::<DecodedColumn>())
        .saturating_add(columns.iter().map(DecodedColumn::retained_bytes).sum())
}

/// The predicates that choose a scan's rows first, and whether they are all
/// of them. A test on a wide column (a JSON document, say, tested only for
/// NULL) decoded that column for every row of the table before the narrow
/// tests beside it had rejected almost all of them. Those tests choose the
/// rows alone; the wide one runs in the Filter above, over the rows they
/// kept, whose wide values are decoded for the output anyway.
fn narrow_first_predicates<'a>(
    scan: &'a Scan,
    snapshot: &TableSnapshot,
) -> (Vec<&'a BoundExpr>, bool) {
    let column_type = |id: u32| {
        snapshot
            .schema()
            .columns()
            .iter()
            .find(|column| column.id() == id)
            .map(pintail_types::Column::data_type)
    };
    let (narrow, wide): (Vec<&BoundExpr>, Vec<&BoundExpr>) =
        scan.predicates.iter().partition(|predicate| {
            let mut ids = Vec::new();
            collect_predicate_columns(predicate, &mut ids);
            !ids.iter()
                .any(|id| column_type(*id).is_some_and(is_wide_prewhere_type))
        });
    if narrow.is_empty() || wide.is_empty() {
        (scan.predicates.iter().collect(), true)
    } else {
        (narrow, false)
    }
}

/// Whether `expr` reads nothing but constants and the column `column_id`
/// of the scanned table, through shapes whose answer depends on the
/// column's value alone.
fn reads_only_column(expr: &BoundExpr, scan: &Scan, column_id: u32) -> bool {
    match &expr.kind {
        BoundExprKind::Column(column) => {
            !column.outer
                && column.table_id == scan.table.table_id
                && column.database_id == scan.table.database_id
                && column.column_id == column_id
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => reads_only_column(expr, scan, column_id),
        BoundExprKind::Binary { left, right, .. } => {
            reads_only_column(left, scan, column_id) && reads_only_column(right, scan, column_id)
        }
        BoundExprKind::Scalar { args, .. } => args
            .iter()
            .all(|argument| reads_only_column(argument, scan, column_id)),
        _ => false,
    }
}

/// The scan's predicates that each read one text column, as questions the
/// store can ask of a value: would a row holding it pass. A low-cardinality
/// text column - a state, a kind - holds a handful of values per segment,
/// so the store asks once per value and skips every block holding none
/// that passes.
///
/// The answer is the predicate itself evaluated over the value under the
/// plan's collation, so equality, lists, negation, patterns and functions
/// of the column all decide blocks exactly as the filter decides rows. A
/// predicate that cannot be evaluated that way admits every value.
#[allow(clippy::too_many_lines)]
fn text_value_filters(
    scan: &Scan,
    snapshot: &TableSnapshot,
    collation: Collation,
) -> Vec<pintail_store::TextValueFilter> {
    let mut filters = Vec::new();
    for predicate in &scan.predicates {
        let mut ids = Vec::new();
        collect_predicate_columns(predicate, &mut ids);
        ids.sort_unstable();
        ids.dedup();
        let [column_id] = ids.as_slice() else {
            continue;
        };
        if !reads_only_column(predicate, scan, *column_id)
            || crate::optimizer::is_volatile(predicate)
        {
            continue;
        }
        let Some(column) = snapshot
            .schema()
            .columns()
            .iter()
            .find(|column| column.id() == *column_id)
        else {
            continue;
        };
        if column.data_type() != pintail_types::DataType::Utf8
            || column.enum_labels().is_some()
            || column.set_members().is_some()
        {
            continue;
        }
        let layout = [pintail_sql::BoundColumn {
            database_id: scan.table.database_id,
            table_id: scan.table.table_id,
            column_id: *column_id,
            relation_name: predicate_relation_name(predicate)
                .unwrap_or_else(|| scan.table.table_name.clone()),
            name: column.name().to_owned(),
            data_type: column.data_type(),
            nullable: column.is_nullable(),
            collation: column.collation().map(str::to_owned),
            enum_labels: None,
            geometry: false,
            timestamp: false,
            binary_width: column.binary_width(),
            bit_width: column.bit_width(),
            float_decimals: column.float_decimals(),
            outer: false,
            using_shadowed: false,
        }];
        let Ok(compiled) = crate::expression::CompiledExpr::compile(predicate, &layout, collation)
        else {
            continue;
        };
        let answers = Mutex::new(std::collections::HashMap::<Option<String>, bool>::new());
        let admits = move |value: Option<&str>| {
            let key = value.map(str::to_owned);
            if let Some(known) = answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
            {
                return *known;
            }
            let passes = || -> Option<bool> {
                let vector = crate::ColumnVector::new(
                    pintail_types::DataType::Utf8,
                    vec![key.clone().map_or(Value::Null, Value::Utf8)],
                )
                .ok()?;
                let batch = RecordBatch::new(1, vec![vector]).ok()?;
                // The filter's own kernels first, then the row evaluation
                // the Filter operator falls back to for the rest.
                let mask = match compiled.evaluate_filter_mask(&batch).ok()? {
                    Some(mask) => Some(mask),
                    None => compiled.evaluate_quiet_mask(&batch),
                };
                match mask {
                    Some(mask) => Some(mask.count() > 0),
                    None => {
                        crate::expression::predicate_truth(&compiled.evaluate(&batch, 0).ok()?).ok()
                    }
                }
            };
            // An evaluation that fails or declines proves nothing: the
            // value stays, and the filter above decides its rows.
            let answer = passes().unwrap_or(true);
            answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, answer);
            answer
        };
        filters.push(pintail_store::TextValueFilter {
            column_id: *column_id,
            admits: Arc::new(admits),
        });
    }
    filters
}

/// Builds the filter-first spec for a scan: every predicate must reference
/// only projected columns and compile against the predicate-subset layout.
fn build_prewhere_spec(
    scan: &Scan,
    snapshot: &TableSnapshot,
    collation: Collation,
    skips_by_value: bool,
) -> Option<PrewhereSpec> {
    if scan.predicates.is_empty() {
        return None;
    }
    let (chosen, complete) = narrow_first_predicates(scan, snapshot);
    let mut predicate_ids = Vec::new();
    for predicate in &chosen {
        collect_predicate_columns(predicate, &mut predicate_ids);
    }
    predicate_ids.sort_unstable();
    predicate_ids.dedup();
    if predicate_ids.is_empty()
        || !predicate_ids
            .iter()
            .all(|id| scan.projected_column_ids.contains(id))
    {
        // The predicate layout must be available in the projection.
        return None;
    }
    // The predicates name their columns under the relation the query gave
    // the table, and compiling matches that name: a layout under the bare
    // table name failed to compile for every aliased table, and the scan
    // silently decoded every projected column of every row.
    let relation_name = chosen
        .iter()
        .copied()
        .find_map(predicate_relation_name)
        .unwrap_or_else(|| scan.table.table_name.clone());
    let mut layout = Vec::with_capacity(predicate_ids.len());
    let mut data_types = Vec::with_capacity(predicate_ids.len());
    let mut enum_labels = Vec::with_capacity(predicate_ids.len());
    let mut set_members = Vec::with_capacity(predicate_ids.len());
    for id in &predicate_ids {
        let column = snapshot
            .schema()
            .columns()
            .iter()
            .find(|column| column.id() == *id)?;
        layout.push(pintail_sql::BoundColumn {
            database_id: scan.table.database_id,
            table_id: scan.table.table_id,
            column_id: *id,
            relation_name: relation_name.clone(),
            name: column.name().to_owned(),
            data_type: column.data_type(),
            nullable: column.is_nullable(),
            collation: column.collation().map(str::to_owned),
            enum_labels: None,
            geometry: false,
            timestamp: false,
            binary_width: column.binary_width(),
            bit_width: column.bit_width(),
            float_decimals: column.float_decimals(),
            outer: false,
            using_shadowed: false,
        });
        data_types.push(column.data_type());
        enum_labels.push(column.enum_labels().map(|labels| Arc::new(labels.to_vec())));
        set_members.push(
            column
                .set_members()
                .map(|members| Arc::new(members.to_vec())),
        );
    }
    // Fuse a multi-column conjunction when its projection consists entirely
    // of integer predicate inputs. The single-column scan keeps its existing
    // packed path; the extra column no longer expands the retained round.
    let exact_ranges = predicate_ids.len() > 1
        && predicate_ids == scan.projected_column_ids
        && data_types.iter().all(|kind| {
            matches!(
                kind,
                pintail_types::DataType::Int64 | pintail_types::DataType::UInt64
            )
        });
    // A scan that projects nothing beyond its predicate columns has nothing
    // to decode second - unless the side index can name its rows, when the
    // predicate columns themselves decode for those rows alone.
    // Nor when a text predicate can rule whole blocks out by the values
    // they hold: the filter-first read is where blocks are skipped.
    let reads_selectively = exact_ranges || skips_by_value;
    let filter_only = predicate_ids.len() >= scan.projected_column_ids.len() && !exact_ranges;
    if predicate_ids.len() >= scan.projected_column_ids.len()
        && !reads_selectively
        && !(pintail_store::side_index_enabled()
            && predicate_index_lookup(scan, snapshot, collation).is_some())
    {
        return None;
    }
    let predicates = chosen
        .iter()
        .map(|predicate| crate::expression::CompiledExpr::compile(predicate, &layout, collation))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let predicates = crate::expression::pair_column_ranges(predicates);
    Some(PrewhereSpec {
        predicate_ids,
        predicates,
        data_types,
        enum_labels,
        set_members,
        collation,
        complete,
        runtime_range: None,
        membership: None,
        filter_only,
    })
}

/// Most values an IN list or a join key set may hand the side index.
pub(crate) const INDEX_LOOKUP_VALUES: usize = 4_096;

/// The side-index lookup a scan's own predicates imply: a top-level
/// equality or IN list of literals, integer literals on an integer column
/// other than the table's key (whose range the key bounds already prune)
/// or text literals on a text column, the table's key included. A text lookup names
/// its values by their key under the collation the comparison itself uses,
/// so every row that compares equal to a literal - whatever its case,
/// accents or trailing spaces where the collation ignores them - is a
/// candidate. Among several, an integer one before a text one (an
/// identifier names fewer rows than a label as a rule, and costs no
/// collation key per row still in the memtable), then the one naming the
/// fewest values.
fn predicate_index_lookup(
    scan: &Scan,
    snapshot: &TableSnapshot,
    collation: Collation,
) -> Option<pintail_store::IndexLookup> {
    let indexed_column = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::Column(column) => {
            let data_type = snapshot
                .schema()
                .columns()
                .iter()
                .find(|candidate| candidate.id() == column.column_id)?
                .data_type();
            if is_integer_type(data_type) {
                // An integer key's range already prunes by the key bounds.
                (!scan.table.key_column_ids.contains(&column.column_id))
                    .then_some((column.column_id, false))
            } else {
                // A text key is included: storage orders its keys by their
                // bytes, which no collation's equality follows, so the key
                // bounds prune nothing for `key = 'literal'`.
                (data_type == pintail_types::DataType::Utf8).then_some((column.column_id, true))
            }
        }
        _ => None,
    };
    let literal =
        |expr: &BoundExpr, keyer: Option<&pintail_store::TextKeyer>| match (&expr.kind, keyer) {
            (BoundExprKind::Literal(value), None) => integer_bound(value),
            (BoundExprKind::Literal(Value::Utf8(text)), Some(keyer)) => {
                Some(i128::from(keyer.value(text)))
            }
            _ => None,
        };
    // The collation a comparison node compiles under: its operands', else
    // the plan's.
    let keyer_for = |expr: &BoundExpr, text: bool| -> Option<Option<pintail_store::TextKeyer>> {
        if !text {
            return Some(None);
        }
        let compared = expr
            .text_collation()
            .and_then(Collation::from_mysql_name)
            .unwrap_or(collation);
        text_keyer(compared).map(Some)
    };
    let lookup = |expr: &BoundExpr| -> Option<pintail_store::IndexLookup> {
        let (column_id, keyer, values) = match &expr.kind {
            BoundExprKind::Binary {
                op: BinaryOp::Equal,
                left,
                right,
            } => {
                let (column, value) = if let Some(column) = indexed_column(left) {
                    (column, right)
                } else {
                    (indexed_column(right)?, left)
                };
                let keyer = keyer_for(expr, column.1)?;
                let value = literal(value, keyer.as_ref())?;
                (column.0, keyer, vec![value])
            }
            BoundExprKind::Scalar {
                function: ScalarFunction::InList { negated: false },
                args,
            } => {
                let (subject, list) = args.split_first()?;
                let (column_id, text) = indexed_column(subject)?;
                let keyer = keyer_for(expr, text)?;
                let values = list
                    .iter()
                    .map(|item| literal(item, keyer.as_ref()))
                    .collect::<Option<Vec<_>>>()?;
                if values.len() > INDEX_LOOKUP_VALUES {
                    return None;
                }
                (column_id, keyer, values)
            }
            _ => return None,
        };
        Some(pintail_store::IndexLookup {
            column_id,
            key: keyer.map_or(
                pintail_store::IndexKey::Integer,
                pintail_store::IndexKey::Text,
            ),
            probe: pintail_store::IndexProbe::Values(values),
        })
    };
    let mut conjuncts = Vec::new();
    for predicate in &scan.predicates {
        flatten_and(predicate, &mut conjuncts);
    }
    let mut chosen = conjuncts
        .into_iter()
        .filter_map(lookup)
        .min_by_key(|lookup| {
            (
                matches!(lookup.key, pintail_store::IndexKey::Text(_)),
                match &lookup.probe {
                    pintail_store::IndexProbe::Values(values) => values.len(),
                    pintail_store::IndexProbe::Span(..) => usize::MAX,
                },
            )
        })?;
    if let pintail_store::IndexProbe::Values(values) = &mut chosen.probe {
        values.sort_unstable();
        values.dedup();
    }
    Some(chosen)
}

/// The side index's view of a collation: its identity and key function.
/// `None` for the JSON ladder, which is not a text collation.
fn text_keyer(collation: Collation) -> Option<pintail_store::TextKeyer> {
    if collation == Collation::Json {
        return None;
    }
    // Any identity distinct per collation will do: it names cache entries
    // within the process and is never persisted.
    let id = collation.mysql_name().bytes().fold(0_u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    Some(pintail_store::TextKeyer::new(
        id,
        std::sync::Arc::new(move |text: &str, out: &mut Vec<u8>| {
            crate::execution::append_collation_key(text, collation, out);
        }),
    ))
}

fn flatten_and<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            flatten_and(left, out);
            flatten_and(right, out);
        }
        _ => out.push(expr),
    }
}

/// Integer carrier types a runtime join span can be compared against.
fn is_integer_type(data_type: pintail_types::DataType) -> bool {
    matches!(
        data_type,
        pintail_types::DataType::Int8
            | pintail_types::DataType::Int16
            | pintail_types::DataType::Int32
            | pintail_types::DataType::Int64
            | pintail_types::DataType::UInt8
            | pintail_types::DataType::UInt16
            | pintail_types::DataType::UInt32
            | pintail_types::DataType::UInt64
    )
}

fn integer_bound(value: &Value) -> Option<i128> {
    match value {
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        _ => None,
    }
}

fn intersect_masks(
    combined: Option<crate::SelectionMask>,
    mask: crate::SelectionMask,
) -> Result<crate::SelectionMask, String> {
    match combined {
        None => Ok(mask),
        Some(mut existing) => {
            existing
                .intersect(&mask)
                .map_err(|error| error.to_string())?;
            Ok(existing)
        }
    }
}

/// Rows of an integer column inside `[lower, upper]`; NULLs are outside.
fn runtime_range_mask(
    range: RuntimeRange,
    columns: &[DecodedColumn],
    rows: usize,
) -> Option<crate::SelectionMask> {
    let mut mask = crate::SelectionMask::none(rows);
    macro_rules! fill {
        ($values:expr, $validity:expr) => {
            for (row, value) in $values.iter().enumerate() {
                let value = i128::from(*value);
                if $validity.is_valid(row) && value >= range.lower && value <= range.upper {
                    mask.set(row, true).ok()?;
                }
            }
        };
    }
    match columns.get(range.index)? {
        DecodedColumn::Int64 { values, validity } => fill!(values, validity),
        DecodedColumn::UInt64 { values, validity } => fill!(values, validity),
        _ => return None,
    }
    Some(mask)
}

/// The relation name the first column a predicate reads is bound under.
fn predicate_relation_name(expr: &BoundExpr) -> Option<String> {
    match &expr.kind {
        BoundExprKind::Column(column) => Some(column.relation_name.clone()),
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. }
        | BoundExprKind::InSubquery { expr, .. } => predicate_relation_name(expr),
        BoundExprKind::Binary { left, right, .. } => {
            predicate_relation_name(left).or_else(|| predicate_relation_name(right))
        }
        BoundExprKind::Scalar { args, .. } => args.iter().find_map(predicate_relation_name),
        _ => None,
    }
}

fn collect_predicate_columns(expr: &BoundExpr, ids: &mut Vec<u32>) {
    match &expr.kind {
        BoundExprKind::Column(column) => ids.push(column.column_id),
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
            collect_predicate_columns(expr, ids);
        }
        BoundExprKind::Binary { left, right, .. } => {
            collect_predicate_columns(left, ids);
            collect_predicate_columns(right, ids);
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                collect_predicate_columns(argument, ids);
            }
        }
        BoundExprKind::InSubquery { expr, .. } => collect_predicate_columns(expr, ids),
        _ => {}
    }
}

/// Borrow integer buffers for an exact all-predicate projection. This
/// avoids cloning both columns merely to wrap them in executor vectors.
fn prewhere_integer_mask(
    predicate: &crate::expression::CompiledExpr,
    columns: &[DecodedColumn],
    rows: usize,
) -> Option<crate::SelectionMask> {
    use crate::expression::CompiledExpr;
    use pintail_sql::BinaryOp;
    use pintail_types::Value;
    let CompiledExpr::Binary {
        op, left, right, ..
    } = predicate
    else {
        return None;
    };
    if *op == BinaryOp::And {
        let mut mask = prewhere_integer_mask(left, columns, rows)?;
        mask.intersect(&prewhere_integer_mask(right, columns, rows)?)
            .ok()?;
        return Some(mask);
    }
    let (column, literal, reversed) = match (left.as_ref(), right.as_ref()) {
        (CompiledExpr::Column(column), CompiledExpr::Literal(value)) => (*column, value, false),
        (CompiledExpr::Literal(value), CompiledExpr::Column(column)) => (*column, value, true),
        _ => return None,
    };
    let literal = match literal {
        Value::Int64(value) => i128::from(*value),
        Value::UInt64(value) => i128::from(*value),
        _ => return None,
    };
    if !matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual
    ) {
        return None;
    }
    let compare = |value: i128| {
        let (left, right) = if reversed {
            (literal, value)
        } else {
            (value, literal)
        };
        match op {
            BinaryOp::Equal => left == right,
            BinaryOp::NotEqual => left != right,
            BinaryOp::Less => left < right,
            BinaryOp::LessOrEqual => left <= right,
            BinaryOp::Greater => left > right,
            _ => left >= right,
        }
    };
    let mut mask = crate::SelectionMask::none(rows);
    macro_rules! fill {
        ($values:expr, $validity:expr) => {
            for (row, value) in $values.iter().enumerate() {
                if $validity.is_valid(row) && compare(i128::from(*value)) {
                    mask.set(row, true).ok()?;
                }
            }
        };
    }
    match columns.get(column)? {
        DecodedColumn::Int64 { values, validity } => fill!(values, validity),
        DecodedColumn::UInt64 { values, validity } => fill!(values, validity),
        _ => return None,
    }
    Some(mask)
}

/// Narrows the predicates' mask by the scan's key membership, when it has
/// one its column can answer.
fn with_membership(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
    combined: Option<crate::batch::SelectionMask>,
) -> Result<Option<crate::batch::SelectionMask>, String> {
    let Some(mask) = spec
        .membership
        .as_ref()
        .and_then(|(index, member)| membership_mask(member, columns.get(*index), row_count))
    else {
        return Ok(combined);
    };
    intersect_masks(combined, mask).map(Some)
}

/// The rows of a decoded integer column whose value `member` accepts;
/// `None` for a column of any other form, which the caller leaves
/// unrestricted.
fn membership_mask(
    member: &crate::execution::IntegerMembership,
    column: Option<&DecodedColumn>,
    rows: usize,
) -> Option<crate::SelectionMask> {
    let mut mask = crate::SelectionMask::none(rows);
    macro_rules! fill {
        ($values:expr, $validity:expr) => {
            for (row, value) in $values.iter().enumerate().take(rows) {
                if $validity.is_valid(row) && member(i128::from(*value)) {
                    mask.set(row, true).ok()?;
                }
            }
        };
    }
    match column? {
        DecodedColumn::Int64 { values, validity } => fill!(values, validity),
        DecodedColumn::UInt64 { values, validity } => fill!(values, validity),
        _ => return None,
    }
    Some(mask)
}

/// The rows the scan's own filter-first predicates keep in one chunk, and
/// whether they could be answered from the decoded columns at all. A spec
/// with no predicates of its own answers with no mask.
/// Every predicate of `spec` answered as a range over its column's packed
/// signed values, read where they lie; `None` when any predicate is another
/// shape or its column has another form or holds a NULL.
///
/// The general path clones each predicate column into a batch to test it,
/// so a range filter on a date column copied the whole column of every
/// chunk once more before comparing it.
fn borrowed_range_masks(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
) -> Option<crate::batch::SelectionMask> {
    use crate::expression::SignedUnits;
    use pintail_types::DataType;
    if spec.predicates.is_empty() {
        return None;
    }
    let column_values = |index: usize| -> Option<(&[i64], SignedUnits, DataType)> {
        let data_type = *spec.data_types.get(index)?;
        let (values, units) = match columns.get(index)? {
            DecodedColumn::NativeUnits {
                units: pintail_store::NativeUnits::Date,
                values,
                validity,
            } if data_type == DataType::Date32 && validity.all_valid() => {
                (values, SignedUnits::Temporal)
            }
            DecodedColumn::NativeUnits {
                units: pintail_store::NativeUnits::DateTime { .. },
                values,
                validity,
            } if matches!(data_type, DataType::DateTime64 { .. }) && validity.all_valid() => {
                (values, SignedUnits::Temporal)
            }
            DecodedColumn::Int64 { values, validity }
                if data_type.storage_type() == DataType::Int64 && validity.all_valid() =>
            {
                (values, SignedUnits::Integer)
            }
            _ => return None,
        };
        (values.len() == row_count).then_some((values.as_slice(), units, data_type))
    };
    let mut combined: Option<crate::batch::SelectionMask> = None;
    for predicate in &spec.predicates {
        let mask = crate::expression::signed_slice_range_mask(predicate, column_values)?;
        match &mut combined {
            None => combined = Some(mask),
            Some(existing) => existing.intersect(&mask).ok()?,
        }
    }
    combined
}

fn predicate_mask(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
    exact_ranges: bool,
) -> Result<(bool, Option<crate::batch::SelectionMask>), String> {
    let packed = exact_ranges
        .then(|| {
            spec.predicates
                .iter()
                .map(|predicate| prewhere_integer_mask(predicate, columns, row_count))
                .collect::<Option<Vec<_>>>()
        })
        .flatten();
    let mut combined: Option<crate::batch::SelectionMask> = None;
    if let Some(masks) = packed {
        for mask in masks {
            match &mut combined {
                None => combined = Some(mask),
                Some(existing) => existing
                    .intersect(&mask)
                    .map_err(|error| error.to_string())?,
            }
        }
    } else if let Some(mask) = borrowed_range_masks(spec, columns, row_count) {
        combined = Some(mask);
    } else if !spec.predicates.is_empty() {
        let vectors = spec
            .data_types
            .iter()
            .zip(columns)
            .zip(spec.enum_labels.iter().zip(spec.set_members.iter()))
            .map(|((data_type, column), (labels, members))| {
                column_vector_from_decoded(
                    *data_type,
                    column.clone(),
                    labels.as_ref(),
                    members.as_ref(),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let batch = RecordBatch::new(row_count, vectors).map_err(|error| error.to_string())?;
        for predicate in &spec.predicates {
            // The packed comparison first, then the kernel tree over the
            // columns this round already decoded: a function of a column
            // skips ranges here rather than paying the decode and skipping
            // nothing. A predicate neither answers keeps every row, and the
            // Filter operator above applies the exact mask.
            let mask = match predicate
                .evaluate_filter_mask(&batch)
                .map_err(|error| error.to_string())?
            {
                Some(mask) => mask,
                None => match predicate.evaluate_quiet_mask(&batch) {
                    Some(mask) => mask,
                    None => return Ok((false, None)),
                },
            };
            match &mut combined {
                None => combined = Some(mask),
                Some(existing) => existing
                    .intersect(&mask)
                    .map_err(|error| error.to_string())?,
            }
        }
    }
    Ok((true, combined))
}

/// The rows a join's keys, pushed into the scan as a span or a set, leave
/// in one chunk; `None` when no join narrowed the scan.
fn runtime_mask(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
) -> Result<Option<crate::batch::SelectionMask>, String> {
    let mut combined = None;
    if let Some(range) = spec.runtime_range
        && let Some(mask) = runtime_range_mask(range, columns, row_count)
    {
        combined = Some(mask);
    }
    with_membership(spec, columns, row_count, combined)
}

/// Evaluates the compiled predicates over one chunk's predicate columns and
/// returns the surviving row ranges exactly, or why the chunk cannot or
/// need not be restricted.
///
/// The ranges are the mask's own runs, never merged across rejected rows.
/// Merging runs less than a block apart used to turn any scattered filter
/// into near-full coverage: one row in a hundred, or one in ten, kept
/// ranges over almost every row, the scan gave up on filter-first for the
/// rest of the table, and every projected column was decoded, adopted and
/// tested for every row. The store reads exact ranges a block at a time
/// and puts only their rows in the output, so the columns the filter does
/// not test cost their selected rows rather than all of them.
fn prewhere_ranges(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
    exact_ranges: bool,
) -> Result<Result<pintail_store::PrewhereRanges, Unrestricted>, String> {
    /// A chunk keeping at least this share of its rows (in percent)
    /// decodes whole: the selection would save almost nothing.
    const DENSE_PERCENT: usize = 90;
    // A join's keys that already left a few rows in the chunk decide the
    // decode alone: testing the scan's own predicates over every row costs
    // more than the Filters above testing them over the few left.
    const SPARSE_RUNTIME: usize = 16;
    let runtime = runtime_mask(spec, columns, row_count)?;
    let sparse = runtime
        .as_ref()
        .is_some_and(|mask| mask.count().saturating_mul(SPARSE_RUNTIME) <= row_count);
    let (predicates_applied, predicates) = if sparse {
        (false, None)
    } else {
        predicate_mask(spec, columns, row_count, exact_ranges)?
    };
    let combined = match (predicates, runtime) {
        (Some(mask), Some(runtime)) => Some(intersect_masks(Some(mask), runtime)?),
        (mask, runtime) => mask.or(runtime),
    };
    let Some(mask) = combined else {
        return Ok(Err(Unrestricted::Unanswered));
    };
    let selected = mask.count();
    if selected == 0 {
        return Ok(Ok(pintail_store::PrewhereRanges {
            ranges: Vec::new(),
            exact: true,
            mask: None,
        }));
    }
    if selected.saturating_mul(100) >= row_count.saturating_mul(DENSE_PERCENT) {
        return Ok(Err(Unrestricted::Dense));
    }
    let exact = spec.complete && !spec.predicates.is_empty() && predicates_applied;
    // Rows kept in runs of a row or two - a range filter on a column that
    // is not the key - would be tens of thousands of ranges per chunk; the
    // mask itself is a few thousand words, and a direct segment read
    // places the other columns from it.
    if mask.len() == row_count && mask.run_count() * MASK_RUN_WORDS > row_count.div_ceil(64) {
        return Ok(Ok(pintail_store::PrewhereRanges {
            ranges: Vec::new(),
            exact,
            mask: Some(mask.into_words()),
        }));
    }
    // The mask's runs a word at a time: a per-row bit test over every row
    // of a chunk cost as much as the comparison that built the mask.
    let mut ranges = mask.selected_runs();
    ranges.retain(|range| range.start < row_count);
    if let Some(last) = ranges.last_mut() {
        last.end = last.end.min(row_count);
        if last.start >= last.end {
            ranges.pop();
        }
    }
    Ok(Ok(pintail_store::PrewhereRanges {
        ranges,
        exact,
        mask: None,
    }))
}

/// A selection averaging more than one run per this many mask words goes
/// to the store as the mask rather than as ranges.
const MASK_RUN_WORDS: usize = 8;

/// Converts one decoded chunk into ready record batches: slices of
/// `DEFAULT_BATCH_ROWS`, each column adopted into its typed executor form.
/// Returns the batches plus the chunk's retained-byte figure to release.
fn adopt_chunk(
    chunk: ProjectedColumnChunk,
    types: &[pintail_types::DataType],
    enum_labels: &[Option<Arc<Vec<String>>>],
    set_members: &[Option<Arc<Vec<String>>>],
) -> Result<(Vec<RecordBatch>, usize), ExecError> {
    let chunk_bytes = chunk
        .retained_bytes()
        .saturating_sub(std::mem::size_of::<ProjectedColumnChunk>());
    let mut row_count = chunk.row_count();
    let mut columns = chunk.into_decoded_columns();
    if columns.len() != types.len() {
        return Err(ExecError::InvalidBatch(
            "stored column count differs from its snapshot schema",
        ));
    }
    let mut batches = Vec::with_capacity(row_count.div_ceil(DEFAULT_BATCH_ROWS));
    while row_count > 0 {
        // A chunk within the executor's ceiling passes through as one batch:
        // splitting it copied the remainder out of every column for nothing.
        let take = if row_count <= crate::batch::MAX_SCAN_BATCH_ROWS {
            row_count
        } else {
            row_count.min(DEFAULT_BATCH_ROWS)
        };
        let taken = columns
            .iter_mut()
            .map(|column| column.take_prefix(take))
            .collect::<Vec<_>>();
        if taken.iter().any(|column| column.len() != take) {
            return Err(ExecError::InvalidBatch(
                "stored column ended before its segment rows",
            ));
        }
        let vectors = types
            .iter()
            .copied()
            .zip(taken)
            .zip(enum_labels.iter().zip(set_members.iter()))
            .map(|((data_type, column), (labels, members))| {
                column_vector_from_decoded(data_type, column, labels.as_ref(), members.as_ref())
            })
            .collect::<Result<Vec<_>, _>>()?;
        batches.push(RecordBatch::new(take, vectors)?);
        row_count -= take;
    }
    Ok((batches, chunk_bytes))
}

/// Adopts one store-decoded column as a typed executor vector, parsing
/// text-carried decimals and temporals once from the arena; row values
/// Promotes a text value to an ENUM value carrying its declaration index.
/// A label absent from the declaration stays text: it has no index, and
/// inventing one would order it confidently and wrongly.
pub(crate) fn ordinal_value(
    value: pintail_types::Value,
    enum_labels: Option<&Arc<Vec<String>>>,
    set_members: Option<&Arc<Vec<String>>>,
) -> pintail_types::Value {
    let pintail_types::Value::Utf8(text) = &value else {
        return value;
    };
    let ordinal = if let Some(labels) = enum_labels {
        // Labels here are the catalog's complete declaration, so an empty
        // label may match honestly: ENUM('', ...) is legal MySQL and ''
        // there is a real member with a real ordinal. (Only reconstructed
        // tables need the gap guard; see StrColumn::enum_index_of.)
        labels
            .iter()
            .position(|declared| declared == text)
            .and_then(|position| u64::try_from(position + 1).ok())
    } else if let Some(members) = set_members {
        // A SET value is a comma-joined subset; its ordinal is the member
        // bitmask. Undeclared members refuse, like undeclared ENUM labels.
        let mut mask = Some(0_u64);
        for member in text.split(',').filter(|member| !member.is_empty()) {
            mask = mask.and_then(|mask| {
                members
                    .iter()
                    .position(|declared| declared == member)
                    .filter(|position| *position < 64)
                    .map(|position| mask | (1_u64 << position))
            });
        }
        mask
    } else {
        None
    };
    ordinal.map_or_else(
        || value.clone(),
        |index| pintail_types::Value::Enum {
            index,
            label: text.clone(),
        },
    )
}

/// Builds a scan column from row-shaped values: the memtable's rows, and
/// the rows of a part merged across versions.
///
/// An ENUM or SET column is built as text carrying the catalog's
/// declaration, exactly as a decoded segment column is. Handing over
/// `Value::Enum` rows instead left the declaration to be rebuilt from the
/// ordinals that batch happened to hold - a partial ENUM table, and for a
/// SET a table of whole values indexed by bitmask - and a consumer that
/// takes one batch's declaration for the whole scan (the grouping that
/// interns text keys does) then ordered every key that batch lacked as
/// plain text. A scan that streams a segment and memtable rows together
/// hands over both kinds of batch, so both must carry the same thing.
pub(crate) fn column_vector_from_values(
    data_type: pintail_types::DataType,
    values: Vec<pintail_types::Value>,
    enum_labels: Option<&Arc<Vec<String>>>,
    set_members: Option<&Arc<Vec<String>>>,
) -> Result<ColumnVector, ExecError> {
    let declared = enum_labels.is_some() || set_members.is_some();
    if declared
        && matches!(data_type, pintail_types::DataType::Utf8)
        && values.iter().all(|value| {
            matches!(
                value,
                pintail_types::Value::Null
                    | pintail_types::Value::Utf8(_)
                    | pintail_types::Value::Enum { .. }
            )
        })
    {
        fn text_of(value: &pintail_types::Value) -> Option<&str> {
            match value {
                pintail_types::Value::Utf8(text)
                | pintail_types::Value::Enum { label: text, .. } => Some(text.as_str()),
                _ => None,
            }
        }
        let mut text = StrColumn::with_capacity_for_lengths(
            values
                .iter()
                .map(|value| text_of(value).map_or(0, str::len)),
        );
        let mut validity = Vec::with_capacity(values.len());
        for value in &values {
            let label = text_of(value);
            validity.push(label.is_some());
            text.push(label.map_or(&[][..], str::as_bytes));
        }
        let text = text
            .with_enum_labels(enum_labels.map(Arc::clone))
            .with_set_members(set_members.map(Arc::clone));
        return Ok(ColumnVector::from_typed(
            data_type,
            TypedValues::Utf8(text),
            ValidityMask::from_bools(&validity),
        ));
    }
    let values = if declared {
        values
            .into_iter()
            .map(|value| ordinal_value(value, enum_labels, set_members))
            .collect()
    } else {
        values
    };
    ColumnVector::new(data_type, widen_decimal_values(data_type, values)).map_err(ExecError::from)
}

/// materialize lazily only if a row-shaped consumer asks. Falls back to
/// row values when the packed shape does not match the declared type.
///
/// A string column is built by three paths - dictionary templates, arena
/// materialization, and the generic value path - so ENUM labels are
/// attached at each. Missing one would make ordering depend on which
/// encoding the segment happened to use, which is worse than not ordering
/// at all.
#[allow(clippy::too_many_lines)]
pub(crate) fn column_vector_from_decoded(
    data_type: pintail_types::DataType,
    decoded: DecodedColumn,
    enum_labels: Option<&Arc<Vec<String>>>,
    set_members: Option<&Arc<Vec<String>>>,
) -> Result<ColumnVector, ExecError> {
    let storage = data_type.storage_type();
    match decoded {
        DecodedColumn::Values(values) => {
            column_vector_from_values(data_type, values, enum_labels, set_members)
        }
        DecodedColumn::Int64 { values, validity }
            if matches!(storage, pintail_types::DataType::Int64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::Int64(values),
                ValidityMask::from_column_validity(&validity),
            ))
        }
        DecodedColumn::UInt64 { values, validity }
            if matches!(storage, pintail_types::DataType::UInt64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::UInt64(values),
                ValidityMask::from_column_validity(&validity),
            ))
        }
        DecodedColumn::Float64 { bits, validity }
            if matches!(storage, pintail_types::DataType::Float64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::Float64(bits.into_iter().map(f64::from_bits).collect()),
                ValidityMask::from_column_validity(&validity),
            ))
        }
        DecodedColumn::Utf8 {
            heap,
            offsets,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            if let Some(values) = widened_decimal_arena(data_type, &heap, &offsets, &validity) {
                return ColumnVector::new(data_type, values).map_err(ExecError::from);
            }
            Ok(typed_from_utf8_arena_labelled(
                data_type,
                &heap,
                &offsets,
                &validity,
                enum_labels,
                set_members,
            ))
        }
        DecodedColumn::DictionaryUtf8 {
            dict_heap,
            dict_offsets,
            codes,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            let mask = ValidityMask::from_column_validity(&validity);
            if matches!(
                data_type,
                pintail_types::DataType::Utf8 | pintail_types::DataType::Json
            ) {
                // Straight to view templates: one 16-byte view per row,
                // dictionary bytes as the only heap.
                let column =
                    StrColumn::from_dictionary(&dict_heap, &dict_offsets, codes, mask.clone())
                        .with_enum_labels(enum_labels.map(Arc::clone))
                        .with_set_members(set_members.map(Arc::clone));
                return Ok(ColumnVector::from_typed(
                    data_type,
                    TypedValues::Utf8(column),
                    mask,
                ));
            }
            // Text-carried decimals/temporals under dictionary encoding are
            // rare (high-cardinality columns don't dictionary-encode);
            // materialize the arena and take the parsing path.
            let mut heap = Vec::new();
            let mut offsets = Vec::with_capacity(codes.len() + 1);
            offsets.push(0);
            for (code, valid) in codes.iter().zip(validity.iter()) {
                if valid {
                    let code = *code as usize;
                    heap.extend_from_slice(&dict_heap[dict_offsets[code]..dict_offsets[code + 1]]);
                }
                offsets.push(heap.len());
            }
            if let Some(values) = widened_decimal_arena(data_type, &heap, &offsets, &validity) {
                return ColumnVector::new(data_type, values).map_err(ExecError::from);
            }
            Ok(typed_from_utf8_arena_labelled(
                data_type,
                &heap,
                &offsets,
                &validity,
                enum_labels,
                set_members,
            ))
        }
        DecodedColumn::NativeUnits {
            units,
            values,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            // PTSEG v2 unit columns: the packed integers ARE the typed
            // representation — no text parse, and no text formatting either:
            // the carrier regenerates lazily only if a text-shaped consumer
            // (output, group keys) ever asks.
            // Whether the unit kind and the schema type agree is settled
            // before the packed integers are touched. Deciding it inside the
            // conversion meant the temporal arms had to clone them, because
            // the defensive arm below still needed them to rebuild the
            // column - so every temporal column copied its whole values
            // vector on a path that then threw the original away.
            let agrees = matches!(
                (units, data_type),
                (
                    pintail_store::NativeUnits::Decimal { .. },
                    pintail_types::DataType::Decimal { .. }
                ) | (
                    pintail_store::NativeUnits::Date,
                    pintail_types::DataType::Date32
                ) | (
                    pintail_store::NativeUnits::DateTime { .. },
                    pintail_types::DataType::DateTime64 { .. }
                )
            );
            if !agrees {
                // Unit kind and schema type disagree (defensive): fall back
                // to row values.
                return ColumnVector::new(
                    data_type,
                    DecodedColumn::NativeUnits {
                        units,
                        values,
                        validity,
                    }
                    .into_values(),
                )
                .map_err(ExecError::from);
            }
            let typed = match units {
                pintail_store::NativeUnits::Decimal { scale } => TypedValues::Decimal128 {
                    values: crate::batch::DecimalUnits::Narrow(values),
                    scale,
                    text: LazyText::decimal(scale),
                },
                pintail_store::NativeUnits::Date => TypedValues::Temporal {
                    units: values,
                    text: LazyText::date(),
                },
                pintail_store::NativeUnits::DateTime { fsp } => TypedValues::Temporal {
                    units: values,
                    text: LazyText::datetime(fsp),
                },
            };
            let mask = ValidityMask::from_column_validity(&validity);
            Ok(ColumnVector::from_typed(data_type, typed, mask))
        }
        decoded => ColumnVector::new(data_type, decoded.into_values()).map_err(ExecError::from),
    }
}

/// Builds the typed projection for a Utf8-carried column straight from the
/// decoded arena: plain strings become view columns; decimal and temporal
/// carriers additionally parse once into packed integers, mirroring
/// `build_typed`'s fallback to plain text if any non-null value fails.
fn typed_from_utf8_arena_labelled(
    data_type: pintail_types::DataType,
    heap: &[u8],
    offsets: &[usize],
    validity: &pintail_store::ColumnValidity,
    enum_labels: Option<&Arc<Vec<String>>>,
    set_members: Option<&Arc<Vec<String>>>,
) -> ColumnVector {
    let mut text =
        StrColumn::with_capacity_for_lengths(offsets.windows(2).map(|pair| pair[1] - pair[0]));
    for row in 0..validity.len() {
        text.push(&heap[offsets[row]..offsets[row + 1]]);
    }
    let text = text
        .with_enum_labels(enum_labels.map(Arc::clone))
        .with_set_members(set_members.map(Arc::clone));
    let mask = ValidityMask::from_column_validity(validity);
    let typed = match data_type {
        pintail_types::DataType::Decimal { scale, .. } => {
            let mut packed = Vec::with_capacity(validity.len());
            let mut homogeneous = true;
            for (row, valid) in validity.iter().enumerate() {
                if !valid {
                    packed.push(0);
                    continue;
                }
                let parsed = std::str::from_utf8(&heap[offsets[row]..offsets[row + 1]])
                    .ok()
                    .and_then(|value| parse_decimal_scaled(value, scale));
                if let Some(scaled) = parsed {
                    packed.push(scaled);
                } else {
                    homogeneous = false;
                    break;
                }
            }
            if homogeneous {
                TypedValues::Decimal128 {
                    values: crate::batch::DecimalUnits::Wide(packed),
                    scale,
                    text: LazyText::ready(text),
                }
            } else {
                TypedValues::Utf8(text)
            }
        }
        pintail_types::DataType::Date32 | pintail_types::DataType::DateTime64 { .. } => {
            let datetime = matches!(data_type, pintail_types::DataType::DateTime64 { .. });
            let mut units = Vec::with_capacity(validity.len());
            let mut homogeneous = true;
            for (row, valid) in validity.iter().enumerate() {
                if !valid {
                    units.push(0);
                    continue;
                }
                let parsed = std::str::from_utf8(&heap[offsets[row]..offsets[row + 1]])
                    .ok()
                    .and_then(|value| {
                        if datetime {
                            parse_datetime_micros(value)
                        } else {
                            parse_date_days(value)
                        }
                    });
                if let Some(value) = parsed {
                    units.push(value);
                } else {
                    homogeneous = false;
                    break;
                }
            }
            if homogeneous {
                TypedValues::Temporal {
                    units,
                    text: LazyText::ready(text),
                }
            } else {
                TypedValues::Utf8(text)
            }
        }
        _ => TypedValues::Utf8(text),
    };
    ColumnVector::from_typed(data_type, typed, mask)
}

fn prefetched_retained_bytes(chunks: &VecDeque<ProjectedColumnChunk>) -> usize {
    chunks
        .iter()
        .map(|chunk| {
            chunk
                .retained_bytes()
                .saturating_sub(std::mem::size_of_val(chunk))
        })
        .sum()
}

fn batch_memory_upper_bound(types: &[pintail_types::DataType], row_count: usize) -> usize {
    std::mem::size_of::<RecordBatch>()
        .saturating_add(types.len().saturating_mul(
            std::mem::size_of::<Vec<Value>>().saturating_add(std::mem::size_of::<ColumnVector>()),
        ))
        .saturating_add(
            types
                .len()
                .saturating_mul(row_count)
                .saturating_mul(std::mem::size_of::<Value>()),
        )
        .saturating_add(
            row_count
                .div_ceil(64)
                .saturating_mul(std::mem::size_of::<u64>()),
        )
}

/// Stored text of a DECIMAL column whose scale has since widened carries
/// fewer fraction digits than its type; it renders at the type's scale, as
/// the source's rebuilt table does after the ALTER. `None` for text that
/// already carries the scale, or for anything that is not a decimal.
fn widened_decimal_text(data_type: pintail_types::DataType, text: &str) -> Option<String> {
    let pintail_types::DataType::Decimal { scale, .. } = data_type else {
        return None;
    };
    let fraction = text
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    if fraction >= usize::from(scale) {
        return None;
    }
    pintail_types::parse_decimal_wide(text, scale)
        .map(|units| pintail_types::format_decimal_wide(&units, scale))
}

fn widen_decimal_values(data_type: pintail_types::DataType, values: Vec<Value>) -> Vec<Value> {
    if !matches!(data_type, pintail_types::DataType::Decimal { .. }) {
        return values;
    }
    values
        .into_iter()
        .map(|value| match value {
            Value::Utf8(text) => {
                Value::Utf8(widened_decimal_text(data_type, &text).unwrap_or(text))
            }
            other => other,
        })
        .collect()
}

/// [`widened_decimal_text`] over a stored text arena: the rows as values
/// when any of them needs widening, `None` when every one already carries
/// the type's scale - the common case, which keeps the packed path.
fn widened_decimal_arena(
    data_type: pintail_types::DataType,
    heap: &[u8],
    offsets: &[usize],
    validity: &pintail_store::ColumnValidity,
) -> Option<Vec<Value>> {
    if !matches!(data_type, pintail_types::DataType::Decimal { .. }) {
        return None;
    }
    let valid = validity.iter().collect::<Vec<_>>();
    let text =
        |row: usize| std::str::from_utf8(&heap[offsets[row]..offsets[row + 1]]).unwrap_or_default();
    if !(0..valid.len())
        .any(|row| valid[row] && widened_decimal_text(data_type, text(row)).is_some())
    {
        return None;
    }
    Some(
        (0..valid.len())
            .map(|row| {
                if valid[row] {
                    let text = text(row);
                    Value::Utf8(
                        widened_decimal_text(data_type, text).unwrap_or_else(|| text.to_owned()),
                    )
                } else {
                    Value::Null
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use crate::collation::Collation;
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, parse_statement};
    use pintail_store::{StoreOptions, TableStore};
    use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

    use crate::{
        ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, ScanProvider,
        SnapshotScanProvider, explain_analyze_statement,
    };

    /// The batch a scan plans takes its stated share of the budget and no
    /// more, whatever the column types, so the decode chunk beside it keeps
    /// three eighths: a tight ceiling must not depend on how large a
    /// column's in-memory header happens to be.
    #[test]
    fn a_planned_scan_batch_leaves_the_decode_chunk_its_share() {
        let layouts = [
            vec![DataType::UInt64],
            vec![DataType::UInt64, DataType::Int64, DataType::Utf8],
            vec![DataType::Utf8; 12],
        ];
        for types in &layouts {
            for budget in [256 * 1024, 12 * 1024 * 1024, 256 * 1024 * 1024] {
                let rows = super::planned_scan_rows(types, budget);
                let batch = super::batch_memory_upper_bound(types, rows);
                assert!(rows >= 1);
                assert!(
                    batch <= budget / super::SCAN_BATCH_SHARE,
                    "{} columns at {budget} bytes: a {rows}-row batch is {batch} bytes",
                    types.len()
                );
                // Conservative, since each planned row is charged a whole
                // selection word, but not wasteful: the batch still uses
                // most of its share.
                if rows > 1 && rows < crate::batch::DEFAULT_BATCH_ROWS {
                    assert!(
                        batch.saturating_mul(2) >= budget / super::SCAN_BATCH_SHARE,
                        "{} columns at {budget} bytes: a {rows}-row batch leaves most of its share",
                        types.len()
                    );
                }
            }
        }
    }

    fn execute_values(
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
    ) -> Vec<Value> {
        execute_values_with_limit(sql, catalog, provider, 64 * 1024)
    }

    fn execute_values_with_limit(
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
        memory_limit: usize,
    ) -> Vec<Value> {
        let statement = parse_statement(sql).expect("parse query");
        let bound = Binder::new(catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, memory_limit, Collation::default())
                .expect("start execution");
        let mut values = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
        {
            values.extend(batch.selection().selected_rows().map(|row| {
                batch
                    .column(0)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value")
            }));
        }
        values
    }

    #[test]
    fn streams_non_overlapping_snapshot_segments_under_the_query_cap() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        for start in [1_u64, 1001, 2001, 3001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| row(key, &format!("value-{key}")))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(4000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        assert_eq!(
            execute_values_with_limit(
                "SELECT COUNT(name) FROM events",
                &catalog,
                &provider,
                1024 * 1024,
            ),
            [Value::UInt64(4000)]
        );
    }

    /// A small table whose rows are still in the memtable was once
    /// materialized when its scan opened, and that row set took whatever
    /// budget the query had left. Under a tight ceiling it took nearly all of
    /// it: the scan beside it could not reserve its own few bytes, and a sort
    /// above a cross join failed before it could spill. The range streams
    /// now, as a large one does.
    #[test]
    fn a_materialized_memtable_scan_leaves_the_query_room_to_spill() {
        fn wide_scan(plan: &crate::LogicalPlan) -> Option<crate::Scan> {
            match plan {
                crate::LogicalPlan::Scan(scan) if scan.table.table_name == "events" => {
                    Some(scan.clone())
                }
                crate::LogicalPlan::Project { input, .. }
                | crate::LogicalPlan::Filter { input, .. }
                | crate::LogicalPlan::Derived { input, .. }
                | crate::LogicalPlan::Sort { input, .. } => wide_scan(input),
                crate::LogicalPlan::Join { left, right, .. } => {
                    wide_scan(left).or_else(|| wide_scan(right))
                }
                crate::LogicalPlan::CrossJoin { inputs } => inputs.iter().find_map(wide_scan),
                _ => None,
            }
        }
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut wide = TableStore::open(
            directory.path().join("wide"),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("open table");
        wide.ingest(
            (1..=3000_u64)
                .map(|id| row(id, &format!("a memtable row long enough to matter {id}")))
                .collect(),
        )
        .expect("ingest");
        let mut small = TableStore::open(
            directory.path().join("small"),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("open table");
        small
            .ingest((1..=20_u64).map(|id| row(id, "tag")).collect())
            .expect("ingest");
        let wide_snapshot = wide.snapshot();
        let small_snapshot = small.snapshot();

        let database_id = DatabaseId::new(19);
        let (wide_id, small_id) = (TableId::new(21), TableId::new(23));
        let entries = [
            TableEntry::new(
                wide_id,
                "events",
                schema.clone(),
                TableStatistics::with_row_count(3000),
            )
            .expect("table entry"),
            TableEntry::new(
                small_id,
                "tags",
                schema,
                TableStatistics::with_row_count(20),
            )
            .expect("table entry"),
        ];
        let database = DatabaseEntry::new(database_id, "app", entries).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider = SnapshotScanProvider::new([
            (database_id, wide_id, &wide_snapshot),
            (database_id, small_id, &small_snapshot),
        ])
        .expect("provider");

        let sql = "SELECT e.id, t.id FROM events e CROSS JOIN tags t ORDER BY t.id DESC, e.id";
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse query"))
            .expect("bind query");
        let plan = Optimizer::optimize(LogicalPlanner::plan(bound));
        let scan = wide_scan(&plan).expect("the plan scans the wide table");
        // The memtable's text alone; the open stream holds none of it, even
        // with room to spare.
        let text = 3000 * "a memtable row long enough to matter 1000".len();
        let held = provider
            .open_scan(&scan, usize::MAX)
            .expect("open scan")
            .retained_bytes();
        assert!(
            held <= text / 4,
            "a scan of {text} bytes of memtable text holds {held} once open"
        );
        // The streamed range still answers the query under a budget about
        // the size of its rows, spilling the sort.
        let values = execute_values_with_limit(sql, &catalog, &provider, text * 2);
        assert_eq!(values.len(), 60_000);
        assert_eq!(values[0], Value::UInt64(1));
        assert_eq!(values[59_999], Value::UInt64(3000));
    }

    /// The streaming path learns its block counters only as chunks are pulled,
    /// so `open_scan` cannot record them: it knows the segment counts and
    /// nothing else. Before the stream folded each chunk's counters back into
    /// the provider, every streamed scan reported `actual_blocks=0/0` while
    /// reading thousands of rows, which made the pruning numbers unusable for
    /// deciding whether a plan change helped.
    ///
    /// The predicate is load-bearing: a bare `COUNT` over a settled snapshot is
    /// answered from the segment summaries without pulling a single chunk, so
    /// it reports zero blocks *correctly* and would not exercise this at all.
    #[test]
    fn a_streamed_scan_reports_the_blocks_it_actually_read() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        for start in [1_u64, 1001, 2001, 3001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| row(key, &format!("value-{key}")))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(4000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        assert_eq!(
            execute_values_with_limit(
                "SELECT COUNT(name) FROM events WHERE name > 'value-1'",
                &catalog,
                &provider,
                1024 * 1024,
            ),
            [Value::UInt64(3999)]
        );

        let stats = provider
            .scan_stats(database_id, table_id)
            .expect("physical scan stats");
        assert_eq!(stats.segments_read, 4, "all four segments carry the count");
        assert!(
            stats.blocks_read > 0,
            "a scan of 4000 rows must report the blocks it read, got {stats:?}"
        );
        assert!(
            stats.blocks_decoded > 0,
            "the counted column is decoded, got {stats:?}"
        );
        assert!(
            stats.blocks_read >= stats.blocks_decoded,
            "a block cannot decode without being read, got {stats:?}"
        );
    }

    /// The defects a robustness review found in the window and aggregate
    /// work. Each of these bound or answered wrongly before the fix.
    #[test]
    fn reviewed_window_and_aggregate_defects_are_repaired() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest((1..=5_u64).map(|id| row(id, "value")).collect())
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(71);
        let table_id = TableId::new(73);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let u = Value::UInt64;
        // FIRST_VALUE/LAST_VALUE read the frame, so a frame on them binds.
        assert_eq!(
            execute_values_with_limit(
                "SELECT LAST_VALUE(id) OVER (ORDER BY id \
                 ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM events",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![u(5), u(5), u(5), u(5), u(5)]
        );
        // A named window nested inside a scalar function resolves. The cast
        // keeps COALESCE's own type unification out of the assertion.
        assert_eq!(
            execute_values_with_limit(
                "SELECT COALESCE(LAG(id) OVER w, CAST(0 AS UNSIGNED)) FROM events \
                 WINDOW w AS (ORDER BY id)",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![u(0), u(1), u(2), u(3), u(4)]
        );
        // NTILE with more buckets than rows terminates promptly.
        assert_eq!(
            execute_values_with_limit(
                "SELECT NTILE(18446744073709551615) OVER (ORDER BY id) FROM events",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![u(1), u(2), u(3), u(4), u(5)]
        );
    }

    /// An inverted frame must reject rather than answer NULL for every row.
    #[test]
    fn an_inverted_window_frame_rejects() {
        let statement = parse_statement(
            "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING \
                 AND 1 PRECEDING) FROM events",
        )
        .expect("parse");
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(75);
        let table_id = TableId::new(77);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(0),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let _ = &snapshot;
        assert!(
            Binder::new(&catalog, Some("app")).bind(&statement).is_err(),
            "a frame whose start follows its end must not bind"
        );
    }

    /// The same aggregate must answer identically whether it runs against a
    /// memtable, a settled snapshot that engages the SMA fold, or a spill.
    ///
    /// This is the gap that hid a wrong answer: the SMA fold only engages
    /// for a settled snapshot, and every oracle case runs against freshly
    /// ingested rows still in the memtable, so nothing ever entered that
    /// path. A disagreement between paths is a defect even when both look
    /// plausible on their own.
    #[test]
    fn aggregates_agree_across_memtable_settled_and_spilled_paths() {
        let queries = [
            "SELECT COUNT(id) FROM events",
            "SELECT SUM(id) FROM events",
            "SELECT MIN(id) FROM events",
            "SELECT MAX(id) FROM events",
            "SELECT VAR_POP(id) FROM events",
            "SELECT STDDEV_POP(id) FROM events",
            "SELECT BIT_AND(id) FROM events",
            "SELECT BIT_OR(id) FROM events",
            "SELECT BIT_XOR(id) FROM events",
            "SELECT COUNT(*) FROM events",
        ];

        let answers = |flush: bool, memory: usize| -> Vec<Vec<Value>> {
            let directory = tempfile::tempdir().expect("temporary table");
            let schema = schema();
            let mut table =
                TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
                    .expect("open table");
            table
                .ingest((1..=64_u64).map(|id| row(id, "value")).collect())
                .expect("ingest");
            if flush {
                // A flushed, checkpointed table leaves the rows in segments
                // with an empty memtable — the shape the SMA fold requires.
                table.flush().expect("flush");
                table.checkpoint().expect("checkpoint");
            }
            let snapshot = table.snapshot();
            let database_id = DatabaseId::new(81);
            let table_id = TableId::new(83);
            let entry = TableEntry::new(
                table_id,
                "events",
                schema,
                TableStatistics::with_row_count(64),
            )
            .expect("table entry");
            let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
            let catalog = CatalogSnapshot::new([database]).expect("catalog");
            let provider =
                SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
            queries
                .iter()
                .map(|sql| execute_values_with_limit(sql, &catalog, &provider, memory))
                .collect()
        };

        let memtable = answers(false, 64 * 1024 * 1024);
        let settled = answers(true, 64 * 1024 * 1024);
        // A ceiling this tight forces the spilling operators onto disk.
        let spilled = answers(true, 256 * 1024);

        for (index, sql) in queries.iter().enumerate() {
            assert_eq!(
                memtable[index], settled[index],
                "memtable and settled snapshot disagree on {sql}"
            );
            assert_eq!(
                memtable[index], spilled[index],
                "memtable and spilled execution disagree on {sql}"
            );
        }
        // Guard the guard: a fold that silently answered from empty state
        // would return NULL here and match nothing real.
        assert_eq!(memtable[0], vec![Value::UInt64(64)], "COUNT sanity");
        assert_ne!(memtable[4], vec![Value::Null], "VAR_POP must not be NULL");
    }

    /// A named window resolves to its definition before binding.
    #[test]
    fn named_windows_resolve_to_their_definition() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest((1..=5_u64).map(|id| row(id, "value")).collect())
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(61);
        let table_id = TableId::new(63);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let u = Value::UInt64;
        let d = |sum: u64| Value::Utf8(sum.to_string());
        // The named window carries the same running frame as the inline form.
        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER w FROM events \
                 WINDOW w AS (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![d(1), d(3), d(6), d(10), d(15)]
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER rolling FROM events \
                 WINDOW base AS (PARTITION BY name), \
                 ordered AS (base ORDER BY id), \
                 rolling AS (ordered ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![d(1), d(3), d(6), d(10), d(15)]
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT ROW_NUMBER() OVER w FROM events WINDOW w AS (ORDER BY id)",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![u(1), u(2), u(3), u(4), u(5)]
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER (w ORDER BY id \
                 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM events \
                 WINDOW w AS (PARTITION BY name)",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            vec![d(1), d(3), d(6), d(10), d(15)]
        );
        let statement = parse_statement(
            "SELECT SUM(id) OVER (w ORDER BY id) FROM events \
             WINDOW w AS (ORDER BY name)",
        )
        .expect("parse illegal named-window redefinition");
        assert!(Binder::new(&catalog, Some("app")).bind(&statement).is_err());
        let statement = parse_statement(
            "SELECT SUM(id) OVER (w) FROM events \
             WINDOW w AS (ROWS UNBOUNDED PRECEDING)",
        )
        .expect("parse illegal frame inheritance");
        assert!(Binder::new(&catalog, Some("app")).bind(&statement).is_err());
        let statement =
            parse_statement("SELECT SUM(id) OVER a FROM events WINDOW a AS (b), b AS (a)")
                .expect("parse cyclic windows");
        assert!(Binder::new(&catalog, Some("app")).bind(&statement).is_err());
    }

    /// Explicit ROWS frames: the running total and the moving window that
    /// every dashboard needs. Without the frame plumbed through evaluation
    /// these would silently compute `MySQL`'s default frame instead.
    #[test]
    fn explicit_rows_frames_bound_the_aggregate() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest((1..=5_u64).map(|id| row(id, "value")).collect())
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(51);
        let table_id = TableId::new(53);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let u = Value::UInt64;
        let d = |sum: u64| Value::Utf8(sum.to_string());
        for (sql, expected) in [
            // Running total: 1, 3, 6, 10, 15.
            (
                "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
                 FROM events",
                vec![d(1), d(3), d(6), d(10), d(15)],
            ),
            // Moving window of three: 1, 3, 6, 9, 12.
            (
                "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) \
                 FROM events",
                vec![d(1), d(3), d(6), d(9), d(12)],
            ),
            // Shorthand form means the same as ... AND CURRENT ROW.
            (
                "SELECT SUM(id) OVER (ORDER BY id ROWS 2 PRECEDING) FROM events",
                vec![d(1), d(3), d(6), d(9), d(12)],
            ),
            // Centred window: 3, 6, 9, 12, 9.
            (
                "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
                 FROM events",
                vec![d(3), d(6), d(9), d(12), d(9)],
            ),
            // Whole partition regardless of position.
            (
                "SELECT SUM(id) OVER (ORDER BY id \
                 ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM events",
                vec![d(15), d(15), d(15), d(15), d(15)],
            ),
            // Strictly future rows: 14, 12, 9, 5, NULL (empty frame).
            (
                "SELECT SUM(id) OVER (ORDER BY id \
                 ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) FROM events",
                vec![d(14), d(12), d(9), d(5), Value::Null],
            ),
            // COUNT over a bounded frame counts framed rows only.
            (
                "SELECT COUNT(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
                 FROM events",
                vec![u(1), u(2), u(2), u(2), u(2)],
            ),
            // MIN/MAX cannot be un-accumulated; the sliding path recomputes.
            (
                "SELECT MAX(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
                 FROM events",
                vec![u(1), u(2), u(3), u(4), u(5)],
            ),
        ] {
            assert_eq!(
                execute_values_with_limit(sql, &catalog, &provider, 4 * 1024 * 1024),
                expected,
                "{sql}"
            );
        }
    }

    #[test]
    fn numeric_range_offsets_frame_by_order_values_not_row_positions() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(
                [1_u64, 2, 10, 11, 20]
                    .into_iter()
                    .map(|id| row(id, "value"))
                    .collect(),
            )
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(61);
        let table_id = TableId::new(63);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER (ORDER BY id RANGE BETWEEN 2 PRECEDING AND CURRENT ROW) \
                 FROM events",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            [1_u64, 3, 10, 21, 20].map(|sum| Value::Utf8(sum.to_string()))
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER (ORDER BY id RANGE BETWEEN 0.5 PRECEDING AND CURRENT ROW) \
                 FROM events",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            [1_u64, 2, 10, 11, 20].map(|sum| Value::Utf8(sum.to_string()))
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT SUM(id) OVER (ORDER BY id DESC RANGE BETWEEN 2 PRECEDING AND 1 FOLLOWING), \
                 id FROM events ORDER BY id DESC",
                &catalog,
                &provider,
                4 * 1024 * 1024,
            ),
            [20_u64, 21, 21, 3, 3].map(|sum| Value::Utf8(sum.to_string()))
        );
    }

    /// The offset window functions, including the `LAST_VALUE` subtlety: under
    /// `MySQL`'s default frame it reads the last row of the current PEER
    /// GROUP, so with a unique ORDER BY key every row returns its own value
    /// rather than the partition's last.
    #[test]
    fn offset_window_functions_read_positionally() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest((1..=5_u64).map(|id| row(id, "value")).collect())
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(41);
        let table_id = TableId::new(43);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let u = Value::UInt64;
        let d = |sum: u64| Value::Utf8(sum.to_string());
        for (sql, expected) in [
            (
                "SELECT LAG(id) OVER (ORDER BY id) FROM events",
                vec![Value::Null, u(1), u(2), u(3), u(4)],
            ),
            (
                "SELECT LEAD(id) OVER (ORDER BY id) FROM events",
                vec![u(2), u(3), u(4), u(5), Value::Null],
            ),
            (
                "SELECT LAG(id, 2, 0) OVER (ORDER BY id) FROM events",
                vec![u(0), u(0), u(1), u(2), u(3)],
            ),
            // 5 rows into 2 buckets: the remainder goes to the earlier one.
            (
                "SELECT NTILE(2) OVER (ORDER BY id) FROM events",
                vec![u(1), u(1), u(1), u(2), u(2)],
            ),
            (
                "SELECT FIRST_VALUE(id) OVER (ORDER BY id) FROM events",
                vec![u(1), u(1), u(1), u(1), u(1)],
            ),
            (
                "SELECT LAST_VALUE(id) OVER (ORDER BY id) FROM events",
                vec![u(1), u(2), u(3), u(4), u(5)],
            ),
            // Without ORDER BY the frame is the whole partition, so
            // LAST_VALUE really is the partition's last row.
            (
                "SELECT LAST_VALUE(id) OVER () FROM events",
                vec![u(5), u(5), u(5), u(5), u(5)],
            ),
            // An explicit RANGE frame with offsetless bounds is the default
            // frame written out, so it must equal the running total.
            (
                "SELECT SUM(id) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING \
                 AND CURRENT ROW) FROM events",
                vec![d(1), d(3), d(6), d(10), d(15)],
            ),
            (
                "SELECT SUM(id) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING) FROM events",
                vec![d(15), d(15), d(15), d(15), d(15)],
            ),
        ] {
            assert_eq!(
                execute_values_with_limit(sql, &catalog, &provider, 4 * 1024 * 1024),
                expected,
                "{sql}"
            );
        }
    }

    #[test]
    fn executes_queries_against_pinned_storage_snapshots() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![row(1, "alpha"), row(2, "Beta"), row(3, "gamma")])
            .expect("ingest");
        let snapshot = table.snapshot();

        let database_id = DatabaseId::new(5);
        let table_id = TableId::new(7);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(3),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let statement =
            parse_statement("SELECT name FROM events WHERE id >= 2").expect("parse query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("execution");

        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .selection()
            .selected_rows()
            .map(|row| {
                batch
                    .column(0)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Utf8("Beta".to_owned()),
                Value::Utf8("gamma".to_owned())
            ]
        );
        assert!(execution.next_batch().expect("end").is_none());

        let statement = parse_statement(
            "WITH recent AS (\
               SELECT id, name AS label FROM events WHERE id >= 2\
             ) \
             SELECT label FROM recent WHERE id <= 2",
        )
        .expect("parse CTE");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind CTE");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("CTE physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("CTE execution");
        let batch = execution
            .next_batch()
            .expect("CTE pull")
            .expect("CTE result");
        assert_eq!(batch.visible_row_count(), 1);
        let row = batch
            .selection()
            .selected_rows()
            .next()
            .expect("selected CTE row");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(row)),
            Some(&Value::Utf8("Beta".to_owned()))
        );
        assert!(execution.next_batch().expect("CTE end").is_none());
    }

    #[test]
    fn opt_in_unique_visibility_hides_the_lower_version_collision() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "email", DataType::Utf8, false),
            ],
        )
        .expect("collision schema");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open collision table");
        let collision_row = |id, email: &str, version| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("collision key"),
                vec![Value::UInt64(id), Value::Utf8(email.to_owned())],
                version,
                false,
            )
        };
        table
            .ingest(vec![
                collision_row(1, "User@Example.com", 1),
                collision_row(2, "user@example.com", 2),
            ])
            .expect("ingest collision");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "collisions",
            schema,
            TableStatistics::with_row_count(2),
        )
        .expect("collision catalog table");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("collision database");
        let catalog = CatalogSnapshot::new([database]).expect("collision catalog");
        let mut provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
        provider
            .enable_unique_visibility_policy(database_id, table_id, vec![vec![2]])
            .expect("enable unique visibility");

        assert_eq!(
            execute_values("SELECT id FROM collisions ORDER BY id", &catalog, &provider),
            [Value::UInt64(2)]
        );
    }

    /// A descending limit by the key tells the scan to keep the last rows
    /// in key order. Unique-key visibility reads the range as a row set,
    /// and that path has to keep the same end the stream would.
    #[test]
    fn unique_visibility_keeps_the_last_rows_for_a_descending_key_limit() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "token", DataType::Int64, false),
            ],
        )
        .expect("token schema");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open token table");
        table
            .ingest(
                (1..=5_u64)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("token key"),
                            vec![Value::UInt64(id), Value::Int64(100 + id.cast_signed())],
                            id,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest tokens");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(18);
        let entry = TableEntry::new(
            table_id,
            "tokens",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("token catalog table")
        .with_key_columns([1])
        .expect("key columns");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("token database");
        let catalog = CatalogSnapshot::new([database]).expect("token catalog");
        let mut provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
        provider
            .enable_unique_visibility_policy(database_id, table_id, vec![vec![2]])
            .expect("enable unique visibility");

        let ids = |values: &[u64]| {
            values
                .iter()
                .map(|id| Value::UInt64(*id))
                .collect::<Vec<_>>()
        };
        for (sql, expected) in [
            ("SELECT id FROM tokens ORDER BY id DESC LIMIT 1", ids(&[5])),
            (
                "SELECT id FROM tokens ORDER BY id DESC LIMIT 2",
                ids(&[5, 4]),
            ),
            (
                "SELECT id FROM tokens ORDER BY id DESC LIMIT 1 OFFSET 1",
                ids(&[4]),
            ),
            (
                "SELECT id FROM tokens ORDER BY id DESC LIMIT 2 OFFSET 3",
                ids(&[2, 1]),
            ),
            (
                "SELECT id FROM tokens ORDER BY id DESC LIMIT 9",
                ids(&[5, 4, 3, 2, 1]),
            ),
            (
                "SELECT id FROM tokens WHERE token > 101 ORDER BY id DESC LIMIT 2",
                ids(&[5, 4]),
            ),
            ("SELECT id FROM tokens ORDER BY id LIMIT 2", ids(&[1, 2])),
        ] {
            assert_eq!(execute_values(sql, &catalog, &provider), expected, "{sql}");
        }
    }

    #[test]
    fn supports_zero_column_scans_for_constant_per_row_results() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![row(1, "alpha"), row(2, "Beta")])
            .expect("ingest");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(5);
        let table_id = TableId::new(7);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(2),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let statement = parse_statement("SELECT 1 FROM events").expect("parse query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(batch.visible_row_count(), 2);
        assert_eq!(
            batch.column(0).expect("constant column").values(),
            [Value::Int64(1), Value::Int64(1)]
        );
    }

    #[test]
    fn reports_selective_primary_key_block_pruning() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let options = StoreOptions {
            block_rows: 2,
            ..StoreOptions::default()
        };
        let mut table =
            TableStore::open(directory.path(), schema.clone(), options).expect("open table");
        table
            .ingest((1..=4).map(|id| row(id, &format!("event-{id}"))).collect())
            .expect("first ingest");
        table.flush().expect("first flush");
        table
            .ingest((5..=8).map(|id| row(id, &format!("event-{id}"))).collect())
            .expect("second ingest");
        table.flush().expect("second flush");
        let snapshot = table.snapshot();

        let database_id = DatabaseId::new(5);
        let table_id = TableId::new(7);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(8),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("key columns");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let statement =
            parse_statement("SELECT name FROM events WHERE id BETWEEN 5 AND 6 ORDER BY name")
                .expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("execution");
        let batch = execution.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).expect("names").values(),
            [
                Value::Utf8("event-5".to_owned()),
                Value::Utf8("event-6".to_owned()),
            ]
        );
        assert!(execution.next_batch().expect("end").is_none());

        let stats = provider
            .scan_stats(database_id, table_id)
            .expect("physical scan stats");
        assert_eq!(stats.segments_read, 1);
        assert_eq!(stats.segments_pruned, 1);
        assert_eq!(stats.segments_total(), 2);
        assert_eq!(stats.blocks_read, 1);
        assert_eq!(stats.blocks_pruned, 1);
        assert_eq!(stats.blocks_total(), 2);

        let analyze_provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
        let statement = parse_statement(
            "EXPLAIN ANALYZE \
             SELECT name FROM events WHERE id BETWEEN 5 AND 6 ORDER BY name",
        )
        .expect("parse analyze");
        let explanation = explain_analyze_statement(
            &statement,
            &catalog,
            Some("app"),
            &analyze_provider,
            64 * 1024,
        )
        .expect("analyze");
        assert!(explanation.contains("actual_segments=1/2"));
        assert!(explanation.contains("actual_blocks=1/2"));
        assert!(explanation.contains("Spill files=0 bytes=0 active_bytes=0"));
    }

    #[test]
    fn key_pruning_requires_an_exact_declared_numeric_mapping() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![row(1, "alpha"), row(2, "Beta"), row(3, "gamma")])
            .expect("ingest");
        table.flush().expect("flush");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(5);
        let table_id = TableId::new(7);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(3),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("physical key mapping");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        assert_eq!(
            execute_values(
                "SELECT name FROM events WHERE id = '2'",
                &catalog,
                &provider
            ),
            [Value::Utf8("Beta".to_owned())]
        );
    }

    #[test]
    fn text_key_predicates_do_not_use_bytewise_storage_pruning() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = TableSchema::new(1, vec![Column::new(1, "name", DataType::Utf8, false)])
            .expect("schema");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::Utf8("Alpha".to_owned())]).expect("key"),
                    vec![Value::Utf8("Alpha".to_owned())],
                    1,
                    false,
                ),
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::Utf8("alpha".to_owned())]).expect("key"),
                    vec![Value::Utf8("alpha".to_owned())],
                    2,
                    false,
                ),
            ])
            .expect("ingest");
        table.flush().expect("flush");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(5);
        let table_id = TableId::new(7);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(2),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("physical key mapping");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let mut values = execute_values(
            "SELECT name FROM events WHERE name = 'alpha'",
            &catalog,
            &provider,
        );
        values.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        assert_eq!(
            values,
            [
                Value::Utf8("Alpha".to_owned()),
                Value::Utf8("alpha".to_owned())
            ]
        );
    }

    #[test]
    fn executes_left_semi_and_anti_hash_joins() {
        let events_directory = tempfile::tempdir().expect("events directory");
        let users_directory = tempfile::tempdir().expect("users directory");
        let events_schema = schema();
        let users_schema = signed_schema();
        let mut events = TableStore::open(
            events_directory.path(),
            events_schema.clone(),
            StoreOptions::default(),
        )
        .expect("open events");
        let mut users = TableStore::open(
            users_directory.path(),
            users_schema.clone(),
            StoreOptions::default(),
        )
        .expect("open users");
        events
            .ingest(vec![
                row(1, "event-a"),
                row(2, "event-b"),
                row(3, "event-c"),
            ])
            .expect("ingest events");
        users
            .ingest(vec![signed_row(2, "user-b")])
            .expect("ingest users");
        let events_snapshot = events.snapshot();
        let users_snapshot = users.snapshot();
        let database_id = DatabaseId::new(5);
        let events_id = TableId::new(7);
        let users_id = TableId::new(8);
        let database = DatabaseEntry::new(
            database_id,
            "app",
            [
                TableEntry::new(
                    events_id,
                    "events",
                    events_schema,
                    TableStatistics::with_row_count(3),
                )
                .expect("events entry"),
                TableEntry::new(
                    users_id,
                    "users",
                    users_schema,
                    TableStatistics::with_row_count(1),
                )
                .expect("users entry"),
            ],
        )
        .expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider = SnapshotScanProvider::new([
            (database_id, events_id, &events_snapshot),
            (database_id, users_id, &users_snapshot),
        ])
        .expect("provider");

        assert_eq!(
            execute_values(
                "SELECT events.name FROM events LEFT SEMI JOIN users \
                 ON events.id = users.id ORDER BY events.name",
                &catalog,
                &provider,
            ),
            [Value::Utf8("event-b".to_owned())]
        );
        assert_eq!(
            execute_values(
                "SELECT events.name FROM events LEFT ANTI JOIN users \
                 ON events.id = users.id ORDER BY events.name",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("event-a".to_owned()),
                Value::Utf8("event-c".to_owned())
            ]
        );
    }

    #[test]
    fn correlated_scalar_lookup_errors_when_one_outer_row_matches_twice() {
        let events_directory = tempfile::tempdir().expect("events directory");
        let users_directory = tempfile::tempdir().expect("users directory");
        let events_schema = schema();
        let users_schema = signed_schema();
        let mut events = TableStore::open(
            events_directory.path(),
            events_schema.clone(),
            StoreOptions::default(),
        )
        .expect("open events");
        let mut users = TableStore::open(
            users_directory.path(),
            users_schema.clone(),
            StoreOptions::default(),
        )
        .expect("open users");
        events
            .ingest(vec![row(1, "duplicate")])
            .expect("ingest event");
        users
            .ingest(vec![signed_row(1, "duplicate"), signed_row(2, "duplicate")])
            .expect("ingest duplicate lookup values");
        let events_snapshot = events.snapshot();
        let users_snapshot = users.snapshot();

        let database_id = DatabaseId::new(5);
        let events_id = TableId::new(7);
        let users_id = TableId::new(8);
        let database = DatabaseEntry::new(
            database_id,
            "app",
            [
                TableEntry::new(
                    events_id,
                    "events",
                    events_schema,
                    TableStatistics::with_row_count(1),
                )
                .expect("events entry"),
                TableEntry::new(
                    users_id,
                    "users",
                    users_schema,
                    TableStatistics::with_row_count(2),
                )
                .expect("users entry")
                .with_key_columns([1])
                .expect("users key"),
            ],
        )
        .expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider = SnapshotScanProvider::new([
            (database_id, events_id, &events_snapshot),
            (database_id, users_id, &users_snapshot),
        ])
        .expect("provider");
        let statement = parse_statement(
            "SELECT (SELECT id FROM users WHERE users.name = events.name) FROM events",
        )
        .expect("parse correlated scalar lookup");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind guarded scalar lookup");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("scalar lookup plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("scalar lookup execution");
        assert!(matches!(
            execution.next_batch(),
            Err(ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn executes_cross_joins_mixed_numeric_hash_joins_and_subqueries() {
        let events_directory = tempfile::tempdir().expect("events directory");
        let users_directory = tempfile::tempdir().expect("users directory");
        let schema = schema();
        let users_schema = signed_schema();
        let mut events = TableStore::open(
            events_directory.path(),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("open events");
        let mut users = TableStore::open(
            users_directory.path(),
            users_schema.clone(),
            StoreOptions::default(),
        )
        .expect("open users");
        events
            .ingest(vec![row(1, "event-a"), row(2, "event-b")])
            .expect("ingest events");
        users
            .ingest(vec![signed_row(1, "user-a"), signed_row(2, "user-b")])
            .expect("ingest users");
        let events_snapshot = events.snapshot();
        let users_snapshot = users.snapshot();

        let database_id = DatabaseId::new(5);
        let events_id = TableId::new(7);
        let users_id = TableId::new(8);
        let database = DatabaseEntry::new(
            database_id,
            "app",
            [
                TableEntry::new(
                    events_id,
                    "events",
                    schema.clone(),
                    TableStatistics::with_row_count(2),
                )
                .expect("events entry"),
                TableEntry::new(
                    users_id,
                    "users",
                    users_schema,
                    TableStatistics::with_row_count(2),
                )
                .expect("users entry")
                .with_key_columns([1])
                .expect("users key"),
            ],
        )
        .expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider = SnapshotScanProvider::new([
            (database_id, events_id, &events_snapshot),
            (database_id, users_id, &users_snapshot),
        ])
        .expect("provider");

        let statement = parse_statement(
            "SELECT events.name AS event_name, users.name AS user_name \
             FROM events, users LIMIT 3",
        )
        .expect("parse query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let rows = batch
            .selection()
            .selected_rows()
            .map(|row| {
                (
                    batch
                        .column(0)
                        .and_then(|column| column.value(row))
                        .cloned()
                        .expect("event"),
                    batch
                        .column(1)
                        .and_then(|column| column.value(row))
                        .cloned()
                        .expect("user"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (
                    Value::Utf8("event-a".to_owned()),
                    Value::Utf8("user-a".to_owned())
                ),
                (
                    Value::Utf8("event-a".to_owned()),
                    Value::Utf8("user-b".to_owned())
                ),
                (
                    Value::Utf8("event-b".to_owned()),
                    Value::Utf8("user-a".to_owned())
                )
            ]
        );
        assert!(execution.next_batch().expect("end").is_none());

        let statement = parse_statement(
            "SELECT events.name AS event_name, users.name AS user_name \
             FROM (events INNER JOIN users ON events.id = users.id)",
        )
        .expect("parse hash join");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind hash join");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("hash plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("hash execution");
        // No ORDER BY, so the row order is the join's own and moves with
        // how its inputs arrive under this small a budget: compare the set.
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("hash pull") {
            rows.extend(batch.selection().selected_rows().map(|row| {
                (
                    batch.column(0).expect("event").value(row).cloned(),
                    batch.column(1).expect("user").value(row).cloned(),
                )
            }));
        }
        rows.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        assert_eq!(
            rows,
            [
                (
                    Some(Value::Utf8("event-a".to_owned())),
                    Some(Value::Utf8("user-a".to_owned()))
                ),
                (
                    Some(Value::Utf8("event-b".to_owned())),
                    Some(Value::Utf8("user-b".to_owned()))
                )
            ]
        );

        assert_eq!(
            execute_rows(
                "SELECT e.id, u.name, marker.name FROM events e \
                 LEFT JOIN (users u LEFT JOIN \
                   (SELECT 1 AS id, 'flag' AS name) marker ON marker.id = u.id) \
                 ON u.id = e.id ORDER BY e.id",
                &catalog,
                &provider,
            ),
            [
                vec![
                    Value::UInt64(1),
                    Value::Utf8("user-a".to_owned()),
                    Value::Utf8("flag".to_owned()),
                ],
                vec![
                    Value::UInt64(2),
                    Value::Utf8("user-b".to_owned()),
                    Value::Null,
                ],
            ]
        );
        assert_eq!(
            execute_rows(
                "SELECT e.id, u.name FROM events e LEFT JOIN users u \
                 ON u.id = e.id AND EXISTS \
                    (SELECT 1 FROM users probe WHERE probe.id > u.id) \
                 ORDER BY e.id",
                &catalog,
                &provider,
            ),
            [
                vec![Value::UInt64(1), Value::Utf8("user-a".to_owned())],
                vec![Value::UInt64(2), Value::Null],
            ]
        );
        assert_eq!(
            execute_rows(
                "SELECT e.id, u.name FROM events e JOIN users u \
                 ON u.id = e.id AND EXISTS \
                    (SELECT 1 FROM users probe WHERE probe.id = 1) \
                 ORDER BY e.id",
                &catalog,
                &provider,
            ),
            [
                vec![Value::UInt64(1), Value::Utf8("user-a".to_owned())],
                vec![Value::UInt64(2), Value::Utf8("user-b".to_owned())],
            ]
        );
        assert_eq!(
            execute_values(
                "SELECT 1 AS n UNION SELECT 1 UNION ALL SELECT 1 ORDER BY n",
                &catalog,
                &provider,
            ),
            [Value::Int64(1), Value::Int64(1)]
        );
        assert_eq!(
            execute_values(
                "(SELECT 2 AS n ORDER BY n DESC LIMIT 1) \
                 UNION ALL SELECT 9 AS n ORDER BY n",
                &catalog,
                &provider,
            ),
            [Value::Int64(2), Value::Int64(9)]
        );
        assert_eq!(
            execute_values(
                "SELECT 1 AS n EXCEPT SELECT 1 UNION ALL SELECT 2 ORDER BY n",
                &catalog,
                &provider,
            ),
            [Value::Int64(2)]
        );
        assert_eq!(
            execute_values(
                "SELECT 1 AS n UNION ALL SELECT 2 INTERSECT SELECT 2 ORDER BY n",
                &catalog,
                &provider,
            ),
            [Value::Int64(1), Value::Int64(2)]
        );
        assert_eq!(
            execute_values(
                "SELECT 1 AS n UNION ALL (SELECT 2 UNION SELECT 2) ORDER BY n",
                &catalog,
                &provider,
            ),
            [Value::Int64(1), Value::Int64(2)]
        );

        let statement = parse_statement(
            "WITH named_events AS (SELECT id, name AS event_name FROM events) \
             SELECT named_events.event_name, users.name \
             FROM named_events INNER JOIN users ON named_events.id = users.id \
             ORDER BY named_events.event_name",
        )
        .expect("parse CTE join");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind CTE join");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("CTE join plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("CTE join execution");
        let batch = execution
            .next_batch()
            .expect("CTE join pull")
            .expect("CTE join batch");
        assert_eq!(
            batch.column(0).expect("events").values(),
            [
                Value::Utf8("event-a".to_owned()),
                Value::Utf8("event-b".to_owned()),
            ]
        );
        assert_eq!(
            batch.column(1).expect("users").values(),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );

        let statement = parse_statement(
            "SELECT events.id, \
             (SELECT users.name FROM users WHERE users.id = events.id) AS user_name \
             FROM events ORDER BY events.id",
        )
        .expect("parse correlated scalar lookup");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind correlated scalar lookup");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("correlated scalar lookup plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("correlated scalar lookup execution");
        let batch = execution
            .next_batch()
            .expect("correlated scalar lookup pull")
            .expect("correlated scalar lookup batch");
        assert_eq!(
            batch.column(1).expect("lookup names").values(),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );

        let rows = execute_rows(
            "SELECT events.id, \
             (SELECT users.name FROM users \
              WHERE users.id >= events.id ORDER BY users.id LIMIT 1) AS next_user \
             FROM events ORDER BY events.id",
            &catalog,
            &provider,
        );
        assert_eq!(
            rows,
            [
                vec![Value::UInt64(1), Value::Utf8("user-a".to_owned())],
                vec![Value::UInt64(2), Value::Utf8("user-b".to_owned())],
            ]
        );
        assert_eq!(
            execute_values(
                "SELECT (SELECT users.name FROM users \
                         WHERE users.id >= events.id ORDER BY users.id LIMIT 1) \
                 FROM events",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );
        assert_eq!(
            execute_values(
                "SELECT (SELECT (SELECT u2.name FROM users u2 \
                                 WHERE u2.id >= u1.id AND u2.id >= events.id \
                                 ORDER BY u2.id LIMIT 1) \
                         FROM users u1 WHERE u1.id = events.id) \
                 FROM events",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );
        assert_eq!(
            execute_values(
                "SELECT (SELECT (SELECT e.name FROM users e WHERE e.id = u.id) \
                         FROM users u WHERE u.id = e.id) \
                 FROM events e",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );

        assert_eq!(
            execute_values(
                "SELECT events.id FROM events \
                 WHERE EXISTS (SELECT 1 FROM users WHERE users.id > events.id) \
                 ORDER BY events.id",
                &catalog,
                &provider,
            ),
            [Value::UInt64(1)]
        );
        assert_eq!(
            execute_values(
                "SELECT events.id FROM events \
                 WHERE events.id + 1 IN \
                       (SELECT users.id FROM users WHERE users.id > events.id) \
                 ORDER BY events.id",
                &catalog,
                &provider,
            ),
            [Value::UInt64(1)]
        );
        assert_eq!(
            execute_rows(
                "SELECT events.id, COUNT(*) FROM events GROUP BY events.id \
                 HAVING EXISTS (SELECT 1 FROM users WHERE users.id > events.id) \
                 ORDER BY events.id",
                &catalog,
                &provider,
            ),
            [vec![Value::UInt64(1), Value::UInt64(1)]]
        );
        assert_eq!(
            execute_values(
                "SELECT (WITH candidates AS (SELECT id, name FROM users) \
                         SELECT name FROM candidates \
                         WHERE candidates.id >= events.id \
                         ORDER BY candidates.id LIMIT 1) \
                 FROM events",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );
        assert_eq!(
            execute_values(
                "SELECT (SELECT candidates.name \
                         FROM (SELECT id, name FROM users) candidates \
                         WHERE candidates.id >= events.id \
                         ORDER BY candidates.id LIMIT 1) \
                 FROM events",
                &catalog,
                &provider,
            ),
            [
                Value::Utf8("user-a".to_owned()),
                Value::Utf8("user-b".to_owned()),
            ]
        );

        let statement = parse_statement(
            "SELECT (SELECT users.name FROM users WHERE users.id >= events.id) FROM events",
        )
        .expect("parse dependent scalar cardinality query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind dependent scalar cardinality query");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan dependent scalar cardinality query");
        assert!(matches!(
            Execution::start(physical, &provider, 64 * 1024, Collation::default()),
            Err(ExecError::ScalarSubqueryRows { rows: 2 })
        ));

        assert_eq!(
            execute_rows(
                "SELECT events.id, \
                        IF(events.id > 0, 'chosen', \
                           (SELECT users.name FROM users WHERE users.id >= events.id)), \
                        COALESCE('chosen', \
                           (SELECT users.name FROM users WHERE users.id >= events.id)) \
                 FROM events ORDER BY events.id",
                &catalog,
                &provider,
            ),
            [
                vec![
                    Value::UInt64(1),
                    Value::Utf8("chosen".to_owned()),
                    Value::Utf8("chosen".to_owned()),
                ],
                vec![
                    Value::UInt64(2),
                    Value::Utf8("chosen".to_owned()),
                    Value::Utf8("chosen".to_owned()),
                ],
            ]
        );

        let statement = parse_statement(
            "SELECT events.id, (SELECT MAX(id) FROM users) AS largest_user, \
             events.id IN (SELECT id FROM users WHERE id = 2) AS selected \
             FROM events ORDER BY events.id",
        )
        .expect("parse relational subqueries");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind relational subqueries");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("relational subquery plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024, Collation::default())
            .expect("subquery execution");
        let batch = execution
            .next_batch()
            .expect("subquery pull")
            .expect("subquery batch");
        assert_eq!(
            batch.column(0).expect("ids").values(),
            [Value::UInt64(1), Value::UInt64(2)]
        );
        assert_eq!(
            batch.column(1).expect("maximum").values(),
            [Value::Int64(2), Value::Int64(2)]
        );
        assert_eq!(
            batch.column(2).expect("membership").values(),
            [Value::Boolean(false), Value::Boolean(true)]
        );

        let statement =
            parse_statement("SELECT (SELECT id FROM users)").expect("parse multi-row subquery");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind multi-row subquery");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("multi-row subquery plan");
        assert!(matches!(
            Execution::start(physical, &provider, 64 * 1024, Collation::default()),
            Err(ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    fn schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "name", DataType::Utf8, true),
            ],
        )
        .expect("schema")
    }

    fn row(id: u64, name: &str) -> StoredRow {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            vec![Value::UInt64(id), Value::Utf8(name.to_owned())],
            id,
            false,
        )
    }

    fn signed_schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "name", DataType::Utf8, true),
            ],
        )
        .expect("schema")
    }

    fn signed_row(id: i64, name: &str) -> StoredRow {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
            vec![Value::Int64(id), Value::Utf8(name.to_owned())],
            u64::try_from(id).expect("positive test ID"),
            false,
        )
    }

    fn execute_rows(
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
    ) -> Vec<Vec<Value>> {
        execute_rows_limited(sql, catalog, provider, 512 * 1024 * 1024)
    }

    fn execute_rows_limited(
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
        memory_limit: usize,
    ) -> Vec<Vec<Value>> {
        let statement = parse_statement(sql).expect("parse query");
        let bound = Binder::new(catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, memory_limit, Collation::default())
                .expect("start execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("pull batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().expect("selected value"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    }

    fn window_fixture() -> (
        tempfile::TempDir,
        pintail_store::TableSnapshot,
        CatalogSnapshot,
    ) {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .ingest(vec![
                row(1, "a"),
                row(2, "a"),
                row(3, "b"),
                row(4, "b"),
                row(5, "b"),
            ])
            .expect("ingest");
        let snapshot = table.snapshot();
        let entry = TableEntry::new(
            TableId::new(17),
            "events",
            schema,
            TableStatistics::with_row_count(5),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(DatabaseId::new(15), "app", [entry]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        drop(table);
        (directory, snapshot, catalog)
    }

    /// The process-wide budget is what a long-running server actually has: it
    /// is shared, finite, and nothing refills it. A query that returns less
    /// than it borrowed walks the balance in one direction until every query
    /// is refused - which is what a 30-minute benchmark phase hit after about
    /// 1,500 queries, while replication carried on looking healthy.
    #[test]
    fn repeated_queries_return_the_memory_they_borrowed() {
        // Held across the whole loop, not just each query: the reading that
        // matters is taken BETWEEN queries, and a sibling test reserving in
        // that gap is what made this fail in the suite while passing alone.
        let _serial = crate::execution::budget_serial::Serial::acquire();
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        // Generous, and restored at the end: the budget is process-wide, so a
        // small ceiling left behind would starve whichever test ran next.
        let previous_limit = crate::shared_memory_budget().limit();
        crate::init_shared_memory_budget(4 * 1024 * 1024 * 1024);
        // A zero limit accounts for nothing, so every measurement below would
        // read zero and this would pass having proved nothing.
        assert!(
            crate::shared_memory_budget().limit() > 0,
            "budget is not accounting"
        );
        let sql = "SELECT name, COUNT(*) AS n FROM events GROUP BY name ORDER BY name";

        execute_rows(sql, &catalog, &provider);
        let after_first = crate::shared_memory_budget().used();
        for iteration in 2..=20 {
            execute_rows(sql, &catalog, &provider);
            assert_eq!(
                crate::shared_memory_budget().used(),
                after_first,
                "query {iteration} left the shared budget higher than query 1 did \
                 ({after_first} bytes); the balance only ever grows",
            );
        }
        crate::init_shared_memory_budget(previous_limit);
    }

    /// The opposite failure to the leak, and the worse one.
    ///
    /// A tracker that repaid more than it borrowed would hand the pool memory
    /// that was never in it, and the budget would drift DOWNWARD until it
    /// believed it had capacity nobody was using - a limit that stops
    /// limiting. Clones are the risk: they inherit what the query is holding,
    /// so a naive release-on-drop would repay one debt twice.
    #[test]
    fn a_cloned_tracker_does_not_repay_the_original_debt() {
        // This one never runs a query, so nothing takes the lock on its
        // behalf - and it reads the same process-wide counter.
        let _serial = crate::execution::budget_serial::Serial::acquire();
        let previous_limit = crate::shared_memory_budget().limit();
        crate::init_shared_memory_budget(4 * 1024 * 1024 * 1024);
        assert!(
            crate::shared_memory_budget().limit() > 0,
            "budget is not accounting"
        );
        let before = crate::shared_memory_budget().used();
        {
            let tracker = crate::MemoryTracker::new(8 * 1024 * 1024);
            tracker.reserve(4096).expect("reserve");
            assert_eq!(crate::shared_memory_budget().used(), before + 4096);
            let clone = tracker.clone();
            assert_eq!(
                clone.used(),
                tracker.used(),
                "the clone inherits the holding"
            );
            drop(clone);
            assert_eq!(
                crate::shared_memory_budget().used(),
                before + 4096,
                "dropping the clone must not repay a debt it never took on",
            );
        }
        assert_eq!(
            crate::shared_memory_budget().used(),
            before,
            "the original repays exactly once, when it goes away",
        );
        crate::init_shared_memory_budget(previous_limit);
    }

    #[test]
    fn window_row_number_partitions_and_orders() {
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        let rows = execute_rows(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) AS rn \
             FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::UInt64(1), Value::UInt64(1)],
                vec![Value::UInt64(2), Value::UInt64(2)],
                vec![Value::UInt64(3), Value::UInt64(1)],
                vec![Value::UInt64(4), Value::UInt64(2)],
                vec![Value::UInt64(5), Value::UInt64(3)],
            ]
        );
    }

    #[test]
    fn window_rank_and_dense_rank_handle_peers() {
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        let rows = execute_rows(
            "SELECT id, RANK() OVER (ORDER BY name) AS r, \
             DENSE_RANK() OVER (ORDER BY name) AS d FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::UInt64(1), Value::UInt64(1), Value::UInt64(1)],
                vec![Value::UInt64(2), Value::UInt64(1), Value::UInt64(1)],
                vec![Value::UInt64(3), Value::UInt64(3), Value::UInt64(2)],
                vec![Value::UInt64(4), Value::UInt64(3), Value::UInt64(2)],
                vec![Value::UInt64(5), Value::UInt64(3), Value::UInt64(2)],
            ]
        );
    }

    #[test]
    fn window_aggregates_run_whole_partition_and_running_frames() {
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        // Whole-partition frame without ORDER BY.
        let rows = execute_rows(
            "SELECT id, SUM(id) OVER (PARTITION BY name) AS total \
             FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        let totals = rows.iter().map(|row| row[1].clone()).collect::<Vec<_>>();
        assert_eq!(
            totals,
            ["3", "3", "12", "12", "12"].map(|sum| Value::Utf8(sum.to_owned()))
        );
        // Running frame with ORDER BY includes the current row's peers.
        let rows = execute_rows(
            "SELECT id, SUM(id) OVER (ORDER BY name) AS running \
             FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        let running = rows.iter().map(|row| row[1].clone()).collect::<Vec<_>>();
        assert_eq!(
            running,
            ["3", "3", "15", "15", "15"].map(|sum| Value::Utf8(sum.to_owned()))
        );
        // COUNT(*) over a partition counts its rows.
        let rows = execute_rows(
            "SELECT id, COUNT(*) OVER (PARTITION BY name) AS n FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        assert_eq!(rows[0][1], Value::UInt64(2));
        assert_eq!(rows[4][1], Value::UInt64(3));
    }

    #[test]
    fn windows_nest_in_expressions_and_ride_above_grouping() {
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        // Window inside arithmetic (the q07 share-of-total shape).
        let rows = execute_rows(
            "SELECT id, id * 100 / SUM(id) OVER (PARTITION BY name) AS share              FROM events ORDER BY id",
            &catalog,
            &provider,
        );
        let shares = rows.iter().map(|row| row[1].clone()).collect::<Vec<_>>();
        // Partition a: ids 1,2 (sum 3); partition b: ids 3,4,5 (sum 12).
        // Integer division is MySQL DECIMAL: scale widened by four, rounded
        // half away from zero.
        assert_eq!(shares[0], Value::Utf8("33.3333".to_owned()));
        assert_eq!(shares[1], Value::Utf8("66.6667".to_owned()));
        assert_eq!(shares[2], Value::Utf8("25.0000".to_owned()));
        assert_eq!(shares[4], Value::Utf8("41.6667".to_owned()));
    }

    #[test]
    fn windows_ride_above_grouping() {
        let (_directory, snapshot, catalog) = window_fixture();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(15), TableId::new(17), &snapshot)])
                .expect("provider");
        // Window over aggregate output: SUM(SUM(id)) OVER () computes each
        // group's share of the grand total exactly like MySQL.
        let rows = execute_rows(
            "SELECT name, SUM(id) AS total,              SUM(id) * 100 / SUM(SUM(id)) OVER () AS share,              ROW_NUMBER() OVER (ORDER BY SUM(id) DESC) AS heaviest              FROM events GROUP BY name ORDER BY name",
            &catalog,
            &provider,
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Utf8("a".to_owned()),
                    Value::Utf8("3".to_owned()),
                    Value::Utf8("20.0000".to_owned()),
                    Value::UInt64(2),
                ],
                vec![
                    Value::Utf8("b".to_owned()),
                    Value::Utf8("12".to_owned()),
                    Value::Utf8("80.0000".to_owned()),
                    Value::UInt64(1),
                ],
            ]
        );
    }

    #[test]
    fn windows_reject_unsupported_combinations() {
        let (_directory, _snapshot, catalog) = window_fixture();
        for sql in [
            // Explicit frames stay v1-unsupported.
            "SELECT ROW_NUMBER() OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) FROM events",
            // Windows never appear in WHERE or inside aggregate arguments.
            "SELECT id FROM events WHERE ROW_NUMBER() OVER (ORDER BY id) = 1",
            "SELECT SUM(ROW_NUMBER() OVER (ORDER BY id)) FROM events",
            // DISTINCT + window stays rejected.
            "SELECT DISTINCT ROW_NUMBER() OVER (ORDER BY id) FROM events",
        ] {
            let statement = parse_statement(sql).expect("parse query");
            assert!(
                Binder::new(&catalog, Some("app")).bind(&statement).is_err(),
                "{sql} must be rejected"
            );
        }
    }

    #[test]
    fn probe_restriction_narrows_an_unstarted_scan() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        for start in [1_u64, 1001, 2001, 3001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| row(key, &format!("value-{key}")))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(4000),
        )
        .expect("table entry")
        .with_key_columns(vec![1])
        .expect("key columns");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let statement = parse_statement("SELECT id FROM events").expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let crate::PhysicalPlan::Project { input, .. } = physical else {
            panic!("expected projection over scan");
        };
        let crate::PhysicalPlan::Scan(scan) = *input else {
            panic!("expected scan input");
        };

        let mut stream = provider.open_scan(&scan, 64 * 1024 * 1024).expect("open");
        stream.restrict_key_position_range(0, &Value::UInt64(1500), &Value::UInt64(1600));
        let mut ids = Vec::new();
        while let Some(batch) = stream.next_batch(64 * 1024 * 1024).expect("pull") {
            for row in batch.selection().selected_rows() {
                let Some(Value::UInt64(id)) = batch.column(0).and_then(|c| c.value(row)) else {
                    panic!("expected id");
                };
                ids.push(*id);
            }
        }
        assert_eq!(ids, (1500..=1600).collect::<Vec<_>>());

        // An empty intersection yields no rows at all.
        let mut stream = provider.open_scan(&scan, 64 * 1024 * 1024).expect("open");
        stream.restrict_key_position_range(0, &Value::UInt64(9000), &Value::UInt64(9001));
        assert!(stream.next_batch(64 * 1024 * 1024).expect("pull").is_none());

        // Restrictions after the stream starts are ignored.
        let mut stream = provider.open_scan(&scan, 64 * 1024 * 1024).expect("open");
        let first = stream
            .next_batch(64 * 1024 * 1024)
            .expect("pull")
            .expect("first batch");
        drop(first);
        stream.restrict_key_position_range(0, &Value::UInt64(9000), &Value::UInt64(9001));
        assert!(
            stream.next_batch(64 * 1024 * 1024).expect("pull").is_some(),
            "started streams ignore restrictions"
        );
    }

    #[test]
    fn prewhere_scans_return_exact_rows_for_non_key_predicates() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        for start in [1_u64, 1001, 2001, 3001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| row(key, &format!("value-{key}")))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(4000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let limit = 8 * 1024 * 1024;
        // Highly selective equality on a non-key string column.
        assert_eq!(
            execute_values_with_limit(
                "SELECT id FROM events WHERE name = 'value-1500'",
                &catalog,
                &provider,
                limit,
            ),
            [Value::UInt64(1500)]
        );
        // A predicate matching nothing.
        assert_eq!(
            execute_values_with_limit(
                "SELECT id FROM events WHERE name = 'value-9999'",
                &catalog,
                &provider,
                limit,
            ),
            []
        );
        // An unselective predicate keeps every row.
        assert_eq!(
            execute_values_with_limit(
                "SELECT COUNT(*) FROM events WHERE name != 'value-1500'",
                &catalog,
                &provider,
                limit,
            ),
            [Value::UInt64(3999)]
        );
        // Scattered survivors across segments and blocks.
        assert_eq!(
            execute_values_with_limit(
                "SELECT COUNT(*) FROM events WHERE name IN ('value-2', 'value-1500', 'value-3999')",
                &catalog,
                &provider,
                limit,
            ),
            [Value::UInt64(3)]
        );
    }

    #[test]
    fn prewhere_stops_reprobing_after_an_unselective_chunk() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        for start in (1_u64..=16_000).step_by(1000) {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| row(key, &format!("value-{key}")))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(16_000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let values = execute_values_with_limit(
            "SELECT id, name FROM events WHERE name != 'missing'",
            &catalog,
            &provider,
            64 * 1024 * 1024,
        );
        assert_eq!(values.len(), 16_000);
        assert_eq!(values.first(), Some(&Value::UInt64(1)));
        assert_eq!(values.last(), Some(&Value::UInt64(16_000)));

        let stats = provider
            .scan_stats(database_id, table_id)
            .expect("physical scan stats");
        assert_eq!(stats.segments_read, 16);
        assert!(
            stats.blocks_decoded <= 40,
            "only the first eight-segment prefetch may pay the predicate probe; got {stats:?}"
        );
    }

    #[test]
    fn prewhere_selects_again_where_later_segments_reject_rows() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        // The first half passes the filter whole; the second half keeps one
        // row in a hundred.
        for start in (1_u64..=64_000).step_by(1000) {
            table
                .bulk_ingest_snapshot(
                    (start..start + 1000)
                        .map(|key| {
                            if key <= 32_000 || key.is_multiple_of(100) {
                                row(key, &format!("value-{key}"))
                            } else {
                                row(key, "late")
                            }
                        })
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(64_000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        // Under a tight ceiling the scan reads one segment a round, so which
        // segments are judged does not depend on the machine's width.
        let values = execute_values_with_limit(
            "SELECT id, name FROM events WHERE name != 'late'",
            &catalog,
            &provider,
            32 * 1024 * 1024,
        );
        assert_eq!(values.len(), 32_000 + 320);
        assert_eq!(values.first(), Some(&Value::UInt64(1)));
        assert_eq!(values.last(), Some(&Value::UInt64(64_000)));

        let stats = provider
            .scan_stats(database_id, table_id)
            .expect("physical scan stats");
        assert_eq!(stats.segments_read, 64);
        // Decoding every row of both columns is 128,000 values. The dense
        // half decodes whole; of the sparse half only the segments up to
        // the first judged one do.
        assert!(
            stats.values_decoded < 110_000,
            "the sparse half must be read through the selection; got {stats:?}"
        );
    }

    #[test]
    fn two_pass_partitioned_aggregate_matches_expected_at_scale() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        // Above the two-pass threshold (262,144) with unique keys: the
        // hardest cardinality shape, every group holds exactly one row.
        for start in [1_u64, 100_001, 200_001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 100_000)
                        .map(|key| row(key, &format!("v{}", key % 7)))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(300_000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let rows = execute_rows(
            "SELECT id, COUNT(*) AS c, SUM(id) AS s, MIN(id) AS lo, MAX(id) AS hi \
             FROM events GROUP BY id ORDER BY id LIMIT 3",
            &catalog,
            &provider,
        );
        assert_eq!(rows.len(), 3);
        for (offset, row) in rows.iter().enumerate() {
            let id = offset as u64 + 1;
            assert_eq!(row[0], Value::UInt64(id), "group key");
            assert_eq!(row[1], Value::UInt64(1), "count");
            assert_eq!(
                row[2],
                Value::Utf8(id.to_string()),
                "integer sums stay exact"
            );
            assert_eq!(row[3], Value::UInt64(id), "min stays exact");
            assert_eq!(row[4], Value::UInt64(id), "max stays exact");
        }

        // Aggregate over every group: total row count via a COUNT(*) with
        // no grouping must agree with the grouped path's group count.
        let total = execute_rows(
            "SELECT COUNT(*) FROM (SELECT id FROM events GROUP BY id) AS g",
            &catalog,
            &provider,
        );
        if let Some(first) = total.first() {
            assert_eq!(first[0], Value::UInt64(300_000), "distinct group count");
        }
    }

    #[test]
    fn two_pass_aggregate_falls_back_to_sequential_when_memory_is_tight() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "bucket", DataType::Int64, false),
            ],
        )
        .expect("schema");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        // Above the two-pass row threshold, but with a memory limit the
        // scatter buffers cannot fit (the scatter alone needs ~7.5MB for
        // 300k rows and two lanes). The query must degrade to the
        // sequential loop and still return exact results. Small segments
        // keep the scan's own transient footprint well under the limit.
        for chunk in 0_u64..12 {
            let start = chunk * 25_000 + 1;
            table
                .bulk_ingest_snapshot(
                    (start..start + 25_000)
                        .map(|key| {
                            StoredRow::new(
                                PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                                vec![
                                    Value::UInt64(key),
                                    Value::Int64(i64::try_from(key % 1000).expect("bucket")),
                                ],
                                key,
                                false,
                            )
                        })
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(21);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(300_000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let rows = execute_rows_limited(
            "SELECT bucket, COUNT(*) AS c, SUM(bucket) AS s, SUM(id) AS ids \
             FROM events GROUP BY bucket ORDER BY bucket LIMIT 5",
            &catalog,
            &provider,
            12 * 1024 * 1024,
        );
        assert_eq!(rows.len(), 5);
        for (offset, row) in rows.iter().enumerate() {
            let bucket = i64::try_from(offset).expect("bucket");
            assert_eq!(row[0], Value::Int64(bucket), "group key");
            assert_eq!(row[1], Value::UInt64(300), "count");
            let bucket_sum = Value::Utf8((bucket * 300).to_string());
            assert_eq!(row[2], bucket_sum, "integer sums stay exact");
            // Bucket b holds ids {b, 1000+b, ..., 299000+b}, except bucket 0
            // whose members start at 1000 because ids begin at 1.
            let id_sum = if bucket == 0 {
                45_150_000
            } else {
                44_850_000 + 300 * u64::try_from(bucket).expect("bucket")
            };
            let id_sum = Value::Utf8(id_sum.to_string());
            assert_eq!(row[3], id_sum, "id sums stay exact");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sma_fold_answers_bare_aggregates_during_ingest_and_declines_on_overlap() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "amount", DataType::Int64, true),
                Column::new(
                    3,
                    "price",
                    DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    true,
                ),
            ],
        )
        .expect("schema");
        let sma_row = |id: u64, amount: Option<i64>, price: &str| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(id),
                    amount.map_or(Value::Null, Value::Int64),
                    Value::Utf8(price.to_owned()),
                ],
                id,
                false,
            )
        };
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        // Two disjoint segments, each carrying manifest-v2 SMAs.
        table
            .bulk_ingest_snapshot((1..=500).map(|id| sma_row(id, Some(2), "1.25")).collect())
            .expect("first segment");
        table
            .bulk_ingest_snapshot(
                (501..=1000)
                    .map(|id| sma_row(id, if id % 2 == 0 { None } else { Some(4) }, "0.75"))
                    .collect(),
            )
            .expect("second segment");
        let database_id = DatabaseId::new(31);
        let table_id = TableId::new(37);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema.clone(),
            TableStatistics::with_row_count(1000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let aggregate = |table: &TableStore| {
            let snapshot = table.snapshot();
            let provider =
                SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
            execute_rows(
                "SELECT COUNT(id), COUNT(amount), SUM(amount), MIN(amount), MAX(amount),                  SUM(price), MIN(price), MAX(price), AVG(amount)                  FROM events",
                &catalog,
                &provider,
            )
            .remove(0)
        };
        // 500 rows of amount=2 plus 250 odd rows of amount=4 (evens NULL).
        let expect_settled = |row: &[Value]| {
            assert_eq!(row[0], Value::UInt64(1000), "COUNT(id)");
            assert_eq!(row[1], Value::UInt64(750), "COUNT(amount)");
            assert_eq!(row[2], Value::Utf8("2000".to_owned()), "SUM(amount)");
            assert_eq!(row[3], Value::Int64(2), "MIN(amount)");
            assert_eq!(row[4], Value::Int64(4), "MAX(amount)");
            assert_eq!(row[5], Value::Utf8("1000.00".to_owned()), "SUM(price)");
            assert_eq!(row[6], Value::Utf8("0.75".to_owned()), "MIN(price)");
            assert_eq!(row[7], Value::Utf8("1.25".to_owned()), "MAX(price)");
        };
        let hits_before = crate::execution::sma_fold_hits();
        let settled = aggregate(&table);
        expect_settled(&settled);
        assert!(
            crate::execution::sma_fold_hits() > hits_before,
            "settled bare aggregates must fold segment SMAs"
        );
        // SMAs are persistent: a reopened store decodes them from the v2
        // manifest. (The settled memo may still serve the reopened query —
        // same directory and generation — so assert on the store API.)
        drop(table);
        let mut table = TableStore::open(directory.path(), schema, StoreOptions::default())
            .expect("reopen table");
        {
            let snapshot = table.snapshot();
            let (segments, residual) = snapshot
                .sma_fold_state()
                .expect("reopened manifest carries decodable SMAs");
            assert_eq!(segments.iter().map(|sma| sma.live_rows).sum::<u64>(), 1000);
            assert!(residual.is_empty());
        }
        expect_settled(&aggregate(&table));
        // Pure inserts above the segment key space: the fold aggregates the
        // residual memtable rows and stays exact DURING ingest.
        table
            .ingest_cdc(vec![sma_row(1001, Some(10), "2.00")])
            .expect("cdc insert");
        let hits_before = crate::execution::sma_fold_hits();
        let during_ingest = aggregate(&table);
        assert!(
            crate::execution::sma_fold_hits() > hits_before,
            "insert-only ingest must keep folding"
        );
        assert_eq!(during_ingest[0], Value::UInt64(1001));
        assert_eq!(during_ingest[1], Value::UInt64(751));
        assert_eq!(during_ingest[2], Value::Utf8("2010".to_owned()));
        assert_eq!(during_ingest[4], Value::Int64(10), "MAX sees the new row");
        assert_eq!(during_ingest[5], Value::Utf8("1002.00".to_owned()));
        assert_eq!(during_ingest[7], Value::Utf8("2.00".to_owned()));
        // An update of an EXISTING key overlaps the segment key space: the
        // fold must refuse (merge-on-read overlay) and the scan stays exact.
        table
            .ingest_cdc(vec![sma_row(5, Some(100), "9.99")])
            .expect("cdc update");
        let hits_before = crate::execution::sma_fold_hits();
        let overlaid = aggregate(&table);
        assert_eq!(
            crate::execution::sma_fold_hits(),
            hits_before,
            "overlapping memtable keys must decline the fold"
        );
        assert_eq!(overlaid[0], Value::UInt64(1001), "count unchanged");
        assert_eq!(
            overlaid[2],
            Value::Utf8("2108".to_owned()),
            "updated amount replaces 2"
        );
        assert_eq!(overlaid[4], Value::Int64(100));
        assert_eq!(overlaid[7], Value::Utf8("9.99".to_owned()));
    }

    #[test]
    fn evaluates_datetime_helpers_and_inline_intervals_exactly() {
        let directory = tempfile::tempdir().expect("temporary table");
        let mut table = TableStore::open(directory.path(), schema(), StoreOptions::default())
            .expect("open table");
        table
            .bulk_ingest_snapshot((1..=3).map(|key| row(key, "v")).collect())
            .expect("seed rows");
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(29);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema(),
            TableStatistics::with_row_count(3),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
        let rows = execute_rows(
            "SELECT CEIL(1.2), FLOOR(-1.2), \
             TIMESTAMPDIFF(SECOND, '2024-01-01 00:00:00', '2024-01-01 00:05:30'), \
             TIMESTAMPDIFF(MONTH, '2020-01-31', '2020-02-29'), \
             TIMESTAMPDIFF(SECOND, '2024-01-01 00:00:10', '2024-01-01 00:00:00'), \
             '2024-01-31' + INTERVAL 1 DAY, \
             '2024-03-31' - INTERVAL 1 MONTH \
             FROM events LIMIT 1",
            &catalog,
            &provider,
        );
        // Exact-valued arguments make CEIL/FLOOR return an integer, as
        // MySQL does; they were f64 only because dotted literals used to be
        // typed Float64.
        assert_eq!(rows[0][0], Value::Int64(2), "CEIL");
        assert_eq!(rows[0][1], Value::Int64(-2), "FLOOR");
        assert_eq!(rows[0][2], Value::Int64(330), "TIMESTAMPDIFF SECOND");
        assert_eq!(rows[0][3], Value::Int64(0), "TIMESTAMPDIFF MONTH boundary");
        assert_eq!(rows[0][4], Value::Int64(-10), "negative direction");
        assert_eq!(
            rows[0][5],
            Value::Utf8("2024-02-01".to_owned()),
            "+ INTERVAL"
        );
        assert_eq!(
            rows[0][6],
            Value::Utf8("2024-02-29".to_owned()),
            "- INTERVAL month clamp"
        );
    }

    #[test]
    fn settled_aggregate_memo_serves_exact_rows_and_invalidates_on_ingest() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        table
            .bulk_ingest_snapshot((1..=1000).map(|key| row(key, "v")).collect())
            .expect("seed segment");
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(23);
        let make_catalog = || {
            let entry = TableEntry::new(
                table_id,
                "events",
                schema.clone(),
                TableStatistics::with_row_count(1000),
            )
            .expect("table entry");
            let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
            CatalogSnapshot::new([database]).expect("catalog")
        };
        let catalog = make_catalog();
        let count = |table: &TableStore, catalog: &CatalogSnapshot| {
            let snapshot = table.snapshot();
            let provider =
                SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
            // COUNT(column), not COUNT(*): the optimizer answers COUNT(*)
            // from catalog statistics before any scan (or memo) is built.
            execute_rows("SELECT COUNT(name) FROM events", catalog, &provider)[0][0].clone()
        };
        // First run computes; the second must serve the memo (same value).
        assert_eq!(count(&table, &catalog), Value::UInt64(1000));
        assert_eq!(count(&table, &catalog), Value::UInt64(1000));
        // A new segment bumps the manifest generation: stale memo entries
        // become unreachable and the fresh count is exact.
        table
            .bulk_ingest_snapshot((1001..=1010).map(|key| row(key, "v")).collect())
            .expect("second segment");
        assert_eq!(count(&table, &catalog), Value::UInt64(1010));
        // Insert-only memtable rows above the segment key space: the memo
        // result merges with the delta (COUNT is finished-mergeable), so
        // answers stay exact DURING ingest.
        table.ingest_cdc(vec![row(1011, "v")]).expect("cdc ingest");
        assert_eq!(count(&table, &catalog), Value::UInt64(1011));
        table.ingest_cdc(vec![row(1012, "v")]).expect("cdc ingest");
        assert_eq!(count(&table, &catalog), Value::UInt64(1012));
        // Filtered aggregates memoize under their own key: same generation,
        // different predicate, different entry — and both stay exact.
        let filtered = |table: &TableStore, catalog: &CatalogSnapshot| {
            let snapshot = table.snapshot();
            let provider =
                SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
            execute_rows(
                "SELECT COUNT(name) FROM events WHERE id > 500",
                catalog,
                &provider,
            )[0][0]
                .clone()
        };
        assert_eq!(filtered(&table, &catalog), Value::UInt64(512));
        assert_eq!(filtered(&table, &catalog), Value::UInt64(512));
        assert_eq!(count(&table, &catalog), Value::UInt64(1012));
        // An update of an EXISTING key overlaps the segment key space: the
        // delta merge must refuse and the full scan stays exact (the row
        // count is unchanged — key 5 is replaced, not added).
        table
            .ingest_cdc(vec![row(5, "updated")])
            .expect("cdc update");
        assert_eq!(count(&table, &catalog), Value::UInt64(1012));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn settled_join_memo_invalidates_when_either_table_changes() {
        let orders_dir = tempfile::tempdir().expect("orders dir");
        let users_dir = tempfile::tempdir().expect("users dir");
        let orders_schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "user_id", DataType::UInt64, false),
            ],
        )
        .expect("orders schema");
        let users_schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "region", DataType::Utf8, true),
            ],
        )
        .expect("users schema");
        let mut orders = TableStore::open(
            orders_dir.path(),
            orders_schema.clone(),
            StoreOptions::default(),
        )
        .expect("orders table");
        let mut users = TableStore::open(
            users_dir.path(),
            users_schema.clone(),
            StoreOptions::default(),
        )
        .expect("users table");
        let order_row = |id: u64, user: u64| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![Value::UInt64(id), Value::UInt64(user)],
                id,
                false,
            )
        };
        let user_row = |id: u64, region: &str| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![Value::UInt64(id), Value::Utf8(region.to_owned())],
                id,
                false,
            )
        };
        orders
            .bulk_ingest_snapshot((1..=100).map(|id| order_row(id, 1 + id % 4)).collect())
            .expect("orders seed");
        users
            .bulk_ingest_snapshot((1..=4).map(|id| user_row(id, "east")).collect())
            .expect("users seed");
        let database_id = DatabaseId::new(21);
        let orders_id = TableId::new(31);
        let users_id = TableId::new(32);
        let catalog = {
            let orders_entry = TableEntry::new(
                orders_id,
                "orders",
                orders_schema.clone(),
                TableStatistics::with_row_count(100),
            )
            .expect("orders entry");
            let users_entry = TableEntry::new(
                users_id,
                "users",
                users_schema.clone(),
                TableStatistics::with_row_count(4),
            )
            .expect("users entry");
            let database = DatabaseEntry::new(database_id, "app", [orders_entry, users_entry])
                .expect("database");
            CatalogSnapshot::new([database]).expect("catalog")
        };
        let joined = |orders: &TableStore, users: &TableStore| {
            let orders_snapshot = orders.snapshot();
            let users_snapshot = users.snapshot();
            let provider = SnapshotScanProvider::new([
                (database_id, orders_id, &orders_snapshot),
                (database_id, users_id, &users_snapshot),
            ])
            .expect("provider");
            execute_rows(
                "SELECT u.region, COUNT(*) FROM orders o JOIN users u ON o.user_id = u.id                  GROUP BY u.region",
                &catalog,
                &provider,
            )
        };
        // Cold, then memoized: identical exact rows.
        assert_eq!(joined(&orders, &users)[0][1], Value::UInt64(100));
        assert_eq!(joined(&orders, &users)[0][1], Value::UInt64(100));
        // Growing the LEFT table changes its generation: fresh exact result.
        orders
            .bulk_ingest_snapshot((101..=110).map(|id| order_row(id, 1)).collect())
            .expect("orders growth");
        assert_eq!(joined(&orders, &users)[0][1], Value::UInt64(110));
        // Growing the RIGHT table changes its generation: user 5 arrives
        // with a new region, and rows for it appear only after the change.
        assert_eq!(joined(&orders, &users).len(), 1);
        users
            .bulk_ingest_snapshot(vec![user_row(5, "west")])
            .expect("users growth");
        orders
            .bulk_ingest_snapshot(vec![order_row(111, 5)])
            .expect("order for new user");
        let rows = joined(&orders, &users);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn dictionary_coded_columns_aggregate_and_filter_exactly() {
        let directory = tempfile::tempdir().expect("temporary table");
        let schema = schema();
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        // Three distinct labels over 6,000 rows: dictionary-encoded on disk
        // (distinct * 10 < rows). One label exceeds the 12-byte inline view
        // limit so template views exercise the shared heap.
        let label = |key: u64| match key % 3 {
            0 => "alpha",
            1 => "beta",
            _ => "a-label-well-past-inline",
        };
        for start in [1_u64, 3001] {
            table
                .bulk_ingest_snapshot(
                    (start..start + 3000)
                        .map(|key| row(key, label(key)))
                        .collect(),
                )
                .expect("bulk snapshot segment");
        }
        let snapshot = table.snapshot();
        let database_id = DatabaseId::new(15);
        let table_id = TableId::new(17);
        let entry = TableEntry::new(
            table_id,
            "events",
            schema,
            TableStatistics::with_row_count(6000),
        )
        .expect("table entry");
        let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let provider =
            SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

        let rows = execute_rows(
            "SELECT name, COUNT(*) AS c FROM events GROUP BY name ORDER BY name",
            &catalog,
            &provider,
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Utf8("a-label-well-past-inline".into()),
                    Value::UInt64(2000)
                ],
                vec![Value::Utf8("alpha".into()), Value::UInt64(2000)],
                vec![Value::Utf8("beta".into()), Value::UInt64(2000)],
            ]
        );
        assert_eq!(
            execute_values_with_limit(
                "SELECT COUNT(*) FROM events WHERE name = 'a-label-well-past-inline'",
                &catalog,
                &provider,
                64 * 1024 * 1024,
            ),
            [Value::UInt64(2000)]
        );
        // Row-shaped output through the dictionary path.
        assert_eq!(
            execute_values_with_limit(
                "SELECT name FROM events WHERE id = 2",
                &catalog,
                &provider,
                64 * 1024 * 1024,
            ),
            [Value::Utf8("a-label-well-past-inline".into())]
        );
    }
}
