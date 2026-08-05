use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_store::{
    DecodedColumn, ProjectedColumnChunk, ProjectedRow, ProjectedScanStream, ScanStats, StoreError,
    TableSnapshot,
};
use pintail_types::{KeyPart, PrimaryKey, Value};
use rayon::prelude::*;

use crate::{
    BatchStream, ColumnVector, DEFAULT_BATCH_ROWS, ExecError, RecordBatch, Scan, ScanProvider,
    array::{StrColumn, ValidityMask},
    batch::{LazyText, TypedValues, parse_date_days, parse_datetime_micros, parse_decimal_scaled},
};

/// Storage scan provider backed by reader-pinned table snapshots.
pub struct SnapshotScanProvider<'snapshot> {
    snapshots: BTreeMap<(DatabaseId, TableId), &'snapshot TableSnapshot>,
    unique_visibility: BTreeMap<(DatabaseId, TableId), Vec<Vec<u32>>>,
    stats: Arc<Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>>,
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
        }
    }
}

impl<'snapshot> SnapshotScanProvider<'snapshot> {
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
            snapshots: indexed,
            unique_visibility: BTreeMap::new(),
            stats: Arc::new(Mutex::new(BTreeMap::new())),
        })
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
    #[allow(clippy::too_many_lines)]
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        let key = (scan.table.database_id, scan.table.table_id);
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
        let stream_overhead = std::mem::size_of::<SnapshotStream>()
            .saturating_add(types.capacity() * std::mem::size_of::<pintail_types::DataType>());
        if stream_overhead > memory_limit {
            return Err(ExecError::MemoryLimitExceeded {
                used: 0,
                requested: stream_overhead,
                limit: memory_limit,
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
                prefetched: VecDeque::new(),
                stream: None,
                prewhere: None,
                key_position: None,
                started: true,
                types,
                retained_bytes: stream_overhead,
                remaining: None,
                settled: None,
                sma: None,
                delta: None,
            }));
        };
        let unique_keys = self.unique_visibility.get(&key);
        // Bounded residual so the eager projection clone stays cheap for
        // scans that never fold; unique-key visibility changes merge-on-read
        // semantics, so those tables decline. Both the streamed and the
        // materialized scan paths carry the same fold input.
        #[allow(clippy::items_after_statements)]
        const SMA_RESIDUAL_ROW_CAP: usize = 16_384;
        let sma = (scan.predicates.is_empty() && scan.limit.is_none() && unique_keys.is_none())
            .then(|| snapshot.sma_fold_state())
            .flatten()
            .filter(|(_, rows)| rows.len() <= SMA_RESIDUAL_ROW_CAP)
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
        let mut physical_column_ids = scan.projected_column_ids.clone();
        if let Some(unique_keys) = unique_keys {
            for column_id in unique_keys.iter().flatten() {
                if !physical_column_ids.contains(column_id) {
                    physical_column_ids.push(*column_id);
                }
            }
        }
        let value_bounds = sma_column_bounds(&scan.predicates);
        if unique_keys.is_none()
            && let Some(stream) = snapshot
                .scan_projected_range_stream_pruned(
                    &start,
                    &end,
                    &physical_column_ids,
                    &value_bounds,
                )
                .map_err(|error| ExecError::Source(error.to_string()))?
        {
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
            // Bounded so merging never costs more than it saves.
            #[allow(clippy::items_after_statements)]
            const DELTA_ROW_CAP: usize = 4096;
            let delta = (scan.predicates.is_empty() && scan.limit.is_none())
                .then(|| snapshot.insert_only_delta())
                .flatten()
                .filter(|(_, _, rows)| rows.len() <= DELTA_ROW_CAP)
                .map(
                    |(directory, generation, rows)| crate::execution::InsertOnlyDelta {
                        directory: directory.to_path_buf(),
                        generation,
                        scan: format!(
                            "{:?}|{:?}|{:?}",
                            scan.projected_column_ids, scan.predicates, scan.limit
                        ),
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
            return Ok(Box::new(SnapshotStream {
                stats: Arc::clone(&self.stats),
                stats_key: key,
                rows: VecDeque::new(),
                columns: Vec::new(),
                column_rows: 0,
                ready: VecDeque::new(),
                prefetched: VecDeque::new(),
                stream: Some(stream),
                prewhere: build_prewhere_spec(scan, snapshot),
                key_position,
                started: false,
                types,
                retained_bytes: stream_overhead,
                remaining: scan
                    .predicates
                    .is_empty()
                    .then_some(scan.limit)
                    .flatten()
                    .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX)),
                // A filtered aggregate over a settled snapshot is just as
                // much a pure function of the data version as a bare one —
                // the predicates and limit simply join the memo key (issue
                // #6: Q2/Q5/Q7 were excluded for no sound reason).
                settled: snapshot.settled_identity().map(|(directory, generation)| {
                    (
                        directory.to_path_buf(),
                        generation,
                        format!(
                            "{:?}|{:?}|{:?}",
                            scan.projected_column_ids, scan.predicates, scan.limit
                        ),
                    )
                }),
                delta,
                sma,
            }));
        }
        let projected = snapshot
            .scan_projected_range_bounded_pruned(
                &start,
                &end,
                &physical_column_ids,
                memory_limit - stream_overhead,
                &value_bounds,
            )
            .map_err(|error| match error {
                StoreError::MemoryLimitExceeded {
                    used,
                    requested,
                    limit: _,
                } => ExecError::MemoryLimitExceeded {
                    used: used.saturating_add(stream_overhead),
                    requested,
                    limit: memory_limit,
                },
                other => ExecError::Source(other.to_string()),
            })?;
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
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            rows.truncate(limit);
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
            prefetched: VecDeque::new(),
            stream: None,
            prewhere: None,
            key_position: None,
            started: true,
            types,
            retained_bytes,
            remaining: None,
            settled: None,
            delta: None,
            sma,
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
            | Value::Binary(_),
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
}

