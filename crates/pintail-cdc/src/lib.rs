//! Native row-binlog CDC for Pintail.
//!
//! The stream buffers one source transaction, converts FULL before/after
//! images into versioned Pintail rows, synchronizes every touched table WAL,
//! and only then advances the `SQLite` source checkpoint. A crash therefore
//! replays at least once with deterministic versions.

mod decoder;
mod gtid;

use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    hash::{Hash as _, Hasher as _},
    path::Path,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use futures_util::StreamExt as _;
use mysql_async::{
    BinlogStream, BinlogStreamRequest, Error as MysqlError, Pool,
    binlog::{
        RowsEventFlags,
        events::{EventData, RowsEventData},
        row::BinlogRow,
    },
    prelude::Queryable as _,
};
use pintail_meta::{MetaStore, SnapshotCheckpointRecord};
use pintail_probe::{ProbeReport, SourceFlavor, SourceTable};
use pintail_snapshot::{SnapshotError, SnapshotOptions, SnapshotTarget, run_snapshot};
use pintail_store::{StoreError, TableStore};
use pintail_types::{KeyMode, SchemaError, StoredRow};
use serde_json::json;
use thiserror::Error;

use crate::{
    decoder::{decode_row, insert_key, physical_key},
    gtid::MysqlGtidSet,
};

/// One probed table and its existing snapshot store.
pub struct CdcTarget {
    source: SourceTable,
    store: TableStore,
}

