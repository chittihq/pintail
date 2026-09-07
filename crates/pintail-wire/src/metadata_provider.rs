use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{BatchStream, ColumnVector, ExecError, RecordBatch, Scan, ScanProvider};
use pintail_sql::{MetadataResult, SourceFacts};
use pintail_types::{Column, TableSchema};

use crate::engine::QueryError;

pub(crate) struct MetadataProvider {
    tables: Vec<MetadataResult>,
}

impl MetadataProvider {
    pub(crate) fn new(
        catalog: &CatalogSnapshot,
        facts: &SourceFacts,
    ) -> Result<(CatalogSnapshot, Self), QueryError> {
        let relations = pintail_sql::metadata_relations(catalog, facts);
        let mut entries = Vec::new();
        let mut tables = Vec::new();
        for (index, (name, result)) in relations.into_iter().enumerate() {
            let columns = result
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    Ok(Column::new(
                        u32::try_from(index).map_err(internal)?,
                        &field.name,
                        field.data_type,
                        field.nullable,
                    ))
                })
                .collect::<Result<Vec<_>, QueryError>>()?;
            let schema = TableSchema::new(1, columns).map_err(internal)?;
            entries.push(
                TableEntry::new(
                    TableId::new(u64::try_from(index).map_err(internal)?),
                    name,
                    schema,
                    TableStatistics::with_row_count(
                        u64::try_from(result.rows.len()).map_err(internal)?,
                    ),
                )
                .map_err(internal)?,
            );
            tables.push(result);
        }
        let database = DatabaseEntry::new(DatabaseId::new(0), "information_schema", entries)
            .map_err(internal)?;
        Ok((
            CatalogSnapshot::new([database]).map_err(internal)?,
            Self { tables },
        ))
    }
}
fn internal(error: impl std::fmt::Display) -> QueryError {
    QueryError::Internal(error.to_string())
}
impl ScanProvider for MetadataProvider {
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        let index = usize::try_from(scan.table.table_id.get())
            .map_err(|error| ExecError::Source(error.to_string()))?;
        let table = self
            .tables
            .get(index)
            .ok_or_else(|| ExecError::Source("metadata table missing".to_owned()))?;
        let columns = scan
            .projected_column_ids
            .iter()
            .map(|id| {
                let index =
                    usize::try_from(*id).map_err(|error| ExecError::Source(error.to_string()))?;
                ColumnVector::new(
                    table.fields[index].data_type,
                    table.rows.iter().map(|row| row[index].clone()).collect(),
                )
                .map_err(|error| ExecError::Source(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = RecordBatch::new(table.rows.len(), columns)
            .map_err(|error| ExecError::Source(error.to_string()))?;
        if batch.estimated_bytes() > memory_limit {
            return Err(ExecError::Source(
                "metadata scan exceeds query memory budget".to_owned(),
            ));
        }
        Ok(Box::new(MetadataStream(Some(batch))))
    }
}
struct MetadataStream(Option<RecordBatch>);
impl BatchStream for MetadataStream {
    fn next_batch(&mut self, _available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        Ok(self.0.take())
    }
    fn retained_bytes(&self) -> usize {
        self.0.as_ref().map_or(0, RecordBatch::estimated_bytes)
    }
    fn next_batch_memory_upper_bound(&self, _budget: usize) -> usize {
        0
    }
}
