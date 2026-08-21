//! Consistent, resumable parallel `MySQL` snapshots for Pintail.
//!
//! The coordinator briefly acquires a global read lock, captures the CDC
//! start position, opens every worker's repeatable-read consistent snapshot,
//! and releases the lock before table data is copied. Each completed keyset
//! page is published directly as a checksummed Pintail segment and then
//! checkpointed in `SQLite`.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use futures_util::future::join_all;
use mysql_async::{
    IsolationLevel, Params, Pool, Row, Transaction, TxOpts, Value as MysqlValue, prelude::Queryable,
};
use pintail_meta::{MetaStore, SnapshotChunkStatus};
use pintail_probe::{ProbeReport, SourceColumn, SourceFlavor, SourceTable};
use pintail_store::{StoreError, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, SchemaError, StoredRow, Value};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

/// Runtime controls for one database snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotOptions {
    /// Maximum simultaneously open source snapshot transactions.
    pub workers: usize,
    /// Source rows per durable chunk.
    pub chunk_rows: usize,
    /// Continue with per-worker consistent snapshots if the brief global lock
    /// is not permitted. The result records the degraded guarantee.
    pub allow_degraded_lock: bool,
    /// Optional execution budget used by supervisors and deterministic resume
    /// tests. Reaching it returns [`SnapshotError::Paused`] after the preceding
    /// chunk is durable.
    pub max_new_chunks: Option<usize>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            // PINTAIL_SNAPSHOT_WORKERS caps copy parallelism for hosts
            // where four workers saturate CPU or disk and starve the query
            // and dashboard paths sharing the process. Clamped to [1, 16];
            // unset or unparsable keeps the tuned default of 4.
            workers: std::env::var("PINTAIL_SNAPSHOT_WORKERS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map_or(4, |workers| workers.clamp(1, 16)),
            chunk_rows: 100_000,
            allow_degraded_lock: true,
            max_new_chunks: None,
        }
    }
}

/// Source position captured while writes were briefly locked.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotPosition {
    /// `MySQL` or `MariaDB` GTID set.
    Gtid {
        /// Server-reported executed/binlog GTID set.
        set: String,
        /// File/position captured alongside the GTID when available.
        file: Option<String>,
        /// File offset captured alongside the GTID when available.
        position: Option<u64>,
    },
    /// Classic binlog file and offset.
    FilePosition {
        /// Binlog file name.
        file: String,
        /// Event offset.
        position: u64,
    },
    /// Binary logging is unavailable, as expected for polling-only sources.
    Unavailable,
}

/// One table/store pair consumed by the snapshot run.
pub struct SnapshotTarget {
    source: SourceTable,
    store: TableStore,
}

impl SnapshotTarget {
    /// Validates and constructs a source-to-store target.
    ///
    /// # Errors
    ///
    /// Returns an error when the store schema differs from the probed source
    /// schema.
    pub fn new(source: SourceTable, store: TableStore) -> Result<Self, SnapshotError> {
        // Compare at the store's catalog generation: live DDL (ALTER,
        // TRUNCATE) advances the durable schema version, and a version-1
        // rebuild would reject every store that ever evolved even though
        // the column layout still matches.
        let expected = source.table_schema_with_version(store.schema().version())?;
        if store.schema() != &expected {
            return Err(SnapshotError::InvalidConfiguration(format!(
                "store schema for {} does not match the probed source schema",
                source.name
            )));
        }
        Ok(Self { source, store })
    }

    /// Returns source table metadata.
    #[must_use]
    pub const fn source(&self) -> &SourceTable {
        &self.source
    }

    /// Returns the populated store.
    #[must_use]
    pub const fn store(&self) -> &TableStore {
        &self.store
    }

    /// Consumes the target and returns the populated store.
    #[must_use]
    pub fn into_store(self) -> TableStore {
        self.store
    }
}

/// Durable progress emitted after each completed chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotProgress {
    /// Control-plane source identifier.
    pub database_id: String,
    /// Source table name.
    pub table: String,
    /// Stable chunk identifier.
    pub chunk_id: String,
    /// Total rows durably checkpointed across all attempts for the table.
    pub rows: u64,
    /// Approximate bytes converted and published in this run.
    pub bytes: u64,
    /// Estimated remaining seconds when source statistics are available.
    pub eta_seconds: Option<u64>,
}

/// Per-table completion summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableSnapshotOutcome {
    /// Source table name.
    pub table: String,
    /// Durable completed chunks, including chunks from an earlier attempt.
    pub chunks: usize,
    /// Durable row count, including chunks from an earlier attempt.
    pub rows: u64,
}

/// Successful snapshot result.
pub struct SnapshotResult {
    /// Source position from which CDC can replay after snapshot completion.
    ///
    /// A pre-existing handoff checkpoint is preserved, so this can sit
    /// BEHIND the data actually read — see [`SnapshotResult::captured_position`].
    pub position: SnapshotPosition,
    /// The position captured fresh under this snapshot's read lock — the
    /// exact point the copied data reflects. Row events at or before it are
    /// already in the data and must not replay.
    pub captured_position: SnapshotPosition,
    /// Whether all workers were established under the global read lock.
    pub globally_consistent: bool,
    /// Explanation when global consistency gracefully degraded.
    pub consistency_warning: Option<String>,
    /// Per-table durable totals.
    pub tables: Vec<TableSnapshotOutcome>,
    /// Populated stores in the same source-name order as the input.
    pub targets: Vec<SnapshotTarget>,
}

