use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_store::{
    DecodedColumn, ProjectedColumnChunk, ProjectedRow, ProjectedScanStream, ScanStats, StoreError,
    TableSnapshot,
};
use pintail_types::{KeyPart, PrimaryKey, Value};

use crate::{
    BatchStream, ColumnVector, DEFAULT_BATCH_ROWS, ExecError, RecordBatch, Scan, ScanProvider,
    array::{StrColumn, ValidityMask},
    batch::{TypedValues, parse_date_days, parse_datetime_micros, parse_decimal_scaled},
};

/// Storage scan provider backed by reader-pinned table snapshots.
pub struct SnapshotScanProvider<'snapshot> {
    snapshots: BTreeMap<(DatabaseId, TableId), &'snapshot TableSnapshot>,
    unique_visibility: BTreeMap<(DatabaseId, TableId), Vec<Vec<u32>>>,
    stats: Mutex<BTreeMap<(DatabaseId, TableId), PhysicalScanStats>>,
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
            stats: Mutex::new(BTreeMap::new()),
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
                rows: VecDeque::new(),
                columns: Vec::new(),
                column_rows: 0,
                prefetched: VecDeque::new(),
                stream: None,
                key_position: None,
                started: true,
                types,
                retained_bytes: stream_overhead,
                remaining: None,
            }));
        };
        let unique_keys = self.unique_visibility.get(&key);
        let mut physical_column_ids = scan.projected_column_ids.clone();
        if let Some(unique_keys) = unique_keys {
            for column_id in unique_keys.iter().flatten() {
                if !physical_column_ids.contains(column_id) {
                    physical_column_ids.push(*column_id);
                }
            }
        }
        if unique_keys.is_none()
            && let Some(stream) = snapshot
                .scan_projected_range_stream(&start, &end, &physical_column_ids)
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
            return Ok(Box::new(SnapshotStream {
                rows: VecDeque::new(),
                columns: Vec::new(),
                column_rows: 0,
                prefetched: VecDeque::new(),
                stream: Some(stream),
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
            }));
        }
        let projected = snapshot
            .scan_projected_range_bounded(
                &start,
                &end,
                &physical_column_ids,
                memory_limit - stream_overhead,
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
            rows: rows.into(),
            columns: Vec::new(),
            column_rows: 0,
            prefetched: VecDeque::new(),
            stream: None,
            key_position: None,
            started: true,
            types,
            retained_bytes,
            remaining: None,
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

struct SnapshotStream {
    rows: VecDeque<Vec<Value>>,
    columns: Vec<DecodedColumn>,
    column_rows: usize,
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
}

impl BatchStream for SnapshotStream {
    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        self.started = true;
        while self.rows.is_empty()
            && self.column_rows == 0
            && self.remaining != Some(0)
            && let Some(stream) = &mut self.stream
        {
            let batch_overhead = batch_memory_upper_bound(&self.types, DEFAULT_BATCH_ROWS);
            if self.prefetched.is_empty() {
                let prefetch_width = if self.types.len() <= 2 { 8 } else { 4 };
                let chunks = stream
                    .next_column_chunks(
                        prefetch_width,
                        available_memory.saturating_sub(batch_overhead),
                    )
                    .map_err(|error| ExecError::Source(error.to_string()))?;
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
                    self.prefetched.push_back(chunk);
                }
            }
            let chunk = self
                .prefetched
                .pop_front()
                .expect("non-empty prefetch batch");
            self.column_rows = chunk.row_count();
            self.columns = chunk.into_decoded_columns();
        }
        if self.rows.is_empty() && self.column_rows == 0 {
            return Ok(None);
        }
        let buffered_rows = if self.column_rows > 0 {
            self.column_rows
        } else {
            self.rows.len()
        };
        let row_count = buffered_rows
            .min(DEFAULT_BATCH_ROWS)
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

    fn next_batch_memory_upper_bound(&self) -> usize {
        let row_count = self
            .rows
            .len()
            .max(self.column_rows)
            .min(DEFAULT_BATCH_ROWS);
        if row_count == 0 {
            return self.stream.as_ref().map_or(0, |_| {
                batch_memory_upper_bound(&self.types, DEFAULT_BATCH_ROWS)
            });
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

/// Adopts one store-decoded column as a typed executor vector, parsing
/// text-carried decimals and temporals once from the arena; row values
/// materialize lazily only if a row-shaped consumer asks. Falls back to
/// row values when the packed shape does not match the declared type.
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
        DecodedColumn::NativeUnits {
            units,
            values,
            validity,
        } if matches!(storage, pintail_types::DataType::Utf8) => {
            // PTSEG v2 unit columns: the packed integers ARE the typed
            // representation — no text parse. Text views are formatted once
            // for the consumers that still need them (output, group keys).
            let mut text = StrColumn::default();
            for (row, valid) in validity.iter().enumerate() {
                if *valid {
                    let value = units
                        .format(values[row])
                        .expect("stored native units round-trip");
                    text.push(value.as_bytes());
                } else {
                    text.push(&[]);
                }
            }
            let mask = ValidityMask::from_bools(&validity);
            let typed = match (units, data_type) {
                (
                    pintail_store::NativeUnits::Decimal { scale },
                    pintail_types::DataType::Decimal { .. },
                ) => TypedValues::Decimal128 {
                    values: values.into_iter().map(i128::from).collect(),
                    scale,
                    text,
                },
                (
                    pintail_store::NativeUnits::Date | pintail_store::NativeUnits::DateTime { .. },
                    pintail_types::DataType::Date32 | pintail_types::DataType::DateTime64 { .. },
                ) => TypedValues::Temporal {
                    units: values,
                    text,
                },
                _ => TypedValues::Utf8(text),
            };
            Ok(ColumnVector::from_typed(data_type, typed, mask))
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
                    text,
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
                TypedValues::Temporal { units, text }
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
        let statement = parse_statement(sql).expect("parse query");
        let bound = Binder::new(catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, 64 * 1024 * 1024).expect("start execution");
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
    fn windows_reject_unsupported_combinations() {
        let (_directory, _snapshot, catalog) = window_fixture();
        for sql in [
            "SELECT name, SUM(id), ROW_NUMBER() OVER (ORDER BY name) FROM events GROUP BY name",
            "SELECT ROW_NUMBER() OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) FROM events",
            "SELECT id + ROW_NUMBER() OVER (ORDER BY id) FROM events",
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
}
