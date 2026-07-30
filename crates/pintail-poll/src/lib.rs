//! Polling and primary-key reconciliation for Pintail.

mod cursor;
mod decoder;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use chrono::Utc;
use mysql_async::{Conn, Params, Pool, Row, Value as MysqlValue, prelude::Queryable as _};
use pintail_meta::{MetaStore, PollStateRecord, PollStateUpdate};
use pintail_probe::{ProbeReport, SourceColumn, SourceTable};
use pintail_snapshot::SnapshotError;
use pintail_store::{StoreError, TableStore};
use pintail_types::{KeyMode, PrimaryKey, SchemaError, StoredRow, Value};
use thiserror::Error;

use crate::{
    cursor::{CursorValue, ProbeToken},
    decoder::{decode_row, quote_identifier, source_projection},
};

/// One probed table and its existing snapshot store.
pub struct PollTarget {
    source: SourceTable,
    store: TableStore,
}

impl PollTarget {
    /// Validates and constructs a polling target.
    ///
    /// # Errors
    ///
    /// Returns an error when the store schema differs from the source.
    pub fn new(source: SourceTable, store: TableStore) -> Result<Self, PollError> {
        if store.schema() != &source.table_schema()? {
            return Err(PollError::InvalidConfiguration(format!(
                "store schema for {} does not match the probed source schema",
                source.name
            )));
        }
        Ok(Self { source, store })
    }

    /// Returns the probed source table.
    #[must_use]
    pub const fn source(&self) -> &SourceTable {
        &self.source
    }

    /// Returns the live target store.
    #[must_use]
    pub const fn store(&self) -> &TableStore {
        &self.store
    }

    /// Consumes the target and returns its store.
    #[must_use]
    pub fn into_store(self) -> TableStore {
        self.store
    }
}

/// Controls one polling cycle. Scheduling cadences belong to the supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollOptions {
    /// Source rows fetched per paginated query.
    pub chunk_rows: usize,
    /// Run the slower complete key reconciliation scheduled independently.
    pub reconcile: bool,
    /// Ignore an unchanged cheap-probe token.
    pub force: bool,
    /// Operator-selected cursor columns by case-insensitive table name.
    pub cursor_overrides: BTreeMap<String, String>,
    /// Operator-selected soft-delete columns by case-insensitive table name.
    pub soft_delete_columns: BTreeMap<String, String>,
    /// Tables explicitly scheduled for reconciliation even in CDC mode.
    pub reconcile_tables: BTreeSet<String>,
}

impl Default for PollOptions {
    fn default() -> Self {
        Self {
            chunk_rows: 10_000,
            reconcile: false,
            force: false,
            cursor_overrides: BTreeMap::new(),
            soft_delete_columns: BTreeMap::new(),
            reconcile_tables: BTreeSet::new(),
        }
    }
}

/// Polling strategy selected for one table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollStrategy {
    /// Inclusive boundary reread from a source cursor.
    Cursor,
    /// Complete keyed diff for a table without a safe cursor.
    KeyedChecksum,
    /// Complete generation replacement for a table without source identity.
    AppendRebuild,
}

/// Durable result for one table cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TablePollOutcome {
    /// Source table.
    pub table: String,
    /// Selected sync strategy.
    pub strategy: PollStrategy,
    /// Whether the source token or replica contents changed.
    pub changed: bool,
    /// Live rows ingested or replaced.
    pub ingested: usize,
    /// Missing or soft-deleted keys tombstoned.
    pub tombstones: usize,
    /// Source count observed by the cheap probe.
    pub source_count: u64,
    /// Durable poll version after this cycle.
    pub version: u64,
    /// Whether a full primary-key reconciliation completed.
    pub reconciled: bool,
}

/// Successful database polling cycle.
pub struct PollResult {
    /// Per-table outcomes in source-name order.
    pub tables: Vec<TablePollOutcome>,
    /// Updated targets in source-name order.
    pub targets: Vec<PollTarget>,
}