struct SnapshotStream {
    /// Shared with the provider that opened this stream. A scan's block
    /// counters are only known once chunks are pulled, long after `open_scan`
    /// returned, so the stream folds each chunk's stats in as it goes.
    stats: Arc<Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>>,
    stats_key: (DatabaseId, TableId),
    rows: VecDeque<Vec<Value>>,
    /// Batches adopted ahead of time in the worker pool (no-LIMIT scans).
    ready: VecDeque<RecordBatch>,
    columns: Vec<DecodedColumn>,
    column_rows: usize,
    prewhere: Option<PrewhereSpec>,
    /// Projected position of the table's single primary-key column, when
    /// projected — the only column a probe-side restriction can prune on.
    key_position: Option<usize>,
    /// Whether any batch was pulled; restrictions are ignored afterwards.
    started: bool,
    prefetched: VecDeque<ProjectedColumnChunk>,
    stream: Option<ProjectedScanStream>,
    types: Vec<pintail_types::DataType>,
    retained_bytes: usize,
    remaining: Option<usize>,
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
}

impl SnapshotStream {
    /// Folds one chunk's counters into the provider's per-table totals.
    fn accumulate(&self, stats: ScanStats) {
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
        let per_row = batch_memory_upper_bound(&self.types, 1).max(1);
        let affordable = (budget / per_row).max(1);
        DEFAULT_BATCH_ROWS.min(affordable)
    }
}

impl BatchStream for SnapshotStream {
    fn settled_identity(&self) -> Option<(std::path::PathBuf, u64, String)> {
        self.settled.clone()
    }