impl CdcTarget {
    /// Validates and constructs a CDC target.
    ///
    /// # Errors
    ///
    /// Returns an error when the store schema differs from the probed source.
    pub fn new(source: SourceTable, store: TableStore) -> Result<Self, CdcError> {
        let expected = source.table_schema()?;
        if store.schema() != &expected {
            return Err(CdcError::InvalidConfiguration(format!(
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

    /// Returns the live table store.
    #[must_use]
    pub const fn store(&self) -> &TableStore {
        &self.store
    }

    /// Consumes the target and returns its table store.
    #[must_use]
    pub fn into_store(self) -> TableStore {
        self.store
    }
}

/// Runtime controls for one CDC stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdcOptions {
    /// Replica server ID. Zero derives a non-zero process-local ID from the
    /// database identifier.
    pub server_id: u32,
    /// Follow new events indefinitely. When false, request the current finite
    /// binlog range and return at EOF.
    pub blocking: bool,
    /// Optional deterministic commit budget for supervisors and tests.
    pub max_commits: Option<usize>,
    /// Maximum retained bytes for one uncommitted source transaction.
    pub max_transaction_bytes: usize,
    /// Consecutive connection failures tolerated before surfacing an error.
    pub max_reconnect_attempts: usize,
    /// First reconnect delay. Subsequent failures use bounded exponential
    /// backoff.
    pub reconnect_initial_delay: Duration,
    /// Automatically rebuild all targets once when the source checkpoint has
    /// fallen outside binlog retention.
    pub auto_resnapshot: bool,
    /// Snapshot controls used by automatic purge recovery.
    pub resnapshot_options: SnapshotOptions,
}

impl Default for CdcOptions {
    fn default() -> Self {
        Self {
            server_id: 0,
            blocking: true,
            max_commits: None,
            max_transaction_bytes: 64 * 1024 * 1024,
            max_reconnect_attempts: 8,
            reconnect_initial_delay: Duration::from_millis(100),
            auto_resnapshot: true,
            resnapshot_options: SnapshotOptions::default(),
        }
    }
}

/// Durable position after one committed source transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdcCheckpoint {
    /// `gtid` or `filepos`.
    pub kind: String,
    /// Executed `MySQL` GTID set when GTID mode is active.
    pub gtid_set: Option<String>,
    /// Current binlog file.
    pub binlog_file: String,
    /// Next event position.
    pub binlog_pos: u64,
}

/// Progress emitted only after WAL synchronization and checkpoint commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdcProgress {
    /// Source transactions durably committed by this runner.
    pub commits: usize,
    /// Row mutations accepted by table stores.
    pub mutations: usize,
    /// Current durable source position.
    pub checkpoint: CdcCheckpoint,
}

/// Finite CDC catch-up result.
pub struct CdcResult {
    /// Transactions durably committed by this invocation.
    pub commits: usize,
    /// Row mutations accepted by this invocation.
    pub mutations: usize,
    /// Last durable position, including an unchanged initial position.
    pub checkpoint: CdcCheckpoint,
    /// Populated stores in deterministic source-name order.
    pub targets: Vec<CdcTarget>,
}

/// CDC failure.
#[derive(Debug, Error)]
pub enum CdcError {
    /// Invalid runner, target, or source configuration.
    #[error("invalid CDC configuration: {0}")]
    InvalidConfiguration(String),
    /// Durable checkpoint metadata is missing or malformed.
    #[error("invalid CDC checkpoint: {0}")]
    InvalidCheckpoint(String),
    /// `MySQL` protocol or server failure.
    #[error("MySQL CDC failed: {0}")]
    Mysql(#[from] MysqlError),
    /// A purged source position requires a fresh snapshot.
    #[error("CDC position requires resnapshot: {reason}")]
    NeedsResync {
        /// Server explanation, normally error 1236.
        reason: String,
    },
    /// `SQLite` control-plane failure.
    #[error("CDC metadata failed: {0}")]
    Metadata(#[from] anyhow::Error),
    /// Pintail WAL or table-store failure.
    #[error("CDC storage failed: {0}")]
    Store(#[from] StoreError),
    /// Automatic full-snapshot recovery failed.
    #[error("CDC resnapshot failed: {0}")]
    Snapshot(#[from] SnapshotError),
    /// Probed schema or physical key failure.
    #[error("CDC schema failed: {0}")]
    Schema(#[from] SchemaError),
    /// A row event could not be decoded.
    #[error("CDC decode failed: {0}")]
    Decode(String),
    /// One source transaction exceeded the configured hard memory cap.
    #[error("CDC transaction retained {retained_bytes} bytes, above the {maximum_bytes}-byte cap")]
    TransactionTooLarge {
        /// Current retained estimate.
        retained_bytes: usize,
        /// Configured cap.
        maximum_bytes: usize,
    },
}

type ProgressListener = Arc<dyn Fn(CdcProgress) + Send + Sync>;

/// Runs CDC without a progress callback.
///
/// Set [`CdcOptions::blocking`] to false for a finite catch-up.
///
/// # Errors
///
/// Returns a source, decode, storage, or metadata error. Error 1236 is
/// classified as [`CdcError::NeedsResync`] and durably marks the database.
pub async fn run_cdc(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
    options: CdcOptions,
) -> Result<CdcResult, CdcError> {
    run_cdc_inner(
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

/// Runs CDC and emits only durable transaction progress.
///
/// # Errors
///
/// Returns the same failures as [`run_cdc`].
pub async fn run_cdc_with_progress<F>(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
    options: CdcOptions,
    progress: F,
) -> Result<CdcResult, CdcError>
where
    F: Fn(CdcProgress) + Send + Sync + 'static,
{
    run_cdc_inner(
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
async fn run_cdc_inner(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    mut targets: Vec<CdcTarget>,
    options: CdcOptions,
    progress: ProgressListener,
) -> Result<CdcResult, CdcError> {
    validate_configuration(report, &targets, &options)?;
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
    let target_indexes = targets
        .iter()
        .enumerate()
        .map(|(index, target)| (target.source.name.to_ascii_lowercase(), index))
        .collect::<BTreeMap<_, _>>();
    let mut metadata = MetaStore::open(metadata_path)?;
    let mut blocked_targets = metadata
        .tables_needing_resync(database_id)?
        .iter()
        .filter_map(|name| target_indexes.get(&name.to_ascii_lowercase()).copied())
        .collect::<BTreeSet<_>>();
    let checkpoint = metadata
        .snapshot_checkpoint(database_id)?
        .ok_or_else(|| CdcError::InvalidCheckpoint("snapshot position is absent".to_owned()))?;
    let mut position = StreamPosition::from_checkpoint(checkpoint, report.server.flavor)?;
    let server_id = if options.server_id == 0 {
        generated_server_id(database_id)
    } else {
        options.server_id
    };
    let mut pending = PendingTransaction::default();
    let mut commits = 0_usize;
    let mut mutations = 0_usize;
    let mut reconnect_attempts = 0_usize;
    let mut resnapshot_attempted = false;
    loop {
        let mut stream = match open_stream(
            pool,
            &metadata,
            database_id,
            &position,
            server_id,
            options.blocking,
        )
        .await
        {
            Ok(stream) => stream,
            Err(CdcError::NeedsResync { .. })
                if options.auto_resnapshot && !resnapshot_attempted =>
            {
                targets = resnapshot_targets(
                    pool,
                    metadata_path,
                    database_id,
                    report,
                    targets,
                    options.resnapshot_options.clone(),
                )
                .await?;
                let checkpoint = metadata.snapshot_checkpoint(database_id)?.ok_or_else(|| {
                    CdcError::InvalidCheckpoint(
                        "automatic resnapshot did not capture a source position".to_owned(),
                    )
                })?;
                position = StreamPosition::from_checkpoint(checkpoint, report.server.flavor)?;
                pending = PendingTransaction::default();
                blocked_targets.clear();
                reconnect_attempts = 0;
                resnapshot_attempted = true;
                continue;
            }
            Err(CdcError::Mysql(error)) => {
                position = reconnect_from_checkpoint(
                    &metadata,
                    database_id,
                    report.server.flavor,
                    &options,
                    &mut reconnect_attempts,
                    error,
                )
                .await?;
                pending = PendingTransaction::default();
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut stream_error = None;
        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(event) => {
                    reconnect_attempts = 0;
                    event
                }
                Err(error) => {
                    stream_error = Some(error);
                    break;
                }
            };
            let event_position = u64::from(event.header().log_pos());
            let event_type = event.header().event_type_raw();
            let Some(data) = event
                .read_data()
                .map_err(|error| CdcError::Decode(error.to_string()))?
            else {
                continue;
            };
            match data {
                EventData::GtidEvent(gtid) => {
                    position.pending_gtid = Some(GtidIdentity {
                        sid: gtid.sid(),
                        tag: gtid.tag().map(ToString::to_string),
                        sequence: gtid.gno(),
                    });
                    pending.ordinal = 0;
                }
                EventData::RotateEvent(rotate) => {
                    if !rotate.is_fake() {
                        position.file = sanitize_binlog_filename(&rotate.name())?;
                        position.pos = rotate.position();
                    }
                }
                EventData::RowsEvent(rows_event) => {
                    let table_map = stream.get_tme(rows_event.table_id()).ok_or_else(|| {
                        CdcError::Decode(format!(
                            "row event references unknown table-map ID {}",
                            rows_event.table_id()
                        ))
                    })?;
                    if !table_map
                        .database_name()
                        .eq_ignore_ascii_case(&report.database)
                    {
                        continue;
                    }
                    let table_name = table_map.table_name().into_owned();
                    let Some(&target_index) = target_indexes.get(&table_name.to_ascii_lowercase())
                    else {
                        continue;
                    };
                    let non_transactional = targets[target_index]
                        .source
                        .engine
                        .as_deref()
                        .is_some_and(|engine| !engine.eq_ignore_ascii_case("InnoDB"));
                    if !blocked_targets.contains(&target_index)
                        && decode_rows_event(
                            &rows_event,
                            table_map,
                            &targets[target_index].source,
                            target_index,
                            &position,
                            event_position,
                            event_type,
                            database_id,
                            &metadata,
                            &mut pending,
                            options.max_transaction_bytes,
                        )?
                    {
                        blocked_targets.insert(target_index);
                    }
                    position.pos = event_position;
                    if non_transactional && rows_event.flags().contains(RowsEventFlags::STMT_END) {
                        let outcome = commit_pending(
                            &mut targets,
                            &mut metadata,
                            database_id,
                            &mut position,
                            &mut pending,
                        )?;
                        commits += 1;
                        mutations += outcome;
                        emit_progress(&progress, commits, mutations, &position)?;
                    }
                }
                EventData::XidEvent(_) => {
                    position.pos = event_position;
                    let outcome = commit_pending(
                        &mut targets,
                        &mut metadata,
                        database_id,
                        &mut position,
                        &mut pending,
                    )?;
                    commits += 1;
                    mutations += outcome;
                    emit_progress(&progress, commits, mutations, &position)?;
                }
                EventData::QueryEvent(query) => {
                    let statement = query.query();
                    let normalized = statement.trim().to_ascii_uppercase();
                    position.pos = event_position;
                    if normalized == "BEGIN" {
                        continue;
                    }
                    if normalized == "ROLLBACK" {
                        pending = PendingTransaction::default();
                    }
                    let outcome = commit_pending(
                        &mut targets,
                        &mut metadata,
                        database_id,
                        &mut position,
                        &mut pending,
                    )?;
                    commits += 1;
                    mutations += outcome;
                    emit_progress(&progress, commits, mutations, &position)?;
                }
                _ => {
                    if event_position > 0 {
                        position.pos = event_position;
                    }
                }
            }
            if options
                .max_commits
                .is_some_and(|maximum| commits >= maximum)
            {
                stream.close().await?;
                return finish_result(commits, mutations, &position, targets);
            }
        }
        if let Some(error) = stream_error {
            position = reconnect_from_checkpoint(
                &metadata,
                database_id,
                report.server.flavor,
                &options,
                &mut reconnect_attempts,
                error,
            )
            .await?;
            pending = PendingTransaction::default();
            continue;
        }
        if options.blocking {
            let error = std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "blocking binlog stream ended",
            )
            .into();
            position = reconnect_from_checkpoint(
                &metadata,
                database_id,
                report.server.flavor,
                &options,
                &mut reconnect_attempts,
                error,
            )
            .await?;
            pending = PendingTransaction::default();
            continue;
        }
        if !pending.mutations.is_empty() {
            return Err(CdcError::Decode(
                "binlog stream ended inside a source transaction".to_owned(),
            ));
        }
        return finish_result(commits, mutations, &position, targets);
    }
}

async fn resnapshot_targets(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
    snapshot_options: SnapshotOptions,
) -> Result<Vec<CdcTarget>, CdcError> {
    let mut snapshot_targets = Vec::with_capacity(targets.len());
    for mut target in targets {
        target.store.reset_for_resnapshot()?;
        snapshot_targets.push(SnapshotTarget::new(target.source, target.store)?);
    }
    MetaStore::open(metadata_path)?.begin_resnapshot(database_id, &Utc::now().to_rfc3339())?;
    let snapshot = run_snapshot(
        pool,
        metadata_path,
        database_id,
        report,
        snapshot_targets,
        snapshot_options,
    )
    .await?;
    let mut metadata = MetaStore::open(metadata_path)?;
    let checkpoint = metadata.snapshot_checkpoint(database_id)?.ok_or_else(|| {
        CdcError::InvalidCheckpoint(
            "automatic resnapshot did not persist its handoff position".to_owned(),
        )
    })?;
    let table_names = snapshot
        .targets
        .iter()
        .map(|target| target.source().name.clone())
        .collect::<Vec<_>>();
    metadata.commit_cdc_checkpoint(
        database_id,
        &checkpoint,
        &table_names,
        &Utc::now().to_rfc3339(),
    )?;
    snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store())
        })
        .collect()
}

async fn open_stream(
    pool: &Pool,
    metadata: &MetaStore,
    database_id: &str,
    position: &StreamPosition,
    server_id: u32,
    blocking: bool,
) -> Result<BinlogStream, CdcError> {
    let mut connection = pool.get_conn().await?;
    if matches!(position.kind, PositionKind::FilePosition) {
        let logs = connection
            .query::<mysql_async::Row, _>("SHOW BINARY LOGS")
            .await?;
        let available_size = logs.iter().find_map(|row| {
            let file = row.get::<String, _>(0)?;
            file.eq_ignore_ascii_case(&position.file)
                .then(|| row.get::<u64, _>(1))
                .flatten()
        });
        if available_size.is_none_or(|size| position.pos > size) {
            let reason = format!(
                "binlog checkpoint {}:{} is no longer retained by the source",
                position.file, position.pos
            );
            metadata.mark_database_needs_resync(database_id, &reason)?;
            return Err(CdcError::NeedsResync { reason });
        }
    }
    let request = position.request(server_id, blocking)?;
    connection
        .get_binlog_stream(request)
        .await
        .map_err(CdcError::Mysql)
}

async fn reconnect_from_checkpoint(
    metadata: &MetaStore,
    database_id: &str,
    flavor: SourceFlavor,
    options: &CdcOptions,
    attempts: &mut usize,
    error: MysqlError,
) -> Result<StreamPosition, CdcError> {
    if matches!(&error, MysqlError::Server(server) if server.code == 1236) {
        return Err(classify_stream_error(metadata, database_id, error)?);
    }
    if !options.blocking || *attempts >= options.max_reconnect_attempts {
        return Err(CdcError::Mysql(error));
    }
    let exponent = u32::try_from((*attempts).min(6))
        .map_err(|conversion| CdcError::Decode(conversion.to_string()))?;
    let delay = options
        .reconnect_initial_delay
        .saturating_mul(1_u32 << exponent)
        .min(Duration::from_secs(5));
    *attempts += 1;
    tokio::time::sleep(delay).await;
    let checkpoint = metadata
        .snapshot_checkpoint(database_id)?
        .ok_or_else(|| CdcError::InvalidCheckpoint("CDC position disappeared".to_owned()))?;
    StreamPosition::from_checkpoint(checkpoint, flavor)
}

fn validate_configuration(
    report: &ProbeReport,
    targets: &[CdcTarget],
    options: &CdcOptions,
) -> Result<(), CdcError> {
    if targets.is_empty() {
        return Err(CdcError::InvalidConfiguration(
            "CDC requires at least one target".to_owned(),
        ));
    }
    if options.max_transaction_bytes == 0 {
        return Err(CdcError::InvalidConfiguration(
            "transaction memory cap must be non-zero".to_owned(),
        ));
    }
    if !report.capabilities.log_bin
        || !report.capabilities.row_binlog
        || !report.capabilities.full_row_image
    {
        return Err(CdcError::InvalidConfiguration(
            "source must enable ROW binlogging with FULL row images".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    for target in targets {
        if !names.insert(target.source.name.to_ascii_lowercase()) {
            return Err(CdcError::InvalidConfiguration(format!(
                "duplicate CDC target {}",
                target.source.name
            )));
        }
        if !report
            .tables
            .iter()
            .any(|table| table.name.eq_ignore_ascii_case(&target.source.name))
        {
            return Err(CdcError::InvalidConfiguration(format!(
                "target {} is absent from the probe report",
                target.source.name
            )));
        }
    }
    Ok(())
}

#[derive(Default)]
struct PendingTransaction {
    mutations: Vec<PendingMutation>,
    retained_bytes: usize,
    ordinal: u32,
}

struct PendingMutation {
    target_index: usize,
    row: StoredRow,
}

#[allow(clippy::too_many_arguments)]
fn decode_rows_event(
    rows_event: &RowsEventData<'_>,
    table_map: &mysql_async::binlog::events::TableMapEvent<'_>,
    source: &SourceTable,
    target_index: usize,
    position: &StreamPosition,
    event_position: u64,
    event_type: u8,
    database_id: &str,
    metadata: &MetaStore,
    pending: &mut PendingTransaction,
    maximum_bytes: usize,
) -> Result<bool, CdcError> {
    let mut failed = false;
    for (row_index, row) in rows_event.rows(table_map).enumerate() {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                record_dlq(
                    metadata,
                    database_id,
                    &source.name,
                    position,
                    EventLocation {
                        position: event_position,
                        event_type,
                        row_index,
                    },
                    &error.to_string(),
                )?;
                metadata.mark_table_needs_resync(database_id, &source.name, &error.to_string())?;
                failed = true;
                continue;
            }
        };
        if let Err(error) = decode_row_pair(
            source,
            target_index,
            row,
            position,
            event_position,
            pending,
            maximum_bytes,
        ) {
            record_dlq(
                metadata,
                database_id,
                &source.name,
                position,
                EventLocation {
                    position: event_position,
                    event_type,
                    row_index,
                },
                &error.to_string(),
            )?;
            metadata.mark_table_needs_resync(database_id, &source.name, &error.to_string())?;
            failed = true;
        }
    }
    if failed {
        discard_target_mutations(pending, target_index);
    }
    Ok(failed)
}

fn discard_target_mutations(pending: &mut PendingTransaction, target_index: usize) {
    let mut removed_bytes = 0_usize;
    pending.mutations.retain(|mutation| {
        if mutation.target_index == target_index {
            removed_bytes = removed_bytes
                .saturating_add(mutation.row.estimated_bytes())
                .saturating_add(std::mem::size_of::<PendingMutation>());
            false
        } else {
            true
        }
    });
    pending.retained_bytes = pending.retained_bytes.saturating_sub(removed_bytes);
}

fn decode_row_pair(
    source: &SourceTable,
    target_index: usize,
    (before, after): (Option<BinlogRow>, Option<BinlogRow>),
    position: &StreamPosition,
    event_position: u64,
    pending: &mut PendingTransaction,
    maximum_bytes: usize,
) -> Result<(), CdcError> {
    match (before, after) {
        (None, Some(after)) => {
            let values = decode_row(source, after)?;
            let version = position.version(event_position, pending.ordinal)?;
            let key = insert_key(source, &values, version)?;
            push_mutations(
                pending,
                vec![PendingMutation {
                    target_index,
                    row: StoredRow::new(key, values, version, false),
                }],
                maximum_bytes,
            )
        }
        (Some(before), None) => {
            if source.key.mode == KeyMode::AppendRowId {
                return Err(CdcError::Decode(format!(
                    "{} DELETE has no stable source key and requires resnapshot",
                    source.name
                )));
            }
            let values = decode_row(source, before)?;
            let key = physical_key(source, &values)?;
            push_mutations(
                pending,
                vec![PendingMutation {
                    target_index,
                    row: StoredRow::new(
                        key,
                        values,
                        position.version(event_position, pending.ordinal)?,
                        true,
                    ),
                }],
                maximum_bytes,
            )
        }
        (Some(before), Some(after)) => {
            if source.key.mode == KeyMode::AppendRowId {
                return Err(CdcError::Decode(format!(
                    "{} UPDATE has no stable source key and requires resnapshot",
                    source.name
                )));
            }
            let before_values = decode_row(source, before)?;
            let after_values = decode_row(source, after)?;
            let before_key = physical_key(source, &before_values)?;
            let after_key = physical_key(source, &after_values)?;
            let mut mutations = Vec::with_capacity(2);
            if before_key != after_key {
                mutations.push(PendingMutation {
                    target_index,
                    row: StoredRow::new(
                        before_key,
                        before_values,
                        position.version(event_position, pending.ordinal)?,
                        true,
                    ),
                });
            }
            let ordinal = pending
                .ordinal
                .checked_add(u32::try_from(mutations.len()).map_err(|error| {
                    CdcError::Decode(format!("mutation ordinal conversion failed: {error}"))
                })?)
                .ok_or_else(|| CdcError::Decode("mutation ordinal overflowed".to_owned()))?;
            mutations.push(PendingMutation {
                target_index,
                row: StoredRow::new(
                    after_key,
                    after_values,
                    position.version(event_position, ordinal)?,
                    false,
                ),
            });
            push_mutations(pending, mutations, maximum_bytes)
        }
        (None, None) => Err(CdcError::Decode(
            "row event contains neither before nor after image".to_owned(),
        )),
    }
}

fn push_mutations(
    pending: &mut PendingTransaction,
    mutations: Vec<PendingMutation>,
    maximum_bytes: usize,
) -> Result<(), CdcError> {
    let mutation_count = u32::try_from(mutations.len())
        .map_err(|error| CdcError::Decode(format!("mutation count conversion failed: {error}")))?;
    let next_ordinal = pending
        .ordinal
        .checked_add(mutation_count)
        .ok_or_else(|| CdcError::Decode("mutation ordinal overflowed".to_owned()))?;
    if next_ordinal > u32::from(u16::MAX) {
        return Err(CdcError::Decode(
            "one source transaction exceeds 65,535 row mutations".to_owned(),
        ));
    }
    let retained_bytes = mutations
        .iter()
        .fold(pending.retained_bytes, |bytes, mutation| {
            bytes
                .saturating_add(mutation.row.estimated_bytes())
                .saturating_add(std::mem::size_of::<PendingMutation>())
        });
    if retained_bytes > maximum_bytes {
        return Err(CdcError::TransactionTooLarge {
            retained_bytes,
            maximum_bytes,
        });
    }
    pending.retained_bytes = retained_bytes;
    pending.mutations.extend(mutations);
    pending.ordinal = next_ordinal;
    Ok(())
}

fn commit_pending(
    targets: &mut [CdcTarget],
    metadata: &mut MetaStore,
    database_id: &str,
    position: &mut StreamPosition,
    pending: &mut PendingTransaction,
) -> Result<usize, CdcError> {
    let mut grouped = BTreeMap::<usize, Vec<StoredRow>>::new();
    for mutation in pending.mutations.drain(..) {
        grouped
            .entry(mutation.target_index)
            .or_default()
            .push(mutation.row);
    }
    let mutation_count = grouped.values().map(Vec::len).sum();
    let mut touched = Vec::with_capacity(grouped.len());
    for (target_index, rows) in grouped {
        targets[target_index].store.ingest_cdc(rows)?;
        touched.push(target_index);
    }
    for target_index in &touched {
        targets[*target_index].store.checkpoint()?;
    }
    position.commit_gtid()?;
    let checkpoint = position.checkpoint()?;
    let touched_names = touched
        .iter()
        .map(|index| targets[*index].source.name.clone())
        .collect::<Vec<_>>();
    let checkpoint_record = SnapshotCheckpointRecord {
        kind: checkpoint.kind,
        gtid_set: checkpoint.gtid_set,
        binlog_file: Some(checkpoint.binlog_file),
        binlog_pos: Some(checkpoint.binlog_pos),
    };
    metadata.commit_cdc_checkpoint(
        database_id,
        &checkpoint_record,
        &touched_names,
        &Utc::now().to_rfc3339(),
    )?;
    *pending = PendingTransaction::default();
    Ok(mutation_count)
}

fn emit_progress(
    progress: &ProgressListener,
    commits: usize,
    mutations: usize,
    position: &StreamPosition,
) -> Result<(), CdcError> {
    progress(CdcProgress {
        commits,
        mutations,
        checkpoint: position.checkpoint()?,
    });
    Ok(())
}

fn finish_result(
    commits: usize,
    mutations: usize,
    position: &StreamPosition,
    targets: Vec<CdcTarget>,
) -> Result<CdcResult, CdcError> {
    Ok(CdcResult {
        commits,
        mutations,
        checkpoint: position.checkpoint()?,
        targets,
    })
}

#[derive(Clone, Copy)]
struct EventLocation {
    position: u64,
    event_type: u8,
    row_index: usize,
}

fn record_dlq(
    metadata: &MetaStore,
    database_id: &str,
    table_name: &str,
    position: &StreamPosition,
    location: EventLocation,
    error: &str,
) -> Result<(), CdcError> {
    let id = format!(
        "cdc:{database_id}:{}:{}:{}:{}",
        position.file, location.position, location.event_type, location.row_index
    );
    let event = serde_json::to_string(&json!({
        "binlog_file": position.file,
        "binlog_position": location.position,
        "event_type": location.event_type,
        "row_index": location.row_index,
    }))
    .map_err(|encode_error| CdcError::Decode(encode_error.to_string()))?;
    metadata.record_dlq(
        &id,
        database_id,
        Some(table_name),
        &event,
        error,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

fn classify_stream_error(
    metadata: &MetaStore,
    database_id: &str,
    error: MysqlError,
) -> Result<CdcError, CdcError> {
    if matches!(&error, MysqlError::Server(server) if server.code == 1236) {
        let reason = error.to_string();
        metadata.mark_database_needs_resync(database_id, &reason)?;
        Ok(CdcError::NeedsResync { reason })
    } else {
        Ok(CdcError::Mysql(error))
    }
}

#[derive(Clone, Debug)]
struct GtidIdentity {
    sid: [u8; 16],
    tag: Option<String>,
    sequence: u64,
}

struct StreamPosition {
    kind: PositionKind,
    gtid_set: Option<MysqlGtidSet>,
    pending_gtid: Option<GtidIdentity>,
    file: String,
    pos: u64,
}

enum PositionKind {
    MysqlGtid,
    FilePosition,
}

fn sanitize_binlog_filename(value: &str) -> Result<String, CdcError> {
    let filename = value
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/')
        })
        .collect::<String>();
    if filename.is_empty() {
        return Err(CdcError::Decode(
            "rotate event contains an empty binlog filename".to_owned(),
        ));
    }
    Ok(filename)
}

impl StreamPosition {
    fn from_checkpoint(
        checkpoint: SnapshotCheckpointRecord,
        flavor: SourceFlavor,
    ) -> Result<Self, CdcError> {
        match checkpoint.kind.as_str() {
            "gtid" if flavor == SourceFlavor::Mysql => Ok(Self {
                kind: PositionKind::MysqlGtid,
                gtid_set: Some(MysqlGtidSet::parse(
                    checkpoint.gtid_set.as_deref().ok_or_else(|| {
                        CdcError::InvalidCheckpoint("GTID set is absent".to_owned())
                    })?,
                )?),
                pending_gtid: None,
                file: checkpoint.binlog_file.unwrap_or_default(),
                pos: checkpoint.binlog_pos.unwrap_or(4),
            }),
            "gtid" | "filepos" => Ok(Self {
                kind: PositionKind::FilePosition,
                gtid_set: None,
                pending_gtid: None,
                file: checkpoint.binlog_file.ok_or_else(|| {
                    CdcError::InvalidCheckpoint(
                        "file/position checkpoint is missing its file".to_owned(),
                    )
                })?,
                pos: checkpoint.binlog_pos.ok_or_else(|| {
                    CdcError::InvalidCheckpoint(
                        "file/position checkpoint is missing its position".to_owned(),
                    )
                })?,
            }),
            "polling" => Err(CdcError::InvalidCheckpoint(
                "polling checkpoint cannot start CDC".to_owned(),
            )),
            kind => Err(CdcError::InvalidCheckpoint(format!(
                "unsupported checkpoint kind {kind}"
            ))),
        }
    }

    fn request(&self, server_id: u32, blocking: bool) -> Result<BinlogStreamRequest<'_>, CdcError> {
        let mut request = BinlogStreamRequest::new(server_id)
            .with_filename(self.file.as_bytes())
            .with_pos(self.pos);
        if !blocking {
            request = request.with_non_blocking();
        }
        if matches!(self.kind, PositionKind::MysqlGtid) {
            request = request
                .with_gtid()
                .with_gtid_set(self.gtid_set.as_ref().expect("GTID set").to_sids()?);
        }
        Ok(request)
    }

    fn version(&self, event_position: u64, ordinal: u32) -> Result<u64, CdcError> {
        let ordinal = u16::try_from(ordinal + 1).map_err(|_| {
            CdcError::Decode("one source transaction exceeds 65,535 row mutations".to_owned())
        })?;
        if let Some(gtid) = &self.pending_gtid {
            return gtid
                .sequence
                .checked_shl(16)
                .and_then(|base| base.checked_add(u64::from(ordinal)))
                .ok_or_else(|| CdcError::Decode("GTID version exceeds UInt64".to_owned()));
        }
        let file_index = self
            .file
            .rsplit_once('.')
            .and_then(|(_, index)| index.parse::<u64>().ok())
            .ok_or_else(|| {
                CdcError::InvalidCheckpoint(format!(
                    "binlog file {} has no numeric suffix",
                    self.file
                ))
            })?;
        let file_index = u16::try_from(file_index).map_err(|_| {
            CdcError::Decode("binlog file index exceeds the version range".to_owned())
        })?;
        let event_position = u32::try_from(event_position).map_err(|_| {
            CdcError::Decode("binlog event position exceeds the version range".to_owned())
        })?;
        Ok((u64::from(file_index) << 48) | (u64::from(event_position) << 16) | u64::from(ordinal))
    }

    fn commit_gtid(&mut self) -> Result<(), CdcError> {
        if let Some(gtid) = self.pending_gtid.take()
            && let Some(set) = &mut self.gtid_set
        {
            set.add_event(gtid.sid, gtid.tag.as_deref(), gtid.sequence)?;
        }
        Ok(())
    }

    fn checkpoint(&self) -> Result<CdcCheckpoint, CdcError> {
        if self.file.is_empty() {
            return Err(CdcError::InvalidCheckpoint(
                "binlog stream has no current file".to_owned(),
            ));
        }
        Ok(CdcCheckpoint {
            kind: match self.kind {
                PositionKind::MysqlGtid => "gtid",
                PositionKind::FilePosition => "filepos",
            }
            .to_owned(),
            gtid_set: self.gtid_set.as_ref().map(ToString::to_string),
            binlog_file: self.file.clone(),
            binlog_pos: self.pos,
        })
    }
}

fn generated_server_id(database_id: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    database_id.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let hash = hasher.finish().to_le_bytes();
    let value = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);
    value.max(1)
}

#[cfg(test)]
mod tests {
    use super::{StreamPosition, generated_server_id, sanitize_binlog_filename};
    use pintail_meta::SnapshotCheckpointRecord;
    use pintail_probe::SourceFlavor;

    #[test]
    fn file_position_versions_are_ordered_and_deterministic() {
        let position = StreamPosition::from_checkpoint(
            SnapshotCheckpointRecord {
                kind: "filepos".to_owned(),
                gtid_set: None,
                binlog_file: Some("mysql-bin.000007".to_owned()),
                binlog_pos: Some(4),
            },
            SourceFlavor::Mysql,
        )
        .expect("position");
        assert!(
            position.version(200, 0).expect("first") < position.version(201, 0).expect("second")
        );
        assert_eq!(
            position.version(200, 3).expect("deterministic"),
            position.version(200, 3).expect("deterministic")
        );
        assert_ne!(generated_server_id("a"), 0);
    }

    #[test]
    fn strips_non_filename_trailers_from_rotate_events() {
        assert_eq!(
            sanitize_binlog_filename("mysql-bin.000002\u{fffd}\u{5cf}\u{fffd}")
                .expect("sanitized filename"),
            "mysql-bin.000002"
        );
    }
}