/// Polling failure.
#[derive(Debug, Error)]
pub enum PollError {
    /// Invalid source, cursor, or target configuration.
    #[error("invalid polling configuration: {0}")]
    InvalidConfiguration(String),
    /// `MySQL` query or protocol failure.
    #[error("MySQL polling failed: {0}")]
    Mysql(#[from] mysql_async::Error),
    /// Source row normalization failure.
    #[error("polling source mapping failed: {0}")]
    Snapshot(#[from] SnapshotError),
    /// Table WAL or segment failure.
    #[error("polling storage failed: {0}")]
    Store(#[from] StoreError),
    /// Schema or physical-key failure.
    #[error("polling schema failed: {0}")]
    Schema(#[from] SchemaError),
    /// `SQLite` control-plane failure.
    #[error("polling metadata failed: {0}")]
    Metadata(#[from] anyhow::Error),
    /// Cursor or token JSON failure.
    #[error("polling checkpoint JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Source rows did not match the probed schema.
    #[error("polling decode failed: {0}")]
    Decode(String),
}

/// Runs one cheap-probe, sync, and optional reconciliation cycle.
///
/// Every changed table WAL is synchronized before its cursor/version is
/// committed to `SQLite`.
///
/// # Errors
///
/// Returns the first source, mapping, storage, or metadata failure.
pub async fn run_poll_cycle(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    mut targets: Vec<PollTarget>,
    options: PollOptions,
) -> Result<PollResult, PollError> {
    validate_configuration(report, &targets, &options)?;
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
    let mut metadata = MetaStore::open(metadata_path)?;
    let mut connection = pool.get_conn().await?;
    let mut outcomes = Vec::with_capacity(targets.len());
    for target in &mut targets {
        outcomes.push(
            poll_table(
                &mut connection,
                &mut metadata,
                database_id,
                &report.database,
                target,
                &options,
            )
            .await?,
        );
    }
    Ok(PollResult {
        tables: outcomes,
        targets,
    })
}

fn validate_configuration(
    report: &ProbeReport,
    targets: &[PollTarget],
    options: &PollOptions,
) -> Result<(), PollError> {
    if targets.is_empty() {
        return Err(PollError::InvalidConfiguration(
            "polling requires at least one target".to_owned(),
        ));
    }
    if options.chunk_rows == 0 {
        return Err(PollError::InvalidConfiguration(
            "polling chunk size must be non-zero".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    for target in targets {
        if !names.insert(target.source.name.to_ascii_lowercase()) {
            return Err(PollError::InvalidConfiguration(format!(
                "duplicate polling target {}",
                target.source.name
            )));
        }
        if !report
            .tables
            .iter()
            .any(|table| table.name.eq_ignore_ascii_case(&target.source.name))
        {
            return Err(PollError::InvalidConfiguration(format!(
                "target {} is absent from the probe report",
                target.source.name
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn poll_table(
    connection: &mut Conn,
    metadata: &mut MetaStore,
    database_id: &str,
    source_database: &str,
    target: &mut PollTarget,
    options: &PollOptions,
) -> Result<TablePollOutcome, PollError> {
    let previous = metadata.poll_state(database_id, &target.source.name)?;
    let cursor = select_cursor(&target.source, options)?;
    let strategy = if target.source.key.mode == KeyMode::AppendRowId {
        PollStrategy::AppendRebuild
    } else if cursor.is_some() {
        PollStrategy::Cursor
    } else {
        PollStrategy::KeyedChecksum
    };
    let token = cheap_probe(connection, source_database, &target.source, cursor.as_ref()).await?;
    let token_json = token.encode()?;
    let token_changed = options.force
        || previous
            .as_ref()
            .and_then(|state| state.source_token_json.as_deref())
            != Some(token_json.as_str());
    let mut version = previous.as_ref().map_or(0, |state| state.version);
    let reconcile_requested = options.reconcile
        || target.source.requires_reconciliation
        || contains_case_insensitive(&options.reconcile_tables, &target.source.name);
    if !token_changed && !reconcile_requested && previous.is_some() {
        return Ok(TablePollOutcome {
            table: target.source.name.clone(),
            strategy,
            changed: false,
            ingested: 0,
            tombstones: 0,
            source_count: token.count,
            version,
            reconciled: false,
        });
    }
    version = version
        .checked_add(1)
        .ok_or_else(|| PollError::Decode("poll version exceeds UInt64".to_owned()))?;

    let soft_delete = option_for_table(&options.soft_delete_columns, &target.source.name)
        .map(|column| resolve_column(&target.source, column))
        .transpose()?
        .cloned();
    let mut cursor_json = previous
        .as_ref()
        .and_then(|state| state.cursor_json.clone());
    let (ingested, tombstones, reconciled) = match strategy {
        PollStrategy::Cursor => {
            let cursor = cursor.as_ref().expect("cursor strategy");
            let previous_cursor = previous_cursor(previous.as_ref(), cursor)?;
            let (ingested, soft_tombstones) = if token_changed {
                sync_cursor_rows(
                    connection,
                    source_database,
                    target,
                    cursor,
                    previous_cursor,
                    soft_delete.as_ref(),
                    version,
                    options.chunk_rows,
                )
                .await?
            } else {
                (0, 0)
            };
            cursor_json = match &token.maximum {
                CursorValue::Null => None,
                maximum => Some(maximum.encode()?),
            };
            let collision = has_unique_collision(target)?;
            let reconcile = reconcile_requested || collision;
            let missing = if reconcile {
                reconcile_missing_keys(
                    connection,
                    source_database,
                    target,
                    version,
                    options.chunk_rows,
                )
                .await?
            } else {
                0
            };
            (ingested, soft_tombstones + missing, reconcile)
        }
        PollStrategy::KeyedChecksum => {
            let (ingested, tombstones) = sync_complete_keyed_table(
                connection,
                source_database,
                target,
                soft_delete.as_ref(),
                version,
                options.chunk_rows,
            )
            .await?;
            (ingested, tombstones, true)
        }
        PollStrategy::AppendRebuild => {
            let ingested = sync_append_table(
                connection,
                source_database,
                target,
                version,
                options.chunk_rows,
            )
            .await?;
            (ingested, 0, true)
        }
    };
    if ingested > 0 || tombstones > 0 {
        target.store.checkpoint()?;
    }
    let update = PollStateUpdate {
        cursor_column: cursor.as_ref().map(|column| column.name.as_str()),
        cursor_json: cursor_json.as_deref(),
        source_token_json: Some(&token_json),
        source_count: token.count,
        version,
        reconciled,
    };
    metadata.commit_poll_state(
        database_id,
        &target.source.name,
        &update,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(TablePollOutcome {
        table: target.source.name.clone(),
        strategy,
        changed: token_changed || ingested > 0 || tombstones > 0,
        ingested,
        tombstones,
        source_count: token.count,
        version,
        reconciled,
    })
}

fn select_cursor(
    table: &SourceTable,
    options: &PollOptions,
) -> Result<Option<SourceColumn>, PollError> {
    if table.key.mode == KeyMode::AppendRowId {
        return Ok(None);
    }
    if let Some(override_name) = option_for_table(&options.cursor_overrides, &table.name) {
        return resolve_column(table, override_name).cloned().map(Some);
    }
    for candidate in ["updated_at", "updatedAt", "modified_at", "created_at"] {
        if let Some(column) = table
            .columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(candidate) && !column.nullable)
        {
            return Ok(Some(column.clone()));
        }
    }
    Ok(table
        .columns
        .iter()
        .find(|column| {
            column.auto_increment
                && table
                    .key
                    .columns
                    .iter()
                    .any(|key| key.eq_ignore_ascii_case(&column.name))
        })
        .cloned())
}

fn resolve_column<'a>(table: &'a SourceTable, column: &str) -> Result<&'a SourceColumn, PollError> {
    table
        .columns
        .iter()
        .find(|candidate| candidate.name.eq_ignore_ascii_case(column))
        .ok_or_else(|| {
            PollError::InvalidConfiguration(format!(
                "{} cursor/delete column {column} does not exist",
                table.name
            ))
        })
}

async fn cheap_probe(
    connection: &mut Conn,
    database: &str,
    table: &SourceTable,
    cursor: Option<&SourceColumn>,
) -> Result<ProbeToken, PollError> {
    let maximum = cursor
        .map(|column| quote_identifier(&column.name))
        .or_else(|| {
            table
                .key
                .columns
                .first()
                .map(|column| quote_identifier(column))
        })
        .map_or_else(|| "NULL".to_owned(), |column| format!("MAX({column})"));
    let sql = format!(
        "SELECT COUNT(*), {maximum} FROM {}.{}",
        quote_identifier(database),
        quote_identifier(&table.name)
    );
    let (count, maximum): (u64, MysqlValue) = connection
        .query_first(sql)
        .await?
        .ok_or_else(|| PollError::Decode("cheap probe returned no row".to_owned()))?;
    Ok(ProbeToken {
        count,
        maximum: maximum.into(),
    })
}

fn previous_cursor(
    previous: Option<&PollStateRecord>,
    cursor: &SourceColumn,
) -> Result<Option<MysqlValue>, PollError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    if !previous
        .cursor_column
        .as_deref()
        .is_some_and(|column| column.eq_ignore_ascii_case(&cursor.name))
    {
        return Ok(None);
    }
    previous
        .cursor_json
        .as_deref()
        .map(CursorValue::decode)
        .transpose()
        .map(|cursor| cursor.map(CursorValue::into_mysql))
}

#[allow(clippy::too_many_arguments)]
async fn sync_cursor_rows(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    cursor: &SourceColumn,
    previous_cursor: Option<MysqlValue>,
    soft_delete: Option<&SourceColumn>,
    version: u64,
    chunk_rows: usize,
) -> Result<(usize, usize), PollError> {
    let condition = previous_cursor
        .as_ref()
        .map(|_| format!(" WHERE {} >= ?", quote_identifier(&cursor.name)))
        .unwrap_or_default();
    let order = poll_order(&target.source, Some(cursor));
    let rows = fetch_rows(
        connection,
        database,
        &target.source,
        &condition,
        previous_cursor.into_iter().collect(),
        &order,
        chunk_rows,
    )
    .await?;
    let current = target
        .store
        .snapshot()
        .scan()?
        .into_iter()
        .map(|row| (row.key().clone(), row))
        .collect::<BTreeMap<_, _>>();
    let soft_delete_index = soft_delete.and_then(|column| {
        target
            .source
            .columns
            .iter()
            .position(|candidate| candidate.id == column.id)
    });
    let mut mutations = Vec::new();
    let mut ingested = 0;
    let mut tombstones = 0;
    for (index, source_row) in rows.into_iter().enumerate() {
        let decoded = decode_row(
            &target.source,
            source_row,
            u64::try_from(index + 1).map_err(|error| PollError::Decode(error.to_string()))?,
            version,
            false,
        )?;
        let deleted =
            soft_delete_index.is_some_and(|column| soft_delete_value(&decoded.values()[column]));
        if deleted {
            if current.contains_key(decoded.key()) {
                tombstones += 1;
                mutations.push(StoredRow::new(
                    decoded.key().clone(),
                    decoded.values().to_vec(),
                    version,
                    true,
                ));
            }
        } else if current
            .get(decoded.key())
            .is_none_or(|row| row.values() != decoded.values())
        {
            ingested += 1;
            mutations.push(decoded);
        }
    }
    target.store.ingest(mutations)?;
    Ok((ingested, tombstones))
}

async fn sync_complete_keyed_table(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    soft_delete: Option<&SourceColumn>,
    version: u64,
    chunk_rows: usize,
) -> Result<(usize, usize), PollError> {
    let rows = fetch_rows(
        connection,
        database,
        &target.source,
        "",
        Vec::new(),
        &poll_order(&target.source, None),
        chunk_rows,
    )
    .await?;
    let soft_delete_index = soft_delete.and_then(|column| {
        target
            .source
            .columns
            .iter()
            .position(|candidate| candidate.id == column.id)
    });
    let mut source = BTreeMap::<PrimaryKey, StoredRow>::new();
    for (index, row) in rows.into_iter().enumerate() {
        let decoded = decode_row(
            &target.source,
            row,
            u64::try_from(index + 1).map_err(|error| PollError::Decode(error.to_string()))?,
            version,
            false,
        )?;
        source.insert(decoded.key().clone(), decoded);
    }
    let current = target
        .store
        .snapshot()
        .scan()?
        .into_iter()
        .map(|row| (row.key().clone(), row))
        .collect::<BTreeMap<_, _>>();
    let mut mutations = Vec::new();
    let mut ingested = 0;
    let mut tombstones = 0;
    for (key, row) in &source {
        let deleted =
            soft_delete_index.is_some_and(|column| soft_delete_value(&row.values()[column]));
        if deleted {
            if current.contains_key(key) {
                tombstones += 1;
                mutations.push(StoredRow::new(
                    key.clone(),
                    row.values().to_vec(),
                    version,
                    true,
                ));
            }
        } else if current
            .get(key)
            .is_none_or(|current| current.values() != row.values())
        {
            ingested += 1;
            mutations.push(row.clone());
        }
    }
    for (key, row) in &current {
        if !source.contains_key(key) {
            tombstones += 1;
            mutations.push(StoredRow::new(
                key.clone(),
                row.values().to_vec(),
                version,
                true,
            ));
        }
    }
    target.store.ingest(mutations)?;
    Ok((ingested, tombstones))
}

async fn sync_append_table(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    version: u64,
    chunk_rows: usize,
) -> Result<usize, PollError> {
    let rows = fetch_rows(
        connection,
        database,
        &target.source,
        "",
        Vec::new(),
        "",
        chunk_rows,
    )
    .await?;
    let mut source_rows = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        source_rows.push(decode_row(
            &target.source,
            row,
            u64::try_from(index + 1).map_err(|error| PollError::Decode(error.to_string()))?,
            version,
            false,
        )?);
    }
    let mut current_values = target
        .store
        .snapshot()
        .scan()?
        .into_iter()
        .map(|row| format!("{:?}", row.values()))
        .collect::<Vec<_>>();
    let mut source_values = source_rows
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect::<Vec<_>>();
    current_values.sort_unstable();
    source_values.sort_unstable();
    if current_values == source_values {
        return Ok(0);
    }
    target.store.reset_for_resnapshot()?;
    let count = source_rows.len();
    target.store.ingest(source_rows)?;
    Ok(count)
}

async fn reconcile_missing_keys(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    version: u64,
    chunk_rows: usize,
) -> Result<usize, PollError> {
    let rows = fetch_rows(
        connection,
        database,
        &target.source,
        "",
        Vec::new(),
        &poll_order(&target.source, None),
        chunk_rows,
    )
    .await?;
    let mut source_keys = BTreeSet::new();
    for (index, row) in rows.into_iter().enumerate() {
        source_keys.insert(
            decode_row(
                &target.source,
                row,
                u64::try_from(index + 1).map_err(|error| PollError::Decode(error.to_string()))?,
                version,
                false,
            )?
            .key()
            .clone(),
        );
    }
    let current = target.store.snapshot().scan()?;
    let tombstones = current
        .into_iter()
        .filter(|row| !source_keys.contains(row.key()))
        .map(|row| StoredRow::new(row.key().clone(), row.values().to_vec(), version, true))
        .collect::<Vec<_>>();
    let count = tombstones.len();
    target.store.ingest(tombstones)?;
    Ok(count)
}

fn has_unique_collision(target: &PollTarget) -> Result<bool, PollError> {
    if target.source.unique_keys.is_empty() {
        return Ok(false);
    }
    let rows = target.store.snapshot().scan()?;
    for unique in &target.source.unique_keys {
        let indices = unique
            .iter()
            .map(|column| {
                target
                    .source
                    .columns
                    .iter()
                    .position(|candidate| candidate.name.eq_ignore_ascii_case(column))
                    .ok_or_else(|| {
                        PollError::Decode(format!(
                            "{} unique column {column} is absent",
                            target.source.name
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = BTreeSet::new();
        for row in &rows {
            let key = indices
                .iter()
                .map(|index| format!("{:?}", row.values()[*index]))
                .collect::<Vec<_>>();
            if !seen.insert(key) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn fetch_rows(
    connection: &mut Conn,
    database: &str,
    table: &SourceTable,
    condition: &str,
    parameters: Vec<MysqlValue>,
    order: &str,
    chunk_rows: usize,
) -> Result<Vec<Row>, PollError> {
    let mut output = Vec::new();
    let mut offset = 0_usize;
    loop {
        let sql = format!(
            "SELECT {} FROM {}.{}{}{} LIMIT {} OFFSET {}",
            source_projection(table),
            quote_identifier(database),
            quote_identifier(&table.name),
            condition,
            order,
            chunk_rows,
            offset
        );
        let rows: Vec<Row> = connection
            .exec(sql, Params::Positional(parameters.clone()))
            .await?;
        let fetched = rows.len();
        output.extend(rows);
        if fetched < chunk_rows {
            break;
        }
        offset = offset
            .checked_add(fetched)
            .ok_or_else(|| PollError::Decode("poll offset exceeds usize".to_owned()))?;
    }
    Ok(output)
}

fn poll_order(table: &SourceTable, cursor: Option<&SourceColumn>) -> String {
    let mut columns = Vec::new();
    if let Some(cursor) = cursor {
        columns.push(cursor.name.clone());
    }
    for key in &table.key.columns {
        if !columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(key))
        {
            columns.push(key.clone());
        }
    }
    if columns.is_empty() {
        String::new()
    } else {
        format!(
            " ORDER BY {}",
            columns
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

fn soft_delete_value(value: &Value) -> bool {
    !matches!(
        value,
        Value::Null | Value::Boolean(false) | Value::Int64(0) | Value::UInt64(0)
    )
}

fn option_for_table<'a>(options: &'a BTreeMap<String, String>, table: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(table))
        .map(|(_, value)| value.as_str())
}

fn contains_case_insensitive(values: &BTreeSet<String>, expected: &str) -> bool {
    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::{PollOptions, PollStrategy, contains_case_insensitive, soft_delete_value};
    use pintail_types::Value;

    #[test]
    fn soft_delete_and_case_insensitive_options_are_deterministic() {
        assert!(!soft_delete_value(&Value::Null));
        assert!(!soft_delete_value(&Value::UInt64(0)));
        assert!(soft_delete_value(&Value::UInt64(1)));
        let options = PollOptions {
            reconcile_tables: ["Events".to_owned()].into_iter().collect(),
            ..PollOptions::default()
        };
        assert!(contains_case_insensitive(
            &options.reconcile_tables,
            "events"
        ));
        assert_eq!(PollStrategy::Cursor, PollStrategy::Cursor);
    }
}
