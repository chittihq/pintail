use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_store::{ProjectedRow, ScanStats, StoreError, TableSnapshot};
use pintail_types::{KeyPart, PrimaryKey, Value};

use crate::{
    BatchStream, ColumnVector, DEFAULT_BATCH_ROWS, ExecError, RecordBatch, Scan, ScanProvider,
};

/// Storage scan provider backed by reader-pinned table snapshots.
pub struct SnapshotScanProvider<'snapshot> {
    snapshots: BTreeMap<(DatabaseId, TableId), &'snapshot TableSnapshot>,
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
            stats: Mutex::new(BTreeMap::new()),
        })
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

        let positions = scan
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
        let types = positions
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
                types,
                retained_bytes: stream_overhead,
            }));
        };
        let projected = snapshot
            .scan_projected_range_bounded(
                &start,
                &end,
                &scan.projected_column_ids,
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
        if scan.predicates.is_empty()
            && let Some(limit) = scan.limit
        {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            rows.truncate(limit);
            rows.shrink_to_fit();
        }
        let retained_bytes = rows.capacity() * std::mem::size_of::<ProjectedRow>()
            + rows
                .iter()
                .map(|row| {
                    row.estimated_bytes()
                        .saturating_sub(std::mem::size_of::<ProjectedRow>())
                })
                .sum::<usize>()
                .saturating_add(stream_overhead);
        Ok(Box::new(SnapshotStream {
            rows: rows.into(),
            types,
            retained_bytes,
        }))
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
        key_column.data_type(),
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
        (key, data_type),
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
    rows: VecDeque<ProjectedRow>,
    types: Vec<pintail_types::DataType>,
    retained_bytes: usize,
}

impl BatchStream for SnapshotStream {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.rows.is_empty() {
            return Ok(None);
        }
        let row_count = self.rows.len().min(DEFAULT_BATCH_ROWS);
        let mut values_by_column = self
            .types
            .iter()
            .map(|_| Vec::with_capacity(row_count))
            .collect::<Vec<_>>();
        for _ in 0..row_count {
            let row = self.rows.pop_front().expect("row count bounded above");
            self.retained_bytes = self.retained_bytes.saturating_sub(
                row.estimated_bytes()
                    .saturating_sub(std::mem::size_of::<ProjectedRow>()),
            );
            let values = row.into_values();
            if values.len() != self.types.len() {
                return Err(ExecError::InvalidBatch(
                    "stored row is shorter than its snapshot schema",
                ));
            }
            for (position, value) in values.into_iter().enumerate() {
                values_by_column[position].push(value);
            }
        }
        if self.rows.is_empty() {
            let capacity = self.rows.capacity();
            self.rows.shrink_to_fit();
            self.retained_bytes = self.retained_bytes.saturating_sub(
                capacity.saturating_sub(self.rows.capacity()) * std::mem::size_of::<ProjectedRow>(),
            );
        }
        let columns = self
            .types
            .iter()
            .copied()
            .zip(values_by_column)
            .map(|(data_type, values)| {
                ColumnVector::new(data_type, values).map_err(ExecError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(RecordBatch::new(row_count, columns)?))
    }

    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
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
        ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
        explain_analyze_statement,
    };

    fn execute_values(
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
    ) -> Vec<Value> {
        let statement = parse_statement(sql).expect("parse query");
        let bound = Binder::new(catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let physical = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical plan");
        let mut execution =
            Execution::start(physical, provider, 64 * 1024).expect("start execution");
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
}