/// Snapshot failure.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// Invalid worker/chunk/schema configuration.
    #[error("invalid snapshot configuration: {0}")]
    InvalidConfiguration(String),
    /// `MySQL` protocol or query failure.
    #[error("MySQL snapshot failed: {0}")]
    Mysql(#[from] mysql_async::Error),
    /// Control-plane checkpoint failure.
    #[error("snapshot metadata failed: {0}")]
    Metadata(#[from] anyhow::Error),
    /// Pintail segment publication failure.
    #[error("snapshot storage failed: {0}")]
    Store(#[from] StoreError),
    /// Probed schema could not form a valid table schema.
    #[error("snapshot schema failed: {0}")]
    Schema(#[from] SchemaError),
    /// A source cell did not match its declared `MySQL` type.
    #[error("cannot map {table}.{column}: {reason}")]
    TypeMapping {
        /// Source table.
        table: String,
        /// Source column.
        column: String,
        /// Precise conversion failure.
        reason: String,
    },
    /// A supervisor-requested chunk budget was reached at a durable boundary.
    #[error("snapshot paused after {completed_chunks} newly completed chunks")]
    Paused {
        /// Number of chunks durably completed by this attempt.
        completed_chunks: usize,
    },
}

type ProgressListener = Arc<dyn Fn(SnapshotProgress) + Send + Sync>;

/// Runs a snapshot without progress callbacks.
///
/// # Errors
///
/// Returns the first coordination, source, mapping, metadata, or storage
/// failure. Completed chunks remain resumable.
pub async fn run_snapshot(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: Vec<SnapshotTarget>,
    options: SnapshotOptions,
) -> Result<SnapshotResult, SnapshotError> {
    run_snapshot_inner(
        pool,
        metadata_path,
        database_id,
        report,
        targets,
        options,
        Arc::new(|_| {}),
    )
    .await
}

/// Runs a snapshot and emits durable per-chunk progress.
///
/// # Errors
///
/// Returns the same failures as [`run_snapshot`].
pub async fn run_snapshot_with_progress<F>(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: Vec<SnapshotTarget>,
    options: SnapshotOptions,
    progress: F,
) -> Result<SnapshotResult, SnapshotError>
where
    F: Fn(SnapshotProgress) + Send + Sync + 'static,
{
    run_snapshot_inner(
        pool,
        metadata_path,
        database_id,
        report,
        targets,
        options,
        Arc::new(progress),
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_snapshot_inner(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    mut targets: Vec<SnapshotTarget>,
    options: SnapshotOptions,
    progress: ProgressListener,
) -> Result<SnapshotResult, SnapshotError> {
    if options.workers == 0 {
        return Err(SnapshotError::InvalidConfiguration(
            "worker count must be non-zero".to_owned(),
        ));
    }
    if options.chunk_rows == 0 {
        return Err(SnapshotError::InvalidConfiguration(
            "chunk row count must be non-zero".to_owned(),
        ));
    }
    if targets.is_empty() {
        return Err(SnapshotError::InvalidConfiguration(
            "snapshot requires at least one table target".to_owned(),
        ));
    }
    // A snapshot is the longest operation the system performs, and until this
    // line existed it began and ended in silence. An operator watching a
    // multi-hour initial copy had no way to tell it apart from a hang.
    pintail_log::log_info!(
        "snapshot start db={database_id} tables={} workers={} chunk_rows={}",
        targets.len(),
        options.workers,
        options.chunk_rows
    );
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
    for pair in targets.windows(2) {
        if pair[0].source.name == pair[1].source.name {
            return Err(SnapshotError::InvalidConfiguration(format!(
                "duplicate target table {}",
                pair[0].source.name
            )));
        }
    }
    for target in &targets {
        if !report
            .tables
            .iter()
            .any(|table| table.name == target.source.name)
        {
            return Err(SnapshotError::InvalidConfiguration(format!(
                "target {} is absent from the probe report",
                target.source.name
            )));
        }
    }

    let metadata = MetaStore::open(metadata_path)?;
    for target in &targets {
        let key_json = serde_json::to_string(&target.source.key.columns)
            .map_err(|error| anyhow::anyhow!("serialize snapshot key: {error}"))?;
        metadata.upsert_snapshot_table(
            database_id,
            &target.source.name,
            Some(&key_json),
            Some(&key_json),
        )?;
    }

    let mut coordinator = pool.get_conn().await?;
    let (globally_consistent, consistency_warning) = match coordinator
        .query_drop("FLUSH TABLES WITH READ LOCK")
        .await
    {
        Ok(()) => (true, None),
        Err(error) if options.allow_degraded_lock => (
            false,
            Some(format!(
                "global read lock unavailable; worker snapshots may have different start times: {error}"
            )),
        ),
        Err(error) => return Err(SnapshotError::Mysql(error)),
    };
    let captured_position = match capture_position(&mut coordinator, report.server.flavor).await {
        Ok(position) => position,
        Err(error) => {
            if globally_consistent {
                let _unlock = coordinator.query_drop("UNLOCK TABLES").await;
            }
            return Err(error);
        }
    };
    let position = match preserve_handoff_position(&metadata, database_id, &captured_position) {
        Ok(position) => position,
        Err(error) => {
            if globally_consistent {
                let _unlock = coordinator.query_drop("UNLOCK TABLES").await;
            }
            return Err(error);
        }
    };

    let worker_count = options.workers.min(targets.len());
    let mut transactions = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let mut transaction_options = TxOpts::default();
        transaction_options
            .with_consistent_snapshot(true)
            .with_isolation_level(IsolationLevel::RepeatableRead)
            .with_readonly(true);
        match pool.start_transaction(transaction_options).await {
            Ok(transaction) => transactions.push(transaction),
            Err(error) => {
                if globally_consistent {
                    let _unlock = coordinator.query_drop("UNLOCK TABLES").await;
                }
                for transaction in transactions {
                    let _rollback = transaction.rollback().await;
                }
                return Err(SnapshotError::Mysql(error));
            }
        }
    }
    if globally_consistent {
        coordinator.query_drop("UNLOCK TABLES").await?;
    }
    drop(coordinator);

    let mut queues = (0..worker_count)
        .map(|_| Vec::new())
        .collect::<Vec<Vec<SnapshotTarget>>>();
    for (index, target) in targets.into_iter().enumerate() {
        queues[index % worker_count].push(target);
    }
    let newly_completed = Arc::new(AtomicUsize::new(0));
    let metadata_path = metadata_path.to_path_buf();
    let futures = transactions
        .into_iter()
        .zip(queues)
        .map(|(transaction, queue)| {
            snapshot_worker(
                transaction,
                queue,
                metadata_path.clone(),
                database_id.to_owned(),
                report.database.clone(),
                options.clone(),
                Arc::clone(&newly_completed),
                Arc::clone(&progress),
            )
        });
    let worker_results = join_all(futures).await;
    let mut populated = Vec::new();
    for result in worker_results {
        populated.extend(result?);
    }
    populated.sort_by(|left, right| left.source.name.cmp(&right.source.name));

    let metadata = MetaStore::open(metadata_path.as_path())?;
    let mut table_outcomes = Vec::with_capacity(populated.len());
    for target in &populated {
        let chunks = metadata.snapshot_chunks(database_id, &target.source.name)?;
        table_outcomes.push(TableSnapshotOutcome {
            table: target.source.name.clone(),
            chunks: chunks
                .iter()
                .filter(|chunk| chunk.status == SnapshotChunkStatus::Completed)
                .count(),
            rows: chunks
                .iter()
                .filter(|chunk| chunk.status == SnapshotChunkStatus::Completed)
                .map(|chunk| chunk.rows)
                .sum(),
        });
    }
    // Consistency is reported because it is a property of the run that no
    // later inspection can recover: whether the copy shared one source
    // transaction is decided here and nowhere else.
    pintail_log::log_info!(
        "snapshot done db={database_id} tables={} rows={} consistent={globally_consistent}{}",
        table_outcomes.len(),
        table_outcomes.iter().map(|table| table.rows).sum::<u64>(),
        consistency_warning
            .as_deref()
            .map_or_else(String::new, |warning| format!(" warning={warning}"))
    );
    Ok(SnapshotResult {
        position,
        captured_position,
        globally_consistent,
        consistency_warning,
        tables: table_outcomes,
        targets: populated,
    })
}

#[allow(clippy::too_many_arguments)]
async fn snapshot_worker(
    mut transaction: Transaction<'static>,
    mut targets: Vec<SnapshotTarget>,
    metadata_path: PathBuf,
    database_id: String,
    source_database: String,
    options: SnapshotOptions,
    newly_completed: Arc<AtomicUsize>,
    progress: ProgressListener,
) -> Result<Vec<SnapshotTarget>, SnapshotError> {
    let mut metadata = MetaStore::open(&metadata_path)?;
    for target in &mut targets {
        let result = snapshot_table(
            &mut transaction,
            &mut metadata,
            &database_id,
            &source_database,
            target,
            &options,
            &newly_completed,
            &progress,
        )
        .await;
        if let Err(error) = result {
            let _record_error =
                metadata.fail_snapshot_table(&database_id, &target.source.name, &error.to_string());
            let _rollback = transaction.rollback().await;
            return Err(error);
        }
        metadata.complete_snapshot_table(&database_id, &target.source.name)?;
    }
    transaction.commit().await?;
    Ok(targets)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn snapshot_table(
    transaction: &mut Transaction<'static>,
    metadata: &mut MetaStore,
    database_id: &str,
    source_database: &str,
    target: &mut SnapshotTarget,
    options: &SnapshotOptions,
    newly_completed: &AtomicUsize,
    progress: &ProgressListener,
) -> Result<(), SnapshotError> {
    let completed = metadata.completed_snapshot_chunks(database_id, &target.source.name)?;
    let previously_completed_rows = metadata
        .snapshot_chunks(database_id, &target.source.name)?
        .into_iter()
        .filter(|chunk| chunk.status == SnapshotChunkStatus::Completed)
        .map(|chunk| chunk.rows)
        .sum::<u64>();
    let columns = target
        .source
        .columns
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>();
    let qualified_table = format!(
        "{}.{}",
        quote_identifier(source_database),
        quote_identifier(&target.source.name)
    );
    let key_indices = target
        .source
        .key
        .columns
        .iter()
        .map(|key| {
            target
                .source
                .columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(key))
                .ok_or_else(|| {
                    SnapshotError::InvalidConfiguration(format!(
                        "snapshot key column {}.{} is not materialized",
                        target.source.name, key
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let order_by = target
        .source
        .key
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let started = Instant::now();
    let mut page = 0_usize;
    let mut cursor: Option<Vec<MysqlValue>> = None;
    let mut run_rows = previously_completed_rows;
    let mut newly_completed_rows = 0_u64;
    let mut run_bytes = 0_u64;
    loop {
        let mut parameters = Vec::new();
        let sql = if key_indices.is_empty() {
            let offset = page.saturating_mul(options.chunk_rows);
            parameters.push(MysqlValue::UInt(options.chunk_rows as u64));
            parameters.push(MysqlValue::UInt(offset as u64));
            format!(
                "SELECT {} FROM {qualified_table} LIMIT ? OFFSET ?",
                columns.join(", ")
            )
        } else if let Some(cursor) = &cursor {
            parameters.extend(cursor.iter().cloned());
            parameters.push(MysqlValue::UInt(options.chunk_rows as u64));
            let key_tuple = target
                .source
                .key
                .columns
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(", ");
            let placeholders = std::iter::repeat_n("?", cursor.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "SELECT {} FROM {qualified_table} \
                 WHERE ({key_tuple}) > ({placeholders}) \
                 ORDER BY {order_by} LIMIT ?",
                columns.join(", ")
            )
        } else {
            parameters.push(MysqlValue::UInt(options.chunk_rows as u64));
            format!(
                "SELECT {} FROM {qualified_table} ORDER BY {order_by} LIMIT ?",
                columns.join(", ")
            )
        };
        let rows: Vec<Row> = transaction
            .exec(sql, Params::Positional(parameters))
            .await?;
        if rows.is_empty() {
            break;
        }
        let next_cursor = if key_indices.is_empty() {
            None
        } else {
            let last = rows.last().expect("non-empty source chunk");
            Some(
                key_indices
                    .iter()
                    .map(|index| {
                        last.as_ref(*index)
                            .cloned()
                            .ok_or_else(|| SnapshotError::TypeMapping {
                                table: target.source.name.clone(),
                                column: target.source.columns[*index].name.clone(),
                                reason: "source row is missing a key value".to_owned(),
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let chunk_id = format!("chunk-{page:020}");
        if !completed.contains(&chunk_id) {
            let chunk_reserved = reserve_snapshot_chunk(newly_completed, options.max_new_chunks)?;
            let lo_json = cursor
                .as_ref()
                .map(|values| cursor_json(values))
                .transpose()?;
            let hi_json = next_cursor
                .as_ref()
                .map(|values| cursor_json(values))
                .transpose()?;
            metadata.start_snapshot_chunk(
                database_id,
                &target.source.name,
                &chunk_id,
                lo_json.as_deref(),
                hi_json.as_deref(),
            )?;
            let row_offset = page.saturating_mul(options.chunk_rows);
            let stored_rows = rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    convert_row(
                        &target.source,
                        row,
                        u64::try_from(row_offset.saturating_add(index)).unwrap_or(u64::MAX),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            let chunk_bytes = stored_rows
                .iter()
                .map(StoredRow::estimated_bytes)
                .sum::<usize>();
            let outcome = target.store.bulk_ingest_snapshot(stored_rows)?;
            let rows = u64::try_from(outcome.row_count()).unwrap_or(u64::MAX);
            metadata.complete_snapshot_chunk(database_id, &target.source.name, &chunk_id, rows)?;
            if !chunk_reserved {
                newly_completed.fetch_add(1, Ordering::Relaxed);
            }
            run_rows = run_rows.saturating_add(rows);
            newly_completed_rows = newly_completed_rows.saturating_add(rows);
            run_bytes = run_bytes.saturating_add(chunk_bytes as u64);
            let eta_seconds = target.source.estimated_rows.and_then(|estimated| {
                if newly_completed_rows == 0 || estimated <= run_rows {
                    None
                } else {
                    let elapsed = started.elapsed().as_secs().max(1);
                    Some(
                        (estimated - run_rows)
                            .saturating_mul(elapsed)
                            .div_ceil(newly_completed_rows),
                    )
                }
            });
            // Debug, not info: a chunk lands every chunk_rows source rows, so
            // a large table emits thousands of these. The value is watching a
            // specific slow table, not narrating every snapshot.
            pintail_log::log_debug!(
                "snapshot chunk db={database_id} table={} chunk={chunk_id} rows={run_rows} bytes={run_bytes} eta={}",
                target.source.name,
                eta_seconds.map_or_else(|| "unknown".to_owned(), |seconds| format!("{seconds}s"))
            );
            progress(SnapshotProgress {
                database_id: database_id.to_owned(),
                table: target.source.name.clone(),
                chunk_id: chunk_id.clone(),
                rows: run_rows,
                bytes: run_bytes,
                eta_seconds,
            });
        }
        cursor = next_cursor;
        page = page.saturating_add(1);
        if key_indices.is_empty() && page.saturating_mul(options.chunk_rows) == usize::MAX {
            return Err(SnapshotError::InvalidConfiguration(
                "PK-less snapshot offset overflowed".to_owned(),
            ));
        }
    }
    Ok(())
}

fn reserve_snapshot_chunk(
    newly_completed: &AtomicUsize,
    maximum: Option<usize>,
) -> Result<bool, SnapshotError> {
    let Some(maximum) = maximum else {
        return Ok(false);
    };
    newly_completed
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |completed| {
            (completed < maximum).then(|| completed + 1)
        })
        .map(|_| true)
        .map_err(|completed_chunks| SnapshotError::Paused { completed_chunks })
}

fn convert_row(
    table: &SourceTable,
    row: Row,
    append_row_id: u64,
) -> Result<StoredRow, SnapshotError> {
    if row.len() != table.columns.len() {
        return Err(SnapshotError::TypeMapping {
            table: table.name.clone(),
            column: "<row>".to_owned(),
            reason: format!(
                "source returned {} values for {} projected columns",
                row.len(),
                table.columns.len()
            ),
        });
    }
    let values = row
        .unwrap()
        .into_iter()
        .zip(&table.columns)
        .map(|(value, column)| map_mysql_value(&table.name, column, value))
        .collect::<Result<Vec<_>, _>>()?;
    let key = if table.key.mode == KeyMode::AppendRowId {
        PrimaryKey::new(vec![KeyPart::UInt64(append_row_id)])?
    } else {
        let parts = table
            .key
            .columns
            .iter()
            .map(|key| {
                let index = table
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(key))
                    .ok_or_else(|| SnapshotError::TypeMapping {
                        table: table.name.clone(),
                        column: key.clone(),
                        reason: "key column is absent from the materialized row".to_owned(),
                    })?;
                key_part(&values[index]).ok_or_else(|| SnapshotError::TypeMapping {
                    table: table.name.clone(),
                    column: key.clone(),
                    reason: "key value is NULL or cannot be ordered".to_owned(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        PrimaryKey::new(parts)?
    };
    Ok(StoredRow::new(key, values, 0, false))
}

/// Converts one value returned by a source query using probed column metadata.
///
/// CDC reuses this normalization after adapting binlog-only representations
/// such as ENUM indexes and packed timestamps.
///
/// # Errors
///
/// Returns a typed mapping error when the source representation is invalid.
pub fn map_mysql_value(
    table: &str,
    column: &SourceColumn,
    value: MysqlValue,
) -> Result<Value, SnapshotError> {
    if value == MysqlValue::NULL {
        return Ok(Value::Null);
    }
    let mapped = match column.pintail_type {
        DataType::Boolean => Value::Boolean(mysql_bool(&value).ok_or_else(|| {
            mapping_error(table, column, "expected a MySQL boolean/integer value")
        })?),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            let value = mysql_i64(&value)
                .ok_or_else(|| mapping_error(table, column, "expected a signed integer"))?;
            validate_signed_range(column.pintail_type, value)
                .map_err(|reason| mapping_error(table, column, reason))?;
            Value::Int64(value)
        }
        DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Year => {
            let value = mysql_u64(&value)
                .ok_or_else(|| mapping_error(table, column, "expected an unsigned integer"))?;
            validate_unsigned_range(column.pintail_type, value)
                .map_err(|reason| mapping_error(table, column, reason))?;
            Value::UInt64(value)
        }
        DataType::Float32 | DataType::Float64 => Value::float64(
            mysql_f64(&value)
                .ok_or_else(|| mapping_error(table, column, "expected a floating-point value"))?,
        ),
        DataType::Decimal { .. } => Value::Utf8(
            mysql_text(&value)
                .ok_or_else(|| mapping_error(table, column, "expected decimal text"))?,
        ),
        DataType::Date32 => match normalize_date(&value) {
            Some(value) => Value::Utf8(value),
            None => Value::Null,
        },
        DataType::DateTime64 { fsp } => match normalize_datetime(&value, fsp) {
            Some(value) => Value::Utf8(value),
            None => Value::Null,
        },
        DataType::Time64 { fsp } => Value::Utf8(
            normalize_time(&value, fsp)
                .ok_or_else(|| mapping_error(table, column, "invalid MySQL TIME value"))?,
        ),
        DataType::Utf8 => Value::Utf8(
            mysql_text(&value)
                .ok_or_else(|| mapping_error(table, column, "value is not valid UTF-8"))?,
        ),
        DataType::Binary => Value::Binary(
            // Geometry stays in MySQL's internal format - 4-byte SRID then
            // WKB - because that is byte-for-byte what a MySQL client reads
            // back with SELECT. Stripping the SRID here made every geometry
            // differ from the source (#263), and the poll path once stripped
            // AGAIN on top of ST_AsWKB, corrupting reconciled values.
            mysql_bytes(value)
                .ok_or_else(|| mapping_error(table, column, "expected binary bytes"))?,
        ),
        DataType::Json => {
            let text = mysql_text(&value)
                .ok_or_else(|| mapping_error(table, column, "JSON is not valid UTF-8"))?;
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| mapping_error(table, column, format!("invalid JSON: {error}")))?;
            Value::Utf8(
                serde_json::to_string(&parsed)
                    .map_err(|error| mapping_error(table, column, error.to_string()))?,
            )
        }
    };
    Ok(mapped)
}

fn mapping_error(table: &str, column: &SourceColumn, reason: impl Into<String>) -> SnapshotError {
    SnapshotError::TypeMapping {
        table: table.to_owned(),
        column: column.name.clone(),
        reason: reason.into(),
    }
}

fn mysql_bool(value: &MysqlValue) -> Option<bool> {
    match value {
        MysqlValue::Int(value) => Some(*value != 0),
        MysqlValue::UInt(value) => Some(*value != 0),
        MysqlValue::Bytes(value) => match value.as_slice() {
            b"0" | [0] => Some(false),
            b"1" | [1] => Some(true),
            _ => std::str::from_utf8(value)
                .ok()?
                .parse::<i64>()
                .ok()
                .map(|value| value != 0),
        },
        _ => None,
    }
}

fn mysql_i64(value: &MysqlValue) -> Option<i64> {
    match value {
        MysqlValue::Int(value) => Some(*value),
        MysqlValue::UInt(value) => i64::try_from(*value).ok(),
        MysqlValue::Bytes(value) => std::str::from_utf8(value).ok()?.parse().ok(),
        _ => None,
    }
}

fn mysql_u64(value: &MysqlValue) -> Option<u64> {
    match value {
        MysqlValue::UInt(value) => Some(*value),
        MysqlValue::Int(value) => u64::try_from(*value).ok(),
        MysqlValue::Bytes(value) if value.len() <= 8 && !value.iter().all(u8::is_ascii_digit) => {
            Some(
                value
                    .iter()
                    .fold(0_u64, |result, byte| (result << 8) | u64::from(*byte)),
            )
        }
        MysqlValue::Bytes(value) => std::str::from_utf8(value).ok()?.parse().ok(),
        _ => None,
    }
}

#[allow(clippy::cast_precision_loss)]
fn mysql_f64(value: &MysqlValue) -> Option<f64> {
    match value {
        MysqlValue::Float(value) => Some(f64::from(*value)),
        MysqlValue::Double(value) => Some(*value),
        MysqlValue::Int(value) => Some(*value as f64),
        MysqlValue::UInt(value) => Some(*value as f64),
        MysqlValue::Bytes(value) => std::str::from_utf8(value).ok()?.parse().ok(),
        _ => None,
    }
}

fn mysql_text(value: &MysqlValue) -> Option<String> {
    match value {
        MysqlValue::Bytes(value) => String::from_utf8(value.clone()).ok(),
        MysqlValue::Int(value) => Some(value.to_string()),
        MysqlValue::UInt(value) => Some(value.to_string()),
        MysqlValue::Float(value) => Some(value.to_string()),
        MysqlValue::Double(value) => Some(value.to_string()),
        MysqlValue::Date(year, month, day, hour, minute, second, micros) => Some(
            format_mysql_datetime(*year, *month, *day, *hour, *minute, *second, *micros, 6),
        ),
        MysqlValue::Time(negative, days, hours, minutes, seconds, micros) => Some(
            format_mysql_time(*negative, *days, *hours, *minutes, *seconds, *micros, 6),
        ),
        MysqlValue::NULL => None,
    }
}

fn mysql_bytes(value: MysqlValue) -> Option<Vec<u8>> {
    match value {
        MysqlValue::Bytes(value) => Some(value),
        _ => mysql_text(&value).map(String::into_bytes),
    }
}

fn validate_signed_range(data_type: DataType, value: i64) -> Result<(), &'static str> {
    let valid = match data_type {
        DataType::Int8 => i8::try_from(value).is_ok(),
        DataType::Int16 => i16::try_from(value).is_ok(),
        DataType::Int32 => i32::try_from(value).is_ok(),
        DataType::Int64 => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err("signed value exceeds the declared source width")
    }
}

fn validate_unsigned_range(data_type: DataType, value: u64) -> Result<(), &'static str> {
    let valid = match data_type {
        DataType::UInt8 => u8::try_from(value).is_ok(),
        DataType::UInt16 => u16::try_from(value).is_ok(),
        DataType::UInt32 => u32::try_from(value).is_ok(),
        DataType::UInt64 => true,
        DataType::Year => value == 0 || (1901..=2155).contains(&value),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err("unsigned value exceeds the declared source width")
    }
}

/// `MySQL`'s all-zero date, which is a value rather than an absence.
///
/// Legacy sources are full of these: every `MySQL` before 5.7 accepted
/// `0000-00-00` by default. `MySQL` returns it from a `SELECT`, does not match
/// it with `IS NULL`, and counts it in `COUNT(column)`. Mapping it to NULL -
/// as this did - inverted all three of those, silently.
const ZERO_DATE: &str = "0000-00-00";
const ZERO_DATETIME: &str = "0000-00-00 00:00:00";

/// Whether every date component is zero, which is the all-zero date rather
/// than a merely invalid one like February 31st.
const fn is_zero_date(year: u16, month: u8, day: u8) -> bool {
    year == 0 && month == 0 && day == 0
}

/// The all-zero datetime rendered at the column's fractional precision, so
/// it matches the width every other value of that column carries.
fn zero_datetime_text(fsp: u8) -> String {
    if fsp == 0 {
        return ZERO_DATETIME.to_owned();
    }
    format!("{ZERO_DATETIME}.{}", "0".repeat(usize::from(fsp)))
}

fn normalize_date(value: &MysqlValue) -> Option<String> {
    let (year, month, day) = match value {
        MysqlValue::Date(year, month, day, ..) => (*year, *month, *day),
        MysqlValue::Bytes(value) => {
            let value = std::str::from_utf8(value).ok()?;
            let date = value.get(..10)?;
            let mut parts = date.split('-');
            (
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            )
        }
        _ => return None,
    };
    // Preserved verbatim: it is a value MySQL round-trips, not a missing
    // one. A genuinely invalid date such as February 31st still becomes
    // NULL, because it has no canonical form to round-trip.
    if is_zero_date(year, month, day) {
        return Some(ZERO_DATE.to_owned());
    }
    NaiveDate::from_ymd_opt(i32::from(year), u32::from(month), u32::from(day))?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn normalize_datetime(value: &MysqlValue, fsp: u8) -> Option<String> {
    match value {
        MysqlValue::Date(year, month, day, hour, minute, second, micros) => {
            // Same reasoning as the zero date: MySQL round-trips this rather
            // than treating it as absent.
            if is_zero_date(*year, *month, *day) {
                return Some(zero_datetime_text(fsp));
            }
            let date =
                NaiveDate::from_ymd_opt(i32::from(*year), u32::from(*month), u32::from(*day))?;
            date.and_hms_micro_opt(
                u32::from(*hour),
                u32::from(*minute),
                u32::from(*second),
                *micros,
            )?;
            Some(format_mysql_datetime(
                *year, *month, *day, *hour, *minute, *second, *micros, fsp,
            ))
        }
        MysqlValue::Bytes(value) => {
            let value = std::str::from_utf8(value).ok()?;
            if value.starts_with(ZERO_DATE) {
                return Some(zero_datetime_text(fsp));
            }
            let formats = [
                "%Y-%m-%d %H:%M:%S%.f",
                "%Y-%m-%dT%H:%M:%S%.f",
                "%Y-%m-%d %H:%M:%S",
            ];
            let parsed = formats
                .iter()
                .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())?;
            Some(format_mysql_datetime(
                u16::try_from(parsed.date().year()).ok()?,
                u8::try_from(parsed.date().month()).ok()?,
                u8::try_from(parsed.date().day()).ok()?,
                u8::try_from(parsed.time().hour()).ok()?,
                u8::try_from(parsed.time().minute()).ok()?,
                u8::try_from(parsed.time().second()).ok()?,
                parsed.time().nanosecond() / 1_000,
                fsp,
            ))
        }
        _ => None,
    }
}

fn normalize_time(value: &MysqlValue, fsp: u8) -> Option<String> {
    match value {
        MysqlValue::Time(negative, days, hours, minutes, seconds, micros) => Some(
            format_mysql_time(*negative, *days, *hours, *minutes, *seconds, *micros, fsp),
        ),
        MysqlValue::Bytes(value) => {
            let value = std::str::from_utf8(value).ok()?;
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn format_mysql_datetime(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
    fsp: u8,
) -> String {
    let mut value = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    append_fraction(&mut value, micros, fsp);
    value
}

fn format_mysql_time(
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
    fsp: u8,
) -> String {
    let total_hours = u64::from(days)
        .saturating_mul(24)
        .saturating_add(u64::from(hours));
    let sign = if negative { "-" } else { "" };
    let mut value = format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}");
    append_fraction(&mut value, micros, fsp);
    value
}

fn append_fraction(value: &mut String, micros: u32, fsp: u8) {
    if fsp == 0 {
        return;
    }
    let fraction = format!("{micros:06}");
    value.push('.');
    value.push_str(&fraction[..usize::from(fsp.min(6))]);
}

fn key_part(value: &Value) -> Option<KeyPart> {
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(KeyPart::UInt64(u64::from(*value))),
        Value::Int64(value) => Some(KeyPart::Int64(*value)),
        Value::UInt64(value) => Some(KeyPart::UInt64(*value)),
        Value::Float64(value) => {
            let normalized = if value.get() == 0.0 { 0.0 } else { value.get() };
            Some(KeyPart::Utf8(normalized.to_string()))
        }
        Value::Utf8(value) | Value::Enum { label: value, .. } => Some(KeyPart::Utf8(value.clone())),
        Value::Binary(value) => Some(KeyPart::Binary(value.clone())),
    }
}

async fn capture_position(
    connection: &mut mysql_async::Conn,
    flavor: SourceFlavor,
) -> Result<SnapshotPosition, SnapshotError> {
    let mut file_position = None;
    for query in ["SHOW BINARY LOG STATUS", "SHOW MASTER STATUS"] {
        if let Ok(Some(row)) = connection.query_first::<Row, _>(query).await {
            let file = row.as_ref(0).and_then(mysql_text).ok_or_else(|| {
                SnapshotError::InvalidConfiguration(
                    "binlog status did not contain a file name".to_owned(),
                )
            })?;
            let position = row.as_ref(1).and_then(mysql_u64).ok_or_else(|| {
                SnapshotError::InvalidConfiguration(
                    "binlog status did not contain a numeric position".to_owned(),
                )
            })?;
            file_position = Some((file, position));
            break;
        }
    }
    let gtid_query = match flavor {
        SourceFlavor::Mysql => "SELECT @@GLOBAL.gtid_executed",
        SourceFlavor::MariaDb => "SELECT @@GLOBAL.gtid_binlog_pos",
    };
    let gtid = connection
        .query_first::<String, _>(gtid_query)
        .await
        .ok()
        .flatten()
        .filter(|value| !value.is_empty());
    Ok(if let Some(set) = gtid {
        SnapshotPosition::Gtid {
            set,
            file: file_position.as_ref().map(|(file, _)| file.clone()),
            position: file_position.as_ref().map(|(_, position)| *position),
        }
    } else if let Some((file, position)) = file_position {
        SnapshotPosition::FilePosition { file, position }
    } else {
        SnapshotPosition::Unavailable
    })
}

fn preserve_handoff_position(
    metadata: &MetaStore,
    database_id: &str,
    position: &SnapshotPosition,
) -> Result<SnapshotPosition, SnapshotError> {
    let now = Utc::now().to_rfc3339();
    match position {
        SnapshotPosition::Gtid {
            set,
            file,
            position,
        } => metadata.insert_snapshot_checkpoint_if_absent(
            database_id,
            "gtid",
            Some(set),
            file.as_deref(),
            *position,
            &now,
        )?,
        SnapshotPosition::FilePosition { file, position } => {
            metadata.insert_snapshot_checkpoint_if_absent(
                database_id,
                "filepos",
                None,
                Some(file),
                Some(*position),
                &now,
            )?;
        }
        SnapshotPosition::Unavailable => metadata.insert_snapshot_checkpoint_if_absent(
            database_id,
            "polling",
            None,
            None,
            None,
            &now,
        )?,
    }
    let checkpoint = metadata.snapshot_checkpoint(database_id)?.ok_or_else(|| {
        SnapshotError::InvalidConfiguration(
            "snapshot handoff checkpoint was not persisted".to_owned(),
        )
    })?;
    match checkpoint.kind.as_str() {
        "gtid" => Ok(SnapshotPosition::Gtid {
            set: checkpoint.gtid_set.ok_or_else(|| {
                SnapshotError::InvalidConfiguration(
                    "GTID snapshot checkpoint is missing its GTID set".to_owned(),
                )
            })?,
            file: checkpoint.binlog_file,
            position: checkpoint.binlog_pos,
        }),
        "filepos" => Ok(SnapshotPosition::FilePosition {
            file: checkpoint.binlog_file.ok_or_else(|| {
                SnapshotError::InvalidConfiguration(
                    "file/position snapshot checkpoint is missing its file".to_owned(),
                )
            })?,
            position: checkpoint.binlog_pos.ok_or_else(|| {
                SnapshotError::InvalidConfiguration(
                    "file/position snapshot checkpoint is missing its position".to_owned(),
                )
            })?,
        }),
        "polling" => Ok(SnapshotPosition::Unavailable),
        kind => Err(SnapshotError::InvalidConfiguration(format!(
            "unsupported snapshot checkpoint kind {kind}"
        ))),
    }
}

fn cursor_json(values: &[MysqlValue]) -> Result<String, SnapshotError> {
    let values = values.iter().map(mysql_value_json).collect::<Vec<_>>();
    serde_json::to_string(&values).map_err(|error| SnapshotError::Metadata(anyhow::anyhow!(error)))
}

fn mysql_value_json(value: &MysqlValue) -> serde_json::Value {
    match value {
        MysqlValue::NULL => serde_json::Value::Null,
        MysqlValue::Bytes(value) => {
            let hex = value.iter().fold(String::new(), |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing to a string cannot fail");
                output
            });
            json!({"bytes_hex": hex})
        }
        MysqlValue::Int(value) => json!({"int": value}),
        MysqlValue::UInt(value) => json!({"uint": value}),
        MysqlValue::Float(value) => json!({"float_bits": value.to_bits()}),
        MysqlValue::Double(value) => json!({"double_bits": value.to_bits()}),
        MysqlValue::Date(year, month, day, hour, minute, second, micros) => {
            json!({"date": [year, month, day, hour, minute, second, micros]})
        }
        MysqlValue::Time(negative, days, hours, minutes, seconds, micros) => {
            json!({"time": [negative, days, hours, minutes, seconds, micros]})
        }
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::{
        append_fraction, key_part, normalize_date, normalize_datetime, normalize_time,
        quote_identifier, reserve_snapshot_chunk,
    };
    use mysql_async::Value as MysqlValue;
    use pintail_types::{KeyPart, Value};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn preserves_zero_dates_and_nulls_only_genuinely_invalid_ones() {
        // MySQL returns the all-zero date from a SELECT, does not match it
        // with IS NULL, and counts it. Mapping it to NULL inverted all three.
        assert_eq!(
            normalize_date(&MysqlValue::Date(0, 0, 0, 0, 0, 0, 0)),
            Some("0000-00-00".to_owned())
        );
        assert_eq!(
            normalize_date(&MysqlValue::Bytes(b"0000-00-00".to_vec())),
            Some("0000-00-00".to_owned())
        );
        // A zero datetime carries the column's fractional width, so it is
        // the same shape as every other value in that column.
        assert_eq!(
            normalize_datetime(&MysqlValue::Date(0, 0, 0, 0, 0, 0, 0), 0),
            Some("0000-00-00 00:00:00".to_owned())
        );
        assert_eq!(
            normalize_datetime(&MysqlValue::Date(0, 0, 0, 0, 0, 0, 0), 3),
            Some("0000-00-00 00:00:00.000".to_owned())
        );
        assert_eq!(
            normalize_datetime(&MysqlValue::Bytes(b"0000-00-00 00:00:00".to_vec()), 6),
            Some("0000-00-00 00:00:00.000000".to_owned())
        );

        // Genuinely invalid dates still become NULL: unlike the all-zero
        // date, they have no canonical form MySQL round-trips.
        assert_eq!(
            normalize_date(&MysqlValue::Date(2024, 2, 30, 0, 0, 0, 0)),
            None
        );
        assert_eq!(
            normalize_date(&MysqlValue::Date(2024, 0, 1, 0, 0, 0, 0)),
            None
        );
        assert_eq!(
            normalize_datetime(&MysqlValue::Date(2024, 2, 30, 12, 0, 0, 0), 0),
            None
        );

        // Ordinary values are unaffected.
        assert_eq!(
            normalize_date(&MysqlValue::Date(1000, 1, 1, 0, 0, 0, 0)),
            Some("1000-01-01".to_owned())
        );
        assert_eq!(
            normalize_datetime(&MysqlValue::Date(2024, 2, 29, 12, 34, 56, 123_456), 3),
            Some("2024-02-29 12:34:56.123".to_owned())
        );
    }

    #[test]
    fn formats_mysql_times_beyond_one_day() {
        assert_eq!(
            normalize_time(&MysqlValue::Time(true, 2, 3, 4, 5, 600_000), 1),
            Some("-51:04:05.6".to_owned())
        );
    }

    #[test]
    fn produces_orderable_physical_keys_and_quotes_identifiers() {
        assert_eq!(
            key_part(&Value::float64(-0.0)),
            Some(KeyPart::Utf8("0".to_owned()))
        );
        assert_eq!(quote_identifier("a`b"), "`a``b`");
        let mut value = "x".to_owned();
        append_fraction(&mut value, 42, 6);
        assert_eq!(value, "x.000042");
    }

    #[test]
    fn chunk_budget_reservation_is_exact() {
        let completed = AtomicUsize::new(0);
        assert!(reserve_snapshot_chunk(&completed, Some(1)).expect("first reservation"));
        let error = reserve_snapshot_chunk(&completed, Some(1)).expect_err("budget exhausted");
        assert!(matches!(
            error,
            super::SnapshotError::Paused {
                completed_chunks: 1
            }
        ));
    }
}
