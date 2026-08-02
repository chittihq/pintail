//! Polling and primary-key reconciliation for Pintail.

mod checksum;
mod cursor;
mod decoder;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use chrono::Utc;
use mysql_async::{Conn, Params, Pool, Row, Value as MysqlValue, prelude::Queryable as _};
use pintail_meta::{
    MetaStore, PollChunkStateRecord, PollChunkStateUpdate, PollStateRecord, PollStateUpdate,
};
use pintail_probe::{ProbeReport, SourceColumn, SourceTable};
use pintail_snapshot::SnapshotError;
use pintail_store::{StoreError, TableStore};
use pintail_types::{KeyMode, PrimaryKey, SchemaError, StoredRow, Value};
use thiserror::Error;

use crate::{
    checksum::{SourceChunk, replica_checksum, source_chunks},
    cursor::{CursorValue, ProbeToken},
    decoder::{decode_key, decode_row, key_projection, quote_identifier, source_projection},
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
        // Compare at the store's catalog generation: live DDL (ALTER,
        // TRUNCATE) advances the durable schema version, and a version-1
        // rebuild would reject every store that ever evolved even though
        // the column layout still matches.
        if store.schema() != &source.table_schema_with_version(store.schema().version())? {
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
    /// Report the cycle as changed even when its cheap-probe token is stable.
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
    /// Source aggregate chunks checked during this cycle.
    pub chunks_scanned: usize,
    /// Mismatched chunks whose full rows were fetched.
    pub chunks_redumped: usize,
    /// Stale rows tombstoned by targeted secondary-UNIQUE lookups.
    pub unique_repairs: usize,
}

/// Successful database polling cycle.
pub struct PollResult {
    /// Per-table outcomes in source-name order.
    pub tables: Vec<TablePollOutcome>,
    /// Updated targets in source-name order.
    pub targets: Vec<PollTarget>,
}

/// One CDC-side key reconciliation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdcReconcileOutcome {
    /// Reconciled child or operator-selected table.
    pub table: String,
    /// Source rows inserted or refreshed after an invisible cascade update.
    pub ingested: usize,
    /// Missing source keys tombstoned.
    pub tombstones: usize,
    /// Source keys observed.
    pub source_count: u64,
    /// Version assigned above all currently visible CDC rows.
    pub version: u64,
}

/// Successful reconciliation that preserves the CDC source checkpoint.
pub struct CdcReconcileResult {
    /// Per-table outcomes in source-name order.
    pub tables: Vec<CdcReconcileOutcome>,
    /// Updated targets in source-name order.
    pub targets: Vec<PollTarget>,
}

struct PollChunkCheckpoint {
    chunk_id: String,
    source_count: u64,
    source_checksum: String,
    replica_checksum: String,
}

