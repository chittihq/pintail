use std::collections::{BTreeMap, VecDeque};

use pintail_catalog::{DatabaseId, TableId};
use pintail_store::TableSnapshot;

use crate::{
    BatchStream, ColumnVector, DEFAULT_BATCH_ROWS, ExecError, RecordBatch, Scan, ScanProvider,
};

/// Storage scan provider backed by reader-pinned table snapshots.
pub struct SnapshotScanProvider<'snapshot> {
    snapshots: BTreeMap<(DatabaseId, TableId), &'snapshot TableSnapshot>,
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
        Ok(Self { snapshots: indexed })
    }
}

impl ScanProvider for SnapshotScanProvider<'_> {
    fn open_scan(&self, scan: &Scan) -> Result<Box<dyn BatchStream>, ExecError> {
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

        let mut rows = snapshot
            .scan()
            .map_err(|error| ExecError::Source(error.to_string()))?;
        if scan.predicates.is_empty()
            && let Some(limit) = scan.limit
        {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            rows.truncate(limit);
        }

        let mut batches = VecDeque::new();
        for rows in rows.chunks(DEFAULT_BATCH_ROWS) {
            let columns = positions
                .iter()
                .zip(&types)
                .map(|(position, data_type)| {
                    let values = rows
                        .iter()
                        .map(|row| {
                            row.values()
                                .get(*position)
                                .cloned()
                                .ok_or(ExecError::InvalidBatch(
                                    "stored row is shorter than its snapshot schema",
                                ))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    ColumnVector::new(*data_type, values).map_err(ExecError::from)
                })
                .collect::<Result<Vec<_>, _>>()?;
            batches.push_back(RecordBatch::new(rows.len(), columns)?);
        }

        Ok(Box::new(SnapshotStream { batches }))
    }
}

struct SnapshotStream {
    batches: VecDeque<RecordBatch>,
}

impl BatchStream for SnapshotStream {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        Ok(self.batches.pop_front())
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

    use crate::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};

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
        assert_eq!(
            batch.column(0).and_then(|column| column.value(1)),
            Some(&Value::Utf8("Beta".to_owned()))
        );
        assert_eq!(batch.visible_row_count(), 1);
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
    #[allow(clippy::too_many_lines)]
    fn executes_guarded_cross_joins_in_bounded_output_batches() {
        let events_directory = tempfile::tempdir().expect("events directory");
        let users_directory = tempfile::tempdir().expect("users directory");
        let schema = schema();
        let mut events = TableStore::open(
            events_directory.path(),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("open events");
        let mut users = TableStore::open(
            users_directory.path(),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("open users");
        events
            .ingest(vec![row(1, "event-a"), row(2, "event-b")])
            .expect("ingest events");
        users
            .ingest(vec![row(1, "user-a"), row(2, "user-b")])
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
                    schema,
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
}
