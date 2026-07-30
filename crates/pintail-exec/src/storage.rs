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