struct TableSync {
    ingested: usize,
    tombstones: usize,
    reconciled: bool,
    chunks: Vec<PollChunkCheckpoint>,
    chunks_scanned: usize,
    chunks_redumped: usize,
    unique_repairs: usize,
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

/// Runs a full-row reconciliation for CDC tables whose source-side cascades
/// are absent from row binlogs.
///
/// Table WALs are synchronized before reconcile metadata. The database's CDC
/// mode and binlog checkpoint are left unchanged.
///
/// # Errors
///
/// Returns the first source, storage, schema, or metadata failure.
pub async fn run_cdc_reconciliation(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    mut targets: Vec<PollTarget>,
    chunk_rows: usize,
) -> Result<CdcReconcileResult, PollError> {
    if chunk_rows == 0 {
        return Err(PollError::InvalidConfiguration(
            "reconciliation chunk size must be non-zero".to_owned(),
        ));
    }
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
    let mut metadata = MetaStore::open(metadata_path)?;
    let mut connection = pool.get_conn().await?;
    let mut outcomes = Vec::with_capacity(targets.len());
    for target in &mut targets {
        if target.source.key.mode == KeyMode::AppendRowId {
            return Err(PollError::InvalidConfiguration(format!(
                "{} has no source key for CDC reconciliation",
                target.source.name
            )));
        }
        if !report
            .tables
            .iter()
            .any(|source| source.name.eq_ignore_ascii_case(&target.source.name))
        {
            return Err(PollError::InvalidConfiguration(format!(
                "target {} is absent from the probe report",
                target.source.name
            )));
        }
        outcomes.push(
            reconcile_cdc_target(
                &mut connection,
                &mut metadata,
                database_id,
                &report.database,
                target,
                chunk_rows,
            )
            .await?,
        );
    }
    Ok(CdcReconcileResult {
        tables: outcomes,
        targets,
    })
}

async fn reconcile_cdc_target(
    connection: &mut Conn,
    metadata: &mut MetaStore,
    database_id: &str,
    source_database: &str,
    target: &mut PollTarget,
    chunk_rows: usize,
) -> Result<CdcReconcileOutcome, PollError> {
    let current = target
        .store
        .snapshot()
        .scan()?
        .into_iter()
        .map(|row| (row.key().clone(), row))
        .collect::<BTreeMap<_, _>>();
    let durable_version = metadata
        .poll_state(database_id, &target.source.name)?
        .map_or(0, |state| state.version);
    let version = current
        .values()
        .map(StoredRow::version)
        .max()
        .unwrap_or(0)
        .max(durable_version)
        .checked_add(1)
        .ok_or_else(|| PollError::Decode("reconcile version exceeds UInt64".to_owned()))?;
    let rows = fetch_rows(
        connection,
        source_database,
        &target.source,
        "",
        Vec::new(),
        &poll_order(&target.source, None),
        chunk_rows,
    )
    .await?;
    let source_count =
        u64::try_from(rows.len()).map_err(|error| PollError::Decode(error.to_string()))?;
    let mut source_keys = BTreeSet::new();
    let mut mutations = Vec::new();
    let mut ingested = 0;
    for (index, row) in rows.into_iter().enumerate() {
        let decoded = decode_row(
            &target.source,
            row,
            u64::try_from(index + 1).map_err(|error| PollError::Decode(error.to_string()))?,
            version,
            false,
        )?;
        source_keys.insert(decoded.key().clone());
        if current
            .get(decoded.key())
            .is_none_or(|stored| stored.values() != decoded.values())
        {
            ingested += 1;
            mutations.push(decoded);
        }
    }
    let tombstones = current
        .into_values()
        .filter(|row| !source_keys.contains(row.key()))
        .map(|row| StoredRow::new(row.key().clone(), row.values().to_vec(), version, true))
        .collect::<Vec<_>>();
    let tombstone_count = tombstones.len();
    mutations.extend(tombstones);
    target.store.ingest_scan(mutations)?;
    if ingested > 0 || tombstone_count > 0 {
        target.store.checkpoint()?;
    }
    metadata.commit_cdc_reconciliation(
        database_id,
        &target.source.name,
        source_count,
        version,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(CdcReconcileOutcome {
        table: target.source.name.clone(),
        ingested,
        tombstones: tombstone_count,
        source_count,
        version,
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
        || contains_case_insensitive(&options.reconcile_tables, &target.source.name);
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
    let sync = match strategy {
        PollStrategy::Cursor => {
            let cursor = cursor.as_ref().expect("cursor strategy");
            let previous_cursor = previous_cursor(previous.as_ref(), cursor)?;
            let (ingested, soft_tombstones) = sync_cursor_rows(
                connection,
                source_database,
                target,
                cursor,
                previous_cursor,
                soft_delete.as_ref(),
                version,
                options.chunk_rows,
            )
            .await?;
            cursor_json = match &token.maximum {
                CursorValue::Null => None,
                maximum => Some(maximum.encode()?),
            };
            let collisions = unique_collision_keys(target)?;
            let repaired =
                repair_unique_collisions(connection, source_database, target, &collisions, version)
                    .await?;
            let missing = if reconcile_requested {
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
            TableSync {
                ingested,
                tombstones: soft_tombstones + repaired + missing,
                reconciled: reconcile_requested || repaired > 0,
                chunks: Vec::new(),
                chunks_scanned: 0,
                chunks_redumped: 0,
                unique_repairs: repaired,
            }
        }
        PollStrategy::KeyedChecksum => {
            let previous_chunks = metadata.poll_chunk_states(database_id, &target.source.name)?;
            sync_checksum_table(
                connection,
                source_database,
                target,
                soft_delete.as_ref(),
                version,
                options.chunk_rows,
                &previous_chunks,
                reconcile_requested,
            )
            .await?
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
            TableSync {
                ingested,
                tombstones: 0,
                reconciled: true,
                chunks: Vec::new(),
                chunks_scanned: 0,
                chunks_redumped: 0,
                unique_repairs: 0,
            }
        }
    };
    if sync.ingested > 0 || sync.tombstones > 0 {
        target.store.checkpoint()?;
    }
    let update = PollStateUpdate {
        cursor_column: cursor.as_ref().map(|column| column.name.as_str()),
        cursor_json: cursor_json.as_deref(),
        source_token_json: Some(&token_json),
        source_count: token.count,
        version,
        reconciled: sync.reconciled,
    };
    let now = Utc::now().to_rfc3339();
    if strategy == PollStrategy::KeyedChecksum {
        let chunks = sync
            .chunks
            .iter()
            .map(|chunk| PollChunkStateUpdate {
                chunk_id: &chunk.chunk_id,
                source_count: chunk.source_count,
                source_checksum: &chunk.source_checksum,
                replica_checksum: &chunk.replica_checksum,
            })
            .collect::<Vec<_>>();
        metadata.commit_poll_state_with_chunks(
            database_id,
            &target.source.name,
            &update,
            &chunks,
            &now,
        )?;
    } else {
        metadata.commit_poll_state(database_id, &target.source.name, &update, &now)?;
    }
    Ok(TablePollOutcome {
        table: target.source.name.clone(),
        strategy,
        changed: token_changed || sync.ingested > 0 || sync.tombstones > 0,
        ingested: sync.ingested,
        tombstones: sync.tombstones,
        source_count: token.count,
        version,
        reconciled: sync.reconciled,
        chunks_scanned: sync.chunks_scanned,
        chunks_redumped: sync.chunks_redumped,
        unique_repairs: sync.unique_repairs,
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
    target.store.ingest_scan(mutations)?;
    Ok((ingested, tombstones))
}

#[allow(clippy::too_many_arguments)]
async fn sync_checksum_table(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    soft_delete: Option<&SourceColumn>,
    version: u64,
    chunk_rows: usize,
    previous_chunks: &[PollChunkStateRecord],
    reconcile_requested: bool,
) -> Result<TableSync, PollError> {
    let source_chunks = source_chunks(connection, database, &target.source, chunk_rows).await?;
    let previous = previous_chunks
        .iter()
        .map(|chunk| (chunk.chunk_id.as_str(), chunk))
        .collect::<BTreeMap<_, _>>();
    let current_rows = target.store.snapshot().scan()?;
    let current = current_rows
        .iter()
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
    let mut redumped = 0;
    for chunk in &source_chunks {
        let local_checksum = checksum_slice(&current_rows, chunk, chunk_rows);
        let unchanged = previous.get(chunk.chunk_id.as_str()).is_some_and(|prior| {
            prior.source_count == chunk.source_count
                && prior.source_checksum == chunk.source_checksum
                && prior.replica_checksum == local_checksum
        });
        if unchanged {
            continue;
        }
        redumped += 1;
        let rows = fetch_rows_page(
            connection,
            database,
            &target.source,
            "",
            Vec::new(),
            &poll_order(&target.source, None),
            chunk_rows,
            chunk.offset,
        )
        .await?;
        for (index, row) in rows.into_iter().enumerate() {
            let decoded = decode_row(
                &target.source,
                row,
                u64::try_from(chunk.offset.saturating_add(index).saturating_add(1))
                    .map_err(|error| PollError::Decode(error.to_string()))?,
                version,
                false,
            )?;
            let deleted = soft_delete_index
                .is_some_and(|column| soft_delete_value(&decoded.values()[column]));
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
                .is_none_or(|current| current.values() != decoded.values())
            {
                ingested += 1;
                mutations.push(decoded);
            }
        }
    }
    target.store.ingest_scan(mutations)?;
    let reconcile = redumped > 0 || reconcile_requested;
    if reconcile {
        tombstones +=
            reconcile_missing_keys(connection, database, target, version, chunk_rows).await?;
    }
    let final_rows = target.store.snapshot().scan()?;
    let chunks = source_chunks
        .iter()
        .map(|chunk| PollChunkCheckpoint {
            chunk_id: chunk.chunk_id.clone(),
            source_count: chunk.source_count,
            source_checksum: chunk.source_checksum.clone(),
            replica_checksum: checksum_slice(&final_rows, chunk, chunk_rows),
        })
        .collect();
    Ok(TableSync {
        ingested,
        tombstones,
        reconciled: reconcile,
        chunks,
        chunks_scanned: source_chunks.len(),
        chunks_redumped: redumped,
        unique_repairs: 0,
    })
}

fn checksum_slice(rows: &[StoredRow], chunk: &SourceChunk, chunk_rows: usize) -> String {
    let start = chunk.offset.min(rows.len());
    let end = start.saturating_add(chunk_rows).min(rows.len());
    replica_checksum(&rows[start..end])
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
    target.store.ingest_scan(source_rows)?;
    Ok(count)
}

async fn reconcile_missing_keys(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    version: u64,
    chunk_rows: usize,
) -> Result<usize, PollError> {
    let source_keys = fetch_source_keys(connection, database, &target.source, chunk_rows).await?;
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

async fn fetch_source_keys(
    connection: &mut Conn,
    database: &str,
    table: &SourceTable,
    chunk_rows: usize,
) -> Result<BTreeSet<PrimaryKey>, PollError> {
    let order = poll_order(table, None);
    let mut output = BTreeSet::new();
    let mut last_key = None;
    loop {
        let (condition, parameters) = last_key.as_ref().map_or_else(
            || (String::new(), Vec::new()),
            |key: &PrimaryKey| {
                let columns = table
                    .key
                    .columns
                    .iter()
                    .map(|column| quote_identifier(column))
                    .collect::<Vec<_>>();
                let placeholders = vec!["?"; columns.len()];
                let comparison = if columns.len() == 1 {
                    format!("{} > ?", columns[0])
                } else {
                    format!("({}) > ({})", columns.join(","), placeholders.join(","))
                };
                (
                    format!(" WHERE {comparison}"),
                    key.parts()
                        .iter()
                        .map(key_part_mysql_value)
                        .collect::<Vec<_>>(),
                )
            },
        );
        let sql = format!(
            "SELECT {} FROM {}.{}{}{} LIMIT {}",
            key_projection(table),
            quote_identifier(database),
            quote_identifier(&table.name),
            condition,
            order,
            chunk_rows
        );
        let rows: Vec<Row> = connection.exec(sql, Params::Positional(parameters)).await?;
        let fetched = rows.len();
        for row in rows {
            let key = decode_key(table, row)?;
            last_key = Some(key.clone());
            output.insert(key);
        }
        if fetched < chunk_rows {
            break;
        }
    }
    Ok(output)
}

fn unique_collision_keys(target: &PollTarget) -> Result<BTreeSet<PrimaryKey>, PollError> {
    if target.source.unique_keys.is_empty() {
        return Ok(BTreeSet::new());
    }
    let rows = target.store.snapshot().scan()?;
    let mut collisions = BTreeSet::new();
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
        let mut seen = BTreeMap::<Vec<String>, PrimaryKey>::new();
        for row in &rows {
            let key = indices
                .iter()
                .map(|index| format!("{:?}", row.values()[*index]))
                .collect::<Vec<_>>();
            if let Some(previous) = seen.insert(key, row.key().clone()) {
                collisions.insert(previous);
                collisions.insert(row.key().clone());
            }
        }
    }
    Ok(collisions)
}

async fn repair_unique_collisions(
    connection: &mut Conn,
    database: &str,
    target: &mut PollTarget,
    collisions: &BTreeSet<PrimaryKey>,
    version: u64,
) -> Result<usize, PollError> {
    if collisions.is_empty() {
        return Ok(0);
    }
    let current = target
        .store
        .snapshot()
        .scan()?
        .into_iter()
        .map(|row| (row.key().clone(), row))
        .collect::<BTreeMap<_, _>>();
    let condition = target
        .source
        .key
        .columns
        .iter()
        .map(|column| format!("{} <=> ?", quote_identifier(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(
        "SELECT 1 FROM {}.{} WHERE {condition} LIMIT 1",
        quote_identifier(database),
        quote_identifier(&target.source.name)
    );
    let mut tombstones = Vec::new();
    for key in collisions {
        let parameters = key
            .parts()
            .iter()
            .map(key_part_mysql_value)
            .collect::<Vec<_>>();
        let exists: Option<u8> = connection
            .exec_first(&sql, Params::Positional(parameters))
            .await?;
        if exists.is_none()
            && let Some(row) = current.get(key)
        {
            tombstones.push(StoredRow::new(
                key.clone(),
                row.values().to_vec(),
                version,
                true,
            ));
        }
    }
    let repaired = tombstones.len();
    target.store.ingest(tombstones)?;
    Ok(repaired)
}

fn key_part_mysql_value(part: &pintail_types::KeyPart) -> MysqlValue {
    match part {
        pintail_types::KeyPart::Int64(value) => MysqlValue::Int(*value),
        pintail_types::KeyPart::UInt64(value) => MysqlValue::UInt(*value),
        pintail_types::KeyPart::Utf8(value) => MysqlValue::Bytes(value.as_bytes().to_vec()),
        pintail_types::KeyPart::Binary(value) => MysqlValue::Bytes(value.clone()),
    }
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
        let rows = fetch_rows_page(
            connection,
            database,
            table,
            condition,
            parameters.clone(),
            order,
            chunk_rows,
            offset,
        )
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

#[allow(clippy::too_many_arguments)]
async fn fetch_rows_page(
    connection: &mut Conn,
    database: &str,
    table: &SourceTable,
    condition: &str,
    parameters: Vec<MysqlValue>,
    order: &str,
    chunk_rows: usize,
    offset: usize,
) -> Result<Vec<Row>, PollError> {
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
    connection
        .exec(sql, Params::Positional(parameters))
        .await
        .map_err(PollError::Mysql)
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
    use super::{
        PollOptions, PollStrategy, PollTarget, contains_case_insensitive, soft_delete_value,
    };
    use pintail_probe::{SourceColumn, SourceKey, SourceTable};
    use pintail_store::{StoreOptions, TableStore};
    use pintail_types::{DataType, KeyMode, Value};

    fn note_table() -> SourceTable {
        SourceTable {
            name: "audit_log".to_owned(),
            engine: Some("InnoDB".to_owned()),
            estimated_rows: Some(1),
            columns: vec![SourceColumn {
                id: 1,
                name: "note".to_owned(),
                mysql_data_type: "varchar".to_owned(),
                mysql_column_type: "varchar(128)".to_owned(),
                pintail_type: DataType::Utf8,
                nullable: false,
                character_set: Some("utf8mb4".to_owned()),
                collation: Some("utf8mb4_0900_ai_ci".to_owned()),
                generated_stored: false,
                auto_increment: false,
            }],
            key: SourceKey {
                mode: KeyMode::AppendRowId,
                index_name: None,
                columns: Vec::new(),
            },
            unique_keys: Vec::new(),
            requires_reconciliation: false,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn poll_target_accepts_stores_at_evolved_schema_versions() {
        let source = note_table();
        let directory = tempfile::tempdir().expect("temp store directory");
        let mut store = TableStore::open(
            directory.path(),
            source.table_schema().expect("schema"),
            StoreOptions::default(),
        )
        .expect("open store");
        // A live TRUNCATE or ALTER advances the durable schema version while
        // keeping the column layout; polling must still accept the store.
        store
            .evolve_schema(source.table_schema_with_version(2).expect("schema v2"))
            .expect("evolve schema");
        let target = PollTarget::new(source.clone(), store).expect("evolved store is accepted");
        let mut renamed = source;
        renamed.columns[0].name = "message".to_owned();
        renamed.key.columns = Vec::new();
        let Err(error) = PollTarget::new(renamed, target.into_store()) else {
            panic!("drifted column layout must be rejected");
        };
        assert!(error.to_string().contains("does not match"));
    }

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