    fn sma_fold_input(&self) -> Option<crate::execution::SmaFoldInput> {
        self.sma.clone()
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
            let planned_rows = {
                let per_row = batch_memory_upper_bound(&self.types, 1).max(1);
                DEFAULT_BATCH_ROWS.min((available_memory / per_row).max(1))
            };
            let batch_overhead = batch_memory_upper_bound(&self.types, planned_rows);
            if self.prefetched.is_empty() {
                let prefetch_width = if self.types.len() <= 2 { 8 } else { 4 };
                let chunk_budget = available_memory.saturating_sub(batch_overhead);
                let chunks = if let Some(spec) = &self.prewhere {
                    let select = |columns: &[DecodedColumn], row_count: usize| {
                        prewhere_ranges(spec, columns, row_count)
                    };
                    stream
                        .next_column_chunks_filtered(
                            prefetch_width,
                            chunk_budget,
                            &spec.predicate_ids,
                            &select,
                        )
                        .map_err(|error| ExecError::Source(error.to_string()))?
                } else {
                    stream
                        .next_column_chunks(prefetch_width, chunk_budget)
                        .map_err(|error| ExecError::Source(error.to_string()))?
                };
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
                    self.accumulate(chunk.stats());
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
                    .map(|chunk| adopt_chunk(chunk, &self.types))
                    .collect::<Result<Vec<_>, ExecError>>()?;
                for (batches, chunk_bytes) in adopted {
                    released = released.saturating_add(chunk_bytes);
                    for batch in batches {
                        self.retained_bytes =
                            self.retained_bytes.saturating_add(batch.estimated_bytes());
                        self.ready.push_back(batch);
                    }
                }
                self.retained_bytes = self.retained_bytes.saturating_sub(released);
                break;
            }
            let chunk = self
                .prefetched
                .pop_front()
                .expect("non-empty prefetch batch");
            self.column_rows = chunk.row_count();
            self.columns = chunk.into_decoded_columns();
        }
        if let Some(batch) = self.ready.pop_front() {
            self.retained_bytes = self.retained_bytes.saturating_sub(batch.estimated_bytes());
            return Ok(Some(batch));
        }
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
                .map(|(data_type, column)| column_vector_from_decoded(data_type, column))
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
                .map(|(data_type, values)| {
                    ColumnVector::new(data_type, values).map_err(ExecError::from)
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

    fn restrict_key_position_range(&mut self, position: usize, min: &Value, max: &Value) {
        if self.started || self.key_position != Some(position) {
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
        if let Ok(Some(rebuilt)) =
            stream
                .snapshot()
                .scan_projected_range_stream(&new_start, &new_end, &column_ids)
        {
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
            (BoundDomain::Temporal(NativeUnits::DateTime { .. }), Value::Utf8(text)) => {
                pintail_types::parse_datetime_micros(text).map(i128::from)
            }
            _ => None,
        }
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

/// Builds the filter-first spec for a scan: every predicate must reference
/// only projected columns and compile against the predicate-subset layout.
fn build_prewhere_spec(scan: &Scan, snapshot: &TableSnapshot) -> Option<PrewhereSpec> {
    if scan.predicates.is_empty() {
        return None;
    }
    let mut predicate_ids = Vec::new();
    for predicate in &scan.predicates {
        collect_predicate_columns(predicate, &mut predicate_ids);
    }
    predicate_ids.sort_unstable();
    predicate_ids.dedup();
    if predicate_ids.is_empty()
        || !predicate_ids
            .iter()
            .all(|id| scan.projected_column_ids.contains(id))
        || predicate_ids.len() >= scan.projected_column_ids.len()
    {
        // Nothing beyond the predicate columns to skip: two-phase decode
        // could only add work.
        return None;
    }
    let mut layout = Vec::with_capacity(predicate_ids.len());
    let mut data_types = Vec::with_capacity(predicate_ids.len());
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
            relation_name: scan.table.table_name.clone(),
            name: column.name().to_owned(),
            data_type: column.data_type(),
            nullable: column.is_nullable(),
            using_shadowed: false,
        });
        data_types.push(column.data_type());
    }
    let predicates = scan
        .predicates
        .iter()
        .map(|predicate| crate::expression::CompiledExpr::compile(predicate, &layout))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(PrewhereSpec {
        predicate_ids,
        predicates,
        data_types,
    })
}

fn collect_predicate_columns(expr: &BoundExpr, ids: &mut Vec<u32>) {
    match &expr.kind {
        BoundExprKind::Column(column) => ids.push(column.column_id),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
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

/// Evaluates the compiled predicates over one chunk's predicate columns and
/// returns the surviving row ranges (coalesced), or `None` when the chunk
/// cannot or need not be restricted.
fn prewhere_ranges(
    spec: &PrewhereSpec,
    columns: &[DecodedColumn],
    row_count: usize,
) -> Result<Option<Vec<std::ops::Range<usize>>>, String> {
    /// Runs separated by fewer than this many rows merge, so near-adjacent
    /// survivors decode as one block-friendly region.
    const COALESCE_GAP: usize = 1024;
    let vectors = spec
        .data_types
        .iter()
        .zip(columns)
        .map(|(data_type, column)| column_vector_from_decoded(*data_type, column.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let batch = RecordBatch::new(row_count, vectors).map_err(|error| error.to_string())?;
    let mut combined: Option<crate::batch::SelectionMask> = None;
    for predicate in &spec.predicates {
        let Some(mask) = predicate
            .evaluate_filter_mask(&batch)
            .map_err(|error| error.to_string())?
        else {
            // A predicate outside the typed kernels: keep every row; the
            // Filter operator above applies the exact mask.
            return Ok(None);
        };
        match &mut combined {
            None => combined = Some(mask),
            Some(existing) => existing
                .intersect(&mask)
                .map_err(|error| error.to_string())?,
        }
    }
    let Some(mask) = combined else {
        return Ok(None);
    };
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut row = 0;
    while row < row_count {
        if !mask.is_selected(row) {
            row += 1;
            continue;
        }
        let start = row;
        while row < row_count && mask.is_selected(row) {
            row += 1;
        }
        match ranges.last_mut() {
            Some(last) if start.saturating_sub(last.end) < COALESCE_GAP => last.end = row,
            _ => ranges.push(start..row),
        }
    }
    if ranges.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // Two-phase decode reads the predicate columns twice, so it only pays
    // off when it skips real bytes. Scattered survivors coalesce into
    // near-full coverage (Q5's uniform date filter regressed 2x this way);
    // above 90% coverage, plain decode wins.
    let selected: usize = ranges.iter().map(std::iter::ExactSizeIterator::len).sum();
    if selected.saturating_mul(10) >= row_count.saturating_mul(9) {
        return Ok(None);
    }
    Ok(Some(ranges))
}

/// Converts one decoded chunk into ready record batches: slices of
/// `DEFAULT_BATCH_ROWS`, each column adopted into its typed executor form.
/// Returns the batches plus the chunk's retained-byte figure to release.
fn adopt_chunk(
    chunk: ProjectedColumnChunk,
    types: &[pintail_types::DataType],
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
        let take = row_count.min(DEFAULT_BATCH_ROWS);
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
            .map(|(data_type, column)| column_vector_from_decoded(data_type, column))
            .collect::<Result<Vec<_>, _>>()?;
        batches.push(RecordBatch::new(take, vectors)?);
        row_count -= take;
    }
    Ok((batches, chunk_bytes))
}

/// Adopts one store-decoded column as a typed executor vector, parsing
/// text-carried decimals and temporals once from the arena; row values
/// materialize lazily only if a row-shaped consumer asks. Falls back to
/// row values when the packed shape does not match the declared type.
#[allow(clippy::too_many_lines)]
fn column_vector_from_decoded(
    data_type: pintail_types::DataType,
    decoded: DecodedColumn,
) -> Result<ColumnVector, ExecError> {
    let storage = data_type.storage_type();
    match decoded {
        DecodedColumn::Values(values) => {
            ColumnVector::new(data_type, values).map_err(ExecError::from)
        }
        DecodedColumn::Int64 { values, validity }
            if matches!(storage, pintail_types::DataType::Int64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::Int64(values),
                ValidityMask::from_bools(&validity),
            ))
        }
        DecodedColumn::UInt64 { values, validity }
            if matches!(storage, pintail_types::DataType::UInt64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::UInt64(values),
                ValidityMask::from_bools(&validity),
            ))
        }
        DecodedColumn::Float64 { bits, validity }
            if matches!(storage, pintail_types::DataType::Float64) =>
        {
            Ok(ColumnVector::from_typed(
                data_type,
                TypedValues::Float64(bits.into_iter().map(f64::from_bits).collect()),
                ValidityMask::from_bools(&validity),
            ))
        }
        DecodedColumn::Utf8 {
            heap,
            offsets,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            Ok(typed_from_utf8_arena(data_type, &heap, &offsets, &validity))
        }
        DecodedColumn::DictionaryUtf8 {
            dict_heap,
            dict_offsets,
            codes,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            let mask = ValidityMask::from_bools(&validity);
            if matches!(
                data_type,
                pintail_types::DataType::Utf8 | pintail_types::DataType::Json
            ) {
                // Straight to view templates: one 16-byte view per row,
                // dictionary bytes as the only heap.
                let column =
                    StrColumn::from_dictionary(&dict_heap, &dict_offsets, &codes, &validity);
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
            for (code, valid) in codes.iter().zip(&validity) {
                if *valid {
                    let code = *code as usize;
                    heap.extend_from_slice(&dict_heap[dict_offsets[code]..dict_offsets[code + 1]]);
                }
                offsets.push(heap.len());
            }
            Ok(typed_from_utf8_arena(data_type, &heap, &offsets, &validity))
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
            let typed = match (units, data_type) {
                (
                    pintail_store::NativeUnits::Decimal { scale },
                    pintail_types::DataType::Decimal { .. },
                ) => Some(TypedValues::Decimal128 {
                    values: values.iter().copied().map(i128::from).collect(),
                    scale,
                    text: LazyText::decimal(scale),
                }),
                (pintail_store::NativeUnits::Date, pintail_types::DataType::Date32) => {
                    Some(TypedValues::Temporal {
                        units: values.clone(),
                        text: LazyText::date(),
                    })
                }
                (
                    pintail_store::NativeUnits::DateTime { fsp },
                    pintail_types::DataType::DateTime64 { .. },
                ) => Some(TypedValues::Temporal {
                    units: values.clone(),
                    text: LazyText::datetime(fsp),
                }),
                _ => None,
            };
            match typed {
                Some(typed) => {
                    let mask = ValidityMask::from_bools(&validity);
                    Ok(ColumnVector::from_typed(data_type, typed, mask))
                }
                // Unit kind and schema type disagree (defensive): fall back
                // to row values.
                None => ColumnVector::new(
                    data_type,
                    DecodedColumn::NativeUnits {
                        units,
                        values,
                        validity,
                    }
                    .into_values(),
                )
                .map_err(ExecError::from),
            }
        }
        decoded => ColumnVector::new(data_type, decoded.into_values()).map_err(ExecError::from),
    }
}

/// Builds the typed projection for a Utf8-carried column straight from the
/// decoded arena: plain strings become view columns; decimal and temporal
/// carriers additionally parse once into packed integers, mirroring
/// `build_typed`'s fallback to plain text if any non-null value fails.
fn typed_from_utf8_arena(
    data_type: pintail_types::DataType,
    heap: &[u8],
    offsets: &[usize],
    validity: &[bool],
) -> ColumnVector {
    let mut text = StrColumn::default();
    for row in 0..validity.len() {
        text.push(&heap[offsets[row]..offsets[row + 1]]);
    }
    let mask = ValidityMask::from_bools(validity);
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
                    values: packed,
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

#[cfg(test)]
mod tests {
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, memory_limit).expect("start execution");
        let mut values = Vec::new();
        while let Some(batch) = execution.next_batch().expect("pull batch") {
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
        let physical = PhysicalPlanner::plan(logical).expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024).expect("execution");

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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("CTE physical plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024).expect("CTE execution");
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024).expect("execution");
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
        let physical =
            PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound))).expect("plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024).expect("execution");
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

        let statement = parse_statement(
            "SELECT events.name AS event_name, users.name AS user_name \
             FROM events, users LIMIT 3",
        )
        .expect("parse query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution = Execution::start(physical, &provider, 64 * 1024).expect("execution");
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
             FROM events INNER JOIN users ON events.id = users.id",
        )
        .expect("parse hash join");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind hash join");
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("hash plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024).expect("hash execution");
        let batch = execution
            .next_batch()
            .expect("hash pull")
            .expect("hash batch");
        let rows = batch
            .selection()
            .selected_rows()
            .map(|row| {
                (
                    batch.column(0).expect("event").value(row).cloned(),
                    batch.column(1).expect("user").value(row).cloned(),
                )
            })
            .collect::<Vec<_>>();
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("CTE join plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024).expect("CTE join execution");
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
            "SELECT events.id, (SELECT MAX(id) FROM users) AS largest_user, \
             events.id IN (SELECT id FROM users WHERE id = 2) AS selected \
             FROM events ORDER BY events.id",
        )
        .expect("parse relational subqueries");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind relational subqueries");
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("relational subquery plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024).expect("subquery execution");
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("multi-row subquery plan");
        assert!(matches!(
            Execution::start(physical, &provider, 64 * 1024),
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, memory_limit).expect("start execution");
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
            vec![
                Value::UInt64(3),
                Value::UInt64(3),
                Value::UInt64(12),
                Value::UInt64(12),
                Value::UInt64(12),
            ]
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
            vec![
                Value::UInt64(3),
                Value::UInt64(3),
                Value::UInt64(15),
                Value::UInt64(15),
                Value::UInt64(15),
            ]
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
                    Value::UInt64(3),
                    Value::Utf8("20.0000".to_owned()),
                    Value::UInt64(2),
                ],
                vec![
                    Value::Utf8("b".to_owned()),
                    Value::UInt64(12),
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
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
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
            assert_eq!(row[2], Value::UInt64(id), "integer sums stay exact");
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
            assert_eq!(
                row[2],
                Value::Int64(bucket * 300),
                "integer sums stay exact"
            );
            // Bucket b holds ids {b, 1000+b, ..., 299000+b}, except bucket 0
            // whose members start at 1000 because ids begin at 1.
            let id_sum = if bucket == 0 {
                45_150_000
            } else {
                44_850_000 + 300 * u64::try_from(bucket).expect("bucket")
            };
            assert_eq!(row[3], Value::UInt64(id_sum), "id sums stay exact");
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
            assert_eq!(row[2], Value::Int64(2000), "SUM(amount)");
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
        assert_eq!(during_ingest[2], Value::Int64(2010));
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
        assert_eq!(overlaid[2], Value::Int64(2108), "updated amount replaces 2");
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
        assert_eq!(rows[0][0], Value::float64(2.0), "CEIL");
        assert_eq!(rows[0][1], Value::float64(-2.0), "FLOOR");
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
